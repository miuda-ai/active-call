use super::{SynthesisClient, SynthesisEvent, SynthesisOption, SynthesisType};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use base64::{Engine, prelude::BASE64_STANDARD};
use bytes::Bytes;
use futures::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use std::{io::Cursor, time::Duration};
use tokio::sync::mpsc;

const ENDPOINT: &str = "https://api.60db.ai/tts-synthesize";
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
type Command = (String, Option<usize>, SynthesisOption);

pub struct SixtyDBTtsClient {
    option: SynthesisOption,
    streaming: bool,
    client: reqwest::Client,
    tx: Option<mpsc::UnboundedSender<Command>>,
}

impl SixtyDBTtsClient {
    pub fn create(streaming: bool, option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let mut option = option.clone();
        option.check_default();
        validate(&option)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Box::new(Self {
            option,
            streaming,
            client,
            tx: None,
        }))
    }
}

fn validate(option: &SynthesisOption) -> Result<()> {
    ensure!(
        option
            .secret_key
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty()),
        "60db TTS requires an API key"
    );
    let voice = option
        .speaker
        .as_deref()
        .context("60db TTS requires a speaker from GET /voices")?;
    uuid::Uuid::parse_str(voice).context("60db TTS speaker must be a voice UUID")?;
    ensure!(
        matches!(option.samplerate.unwrap_or(16000), 16000 | 24000 | 48000),
        "60db TTS supports 16000, 24000 or 48000 Hz"
    );
    ensure!(
        matches!(option.codec.as_deref().unwrap_or("pcm"), "pcm" | "linear16"),
        "60db TTS requires PCM16 output"
    );
    let speed = option.speed.unwrap_or(1.0);
    ensure!(
        speed.is_finite() && (0.5..=2.0).contains(&speed),
        "60db TTS speed must be between 0.5 and 2.0"
    );
    let url = reqwest::Url::parse(option.endpoint.as_deref().unwrap_or(ENDPOINT))
        .context("Invalid 60db TTS endpoint")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none(),
        "Invalid 60db TTS endpoint"
    );
    Ok(())
}

fn decode_audio(encoded: &str, rate: i32) -> Result<Bytes> {
    let mut bytes = BASE64_STANDARD
        .decode(encoded)
        .context("Invalid 60db audio base64")?;
    if bytes.first() == Some(&b'{') {
        let inner: Value = serde_json::from_slice(&bytes).context("Invalid 60db audio wrapper")?;
        let audio = inner
            .get("result")
            .unwrap_or(&inner)
            .get("audioContent")
            .and_then(Value::as_str)
            .context("Missing 60db wrapped audio")?;
        bytes = BASE64_STANDARD
            .decode(audio)
            .context("Invalid 60db wrapped audio base64")?;
    }
    if bytes.starts_with(b"RIFF") {
        ensure!(bytes.len() >= 12, "Invalid 60db WAV header");
        let declared = u32::from_le_bytes(bytes[4..8].try_into()?) as usize;
        ensure!(
            declared.checked_add(8) == Some(bytes.len()),
            "Invalid 60db WAV length"
        );
        let mut wav = hound::WavReader::new(Cursor::new(bytes)).context("Invalid 60db WAV")?;
        let spec = wav.spec();
        ensure!(
            spec.channels == 1
                && spec.bits_per_sample == 16
                && spec.sample_format == hound::SampleFormat::Int
                && spec.sample_rate == rate as u32,
            "60db WAV must match mono PCM16 at the configured sample rate"
        );
        bytes = wav
            .samples::<i16>()
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect();
    }
    ensure!(
        !bytes.is_empty() && bytes.len() % 2 == 0,
        "60db returned invalid PCM16 audio"
    );
    ensure!(
        !bytes.starts_with(b"ID3") && !bytes.starts_with(b"OggS") && !bytes.starts_with(b"fLaC"),
        "60db returned compressed audio instead of PCM16"
    );
    Ok(Bytes::from(bytes))
}

