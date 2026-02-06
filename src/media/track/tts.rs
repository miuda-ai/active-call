use crate::{
    event::{EventSender, SessionEvent},
    media::{
        AudioFrame, Samples, cache,
        processor::ProcessorChain,
        track::{Track, TrackConfig, TrackId, TrackPacketSender},
    },
    synthesis::{
        Subtitle, SynthesisClient, SynthesisCommand, SynthesisCommandReceiver,
        SynthesisCommandSender, SynthesisEvent, bytes_size_to_duration,
    },
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use audio_codec::bytes_to_samples;
use base64::{Engine, prelude::BASE64_STANDARD};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::{Mutex, mpsc},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use unic_emoji::char::is_emoji;

#[derive(Clone)]
pub struct SynthesisHandle {
    pub play_id: Option<String>,
    pub ssrc: u32,
    pub command_tx: SynthesisCommandSender,
}

struct EmitEntry {
    chunks: VecDeque<Bytes>,
    finished: bool,
    finish_at: Instant,
}

struct Metadata {
    cache_key: String,
    text: String,
    first_chunk: bool,
    chunks: Vec<Bytes>,
    subtitles: Vec<Subtitle>,
    total_bytes: usize,
    emitted_bytes: usize,
    recv_time: u64,
    ttfb: u64,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            cache_key: String::new(),
            text: String::new(),
            chunks: Vec::new(),
            first_chunk: true,
            subtitles: Vec::new(),
            total_bytes: 0,
            emitted_bytes: 0,
            recv_time: 0,
            ttfb: 0,
        }
    }
}

// Synthesis task for TTS track, handle tts command and synthesis event emit audio chunk to media stream
struct TtsTask {
    ssrc: u32,
    play_id: Option<String>,
    track_id: TrackId,
    session_id: String,
    client: Box<dyn SynthesisClient>,
    command_rx: SynthesisCommandReceiver,
    packet_sender: TrackPacketSender,
    event_sender: EventSender,
    cancel_token: CancellationToken,
    processor_chain: ProcessorChain,
    cache_enabled: bool,
    sample_rate: u32,
    ptime: Duration,
    cache_buffer: BytesMut,
    emit_q: VecDeque<EmitEntry>,
    // metadatas for each tts command
    metadatas: HashMap<usize, Metadata>,
    // seq of current progressing tts command, ignore result from cmd_seq less than cur_seq
    cur_seq: usize,
    streaming: bool,
    graceful: Arc<AtomicBool>,
    // Jitter buffer state
    buffering_state: Option<Instant>,
    min_buffer_size: usize,
    max_buffer_wait: Duration,
}

pub fn strip_emoji_chars(text: &str) -> String {
    text.chars()
        .filter(|&c| c.is_ascii() || !is_emoji(c))
        .collect()
}

