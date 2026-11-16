use crate::{
    ControlMessage, MAX_PACKET_SIZE, MAX_PEERS, NICKNAME_CAPACITY, PROTOCOL_MAGIC, PeerInfo,
    ProtocolError, SAMPLE_RATE,
};

fn peer(peer_id: u8) -> PeerInfo {
    PeerInfo {
        peer_id,
        nickname: [b'A'; NICKNAME_CAPACITY],
        nickname_len: 32,
    }
}

fn messages() -> Vec<ControlMessage> {
    vec![
        ControlMessage::JoinRequest {
            nickname: [b'Z'; 32],
            nickname_len: 32,
        },
        ControlMessage::JoinRequest {
            nickname: [0; 32],
            nickname_len: 0,
        },
        ControlMessage::JoinAccepted {
            assigned_peer_id: 8,
            sample_rate: SAMPLE_RATE,
        },
        ControlMessage::PeerListUpdate { peers: vec![] },
        ControlMessage::PeerListUpdate {
            peers: (1..=8).map(peer).collect(),
        },
        ControlMessage::Ping {
            client_timestamp_us: 0,
        },
        ControlMessage::Ping {
            client_timestamp_us: u64::MAX,
        },
        ControlMessage::Pong {
            client_timestamp_us: u64::MAX,
            server_timestamp_us: 0x0807_0605_0403_0201,
        },
        ControlMessage::Leave { peer_id: 1 },
    ]
}

#[test]
fn every_message_round_trips_and_only_modifies_its_output_prefix() {
    for message in messages() {
        let mut buf = [0xCC; MAX_PACKET_SIZE];
        let size = message.to_bytes(&mut buf).unwrap();
        assert!(size < MAX_PACKET_SIZE);
        assert_eq!(
            ControlMessage::from_bytes(&buf[..size]),
            Ok(message.clone())
        );
        assert!(buf[size..].iter().all(|&byte| byte == 0xCC));
        let mut exact = vec![0; size];
        assert_eq!(message.to_bytes(&mut exact), Ok(size));
        assert_eq!(exact, buf[..size]);
    }
}

#[test]
fn every_truncation_and_short_output_buffer_is_rejected() {
    for message in messages() {
        let mut wire = [0; MAX_PACKET_SIZE];
        let size = message.to_bytes(&mut wire).unwrap();
        for length in 0..size {
            assert_eq!(
                ControlMessage::from_bytes(&wire[..length]),
                Err(ProtocolError::TruncatedPacket)
            );
            let mut output = vec![0xCC; length];
            assert_eq!(
                message.to_bytes(&mut output),
                Err(ProtocolError::BufferTooSmall)
            );
            assert!(output.iter().all(|&byte| byte == 0xCC));
        }
        wire[size] = 0;
        assert_eq!(
            ControlMessage::from_bytes(&wire[..=size]),
            Err(ProtocolError::TrailingBytes)
        );
    }
}

#[test]
fn wire_layout_is_explicit_little_endian_with_fixed_fields() {
    let mut cases = vec![
        (
            ControlMessage::JoinAccepted {
                assigned_peer_id: 2,
                sample_rate: SAMPLE_RATE,
            },
            vec![0x41, 0x4A, 0x03, 2, 0x80, 0xBB, 0, 0],
        ),
        (
            ControlMessage::Ping {
                client_timestamp_us: 0x0807_0605_0403_0201,
            },
            vec![0x41, 0x4A, 0x05, 1, 2, 3, 4, 5, 6, 7, 8],
        ),
        (
            ControlMessage::Pong {
                client_timestamp_us: 0x0807_0605_0403_0201,
                server_timestamp_us: 0x100F_0E0D_0C0B_0A09,
            },
            vec![
                0x41, 0x4A, 0x06, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
            ],
        ),
        (
            ControlMessage::Leave { peer_id: 8 },
            vec![0x41, 0x4A, 0x07, 8],
        ),
    ];
    let mut join = vec![0x41, 0x4A, 0x02];
    join.extend_from_slice(&[b'A'; 32]);
    join.push(32);
    cases.push((
        ControlMessage::JoinRequest {
            nickname: [b'A'; 32],
            nickname_len: 32,
        },
        join,
    ));
    let mut list = vec![0x41, 0x4A, 0x04, 2];
    for id in [1, 8] {
        list.push(id);
        list.extend_from_slice(&[b'A'; 32]);
        list.push(32);
    }
    cases.push((
        ControlMessage::PeerListUpdate {
            peers: vec![peer(1), peer(8)],
        },
        list,
    ));
    for (message, expected) in cases {
        let mut buf = [0; MAX_PACKET_SIZE];
        let size = message.to_bytes(&mut buf).unwrap();
        assert_eq!(&buf[..size], expected);
        assert_eq!(ControlMessage::from_bytes(&expected), Ok(message));
    }
}

