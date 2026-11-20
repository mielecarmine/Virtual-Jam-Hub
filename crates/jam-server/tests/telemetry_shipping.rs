//! Integration tests verifying asynchronous `jam-server` log shipping and graceful failure handling.

use std::{
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use jam_collector::{Collector, CollectorConfig, storage};
use jam_protocol::{
    AudioHeader, ControlMessage, MAX_PACKET_SIZE, NICKNAME_CAPACITY, STREAM_PRIMARY,
};
use jam_server::{Server, telemetry::TelemetryConfig};

fn encode_control(message: &ControlMessage) -> Vec<u8> {
    let mut buf = [0_u8; MAX_PACKET_SIZE];
    let len = message.to_bytes(&mut buf).unwrap();
    buf[..len].to_vec()
}

fn nickname(name: &str) -> ([u8; NICKNAME_CAPACITY], u8) {
    let mut buf = [0_u8; NICKNAME_CAPACITY];
    let bytes = name.as_bytes();
    buf[..bytes.len()].copy_from_slice(bytes);
    (buf, u8::try_from(bytes.len()).unwrap())
}

#[test]
fn ships_operational_logs_asynchronously_to_collector() {
    let storage_dir = std::env::temp_dir().join(format!(
        "jam-server-ship-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let collector_cfg = CollectorConfig {
        bind_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        storage_dir: storage_dir.clone(),
        beta_token: Some("beta-test-token".into()),
        trust_proxy_headers: false,
        ..CollectorConfig::default()
    };
    let collector = Collector::bind(collector_cfg).unwrap();
    let collector_addr = collector.local_addr().unwrap();
    let stop_collector = Arc::new(AtomicBool::new(false));
    let stop_c = Arc::clone(&stop_collector);
    let collector_thread = thread::spawn(move || collector.run(&stop_c).unwrap());

    let mut telemetry = TelemetryConfig::new(format!("http://{collector_addr}/api/v1/logs/server"));
    telemetry.beta_token = Some("beta-test-token".into());
    telemetry.flush_interval = Duration::from_secs(60);

    let mut server = Server::bind_with_telemetry("127.0.0.1:0", Some(telemetry)).unwrap();
    let server_addr = server.local_addr().unwrap();

    let client_a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let client_b = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (nick_a, len_a) = nickname("alpha");
    let (nick_b, len_b) = nickname("beta");
    client_a
        .send_to(
            &encode_control(&ControlMessage::JoinRequest {
                nickname: nick_a,
                nickname_len: len_a,
                local_addr: None,
            }),
            server_addr,
        )
        .unwrap();
    server.poll().unwrap();
    client_b
        .send_to(
            &encode_control(&ControlMessage::JoinRequest {
                nickname: nick_b,
                nickname_len: len_b,
                local_addr: None,
            }),
            server_addr,
        )
        .unwrap();
    server.poll().unwrap();

    // Send one valid audio packet from peer 1 -> relayed to peer 2.
    let mut audio_pkt = [0_u8; 42];
    AudioHeader::new(1, STREAM_PRIMARY, 1, 0)
        .to_bytes(&mut audio_pkt[..12])
        .unwrap();
    client_a.send_to(&audio_pkt, server_addr).unwrap();
    server.poll().unwrap();

    // Send a direct route update and a clean leave.
    client_a
        .send_to(
            &encode_control(&ControlMessage::DirectRouteUpdate {
                direct_peers_mask: 0b0000_0010,
            }),
            server_addr,
        )
        .unwrap();
    server.poll().unwrap();
    client_b
        .send_to(
            &encode_control(&ControlMessage::Leave { peer_id: 2 }),
            server_addr,
        )
        .unwrap();
    server.poll().unwrap();

    // Dropping the server flushes the final operational log asynchronously and joins the worker.
    drop(server);

    stop_collector.store(true, Ordering::Relaxed);
    collector_thread.join().unwrap();

    let date = storage::utc_date_string(SystemTime::now());
    let dir = storage_dir.join(date).join("127.0.0.1");
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1);
    let content = std::fs::read_to_string(&files[0]).unwrap();
    assert!(content.contains("joins=2"), "{content}");
    assert!(content.contains("leaves=1"), "{content}");
    assert!(content.contains("packets_forwarded=1"), "{content}");
    assert!(content.contains("direct_route_updates=1"), "{content}");
    assert!(content.contains("final_flush=true"), "{content}");

    let _ = std::fs::remove_dir_all(storage_dir);
}

#[test]
fn unreachable_collector_does_not_block_or_fail_udp_reflector() {
    // Bind and immediately drop a TCP listener to obtain an unused local port.
    let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut telemetry = TelemetryConfig::new(format!("http://{closed_port}/api/v1/logs/server"));
    telemetry.connect_timeout = Duration::from_millis(100);
    telemetry.io_timeout = Duration::from_millis(100);
    telemetry.max_retries = 0;

    let mut server = Server::bind_with_telemetry("127.0.0.1:0", Some(telemetry)).unwrap();
    let server_addr = server.local_addr().unwrap();

    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (nick, len) = nickname("solo");
    client
        .send_to(
            &encode_control(&ControlMessage::JoinRequest {
                nickname: nick,
                nickname_len: len,
                local_addr: None,
            }),
            server_addr,
        )
        .unwrap();

    let start = std::time::Instant::now();
    server.poll().unwrap();
    assert!(
        start.elapsed() < Duration::from_millis(50),
        "UDP poll must not block on unreachable collector"
    );
    drop(server);
}