impl TtsTask {
    async fn run(mut self) -> Result<()> {
        let mut stream;
        match self.client.start().await {
            Ok(s) => stream = s,
            Err(e) => {
                error!(
                    session_id = %self.session_id,
                    track_id = %self.track_id,
                    play_id = ?self.play_id,
                    provider = %self.client.provider(),
                    error = %e,
                    "failed to start tts task"
                );
                return Err(e);
            }
        };

        info!(
            session_id = %self.session_id,
            track_id = %self.track_id,
            play_id = ?self.play_id,
            streaming = self.streaming,
            provider = %self.client.provider(),
            "tts task started"
        );
        let start_time = crate::media::get_timestamp();
        // seqence number of next tts command in stream, used for non streaming mode
        let mut cmd_seq = if self.streaming { None } else { Some(0) };
        let mut cmd_finished = false;
        let mut tts_finished = false;
        let mut cancel_received = false;
        let sample_rate = self.sample_rate;
        let packet_duration_ms = self.ptime.as_millis();
        // capacity of samples buffer
        let capacity = sample_rate as usize * packet_duration_ms as usize / 500;
        let mut ptimer = tokio::time::interval(self.ptime);
        // samples buffer, emit all even if it was not fully filled
        let mut samples = vec![0u8; capacity];
        let mut last_chunk_recv_time = Instant::now();
        // loop until cancelled
        loop {
            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled(), if !cancel_received => {
                    cancel_received = true;
                    let graceful = self.graceful.load(Ordering::Relaxed);
                    let emitted_bytes = self.metadatas.get(&self.cur_seq).map(|entry| entry.emitted_bytes).unwrap_or(0);
                    let total_bytes = self.metadatas.get(&self.cur_seq).map(|entry| entry.total_bytes).unwrap_or(0);
                    debug!(
                        session_id = %self.session_id,
                        track_id = %self.track_id,
                        play_id = ?self.play_id,
                        cur_seq = self.cur_seq,
                        emitted_bytes,
                        total_bytes,
                        graceful,
                        streaming = self.streaming,
                        "tts task cancelled"
                    );
                    self.handle_interrupt();
                    // quit if on streaming mode (graceful only work for non streaming mode)
                    //         or graceful not set, this is ordinary cancel
                    //         or cur seq not started
                    if self.streaming || !graceful || emitted_bytes == 0 {
                        break;
                    }

                    // else, stop receiving command
                    cmd_finished = true;
                    self.client.stop().await?;
                }
                _ = ptimer.tick() => {
                    samples.fill(0);
                    let mut i = 0;

                    // Jitter Buffer Logic
                    let total_buffered_bytes: usize = self.emit_q.iter()
                        .map(|e| e.chunks.iter().map(|c| c.len()).sum::<usize>())
                        .sum();

                    if let Some(start) = self.buffering_state {
                        let elapsed = start.elapsed();
                        // Check if the current processing entry is already marked as finished (no more data coming)
                        let current_entry_finished = self.emit_q.front().map(|e| e.finished).unwrap_or(false);

                        if total_buffered_bytes >= self.min_buffer_size
                            || elapsed >= self.max_buffer_wait
                            || (cmd_finished && tts_finished)
                            || current_entry_finished
                        {
                            if total_buffered_bytes > 0 {
                                debug!(
                                    session_id = %self.session_id,
                                    track_id = %self.track_id,
                                    play_id = ?self.play_id,
                                    cur_seq = self.cur_seq,
                                    buffered_bytes = total_buffered_bytes,
                                    elapsed_ms = elapsed.as_millis(),
                                    "tts jitter buffer released"
                                );
                            }
                            self.buffering_state = None;
                        }
                    }

                    while self.buffering_state.is_none() && i < capacity && !self.emit_q.is_empty(){
                        let first_entry = &mut self.emit_q[0];
                        while i < capacity && !first_entry.chunks.is_empty() {
                            let first_chunk = &mut first_entry.chunks[0];
                            let remaining = capacity - i;
                            let available = first_chunk.len();
                            let len = usize::min(remaining, available);
                            let cut = first_chunk.split_to(len);
                            samples[i..i+len].copy_from_slice(&cut);
                            i += len;
                            self.metadatas.get_mut(&self.cur_seq).map(|entry| {
                                entry.emitted_bytes += len;
                            });
                            if first_chunk.is_empty() {
                                first_entry.chunks.pop_front();
                            }
                        }

                        if first_entry.chunks.is_empty(){
                            // finish_at is estimated duration, it may be later than now
                            let elapsed = if first_entry.finish_at <= Instant::now() {
                                first_entry.finish_at.elapsed()
                            } else {
                                Duration::new(0, 0)
                            };

                            if !first_entry.finished {
                                let elapsed_ms = elapsed.as_millis();
                                if elapsed_ms > 100 {
                                     // Underrun detection
                                    if elapsed_ms > 200 && last_chunk_recv_time.elapsed().as_millis() > 500 {
                                        if elapsed_ms % 500 < 50 {
                                            warn!(
                                                    session_id = %self.session_id,
                                                    track_id = %self.track_id,
                                                    play_id = ?self.play_id,
                                                    cur_seq = self.cur_seq,
                                                    elapsed_ms,
                                                    stall_duration_ms = last_chunk_recv_time.elapsed().as_millis(),
                                                    "tts playback stalled: waiting for more chunks (potential silence)"
                                                );
                                        }
                                    } else if elapsed_ms % 500 < 50 {
                                        info!(
                                            session_id = %self.session_id,
                                            track_id = %self.track_id,
                                            play_id = ?self.play_id,
                                            cur_seq = self.cur_seq,
                                            elapsed_ms,
                                            "tts buffer underrun: waiting for more chunks"
                                        );
                                    }
                                }
                                if self.buffering_state.is_none() {
                                    self.buffering_state = Some(Instant::now());
                                    // Stop consuming for this tick
                                    break;
                                }
                            }
                            if self.streaming && cmd_finished && (tts_finished || elapsed > Duration::from_secs(10)) {
                                debug!(
                                    session_id = %self.session_id,
                                    track_id = %self.track_id,
                                    play_id = ?self.play_id,
                                    tts_finished,
                                    entry_finished = first_entry.finished,
                                    elapsed_ms = elapsed.as_millis(),
                                    "tts streaming finished"
                                );
                                tts_finished = true;
                                self.emit_q.clear();
                                continue;
                            }

                            if !self.streaming && (first_entry.finished || elapsed > Duration::from_secs(3))
                            {
                                debug!(
                                    session_id = %self.session_id,
                                    track_id = %self.track_id,
                                    play_id = ?self.play_id,
                                    cur_seq = self.cur_seq,
                                    entry_finished = first_entry.finished,
                                    elapsed_ms = elapsed.as_millis(),
                                    "tts entry finished"
                                );

                                self.emit_q.pop_front();
                                self.cur_seq += 1;

                                // if passage is set, clearn emit_q, task will quit at next iteration
                                if self.graceful.load(Ordering::Relaxed) {
                                    self.emit_q.clear();
                                }

                                // else, continue process next seq
                                continue;
                            }

                            // current seq not finished, but have no more data to emit
                            break;
                        }
                    }

                    if i == 0 && self.emit_q.is_empty() && cmd_finished && tts_finished {
                        debug!(
                            session_id = %self.session_id,
                            track_id = %self.track_id,
                            play_id = ?self.play_id,
                            "tts task processed all data"
                        );
                        break;
                    }

                    let samples = if i == 0 {
                        Samples::Empty
                    } else {
                        Samples::PCM {
                            samples: bytes_to_samples(&samples[..]),
                        }
                    };

                    let mut frame = AudioFrame {
                        track_id: self.track_id.clone(),
                        samples,
                        timestamp: crate::media::get_timestamp(),
                        sample_rate,
                        channels: 1,
                        ..Default::default()
                    };

                    if let Err(e) = self.processor_chain.process_frame(&mut frame) {
                        warn!(
                            session_id = %self.session_id,
                            track_id = %self.track_id,
                            play_id = ?self.play_id,
                            error = %e,
                            "error processing audio frame"
                        );
                        break;
                    }

                    if let Err(_) = self.packet_sender.send(frame) {
                        warn!(
                            session_id = %self.session_id,
                            track_id = %self.track_id,
                            play_id = ?self.play_id,
                            "track packet sender closed, stopping task"
                        );
                        break;
                    }
                }
                mut cmd = self.command_rx.recv(), if !cmd_finished => {
                    if let Some(cmd) = cmd.as_mut() {
                        if cmd.option.session_id.is_none() {
                            cmd.option.session_id = Some(self.session_id.clone());
                        }
                        self.handle_cmd(cmd, cmd_seq).await;
                        cmd_seq.as_mut().map(|seq| *seq += 1);
                    }

                    // set finished if command sender is exhausted or end_of_stream is true
                    if cmd.is_none() || cmd.as_ref().map(|c| c.end_of_stream).unwrap_or(false) {
                        debug!(
                            session_id = %self.session_id,
                            track_id = %self.track_id,
                            play_id = ?self.play_id,
                            cmd_seq = ?cmd_seq,
                            end_of_stream = cmd.as_ref().map(|c| c.end_of_stream).unwrap_or(true),
                            "tts command finished"
                        );
                        cmd_finished = true;
                        self.client.stop().await?;
                    }
                }
                item = stream.next(), if !tts_finished => {
                    if let Some((cmd_seq, res)) = item {
                        if self.handle_event(cmd_seq, res).await {
                             last_chunk_recv_time = Instant::now();
                        }
                    }else{
                        debug!(
                            session_id = %self.session_id,
                            track_id = %self.track_id,
                            play_id = ?self.play_id,
                            "tts event stream finished"
                        );
                        tts_finished = true;
                    }
                }
            }
        }

