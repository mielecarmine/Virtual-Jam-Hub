use super::*;
use rtrb::RingBuffer;

mod allocation;

#[test]
fn stalled_local_monitor_does_not_drop_or_change_network_audio() {
    let stats = Stats::default();
    let (mut mic, mut net) = RingBuffer::new(BUFFER_FRAMES);
    let (mut monitor, _playback) = crate::monitor::channel(BUFFER_FRAMES);
    for _ in 0..100 {
        capture(
            &[0.25; BUFFER_FRAMES],
            1,
            &mut mic,
            Some(&mut monitor),
            &stats,
        );
        for _ in 0..BUFFER_FRAMES {
            assert_eq!(net.pop(), Ok(0.25));
        }
    }
    assert_eq!(stats.input_overflows.load(Ordering::Relaxed), 0);
}

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
    let selected = select_input_config(
        [
            config(1, SampleFormat::I16, range),
            config(2, SampleFormat::F32, range),
            config(1, SampleFormat::F32, range),
        ]
        .into_iter(),
    )
    .expect("supported configuration");
    assert_eq!(selected.stream.channels, 1);
    assert_eq!(selected.format, SampleFormat::F32);
    assert_eq!(selected.stream.sample_rate, SampleRate(SAMPLE_RATE));
    assert_eq!(selected.stream.buffer_size, BufferSize::Fixed(128));
}

#[test]
fn output_prefers_stereo_while_input_stays_mono_for_all_buffers() {
    for frames in [64, 128, 256] {
        // ASIO exposes every channel count up to the hardware maximum.
        let configs = [4, 1, 2]
            .map(|channels| config(channels, SampleFormat::F32, SupportedBufferSize::Unknown));
        let output = select_output_config_for_buffer(configs.into_iter(), frames).unwrap();
        let input = select_input_config_for_buffer(configs.into_iter(), frames).unwrap();
        assert_eq!(output.stream.channels, 2);
        assert_eq!(input.stream.channels, 1);
        assert_eq!(output.stream.sample_rate, SampleRate(SAMPLE_RATE));
        assert_eq!(output.stream.buffer_size, BufferSize::Fixed(frames));
        assert_eq!(
            select_output_config(configs.into_iter())
                .unwrap()
                .stream
                .channels,
            2
        );
    }
}

#[test]
fn output_falls_back_to_smallest_multichannel_then_mono() {
    for (channels, expected) in [(&[1, 8, 4][..], 4), (&[8, 4][..], 4), (&[1][..], 1)] {
        let selected =
            select_output_config(channels.iter().map(|&channels| {
                config(channels, SampleFormat::F32, SupportedBufferSize::Unknown)
            }))
            .unwrap();
        assert_eq!(selected.stream.channels, expected);
    }
}

#[test]
fn output_keeps_format_priority_and_selects_stereo_within_each_format() {
    for (configs, expected_channels, expected_format) in [
        (
            [(2, SampleFormat::I16), (1, SampleFormat::F32)],
            1,
            SampleFormat::F32,
        ),
        (
            [(2, SampleFormat::I16), (4, SampleFormat::F32)],
            4,
            SampleFormat::F32,
        ),
        (
            [(1, SampleFormat::I16), (2, SampleFormat::I16)],
            2,
            SampleFormat::I16,
        ),
    ] {
        let selected = select_output_config(
            configs
                .into_iter()
                .map(|(channels, format)| config(channels, format, SupportedBufferSize::Unknown)),
        )
        .unwrap();
        assert_eq!(selected.stream.channels, expected_channels);
        assert_eq!(selected.format, expected_format);
    }
}

