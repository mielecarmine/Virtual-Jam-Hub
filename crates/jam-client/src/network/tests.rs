use super::*;
use rtrb::RingBuffer;

#[test]
fn latency_plateaus_keep_peer_and_arrival_history_alive_and_restore_audio() {
    for delay in [20, 80, 240] {
        for nominal in [3, 8, 16] {
            let server = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut network = Network::connect(server.local_addr().unwrap(), "test").unwrap();
            network.settings.target_chunks = nominal;
            let start = network.started;
            let (mut speaker, mut mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
            let mut enc = codec::encoder().unwrap();
            // Late tail packets can raise the target after the plateau ends;
            // allow one stable second per adaptive step before checking nominal.
            let mut packets: Vec<_> = (0..6000)
                .map(|sequence| {
                    let mut bytes = [0; PACKET_SIZE];
                    AudioHeader::new(2, STREAM_PRIMARY, sequence, 0)
                        .to_bytes(&mut bytes)
                        .unwrap();
                    bytes[AUDIO_HEADER_SIZE..]
                        .copy_from_slice(&crate::codec::tests::encode(&mut enc, 0.5));
                    let due = sequence
                        + if (400..3200).contains(&sequence) {
                            delay
                        } else {
                            0
                        };
                    (due, sequence, bytes)
                })
                .collect();
            packets.sort_by_key(|&(due, sequence, _)| std::cmp::Reverse((due, sequence)));
            let mut recovered = None;
            for tick in 0..6000 {
                let now = start + CHUNK_TIME * tick;
                while packets.last().is_some_and(|&(due, _, _)| due <= tick) {
                    let (_, _, bytes) = packets.pop().unwrap();
                    network.incoming(&bytes, 1, now);
                }
                network.playback(&mut speaker, now).unwrap();
                let peer = &network.peers[&2];
                let audible = std::iter::from_fn(|| mixer.rings[1].pop().ok())
                    .any(|sample| sample.value.abs() > 0.1);
                while mixer.rings[1].pop().is_ok() {}
                if tick >= 400 + delay && audible {
                    recovered.get_or_insert(tick);
                }
                if tick > 400 + delay + 40 && tick < 3200 {
                    assert!(audible, "delay={delay}, target={nominal}, tick={tick}");
                    assert!(peer.buffered_ms() > 0.0);
                }
                if tick > 3240 {
                    assert!(audible, "playout starved at tick {tick}");
                    assert!(peer.buffered_ms() <= (peer.target_chunks() + 4) as f64 * 2.5);
                }
                if tick == 3199 {
                    // A false five-second retirement would reset these cumulative counts.
                    assert!(network.arrivals[&2].expected > 2800);
                }
            }
            assert!(recovered.unwrap() - (400 + delay) <= 20);
            assert_eq!(network.peers[&2].target_chunks(), nominal);
            network
                .playback(&mut speaker, start + Duration::from_secs(21))
                .unwrap();
            assert!(network.peers.is_empty());
            assert!(network.arrivals.is_empty());
        }
    }
}

#[test]
fn unusable_late_packets_and_duplicates_prevent_false_peer_timeout() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "test").unwrap();
    let start = network.started;
    let (mut speaker, mut mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
    let mut bytes = packet(2, 0.5);
    AudioHeader::new(2, STREAM_PRIMARY, 1000, 0)
        .to_bytes(&mut bytes)
        .unwrap();
    network.incoming(&bytes, 1, start);
    network.playback(&mut speaker, start).unwrap();
    // These cannot trigger recovery: they repeat a sequence more than one second late.
    AudioHeader::new(2, STREAM_PRIMARY, 0, 0)
        .to_bytes(&mut bytes)
        .unwrap();
    for tick in 1..=3200 {
        let now = start + CHUNK_TIME * tick;
        network.incoming(&bytes, 1, now);
        network.playback(&mut speaker, now).unwrap();
        assert_eq!(network.peers[&2].last_received, now);
        assert!(network.arrivals.contains_key(&2));
        while mixer.rings[1].pop().is_ok() {}
    }
    network
        .playback(&mut speaker, start + Duration::from_secs(13))
        .unwrap();
    assert!(network.peers.is_empty());
    assert!(network.arrivals.is_empty());
}

