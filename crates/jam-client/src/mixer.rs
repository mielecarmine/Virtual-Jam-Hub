//! Fixed peer slots and SPSC queues, allocated before the hardware streams start.
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

use cpal::{FromSample, Sample};
use jam_protocol::MAX_PEERS;
use rtrb::{Consumer, Producer, RingBuffer};

use crate::{audio::Stats, codec::FRAME_SAMPLES};

mod metronome;
use metronome::Metronome;
pub use metronome::MetronomeControls;

// 15 ms scheduling reserve plus one production interval (2.5 ms) at 48 kHz.
pub(crate) const RX_TARGET_SAMPLES: usize = FRAME_SAMPLES * 7;

// 1 ms at 48 kHz; tolerate one Opus interval before rebuilding the reserve.
const FADE_SAMPLES: u16 = 48;
const STARVATION_SAMPLES: usize = FRAME_SAMPLES;

#[derive(Clone, Copy, Default)]
struct Continuity {
    last: f32,
    missing: usize,
    debt: usize,
    offset: f32,
    fade_in: u16,
}

impl Continuity {
    fn render(&mut self, sample: Option<f32>) -> f32 {
        let value = if let Some(value) = sample {
            let value = if value.is_finite() { value } else { 0.0 };
            if self.missing > 0 {
                self.offset = self.last - value;
                self.fade_in = FADE_SAMPLES;
                self.missing = 0;
            }
            self.fade_in = self.fade_in.saturating_sub(1);
            value + self.offset * f32::from(self.fade_in) / f32::from(FADE_SAMPLES)
        } else {
            let remaining = usize::from(FADE_SAMPLES).saturating_sub(self.missing);
            self.missing = self.missing.saturating_add(1).min(STARVATION_SAMPLES);
            self.fade_in = 0;
            if remaining > 0 {
                self.last * f32::from(u16::try_from(remaining - 1).unwrap_or(0))
                    / f32::from(u16::try_from(remaining).unwrap_or(FADE_SAMPLES))
            } else {
                0.0
            }
        };
        self.last = value;
        value
    }
}

fn queue_capacity(frames: usize) -> usize {
    (frames + RX_TARGET_SAMPLES).div_ceil(FRAME_SAMPLES) * FRAME_SAMPLES + FRAME_SAMPLES
}

pub struct Controls {
    pub peak: AtomicU32,
    /// Linear amplitude, encoded with `f32::to_bits()`. Nonfinite values are silenced.
    pub gain: AtomicU32,
    pub mute: AtomicBool,
    pub solo: AtomicBool,
    // Odd epochs are active; even epochs are inactive. One atomic avoids torn lifecycle state.
    epoch: AtomicU32,
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            peak: AtomicU32::new(0),
            gain: AtomicU32::new(1.0_f32.to_bits()),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            epoch: AtomicU32::new(0),
        }
    }
}

#[derive(Clone, Copy)]
pub struct QueuedSample {
    pub value: f32,
    epoch: u32,
}

pub struct RxChannels {
    pub rings: [Producer<QueuedSample>; MAX_PEERS],
    pub controls: Arc<[Controls; MAX_PEERS]>,
    pub local_control: Arc<Controls>,
    pub metronome: Arc<MetronomeControls>,
}

pub struct Mixer {
    pub(crate) rings: [Consumer<QueuedSample>; MAX_PEERS],
    pub controls: Arc<[Controls; MAX_PEERS]>,
    local_control: Arc<Controls>,
    local: Option<crate::monitor::Playback>,
    epochs: [u32; MAX_PEERS],
    ready: [bool; MAX_PEERS],
    continuity: [Continuity; MAX_PEERS],
    metronome: Metronome,
}

pub fn channels(frames: usize) -> (RxChannels, Mixer) {
    channels_with_capacity(queue_capacity(frames))
}

