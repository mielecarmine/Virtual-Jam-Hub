use super::*;
use crate::codec::{encoder, tests::encode};

fn decode(decoder: &mut Decoder, input: &[u8]) -> Chunk {
    let mut chunk = [0.0; FRAME_SAMPLES];
    assert_eq!(
        decoder.decode_float(input, &mut chunk, false).unwrap(),
        FRAME_SAMPLES
    );
    chunk.map(|sample| sample.clamp(-1.0, 1.0))
}

#[test]
fn reorders_compressed_packets_before_decoding_and_rejects_duplicates() {
    let now = Instant::now();
    let mut encoder = encoder().unwrap();
    let frames: Vec<_> = (0..4).map(|_| encode(&mut encoder, 0.5)).collect();
    let mut decoder = Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
    let expected: Vec<_> = frames
        .iter()
        .map(|frame| decode(&mut decoder, frame))
        .collect();
    let mut buffer = PeerJitterBuffer::new(102, now, 3).unwrap();
    buffer.insert(102, &frames[1], now).unwrap();
    buffer.insert(101, &frames[0], now).unwrap();
    buffer.insert(102, &frames[3], now).unwrap();
    assert!(buffer.pop(now + CHUNK_TIME * 2).unwrap().is_none());
    let ready = now + CHUNK_TIME * 3;
    assert_eq!(buffer.pop(ready).unwrap().unwrap(), expected[0]);
    assert_eq!(buffer.pop(ready).unwrap().unwrap(), expected[1]);
    buffer.insert(104, &frames[3], ready).unwrap();
    buffer.insert(103, &frames[2], ready).unwrap();
    assert_eq!(buffer.pop(ready).unwrap().unwrap(), expected[2]);
    assert_eq!(buffer.pop(ready).unwrap().unwrap(), expected[3]);
}

#[test]
fn missing_frame_uses_opus_plc_and_recovers_across_sequence_wrap() {
    let now = Instant::now();
    let ready = now + CHUNK_TIME * 3;
    let mut buffer = PeerJitterBuffer::new(u32::MAX - 19, now, 3).unwrap();
    let mut encoder = encoder().unwrap();
    let mut decoder = Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
    let mut last = [0.0; FRAME_SAMPLES];
    for index in 0..20 {
        let frame = encode(&mut encoder, 0.5);
        buffer
            .insert((u32::MAX - 19).wrapping_add(index), &frame, now)
            .unwrap();
        last = decode(&mut decoder, &frame);
        assert_eq!(buffer.pop(ready).unwrap().unwrap(), last);
    }
    // Sequence 0 is lost, but the encoder still processes that capture frame.
    encode(&mut encoder, 0.5);
    let expected_plc = decode(&mut decoder, &[]);
    let concealed = buffer.pop(ready).unwrap().unwrap();
    assert_eq!(concealed, expected_plc);
    assert!(concealed.iter().any(|sample| sample.abs() > 0.01));
    assert_ne!(concealed, last.map(|sample| sample * 0.8));
    let step = (concealed[0] - last[FRAME_SAMPLES - 1]).abs();
    assert!(
        step < last[FRAME_SAMPLES - 1].abs(),
        "PLC reduces the boundary jump"
    );
    let resumed = encode(&mut encoder, 0.5);
    buffer.insert(1, &resumed, ready).unwrap();
    assert_eq!(
        buffer.pop(ready).unwrap().unwrap(),
        decode(&mut decoder, &resumed)
    );
    for _ in 0..500 {
        let frame = buffer.pop(ready).unwrap().unwrap();
        assert_eq!(frame, decode(&mut decoder, &[]));
        assert!(
            frame
                .iter()
                .all(|sample| sample.is_finite() && sample.abs() <= 1.0)
        );
    }
}

fn assert_continuous(previous: &Chunk, next: &Chunk) {
    let step = (next[0] - previous[FRAME_SAMPLES - 1]).abs();
    assert!(
        step < 0.08,
        "unexpected jump in a 0.5-amplitude 400 Hz tone: {step}"
    );
    assert!(next.iter().any(|sample| sample.abs() > 0.1));
}

#[test]
fn late_frame_hold_uses_plc_and_resumes_queued_audio_continuously() {
    let now = Instant::now();
    let mut buffer = PeerJitterBuffer::new(0, now, 3).unwrap();
    let ready = buffer.ready_at;
    let mut encoder = encoder().unwrap();
    let mut reference = Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
    for sequence in 0..20 {
        let frame = encode(&mut encoder, 0.5);
        buffer.insert(sequence, &frame, now).unwrap();
        assert_eq!(
            buffer.pop(ready).unwrap().unwrap(),
            decode(&mut reference, &frame)
        );
    }
    let late = encode(&mut encoder, 0.5);
    let previous = buffer.pop(ready).unwrap().unwrap();
    assert_eq!(previous, decode(&mut reference, &[]));
    buffer.insert(20, &late, ready).unwrap();
    assert_eq!(buffer.target, 4);
    let next = encode(&mut encoder, 0.5);
    buffer.insert(21, &next, ready).unwrap();
    let held = buffer.pop(ready).unwrap().unwrap();
    assert_eq!(held, decode(&mut reference, &[]));
    assert_ne!(held, previous.map(|sample| sample * 0.8));
    assert_eq!(buffer.next, 21, "hold must preserve the queued sequence");
    assert_continuous(&previous, &held);
    let resumed = buffer.pop(ready).unwrap().unwrap();
    assert_eq!(resumed, decode(&mut reference, &next));
    assert_continuous(&held, &resumed);
}

