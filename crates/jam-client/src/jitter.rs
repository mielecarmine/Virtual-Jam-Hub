//! Bounded per-peer reordering and concealment, owned exclusively by the UDP worker.
use crate::codec::{Chunk, Encoded, FRAME_SAMPLES};
use jam_protocol::SAMPLE_RATE;
use opus::{Channels, Decoder};
use std::time::{Duration, Instant};

pub const CHUNK_TIME: Duration = Duration::from_micros(2500);
const WINDOW: u32 = 64;
const CAPACITY: usize = 64;
// Preserve the CLI's explicit 1..=16 setting, but cap automatic growth at 20 ms.
const MAX_TARGET: usize = 16;
const MAX_ADAPTIVE_TARGET: usize = 8;
const LATE_THRESHOLD: u8 = 3;
const RECOVERY_THRESHOLD: u8 = 8;
// Never rewind more than one second of audio (400 x 2.5 ms).
const MAX_RECOVERY_LAG: u32 = 400;
const STABLE_WINDOW: Duration = Duration::from_secs(1);
// Recover one step every 250 ms without late packets; explicit CLI targets remain floors.
const DECAY_WINDOW: Duration = Duration::from_millis(250);
const QUEUE_SLACK: usize = 1;

/// Cumulative worker counters; `late_dropped` includes `late_dropped_far` (>= WINDOW).
#[derive(Clone, Copy, Debug, Default)]
pub struct Diagnostics {
    pub late_dropped: u32,
    pub late_dropped_far: u32,
    pub plc_frames: u32,
    pub skipped_frames: u32,
    pub rx_push_dropped: u32,
    pub resyncs: u32,
}

impl Diagnostics {
    pub fn accumulate(&mut self, other: Self) {
        self.late_dropped = self.late_dropped.saturating_add(other.late_dropped);
        self.late_dropped_far = self.late_dropped_far.saturating_add(other.late_dropped_far);
        self.plc_frames = self.plc_frames.saturating_add(other.plc_frames);
        self.skipped_frames = self.skipped_frames.saturating_add(other.skipped_frames);
        self.rx_push_dropped = self.rx_push_dropped.saturating_add(other.rx_push_dropped);
        self.resyncs = self.resyncs.saturating_add(other.resyncs);
    }
}

pub struct PeerJitterBuffer {
    pub diagnostics: Diagnostics,
    slots: Box<[Option<(u32, Encoded)>]>,
    next: u32,
    pub ready_at: Instant,
    started: bool,
    pending_playout: u128,
    target: usize,
    nominal_target: usize,
    stable_since: Instant,
    late_count: u8,
    last_late: Option<u32>,
    recovery_last: Option<u32>,
    recovery_count: u8,
    hold: bool,
    batch_caught_up: bool,
    splice_from: Option<f32>,
    last: Chunk,
    decoder: Decoder,
    pub last_received: Instant,
}

impl PeerJitterBuffer {
    pub(crate) fn target_chunks(&self) -> usize {
        self.target
    }

    pub fn buffered_ms(&self) -> f64 {
        f64::from(u32::try_from(self.slots.iter().filter(|s| s.is_some()).count()).unwrap_or(0))
            * 2.5
    }
    pub fn new(sequence: u32, now: Instant, target: usize) -> opus::Result<Self> {
        let target = target.clamp(1, MAX_TARGET);
        Ok(Self {
            diagnostics: Diagnostics::default(),
            slots: vec![None; CAPACITY].into_boxed_slice(),
            next: sequence,
            ready_at: now + CHUNK_TIME * u32::try_from(target).unwrap_or(3),
            started: false,
            pending_playout: 0,
            target,
            nominal_target: target,
            stable_since: now,
            late_count: 0,
            last_late: None,
            recovery_last: None,
            recovery_count: 0,
            hold: false,
            batch_caught_up: false,
            splice_from: None,
            last: [0.0; FRAME_SAMPLES],
            decoder: Decoder::new(SAMPLE_RATE, Channels::Mono)?,
            last_received: now,
        })
    }

