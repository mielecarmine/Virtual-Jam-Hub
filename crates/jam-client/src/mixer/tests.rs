use super::*;

#[test]
fn remote_audio_and_local_monitor_fill_every_output_channel() {
    for output_channels in [1, 2, 4] {
        for (remote, local) in [(0.25_f32, 0.0_f32), (0.0, 0.5), (0.25, 0.5)] {
            let (mut rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
            let mut monitor = mixer.enable_local_monitor();
            let stats = Stats::default();
            let (mut mic, _net) = RingBuffer::new(1);
            crate::audio::capture(&[local], 1, &mut mic, Some(&mut monitor), &stats);
            rx.push(1, &[remote; RX_TARGET_SAMPLES + 1]);
            let mut out = vec![0.0_f32; output_channels];
            mixer.playback(&mut out, output_channels, &stats);
            assert_eq!(out, vec![(remote + local).tanh(); output_channels]);
            assert!(!stats.has_errors());
        }
    }
}

#[test]
fn local_monitor_controls_and_solos_never_change_transmitted_samples() {
    let (mut rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
    let mut monitor = mixer.enable_local_monitor();
    let (mut mic, mut net) = RingBuffer::new(64);
    let stats = Stats::default();
    let mut out = [0.0; 2];
    // Local gain, mute, remote solo, local solo, both solos, and mute over solo.
    for (gain, mute, local_solo, remote_solo, expected) in [
        (1.0_f32, false, false, false, 0.75_f32),
        (0.5, false, false, false, 0.5),
        (2.0, true, false, false, 0.25),
        (1.0, false, false, true, 0.25),
        (1.0, false, true, false, 0.5),
        (1.0, false, true, true, 0.75),
        (1.0, true, true, false, 0.0),
        (f32::NAN, false, false, false, 0.25),
        (1.0, false, false, false, 0.75),
    ] {
        rx.local_control
            .gain
            .store(gain.to_bits(), Ordering::Relaxed);
        rx.local_control.mute.store(mute, Ordering::Relaxed);
        rx.local_control.solo.store(local_solo, Ordering::Relaxed);
        rx.controls[0].solo.store(remote_solo, Ordering::Relaxed);
        // Capture uses channel 1; channel 2 must never leak into monitoring or UDP.
        crate::audio::capture(&[0.5, -0.9], 2, &mut mic, Some(&mut monitor), &stats);
        rx.push(1, &[0.25; RX_TARGET_SAMPLES + 1]);
        mixer.playback(&mut out, 2, &stats);
        assert_eq!(out, [expected.tanh(); 2]);
        assert_eq!(net.pop(), Ok(0.5));
    }
    assert_eq!(net.slots(), 0);
    assert_eq!(
        f32::from_bits(stats.input_peak.load(Ordering::Relaxed)),
        0.5
    );
}

#[test]
fn local_monitor_starts_without_peers_or_prefill_and_silences_underflow() {
    let (_rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
    let mut monitor = mixer.enable_local_monitor();
    let (mut mic, _net) = RingBuffer::new(1);
    let stats = Stats::default();
    crate::audio::capture(&[0.25, -0.5], 1, &mut mic, Some(&mut monitor), &stats);
    let mut out = [1.0; 6];
    mixer.playback(&mut out, 2, &stats);
    assert_eq!(
        out,
        [
            0.25_f32.tanh(),
            0.25_f32.tanh(),
            (-0.5_f32).tanh(),
            (-0.5_f32).tanh(),
            0.0,
            0.0
        ]
    );
    // A full UDP ring must not prevent local monitoring.
    assert_eq!(stats.input_overflows.load(Ordering::Relaxed), 1);
    crate::audio::capture(
        &[f32::NAN, f32::INFINITY],
        1,
        &mut mic,
        Some(&mut monitor),
        &stats,
    );
    mixer.playback(&mut out, 2, &stats);
    assert_eq!(out, [0.0; 6]);
}

#[test]
fn delayed_local_output_keeps_only_one_callback_and_rejects_overflowed_old_audio() {
    for frames in [64, 128, 256] {
        let (_rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
        let mut monitor = mixer.enable_local_monitor();
        let (mut mic, _net) = RingBuffer::new(1);
        let stats = Stats::default();
        crate::audio::capture(
            &vec![0.8; frames * 4],
            1,
            &mut mic,
            Some(&mut monitor),
            &stats,
        );
        crate::audio::capture(
            &vec![-0.25; frames],
            1,
            &mut mic,
            Some(&mut monitor),
            &stats,
        );
        let mut out = vec![0.0; frames];
        mixer.playback(&mut out, 1, &stats);
        assert_eq!(out, vec![(-0.25_f32).tanh(); frames]);
        mixer.playback(&mut out, 1, &stats);
        assert_eq!(out, vec![0.0; frames]);

        crate::audio::capture(
            &vec![0.8; crate::audio::RING_CAPACITY],
            1,
            &mut mic,
            Some(&mut monitor),
            &stats,
        );
        crate::audio::capture(
            &vec![-0.25; frames],
            1,
            &mut mic,
            Some(&mut monitor),
            &stats,
        );
        // The full ring contains only stale audio; never play it on resume.
        mixer.playback(&mut out, 1, &stats);
        assert_eq!(out, vec![0.0; frames]);
        crate::audio::capture(
            &vec![-0.25; frames],
            1,
            &mut mic,
            Some(&mut monitor),
            &stats,
        );
        mixer.playback(&mut out, 1, &stats);
        assert_eq!(out, vec![(-0.25_f32).tanh(); frames]);
    }
}

#[test]
fn four_full_scale_peers_are_soft_clipped_and_duplicated_to_stereo() {
    let (mut rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
    let stats = Stats::default();
    for id in 1..=4 {
        rx.push(id, &[0.8; RX_TARGET_SAMPLES + 128]);
    }
    let mut output = [0.0; 256];
    mixer.playback(&mut output, 2, &stats);
    let expected = (4.0_f32 * 0.8).tanh();
    assert!(output.iter().all(|v| (-1.0..=1.0).contains(v)));
    assert!(output.iter().all(|v| (*v - expected).abs() < 1e-6));
    for id in 1..=4 {
        rx.remove(id);
    }
    mixer.playback(&mut output, 2, &stats);
    for id in 1..=4 {
        rx.push(id, &[-0.8; RX_TARGET_SAMPLES + 128]);
    }
    mixer.playback(&mut output, 2, &stats);
    assert!(output.iter().all(|v| (*v + expected).abs() < 1e-6));
}

#[test]
fn atomic_gain_mute_and_solo_take_effect_on_already_queued_audio() {
    let (mut rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
    let controls = Arc::clone(&mixer.controls);
    for id in 1..=2 {
        rx.push(id, &[0.5; RX_TARGET_SAMPLES + 16]);
    }
    let stats = Stats::default();
    let mut out = [0.0];
    controls[0].gain.store(0.5_f32.to_bits(), Ordering::Relaxed);
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out[0], 0.75_f32.tanh());
    controls[1].mute.store(true, Ordering::Relaxed);
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out[0], 0.25_f32.tanh());
    controls[1].solo.store(true, Ordering::Relaxed);
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out[0], 0.0); // Mute takes precedence over solo.
    controls[1].mute.store(false, Ordering::Relaxed);
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out[0], 0.5_f32.tanh());
    controls[0].solo.store(true, Ordering::Relaxed);
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out[0], 0.75_f32.tanh());
    controls[1].solo.store(false, Ordering::Relaxed);
    controls[0]
        .gain
        .store(f32::NAN.to_bits(), Ordering::Relaxed);
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out[0], 0.0);
}