        let (emitted_bytes, total_bytes) = self.metadatas.values().fold((0, 0), |(a, b), entry| {
            (a + entry.emitted_bytes, b + entry.total_bytes)
        });

        let duration_ms = (crate::media::get_timestamp() - start_time) as f64 / 1000.0;
        info!(
            session_id = %self.session_id,
            track_id = %self.track_id,
            play_id = ?self.play_id,
            cur_seq = self.cur_seq,
            cmd_seq = ?cmd_seq,
            cmd_finished,
            tts_finished,
            streaming = self.streaming,
            emitted_bytes,
            total_bytes,
            duration_ms,
            provider = %self.client.provider(),
            "tts task finished"
        );

        self.event_sender
            .send(SessionEvent::TrackEnd {
                track_id: self.track_id.clone(),
                timestamp: crate::media::get_timestamp(),
                duration: crate::media::get_timestamp() - start_time,
                ssrc: self.ssrc,
                play_id: self.play_id.clone(),
            })
            .ok();
        Ok(())
    }

    async fn handle_cmd(&mut self, cmd: &SynthesisCommand, cmd_seq: Option<usize>) {
        let session_id = self.session_id.clone();
        let track_id = self.track_id.clone();
        let play_id = self.play_id.clone();
        let streaming = self.streaming;
        debug!(
            session_id = %session_id,
            track_id = %self.track_id,
            play_id = ?play_id,
            cmd_seq = ?cmd_seq,
            text_preview = %cmd.text.chars().take(20).collect::<String>(),
            text_length = cmd.text.len(),
            base64 = cmd.base64,
            end_of_stream = cmd.end_of_stream,
            cache_key = cmd.cache_key.as_deref(),
            "tts track: received command"
        );
        let text = &cmd.text;

        // if cmd_seq is None(streaming mode), all metadata save into index 0
        let assume_seq = cmd_seq.unwrap_or(0);
        let meta_entry = self.metadatas.entry(assume_seq).or_default();
        meta_entry.text = text.clone();
        meta_entry.recv_time = crate::media::get_timestamp();

        // Handle empty text (e.g. just for signaling end_of_stream or hangup updates)
        if text.is_empty() && !cmd.base64 {
            // In non-streaming mode, we need to mark this specific sequence as finished
            // because we won't call synthesize for it.
            // In streaming mode, multiple commands share the same entry (0).
            // We must NOT mark it finished here, because previous commands might still be generating audio.
            // The TTS provider (client) is responsible for marking entry 0 finished when the stream ends.
            if !self.streaming {
                let emit_entry = self.get_emit_entry_mut(assume_seq);
                emit_entry.map(|entry| entry.finished = true);
            }
            return;
        }

        let emit_entry = self.get_emit_entry_mut(assume_seq);

        let text = strip_emoji_chars(text);

        // if text is empty:
        // in streaming mode, skip it
        // in non streaming mode, set entry[seq] finished to true
        if text.is_empty() {
            if !streaming {
                emit_entry.map(|entry| entry.finished = true);
            }
            return;
        }

        if cmd.base64 {
            match BASE64_STANDARD.decode(text) {
                Ok(bytes) => {
                    emit_entry.map(|entry| {
                        entry.chunks.push_back(Bytes::from(bytes));
                        entry.finished = true;
                    });
                }
                Err(e) => {
                    warn!(
                        session_id = %session_id,
                        track_id = %track_id,
                        play_id = ?play_id,
                        cmd_seq = ?cmd_seq,
                        error = %e,
                        "failed to decode base64 text"
                    );
                    emit_entry.map(|entry| entry.finished = true);
                }
            }
            return;
        }

        if self.cache_enabled && self.handle_cache(&cmd, assume_seq).await {
            return;
        }

        if let Err(e) = self
            .client
            .synthesize(&text, cmd_seq, Some(cmd.option.clone()))
            .await
        {
            warn!(
                session_id = %session_id,
                track_id = %track_id,
                play_id = ?play_id,
                cmd_seq = ?cmd_seq,
                text_length = text.len(),
                provider = %self.client.provider(),
                error = %e,
                "failed to synthesize text"
            );
        }
    }

    // set cache key for each cmd, return true if cached and retrieve succeed
    async fn handle_cache(&mut self, cmd: &SynthesisCommand, cmd_seq: usize) -> bool {
        let start_time_ms = crate::media::get_timestamp();
        let cache_key = cmd.cache_key.clone().unwrap_or_else(|| {
            cache::generate_cache_key(
                &format!("tts:{}{}", self.client.provider(), cmd.text),
                self.sample_rate,
                cmd.option.speaker.as_ref(),
                cmd.option.speed,
            )
        });

        // initial chunks map at cmd_seq for tts to save chunks
        self.metadatas.get_mut(&cmd_seq).map(|entry| {
            entry.cache_key = cache_key.clone();
        });

        if cache::is_cached(&cache_key).await.unwrap_or_default() {
            match cache::retrieve_from_cache_with_buffer(&cache_key, &mut self.cache_buffer).await {
                Ok(()) => {
                    debug!(
                        session_id = %self.session_id,
                        track_id = %self.track_id,
                        play_id = ?self.play_id,
                        cmd_seq,
                        cache_key = %cache_key,
                        text_preview = %cmd.text.chars().take(20).collect::<String>(),
                        "using cached audio"
                    );
                    let bytes = self.cache_buffer.split().freeze();
                    let len = bytes.len();

                    self.get_emit_entry_mut(cmd_seq).map(|entry| {
                        entry.chunks.push_back(bytes);
                        entry.finished = true;
                    });

                    let duration = (crate::media::get_timestamp() - start_time_ms) as u32;
                    self.event_sender
                        .send(SessionEvent::Metrics {
                            timestamp: crate::media::get_timestamp(),
                            key: format!("completed.tts.{}", self.client.provider()),
                            data: serde_json::json!({
                                    "speaker": cmd.option.speaker,
                                    "playId": self.play_id,
                                    "cmdSeq": cmd_seq,
                                    "length": len,
                                    "cached": true,
                                    "ttfb": duration,
                                    "duration": duration,
                            }),
                            duration,
                        })
                        .ok();
                    return true;
                }
                Err(e) => {
                    warn!(
                        session_id = %self.session_id,
                        track_id = %self.track_id,
                        play_id = ?self.play_id,
                        cmd_seq,
                        cache_key = %cache_key,
                        error = %e,
                        "error retrieving cached audio"
                    );
                }
            }
        }
        false
    }

    async fn handle_event(
        &mut self,
        cmd_seq: Option<usize>,
        event: Result<SynthesisEvent>,
    ) -> bool {
        let assume_seq = cmd_seq.unwrap_or(0);
        match event {
            Ok(SynthesisEvent::AudioChunk(mut chunk)) => {
                let recv_len = chunk.len();
                let entry = self.metadatas.entry(assume_seq).or_default();

                if entry.first_chunk {
                    // first chunk
                    if chunk.len() > 44 && chunk[..4] == [0x52, 0x49, 0x46, 0x46] {
                        let _ = chunk.split_to(44);
                    }
                    entry.first_chunk = false;
                    entry.ttfb = crate::media::get_timestamp() - entry.recv_time;
                }

                entry.total_bytes += chunk.len();

                // if cache is enabled, save complete chunks for caching
                if self.cache_enabled {
                    entry.chunks.push(chunk.clone());
                }

                let duration = Duration::from_millis(bytes_size_to_duration(
                    chunk.len(),
                    self.sample_rate,
                ) as u64);
                self.get_emit_entry_mut(assume_seq).map(|entry| {
                    entry.chunks.push_back(chunk.clone());
                    entry.finish_at += duration;
                });
                return recv_len > 0;
            }
            Ok(SynthesisEvent::Subtitles(subtitles)) => {
                self.metadatas.get_mut(&assume_seq).map(|entry| {
                    entry.subtitles.extend(subtitles);
                });
            }
            Ok(SynthesisEvent::Finished) => {
                let entry = self.metadatas.entry(assume_seq).or_default();
                let duration = (crate::media::get_timestamp() - entry.recv_time) as u32;
                debug!(
                    session_id = %self.session_id,
                    track_id = %self.track_id,
                    play_id = ?self.play_id,
                    cmd_seq = ?cmd_seq,
                    streaming = self.streaming,
                    total_bytes = entry.total_bytes,
                    duration,
                    "tts synthesis completed for command sequence"
                );
                self.event_sender
                    .send(SessionEvent::Metrics {
                        timestamp: crate::media::get_timestamp(),
                        key: format!("completed.tts.{}", self.client.provider()),
                        data: serde_json::json!({
                                "playId": self.play_id,
                                "cmdSeq": cmd_seq,
                                "length": entry.total_bytes,
                                "cached": false,
                                "ttfb": entry.ttfb,
                                "duration": duration,
                        }),
                        duration,
                    })
                    .ok();

                // For streaming mode, mark the emit entry as finished
                // This allows the emit loop to complete after all chunks are sent
                if self.streaming {
                    self.get_emit_entry_mut(assume_seq)
                        .map(|entry| entry.finished = true);
                    return false;
                }

                // if cache is enabled, cache key set by handle_cache
                if self.cache_enabled
                    && !cache::is_cached(&entry.cache_key).await.unwrap_or_default()
                {
                    if let Err(e) =
                        cache::store_in_cache_vectored(&entry.cache_key, &entry.chunks).await
                    {
                        warn!(
                            session_id = %self.session_id,
                            track_id = %self.track_id,
                            play_id = ?self.play_id,
                            cmd_seq = ?cmd_seq,
                            cache_key = %entry.cache_key,
                            error = %e,
                            "failed to store audio in cache"
                        );
                    } else {
                        debug!(
                            session_id = %self.session_id,
                            track_id = %self.track_id,
                            play_id = ?self.play_id,
                            cmd_seq = ?cmd_seq,
                            cache_key = %entry.cache_key,
                            total_bytes = entry.total_bytes,
                            "stored audio in cache"
                        );
                    }
                    entry.chunks.clear();
                }

                self.get_emit_entry_mut(assume_seq)
                    .map(|entry| entry.finished = true);
            }
            Err(e) => {
                error!(
                    session_id = %self.session_id,
                    track_id = %self.track_id,
                    play_id = ?self.play_id,
                    cmd_seq = ?cmd_seq,
                    error = %e,
                    "tts synthesis event error"
                );
                // emit to client
                self.event_sender
                    .send(SessionEvent::Error {
                        track_id: self.track_id.clone(),
                        timestamp: crate::media::get_timestamp(),
                        sender: "tts".to_string(),
                        error: e.to_string(),
                        code: Some(500),
                    })
                    .ok();
                // set finished to true if cmd_seq failed
                self.get_emit_entry_mut(assume_seq)
                    .map(|entry| entry.finished = true);
            }
        }
        false
    }

    // get mutable reference of result at cmd_seq, resize if needed, update the last_update
    // if cmd_seq is less than cur_seq, return none
    fn get_emit_entry_mut(&mut self, cmd_seq: usize) -> Option<&mut EmitEntry> {
        // ignore if cmd_seq is less than cur_seq
        if cmd_seq < self.cur_seq {
            debug!(
                session_id = %self.session_id,
                track_id = %self.track_id,
                play_id = ?self.play_id,
                cmd_seq,
                cur_seq = self.cur_seq,
                "ignoring timeout tts result"
            );
            return None;
        }

        // resize emit_q if needed
        let i = cmd_seq - self.cur_seq;
        if i >= self.emit_q.len() {
            self.emit_q.resize_with(i + 1, || EmitEntry {
                chunks: VecDeque::new(),
                finished: false,
                finish_at: Instant::now(),
            });
        }
        Some(&mut self.emit_q[i])
    }

    fn handle_interrupt(&self) {
        if let Some(entry) = self.metadatas.get(&self.cur_seq) {
            let current = bytes_size_to_duration(entry.emitted_bytes, self.sample_rate);
            let total_duration = bytes_size_to_duration(entry.total_bytes, self.sample_rate);
            let text = entry.text.clone();
            let mut position = None;

            for subtitle in entry.subtitles.iter().rev() {
                if subtitle.begin_time < current {
                    position = Some(subtitle.begin_index);
                    break;
                }
            }

            let interruption = SessionEvent::Interruption {
                track_id: self.track_id.clone(),
                timestamp: crate::media::get_timestamp(),
                play_id: self.play_id.clone(),
                subtitle: Some(text),
                position,
                total_duration,
                current,
            };
            self.event_sender.send(interruption).ok();
        }
    }
}

