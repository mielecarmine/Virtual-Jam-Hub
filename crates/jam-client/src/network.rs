//! Nonblocking UDP worker. All sockets and protocol processing stay off audio callbacks.

use crate::{
    audio::BUFFER_FRAMES,
    jitter::{CHUNK_TIME, Chunk, PeerJitterBuffer},
};
use jam_protocol::{
    AUDIO_HEADER_SIZE, AudioHeader, ControlMessage, MAX_PACKET_SIZE, NICKNAME_CAPACITY,
    SAMPLE_RATE, STREAM_PRIMARY,
};
use rtrb::{Consumer, Producer};
use std::collections::BTreeMap;
use std::{
    io,
    net::{SocketAddr, UdpSocket},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

const PACKET_SIZE: usize = AUDIO_HEADER_SIZE + BUFFER_FRAMES * size_of::<f32>();
const JOIN_INTERVAL: Duration = Duration::from_millis(500);
const PING_INTERVAL: Duration = Duration::from_secs(1);
const SERVER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
pub struct Settings {
    pub target_chunks: usize,
    pub loss: f64,
    pub jitter_ms: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            target_chunks: 3,
            loss: 0.0,
            jitter_ms: 0,
        }
    }
}

struct Delayed {
    due: Instant,
    peer: u8,
    sequence: u32,
    chunk: Chunk,
}

pub struct Network {
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
        if nickname.is_empty() || nickname.len() > NICKNAME_CAPACITY {
            return Err(io::Error::other("Nickname must contain 1..=32 UTF-8 bytes"));
        }
        let socket = UdpSocket::bind(if server.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })?;
        socket.connect(server)?; // Connected UDP filters datagrams from other endpoints.
        socket.set_nonblocking(true)?;
        let mut bytes = [0; NICKNAME_CAPACITY];
        bytes[..nickname.len()].copy_from_slice(nickname.as_bytes());
        let now = Instant::now();
        Ok(Self {
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
        let delay =
            Duration::from_secs_f64(self.random() * f64::from(self.settings.jitter_ms) / 1000.0);
        self.delayed.push(Delayed {
            due: now + delay,
            peer: header.peer_id,
            sequence: header.sequence_number,
            chunk,
        });
    }

    fn playback(&mut self, speaker: &mut Producer<f32>, now: Instant) {
        // Deliver in simulated arrival order even when several deadlines fall in one tick.
        self.delayed
            .sort_unstable_by_key(|packet| std::cmp::Reverse(packet.due));
        while self.delayed.last().is_some_and(|packet| packet.due <= now) {
            let Some(packet) = self.delayed.pop() else {
                break;
            };
            self.peers
                .entry(packet.peer)
                .or_insert_with(|| {
                    PeerJitterBuffer::new(packet.sequence, now, self.settings.target_chunks)
                })
                .insert(packet.sequence, &packet.chunk, now);
        }
        self.peers
            .retain(|_, peer| now.duration_since(peer.last_received) < SERVER_TIMEOUT);
        if self.peers.is_empty() {
            self.next_playout = None;
            return;
        }
        let next = self
            .next_playout
            .get_or_insert_with(|| self.peers.values().map(|p| p.ready_at).min().unwrap_or(now));
        if now < *next {
            return;
        }
        // Never catch up with a burst after a worker stall: discard stale timing debt.
        *next = if now.duration_since(*next) >= CHUNK_TIME {
            now + CHUNK_TIME
        } else {
            *next + CHUNK_TIME
        };
        let mut mixed = [0.0; BUFFER_FRAMES];
        for peer in self.peers.values_mut() {
            if let Some(chunk) = peer.pop(now) {
                for (out, sample) in mixed.iter_mut().zip(chunk) {
                    *out += sample;
                }
            }
        }
        if speaker.slots() >= BUFFER_FRAMES {
            for sample in mixed {
                let _ = speaker.push(sample.clamp(-1.0, 1.0));
            }
        }
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

    fn tick(
        &mut self,
        mic: &mut Consumer<f32>,
        speaker: &mut Producer<f32>,
        now: Instant,
    ) -> io::Result<()> {
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
                        self.peer_id = Some(assigned_peer_id);
                        self.last_server = now;
                    }
                    ControlMessage::PeerListUpdate { peers } if self.peer_id.is_some() => {
                        self.peers
                            .retain(|id, _| peers.iter().any(|p| p.peer_id == *id));
                        self.delayed
                            .retain(|packet| peers.iter().any(|p| p.peer_id == packet.peer));
                        self.last_server = now;
                    }
                    ControlMessage::Pong { .. } if self.peer_id.is_some() => {
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
        self.playback(speaker, now);
        let Some(id) = self.peer_id else {
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
            self.control(&ControlMessage::Ping {
                client_timestamp_us: u64::try_from(now.duration_since(self.started).as_micros())
                    .unwrap_or(u64::MAX),
            })?;
            self.last_ping = now;
        }
        for _ in 0..32 {
            if mic.slots() < BUFFER_FRAMES {
                break;
            }
            let mut packet = [0; PACKET_SIZE];
            AudioHeader::new(id, STREAM_PRIMARY, self.sequence, self.timestamp)
                .to_bytes(&mut packet)
                .map_err(io::Error::other)?;
            for bytes in packet[AUDIO_HEADER_SIZE..].chunks_exact_mut(4) {
                bytes.copy_from_slice(&mic.pop().map_err(io::Error::other)?.to_le_bytes());
            }
            self.send(&packet)?;
            self.sequence = self.sequence.wrapping_add(1);
            self.timestamp = self.timestamp.wrapping_add(128);
        }
        Ok(())
    }

    pub fn run(
        mut self,
        mut mic: Consumer<f32>,
        mut speaker: Producer<f32>,
        stop: &AtomicBool,
    ) -> io::Result<()> {
        let result = (|| {
            while !stop.load(Ordering::Relaxed) {
                self.tick(&mut mic, &mut speaker, Instant::now())?;
                thread::sleep(Duration::from_micros(250));
            }
            Ok(())
        })();
        if let Some(peer_id) = self.peer_id {
            let _ = self.control(&ControlMessage::Leave { peer_id });
        }
        result
    }
}

fn receive_audio(packet: &[u8], local_id: u8) -> Option<(AudioHeader, Chunk)> {
    let header = AudioHeader::from_bytes(packet).ok()?;
    if packet.len() != PACKET_SIZE
        || header.peer_id == local_id
        || header.stream_id != STREAM_PRIMARY
        || header.reserved != 0
    {
        return None;
    }
    let mut samples = [0.0; BUFFER_FRAMES];
    for (sample, bytes) in samples
        .iter_mut()
        .zip(packet[AUDIO_HEADER_SIZE..].chunks_exact(4))
    {
        let Ok(bytes) = bytes.try_into() else {
            return None;
        };
        *sample = f32::from_le_bytes(bytes);
        if !sample.is_finite() || sample.abs() > 1.0 {
            return None;
        }
    }
    Some((header, samples))
}

#[cfg(test)]
mod tests;
