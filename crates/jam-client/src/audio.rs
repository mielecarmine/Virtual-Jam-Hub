//! Allocation-free audio callbacks and the local stand-in for the network/mixer worker.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use cpal::{BufferSize, SampleFormat, SampleRate, StreamConfig, SupportedBufferSize};
use rtrb::{Consumer, Producer};

pub const SAMPLE_RATE: u32 = 48_000;
pub const BUFFER_FRAMES: usize = 128;
pub const RING_CAPACITY: usize = 4096;
const PREFILL_SAMPLES: usize = 2 * BUFFER_FRAMES;

#[derive(Default)]
pub struct Stats {
    pub input_callbacks: AtomicU32,
    pub output_callbacks: AtomicU32,
    pub input_overflows: AtomicU32,
    pub output_underflows: AtomicU32,
    pub input_errors: AtomicU32,
    pub output_errors: AtomicU32,
    pub wrong_buffer_sizes: AtomicU32,
    pub playback_started: AtomicBool,
}

impl Stats {
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

pub fn select_config(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
) -> Option<StreamConfig> {
    configs
        .filter(|config| {
            config.sample_format() == SampleFormat::F32
                && config.channels() > 0
                && config.min_sample_rate().0 <= SAMPLE_RATE
                && config.max_sample_rate().0 >= SAMPLE_RATE
                && match config.buffer_size() {
                    SupportedBufferSize::Range { min, max } => *min <= 128 && *max >= 128,
                    // Verify actual callback sizes when the backend cannot report its range.
                    SupportedBufferSize::Unknown => true,
                }
        })
        .min_by_key(cpal::SupportedStreamConfigRange::channels)
        .map(|config| StreamConfig {
            channels: config.channels(),
            sample_rate: SampleRate(SAMPLE_RATE),
            buffer_size: BufferSize::Fixed(128),
        })
}

/// Keep channel 1 as the mono instrument signal. Drop whole frames on overflow.
pub fn capture(data: &[f32], channels: usize, mic_to_net: &mut Producer<f32>, stats: &Stats) {
    stats.input_callbacks.fetch_add(1, Ordering::Relaxed);
    if data.len() != BUFFER_FRAMES * channels {
        stats.wrong_buffer_sizes.fetch_add(1, Ordering::Relaxed);
        return;
    }
    for frame in data.chunks_exact(channels) {
        if mic_to_net.push(frame[0]).is_err() {
            stats.input_overflows.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Duplicate mono on all output channels; an empty ring produces silence.
pub fn playback(
    data: &mut [f32],
    channels: usize,
    net_to_speaker: &mut Consumer<f32>,
    started: &mut bool,
    stats: &Stats,
) {
    stats.output_callbacks.fetch_add(1, Ordering::Relaxed);
    if data.len() != BUFFER_FRAMES * channels {
        stats.wrong_buffer_sizes.fetch_add(1, Ordering::Relaxed);
        data.fill(0.0);
        return;
    }
    if !*started {
        if net_to_speaker.slots() < PREFILL_SAMPLES {
            data.fill(0.0);
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
        frame.fill(sample);
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
mod tests;
