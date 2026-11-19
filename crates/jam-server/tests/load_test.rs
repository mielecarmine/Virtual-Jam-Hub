use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use jam_protocol::{
    AUDIO_HEADER_SIZE, AudioHeader, ControlMessage, MAX_PACKET_SIZE, NICKNAME_CAPACITY,
    STREAM_PRIMARY,
};
use jam_server::Server;

const BOT_COUNT: usize = 5;
const PACKET_BYTES: usize = 120;
const FRAME_INTERVAL: Duration = Duration::from_micros(2_500);
const SIMULATION_DURATION: Duration = Duration::from_secs(10);
const FRAMES_PER_BOT: u32 = 4_000;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const DRAIN_DURATION: Duration = Duration::from_millis(300);

#[derive(Debug)]
struct BotReport {
    peer_id: u8,
    sent: u32,
    received: [u32; BOT_COUNT],
}

fn join_room(socket: &UdpSocket) -> u8 {
    socket.set_read_timeout(Some(STARTUP_TIMEOUT)).unwrap();
    let message = ControlMessage::JoinRequest {
        nickname: [0; NICKNAME_CAPACITY],
        nickname_len: 0,
    };
    let mut buffer = [0; MAX_PACKET_SIZE];
    let length = message.to_bytes(&mut buffer).unwrap();
    assert_eq!(socket.send(&buffer[..length]).unwrap(), length);

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut peer_id = None;
    let mut full_room = false;
    while peer_id.is_none() || !full_room {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for five joined bots"
        );
        socket.set_read_timeout(Some(remaining)).unwrap();
        let length = socket.recv(&mut buffer).unwrap();
        match ControlMessage::from_bytes(&buffer[..length]).unwrap() {
            ControlMessage::JoinAccepted {
                assigned_peer_id, ..
            } => peer_id = Some(assigned_peer_id),
            ControlMessage::PeerListUpdate { peers } => full_room = peers.len() == BOT_COUNT,
            ControlMessage::MetronomeState { .. } => {}
            message => panic!("unexpected startup message: {message:?}"),
        }
    }
    socket.set_nonblocking(true).unwrap();
    peer_id.unwrap()
}

fn receive_audio(
    socket: &UdpSocket,
    report: &mut BotReport,
    last_sequence: &mut [Option<u32>; BOT_COUNT],
) {
    let mut buffer = [0; MAX_PACKET_SIZE];
    // Bound each drain so receiving cannot starve the next scheduled send.
    for _ in 0..128 {
        let length = match socket.recv(&mut buffer) {
            Ok(length) => length,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("bot {} receive failed: {error}", report.peer_id),
        };
        // The initial room state can follow the final roster datagram.
        if matches!(
            ControlMessage::from_bytes(&buffer[..length]),
            Ok(ControlMessage::MetronomeState { .. })
        ) {
            continue;
        }
        let header = AudioHeader::from_bytes(&buffer[..length]).unwrap();
        assert_eq!(length, PACKET_BYTES);
        assert_ne!(header.peer_id, report.peer_id, "server echoed local audio");
        let sender = usize::from(header.peer_id) - 1;
        assert!(sender < BOT_COUNT, "unexpected peer {}", header.peer_id);
        assert!(header.sequence_number < FRAMES_PER_BOT);
        assert_eq!(header.stream_id, STREAM_PRIMARY);
        assert_eq!(header.reserved, 0);
        assert_eq!(
            header.timestamp_samples,
            u16::try_from((header.sequence_number * 120) & 0xffff).unwrap()
        );
        assert!(
            buffer[AUDIO_HEADER_SIZE..length]
                .iter()
                .all(|&byte| byte == header.peer_id)
        );
        if let Some(previous) = last_sequence[sender] {
            assert!(
                header.sequence_number > previous,
                "bot {}: peer {} sequence {} after {previous} (duplicate or reorder)",
                report.peer_id,
                header.peer_id,
                header.sequence_number
            );
        }
        last_sequence[sender] = Some(header.sequence_number);
        report.received[sender] += 1;
    }
}

