//! Headless benchmark using the same codec, UDP worker and per-peer jitter buffers as the client.

// Share the production engine without widening the desktop application's public API.
#[allow(dead_code)]
mod audio;
#[allow(dead_code)]
mod codec;
#[allow(dead_code)]
mod jitter;
#[allow(dead_code)]
mod mixer;
#[allow(dead_code)]
mod monitor;
#[allow(dead_code)]
mod network;

use std::{
    fmt::Write as _,
    io,
    net::SocketAddr,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use codec::FRAME_SAMPLES;
use network::{Network, Settings, Telemetry};
use rtrb::{Consumer, Producer, RingBuffer};

const USAGE: &str = "net_bench <SERVER_IP:PORT> [--peers N] [--seconds S] [--output report.txt] \
[--heterogeneous] [--jitter-chunks N] [--simulate-delay-ms MS] [--simulate-jitter-ms MS] [--simulate-loss P]";

const PROFILES: [(&str, Settings); 4] = [
    (
        "fibra-lan",
        Settings {
            target_chunks: 3,
            loss: 0.0,
            jitter_ms: 1,
            delay_ms: 2,
        },
    ),
    (
        "dsl-home",
        Settings {
            target_chunks: 3,
            loss: 0.002,
            jitter_ms: 4,
            delay_ms: 10,
        },
    ),
    (
        "wifi-congested",
        Settings {
            target_chunks: 4,
            loss: 0.015,
            jitter_ms: 12,
            delay_ms: 15,
        },
    ),
    (
        "mobile-4g",
        Settings {
            target_chunks: 6,
            loss: 0.02,
            jitter_ms: 16,
            delay_ms: 22,
        },
    ),
];

struct Options {
    server: SocketAddr,
    peers: usize,
    seconds: u32,
    output: Option<PathBuf>,
    network: Settings,
    heterogeneous: bool,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Self>, String> {
        let mut args = args.into_iter();
        let first = args.next().ok_or(USAGE)?;
        if first == "--help" || first == "-h" {
            return Ok(None);
        }
        let mut options = Self {
            server: first.parse().map_err(|_| "Invalid SERVER_IP:PORT")?,
            peers: 2,
            seconds: 10,
            output: None,
            network: Settings::default(),
            heterogeneous: false,
        };
        while let Some(flag) = args.next() {
            if flag == "--heterogeneous" {
                options.heterogeneous = true;
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| format!("Missing value for {flag}"))?;
            match flag.as_str() {
                "--peers" => options.peers = value.parse().map_err(|_| "Invalid peer count")?,
                "--seconds" => options.seconds = value.parse().map_err(|_| "Invalid duration")?,
                "--output" => options.output = Some(value.into()),
                "--jitter-chunks" => {
                    let chunks: usize = value.parse().map_err(|_| "Invalid jitter chunks")?;
                    if !(1..=16).contains(&chunks) {
                        return Err("--jitter-chunks must be 1..=16".into());
                    }
                    options.network.target_chunks = chunks;
                }
                "--simulate-loss" => {
                    let loss: f64 = value.parse().map_err(|_| "Invalid simulated loss")?;
                    if !loss.is_finite() || !(0.0..=1.0).contains(&loss) {
                        return Err("--simulate-loss must be 0..=1".into());
                    }
                    options.network.loss = loss;
                }
                "--simulate-jitter-ms" => {
                    let jitter: u32 = value.parse().map_err(|_| "Invalid simulated jitter")?;
                    if jitter > 1000 {
                        return Err("--simulate-jitter-ms must be 0..=1000".into());
                    }
                    options.network.jitter_ms = jitter;
                }
                "--simulate-delay-ms" => {
                    let delay: u32 = value.parse().map_err(|_| "Invalid simulated delay")?;
                    if delay > 1000 {
                        return Err("--simulate-delay-ms must be 0..=1000".into());
                    }
                    options.network.delay_ms = delay;
                }
                _ => return Err(format!("Unknown option: {flag}")),
            }
        }
        if !(2..=jam_protocol::MAX_PEERS).contains(&options.peers) {
            return Err(format!("Peers must be in 2..={}", jam_protocol::MAX_PEERS));
        }
        if options.seconds < 2 {
            return Err("Duration must be at least 2 seconds to collect Ping RTT".into());
        }
        if options
            .output
            .as_ref()
            .is_some_and(|p| p.extension().is_none_or(|e| e != "txt"))
        {
            return Err("Output must have a .txt extension".into());
        }
        Ok(Some(options))
    }

    fn peer_profile(&self, index: usize) -> (&'static str, Settings) {
        if self.heterogeneous {
            PROFILES[(index - 1) % PROFILES.len()]
        } else {
            ("uniform", self.network)
        }
    }
}

struct Peer {
    profile: &'static str,
    settings: Settings,
    network: Network,
    capture: Producer<f32>,
    mic: Consumer<f32>,
    rx: mixer::RxChannels,
    mixer: mixer::Mixer,
    stats: audio::Stats,
    samples: Vec<Telemetry>,
    frames: u64,
    audible: bool,
}

impl Peer {
    fn new(
        server: SocketAddr,
        index: usize,
        profile: &'static str,
        settings: Settings,
    ) -> io::Result<Self> {
        let (capture, mic) = RingBuffer::new(FRAME_SAMPLES);
        let (rx, mixer) = mixer::channels(FRAME_SAMPLES);
        Ok(Self {
            profile,
            settings,
            network: Network::connect(server, &format!("bench-{index}"))?.configure(settings),
            capture,
            mic,
            rx,
            mixer,
            stats: audio::Stats::default(),
            samples: Vec::new(),
            frames: 0,
            audible: false,
        })
    }

    fn tick(&mut self, now: Instant, frame: bool) -> io::Result<()> {
        if frame {
            // A periodic 400 Hz tone has exactly one period per 120-sample frame.
            for sample in 0..120_u16 {
                let value = (f32::from(sample) * std::f32::consts::TAU / 120.0).sin() * 0.2;
                self.capture.push(value).map_err(io::Error::other)?;
            }
            self.frames += 1;
        }
        self.network.tick(&mut self.mic, &mut self.rx, now)?;
        if frame {
            let mut output = [0.0_f32; FRAME_SAMPLES];
            self.mixer.playback(&mut output, 1, &self.stats);
            self.audible |= output.iter().any(|sample| sample.abs() > 0.0001);
        }
        Ok(())
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.network.disconnect();
    }
}

#[derive(Debug, PartialEq)]
struct Summary {
    count: usize,
    min: f64,
    avg: f64,
    p95: f64,
    max: f64,
}

fn summarize(values: impl Iterator<Item = f64>) -> Option<Summary> {
    let mut values: Vec<_> = values.filter(|v| v.is_finite()).collect();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable_by(f64::total_cmp);
    let count = values.len();
    // Nearest-rank percentile: ceil(0.95 * n), with a one-based rank.
    let rank = count - count / 20;
    #[allow(clippy::cast_precision_loss)]
    let avg = values.iter().sum::<f64>() / count as f64;
    Some(Summary {
        count,
        min: values[0],
        avg,
        p95: values[rank - 1],
        max: values[count - 1],
    })
}

fn report(options: &Options, peers: &[Peer], elapsed: Duration, skipped: u64) -> String {
    let mut text = format!(
        "Virtual Jam network benchmark\nServer: {}\nPeers: {}\nDuration: {:.3} s\n\
         Opus: LowDelay, mono 48000 Hz, 120 samples / 2.5 ms, 30-byte CBR payload\n\
         Telemetry interval: 500 ms; p95: nearest rank; missing RTT excluded\n\
         Skipped source frames per peer (host scheduling): {skipped}\n\
         Metrics aggregate peer snapshots; buffer excludes hardware/SPSC queues.\n",
        options.server,
        peers.len(),
        elapsed.as_secs_f64()
    );
    for (index, peer) in peers.iter().enumerate() {
        let _ = writeln!(
            text,
            "Peer {}: {} snapshots, {} source frames, decoded audio: {}",
            index + 1,
            peer.samples.len(),
            peer.frames,
            peer.audible
        );
        if options.heterogeneous || peer.settings != Settings::default() {
            let _ = writeln!(
                text,
                "  Profile peer {}: {} (delay={}ms, jitter={}ms, loss={:.2}%, chunks={})",
                index + 1,
                peer.profile,
                peer.settings.delay_ms,
                peer.settings.jitter_ms,
                peer.settings.loss * 100.0,
                peer.settings.target_chunks
            );
        }
    }
    let samples: Vec<_> = peers.iter().flat_map(|p| p.samples.iter()).collect();
    let metrics = [
        (
            "Ping RTT (ms)",
            summarize(samples.iter().filter_map(|s| s.rtt_ms)),
        ),
        (
            "Jitter (ms)",
            summarize(samples.iter().map(|s| s.jitter_ms)),
        ),
        (
            "Packet Loss (%)",
            summarize(samples.iter().map(|s| s.loss_percent)),
        ),
        (
            "Jitter Buffer (ms)",
            summarize(samples.iter().map(|s| s.buffer_ms)),
        ),
    ];
    let _ = writeln!(
        text,
        "\n{:<20} {:>8} {:>10} {:>10} {:>10} {:>10}",
        "Metric", "Samples", "min", "avg", "p95", "max"
    );
    for (name, summary) in metrics {
        if let Some(s) = summary {
            let _ = writeln!(
                text,
                "{name:<20} {:>8} {:>10.3} {:>10.3} {:>10.3} {:>10.3}",
                s.count, s.min, s.avg, s.p95, s.max
            );
        } else {
            let _ = writeln!(text, "{name:<20} N/A (no samples)");
        }
    }
    text
}

fn run(options: &Options) -> io::Result<String> {
    let mut peers = (1..=options.peers)
        .map(|index| {
            let (profile, settings) = options.peer_profile(index);
            Peer::new(options.server, index, profile, settings)
        })
        .collect::<io::Result<Vec<_>>>()?;
    // Start the measured interval only after every simulated peer has joined.
    while peers.iter().any(|p| !p.network.is_connected()) {
        for peer in &mut peers {
            peer.tick(Instant::now(), false)?;
            let _ = peer.network.take_telemetry();
        }
        thread::sleep(Duration::from_micros(250));
    }
    let start = Instant::now();
    let mut next_frame = start;
    let mut skipped = 0;
    while start.elapsed() < Duration::from_secs(u64::from(options.seconds)) {
        let now = Instant::now();
        let frame = now >= next_frame;
        if frame {
            let late = now.duration_since(next_frame).as_nanos() / jitter::CHUNK_TIME.as_nanos();
            skipped += u64::try_from(late).unwrap_or(u64::MAX);
            let phase = now.duration_since(next_frame).as_nanos() % jitter::CHUNK_TIME.as_nanos();
            next_frame = now
                + jitter::CHUNK_TIME
                    .checked_sub(Duration::from_nanos(u64::try_from(phase).unwrap_or(0)))
                    .unwrap_or(jitter::CHUNK_TIME);
        }
        for peer in &mut peers {
            peer.tick(Instant::now(), frame)?;
            if let Some(sample) = peer.network.take_telemetry() {
                peer.samples.push(sample);
            }
        }
        thread::sleep(Duration::from_micros(250));
    }
    Ok(report(options, &peers, start.elapsed(), skipped))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = Options::parse(std::env::args().skip(1)).map_err(io::Error::other)? else {
        println!("{USAGE}");
        return Ok(());
    };
    let text = run(&options)?;
    print!("{text}");
    if let Some(path) = options.output {
        std::fs::write(path, text)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "net_bench/tests.rs"]
mod tests;