#[test]
fn stable_target_decay_trims_without_resetting_codec_history() {
    let start = Instant::now();
    let mut buffer = PeerJitterBuffer::new(0, start, 3).unwrap();
    let ready = buffer.ready_at;
    let mut encoder = encoder().unwrap();
    let mut reference = Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
    let mut frames = Vec::new();
    let mut previous = [0.0; FRAME_SAMPLES];
    for sequence in 0..20 {
        let frame = encode(&mut encoder, 0.5);
        frames.push(frame);
        buffer.insert(sequence, &frame, start).unwrap();
        previous = buffer.pop(ready).unwrap().unwrap();
        assert_eq!(previous, decode(&mut reference, &frame));
    }
    // Grow headroom twice using late packets; the queued backlog later forces trims.
    for sequence in 20..22 {
        let frame = encode(&mut encoder, 0.5);
        frames.push(frame);
        buffer.insert(sequence, &frame, ready).unwrap();
        buffer.insert(0, &frames[0], ready).unwrap();
        let held = buffer.pop(ready).unwrap().unwrap();
        assert_eq!(held, decode(&mut reference, &[]));
        assert_continuous(&previous, &held);
        previous = held;
    }
    assert_eq!(buffer.target, 5);
    // Fill the permitted span (including queue slack) before stability shrinks it.
    for sequence in 22..26 {
        let frame = encode(&mut encoder, 0.5);
        frames.push(frame);
        buffer.insert(sequence, &frame, ready).unwrap();
    }
    let mut trims = 0;
    for tick in 1..=850 {
        let sequence = u32::try_from(frames.len()).unwrap();
        let frame = encode(&mut encoder, 0.5);
        frames.push(frame);
        let now = ready + CHUNK_TIME * tick;
        let before = buffer.next;
        buffer.insert(sequence, &frame, now).unwrap();
        for skipped in before..buffer.next {
            decode(&mut reference, &frames[skipped as usize]);
            trims += 1;
        }
        let expected = decode(&mut reference, &frames[buffer.next as usize]);
        let output = buffer.pop(now).unwrap().unwrap();
        assert_eq!(output, expected, "codec history changed at tick {tick}");
        assert_continuous(&previous, &output);
        previous = output;
    }
    assert_eq!(trims, 2);
    assert_eq!(buffer.target, buffer.nominal_target);
}

#[test]
fn short_deadline_skips_decode_packets_and_conceal_holes_across_wrap() {
    let start = Instant::now();
    let initial = u32::MAX - 20;
    let mut buffer = PeerJitterBuffer::new(initial, start, 1).unwrap();
    let ready = buffer.ready_at;
    let mut encoder = encoder().unwrap();
    let mut reference = Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
    for offset in 0..20 {
        let frame = encode(&mut encoder, 0.5);
        buffer
            .insert(initial.wrapping_add(offset), &frame, start)
            .unwrap();
        assert_eq!(
            buffer.pop(ready).unwrap().unwrap(),
            decode(&mut reference, &frame)
        );
    }
    let discarded = encode(&mut encoder, 0.5);
    buffer.insert(u32::MAX, &discarded, ready).unwrap();
    decode(&mut reference, &discarded);
    encode(&mut encoder, 0.5); // Sequence 0 is missing.
    decode(&mut reference, &[]);
    let resumed = encode(&mut encoder, 0.5);
    buffer.insert(1, &resumed, ready).unwrap();
    buffer.skip_missed(ready, ready + CHUNK_TIME * 2).unwrap();
    assert_eq!(buffer.next, 1);
    assert_eq!(
        buffer.pop(ready).unwrap().unwrap(),
        decode(&mut reference, &resumed)
    );
}

#[test]
fn window_sized_and_very_large_skips_reset_codec_in_bounded_work() {
    for count in [WINDOW, u32::MAX] {
        let start = Instant::now();
        let mut buffer = PeerJitterBuffer::new(0, start, 1).unwrap();
        let ready = buffer.ready_at;
        let mut encoder = encoder().unwrap();
        for sequence in 0..20 {
            let frame = encode(&mut encoder, 0.5);
            buffer.insert(sequence, &frame, start).unwrap();
            buffer.pop(ready).unwrap();
        }
        let frame = encode(&mut encoder, 0.5);
        buffer.insert(20, &frame, ready).unwrap();
        buffer.skip(count).unwrap();
        assert_eq!(buffer.next, 20_u32.wrapping_add(count));
        assert_eq!(buffer.last, [0.0; FRAME_SAMPLES]);
        assert!(buffer.slots.iter().all(Option::is_none));
        let mut fresh = Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
        assert_eq!(buffer.pop(ready).unwrap().unwrap(), decode(&mut fresh, &[]));
    }
}

