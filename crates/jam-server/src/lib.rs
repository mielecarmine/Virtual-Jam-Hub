//! UDP audio reflector, room membership and watchdog for the headless Virtual Jam server.

mod session;

use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use jam_protocol::{AudioHeader, ControlMessage, MAX_PACKET_SIZE, MetronomeState, SAMPLE_RATE};
use session::SessionTable;

/// Maximum idle wait between watchdog scans, even when no packets arrive.
pub const RECEIVE_TIMEOUT: Duration = Duration::from_millis(100);
/// A session is removed once its inactivity exceeds this interval.
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(5);

/// Single-threaded UDP server. No audio processing runs on this thread.
pub struct Server {
    socket: UdpSocket,
    sessions: SessionTable,
    started: Instant,
    metronome: MetronomeState,
    last_reflection_duration: Option<Duration>,
}

impl Server {
    /// Binds a UDP socket with a bounded receive timeout for watchdog maintenance.
    ///
    /// # Errors
    /// Returns socket binding or timeout configuration errors.
    pub fn bind(address: impl ToSocketAddrs) -> io::Result<Self> {
        let socket = UdpSocket::bind(address)?;
        socket.set_read_timeout(Some(RECEIVE_TIMEOUT))?;
        Ok(Self {
            socket,
            sessions: SessionTable::default(),
            started: Instant::now(),
            metronome: MetronomeState::default(),
            last_reflection_duration: None,
        })
    }

    /// Returns the bound endpoint, including the actual port for an ephemeral bind.
    ///
    /// # Errors
    /// Returns the underlying socket address query error.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Time from the last audio receive to completion of its forwarding attempts.
    /// Includes validation and watchdog work, but excludes waiting for a packet.
    /// Returns `None` when the latest poll did not accept an audio frame.
    #[must_use]
    pub const fn last_reflection_duration(&self) -> Option<Duration> {
        self.last_reflection_duration
    }

    /// Receives at most one datagram and scans the watchdog on every iteration.
    /// Invalid, oversized and unsolicited packets do not keep sessions alive.
    ///
    /// # Errors
    /// Returns fatal receive errors. Individual reply failures are logged and do
    /// not stop the room or prevent notifications to the remaining participants.
    pub fn poll(&mut self) -> io::Result<()> {
        // A ceiling-sized buffer also detects larger, truncated UDP datagrams:
        // every valid packet must be strictly smaller than MAX_PACKET_SIZE.
        let mut buffer = [0; MAX_PACKET_SIZE];
        let received = self.socket.recv_from(&mut buffer);
        let now = Instant::now();
        self.last_reflection_duration = None;
        self.expire_sessions(now);
        match received {
            Ok((length, source)) if length < MAX_PACKET_SIZE => {
                self.handle(&buffer[..length], source, now);
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            // Winsock consumes oversized UDP datagrams but reports WSAEMSGSIZE.
            #[cfg(windows)]
            Err(error) if error.raw_os_error() == Some(10040) => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn handle(&mut self, packet: &[u8], source: SocketAddr, now: Instant) {
        if let Ok(header) = AudioHeader::from_bytes(packet) {
            if self.sessions.touch(source, header.peer_id, now) {
                for destination in self
                    .sessions
                    .addresses()
                    .filter(|address| *address != source)
                {
                    if let Err(error) = self.socket.send_to(packet, destination) {
                        eprintln!("UDP audio to {destination} failed: {error}");
                    }
                }
                self.last_reflection_duration = Some(now.elapsed());
            }
            return;
        }
        let Ok(message) = ControlMessage::from_bytes(packet) else {
            return;
        };
        match message {
            ControlMessage::JoinRequest {
                nickname,
                nickname_len,
            } => {
                let Some(peer_id) = self.sessions.join(source, nickname, nickname_len, now) else {
                    // The current protocol has no room-full rejection message.
                    return;
                };
                self.send(
                    &ControlMessage::JoinAccepted {
                        assigned_peer_id: peer_id,
                        sample_rate: SAMPLE_RATE,
                    },
                    source,
                );
                self.broadcast_peers();
                self.send(
                    &ControlMessage::MetronomeState {
                        state: self.metronome,
                    },
                    source,
                );
            }
            ControlMessage::Ping {
                client_timestamp_us,
            } => {
                if self.sessions.touch_source(source, now) {
                    self.send(
                        &ControlMessage::Pong {
                            client_timestamp_us,
                            server_timestamp_us: u64::try_from(
                                now.duration_since(self.started).as_micros(),
                            )
                            .unwrap_or(u64::MAX),
                        },
                        source,
                    );
                }
            }
            ControlMessage::Leave { peer_id } => {
                if self.sessions.remove(source, peer_id) {
                    self.broadcast_peers();
                    self.reset_missing_leader();
                }
            }
            ControlMessage::MetronomeUpdate { config } => {
                // Only room members may change tempo; a designated leader must be active.
                if (config.leader_peer_id == 0
                    || self.sessions.contains_peer(config.leader_peer_id))
                    && self.sessions.touch_source(source, now)
                {
                    if (config.enabled && !self.metronome.config.enabled)
                        || config.bpm != self.metronome.config.bpm
                    {
                        self.metronome.anchor_server_timestamp_us =
                            u64::try_from(now.duration_since(self.started).as_micros())
                                .unwrap_or(u64::MAX);
                    }
                    self.metronome.config = config;
                    self.broadcast_metronome();
                }
            }
            ControlMessage::JoinAccepted { .. }
            | ControlMessage::PeerListUpdate { .. }
            | ControlMessage::Pong { .. }
            | ControlMessage::MetronomeState { .. } => {}
        }
    }

    fn expire_sessions(&mut self, now: Instant) {
        if self.sessions.expire(now) {
            self.broadcast_peers();
            self.reset_missing_leader();
        }
    }

    fn reset_missing_leader(&mut self) {
        let leader = self.metronome.config.leader_peer_id;
        if leader != 0 && !self.sessions.contains_peer(leader) {
            self.metronome.config.leader_peer_id = 0;
            self.broadcast_metronome();
        }
    }

    fn broadcast_metronome(&self) {
        for destination in self.sessions.addresses() {
            self.send(
                &ControlMessage::MetronomeState {
                    state: self.metronome,
                },
                destination,
            );
        }
    }

    fn broadcast_peers(&self) {
        let message = ControlMessage::PeerListUpdate {
            peers: self.sessions.peers(),
        };
        for source in self.sessions.addresses() {
            self.send(&message, source);
        }
    }

    fn send(&self, message: &ControlMessage, destination: SocketAddr) {
        let mut buffer = [0; MAX_PACKET_SIZE];
        match message.to_bytes(&mut buffer) {
            Ok(length) => {
                if let Err(error) = self.socket.send_to(&buffer[..length], destination) {
                    eprintln!("UDP reply to {destination} failed: {error}");
                }
            }
            Err(error) => eprintln!("Cannot encode server message: {error}"),
        }
    }
}

#[cfg(test)]
mod tests;
