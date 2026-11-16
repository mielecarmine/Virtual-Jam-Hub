//! Mono CELT frames. Only the UDP worker owns or calls codec instances.
use jam_protocol::SAMPLE_RATE;
use opus::{Application, Bitrate, Channels, Encoder};

pub const FRAME_SAMPLES: usize = 120;
pub const PAYLOAD_SIZE: usize = 30; // 96 kbit/s CBR for 2.5 ms.
pub type Chunk = [f32; FRAME_SAMPLES];
pub type Encoded = [u8; PAYLOAD_SIZE];

pub fn encoder() -> opus::Result<Encoder> {
    let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::LowDelay)?;
    encoder.set_bitrate(Bitrate::Bits(96_000))?;
    encoder.set_vbr(false)?;
    Ok(encoder)
}

pub fn valid_payload(payload: &[u8]) -> bool {
    payload.len() == PAYLOAD_SIZE
        && matches!(opus::packet::get_nb_channels(payload), Ok(Channels::Mono))
        && opus::packet::get_nb_samples(payload, SAMPLE_RATE).is_ok_and(|n| n == FRAME_SAMPLES)
}

#[cfg(test)]
pub(crate) mod tests;
