use crate::CallOption;
use crate::call::ActiveCallRef;
use anyhow::{Result, anyhow};
use serde_json::json;
use tracing::{error, info, warn};

use super::{Playbook, PlaybookConfig, dialogue::DialogueHandler, handler::LlmHandler};

pub struct PlaybookRunner {
    handler: Box<dyn DialogueHandler>,
    call: ActiveCallRef,
    config: PlaybookConfig,
}

impl PlaybookRunner {
    pub fn with_handler(
        handler: Box<dyn DialogueHandler>,
        call: ActiveCallRef,
        config: PlaybookConfig,
    ) -> Self {
        Self {
            handler,
            call,
            config,
        }
    }

    pub fn new(playbook: Playbook, call: ActiveCallRef) -> Result<Self> {
        if let Ok(mut state) = call.call_state.try_write() {
            // Ensure option exists before applying config
            if state.option.is_none() {
                state.option = Some(CallOption::default());
            }
            if let Some(option) = state.option.as_mut() {
                apply_playbook_config(option, &playbook.config);
            }
        }

        let handler: Box<dyn DialogueHandler> = if let Some(llm_config) = &playbook.config.llm {
            let mut llm_config = llm_config.clone();
            if let Some(greeting) = playbook.config.greeting.clone() {
                llm_config.greeting = Some(greeting);
            }
            let interruption_config = playbook.config.interruption.clone().unwrap_or_default();
            let dtmf_config = playbook.config.dtmf.clone();

            let mut llm_handler = LlmHandler::new(
                llm_config,
                interruption_config,
                playbook.scenes.clone(),
                dtmf_config,
                playbook.initial_scene_id.clone(),
            );
            // Set event sender for debugging
            llm_handler.set_event_sender(call.event_sender.clone());
            llm_handler.set_call(call.clone());
            Box::new(llm_handler)
        } else {
            return Err(anyhow!(
                "No valid dialogue handler configuration found (e.g. missing 'llm')"
            ));
        };

        Ok(Self {
            handler,
            call,
            config: playbook.config,
        })
    }

    pub async fn run(mut self) {
        info!(
            "PlaybookRunner started for session {}",
            self.call.session_id
        );

        let mut event_receiver = self.call.event_sender.subscribe();

        if let Ok(commands) = self.handler.on_start().await {
            for cmd in commands {
                if let Err(e) = self.call.enqueue_command(cmd).await {
                    error!("Failed to enqueue start command: {}", e);
                }
            }
        }

        // Wait for call to be established before running playbook greeting
        let mut answered = false;
        {
            let state = self.call.call_state.read().await;
            if state.answer_time.is_some() {
                answered = true;
            }
        }

        if !answered {
            info!("Waiting for call establishment...");
            while let Ok(event) = event_receiver.recv().await {
                match &event {
                    crate::event::SessionEvent::Answer { .. } => {
                        info!("Call established, proceeding to playbook handles");
                        break;
                    }
                    crate::event::SessionEvent::Hangup { .. } => {
                        info!("Call hung up before established, stopping");
                        return;
                    }
                    _ => {}
                }
            }
        }

        while let Ok(event) = event_receiver.recv().await {
            match &event {
                crate::event::SessionEvent::AsrFinal { text, .. } => {
                    info!("User said: {}", text);
                }
                crate::event::SessionEvent::Hangup { .. } => {
                    info!("Call hung up, stopping playbook");
                    break;
                }
                _ => {}
            }

            if let Ok(commands) = self.handler.on_event(&event).await {
                for cmd in commands {
                    if let Err(e) = self.call.enqueue_command(cmd).await {
                        error!("Failed to enqueue command: {}", e);
                    }
                }
            }
        }

        // Post-hook logic
        if let Some(posthook) = self.config.posthook.clone() {
            let mut handler = self.handler;
            let call = self.call.clone();
            tokio::spawn(async move {
                info!("Executing posthook for session {}", call.session_id);

                let summary = if let Some(summary_type) = &posthook.summary {
                    match handler.summarize(summary_type.prompt()).await {
                        Ok(s) => Some(s),
                        Err(e) => {
                            error!("Failed to generate summary: {}", e);
                            None
                        }
                    }
                } else {
                    None
                };

                let history = if posthook.include_history.unwrap_or(true) {
                    Some(handler.get_history().await)
                } else {
                    None
                };

                let payload = json!({
                    "sessionId": call.session_id,
                    "summary": summary,
                    "history": history,
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                });

                let client = reqwest::Client::new();
                let method = posthook
                    .method
                    .as_deref()
                    .unwrap_or("POST")
                    .parse::<reqwest::Method>()
                    .unwrap_or(reqwest::Method::POST);

                let mut request = client.request(method, &posthook.url).json(&payload);

                if let Some(headers) = posthook.headers {
                    for (k, v) in headers {
                        request = request.header(k, v);
                    }
                }

                match request.send().await {
                    Ok(resp) => {
                        if resp.status().is_success() {
                            info!("Posthook sent successfully");
                        } else {
                            warn!("Posthook failed with status: {}", resp.status());
                        }
                    }
                    Err(e) => {
                        error!("Failed to send posthook: {}", e);
                    }
                }
            });
        }
    }
}

