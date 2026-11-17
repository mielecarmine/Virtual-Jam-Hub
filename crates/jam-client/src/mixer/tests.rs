use super::*;

#[test]
fn four_full_scale_peers_are_soft_clipped_and_duplicated_to_stereo() {
    let (mut rx, mut mixer) = channels();
    let stats = Stats::default();
    for id in 1..=4 {
        rx.push(id, &[0.8; 128]);
    }
    let mut output = [0.0; 256];
    mixer.playback(&mut output, 2, &stats);
    let expected = (4.0_f32 * 0.8).tanh();
    assert!(output.iter().all(|v| (-1.0..=1.0).contains(v)));
    assert!(output.iter().all(|v| (*v - expected).abs() < 1e-6));
    for id in 1..=4 {
        rx.push(id, &[-0.8; 128]);
    }
    mixer.playback(&mut output, 2, &stats);
    assert!(output.iter().all(|v| (*v + expected).abs() < 1e-6));
}

#[test]
fn atomic_gain_mute_and_solo_take_effect_on_already_queued_audio() {
    let (mut rx, mut mixer) = channels();
    let controls = Arc::clone(&mixer.controls);
    for id in 1..=2 {
        rx.push(id, &[0.5; 16]);
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
    let (mut rx, mut mixer) = channels();
    let stats = Stats::default();
    rx.push(8, &[0.8; 128]);
    rx.controls[7].mute.store(true, Ordering::Relaxed);
    let mut out = [1.0; 64];
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out, [0.0; 64]);
    rx.remove(8);
    rx.push(8, &[-0.25; 64]);
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out, [(-0.25_f32).tanh(); 64]);
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out, [0.0; 64]);
    rx.remove(8);
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out, [0.0; 64]);
}

#[test]
fn full_peer_queue_does_not_block_another_and_invalid_output_is_silent() {
    let (mut rx, mut mixer) = channels_with_capacity(128);
    let stats = Stats::default();
    rx.push(1, &[0.8; 128]);
    rx.push(1, &[-0.8; 128]);
    rx.push(2, &[-0.5; 128]);
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
    let (mut rx, mut mixer) = channels();
    let stats = Stats::default();
    let mut previous = -1.0;
    for step in -400_i16..=400 {
        let sample = f32::from(step) / 400.0;
        for id in 1..=4 {
            rx.push(id, &[sample]);
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
    let (mut rx, mut mixer) = channels();
    let stats = Stats::default();
    rx.push(1, &[-0.8, 0.2]);
    rx.push(2, &[0.3, -0.4]);
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
fn receive_queue_is_capped_and_callback_discards_oldest_samples() {
    let (mut rx, mut mixer) = channels();
    let stats = Stats::default();
    rx.push(1, &[0.8; RX_TARGET_SAMPLES]);
    rx.push(1, &[-0.5; RX_TARGET_SAMPLES]);
    rx.push(1, &[0.1; FRAME_SAMPLES]);
    assert_eq!(mixer.rings[0].slots(), MAX_RX_SAMPLES);
    rx.push(2, &[0.25; FRAME_SAMPLES]);
    let mut out = [0.0; FRAME_SAMPLES];
    mixer.playback(&mut out, 1, &stats);
    assert!(out.iter().all(|v| (*v - (-0.25_f32).tanh()).abs() < 1e-6));
    assert_eq!(mixer.rings[0].slots(), FRAME_SAMPLES);
    assert_eq!(mixer.rings[1].slots(), 0);
}