pub struct TtsTrack {
    track_id: TrackId,
    session_id: String,
    streaming: bool,
    play_id: Option<String>,
    processor_chain: ProcessorChain,
    config: TrackConfig,
    cancel_token: CancellationToken,
    use_cache: bool,
    command_rx: Mutex<Option<SynthesisCommandReceiver>>,
    client: Mutex<Option<Box<dyn SynthesisClient>>>,
    ssrc: u32,
    graceful: Arc<AtomicBool>,
    min_buffer_duration: Duration,
    max_buffer_wait: Duration,
}

impl SynthesisHandle {
    pub fn new(command_tx: SynthesisCommandSender, play_id: Option<String>, ssrc: u32) -> Self {
        Self {
            play_id,
            ssrc,
            command_tx,
        }
    }
    pub fn try_send(
        &self,
        cmd: SynthesisCommand,
    ) -> Result<(), mpsc::error::SendError<SynthesisCommand>> {
        if self.play_id == cmd.play_id {
            self.command_tx.send(cmd)
        } else {
            Err(mpsc::error::SendError(cmd))
        }
    }
}

impl TtsTrack {
    pub fn new(
        track_id: TrackId,
        session_id: String,
        streaming: bool,
        play_id: Option<String>,
        command_rx: SynthesisCommandReceiver,
        client: Box<dyn SynthesisClient>,
    ) -> Self {
        let config = TrackConfig::default();
        Self {
            track_id,
            session_id,
            streaming,
            play_id,
            processor_chain: ProcessorChain::new(config.samplerate),
            config,
            cancel_token: CancellationToken::new(),
            command_rx: Mutex::new(Some(command_rx)),
            use_cache: true,
            client: Mutex::new(Some(client)),
            graceful: Arc::new(AtomicBool::new(false)),
            ssrc: 0,
            min_buffer_duration: Duration::from_millis(200), // Default 200ms
            max_buffer_wait: Duration::from_millis(500),     // Default 500ms
        }
    }
    pub fn with_ssrc(mut self, ssrc: u32) -> Self {
        self.ssrc = ssrc;
        self
    }
    pub fn with_config(mut self, config: TrackConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.config = self.config.with_sample_rate(sample_rate);
        self.processor_chain = ProcessorChain::new(sample_rate);
        self
    }

