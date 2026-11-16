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

#[test]
fn late_frame_hold_does_not_advance_decoder_or_replay_the_late_packet() {
    let now = Instant::now();
    let ready = now + CHUNK_TIME * 3;
    let mut buffer = PeerJitterBuffer::new(0, now, 3).unwrap();
    let mut encoder = encoder().unwrap();
    let first = encode(&mut encoder, 0.5);
    buffer.insert(0, &first, now).unwrap();
    buffer.pop(ready).unwrap();
    let lost = encode(&mut encoder, 0.5);
    let plc = buffer.pop(ready).unwrap().unwrap();
    buffer.insert(1, &lost, ready).unwrap();
    assert_eq!(buffer.target, 4);
    let next = encode(&mut encoder, 0.5);
    buffer.insert(2, &next, ready).unwrap();
    assert_eq!(
        buffer.pop(ready).unwrap().unwrap(),
        plc.map(|sample| sample * 0.8)
    );
    let mut reference = Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
    decode(&mut reference, &first);
    decode(&mut reference, &[]);
    assert_eq!(
        buffer.pop(ready).unwrap().unwrap(),
        decode(&mut reference, &next)
    );
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
