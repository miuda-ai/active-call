use super::{
    INTERNAL_SAMPLERATE,
    asr_processor::AsrProcessor,
    denoiser::NoiseReducer,
    processor::Processor,
    track::{
        Track, TrackPacketSender,
        tts::{SynthesisHandle, TtsTrack},
    },
    vad::{VADOption, VadProcessor, VadType},
};
use crate::{
    CallOption, EouOption,
    event::EventSender,
    media::TrackId,
    synthesis::{
        AliyunTtsClient, DeepegramTtsClient, SynthesisClient, SynthesisOption, SynthesisType,
        TencentCloudTtsBasicClient, TencentCloudTtsClient,
    },
    transcription::{
        AliyunAsrClientBuilder, TencentCloudAsrClientBuilder, TranscriptionClient,
        TranscriptionOption, TranscriptionType,
    },
};

#[cfg(feature = "offline")]
use crate::{synthesis::SupertonicTtsClient, transcription::SensevoiceAsrClientBuilder};

use anyhow::Result;
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

pub type FnCreateVadProcessor = fn(
    token: CancellationToken,
    event_sender: EventSender,
    option: VADOption,
) -> Result<Box<dyn Processor>>;

pub type FnCreateEouProcessor = fn(
    token: CancellationToken,
    event_sender: EventSender,
    option: EouOption,
) -> Result<Box<dyn Processor>>;

pub type FnCreateAsrClient = Box<
    dyn Fn(
            TrackId,
            CancellationToken,
            TranscriptionOption,
            EventSender,
        ) -> Pin<Box<dyn Future<Output = Result<Box<dyn TranscriptionClient>>> + Send>>
        + Send
        + Sync,
