//! Asynchronous, non-blocking operational log shipper for `jam-server`.
//!
//! The UDP reflector thread only performs `try_send` of a fixed-size `Copy` event
//! into a bounded channel. All formatting, HTTP/HTTPS I/O, retries, and timeouts
//! execute exclusively on the dedicated `jam-server-telemetry` background thread.

use std::{
    fmt::Write as _,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Default central collector endpoint used only when `--share-logs` / `JAM_SHARE_LOGS` is enabled.
pub const DEFAULT_BETA_SERVER_ENDPOINT: &str = "https://185.198.245.40/api/v1/logs/server";

const CHANNEL_CAPACITY: usize = 2048;
const MAX_BUFFERED_EVENT_LINES: usize = 1024;
const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_MAX_RETRIES: usize = 2;

/// Configuration for optional server log shipping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelemetryConfig {
    pub endpoint: String,
    pub beta_token: Option<String>,
    pub flush_interval: Duration,
    pub connect_timeout: Duration,
    pub io_timeout: Duration,
    pub max_retries: usize,
}

impl TelemetryConfig {
    /// Creates a configuration targeting `endpoint` with safe beta defaults.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            beta_token: None,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            io_timeout: DEFAULT_IO_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

/// Fixed-size, zero-allocation event emitted by the UDP reflector thread.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ServerEvent {
    Join {
        peer_id: u8,
        active_peers: u8,
    },
    Leave {
        peer_id: u8,
        active_peers: u8,
    },
    WatchdogTimeout {
        expired_count: u8,
        active_peers: u8,
    },
    DirectRouteUpdate {
        peer_id: u8,
        direct_peers_mask: u8,
    },
    AudioForwarded {
        relayed_destinations: u8,
        direct_bypassed: u8,
    },
    PacketDropped,
    SocketError,
}

#[derive(Clone, Copy)]
struct TimedEvent {
    elapsed_ms: u64,
    event: ServerEvent,
}

/// Non-blocking handle held by `Server`. Dropping it triggers a final best-effort flush.
pub(crate) struct TelemetryShipper {
    sender: Option<SyncSender<TimedEvent>>,
    started: Instant,
    dropped_events: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
}

impl TelemetryShipper {
    pub(crate) fn start(config: TelemetryConfig) -> Self {
        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let started = Instant::now();
        let dropped_events = Arc::new(AtomicU64::new(0));
        let worker_dropped = Arc::clone(&dropped_events);
        let session_id = generate_session_id();

        let worker = thread::Builder::new()
            .name("jam-server-telemetry".into())
            .spawn(move || {
                run_worker(&config, &session_id, started, &receiver, &worker_dropped);
            })
            .ok();

        Self {
            sender: Some(sender),
            started,
            dropped_events,
            worker,
        }
    }

