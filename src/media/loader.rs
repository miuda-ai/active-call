use crate::media::cache;
use anyhow::{Result, anyhow};
use audio_codec::Resampler;
use audio_codec::opus::OpusDecoder;
use hound::WavReader;
use ogg::reading::PacketReader;
use reqwest::Client;
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom, Write};
use std::time::Instant;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::default::{get_codecs, get_probe};
use tracing::{info, warn};
use url::Url;

pub async fn download_from_url(url: &str, use_cache: bool) -> Result<(File, Option<String>)> {
    let cache_key = cache::generate_cache_key(url, 0, None, None);
    if use_cache && cache::is_cached(&cache_key).await? {
        match cache::get_cache_path(&cache_key) {
            Ok(path) => return Ok((File::open(&path).map_err(|e| anyhow!(e))?, None)),
            Err(e) => {
                warn!("loader: Error getting cache path: {}", e);
                return Err(e);
            }
        }
    }

    let start_time = Instant::now();
    let client = Client::new();
    let response = client.get(url).send().await?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());
    let bytes = response.bytes().await?;
    let data = bytes.to_vec();
    let duration = start_time.elapsed();

    info!(
        "loader: Downloaded {} bytes in {:?} for {} (content-type: {:?})",
        data.len(),
        duration,
        url,
        content_type,
    );

    if use_cache {
        cache::store_in_cache(&cache_key, &data).await?;
        match cache::get_cache_path(&cache_key) {
            Ok(path) => return Ok((File::open(path).map_err(|e| anyhow!(e))?, content_type)),
            Err(e) => {
                warn!("loader: Error getting cache path: {}", e);
                return Err(e);
            }
        }
    }

    let mut temp_file = tempfile::tempfile()?;
    temp_file.write_all(&data)?;
    temp_file.seek(SeekFrom::Start(0))?;
    Ok((temp_file, content_type))
}

fn is_ogg(extension: &str, mime_type: Option<&str>) -> bool {
    matches!(extension, "ogg" | "opus")
        || matches!(
            mime_type,
            Some("audio/ogg") | Some("audio/opus") | Some("application/ogg")
        )
}

enum OggCodec {
    Opus { channels: u16 },
    Other,
}

fn detect_ogg_codec(file: &mut File) -> Result<OggCodec> {
    let mut reader = PacketReader::new(BufReader::new(&mut *file));
    let head = reader
        .read_packet_expected()
        .map_err(|e| anyhow!("loader: failed reading OGG header: {e}"))?;
    let codec = if head.data.starts_with(b"OpusHead") {
        let channels = if head.data.len() > 9 {
            head.data[9] as u16
        } else {
            2
        };
        OggCodec::Opus { channels }
    } else {
        OggCodec::Other
    };
    file.seek(SeekFrom::Start(0))?;
    Ok(codec)
}

fn decode_opus_ogg(file: File, channels: u16, target_sample_rate: u32) -> Result<Vec<i16>> {
    let mut reader = PacketReader::new(BufReader::new(file));

    // Consume OpusHead (already peeked, but file was seeked back)
    let head = reader
        .read_packet_expected()
        .map_err(|e| anyhow!("loader: failed reading OGG header: {e}"))?;
    let channels = if head.data.len() > 9 {
        head.data[9] as u16
    } else {
        channels
    };

    // Skip OpusTags packet
    reader
        .read_packet_expected()
        .map_err(|e| anyhow!("loader: failed reading OpusTags: {e}"))?;

    // Opus always encodes at 48 kHz; decode there and resample afterwards
    let mut decoder = OpusDecoder::new(48000, channels);
    let mut all_samples: Vec<i16> = Vec::new();

    loop {
        let packet = match reader.read_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => return Err(anyhow!("loader: failed reading OGG packet: {e}")),
        };
        let samples = audio_codec::Decoder::decode(&mut decoder, &packet.data);
        all_samples.extend_from_slice(&samples);
    }

    if all_samples.is_empty() {
        return Err(anyhow!(
            "loader: no decodable audio samples found in Opus stream"
        ));
    }

    info!(
        "loader: decoded Opus stream at 48000 Hz, {} channel(s)",
        channels
    );

    if target_sample_rate != 48000 {
        let mut resampler = Resampler::new(48000, target_sample_rate as usize);
        all_samples = resampler.resample(&all_samples);
    }

    Ok(all_samples)
}

