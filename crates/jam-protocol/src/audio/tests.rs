use crate::{
    AUDIO_HEADER_SIZE, AudioHeader, MAX_PACKET_SIZE, ProtocolError, STREAM_PRIMARY, STREAM_TALKBACK,
};

#[test]
fn header_has_fixed_size_and_explicit_little_endian_wire_layout() {
    assert_eq!(size_of::<AudioHeader>(), 12);
    assert_eq!(align_of::<AudioHeader>(), 4);
    assert_eq!(std::mem::offset_of!(AudioHeader, sequence_number), 4);
    assert_eq!(std::mem::offset_of!(AudioHeader, timestamp_samples), 8);
    assert_eq!(std::mem::offset_of!(AudioHeader, stream_id), 10);
    assert_eq!(std::mem::offset_of!(AudioHeader, reserved), 11);
    let header = AudioHeader::new(8, STREAM_PRIMARY, 0x0403_0201, 0x0605);
    let expected = [0x41, 0x4A, 0x01, 8, 1, 2, 3, 4, 5, 6, STREAM_PRIMARY, 0];
    let mut wire = [0; AUDIO_HEADER_SIZE];
    assert_eq!(header.to_bytes(&mut wire), Ok(12));
    assert_eq!(wire, expected);
    assert_eq!(AudioHeader::from_bytes(&expected), Ok(header));
}

#[test]
fn headers_round_trip_including_counter_boundaries() {
    for peer_id in 1..=8 {
        for sequence_number in [0, 1, u32::MAX - 1, u32::MAX] {
            for timestamp_samples in [0, 120, u16::MAX] {
                for stream_id in [STREAM_PRIMARY, STREAM_TALKBACK, u8::MAX] {
                    let mut header =
                        AudioHeader::new(peer_id, stream_id, sequence_number, timestamp_samples);
                    for reserved in [0, u8::MAX] {
                        header.reserved = reserved;
                        let mut wire = [0; AUDIO_HEADER_SIZE];
                        header.to_bytes(&mut wire).unwrap();
                        assert_eq!(wire[10], stream_id);
                        assert_eq!(wire[11], reserved);
                        assert_eq!(AudioHeader::from_bytes(&wire), Ok(header));
                    }
                }
            }
        }
    }
}

#[test]
fn sequence_and_sample_clocks_wrap_without_panicking() {
    let mut header = AudioHeader::new(1, STREAM_PRIMARY, u32::MAX, u16::MAX - 119);
    for expected in [u32::MAX, 0, 1] {
        let mut wire = [0; AUDIO_HEADER_SIZE];
        header.to_bytes(&mut wire).unwrap();
        let decoded = AudioHeader::from_bytes(&wire).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(decoded.sequence_number, expected);
        header.sequence_number = header.sequence_number.wrapping_add(1);
        header.timestamp_samples = header.timestamp_samples.wrapping_add(120);
    }
    assert_eq!(header.timestamp_samples, 240);
}

#[test]
fn every_truncated_input_and_short_output_is_rejected() {
    let header = AudioHeader::new(1, STREAM_PRIMARY, 7, 120);
    let mut wire = [0; AUDIO_HEADER_SIZE];
    header.to_bytes(&mut wire).unwrap();
    for length in 0..AUDIO_HEADER_SIZE {
        assert_eq!(
            AudioHeader::from_bytes(&wire[..length]),
            Err(ProtocolError::TruncatedPacket)
        );
        let mut output = [0xCC; AUDIO_HEADER_SIZE];
        assert_eq!(
            header.to_bytes(&mut output[..length]),
            Err(ProtocolError::BufferTooSmall)
        );
        assert_eq!(output, [0xCC; AUDIO_HEADER_SIZE]);
    }
}

#[test]
fn payload_is_preserved_and_unaligned_input_is_supported() {
    let header = AudioHeader::new(2, STREAM_PRIMARY, 123, 456);
    let mut storage = [0xCC; MAX_PACKET_SIZE];
    // Offset by one byte so parsing cannot rely on native u32 alignment.
    assert_eq!(header.to_bytes(&mut storage[1..]), Ok(AUDIO_HEADER_SIZE));
    assert_eq!(AudioHeader::from_bytes(&storage[1..]), Ok(header));
    assert_eq!(storage[0], 0xCC);
    assert!(storage[1 + AUDIO_HEADER_SIZE..].iter().all(|&b| b == 0xCC));
    // Output capacity may exceed MTU; the sender bounds the final datagram.
    assert_eq!(header.to_bytes(&mut [0; 1600]), Ok(AUDIO_HEADER_SIZE));
    for size in [MAX_PACKET_SIZE, MAX_PACKET_SIZE + 1] {
        assert_eq!(
            AudioHeader::from_bytes(&vec![0; size]),
            Err(ProtocolError::PacketTooLarge)
        );
    }
}

#[test]
fn invalid_fields_are_rejected_on_both_read_and_write() {
    let valid = AudioHeader::new(1, STREAM_PRIMARY, 0, 0);
    let mut cases = vec![
        (
            AudioHeader {
                magic: [0, 0],
                ..valid
            },
            ProtocolError::InvalidMagic,
        ),
        (
            AudioHeader {
                magic: [0x4A, 0x41],
                ..valid
            },
            ProtocolError::InvalidMagic,
        ),
    ];
    for packet_type in [0, 2, 7, 255] {
        cases.push((
            AudioHeader {
                packet_type,
                ..valid
            },
            ProtocolError::UnknownMessageType(packet_type),
        ));
    }
    for peer_id in [0, 9, 255] {
        cases.push((
            AudioHeader { peer_id, ..valid },
            ProtocolError::InvalidPeerId(peer_id),
        ));
    }
    for (header, error) in cases {
        let mut output = [0xCC; AUDIO_HEADER_SIZE];
        assert_eq!(header.to_bytes(&mut output), Err(error));
        assert_eq!(output, [0xCC; AUDIO_HEADER_SIZE]);
        valid.to_bytes(&mut output).unwrap();
        output[..2].copy_from_slice(&header.magic);
        output[2] = header.packet_type;
        output[3] = header.peer_id;
        assert_eq!(AudioHeader::from_bytes(&output), Err(error));
    }
}
