//! Control datagrams. All multi-byte integers, including magic, use little endian.
//!
//! Each datagram starts with the two-byte magic and one-byte message type. Audio
//! reserves type 0x01; control types are 0x02 through 0x0c in enum declaration order.
//! Nicknames occupy exactly 32 bytes followed by their meaningful byte length.
//! Peer lists start with a one-byte count, then count records containing ID,
//! nickname, length, public endpoint and optional local endpoint.
//! Parsing requires exactly one complete datagram, with no trailing bytes.
//!
//! | Type | Message | Payload after magic + type | Total bytes |
//! | --- | --- | --- | --- |
//! | 0x02 | `JoinRequest` | `nickname[32], nickname_len: u8, local_addr[27]` | 63 |
//! | 0x03 | `JoinAccepted` | `assigned_peer_id: u8, sample_rate: u32` | 8 |
//! | 0x04 | `PeerListUpdate` | `count: u8`, followed by peer records | 4 + 88 × count |
//! | 0x05 | `Ping` | `client_timestamp_us: u64` | 11 |
//! | 0x06 | `Pong` | `client_timestamp_us: u64, server_timestamp_us: u64` | 19 |
//! | 0x07 | `Leave` | `peer_id: u8` | 4 |
//! | 0x08 | `MetronomeUpdate` | `enabled: u8, bpm: u16, beats_per_bar: u8, leader_peer_id: u8` | 8 |
//! | 0x09 | `MetronomeState` | same configuration, then `anchor_server_timestamp_us: u64` | 16 |
//! | 0x0a | `P2pProbe` | `sender: u8, target: u8, timestamp: u64` | 13 |
//! | 0x0b | `P2pAck` | `sender: u8, target: u8, echo: u64` | 13 |
//! | 0x0c | `DirectRouteUpdate` | `direct_peers_mask: u8` | 4 |
//!
//! Endpoints use 27 bytes: family (0 absent, 4 IPv4, 6 IPv6), 16 IP octets,
//! port u16 LE, IPv6 flowinfo u32 LE and scope ID u32 LE. Unused bytes are zero.
//! Public endpoints are mandatory. IPv4 uses the first four IP octets.
//! IP octets retain their standard address order. IPv6 scope IDs are local to
//! the advertising host and must not be treated as receiver interface indices.
//! Route bit `peer_id - 1` suppresses relay from the sender to that peer only.
//! This extends `JoinRequest` and `PeerListUpdate` incompatibly; deploy server and
//! clients together. Old datagrams are rejected by exact-length parsing.
//!
//! Metronome booleans are exactly 0 or 1. Commands cannot supply an anchor;
//! the server starts beat zero on enable or BPM change, preserving it otherwise.
//!
//! Magic 0x4A41 is encoded as [0x41, 0x4A]. Nicknames are opaque byte arrays:
//! lengths 0..=32 are accepted, including unused bytes, which are preserved.
//! Peer lists contain at most 8 unique IDs in 1..=8 (maximum datagram: 708 bytes).

use crate::{MAX_PACKET_SIZE, MAX_PEERS, PROTOCOL_MAGIC};
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

/// Fixed nickname storage size, in bytes (not Unicode characters).
pub const NICKNAME_CAPACITY: usize = 32;
const HEADER_SIZE: usize = 3;
const ENDPOINT_SIZE: usize = 27;
const PEER_SIZE: usize = 1 + NICKNAME_CAPACITY + 1 + 2 * ENDPOINT_SIZE;

/// Room tempo configuration. Leader zero selects the absolute room clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetronomeConfig {
    pub enabled: bool,
    pub bpm: u16,
    pub beats_per_bar: u8,
    pub leader_peer_id: u8,
}

impl Default for MetronomeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bpm: 120,
            beats_per_bar: 4,
            leader_peer_id: 0,
        }
    }
}

impl MetronomeConfig {
    /// Checks the wire limits: 30..=300 BPM, 1..=16 beats, leader `0..=MAX_PEERS`.
    ///
    /// # Errors
    /// Returns the first invalid configuration field.
    pub fn validate(self) -> Result<(), ProtocolError> {
        if !(30..=300).contains(&self.bpm) {
            return Err(ProtocolError::InvalidBpm(self.bpm));
        }
        if !(1..=16).contains(&self.beats_per_bar) {
            return Err(ProtocolError::InvalidBeatsPerBar(self.beats_per_bar));
        }
        if usize::from(self.leader_peer_id) > MAX_PEERS {
            return Err(ProtocolError::InvalidPeerId(self.leader_peer_id));
        }
        Ok(())
    }
}

