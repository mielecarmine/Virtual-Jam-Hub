//! Desktop setup GUI, hardware loopback, or bidirectional Opus over UDP.

mod audio;
mod cli;
mod codec;
mod controller;
mod gui;
mod jitter;
mod mixer;
mod monitor;
mod network;
mod scheduling;
mod session_report;
mod setup;
mod streams;
#[cfg(feature = "telemetry-log")]
mod telemetry_log;

use std::{
    error::Error,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use audio::{RING_CAPACITY, Stats};
use cpal::traits::{DeviceTrait, StreamTrait};
use rtrb::RingBuffer;

fn main() -> Result<(), Box<dyn Error>> {
    if std::env::args_os().len() == 1 {
        return gui::run();
    }
    let Some(options) = cli::Options::parse(std::env::args().skip(1))? else {
        return Ok(());
    };
    if options.synthetic {
        return run_synthetic(&options);
    }
    run_cli(&options)
}

fn run_cli(options: &cli::Options) -> Result<(), Box<dyn Error>> {
    let host = cli::host(options.host.as_deref())?;
    if options.list {
        return cli::list_devices(&host);
    }
    let (input, output) = cli::devices(&host, options.input.as_deref(), options.output.as_deref())?;
    let input_config = audio::select_input_config(input.supported_input_configs()?)
        .ok_or_else(|| io::Error::other("Input does not support 48000 Hz / 128 frames"))?;
    let output_config = audio::select_output_config(output.supported_output_configs()?)
        .ok_or_else(|| io::Error::other("Output does not support 48000 Hz / 128 frames"))?;
    describe_audio(
        &host,
        &input,
        &output,
        &input_config,
        &output_config,
        options.duration,
    )?;

    let network = options
        .server
        .map(|server| {
            network::Network::connect_with_bind(server, &options.nickname, options.bind_ip)
                .map(|network| network.configure(options.network))
        })
        .transpose()?;
    let network_mode = network.is_some();

    // Allocate shared state and ring storage before starting either audio stream.
    let stats = Arc::new(Stats::default());
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || signal_stop.store(true, Ordering::Relaxed))?;
    let (mic_producer, mut mic_consumer) = RingBuffer::<f32>::new(RING_CAPACITY);
    let (mut speaker_producer, speaker_consumer) = RingBuffer::<f32>::new(RING_CAPACITY);
    let (rx, mut mixer) = mixer::channels(crate::audio::BUFFER_FRAMES);
    let monitor = network_mode.then(|| mixer.enable_local_monitor());
    let input_stream = streams::input(
        &input,
        &input_config,
        mic_producer,
        monitor,
        Arc::clone(&stats),
    )?;
    let output_stream = streams::output(
        &output,
        &output_config,
        if network_mode {
            streams::Playback::Mix(Box::new(mixer))
        } else {
            streams::Playback::Loopback {
                consumer: speaker_consumer,
                started: false,
            }
        },
        Arc::clone(&stats),
    )?;

    // Stop and join the scoped worker even if stream startup fails.
    let worker_stop = AtomicBool::new(false);
    let result = thread::scope(|scope| -> Result<bool, Box<dyn Error>> {
        let worker_stop = &worker_stop;
        let worker = thread::Builder::new()
            .name(
                if network_mode {
                    "udp-network"
                } else {
                    "local-loopback"
                }
                .into(),
            )
            .spawn_scoped(scope, move || -> io::Result<()> {
                if let Some(network) = network {
                    let result = network.run(mic_consumer, rx, worker_stop);
                    worker_stop.store(true, Ordering::Relaxed);
                    return result;
                }
                while !worker_stop.load(Ordering::Relaxed) {
                    audio::bridge(&mut mic_consumer, &mut speaker_producer);
                    thread::sleep(Duration::from_micros(250));
                }
                Ok(())
            })?;
        let result = run_streams(
            &input_stream,
            &output_stream,
            options.duration,
            &stats,
            &stop,
            network_mode.then_some(worker_stop),
        );
        // Stop callbacks before stopping their producer to avoid shutdown-only underruns.
        drop(output_stream);
        drop(input_stream);
        worker_stop.store(true, Ordering::Relaxed);
        worker
            .join()
            .map_err(|_| io::Error::other("Audio worker panicked"))??;
        result
    });
    report(&stats);
    let completed = result?;
    finish(&stats, network_mode, completed)
}

