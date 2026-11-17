//! Direct capture-to-playback SPSC, with sample positions to reject stale audio after overflow.

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use rtrb::{Consumer, Producer, RingBuffer};

#[derive(Clone, Copy)]
struct Sample {
    value: f32,
    position: u32,
}

pub struct Capture {
    ring: Producer<Sample>,
    position: u32,
    latest: Arc<AtomicU32>,
}

pub struct Playback {
    ring: Consumer<Sample>,
    latest: Arc<AtomicU32>,
}

pub fn channel(capacity: usize) -> (Capture, Playback) {
    let (producer, consumer) = RingBuffer::new(capacity);
    let latest = Arc::new(AtomicU32::new(0));
    (
        Capture {
            ring: producer,
            position: 0,
            latest: Arc::clone(&latest),
        },
        Playback {
            ring: consumer,
            latest,
        },
    )
}

impl Capture {
    pub fn push(&mut self, value: f32) {
        // Advance even if full: playback must detect that queued samples are now stale.
        self.position = self.position.wrapping_add(1);
        let _ = self.ring.push(Sample {
            value,
            position: self.position,
        });
    }

    pub fn publish(&self) {
        self.latest.store(self.position, Ordering::Release);
    }
}

impl Playback {
    pub fn align(&mut self, frames: usize) {
        let latest = self.latest.load(Ordering::Acquire);
        // Snapshot bounds the work even while capture is running. Only the consumer discards.
        let queued = self.ring.slots();
        for _ in 0..queued {
            let Ok(sample) = self.ring.peek() else { break };
            let age = latest.wrapping_sub(sample.position);
            // A position ahead of the published snapshot belongs to concurrent capture.
            if age > u32::MAX / 2 || age < u32::try_from(frames).unwrap_or(u32::MAX) {
                break;
            }
            let _ = self.ring.pop();
        }
    }

    pub fn pop(&mut self) -> Option<f32> {
        self.ring.pop().ok().map(|sample| sample.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_survives_sample_position_wrap() {
        let (mut capture, mut playback) = channel(8);
        capture.position = u32::MAX - 2;
        for value in 0..6_u8 {
            capture.push(f32::from(value));
        }
        capture.publish();
        playback.align(3);
        assert_eq!(playback.pop(), Some(3.0));
        assert_eq!(playback.pop(), Some(4.0));
        assert_eq!(playback.pop(), Some(5.0));
        assert_eq!(playback.pop(), None);
    }

    #[test]
    fn samples_ahead_of_published_capture_position_are_retained() {
        let (mut capture, mut playback) = channel(8);
        capture.push(0.25);
        capture.publish();
        // Model output running while capture is writing the next callback.
        capture.push(0.5);
        playback.align(1);
        assert_eq!(playback.pop(), Some(0.25));
        assert_eq!(playback.pop(), Some(0.5));
    }
}