pub fn decode_wav(file: File, target_sample_rate: u32) -> Result<Vec<i16>> {
    let reader = BufReader::new(file);
    let mut wav_reader = WavReader::new(reader)?;
    let spec = wav_reader.spec();
    let sample_rate = spec.sample_rate;
    let is_stereo = spec.channels == 2;

    info!(
        "WAV file detected with sample rate: {} Hz, channels: {}, bits: {}",
        sample_rate, spec.channels, spec.bits_per_sample
    );

    let mut all_samples = Vec::new();

    // Read all samples based on format and bit depth
    match spec.sample_format {
        hound::SampleFormat::Int => match spec.bits_per_sample {
            16 => {
                for sample in wav_reader.samples::<i16>() {
                    if let Ok(s) = sample {
                        all_samples.push(s);
                    } else {
                        break;
                    }
                }
            }
            8 => {
                for sample in wav_reader.samples::<i8>() {
                    if let Ok(s) = sample {
                        all_samples.push((s as i16) * 256); // Convert 8-bit to 16-bit
                    } else {
                        break;
                    }
                }
            }
            24 | 32 => {
                for sample in wav_reader.samples::<i32>() {
                    if let Ok(s) = sample {
                        all_samples.push((s >> 16) as i16); // Convert 24/32-bit to 16-bit
                    } else {
                        break;
                    }
                }
            }
            _ => {
                return Err(anyhow!(
                    "Unsupported bits per sample: {}",
                    spec.bits_per_sample
                ));
            }
        },
        hound::SampleFormat::Float => {
            for sample in wav_reader.samples::<f32>() {
                if let Ok(s) = sample {
                    all_samples.push((s * 32767.0) as i16); // Convert float to 16-bit
                } else {
                    break;
                }
            }
        }
    }

    // Convert stereo to mono if needed
    if is_stereo {
        let mono_samples = all_samples
            .chunks(2)
            .map(|chunk| {
                if chunk.len() == 2 {
                    ((chunk[0] as i32 + chunk[1] as i32) / 2) as i16
                } else {
                    chunk[0]
                }
            })
            .collect();
        all_samples = mono_samples;
    }

    if sample_rate != target_sample_rate && sample_rate > 0 {
        let mut resampler = Resampler::new(sample_rate as usize, target_sample_rate as usize);
        all_samples = resampler.resample(&all_samples);
    }

    Ok(all_samples)
}

pub fn decode_audio(
    mut file: File,
    extension: &str,
    mime_type: Option<&str>,
    target_sample_rate: u32,
) -> Result<Vec<i16>> {
    if matches!(extension, "wav")
        || matches!(
            mime_type,
            Some("audio/wav") | Some("audio/wave") | Some("audio/x-wav")
        )
    {
        return decode_wav(file, target_sample_rate);
    }

    if is_ogg(extension, mime_type) {
        match detect_ogg_codec(&mut file)? {
            OggCodec::Opus { channels } => {
                return decode_opus_ogg(file, channels, target_sample_rate);
            }
            OggCodec::Other => {} // fall through to symphonia (e.g. Vorbis)
        }
    }

    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if !extension.is_empty() {
        hint.with_extension(extension);
    }
    if let Some(mime) = mime_type {
        hint.mime_type(mime);
    }

    let probed = get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;

    let mut format = probed.format;
    let (track_id, codec_params) = {
        let track = format
            .default_track()
            .ok_or_else(|| anyhow!("loader: no default audio track found"))?;
        (track.id, track.codec_params.clone())
    };

    let mut decoder = get_codecs().make(&codec_params, &DecoderOptions::default())?;
    let mut all_samples = Vec::new();
    let mut sample_rate = codec_params.sample_rate.unwrap_or(0);

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(_)) => break,
            Err(SymphoniaError::ResetRequired) => continue,
            Err(e) => return Err(anyhow!("loader: failed reading audio packet: {e}")),
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                if sample_rate == 0 {
                    sample_rate = decoded.spec().rate;
                    info!(
                        "loader: detected {:?} with sample rate: {} Hz, channels: {}",
                        codec_params.codec,
                        sample_rate,
                        decoded.spec().channels.count()
                    );
                }
                let spec = *decoded.spec();
                let channels = spec.channels.count();

                let mut sample_buffer = SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
                sample_buffer.copy_interleaved_ref(decoded);
                let interleaved = sample_buffer.samples();

                if channels <= 1 {
                    all_samples.extend_from_slice(interleaved);
                } else {
                    for frame in interleaved.chunks(channels) {
                        if frame.is_empty() {
                            continue;
                        }
                        let sum: i32 = frame.iter().map(|s| *s as i32).sum();
                        all_samples.push((sum / frame.len() as i32) as i16);
                    }
                }
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(_)) => break,
            Err(SymphoniaError::ResetRequired) => continue,
            Err(e) => return Err(anyhow!("loader: failed decoding audio packet: {e}")),
        }
    }

    if all_samples.is_empty() {
        return Err(anyhow!("loader: no decodable audio samples found"));
    }

    if sample_rate != target_sample_rate && sample_rate > 0 {
        let mut resampler = Resampler::new(sample_rate as usize, target_sample_rate as usize);
        all_samples = resampler.resample(&all_samples);
    }

    Ok(all_samples)
}

pub async fn load_audio_as_pcm(
    path: &str,
    target_sample_rate: u32,
    use_cache: bool,
) -> Result<Vec<i16>> {
    let is_url = path.starts_with("http://") || path.starts_with("https://");

    let (file, content_type) = if is_url {
        download_from_url(path, use_cache).await?
    } else {
        (File::open(path).map_err(|e| anyhow!("loader: {} {}", path, e))?, None)
    };

    let extension = if is_url {
        path.parse::<Url>()?.path().split('.').last().unwrap_or("").to_string()
    } else {
        path.split('.').last().unwrap_or("").to_string()
    };

    decode_audio(
        file,
        &extension,
        content_type.as_deref(),
        target_sample_rate,
    )
}
