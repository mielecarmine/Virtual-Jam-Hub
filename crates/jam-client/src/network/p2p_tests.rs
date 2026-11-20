use super::*;
use rtrb::RingBuffer;

fn drain(socket: &UdpSocket) -> Vec<(Vec<u8>, SocketAddr)> {
    let mut packets = Vec::new();
    let mut bytes = [0; MAX_PACKET_SIZE];
    loop {
        match socket.recv_from(&mut bytes) {
            Ok((length, source)) => packets.push((bytes[..length].to_vec(), source)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return packets,
            Err(error) => panic!("drain: {error}"),
        }
    }
}

#[test]
fn real_sfu_handshake_direct_audio_filter_and_fallback_without_rejoining() {
    let mut server = jam_server::Server::bind("127.0.0.1:0").unwrap();
    let server_addr = server.local_addr().unwrap();
    let stop = AtomicBool::new(false);
    let reflected = std::sync::atomic::AtomicU32::new(0);
    thread::scope(|scope| {
        let worker = scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                server.poll().unwrap();
                if server.last_reflection_duration().is_some() {
                    reflected.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut a = Network::connect(server_addr, "a").unwrap();
            let mut b = Network::connect(server_addr, "b").unwrap();
            let (mut a_input, mut a_mic) = RingBuffer::new(4096);
            let (mut b_input, mut b_mic) = RingBuffer::new(4096);
            let (mut a_rx, _a_mix) = crate::mixer::channels(128);
            let (mut b_rx, _b_mix) = crate::mixer::channels(128);
            let deadline = Instant::now() + Duration::from_secs(3);
            while a.routes.mask() == 0 || b.routes.mask() == 0 {
                assert!(Instant::now() < deadline, "P2P handshake timed out");
                a.tick(&mut a_mic, &mut a_rx, Instant::now()).unwrap();
                b.tick(&mut b_mic, &mut b_rx, Instant::now()).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(a.routes.mask(), 1 << (b.peer_id.unwrap() - 1));
            assert_eq!(b.routes.mask(), 1 << (a.peer_id.unwrap() - 1));
            assert!(!a.routes.needs_relay());
            assert!(!b.routes.needs_relay());
            let assigned_ids = (a.peer_id, b.peer_id);
            drain(&a.socket);
            drain(&b.socket);
            let baseline = Instant::now();
            for _ in 0..FRAME_SAMPLES {
                a_input.push(0.25).unwrap();
            }
            a.send_audio(a.peer_id.unwrap(), &mut a_mic).unwrap();
            super::tests::wait_for_datagram(&b.socket);
            let audio: Vec<_> = drain(&b.socket)
                .into_iter()
                .filter(|(bytes, _)| AudioHeader::from_bytes(bytes).is_ok())
                .collect();
            assert_eq!(audio.len(), 1, "one direct frame, no SFU duplicate");
            assert_eq!(audio[0].1, a.socket.local_addr().unwrap());
            assert!(receive_audio(&audio[0].0, b.peer_id.unwrap()).is_some());
            assert_eq!(
                reflected.load(Ordering::Relaxed),
                0,
                "direct-only sends no server audio"
            );

            // Deliberately send a second copy to SFU: the route notification must
            // already suppress it for B. This verifies the server received the mask.
            a.send(&audio[0].0).unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            while reflected.load(Ordering::Relaxed) == 0 {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
            assert!(
                drain(&b.socket)
                    .iter()
                    .all(|(bytes, _)| AudioHeader::from_bytes(bytes).is_err())
            );

            // Interrupt the direct link by withholding B's traffic. Simulated time
            // keeps the >600ms boundary deterministic; no real-time sleeping needed.
            for _ in 0..FRAME_SAMPLES {
                a_input.push(0.5).unwrap();
            }
            let fallback = baseline + Duration::from_millis(650);
            a.tick(&mut a_mic, &mut a_rx, fallback).unwrap();
            assert_eq!(a.routes.mask(), 0);
            assert!(a.routes.needs_relay());
            super::tests::wait_for_datagram(&b.socket);
            let audio: Vec<_> = drain(&b.socket)
                .into_iter()
                .filter(|(bytes, _)| AudioHeader::from_bytes(bytes).is_ok())
                .collect();
            assert_eq!(audio.len(), 1);
            assert_eq!(audio[0].1, server_addr, "relay resumes in the timeout tick");
            let (header, _) = receive_audio(&audio[0].0, b.peer_id.unwrap()).unwrap();
            assert_eq!(header.sequence_number, 1, "same continuous encoder stream");
            for _ in 0..FRAME_SAMPLES {
                b_input.push(-0.5).unwrap();
            }
            b.tick(&mut b_mic, &mut b_rx, fallback).unwrap();
            assert_eq!(b.routes.mask(), 0);
            super::tests::wait_for_datagram(&a.socket);
            a.tick(&mut a_mic, &mut a_rx, fallback).unwrap();
            assert!(a.peers.contains_key(&b.peer_id.unwrap()));
            assert_eq!((a.peer_id, b.peer_id), assigned_ids);
        }));
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        result.unwrap();
    });
}

#[test]
fn direct_rtt_drives_budget_and_leader_phase_and_peer_controls_cannot_forge_server() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "follower").unwrap();
    network.peer_id = Some(1);
    network.rtt = Some((40.0, network.started));
    let (mut rx, _) = crate::mixer::channels(128);
    let remote = jam_protocol::PeerInfo {
        peer_id: 2,
        nickname: [0; 32],
        nickname_len: 0,
        public_addr: peer.local_addr().unwrap(),
        local_addr: None,
    };
    network.update_roster(&[remote], &rx);
    let now = network.started;
    let probes = network.routes.probes(1, now);
    let ControlMessage::P2pProbe {
        client_timestamp_us,
        ..
    } = probes[0].1
    else {
        panic!()
    };
    // Only probe/ACK are accepted from peer endpoints, never room authority.
    network.direct_control(
        &ControlMessage::JoinAccepted {
            assigned_peer_id: 2,
            sample_rate: SAMPLE_RATE,
        },
        remote.public_addr,
        Some(2),
        now,
    );
    network.direct_control(
        &ControlMessage::MetronomeState {
            state: MetronomeState::default(),
        },
        remote.public_addr,
        Some(2),
        now,
    );
    assert_eq!(network.peer_id, Some(1));
    assert_eq!(network.metronome, None);
    let config = MetronomeConfig {
        leader_peer_id: 2,
        ..MetronomeConfig::default()
    };
    let relayed = network.leader_phase_delay_samples(config, &rx);
    let ack = ControlMessage::P2pAck {
        sender_peer_id: 2,
        target_peer_id: 1,
        echo_timestamp_us: client_timestamp_us,
    };
    network.direct_control(&ack, remote.public_addr, Some(3), now);
    assert_eq!(network.routes.mask(), 0);
    network.direct_control(
        &ack,
        remote.public_addr,
        Some(2),
        now + Duration::from_millis(8),
    );
    let direct = network.leader_phase_delay_samples(config, &rx);
    assert_eq!(relayed - direct, 36 * 48, "40ms SFU vs 8/2ms P2P");
    network.publish_telemetry(now + Duration::from_millis(500));
    let metrics = network.take_telemetry().unwrap();
    assert_eq!(metrics.rtt_ms, Some(40.0));
    assert_eq!(metrics.route_rtt_ms, Some(8.0));
    assert_eq!(metrics.direct_peers_mask, 2);
    let budget = telemetry::LatencyBudget::estimate(
        128,
        128,
        network.codec_lookahead_samples,
        metrics.route_rtt_ms,
        metrics.buffer_ms,
    );
    assert_eq!(budget.network_one_way_ms, Some(4.0));
    network.sync_metronome(&mut rx, now);
}

