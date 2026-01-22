use crate::offline::get_offline_models;
use crate::synthesis::{SynthesisClient, SynthesisEvent, SynthesisOption, SynthesisType};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use audio_codec::Resampler;
use bytes::Bytes;
use futures::stream::BoxStream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

pub struct SupertonicTtsClient {
    voice_style: String,
    speed: f32,
    target_rate: i32,
    tx: Option<mpsc::UnboundedSender<(Option<usize>, Result<SynthesisEvent>)>>,
    token: CancellationToken,
}

impl SupertonicTtsClient {
    pub fn create(_streaming: bool, option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let voice_style = option.speaker.clone().unwrap_or_else(|| "M1".to_string());
        let speed = option.speed.unwrap_or(1.0);
        let target_rate = option.samplerate.unwrap_or(16000);

        Ok(Box::new(Self {
            voice_style,
            speed,
            target_rate,
            tx: None,
            token: CancellationToken::new(),
        }))
    }

    fn ensure_models_initialized() -> Result<()> {
        if get_offline_models().is_none() {
            anyhow::bail!(
                "Offline models not initialized. Please call init_offline_models() first."
            );
        }
        Ok(())
    }

    async fn synthesize_text(&self, text: String, cmd_seq: Option<usize>) -> Result<()> {
        let Some(tx) = self.tx.as_ref() else {
            return Ok(());
        };

        let models =
            get_offline_models().ok_or_else(|| anyhow!("offline models not initialized"))?;

        let voice_style = self.voice_style.clone();
        let speed = self.speed;
        let target_rate = self.target_rate;
        let tx_clone = tx.clone();
        // Supertonic: en is hardcoded for now, or detect from text?
        let language = "en".to_string();

        let tts_arc = models.get_supertonic().await?;

        // Run synthesis in blocking task
        tokio::task::spawn_blocking(move || {
            // Need write lock to use synthesize if it takes &mut self
            // But if synthesize takes &mut self, blocking_write is needed.
            let mut guard = tts_arc.blocking_write();

            if let Some(tts) = guard.as_mut() {
                debug!(
                    text = %text,
                    voice = %voice_style,
                    speed = speed,
                    target_rate = target_rate,
                    "Calling Supertonic TTS synthesis"
                );

                match tts.synthesize(&text, &language, Some(&voice_style), Some(speed)) {
                    Ok(samples) => {
                        if !samples.is_empty() {
                            let mut samples_i16: Vec<i16> = samples
                                .iter()
                                .map(|&s| (s * 32768.0).max(-32768.0).min(32767.0) as i16)
                                .collect();

                            // Resample if needed
                            if tts.sample_rate() != target_rate {
                                let mut resampler = Resampler::new(
                                    tts.sample_rate() as usize,
                                    target_rate as usize,
                                );
                                samples_i16 = resampler.resample(&samples_i16);
                            }

                            // Convert i16 samples to PCM bytes
                            let mut bytes = Vec::with_capacity(samples_i16.len() * 2);
                            for s in samples_i16 {
                                bytes.extend_from_slice(&s.to_le_bytes());
                            }

                            // Send AudioChunk
                            let _ = tx_clone.send((
                                cmd_seq,
                                Ok(SynthesisEvent::AudioChunk(Bytes::from(bytes))),
                            ));

                            // Send Finished
                            let _ = tx_clone.send((cmd_seq, Ok(SynthesisEvent::Finished)));
                        } else {
                            warn!("Supertonic produced empty audio");
                            let _ = tx_clone.send((cmd_seq, Ok(SynthesisEvent::Finished)));
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Supertonic inference failed");
                        let _ = tx_clone.send((cmd_seq, Err(anyhow!("Synthesis failed: {}", e))));
                    }
                }
            } else {
                warn!("Supertonic TTS not initialized");
                let _ = tx_clone.send((cmd_seq, Err(anyhow!("TTS not initialized"))));
            }
        })
        .await
        .map_err(|e| anyhow!("task join error: {}", e))?;

        Ok(())
    }
}

#[async_trait]
impl SynthesisClient for SupertonicTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Supertonic
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        Self::ensure_models_initialized()?;

        let (tx, rx) = mpsc::unbounded_channel();
        self.tx = Some(tx);

        // Initialize TTS if needed
        let models =
            get_offline_models().ok_or_else(|| anyhow!("offline models not initialized"))?;
        models.init_supertonic().await?;

        debug!(
            "SupertonicTtsClient started with voice: {}",
            self.voice_style
        );

        Ok(Box::pin(UnboundedReceiverStream::new(rx)))
    }

    async fn synthesize(
        &mut self,
        text: &str,
        cmd_seq: Option<usize>,
        _option: Option<SynthesisOption>,
    ) -> Result<()> {
        self.synthesize_text(text.to_string(), cmd_seq).await
    }

    async fn stop(&mut self) -> Result<()> {
        self.token.cancel();
        self.tx = None;
        Ok(())
    }
}