#[test]
fn muted_audio_is_consumed_and_peer_reuse_discards_queued_old_samples() {
    let (mut rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
    let stats = Stats::default();
    rx.push(8, &[0.8; RX_TARGET_SAMPLES + 128]);
    rx.controls[7].mute.store(true, Ordering::Relaxed);
    let mut out = [1.0; 64];
    for _ in 0..20 {
        mixer.playback(&mut out, 1, &stats);
    }
    assert_eq!(out, [0.0; 64]);
    rx.remove(8);
    rx.push(8, &[-0.25; RX_TARGET_SAMPLES + 64]);
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out, [(-0.25_f32).tanh(); 64]);
    for _ in 0..20 {
        mixer.playback(&mut out, 1, &stats);
    }
    assert_eq!(out, [0.0; 64]);
    rx.remove(8);
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out, [0.0; 64]);
}

#[test]
fn full_peer_queue_does_not_block_another_and_invalid_output_is_silent() {
    let (mut rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
    let stats = Stats::default();
    rx.push(1, &[0.8; RX_TARGET_SAMPLES + 128]);
    rx.push(1, &[-0.8; 128]);
    rx.push(2, &[-0.5; RX_TARGET_SAMPLES + 128]);
    let mut out = [1.0; 128];
    mixer.playback(&mut out, 3, &stats);
    assert_eq!(out, [0.0; 128]);
    mixer.playback(&mut out, 0, &stats);
    mixer.playback(&mut out, 1, &stats);
    assert!(out.iter().all(|v| (*v - 0.3_f32.tanh()).abs() < 1e-6));
    assert_eq!(stats.wrong_buffer_sizes.load(Ordering::Relaxed), 2);
}

#[test]
fn soft_clip_is_continuous_monotonic_and_bounded_across_the_threshold() {
    let stats = Stats::default();
    let mut previous = -1.0;
    for step in -400_i16..=400 {
        let (mut rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
        let sample = f32::from(step) / 400.0;
        for id in 1..=4 {
            rx.push(id, &[sample; RX_TARGET_SAMPLES + 1]);
        }
        let mut out = [0.0];
        mixer.playback(&mut out, 1, &stats);
        assert!((-1.0..=1.0).contains(&out[0]));
        assert!(out[0] >= previous);
        assert!(out[0] - previous < 0.011);
        previous = out[0];
    }
}

#[test]
fn peer_peaks_are_independent_pre_fader_and_reset_on_removal() {
    let (mut rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
    let stats = Stats::default();
    rx.push(1, &[-0.8, 0.2]);
    rx.push(1, &[0.0; RX_TARGET_SAMPLES]);
    rx.push(2, &[0.3, -0.4]);
    rx.push(2, &[0.0; RX_TARGET_SAMPLES]);
    rx.controls[0].mute.store(true, Ordering::Relaxed);
    rx.controls[1]
        .gain
        .store(0.0_f32.to_bits(), Ordering::Relaxed);
    mixer.playback(&mut [0.0_f32; 2], 1, &stats);
    assert_eq!(
        f32::from_bits(rx.controls[0].peak.swap(0, Ordering::Relaxed)),
        0.8
    );
    assert_eq!(
        f32::from_bits(rx.controls[1].peak.load(Ordering::Relaxed)),
        0.4
    );
    mixer.playback(&mut [0.0_f32; 2], 1, &stats);
    assert_eq!(rx.controls[0].peak.load(Ordering::Relaxed), 0);
    rx.remove(2);
    assert_eq!(rx.controls[1].peak.load(Ordering::Relaxed), 0);
}

#[test]
fn whole_chunks_reach_prefill_without_alignment_loss() {
    for frames in [64, 128, 256] {
        let (mut rx, mut mixer) = channels(frames);
        let stats = Stats::default();
        let chunks = (frames + RX_TARGET_SAMPLES).div_ceil(FRAME_SAMPLES);
        for _ in 0..chunks {
            assert!(rx.push(1, &[0.5; FRAME_SAMPLES]));
        }
        let queued = mixer.rings[0].slots();
        mixer.playback(&mut vec![0.0; frames], 1, &stats);
        assert!(mixer.ready[0]);
        assert_eq!(mixer.rings[0].slots(), queued - frames);
        assert_eq!(stats.telemetry().align_dropped_samples, 0);
        assert_eq!(stats.output_underflows.load(Ordering::Relaxed), 0);
        assert!(rx.rings[0].buffer().capacity() >= frames + RX_TARGET_SAMPLES + FRAME_SAMPLES);
    }
}

#[test]
fn remote_prefill_is_per_peer_and_restarts_after_starvation_or_reuse() {
    let (mut rx, mut mixer) = channels(128);
    let stats = Stats::default();
    let mut out = [0.0; 128];
    assert!(rx.push(1, &[0.5; FRAME_SAMPLES]));
    assert!(rx.push(2, &[0.25; 1080]));
    mixer.playback(&mut out, 1, &stats);
    assert!(!mixer.ready[0]);
    assert!(mixer.ready[1]);
    assert_eq!(out, [0.25_f32.tanh(); 128]);
    rx.remove(2);
    assert!(rx.push(1, &[0.5; 960]));
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out, [0.5_f32.tanh(); 128]);
    for _ in 0..8 {
        mixer.playback(&mut out, 1, &stats);
    }
    assert!(!mixer.ready[0]);
    assert_eq!(stats.output_underflows.load(Ordering::Relaxed), 72);
    assert!(rx.push(1, &[0.5; FRAME_SAMPLES]));
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out, [0.0; 128]);
    rx.remove(1);
    mixer.playback(&mut out, 1, &stats);
    assert!(rx.push(1, &[-0.5; 1080]));
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out, [(-0.5_f32).tanh(); 128]);
}

