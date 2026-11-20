//! Nonblocking UDP worker. All sockets and protocol processing stay off audio callbacks.

use crate::{
    codec::{self, Encoded, FRAME_SAMPLES, PAYLOAD_SIZE},
    jitter::{CHUNK_TIME, PeerJitterBuffer},
    mixer::RxChannels,
};
use jam_protocol::{
    AUDIO_HEADER_SIZE, AudioHeader, ControlMessage, MAX_PACKET_SIZE, MetronomeConfig,
    MetronomeState, NICKNAME_CAPACITY, SAMPLE_RATE, STREAM_PRIMARY,
};
use rtrb::Consumer;
use std::collections::BTreeMap;
use std::{
    io,
    net::{SocketAddr, UdpSocket},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

mod clock;
pub(crate) mod telemetry;
use clock::ClockSync;
use telemetry::ArrivalStats;
pub use telemetry::Telemetry;

const PACKET_SIZE: usize = AUDIO_HEADER_SIZE + PAYLOAD_SIZE;
const JOIN_INTERVAL: Duration = Duration::from_millis(500);
const PING_INTERVAL: Duration = Duration::from_secs(1);
const SERVER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub target_chunks: usize,
    pub loss: f64,
    pub jitter_ms: u32,
    pub delay_ms: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            target_chunks: 3,
            loss: 0.0,
            jitter_ms: 0,
            delay_ms: 0,
        }
    }
}

struct Delayed {
    due: Instant,
    peer: u8,
    sequence: u32,
    chunk: Encoded,
}

pub struct Network {
    arrivals: BTreeMap<u8, ArrivalStats>,
    telemetry: Option<Telemetry>,
    last_telemetry: Instant,
    retired_diagnostics: crate::jitter::Diagnostics,
    last_tick: Option<Instant>,
    worker_max_gap_ms: f64,
    rtt_max_ms: Option<f64>,
    rtt: Option<(f64, Instant)>,
    pending_ping: Option<(u64, Instant)>,
    clock: ClockSync,
    metronome: Option<MetronomeState>,
    rendered_metronome: Option<(MetronomeState, i64)>,
    roster: Vec<jam_protocol::PeerInfo>,
    roster_changed: bool,
    encoder: opus::Encoder,
    pub codec_lookahead_samples: u32,
    settings: Settings,
    peers: BTreeMap<u8, PeerJitterBuffer>,
    delayed: Vec<Delayed>,
    random: u32,
    next_playout: Option<Instant>,
    socket: UdpSocket,
    join: ControlMessage,
    peer_id: Option<u8>,
    sequence: u32,
    timestamp: u16,
    started: Instant,
    last_join: Option<Instant>,
    last_ping: Instant,
    last_server: Instant,
}

impl Network {
    pub fn connect(server: SocketAddr, nickname: &str) -> io::Result<Self> {
        Self::connect_with_bind(server, nickname, None)
    }

