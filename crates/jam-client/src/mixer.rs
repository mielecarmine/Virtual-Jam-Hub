//! Fixed peer slots and SPSC queues, allocated before the hardware streams start.
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

use cpal::{FromSample, Sample};
use jam_protocol::MAX_PEERS;
use rtrb::{Consumer, Producer, RingBuffer};

use crate::{audio::Stats, codec::FRAME_SAMPLES};

// At 48 kHz these budgets are 10 ms queued and 5 ms after realignment.
const MAX_RX_SAMPLES: usize = FRAME_SAMPLES * 4;
const RX_TARGET_SAMPLES: usize = FRAME_SAMPLES * 2;

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
}

pub struct Mixer {
    pub(crate) rings: [Consumer<QueuedSample>; MAX_PEERS],
    pub controls: Arc<[Controls; MAX_PEERS]>,
    local_control: Arc<Controls>,
    local: Option<crate::monitor::Playback>,
}

pub fn channels() -> (RxChannels, Mixer) {
    channels_with_capacity(MAX_RX_SAMPLES)
}

pub(crate) fn channels_with_capacity(capacity: usize) -> (RxChannels, Mixer) {
    let controls = Arc::new(std::array::from_fn(|_| Controls::default()));
    let local_control = Arc::new(Controls::default());
    let pairs = std::array::from_fn::<_, MAX_PEERS, _>(|_| RingBuffer::new(capacity));
    let (producers, consumers): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
    let rings = producers.try_into().unwrap_or_else(|_| unreachable!());
    let rx = RxChannels {
        rings,
        controls: Arc::clone(&controls),
        local_control: Arc::clone(&local_control),
    };
    let rings = consumers.try_into().unwrap_or_else(|_| unreachable!());
    (
        rx,
        Mixer {
            rings,
            controls,
            local_control,
            local: None,
        },
    )
}

impl RxChannels {
    /// IDs are validated by the protocol and mapped directly to fixed slots `0..MAX_PEERS`.
    pub fn push(&mut self, id: u8, chunk: &[f32]) {
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
        let queued = ring.buffer().capacity() - free;
        if free >= chunk.len() && queued + chunk.len() <= MAX_RX_SAMPLES {
            for &value in chunk {
                let _ = ring.push(QueuedSample { value, epoch });
            }
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
        if let Some(local) = &mut self.local {
            local.align(data.len() / channels);
        }
        // Only the consumer may discard old samples. Snapshot the count so work is bounded
        // even while the UDP producer runs, and retain the most recent 5 ms.
        for ring in &mut self.rings {
            let queued = ring.slots();
            if queued > RX_TARGET_SAMPLES {
                for _ in 0..queued - RX_TARGET_SAMPLES {
                    let _ = ring.pop();
                }
            }
        }
        let mut peaks = [0.0_f32; MAX_PEERS];
        for frame in data.chunks_exact_mut(channels) {
            let solo = (self.local.is_some() && self.local_control.solo.load(Ordering::Relaxed))
                || self.controls.iter().any(|c| {
                    c.epoch.load(Ordering::Relaxed) % 2 != 0 && c.solo.load(Ordering::Relaxed)
                });
            let mut sum = 0.0;
            if let Some(local) = &mut self.local {
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
                    sum += f64::from(value) * f64::from(gain);
                }
            }
            for ((ring, control), peak) in self
                .rings
                .iter_mut()
                .zip(self.controls.iter())
                .zip(peaks.iter_mut())
            {
                let epoch = control.epoch.load(Ordering::Relaxed);
                // Bounded by the queue capacity, including stale samples after a disconnect.
                let mut sample = None;
                for _ in 0..ring.slots() {
                    if let Ok(queued) = ring.pop() {
                        if queued.epoch == epoch {
                            sample = Some(queued.value);
                            break;
                        }
                    }
                }
                if epoch % 2 == 0 {
                    continue;
                }
                let value = sample.unwrap_or_else(|| {
                    stats.output_underflows.fetch_add(1, Ordering::Relaxed);
                    0.0
                });
                if value.is_finite() {
                    *peak = peak.max(value.abs());
                }
                stats.playback_started.store(true, Ordering::Relaxed);
                let gain = f32::from_bits(control.gain.load(Ordering::Relaxed));
                if gain.is_finite()
                    && !control.mute.load(Ordering::Relaxed)
                    && (!solo || control.solo.load(Ordering::Relaxed))
                {
                    // Accumulate in f64 so even finite f32 gains cannot overflow the sum.
                    sum += f64::from(value) * f64::from(gain);
                }
            }
            #[allow(clippy::cast_possible_truncation)]
            let output = sum.tanh() as f32;
            frame.fill(T::from_sample(output));
        }
        for (control, peak) in self.controls.iter().zip(peaks) {
            control.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests;
