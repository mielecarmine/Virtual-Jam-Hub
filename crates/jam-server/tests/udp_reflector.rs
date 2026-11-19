use std::io;
use std::net::UdpSocket;
use std::time::Duration;

use jam_protocol::{
    AudioHeader, ControlMessage, MAX_PACKET_SIZE, NICKNAME_CAPACITY, STREAM_PRIMARY,
    STREAM_TALKBACK,
};
use jam_server::Server;

struct Room {
    server: Server,
    clients: Vec<UdpSocket>,
}

impl Room {
    fn new(count: usize) -> Self {
        let mut server = Server::bind("127.0.0.1:0").unwrap();
        let mut clients = Vec::new();
        for index in 0..count {
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            socket.connect(server.local_addr().unwrap()).unwrap();
            socket.set_nonblocking(true).unwrap();
            send_control(
                &socket,
                &ControlMessage::JoinRequest {
                    nickname: [0; NICKNAME_CAPACITY],
                    nickname_len: 0,
                },
            );
            server.poll().unwrap();
            assert!(matches!(
                receive_control(&socket),
                ControlMessage::JoinAccepted { assigned_peer_id, .. }
                    if usize::from(assigned_peer_id) == index + 1
            ));
            clients.push(socket);
            for client in &clients {
                assert!(matches!(
                    receive_control(client),
                    ControlMessage::PeerListUpdate { peers } if peers.len() == index + 1
                ));
            }
            assert!(matches!(
                receive_control(clients.last().unwrap()),
                ControlMessage::MetronomeState { .. }
            ));
        }
        Self { server, clients }
    }

    fn assert_silent(&self) {
        for client in &self.clients {
            assert_silent(client);
        }
    }

    fn reflect(&mut self, sender: usize, packet: &[u8]) -> Duration {
        assert_eq!(self.clients[sender].send(packet).unwrap(), packet.len());
        self.server.poll().unwrap();
        let duration = self.server.last_reflection_duration().unwrap();
        let mut buffer = [0; MAX_PACKET_SIZE];
        for (index, client) in self.clients.iter().enumerate() {
            if index != sender {
                let length = receive(client, &mut buffer);
                assert_eq!(&buffer[..length], packet);
            }
            assert_silent(client);
        }
        duration
    }
}

// Sending on loopback does not make the packet immediately readable on every OS.
fn receive(socket: &UdpSocket, buffer: &mut [u8]) -> usize {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let length = socket.recv(buffer).unwrap();
    socket.set_nonblocking(true).unwrap();
    length
}

