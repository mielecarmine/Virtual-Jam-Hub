//! Central HTTPS-ready log collector service for Virtual Jam beta testing.
//!
//! Designed to run behind a TLS-terminating reverse proxy (Caddy/Nginx) on `185.198.245.40`,
//! storing server operational logs and client end-of-session reports under
//! `<storage_dir>/<YYYY-MM-DD>/<source_ip>/`.

mod http;
mod rate_limit;
pub mod storage;

use std::{
    io::{self, BufReader},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use http::{HttpRequestHead, ParseError, read_body, read_request_head, write_response};
use rate_limit::RateLimiter;
use storage::{LogKind, enforce_retention, store_payload};

/// Maximum accepted payload size for client end-of-session reports (256 KiB).
pub const DEFAULT_MAX_CLIENT_BYTES: usize = 256 * 1024;
/// Maximum accepted payload size for server operational logs (2 MiB).
pub const DEFAULT_MAX_SERVER_BYTES: usize = 2 * 1024 * 1024;
/// Default per-IP request limit per 60-second window.
pub const DEFAULT_RATE_LIMIT_PER_MIN: u32 = 30;
/// Default log retention in days.
pub const DEFAULT_RETENTION_DAYS: u64 = 30;
/// Default disk quota in MiB.
pub const DEFAULT_MAX_DISK_MB: u64 = 2048;

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONCURRENT_CONNECTIONS: usize = 32;
const RETENTION_INTERVAL: Duration = Duration::from_secs(3600);

/// Configuration for the collector HTTP service.
#[derive(Clone, Debug)]
pub struct CollectorConfig {
    pub bind_addr: SocketAddr,
    pub storage_dir: PathBuf,
    pub beta_token: Option<String>,
    pub trust_proxy_headers: bool,
    pub max_client_bytes: usize,
    pub max_server_bytes: usize,
    pub rate_limit_per_min: u32,
    pub retention_days: u64,
    pub max_disk_bytes: u64,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], 8080)),
            storage_dir: PathBuf::from("/var/log/jam-collector"),
            beta_token: None,
            trust_proxy_headers: true,
            max_client_bytes: DEFAULT_MAX_CLIENT_BYTES,
            max_server_bytes: DEFAULT_MAX_SERVER_BYTES,
            rate_limit_per_min: DEFAULT_RATE_LIMIT_PER_MIN,
            retention_days: DEFAULT_RETENTION_DAYS,
            max_disk_bytes: DEFAULT_MAX_DISK_MB * 1024 * 1024,
        }
    }
}

struct SharedState {
    config: CollectorConfig,
    limiter: Mutex<RateLimiter>,
    active_connections: AtomicUsize,
    last_retention: Mutex<Instant>,
}

/// Bound HTTP/1.1 collector service.
pub struct Collector {
    listener: TcpListener,
    state: Arc<SharedState>,
}

impl Collector {
    /// Binds the TCP listener and prepares the storage directory.
    ///
    /// # Errors
    /// Returns socket binding or directory creation errors.
    pub fn bind(config: CollectorConfig) -> io::Result<Self> {
        std::fs::create_dir_all(&config.storage_dir)?;
        let _ = enforce_retention(
            &config.storage_dir,
            SystemTime::now(),
            config.retention_days,
            config.max_disk_bytes,
        );
        let listener = TcpListener::bind(config.bind_addr)?;
        listener.set_nonblocking(true)?;
        let limiter = RateLimiter::new(config.rate_limit_per_min, Duration::from_secs(60));
        Ok(Self {
            listener,
            state: Arc::new(SharedState {
                config,
                limiter: Mutex::new(limiter),
                active_connections: AtomicUsize::new(0),
                last_retention: Mutex::new(Instant::now()),
            }),
        })
    }

