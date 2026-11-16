//! Bounded per-peer reordering and concealment, owned exclusively by the UDP worker.
use crate::audio::BUFFER_FRAMES;
use std::time::{Duration, Instant};

pub type Chunk = [f32; BUFFER_FRAMES];
pub const CHUNK_TIME: Duration = Duration::from_nanos(2_666_667);
const WINDOW: u32 = 64;
const CAPACITY: usize = 64;
const MAX_TARGET: usize = 16;

pub struct PeerJitterBuffer {
    slots: Box<[Option<(u32, Chunk)>]>,
    next: u32,
    pub ready_at: Instant,
    started: bool,
    target: usize,
    hold: bool,
    last: Chunk,
    pub last_received: Instant,
}

impl PeerJitterBuffer {
    pub fn new(sequence: u32, now: Instant, target: usize) -> Self {
        Self {
            slots: vec![None; CAPACITY].into_boxed_slice(),
            next: sequence,
            ready_at: now + CHUNK_TIME * u32::try_from(target).unwrap_or(3),
            started: false,
            target,
            hold: false,
            last: [0.0; BUFFER_FRAMES],
            last_received: now,
        }
    }

    pub fn insert(&mut self, sequence: u32, chunk: &Chunk, now: Instant) {
        let distance = sequence.wrapping_sub(self.next);
        if distance >= (1 << 31) {
            // Before startup, allow an earlier frame within the bounded reorder window.
            if !self.started && self.next.wrapping_sub(sequence) < WINDOW {
                self.next = sequence;
            } else {
                // A late frame increases playout headroom by one chunk, up to ~43 ms.
                if self.target < MAX_TARGET && !self.hold {
                    self.target += 1;
                    self.hold = true;
                }
                return;
            }
        } else if distance >= WINDOW {
            // A large discontinuity restarts this peer only, without unbounded buffering.
            self.slots.fill(None);
            self.next = sequence;
            self.started = false;
            self.hold = false;
            self.last.fill(0.0);
            self.ready_at = now + CHUNK_TIME * u32::try_from(self.target).unwrap_or(3);
        }
        let slot = &mut self.slots[sequence as usize % CAPACITY];
        if slot.as_ref().is_some_and(|(seq, _)| *seq == sequence) {
            return;
        }
        *slot = Some((sequence, *chunk));
        self.last_received = now;
    }

    pub fn pop(&mut self, now: Instant) -> Option<Chunk> {
        if now < self.ready_at {
            return None;
        }
        self.started = true;
        if self.hold {
            self.hold = false;
            return Some(self.conceal());
        }
        let slot = &mut self.slots[self.next as usize % CAPACITY];
        let frame = if slot.as_ref().is_some_and(|(seq, _)| *seq == self.next) {
            slot.take().map(|(_, chunk)| chunk)
        } else {
            None
        };
        self.next = self.next.wrapping_add(1);
        if let Some(frame) = frame {
            self.last = frame;
            Some(frame)
        } else {
            Some(self.conceal())
        }
    }

    fn conceal(&mut self) -> Chunk {
        // Bounded PCM concealment: repeat the last chunk with progressive attenuation.
        for sample in &mut self.last {
            *sample *= 0.8;
        }
        self.last
    }
}

#[cfg(test)]
mod tests;
