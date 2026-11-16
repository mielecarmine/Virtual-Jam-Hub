//! Hardware loopback or bidirectional Opus over UDP.

mod audio;
mod cli;
mod codec;
mod jitter;
mod mixer;
mod network;
mod streams;

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
    let Some(options) = cli::Options::parse(std::env::args().skip(1))? else {
        return Ok(());
    };
    let host = cli::host(options.host.as_deref())?;
    if options.list {
        return cli::list_devices(&host);
    }
    let (input, output) = cli::devices(&host, options.input.as_deref(), options.output.as_deref())?;
    let input_config = audio::select_config(input.supported_input_configs()?)
        .ok_or_else(|| io::Error::other("Input does not support 48000 Hz / 128 frames"))?;
    let output_config = audio::select_config(output.supported_output_configs()?)
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
            network::Network::connect(server, &options.nickname)
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
    let (rx, mixer) = mixer::channels();
    let input_stream = streams::input(&input, &input_config, mic_producer, Arc::clone(&stats))?;
    let output_stream = streams::output(
        &output,
        &output_config,
        if network_mode {
            streams::Playback::Mix(mixer)
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
