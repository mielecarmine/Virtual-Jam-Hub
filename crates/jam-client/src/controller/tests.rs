use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

struct FakeSession {
    drops: Arc<AtomicU32>,
    fail: bool,
}
impl ActiveSession for FakeSession {
    #[cfg(feature = "telemetry-log")]
    fn telemetry_update(&mut self) -> Option<crate::network::Telemetry> {
        Some(crate::network::Telemetry {
            rtt_ms: Some(12.0),
            ..crate::network::Telemetry::default()
        })
    }
    fn poll(&mut self) -> io::Result<bool> {
        if self.fail {
            Err(io::Error::other("device unplugged"))
        } else {
            Ok(true)
        }
    }
}
impl Drop for FakeSession {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

fn wait_for(events: &Receiver<Event>, predicate: impl Fn(&Event) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if predicate(&event) {
            return;
        }
    }
}

#[test]
fn reconnects_releases_sessions_and_persists_selection_on_shutdown() {
    let path = crate::setup::tests::temporary_path("lifecycle");
    let drops = Arc::new(AtomicU32::new(0));
    let (commands, receiver) = crossbeam_channel::unbounded();
    let (sender, events) = crossbeam_channel::unbounded();
    thread::scope(|scope| {
        let drops = &drops;
        let path = &path;
        let worker = scope.spawn(move || {
            control_loop(
                &receiver,
                &sender,
                Preferences::default(),
                path,
                |_| {
                    Ok(FakeSession {
                        drops: Arc::clone(drops),
                        fail: false,
                    })
                },
                |_| Ok(Catalog::default()),
            );
        });
        wait_for(&events, |e| matches!(e, Event::Catalog(Ok(_))));
        for cycle in 0..3 {
            let preferences = Preferences {
                nickname: format!("guitar-{cycle}"),
                buffer_frames: 64,
                ..Preferences::default()
            };
            commands
                .send(Command::SelectDevice(preferences.clone()))
                .unwrap();
            commands.send(Command::Connect(preferences)).unwrap();
            wait_for(&events, |e| matches!(e, Event::Phase(Phase::Connected)));
            commands.send(Command::Disconnect).unwrap();
            wait_for(&events, |e| matches!(e, Event::Phase(Phase::Disconnected)));
            assert_eq!(drops.load(Ordering::Relaxed), cycle + 1);
        }
        commands
            .send(Command::Connect(Preferences::default()))
            .unwrap();
        wait_for(&events, |e| matches!(e, Event::Phase(Phase::Connected)));
        let final_preferences = Preferences {
            nickname: "last edit before closing".into(),
            ..Preferences::default()
        };
        commands
            .send(Command::Shutdown(Some(final_preferences.clone())))
            .unwrap();
        worker.join().unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 4);
        assert_eq!(Preferences::load(path).unwrap(), final_preferences);
        #[cfg(feature = "telemetry-log")]
        assert_logs(path, 4);
    });
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn startup_and_runtime_errors_allow_another_connection() {
    let path = crate::setup::tests::temporary_path("recovery");
    let drops = Arc::new(AtomicU32::new(0));
    let (commands, receiver) = crossbeam_channel::unbounded();
    let (sender, events) = crossbeam_channel::unbounded();
    thread::scope(|scope| {
        let drops = &drops;
        let path = &path;
        let worker = scope.spawn(move || {
            let mut attempt = 0;
            control_loop(
                &receiver,
                &sender,
                Preferences::default(),
                path,
                |_| {
                    attempt += 1;
                    if attempt == 1 {
                        return Err(io::Error::other("busy device"));
                    }
                    Ok(FakeSession {
                        drops: Arc::clone(drops),
                        fail: attempt == 2,
                    })
                },
                |_| Ok(Catalog::default()),
            );
        });
        for expected in ["busy device", "device unplugged"] {
            commands
                .send(Command::Connect(Preferences::default()))
                .unwrap();
            wait_for(
                &events,
                |e| matches!(e, Event::Error(error) if error == expected),
            );
            wait_for(&events, |e| matches!(e, Event::Phase(Phase::Disconnected)));
        }
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        commands
            .send(Command::Connect(Preferences::default()))
            .unwrap();
        wait_for(&events, |e| matches!(e, Event::Phase(Phase::Connected)));
        // Losing the UI channel must also release active streams and the socket.
        drop(commands);
        worker.join().unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        #[cfg(feature = "telemetry-log")]
        assert_logs(path, 1);
    });
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[cfg(feature = "telemetry-log")]
fn assert_logs(path: &std::path::Path, expected: usize) {
    let logs: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "txt"))
        .collect();
    assert_eq!(logs.len(), expected);
    for path in logs {
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text.matches("Summary").count(), 1);
        assert!(text.contains("\t12.000\t12.000\t12.000\t12.000"));
    }
}

