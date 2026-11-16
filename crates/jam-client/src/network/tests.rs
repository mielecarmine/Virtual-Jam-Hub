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
    let (mut speaker, _mixer) = crate::mixer::channels();
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
fn rejects_malformed_echo_non_primary_and_nonfinite_atomically() {
    let good = packet(2, -0.25);
    assert!(receive_audio(&good[..PACKET_SIZE - 1], 1).is_none());
    assert!(receive_audio(&packet(1, 0.5), 1).is_none());
    assert!(receive_audio(&packet(2, f32::NAN), 1).is_none());
    assert!(receive_audio(&packet(2, f32::INFINITY), 1).is_none());
    let mut bad = good;
    bad[10] = 1;
    assert!(receive_audio(&bad, 1).is_none());
    bad = good;
    bad[0] = 0;
    assert!(receive_audio(&bad, 1).is_none());
    assert_eq!(receive_audio(&good, 1).unwrap().1, [-0.25; 128]);
}

#[test]
fn rejects_wrong_sample_rate_and_ignores_foreign_endpoint() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let foreign = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "peer").unwrap();
    let destination = network.socket.local_addr().unwrap();
    let (_, mut mic) = RingBuffer::new(4096);
    let (mut speaker, _mixer) = crate::mixer::channels();
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
        let (mut a_speaker, mut a_mixer) = crate::mixer::channels();
        let (mut b_input, mut b_mic) = RingBuffer::new(4096);
        let (mut b_speaker, mut b_mixer) = crate::mixer::channels();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while a.peer_id.is_none() || b.peer_id.is_none() {
                assert!(Instant::now() < deadline, "join timeout");
                a.tick(&mut a_mic, &mut a_speaker, Instant::now()).unwrap();
                b.tick(&mut b_mic, &mut b_speaker, Instant::now()).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
            assert_ne!(a.peer_id, b.peer_id);
            let a_output = &mut a_mixer.rings[usize::from(b.peer_id.unwrap()) - 1];
            let b_output = &mut b_mixer.rings[usize::from(a.peer_id.unwrap()) - 1];
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
                assert_eq!(a_output.pop().unwrap().value, -0.5);
                assert_eq!(b_output.pop().unwrap().value, 0.25);
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
    let (speaker, _mixer) = crate::mixer::channels();
    network.run(mic, speaker, &AtomicBool::new(true)).unwrap();
    assert_eq!(
        ControlMessage::from_bytes(&receive(&server)).unwrap(),
        ControlMessage::Leave { peer_id: 1 }
    );
}

#[test]
fn simulated_loss_and_jitter_keep_playout_paced_and_bounded() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "test")
        .unwrap()
        .configure(Settings {
            target_chunks: 3,
            loss: 0.02,
            jitter_ms: 10,
        });
    network.random = 12345;
    let (mut speaker, mut mixer) = crate::mixer::channels_with_capacity(4096);
    let output = &mut mixer.rings[1];
    let start = network.started;
    let mut emitted = 0;
    let mut strong = 0;
    let mut next_packet = start;
    let mut sequence = 0;
    let mut started = false;
    for tick in 0..20_000 {
        let now = start + Duration::from_micros(tick * 250);
        if now >= next_packet {
            let mut bytes = packet(2, 0.5);
            AudioHeader::new(2, STREAM_PRIMARY, sequence, 0)
                .to_bytes(&mut bytes)
                .unwrap();
            network.incoming(&bytes, 1, now);
            next_packet += CHUNK_TIME;
            sequence += 1;
        }
        network.playback(&mut speaker, now);
        if output.slots() > 0 {
            assert_eq!(
                output.slots(),
                BUFFER_FRAMES,
                "no catch-up bursts or partial chunks"
            );
            started = true;
            emitted += 1;
            for _ in 0..BUFFER_FRAMES {
                let sample = output.pop().unwrap().value;
                assert!(sample.is_finite() && (0.0..=0.5).contains(&sample));
                if sample >= 0.32 {
                    strong += 1;
                }
            }
        }
        if started {
            assert!(network.next_playout.unwrap() > now);
        }
        assert!(network.delayed.len() <= 512);
        assert_eq!(
            network.peers.len(),
            usize::from(started || !network.peers.is_empty())
        );
    }
    assert!(emitted > 1800, "continuous cadence after initial prefill");
    assert!(
        strong > emitted * BUFFER_FRAMES * 95 / 100,
        "signal survives impairment"
    );
}