fn assert_silent(socket: &UdpSocket) {
    let error = socket.recv(&mut [0; MAX_PACKET_SIZE]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
}

fn send_control(socket: &UdpSocket, message: &ControlMessage) {
    let mut buffer = [0; MAX_PACKET_SIZE];
    let length = message.to_bytes(&mut buffer).unwrap();
    assert_eq!(socket.send(&buffer[..length]).unwrap(), length);
}

fn receive_control(socket: &UdpSocket) -> ControlMessage {
    let mut buffer = [0; MAX_PACKET_SIZE];
    let length = receive(socket, &mut buffer);
    ControlMessage::from_bytes(&buffer[..length]).unwrap()
}

fn audio(peer_id: u8, sequence: u32, length: usize) -> Vec<u8> {
    let mut packet = vec![0xa5; length];
    AudioHeader::new(
        peer_id,
        STREAM_PRIMARY,
        sequence,
        u16::try_from(sequence.wrapping_mul(120) & 0xffff).unwrap(),
    )
    .to_bytes(&mut packet)
    .unwrap();
    packet
}

#[test]
fn five_peers_receive_exact_audio_bytes_without_echo_or_duplicates() {
    let mut room = Room::new(5);
    for sender in 0..5 {
        for length in [12, 120, MAX_PACKET_SIZE - 1] {
            for sequence in [u32::MAX, 0, 1] {
                let packet = audio(u8::try_from(sender + 1).unwrap(), sequence, length);
                room.reflect(sender, &packet);
            }
        }
    }
}

#[test]
fn reflects_independent_streams_from_the_same_peer_unchanged() {
    let mut room = Room::new(3);
    for stream_id in [STREAM_PRIMARY, STREAM_TALKBACK] {
        let mut packet = audio(1, 42, 120);
        let mut header = AudioHeader::from_bytes(&packet).unwrap();
        header.stream_id = stream_id;
        header.to_bytes(&mut packet).unwrap();
        room.reflect(0, &packet);
    }
}

#[test]
fn rejects_unknown_spoofed_malformed_and_oversized_audio() {
    let mut room = Room::new(2);
    let unknown = UdpSocket::bind("127.0.0.1:0").unwrap();
    unknown.connect(room.server.local_addr().unwrap()).unwrap();
    unknown.set_nonblocking(true).unwrap();
    unknown.send(&audio(1, 0, 120)).unwrap();
    room.server.poll().unwrap();
    assert!(room.server.last_reflection_duration().is_none());
    room.assert_silent();
    assert_silent(&unknown);

    let valid = audio(1, 0, 120);
    let mut bad_magic = valid.clone();
    bad_magic[0] ^= 0xff;
    let mut bad_type = valid.clone();
    bad_type[2] = 0xff;
    let mut bad_id = valid.clone();
    bad_id[3] = 0;
    let mut oversized = vec![0; MAX_PACKET_SIZE + 100];
    oversized[..valid.len()].copy_from_slice(&valid);
    for packet in [
        audio(2, 0, 120),
        bad_magic,
        bad_type,
        bad_id,
        valid[..11].to_vec(),
        oversized[..MAX_PACKET_SIZE].to_vec(),
        oversized,
    ] {
        room.clients[0].send(&packet).unwrap();
        room.server.poll().unwrap();
        assert!(room.server.last_reflection_duration().is_none());
        room.assert_silent();
    }
    room.reflect(0, &valid);
}

#[test]
fn departed_peer_neither_sends_nor_receives_audio() {
    let mut room = Room::new(3);
    send_control(&room.clients[1], &ControlMessage::Leave { peer_id: 2 });
    room.server.poll().unwrap();
    for index in [0, 2] {
        assert!(matches!(
            receive_control(&room.clients[index]),
            ControlMessage::PeerListUpdate { peers } if peers.len() == 2
        ));
    }
    room.clients[1].send(&audio(2, 0, 120)).unwrap();
    room.server.poll().unwrap();
    room.assert_silent();
    let packet = audio(1, 1, 120);
    room.clients[0].send(&packet).unwrap();
    room.server.poll().unwrap();
    let mut buffer = [0; MAX_PACKET_SIZE];
    let length = receive(&room.clients[2], &mut buffer);
    assert_eq!(&buffer[..length], packet);
    room.assert_silent();
}

#[test]
fn ping_replies_only_to_sender_with_receive_timestamp() {
    let mut room = Room::new(2);
    let mut previous = 0;
    for client_timestamp_us in [42, u64::MAX] {
        send_control(
            &room.clients[0],
            &ControlMessage::Ping {
                client_timestamp_us,
            },
        );
        room.server.poll().unwrap();
        let ControlMessage::Pong {
            client_timestamp_us: echoed,
            server_timestamp_us,
        } = receive_control(&room.clients[0])
        else {
            panic!("Expected Pong");
        };
        assert_eq!(echoed, client_timestamp_us);
        assert!(server_timestamp_us > 0);
        assert!(server_timestamp_us >= previous);
        previous = server_timestamp_us;
        assert!(room.server.last_reflection_duration().is_none());
        room.assert_silent();
    }
}

#[test]
#[ignore = "timing gate: run explicitly in release mode on an idle machine"]
fn five_peer_forwarding_stays_below_200_microseconds() {
    let mut room = Room::new(5);
    let packets: Vec<_> = (1..=5).map(|id| audio(id, 0, 120)).collect();
    for (sender, packet) in packets.iter().enumerate() {
        room.reflect(sender, packet);
    }
    let mut maximum = Duration::ZERO;
    for _ in 0..100 {
        for (sender, packet) in packets.iter().enumerate() {
            maximum = maximum.max(room.reflect(sender, packet));
        }
    }
    eprintln!("5 peers, 500 frames, recv_from to final send_to: max {maximum:?}");
    assert!(maximum < Duration::from_micros(200), "max: {maximum:?}");
}