fn parse_record(record: &[u8], rate: i32) -> Result<Option<Bytes>> {
    let value: Value = serde_json::from_slice(record).context("Invalid 60db JSON response")?;
    ensure!(
        value.get("success") != Some(&Value::Bool(false))
            && value.get("type").and_then(Value::as_str) != Some("error")
            && value.get("error").is_none_or(Value::is_null),
        "60db TTS synthesis failed"
    );
    let result = value
        .get("backendResponse")
        .or_else(|| value.get("result"))
        .unwrap_or(&value);
    ensure!(
        result.get("success") != Some(&Value::Bool(false))
            && result.get("error").is_none_or(Value::is_null),
        "60db TTS synthesis failed"
    );
    for metadata in [&value, result] {
        if let Some(actual) = metadata.get("sample_rate").and_then(Value::as_i64) {
            ensure!(
                actual == rate as i64,
                "60db returned an unexpected sample rate"
            );
        }
        for key in ["encoding", "output_format"] {
            if let Some(encoding) = metadata.get(key).and_then(Value::as_str) {
                ensure!(
                    matches!(
                        encoding.to_ascii_lowercase().as_str(),
                        "linear16" | "pcm" | "pcm16" | "wav"
                    ),
                    "60db returned an unsupported audio format"
                );
            }
        }
    }
    match result
        .get("audioContent")
        .or_else(|| result.get("audio_base64"))
    {
        Some(Value::String(audio)) if !audio.is_empty() => Ok(Some(decode_audio(audio, rate)?)),
        Some(Value::String(_)) | None => Ok(None),
        _ => bail!("Invalid 60db audio field"),
    }
}

fn check(condition: bool, message: &str) -> Result<()> {
    ensure!(condition, "{}", message);
    Ok(())
}

fn audio_stream(
    client: reqwest::Client,
    option: SynthesisOption,
    text: String,
) -> BoxStream<'static, Result<SynthesisEvent>> {
    Box::pin(async_stream::try_stream! {
        validate(&option)?;
        check(!text.trim().is_empty() && text.chars().count() <= 5000, "60db TTS text must contain 1 to 5000 characters")?;
        let rate = option.samplerate.unwrap_or(16000);
        let mut payload = json!({
            "text": text,
            "voice_id": option.speaker,
            "audio_config": {"audio_encoding": "LINEAR16", "sample_rate_hertz": rate},
            "speed": option.speed.unwrap_or(1.0),
            "timestamp_type": "NONE"
        });
        if let Some(language) = &option.language { payload["target_language"] = json!(language); }
        let response = client.post(option.endpoint.as_deref().unwrap_or(ENDPOINT))
            .bearer_auth(option.secret_key.as_deref().unwrap_or_default())
            .json(&payload).send().await.map_err(|_| anyhow::anyhow!("60db TTS request failed"))?;
        check(response.status().is_success(), &format!("60db TTS returned HTTP {}", response.status().as_u16()))?;
        let content_type = response.headers().get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        let ndjson = content_type.contains("ndjson") || content_type.contains("jsonl");
        let binary = content_type.contains("audio/wav") || content_type.contains("audio/x-wav") || content_type.contains("audio/pcm") || content_type.contains("application/octet-stream");
        check(ndjson || binary || content_type.contains("json"), "60db TTS returned an unsupported content type")?;
        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut total = 0usize;
        let mut emitted = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| anyhow::anyhow!("60db TTS response interrupted"))?;
            total = total.checked_add(chunk.len()).context("60db response size overflow")?;
            check(total <= MAX_RESPONSE_BYTES, "60db TTS response exceeds size limit")?;
            pending.extend_from_slice(&chunk);
            if ndjson {
                while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                    let record: Vec<_> = pending.drain(..=end).collect();
                    if record.iter().all(u8::is_ascii_whitespace) { continue; }
                    if let Some(audio) = parse_record(&record, rate)? {
                        emitted = true;
                        yield SynthesisEvent::AudioChunk(audio);
                    }
                }
            }
        }
        if binary {
            check(!pending.is_empty(), "60db TTS returned no audio")?;
            let audio = decode_audio(&BASE64_STANDARD.encode(&pending), rate)?;
            emitted = true;
            yield SynthesisEvent::AudioChunk(audio);
        } else {
            let audio = if pending.iter().all(u8::is_ascii_whitespace) {
                None
            } else {
                parse_record(&pending, rate)?
            };
            if let Some(audio) = audio {
                emitted = true;
                yield SynthesisEvent::AudioChunk(audio);
            }
        }
        check(emitted, "60db TTS returned no audio")?;
    })
}