#[test]
fn opus_pushes_and_hardware_callbacks_do_not_underflow_with_worker_jitter() {
    for frames in [64, 128, 256] {
        for phase in 0..FRAME_SAMPLES {
            let (mut rx, mut mixer) = channels(frames);
            let stats = Stats::default();
            let mut out = vec![0.0; frames * 2];
            let mut next_callback = phase;
            let mut next_wake = 0;
            let mut produced = 0;
            let mut wake = 0;
            let mut started = false;
            for now in 0..48_000 {
                if now == next_callback {
                    mixer.playback(&mut out, 2, &stats);
                    if stats.playback_started.load(Ordering::Relaxed) {
                        if started {
                            assert_eq!(stats.telemetry().not_ready_samples, 0);
                        }
                        stats.not_ready_samples.store(0, Ordering::Relaxed);
                        started = true;
                        assert!(mixer.ready[0]);
                        assert!(out.iter().all(|&v| v == 0.5_f32.tanh()));
                    }
                    next_callback += frames;
                }
                if now == next_wake {
                    while produced <= now && rx.available_chunks(1) > 0 {
                        assert!(rx.push(1, &[0.5; FRAME_SAMPLES]));
                        produced += FRAME_SAMPLES;
                    }
                    next_wake += [120, 240, 480, 720, 48][wake % 5];
                    wake += 1;
                }
            }
            assert!(started);
            assert_eq!(
                stats.output_underflows.load(Ordering::Relaxed),
                0,
                "frames={frames}, phase={phase}"
            );
            assert_eq!(stats.telemetry().align_dropped_samples, 0);
        }
    }
}