fn finish(stats: &Stats, network_mode: bool, completed: bool) -> Result<(), Box<dyn Error>> {
    let hardware_failed = if network_mode {
        stats.has_hardware_errors()
    } else {
        stats.has_errors() || !stats.playback_started.load(Ordering::Relaxed)
    };
    if hardware_failed {
        return Err(io::Error::other("Hardware check failed; see counters above").into());
    }
    if completed {
        println!("Requested duration completed. Confirm by listening that no glitches occurred.");
    } else {
        println!("Stopped early: the requested duration was not completed.");
    }
    Ok(())
}

fn describe_audio(
    host: &cpal::Host,
    input: &cpal::Device,
    output: &cpal::Device,
    input_config: &audio::AudioConfig,
    output_config: &audio::AudioConfig,
    duration: Duration,
) -> Result<(), Box<dyn Error>> {
    println!("Backend: {}", host.id().name());
    println!(
        "Input: {} ({} channels, {})",
        input.name()?,
        input_config.stream.channels,
        input_config.format
    );
    println!(
        "Output: {} ({} channels, {})",
        output.name()?,
        output_config.stream.channels,
        output_config.format
    );
    println!(
        "48000 Hz, 128 frames; internal f32; channel 1 -> all outputs; 4096-sample SPSC rings."
    );
    println!(
        "Use headphones. Running for {} seconds; Ctrl+C stops early.",
        duration.as_secs()
    );

    Ok(())
}