/// Authoritative room phase, expressed in the server's monotonic microseconds.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MetronomeState {
    pub config: MetronomeConfig,
    pub anchor_server_timestamp_us: u64,
}

/// A musician advertised in a peer list. IDs range from 1 through `MAX_PEERS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerInfo {
    pub peer_id: u8,
    pub nickname: [u8; NICKNAME_CAPACITY],
    pub nickname_len: u8,
    pub public_addr: SocketAddr,
    pub local_addr: Option<SocketAddr>,
}

/// Control-plane messages. The enum's memory layout is never sent over the wire.
///
/// `to_bytes` performs no heap allocations. `from_bytes` allocates only for the
/// bounded `PeerListUpdate` vector; decode control messages on the network thread.
#[derive(Debug, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlMessage {
    JoinRequest {
        nickname: [u8; NICKNAME_CAPACITY],
        nickname_len: u8,
        local_addr: Option<SocketAddr>,
    } = 0x02,
    JoinAccepted {
        assigned_peer_id: u8,
        sample_rate: u32,
    } = 0x03,
    PeerListUpdate {
        peers: Vec<PeerInfo>,
    } = 0x04,
    Ping {
        client_timestamp_us: u64,
    } = 0x05,
    Pong {
        client_timestamp_us: u64,
        server_timestamp_us: u64,
    } = 0x06,
    Leave {
        peer_id: u8,
    } = 0x07,
    MetronomeUpdate {
        config: MetronomeConfig,
    } = 0x08,
    MetronomeState {
        state: MetronomeState,
    } = 0x09,
    P2pProbe {
        sender_peer_id: u8,
        target_peer_id: u8,
        client_timestamp_us: u64,
    } = 0x0a,
    P2pAck {
        sender_peer_id: u8,
        target_peer_id: u8,
        echo_timestamp_us: u64,
    } = 0x0b,
    DirectRouteUpdate {
        direct_peers_mask: u8,
    } = 0x0c,
}