fn run_bot(
    address: SocketAddr,
    ready: &mpsc::Sender<()>,
    start: &mpsc::Receiver<Instant>,
) -> BotReport {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.connect(address).unwrap();
    let peer_id = join_room(&socket);
    ready.send(()).unwrap();
    let started = start.recv_timeout(STARTUP_TIMEOUT).unwrap();
    let finish = started + SIMULATION_DURATION;
    let mut report = BotReport {
        peer_id,
        sent: 0,
        received: [0; BOT_COUNT],
    };
    let mut last_sequence = [None; BOT_COUNT];
    let mut packet = [peer_id; PACKET_BYTES];
    let mut next_send = started;
    while Instant::now() < finish {
        if report.sent < FRAMES_PER_BOT && Instant::now() >= next_send {
            AudioHeader::new(
                peer_id,
                STREAM_PRIMARY,
                report.sent,
                u16::try_from((report.sent * 120) & 0xffff).unwrap(),
            )
            .to_bytes(&mut packet)
            .unwrap();
            assert_eq!(socket.send(&packet).unwrap(), PACKET_BYTES);
            report.sent += 1;
            // Absolute deadlines avoid accumulating sleep and processing drift.
            next_send = started + FRAME_INTERVAL * report.sent;
        }
        receive_audio(&socket, &mut report, &mut last_sequence);
        let wait = next_send
            .saturating_duration_since(Instant::now())
            .min(Duration::from_micros(100));
        if !wait.is_zero() {
            thread::sleep(wait);
        }
    }
    while Instant::now() < finish + DRAIN_DURATION {
        receive_audio(&socket, &mut report, &mut last_sequence);
        thread::sleep(Duration::from_micros(100));
    }
    report
}

#[test]
fn five_bots_stream_for_ten_seconds_with_ordered_delivery_above_99_9_percent() {
    let mut server = Server::bind("127.0.0.1:0").unwrap();
    let address = server.local_addr().unwrap();
    let stop = AtomicBool::new(false);
    let reports = thread::scope(|scope| {
        let server_thread = scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                server.poll().unwrap();
            }
        });
        let (ready_tx, ready_rx) = mpsc::channel();
        let mut starts = Vec::new();
        let mut bots = Vec::new();
        for _ in 0..BOT_COUNT {
            let (start_tx, start_rx) = mpsc::channel();
            starts.push(start_tx);
            let ready = ready_tx.clone();
            bots.push(scope.spawn(move || run_bot(address, &ready, &start_rx)));
        }
        drop(ready_tx);
        let all_ready = (0..BOT_COUNT).all(|_| ready_rx.recv_timeout(STARTUP_TIMEOUT).is_ok());
        if all_ready {
            let started = Instant::now() + Duration::from_millis(50);
            for start in &starts {
                // A failed bot must not prevent the server from being stopped and joined.
                let _ = start.send(started);
            }
        }
        drop(starts);
        let results: Vec<_> = bots
            .into_iter()
            .map(thread::ScopedJoinHandle::join)
            .collect();
        stop.store(true, Ordering::Relaxed);
        server_thread.join().unwrap();
        assert!(all_ready, "not all bots completed the UDP join handshake");
        results.into_iter().map(Result::unwrap).collect::<Vec<_>>()
    });

    let mut peer_ids: Vec<_> = reports.iter().map(|report| report.peer_id).collect();
    peer_ids.sort_unstable();
    assert_eq!(peer_ids, [1, 2, 3, 4, 5]);
    for report in &reports {
        assert_eq!(report.sent, FRAMES_PER_BOT, "{report:?}");
        let expected: u32 = reports
            .iter()
            .filter(|sender| sender.peer_id != report.peer_id)
            .map(|sender| sender.sent)
            .sum();
        let received: u32 = report.received.iter().sum();
        eprintln!(
            "bot {}: sent {}, received {received}/{expected}; per peer: {:?}",
            report.peer_id, report.sent, report.received
        );
        // Integer arithmetic keeps the inclusive 99.9% threshold exact.
        assert!(
            u64::from(received) * 1_000 >= u64::from(expected) * 999,
            "bot {} received fewer than 99.9% of forwarded packets: {received}/{expected}",
            report.peer_id
        );
    }
}
