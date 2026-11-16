use super::*;
use rtrb::RingBuffer;

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

#[test]
fn join_timeout_is_reported_and_disconnect_sends_leave_after_acceptance() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let (_, mut mic) = RingBuffer::new(4096);
    let (mut speaker, _mixer) = crate::mixer::channels();
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
            for (a_sample, b_sample) in crate::codec::tests::tone(0.25)
                .into_iter()
                .zip(crate::codec::tests::tone(-0.5))
            {
                a_input.push(a_sample).unwrap();
                b_input.push(b_sample).unwrap();
            }
            while a_output.slots() < FRAME_SAMPLES || b_output.slots() < FRAME_SAMPLES {
                assert!(Instant::now() < deadline, "audio timeout");
                a.tick(&mut a_mic, &mut a_speaker, Instant::now()).unwrap();
                b.tick(&mut b_mic, &mut b_speaker, Instant::now()).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
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
fn full_speaker_discards_whole_frame_and_stall_does_not_burst() {
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
                for sample in crate::codec::tests::tone(0.8) {
                    input.push(sample).unwrap();
                }
            }
            loop {
                assert!(Instant::now() < deadline, "audio timeout");
                for (net, _, mic, rx, _) in &mut clients {
                    net.tick(mic, rx, Instant::now()).unwrap();
                }
                if remote_ids
                    .iter()
                    .all(|&id| clients[4].4.rings[id].slots() >= FRAME_SAMPLES)
                {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            let (_, _, _, _, mixer) = &mut clients[4];
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
    let (speaker, _) = crate::mixer::channels();
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
