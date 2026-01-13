use crate::media::cache;
use anyhow::{Result, anyhow};
use audio_codec::Resampler;
use hound::WavReader;
use reqwest::Client;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::time::Instant;
use tracing::{info, warn};
use url::Url;

pub async fn download_from_url(url: &str, use_cache: bool) -> Result<File> {
    // Check if file is already cached
    let cache_key = cache::generate_cache_key(url, 0, None, None);
    if use_cache && cache::is_cached(&cache_key).await? {
        match cache::get_cache_path(&cache_key) {
            Ok(path) => return File::open(&path).map_err(|e| anyhow!(e)),
            Err(e) => {
                warn!("loader: Error getting cache path: {}", e);
                return Err(e);
            }
        }
    }

    // Download file if not cached
    let start_time = Instant::now();
    let client = Client::new();
    let response = client.get(url).send().await?;
    let bytes = response.bytes().await?;
    let data = bytes.to_vec();
    let duration = start_time.elapsed();

    info!(
        "loader: Downloaded {} bytes in {:?} for {}",
        data.len(),
        duration,
        url,
    );

    // Store in cache if enabled
    if use_cache {
        cache::store_in_cache(&cache_key, &data).await?;
        match cache::get_cache_path(&cache_key) {
            Ok(path) => return File::open(path).map_err(|e| anyhow!(e)),
            Err(e) => {
                warn!("loader: Error getting cache path: {}", e);
                return Err(e);
            }
        }
    }

    // Return temporary file with downloaded data
    let mut temp_file = tempfile::tempfile()?;
    temp_file.write_all(&data)?;
    temp_file.seek(SeekFrom::Start(0))?;
    Ok(temp_file)
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

pub fn decode_mp3(file: File, target_sample_rate: u32) -> Result<Vec<i16>> {
    let mut reader = BufReader::new(file);
    let mut file_data = Vec::new();
    reader.read_to_end(&mut file_data)?;

    let mut decoder = rmp3::Decoder::new(&file_data);
    let mut all_samples = Vec::new();
    let mut sample_rate = 0;

    while let Some(frame) = decoder.next() {
        match frame {
            rmp3::Frame::Audio(audio) => {
                if sample_rate == 0 {
                    sample_rate = audio.sample_rate();
                    info!("MP3 file detected with sample rate: {} Hz", sample_rate);
                }
                all_samples.extend_from_slice(audio.samples());
            }
            rmp3::Frame::Other(_) => {}
        }
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
    let extension = if path.starts_with("http://") || path.starts_with("https://") {
        path.parse::<Url>()?
            .path()
            .split(".")
            .last()
            .unwrap_or("")
            .to_string()
    } else {
        path.split('.').last().unwrap_or("").to_string()
    };

    let file = if path.starts_with("http://") || path.starts_with("https://") {
        download_from_url(path, use_cache).await?
    } else {
        File::open(path).map_err(|e| anyhow!("loader: {} {}", path, e))?
    };

    match extension.to_lowercase().as_str() {
        "wav" => decode_wav(file, target_sample_rate),
        "mp3" => decode_mp3(file, target_sample_rate),
        _ => Err(anyhow!("loader: Unsupported file extension: {}", extension)),
    }
}
