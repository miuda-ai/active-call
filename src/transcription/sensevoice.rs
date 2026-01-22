use crate::event::EventSender;
use crate::event::SessionEvent;
use crate::media::vad::{TinySilero, VADOption, VadEngine};
use crate::media::{AudioFrame, INTERNAL_SAMPLERATE, Sample, Samples, TrackId};
use crate::offline::get_offline_models;
use crate::offline::sensevoice::{FeaturePipeline, FrontendConfig, language_id_from_code};
use crate::transcription::{TranscriptionClient, TranscriptionOption};
use anyhow::{Result, anyhow};
use audio_codec::Resampler;
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

type TranscriptionClientFuture =
    Pin<Box<dyn Future<Output = Result<Box<dyn TranscriptionClient>>> + Send>>;

struct SensevoiceAsrClientInner {
    audio_tx: mpsc::UnboundedSender<Vec<Sample>>,
    _option: TranscriptionOption,
}

pub struct SensevoiceAsrClient {
    inner: Arc<SensevoiceAsrClientInner>,
}

pub struct SensevoiceAsrClientBuilder;

impl SensevoiceAsrClientBuilder {
    pub fn create(
        track_id: TrackId,
        _token: CancellationToken,
        option: TranscriptionOption,
        event_sender: EventSender,
    ) -> TranscriptionClientFuture {
        Box::pin(async move {
            // Ensure offline models are initialized
            if get_offline_models().is_none() {
                return Err(anyhow!(
                    "Offline models not initialized. Please initialize with init_offline_models()"
                ));
            }

            let models = get_offline_models().unwrap();

            // Initialize SenseVoice if not already initialized
            models.init_sensevoice().await?;

            let (audio_tx, audio_rx) = mpsc::unbounded_channel::<Vec<Sample>>();

            let language = option
                .language
                .clone()
                .unwrap_or_else(|| "auto".to_string());
            let lang_id = language_id_from_code(&language);
            let track_id_clone = track_id.clone();
            // Default input sample rate to 8000Hz (standard SIP) if not specified
            let input_rate = option.samplerate.unwrap_or(8000);

            let mut vad_option = VADOption::default();
            vad_option.samplerate = INTERNAL_SAMPLERATE;

            if let Some(extra) = &option.extra {
                if let Some(v) = extra
                    .get("vad_voice_threshold")
                    .and_then(|s| s.parse::<f32>().ok())
                {
                    vad_option.voice_threshold = v;
                }
                if let Some(v) = extra
                    .get("vad_speech_padding")
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    vad_option.speech_padding = v;
                }
                if let Some(v) = extra
                    .get("vad_silence_padding")
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    vad_option.silence_padding = v;
                }
                if let Some(v) = extra
                    .get("vad_max_buffer_duration_secs")
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    vad_option.max_buffer_duration_secs = v;
                }
            }

            // Spawn audio processing task
            tokio::spawn(process_stream(
                audio_rx,
                track_id_clone,
                lang_id,
                event_sender,
                input_rate,
                vad_option,
            ));

            let inner = SensevoiceAsrClientInner {
                audio_tx,
                _option: option,
            };

            Ok(Box::new(SensevoiceAsrClient {
                inner: Arc::new(inner),
            }) as Box<dyn TranscriptionClient>)
        })
    }
}

