use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use jam_protocol::{ControlMessage, MAX_PACKET_SIZE, NICKNAME_CAPACITY, PeerInfo, SAMPLE_RATE};
use jam_server::Server;

struct RunningServer {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RunningServer {
    fn start() -> Self {
        let mut server = Server::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                server.poll().unwrap();
            }
        });
        Self {
            address,
            stop,
            thread: Some(thread),
        }
    }

    fn client(&self) -> UdpSocket {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.connect(self.address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        socket
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn send(socket: &UdpSocket, message: &ControlMessage) {
    let mut buffer = [0; MAX_PACKET_SIZE];
    let length = message.to_bytes(&mut buffer).unwrap();
    assert_eq!(socket.send(&buffer[..length]).unwrap(), length);
}

fn receive_until(
    socket: &UdpSocket,
    deadline: Instant,
    predicate: impl Fn(&ControlMessage) -> bool,
) -> ControlMessage {
    let mut buffer = [0; MAX_PACKET_SIZE];
    while Instant::now() < deadline {
        match socket.recv(&mut buffer) {
            Ok(length) => {
                let message = ControlMessage::from_bytes(&buffer[..length]).unwrap();
                if predicate(&message) {
                    return message;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("UDP receive failed: {error}"),
        }
    }
    panic!("Expected server notification before deadline");
}

fn join(socket: &UdpSocket, name: &str) -> u8 {
    let mut nickname = [0; NICKNAME_CAPACITY];
    nickname[..name.len()].copy_from_slice(name.as_bytes());
    send(
        socket,
        &ControlMessage::JoinRequest {
            local_addr: None,
            nickname,
            nickname_len: u8::try_from(name.len()).unwrap(),
        },
    );
    let message = receive_until(socket, Instant::now() + Duration::from_secs(1), |message| {
        matches!(message, ControlMessage::JoinAccepted { .. })
    });
    let ControlMessage::JoinAccepted {
        assigned_peer_id,
        sample_rate,
    } = message
    else {
        unreachable!()
    };
    assert_eq!(sample_rate, SAMPLE_RATE);
    assigned_peer_id
}

fn wait_for_peers(socket: &UdpSocket, count: usize, deadline: Instant) -> Vec<PeerInfo> {
    let message = receive_until(
        socket,
        deadline,
        |message| matches!(message, ControlMessage::PeerListUpdate { peers } if peers.len() == count),
    );
    let ControlMessage::PeerListUpdate { peers } = message else {
        unreachable!()
    };
    peers
}

#[test]
fn joins_broadcast_unique_ids_and_leave_reuses_the_first_free_id() {
    let server = RunningServer::start();
    let alice = server.client();
    let bob = server.client();
    assert_eq!(join(&alice, "Alice"), 1);
    assert_eq!(join(&bob, "Bob"), 2);
    let deadline = Instant::now() + Duration::from_secs(1);
    let peers = wait_for_peers(&alice, 2, deadline);
    assert_eq!(peers, wait_for_peers(&bob, 2, deadline));
    assert_eq!(
        peers.iter().map(|peer| peer.peer_id).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(&peers[0].nickname[..5], b"Alice");
    assert_eq!(&peers[1].nickname[..3], b"Bob");

    // Duplicate joins keep the same ID and publish the updated nickname.
    assert_eq!(join(&alice, "Alice2"), 1);
    let peers = wait_for_peers(&bob, 2, Instant::now() + Duration::from_secs(1));
    assert_eq!(&peers[0].nickname[..6], b"Alice2");
    send(&alice, &ControlMessage::Leave { peer_id: 1 });
    let peers = wait_for_peers(&bob, 1, Instant::now() + Duration::from_secs(1));
    assert_eq!(peers[0].peer_id, 2);
    let carol = server.client();
    assert_eq!(join(&carol, "Carol"), 1);
    assert_eq!(
        wait_for_peers(&bob, 2, Instant::now() + Duration::from_secs(1))[0].peer_id,
        1
    );
    // Finish queued join notifications before closing destinations: Winsock can
    // otherwise report ICMP port-unreachable as a receive-side connection reset.
    drop(server);
}

#[test]
fn idle_client_is_removed_within_five_and_a_half_seconds() {
    let server = RunningServer::start();
    let active = server.client();
    let idle = server.client();
    let active_id = join(&active, "Active");
    join(&idle, "Idle");
    let joined = Instant::now();
    wait_for_peers(&active, 2, joined + Duration::from_secs(1));
    wait_for_peers(&idle, 2, joined + Duration::from_secs(1));
    let deadline = joined + Duration::from_millis(5500);
    let mut buffer = [0; MAX_PACKET_SIZE];
    while Instant::now() < deadline {
        send(
            &active,
            &ControlMessage::Ping {
                client_timestamp_us: 42,
            },
        );
        match active.recv(&mut buffer) {
            Ok(length) => match ControlMessage::from_bytes(&buffer[..length]).unwrap() {
                ControlMessage::PeerListUpdate { peers } if peers.len() == 1 => {
                    assert_eq!(peers[0].peer_id, active_id);
                    assert!(joined.elapsed() <= Duration::from_millis(5500));
                    drop(server);
                    return;
                }
                ControlMessage::Pong {
                    client_timestamp_us,
                    ..
                } => assert_eq!(client_timestamp_us, 42),
                ControlMessage::MetronomeState { .. } => {}
                message => panic!("Unexpected message: {message:?}"),
            },
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("UDP receive failed: {error}"),
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("Idle peer was not removed within 5.5 seconds");
}

#[test]
fn receive_timeout_keeps_poll_bounded_without_traffic() {
    let mut server = Server::bind("127.0.0.1:0").unwrap();
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client.connect(server.local_addr().unwrap()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    send(
        &client,
        &ControlMessage::JoinRequest {
            local_addr: None,
            nickname: [0; NICKNAME_CAPACITY],
            nickname_len: 0,
        },
    );
    server.poll().unwrap();
    let mut buffer = [0; MAX_PACKET_SIZE];
    client.recv(&mut buffer).unwrap(); // Accepted.
    client.recv(&mut buffer).unwrap(); // Initial peer list.
    client.recv(&mut buffer).unwrap(); // Initial metronome state.
    let now = Instant::now();
    server.poll().unwrap(); // No traffic: returns after the bounded receive timeout.
    assert!(now.elapsed() < Duration::from_millis(500));
}
