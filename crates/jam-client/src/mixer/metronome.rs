//! Sample-clock click synthesis. No wall-clock reads, allocation, locks or retry loops on RT.
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

use jam_protocol::{MetronomeConfig, SAMPLE_RATE};

const MINUTE_SAMPLES: u64 = SAMPLE_RATE as u64 * 60;
const CLICK_SAMPLES: u32 = SAMPLE_RATE / 50; // 20 ms
const RAMP_SAMPLES: u32 = SAMPLE_RATE / 1000;

pub struct MetronomeControls {
    /// Local monitoring only, encoded with `f32::to_bits()`.
    pub gain: AtomicU32,
    pub mute: AtomicBool,
    /// Zero-based beat within the bar, including while locally muted.
    pub current_beat: AtomicU32,
    /// Positive follower playout delay, published by the network worker in samples.
    pub phase_delay_samples: AtomicU32,
    // Only RxChannels publishes. SeqCst gives the bounded reader a coherent snapshot;
    // an interrupted writer leaves the previous snapshot active, never a spinning callback.
    version: AtomicU32,
    config: AtomicU32,
    anchor_sample: AtomicU64,
    rendered_samples: AtomicU64,
}

impl Default for MetronomeControls {
    fn default() -> Self {
        Self {
            gain: AtomicU32::new(0.5_f32.to_bits()),
            mute: AtomicBool::new(false),
            current_beat: AtomicU32::new(0),
            phase_delay_samples: AtomicU32::new(0),
            version: AtomicU32::new(0),
            config: AtomicU32::new(pack(MetronomeConfig::default())),
            anchor_sample: AtomicU64::new(0),
            rendered_samples: AtomicU64::new(0),
        }
    }
}

fn pack(config: MetronomeConfig) -> u32 {
    u32::from(config.enabled)
        | (u32::from(config.bpm) << 1)
        | (u32::from(config.beats_per_bar) << 17)
}

#[derive(Clone, Copy)]
struct Snapshot {
    config: u32,
    anchor: i64,
}

impl MetronomeControls {
    fn snapshot(&self) -> Option<Snapshot> {
        let version = self.version.load(Ordering::SeqCst);
        if version % 2 != 0 {
            return None;
        }
        let snapshot = Snapshot {
            config: self.config.load(Ordering::SeqCst),
            anchor: i64::from_ne_bytes(self.anchor_sample.load(Ordering::SeqCst).to_ne_bytes()),
        };
        (version == self.version.load(Ordering::SeqCst)).then_some(snapshot)
    }
}

impl super::RxChannels {
    /// Preserve the sample epoch for room changes that do not move the clock anchor.
    pub(crate) fn configure_metronome(&mut self, config: MetronomeConfig) {
        self.metronome.config.store(pack(config), Ordering::SeqCst);
    }

    /// Sole writer, called off RT when authoritative room state or clock offset changes.
    /// Correlates the most recently rendered sample cursor with room time. Callback
    /// scheduling/hardware latency limits this epoch estimate, not the sample cadence.
    pub(crate) fn sync_metronome(&mut self, config: MetronomeConfig, elapsed_us: i128) {
        let controls = &self.metronome;
        let cursor = controls.rendered_samples.load(Ordering::Relaxed);
        let elapsed_samples = elapsed_us * i128::from(SAMPLE_RATE) / 1_000_000;
        let anchor =
            i64::try_from(i128::from(cursor) - elapsed_samples).unwrap_or(if elapsed_samples > 0 {
                i64::MIN
            } else {
                i64::MAX
            });
        controls.version.fetch_add(1, Ordering::SeqCst);
        controls
            .anchor_sample
            .store(u64::from_ne_bytes(anchor.to_ne_bytes()), Ordering::SeqCst);
        controls.config.store(pack(config), Ordering::SeqCst);
        controls.version.fetch_add(1, Ordering::SeqCst);
    }
}

pub(super) struct Metronome {
    controls: Arc<MetronomeControls>,
    snapshot: Snapshot,
    cursor: u64,
    age: u32,
    frequency: f64,
    accent: f64,
    gain: f64,
    target_gain: f64,
    gain_step: f64,
    ramp: u32,
    phase_delay_samples: u32,
}

impl Metronome {
    pub(super) fn new(controls: Arc<MetronomeControls>) -> Self {
        Self {
            snapshot: Snapshot {
                config: pack(MetronomeConfig::default()),
                anchor: 0,
            },
            controls,
            cursor: 0,
            age: CLICK_SAMPLES,
            frequency: 1200.0,
            accent: 1.0,
            gain: 0.5,
            target_gain: 0.5,
            gain_step: 0.0,
            ramp: 0,
            phase_delay_samples: 0,
        }
    }

    pub(super) fn begin_block(&mut self) {
        self.phase_delay_samples = self.controls.phase_delay_samples.load(Ordering::Relaxed);
        if let Some(snapshot) = self.controls.snapshot() {
            self.snapshot = snapshot;
        }
        let gain = f32::from_bits(self.controls.gain.load(Ordering::Relaxed));
        let target = if self.controls.mute.load(Ordering::Relaxed) || !gain.is_finite() {
            0.0
        } else {
            f64::from(gain.max(0.0))
        };
        if target.to_bits() != self.target_gain.to_bits() {
            self.target_gain = target;
            self.ramp = RAMP_SAMPLES;
            self.gain_step = (target - self.gain) / f64::from(RAMP_SAMPLES);
        }
    }