pub(crate) fn channels_with_capacity(capacity: usize) -> (RxChannels, Mixer) {
    let controls = Arc::new(std::array::from_fn(|_| Controls::default()));
    let local_control = Arc::new(Controls::default());
    let metronome = Arc::new(MetronomeControls::default());
    let pairs = std::array::from_fn::<_, MAX_PEERS, _>(|_| RingBuffer::new(capacity));
    let (producers, consumers): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
    let rings = producers.try_into().unwrap_or_else(|_| unreachable!());
    let rx = RxChannels {
        rings,
        controls: Arc::clone(&controls),
        local_control: Arc::clone(&local_control),
        metronome: Arc::clone(&metronome),
    };
    let rings = consumers.try_into().unwrap_or_else(|_| unreachable!());
    (
        rx,
        Mixer {
            rings,
            controls,
            local_control,
            local: None,
            epochs: [0; MAX_PEERS],
            ready: [false; MAX_PEERS],
            continuity: [Continuity::default(); MAX_PEERS],
            metronome: Metronome::new(metronome),
        },
    )
}

impl RxChannels {
    /// Whole Opus frames that fit within both the SPSC capacity and the latency budget.
    pub fn available_chunks(&mut self, id: u8) -> usize {
        let ring = &mut self.rings[usize::from(id) - 1];
        let free = ring.slots();
        free / FRAME_SAMPLES
    }

    /// IDs are validated by the protocol and mapped directly to fixed slots `0..MAX_PEERS`.
    pub fn push(&mut self, id: u8, chunk: &[f32]) -> bool {
        let index = usize::from(id) - 1;
        let control = &self.controls[index];
        let mut epoch = control.epoch.load(Ordering::Relaxed);
        if epoch % 2 == 0 {
            epoch = control
                .epoch
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1);
        }
        let ring = &mut self.rings[index];
        let free = ring.slots();
        if free >= chunk.len() {
            for &value in chunk {
                let _ = ring.push(QueuedSample { value, epoch });
            }
            true
        } else {
            false
        }
    }

    pub fn remove(&self, id: u8) {
        let control = &self.controls[usize::from(id) - 1];
        // Tagging invalidates queued audio even if the ID is reused before the next callback.
        if control.epoch.load(Ordering::Relaxed) % 2 != 0 {
            control.epoch.fetch_add(1, Ordering::Relaxed);
        }
        control.peak.store(0, Ordering::Relaxed);
        control.gain.store(1.0_f32.to_bits(), Ordering::Relaxed);
        control.mute.store(false, Ordering::Relaxed);
        control.solo.store(false, Ordering::Relaxed);
    }
}

impl Mixer {
    /// Allocate the direct monitoring path before starting the hardware callbacks.
    pub fn enable_local_monitor(&mut self) -> crate::monitor::Capture {
        let (capture, playback) = crate::monitor::channel(crate::audio::RING_CAPACITY);
        self.local = Some(playback);
        capture
    }

    fn align(&mut self, frames: usize, stats: &Stats) {
        // Only the consumer discards audio. Snapshot each queue so work stays bounded.
        // Keep the callback's demand AND the reserve, rather than consuming the reserve.
        let prefill = frames.saturating_add(RX_TARGET_SAMPLES);
        // Chunk quantization must never discard the tail that completed startup.
        let target = prefill.div_ceil(FRAME_SAMPLES) * FRAME_SAMPLES + FRAME_SAMPLES;
        for (index, ring) in self.rings.iter_mut().enumerate() {
            let epoch = self.controls[index].epoch.load(Ordering::Relaxed);
            if self.epochs[index] != epoch {
                self.epochs[index] = epoch;
                self.ready[index] = false;
                self.continuity[index] = Continuity::default();
            }
            // Old generations must not count towards a new peer's prefill.
            for _ in 0..ring.slots() {
                if ring
                    .peek()
                    .is_ok_and(|sample| sample.epoch != epoch || epoch % 2 == 0)
                {
                    let _ = ring.pop();
                    stats.align_dropped_samples.fetch_add(1, Ordering::Relaxed);
                } else {
                    break;
                }
            }
            let queued = ring.slots();
            if queued > target {
                for _ in 0..queued - target {
                    let _ = ring.pop();
                    stats.align_dropped_samples.fetch_add(1, Ordering::Relaxed);
                }
            }
            // Drivers may deliver a larger block than requested; keep startup reachable.
            let prefill = prefill.min(ring.buffer().capacity());
            if epoch % 2 != 0 && queued >= prefill {
                self.ready[index] = true;
            }
        }
    }

