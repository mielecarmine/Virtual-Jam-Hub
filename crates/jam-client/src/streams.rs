//! Build typed hardware streams while keeping both SPSC rings in `f32`.

use std::sync::{Arc, atomic::Ordering};

use cpal::{FromSample, SampleFormat, SizedSample, traits::DeviceTrait};
use rtrb::{Consumer, Producer};

use crate::audio::{self, AudioConfig, Stats};

// Dispatch only at startup, never within the audio callbacks.
macro_rules! dispatch_format {
    ($format:expr, $builder:ident, $($arg:expr),+) => {
        match $format {
            SampleFormat::I8 => $builder::<i8>($($arg),+),
            SampleFormat::I16 => $builder::<i16>($($arg),+),
            SampleFormat::I32 => $builder::<i32>($($arg),+),
            SampleFormat::I64 => $builder::<i64>($($arg),+),
            SampleFormat::U8 => $builder::<u8>($($arg),+),
            SampleFormat::U16 => $builder::<u16>($($arg),+),
            SampleFormat::U32 => $builder::<u32>($($arg),+),
            SampleFormat::U64 => $builder::<u64>($($arg),+),
            SampleFormat::F32 => $builder::<f32>($($arg),+),
            SampleFormat::F64 => $builder::<f64>($($arg),+),
            _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
        }
    };
}

pub fn input(
    device: &cpal::Device,
    config: &AudioConfig,
    producer: Producer<f32>,
    monitor: Option<crate::monitor::Capture>,
    stats: Arc<Stats>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    dispatch_format!(
        config.format,
        build_input,
        device,
        config,
        producer,
        monitor,
        stats
    )
}

fn build_input<T: SizedSample>(
    device: &cpal::Device,
    config: &AudioConfig,
    mut producer: Producer<f32>,
    mut monitor: Option<crate::monitor::Capture>,
    stats: Arc<Stats>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    f32: FromSample<T>,
{
    let errors = Arc::clone(&stats);
    let channels = usize::from(config.stream.channels);
    device.build_input_stream(
        &config.stream,
        move |data: &[T], _| {
            audio::capture(data, channels, &mut producer, monitor.as_mut(), &stats);
        },
        move |_| {
            errors.input_errors.fetch_add(1, Ordering::Relaxed);
        },
        None,
    )
}

pub enum Playback {
    Loopback {
        consumer: Consumer<f32>,
        started: bool,
    },
    Mix(Box<crate::mixer::Mixer>),
}

pub fn output(
    device: &cpal::Device,
    config: &AudioConfig,
    playback: Playback,
    stats: Arc<Stats>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    dispatch_format!(config.format, build_output, device, config, playback, stats)
}

fn build_output<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &AudioConfig,
    mut playback: Playback,
    stats: Arc<Stats>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let errors = Arc::clone(&stats);
    let channels = usize::from(config.stream.channels);
    device.build_output_stream(
        &config.stream,
        move |data: &mut [T], _| match &mut playback {
            Playback::Loopback { consumer, started } => {
                audio::playback(data, channels, consumer, started, &stats);
            }
            Playback::Mix(mixer) => mixer.playback(data, channels, &stats),
        },
        move |_| {
            errors.output_errors.fetch_add(1, Ordering::Relaxed);
        },
        None,
    )
}
