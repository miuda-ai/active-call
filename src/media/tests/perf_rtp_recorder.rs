#[cfg(test)]
mod tests {
    use crate::media::{
        AudioFrame, Samples,
        recorder::{Recorder, RecorderOption},
    };
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    // Simulate RTP forwarding loop
    async fn run_rtp_forwarding(
        total_packets: usize,
        payload: Vec<u8>,
        recorder_tx: Option<mpsc::UnboundedSender<AudioFrame>>,
    ) -> Duration {
        let start = Instant::now();

        for i in 0..total_packets {
            // Simulate receiving an RTP packet
            let frame = AudioFrame {
                track_id: "track_1".to_string(),
                samples: Samples::RTP {
                    sequence_number: i as u16,
                    payload_type: 0, // PCMU
                    payload: payload.clone(),
                },
                timestamp: (i * 160) as u64,
                sample_rate: 8000,
                channels: 1,
                ..Default::default()
            };

            // Simulate forwarding logic (e.g. sending to another track)
            // In a real scenario, this would involve network I/O.
            // Here we just clone the frame to simulate the overhead of handling it.
            let _forwarded_frame = frame.clone();

            // If recording is enabled, send to recorder
            if let Some(tx) = &recorder_tx {
                tx.send(frame).ok();
            }
        }

        start.elapsed()
    }

    use std::fs::File;
    use std::io::BufWriter;
    use std::io::Write;

    // Simulate Buffered Write manually to test potential improvement
    async fn run_buffered_write_simulation(total_packets: usize, payload: Vec<u8>) -> Duration {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("bench_buffered.wav");
        let file = File::create(file_path).unwrap();
        let mut writer = BufWriter::new(file);

        let start = Instant::now();
        for _ in 0..total_packets {
            // Simulate receiving
            writer.write_all(&payload).unwrap();
            // In real async recorder, we might flush periodically or let BufWriter handle it
        }
        writer.flush().unwrap();
        start.elapsed()
    }

    #[tokio::test]
    async fn test_rtp_forwarding_overhead() {
        let duration_sec = 60;
        let packet_ms = 20;
        let total_packets = (duration_sec * 1000) / packet_ms;
        let payload = vec![0u8; 160]; // 20ms PCMU payload

        println!(
            "Benchmark: RTP Forwarding ({}s, {} packets)",
            duration_sec, total_packets
        );

        // 1. Baseline: No Recording
        let duration_no_rec = run_rtp_forwarding(total_packets, payload.clone(), None).await;
        println!("Without Recording:       {:?}", duration_no_rec);

        // 2. With Recording (Current Implementation - Unbuffered Async File)
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join("bench_rtp.wav");
        let cancel_token = CancellationToken::new();
        let config = RecorderOption::default();

        let recorder = Arc::new(Recorder::new(
            cancel_token.clone(),
            "bench_rtp".to_string(),
            config,
        ));

        let (tx, rx) = mpsc::unbounded_channel();
        let recorder_clone = recorder.clone();
        let file_path_clone = file_path.clone();

        let record_handle =
            tokio::spawn(
                async move { recorder_clone.process_recording(&file_path_clone, rx).await },
            );

        let duration_with_rec = run_rtp_forwarding(total_packets, payload.clone(), Some(tx)).await;
        println!("With Recording (Async):  {:?}", duration_with_rec);

        record_handle.await.unwrap().unwrap();

        // 3. Buffered Write Simulation (Sync for comparison)
        let duration_buffered = run_buffered_write_simulation(total_packets, payload.clone()).await;
        println!("Buffered Write (Sync):   {:?}", duration_buffered);

        // Calculate overhead
        let overhead = if duration_with_rec > duration_no_rec {
            duration_with_rec - duration_no_rec
        } else {
            Duration::from_secs(0)
        };

        println!("Async Recording Overhead: {:?}", overhead);
        println!(
            "Overhead per packet:      {:?}",
            overhead / total_packets as u32
        );

        // Verify file size
        let metadata = std::fs::metadata(&file_path).unwrap();
        let expected_size = 44 + (total_packets * 160) as u64;
        assert_eq!(
            metadata.len(),
            expected_size,
            "Recording file size mismatch"
        );
    }
}