    pub fn with_ptime(mut self, ptime: Duration) -> Self {
        self.config = self.config.with_ptime(ptime);
        self
    }

    pub fn with_cache_enabled(mut self, use_cache: bool) -> Self {
        self.use_cache = use_cache;
        self
    }

    pub fn with_jitter_buffer(mut self, min: Duration, max: Duration) -> Self {
        self.min_buffer_duration = min;
        self.max_buffer_wait = max;
        self
    }
}

#[async_trait]
impl Track for TtsTrack {
    fn ssrc(&self) -> u32 {
        self.ssrc
    }
    fn id(&self) -> &TrackId {
        &self.track_id
    }
    fn config(&self) -> &TrackConfig {
        &self.config
    }
    fn processor_chain(&mut self) -> &mut ProcessorChain {
        &mut self.processor_chain
    }

    async fn handshake(&mut self, _offer: String, _timeout: Option<Duration>) -> Result<String> {
        Ok("".to_string())
    }
    async fn update_remote_description(&mut self, _answer: &String) -> Result<()> {
        Ok(())
    }

    async fn start(
        &mut self,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let client = self
            .client
            .lock()
            .await
            .take()
            .ok_or(anyhow!("Client not found"))?;
        let command_rx = self
            .command_rx
            .lock()
            .await
            .take()
            .ok_or(anyhow!("Command receiver not found"))?;

        let task = TtsTask {
            play_id: self.play_id.clone(),
            track_id: self.track_id.clone(),
            session_id: self.session_id.clone(),
            client,
            command_rx,
            event_sender,
            packet_sender,
            cancel_token: self.cancel_token.clone(),
            processor_chain: self.processor_chain.clone(),
            cache_enabled: self.use_cache && !self.streaming,
            sample_rate: self.config.samplerate,
            ptime: self.config.ptime,
            cache_buffer: BytesMut::new(),
            emit_q: VecDeque::new(),
            metadatas: HashMap::new(),
            cur_seq: 0,
            streaming: self.streaming,
            graceful: self.graceful.clone(),
            ssrc: self.ssrc,
            buffering_state: Some(Instant::now()),
            min_buffer_size: (self.config.samplerate as usize
                * 2
                * self.min_buffer_duration.as_millis() as usize)
                / 1000,
            max_buffer_wait: self.max_buffer_wait,
        };
        debug!(
            session_id = %self.session_id,
            track_id = %self.track_id,
            play_id = ?self.play_id,
            streaming = self.streaming,
            "spawning tts task"
        );
        crate::spawn(async move {
            if let Err(e) = task.run().await {
                tracing::error!("tts task error: {:?}", e);
            }
        });
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    async fn stop_graceful(&self) -> Result<()> {
        self.graceful.store(true, Ordering::Relaxed);
        self.stop().await
    }

    async fn send_packet(&mut self, _packet: &AudioFrame) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesis::SynthesisType;
    use futures::stream::BoxStream;
    use tokio::sync::{broadcast, mpsc};
    use tokio_stream::wrappers::UnboundedReceiverStream;

    struct MockClient {
        stream_rx: Option<mpsc::UnboundedReceiver<(Option<usize>, Result<SynthesisEvent>)>>,
    }

    impl MockClient {
        fn new(rx: mpsc::UnboundedReceiver<(Option<usize>, Result<SynthesisEvent>)>) -> Self {
            Self {
                stream_rx: Some(rx),
            }
        }
    }

    #[async_trait]
    impl SynthesisClient for MockClient {
        fn provider(&self) -> SynthesisType {
            SynthesisType::Other("test".to_string())
        }
        async fn start(
            &mut self,
        ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
            let rx = self.stream_rx.take().unwrap();
            Ok(UnboundedReceiverStream::new(rx).boxed())
        }
        async fn synthesize(
            &mut self,
            _text: &str,
            _cmd_seq: Option<usize>,
            _option: Option<crate::synthesis::SynthesisOption>,
        ) -> Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_jitter_buffer() {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel(); // Fixed: Unbounded
        let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();
        let (session_event_tx, _session_event_rx) = broadcast::channel(10); // Fixed: Broadcast

        let client = Box::new(MockClient::new(event_rx));
        let cancel_token = CancellationToken::new();

        let sample_rate = 8000;
        let ptime = Duration::from_millis(20);
        let _min_buffer_duration = Duration::from_millis(200);
        // 200ms at 8000Hz (16bit) = 3200 bytes
        let min_buffer_size = (sample_rate as usize * 2 * 200) / 1000;

        let task = TtsTask {
            play_id: Some("test".to_string()),
            track_id: "track1".to_string(),
            session_id: "session1".to_string(),
            client,
            command_rx: cmd_rx,
            event_sender: session_event_tx,
            packet_sender: packet_tx,
            cancel_token: cancel_token.clone(),
            processor_chain: ProcessorChain::new(sample_rate),
            cache_enabled: false,
            sample_rate,
            ptime,
            cache_buffer: BytesMut::new(),
            emit_q: VecDeque::new(),
            metadatas: HashMap::new(),
            cur_seq: 0,
            streaming: true,
            graceful: Arc::new(AtomicBool::new(false)),
            ssrc: 1234,
            buffering_state: Some(Instant::now()),
            min_buffer_size,
            max_buffer_wait: Duration::from_secs(10),
        };

        tokio::spawn(async move {
            task.run().await.unwrap();
        });

        // 1. Initial state: buffering.
        let frame = packet_rx.recv().await.unwrap();
        match frame.samples {
            Samples::Empty => {}
            Samples::PCM { ref samples } => assert!(samples.len() == 0),
            _ => panic!("Unexpected sample type"),
        }

        // 2. Send 1 chunk (1000 bytes, ~62ms).
        let chunk = Bytes::from(vec![0u8; 1000]);
        event_tx
            .send((None, Ok(SynthesisEvent::AudioChunk(chunk))))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Drain rx a bit, should still be empty
        while let Ok(frame) = packet_rx.try_recv() {
            match frame.samples {
                Samples::Empty => {}
                Samples::PCM { ref samples } => assert!(samples.len() == 0),
                _ => panic!("Unexpected sample type while buffering"),
            }
        }

        // 3. Send enough data to cross 200ms threshold.
        // Need > 2200 bytes more.
        let chunk2 = Bytes::from(vec![1u8; 3000]);
        event_tx
            .send((None, Ok(SynthesisEvent::AudioChunk(chunk2))))
            .unwrap();

        // Wait for audio
        let mut received_audio = false;
        let timeout = tokio::time::sleep(Duration::from_millis(1000));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                Some(frame) = packet_rx.recv() => {
                    match frame.samples {
                        Samples::PCM { samples } if samples.len() > 0 => {
                            received_audio = true;
                            break;
                        }
                        _ => {}
                    }
                }
                _ = &mut timeout => {
                    break;
                }
            }
        }
        assert!(
            received_audio,
            "Should have received audio after buffer full"
        );

        cancel_token.cancel();
    }