#[async_trait]
impl SynthesisClient for SixtyDBTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::SixtyDB
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        ensure!(self.tx.is_none(), "60db TTS client is already started");
        let (tx, mut rx) = mpsc::unbounded_channel::<Command>();
        self.tx = Some(tx);
        let client = self.client.clone();
        let streaming = self.streaming;
        Ok(Box::pin(async_stream::stream! {
            let mut failed = false;
            while let Some((text, seq, option)) = rx.recv().await {
                let mut audio = audio_stream(client.clone(), option, text);
                let mut command_failed = false;
                while let Some(event) = audio.next().await {
                    if event.is_err() { command_failed = true; failed = true; }
                    yield (seq, event);
                }
                if !streaming && !command_failed { yield (seq, Ok(SynthesisEvent::Finished)); }
            }
            if streaming && !failed { yield (None, Ok(SynthesisEvent::Finished)); }
        }))
    }

    async fn synthesize(
        &mut self,
        text: &str,
        cmd_seq: Option<usize>,
        option: Option<SynthesisOption>,
    ) -> Result<()> {
        let option = self.option.merge_with(option);
        validate(&option)?;
        ensure!(
            option.samplerate.unwrap_or(16000) == self.option.samplerate.unwrap_or(16000),
            "60db TTS sample rate cannot change within a track"
        );
        self.tx
            .as_ref()
            .context("60db TTS client is not started")?
            .send((text.to_owned(), cmd_seq, option))
            .map_err(|_| anyhow::anyhow!("60db TTS stream closed"))?;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        // End-of-stream closes input; accepted requests drain until the output stream is dropped.
        self.tx.take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, method, path},
    };

    const VOICE: &str = "038cf0d1-eef8-45a6-81b0-99c5e57a33d2";

    fn option(endpoint: String) -> SynthesisOption {
        SynthesisOption {
            provider: Some(SynthesisType::SixtyDB),
            secret_key: Some("local-test-token".into()),
            speaker: Some(VOICE.into()),
            endpoint: Some(endpoint),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn http_contract_order_and_stream_completion() {
        let server = MockServer::start().await;
        for text in ["First.", "Second."] {
            Mock::given(method("POST"))
                .and(path("/tts-synthesize"))
                .and(header("authorization", "Bearer local-test-token"))
                .and(body_json(json!({"text": text, "voice_id": VOICE,
                    "audio_config": {"audio_encoding": "LINEAR16", "sample_rate_hertz": 16000},
                    "speed": 1.0, "timestamp_type": "NONE"})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "success": true, "audio_base64": BASE64_STANDARD.encode([1, 0, 2, 0]),
                    "sample_rate": 16000, "encoding": "LINEAR16"
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
        let mut client =
            SixtyDBTtsClient::create(true, &option(format!("{}/tts-synthesize", server.uri())))
                .unwrap();
        let mut events = client.start().await.unwrap();
        client.synthesize("First.", None, None).await.unwrap();
        client.synthesize("Second.", None, None).await.unwrap();
        client.stop().await.unwrap();
        let mut audio_count = 0;
        let mut finish_count = 0;
        while let Some((seq, event)) = events.next().await {
            assert_eq!(seq, None);
            match event.unwrap() {
                SynthesisEvent::AudioChunk(bytes) => {
                    assert_eq!(bytes.as_ref(), &[1, 0, 2, 0]);
                    audio_count += 1;
                }
                SynthesisEvent::Finished => {
                    assert_eq!(audio_count, 2);
                    finish_count += 1;
                }
                _ => panic!("unexpected subtitles"),
            }
        }
        assert_eq!((audio_count, finish_count), (2, 1));
    }

    #[tokio::test]
    async fn ndjson_wrapped_audio_and_command_sequences() {
        let server = MockServer::start().await;
        let wrapped = BASE64_STANDARD.encode(
            serde_json::to_vec(
                &json!({"result": {"audioContent": BASE64_STANDARD.encode([1, 0])}}),
            )
            .unwrap(),
        );
        let body = format!(
            "{}\n{}",
            json!({"type":"meta"}),
            json!({"result":{"audioContent":wrapped}})
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/x-ndjson"))
            .expect(2)
            .mount(&server)
            .await;
        let mut client = SixtyDBTtsClient::create(false, &option(server.uri())).unwrap();
        let mut events = client.start().await.unwrap();
        client.synthesize("First.", Some(0), None).await.unwrap();
        client.synthesize("Second.", Some(1), None).await.unwrap();
        client.stop().await.unwrap();
        let mut completed = Vec::new();
        let mut audio_count = 0;
        while let Some((seq, event)) = events.next().await {
            match event.unwrap() {
                SynthesisEvent::AudioChunk(bytes) => {
                    assert_eq!(bytes.as_ref(), &[1, 0]);
                    assert_eq!(seq, Some(audio_count));
                    audio_count += 1;
                }
                SynthesisEvent::Finished => completed.push(seq),
                _ => panic!("unexpected event"),
            }
        }
        assert_eq!(completed, [Some(0), Some(1)]);
    }

    #[test]
    fn reject_errors_invalid_audio_and_wrong_rate() {
        for record in [
            json!({"success":false,"message":"private text"}),
            json!({"type":"error"}),
            json!({"backendResponse":{"success":false}}),
            json!({"audio_base64":"%%%"}),
            json!({"audio_base64":BASE64_STANDARD.encode([1])}),
            json!({"audio_base64":BASE64_STANDARD.encode([1,0]),"sample_rate":24000}),
            json!({"audio_base64":BASE64_STANDARD.encode([1,0]),"encoding":"mp3"}),
        ] {
            let error = parse_record(&serde_json::to_vec(&record).unwrap(), 16000).unwrap_err();
            assert!(!error.to_string().contains("private text"));
        }
        assert!(parse_record(b"not JSON", 16000).is_err());
    }

    #[test]
    fn wav_decode_and_provider_serialization() {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(
                &mut buffer,
                hound::WavSpec {
                    channels: 1,
                    sample_rate: 16000,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )
            .unwrap();
            writer.write_sample(123i16).unwrap();
            writer.write_sample(-456i16).unwrap();
            writer.finalize().unwrap();
        }
        let encoded = BASE64_STANDARD.encode(buffer.into_inner());
        assert_eq!(
            decode_audio(&encoded, 16000).unwrap().as_ref(),
            &[123, 0, 56, 254]
        );
        assert!(decode_audio(&encoded, 24000).is_err());
        let mut trailing = BASE64_STANDARD.decode(&encoded).unwrap();
        trailing.extend([0, 0]);
        assert!(decode_audio(&BASE64_STANDARD.encode(trailing), 16000).is_err());
        let provider: SynthesisType = serde_json::from_str("\"60db\"").unwrap();
        assert_eq!(provider, SynthesisType::SixtyDB);
        assert_eq!(serde_json::to_string(&provider).unwrap(), "\"60db\"");
    }

    #[tokio::test]
    async fn engine_registration_and_validation() {
        let server = MockServer::start().await;
        let config = option(server.uri());
        let mut client = crate::media::engine::StreamEngine::default()
            .create_tts_client(false, &config)
            .await
            .unwrap();
        assert_eq!(client.provider(), SynthesisType::SixtyDB);
        let events = client.start().await.unwrap();
        let mut invalid = config.clone();
        invalid.samplerate = Some(24000);
        assert!(
            client
                .synthesize("text", Some(0), Some(invalid))
                .await
                .is_err()
        );
        drop(events);
        assert!(client.synthesize("text", Some(0), None).await.is_err());
        for rate in [0, 8000, -1] {
            let mut invalid = config.clone();
            invalid.samplerate = Some(rate);
            assert!(SixtyDBTtsClient::create(false, &invalid).is_err());
        }
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    fn option(endpoint: String) -> SynthesisOption {
        SynthesisOption {
            provider: Some(SynthesisType::SixtyDB),
            secret_key: Some("local-test-token".into()),
            speaker: Some("038cf0d1-eef8-45a6-81b0-99c5e57a33d2".into()),
            endpoint: Some(endpoint),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn http_errors_and_empty_response_never_finish_successfully() {
        for response in [
            ResponseTemplate::new(401).set_body_string("local-test-token"),
            ResponseTemplate::new(200).set_body_json(json!({"success":true})),
            ResponseTemplate::new(200)
                .set_body_raw("{\"type\":\"error\"}\n", "application/x-ndjson"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;
            let mut client = SixtyDBTtsClient::create(false, &option(server.uri())).unwrap();
            let mut events = client.start().await.unwrap();
            client.synthesize("text", Some(0), None).await.unwrap();
            client.stop().await.unwrap();
            let mut errors = 0;
            while let Some((_, event)) = events.next().await {
                let error = event.unwrap_err();
                assert!(!error.to_string().contains("local-test-token"));
                errors += 1;
            }
            assert_eq!(errors, 1);
        }
    }

    #[tokio::test]
    async fn fragmented_ndjson_and_inflight_cancellation_close_the_socket() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let size = socket.read(&mut buffer).await.unwrap();
                assert!(size > 0);
                request.extend_from_slice(&buffer[..size]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: 5000\r\n\r\n{\"result\":{\"audio").await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            socket
                .write_all(b"Content\":\"AQACAA==\"}}\n")
                .await
                .unwrap();
            let size = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                size, 0,
                "dropping the response must close its incomplete HTTP body"
            );
        });
        let mut client = SixtyDBTtsClient::create(true, &option(endpoint)).unwrap();
        let mut events = client.start().await.unwrap();
        client.synthesize("text", None, None).await.unwrap();
        client.stop().await.unwrap();
        let event = tokio::time::timeout(Duration::from_secs(3), events.next())
            .await
            .unwrap()
            .unwrap()
            .1
            .unwrap();
        match event {
            SynthesisEvent::AudioChunk(bytes) => assert_eq!(bytes.as_ref(), &[1, 0, 2, 0]),
            _ => panic!("expected PCM"),
        }
        drop(events);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn binary_pcm_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1, 0, 2, 0], "audio/pcm"))
            .expect(1)
            .mount(&server)
            .await;
        let mut client = SixtyDBTtsClient::create(false, &option(server.uri())).unwrap();
        let mut events = client.start().await.unwrap();
        client.synthesize("text", Some(0), None).await.unwrap();
        client.stop().await.unwrap();
        match events.next().await.unwrap().1.unwrap() {
            SynthesisEvent::AudioChunk(bytes) => assert_eq!(bytes.as_ref(), &[1, 0, 2, 0]),
            _ => panic!("expected PCM"),
        }
        assert!(matches!(
            events.next().await.unwrap().1.unwrap(),
            SynthesisEvent::Finished
        ));
        assert!(events.next().await.is_none());
    }
}