fn run_streams(
    input: &cpal::Stream,
    output: &cpal::Stream,
    duration: Duration,
    stats: &Stats,
    stop: &AtomicBool,
    network_stop: Option<&AtomicBool>,
) -> Result<bool, Box<dyn Error>> {
    output.play()?;
    input.play()?;
    let should_stop = || {
        stop.load(Ordering::Relaxed)
            || network_stop.is_some_and(|stop| stop.load(Ordering::Relaxed))
            || if network_stop.is_some() {
                stats.has_hardware_errors()
            } else {
                stats.has_errors()
            }
    };
    let startup = Instant::now();
    while network_stop.is_none() && !stats.playback_started.load(Ordering::Relaxed) {
        if should_stop() {
            return Ok(false);
        }
        if startup.elapsed() >= Duration::from_secs(5) {
            return Err(io::Error::other("No audio received within 5 seconds").into());
        }
        thread::sleep(Duration::from_millis(1));
    }
    // Loopback duration starts at playback; network mode may wait for peers silently.
    let start = Instant::now();
    while start.elapsed() < duration {
        if should_stop() {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(true)
}

fn report(stats: &Stats) {
    println!(
        "Callbacks input/output: {}/{}; dropped input samples: {}; silent output samples: {}; \
         cpal errors input/output: {}/{}; unexpected buffer sizes: {}",
        stats.input_callbacks.load(Ordering::Relaxed),
        stats.output_callbacks.load(Ordering::Relaxed),
        stats.input_overflows.load(Ordering::Relaxed),
        stats.output_underflows.load(Ordering::Relaxed),
        stats.input_errors.load(Ordering::Relaxed),
        stats.output_errors.load(Ordering::Relaxed),
        stats.wrong_buffer_sizes.load(Ordering::Relaxed),
    );
}

fn run_synthetic(options: &cli::Options) -> Result<(), Box<dyn Error>> {
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || signal_stop.store(true, Ordering::Relaxed))?;
    let (stats, completed) = run_synthetic_session(options, &stop)?;
    report(&stats);
    finish(&stats, true, completed)
}

#[allow(clippy::too_many_lines)]
fn run_synthetic_session(
    options: &cli::Options,
    stop: &AtomicBool,
) -> Result<(Stats, bool), Box<dyn Error>> {
    #[cfg(not(feature = "telemetry-log"))]
    if options.telemetry_dir.is_some() {
        return Err(io::Error::other("--telemetry-dir requires --features telemetry-log").into());
    }
    let server = options
        .server
        .ok_or_else(|| io::Error::other("--synthetic requires --server IP:PORT"))?;
    let frames = usize::try_from(options.buffer_frames).unwrap_or(audio::BUFFER_FRAMES);
    println!(
        "Backend: synthetic-headless\nInput: 400 Hz sine (1 channel, f32)\n\
         Output: in-memory mixer (1 channel, f32)\n\
         48000 Hz, {} frames; internal f32; 4096-sample SPSC rings.\n\
         Running for {} seconds; Ctrl+C stops early.",
        options.buffer_frames,
        options.duration.as_secs()
    );
    let mut network =
        network::Network::connect_with_bind(server, &options.nickname, options.bind_ip)?
            .configure(options.network);
    let _priority = scheduling::controller().ok();
    let stats = Stats::default();
    let (mut mic_producer, mut mic_consumer) = RingBuffer::<f32>::new(RING_CAPACITY);
    let (mut rx, mut mixer) = mixer::channels(frames);
    let mut monitor = mixer.enable_local_monitor();
    #[cfg(feature = "telemetry-log")]
    let mut log: Option<telemetry_log::AsyncSessionLog> = None;
    #[cfg(feature = "telemetry-log")]
    let (events_tx, _events_rx) = crossbeam_channel::unbounded();
    let mut remote_peers = 0_u32;
    let mut session_report: Option<(
        session_report::SessionReportAccumulator,
        session_report::ReportTarget,
    )> = None;

    let callback_period =
        Duration::from_nanos((u64::from(options.buffer_frames) * 1_000_000_000) / 48_000);
    let start = Instant::now();
    let mut next_callback = start;
    let mut sample_index = 0_u64;
    let mut output_block = [0.0_f32; 256];
    let mut connected = false;

    while !stop.load(Ordering::Relaxed) && start.elapsed() < options.duration {
        let now = Instant::now();
        while now >= next_callback {
            for _ in 0..frames {
                #[allow(clippy::cast_precision_loss)]
                let phase = (sample_index % 120) as f32 * std::f32::consts::TAU / 120.0;
                sample_index = sample_index.wrapping_add(1);
                let value = phase.sin() * 0.2;
                monitor.push(value);
                if mic_producer.push(value).is_err()
                    && stats.playback_started.load(Ordering::Relaxed)
                {
                    stats.input_overflows.fetch_add(1, Ordering::Relaxed);
                }
            }
            stats.input_callbacks.fetch_add(1, Ordering::Relaxed);
            mixer.playback(&mut output_block[..frames], 1, &stats);
            next_callback += callback_period;
        }
        network.tick(&mut mic_consumer, &mut rx, Instant::now())?;
        if !connected && network.is_connected() {
            connected = true;
            if let Some(target) = options.report_target() {
                session_report = Some((
                    session_report::SessionReportAccumulator::start(
                        session_report::SessionMetadata {
                            audio_backend: "synthetic-headless".into(),
                            sample_rate_hz: jam_protocol::SAMPLE_RATE,
                            buffer_frames: options.buffer_frames,
                            peer_id: network.local_peer_id(),
                        },
                    ),
                    target,
                ));
            }
            #[cfg(feature = "telemetry-log")]
            if let Some(dir) = &options.telemetry_dir {
                let preferences = setup::Preferences {
                    nickname: options.nickname.clone(),
                    server: server.to_string(),
                    host: Some("synthetic-headless".into()),
                    input: Some("400 Hz sine (synthetic)".into()),
                    output: Some("in-memory mixer (synthetic)".into()),
                    buffer_frames: options.buffer_frames,
                    ..setup::Preferences::default()
                };
                let audio_info = telemetry_log::AudioInfo {
                    input_channels: 1,
                    output_channels: 1,
                    input_sample_rate: jam_protocol::SAMPLE_RATE,
                    output_sample_rate: jam_protocol::SAMPLE_RATE,
                    last_output_frames: options.buffer_frames,
                };
                log = Some(telemetry_log::AsyncSessionLog::start(
                    &dir.join("client.toml"),
                    &preferences,
                    audio_info,
                    events_tx.clone(),
                )?);
            }
            if let Some(bpm) = options.metronome_bpm {
                network.update_metronome(jam_protocol::MetronomeConfig {
                    enabled: true,
                    bpm,
                    beats_per_bar: 4,
                    leader_peer_id: network.local_peer_id().unwrap_or(0),
                })?;
            }
        }
        if let Some(roster) = network.take_roster() {
            remote_peers = u32::try_from(roster.len()).unwrap_or(u32::MAX);
        }
        if let Some(mut metrics) = network.take_telemetry() {
            metrics.audio = stats.telemetry();
            metrics.latency = Some(network::telemetry::LatencyBudget::estimate(
                options.buffer_frames,
                metrics.audio.last_output_frames,
                network.codec_lookahead_samples,
                metrics.route_rtt_ms,
                metrics.buffer_ms,
            ));
            let xruns = [
                stats.input_overflows.load(Ordering::Relaxed),
                stats.output_underflows.load(Ordering::Relaxed),
            ];
            #[cfg(feature = "telemetry-log")]
            if let Some(writer) = log.as_mut() {
                writer.record(remote_peers, metrics, xruns);
            }
            if let Some((accumulator, _)) = session_report.as_mut() {
                accumulator.record(remote_peers, metrics, xruns);
            }
        }
        thread::sleep(Duration::from_micros(250));
    }
    let completed = start.elapsed() >= options.duration && !stop.load(Ordering::Relaxed);
    network.disconnect();
    #[cfg(feature = "telemetry-log")]
    drop(log);
    if let Some((accumulator, target)) = session_report.take() {
        if let Some(handle) = session_report::upload_async(accumulator.finalize(), target) {
            let _ = handle.join();
        }
    }
    Ok((stats, completed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_sessions_exchange_audio_and_record_telemetry() {
        let mut server = jam_server::Server::bind("127.0.0.1:0").unwrap();
        let endpoint = server.local_addr().unwrap();
        let stop_server = AtomicBool::new(false);
        #[cfg(feature = "telemetry-log")]
        let log_root = setup::tests::temporary_path("synthetic-beta");
        #[cfg(feature = "telemetry-log")]
        let log_dir = log_root.parent().unwrap().to_path_buf();

        thread::scope(|scope| {
            let server_worker = scope.spawn(|| {
                while !stop_server.load(Ordering::Relaxed) {
                    server.poll().unwrap();
                }
            });
            let client_a = scope.spawn(|| {
                #[allow(unused_mut)]
                let mut args = vec![
                    "--server".to_owned(),
                    endpoint.to_string(),
                    "--nickname".into(),
                    "tester-a".into(),
                    "--synthetic".into(),
                    "--buffer-frames".into(),
                    "64".into(),
                    "--simulate-delay-ms".into(),
                    "5".into(),
                    "--metronome-bpm".into(),
                    "120".into(),
                    "--seconds".into(),
                    "2".into(),
                ];
                #[cfg(feature = "telemetry-log")]
                {
                    args.push("--telemetry-dir".into());
                    args.push(log_dir.join("peer-a").to_string_lossy().into_owned());
                }
                let options = cli::Options::parse(args).unwrap().unwrap();
                let (stats, completed) =
                    run_synthetic_session(&options, &AtomicBool::new(false)).unwrap();
                assert!(completed);
                assert!(stats.input_callbacks.load(Ordering::Relaxed) > 0);
                assert!(stats.output_callbacks.load(Ordering::Relaxed) > 0);
                assert!(stats.playback_started.load(Ordering::Relaxed));
            });
            let client_b = scope.spawn(|| {
                #[allow(unused_mut)]
                let mut args = vec![
                    "--server".to_owned(),
                    endpoint.to_string(),
                    "--nickname".into(),
                    "tester-b".into(),
                    "--synthetic".into(),
                    "--buffer-frames".into(),
                    "256".into(),
                    "--simulate-delay-ms".into(),
                    "12".into(),
                    "--simulate-jitter-ms".into(),
                    "6".into(),
                    "--simulate-loss".into(),
                    "0.01".into(),
                    "--seconds".into(),
                    "2".into(),
                ];
                #[cfg(feature = "telemetry-log")]
                {
                    args.push("--telemetry-dir".into());
                    args.push(log_dir.join("peer-b").to_string_lossy().into_owned());
                }
                let options = cli::Options::parse(args).unwrap().unwrap();
                let (stats, completed) =
                    run_synthetic_session(&options, &AtomicBool::new(false)).unwrap();
                assert!(completed);
                assert!(stats.input_callbacks.load(Ordering::Relaxed) > 0);
                assert!(stats.output_callbacks.load(Ordering::Relaxed) > 0);
                assert!(stats.playback_started.load(Ordering::Relaxed));
            });
            client_a.join().unwrap();
            client_b.join().unwrap();
            stop_server.store(true, Ordering::Relaxed);
            server_worker.join().unwrap();
        });

        #[cfg(feature = "telemetry-log")]
        {
            for peer in ["peer-a", "peer-b"] {
                let dir = log_dir.join(peer);
                let file = std::fs::read_dir(&dir)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| path.extension().is_some_and(|ext| ext == "txt"))
                    .unwrap();
                let text = std::fs::read_to_string(file).unwrap();
                assert!(text.contains("backend=Some(\"synthetic-headless\")"));
                assert!(text.contains("Summary\nmetric\tsamples\tmin\tavg\tp95\tmax"));
                assert!(text.contains("telemetry_dropped_samples=0"));
            }
            let _ = std::fs::remove_dir_all(log_dir);
        }
    }
}