    #[tokio::test]
    async fn test_jitter_buffer_bypass() {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();
        let (session_event_tx, _session_event_rx) = broadcast::channel(10);

        let client = Box::new(MockClient::new(event_rx));
        let cancel_token = CancellationToken::new();

        let sample_rate = 8000;
        let ptime = Duration::from_millis(20);
        // Min buffer ~ 3200 bytes
        let min_buffer_size = (sample_rate as usize * 2 * 200) / 1000;

        let task = TtsTask {
            play_id: Some("test_bypass".to_string()),
            track_id: "track_bypass".to_string(),
            session_id: "session_bypass".to_string(),
            client,
            command_rx: cmd_rx,
            event_sender: session_event_tx,
            packet_sender: packet_tx,
            cancel_token: cancel_token.clone(),
            processor_chain: ProcessorChain::new(sample_rate),
            cache_enabled: false,
            sample_rate,
            ptime,
            cache_buffer: BytesMut::new(),
            emit_q: VecDeque::new(),
            metadatas: HashMap::new(),
            cur_seq: 0,
            streaming: true,
            graceful: Arc::new(AtomicBool::new(false)),
            ssrc: 5678,
            buffering_state: Some(Instant::now()),
            min_buffer_size,
            max_buffer_wait: Duration::from_secs(10),
        };

        tokio::spawn(async move {
            task.run().await.unwrap();
        });

        // 1. Send small chunk (1000 bytes) < threshold (3200)
        let chunk = Bytes::from(vec![0u8; 1000]);
        event_tx
            .send((None, Ok(SynthesisEvent::AudioChunk(chunk))))
            .unwrap();

        // 2. Send Finished event immediately (simulating short phrase or end of stream)
        // This sets 'finished' on the emit entry, which should trigger the bypass logic: "|| current_entry_finished"
        event_tx.send((None, Ok(SynthesisEvent::Finished))).unwrap();

        // 3. Expect audio to arrive immediately, NOT waiting for 3200 bytes or timeout
        let timeout = tokio::time::sleep(Duration::from_millis(50)); // Expect very fast response
        tokio::pin!(timeout);

        let mut received = false;
        loop {
            tokio::select! {
                Some(frame) = packet_rx.recv() => {
                    if let Samples::PCM { samples } = frame.samples {
                        if samples.len() > 0 {
                            received = true;
                            break;
                        }
                    }
                }
                _ = &mut timeout => break,
            }
        }

        assert!(
            received,
            "Jitter buffer should release immediately on Finish event for short phrases"
        );
        cancel_token.cancel();
    }

