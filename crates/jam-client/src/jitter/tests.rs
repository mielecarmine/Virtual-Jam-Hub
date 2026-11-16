use super::*;

#[test]
fn reorders_startup_and_running_packets_and_rejects_duplicates() {
    let now = Instant::now();
    let mut buffer = PeerJitterBuffer::new(102, now, 3);
    buffer.insert(102, &[0.2; BUFFER_FRAMES], now);
    buffer.insert(101, &[0.1; BUFFER_FRAMES], now);
    buffer.insert(102, &[0.9; BUFFER_FRAMES], now);
    assert!(buffer.pop(now + CHUNK_TIME * 2).is_none());
    let ready = now + CHUNK_TIME * 3;
    assert_eq!(buffer.pop(ready).unwrap()[0], 0.1);
    assert_eq!(buffer.pop(ready).unwrap()[0], 0.2);
    buffer.insert(104, &[0.4; BUFFER_FRAMES], ready);
    buffer.insert(103, &[0.3; BUFFER_FRAMES], ready);
    assert_eq!(buffer.pop(ready).unwrap()[0], 0.3);
    assert_eq!(buffer.pop(ready).unwrap()[0], 0.4);
}

#[test]
fn wraps_conceals_loss_and_adapts_to_late_frames_without_replaying_them() {
    let now = Instant::now();
    let ready = now + CHUNK_TIME * 3;
    let mut buffer = PeerJitterBuffer::new(u32::MAX, now, 3);
    buffer.insert(u32::MAX, &[0.5; BUFFER_FRAMES], now);
    buffer.insert(0, &[0.25; BUFFER_FRAMES], now);
    assert_eq!(buffer.pop(ready).unwrap()[0], 0.5);
    assert_eq!(buffer.pop(ready).unwrap()[0], 0.25);
    assert_eq!(buffer.pop(ready).unwrap()[0], 0.2);
    buffer.insert(1, &[1.0; BUFFER_FRAMES], ready);
    assert_eq!(buffer.target, 4);
    buffer.insert(2, &[0.75; BUFFER_FRAMES], ready);
    assert!((buffer.pop(ready).unwrap()[0] - 0.16).abs() < 0.0001);
    assert_eq!(buffer.pop(ready).unwrap()[0], 0.75);
    for _ in 0..100 {
        buffer.pop(ready);
    }
    assert!(buffer.last[0].abs() < 0.0001);
}

#[test]
fn discontinuity_resets_only_affected_peer_with_bounded_storage() {
    let now = Instant::now();
    let mut a = PeerJitterBuffer::new(0, now, 3);
    let mut b = PeerJitterBuffer::new(0, now, 3);
    a.insert(1000, &[0.5; BUFFER_FRAMES], now);
    b.insert(0, &[-0.5; BUFFER_FRAMES], now);
    assert_eq!(a.pop(now + CHUNK_TIME * 3).unwrap()[0], 0.5);
    assert_eq!(b.pop(now + CHUNK_TIME * 3).unwrap()[0], -0.5);
    assert_eq!(a.slots.len(), CAPACITY);
}
