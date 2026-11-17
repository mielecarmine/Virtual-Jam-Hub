use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn aggregates_nearest_rank_and_excludes_missing_or_invalid_values() {
    assert_eq!(summarize(std::iter::empty()), None);
    assert_eq!(summarize([f64::NAN, f64::INFINITY].into_iter()), None);
    let summary = summarize((1..=20).rev().map(f64::from)).unwrap();
    assert_eq!(
        summary,
        Summary {
            count: 20,
            min: 1.0,
            avg: 10.5,
            p95: 19.0,
            max: 20.0
        }
    );
    let samples = [
        Telemetry::default(),
        Telemetry {
            rtt_ms: Some(4.0),
            ..Telemetry::default()
        },
    ];
    assert_eq!(
        summarize(samples.iter().filter_map(|s| s.rtt_ms))
            .unwrap()
            .count,
        1
    );
    assert_eq!(summarize([7.0].into_iter()).unwrap().p95, 7.0);
}

#[test]
fn validates_options_and_defaults() {
    let parse = |args: &[&str]| Options::parse(args.iter().map(|s| (*s).to_owned()));
    let defaults = parse(&["127.0.0.1:7777"]).unwrap().unwrap();
    assert_eq!(defaults.peers, 2);
    assert_eq!(defaults.seconds, 10);
    assert!(defaults.output.is_none());
    assert!(parse(&["--help"]).unwrap().is_none());
    for args in [
        vec![],
        vec!["invalid"],
        vec!["127.0.0.1:7777", "--peers", "1"],
        vec!["127.0.0.1:7777", "--peers", "9"],
        vec!["127.0.0.1:7777", "--seconds", "0"],
        vec!["127.0.0.1:7777", "--seconds", "NaN"],
        vec!["127.0.0.1:7777", "--output", "report.csv"],
        vec!["127.0.0.1:7777", "--unknown", "2"],
        vec!["127.0.0.1:7777", "--peers"],
    ] {
        assert!(parse(&args).is_err(), "{args:?}");
    }
    let options = parse(&[
        "[::1]:7777",
        "--peers",
        "3",
        "--seconds",
        "2",
        "--output",
        "report.txt",
    ])
    .unwrap()
    .unwrap();
    assert_eq!(options.peers, 3);
    assert_eq!(options.output, Some(PathBuf::from("report.txt")));
}

#[test]
fn localhost_three_peers_collect_telemetry_and_decode_opus() {
    let mut server = jam_server::Server::bind("127.0.0.1:0").unwrap();
    let options = Options {
        server: server.local_addr().unwrap(),
        peers: 3,
        seconds: 2,
        output: None,
    };
    let stop = AtomicBool::new(false);
    let text = thread::scope(|scope| {
        let worker = scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                server.poll().unwrap();
            }
        });
        let result = run(&options);
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        result.unwrap()
    });
    for index in 1..=3 {
        let line = text
            .lines()
            .find(|line| line.starts_with(&format!("Peer {index}:")))
            .unwrap();
        assert!(line.ends_with("decoded audio: true"), "{text}");
        let count: usize = line.split_whitespace().nth(2).unwrap().parse().unwrap();
        assert!(count >= 3, "{text}");
    }
    for name in [
        "Ping RTT (ms)",
        "Jitter (ms)",
        "Packet Loss (%)",
        "Jitter Buffer (ms)",
    ] {
        let line = text.lines().find(|line| line.starts_with(name)).unwrap();
        assert!(!line.contains("N/A"), "{text}");
    }
}