/// Invalid input or insufficient output storage for a protocol datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    BufferTooSmall,
    TruncatedPacket,
    PacketTooLarge,
    InvalidMagic,
    UnknownMessageType(u8),
    InvalidNicknameLength(u8),
    InvalidPeerId(u8),
    TooManyPeers,
    DuplicatePeerId(u8),
    TrailingBytes,
    InvalidBpm(u16),
    InvalidBeatsPerBar(u8),
    InvalidBoolean(u8),
    InvalidEndpoint,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoint => f.write_str("invalid endpoint encoding"),
            Self::BufferTooSmall => f.write_str("output buffer is too small"),
            Self::TruncatedPacket => f.write_str("truncated protocol packet"),
            Self::PacketTooLarge => f.write_str("packet must be smaller than MAX_PACKET_SIZE"),
            Self::InvalidMagic => f.write_str("invalid protocol magic"),
            Self::UnknownMessageType(value) => write!(f, "unexpected message type: {value}"),
            Self::InvalidNicknameLength(value) => {
                write!(f, "invalid nickname byte length: {value}")
            }
            Self::InvalidPeerId(value) => write!(f, "invalid peer ID: {value}"),
            Self::TooManyPeers => f.write_str("peer count exceeds MAX_PEERS"),
            Self::DuplicatePeerId(value) => write!(f, "duplicate peer ID: {value}"),
            Self::TrailingBytes => f.write_str("trailing bytes after control message"),
            Self::InvalidBpm(value) => write!(f, "invalid BPM: {value}"),
            Self::InvalidBeatsPerBar(value) => write!(f, "invalid beats per bar: {value}"),
            Self::InvalidBoolean(value) => write!(f, "invalid boolean: {value}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

fn validate_nickname(length: u8) -> Result<(), ProtocolError> {
    if usize::from(length) > NICKNAME_CAPACITY {
        return Err(ProtocolError::InvalidNicknameLength(length));
    }
    Ok(())
}

pub(crate) fn validate_peer(id: u8) -> Result<(), ProtocolError> {
    if id == 0 || usize::from(id) > MAX_PEERS {
        return Err(ProtocolError::InvalidPeerId(id));
    }
    Ok(())
}

impl ControlMessage {
    fn wire_shape(&self) -> Result<(u8, usize), ProtocolError> {
        let shape = match self {
            Self::JoinRequest { nickname_len, .. } => {
                validate_nickname(*nickname_len)?;
                (0x02, NICKNAME_CAPACITY + 1 + ENDPOINT_SIZE)
            }
            Self::JoinAccepted {
                assigned_peer_id, ..
            } => {
                validate_peer(*assigned_peer_id)?;
                (0x03, 5)
            }
            Self::PeerListUpdate { peers } => {
                if peers.len() > MAX_PEERS {
                    return Err(ProtocolError::TooManyPeers);
                }
                let mut seen = [false; MAX_PEERS + 1];
                for peer in peers {
                    validate_peer(peer.peer_id)?;
                    validate_nickname(peer.nickname_len)?;
                    if seen[usize::from(peer.peer_id)] {
                        return Err(ProtocolError::DuplicatePeerId(peer.peer_id));
                    }
                    seen[usize::from(peer.peer_id)] = true;
                }
                (0x04, 1 + peers.len() * PEER_SIZE)
            }
            Self::Ping { .. } => (0x05, 8),
            Self::Pong { .. } => (0x06, 16),
            Self::Leave { peer_id } => {
                validate_peer(*peer_id)?;
                (0x07, 1)
            }
            Self::MetronomeUpdate { config } => {
                config.validate()?;
                (0x08, 5)
            }
            Self::MetronomeState { state } => {
                state.config.validate()?;
                (0x09, 13)
            }
            Self::P2pProbe {
                sender_peer_id,
                target_peer_id,
                ..
            }
            | Self::P2pAck {
                sender_peer_id,
                target_peer_id,
                ..
            } => {
                validate_peer(*sender_peer_id)?;
                validate_peer(*target_peer_id)?;
                (
                    if matches!(self, Self::P2pProbe { .. }) {
                        0x0a
                    } else {
                        0x0b
                    },
                    10,
                )
            }
            Self::DirectRouteUpdate { .. } => (0x0c, 1),
        };
        Ok(shape)
    }

    /// Writes a complete datagram without allocating, returning its byte length.
    /// Only the returned prefix of `buf` should be sent.
    ///
    /// # Errors
    /// Returns an error for invalid message fields or insufficient output capacity.
    /// Validation finishes before any output bytes are modified.
    pub fn to_bytes(&self, buf: &mut [u8]) -> Result<usize, ProtocolError> {
        let (kind, payload_size) = self.wire_shape()?;
        let size = HEADER_SIZE + payload_size;
        if size >= MAX_PACKET_SIZE {
            return Err(ProtocolError::PacketTooLarge);
        }
        if buf.len() < size {
            return Err(ProtocolError::BufferTooSmall);
        }
        buf[..2].copy_from_slice(&PROTOCOL_MAGIC.to_le_bytes());
        buf[2] = kind;
        match self {
            Self::JoinRequest {
                nickname,
                nickname_len,
                local_addr,
            } => {
                buf[3..35].copy_from_slice(nickname);
                buf[35] = *nickname_len;
                write_endpoint(*local_addr, &mut buf[36..size]);
            }
            Self::JoinAccepted {
                assigned_peer_id,
                sample_rate,
            } => {
                buf[3] = *assigned_peer_id;
                buf[4..8].copy_from_slice(&sample_rate.to_le_bytes());
            }
            Self::PeerListUpdate { peers } => {
                // The peer count was bounded above before modifying the buffer.
                buf[3] = u8::try_from(peers.len()).map_err(|_| ProtocolError::TooManyPeers)?;
                for (peer, record) in peers.iter().zip(buf[4..size].chunks_exact_mut(PEER_SIZE)) {
                    record[0] = peer.peer_id;
                    record[1..33].copy_from_slice(&peer.nickname);
                    record[33] = peer.nickname_len;
                    write_endpoint(Some(peer.public_addr), &mut record[34..61]);
                    write_endpoint(peer.local_addr, &mut record[61..88]);
                }
            }
            Self::Ping {
                client_timestamp_us,
            } => {
                buf[3..11].copy_from_slice(&client_timestamp_us.to_le_bytes());
            }
            Self::Pong {
                client_timestamp_us,
                server_timestamp_us,
            } => {
                buf[3..11].copy_from_slice(&client_timestamp_us.to_le_bytes());
                buf[11..19].copy_from_slice(&server_timestamp_us.to_le_bytes());
            }
            Self::P2pProbe {
                sender_peer_id,
                target_peer_id,
                client_timestamp_us,
            }
            | Self::P2pAck {
                sender_peer_id,
                target_peer_id,
                echo_timestamp_us: client_timestamp_us,
            } => {
                buf[3] = *sender_peer_id;
                buf[4] = *target_peer_id;
                buf[5..13].copy_from_slice(&client_timestamp_us.to_le_bytes());
            }
            Self::DirectRouteUpdate { direct_peers_mask } => buf[3] = *direct_peers_mask,
            Self::Leave { peer_id } => buf[3] = *peer_id,
            Self::MetronomeUpdate { config } => write_metronome(*config, buf),
            Self::MetronomeState { state } => {
                write_metronome(state.config, buf);
                buf[8..16].copy_from_slice(&state.anchor_server_timestamp_us.to_le_bytes());
            }
        }
        Ok(size)
    }

    /// Parses exactly one control datagram. Only peer lists allocate (at most 8 records).
    ///
    /// # Errors
    /// Rejects invalid magic/types/fields, truncated or oversized packets and trailing bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.len() >= MAX_PACKET_SIZE {
            return Err(ProtocolError::PacketTooLarge);
        }
        let mut reader = Reader { remaining: buf };
        if u16::from_le_bytes(reader.take()?) != PROTOCOL_MAGIC {
            return Err(ProtocolError::InvalidMagic);
        }
        let message = match reader.byte()? {
            0x02 => {
                let nickname = reader.take()?;
                let nickname_len = reader.byte()?;
                validate_nickname(nickname_len)?;
                Self::JoinRequest {
                    nickname,
                    nickname_len,
                    local_addr: reader.endpoint()?,
                }
            }
            0x03 => {
                let assigned_peer_id = reader.byte()?;
                validate_peer(assigned_peer_id)?;
                Self::JoinAccepted {
                    assigned_peer_id,
                    sample_rate: u32::from_le_bytes(reader.take()?),
                }
            }
            0x04 => Self::PeerListUpdate {
                peers: reader.peers()?,
            },
            0x05 => Self::Ping {
                client_timestamp_us: u64::from_le_bytes(reader.take()?),
            },
            0x06 => Self::Pong {
                client_timestamp_us: u64::from_le_bytes(reader.take()?),
                server_timestamp_us: u64::from_le_bytes(reader.take()?),
            },
            0x07 => {
                let peer_id = reader.byte()?;
                validate_peer(peer_id)?;
                Self::Leave { peer_id }
            }
            0x08 => Self::MetronomeUpdate {
                config: reader.metronome()?,
            },
            0x09 => Self::MetronomeState {
                state: MetronomeState {
                    config: reader.metronome()?,
                    anchor_server_timestamp_us: u64::from_le_bytes(reader.take()?),
                },
            },
            kind @ (0x0a | 0x0b) => {
                let sender_peer_id = reader.byte()?;
                let target_peer_id = reader.byte()?;
                validate_peer(sender_peer_id)?;
                validate_peer(target_peer_id)?;
                let timestamp = u64::from_le_bytes(reader.take()?);
                if kind == 0x0a {
                    Self::P2pProbe {
                        sender_peer_id,
                        target_peer_id,
                        client_timestamp_us: timestamp,
                    }
                } else {
                    Self::P2pAck {
                        sender_peer_id,
                        target_peer_id,
                        echo_timestamp_us: timestamp,
                    }
                }
            }
            0x0c => Self::DirectRouteUpdate {
                direct_peers_mask: reader.byte()?,
            },
            kind => return Err(ProtocolError::UnknownMessageType(kind)),
        };
        if !reader.remaining.is_empty() {
            return Err(ProtocolError::TrailingBytes);
        }
        Ok(message)
    }
}

