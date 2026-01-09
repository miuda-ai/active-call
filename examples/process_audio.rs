use active_call::media::denoiser::NoiseReducer;
use active_call::media::processor::Processor;
use active_call::media::vad::{VADOption, VadProcessor, VadType};
use active_call::media::{AudioFrame, Samples};
use anyhow::Result;
use audio_codec::samples_to_bytes;
use clap::Parser;
use std::fs::File;
use std::io::Write;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Input WAV file path
    #[arg(short, long)]
    input: String,

    /// Output denoised PCM file path
    #[arg(short, long)]
    output: Option<String>,

    /// VAD type: silero or ten
    #[arg(short, long, default_value = "silero")]
    vad: String,

    /// Voice threshold
    #[arg(long, default_value_t = 0.5)]
    threshold: f32,

    /// Speech padding (ms)
    #[arg(long, default_value_t = 300)]
    speech_pad: u64,

    /// Silence padding (ms)
    #[arg(long, default_value_t = 100)]
    silence_pad: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    println!("Processing file: {}", args.input);
    let (all_samples, sample_rate) = active_call::media::track::file::read_wav_file(&args.input)?;
    println!(
        "Sample rate: {}, Total samples: {}, Duration: {:.2}s",
        sample_rate,
        all_samples.len(),
        all_samples.len() as f64 / sample_rate as f64
    );

    let nr = NoiseReducer::new(sample_rate as usize);
    let (event_sender, mut event_receiver) = broadcast::channel(128);

    let mut option = VADOption::default();
    option.r#type = if args.vad.to_lowercase() == "ten" {
        VadType::Ten
    } else {
        VadType::Silero
    };
    option.voice_threshold = args.threshold;
    option.speech_padding = args.speech_pad;
    option.silence_padding = args.silence_pad;
    option.r#type = if args.vad == "ten" {
        VadType::Ten
    } else {
        VadType::Silero
    };

    println!("VAD Config: {:?}", option);

    let token = CancellationToken::new();
    let vad = VadProcessor::create(token, event_sender, option)?;

    let mut out_file = if let Some(path) = args.output {
        Some(File::create(path)?)
    } else {
        None
    };

    let frame_size = 320; // 20ms at 16k
    let _chunk_duration_ms = (frame_size as u64 * 1000) / sample_rate as u64;

    let mut processed_samples_count = 0;

    for chunk in all_samples.chunks(frame_size) {
        let mut chunk_vec = chunk.to_vec();
        if chunk_vec.len() < frame_size {
            chunk_vec.resize(frame_size, 0);
        }

        let mut frame = AudioFrame {
            track_id: "demo".to_string(),
            samples: Samples::PCM { samples: chunk_vec },
            sample_rate,
            timestamp: (processed_samples_count * 1000) / sample_rate as u64,
        };
        processed_samples_count += frame_size as u64;

        // Denoise
        nr.process_frame(&mut frame)?;

        // VAD
        vad.process_frame(&mut frame)?;

        if let Some(file) = &mut out_file {
            if let Samples::PCM { samples } = &frame.samples {
                file.write_all(&samples_to_bytes(samples))?;
            }
        }
    }

    // Flush VAD with silence to catch any final segment
    for _ in 1..=10 {
        let mut frame = AudioFrame {
            track_id: "demo".to_string(),
            samples: Samples::PCM {
                samples: vec![0; frame_size],
            },
            sample_rate,
            timestamp: (processed_samples_count * 1000) / sample_rate as u64,
        };
        processed_samples_count += frame_size as u64;
        vad.process_frame(&mut frame)?;
    }

    // Process events
    println!("\nDetected Speech Segments:");
    use active_call::event::SessionEvent;

    let mut segment_count = 0;
    while let Ok(event) = event_receiver.try_recv() {
        match event {
            SessionEvent::Speaking { start_time, .. } => {
                print!("  Start: {:.3}s", start_time as f64 / 1000.0);
            }
            SessionEvent::Silence {
                start_time,
                duration,
                ..
            } => {
                println!(
                    " - End: {:.3}s (Duration: {:.3}s)",
                    (start_time + duration) as f64 / 1000.0,
                    duration as f64 / 1000.0
                );
                segment_count += 1;
            }
            _ => {}
        }
    }
    println!("Total segments: {}", segment_count);

    Ok(())
}
