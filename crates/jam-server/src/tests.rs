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

fn room_client(server: &mut Server, now: Instant) -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    server
        .sessions
        .join(socket.local_addr().unwrap(), [0; NICKNAME_CAPACITY], 0, now)
        .unwrap();
    socket
}

fn receive_control(socket: &UdpSocket) -> ControlMessage {
    let mut bytes = [0; MAX_PACKET_SIZE];
    let length = socket.recv(&mut bytes).unwrap();
    ControlMessage::from_bytes(&bytes[..length]).unwrap()
}

#[test]
fn metronome_broadcast_join_and_server_owned_anchor() {
    let mut server = Server::bind("127.0.0.1:0").unwrap();
    let start = server.started;
    let alice = room_client(&mut server, start);
    let bob = room_client(&mut server, start);
    let mut config = jam_protocol::MetronomeConfig {
        enabled: true,
        leader_peer_id: 1,
        ..Default::default()
    };
    handle(
        &mut server,
        &ControlMessage::MetronomeUpdate { config },
        alice.local_addr().unwrap(),
        start + Duration::from_secs(1),
    );
    let first = server.metronome;
    assert_eq!(first.anchor_server_timestamp_us, 1_000_000);
    for client in [&alice, &bob] {
        assert_eq!(
            receive_control(client),
            ControlMessage::MetronomeState { state: first }
        );
    }
    // A retry and a meter/leader change preserve the room phase.
    for change in [
        config,
        jam_protocol::MetronomeConfig {
            beats_per_bar: 3,
            leader_peer_id: 2,
            ..config
        },
    ] {
        handle(
            &mut server,
            &ControlMessage::MetronomeUpdate { config: change },
            alice.local_addr().unwrap(),
            start + Duration::from_secs(2),
        );
        assert_eq!(
            server.metronome.anchor_server_timestamp_us,
            first.anchor_server_timestamp_us
        );
        for client in [&alice, &bob] {
            assert_eq!(
                receive_control(client),
                ControlMessage::MetronomeState {
                    state: server.metronome
                }
            );
        }
    }
    config.bpm = 140;
    handle(
        &mut server,
        &ControlMessage::MetronomeUpdate { config },
        bob.local_addr().unwrap(),
        start + Duration::from_secs(3),
    );
    assert_eq!(server.metronome.anchor_server_timestamp_us, 3_000_000);
    for client in [&alice, &bob] {
        let _ = receive_control(client);
    }
    let carol = UdpSocket::bind("127.0.0.1:0").unwrap();
    carol
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    handle(
        &mut server,
        &join(),
        carol.local_addr().unwrap(),
        start + Duration::from_secs(4),
    );
    assert!(matches!(
        receive_control(&carol),
        ControlMessage::JoinAccepted {
            assigned_peer_id: 3,
            ..
        }
    ));
    assert!(
        matches!(receive_control(&carol), ControlMessage::PeerListUpdate { peers } if peers.len() == 3)
    );
    assert_eq!(
        receive_control(&carol),
        ControlMessage::MetronomeState {
            state: server.metronome
        }
    );
}

#[test]
fn leader_departure_or_timeout_notifies_survivors_and_keeps_phase() {
    for timeout in [false, true] {
        let mut server = Server::bind("127.0.0.1:0").unwrap();
        let start = server.started;
        let leader = room_client(&mut server, start);
        let follower = room_client(&mut server, start);
        server.metronome = MetronomeState {
            config: jam_protocol::MetronomeConfig {
                enabled: true,
                leader_peer_id: 1,
                ..Default::default()
            },
            anchor_server_timestamp_us: 123,
        };
        if timeout {
            server
                .sessions
                .touch_source(follower.local_addr().unwrap(), start + SESSION_TIMEOUT);
            server.expire_sessions(start + SESSION_TIMEOUT + Duration::from_micros(1));
        } else {
            handle(
                &mut server,
                &ControlMessage::Leave { peer_id: 1 },
                leader.local_addr().unwrap(),
                start,
            );
        }
        assert_eq!(server.metronome.config.leader_peer_id, 0);
        assert_eq!(server.metronome.anchor_server_timestamp_us, 123);
        assert!(server.metronome.config.enabled);
        assert!(
            matches!(receive_control(&follower), ControlMessage::PeerListUpdate { peers } if peers.len() == 1)
        );
        assert_eq!(
            receive_control(&follower),
            ControlMessage::MetronomeState {
                state: server.metronome
            }
        );
        // The reused ID must not inherit leadership.
        assert_eq!(
            server.sessions.join(
                leader.local_addr().unwrap(),
                [0; NICKNAME_CAPACITY],
                0,
                start + SESSION_TIMEOUT
            ),
            Some(1)
        );
        assert_eq!(server.metronome.config.leader_peer_id, 0);
    }
}

#[test]
fn metronome_rejects_outsiders_inactive_leaders_and_forged_state() {
    let mut server = Server::bind("127.0.0.1:0").unwrap();
    let start = server.started;
    let member = room_client(&mut server, start);
    let outsider = UdpSocket::bind("127.0.0.1:0").unwrap();
    let config = jam_protocol::MetronomeConfig {
        enabled: true,
        ..Default::default()
    };
    handle(
        &mut server,
        &ControlMessage::MetronomeUpdate { config },
        outsider.local_addr().unwrap(),
        start,
    );
    handle(
        &mut server,
        &ControlMessage::MetronomeUpdate {
            config: jam_protocol::MetronomeConfig {
                leader_peer_id: 8,
                ..config
            },
        },
        member.local_addr().unwrap(),
        start,
    );
    handle(
        &mut server,
        &ControlMessage::MetronomeState {
            state: MetronomeState {
                config,
                anchor_server_timestamp_us: 123,
            },
        },
        member.local_addr().unwrap(),
        start,
    );
    assert_eq!(server.metronome, MetronomeState::default());
    member.set_nonblocking(true).unwrap();
    assert_eq!(
        member.recv(&mut [0; MAX_PACKET_SIZE]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn disable_preserves_anchor_and_reenable_starts_new_beat_zero() {
    let mut server = Server::bind("127.0.0.1:0").unwrap();
    let start = server.started;
    let client = room_client(&mut server, start);
    for (second, enabled, anchor) in [
        (1, true, 1_000_000),
        (2, false, 1_000_000),
        (3, true, 3_000_000),
    ] {
        let config = jam_protocol::MetronomeConfig {
            enabled,
            ..Default::default()
        };
        handle(
            &mut server,
            &ControlMessage::MetronomeUpdate { config },
            client.local_addr().unwrap(),
            start + Duration::from_secs(second),
        );
        assert_eq!(server.metronome.anchor_server_timestamp_us, anchor);
        assert_eq!(
            receive_control(&client),
            ControlMessage::MetronomeState {
                state: server.metronome
            }
        );
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
    active
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
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
    active.set_nonblocking(true).unwrap();
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
