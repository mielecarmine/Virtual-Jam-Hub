//! Allocation-free audio callbacks and the local stand-in for the network/mixer worker.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use cpal::{
    BufferSize, FromSample, Sample, SampleFormat, SampleRate, StreamConfig, SupportedBufferSize,
};
use rtrb::{Consumer, Producer};

pub const SAMPLE_RATE: u32 = 48_000;
pub const BUFFER_FRAMES: usize = 128;
pub const RING_CAPACITY: usize = 4096;
const PREFILL_SAMPLES: usize = 2 * BUFFER_FRAMES;

#[derive(Default)]
pub struct Stats {
    pub input_peak: AtomicU32,
    pub input_callbacks: AtomicU32,
    pub output_callbacks: AtomicU32,
    pub not_ready_samples: AtomicU32,
    pub align_dropped_samples: AtomicU32,
    pub last_output_frames: AtomicU32,
    pub input_overflows: AtomicU32,
    pub output_underflows: AtomicU32,
    pub input_errors: AtomicU32,
    pub output_errors: AtomicU32,
    pub wrong_buffer_sizes: AtomicU32,
    pub playback_started: AtomicBool,
}

/// Relaxed snapshots of RT counters; samples are mono frames summed across active peers.
// These values are exported by the optional session logger and exercised by RT tests.
#[cfg_attr(not(feature = "telemetry-log"), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioTelemetry {
    pub input_callbacks: u32,
    pub output_callbacks: u32,
    pub not_ready_samples: u32,
    pub align_dropped_samples: u32,
    pub last_output_frames: u32,
}

impl Stats {
    pub fn telemetry(&self) -> AudioTelemetry {
        AudioTelemetry {
            input_callbacks: self.input_callbacks.load(Ordering::Relaxed),
            output_callbacks: self.output_callbacks.load(Ordering::Relaxed),
            not_ready_samples: self.not_ready_samples.load(Ordering::Relaxed),
            align_dropped_samples: self.align_dropped_samples.load(Ordering::Relaxed),
            last_output_frames: self.last_output_frames.load(Ordering::Relaxed),
        }
    }

    pub fn has_hardware_errors(&self) -> bool {
        [
            &self.input_errors,
            &self.output_errors,
            &self.wrong_buffer_sizes,
        ]
        .iter()
        .any(|count| count.load(Ordering::Relaxed) != 0)
    }
    pub fn has_errors(&self) -> bool {
        [
            &self.input_overflows,
            &self.output_underflows,
            &self.input_errors,
            &self.output_errors,
            &self.wrong_buffer_sizes,
        ]
        .iter()
        .any(|count| count.load(Ordering::Relaxed) != 0)
    }
}

pub struct AudioConfig {
    pub stream: StreamConfig,
    pub format: SampleFormat,
}

pub fn select_input_config(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
) -> Option<AudioConfig> {
    select_input_config_for_buffer(configs, 128)
}

pub fn select_output_config(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
) -> Option<AudioConfig> {
    select_output_config_for_buffer(configs, 128)
}

pub fn select_input_config_for_buffer(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    frames: u32,
) -> Option<AudioConfig> {
    select_config_for_buffer(configs, frames, false)
}

pub fn select_output_config_for_buffer(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    frames: u32,
) -> Option<AudioConfig> {
    select_config_for_buffer(configs, frames, true)
}

fn select_config_for_buffer(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    frames: u32,
    output: bool,
) -> Option<AudioConfig> {
    configs
        .filter(|config| {
            config.channels() > 0
                && config.min_sample_rate().0 <= SAMPLE_RATE
                && config.max_sample_rate().0 >= SAMPLE_RATE
                && match config.buffer_size() {
                    SupportedBufferSize::Range { min, max } => *min <= frames && *max >= frames,
                    // Stream construction validates the request if the range is unknown.
                    SupportedBufferSize::Unknown => true,
                }
        })
        .min_by_key(|config| {
            (
                config.sample_format() != SampleFormat::F32,
                // Output: stereo first, then the smallest multi-out, then mono.
                output && config.channels() == 1,
                config.channels(),
            )
        })
        .map(|config| AudioConfig {
            format: config.sample_format(),
            stream: StreamConfig {
                channels: config.channels(),
                sample_rate: SampleRate(SAMPLE_RATE),
                buffer_size: BufferSize::Fixed(frames),
            },
        })
}

/// Keep channel 1 as the mono instrument signal. Drop whole frames on overflow.
pub fn capture<T: Sample>(
    data: &[T],
    channels: usize,
    mic_to_net: &mut Producer<f32>,
    mut monitor: Option<&mut crate::monitor::Capture>,
    stats: &Stats,
) where
    f32: FromSample<T>,
{
    stats.input_callbacks.fetch_add(1, Ordering::Relaxed);
    // ALSA may deliver part of the hardware buffer, or coalesce several periods.
    if channels == 0 || data.len() % channels != 0 {
        stats.wrong_buffer_sizes.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut peak = 0.0_f32;
    for frame in data.chunks_exact(channels) {
        let value = frame[0].to_sample::<f32>();
        if value.is_finite() {
            peak = peak.max(value.abs());
        }
        if let Some(monitor) = &mut monitor {
            monitor.push(value);
        }
        if mic_to_net.push(value).is_err() {
            stats.input_overflows.fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(monitor) = monitor {
        monitor.publish();
    }
    stats
        .input_peak
        .fetch_max(peak.to_bits(), Ordering::Relaxed);
}

/// Duplicate mono on all output channels; an empty ring produces silence.
pub fn playback<T: Sample + FromSample<f32>>(
    data: &mut [T],
    channels: usize,
    net_to_speaker: &mut Consumer<f32>,
    started: &mut bool,
    stats: &Stats,
) {
    stats.output_callbacks.fetch_add(1, Ordering::Relaxed);
    if channels == 0 || data.len() % channels != 0 {
        stats.wrong_buffer_sizes.fetch_add(1, Ordering::Relaxed);
        data.fill(T::EQUILIBRIUM);
        return;
    }
    stats.last_output_frames.store(
        u32::try_from(data.len() / channels).unwrap_or(u32::MAX),
        Ordering::Relaxed,
    );
    if !*started {
        if net_to_speaker.slots() < PREFILL_SAMPLES.max(data.len() / channels) {
            data.fill(T::EQUILIBRIUM);
            return;
        }
        *started = true;
        stats.playback_started.store(true, Ordering::Relaxed);
    }
    for frame in data.chunks_exact_mut(channels) {
        let sample = net_to_speaker.pop().unwrap_or_else(|_| {
            stats.output_underflows.fetch_add(1, Ordering::Relaxed);
            0.0
        });
        frame.fill(T::from_sample(sample));
    }
}

/// Retain unread input when the output ring is full.
pub fn bridge(mic_to_net: &mut Consumer<f32>, net_to_speaker: &mut Producer<f32>) -> usize {
    let count = mic_to_net.slots().min(net_to_speaker.slots());
    for _ in 0..count {
        if let Ok(sample) = mic_to_net.pop() {
            // This worker is the sole producer, so available capacity cannot shrink.
            let _ = net_to_speaker.push(sample);
        }
    }
    count
}

#[cfg(test)]
pub(crate) mod tests;
