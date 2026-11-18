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
    let mut previous = [0.0; FRAME_SAMPLES];
    for sequence in 20..23 {
        let late = encode(&mut encoder, 0.5);
        previous = buffer.pop(ready).unwrap().unwrap();
        assert_eq!(previous, decode(&mut reference, &[]));
        buffer.insert(sequence, &late, ready).unwrap();
    }
    assert_eq!(buffer.target, 4);
    let next = encode(&mut encoder, 0.5);
    buffer.insert(23, &next, ready).unwrap();
    let held = buffer.pop(ready).unwrap().unwrap();
    assert_eq!(held, decode(&mut reference, &[]));
    assert_ne!(held, previous.map(|sample| sample * 0.8));
    assert_eq!(buffer.next, 23, "hold must preserve the queued sequence");
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
    // Exercise two PLC holds; adaptation thresholds are tested separately.
    for sequence in 20..22 {
        let frame = encode(&mut encoder, 0.5);
        frames.push(frame);
        buffer.insert(sequence, &frame, ready).unwrap();
        buffer.target += 1;
        buffer.hold = true;
        let held = buffer.pop(ready).unwrap().unwrap();
        assert_eq!(held, decode(&mut reference, &[]));
        assert_continuous(&previous, &held);
        previous = held;
    }
    assert_eq!(buffer.target, 5);
    // Fill the permitted span (including queue slack) before stability shrinks it.
    for sequence in 22..27 {
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
        buffer.trim_backlog().unwrap();
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
    buffer
        .playout_frames(ready, ready + CHUNK_TIME * 2, 1)
        .unwrap();
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
    // Sustained real lateness (three distinct missed frames per increase).
    let mut sequence = 1;
    for _ in 0..20 {
        for _ in 0..LATE_THRESHOLD {
            buffer.pop(ready).unwrap();
            buffer.insert(sequence, &frame, ready).unwrap();
            sequence += 1;
        }
        if buffer.hold {
            buffer.pop(ready).unwrap(); // Consume the PLC hold only when headroom grew.
        }
    }
    assert_eq!(buffer.target, MAX_ADAPTIVE_TARGET);
    let mut maximum = 0.0_f64;
    for tick in 1..=6000 {
        let now = ready + CHUNK_TIME * tick;
        buffer.insert(sequence, &frame, now).unwrap();
        sequence = sequence.wrapping_add(1);
        buffer.trim_backlog().unwrap();
        buffer.pop(now).unwrap();
        maximum = maximum.max(buffer.buffered_ms());
    }
    assert_eq!(buffer.target, buffer.nominal_target);
    assert!(buffer.buffered_ms() <= 10.0);
    assert!(maximum <= 27.5);
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
        buffer.trim_backlog().unwrap();
        assert!(buffer.buffered_ms() <= 15.0);
    }
    assert_eq!(buffer.next, initial.wrapping_add(54));
    assert!(
        buffer
            .slots
            .iter()
            .flatten()
            .all(|(seq, _)| seq.wrapping_sub(buffer.next) < 6)
    );
}

#[test]
fn missed_deadlines_respect_startup_and_skip_large_stalls_in_constant_work() {
    let start = Instant::now();
    let mut buffer = PeerJitterBuffer::new(u32::MAX - 2, start, 3).unwrap();
    buffer
        .playout_frames(start, start + CHUNK_TIME * 2, 1)
        .unwrap();
    assert_eq!(buffer.next, u32::MAX - 2);
    buffer
        .playout_frames(start, start + CHUNK_TIME * 10, 1)
        .unwrap();
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
    buffer
        .playout_frames(start, start + CHUNK_TIME * 4, 1)
        .unwrap();
    assert_eq!(buffer.next, 100);
    buffer
        .playout_frames(start, start + CHUNK_TIME * 5, 1)
        .unwrap();
    assert_eq!(buffer.next, 101);
}

#[test]
fn delayed_receive_batch_is_decoded_in_order_without_target_growth_across_wrap() {
    let start = Instant::now();
    let initial = u32::MAX - 1;
    let mut buffer = PeerJitterBuffer::new(initial, start, 3).unwrap();
    let mut encoder = encoder().unwrap();
    let mut reference = Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
    let ready = buffer.ready_at;
    // All packets from this delayed tick are inserted before recovering deadlines.
    for offset in 0..6 {
        let frame = encode(&mut encoder, 0.5);
        buffer
            .insert(initial.wrapping_add(offset), &frame, ready + CHUNK_TIME * 2)
            .unwrap();
    }
    assert_eq!(
        buffer.next, initial,
        "insertion must not trim a receive batch"
    );
    let frames = buffer
        .playout_frames(ready, ready + CHUNK_TIME * 2, 4)
        .unwrap();
    assert_eq!(frames, 3);
    let mut encoder = crate::codec::encoder().unwrap();
    for _ in 0..frames {
        let frame = encode(&mut encoder, 0.5);
        assert_eq!(
            buffer.pop(ready).unwrap().unwrap(),
            decode(&mut reference, &frame)
        );
    }
    assert_eq!(buffer.next, 1);
    assert_eq!(buffer.target, 3);
    assert_eq!(buffer.buffered_ms(), 7.5);
}

