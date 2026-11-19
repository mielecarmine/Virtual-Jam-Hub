//! Bounded handoff: sampling never formats, allocates, wakes a thread, or waits for disk.
use super::{AudioInfo, Preferences, SessionLog, Telemetry};
use crate::controller::Event;
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    io::{self, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

// 32 seconds at the normal 500 ms sample interval. Drop newest on overload.
const CAPACITY: usize = 64;

#[derive(Clone, Copy)]
struct Sample {
    elapsed: f64,
    peers: u32,
    metrics: Telemetry,
    xruns: [u32; 2],
}

pub struct AsyncSessionLog {
    samples: Producer<Sample>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU32>,
    started: Instant,
    worker: Option<JoinHandle<()>>,
}

impl AsyncSessionLog {
    pub fn start(
        config: &Path,
        preferences: &Preferences,
        audio: AudioInfo,
        events: crossbeam_channel::Sender<Event>,
    ) -> io::Result<Self> {
        let config = config.to_path_buf();
        let preferences = preferences.clone();
        Self::spawn(
            move || SessionLog::start(&config, &preferences, audio),
            events,
        )
    }

    fn spawn(
        open: impl FnOnce() -> io::Result<SessionLog> + Send + 'static,
        events: crossbeam_channel::Sender<Event>,
    ) -> io::Result<Self> {
        let (samples, receiver) = RingBuffer::new(CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU32::new(0));
        let stopping = Arc::clone(&stop);
        let losses = Arc::clone(&dropped);
        let started = Instant::now();
        let worker = thread::Builder::new()
            .name("jam-telemetry".into())
            .spawn(move || {
                #[cfg(target_os = "macos")]
                if let Err(error) = crate::scheduling::logger() {
                    let _ = events.send(Event::Error(format!("Telemetry scheduling: {error}")));
                }
                let result = open().and_then(|mut log| {
                    drain(&mut log, receiver, &stopping)?;
                    writeln!(
                        log.writer,
                        "telemetry_dropped_samples={}",
                        losses.load(Ordering::Relaxed)
                    )
                });
                if let Err(error) = result {
                    let _ = events.send(Event::Error(format!("Telemetry log: {error}")));
                }
            })?;
        Ok(Self {
            samples,
            stop,
            dropped,
            started,
            worker: Some(worker),
        })
    }

    pub fn record(&mut self, peers: u32, metrics: Telemetry, xruns: [u32; 2]) {
        if self
            .samples
            .push(Sample {
                elapsed: self.started.elapsed().as_secs_f64(),
                peers,
                metrics,
                xruns,
            })
            .is_err()
        {
            // Only this producer writes the counter; the logger reads after shutdown.
            let count = self.dropped.load(Ordering::Relaxed);
            self.dropped
                .store(count.saturating_add(1), Ordering::Relaxed);
        }
    }
}

fn drain(log: &mut SessionLog, mut samples: Consumer<Sample>, stop: &AtomicBool) -> io::Result<()> {
    loop {
        // Acquire shutdown before draining so every accepted final sample is included.
        let stopping = stop.load(Ordering::Acquire);
        while let Ok(sample) = samples.pop() {
            log.record_at(sample.elapsed, sample.peers, sample.metrics, sample.xruns)?;
        }
        if stopping {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

impl Drop for AsyncSessionLog {
    fn drop(&mut self) {
        // Controller must stop audio/network before joining this disk-owning thread.
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_logger_cannot_block_sampling_and_shutdown_drains_accepted_rows() {
        let path = crate::setup::tests::temporary_path("telemetry-stalled");
        let directory = path.parent().unwrap().to_path_buf();
        let (resume, stalled) = crossbeam_channel::bounded(1);
        let (events, errors) = crossbeam_channel::unbounded();
        let mut log = AsyncSessionLog::spawn(
            move || {
                assert_eq!(thread::current().name(), Some("jam-telemetry"));
                stalled.recv_timeout(Duration::from_secs(3)).unwrap();
                SessionLog::start(&path, &Preferences::default(), AudioInfo::default())
            },
            events,
        )
        .unwrap();
        crate::audio::tests::allocation::assert_no_allocations(|| {
            for _ in 0..CAPACITY + 7 {
                log.record(
                    2,
                    Telemetry {
                        rtt_ms: Some(12.0),
                        ..Telemetry::default()
                    },
                    [3, 4],
                );
            }
        });
        assert_eq!(log.dropped.load(Ordering::Relaxed), 7);
        // Sampling reached this point while the logger was completely unable to progress.
        assert_eq!(log.samples.slots(), 0);
        resume.send(()).unwrap();
        drop(log);
        assert!(errors.try_recv().is_err());
        let file = std::fs::read_dir(&directory)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let text = std::fs::read_to_string(file).unwrap();
        assert!(text.contains("telemetry_dropped_samples=7"));
        assert!(text.contains("rtt_ms\t64\t12.000\t12.000\t12.000\t12.000"));
        assert_eq!(text.matches("Summary").count(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn writer_failure_is_reported_asynchronously_without_stopping_producer() {
        let path = crate::setup::tests::temporary_path("telemetry-write-failure");
        let directory = path.parent().unwrap().to_path_buf();
        let (events, errors) = crossbeam_channel::unbounded();
        let mut log = AsyncSessionLog::spawn(
            move || {
                let mut log =
                    SessionLog::start(&path, &Preferences::default(), AudioInfo::default())?;
                std::fs::write(&path, "read only handle")?;
                log.writer = std::io::BufWriter::new(std::fs::File::open(path)?);
                Ok(log)
            },
            events,
        )
        .unwrap();
        log.record(0, Telemetry::default(), [0; 2]);
        assert!(
            matches!(errors.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Error(error) if error.starts_with("Telemetry log:"))
        );
        // Even an abandoned consumer is a nonblocking handoff.
        for _ in 0..CAPACITY + 1 {
            log.record(0, Telemetry::default(), [0; 2]);
        }
        drop(log);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
