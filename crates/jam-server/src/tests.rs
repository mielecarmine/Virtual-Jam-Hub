use super::*;
use jam_protocol::{MAX_PEERS, NICKNAME_CAPACITY};

fn source(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn handle(server: &mut Server, message: &ControlMessage, source: SocketAddr, now: Instant) {
    let mut packet = [0; MAX_PACKET_SIZE];
    let length = message.to_bytes(&mut packet).unwrap();
    server.handle(&packet[..length], source, now);
}

fn join() -> ControlMessage {
    ControlMessage::JoinRequest {
        nickname: [b'a'; NICKNAME_CAPACITY],
        nickname_len: 1,
    }
}

#[test]
fn full_room_retries_and_lowest_free_id() {
    let mut table = SessionTable::default();
    let now = Instant::now();
    for id in 1..=u8::try_from(MAX_PEERS).unwrap() {
        assert_eq!(
            table.join(source(u16::from(id)), [b'a'; NICKNAME_CAPACITY], 1, now),
            Some(id)
        );
    }
    assert_eq!(table.join(source(9), [0; NICKNAME_CAPACITY], 0, now), None);
    // A retry from an existing endpoint remains idempotent, even in a full room.
    assert_eq!(
        table.join(source(2), [b'b'; NICKNAME_CAPACITY], 2, now),
        Some(2)
    );
    assert_eq!(table.peers().len(), MAX_PEERS);
    assert_eq!(table.peers()[1].nickname, [b'b'; NICKNAME_CAPACITY]);
    assert!(!table.remove(source(2), 3));
    assert!(table.remove(source(2), 2));
    assert_eq!(
        table.join(source(9), [0; NICKNAME_CAPACITY], 0, now),
        Some(2)
    );
}

#[test]
fn timeout_boundary_and_authenticated_activity() {
    let mut table = SessionTable::default();
    let now = Instant::now();
    table.join(source(1), [0; NICKNAME_CAPACITY], 0, now);
    table.join(source(2), [0; NICKNAME_CAPACITY], 0, now);
    assert!(!table.expire(now + SESSION_TIMEOUT));
    table.touch(source(1), 2, now + SESSION_TIMEOUT); // Wrong peer ID.
    table.touch(source(3), 1, now + SESSION_TIMEOUT); // Unknown address.
    table.touch(source(2), 2, now + SESSION_TIMEOUT);
    assert!(table.expire(now + SESSION_TIMEOUT + Duration::from_nanos(1)));
    assert_eq!(table.peers().len(), 1);
    assert_eq!(table.peers()[0].peer_id, 2);
}

#[test]
fn only_valid_client_packets_refresh_sessions() {
    let mut server = Server::bind("127.0.0.1:0").unwrap();
    // Real destinations avoid ICMP errors from unbound local ports.
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    let source = client.local_addr().unwrap();
    let now = Instant::now();
    handle(&mut server, &join(), source, now);
    let later = now + Duration::from_secs(4);
    server.handle(&[0; 16], source, later);
    let mut spoofed = [0; 120];
    AudioHeader::new(2, jam_protocol::STREAM_PRIMARY, 0, 0)
        .to_bytes(&mut spoofed)
        .unwrap();
    server.handle(&spoofed, source, later);
    handle(
        &mut server,
        &ControlMessage::JoinAccepted {
            assigned_peer_id: 1,
            sample_rate: SAMPLE_RATE,
        },
        source,
        later,
    );
    handle(
        &mut server,
        &ControlMessage::Leave { peer_id: 2 },
        source,
        later,
    );
    assert!(
        server
            .sessions
            .expire(now + SESSION_TIMEOUT + Duration::from_millis(1))
    );

    handle(&mut server, &join(), source, now);
    handle(
        &mut server,
        &ControlMessage::Ping {
            client_timestamp_us: 123,
        },
        source,
        later,
    );
    assert!(!server.sessions.expire(now + Duration::from_secs(6)));
    let mut packet = [0; 16];
    AudioHeader::new(1, jam_protocol::STREAM_PRIMARY, 0, 0)
        .to_bytes(&mut packet)
        .unwrap();
    server.handle(&packet, source, now + Duration::from_secs(8));
    assert!(!server.sessions.expire(now + Duration::from_secs(10)));
}

#[test]
fn opaque_nickname_bytes_are_preserved() {
    let mut table = SessionTable::default();
    let nickname = [0xff; NICKNAME_CAPACITY];
    table.join(source(1), nickname, 3, Instant::now());
    let peer = table.peers()[0];
    assert_eq!(peer.nickname, nickname);
    assert_eq!(peer.nickname_len, 3);
}

#[test]
fn watchdog_expires_sessions_even_without_incoming_packets() {
    let mut server = Server::bind("127.0.0.1:0").unwrap();
    server.sessions.join(
        source(1),
        [0; NICKNAME_CAPACITY],
        0,
        Instant::now()
            .checked_sub(SESSION_TIMEOUT + Duration::from_millis(1))
            .unwrap(),
    );
    server.poll().unwrap();
    assert_eq!(server.sessions.peers(), Vec::new());
}

#[test]
fn oversized_and_malformed_datagrams_cannot_register_clients() {
    let mut server = Server::bind("127.0.0.1:0").unwrap();
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client.connect(server.local_addr().unwrap()).unwrap();
    // A valid JoinRequest prefix must not conceal a ceiling-sized or truncated packet.
    let mut packet = [0; MAX_PACKET_SIZE + 100];
    join().to_bytes(&mut packet).unwrap();
    for length in [MAX_PACKET_SIZE, MAX_PACKET_SIZE + 100, 2] {
        client.send(&packet[..length]).unwrap();
        server.poll().unwrap();
        assert_eq!(server.sessions.peers(), Vec::new());
    }
}

#[test]
fn expired_peer_cannot_send_or_receive_audio() {
    let mut server = Server::bind("127.0.0.1:0").unwrap();
    let active = UdpSocket::bind("127.0.0.1:0").unwrap();
    let expired = UdpSocket::bind("127.0.0.1:0").unwrap();
    active.connect(server.local_addr().unwrap()).unwrap();
    expired.connect(server.local_addr().unwrap()).unwrap();
    active.set_nonblocking(true).unwrap();
    expired.set_nonblocking(true).unwrap();
    let now = Instant::now();
    server
        .sessions
        .join(active.local_addr().unwrap(), [0; NICKNAME_CAPACITY], 0, now);
    server.sessions.join(
        expired.local_addr().unwrap(),
        [0; NICKNAME_CAPACITY],
        0,
        now.checked_sub(SESSION_TIMEOUT + Duration::from_secs(1))
            .unwrap(),
    );
    let mut packet = [0; 120];
    AudioHeader::new(2, jam_protocol::STREAM_PRIMARY, 0, 0)
        .to_bytes(&mut packet)
        .unwrap();
    expired.send(&packet).unwrap();
    server.poll().unwrap();
    assert!(server.last_reflection_duration().is_none());
    let mut buffer = [0; MAX_PACKET_SIZE];
    let length = active.recv(&mut buffer).unwrap();
    assert!(matches!(
        ControlMessage::from_bytes(&buffer[..length]).unwrap(),
        ControlMessage::PeerListUpdate { peers } if peers.len() == 1
    ));
    AudioHeader::new(1, jam_protocol::STREAM_PRIMARY, 0, 0)
        .to_bytes(&mut packet)
        .unwrap();
    active.send(&packet).unwrap();
    server.poll().unwrap();
    for client in [&active, &expired] {
        assert_eq!(
            client.recv(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