    /// Inspect only packets ready in this tick, relative to the pre-batch cursor.
    pub fn begin_receive_batch(&mut self, sequences: impl Iterator<Item = u32>) {
        self.batch_caught_up = sequences.into_iter().any(|sequence| {
            sequence.wrapping_sub(self.next) < WINDOW || self.next.wrapping_sub(sequence) <= 1
        });
        if self.batch_caught_up {
            self.recovery_count = 0;
            self.recovery_last = None;
        }
    }

    pub fn insert(&mut self, sequence: u32, chunk: &Encoded, now: Instant) -> opus::Result<()> {
        // Network activity is independent of whether playout can use this packet.
        self.last_received = now;
        let distance = sequence.wrapping_sub(self.next);
        if distance >= (1 << 31) {
            // Before startup, allow an earlier frame within the bounded reorder window.
            if !self.started && self.next.wrapping_sub(sequence) < WINDOW {
                self.next = sequence;
                self.diagnostics.resyncs = self.diagnostics.resyncs.saturating_add(1);
            } else {
                if !self.batch_caught_up && self.recover_late(sequence, now)? {
                    self.slots[sequence as usize % CAPACITY] = Some((sequence, *chunk));
                    return Ok(());
                }
                self.diagnostics.late_dropped = self.diagnostics.late_dropped.saturating_add(1);
                if self.next.wrapping_sub(sequence) >= WINDOW {
                    self.diagnostics.late_dropped_far =
                        self.diagnostics.late_dropped_far.saturating_add(1);
                }
                // A caught-up burst is scheduling jitter, not a sustained late stream.
                if self.batch_caught_up {
                    return Ok(());
                }
                // Ignore duplicates/replays and require repeated recent lateness, rather
                // than inflating headroom for every isolated late packet in a batch.
                if self.next.wrapping_sub(sequence) < WINDOW
                    && self.last_late.is_none_or(|last| {
                        sequence != last && sequence.wrapping_sub(last) < (1 << 31)
                    })
                {
                    if now.duration_since(self.stable_since) >= STABLE_WINDOW {
                        self.late_count = 0;
                    }
                    self.stable_since = now;
                    self.last_late = Some(sequence);
                    self.late_count += 1;
                    if self.late_count >= LATE_THRESHOLD {
                        self.late_count = 0;
                        if self.target < MAX_ADAPTIVE_TARGET.max(self.nominal_target) && !self.hold
                        {
                            self.target += 1;
                            self.hold = true;
                        }
                    }
                }
                return Ok(());
            }
        } else if distance >= WINDOW {
            // A large discontinuity restarts this peer only, without unbounded buffering.
            self.diagnostics.resyncs = self.diagnostics.resyncs.saturating_add(1);
            self.slots.fill(None);
            self.next = sequence;
            self.started = false;
            self.pending_playout = 0;
            self.hold = false;
            self.late_count = 0;
            self.last_late = None;
            self.last.fill(0.0);
            self.decoder.reset_state()?;
            self.ready_at = now + CHUNK_TIME * u32::try_from(self.target).unwrap_or(3);
        }
        self.recovery_count = 0;
        self.recovery_last = None;
        let slot = &mut self.slots[sequence as usize % CAPACITY];
        if slot.as_ref().is_some_and(|(seq, _)| *seq == sequence) {
            return Ok(());
        }
        *slot = Some((sequence, *chunk));
        if now.duration_since(self.stable_since) >= DECAY_WINDOW {
            self.stable_since = now;
            self.late_count = 0;
            if self.target > self.nominal_target {
                self.target -= 1;
                self.hold = false;
                // The playout span limit also releases excess queued headroom.
            }
        }
        Ok(())
    }

