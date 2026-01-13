use anyhow::{Result, anyhow};
use audio_codec::{PcmBuf, samples_to_bytes};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
    u32,
};
use tokio::{
    fs::File,
    io::{AsyncSeekExt, AsyncWriteExt},
    select,
    sync::mpsc::UnboundedReceiver,
};
use tokio_stream::wrappers::IntervalStream;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::media::{AudioFrame, Samples};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecorderFormat {
    Wav,
    Pcm,
    Pcmu,
    Pcma,
    G722,
}

impl RecorderFormat {
    pub fn extension(&self) -> &'static str {
        "wav"
    }

    pub fn is_supported(&self) -> bool {
        true
    }

    pub fn effective(&self) -> RecorderFormat {
        *self
    }
}

impl Default for RecorderFormat {
    fn default() -> Self {
        RecorderFormat::Wav
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct RecorderOption {
    #[serde(default)]
    pub recorder_file: String,
    #[serde(default)]
    pub samplerate: u32,
    #[serde(default)]
    pub ptime: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<RecorderFormat>,
}

impl RecorderOption {
    pub fn new(recorder_file: String) -> Self {
        Self {
            recorder_file,
            ..Default::default()
        }
    }

    pub fn resolved_format(&self, default: RecorderFormat) -> RecorderFormat {
        self.format.unwrap_or(default).effective()
    }

    pub fn ensure_path_extension(&mut self, fallback_format: RecorderFormat) {
        let effective_format = self.format.unwrap_or(fallback_format).effective();
        self.format = Some(effective_format);

        if self.recorder_file.is_empty() {
            return;
        }

        let extension = effective_format.extension();
        if !self
            .recorder_file
            .to_lowercase()
            .ends_with(&format!(".{}", extension.to_lowercase()))
        {
            self.recorder_file = format!("{}.{}", self.recorder_file, extension);
        }
    }
}

impl Default for RecorderOption {
    fn default() -> Self {
        Self {
            recorder_file: "".to_string(),
            samplerate: 16000,
            ptime: 200,
            format: None,
        }
    }
}

pub struct Recorder {
    session_id: String,
    option: RecorderOption,
    samples_written: AtomicUsize,
    cancel_token: CancellationToken,
    channel_idx: AtomicUsize,
    channels: Mutex<HashMap<String, usize>>,
    stereo_buf: Mutex<PcmBuf>,
    mono_buf: Mutex<PcmBuf>,
}

impl Recorder {
    pub fn new(
        cancel_token: CancellationToken,
        session_id: String,
        option: RecorderOption,
    ) -> Self {
        Self {
            session_id,
            option,
            samples_written: AtomicUsize::new(0),
            cancel_token,
            channel_idx: AtomicUsize::new(0),
            channels: Mutex::new(HashMap::new()),
            stereo_buf: Mutex::new(Vec::new()),
            mono_buf: Mutex::new(Vec::new()),
        }
    }

    async fn update_wav_header(&self, file: &mut File, payload_type: Option<u8>) -> Result<()> {
        let total = self.samples_written.load(Ordering::SeqCst);

        let (format_tag, sample_rate, channels, bits_per_sample, data_size): (
            u16,
            u32,
            u16,
            u16,
            usize,
        ) = match payload_type {
            Some(pt) => {
                let (tag, rate, chan): (u16, u32, u16) = match pt {
                    0 => (0x0007, 8000, 1),   // PCMU
                    8 => (0x0006, 8000, 1),   // PCMA
                    9 => (0x0064, 16000, 1),  // G722
                    10 => (0x0001, 44100, 2), // L16 Stereo 44.1k
                    11 => (0x0001, 44100, 1), // L16 Mono 44.1k
                    _ => (0x0001, 16000, 1),  // Default to PCM 16k Mono
                };
                let bits: u16 = match pt {
                    9 => 4,
                    0 | 8 => 8,
                    _ => 16,
                };
                (tag, rate, chan, bits, total)
            }
            None => (0x0001, self.option.samplerate, 2, 16, total),
        };

        let mut header_buf = Vec::new();
        header_buf.extend_from_slice(b"RIFF");
        let file_size = data_size + 36;
        header_buf.extend_from_slice(&(file_size as u32).to_le_bytes());
        header_buf.extend_from_slice(b"WAVE");

        header_buf.extend_from_slice(b"fmt ");
        header_buf.extend_from_slice(&16u32.to_le_bytes());
        header_buf.extend_from_slice(&format_tag.to_le_bytes());
        header_buf.extend_from_slice(&(channels as u16).to_le_bytes());
        header_buf.extend_from_slice(&sample_rate.to_le_bytes());

        let bytes_per_sec: u32 = match format_tag {
            0x0064 => 8000, // G.722 is 64kbps
            _ => sample_rate * (channels as u32) * (bits_per_sample as u32 / 8),
        };
        header_buf.extend_from_slice(&bytes_per_sec.to_le_bytes());

        let block_align: u16 = match format_tag {
            0x0064 | 0x0007 | 0x0006 => 1 * channels,
            _ => (bits_per_sample / 8) * channels,
        };
        header_buf.extend_from_slice(&block_align.to_le_bytes());
        header_buf.extend_from_slice(&bits_per_sample.to_le_bytes());

        header_buf.extend_from_slice(b"data");
        header_buf.extend_from_slice(&(data_size as u32).to_le_bytes());

        file.seek(std::io::SeekFrom::Start(0)).await?;
        file.write_all(&header_buf).await?;
        file.seek(std::io::SeekFrom::End(0)).await?;

        Ok(())
    }

    pub async fn process_recording(
        &self,
        file_path: &Path,
        mut receiver: UnboundedReceiver<AudioFrame>,
    ) -> Result<()> {
        let first_frame = match receiver.recv().await {
            Some(f) => f,
            None => return Ok(()),
        };

        if let Samples::RTP { .. } = first_frame.samples {
            return self
                .process_recording_rtp(file_path, receiver, first_frame)
                .await;
        }

        let _requested_format = self.option.format.unwrap_or(RecorderFormat::Wav);

        self.process_recording_wav(file_path, receiver, first_frame)
            .await
    }

    fn ensure_parent_dir(&self, file_path: &Path) -> Result<()> {
        if let Some(parent) = file_path.parent() {
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    warn!(
                        "Failed to create recording file parent directory: {} {}",
                        e,
                        file_path.display()
                    );
                    return Err(anyhow!("Failed to create recording file parent directory"));
                }
            }
        }
        Ok(())
    }

