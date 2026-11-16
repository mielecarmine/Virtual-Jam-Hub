use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

struct FakeSession {
    drops: Arc<AtomicU32>,
    fail: bool,
}
impl ActiveSession for FakeSession {
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
    });
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