fn packet(id: u8, value: f32) -> [u8; PACKET_SIZE] {
    let mut bytes = [0; PACKET_SIZE];
    AudioHeader::new(id, STREAM_PRIMARY, 0, 0)
        .to_bytes(&mut bytes)
        .unwrap();
    bytes[AUDIO_HEADER_SIZE..].copy_from_slice(&crate::codec::tests::encode(
        &mut codec::encoder().unwrap(),
        value,
    ));
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

// UDP send completion does not guarantee receive readiness, even on loopback.
// Keep the simulated tick timestamp deterministic while waiting for the OS.
fn wait_for_datagram(socket: &UdpSocket) {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match socket.peek(&mut [0; 65_535]) {
            Ok(_) => return,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for test datagram"
                );
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => panic!("cannot wait for test datagram: {error}"),
        }
    }
}

#[test]
fn join_timeout_is_reported_and_disconnect_sends_leave_after_acceptance() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let (_, mut mic) = RingBuffer::new(4096);
    let (mut speaker, _mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
    let mut timed_out = Network::connect(server.local_addr().unwrap(), "guitar").unwrap();
    assert!(!timed_out.is_connected());
    let now = timed_out.started + SERVER_TIMEOUT;
    assert_eq!(
        timed_out
            .tick(&mut mic, &mut speaker, now)
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    let _ = receive(&server); // JoinRequest from the timed-out attempt.
    for _ in 0..3 {
        let mut network = Network::connect(server.local_addr().unwrap(), "guitar").unwrap();
        network
            .tick(&mut mic, &mut speaker, Instant::now())
            .unwrap();
        let _ = receive(&server);
        send_control(
            &server,
            network.socket.local_addr().unwrap(),
            &ControlMessage::JoinAccepted {
                assigned_peer_id: 1,
                sample_rate: SAMPLE_RATE,
            },
        );
        wait_for_datagram(&network.socket);
        network
            .tick(&mut mic, &mut speaker, Instant::now())
            .unwrap();
        assert!(network.is_connected());
        network.disconnect();
        assert!(matches!(
            ControlMessage::from_bytes(&receive(&server)),
            Ok(ControlMessage::Leave { peer_id: 1 })
        ));
    }
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
    let (mut speaker, _mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
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
    wait_for_datagram(&network.socket);
    network
        .tick(&mut mic, &mut speaker, start + JOIN_INTERVAL)
        .unwrap();
    assert_eq!(network.peer_id, Some(3));
    network.sequence = u32::MAX;
    network.timestamp = u16::MAX - 119;
    for _ in 0..119 {
        input.push(0.25).unwrap();
    }
    network
        .tick(&mut mic, &mut speaker, start + JOIN_INTERVAL)
        .unwrap();
    assert_eq!(mic.slots(), 119);
    input.push(-0.5).unwrap();
    network
        .tick(&mut mic, &mut speaker, start + JOIN_INTERVAL)
        .unwrap();
    let bytes = receive(&server);
    assert_eq!(bytes.len(), PACKET_SIZE);
    assert!(codec::valid_payload(&bytes[AUDIO_HEADER_SIZE..]));
    let header = AudioHeader::from_bytes(&bytes).unwrap();
    assert_eq!(header.peer_id, 3);
    assert_eq!(header.sequence_number, u32::MAX);
    let decoded = crate::codec::tests::decode(&bytes[AUDIO_HEADER_SIZE..]);
    assert!(decoded.iter().all(|sample| sample.is_finite()));
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
fn rejects_malformed_echo_non_primary_and_pcm_packets() {
    let good = packet(2, -0.25);
    assert!(receive_audio(&good[..PACKET_SIZE - 1], 1).is_none());
    assert!(receive_audio(&packet(1, 0.5), 1).is_none());
    assert!(receive_audio(&[0; 524], 1).is_none());
    let mut bad = good;
    bad[10] = 1;
    assert!(receive_audio(&bad, 1).is_none());
    bad = good;
    bad[0] = 0;
    assert!(receive_audio(&bad, 1).is_none());
    assert_eq!(
        receive_audio(&good, 1).unwrap().1,
        good[AUDIO_HEADER_SIZE..]
    );
}

#[test]
fn rejects_wrong_sample_rate_and_ignores_foreign_endpoint() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let foreign = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "peer").unwrap();
    let destination = network.socket.local_addr().unwrap();
    let (_, mut mic) = RingBuffer::new(4096);
    let (mut speaker, _mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
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
    wait_for_datagram(&network.socket);
    assert!(
        network
            .tick(&mut mic, &mut speaker, Instant::now())
            .is_err()
    );
}

#[test]
fn two_clients_exchange_opus_through_real_sfu_without_echo() {
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
        let (mut a_speaker, mut a_mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
        let (mut b_input, mut b_mic) = RingBuffer::new(4096);
        let (mut b_speaker, mut b_mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while a.peer_id.is_none() || b.peer_id.is_none() {
                assert!(Instant::now() < deadline, "join timeout");
                a.tick(&mut a_mic, &mut a_speaker, Instant::now()).unwrap();
                b.tick(&mut b_mic, &mut b_speaker, Instant::now()).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
            assert_ne!(a.peer_id, b.peer_id);
            let a_remote = b.peer_id.unwrap();
            let b_remote = a.peer_id.unwrap();
            for (a_sample, b_sample) in crate::codec::tests::tone(0.25)
                .into_iter()
                .zip(crate::codec::tests::tone(-0.5))
            {
                a_input.push(a_sample).unwrap();
                b_input.push(b_sample).unwrap();
            }
            // Hold the playout clock while the real UDP transport delivers the frames.
            // A descheduled CI runner must not skip the first frame as stale audio.
            let now = Instant::now();
            while !a.peers.contains_key(&a_remote) || !b.peers.contains_key(&b_remote) {
                assert!(Instant::now() < deadline, "audio timeout");
                a.tick(&mut a_mic, &mut a_speaker, now).unwrap();
                b.tick(&mut b_mic, &mut b_speaker, now).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
            let ready = a.peers[&a_remote].ready_at.max(b.peers[&b_remote].ready_at);
            a.tick(&mut a_mic, &mut a_speaker, ready).unwrap();
            b.tick(&mut b_mic, &mut b_speaker, ready).unwrap();
            let a_output = &mut a_mixer.rings[usize::from(a_remote) - 1];
            let b_output = &mut b_mixer.rings[usize::from(b_remote) - 1];
            let expected_a = crate::codec::tests::decode(&packet(2, -0.5)[AUDIO_HEADER_SIZE..]);
            let expected_b = crate::codec::tests::decode(&packet(2, 0.25)[AUDIO_HEADER_SIZE..]);
            for (a, b) in expected_a.into_iter().zip(expected_b) {
                assert_eq!(a_output.pop().unwrap().value, a);
                assert_eq!(b_output.pop().unwrap().value, b);
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
    let (speaker, _mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
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
    let mut encoder = codec::encoder().unwrap();
    let mut next_packet = start;
    let mut sequence = 0;
    let mut started = false;
    for tick in 0..20_000 {
        let now = start + Duration::from_micros(tick * 250);
        if now >= next_packet {
            let mut bytes = packet(2, 0.5);
            bytes[AUDIO_HEADER_SIZE..]
                .copy_from_slice(&crate::codec::tests::encode(&mut encoder, 0.5));
            AudioHeader::new(2, STREAM_PRIMARY, sequence, 0)
                .to_bytes(&mut bytes)
                .unwrap();
            network.incoming(&bytes, 1, now);
            next_packet += CHUNK_TIME;
            sequence += 1;
        }
        network.playback(&mut speaker, now).unwrap();
        if output.slots() > 0 {
            assert_eq!(
                output.slots(),
                FRAME_SAMPLES,
                "no catch-up bursts or partial chunks"
            );
            started = true;
            emitted += 1;
            for _ in 0..FRAME_SAMPLES {
                let sample = output.pop().unwrap().value;
                assert!(sample.is_finite() && sample.abs() <= 1.0);
                if sample.abs() >= 0.1 {
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
    assert!(emitted > 1900, "continuous cadence after initial prefill");
    assert!(
        strong > emitted * FRAME_SAMPLES * 75 / 100,
        "signal survives impairment"
    );
}

#[test]
fn independent_peers_are_paced_together_and_expire_without_unbounded_silence() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "test").unwrap();
    let (mut speaker, mut mixer) = crate::mixer::channels_with_capacity(FRAME_SAMPLES);
    let [_, output, other, ..] = &mut mixer.rings;
    let start = network.started;
    network.incoming(&packet(2, 0.25), 1, start);
    network.incoming(&packet(3, -0.5), 1, start);
    network.playback(&mut speaker, start).unwrap();
    assert_eq!(network.peers.len(), 2);
    network
        .playback(&mut speaker, start + CHUNK_TIME * 3)
        .unwrap();
    let expected_a = crate::codec::tests::decode(&packet(2, 0.25)[AUDIO_HEADER_SIZE..]);
    let expected_b = crate::codec::tests::decode(&packet(3, -0.5)[AUDIO_HEADER_SIZE..]);
    for (a, b) in expected_a.into_iter().zip(expected_b) {
        assert_eq!(output.pop().unwrap().value, a);
        assert_eq!(other.pop().unwrap().value, b);
    }
    network
        .playback(&mut speaker, start + SERVER_TIMEOUT)
        .unwrap();
    assert!(network.peers.is_empty());
    assert!(network.next_playout.is_none());
    assert!(output.pop().is_err());
}

#[test]
fn full_speaker_retains_frames_and_stall_does_not_overfill() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "test").unwrap();
    let (mut speaker, mut mixer) = crate::mixer::channels_with_capacity(FRAME_SAMPLES);
    let output = &mut mixer.rings[1];
    let start = network.started;
    network.incoming(&packet(2, 0.25), 1, start);
    network.playback(&mut speaker, start).unwrap();
    speaker.push(2, &[0.75]);
    network
        .playback(&mut speaker, start + CHUNK_TIME * 3)
        .unwrap();
    assert_eq!(output.slots(), 1);
    assert_eq!(network.peers[&2].buffered_ms(), 2.5);
    assert_eq!(network.peers[&2].diagnostics.rx_push_dropped, 0);
    assert_eq!(network.peers[&2].diagnostics.skipped_frames, 0);
    assert_eq!(output.pop().unwrap().value, 0.75);
    let stalled = start + CHUNK_TIME * 30;
    network.playback(&mut speaker, stalled).unwrap();
    assert_eq!(output.slots(), FRAME_SAMPLES);
    network.playback(&mut speaker, stalled).unwrap();
    assert_eq!(output.slots(), FRAME_SAMPLES);
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
        network
            .playback(&mut speaker, start + CHUNK_TIME * sequence)
            .unwrap();
    }
    assert!(output.pop().is_err());
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();
    for tick in 3..6 {
        network
            .playback(&mut speaker, start + CHUNK_TIME * tick)
            .unwrap();
        let mut expected = [0.0; FRAME_SAMPLES];
        decoder
            .decode_float(&packet(2, 0.25)[AUDIO_HEADER_SIZE..], &mut expected, false)
            .unwrap();
        for sample in expected {
            assert_eq!(output.pop().unwrap().value, sample);
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
                    let (rx, mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
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
                .map(|(net, ..)| net.peer_id.unwrap())
                .collect();
            for (_, input, ..) in &mut clients[..4] {
                for sample in crate::codec::tests::tone(0.8) {
                    input.push(sample).unwrap();
                }
            }
            // Receive every peer at a fixed playout time, independently of OS scheduling.
            let now = Instant::now();
            loop {
                assert!(Instant::now() < deadline, "audio timeout");
                for (net, _, mic, rx, _) in &mut clients {
                    net.tick(mic, rx, now).unwrap();
                }
                if remote_ids
                    .iter()
                    .all(|id| clients[4].0.peers.contains_key(id))
                {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            let (net, _, mic, rx, mixer) = &mut clients[4];
            let ready = net.peers.values().map(|peer| peer.ready_at).max().unwrap();
            // Prefill the remote SPSC queues before the first hardware callback.
            // The first decoded frame remains at the head; subsequent ticks supply the reserve.
            for tick in 0..8 {
                net.tick(mic, rx, ready + CHUNK_TIME * tick).unwrap();
            }
            let mut output = [0.0_f32; FRAME_SAMPLES * 2];
            mixer.playback(&mut output, 2, &crate::audio::Stats::default());
            let decoded = crate::codec::tests::decode(&packet(2, 0.8)[AUDIO_HEADER_SIZE..]);
            for (frame, sample) in output.chunks_exact(2).zip(decoded) {
                let expected = (f64::from(sample) * 4.0).tanh();
                assert!((f64::from(frame[0]) - expected).abs() < 1e-6);
                assert_eq!(frame[0], frame[1]);
            }
        }));
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    });
}

#[test]
fn capture_blocks_are_reframed_without_sample_loss_and_invalid_samples_are_sanitized() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "test").unwrap();
    let (mut input, mut mic) = RingBuffer::new(4096);
    let mut reference = codec::encoder().unwrap();
    let mut captured = Vec::new();
    // Fifteen 128-sample callbacks become exactly sixteen 120-sample Opus packets.
    for block in 0..15 {
        for index in 0..crate::audio::BUFFER_FRAMES {
            let value = match index {
                0 => f32::NAN,
                1 => f32::INFINITY,
                2 => 2.0,
                3 => -2.0,
                _ => crate::codec::tests::tone(0.5)[(block * 128 + index) % FRAME_SAMPLES],
            };
            input.push(value).unwrap();
            captured.push(if value.is_finite() {
                value.clamp(-1.0, 1.0)
            } else {
                0.0
            });
        }
        network.send_audio(1, &mut mic).unwrap();
        assert_eq!(mic.slots(), (block + 1) * 128 % FRAME_SAMPLES);
    }
    for (index, chunk) in captured.chunks_exact(FRAME_SAMPLES).enumerate() {
        let bytes = receive(&server);
        assert_eq!(bytes.len(), PACKET_SIZE);
        let header = AudioHeader::from_bytes(&bytes).unwrap();
        assert_eq!(header.sequence_number, u32::try_from(index).unwrap());
        assert_eq!(
            header.timestamp_samples,
            u16::try_from(index * FRAME_SAMPLES).unwrap()
        );
        let mut expected = [0; PAYLOAD_SIZE];
        reference.encode_float(chunk, &mut expected).unwrap();
        assert_eq!(&bytes[AUDIO_HEADER_SIZE..], &expected);
    }
    assert_eq!(mic.slots(), 0);
    assert_eq!(network.sequence, 16);
}

#[test]
fn roster_tracks_silent_peers_and_resets_departed_controls() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "local").unwrap();
    network.peer_id = Some(1);
    let (speaker, _) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
    let peer = |id, name| jam_protocol::PeerInfo {
        peer_id: id,
        nickname: [name; NICKNAME_CAPACITY],
        nickname_len: 1,
    };
    let local = peer(1, b'L');
    let remote = peer(2, b'G');
    network.update_roster(&[local, remote], &speaker);
    assert_eq!(network.take_roster(), Some(vec![remote]));
    assert!(network.take_roster().is_none());
    network.update_roster(&[local, remote], &speaker);
    assert!(network.take_roster().is_none());
    // A silent musician may already have UI settings before sending any audio.
    speaker.controls[1].solo.store(true, Ordering::Relaxed);
    speaker.controls[1].mute.store(true, Ordering::Relaxed);
    speaker.controls[1]
        .gain
        .store(0.5_f32.to_bits(), Ordering::Relaxed);
    network.update_roster(&[local], &speaker);
    assert_eq!(network.take_roster(), Some(vec![]));
    assert!(!speaker.controls[1].solo.load(Ordering::Relaxed));
    assert!(!speaker.controls[1].mute.load(Ordering::Relaxed));
    assert_eq!(
        f32::from_bits(speaker.controls[1].gain.load(Ordering::Relaxed)),
        1.0
    );
    network.update_roster(&[local, peer(2, b'B')], &speaker);
    assert_eq!(network.take_roster(), Some(vec![peer(2, b'B')]));
}

#[test]
fn telemetry_matches_ping_and_publishes_every_half_second() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "meter").unwrap();
    network.peer_id = Some(1);
    let start = network.started;
    let (_, mut mic) = RingBuffer::new(128);
    let (mut rx, _mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
    network
        .tick(&mut mic, &mut rx, start + PING_INTERVAL)
        .unwrap();
    let (stamp, _) = network.pending_ping.unwrap();
    let _ = network.take_telemetry();
    send_control(
        &server,
        network.socket.local_addr().unwrap(),
        &ControlMessage::Pong {
            client_timestamp_us: stamp + 1,
            server_timestamp_us: 0,
        },
    );
    wait_for_datagram(&network.socket);
    network
        .tick(&mut mic, &mut rx, start + Duration::from_millis(1010))
        .unwrap();
    assert!(network.rtt.is_none());
    send_control(
        &server,
        network.socket.local_addr().unwrap(),
        &ControlMessage::Pong {
            client_timestamp_us: stamp,
            server_timestamp_us: 0,
        },
    );
    wait_for_datagram(&network.socket);
    network
        .tick(&mut mic, &mut rx, start + Duration::from_millis(1020))
        .unwrap();
    assert!(network.take_telemetry().is_none());
    network
        .tick(&mut mic, &mut rx, start + Duration::from_millis(1500))
        .unwrap();
    let metrics = network.take_telemetry().unwrap();
    assert_eq!(metrics.rtt_ms, Some(20.0));
    assert_eq!(metrics.rtt_max_ms, Some(20.0));
    assert_eq!(metrics.worker_max_gap_ms, 480.0);
    assert!(network.take_telemetry().is_none());
    network.publish_telemetry(start + Duration::from_secs(7));
    assert!(network.take_telemetry().unwrap().rtt_ms.is_none());
}

#[test]
fn telemetry_observes_simulated_loss_jitter_and_actual_buffer_occupancy() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "meter")
        .unwrap()
        .configure(Settings {
            loss: 0.02,
            jitter_ms: 10,
            target_chunks: 3,
        });
    network.random = 12345;
    let (mut rx, _mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
    let start = network.started;
    let mut bytes = packet(2, 0.3);
    for sequence in 0..4000 {
        AudioHeader::new(2, STREAM_PRIMARY, sequence, 0)
            .to_bytes(&mut bytes)
            .unwrap();
        let now = start + CHUNK_TIME * sequence;
        network.incoming(&bytes, 1, now);
        network.playback(&mut rx, now).unwrap();
    }
    network.publish_telemetry(start + Duration::from_secs(10));
    let metrics = network.take_telemetry().unwrap();
    assert!((1.0..3.5).contains(&metrics.loss_percent), "{metrics:?}");
    assert!(metrics.jitter_ms > 0.5, "{metrics:?}");
    assert_eq!(metrics.buffer_ms, network.peers[&2].buffered_ms());
    assert!(metrics.buffer_ms > 0.0);
    network
        .playback(&mut rx, start + Duration::from_millis(10020))
        .unwrap();
    network
        .playback(&mut rx, start + Duration::from_secs(16))
        .unwrap();
    network.publish_telemetry(start + Duration::from_secs(16));
    let metrics = network.take_telemetry().unwrap();
    assert_eq!(metrics.buffer_ms, 0.0);
    assert_eq!(metrics.loss_percent, 0.0);
}

#[test]
fn oversized_datagrams_are_dropped_and_valid_handshake_still_succeeds() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "peer").unwrap();
    let destination = network.socket.local_addr().unwrap();
    let (_, mut mic) = RingBuffer::new(128);
    let (mut speaker, _mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
    let accepted = ControlMessage::JoinAccepted {
        assigned_peer_id: 1,
        sample_rate: SAMPLE_RATE,
    };
    let mut packet = [0; MAX_PACKET_SIZE + 100];
    accepted.to_bytes(&mut packet).unwrap();
    let last_server = network.last_server;
    for length in [MAX_PACKET_SIZE, MAX_PACKET_SIZE + 100] {
        server.send_to(&packet[..length], destination).unwrap();
        wait_for_datagram(&network.socket);
        network
            .tick(&mut mic, &mut speaker, Instant::now())
            .unwrap();
        assert!(!network.is_connected());
        assert_eq!(network.last_server, last_server);
    }
    send_control(&server, destination, &accepted);
    wait_for_datagram(&network.socket);
    network
        .tick(&mut mic, &mut speaker, Instant::now())
        .unwrap();
    assert!(network.is_connected());
}