    async fn create_output_file(&self, file_path: &Path) -> Result<File> {
        self.ensure_parent_dir(file_path)?;
        match File::create(file_path).await {
            Ok(file) => {
                info!(
                    session_id = self.session_id,
                    "recorder: created recording file: {}",
                    file_path.display()
                );
                Ok(file)
            }
            Err(e) => {
                warn!(
                    "Failed to create recording file: {} {}",
                    e,
                    file_path.display()
                );
                Err(anyhow!("Failed to create recording file"))
            }
        }
    }

    async fn process_recording_rtp(
        &self,
        file_path: &Path,
        mut receiver: UnboundedReceiver<AudioFrame>,
        first_frame: AudioFrame,
    ) -> Result<()> {
        let (payload_type, mut file) =
            if let Samples::RTP { payload_type, .. } = &first_frame.samples {
                let file = self.create_output_file(file_path).await?;
                (*payload_type, file)
            } else {
                return Err(anyhow!("Invalid frame type for RTP recording"));
            };

        self.update_wav_header(&mut file, Some(payload_type))
            .await?;

        if let Samples::RTP { payload, .. } = first_frame.samples {
            file.write_all(&payload).await?;
            self.samples_written
                .fetch_add(payload.len(), Ordering::SeqCst);
        }

        loop {
            match receiver.recv().await {
                Some(frame) => {
                    if let Samples::RTP { payload, .. } = frame.samples {
                        file.write_all(&payload).await?;
                        self.samples_written
                            .fetch_add(payload.len(), Ordering::SeqCst);
                    }
                }
                None => break,
            }
        }

        self.update_wav_header(&mut file, Some(payload_type))
            .await?;

        file.sync_all().await?;

        Ok(())
    }

    async fn process_recording_wav(
        &self,
        file_path: &Path,
        mut receiver: UnboundedReceiver<AudioFrame>,
        first_frame: AudioFrame,
    ) -> Result<()> {
        let mut file = self.create_output_file(file_path).await?;
        self.update_wav_header(&mut file, None).await?;

        self.append_frame(first_frame).await.ok();

        let chunk_size = (self.option.samplerate / 1000 * self.option.ptime) as usize;
        info!(
            session_id = self.session_id,
            format = "wav",
            "Recording to {} ptime: {}ms chunk_size: {}",
            file_path.display(),
            self.option.ptime,
            chunk_size
        );

        let mut interval = IntervalStream::new(tokio::time::interval(Duration::from_millis(
            self.option.ptime as u64,
        )));
        loop {
            select! {
                Some(frame) = receiver.recv() => {
                    self.append_frame(frame).await.ok();
                }
                _ = interval.next() => {
                    let (mono_buf, stereo_buf) = self.pop(chunk_size).await;
                    self.process_buffers(&mut file, mono_buf, stereo_buf).await?;
                    self.update_wav_header(&mut file, None).await?;
                }
                _ = self.cancel_token.cancelled() => {
                    self.flush_buffers(&mut file).await?;
                    self.update_wav_header(&mut file, None).await?;
                    return Ok(());
                }
            }
        }
    }

    fn get_channel_index(&self, track_id: &str) -> usize {
        let mut channels = self.channels.lock().unwrap();
        if let Some(&channel_idx) = channels.get(track_id) {
            channel_idx % 2
        } else {
            let new_idx = self.channel_idx.fetch_add(1, Ordering::SeqCst);
            channels.insert(track_id.to_string(), new_idx);
            info!(
                session_id = self.session_id,
                "Assigned channel {} to track: {}",
                new_idx % 2,
                track_id
            );
            new_idx % 2
        }
    }

