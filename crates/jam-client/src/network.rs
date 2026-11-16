//! Nonblocking UDP worker. All sockets and protocol processing stay off audio callbacks.

use crate::audio::BUFFER_FRAMES;
use jam_protocol::{
    AUDIO_HEADER_SIZE, AudioHeader, ControlMessage, MAX_PACKET_SIZE, NICKNAME_CAPACITY,
    SAMPLE_RATE, STREAM_PRIMARY,
};
use rtrb::{Consumer, Producer};
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

pub struct Network {
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
                    ControlMessage::Pong { .. } | ControlMessage::PeerListUpdate { .. }
                        if self.peer_id.is_some() =>
                    {
                        self.last_server = now;
                    }
                    _ => {}
                }
                continue;
            }
            if let Some(id) = self.peer_id {
                receive_audio(bytes, id, speaker);
            }
        }
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

fn receive_audio(packet: &[u8], local_id: u8, speaker: &mut Producer<f32>) {
    let Ok(header) = AudioHeader::from_bytes(packet) else {
        return;
    };
    if packet.len() != PACKET_SIZE
        || header.peer_id == local_id
        || header.stream_id != STREAM_PRIMARY
        || header.reserved != 0
        || speaker.slots() < BUFFER_FRAMES
    {
        return;
    }
    let mut samples = [0.0; BUFFER_FRAMES];
    for (sample, bytes) in samples
        .iter_mut()
        .zip(packet[AUDIO_HEADER_SIZE..].chunks_exact(4))
    {
        let Ok(bytes) = bytes.try_into() else {
            return;
        };
        *sample = f32::from_le_bytes(bytes);
        if !sample.is_finite() || sample.abs() > 1.0 {
            return;
        }
    }
    for sample in samples {
        let _ = speaker.push(sample);
    }
}

#[cfg(test)]
mod tests;