    pub fn connect_with_bind(
        server: SocketAddr,
        nickname: &str,
        bind_ip: Option<std::net::IpAddr>,
    ) -> io::Result<Self> {
        if nickname.is_empty() || nickname.len() > NICKNAME_CAPACITY {
            return Err(io::Error::other("Nickname must contain 1..=32 UTF-8 bytes"));
        }
        let bind_addr = match bind_ip {
            Some(ip) if ip.is_ipv4() == server.is_ipv4() => SocketAddr::new(ip, 0),
            Some(_) => {
                return Err(io::Error::other(
                    "Bind IP address family must match server address",
                ));
            }
            None if server.is_ipv4() => SocketAddr::from(([0, 0, 0, 0], 0)),
            None => SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], 0)),
        };
        let socket = UdpSocket::bind(bind_addr)?;
        socket.connect(server)?; // Connected UDP filters datagrams from other endpoints.
        socket.set_nonblocking(true)?;
        let mut bytes = [0; NICKNAME_CAPACITY];
        bytes[..nickname.len()].copy_from_slice(nickname.as_bytes());
        let now = Instant::now();
        let mut encoder = codec::encoder().map_err(io::Error::other)?;
        let codec_lookahead_samples =
            u32::try_from(encoder.get_lookahead().map_err(io::Error::other)?)
                .map_err(io::Error::other)?;
        Ok(Self {
            arrivals: BTreeMap::new(),
            telemetry: None,
            last_telemetry: now,
            retired_diagnostics: crate::jitter::Diagnostics::default(),
            last_tick: None,
            worker_max_gap_ms: 0.0,
            rtt_max_ms: None,
            rtt: None,
            pending_ping: None,
            clock: ClockSync::default(),
            metronome: None,
            rendered_metronome: None,
            roster: Vec::new(),
            roster_changed: false,
            encoder,
            codec_lookahead_samples,
            settings: Settings::default(),
            peers: BTreeMap::new(),
            delayed: Vec::new(),
            random: u32::from(socket.local_addr()?.port()) + 1,
            next_playout: None,
            socket,
            join: ControlMessage::JoinRequest {
                nickname: bytes,
                nickname_len: u8::try_from(nickname.len()).map_err(io::Error::other)?,
            },
            peer_id: None,
            sequence: 0,
            timestamp: 0,
            started: now,
            last_join: None,
            last_ping: now,
            last_server: now,
        })
    }

    fn update_roster(&mut self, peers: &[jam_protocol::PeerInfo], speaker: &RxChannels) {
        let remote: Vec<_> = peers
            .iter()
            .copied()
            .filter(|peer| Some(peer.peer_id) != self.peer_id)
            .collect();
        if remote != self.roster {
            for old in &self.roster {
                if !remote.contains(old) {
                    speaker.remove(old.peer_id);
                }
            }
            self.roster = remote;
            self.roster_changed = true;
        }
        self.peers.retain(|id, peer| {
            let keep = peers.iter().any(|p| p.peer_id == *id);
            if !keep {
                self.retired_diagnostics.accumulate(peer.diagnostics);
                speaker.remove(*id);
            }
            keep
        });
        self.arrivals
            .retain(|id, _| peers.iter().any(|p| p.peer_id == *id));
        self.delayed
            .retain(|packet| peers.iter().any(|p| p.peer_id == packet.peer));
    }

    pub fn take_roster(&mut self) -> Option<Vec<jam_protocol::PeerInfo>> {
        if std::mem::take(&mut self.roster_changed) {
            Some(self.roster.clone())
        } else {
            None
        }
    }

    pub fn configure(mut self, settings: Settings) -> Self {
        self.settings = settings;
        self
    }

    fn random(&mut self) -> f64 {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 17;
        self.random ^= self.random << 5;
        f64::from(self.random) / (f64::from(u32::MAX) + 1.0)
    }

    fn incoming(&mut self, bytes: &[u8], id: u8, now: Instant) {
        let Some((header, chunk)) = receive_audio(bytes, id) else {
            return;
        };
        if self.settings.loss > 0.0 && self.random() < self.settings.loss {
            return;
        }
        // Keep simulated traffic bounded even when the socket is flooded.
        if self.delayed.len() >= 512 {
            return;
        }
        let delay = Duration::from_millis(u64::from(self.settings.delay_ms))
            + Duration::from_secs_f64(self.random() * f64::from(self.settings.jitter_ms) / 1000.0);
        self.delayed.push(Delayed {
            due: now + delay,
            peer: header.peer_id,
            sequence: header.sequence_number,
            chunk,
        });
    }

    fn playback(&mut self, speaker: &mut RxChannels, now: Instant) -> io::Result<()> {
        // Deliver in simulated arrival order even when several deadlines fall in one tick.
        self.delayed
            .sort_unstable_by_key(|packet| std::cmp::Reverse(packet.due));
        for (&id, peer) in &mut self.peers {
            peer.begin_receive_batch(
                self.delayed
                    .iter()
                    .filter(|packet| packet.peer == id && packet.due <= now)
                    .map(|packet| packet.sequence),
            );
        }
        while self.delayed.last().is_some_and(|packet| packet.due <= now) {
            let Some(packet) = self.delayed.pop() else {
                break;
            };
            if let std::collections::btree_map::Entry::Vacant(entry) = self.peers.entry(packet.peer)
            {
                entry.insert(
                    PeerJitterBuffer::new(packet.sequence, now, self.settings.target_chunks)
                        .map_err(io::Error::other)?,
                );
            }
            self.arrivals
                .entry(packet.peer)
                .and_modify(|stats| stats.receive(packet.sequence, packet.due))
                .or_insert_with(|| ArrivalStats::new(packet.sequence, packet.due));
            if let Some(peer) = self.peers.get_mut(&packet.peer) {
                peer.insert(packet.sequence, &packet.chunk, now)
                    .map_err(io::Error::other)?;
            }
        }
        self.peers.retain(|id, peer| {
            let keep = now.duration_since(peer.last_received) < SERVER_TIMEOUT;
            if !keep {
                self.retired_diagnostics.accumulate(peer.diagnostics);
                speaker.remove(*id);
            }
            keep
        });
        self.arrivals.retain(|id, _| self.peers.contains_key(id));
        if self.peers.is_empty() {
            self.next_playout = None;
            return Ok(());
        }
        let next = self
            .next_playout
            .get_or_insert_with(|| self.peers.values().map(|p| p.ready_at).min().unwrap_or(now));
        if now < *next {
            return Ok(());
        }
        let deadline = *next;
        // Preserve the fractional clock phase, including when bounded catch-up is possible.
        let phase = now.duration_since(*next).as_nanos() % CHUNK_TIME.as_nanos();
        let remaining = CHUNK_TIME
            .checked_sub(Duration::from_nanos(u64::try_from(phase).unwrap_or(0)))
            .unwrap_or(CHUNK_TIME);
        *next = now + remaining;
        for (&id, peer) in &mut self.peers {
            // The sole consumer can only free slots while this batch is decoded.
            // Reserving whole chunks here prevents pop() from losing decoded audio.
            let frames = peer
                .playout_frames(deadline, now, speaker.available_chunks(id))
                .map_err(io::Error::other)?;
            for _ in 0..frames {
                if let Some(chunk) = peer.pop(now).map_err(io::Error::other)? {
                    if !speaker.push(id, &chunk) {
                        peer.diagnostics.rx_push_dropped =
                            peer.diagnostics.rx_push_dropped.saturating_add(1);
                    }
                }
            }
        }
        Ok(())
    }

    fn send(&self, packet: &[u8]) -> io::Result<()> {
        match self.socket.send(packet) {
            Ok(length) if length == packet.len() => Ok(()),
            Ok(_) => Err(io::Error::other("Incomplete UDP datagram send")),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn control(&self, message: &ControlMessage) -> io::Result<()> {
        let mut packet = [0; MAX_PACKET_SIZE];
        let length = message.to_bytes(&mut packet).map_err(io::Error::other)?;
        self.send(&packet[..length])
    }

    fn observe_tick(&mut self, now: Instant) {
        if let Some(last) = self.last_tick.replace(now) {
            self.worker_max_gap_ms = self
                .worker_max_gap_ms
                .max(now.saturating_duration_since(last).as_secs_f64() * 1000.0);
        }
    }

    fn pong(&mut self, stamp: u64, server_timestamp_us: u64, now: Instant) {
        if let Some((pending, sent)) = self.pending_ping {
            if pending == stamp {
                let simulated_rtt =
                    Duration::from_millis(u64::from(self.settings.delay_ms).saturating_mul(2));
                let elapsed = now.duration_since(sent).saturating_add(simulated_rtt);
                let rtt = elapsed.as_secs_f64() * 1000.0;
                self.rtt = Some((rtt, now));
                self.rtt_max_ms = Some(self.rtt_max_ms.map_or(rtt, |peak| peak.max(rtt)));
                self.pending_ping = None;
                self.clock.observe(stamp, server_timestamp_us, elapsed);
            }
        }
    }

    pub(crate) fn tick(
        &mut self,
        mic: &mut Consumer<f32>,
        speaker: &mut RxChannels,
        now: Instant,
    ) -> io::Result<()> {
        self.observe_tick(now);
        if self.peer_id.is_none()
            && self
                .last_join
                .is_none_or(|last| now.duration_since(last) >= JOIN_INTERVAL)
        {
            self.control(&self.join)?;
            self.last_join = Some(now);
        }
        // Bound receive work so a busy room cannot starve capture or shutdown.
        let mut packet = [0; MAX_PACKET_SIZE];
        for _ in 0..64 {
            let length = match self.socket.recv(&mut packet) {
                Ok(length) => length,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error)
                    if error.kind() == io::ErrorKind::ConnectionRefused
                        && self.peer_id.is_none() =>
                {
                    break;
                }
                // Winsock reports WSAEMSGSIZE after discarding an oversized datagram.
                #[cfg(windows)]
                Err(error) if error.raw_os_error() == Some(10040) => continue,
                Err(error) => return Err(error),
            };
            let bytes = &packet[..length];
            if let Ok(message) = ControlMessage::from_bytes(bytes) {
                match message {
                    ControlMessage::JoinAccepted {
                        assigned_peer_id,
                        sample_rate,
                    } => {
                        if sample_rate != SAMPLE_RATE {
                            return Err(io::Error::other("Server sample rate must be 48000 Hz"));
                        }
                        if self.peer_id.is_some_and(|id| id != assigned_peer_id) {
                            return Err(io::Error::other("Server changed peer ID"));
                        }
                        if self.peer_id.is_none() {
                            println!("Joined server as peer {assigned_peer_id}");
                        }
                        self.roster_changed |= self.peer_id.is_none();
                        self.peer_id = Some(assigned_peer_id);
                        self.last_server = now;
                    }
                    ControlMessage::PeerListUpdate { peers } if self.peer_id.is_some() => {
                        self.update_roster(&peers, speaker);
                        self.last_server = now;
                    }
                    ControlMessage::Pong {
                        client_timestamp_us,
                        server_timestamp_us,
                    } if self.peer_id.is_some() => {
                        self.pong(client_timestamp_us, server_timestamp_us, now);
                        self.last_server = now;
                    }
                    // State can arrive before JoinAccepted because UDP can reorder packets.
                    ControlMessage::MetronomeState { state } => {
                        self.metronome = Some(state);
                        self.last_server = now;
                    }
                    _ => {}
                }
                continue;
            }
            if let Some(id) = self.peer_id {
                self.incoming(bytes, id, now);
            }
        }
        self.playback(speaker, now)?;
        self.sync_metronome(speaker, now);
        self.publish_telemetry(now);
        let Some(id) = self.peer_id else {
            if now.duration_since(self.started) >= SERVER_TIMEOUT {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Server did not accept the connection within 5 seconds",
                ));
            }
            // Capture continues during join; do not replay stale audio after acceptance.
            for _ in 0..mic.slots() {
                let _ = mic.pop();
            }
            return Ok(());
        };
        if now.duration_since(self.last_server) >= SERVER_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Server has not responded for 5 seconds",
            ));
        }
        if now.duration_since(self.last_ping) >= PING_INTERVAL {
            let stamp =
                u64::try_from(now.duration_since(self.started).as_micros()).unwrap_or(u64::MAX);
            self.control(&ControlMessage::Ping {
                client_timestamp_us: stamp,
            })?;
            self.pending_ping = Some((stamp, now));
            self.last_ping = now;
        }
        self.send_audio(id, mic)
    }

    pub fn take_telemetry(&mut self) -> Option<Telemetry> {
        self.telemetry.take()
    }

    /// Server clock minus local microseconds since this connection started.
    /// Uses the lowest RTT among the last 32 matched Pong samples.
    pub fn clock_offset_us(&self) -> Option<i64> {
        self.clock.offset_us()
    }

    /// Estimated room time, in the same clock domain as the metronome anchor.
    pub fn server_timestamp_us(&self, now: Instant) -> Option<u64> {
        let local = i128::try_from(now.checked_duration_since(self.started)?.as_micros()).ok()?;
        u64::try_from(local + i128::from(self.clock_offset_us()?)).ok()
    }

    pub fn metronome_state(&self) -> Option<MetronomeState> {
        self.metronome
    }

    pub fn local_peer_id(&self) -> Option<u8> {
        self.peer_id
    }

    /// The protocol does not report the leader's RTT or hardware. Approximate its
    /// uplink with local RTT/2, plus local RTT/2 for the SFU downlink; use local
    /// capture hardware as the leader's capture proxy. These are estimates.
    fn leader_phase_delay_samples(&self, config: MetronomeConfig, speaker: &RxChannels) -> u32 {
        if config.leader_peer_id == 0 || self.peer_id.is_none_or(|id| id == config.leader_peer_id) {
            return 0;
        }
        let network_ms = self.rtt.map_or(0.0, |(ms, _)| ms);
        let jitter_samples = self
            .peers
            .get(&config.leader_peer_id)
            .map_or(0, |peer| peer.target_chunks() * FRAME_SAMPLES);
        let output_frames = speaker.output_frames.load(Ordering::Relaxed) as usize;
        let local_samples = speaker.capture_frames
            + output_frames
            + FRAME_SAMPLES
            + self.codec_lookahead_samples as usize
            + jitter_samples
            + crate::mixer::rx_target_samples(output_frames);
        // Use the stable adaptive target, never packet-by-packet queue occupancy.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let transit_samples = (network_ms * f64::from(SAMPLE_RATE) / 1000.0).round() as u32;
        transit_samples.saturating_add(u32::try_from(local_samples).unwrap_or(u32::MAX))
    }

    fn sync_metronome(&mut self, speaker: &mut RxChannels, now: Instant) {
        let delay = self.metronome_state().map_or(0, |state| {
            self.leader_phase_delay_samples(state.config, speaker)
        });
        speaker
            .metronome
            .phase_delay_samples
            .store(delay, Ordering::Relaxed);
        let (Some(state), Some(offset), Some(server_us)) = (
            self.metronome_state(),
            self.clock_offset_us(),
            self.server_timestamp_us(now),
        ) else {
            return; // Stay silent until both room state and clock are known.
        };
        if self.rendered_metronome != Some((state, offset)) {
            if self.rendered_metronome.is_some_and(|(old, old_offset)| {
                old.anchor_server_timestamp_us == state.anchor_server_timestamp_us
                    && old_offset == offset
            }) {
                speaker.configure_metronome(state.config);
            } else {
                let elapsed = i128::from(server_us) - i128::from(state.anchor_server_timestamp_us);
                speaker.sync_metronome(state.config, elapsed);
            }
            self.rendered_metronome = Some((state, offset));
        }
    }

    /// Sends a room configuration command; local state changes on server confirmation.
    pub fn update_metronome(&self, config: MetronomeConfig) -> io::Result<()> {
        if self.peer_id.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Not joined to a room",
            ));
        }
        self.control(&ControlMessage::MetronomeUpdate { config })
    }

    fn publish_telemetry(&mut self, now: Instant) {
        if now.duration_since(self.last_telemetry) < Duration::from_millis(500) {
            return;
        }
        self.last_telemetry = now;
        let expected: f64 = self.arrivals.values().map(|s| f64::from(s.expected)).sum();
        let lost: f64 = self.arrivals.values().map(|s| f64::from(s.lost)).sum();
        let mut diagnostics = self.retired_diagnostics;
        for peer in self.peers.values() {
            diagnostics.accumulate(peer.diagnostics);
        }
        self.telemetry = Some(Telemetry {
            latency: None,
            diagnostics,
            audio: crate::audio::AudioTelemetry::default(),
            worker_max_gap_ms: std::mem::take(&mut self.worker_max_gap_ms),
            rtt_max_ms: self.rtt_max_ms.take(),
            rtt_ms: self
                .rtt
                .filter(|(_, at)| now.duration_since(*at) < SERVER_TIMEOUT)
                .map(|(rtt, _)| rtt),
            jitter_ms: self
                .arrivals
                .values()
                .map(|s| s.jitter_ms)
                .fold(0.0, f64::max),
            loss_percent: if expected > 0.0 {
                100.0 * lost / expected
            } else {
                0.0
            },
            buffer_ms: self
                .peers
                .values()
                .map(PeerJitterBuffer::buffered_ms)
                .fold(0.0, f64::max),
        });
    }

    fn send_audio(&mut self, id: u8, mic: &mut Consumer<f32>) -> io::Result<()> {
        for _ in 0..32 {
            if mic.slots() < FRAME_SAMPLES {
                break;
            }
            let mut packet = [0; PACKET_SIZE];
            AudioHeader::new(id, STREAM_PRIMARY, self.sequence, self.timestamp)
                .to_bytes(&mut packet)
                .map_err(io::Error::other)?;
            let mut samples = [0.0; FRAME_SAMPLES];
            for sample in &mut samples {
                let captured = mic.pop().map_err(io::Error::other)?;
                *sample = if captured.is_finite() {
                    captured.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
            }
            let length = self
                .encoder
                .encode_float(&samples, &mut packet[AUDIO_HEADER_SIZE..])
                .map_err(io::Error::other)?;
            self.send(&packet[..AUDIO_HEADER_SIZE + length])?;
            self.sequence = self.sequence.wrapping_add(1);
            self.timestamp = self.timestamp.wrapping_add(120);
        }
        Ok(())
    }

    pub fn run(
        mut self,
        mut mic: Consumer<f32>,
        mut speaker: RxChannels,
        stop: &AtomicBool,
    ) -> io::Result<()> {
        // Keep the platform guard on the worker that owns its native registration.
        let _priority = match crate::scheduling::udp_worker() {
            Ok(priority) => Some(priority),
            Err(error) => {
                eprintln!("UDP worker scheduling: {error}");
                None
            }
        };
        let result = (|| {
            while !stop.load(Ordering::Relaxed) {
                self.tick(&mut mic, &mut speaker, Instant::now())?;
                thread::sleep(Duration::from_micros(250));
            }
            Ok(())
        })();
        self.disconnect();
        result
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.peer_id.is_some()
    }

    pub(crate) fn disconnect(&self) {
        if let Some(peer_id) = self.peer_id {
            let _ = self.control(&ControlMessage::Leave { peer_id });
        }
    }
}

fn receive_audio(packet: &[u8], local_id: u8) -> Option<(AudioHeader, Encoded)> {
    let header = AudioHeader::from_bytes(packet).ok()?;
    if packet.len() != PACKET_SIZE
        || header.peer_id == local_id
        || header.stream_id != STREAM_PRIMARY
        || header.reserved != 0
    {
        return None;
    }
    let payload = &packet[AUDIO_HEADER_SIZE..];
    if !codec::valid_payload(payload) {
        return None;
    }
    Some((header, payload.try_into().ok()?))
}

#[cfg(test)]
mod tests;