    #[tokio::test]
    async fn test_jitter_buffer_multi_entry_underrun() {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();
        let (session_event_tx, _session_event_rx) = broadcast::channel(10);

        let client = Box::new(MockClient::new(event_rx));
        let cancel_token = CancellationToken::new();

        let sample_rate = 8000;
        let ptime = Duration::from_millis(20);
        // Min buffer ~ 3200 bytes
        let min_buffer_size = (sample_rate as usize * 2 * 200) / 1000;

        let task = TtsTask {
            play_id: Some("test_multi".to_string()),
            track_id: "track_multi".to_string(),
            session_id: "session_multi".to_string(),
            client,
            command_rx: cmd_rx,
            event_sender: session_event_tx,
            packet_sender: packet_tx,
            cancel_token: cancel_token.clone(),
            processor_chain: ProcessorChain::new(sample_rate),
            cache_enabled: false,
            sample_rate,
            ptime,
            cache_buffer: BytesMut::new(),
            emit_q: VecDeque::new(),
            metadatas: HashMap::new(),
            cur_seq: 0,
            streaming: true,
            graceful: Arc::new(AtomicBool::new(false)),
            ssrc: 9999,
            buffering_state: Some(Instant::now()),
            min_buffer_size,
            max_buffer_wait: Duration::from_secs(10),
        };

        tokio::spawn(async move {
            task.run().await.unwrap();
        });

        // 1. Entry 0: Send some data, but NOT finished
        // sequence 0 audio
        let chunk0 = Bytes::from(vec![1u8; 1000]);
        event_tx
            .send((Some(0), Ok(SynthesisEvent::AudioChunk(chunk0))))
            .unwrap();

        // 2. Entry 1: Send a LOT of data (enough to trigger buffer release)
        // sequence 1 audio - simulating next sentence arriving
        let chunk1 = Bytes::from(vec![2u8; 4000]);
        event_tx
            .send((Some(1), Ok(SynthesisEvent::AudioChunk(chunk1))))
            .unwrap();

        // 3. The total buffer is 5000 > 3200. It SHOULD play.
        // However, if the bug exists:
        // - It will play Entry 0 (1000 bytes)
        // - Entry 0 runs out. Entry 0 is NOT finished.
        // - It detects "Underrun" on Entry 0.
        // - It goes back to buffering.
        // - Next tick, it sees total buffer (4000 bytes from Entry 1) > threshold.
        // - It releases buffer.
        // - It tries to play Entry 0 again. Still empty.
        // - Underrun again.
        // -> DEADLOCK loop. Entry 1 never gets played.

        let timeout = tokio::time::sleep(Duration::from_millis(500));
        tokio::pin!(timeout);

        let mut received_bytes = 0;
        loop {
            tokio::select! {
                Some(frame) = packet_rx.recv() => {
                    if let Samples::PCM { samples } = frame.samples {
                        received_bytes += samples.len() * 2; // 16bit
                    }
                }
                _ = &mut timeout => break,
            }
        }
        println!("Received bytes: {}", received_bytes);
        // Ideally we should receive everything (5000 bytes) or at least progress to seq 1
        // If bug exists, we might only get chunk0 (1000 bytes) and then get stuck.
        cancel_token.cancel();
    }

