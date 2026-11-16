use super::*;
use rtrb::RingBuffer;

mod allocation;

fn config(
    channels: u16,
    format: SampleFormat,
    buffer: SupportedBufferSize,
) -> cpal::SupportedStreamConfigRange {
    cpal::SupportedStreamConfigRange::new(
        channels,
        SampleRate(44_100),
        SampleRate(96_000),
        buffer,
        format,
    )
}

#[test]
fn selects_f32_48khz_128_frames_and_fewest_channels() {
    let range = SupportedBufferSize::Range { min: 64, max: 256 };
    let selected = select_config(
        [
            config(1, SampleFormat::I16, range),
            config(2, SampleFormat::F32, range),
            config(1, SampleFormat::F32, range),
        ]
        .into_iter(),
    )
    .expect("supported configuration");
    assert_eq!(selected.channels, 1);
    assert_eq!(selected.sample_rate, SampleRate(SAMPLE_RATE));
    assert_eq!(selected.buffer_size, BufferSize::Fixed(128));
}

#[test]
fn rejects_incompatible_rate_format_and_buffer() {
    let fixed = SupportedBufferSize::Range { min: 256, max: 512 };
    assert!(select_config([config(1, SampleFormat::F32, fixed)].into_iter()).is_none());
    assert!(
        select_config([config(1, SampleFormat::I16, SupportedBufferSize::Unknown)].into_iter())
            .is_none()
    );
    let wrong_rate = cpal::SupportedStreamConfigRange::new(
        1,
        SampleRate(44_100),
        SampleRate(44_100),
        SupportedBufferSize::Unknown,
        SampleFormat::F32,
    );
    assert!(select_config([wrong_rate].into_iter()).is_none());
    assert!(select_config(std::iter::empty()).is_none());
    assert!(
        select_config([config(1, SampleFormat::F32, SupportedBufferSize::Unknown)].into_iter())
            .is_some()
    );
}

#[test]
fn loopback_preserves_first_input_channel_and_duplicates_to_stereo_across_wraps() {
    let stats = Stats::default();
    let (mut mic, mut net_input) = RingBuffer::new(RING_CAPACITY);
    let (mut net_output, mut speaker) = RingBuffer::new(RING_CAPACITY);
    let mut input = [0.0; 2 * BUFFER_FRAMES];
    for (index, frame) in input.chunks_exact_mut(2).enumerate() {
        #[allow(clippy::cast_precision_loss)]
        {
            frame[0] = index as f32 / 128.0;
        }
        frame[1] = -1.0;
    }
    let mut output = [0.0; 2 * BUFFER_FRAMES];
    let mut started = false;
    capture(&input, 2, &mut mic, &stats);
    bridge(&mut net_input, &mut net_output);
    // Two blocks establish the small startup cushion.
    for _ in 0..200 {
        capture(&input, 2, &mut mic, &stats);
        assert_eq!(bridge(&mut net_input, &mut net_output), BUFFER_FRAMES);
        playback(&mut output, 2, &mut speaker, &mut started, &stats);
        for (captured, played) in input.chunks_exact(2).zip(output.chunks_exact(2)) {
            assert_eq!(played, [captured[0], captured[0]]);
        }
    }
    assert!(started);
    assert!(!stats.has_errors());
}

#[test]
fn startup_is_silent_then_underflow_zero_fills_and_counts_missing_samples() {
    let stats = Stats::default();
    let (mut producer, mut consumer) = RingBuffer::new(RING_CAPACITY);
    let mut output = [1.0; 2 * BUFFER_FRAMES];
    let mut started = false;
    playback(&mut output, 2, &mut consumer, &mut started, &stats);
    assert_eq!(output, [0.0; 2 * BUFFER_FRAMES]);
    assert!(!started);
    assert!(!stats.has_errors());
    for _ in 0..PREFILL_SAMPLES {
        producer.push(0.5).expect("space");
    }
    for _ in 0..2 {
        playback(&mut output, 2, &mut consumer, &mut started, &stats);
        assert_eq!(output, [0.5; 2 * BUFFER_FRAMES]);
    }
    producer.push(0.25).expect("space");
    playback(&mut output, 2, &mut consumer, &mut started, &stats);
    assert_eq!(&output[..2], &[0.25; 2]);
    assert!(output[2..].iter().all(|sample| *sample == 0.0));
    assert_eq!(stats.output_underflows.load(Ordering::Relaxed), 127);
}

#[test]
fn full_rings_never_block_or_overwrite_queued_audio() {
    let stats = Stats::default();
    let (mut mic, mut net_input) = RingBuffer::new(RING_CAPACITY);
    let (mut net_output, mut speaker) = RingBuffer::new(RING_CAPACITY);
    for _ in 0..RING_CAPACITY / BUFFER_FRAMES {
        capture(&[0.5; BUFFER_FRAMES], 1, &mut mic, &stats);
    }
    capture(&[1.0; BUFFER_FRAMES], 1, &mut mic, &stats);
    assert_eq!(stats.input_overflows.load(Ordering::Relaxed), 128);
    for _ in 0..RING_CAPACITY {
        net_output.push(-0.5).expect("space");
    }
    assert_eq!(bridge(&mut net_input, &mut net_output), 0);
    assert_eq!(net_input.slots(), RING_CAPACITY);
    assert_eq!(speaker.pop(), Ok(-0.5));
    assert_eq!(bridge(&mut net_input, &mut net_output), 1);
    for _ in 0..RING_CAPACITY - 1 {
        assert_eq!(speaker.pop(), Ok(-0.5));
    }
    assert_eq!(speaker.pop(), Ok(0.5));
}

#[test]
fn unexpected_hardware_sizes_are_reported_and_output_is_silenced() {
    let stats = Stats::default();
    let (mut mic, _net_input) = RingBuffer::new(RING_CAPACITY);
    let (_net_output, mut speaker) = RingBuffer::new(RING_CAPACITY);
    capture(&[0.5; 64], 1, &mut mic, &stats);
    let mut output = [1.0; 64];
    playback(&mut output, 1, &mut speaker, &mut false, &stats);
    assert_eq!(output, [0.0; 64]);
    assert_eq!(stats.wrong_buffer_sizes.load(Ordering::Relaxed), 2);
    assert!(stats.has_errors());
}
