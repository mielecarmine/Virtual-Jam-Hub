//! Shared protocol constants for Virtual Jam clients and servers.

/// Audio sample rate in samples per second.
pub const SAMPLE_RATE: u32 = 48_000;

/// Maximum number of musicians in a session.
pub const MAX_PEERS: usize = 8;

/// Protocol identifier (ASCII "JA" when written most significant byte first).
pub const PROTOCOL_MAGIC: u16 = 0x4A41;

/// Packet size ceiling in bytes, below the standard network MTU.
///
/// Wire packets must remain strictly smaller than this ceiling.
pub const MAX_PACKET_SIZE: usize = 1400;