    /// Returns the actual bound socket address.
    ///
    /// # Errors
    /// Returns the underlying socket query error.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serves incoming HTTP requests until `stop` becomes `true`.
    ///
    /// # Errors
    /// Returns fatal listener accept errors.
    pub fn run(&self, stop: &AtomicBool) -> io::Result<()> {
        while !stop.load(Ordering::Relaxed) {
            self.maybe_run_retention();
            match self.listener.accept() {
                Ok((stream, peer_addr)) => {
                    let current = self.state.active_connections.fetch_add(1, Ordering::AcqRel);
                    if current >= MAX_CONCURRENT_CONNECTIONS {
                        self.state.active_connections.fetch_sub(1, Ordering::AcqRel);
                        let mut stream = stream;
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                        let _ = write_response(
                            &mut stream,
                            503,
                            "Service Unavailable",
                            "collector busy\n",
                        );
                        continue;
                    }
                    let state = Arc::clone(&self.state);
                    let spawn_result = thread::Builder::new()
                        .name("jam-collector-conn".into())
                        .spawn(move || {
                            handle_connection(stream, peer_addr, &state);
                            state.active_connections.fetch_sub(1, Ordering::AcqRel);
                        });
                    if spawn_result.is_err() {
                        self.state.active_connections.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn maybe_run_retention(&self) {
        let should_run = self.state.last_retention.lock().is_ok_and(|mut last| {
            if last.elapsed() >= RETENTION_INTERVAL {
                *last = Instant::now();
                true
            } else {
                false
            }
        });
        if should_run {
            let _ = enforce_retention(
                &self.state.config.storage_dir,
                SystemTime::now(),
                self.state.config.retention_days,
                self.state.config.max_disk_bytes,
            );
        }
    }
}

#[allow(clippy::too_many_lines)]
fn handle_connection(mut stream: TcpStream, peer_addr: SocketAddr, state: &SharedState) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    let mut reader = BufReader::new(&mut stream);
    let head = match read_request_head(&mut reader) {
        Ok(head) => head,
        Err(ParseError::BadRequest(reason)) => {
            drop(reader);
            let _ = write_response(&mut stream, 400, "Bad Request", &format!("{reason}\n"));
            return;
        }
        Err(ParseError::Io) => return,
    };

    if head.method == "GET" && head.path == "/healthz" {
        drop(reader);
        let _ = write_response(&mut stream, 200, "OK", "ok\n");
        return;
    }

    if head.method != "POST" {
        drop(reader);
        let _ = write_response(&mut stream, 405, "Method Not Allowed", "use POST\n");
        return;
    }

    let Some(kind) = resolve_log_kind(&head) else {
        drop(reader);
        let _ = write_response(&mut stream, 404, "Not Found", "unknown endpoint\n");
        return;
    };

    let source_ip = resolve_source_ip(peer_addr.ip(), &head, state.config.trust_proxy_headers);

    if let Some(expected) = state.config.beta_token.as_deref() {
        if !authorized(&head, expected) {
            drop(reader);
            let _ = write_response(&mut stream, 401, "Unauthorized", "invalid beta token\n");
            return;
        }
    }

    let allowed = state
        .limiter
        .lock()
        .is_ok_and(|mut limiter| limiter.allow(source_ip, Instant::now()));
    if !allowed {
        drop(reader);
        let _ = write_response(
            &mut stream,
            429,
            "Too Many Requests",
            "rate limit exceeded\n",
        );
        return;
    }

    if head.header("transfer-encoding").is_some() {
        drop(reader);
        let _ = write_response(
            &mut stream,
            411,
            "Length Required",
            "chunked transfer not supported; send Content-Length\n",
        );
        return;
    }

    let Some(content_length_raw) = head.header("content-length") else {
        drop(reader);
        let _ = write_response(
            &mut stream,
            411,
            "Length Required",
            "Content-Length required\n",
        );
        return;
    };
    let Ok(content_length) = content_length_raw.parse::<usize>() else {
        drop(reader);
        let _ = write_response(&mut stream, 400, "Bad Request", "invalid Content-Length\n");
        return;
    };

    let max_bytes = match kind {
        LogKind::Client => state.config.max_client_bytes,
        LogKind::Server => state.config.max_server_bytes,
    };
    if content_length == 0 {
        drop(reader);
        let _ = write_response(&mut stream, 400, "Bad Request", "empty payload\n");
        return;
    }
    if content_length > max_bytes {
        drop(reader);
        let _ = write_response(
            &mut stream,
            413,
            "Payload Too Large",
            "payload exceeds size cap\n",
        );
        return;
    }

    let Ok(body) = read_body(&mut reader, content_length) else {
        drop(reader);
        let _ = write_response(&mut stream, 400, "Bad Request", "incomplete body\n");
        return;
    };
    drop(reader);

    let identifier = match kind {
        LogKind::Server => head.header("x-jam-session-id"),
        LogKind::Client => head.header("x-jam-peer-id"),
    };
    let extension = match kind {
        LogKind::Server => "log",
        LogKind::Client => {
            if head
                .header("content-type")
                .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("application/json"))
            {
                "json"
            } else {
                "txt"
            }
        }
    };

    match store_payload(
        &state.config.storage_dir,
        SystemTime::now(),
        source_ip,
        kind,
        identifier,
        extension,
        &body,
    ) {
        Ok(_) => {
            let _ = write_response(&mut stream, 202, "Accepted", "stored\n");
        }
        Err(error) => {
            eprintln!("Collector storage error: {error}");
            let _ = write_response(&mut stream, 500, "Internal Server Error", "storage error\n");
        }
    }
}

fn resolve_log_kind(head: &HttpRequestHead) -> Option<LogKind> {
    match head.path.as_str() {
        "/api/v1/logs/server" => Some(LogKind::Server),
        "/api/v1/logs/client" => Some(LogKind::Client),
        "/api/v1/logs" => match head.header("x-jam-log-kind") {
            Some("server") => Some(LogKind::Server),
            Some("client") => Some(LogKind::Client),
            _ => None,
        },
        _ => None,
    }
}

/// Resolves source IP strictly from socket peer address or trusted loopback reverse proxy headers.
fn resolve_source_ip(peer_ip: IpAddr, head: &HttpRequestHead, trust_proxy: bool) -> IpAddr {
    if !trust_proxy || !peer_ip.is_loopback() {
        return peer_ip;
    }
    if let Some(ip) = head
        .header("x-real-ip")
        .and_then(|value| value.trim().parse::<IpAddr>().ok())
    {
        return ip;
    }
    if let Some(ip) = head
        .header("x-forwarded-for")
        .and_then(|value| value.split(',').next())
        .and_then(|first| first.trim().parse::<IpAddr>().ok())
    {
        return ip;
    }
    peer_ip
}

fn authorized(head: &HttpRequestHead, expected: &str) -> bool {
    if let Some(key) = head.header("x-jam-beta-key") {
        if constant_time_eq(key.as_bytes(), expected.as_bytes()) {
            return true;
        }
    }
    if let Some(auth) = head.header("authorization") {
        if let Some(bearer) = auth.strip_prefix("Bearer ") {
            if constant_time_eq(bearer.trim().as_bytes(), expected.as_bytes()) {
                return true;
            }
        }
    }
    false
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (&left, &right) in a.iter().zip(b.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "jam-collector-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn send_raw(addr: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn stores_server_and_client_logs_by_utc_date_and_sanitized_ip() {
        let storage_dir = temp_dir("catalog");
        let config = CollectorConfig {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            storage_dir: storage_dir.clone(),
            beta_token: Some("secret-beta-key".into()),
            trust_proxy_headers: true,
            max_client_bytes: 128,
            max_server_bytes: 256,
            rate_limit_per_min: 10,
            retention_days: 30,
            max_disk_bytes: 0,
        };
        let collector = Collector::bind(config).unwrap();
        let addr = collector.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_server = Arc::clone(&stop);
        let worker = thread::spawn(move || collector.run(&stop_server).unwrap());

        // 1. Healthz works without token
        let health = send_raw(addr, "GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(health.starts_with("HTTP/1.1 200 OK"));

        // 2. Missing token -> 401
        let unauth = send_raw(
            addr,
            "POST /api/v1/logs/client HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
        );
        assert!(unauth.starts_with("HTTP/1.1 401 Unauthorized"));

        // 3. Valid server log with traversal attempt in session id & trusted proxy IP
        let server_body = "event=join peer=1\n";
        let server_req = format!(
            "POST /api/v1/logs/server HTTP/1.1\r\n\
             Host: localhost\r\n\
             X-Jam-Beta-Key: secret-beta-key\r\n\
             X-Real-IP: 203.0.113.42\r\n\
             X-Jam-Session-Id: ../../evil/sess_01\r\n\
             Content-Type: text/plain\r\n\
             Content-Length: {}\r\n\r\n\
             {server_body}",
            server_body.len()
        );
        let resp = send_raw(addr, &server_req);
        assert!(resp.starts_with("HTTP/1.1 202 Accepted"), "{resp}");

        // 4. Valid client JSON report via Bearer token & /api/v1/logs + X-Jam-Log-Kind
        let client_body = r#"{"rtt_avg_ms":14.2}"#;
        let client_req = format!(
            "POST /api/v1/logs HTTP/1.1\r\n\
             Host: localhost\r\n\
             Authorization: Bearer secret-beta-key\r\n\
             X-Jam-Log-Kind: client\r\n\
             X-Forwarded-For: 198.51.100.7, 10.0.0.1\r\n\
             X-Jam-Peer-Id: peer-3\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n\
             {client_body}",
            client_body.len()
        );
        let resp = send_raw(addr, &client_req);
        assert!(resp.starts_with("HTTP/1.1 202 Accepted"), "{resp}");

        // 5. Oversized client payload -> 413
        let big_req = format!(
            "POST /api/v1/logs/client HTTP/1.1\r\n\
             Host: localhost\r\n\
             X-Jam-Beta-Key: secret-beta-key\r\n\
             Content-Length: 200\r\n\r\n{}",
            "x".repeat(200)
        );
        let resp = send_raw(addr, &big_req);
        assert!(resp.starts_with("HTTP/1.1 413 Payload Too Large"), "{resp}");

        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();

        let date = storage::utc_date_string(SystemTime::now());
        let server_dir = storage_dir.join(&date).join("203.0.113.42");
        let server_files: Vec<_> = std::fs::read_dir(&server_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(server_files.len(), 1);
        let server_name = server_files[0].file_name().unwrap().to_string_lossy();
        assert!(server_name.starts_with("server-"));
        assert!(server_name.ends_with("-evilsess_01.log"));
        assert_eq!(
            std::fs::read_to_string(&server_files[0]).unwrap(),
            server_body
        );

        let client_dir = storage_dir.join(&date).join("198.51.100.7");
        let client_files: Vec<_> = std::fs::read_dir(&client_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(client_files.len(), 1);
        let client_name = client_files[0].file_name().unwrap().to_string_lossy();
        assert!(client_name.starts_with("client-"));
        assert!(client_name.ends_with("-peer-3.json"));
        assert_eq!(
            std::fs::read_to_string(&client_files[0]).unwrap(),
            client_body
        );

        let _ = std::fs::remove_dir_all(storage_dir);
    }

    #[test]
    fn enforces_rate_limit_per_source_ip() {
        let storage_dir = temp_dir("ratelimit");
        let config = CollectorConfig {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            storage_dir: storage_dir.clone(),
            beta_token: None,
            trust_proxy_headers: true,
            max_client_bytes: 1024,
            max_server_bytes: 1024,
            rate_limit_per_min: 2,
            retention_days: 30,
            max_disk_bytes: 0,
        };
        let collector = Collector::bind(config).unwrap();
        let addr = collector.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_server = Arc::clone(&stop);
        let worker = thread::spawn(move || collector.run(&stop_server).unwrap());

        let req = "POST /api/v1/logs/client HTTP/1.1\r\nHost: localhost\r\nX-Real-IP: 192.0.2.55\r\nContent-Length: 4\r\n\r\ntest";
        assert!(send_raw(addr, req).starts_with("HTTP/1.1 202 Accepted"));
        assert!(send_raw(addr, req).starts_with("HTTP/1.1 202 Accepted"));
        assert!(send_raw(addr, req).starts_with("HTTP/1.1 429 Too Many Requests"));

        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        let _ = std::fs::remove_dir_all(storage_dir);
    }
}