#[test]
fn repeated_worker_stalls_preserve_clock_phase_and_nominal_buffering() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "test").unwrap();
    let (mut speaker, mut mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
    let start = network.started;
    for sequence in 0..400_u32 {
        let at = start + CHUNK_TIME * sequence;
        let mut bytes = packet(2, 0.25);
        AudioHeader::new(2, STREAM_PRIMARY, sequence, 0)
            .to_bytes(&mut bytes)
            .unwrap();
        network.incoming(&bytes, 1, at);
        // Wake only every fourth deadline, with a fractional scheduling offset.
        if sequence % 4 == 0 {
            let now = at + Duration::from_micros(700);
            network.playback(&mut speaker, now).unwrap();
            assert!(network.peers[&2].buffered_ms() <= 10.0);
            assert!(mixer.rings[1].slots() <= FRAME_SAMPLES * 4);
            assert_eq!(network.peers[&2].target_chunks(), 3);
            while mixer.rings[1].pop().is_ok() {}
            if sequence >= 4 {
                assert_eq!(
                    network.next_playout.unwrap(),
                    start + CHUNK_TIME * (sequence + 1) + Duration::from_micros(700)
                );
            }
        }
    }
}

#[test]
fn delayed_tick_delivers_received_frames_before_advancing_playout() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "test").unwrap();
    let (mut speaker, mut mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
    let start = network.started;
    let mut encoder = codec::encoder().unwrap();
    let mut reference = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();
    let mut expected = Vec::new();
    for sequence in 0..8 {
        let chunk = crate::codec::tests::encode(&mut encoder, 0.5);
        let mut decoded = [0.0; FRAME_SAMPLES];
        reference.decode_float(&chunk, &mut decoded, false).unwrap();
        expected.push(decoded);
        network.delayed.push(Delayed {
            due: start + CHUNK_TIME * sequence,
            peer: 2,
            sequence,
            chunk,
        });
    }
    network.playback(&mut speaker, start).unwrap();
    let ready = start + CHUNK_TIME * 3;
    network.playback(&mut speaker, ready).unwrap();
    for sample in expected[0] {
        assert_eq!(mixer.rings[1].pop().unwrap().value, sample);
    }
    // Wake 5 ms late. Three expired/current frames fit, so none should be discarded.
    let now = ready + CHUNK_TIME * 3;
    network.playback(&mut speaker, now).unwrap();
    assert_eq!(mixer.rings[1].slots(), FRAME_SAMPLES * 3);
    for frame in &expected[1..4] {
        for &sample in frame {
            assert_eq!(mixer.rings[1].pop().unwrap().value, sample);
        }
    }
    assert_eq!(network.peers[&2].target_chunks(), 3);
    assert_eq!(network.peers[&2].buffered_ms(), 7.5);
    assert_eq!(network.next_playout, Some(now + CHUNK_TIME));
}