    fn recover_late(&mut self, sequence: u32, now: Instant) -> opus::Result<bool> {
        let lag = self.next.wrapping_sub(sequence);
        let consecutive = self
            .recovery_last
            .is_some_and(|last| sequence == last.wrapping_add(1));
        let advancing = self
            .recovery_last
            .is_none_or(|last| sequence != last && sequence.wrapping_sub(last) < (1 << 31));
        if self.started
            && lag <= MAX_RECOVERY_LAG
            && self.slots.iter().all(Option::is_none)
            && advancing
        {
            self.recovery_count = if consecutive {
                self.recovery_count + 1
            } else {
                1
            };
        } else {
            self.recovery_count = 0;
        }
        if advancing {
            self.recovery_last = Some(sequence);
        }
        // Detection plus refill must fit in 20 ticks (50 ms), even at target=16.
        let threshold = usize::from(RECOVERY_THRESHOLD).min(21 - self.target);
        if usize::from(self.recovery_count) < threshold {
            return Ok(false);
        }
        // Refill from this packet instead of spending the plateau chasing the sender.
        // Short rewinds preserve Opus history, just like short forward trims.
        if lag >= WINDOW {
            self.last.fill(0.0);
            self.decoder.reset_state()?;
        }
        self.next = sequence;
        self.started = false;
        self.pending_playout = 0;
        self.hold = false;
        self.late_count = 0;
        self.last_late = None;
        self.recovery_count = 0;
        self.recovery_last = None;
        self.stable_since = now;
        self.ready_at = now + CHUNK_TIME * u32::try_from(self.target).unwrap_or(3);
        self.diagnostics.resyncs = self.diagnostics.resyncs.saturating_add(1);
        Ok(true)
    }

    #[cfg(test)]
    fn trim_backlog(&mut self) -> opus::Result<()> {
        self.trim_backlog_with_allowance(0)
    }

    fn trim_backlog_with_allowance(&mut self, allowance: usize) -> opus::Result<()> {
        // Trim only after the entire receive batch has been inserted and deadlines
        // accounted for; otherwise a scheduling burst can discard playable frames.
        let distance = self
            .slots
            .iter()
            .flatten()
            .map(|(seq, _)| seq.wrapping_sub(self.next))
            .filter(|&distance| distance < (1 << 31))
            .max()
            .unwrap_or(0);
        let budget = u32::try_from(self.target + QUEUE_SLACK + allowance).unwrap_or(19);
        if distance < (1 << 31) && distance >= budget {
            self.skip(distance - budget + 1)?;
        }
        Ok(())
    }

    /// Retain elapsed deadlines under backpressure; trim only actual latency excess.
    pub fn playout_frames(
        &mut self,
        deadline: Instant,
        now: Instant,
        capacity: usize,
    ) -> opus::Result<u32> {
        if now >= deadline {
            let elapsed = now.duration_since(deadline).as_nanos() / CHUNK_TIME.as_nanos();
            // Startup can fall between global deadlines. Exclude all ticks before ready_at.
            let startup = self
                .ready_at
                .saturating_duration_since(deadline)
                .as_nanos()
                .div_ceil(CHUNK_TIME.as_nanos());
            let due = elapsed.saturating_add(1).saturating_sub(startup);
            self.pending_playout = self.pending_playout.saturating_add(due);
            // Backpressure is not packet loss. Leave both packets and decoder untouched.
            if capacity == 0 {
                return Ok(0);
            }
            // Only a suspension beyond the reorder window expires wall-clock deadlines.
            let expired = self.pending_playout.saturating_sub(u128::from(WINDOW));
            if expired > 0 {
                self.hold = false;
                self.skip(u32::try_from(expired).unwrap_or(u32::MAX))?;
                self.pending_playout -= expired;
            }
            let before = self.next;
            let deliverable = self
                .pending_playout
                .min(capacity as u128)
                .min(u128::from(WINDOW));
            self.trim_backlog_with_allowance(usize::try_from(deliverable).unwrap_or(CAPACITY))?;
            // The trim budget already reserves this tick's deliverable frames.
            // Retire only excess pending deadlines, or a trim can starve every tick.
            self.pending_playout = self
                .pending_playout
                .saturating_sub(u128::from(self.next.wrapping_sub(before)))
                .max(deliverable);
            let frames = self
                .pending_playout
                .min(capacity as u128)
                .min(u128::from(WINDOW));
            self.pending_playout -= frames;
            return Ok(u32::try_from(frames).unwrap_or(WINDOW));
        }
        Ok(0)
    }

