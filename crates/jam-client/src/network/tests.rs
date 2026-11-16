use super::*;
use rtrb::RingBuffer;

fn packet(id: u8, value: f32) -> [u8; PACKET_SIZE] {
    let mut bytes = [0; PACKET_SIZE];
    AudioHeader::new(id, STREAM_PRIMARY, 0, 0)
        .to_bytes(&mut bytes)
        .unwrap();
    for sample in bytes[AUDIO_HEADER_SIZE..].chunks_exact_mut(4) {
        sample.copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn receive(socket: &UdpSocket) -> Vec<u8> {
    let mut bytes = [0; MAX_PACKET_SIZE];
    let len = socket.recv(&mut bytes).unwrap();
    bytes[..len].to_vec()
}

fn send_control(socket: &UdpSocket, destination: SocketAddr, message: &ControlMessage) {
    let mut bytes = [0; MAX_PACKET_SIZE];
    let len = message.to_bytes(&mut bytes).unwrap();
    socket.send_to(&bytes[..len], destination).unwrap();
}

#[test]
fn retries_join_sends_heartbeat_frames_and_wraps_counters() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "guitar").unwrap();
    let destination = network.socket.local_addr().unwrap();
    let (mut input, mut mic) = RingBuffer::new(4096);
    let (mut speaker, _) = RingBuffer::new(4096);
    let start = network.started;
    network.tick(&mut mic, &mut speaker, start).unwrap();
    assert!(matches!(
        ControlMessage::from_bytes(&receive(&server)),
        Ok(ControlMessage::JoinRequest {
            nickname_len: 6,
            ..
        })
    ));
    network
        .tick(&mut mic, &mut speaker, start + JOIN_INTERVAL)
        .unwrap();
    assert!(matches!(
        ControlMessage::from_bytes(&receive(&server)),
        Ok(ControlMessage::JoinRequest { .. })
    ));
    send_control(
        &server,
        destination,
        &ControlMessage::JoinAccepted {
            assigned_peer_id: 3,
            sample_rate: SAMPLE_RATE,
        },
    );
    network
        .tick(&mut mic, &mut speaker, start + JOIN_INTERVAL)
        .unwrap();
    assert_eq!(network.peer_id, Some(3));
    network.sequence = u32::MAX;
    network.timestamp = u16::MAX - 127;
    for _ in 0..127 {
        input.push(0.25).unwrap();
    }
    network
        .tick(&mut mic, &mut speaker, start + JOIN_INTERVAL)
        .unwrap();
    assert_eq!(mic.slots(), 127);
    input.push(-0.5).unwrap();
    network
        .tick(&mut mic, &mut speaker, start + JOIN_INTERVAL)
        .unwrap();
    let bytes = receive(&server);
    assert_eq!(bytes.len(), PACKET_SIZE);
    let header = AudioHeader::from_bytes(&bytes).unwrap();
    assert_eq!(header.peer_id, 3);
    assert_eq!(header.sequence_number, u32::MAX);
    assert_eq!(
        &bytes[AUDIO_HEADER_SIZE..AUDIO_HEADER_SIZE + 4],
        &0.25_f32.to_le_bytes()
    );
    assert_eq!(&bytes[PACKET_SIZE - 4..], &(-0.5_f32).to_le_bytes());
    assert_eq!((network.sequence, network.timestamp), (0, 0));
    network
        .tick(&mut mic, &mut speaker, start + PING_INTERVAL)
        .unwrap();
    assert!(matches!(
        ControlMessage::from_bytes(&receive(&server)),
        Ok(ControlMessage::Ping { .. })
    ));
    assert!(
        network
            .tick(&mut mic, &mut speaker, start + Duration::from_secs(6))
            .is_err()
    );
}

