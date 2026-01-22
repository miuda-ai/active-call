use super::{SynthesisClient, SynthesisOption, SynthesisType};
use crate::synthesis::SynthesisEvent;
use anyhow::Result;
use async_trait::async_trait;
use audio_codec::Resampler;
use bytes::Bytes;
use futures::{
    StreamExt,
    stream::{self, BoxStream},
};
use msedge_tts::tts::SpeechConfig;
use msedge_tts::tts::client::connect_async;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

pub struct MsEdgeTtsClient {
    option: SynthesisOption,
    tx: Option<mpsc::UnboundedSender<(String, Option<usize>, Option<SynthesisOption>)>>,
}

impl MsEdgeTtsClient {
    pub fn create(_streaming: bool, option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        Ok(Box::new(Self {
            option: option.clone(),
            tx: None,
        }))
    }

    pub fn new(option: SynthesisOption) -> Self {
        Self { option, tx: None }
    }
}

#[async_trait]
impl SynthesisClient for MsEdgeTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::MsEdge
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.tx = Some(tx);
        let client_option = self.option.clone();

        let stream = UnboundedReceiverStream::new(rx)
            .then(move |(text, seq, option)| {
                let current_option = client_option.merge_with(option);
                let text = text.clone();

                async move {
                    let result = async {
                        let mut tts = connect_async().await.map_err(|e| {
                            anyhow::anyhow!("Failed to connect to MsEdge TTS: {}", e)
                        })?;

                        let voice_name = current_option
                            .speaker
                            .clone()
                            .unwrap_or_else(|| "zh-CN-XiaoxiaoNeural".to_string());

                        let rate = current_option
                            .speed
                            .map(|s| ((s - 1.0) * 100.0) as i32)
                            .unwrap_or(0);

                        let config = SpeechConfig {
                            voice_name,
                            audio_format: "audio-24khz-48kbitrate-mono-mp3".to_string(),
                            pitch: 0,
                            rate,
                            volume: 0,
                        };

                        tts.synthesize(&text, &config)
                            .await
                            .map(|audio| (audio, current_option))
                            .map_err(|e| anyhow::anyhow!("MsEdge TTS error: {}", e))
                    }
                    .await;

                    match result {
                        Ok((audio, current_option)) => {
                            let audio_bytes = audio.audio_bytes;
                            let mut samples = Vec::new();
                            let mut sample_rate = 0;
                            let mut decoder = rmp3::Decoder::new(&audio_bytes);

                            while let Some(frame) = decoder.next() {
                                if let rmp3::Frame::Audio(audio) = frame {
                                    if sample_rate == 0 {
                                        sample_rate = audio.sample_rate();
                                    }
                                    samples.extend_from_slice(audio.samples());
                                }
                            }

                            if !samples.is_empty() {
                                let target_rate = current_option.samplerate.unwrap_or(16000) as u32;
                                if sample_rate > 0 && sample_rate != target_rate {
                                    let mut resampler =
                                        Resampler::new(sample_rate as usize, target_rate as usize);
                                    samples = resampler.resample(&samples);
                                }

                                let mut pcm_bytes = Vec::with_capacity(samples.len() * 2);
                                for s in samples {
                                    pcm_bytes.extend_from_slice(&s.to_le_bytes());
                                }

                                let events = vec![
                                    Ok(SynthesisEvent::AudioChunk(Bytes::from(pcm_bytes))),
                                    Ok(SynthesisEvent::Finished),
                                ];
                                (seq, stream::iter(events).boxed())
                            } else {
                                let events = vec![Ok(SynthesisEvent::Finished)];
                                (seq, stream::iter(events).boxed())
                            }
                        }
                        Err(e) => (seq, stream::once(async move { Err(e) }).boxed()),
                    }
                }
            })
            .flat_map(|(seq, stream)| stream.map(move |event| (seq, event)))
            .boxed();

        Ok(stream)
    }

    async fn synthesize(
        &mut self,
        text: &str,
        cmd_seq: Option<usize>,
        option: Option<SynthesisOption>,
    ) -> Result<()> {
        if let Some(tx) = &self.tx {
            tx.send((text.to_string(), cmd_seq, option))?;
            Ok(())
        } else {
            Err(anyhow::anyhow!("MsEdge TTS: client not started"))
        }
    }

    async fn stop(&mut self) -> Result<()> {
        self.tx.take();
        Ok(())
    }
}