struct Reader<'a> {
    remaining: &'a [u8],
}

fn write_metronome(config: MetronomeConfig, buf: &mut [u8]) {
    buf[3] = u8::from(config.enabled);
    buf[4..6].copy_from_slice(&config.bpm.to_le_bytes());
    buf[6] = config.beats_per_bar;
    buf[7] = config.leader_peer_id;
}

fn write_endpoint(endpoint: Option<SocketAddr>, buf: &mut [u8]) {
    buf.fill(0);
    match endpoint {
        None => {}
        Some(SocketAddr::V4(addr)) => {
            buf[0] = 4;
            buf[1..5].copy_from_slice(&addr.ip().octets());
            buf[17..19].copy_from_slice(&addr.port().to_le_bytes());
        }
        Some(SocketAddr::V6(addr)) => {
            buf[0] = 6;
            buf[1..17].copy_from_slice(&addr.ip().octets());
            buf[17..19].copy_from_slice(&addr.port().to_le_bytes());
            buf[19..23].copy_from_slice(&addr.flowinfo().to_le_bytes());
            buf[23..27].copy_from_slice(&addr.scope_id().to_le_bytes());
        }
    }
}

impl Reader<'_> {
    fn peers(&mut self) -> Result<Vec<PeerInfo>, ProtocolError> {
        let count = usize::from(self.byte()?);
        if count > MAX_PEERS {
            return Err(ProtocolError::TooManyPeers);
        }
        if self.remaining.len() < count * PEER_SIZE {
            return Err(ProtocolError::TruncatedPacket);
        }
        let mut seen = [false; MAX_PEERS + 1];
        let mut peers = Vec::with_capacity(count);
        for _ in 0..count {
            let peer_id = self.byte()?;
            validate_peer(peer_id)?;
            if seen[usize::from(peer_id)] {
                return Err(ProtocolError::DuplicatePeerId(peer_id));
            }
            seen[usize::from(peer_id)] = true;
            let nickname = self.take()?;
            let nickname_len = self.byte()?;
            validate_nickname(nickname_len)?;
            peers.push(PeerInfo {
                peer_id,
                nickname,
                nickname_len,
                public_addr: self.endpoint()?.ok_or(ProtocolError::InvalidEndpoint)?,
                local_addr: self.endpoint()?,
            });
        }
        Ok(peers)
    }

    fn endpoint(&mut self) -> Result<Option<SocketAddr>, ProtocolError> {
        let bytes: [u8; ENDPOINT_SIZE] = self.take()?;
        let mut reader = Reader {
            remaining: &bytes[1..],
        };
        let ip: [u8; 16] = reader.take()?;
        let port = u16::from_le_bytes(reader.take()?);
        let flowinfo = u32::from_le_bytes(reader.take()?);
        let scope_id = u32::from_le_bytes(reader.take()?);
        match bytes[0] {
            0 if bytes[1..].iter().all(|byte| *byte == 0) => Ok(None),
            4 if ip[4..].iter().all(|byte| *byte == 0) && flowinfo == 0 && scope_id == 0 => {
                Ok(Some(SocketAddr::from((
                    Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]),
                    port,
                ))))
            }
            6 => Ok(Some(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(ip),
                port,
                flowinfo,
                scope_id,
            )))),
            _ => Err(ProtocolError::InvalidEndpoint),
        }
    }

    fn metronome(&mut self) -> Result<MetronomeConfig, ProtocolError> {
        let enabled = match self.byte()? {
            0 => false,
            1 => true,
            value => return Err(ProtocolError::InvalidBoolean(value)),
        };
        let config = MetronomeConfig {
            enabled,
            bpm: u16::from_le_bytes(self.take()?),
            beats_per_bar: self.byte()?,
            leader_peer_id: self.byte()?,
        };
        config.validate()?;
        Ok(config)
    }
    fn take<const N: usize>(&mut self) -> Result<[u8; N], ProtocolError> {
        let bytes = self
            .remaining
            .get(..N)
            .ok_or(ProtocolError::TruncatedPacket)?;
        let value = bytes
            .try_into()
            .map_err(|_| ProtocolError::TruncatedPacket)?;
        self.remaining = &self.remaining[N..];
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take::<1>()?[0])
    }
}

#[cfg(test)]
mod tests;