#[test]
fn low_network_jitter_and_one_to_five_ms_worker_delays_keep_nominal_latency() {
    for nominal in [2, 3] {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut network = Network::connect(server.local_addr().unwrap(), "test").unwrap();
        network.settings.target_chunks = nominal;
        let (mut speaker, mut mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
        let start = network.started;
        let mut encoder = codec::encoder().unwrap();
        // Arrival timestamps remain independent of when the worker wakes.
        let mut pending = Vec::new();
        for sequence in 0..4000 {
            pending.push(Delayed {
                due: start
                    + CHUNK_TIME * sequence
                    + Duration::from_micros(u64::from((sequence * 7919) % 3001)),
                peer: 2,
                sequence,
                chunk: crate::codec::tests::encode(&mut encoder, 0.5),
            });
        }
        pending.sort_unstable_by_key(|packet| std::cmp::Reverse(packet.due));
        let mut now = start;
        let mut emitted = 0;
        for tick in 0..3300 {
            now += Duration::from_millis(1 + tick % 5);
            while pending.last().is_some_and(|packet| packet.due <= now) {
                network.delayed.push(pending.pop().unwrap());
            }
            assert!(network.delayed.len() <= 4);
            network.playback(&mut speaker, now).unwrap();
            if let Some(peer) = network.peers.get(&2) {
                assert_eq!(peer.target_chunks(), nominal, "at tick {tick}");
                assert!(
                    peer.buffered_ms() <= 10.0,
                    "at tick {tick}: {} ms",
                    peer.buffered_ms()
                );
            }
            assert!(mixer.rings[1].slots() <= FRAME_SAMPLES * 4);
            emitted += mixer.rings[1].slots() / FRAME_SAMPLES;
            while mixer.rings[1].pop().is_ok() {}
        }
        assert!(
            emitted > 3900,
            "catch-up should preserve the stream: {emitted}"
        );
    }
}

#[test]
fn diagnostic_totals_survive_peer_removal_and_id_reuse_without_loss() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "diagnostic").unwrap();
    let start = network.started;
    let (mut rx, _mixer) = crate::mixer::channels(crate::audio::BUFFER_FRAMES);
    let bytes = packet(2, 0.3);
    let chunk: Encoded = bytes[AUDIO_HEADER_SIZE..].try_into().unwrap();
    let mut peer = PeerJitterBuffer::new(100, start, 1).unwrap();
    peer.insert(100, &chunk, start).unwrap();
    peer.pop(start + CHUNK_TIME).unwrap();
    peer.insert(100, &chunk, start + CHUNK_TIME).unwrap();
    peer.insert(0, &chunk, start + CHUNK_TIME).unwrap();
    // Seed prior counters to test retention independently of queue-overflow behavior.
    peer.diagnostics.rx_push_dropped = 1;
    peer.diagnostics.plc_frames = 2;
    peer.diagnostics.skipped_frames = 1;
    network.peers.insert(2, peer);
    network.arrivals.insert(2, ArrivalStats::new(100, start));
    while rx.available_chunks(2) > 0 {
        assert!(rx.push(2, &[0.0; FRAME_SAMPLES]));
    }
    network.playback(&mut rx, start + CHUNK_TIME * 2).unwrap();
    network.publish_telemetry(start + Duration::from_millis(500));
    let first = network.take_telemetry().unwrap();
    assert_eq!(first.loss_percent, 0.0);
    assert_eq!(first.diagnostics.late_dropped, 2);
    assert_eq!(first.diagnostics.late_dropped_far, 1);
    assert_eq!(first.diagnostics.rx_push_dropped, 1);
    assert_eq!(first.diagnostics.plc_frames, 2);
    assert_eq!(first.diagnostics.skipped_frames, 1);
    network.update_roster(&[], &rx);
    network.publish_telemetry(start + Duration::from_secs(1));
    assert_eq!(
        network
            .take_telemetry()
            .unwrap()
            .diagnostics
            .rx_push_dropped,
        1
    );
    network
        .peers
        .insert(2, PeerJitterBuffer::new(0, start, 1).unwrap());
    network.peers.get_mut(&2).unwrap().diagnostics = crate::jitter::Diagnostics {
        late_dropped: 7,
        late_dropped_far: 2,
        plc_frames: 5,
        skipped_frames: 9,
        rx_push_dropped: 4,
        resyncs: 3,
    };
    network
        .playback(&mut rx, start + Duration::from_secs(6))
        .unwrap();
    network.publish_telemetry(start + Duration::from_secs(6));
    let final_metrics = network.take_telemetry().unwrap();
    assert_eq!(final_metrics.diagnostics.late_dropped, 9);
    assert_eq!(final_metrics.diagnostics.late_dropped_far, 3);
    assert_eq!(final_metrics.diagnostics.plc_frames, 7);
    assert_eq!(final_metrics.diagnostics.skipped_frames, 10);
    assert_eq!(final_metrics.diagnostics.rx_push_dropped, 5);
    assert_eq!(final_metrics.diagnostics.resyncs, 3);
}