    async fn append_frame(&self, frame: AudioFrame) -> Result<()> {
        let buffer = match frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Ok(()), // ignore non-PCM frames
        };

        if buffer.is_empty() {
            return Ok(());
        }

        let channel_idx = self.get_channel_index(&frame.track_id);
        match channel_idx {
            0 => {
                let mut mono_buf = self.mono_buf.lock().unwrap();
                mono_buf.extend(buffer.iter());
            }
            1 => {
                let mut stereo_buf = self.stereo_buf.lock().unwrap();
                stereo_buf.extend(buffer.iter());
            }
            _ => {}
        }

        Ok(())
    }

    pub(crate) fn extract_samples(buffer: &mut PcmBuf, extract_size: usize) -> PcmBuf {
        if extract_size > 0 && !buffer.is_empty() {
            let take_size = extract_size.min(buffer.len());
            buffer.drain(..take_size).collect()
        } else {
            Vec::new()
        }
    }

    async fn pop(&self, chunk_size: usize) -> (PcmBuf, PcmBuf) {
        let mut mono_buf = self.mono_buf.lock().unwrap();
        let mut stereo_buf = self.stereo_buf.lock().unwrap();

        let safe_chunk_size = chunk_size.min(16000 * 10);

        let mono_result = if mono_buf.len() >= safe_chunk_size {
            Self::extract_samples(&mut mono_buf, safe_chunk_size)
        } else if !mono_buf.is_empty() {
            let available_len = mono_buf.len();
            let mut result = Self::extract_samples(&mut mono_buf, available_len);
            if chunk_size != usize::MAX {
                result.resize(safe_chunk_size, 0);
            }
            result
        } else {
            if chunk_size != usize::MAX {
                vec![0; safe_chunk_size]
            } else {
                Vec::new()
            }
        };

        let stereo_result = if stereo_buf.len() >= safe_chunk_size {
            Self::extract_samples(&mut stereo_buf, safe_chunk_size)
        } else if !stereo_buf.is_empty() {
            let available_len = stereo_buf.len();
            let mut result = Self::extract_samples(&mut stereo_buf, available_len);
            if chunk_size != usize::MAX {
                result.resize(safe_chunk_size, 0);
            }
            result
        } else {
            if chunk_size != usize::MAX {
                vec![0; safe_chunk_size]
            } else {
                Vec::new()
            }
        };

        if chunk_size == usize::MAX {
            let max_len = mono_result.len().max(stereo_result.len());
            let mut mono_final = mono_result;
            let mut stereo_final = stereo_result;
            mono_final.resize(max_len, 0);
            stereo_final.resize(max_len, 0);
            (mono_final, stereo_final)
        } else {
            (mono_result, stereo_result)
        }
    }

    pub fn stop_recording(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    pub(crate) fn mix_buffers(mono_buf: &PcmBuf, stereo_buf: &PcmBuf) -> Vec<i16> {
        assert_eq!(
            mono_buf.len(),
            stereo_buf.len(),
            "Buffer lengths must be equal after pop()"
        );

        let len = mono_buf.len();
        let mut mix_buff = Vec::with_capacity(len * 2);

        for i in 0..len {
            mix_buff.push(mono_buf[i]);
            mix_buff.push(stereo_buf[i]);
        }

        mix_buff
    }

    async fn write_audio_data(
        &self,
        file: &mut File,
        mono_buf: &PcmBuf,
        stereo_buf: &PcmBuf,
    ) -> Result<usize> {
        let max_len = mono_buf.len().max(stereo_buf.len());
        if max_len == 0 {
            return Ok(0);
        }

        let mix_buff = Self::mix_buffers(mono_buf, stereo_buf);

        file.seek(std::io::SeekFrom::End(0)).await?;
        file.write_all(&samples_to_bytes(&mix_buff)).await?;

        Ok(max_len)
    }

    async fn process_buffers(
        &self,
        file: &mut File,
        mono_buf: PcmBuf,
        stereo_buf: PcmBuf,
    ) -> Result<()> {
        if mono_buf.is_empty() && stereo_buf.is_empty() {
            return Ok(());
        }
        let samples_written = self.write_audio_data(file, &mono_buf, &stereo_buf).await?;
        if samples_written > 0 {
            self.samples_written
                .fetch_add(samples_written * 4, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn flush_buffers(&self, file: &mut File) -> Result<()> {
        loop {
            let (mono_buf, stereo_buf) = self.pop(usize::MAX).await;

            if mono_buf.is_empty() && stereo_buf.is_empty() {
                break;
            }

            let samples_written = self.write_audio_data(file, &mono_buf, &stereo_buf).await?;
            if samples_written > 0 {
                self.samples_written
                    .fetch_add(samples_written * 4, Ordering::SeqCst);
            }
        }

        Ok(())
    }
}