#[test]
fn output_filters_incompatible_stereo_before_channel_selection() {
    let mono = config(1, SampleFormat::F32, SupportedBufferSize::Unknown);
    let stereo_buffer = config(
        2,
        SampleFormat::F32,
        SupportedBufferSize::Range { min: 256, max: 512 },
    );
    let stereo_rate = cpal::SupportedStreamConfigRange::new(
        2,
        SampleRate(44_100),
        SampleRate(44_100),
        SupportedBufferSize::Unknown,
        SampleFormat::F32,
    );
    let zero_channels = config(0, SampleFormat::F32, SupportedBufferSize::Unknown);
    let selected =
        select_output_config([stereo_buffer, stereo_rate, zero_channels, mono].into_iter())
            .unwrap();
    assert_eq!(selected.stream.channels, 1);
    assert!(
        select_output_config([stereo_buffer, stereo_rate, zero_channels].into_iter()).is_none()
    );
    assert!(select_output_config(std::iter::empty()).is_none());
}

#[test]
fn playback_duplicates_mono_to_every_output_channel() {
    for channels in [1, 2, 4] {
        let stats = Stats::default();
        let (mut producer, mut consumer) = RingBuffer::new(PREFILL_SAMPLES);
        for sample in 0..PREFILL_SAMPLES {
            producer
                .push(if sample % 2 == 0 { 0.25 } else { -0.5 })
                .unwrap();
        }
        let mut output = vec![0.0_f32; PREFILL_SAMPLES * channels];
        playback(&mut output, channels, &mut consumer, &mut false, &stats);
        for (index, frame) in output.chunks_exact(channels).enumerate() {
            let expected = if index % 2 == 0 { 0.25 } else { -0.5 };
            assert!(frame.iter().all(|&sample| sample == expected));
        }
        assert!(!stats.has_errors());
    }
}

#[test]
fn selects_requested_hardware_buffer_and_rejects_out_of_range_sizes() {
    for frames in [64, 128, 256] {
        let range = config(
            2,
            SampleFormat::F32,
            SupportedBufferSize::Range { min: 64, max: 256 },
        );
        let selected = select_input_config_for_buffer([range].into_iter(), frames).unwrap();
        assert_eq!(selected.stream.buffer_size, BufferSize::Fixed(frames));
        assert_eq!(selected.stream.sample_rate, SampleRate(SAMPLE_RATE));
    }
    let range = config(
        1,
        SampleFormat::F32,
        SupportedBufferSize::Range { min: 128, max: 256 },
    );
    assert!(select_input_config_for_buffer([range].into_iter(), 64).is_none());
}

fn check_hardware_format<T: cpal::SizedSample + FromSample<f32> + std::fmt::Debug + PartialEq>()
where
    f32: FromSample<T>,
{
    let stats = Stats::default();
    let (mut mic, mut net_input) = RingBuffer::new(RING_CAPACITY);
    let (mut net_output, mut speaker) = RingBuffer::new(RING_CAPACITY);
    let mut input = [T::EQUILIBRIUM; PREFILL_SAMPLES * 2];
    for (index, frame) in input.chunks_exact_mut(2).enumerate() {
        frame[0] = T::from_sample(if index % 2 == 0 { -0.5_f32 } else { 0.5 });
        frame[1] = T::EQUILIBRIUM;
    }
    capture(&input, 2, &mut mic, None, &stats);
    assert_eq!(bridge(&mut net_input, &mut net_output), PREFILL_SAMPLES);
    let mut output = [T::EQUILIBRIUM; PREFILL_SAMPLES * 2];
    let mut started = false;
    playback(&mut output, 2, &mut speaker, &mut started, &stats);
    for (captured, played) in input.chunks_exact(2).zip(output.chunks_exact(2)) {
        assert_eq!(played, &[captured[0]; 2]);
    }
    // Integer unsigned silence must use the midpoint, rather than zero.
    playback(&mut output, 2, &mut speaker, &mut started, &stats);
    assert!(output.iter().all(|sample| *sample == T::EQUILIBRIUM));
    assert_eq!(
        stats.output_underflows.load(Ordering::Relaxed),
        u32::try_from(PREFILL_SAMPLES).expect("prefill fits u32")
    );
}

#[test]
fn all_cpal_hardware_formats_convert_through_f32_and_use_correct_silence() {
    check_hardware_format::<i8>();
    check_hardware_format::<i16>();
    check_hardware_format::<i32>();
    check_hardware_format::<i64>();
    check_hardware_format::<u8>();
    check_hardware_format::<u16>();
    check_hardware_format::<u32>();
    check_hardware_format::<u64>();
    check_hardware_format::<f32>();
    check_hardware_format::<f64>();
}

