//! Compact end-of-session telemetry report accumulator and asynchronous uploader.
//!
//! Runs strictly outside the real-time audio callbacks (controller or CLI thread for
//! accumulation, dedicated short-lived thread for HTTPS upload on session end).

use std::{
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::network::Telemetry;

/// Default central collector endpoint used only when explicit opt-in consent is enabled.
pub const DEFAULT_BETA_CLIENT_ENDPOINT: &str = "https://185.198.245.40/api/v1/logs/client";

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_MAX_RETRIES: usize = 2;

/// Target endpoint and optional beta key for uploading a client end-of-session report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportTarget {
    pub endpoint: String,
    pub beta_token: Option<String>,
}

/// Non-sensitive technical metadata describing the client session environment.
#[derive(Clone, Debug)]
pub struct SessionMetadata {
    pub audio_backend: String,
    pub sample_rate_hz: u32,
    pub buffer_frames: u32,
    pub peer_id: Option<u8>,
}

/// Compact JSON-serializable summary emitted once at the end of a session.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionReport {
    pub schema_version: u8,
    pub client_version: String,
    pub os: String,
    pub arch: String,
    pub audio_backend: String,
    pub sample_rate_hz: u32,
    pub buffer_frames: u32,
    pub connection_mode: String,
    pub peer_id: Option<u8>,
    pub duration_secs: f64,
    pub samples: u32,
    pub max_remote_peers: u32,
    pub p2p_samples: u32,
    pub sfu_only_samples: u32,
    pub rtt_avg_ms: Option<f64>,
    pub rtt_p95_ms: Option<f64>,
    pub rtt_max_ms: Option<f64>,
    pub route_rtt_avg_ms: Option<f64>,
    pub route_rtt_p95_ms: Option<f64>,
    pub route_rtt_max_ms: Option<f64>,
    pub jitter_avg_ms: Option<f64>,
    pub jitter_max_ms: Option<f64>,
    pub packet_loss_avg_percent: Option<f64>,
    pub packet_loss_max_percent: Option<f64>,
    pub total_one_way_avg_ms: Option<f64>,
    pub total_one_way_p95_ms: Option<f64>,
    pub input_overflows: u32,
    pub output_underflows: u32,
    pub jitter_late_dropped: u32,
    pub jitter_late_dropped_far: u32,
    pub jitter_skipped_frames: u32,
    pub rx_push_dropped: u32,
    pub resyncs: u32,
    pub plc_frames: u32,
}

/// Accumulates 500 ms telemetry snapshots on the non-RT controller/CLI thread.
pub struct SessionReportAccumulator {
    metadata: SessionMetadata,
    started: Instant,
    samples: u32,
    max_remote_peers: u32,
    p2p_samples: u32,
    sfu_only_samples: u32,
    rtt_ms: Vec<f64>,
    route_rtt_ms: Vec<f64>,
    jitter_ms: Vec<f64>,
    loss_percent: Vec<f64>,
    total_one_way_ms: Vec<f64>,
    input_overflows: u32,
    output_underflows: u32,
    jitter_late_dropped: u32,
    jitter_late_dropped_far: u32,
    jitter_skipped_frames: u32,
    rx_push_dropped: u32,
    resyncs: u32,
    plc_frames: u32,
}

impl SessionReportAccumulator {
    #[must_use]
    pub fn start(metadata: SessionMetadata) -> Self {
        Self {
            metadata,
            started: Instant::now(),
            samples: 0,
            max_remote_peers: 0,
            p2p_samples: 0,
            sfu_only_samples: 0,
            rtt_ms: Vec::new(),
            route_rtt_ms: Vec::new(),
            jitter_ms: Vec::new(),
            loss_percent: Vec::new(),
            total_one_way_ms: Vec::new(),
            input_overflows: 0,
            output_underflows: 0,
            jitter_late_dropped: 0,
            jitter_late_dropped_far: 0,
            jitter_skipped_frames: 0,
            rx_push_dropped: 0,
            resyncs: 0,
            plc_frames: 0,
        }
    }

    pub fn set_peer_id(&mut self, peer_id: Option<u8>) {
        if peer_id.is_some() {
            self.metadata.peer_id = peer_id;
        }
    }