    fn local_sample(&mut self, solo: bool, stats: &Stats) -> f64 {
        let Some(local) = &mut self.local else {
            return 0.0;
        };
        // Always consume, including while muted or excluded by a remote solo.
        let sample = local.pop();
        if sample.is_some() {
            stats.playback_started.store(true, Ordering::Relaxed);
        }
        let value = sample.unwrap_or(0.0);
        let control = &self.local_control;
        let gain = f32::from_bits(control.gain.load(Ordering::Relaxed));
        if value.is_finite()
            && gain.is_finite()
            && !control.mute.load(Ordering::Relaxed)
            && (!solo || control.solo.load(Ordering::Relaxed))
        {
            f64::from(value) * f64::from(gain)
        } else {
            0.0
        }
    }

    pub fn playback<T: Sample + FromSample<f32>>(
        &mut self,
        data: &mut [T],
        channels: usize,
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
        if let Some(local) = &mut self.local {
            local.align(data.len() / channels);
        }
        self.align(data.len() / channels, stats);
        self.metronome.begin_block();
        let mut peaks = [0.0_f32; MAX_PEERS];
        for frame in data.chunks_exact_mut(channels) {
            let solo = (self.local.is_some() && self.local_control.solo.load(Ordering::Relaxed))
                || self.controls.iter().any(|c| {
                    c.epoch.load(Ordering::Relaxed) % 2 != 0 && c.solo.load(Ordering::Relaxed)
                });
            let mut sum = self.local_sample(solo, stats);
            for (index, ((ring, control), peak)) in self
                .rings
                .iter_mut()
                .zip(self.controls.iter())
                .zip(peaks.iter_mut())
                .enumerate()
            {
                let epoch = control.epoch.load(Ordering::Relaxed);
                if epoch != self.epochs[index] || epoch % 2 == 0 {
                    continue;
                }
                let continuity = &mut self.continuity[index];
                // Missing output advanced wall time, but not the source cursor. Retire
                // that debt before resuming so repeated glitches cannot add latency.
                if self.ready[index] {
                    for _ in 0..continuity.debt.min(ring.slots()) {
                        let _ = ring.pop();
                        continuity.debt -= 1;
                        stats.align_dropped_samples.fetch_add(1, Ordering::Relaxed);
                    }
                }
                // Bounded by the queue capacity, including stale samples after a disconnect.
                let mut sample = None;
                for _ in 0..if self.ready[index] { ring.slots() } else { 0 } {
                    if let Ok(queued) = ring.pop() {
                        if queued.epoch == epoch {
                            sample = Some(queued.value);
                            break;
                        }
                    }
                }
                if !self.ready[index] {
                    stats.not_ready_samples.fetch_add(1, Ordering::Relaxed);
                    if continuity.missing == 0 {
                        continue;
                    }
                } else if sample.is_none() {
                    continuity.debt = continuity.debt.saturating_add(1);
                    stats.output_underflows.fetch_add(1, Ordering::Relaxed);
                }
                let value = continuity.render(sample);
                if self.ready[index] && continuity.missing >= STARVATION_SAMPLES {
                    self.ready[index] = false;
                    continuity.debt = 0;
                }
                if value.is_finite() {
                    *peak = peak.max(value.abs());
                }
                if sample.is_some() {
                    stats.playback_started.store(true, Ordering::Relaxed);
                }
                let gain = f32::from_bits(control.gain.load(Ordering::Relaxed));
                if gain.is_finite()
                    && !control.mute.load(Ordering::Relaxed)
                    && (!solo || control.solo.load(Ordering::Relaxed))
                {
                    // Accumulate in f64 so even finite f32 gains cannot overflow the sum.
                    sum += f64::from(value) * f64::from(gain);
                }
            }
            sum += self.metronome.next_sample();
            #[allow(clippy::cast_possible_truncation)]
            let output = sum.tanh() as f32;
            frame.fill(T::from_sample(output));
        }
        self.metronome.end_block();
        for (control, peak) in self.controls.iter().zip(peaks) {
            control.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests;
