//! Receive-side measurements: a 64-packet window tolerates duplicates and reordering.
use std::time::Instant;

#[derive(Clone, Copy, Debug, Default)]
pub struct Telemetry {
    pub rtt_ms: Option<f64>,
    /// Peak of newly measured RTTs in this interval; None when no Pong was received.
    #[cfg_attr(not(feature = "telemetry-log"), allow(dead_code))]
    pub rtt_max_ms: Option<f64>,
    #[cfg_attr(not(feature = "telemetry-log"), allow(dead_code))]
    pub worker_max_gap_ms: f64,
    #[cfg_attr(not(feature = "telemetry-log"), allow(dead_code))]
    pub diagnostics: crate::jitter::Diagnostics,
    pub audio: crate::audio::AudioTelemetry,
    pub jitter_ms: f64,
    pub loss_percent: f64,
    /// Maximum queued encoded audio across peers, excluding the hardware/SPSC queues.
    pub buffer_ms: f64,
}

pub(super) struct ArrivalStats {
    highest: u32,
    seen: u64,
    width: u32,
    pub expected: u32,
    pub lost: u32,
    last: Instant,
    last_sequence: u32,
    pub jitter_ms: f64,
}

impl ArrivalStats {
    pub fn new(sequence: u32, now: Instant) -> Self {
        Self {
            highest: sequence,
            seen: 1,
            width: 1,
            expected: 0,
            lost: 0,
            last: now,
            last_sequence: sequence,
            jitter_ms: 0.0,
        }
    }

    fn update_jitter(&mut self, sequence: u32, now: Instant) {
        // Signed modular distance also accounts for out-of-order arrivals and wraparound.
        let samples = i32::from_ne_bytes(sequence.wrapping_sub(self.last_sequence).to_ne_bytes());
        let variation =
            (now.duration_since(self.last).as_secs_f64() * 1000.0 - f64::from(samples) * 2.5).abs();
        self.jitter_ms += (variation - self.jitter_ms) / 16.0;
        self.last = now;
        self.last_sequence = sequence;
    }

    pub fn receive(&mut self, sequence: u32, now: Instant) {
        let forward = sequence.wrapping_sub(self.highest);
        if forward == 0 {
            return;
        }
        if forward >= 1 << 31 {
            let behind = self.highest.wrapping_sub(sequence);
            if behind < self.width && self.seen & (1 << behind) == 0 {
                self.seen |= 1 << behind;
                self.update_jitter(sequence, now);
            }
            return;
        }
        // Treat very large jumps as a stream restart, not billions of lost packets.
        if forward > 4096 {
            *self = Self::new(sequence, now);
            return;
        }
        self.update_jitter(sequence, now);
        for _ in 0..forward {
            if self.width == 64 {
                self.expected = self.expected.saturating_add(1);
                self.lost = self
                    .lost
                    .saturating_add(u32::from(self.seen & (1 << 63) == 0));
            } else {
                self.width += 1;
            }
            self.seen <<= 1;
        }
        self.seen |= 1;
        self.highest = sequence;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn loss_reordering_duplicates_and_wrap() {
        let now = Instant::now();
        let mut stats = ArrivalStats::new(u32::MAX - 1, now);
        stats.receive(0, now + Duration::from_millis(5));
        stats.receive(u32::MAX, now + Duration::from_micros(5500));
        stats.receive(0, now + Duration::from_millis(6));
        for seq in 1..200 {
            if seq != 20 {
                stats.receive(
                    seq,
                    now + Duration::from_micros((u64::from(seq) + 2) * 2500),
                );
            }
        }
        assert_eq!(stats.lost, 1);
        assert_eq!(stats.expected, 138);
        assert!(stats.jitter_ms < 0.001);
    }
    #[test]
    fn arrival_variation_increases_jitter() {
        let now = Instant::now();
        let mut stats = ArrivalStats::new(0, now);
        stats.receive(1, now + Duration::from_millis(12));
        assert!((stats.jitter_ms - 9.5 / 16.0).abs() < 0.001);
    }
}
