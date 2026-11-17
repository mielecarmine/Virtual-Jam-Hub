use super::*;

#[test]
fn writes_flushes_and_summarizes_entire_session_without_overwriting() {
    let path = crate::setup::tests::temporary_path("telemetry");
    let preferences = Preferences {
        host: Some("test backend".into()),
        input: Some("input\nname".into()),
        output: Some("output".into()),
        ..Preferences::default()
    };
    let mut log = SessionLog::start(&path, &preferences).unwrap();
    let files = || {
        std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>()
    };
    let first = files().pop().unwrap();
    for value in 1..=20 {
        log.record(
            2,
            Telemetry {
                rtt_ms: Some(f64::from(value)),
                jitter_ms: 2.0,
                loss_percent: 3.0,
                buffer_ms: 4.0,
            },
            [5, 6],
        )
        .unwrap();
    }
    log.record(0, Telemetry::default(), [7, 8]).unwrap();
    let live = std::fs::read_to_string(&first).unwrap();
    assert!(live.contains("backend=Some(\"test backend\")"));
    assert!(live.contains("input=Some(\"input\\nname\")"));
    assert!(live.contains("buffer_frames=128"));
    assert!(live.contains("server=\"127.0.0.1:7777\""));
    assert!(live.contains("elapsed_s\tremote_peers\trtt_ms"));
    assert!(live.contains(concat!("version=", env!("CARGO_PKG_VERSION"))));
    assert!(live.contains(&format!("os={}", std::env::consts::OS)));
    let rows: Vec<_> = live
        .lines()
        .skip_while(|line| !line.starts_with("elapsed_s"))
        .skip(1)
        .collect();
    assert_eq!(rows.len(), 21);
    for row in rows {
        let columns: Vec<_> = row.split('\t').collect();
        assert_eq!(columns.len(), 8);
        assert!(columns[0].parse::<f64>().unwrap() >= 0.0);
    }
    assert!(live.contains("\t0.000\tN/A\t0.000"));
    assert!(!live.contains("Summary"));
    drop(log);
    let finished = std::fs::read_to_string(&first).unwrap();
    assert!(finished.contains("rtt_ms\t20\t1.000\t10.500\t19.000\t20.000"));
    assert!(finished.contains("output_underflows\t21\t6.000"));
    drop(SessionLog::start(&path, &preferences).unwrap());
    assert_eq!(files().len(), 2);
    assert_eq!(std::fs::read_to_string(&first).unwrap(), finished);
    let second = files().into_iter().find(|file| file != &first).unwrap();
    assert!(
        std::fs::read_to_string(second)
            .unwrap()
            .contains("rtt_ms\t0\tN/A\tN/A\tN/A\tN/A")
    );
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn invalid_directory_returns_error() {
    let path = crate::setup::tests::temporary_path("telemetry-failure");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "not a directory").unwrap();
    assert!(SessionLog::start(&path.join("client.toml"), &Preferences::default()).is_err());
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
