//! Minimum-delay clock filter. All storage belongs to the network worker.

use std::time::Duration;

const WINDOW: usize = 32;

#[derive(Default)]
pub(super) struct ClockSync {
    samples: [Option<(u64, i64)>; WINDOW],
    next: usize,
}

impl ClockSync {
    pub(super) fn observe(&mut self, sent_us: u64, server_us: u64, rtt: Duration) {
        let Ok(rtt_us) = u64::try_from(rtt.as_micros()) else {
            return;
        };
        // i128 avoids overflow/underflow for either clock epoch and either offset sign.
        let midpoint = i128::from(sent_us) + i128::from(rtt_us / 2);
        let Ok(offset) = i64::try_from(i128::from(server_us) - midpoint) else {
            return;
        };
        self.samples[self.next] = Some((rtt_us, offset));
        self.next = (self.next + 1) % WINDOW;
    }

    pub(super) fn offset_us(&self) -> Option<i64> {
        // Prefer recent samples on ties, so a stable path can still follow clock drift.
        (0..WINDOW)
            .filter_map(|age| self.samples[(self.next + WINDOW - 1 - age) % WINDOW])
            .min_by_key(|&(rtt, _)| rtt)
            .map(|(_, offset)| offset)
    }
}