#[test]
fn diagnostics_separate_active_prefill_silence_underflow_and_alignment_drops() {
    let (mut rx, mut mixer) = channels(128);
    let stats = Stats::default();
    let mut output = [0.0; 256];
    mixer.playback(&mut output, 2, &stats);
    assert_eq!(stats.telemetry().not_ready_samples, 0);
    assert!(rx.push(1, &[0.5; FRAME_SAMPLES]));
    mixer.playback(&mut output, 2, &stats);
    assert_eq!(stats.telemetry().not_ready_samples, 128);
    assert!(rx.push(1, &[0.5; 960]));
    mixer.playback(&mut output, 2, &stats);
    assert_eq!(stats.telemetry().align_dropped_samples, 0);
    for _ in 0..8 {
        mixer.playback(&mut output, 2, &stats);
    }
    assert_eq!(stats.output_underflows.load(Ordering::Relaxed), 72);
    mixer.playback(&mut output, 2, &stats);
    assert_eq!(stats.telemetry().not_ready_samples, 256);
    let callbacks = stats.telemetry().output_callbacks;
    rx.remove(1);
    mixer.playback(&mut output, 2, &stats);
    assert_eq!(stats.telemetry().not_ready_samples, 256);
    assert_eq!(stats.telemetry().output_callbacks, callbacks + 1);
    assert_eq!(stats.telemetry().last_output_frames, 128);
}

#[test]
fn stale_epoch_alignment_drops_are_counted() {
    let (mut rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
    let stats = Stats::default();
    assert!(rx.push(1, &[0.5; FRAME_SAMPLES]));
    rx.remove(1);
    mixer.playback(&mut [0.0; 64], 1, &stats);
    assert_eq!(
        stats.telemetry().align_dropped_samples,
        FRAME_SAMPLES as u32
    );
    assert_eq!(stats.telemetry().not_ready_samples, 0);
}
