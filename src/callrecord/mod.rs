use crate::CallOption;
use crate::{
    call::ActiveCallType,
    config::{CallRecordConfig, S3Vendor},
};
use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use futures::stream::{FuturesUnordered, StreamExt};
use object_store::PutPayload;
use object_store::{
    ObjectStore, ObjectStoreExt, aws::AmazonS3Builder, azure::MicrosoftAzureBuilder,
    gcp::GoogleCloudStorageBuilder, path::Path as ObjectPath,
};
use reqwest;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::{
    collections::HashMap, future::Future, path::Path, pin::Pin, str::FromStr, sync::Arc,
    time::Instant,
};
use tokio::{fs::File, io::AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub type CallRecordSender = tokio::sync::mpsc::UnboundedSender<CallRecord>;
pub type CallRecordReceiver = tokio::sync::mpsc::UnboundedReceiver<CallRecord>;

pub type FnSaveCallRecord = Arc<
    Box<
        dyn Fn(
                CancellationToken,
                Arc<dyn CallRecordFormatter>,
                Arc<CallRecordConfig>,
                CallRecord,
            ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>
            + Send
            + Sync,
    >,
>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CallRecordEventType {
    Event,
    Command,
    Sip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRecordEvent {
    pub r#type: CallRecordEventType,
    pub timestamp: u64,
    pub content: String,
}

impl CallRecordEvent {
    pub async fn write<T: Serialize>(r#type: CallRecordEventType, obj: T, file: &mut File) {
        let content = match serde_json::to_string(&obj) {
            Ok(s) => s,
            Err(_) => return,
        };
        let event = Self {
            r#type,
            timestamp: crate::media::get_timestamp(),
            content,
        };
        match serde_json::to_string(&event) {
            Ok(line) => {
                file.write_all(format!("{}\n", line).as_bytes()).await.ok();
            }
            Err(_) => {}
        }
    }
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CallRecord {
    pub call_type: ActiveCallType,
    pub option: Option<CallOption>,
    pub call_id: String,
    pub start_time: DateTime<Utc>,
    pub ring_time: Option<DateTime<Utc>>,
    pub answer_time: Option<DateTime<Utc>>,
    pub end_time: DateTime<Utc>,
    pub caller: String,
    pub callee: String,
    pub status_code: u16,
    pub hangup_reason: Option<CallRecordHangupReason>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(default)]
    pub hangup_messages: Vec<CallRecordHangupMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(default)]
    pub recorder: Vec<CallRecordMedia>,
    pub extras: Option<HashMap<String, serde_json::Value>>,
    pub dump_event_file: Option<String>,
    pub refer_callrecord: Option<Box<CallRecord>>,
    /// SIP Call-IDs of every dialog belonging to this session (primary leg,
    /// bridge legs, REFER transfer legs), resolved by the sipflow capture
    /// ([`crate::sipflow`]) so post-call hooks can merge all legs' signaling.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sip_call_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRecordMedia {
    pub track_id: String,
    pub path: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum CallRecordHangupReason {
    ByCaller,
    ByCallee,
    ByRefer,
    BySystem,
    Autohangup,
    InactivityTimeout,
    NoAnswer,
    NoBalance,
    AnswerMachine,
    ServerUnavailable,
    Canceled,
    Rejected,
    Failed,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRecordHangupMessage {
    pub code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

impl FromStr for CallRecordHangupReason {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "caller" => Ok(Self::ByCaller),
            "callee" => Ok(Self::ByCallee),
            "refer" => Ok(Self::ByRefer),
            "system" => Ok(Self::BySystem),
            "autohangup" => Ok(Self::Autohangup),
            "inactivitytimeout" => Ok(Self::InactivityTimeout),
            "noanswer" => Ok(Self::NoAnswer),
            "nobalance" => Ok(Self::NoBalance),
            "answermachine" => Ok(Self::AnswerMachine),
            "serverunavailable" => Ok(Self::ServerUnavailable),
            "canceled" => Ok(Self::Canceled),
            "rejected" => Ok(Self::Rejected),
            "failed" => Ok(Self::Failed),
            _ => Ok(Self::Other(s.to_string())),
        }
    }
}
impl ToString for CallRecordHangupReason {
    fn to_string(&self) -> String {
        match self {
            Self::ByCaller => "caller".to_string(),
            Self::ByCallee => "callee".to_string(),
            Self::ByRefer => "refer".to_string(),
            Self::BySystem => "system".to_string(),
            Self::Autohangup => "autohangup".to_string(),
            Self::InactivityTimeout => "inactivityTimeout".to_string(),
            Self::NoAnswer => "noAnswer".to_string(),
            Self::NoBalance => "noBalance".to_string(),
            Self::AnswerMachine => "answerMachine".to_string(),
            Self::ServerUnavailable => "serverUnavailable".to_string(),
            Self::Canceled => "canceled".to_string(),
            Self::Rejected => "rejected".to_string(),
            Self::Failed => "failed".to_string(),
            Self::Other(s) => s.to_string(),
        }
    }
}

pub fn default_cdr_file_name(record: &CallRecord) -> String {
    format!(
        "{}_{}.json",
        record
            .start_time
            .with_timezone(&Local)
            .format("%Y%m%d-%H%M%S"),
        record.call_id
    )
}

pub trait CallRecordFormatter: Send + Sync {
    fn format(&self, record: &CallRecord) -> Result<String> {
        Ok(serde_json::to_string(record)?)
    }
    fn format_file_name(&self, record: &CallRecord) -> String;
    fn format_dump_events_path(&self, record: &CallRecord) -> String;
    fn format_media_path(&self, record: &CallRecord, media: &CallRecordMedia) -> String;
}

/// Post-processing hooks run by the [`CallRecordManager`] around the saver
/// (single-record counterpart of rustpbx's `CallRecordHook`). Implement
/// either method; both default to no-ops. Heavy work should be spawned.
#[async_trait::async_trait]
pub trait CallRecordHook: Send + Sync {
    /// Runs before the record is handed to the saver; may mutate the record
    /// (e.g. enrich fields that must appear in the persisted document).
    async fn on_record_enrich(&self, _record: &mut CallRecord) -> Result<()> {
        Ok(())
    }

    /// Runs after the saver finished (successfully or not).
    async fn on_record_completed(&self, _record: &mut CallRecord) -> Result<()> {
        Ok(())
    }
}

pub struct DefaultCallRecordFormatter {
    pub root: String,
}

impl Default for DefaultCallRecordFormatter {
    fn default() -> Self {
        Self {
            root: "./config/cdr".to_string(),
        }
    }
}

impl DefaultCallRecordFormatter {
    pub fn new_with_config(config: &CallRecordConfig) -> Self {
        let root = match config {
            CallRecordConfig::Local { root } => root.clone(),
            CallRecordConfig::S3 { root, .. } => root.clone(),
            _ => "./config/cdr".to_string(),
        };
        Self { root }
    }
}

impl CallRecordFormatter for DefaultCallRecordFormatter {
    fn format_file_name(&self, record: &CallRecord) -> String {
        let trimmed_root = self.root.trim_end_matches('/');
        let file_name = default_cdr_file_name(record);
        if trimmed_root.is_empty() {
            file_name
        } else {
            format!(
                "{}/{}/{}",
                trimmed_root,
                record.start_time.with_timezone(&Local).format("%Y%m%d"),
                file_name
            )
        }
    }

    fn format_dump_events_path(&self, record: &CallRecord) -> String {
        format!(
            "{}/{}/{}.jsonl",
            self.root.trim_end_matches('/'),
            record.start_time.with_timezone(&Local).format("%Y%m%d"),
            record.call_id
        )
    }

    fn format_media_path(&self, record: &CallRecord, media: &CallRecordMedia) -> String {
        let file_name = Path::new(&media.path)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
            .to_string_lossy()
            .to_string();

        format!(
            "{}/{}/{}_{}_{}",
            self.root.trim_end_matches('/'),
            record.start_time.with_timezone(&Local).format("%Y%m%d"),
            record.call_id,
            media.track_id,
            file_name
        )
    }
}

pub fn build_object_store_from_s3(
    vendor: &S3Vendor,
    bucket: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
    endpoint: &str,
) -> Result<Arc<dyn ObjectStore>> {
    let store: Arc<dyn ObjectStore> = match vendor {
        S3Vendor::AWS => {
            let builder = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_region(region)
                .with_access_key_id(access_key)
                .with_secret_access_key(secret_key);

            let instance = if !endpoint.is_empty() {
                builder.with_endpoint(endpoint).build()?
            } else {
                builder.build()?
            };
            Arc::new(instance)
        }
        S3Vendor::GCP => {
            let instance = GoogleCloudStorageBuilder::new()
                .with_bucket_name(bucket)
                .with_service_account_key(secret_key)
                .build()?;
            Arc::new(instance)
        }
        S3Vendor::Azure => {
            let instance = MicrosoftAzureBuilder::new()
                .with_container_name(bucket)
                .with_account(access_key)
                .with_access_key(secret_key)
                .build()?;
            Arc::new(instance)
        }
        S3Vendor::Aliyun | S3Vendor::Tencent | S3Vendor::Minio | S3Vendor::DigitalOcean => {
            let instance = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_region(region)
                .with_access_key_id(access_key)
                .with_secret_access_key(secret_key)
                .with_endpoint(endpoint)
                .with_virtual_hosted_style_request(false)
                .build()?;
            Arc::new(instance)
        }
    };

    Ok(store)
}

pub struct CallRecordManager {
    pub max_concurrent: usize,
    pub sender: CallRecordSender,
    config: Arc<CallRecordConfig>,
    cancel_token: CancellationToken,
    receiver: CallRecordReceiver,
    saver_fn: FnSaveCallRecord,
    formatter: Arc<dyn CallRecordFormatter>,
    hooks: Vec<Arc<dyn CallRecordHook>>,
}

pub struct CallRecordManagerBuilder {
    pub cancel_token: Option<CancellationToken>,
    pub config: Option<CallRecordConfig>,
    pub max_concurrent: Option<usize>,
    saver_fn: Option<FnSaveCallRecord>,
    formatter: Option<Arc<dyn CallRecordFormatter>>,
    hooks: Vec<Arc<dyn CallRecordHook>>,
}

impl CallRecordManagerBuilder {
    pub fn new() -> Self {
        Self {
            cancel_token: None,
            config: None,
            max_concurrent: None,
            saver_fn: None,
            formatter: None,
            hooks: Vec::new(),
        }
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    pub fn with_config(mut self, config: CallRecordConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_saver(mut self, saver: FnSaveCallRecord) -> Self {
        self.saver_fn = Some(saver);
        self
    }

    pub fn with_formatter(mut self, formatter: Arc<dyn CallRecordFormatter>) -> Self {
        self.formatter = Some(formatter);
        self
    }

    pub fn with_max_concurrent(mut self, max_concurrent: usize) -> Self {
        self.max_concurrent = Some(max_concurrent);
        self
    }

    /// Appends a post-processing hook (see [`CallRecordHook`]).
    pub fn with_hook(mut self, hook: Arc<dyn CallRecordHook>) -> Self {
        self.hooks.push(hook);
        self
    }

    pub fn build(self) -> CallRecordManager {
        let cancel_token = self.cancel_token.unwrap_or_default();
        let config = Arc::new(self.config.unwrap_or_default());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let saver_fn = self
            .saver_fn
            .unwrap_or_else(|| Arc::new(Box::new(CallRecordManager::default_saver)));
        let formatter = self
            .formatter
            .unwrap_or_else(|| Arc::new(DefaultCallRecordFormatter::default()));
        let max_concurrent = self.max_concurrent.unwrap_or(64);

        match config.as_ref() {
            CallRecordConfig::Local { root } => {
                if !Path::new(&root).exists() {
                    match std::fs::create_dir_all(&root) {
                        Ok(_) => {
                            info!("CallRecordManager created directory: {}", root);
                        }
                        Err(e) => {
                            warn!("CallRecordManager failed to create directory: {}", e);
                        }
                    }
                }
            }
            _ => {}
        }

        CallRecordManager {
            max_concurrent,
            cancel_token,
            sender,
            receiver,
            config,
            saver_fn,
            formatter,
            hooks: self.hooks,
        }
    }
}

impl CallRecordManager {
    fn default_saver(
        _cancel_token: CancellationToken,
        formatter: Arc<dyn CallRecordFormatter>,
        config: Arc<CallRecordConfig>,
        record: CallRecord,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        Box::pin(async move {
            let mut record = record;
            let start_time = Instant::now();
            let result = match config.as_ref() {
                CallRecordConfig::Local { .. } => {
                    Self::save_local_record(formatter.clone(), &mut record).await
                }
                CallRecordConfig::S3 {
                    vendor,
                    bucket,
                    region,
                    access_key,
                    secret_key,
                    endpoint,
                    with_media,
                    keep_media_copy,
                    part_size_mb,
                    delete_after_upload,
                    ..
                } => {
                    Self::save_with_s3_like(
                        formatter.clone(),
                        vendor,
                        bucket,
                        region,
                        access_key,
                        secret_key,
                        endpoint,
                        with_media,
                        keep_media_copy,
                        part_size_mb,
                        delete_after_upload,
                        &record,
                    )
                    .await
                }
                CallRecordConfig::Http {
                    url,
                    headers,
                    with_media,
                    keep_media_copy,
                    part_size_mb,
                    delete_after_upload,
                } => {
                    Self::save_with_http(
                        formatter.clone(),
                        url,
                        headers,
                        with_media,
                        keep_media_copy,
                        part_size_mb,
                        delete_after_upload,
                        &record,
                    )
                    .await
                }
            };
            let file_name = match result {
                Ok(file_name) => file_name,
                Err(e) => {
                    warn!("Failed to save call record: {}", e);
                    return Err(e);
                }
            };
            let elapsed = start_time.elapsed();
            info!(
                ?elapsed,
                call_id = record.call_id,
                file_name,
                "CallRecordManager saved"
            );
            Ok(())
        })
    }

    async fn save_local_record(
        formatter: Arc<dyn CallRecordFormatter>,
        record: &mut CallRecord,
    ) -> Result<String> {
        let file_content = formatter.format(record)?;
        let file_name = formatter.format_file_name(record);

        // Ensure parent directory exists
        if let Some(parent) = Path::new(&file_name).parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                anyhow::anyhow!("Failed to create parent directory for {}: {}", file_name, e)
            })?;
        }

        let mut file = File::create(&file_name).await.map_err(|e| {
            anyhow::anyhow!("Failed to create call record file {}: {}", file_name, e)
        })?;
        file.write_all(file_content.as_bytes()).await?;
        file.flush().await?;
        Ok(file_name.to_string())
    }

    /// Stream a local file into the object store without reading the whole file
/// into memory. `part_size` bounds the in-memory buffer; the buffered writer
/// spills full parts as multipart uploads (files smaller than the buffer are
/// still a single PUT). Returns the file size in bytes.
async fn upload_file_streaming(
    store: std::sync::Arc<dyn object_store::ObjectStore>,
    local_path: &std::path::Path,
    key: &ObjectPath,
    part_size: usize,
) -> Result<u64> {
    use tokio::io::AsyncReadExt;
    let size = tokio::fs::metadata(local_path).await?.len();
    let mut file = tokio::fs::File::open(local_path).await?;
    let mut writer = object_store::buffered::BufWriter::with_capacity(store, key.clone(), part_size);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).await?;
    }
    // flush + complete the multipart upload
    tokio::io::AsyncWriteExt::shutdown(&mut writer).await?;
    Ok(size)
}

pub async fn save_with_http(
        formatter: Arc<dyn CallRecordFormatter>,
        url: &String,
        headers: &Option<HashMap<String, String>>,
        with_media: &Option<bool>,
        keep_media_copy: &Option<bool>,
        part_size_mb: &Option<u64>,
        delete_after_upload: &Option<bool>,
        record: &CallRecord,
    ) -> Result<String> {
        let _ = part_size_mb; // HTTP path streams by file length; part size is S3-only
        let mut uploaded_paths: Vec<String> = Vec::new();
        let client = reqwest::Client::new();
        // Serialize call record to JSON
        let call_log_json = formatter.format(record)?;
        // Create multipart form
        let mut form = reqwest::multipart::Form::new().text("calllog.json", call_log_json);

        // Add media files if with_media is true
        if with_media.unwrap_or(false) {
            for media in &record.recorder {
                if std::path::Path::new(&media.path).exists() {
                    let file_name = std::path::Path::new(&media.path)
                        .file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
                        .to_string_lossy()
                        .to_string();
                    match tokio::fs::File::open(&media.path).await {
                        Ok(file) => {
                            let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
                            let part = match reqwest::multipart::Part::stream_with_length(
                                reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file)),
                                len,
                            )
                            .file_name(file_name)
                            .mime_str("application/octet-stream")
                            {
                                Ok(p) => p,
                                Err(e) => {
                                    warn!("failed to set mime on media part: {}", e);
                                    continue;
                                }
                            };
                            form = form.part(format!("media_{}", media.track_id), part);
                            uploaded_paths.push(media.path.clone());
                        }
                        Err(e) => {
                            warn!("Failed to open media file {}: {}", media.path, e);
                        }
                    }
                }
            }
            if let Some(dump_events_file) = &record.dump_event_file {
                if Path::new(&dump_events_file).exists() {
                    let file_name = Path::new(&dump_events_file)
                        .file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
                        .to_string_lossy()
                        .to_string();
                    match tokio::fs::File::open(&dump_events_file).await {
                        Ok(file) => {
                            let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
                            if let Ok(part) = reqwest::multipart::Part::stream_with_length(
                                reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file)),
                                len,
                            )
                            .file_name(file_name.clone())
                            .mime_str("application/octet-stream")
                            {
                                form = form.part(format!("dump_events_{}", file_name), part);
                                uploaded_paths.push(dump_events_file.clone());
                            }
                        }
                        Err(e) => {
                            warn!("Failed to open events file {}: {}", dump_events_file, e);
                        }
                    }
                }
            }
        }
        let mut request = client.post(url).multipart(form);
        if let Some(headers_map) = headers {
            for (key, value) in headers_map {
                request = request.header(key, value);
            }
        }
        let response = request.send().await?;
        if response.status().is_success() {
            let response_text = response.text().await.unwrap_or_default();

            // Delete local copies only after a successful upload.
            let delete_local = delete_after_upload.unwrap_or(true) && !keep_media_copy.unwrap_or(false);
            if delete_local {
                for p in &uploaded_paths {
                    let path = Path::new(p);
                    if path.exists() {
                        if let Err(e) = tokio::fs::remove_file(path).await {
                            warn!(path = %p, "failed to delete local file after HTTP upload: {}", e);
                        }
                    }
                }
            }
            Ok(format!("HTTP upload successful: {}", response_text))
        } else {
            Err(anyhow::anyhow!(
                "HTTP upload failed with status: {} - {}",
                response.status(),
                response.text().await.unwrap_or_default()
            ))
        }
    }

    pub async fn save_with_s3_like(
        formatter: Arc<dyn CallRecordFormatter>,
        vendor: &S3Vendor,
        bucket: &String,
        region: &String,
        access_key: &String,
        secret_key: &String,
        endpoint: &String,
        with_media: &Option<bool>,
        keep_media_copy: &Option<bool>,
        part_size_mb: &Option<u64>,
        delete_after_upload: &Option<bool>,
        record: &CallRecord,
    ) -> Result<String> {
        let start_time = Instant::now();
        let part_size = part_size_mb.map(|m| (m * 1024 * 1024) as usize).unwrap_or(5 * 1024 * 1024);
        let object_store =
            build_object_store_from_s3(vendor, bucket, region, access_key, secret_key, endpoint)?;

        // Serialize call record to JSON
        let call_log_json = formatter.format(record)?;
        // Upload call log JSON
        let filename = formatter.format_file_name(record);
        let json_path = ObjectPath::from(filename.clone());
        let buf_size = call_log_json.len();
        let json_uploaded = match object_store
            .put(&json_path, PutPayload::from(call_log_json))
            .await
        {
            Ok(_) => {
                info!(
                    elapsed = start_time.elapsed().as_secs_f64(),
                    %json_path,
                    buf_size,
                    "upload call record"
                );
                true
            }
            Err(e) => {
                warn!(
                   %json_path,
                    "failed to upload call record: {}", e
                );
                false
            }
        };
        // Upload media files if with_media is true
        let mut uploaded_paths: Vec<String> = Vec::new();
        if with_media.unwrap_or(false) {
            let mut media_files = vec![];
            for media in &record.recorder {
                if Path::new(&media.path).exists() {
                    let media_path = ObjectPath::from(formatter.format_media_path(record, media));
                    media_files.push((media.path.clone(), media_path));
                }
            }
            if let Some(dump_events_file) = &record.dump_event_file {
                if Path::new(&dump_events_file).exists() {
                    let dump_events_path =
                        ObjectPath::from(formatter.format_dump_events_path(record));
                    media_files.push((dump_events_file.clone(), dump_events_path));
                }
            }
            for (path, media_path) in &media_files {
                let start_time = Instant::now();
                match Self::upload_file_streaming(
                    object_store.clone(),
                    std::path::Path::new(path),
                    media_path,
                    part_size,
                )
                .await
                {
                    Ok(buf_size) => {
                        info!(
                            elapsed = start_time.elapsed().as_secs_f64(),
                            %media_path,
                            buf_size,
                            "upload media file"
                        );
                        uploaded_paths.push(path.clone());
                    }
                    Err(e) => {
                        warn!(%media_path,"failed to upload media file: {}", e);
                    }
                }
            }
        }
        // Delete local copies only for files whose upload succeeded.
        let delete_local = delete_after_upload.unwrap_or(true) && !keep_media_copy.unwrap_or(false);
        if delete_local {
            for p in &uploaded_paths {
                let path = Path::new(p);
                if path.exists() {
                    if let Err(e) = tokio::fs::remove_file(path).await {
                        warn!(path = %p, "failed to delete local media after upload: {}", e);
                    }
                }
            }
            if json_uploaded {
                let path = Path::new(&filename);
                if path.exists() {
                    if let Err(e) = tokio::fs::remove_file(path).await {
                        warn!(path = %filename, "failed to delete local record after upload: {}", e);
                    }
                }
            }
        }

        Ok(format!(
            "{}/{}",
            endpoint.trim_end_matches('/'),
            json_path.to_string().trim_start_matches('/')
        ))
    }

    pub async fn serve(&mut self) {
        let token = self.cancel_token.clone();
        info!("CallRecordManager serving");
        tokio::select! {
            _ = token.cancelled() => {}
            _ = self.recv_loop() => {}
        }
        info!("CallRecordManager served");
    }

    async fn recv_loop(&mut self) -> Result<()> {
        let mut futures = FuturesUnordered::new();
        loop {
            let limit = self.max_concurrent - futures.len();
            if limit == 0 {
                if let Some(_) = futures.next().await {}
                continue;
            }
            let mut buffer = Vec::with_capacity(limit);
            if self.receiver.recv_many(&mut buffer, limit).await == 0 {
                break;
            }

            for mut record in buffer {
                let cancel_token_ref = self.cancel_token.clone();
                let save_fn_ref = self.saver_fn.clone();
                let config_ref = self.config.clone();
                let formatter_ref = self.formatter.clone();
                let hooks = self.hooks.clone();

                futures.push(async move {
                    for hook in &hooks {
                        if let Err(e) = hook.on_record_enrich(&mut record).await {
                            warn!(
                                call_id = record.call_id,
                                "callrecord on_record_enrich hook failed: {}", e
                            );
                        }
                    }
                    // The saver consumes the record; keep a copy for the
                    // post-save hooks (records are small).
                    let result =
                        save_fn_ref(cancel_token_ref, formatter_ref, config_ref, record.clone())
                            .await;
                    for hook in &hooks {
                        if let Err(e) = hook.on_record_completed(&mut record).await {
                            warn!(
                                call_id = record.call_id,
                                "callrecord on_record_completed hook failed: {}", e
                            );
                        }
                    }
                    if let Err(e) = result {
                        warn!("Failed to save call record: {}", e);
                    }
                });
            }
            while let Some(_) = futures.next().await {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the order hook/saver invocations happen in.
    struct OrderingHook {
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl CallRecordHook for OrderingHook {
        async fn on_record_enrich(&self, record: &mut CallRecord) -> Result<()> {
            self.events
                .lock()
                .unwrap()
                .push(format!("{}:enrich", self.name));
            // Enrich must be able to mutate the persisted record.
            record.sip_call_ids.push(format!("from-{}", self.name));
            Ok(())
        }

        async fn on_record_completed(&self, _record: &mut CallRecord) -> Result<()> {
            self.events
                .lock()
                .unwrap()
                .push(format!("{}:completed", self.name));
            Ok(())
        }
    }

    fn sample_record() -> CallRecord {
        CallRecord {
            call_type: crate::call::ActiveCallType::Sip,
            option: None,
            call_id: "s.test".into(),
            start_time: Utc::now(),
            ring_time: None,
            answer_time: None,
            end_time: Utc::now(),
            caller: "a".into(),
            callee: "b".into(),
            status_code: 200,
            hangup_reason: None,
            hangup_messages: Vec::new(),
            recorder: Vec::new(),
            extras: None,
            dump_event_file: None,
            refer_callrecord: None,
            sip_call_ids: Vec::new(),
        }
    }

    #[tokio::test]
    async fn hooks_run_around_the_saver() {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let saved: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

        let saver_saved = saved.clone();
        let saver: FnSaveCallRecord = Arc::new(Box::new(
            move |_token, _formatter, _config, record| {
                let saver_saved = saver_saved.clone();
                Box::pin(async move {
                    saver_saved.lock().unwrap().push(record.call_id.clone());
                    Ok(())
                })
            },
        ));

        // Build the manager field-by-field so the test can close the
        // channel after sending (swap out the manager's own sender clone).
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = CallRecordManager {
            max_concurrent: 4,
            sender,
            config: Arc::new(CallRecordConfig::default()),
            cancel_token: CancellationToken::new(),
            receiver,
            saver_fn: saver,
            formatter: Arc::new(DefaultCallRecordFormatter::default()),
            hooks: vec![Arc::new(OrderingHook {
                events: events.clone(),
                name: "h1",
            })],
        };

        manager.sender.send(sample_record()).unwrap();
        let real_sender = std::mem::replace(
            &mut manager.sender,
            tokio::sync::mpsc::unbounded_channel().0,
        );
        drop(real_sender);
        manager.recv_loop().await.unwrap();

        let timeline = events.lock().unwrap().clone();
        assert_eq!(
            timeline,
            vec!["h1:enrich".to_string(), "h1:completed".to_string()]
        );
        // The saver saw the enriched record.
        assert_eq!(saved.lock().unwrap().len(), 1);
    }
}