    pub(super) fn next_sample(&mut self) -> f64 {
        let elapsed = i128::from(self.cursor)
            - i128::from(self.snapshot.anchor)
            - i128::from(self.phase_delay_samples);
        let bpm = u64::from((self.snapshot.config >> 1) & 0xffff);
        let beats = u64::from(self.snapshot.config >> 17);
        if self.snapshot.config & 1 != 0 && elapsed >= 0 {
            // Integer rational phase: ceil(n * 48_000 * 60 / bpm), no accumulated drift.
            let ticks = u128::try_from(elapsed).unwrap_or(0) * u128::from(bpm);
            let beat = ticks / u128::from(MINUTE_SAMPLES);
            let in_bar = u32::try_from(beat % u128::from(beats)).unwrap_or(0);
            self.controls.current_beat.store(in_bar, Ordering::Relaxed);
            if ticks % u128::from(MINUTE_SAMPLES) < u128::from(bpm) && self.age == CLICK_SAMPLES {
                self.age = 0;
                self.frequency = if in_bar == 0 { 1200.0 } else { 800.0 };
                self.accent = if in_bar == 0 { 1.0 } else { 0.7 };
            }
        }
        self.cursor = self.cursor.wrapping_add(1);
        if self.ramp > 0 {
            self.gain += self.gain_step;
            self.ramp -= 1;
            if self.ramp == 0 {
                self.gain = self.target_gain;
            }
        }
        if self.age == CLICK_SAMPLES {
            return 0.0;
        }
        let age = f64::from(self.age);
        let attack = (age / f64::from(RAMP_SAMPLES)).min(1.0);
        let release = (f64::from(CLICK_SAMPLES - 1 - self.age) / f64::from(RAMP_SAMPLES)).min(1.0);
        // Raised-cosine edges and a fast exponential decay avoid DC steps at either end.
        let edge = |x: f64| 0.5 - 0.5 * (std::f64::consts::PI * x).cos();
        let envelope = (-6.0 * age / f64::from(CLICK_SAMPLES)).exp() * edge(attack) * edge(release);
        let phase = std::f64::consts::TAU * self.frequency * age / f64::from(SAMPLE_RATE);
        self.age += 1;
        // Existing voices finish unchanged across BPM, phase, meter and enabled changes.
        phase.sin() * envelope * self.accent * self.gain
    }

    pub(super) fn end_block(&self) {
        self.controls
            .rendered_samples
            .store(self.cursor, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_follower_offset_delays_click_and_beat_while_leader_stays_nominal() {
        let config = MetronomeConfig {
            enabled: true,
            bpm: 300,
            beats_per_bar: 3,
            leader_peer_id: 1,
        };
        let (mut leader, _) = crate::mixer::channels(128);
        let (mut follower, _) = crate::mixer::channels(128);
        leader.sync_metronome(config, 0);
        follower.sync_metronome(config, 0);
        follower
            .metronome
            .phase_delay_samples
            .store(1440, Ordering::Relaxed);
        let mut nominal = Metronome::new(Arc::clone(&leader.metronome));
        let mut delayed = Metronome::new(Arc::clone(&follower.metronome));
        nominal.begin_block();
        delayed.begin_block();
        let mut reference = Vec::new();
        for index in 0..30_340 {
            reference.push(nominal.next_sample());
            let actual = delayed.next_sample();
            if index < 1440 {
                assert_eq!(actual, 0.0);
            } else {
                assert_eq!(actual, reference[index - 1440]);
                assert_eq!(
                    follower.metronome.current_beat.load(Ordering::Relaxed),
                    u32::try_from((index - 1440) / 9600 % 3).unwrap()
                );
            }
        }
        // Removing a leader offset preserves an in-flight voice without a discontinuity.
        follower
            .metronome
            .phase_delay_samples
            .store(0, Ordering::Relaxed);
        delayed.begin_block();
        assert_eq!(delayed.next_sample(), reference[28_900]);
    }

    #[test]
    fn absolute_room_phase_matches_across_different_local_clock_and_sample_epochs() {
        let config = MetronomeConfig {
            enabled: true,
            bpm: 137,
            ..MetronomeConfig::default()
        };
        let (mut first, _) = crate::mixer::channels(64);
        let (mut second, _) = crate::mixer::channels(256);
        first
            .metronome
            .rendered_samples
            .store(48_000, Ordering::Relaxed);
        second
            .metronome
            .rendered_samples
            .store(240_000, Ordering::Relaxed);
        // Both clock mappings report the same room time despite different callback cursors.
        first.sync_metronome(config, 2_000_000);
        second.sync_metronome(config, 2_000_000);
        let mut a = Metronome::new(Arc::clone(&first.metronome));
        let mut b = Metronome::new(Arc::clone(&second.metronome));
        a.cursor = 48_000;
        b.cursor = 240_000;
        a.begin_block();
        b.begin_block();
        for _ in 0..100_000 {
            assert_eq!(a.next_sample(), b.next_sample());
            assert_eq!(
                first.metronome.current_beat.load(Ordering::Relaxed),
                second.metronome.current_beat.load(Ordering::Relaxed)
            );
        }
    }

    #[test]
    fn interrupted_publication_keeps_rendering_previous_snapshot_without_waiting() {
        let (mut rx, _) = crate::mixer::channels(128);
        let config = MetronomeConfig {
            enabled: true,
            ..MetronomeConfig::default()
        };
        rx.sync_metronome(config, 0);
        let mut synth = Metronome::new(Arc::clone(&rx.metronome));
        synth.begin_block();
        rx.metronome.version.fetch_add(1, Ordering::SeqCst);
        rx.metronome.anchor_sample.store(u64::MAX, Ordering::SeqCst);
        synth.begin_block();
        assert_eq!(synth.snapshot.anchor, 0);
        assert_eq!(synth.next_sample(), 0.0);
        assert!(synth.next_sample() > 0.0);
        synth.end_block();
        assert_eq!(rx.metronome.rendered_samples.load(Ordering::Relaxed), 2);
    }
}