async fn process_stream(
    mut audio_rx: mpsc::UnboundedReceiver<Vec<Sample>>,
    track_id: TrackId,
    lang_id: i32,
    event_sender: EventSender,
    input_rate: u32,
    vad_option: VADOption,
) {
    // Buffer for accumulating audio samples (target rate 16000)
    let mut buffer: Vec<i16> = Vec::with_capacity(16000 * 10);

    // VAD/Segmentation parameters
    let sample_rate: usize = INTERNAL_SAMPLERATE as usize;
    let max_segment_samples = vad_option.max_buffer_duration_secs as usize * sample_rate;
    let min_silence_samples =
        (vad_option.silence_padding as f32 * sample_rate as f32 / 1000.0) as usize;
    let min_speech_samples =
        (vad_option.speech_padding as f32 * sample_rate as f32 / 1000.0) as usize;

    // Resampler setup
    let mut resampler = if input_rate != sample_rate as u32 {
        Some(Resampler::new(input_rate as usize, sample_rate))
    } else {
        None
    };

    let mut vad = match TinySilero::new(vad_option.clone()) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "Failed to create TinySilero VAD");
            return;
        }
    };

    let mut current_timestamp = crate::media::get_timestamp();
    let mut speaking_started = false;
    let mut silence_samples = 0;

    // Create frontend feature pipeline
    let mut frontend = FeaturePipeline::new(FrontendConfig::default());

    debug!(track_id = %track_id, "SenseVoice processing loop started");

    while let Some(samples) = audio_rx.recv().await {
        // Resample or pass through
        let processed_samples = if let Some(resampler) = &mut resampler {
            resampler.resample(&samples)
        } else {
            samples
        };

        if processed_samples.is_empty() {
            continue;
        }

        buffer.extend_from_slice(&processed_samples);

        // Update VAD
        let mut frame = AudioFrame {
            track_id: track_id.clone(),
            samples: Samples::PCM {
                samples: processed_samples.clone(),
            },
            timestamp: current_timestamp,
            sample_rate: sample_rate as u32,
            channels: 1,
        };

        let vad_results = vad.process(&mut frame);
        for (is_voice, _ts) in vad_results {
            if is_voice {
                speaking_started = true;
                silence_samples = 0;
            } else if speaking_started {
                silence_samples += 512; // TinySilero CHUNK_SIZE
            }
        }

        current_timestamp += (processed_samples.len() as u64 * 1000) / sample_rate as u64;

        let should_transcribe = (speaking_started && silence_samples >= min_silence_samples)
            || (buffer.len() >= max_segment_samples);

        if !should_transcribe {
            continue;
        }

        if buffer.len() < min_speech_samples && buffer.len() < max_segment_samples {
            continue;
        }

        // Take all
        let segment_i16: Vec<i16> = buffer.drain(..).collect();

        // Reset VAD state for next segment
        speaking_started = false;
        silence_samples = 0;

        let models = match get_offline_models() {
            Some(m) => m,
            None => {
                continue;
            }
        };

        // Convert i16 to f32 for feature extraction
        let segment_f32: Vec<f32> = segment_i16.iter().map(|&x| x as f32 / 32768.0).collect();

        let feats = match frontend.compute_features(&segment_f32, sample_rate as u32) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "Feature extraction failed");
                continue;
            }
        };

        let encoder_lock = models.get_sensevoice().await.unwrap();
        let mut encoder_guard = encoder_lock.write().await;
        if let Some(encoder) = encoder_guard.as_mut() {
            let feats = feats.insert_axis(ndarray::Axis(0));
            // Run inference
            let start_time = std::time::Instant::now();
            match encoder.run_and_decode(feats.view(), lang_id, true) {
                Ok(text) => {
                    let clean_text = text.trim();
                    if !clean_text.is_empty() {
                        info!(track_id = %track_id, text = %clean_text, elapsed_ms = %start_time.elapsed().as_millis(),
                             "SenseVoice transcription");
                        // Send event
                        let event = SessionEvent::AsrFinal {
                            track_id: track_id.clone(),
                            index: 0,
                            text: clean_text.to_string(),
                            timestamp: crate::media::get_timestamp(),
                            start_time: None,
                            end_time: None,
                            is_filler: None,
                            confidence: Some(1.0),
                        };

                        if let Err(e) = event_sender.send(event) {
                            warn!(error = %e, "Failed to send transcription event");
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "SenseVoice inference failed");
                }
            }
        }
    }
}

impl TranscriptionClient for SensevoiceAsrClient {
    fn send_audio(&self, samples: &[Sample]) -> Result<()> {
        self.inner
            .audio_tx
            .send(samples.to_vec())
            .map_err(|_| anyhow!("Failed to send audio data"))?;
        Ok(())
    }
}