#[test]
fn rejects_malformed_echo_non_primary_nonfinite_and_overflow_atomically() {
    let (mut speaker, mut output) = RingBuffer::new(128);
    let good = packet(2, -0.25);
    receive_audio(&good[..PACKET_SIZE - 1], 1, &mut speaker);
    receive_audio(&packet(1, 0.5), 1, &mut speaker);
    receive_audio(&packet(2, f32::NAN), 1, &mut speaker);
    receive_audio(&packet(2, f32::INFINITY), 1, &mut speaker);
    let mut bad = good;
    bad[10] = 1;
    receive_audio(&bad, 1, &mut speaker);
    bad = good;
    bad[0] = 0;
    receive_audio(&bad, 1, &mut speaker);
    assert!(output.pop().is_err());
    speaker.push(0.75).unwrap();
    receive_audio(&good, 1, &mut speaker);
    assert_eq!(output.pop().unwrap(), 0.75);
    assert!(output.pop().is_err());
    receive_audio(&good, 1, &mut speaker);
    for _ in 0..128 {
        assert_eq!(output.pop().unwrap(), -0.25);
    }
}

#[test]
fn rejects_wrong_sample_rate_and_ignores_foreign_endpoint() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let foreign = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "peer").unwrap();
    let destination = network.socket.local_addr().unwrap();
    let (_, mut mic) = RingBuffer::new(4096);
    let (mut speaker, _) = RingBuffer::new(4096);
    send_control(
        &foreign,
        destination,
        &ControlMessage::JoinAccepted {
            assigned_peer_id: 2,
            sample_rate: SAMPLE_RATE,
        },
    );
    network
        .tick(&mut mic, &mut speaker, Instant::now())
        .unwrap();
    assert!(network.peer_id.is_none());
    send_control(
        &server,
        destination,
        &ControlMessage::JoinAccepted {
            assigned_peer_id: 2,
            sample_rate: 44100,
        },
    );
    assert!(
        network
            .tick(&mut mic, &mut speaker, Instant::now())
            .is_err()
    );
}

#[test]
fn two_clients_exchange_pcm_through_real_sfu_without_echo() {
    let mut server = jam_server::Server::bind("127.0.0.1:0").unwrap();
    let address = server.local_addr().unwrap();
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let server_thread = scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                server.poll().unwrap();
            }
        });
        let mut a = Network::connect(address, "a").unwrap();
        let mut b = Network::connect(address, "b").unwrap();
        let (mut a_input, mut a_mic) = RingBuffer::new(4096);
        let (mut a_speaker, mut a_output) = RingBuffer::new(4096);
        let (mut b_input, mut b_mic) = RingBuffer::new(4096);
        let (mut b_speaker, mut b_output) = RingBuffer::new(4096);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while a.peer_id.is_none() || b.peer_id.is_none() {
                assert!(Instant::now() < deadline, "join timeout");
                a.tick(&mut a_mic, &mut a_speaker, Instant::now()).unwrap();
                b.tick(&mut b_mic, &mut b_speaker, Instant::now()).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
            assert_ne!(a.peer_id, b.peer_id);
            for _ in 0..128 {
                a_input.push(0.25).unwrap();
                b_input.push(-0.5).unwrap();
            }
            while a_output.slots() < 128 || b_output.slots() < 128 {
                assert!(Instant::now() < deadline, "audio timeout");
                a.tick(&mut a_mic, &mut a_speaker, Instant::now()).unwrap();
                b.tick(&mut b_mic, &mut b_speaker, Instant::now()).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
            for _ in 0..128 {
                assert_eq!(a_output.pop().unwrap(), -0.5);
                assert_eq!(b_output.pop().unwrap(), 0.25);
            }
            assert!(a_output.pop().is_err());
            assert!(b_output.pop().is_err());
        }));
        stop.store(true, Ordering::Relaxed);
        server_thread.join().unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    });
}

#[test]
fn stopped_worker_sends_leave() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "peer").unwrap();
    network.peer_id = Some(1);
    let (_, mic) = RingBuffer::new(4096);
    let (speaker, _) = RingBuffer::new(4096);
    network.run(mic, speaker, &AtomicBool::new(true)).unwrap();
    assert_eq!(
        ControlMessage::from_bytes(&receive(&server)).unwrap(),
        ControlMessage::Leave { peer_id: 1 }
    );
}
