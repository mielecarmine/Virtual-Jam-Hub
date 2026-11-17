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
const STABLE_WINDOW: Duration = Duration::from_secs(1);
// One extra slot absorbs a batch after up to 5 ms of worker scheduling delay.
const QUEUE_SLACK: usize = 3;

pub struct PeerJitterBuffer {
    slots: Box<[Option<(u32, Encoded)>]>,
    next: u32,
    pub ready_at: Instant,
    started: bool,
    target: usize,
    nominal_target: usize,
    stable_since: Instant,
    late_count: u8,
    last_late: Option<u32>,
    hold: bool,
    last: Chunk,
    decoder: Decoder,
    pub last_received: Instant,
}

impl PeerJitterBuffer {
    #[cfg(test)]
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
            slots: vec![None; CAPACITY].into_boxed_slice(),
            next: sequence,
            ready_at: now + CHUNK_TIME * u32::try_from(target).unwrap_or(3),
            started: false,
            target,
            nominal_target: target,
            stable_since: now,
            late_count: 0,
            last_late: None,
            hold: false,
            last: [0.0; FRAME_SAMPLES],
            decoder: Decoder::new(SAMPLE_RATE, Channels::Mono)?,
            last_received: now,
        })
    }

    pub fn insert(&mut self, sequence: u32, chunk: &Encoded, now: Instant) -> opus::Result<()> {
        let distance = sequence.wrapping_sub(self.next);
        if distance >= (1 << 31) {
            // Before startup, allow an earlier frame within the bounded reorder window.
            if !self.started && self.next.wrapping_sub(sequence) < WINDOW {
                self.next = sequence;
            } else {
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
            self.slots.fill(None);
            self.next = sequence;
            self.started = false;
            self.hold = false;
            self.late_count = 0;
            self.last_late = None;
            self.last.fill(0.0);
            self.decoder.reset_state()?;
            self.ready_at = now + CHUNK_TIME * u32::try_from(self.target).unwrap_or(3);
        }
        let slot = &mut self.slots[sequence as usize % CAPACITY];
        if slot.as_ref().is_some_and(|(seq, _)| *seq == sequence) {
            return Ok(());
        }
        *slot = Some((sequence, *chunk));
        self.last_received = now;
        if now.duration_since(self.stable_since) >= STABLE_WINDOW {
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

    fn trim_backlog(&mut self) -> opus::Result<()> {
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
        let budget = u32::try_from(self.target + QUEUE_SLACK).unwrap_or(19);
        if distance < (1 << 31) && distance >= budget {
            self.skip(distance - budget + 1)?;
        }
        Ok(())
    }

    /// Recover elapsed deadlines within downstream capacity; discard only the excess.
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
            // Even a full queue advances one current frame (push discards it whole).
            // Never decode an unbounded catch-up burst after a long suspension.
            let frames = due.min(capacity.clamp(1, 4) as u128);
            let missed = u32::try_from(due - frames).unwrap_or(u32::MAX);
            if missed > 0 {
                self.hold = false;
                self.skip(missed)?;
            }
            self.trim_backlog()?;
            return Ok(u32::try_from(frames).unwrap_or(4));
        }
        Ok(0)
    }

    fn skip(&mut self, count: u32) -> opus::Result<()> {
        if count == 0 {
            return Ok(());
        }
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
            self.decoder.decode_float(&[], &mut self.last, false)?;
            self.sanitize_last();
        } else {
            self.decode_next()?;
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
        let input = frame.as_ref().map_or(&[][..], |bytes| &bytes[..]);
        if self
            .decoder
            .decode_float(input, &mut self.last, false)
            .is_err()
        {
            // Corrupt compressed data is a lost frame, never fatal to the room.
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