    #[tokio::test]
    async fn test_jitter_buffer_streaming_append() {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();
        let (session_event_tx, _session_event_rx) = broadcast::channel(10);

        let client = Box::new(MockClient::new(event_rx));
        let cancel_token = CancellationToken::new();

        let sample_rate = 8000;
        let ptime = Duration::from_millis(20);
        let min_buffer_size = (sample_rate as usize * 2 * 200) / 1000; // ~320 bytes

        let task = TtsTask {
            play_id: Some("test_stream".to_string()),
            track_id: "track_stream".to_string(),
            session_id: "session_stream".to_string(),
            client,
            command_rx: cmd_rx,
            event_sender: session_event_tx,
            packet_sender: packet_tx,
            cancel_token: cancel_token.clone(),
            processor_chain: ProcessorChain::new(sample_rate),
            cache_enabled: false,
            sample_rate,
            ptime,
            cache_buffer: BytesMut::new(),
            emit_q: VecDeque::new(),
            metadatas: HashMap::new(),
            cur_seq: 0,
            streaming: true,
            graceful: Arc::new(AtomicBool::new(false)),
            ssrc: 1111,
            buffering_state: Some(Instant::now()),
            min_buffer_size,
            max_buffer_wait: Duration::from_secs(10),
        };

        tokio::spawn(async move {
            task.run().await.unwrap();
        });

        // 1. Send Batch 1 (1000 bytes)
        event_tx
            .send((
                None,
                Ok(SynthesisEvent::AudioChunk(Bytes::from(vec![1u8; 1000]))),
            ))
            .unwrap();

        // 2. Mark Finished (Simulating first sentence complete)
        event_tx.send((None, Ok(SynthesisEvent::Finished))).unwrap();

        // 3. Send Batch 2 (4000 bytes) - Arriving AFTER finish
        // This simulates second sentence arriving in the same stream (Entry 0)
        tokio::time::sleep(Duration::from_millis(100)).await;
        event_tx
            .send((
                None,
                Ok(SynthesisEvent::AudioChunk(Bytes::from(vec![2u8; 4000]))),
            ))
            .unwrap();

        let timeout = tokio::time::sleep(Duration::from_millis(1000));
        tokio::pin!(timeout);

        let mut received_bytes = 0;
        loop {
            tokio::select! {
                Some(frame) = packet_rx.recv() => {
                    if let Samples::PCM { samples } = frame.samples {
                        received_bytes += samples.len() * 2;
                    }
                }
                _ = &mut timeout => break,
            }
        }

        // We expect ALL 5000 bytes to be played.
        println!("Received bytes: {}", received_bytes);
        assert!(
            received_bytes > 4000,
            "Should play appended data even after Finished event. Received: {}",
            received_bytes
        );

        cancel_token.cancel();
    }
}