#[test]
fn independent_peers_are_paced_together_and_expire_without_unbounded_silence() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "test").unwrap();
    let (mut speaker, mut mixer) = crate::mixer::channels_with_capacity(128);
    let [_, output, other, ..] = &mut mixer.rings;
    let start = network.started;
    network.incoming(&packet(2, 0.25), 1, start);
    network.incoming(&packet(3, -0.5), 1, start);
    network.playback(&mut speaker, start);
    assert_eq!(network.peers.len(), 2);
    network.playback(&mut speaker, start + CHUNK_TIME * 3);
    for _ in 0..128 {
        assert_eq!(output.pop().unwrap().value, 0.25);
        assert_eq!(other.pop().unwrap().value, -0.5);
    }
    network.playback(&mut speaker, start + SERVER_TIMEOUT);
    assert!(network.peers.is_empty());
    assert!(network.next_playout.is_none());
    assert!(output.pop().is_err());
}

#[test]
fn full_speaker_discards_whole_frame_and_stall_does_not_burst() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "test").unwrap();
    let (mut speaker, mut mixer) = crate::mixer::channels_with_capacity(128);
    let output = &mut mixer.rings[1];
    let start = network.started;
    network.incoming(&packet(2, 0.25), 1, start);
    network.playback(&mut speaker, start);
    speaker.push(2, &[0.75]);
    network.playback(&mut speaker, start + CHUNK_TIME * 3);
    assert_eq!(output.slots(), 1);
    assert_eq!(output.pop().unwrap().value, 0.75);
    let stalled = start + CHUNK_TIME * 30;
    network.playback(&mut speaker, stalled);
    assert_eq!(output.slots(), BUFFER_FRAMES);
    network.playback(&mut speaker, stalled);
    assert_eq!(output.slots(), BUFFER_FRAMES);
}

#[test]
fn simulator_drops_all_at_one_and_preserves_arrival_order_when_disabled() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "test").unwrap();
    let (mut speaker, mut mixer) = crate::mixer::channels_with_capacity(4096);
    let output = &mut mixer.rings[1];
    let start = network.started;
    network.settings.loss = 1.0;
    network.incoming(&packet(2, 0.25), 1, start);
    assert!(network.delayed.is_empty());
    network.settings.loss = 0.0;
    for sequence in 0..3 {
        let mut bytes = packet(2, 0.25);
        AudioHeader::new(2, STREAM_PRIMARY, sequence, 0)
            .to_bytes(&mut bytes)
            .unwrap();
        network.incoming(&bytes, 1, start + CHUNK_TIME * sequence);
        network.playback(&mut speaker, start + CHUNK_TIME * sequence);
    }
    assert!(output.pop().is_err());
    for tick in 3..6 {
        network.playback(&mut speaker, start + CHUNK_TIME * tick);
        for _ in 0..BUFFER_FRAMES {
            assert_eq!(output.pop().unwrap().value, 0.25);
        }
    }
}

#[test]
fn four_remote_peers_reach_the_hardware_mixer_through_the_real_sfu() {
    let mut server = jam_server::Server::bind("127.0.0.1:0").unwrap();
    let address = server.local_addr().unwrap();
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let worker = scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                server.poll().unwrap();
            }
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut clients: Vec<_> = (0..5)
                .map(|_| {
                    let network = Network::connect(address, "musician").unwrap();
                    let (input, mic) = RingBuffer::new(4096);
                    let (rx, mixer) = crate::mixer::channels();
                    (network, input, mic, rx, mixer)
                })
                .collect();
            let deadline = Instant::now() + Duration::from_secs(3);
            while clients.iter().any(|(net, ..)| net.peer_id.is_none()) {
                assert!(Instant::now() < deadline, "join timeout");
                for (net, _, mic, rx, _) in &mut clients {
                    net.tick(mic, rx, Instant::now()).unwrap();
                }
                thread::sleep(Duration::from_millis(1));
            }
            let remote_ids: Vec<_> = clients[..4]
                .iter()
                .map(|(net, ..)| usize::from(net.peer_id.unwrap()) - 1)
                .collect();
            for (_, input, ..) in &mut clients[..4] {
                for _ in 0..128 {
                    input.push(0.8).unwrap();
                }
            }
            loop {
                assert!(Instant::now() < deadline, "audio timeout");
                for (net, _, mic, rx, _) in &mut clients {
                    net.tick(mic, rx, Instant::now()).unwrap();
                }
                if remote_ids
                    .iter()
                    .all(|&id| clients[4].4.rings[id].slots() >= 128)
                {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            let (_, _, _, _, mixer) = &mut clients[4];
            let mut output = [0.0; 256];
            mixer.playback(&mut output, 2, &crate::audio::Stats::default());
            assert!(
                output
                    .iter()
                    .all(|sample| (*sample - 3.2_f32.tanh()).abs() < 1e-6)
            );
        }));
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    });
}