#[test]
fn discontinuity_resets_only_affected_decoder_with_bounded_storage() {
    let now = Instant::now();
    let ready = now + CHUNK_TIME * 3;
    let mut a = PeerJitterBuffer::new(0, now, 3).unwrap();
    let mut b = PeerJitterBuffer::new(0, now, 3).unwrap();
    let mut encoder = encoder().unwrap();
    let frame = encode(&mut encoder, 0.5);
    a.insert(0, &frame, now).unwrap();
    b.insert(0, &frame, now).unwrap();
    a.pop(ready).unwrap();
    b.pop(ready).unwrap();
    a.insert(1000, &frame, ready).unwrap();
    b.insert(1, &frame, ready).unwrap();
    assert!(a.pop(ready).unwrap().is_none());
    let mut fresh = Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
    assert_eq!(
        a.pop(ready + CHUNK_TIME * 3).unwrap().unwrap(),
        decode(&mut fresh, &frame)
    );
    let mut continuous = Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
    decode(&mut continuous, &frame);
    assert_eq!(
        b.pop(ready).unwrap().unwrap(),
        decode(&mut continuous, &frame)
    );
    assert_eq!(a.slots.len(), CAPACITY);
}

#[test]
fn corrupt_packet_is_concealed_without_stopping_playout() {
    let now = Instant::now();
    let mut buffer = PeerJitterBuffer::new(0, now, 1).unwrap();
    // Code 3 with zero frames is an invalid Opus packet.
    buffer
        .insert(0, &[3; crate::codec::PAYLOAD_SIZE], now)
        .unwrap();
    assert!(
        buffer
            .pop(now + CHUNK_TIME)
            .unwrap()
            .unwrap()
            .iter()
            .all(|sample| sample.is_finite())
    );
}

#[test]
fn stable_stream_returns_to_nominal_after_late_packets() {
    let start = Instant::now();
    let mut buffer = PeerJitterBuffer::new(0, start, 3).unwrap();
    let frame = encode(&mut encoder().unwrap(), 0.5);
    let ready = buffer.ready_at;
    buffer.insert(0, &frame, start).unwrap();
    buffer.pop(ready).unwrap();
    // Repeated late frames consume holds and raise headroom to the maximum.
    for sequence in 1..=13 {
        buffer.insert(sequence, &frame, ready).unwrap();
        buffer.insert(0, &frame, ready).unwrap();
        buffer.pop(ready).unwrap();
    }
    assert_eq!(buffer.target, MAX_TARGET);
    let mut sequence = 14;
    let mut maximum = 0.0_f64;
    for tick in 1..=6000 {
        let now = ready + CHUNK_TIME * tick;
        buffer.insert(sequence, &frame, now).unwrap();
        sequence = sequence.wrapping_add(1);
        buffer.pop(now).unwrap();
        maximum = maximum.max(buffer.buffered_ms());
    }
    assert_eq!(buffer.target, buffer.nominal_target);
    assert!(buffer.buffered_ms() <= 10.0);
    assert!(maximum <= 45.0);
}

#[test]
fn backlog_is_bounded_by_sequence_span_including_wrap_and_holes() {
    let start = Instant::now();
    let initial = u32::MAX - 10;
    let mut buffer = PeerJitterBuffer::new(initial, start, 3).unwrap();
    let frame = encode(&mut encoder().unwrap(), 0.5);
    for offset in 0..60 {
        buffer
            .insert(initial.wrapping_add(offset), &frame, start)
            .unwrap();
        assert!(buffer.buffered_ms() <= 12.5);
    }
    assert_eq!(buffer.next, initial.wrapping_add(55));
    assert!(
        buffer
            .slots
            .iter()
            .flatten()
            .all(|(seq, _)| seq.wrapping_sub(buffer.next) < 5)
    );
}

#[test]
fn missed_deadlines_respect_startup_and_skip_large_stalls_in_constant_work() {
    let start = Instant::now();
    let mut buffer = PeerJitterBuffer::new(u32::MAX - 2, start, 3).unwrap();
    buffer.skip_missed(start, start + CHUNK_TIME * 2).unwrap();
    assert_eq!(buffer.next, u32::MAX - 2);
    buffer.skip_missed(start, start + CHUNK_TIME * 10).unwrap();
    assert_eq!(buffer.next, 4);
    assert!(buffer.started);
    let frame = encode(&mut encoder().unwrap(), 0.5);
    buffer.insert(3, &frame, start + CHUNK_TIME * 10).unwrap();
    assert_eq!(
        buffer.next, 4,
        "old packets must not rewind a skipped startup"
    );
}

#[test]
fn peer_starting_between_global_ticks_does_not_skip_its_first_frame() {
    let start = Instant::now();
    let mut buffer = PeerJitterBuffer::new(100, start + Duration::from_micros(700), 3).unwrap();
    buffer.skip_missed(start, start + CHUNK_TIME * 4).unwrap();
    assert_eq!(buffer.next, 100);
    buffer.skip_missed(start, start + CHUNK_TIME * 5).unwrap();
    assert_eq!(buffer.next, 101);
}