>;
pub type FnCreateTtsClient =
    fn(streaming: bool, option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>>;

// Define hook types
pub type CreateProcessorsHook = Box<
    dyn Fn(
            Arc<StreamEngine>,
            TrackId,
            CancellationToken,
            EventSender,
            TrackPacketSender,
            CallOption,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Box<dyn Processor>>>> + Send>>
        + Send
        + Sync,
>;

pub struct StreamEngine {
    vad_creators: HashMap<VadType, FnCreateVadProcessor>,
    eou_creators: HashMap<String, FnCreateEouProcessor>,
    asr_creators: HashMap<TranscriptionType, FnCreateAsrClient>,
    tts_creators: HashMap<SynthesisType, FnCreateTtsClient>,
    create_processors_hook: Arc<CreateProcessorsHook>,
}

impl Default for StreamEngine {
    fn default() -> Self {
        let mut engine = Self::new();
        engine.register_vad(VadType::Silero, VadProcessor::create);
        engine.register_vad(VadType::Other("nop".to_string()), VadProcessor::create_nop);

        engine.register_asr(
            TranscriptionType::TencentCloud,
            Box::new(TencentCloudAsrClientBuilder::create),
        );
        engine.register_asr(
            TranscriptionType::Aliyun,
            Box::new(AliyunAsrClientBuilder::create),
        );

        #[cfg(feature = "offline")]
        engine.register_asr(
            TranscriptionType::Sensevoice,
            Box::new(SensevoiceAsrClientBuilder::create),
        );

        engine.register_tts(SynthesisType::Aliyun, AliyunTtsClient::create);
        engine.register_tts(SynthesisType::TencentCloud, TencentCloudTtsClient::create);
        engine.register_tts(
            SynthesisType::Other("tencent_basic".to_string()),
            TencentCloudTtsBasicClient::create,
        );
        engine.register_tts(SynthesisType::Deepgram, DeepegramTtsClient::create);

        #[cfg(feature = "offline")]
        engine.register_tts(SynthesisType::Supertonic, SupertonicTtsClient::create);

        engine
    }
}

impl StreamEngine {
    pub fn new() -> Self {
        Self {
            vad_creators: HashMap::new(),
            asr_creators: HashMap::new(),
            tts_creators: HashMap::new(),
            eou_creators: HashMap::new(),
            create_processors_hook: Arc::new(Box::new(Self::default_create_procesors_hook)),
        }
    }

    pub fn register_vad(&mut self, vad_type: VadType, creator: FnCreateVadProcessor) -> &mut Self {
        self.vad_creators.insert(vad_type, creator);
        self
    }

    pub fn register_eou(&mut self, name: String, creator: FnCreateEouProcessor) -> &mut Self {
        self.eou_creators.insert(name, creator);
        self
    }

    pub fn register_asr(
        &mut self,
        asr_type: TranscriptionType,
        creator: FnCreateAsrClient,
    ) -> &mut Self {
        self.asr_creators.insert(asr_type, creator);
        self
    }

    pub fn register_tts(
        &mut self,
        tts_type: SynthesisType,
        creator: FnCreateTtsClient,
    ) -> &mut Self {
        self.tts_creators.insert(tts_type, creator);
        self
    }

    pub fn create_vad_processor(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        option: VADOption,
    ) -> Result<Box<dyn Processor>> {
        let creator = self.vad_creators.get(&option.r#type);
        if let Some(creator) = creator {
            creator(token, event_sender, option)
        } else {
            Err(anyhow::anyhow!("VAD type not found: {}", option.r#type))
        }
    }
    pub fn create_eou_processor(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        option: EouOption,
    ) -> Result<Box<dyn Processor>> {
        let creator = self
            .eou_creators
            .get(&option.r#type.clone().unwrap_or_default());
        if let Some(creator) = creator {
            creator(token, event_sender, option)
        } else {
            Err(anyhow::anyhow!("EOU type not found: {:?}", option.r#type))
        }
    }

    pub async fn create_asr_processor(
        &self,
        track_id: TrackId,
        cancel_token: CancellationToken,
        option: TranscriptionOption,
        event_sender: EventSender,
    ) -> Result<Box<dyn Processor>> {
        let asr_client = match option.provider {
            Some(ref provider) => {
                let creator = self.asr_creators.get(&provider);
                if let Some(creator) = creator {
                    creator(track_id, cancel_token, option, event_sender).await?
                } else {
                    return Err(anyhow::anyhow!("ASR type not found: {}", provider));
                }
            }
            None => return Err(anyhow::anyhow!("ASR type not found: {:?}", option.provider)),
        };
        Ok(Box::new(AsrProcessor { asr_client }))
    }

    pub async fn create_tts_client(
        &self,
        streaming: bool,
        tts_option: &SynthesisOption,
    ) -> Result<Box<dyn SynthesisClient>> {
        match tts_option.provider {
            Some(ref provider) => {
                let creator = self.tts_creators.get(&provider);
                if let Some(creator) = creator {
                    creator(streaming, tts_option)
                } else {
                    Err(anyhow::anyhow!("TTS type not found: {}", provider))
                }
            }
            None => Err(anyhow::anyhow!(
                "TTS type not found: {:?}",
                tts_option.provider
            )),
        }
    }

    pub async fn create_processors(
        engine: Arc<StreamEngine>,
        track: &dyn Track,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
        option: &CallOption,
    ) -> Result<Vec<Box<dyn Processor>>> {
        (engine.clone().create_processors_hook)(
            engine,
            track.id().clone(),
            cancel_token,
            event_sender,
            packet_sender,
            option.clone(),
        )
        .await
    }

    pub async fn create_tts_track(
        engine: Arc<StreamEngine>,
        cancel_token: CancellationToken,
        session_id: String,
        track_id: TrackId,
        ssrc: u32,
        play_id: Option<String>,
        streaming: bool,
        tts_option: &SynthesisOption,
    ) -> Result<(SynthesisHandle, Box<dyn Track>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let new_handle = SynthesisHandle::new(tx, play_id.clone(), ssrc);
        let tts_client = engine.create_tts_client(streaming, tts_option).await?;
        let sample_rate = tts_option.samplerate.unwrap_or(16000) as u32;
        let tts_track = TtsTrack::new(track_id, session_id, streaming, play_id, rx, tts_client)
            .with_ssrc(ssrc)
            .with_sample_rate(sample_rate)
            .with_cancel_token(cancel_token);
        Ok((new_handle, Box::new(tts_track) as Box<dyn Track>))
    }

    pub fn with_processor_hook(&mut self, hook_fn: CreateProcessorsHook) -> &mut Self {
        self.create_processors_hook = Arc::new(Box::new(hook_fn));
        self
    }

    pub fn default_create_procesors_hook(
        engine: Arc<StreamEngine>,
        track_id: TrackId,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
        option: CallOption,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Box<dyn Processor>>>> + Send>> {
        Box::pin(async move {
            let mut processors = vec![];
            debug!(%track_id, "Creating processors for track");

            if let Some(realtime_option) = option.realtime {
                debug!(%track_id, "Adding RealtimeProcessor");
                let realtime_processor = crate::media::realtime_processor::RealtimeProcessor::new(
                    track_id.clone(),
                    cancel_token.child_token(),
                    event_sender.clone(),
                    packet_sender.clone(),
                    realtime_option,
                )?;
                processors.push(Box::new(realtime_processor) as Box<dyn Processor>);
                // In realtime mode, we usually don't need separate VAD or ASR processors
                // as they are handled by the realtime service (OpenAI/Azure)
                return Ok(processors);
            }

            match option.denoise {
                Some(true) => {
                    debug!(%track_id, "Adding NoiseReducer processor");
                    let noise_reducer = NoiseReducer::new(INTERNAL_SAMPLERATE as usize);
                    processors.push(Box::new(noise_reducer) as Box<dyn Processor>);
                }
                _ => {}
            }
            match option.vad {
                Some(mut option) => {
                    debug!(%track_id, "Adding VadProcessor processor type={:?}", option.r#type);
                    option.samplerate = INTERNAL_SAMPLERATE;
                    let vad_processor: Box<dyn Processor + 'static> = engine.create_vad_processor(
                        cancel_token.child_token(),
                        event_sender.clone(),
                        option.to_owned(),
                    )?;
                    processors.push(vad_processor);
                }
                None => {}
            }
            match option.asr {
                Some(mut option) => {
                    debug!(%track_id, "Adding AsrProcessor processor provider={:?}", option.provider);
                    option.samplerate = Some(INTERNAL_SAMPLERATE);
                    let asr_processor = engine
                        .create_asr_processor(
                            track_id.clone(),
                            cancel_token.child_token(),
                            option.to_owned(),
                            event_sender.clone(),
                        )
                        .await?;
                    processors.push(asr_processor);
                }
                None => {}
            }
            match option.eou {
                Some(ref option) => {
                    let eou_processor = engine.create_eou_processor(
                        cancel_token.child_token(),
                        event_sender.clone(),
                        option.to_owned(),
                    )?;
                    processors.push(eou_processor);
                }
                None => {}
            }
            match option.inactivity_timeout {
                Some(timeout_secs) if timeout_secs > 0 => {
                    let inactivity_processor = crate::media::inactivity::InactivityProcessor::new(
                        track_id.clone(),
                        std::time::Duration::from_secs(timeout_secs),
                        event_sender.clone(),
                        cancel_token.child_token(),
                    );
                    processors.push(Box::new(inactivity_processor) as Box<dyn Processor>);
                }
                _ => {}
            }

            Ok(processors)
        })
    }
}