    fn skip(&mut self, count: u32) -> opus::Result<()> {
        if count == 0 {
            return Ok(());
        }
        // Preserve the last emitted sample, not audio decoded only to advance history.
        self.splice_from.get_or_insert(self.last[FRAME_SAMPLES - 1]);
        self.diagnostics.skipped_frames = self.diagnostics.skipped_frames.saturating_add(count);
        // Intentional trims and scheduling skips are not evidence of network lateness.
        self.last_late = Some(self.next.wrapping_add(count).wrapping_sub(1));
        self.late_count = 0;
        self.started = true;
        if count < WINDOW {
            // Advance codec history through short trims, including PLC for missing slots.
            // Work is bounded by the reorder window even after a long suspension.
            for _ in 0..count {
                self.decode_next()?;
            }
            return Ok(());
        }
        self.diagnostics.resyncs = self.diagnostics.resyncs.saturating_add(1);
        self.next = self.next.wrapping_add(count);
        self.slots.fill(None);
        // Only a discontinuity outside the reorder window discards codec history.
        self.last.fill(0.0);
        self.decoder.reset_state()
    }

    pub fn pop(&mut self, now: Instant) -> opus::Result<Option<Chunk>> {
        if now < self.ready_at {
            return Ok(None);
        }
        self.started = true;
        if self.hold {
            self.hold = false;
            // Extend playout using native PLC, leaving the queued sequence untouched.
            self.diagnostics.plc_frames = self.diagnostics.plc_frames.saturating_add(1);
            self.decoder.decode_float(&[], &mut self.last, false)?;
            self.sanitize_last();
        } else {
            self.decode_next()?;
        }
        if let Some(previous) = self.splice_from.take() {
            const FADE: u16 = 48; // 1 ms at 48 kHz, entirely on the UDP worker.
            let offset = previous - self.last[0];
            for (sample, remaining) in self.last.iter_mut().zip((1..=FADE).rev()) {
                *sample =
                    (*sample + offset * f32::from(remaining) / f32::from(FADE)).clamp(-1.0, 1.0);
            }
        }
        Ok(Some(self.last))
    }

    fn decode_next(&mut self) -> opus::Result<()> {
        let slot = &mut self.slots[self.next as usize % CAPACITY];
        let frame = if slot.as_ref().is_some_and(|(seq, _)| *seq == self.next) {
            slot.take().map(|(_, chunk)| chunk)
        } else {
            None
        };
        self.next = self.next.wrapping_add(1);
        // Decode only after reordering. An empty input invokes Opus PLC; CELT has no FEC.
        if frame.is_none() {
            self.diagnostics.plc_frames = self.diagnostics.plc_frames.saturating_add(1);
        }
        let input = frame.as_ref().map_or(&[][..], |bytes| &bytes[..]);
        if self
            .decoder
            .decode_float(input, &mut self.last, false)
            .is_err()
        {
            // Corrupt compressed data is a lost frame, never fatal to the room.
            self.diagnostics.plc_frames = self.diagnostics.plc_frames.saturating_add(1);
            self.decoder.decode_float(&[], &mut self.last, false)?;
        }
        self.sanitize_last();
        Ok(())
    }

    fn sanitize_last(&mut self) {
        for sample in &mut self.last {
            *sample = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
    }
}

#[cfg(test)]
mod tests;
