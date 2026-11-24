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
    pub mic_peak: AtomicU32,
    pub mic_tx_mute: AtomicBool,
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

/// Validated, zero-based routing, prepared before starting hardware callbacks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputRouting {
    instrument: u16,
    mic: Option<u16>,
}

impl InputRouting {
    pub fn new(instrument: u16, mic: Option<u16>) -> Result<Self, &'static str> {
        if instrument == 0 || mic == Some(0) {
            return Err("Audio channels must be greater than zero.");
        }
        if mic == Some(instrument) {
            return Err("Instrument and microphone must use different channels.");
        }
        Ok(Self {
            instrument: instrument - 1,
            mic: mic.map(|channel| channel - 1),
        })
    }

    pub fn required_channels(self) -> u16 {
        self.instrument.max(self.mic.unwrap_or(0)) + 1
    }

    pub fn mic_enabled(self) -> bool {
        self.mic.is_some()
    }
}

#[allow(dead_code)] // Compatibility selector for the default mono routing.
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
    select_input_config_for_channels(configs, frames, 1)
}

pub fn select_input_config_for_channels(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    frames: u32,
    required_channels: u16,
) -> Option<AudioConfig> {
    select_config_for_buffer(configs, frames, false, required_channels.max(1))
}

pub fn select_output_config_for_buffer(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    frames: u32,
) -> Option<AudioConfig> {
    select_config_for_buffer(configs, frames, true, 1)
}

fn select_config_for_buffer(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    frames: u32,
    output: bool,
    required_channels: u16,
) -> Option<AudioConfig> {
    configs
        .filter(|config| {
            config.channels() >= required_channels
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

/// Capture independent local sources and one mono TX mix. Drop whole frames on overflow.
pub fn capture<T: Sample>(
    data: &[T],
    channels: usize,
    routing: InputRouting,
    mic_to_net: &mut Producer<f32>,
    mut monitor: Option<&mut crate::monitor::Capture>,
    stats: &Stats,
) where
    f32: FromSample<T>,
{
    stats.input_callbacks.fetch_add(1, Ordering::Relaxed);
    // ALSA may deliver part of the hardware buffer, or coalesce several periods.
    if channels < usize::from(routing.required_channels()) || data.len() % channels != 0 {
        stats.wrong_buffer_sizes.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut peak = 0.0_f32;
    let mut mic_peak = 0.0_f32;
    let mic_tx_mute = stats.mic_tx_mute.load(Ordering::Relaxed);
    for frame in data.chunks_exact(channels) {
        let finite = |value: f32| if value.is_finite() { value } else { 0.0 };
        let value = finite(frame[usize::from(routing.instrument)].to_sample::<f32>());
        let mic = routing.mic.map_or(0.0, |index| {
            finite(frame[usize::from(index)].to_sample::<f32>())
        });
        peak = peak.max(value.abs());
        mic_peak = mic_peak.max(mic.abs());
        if let Some(monitor) = &mut monitor {
            monitor.push_sources(value, mic);
        }
        let tx = if mic_tx_mute { value } else { value + mic };
        if mic_to_net.push(tx).is_err() {
            stats.input_overflows.fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(monitor) = monitor {
        monitor.publish();
    }
    stats
        .input_peak
        .fetch_max(peak.to_bits(), Ordering::Relaxed);
    stats
        .mic_peak
        .fetch_max(mic_peak.to_bits(), Ordering::Relaxed);
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