#[test]
fn reordered_roster_starts_probing_and_hybrid_audio_sends_one_sfu_copy() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server.set_nonblocking(true).unwrap();
    let direct = UdpSocket::bind("127.0.0.1:0").unwrap();
    direct.set_nonblocking(true).unwrap();
    let relayed = UdpSocket::bind("127.0.0.1:0").unwrap();
    relayed.set_nonblocking(true).unwrap();
    let mut network = Network::connect(server.local_addr().unwrap(), "mixed").unwrap();
    let now = network.started;
    let (rx, _) = crate::mixer::channels(128);
    let roster = [direct.local_addr().unwrap(), relayed.local_addr().unwrap()]
        .into_iter()
        .enumerate()
        .map(|(index, public_addr)| jam_protocol::PeerInfo {
            peer_id: u8::try_from(index + 2).unwrap(),
            nickname: [0; 32],
            nickname_len: 0,
            public_addr,
            local_addr: None,
        })
        .collect();
    network
        .server_control(ControlMessage::PeerListUpdate { peers: roster }, &rx, now)
        .unwrap();
    assert_eq!(network.roster.len(), 0);
    network
        .server_control(
            ControlMessage::JoinAccepted {
                assigned_peer_id: 1,
                sample_rate: SAMPLE_RATE,
            },
            &rx,
            now,
        )
        .unwrap();
    assert_eq!(network.roster.len(), 2);
    let probes = network.routes.probes(1, now);
    let ControlMessage::P2pProbe {
        client_timestamp_us,
        ..
    } = probes[0].1
    else {
        panic!()
    };
    network.routes.ack(
        2,
        direct.local_addr().unwrap(),
        client_timestamp_us,
        now + Duration::from_millis(4),
    );
    assert!(network.routes.needs_relay());
    assert_eq!(network.routes.effective_rtt(Some(40.0)), Some(40.0));
    assert_eq!(network.routes.effective_rtt(None), None);
    let (mut input, mut mic) = RingBuffer::new(FRAME_SAMPLES);
    for _ in 0..FRAME_SAMPLES {
        input.push(0.2).unwrap();
    }
    network.send_audio(1, &mut mic).unwrap();
    super::tests::wait_for_datagram(&server);
    super::tests::wait_for_datagram(&direct);
    let sfu_packets = drain(&server);
    let direct_packets = drain(&direct);
    assert_eq!(sfu_packets.len(), 1);
    assert_eq!(direct_packets.len(), 1);
    assert_eq!(
        sfu_packets, direct_packets,
        "same bytes and same local UDP port"
    );
    assert!(
        drain(&relayed).is_empty(),
        "relayed musicians receive via SFU only"
    );
}
