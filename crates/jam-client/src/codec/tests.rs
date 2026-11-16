use super::*;

pub(crate) fn tone(amplitude: f32) -> Chunk {
    std::array::from_fn(|i| {
        #[allow(clippy::cast_precision_loss)]
        let phase = i as f32 * std::f32::consts::TAU / FRAME_SAMPLES as f32;
        amplitude * phase.sin()
    })
}

pub(crate) fn encode(encoder: &mut Encoder, amplitude: f32) -> Encoded {
    let mut bytes = [0; PAYLOAD_SIZE];
    assert_eq!(
        encoder.encode_float(&tone(amplitude), &mut bytes).unwrap(),
        PAYLOAD_SIZE
    );
    bytes
}

pub(crate) fn decode(bytes: &[u8]) -> Chunk {
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
    let mut chunk = [0.0; FRAME_SAMPLES];
    assert_eq!(
        decoder.decode_float(bytes, &mut chunk, false).unwrap(),
        FRAME_SAMPLES
    );
    chunk
}

#[test]
fn low_delay_cbr_roundtrip_has_exact_size_duration_and_bounded_error() {
    let mut encoder = encoder().unwrap();
    assert_eq!(encoder.get_application().unwrap(), Application::LowDelay);
    assert_eq!(encoder.get_bitrate().unwrap(), Bitrate::Bits(96_000));
    assert!(!encoder.get_vbr().unwrap());
    let lookahead = usize::try_from(encoder.get_lookahead().unwrap()).unwrap();
    assert!(lookahead <= FRAME_SAMPLES);
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, Channels::Mono).unwrap();
    let mut output = Vec::new();
    for _ in 0..40 {
        let bytes = encode(&mut encoder, 0.5);
        assert!(valid_payload(&bytes));
        let mut chunk = [0.0; FRAME_SAMPLES];
        assert_eq!(
            decoder.decode_float(&bytes, &mut chunk, false).unwrap(),
            FRAME_SAMPLES
        );
        output.extend(chunk);
    }
    let expected = tone(0.5);
    let error: f32 = output[FRAME_SAMPLES * 10..]
        .iter()
        .enumerate()
        .map(|(i, sample)| {
            let original = expected[(i + FRAME_SAMPLES - lookahead) % FRAME_SAMPLES];
            (sample - original).powi(2)
        })
        .sum();
    #[allow(clippy::cast_precision_loss)]
    let mse = error / (output.len() - FRAME_SAMPLES * 10) as f32;
    assert!(mse < 0.005, "roundtrip MSE {mse}");
}

#[test]
fn rejects_empty_wrong_duration_stereo_and_pcm_payloads() {
    assert!(!valid_payload(&[]));
    assert!(!valid_payload(&[0; 512]));
    let mut encoder = encoder().unwrap();
    let mut bytes = [0; 120];
    let length = encoder.encode_float(&[0.1; 240], &mut bytes).unwrap();
    assert!(!valid_payload(&bytes[..length]));
    let mut stereo = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::LowDelay).unwrap();
    stereo.set_bitrate(Bitrate::Bits(96_000)).unwrap();
    stereo.set_vbr(false).unwrap();
    let length = stereo
        .encode_float(&[0.1; FRAME_SAMPLES * 2], &mut bytes)
        .unwrap();
    assert!(!valid_payload(&bytes[..length]));
}
