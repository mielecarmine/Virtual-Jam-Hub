//! Fixed audio header. Multi-byte wire fields use explicit little endian.
//!
//! Reads and writes operate directly on slices without heap allocation or unsafe
//! casts, including for unaligned input. The native struct must not be sent as raw
//! memory: its integer fields use host endianness. Payload bytes remain untouched.

use crate::{MAX_PACKET_SIZE, PROTOCOL_MAGIC, ProtocolError, control::validate_peer};

/// Number of bytes preceding the encoded audio payload.
pub const AUDIO_HEADER_SIZE: usize = 12;

/// Wire discriminator for an audio frame.
pub const AUDIO_PACKET_TYPE: u8 = 0x01;

/// Header shared by every audio frame, with a deterministic 12-byte C layout.
///
/// Sequence numbers and sample timestamps wrap modulo 2^32. Senders must advance
/// them with `wrapping_add`; consumers must account for wraparound when ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct AudioHeader {
    pub magic: [u8; 2],
    pub packet_type: u8,
    pub peer_id: u8,
    pub sequence_number: u32,
    pub timestamp_samples: u32,
}

const _: () = assert!(size_of::<AudioHeader>() == AUDIO_HEADER_SIZE);

impl AudioHeader {
    /// Creates an audio header. Peer IDs are validated when encoding.
    #[must_use]
    pub const fn new(peer_id: u8, sequence_number: u32, timestamp_samples: u32) -> Self {
        Self {
            magic: PROTOCOL_MAGIC.to_le_bytes(),
            packet_type: AUDIO_PACKET_TYPE,
            peer_id,
            sequence_number,
            timestamp_samples,
        }
    }

    fn validate(&self) -> Result<(), ProtocolError> {
        if self.magic != PROTOCOL_MAGIC.to_le_bytes() {
            return Err(ProtocolError::InvalidMagic);
        }
        if self.packet_type != AUDIO_PACKET_TYPE {
            return Err(ProtocolError::UnknownMessageType(self.packet_type));
        }
        validate_peer(self.peer_id)
    }

    /// Writes only the header prefix, returning 12. Audio payload is written separately.
    /// Only the final datagram length, rather than output capacity, must be below MTU.
    ///
    /// # Errors
    /// Rejects invalid fields or buffers shorter than 12 bytes before writing anything.
    pub fn to_bytes(&self, buf: &mut [u8]) -> Result<usize, ProtocolError> {
        self.validate()?;
        if buf.len() < AUDIO_HEADER_SIZE {
            return Err(ProtocolError::BufferTooSmall);
        }
        buf[..2].copy_from_slice(&self.magic);
        buf[2] = self.packet_type;
        buf[3] = self.peer_id;
        buf[4..8].copy_from_slice(&self.sequence_number.to_le_bytes());
        buf[8..12].copy_from_slice(&self.timestamp_samples.to_le_bytes());
        Ok(AUDIO_HEADER_SIZE)
    }

    /// Reads the header from a header-only slice or complete audio datagram.
    /// Bytes after the first 12 belong to the payload and are not inspected or copied.
    ///
    /// # Errors
    /// Rejects truncated or oversized datagrams and invalid magic, type or peer ID.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.len() >= MAX_PACKET_SIZE {
            return Err(ProtocolError::PacketTooLarge);
        }
        if buf.len() < AUDIO_HEADER_SIZE {
            return Err(ProtocolError::TruncatedPacket);
        }
        let header = Self {
            magic: [buf[0], buf[1]],
            packet_type: buf[2],
            peer_id: buf[3],
            sequence_number: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            timestamp_samples: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
        };
        header.validate()?;
        Ok(header)
    }
}

#[cfg(test)]
mod tests;
