//! Local hardware loopback: capture -> `mic_to_net` -> worker -> `net_to_speaker` -> playback.

mod audio;

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
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::RingBuffer;

fn main() -> Result<(), Box<dyn Error>> {
    let Some(duration) = parse_duration()? else {
        return Ok(());
    };
    let host = cpal::default_host();
    let input = host.default_input_device().ok_or_else(|| {
        io::Error::other("No default audio input device. Connect an audio interface first.")
    })?;
    let output = host.default_output_device().ok_or_else(|| {
        io::Error::other("No default audio output device. Connect an audio interface first.")
    })?;
    let input_config = audio::select_config(input.supported_input_configs()?)
        .ok_or_else(|| io::Error::other("Input does not support f32 / 48000 Hz / 128 frames"))?;
    let output_config = audio::select_config(output.supported_output_configs()?)
        .ok_or_else(|| io::Error::other("Output does not support f32 / 48000 Hz / 128 frames"))?;

    println!(
        "Input: {} ({} channels)",
        input.name()?,
        input_config.channels
    );
    println!(
        "Output: {} ({} channels)",
        output.name()?,
        output_config.channels
    );
    println!("48000 Hz, f32, 128 frames; channel 1 -> all outputs; two 4096-sample rings.");
    println!(
        "Use headphones. Running for {} seconds; Ctrl+C stops early.",
        duration.as_secs()
    );

    // Allocate shared state and ring storage before starting either audio stream.
    let stats = Arc::new(Stats::default());
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || signal_stop.store(true, Ordering::Relaxed))?;
    let (mut mic_producer, mut mic_consumer) = RingBuffer::<f32>::new(RING_CAPACITY);
    let (mut speaker_producer, mut speaker_consumer) = RingBuffer::<f32>::new(RING_CAPACITY);

    let input_stats = Arc::clone(&stats);
    let input_errors = Arc::clone(&stats);
    let input_channels = usize::from(input_config.channels);
    let input_stream = input.build_input_stream(
        &input_config,
        move |data: &[f32], _| {
            audio::capture(data, input_channels, &mut mic_producer, &input_stats);
        },
        move |_| {
            input_errors.input_errors.fetch_add(1, Ordering::Relaxed);
        },
        None,
    )?;
    let output_stats = Arc::clone(&stats);
    let output_errors = Arc::clone(&stats);
    let output_channels = usize::from(output_config.channels);
    let mut started = false;
    let output_stream = output.build_output_stream(
        &output_config,
        move |data: &mut [f32], _| {
            audio::playback(
                data,
                output_channels,
                &mut speaker_consumer,
                &mut started,
                &output_stats,
            );
        },
        move |_| {
            output_errors.output_errors.fetch_add(1, Ordering::Relaxed);
        },
        None,
    )?;

    // Stop and join the scoped worker even if stream startup fails.
    let worker_stop = AtomicBool::new(false);
    let result = thread::scope(|scope| -> Result<bool, Box<dyn Error>> {
        let worker_stop = &worker_stop;
        let worker = thread::Builder::new()
            .name("local-loopback".into())
            .spawn_scoped(scope, move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    audio::bridge(&mut mic_consumer, &mut speaker_producer);
                    thread::sleep(Duration::from_micros(250));
                }
            })?;
        let result = run_streams(&input_stream, &output_stream, duration, &stats, &stop);
        // Stop callbacks before stopping their producer to avoid shutdown-only underruns.
        drop(output_stream);
        drop(input_stream);
        worker_stop.store(true, Ordering::Relaxed);
        worker
            .join()
            .map_err(|_| io::Error::other("Loopback worker panicked"))?;
        result
    });
    report(&stats);
    let completed = result?;
    if stats.has_errors() || !stats.playback_started.load(Ordering::Relaxed) {
        return Err(io::Error::other("Hardware check failed; see counters above").into());
    }
    if completed {
        println!("Requested duration completed. Confirm by listening that no glitches occurred.");
    } else {
        println!("Stopped early: the requested duration was not completed.");
    }
    Ok(())
}

fn run_streams(
    input: &cpal::Stream,
    output: &cpal::Stream,
    duration: Duration,
    stats: &Stats,
    stop: &AtomicBool,
) -> Result<bool, Box<dyn Error>> {
    output.play()?;
    input.play()?;
    let startup = Instant::now();
    while !stats.playback_started.load(Ordering::Relaxed) {
        if stop.load(Ordering::Relaxed) || stats.has_errors() {
            return Ok(false);
        }
        if startup.elapsed() >= Duration::from_secs(5) {
            return Err(io::Error::other("No audio received within 5 seconds").into());
        }
        thread::sleep(Duration::from_millis(1));
    }
    // Count the acceptance interval only after the first real output frame.
    let start = Instant::now();
    while start.elapsed() < duration {
        if stop.load(Ordering::Relaxed) || stats.has_errors() {
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

fn parse_duration() -> Result<Option<Duration>, Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => Ok(Some(Duration::from_secs(300))),
        Some("--help" | "-h") => {
            println!(
                "Usage: jam-client [--seconds N]\nDefault: 300 seconds of local hardware loopback."
            );
            Ok(None)
        }
        Some("--seconds") => {
            let seconds: u64 = args
                .next()
                .ok_or("--seconds requires a positive integer")?
                .parse()?;
            if seconds == 0 || args.next().is_some() {
                return Err("Usage: jam-client [--seconds N], N > 0".into());
            }
            Ok(Some(Duration::from_secs(seconds)))
        }
        Some(_) => Err("Usage: jam-client [--seconds N]".into()),
    }
}