#[test]
fn interval_peaks_reset_without_repeating_stale_rtt() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "peaks").unwrap();
    let start = network.started;
    network.observe_tick(start);
    network.observe_tick(start + Duration::from_millis(16));
    network.observe_tick(start + Duration::from_millis(18));
    network.pending_ping = Some((1, start));
    network.pong(2, start + Duration::from_millis(90));
    assert!(network.rtt_max_ms.is_none());
    network.pong(1, start + Duration::from_millis(80));
    network.pending_ping = Some((2, start + Duration::from_millis(100)));
    network.pong(2, start + Duration::from_millis(120));
    network.publish_telemetry(start + Duration::from_millis(500));
    let metrics = network.take_telemetry().unwrap();
    assert_eq!(metrics.rtt_ms, Some(20.0));
    assert_eq!(metrics.rtt_max_ms, Some(80.0));
    assert_eq!(metrics.worker_max_gap_ms, 16.0);
    network.publish_telemetry(start + Duration::from_secs(1));
    let metrics = network.take_telemetry().unwrap();
    assert_eq!(metrics.rtt_ms, Some(20.0));
    assert_eq!(metrics.rtt_max_ms, None);
    assert_eq!(metrics.worker_max_gap_ms, 0.0);
}

#[test]
fn mixer_stays_ready_through_five_to_fifteen_ms_worker_gaps() {
    use std::sync::atomic::Ordering;
    for frames in [64, 128, 256] {
        for phase in [0, 1, 63, 119] {
            let server = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut network = Network::connect(server.local_addr().unwrap(), "scheduling").unwrap();
            let (mut speaker, mut mixer) = crate::mixer::channels(frames);
            let stats = crate::audio::Stats::default();
            let start = network.started;
            let mut encoder = codec::encoder().unwrap();
            let mut next_callback = phase;
            let mut next_wake = 0;
            let mut wake = 0;
            let mut started = false;
            let mut output = vec![0.0; frames * 2];
            for sample in 0..48_000_u32 {
                let now = start + Duration::from_secs_f64(f64::from(sample) / 48_000.0);
                if sample % FRAME_SAMPLES as u32 == 0 {
                    let chunk = crate::codec::tests::encode(&mut encoder, 0.5);
                    for peer in [2, 3] {
                        network.delayed.push(Delayed {
                            due: now,
                            peer,
                            sequence: sample / FRAME_SAMPLES as u32,
                            chunk,
                        });
                    }
                }
                // Worst equal-time order: hardware runs before the worker refills.
                if sample as usize == next_callback {
                    mixer.playback(&mut output, 2, &stats);
                    if stats.playback_started.load(Ordering::Relaxed) {
                        if started {
                            assert_eq!(stats.telemetry().not_ready_samples, 0);
                        }
                        stats.not_ready_samples.store(0, Ordering::Relaxed);
                        started = true;
                    }
                    next_callback += frames;
                }
                if sample as usize == next_wake {
                    network.playback(&mut speaker, now).unwrap();
                    next_wake += [120, 240, 480, 720, 48][wake % 5];
                    wake += 1;
                }
            }
            assert!(started);
            assert_eq!(
                stats.output_underflows.load(Ordering::Relaxed),
                0,
                "frames={frames}, phase={phase}"
            );
            assert_eq!(stats.telemetry().not_ready_samples, 0);
            assert_eq!(stats.telemetry().align_dropped_samples, 0);
            for peer in network.peers.values() {
                assert_eq!(peer.diagnostics.rx_push_dropped, 0);
                assert_eq!(peer.diagnostics.skipped_frames, 0);
                assert_eq!(peer.diagnostics.plc_frames, 0);
            }
        }
    }
}