#[test]
fn isolated_lateness_and_duplicate_replays_do_not_inflate_target() {
    let start = Instant::now();
    let frame = encode(&mut encoder().unwrap(), 0.5);
    let mut buffer = PeerJitterBuffer::new(0, start, 3).unwrap();
    let ready = buffer.ready_at;
    buffer.pop(ready).unwrap();
    for _ in 0..100 {
        buffer.insert(0, &frame, ready).unwrap();
    }
    assert_eq!(buffer.target, 3);
    assert!(!buffer.hold);
    // Distinct isolated late frames separated by stable windows also stay nominal.
    for sequence in 1..10 {
        let now = ready + STABLE_WINDOW * sequence;
        buffer.pop(now).unwrap();
        buffer.insert(sequence, &frame, now).unwrap();
        assert_eq!(buffer.target, 3);
    }
}

#[test]
fn catch_up_respects_each_peers_startup_and_downstream_capacity() {
    let start = Instant::now();
    for capacity in 0..=4 {
        let mut buffer = PeerJitterBuffer::new(100, start, 3).unwrap();
        let ready = buffer.ready_at;
        let frames = buffer
            .playout_frames(ready, ready + CHUNK_TIME * 5, capacity)
            .unwrap();
        assert_eq!(frames, u32::try_from(capacity.max(1)).unwrap());
        assert_eq!(buffer.next, 106 - frames);
    }
    let mut later = PeerJitterBuffer::new(100, start + Duration::from_micros(700), 3).unwrap();
    assert_eq!(
        later
            .playout_frames(start, start + CHUNK_TIME * 5, 4)
            .unwrap(),
        2
    );
    assert_eq!(later.next, 100);
}

#[test]
fn intentionally_skipped_frames_do_not_count_as_network_lateness() {
    let start = Instant::now();
    let mut buffer = PeerJitterBuffer::new(0, start, 3).unwrap();
    let ready = buffer.ready_at;
    let frame = encode(&mut encoder().unwrap(), 0.5);
    buffer
        .playout_frames(ready, ready + CHUNK_TIME * 5, 1)
        .unwrap();
    for sequence in 0..5 {
        buffer
            .insert(sequence, &frame, ready + CHUNK_TIME * 5)
            .unwrap();
    }
    assert_eq!(buffer.target, 3);
    assert_eq!(buffer.late_count, 0);
    assert!(!buffer.hold);
}

#[test]
fn diagnostics_count_late_ranges_plc_skips_and_reanchors() {
    let now = Instant::now();
    let ready = now + CHUNK_TIME * 3;
    let chunk = encode(&mut encoder().unwrap(), 0.5);
    let mut peer = PeerJitterBuffer::new(100, now, 3).unwrap();
    peer.insert(100, &chunk, now).unwrap();
    peer.pop(ready).unwrap();
    peer.insert(100, &chunk, ready).unwrap();
    peer.insert(101 - WINDOW, &chunk, ready).unwrap();
    assert_eq!(peer.diagnostics.late_dropped, 2);
    assert_eq!(peer.diagnostics.late_dropped_far, 1);
    peer.pop(ready).unwrap();
    assert_eq!(peer.diagnostics.plc_frames, 1);
    peer.hold = true;
    peer.pop(ready).unwrap();
    assert_eq!(peer.diagnostics.plc_frames, 2);
    peer.skip(2).unwrap();
    assert_eq!(peer.diagnostics.skipped_frames, 2);
    assert_eq!(peer.diagnostics.plc_frames, 4);
    peer.skip(WINDOW).unwrap();
    assert_eq!(peer.diagnostics.skipped_frames, 2 + WINDOW);
    assert_eq!(peer.diagnostics.plc_frames, 4);
    assert_eq!(peer.diagnostics.resyncs, 1);
    peer.insert(peer.next.wrapping_add(WINDOW), &chunk, ready)
        .unwrap();
    assert_eq!(peer.diagnostics.resyncs, 2);
    assert_eq!(peer.diagnostics.late_dropped, 2);
}

#[test]
fn backlog_trim_counts_only_skipped_frames_and_corruption_counts_plc() {
    let now = Instant::now();
    let mut peer = PeerJitterBuffer::new(0, now, 3).unwrap();
    let mut enc = encoder().unwrap();
    for sequence in 0..12 {
        peer.insert(sequence, &encode(&mut enc, 0.5), now).unwrap();
    }
    peer.trim_backlog().unwrap();
    assert_eq!(peer.diagnostics.skipped_frames, 6);
    assert_eq!(peer.diagnostics.plc_frames, 0);
    assert_eq!(peer.diagnostics.resyncs, 0);
    let mut corrupt = PeerJitterBuffer::new(0, now, 1).unwrap();
    corrupt
        .insert(0, &[0xff; crate::codec::PAYLOAD_SIZE], now)
        .unwrap();
    corrupt.pop(now + CHUNK_TIME).unwrap();
    assert_eq!(corrupt.diagnostics.plc_frames, 1);
}