    /// Enqueues an event without blocking or allocating. Drops newest on backpressure.
    pub(crate) fn record(&self, event: ServerEvent) {
        let Some(sender) = &self.sender else {
            return;
        };
        let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if matches!(
            sender.try_send(TimedEvent { elapsed_ms, event }),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_))
        ) {
            self.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for TelemetryShipper {
    fn drop(&mut self) {
        // Closing the channel signals the worker to drain remaining events and flush.
        drop(self.sender.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Default)]
struct AggregatedStats {
    joins: u64,
    leaves: u64,
    watchdog_timeouts: u64,
    direct_route_updates: u64,
    packets_forwarded: u64,
    packets_direct_bypassed: u64,
    packets_dropped: u64,
    socket_errors: u64,
    peak_peers: u8,
    active_peers: u8,
    has_activity: bool,
    event_lines: Vec<String>,
}

impl AggregatedStats {
    fn apply(&mut self, item: TimedEvent) {
        self.has_activity = true;
        match item.event {
            ServerEvent::Join {
                peer_id,
                active_peers,
            } => {
                self.joins = self.joins.saturating_add(1);
                self.active_peers = active_peers;
                self.peak_peers = self.peak_peers.max(active_peers);
                self.push_line(format!(
                    "t_ms={} event=join peer_id={peer_id} active_peers={active_peers}",
                    item.elapsed_ms
                ));
            }
            ServerEvent::Leave {
                peer_id,
                active_peers,
            } => {
                self.leaves = self.leaves.saturating_add(1);
                self.active_peers = active_peers;
                self.push_line(format!(
                    "t_ms={} event=leave peer_id={peer_id} active_peers={active_peers}",
                    item.elapsed_ms
                ));
            }
            ServerEvent::WatchdogTimeout {
                expired_count,
                active_peers,
            } => {
                self.watchdog_timeouts = self
                    .watchdog_timeouts
                    .saturating_add(u64::from(expired_count));
                self.active_peers = active_peers;
                self.push_line(format!(
                    "t_ms={} event=watchdog_timeout expired={expired_count} active_peers={active_peers}",
                    item.elapsed_ms
                ));
            }
            ServerEvent::DirectRouteUpdate {
                peer_id,
                direct_peers_mask,
            } => {
                self.direct_route_updates = self.direct_route_updates.saturating_add(1);
                self.push_line(format!(
                    "t_ms={} event=direct_route_update peer_id={peer_id} direct_peers_mask={direct_peers_mask:#04x}",
                    item.elapsed_ms
                ));
            }
            ServerEvent::AudioForwarded {
                relayed_destinations,
                direct_bypassed,
            } => {
                self.packets_forwarded = self
                    .packets_forwarded
                    .saturating_add(u64::from(relayed_destinations));
                self.packets_direct_bypassed = self
                    .packets_direct_bypassed
                    .saturating_add(u64::from(direct_bypassed));
            }
            ServerEvent::PacketDropped => {
                self.packets_dropped = self.packets_dropped.saturating_add(1);
            }
            ServerEvent::SocketError => {
                self.socket_errors = self.socket_errors.saturating_add(1);
                self.push_line(format!("t_ms={} event=socket_error", item.elapsed_ms));
            }
        }
    }

    fn push_line(&mut self, line: String) {
        if self.event_lines.len() < MAX_BUFFERED_EVENT_LINES {
            self.event_lines.push(line);
        }
    }

    fn render_payload(
        &self,
        session_id: &str,
        duration_secs: f64,
        dropped_telemetry_events: u64,
        final_flush: bool,
    ) -> String {
        let mut out = String::with_capacity(512 + self.event_lines.len() * 64);
        let _ = writeln!(out, "Virtual Jam server operational log");
        let _ = writeln!(out, "version={}", env!("CARGO_PKG_VERSION"));
        let _ = writeln!(out, "os={}", std::env::consts::OS);
        let _ = writeln!(out, "session_id={session_id}");
        let _ = writeln!(out, "final_flush={final_flush}");
        let _ = writeln!(out, "duration_secs={duration_secs:.3}");
        let _ = writeln!(out, "active_peers={}", self.active_peers);
        let _ = writeln!(out, "peak_peers={}", self.peak_peers);
        let _ = writeln!(out, "joins={}", self.joins);
        let _ = writeln!(out, "leaves={}", self.leaves);
        let _ = writeln!(out, "watchdog_timeouts={}", self.watchdog_timeouts);
        let _ = writeln!(out, "direct_route_updates={}", self.direct_route_updates);
        let _ = writeln!(out, "packets_forwarded={}", self.packets_forwarded);
        let _ = writeln!(
            out,
            "packets_direct_bypassed={}",
            self.packets_direct_bypassed
        );
        let _ = writeln!(out, "packets_dropped={}", self.packets_dropped);
        let _ = writeln!(out, "socket_errors={}", self.socket_errors);
        let _ = writeln!(out, "telemetry_dropped_events={dropped_telemetry_events}");
        if !self.event_lines.is_empty() {
            let _ = writeln!(out, "--- events ---");
            for line in &self.event_lines {
                let _ = writeln!(out, "{line}");
            }
        }
        out
    }
}

fn run_worker(
    config: &TelemetryConfig,
    session_id: &str,
    started: Instant,
    receiver: &mpsc::Receiver<TimedEvent>,
    dropped_events: &AtomicU64,
) {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(config.connect_timeout)
        .timeout_read(config.io_timeout)
        .timeout_write(config.io_timeout)
        .build();
    let mut stats = AggregatedStats::default();
    let mut last_flush = Instant::now();
    let poll_step = config.flush_interval.min(Duration::from_millis(250));

    loop {
        match receiver.recv_timeout(poll_step) {
            Ok(item) => {
                stats.apply(item);
                while let Ok(more) = receiver.try_recv() {
                    stats.apply(more);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if stats.has_activity {
                    let payload = stats.render_payload(
                        session_id,
                        started.elapsed().as_secs_f64(),
                        dropped_events.load(Ordering::Relaxed),
                        true,
                    );
                    ship_with_retries(&agent, config, session_id, &payload);
                }
                return;
            }
        }

        if stats.has_activity && last_flush.elapsed() >= config.flush_interval {
            let payload = stats.render_payload(
                session_id,
                started.elapsed().as_secs_f64(),
                dropped_events.load(Ordering::Relaxed),
                false,
            );
            ship_with_retries(&agent, config, session_id, &payload);
            stats.event_lines.clear();
            stats.has_activity = false;
            last_flush = Instant::now();
        }
    }
}

fn ship_with_retries(
    agent: &ureq::Agent,
    config: &TelemetryConfig,
    session_id: &str,
    payload: &str,
) {
    let attempts = config.max_retries.saturating_add(1);
    for attempt in 0..attempts {
        let mut request = agent
            .post(&config.endpoint)
            .set("Content-Type", "text/plain; charset=utf-8")
            .set("X-Jam-Log-Kind", "server")
            .set("X-Jam-Session-Id", session_id);
        if let Some(token) = &config.beta_token {
            request = request.set("X-Jam-Beta-Key", token);
        }
        match request.send_string(payload) {
            Ok(_) => return,
            Err(error) => {
                if attempt + 1 < attempts {
                    let backoff_ms = 200_u64.saturating_mul(1_u64 << attempt.min(4));
                    thread::sleep(Duration::from_millis(backoff_ms));
                } else {
                    eprintln!("Server telemetry upload skipped ({error})");
                }
            }
        }
    }
}

fn generate_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("s{}-{:x}", std::process::id(), nanos & 0xffff_ffff)
}