pub fn apply_playbook_config(option: &mut CallOption, config: &PlaybookConfig) {
    let api_key = config.llm.as_ref().and_then(|llm| llm.api_key.clone());

    if let Some(mut asr) = config.asr.clone() {
        if asr.secret_key.is_none() {
            asr.secret_key = api_key.clone();
        }
        option.asr = Some(asr);
    }
    if let Some(mut tts) = config.tts.clone() {
        if tts.secret_key.is_none() {
            tts.secret_key = api_key.clone();
        }
        option.tts = Some(tts);
    }
    if let Some(vad) = config.vad.clone() {
        option.vad = Some(vad);
    }
    if let Some(denoise) = config.denoise {
        option.denoise = Some(denoise);
    }
    if let Some(ambiance) = config.ambiance.clone() {
        option.ambiance = Some(ambiance);
    }
    if let Some(recorder) = config.recorder.clone() {
        option.recorder = Some(recorder);
    }
    if let Some(extra) = config.extra.clone() {
        option.extra = Some(extra);
    }
    if let Some(mut realtime) = config.realtime.clone() {
        if realtime.secret_key.is_none() {
            realtime.secret_key = api_key.clone();
        }
        option.realtime = Some(realtime);
    }
    if let Some(mut eou) = config.eou.clone() {
        if eou.secret_key.is_none() {
            eou.secret_key = api_key;
        }
        option.eou = Some(eou);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EouOption, media::recorder::RecorderOption, media::vad::VADOption,
        synthesis::SynthesisOption, transcription::TranscriptionOption,
    };
    use std::collections::HashMap;

    #[test]
    fn apply_playbook_config_sets_fields() {
        let mut option = CallOption::default();
        let mut extra = HashMap::new();
        extra.insert("k".to_string(), "v".to_string());

        let config = PlaybookConfig {
            asr: Some(TranscriptionOption::default()),
            tts: Some(SynthesisOption::default()),
            vad: Some(VADOption::default()),
            denoise: Some(true),
            recorder: Some(RecorderOption::default()),
            extra: Some(extra.clone()),
            eou: Some(EouOption {
                r#type: Some("test".to_string()),
                endpoint: None,
                secret_key: Some("key".to_string()),
                secret_id: Some("id".to_string()),
                timeout: Some(123),
                extra: None,
            }),
            ..Default::default()
        };

        apply_playbook_config(&mut option, &config);

        assert!(option.asr.is_some());
        assert!(option.tts.is_some());
        assert!(option.vad.is_some());
        assert_eq!(option.denoise, Some(true));
        assert!(option.recorder.is_some());
        assert_eq!(option.extra, Some(extra));
        assert!(option.eou.is_some());
    }

    #[test]
    fn apply_playbook_config_propagates_api_key() {
        let mut option = CallOption::default();
        let config = PlaybookConfig {
            llm: Some(super::super::LlmConfig {
                api_key: Some("test-key".to_string()),
                ..Default::default()
            }),
            asr: Some(TranscriptionOption::default()),
            tts: Some(SynthesisOption::default()),
            eou: Some(EouOption::default()),
            ..Default::default()
        };

        apply_playbook_config(&mut option, &config);

        assert_eq!(
            option.asr.as_ref().unwrap().secret_key,
            Some("test-key".to_string())
        );
        assert_eq!(
            option.tts.as_ref().unwrap().secret_key,
            Some("test-key".to_string())
        );
        assert_eq!(
            option.eou.as_ref().unwrap().secret_key,
            Some("test-key".to_string())
        );
    }
}