#[test]
fn rejects_incompatible_rate_and_buffer_and_accepts_integer_devices() {
    let fixed = SupportedBufferSize::Range { min: 256, max: 512 };
    assert!(select_input_config([config(1, SampleFormat::F32, fixed)].into_iter()).is_none());
    let integer = select_input_config(
        [config(1, SampleFormat::I16, SupportedBufferSize::Unknown)].into_iter(),
    )
    .expect("integer devices convert to internal f32");
    assert_eq!(integer.format, SampleFormat::I16);
    let wrong_rate = cpal::SupportedStreamConfigRange::new(
        1,
        SampleRate(44_100),
        SampleRate(44_100),
        SupportedBufferSize::Unknown,
        SampleFormat::F32,
    );
    assert!(select_input_config([wrong_rate].into_iter()).is_none());
    assert!(select_input_config(std::iter::empty()).is_none());
    assert!(
        select_input_config(
            [config(1, SampleFormat::F32, SupportedBufferSize::Unknown)].into_iter()
        )
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
    capture(&input, 2, &mut mic, None, &stats);
    bridge(&mut net_input, &mut net_output);
    // Two blocks establish the small startup cushion.
    for _ in 0..200 {
        capture(&input, 2, &mut mic, None, &stats);
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
        capture(&[0.5; BUFFER_FRAMES], 1, &mut mic, None, &stats);
    }
    capture(&[1.0; BUFFER_FRAMES], 1, &mut mic, None, &stats);
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
fn incomplete_frames_are_reported_and_output_is_silenced() {
    let stats = Stats::default();
    let (mut mic, _net_input) = RingBuffer::new(RING_CAPACITY);
    let (_net_output, mut speaker) = RingBuffer::new(RING_CAPACITY);
    capture(&[0.5; 64], 3, &mut mic, None, &stats);
    let mut output = [1.0; 64];
    playback(&mut output, 3, &mut speaker, &mut false, &stats);
    assert_eq!(output, [0.0; 64]);
    assert_eq!(stats.wrong_buffer_sizes.load(Ordering::Relaxed), 2);
    assert!(stats.has_errors());
}

#[test]
fn alsa_periods_and_coalesced_callbacks_preserve_sample_order() {
    let stats = Stats::default();
    let (mut mic, mut net_input) = RingBuffer::new(RING_CAPACITY);
    let (mut net_output, mut speaker) = RingBuffer::new(RING_CAPACITY);
    for value in [0.25, 0.5, 0.75, 1.0] {
        capture(&[value; 64], 2, &mut mic, None, &stats);
    }
    capture(&[0.0; 256], 2, &mut mic, None, &stats);
    assert_eq!(bridge(&mut net_input, &mut net_output), 256);
    let mut started = false;
    let mut output = [0.0; 512];
    playback(&mut output, 2, &mut speaker, &mut started, &stats);
    assert!(started);
    for (chunk, expected) in output[..256].chunks_exact(64).zip([0.25, 0.5, 0.75, 1.0]) {
        assert_eq!(chunk, &[expected; 64]);
    }
    assert!(output[256..].iter().all(|sample| *sample == 0.0));
    assert!(!stats.has_errors());
}

#[test]
fn input_peak_survives_between_ui_frames_even_if_ring_overflows() {
    let stats = Stats::default();
    let (mut input, _consumer) = RingBuffer::new(1);
    capture(&[0.2, 0.9, -1.2, f32::NAN], 1, &mut input, None, &stats);
    capture(&[0.1], 1, &mut input, None, &stats);
    assert_eq!(
        f32::from_bits(stats.input_peak.swap(0, Ordering::Relaxed)),
        1.2
    );
    capture(&[0.1, 1.0], 2, &mut input, None, &stats);
    assert_eq!(
        f32::from_bits(stats.input_peak.load(Ordering::Relaxed)),
        0.1
    );
}