    pub fn record(&mut self, remote_peers: u32, metrics: Telemetry, xruns: [u32; 2]) {
        self.samples = self.samples.saturating_add(1);
        self.max_remote_peers = self.max_remote_peers.max(remote_peers);
        if remote_peers > 0 {
            if metrics.direct_peers_mask != 0 {
                self.p2p_samples = self.p2p_samples.saturating_add(1);
            } else {
                self.sfu_only_samples = self.sfu_only_samples.saturating_add(1);
            }
        }
        if let Some(rtt) = metrics.rtt_ms.filter(|v| v.is_finite()) {
            self.rtt_ms.push(rtt);
        }
        if let Some(route_rtt) = metrics.route_rtt_ms.filter(|v| v.is_finite()) {
            self.route_rtt_ms.push(route_rtt);
        }
        if metrics.jitter_ms.is_finite() {
            self.jitter_ms.push(metrics.jitter_ms);
        }
        if metrics.loss_percent.is_finite() {
            self.loss_percent.push(metrics.loss_percent);
        }
        if let Some(total) = metrics
            .latency
            .and_then(|budget| budget.total_one_way_ms)
            .filter(|v| v.is_finite())
        {
            self.total_one_way_ms.push(total);
        }
        self.input_overflows = xruns[0];
        self.output_underflows = xruns[1];
        self.jitter_late_dropped = metrics.diagnostics.late_dropped;
        self.jitter_late_dropped_far = metrics.diagnostics.late_dropped_far;
        self.jitter_skipped_frames = metrics.diagnostics.skipped_frames;
        self.rx_push_dropped = metrics.diagnostics.rx_push_dropped;
        self.resyncs = metrics.diagnostics.resyncs;
        self.plc_frames = metrics.diagnostics.plc_frames;
    }

    #[must_use]
    pub fn finalize(mut self) -> SessionReport {
        let (rtt_avg_ms, rtt_p95_ms, rtt_max_ms) = summarize(&mut self.rtt_ms);
        let (route_rtt_avg_ms, route_rtt_p95_ms, route_rtt_max_ms) =
            summarize(&mut self.route_rtt_ms);
        let (jitter_avg_ms, _, jitter_max_ms) = summarize(&mut self.jitter_ms);
        let (packet_loss_avg_percent, _, packet_loss_max_percent) =
            summarize(&mut self.loss_percent);
        let (total_one_way_avg_ms, total_one_way_p95_ms, _) = summarize(&mut self.total_one_way_ms);

        let connection_mode = match (self.p2p_samples > 0, self.sfu_only_samples > 0) {
            (true, false) => "p2p",
            (true, true) => "hybrid",
            (false, true) => "sfu",
            (false, false) => "none",
        }
        .to_owned();

        SessionReport {
            schema_version: 1,
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            audio_backend: self.metadata.audio_backend,
            sample_rate_hz: self.metadata.sample_rate_hz,
            buffer_frames: self.metadata.buffer_frames,
            connection_mode,
            peer_id: self.metadata.peer_id,
            duration_secs: round3(self.started.elapsed().as_secs_f64()),
            samples: self.samples,
            max_remote_peers: self.max_remote_peers,
            p2p_samples: self.p2p_samples,
            sfu_only_samples: self.sfu_only_samples,
            rtt_avg_ms,
            rtt_p95_ms,
            rtt_max_ms,
            route_rtt_avg_ms,
            route_rtt_p95_ms,
            route_rtt_max_ms,
            jitter_avg_ms,
            jitter_max_ms,
            packet_loss_avg_percent,
            packet_loss_max_percent,
            total_one_way_avg_ms,
            total_one_way_p95_ms,
            input_overflows: self.input_overflows,
            output_underflows: self.output_underflows,
            jitter_late_dropped: self.jitter_late_dropped,
            jitter_late_dropped_far: self.jitter_late_dropped_far,
            jitter_skipped_frames: self.jitter_skipped_frames,
            rx_push_dropped: self.rx_push_dropped,
            resyncs: self.resyncs,
            plc_frames: self.plc_frames,
        }
    }
}

/// Spawns a background thread to POST the compact JSON report without blocking audio or UI.
pub fn upload_async(
    report: SessionReport,
    target: ReportTarget,
) -> Option<JoinHandle<Result<(), String>>> {
    thread::Builder::new()
        .name("jam-report-upload".into())
        .spawn(move || {
            upload_blocking(
                &report,
                &target,
                DEFAULT_CONNECT_TIMEOUT,
                DEFAULT_IO_TIMEOUT,
                DEFAULT_MAX_RETRIES,
            )
        })
        .ok()
}

fn upload_blocking(
    report: &SessionReport,
    target: &ReportTarget,
    connect_timeout: Duration,
    io_timeout: Duration,
    max_retries: usize,
) -> Result<(), String> {
    let payload = serde_json::to_string(report).map_err(|error| error.to_string())?;
    let peer_header = report
        .peer_id
        .map_or_else(|| "unknown".to_owned(), |id| format!("peer-{id}"));
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(connect_timeout)
        .timeout_read(io_timeout)
        .timeout_write(io_timeout)
        .build();

    let attempts = max_retries.saturating_add(1);
    let mut last_error = String::new();
    for attempt in 0..attempts {
        let mut request = agent
            .post(&target.endpoint)
            .set("Content-Type", "application/json")
            .set("X-Jam-Log-Kind", "client")
            .set("X-Jam-Peer-Id", &peer_header);
        if let Some(token) = &target.beta_token {
            request = request.set("X-Jam-Beta-Key", token);
        }
        match request.send_string(&payload) {
            Ok(_) => return Ok(()),
            Err(error) => {
                last_error = error.to_string();
                if attempt + 1 < attempts {
                    let backoff_ms = 200_u64.saturating_mul(1_u64 << attempt.min(4));
                    thread::sleep(Duration::from_millis(backoff_ms));
                }
            }
        }
    }
    eprintln!("Client end-of-session report upload skipped ({last_error})");
    Err(last_error)
}