#[test]
fn invalid_magic_unknown_types_and_oversized_packets_are_rejected() {
    assert_eq!(
        ControlMessage::from_bytes(&[0, 0, 5]),
        Err(ProtocolError::InvalidMagic)
    );
    for kind in [0, 1, 8, 255] {
        assert_eq!(
            ControlMessage::from_bytes(&[0x41, 0x4A, kind]),
            Err(ProtocolError::UnknownMessageType(kind))
        );
    }
    for size in [MAX_PACKET_SIZE, MAX_PACKET_SIZE + 1] {
        assert_eq!(
            ControlMessage::from_bytes(&vec![0; size]),
            Err(ProtocolError::PacketTooLarge)
        );
    }
    // A large output capacity is valid: only the encoded datagram is bounded.
    assert!(
        ControlMessage::Leave { peer_id: 1 }
            .to_bytes(&mut [0; 1600])
            .is_ok()
    );
    assert_eq!(PROTOCOL_MAGIC.to_le_bytes(), [0x41, 0x4A]);
}

#[test]
fn invalid_messages_are_rejected_without_writing_output() {
    let mut invalid = vec![
        (
            ControlMessage::JoinRequest {
                nickname: [0; 32],
                nickname_len: 33,
            },
            ProtocolError::InvalidNicknameLength(33),
        ),
        (
            ControlMessage::PeerListUpdate {
                peers: vec![peer(1); MAX_PEERS + 1],
            },
            ProtocolError::TooManyPeers,
        ),
        (
            ControlMessage::PeerListUpdate {
                peers: vec![peer(1), peer(1)],
            },
            ProtocolError::DuplicatePeerId(1),
        ),
    ];
    let mut bad_name = peer(1);
    bad_name.nickname_len = 255;
    invalid.push((
        ControlMessage::PeerListUpdate {
            peers: vec![bad_name],
        },
        ProtocolError::InvalidNicknameLength(255),
    ));
    for id in [0, 9, 255] {
        invalid.push((
            ControlMessage::Leave { peer_id: id },
            ProtocolError::InvalidPeerId(id),
        ));
        invalid.push((
            ControlMessage::JoinAccepted {
                assigned_peer_id: id,
                sample_rate: SAMPLE_RATE,
            },
            ProtocolError::InvalidPeerId(id),
        ));
        invalid.push((
            ControlMessage::PeerListUpdate {
                peers: vec![peer(id)],
            },
            ProtocolError::InvalidPeerId(id),
        ));
    }
    for (message, error) in invalid {
        let mut output = [0xCC; MAX_PACKET_SIZE];
        assert_eq!(message.to_bytes(&mut output), Err(error));
        assert_eq!(output, [0xCC; MAX_PACKET_SIZE]);
    }
}

#[test]
fn malformed_wire_fields_are_rejected() {
    let mut buf = [0; MAX_PACKET_SIZE];
    let size = ControlMessage::JoinRequest {
        nickname: [0; 32],
        nickname_len: 0,
    }
    .to_bytes(&mut buf)
    .unwrap();
    buf[35] = 33;
    assert_eq!(
        ControlMessage::from_bytes(&buf[..size]),
        Err(ProtocolError::InvalidNicknameLength(33))
    );
    assert_eq!(
        ControlMessage::from_bytes(&[0x41, 0x4A, 4, 9]),
        Err(ProtocolError::TooManyPeers)
    );
    assert_eq!(
        ControlMessage::from_bytes(&[0x41, 0x4A, 4, 8]),
        Err(ProtocolError::TruncatedPacket)
    );
    for kind in [3, 7] {
        for id in [0, 9, 255] {
            let packet = [0x41, 0x4A, kind, id, 0, 0, 0, 0];
            let size = if kind == 3 { 8 } else { 4 };
            assert_eq!(
                ControlMessage::from_bytes(&packet[..size]),
                Err(ProtocolError::InvalidPeerId(id))
            );
        }
    }
    let size = ControlMessage::PeerListUpdate {
        peers: vec![peer(1), peer(2)],
    }
    .to_bytes(&mut buf)
    .unwrap();
    buf[38] = 1;
    assert_eq!(
        ControlMessage::from_bytes(&buf[..size]),
        Err(ProtocolError::DuplicatePeerId(1))
    );
    buf[38] = 0;
    assert_eq!(
        ControlMessage::from_bytes(&buf[..size]),
        Err(ProtocolError::InvalidPeerId(0))
    );
    buf[38] = 2;
    buf[71] = 33;
    assert_eq!(
        ControlMessage::from_bytes(&buf[..size]),
        Err(ProtocolError::InvalidNicknameLength(33))
    );
}
