//! Bounded per-peer reordering and concealment, owned exclusively by the UDP worker.
use crate::codec::{Chunk, Encoded, FRAME_SAMPLES};
use jam_protocol::SAMPLE_RATE;
use opus::{Channels, Decoder};
use std::time::{Duration, Instant};

pub const CHUNK_TIME: Duration = Duration::from_micros(2500);
const WINDOW: u32 = 64;
const CAPACITY: usize = 64;
const MAX_TARGET: usize = 16;

pub struct PeerJitterBuffer {
    slots: Box<[Option<(u32, Encoded)>]>,
    next: u32,
    pub ready_at: Instant,
    started: bool,
    target: usize,
    hold: bool,
    last: Chunk,
    decoder: Decoder,
    pub last_received: Instant,
}

impl PeerJitterBuffer {
    pub fn buffered_ms(&self) -> f64 {
        f64::from(u32::try_from(self.slots.iter().filter(|s| s.is_some()).count()).unwrap_or(0))
            * 2.5
    }
    pub fn new(sequence: u32, now: Instant, target: usize) -> opus::Result<Self> {
        Ok(Self {
            slots: vec![None; CAPACITY].into_boxed_slice(),
            next: sequence,
            ready_at: now + CHUNK_TIME * u32::try_from(target).unwrap_or(3),
            started: false,
            target,
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
                // A late frame increases playout headroom by one chunk, up to 40 ms.
                if self.target < MAX_TARGET && !self.hold {
                    self.target += 1;
                    self.hold = true;
                }
                return Ok(());
            }
        } else if distance >= WINDOW {
            // A large discontinuity restarts this peer only, without unbounded buffering.
            self.slots.fill(None);
            self.next = sequence;
            self.started = false;
            self.hold = false;
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
        Ok(())
    }

    pub fn pop(&mut self, now: Instant) -> opus::Result<Option<Chunk>> {
        if now < self.ready_at {
            return Ok(None);
        }
        self.started = true;
        if self.hold {
            self.hold = false;
            // Hold playout without advancing the codec relative to queued packets.
            for sample in &mut self.last {
                *sample *= 0.8;
            }
            return Ok(Some(self.last));
        }
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
        for sample in &mut self.last {
            *sample = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
        Ok(Some(self.last))
    }
}

#[cfg(test)]
mod tests;