fn summarize(values: &mut [f64]) -> (Option<f64>, Option<f64>, Option<f64>) {
    if values.is_empty() {
        return (None, None, None);
    }
    values.sort_unstable_by(f64::total_cmp);
    let count = values.len();
    #[allow(clippy::cast_precision_loss)]
    let avg = values.iter().sum::<f64>() / count as f64;
    let rank = count - count / 20 - 1;
    (
        Some(round3(avg)),
        Some(round3(values[rank])),
        Some(round3(values[count - 1])),
    )
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::SocketAddr,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn aggregates_session_metrics_and_classifies_connection_mode() {
        let mut acc = SessionReportAccumulator::start(SessionMetadata {
            audio_backend: "ASIO".into(),
            sample_rate_hz: 48_000,
            buffer_frames: 64,
            peer_id: Some(2),
        });
        let mut m1 = Telemetry {
            rtt_ms: Some(12.0),
            route_rtt_ms: Some(12.0),
            jitter_ms: 1.5,
            loss_percent: 0.0,
            direct_peers_mask: 0,
            ..Telemetry::default()
        };
        m1.diagnostics.plc_frames = 1;
        acc.record(1, m1, [0, 2]);

        let mut m2 = Telemetry {
            rtt_ms: Some(18.0),
            route_rtt_ms: Some(8.0),
            jitter_ms: 2.5,
            loss_percent: 0.5,
            direct_peers_mask: 0b0000_0001,
            ..Telemetry::default()
        };
        m2.diagnostics.late_dropped = 3;
        m2.diagnostics.plc_frames = 4;
        acc.record(1, m2, [1, 5]);

        let report = acc.finalize();
        assert_eq!(report.connection_mode, "hybrid");
        assert_eq!(report.samples, 2);
        assert_eq!(report.max_remote_peers, 1);
        assert_eq!(report.rtt_avg_ms, Some(15.0));
        assert_eq!(report.rtt_max_ms, Some(18.0));
        assert_eq!(report.jitter_max_ms, Some(2.5));
        assert_eq!(report.packet_loss_max_percent, Some(0.5));
        assert_eq!(report.input_overflows, 1);
        assert_eq!(report.output_underflows, 5);
        assert_eq!(report.jitter_late_dropped, 3);
        assert_eq!(report.plc_frames, 4);
    }

    #[test]
    fn uploads_compact_json_report_to_collector_asynchronously() {
        let storage_dir = std::env::temp_dir().join(format!(
            "jam-client-report-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let collector = jam_collector::Collector::bind(jam_collector::CollectorConfig {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            storage_dir: storage_dir.clone(),
            beta_token: Some("client-beta-key".into()),
            trust_proxy_headers: false,
            ..jam_collector::CollectorConfig::default()
        })
        .unwrap();
        let addr = collector.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_c = Arc::clone(&stop);
        let server_thread = thread::spawn(move || collector.run(&stop_c).unwrap());

        let mut acc = SessionReportAccumulator::start(SessionMetadata {
            audio_backend: "synthetic-headless".into(),
            sample_rate_hz: 48_000,
            buffer_frames: 128,
            peer_id: Some(3),
        });
        acc.record(
            1,
            Telemetry {
                rtt_ms: Some(14.5),
                route_rtt_ms: Some(9.2),
                jitter_ms: 1.2,
                loss_percent: 0.1,
                direct_peers_mask: 0b0000_0001,
                ..Telemetry::default()
            },
            [0, 0],
        );
        let report = acc.finalize();
        let target = ReportTarget {
            endpoint: format!("http://{addr}/api/v1/logs/client"),
            beta_token: Some("client-beta-key".into()),
        };

        let result = upload_async(report.clone(), target)
            .unwrap()
            .join()
            .unwrap();
        assert!(result.is_ok(), "{result:?}");

        stop.store(true, Ordering::Relaxed);
        server_thread.join().unwrap();

        let date = jam_collector::storage::utc_date_string(SystemTime::now());
        let dir = storage_dir.join(date).join("127.0.0.1");
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        let name = files[0].file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("client-"));
        assert!(name.ends_with("-peer-3.json"));
        let decoded: SessionReport =
            serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
        assert_eq!(decoded.connection_mode, "p2p");
        assert_eq!(decoded.audio_backend, "synthetic-headless");
        assert_eq!(decoded.rtt_avg_ms, Some(14.5));

        let _ = std::fs::remove_dir_all(storage_dir);
    }
}