#[cfg(feature = "telemetry-log")]
#[test]
fn runtime_error_finalizes_log_and_log_open_failure_keeps_session_connected() {
    struct RuntimeFailure(u32);
    impl ActiveSession for RuntimeFailure {
        fn poll(&mut self) -> io::Result<bool> {
            self.0 += 1;
            if self.0 == 2 {
                Err(io::Error::other("runtime failure"))
            } else {
                Ok(true)
            }
        }
        fn telemetry_update(&mut self) -> Option<crate::network::Telemetry> {
            Some(crate::network::Telemetry {
                rtt_ms: Some(12.0),
                ..crate::network::Telemetry::default()
            })
        }
    }
    let path = crate::setup::tests::temporary_path("telemetry-runtime");
    let (commands, receiver) = crossbeam_channel::unbounded();
    let (sender, events) = crossbeam_channel::unbounded();
    thread::scope(|scope| {
        let path = &path;
        let worker = scope.spawn(move || {
            control_loop(
                &receiver,
                &sender,
                Preferences::default(),
                path,
                |_| Ok(RuntimeFailure(0)),
                |_| Ok(Catalog::default()),
            );
        });
        commands
            .send(Command::Connect(Preferences::default()))
            .unwrap();
        wait_for(
            &events,
            |event| matches!(event, Event::Error(error) if error == "runtime failure"),
        );
        wait_for(&events, |event| {
            matches!(event, Event::Phase(Phase::Disconnected))
        });
        commands.send(Command::Shutdown(None)).unwrap();
        worker.join().unwrap();
    });
    assert_logs(&path, 1);
    // A regular file cannot be used as the logger/config parent directory.
    let blocked = path.join("client.toml");
    let (commands, receiver) = crossbeam_channel::unbounded();
    let (sender, events) = crossbeam_channel::unbounded();
    let drops = Arc::new(AtomicU32::new(0));
    thread::scope(|scope| {
        let drops = &drops;
        let worker = scope.spawn(move || {
            control_loop(
                &receiver,
                &sender,
                Preferences::default(),
                &blocked,
                |_| {
                    Ok(FakeSession {
                        drops: Arc::clone(drops),
                        fail: false,
                    })
                },
                |_| Ok(Catalog::default()),
            );
        });
        commands
            .send(Command::Connect(Preferences::default()))
            .unwrap();
        wait_for(
            &events,
            |event| matches!(event, Event::Error(error) if error.starts_with("Telemetry log:")),
        );
        wait_for(&events, |event| {
            matches!(event, Event::Phase(Phase::Connected))
        });
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        commands.send(Command::Shutdown(None)).unwrap();
        worker.join().unwrap();
    });
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn active_wait_preserves_commands_and_reports_channel_shutdown() {
    let (sender, receiver) = crossbeam_channel::unbounded();
    for active in [false, true] {
        sender.send(Command::Disconnect).unwrap();
        assert!(matches!(
            receive_command(&receiver, Duration::from_micros(250), active),
            Ok(Command::Disconnect)
        ));
        assert!(matches!(
            receive_command(&receiver, Duration::from_micros(250), active),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        ));
    }
    drop(sender);
    assert!(matches!(
        receive_command(&receiver, Duration::from_micros(250), true),
        Err(crossbeam_channel::RecvTimeoutError::Disconnected)
    ));
}
