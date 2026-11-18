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
    let mut log = SessionLog::start(&path, &preferences, AudioInfo::default()).unwrap();
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
                ..Telemetry::default()
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
        assert_eq!(columns.len(), NAMES.len() + 1);
        assert!(columns[0].parse::<f64>().unwrap() >= 0.0);
    }
    assert!(live.contains("\t0.000\tN/A\t0.000"));
    assert!(!live.contains("Summary"));
    drop(log);
    let finished = std::fs::read_to_string(&first).unwrap();
    assert!(finished.contains("rtt_ms\t20\t1.000\t10.500\t19.000\t20.000"));
    assert!(finished.contains("output_underflows\t21\t6.000"));
    drop(SessionLog::start(&path, &preferences, AudioInfo::default()).unwrap());
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
    assert!(
        SessionLog::start(
            &path.join("client.toml"),
            &Preferences::default(),
            AudioInfo::default()
        )
        .is_err()
    );
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn extended_columns_preserve_units_peaks_counters_and_device_configuration() {
    let path = crate::setup::tests::temporary_path("telemetry-diagnostics");
    let audio = AudioInfo {
        input_channels: 1,
        output_channels: 2,
        input_sample_rate: 48_000,
        output_sample_rate: 48_000,
        last_output_frames: 256,
    };
    let mut log = SessionLog::start(&path, &Preferences::default(), audio).unwrap();
    let metrics = Telemetry {
        rtt_ms: Some(23.0),
        rtt_max_ms: Some(81.0),
        worker_max_gap_ms: 15.625,
        jitter_ms: 2.0,
        loss_percent: 0.0,
        buffer_ms: 7.5,
        audio: crate::audio::AudioTelemetry {
            input_callbacks: 101,
            output_callbacks: 102,
            not_ready_samples: 103,
            align_dropped_samples: 104,
            last_output_frames: 256,
        },
        diagnostics: crate::jitter::Diagnostics {
            late_dropped: 10,
            late_dropped_far: 3,
            plc_frames: 12,
            skipped_frames: 13,
            rx_push_dropped: 14,
            resyncs: 15,
        },
    };
    log.record(2, metrics, [5, 6]).unwrap();
    log.record(
        0,
        Telemetry {
            rtt_max_ms: None,
            worker_max_gap_ms: 1.0,
            ..metrics
        },
        [5, 6],
    )
    .unwrap();
    let file = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let live = std::fs::read_to_string(&file).unwrap();
    assert!(live.contains("input_channels=1\noutput_channels=2"));
    assert!(live.contains("input_sample_rate=48000\noutput_sample_rate=48000"));
    assert!(live.contains("last_output_frames=256"));
    assert!(live.contains("late_dropped_far (>=64 frames)"));
    let expected = [
        2.0, 23.0, 2.0, 0.0, 7.5, 5.0, 6.0, 101.0, 102.0, 103.0, 104.0, 256.0, 10.0, 3.0, 12.0,
        13.0, 14.0, 15.0, 15.625, 81.0,
    ];
    let row = live
        .lines()
        .skip_while(|l| !l.starts_with("elapsed_s"))
        .nth(1)
        .unwrap();
    for (column, value) in row.split('\t').skip(1).zip(expected) {
        assert_eq!(column.parse::<f64>().unwrap(), value);
    }
    drop(log);
    let text = std::fs::read_to_string(file).unwrap();
    for (name, value) in NAMES.iter().zip(expected).skip(7) {
        if *name != "worker_max_gap_ms" && *name != "rtt_max_ms" {
            assert!(
                text.contains(&format!(
                    "{name}\t2\t{value:.3}\t{value:.3}\t{value:.3}\t{value:.3}"
                )),
                "{name}"
            );
        }
    }
    assert!(text.contains("rtt_max_ms\t1\t81.000\t81.000\t81.000\t81.000"));
    assert!(text.contains("worker_max_gap_ms\t2\t1.000\t8.312\t15.625\t15.625"));
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn first_periodic_sample_supplies_callback_size_if_not_observed_at_connection() {
    let path = crate::setup::tests::temporary_path("telemetry-first-callback");
    let mut log = SessionLog::start(&path, &Preferences::default(), AudioInfo::default()).unwrap();
    log.record(
        0,
        Telemetry {
            audio: crate::audio::AudioTelemetry {
                last_output_frames: 64,
                ..Default::default()
            },
            ..Default::default()
        },
        [0; 2],
    )
    .unwrap();
    drop(log);
    let file = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let text = std::fs::read_to_string(file).unwrap();
    assert!(text.contains("last_output_frames=64"));
    assert!(text.find("last_output_frames=64").unwrap() < text.find("elapsed_s").unwrap());
    assert_eq!(text.matches("elapsed_s").count(), 1);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
