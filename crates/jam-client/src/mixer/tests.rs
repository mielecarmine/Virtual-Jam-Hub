use super::*;

fn click_config(bpm: u16, beats_per_bar: u8) -> jam_protocol::MetronomeConfig {
    jam_protocol::MetronomeConfig {
        enabled: true,
        bpm,
        beats_per_bar,
        ..jam_protocol::MetronomeConfig::default()
    }
}

#[test]
fn metronome_is_added_before_clipping_and_remains_audible_during_channel_solo() {
    let (mut rx, mut mixer) = channels(128);
    let (mut click_rx, mut click_only) = channels(128);
    let stats = Stats::default();
    rx.sync_metronome(click_config(120, 4), 0);
    click_rx.sync_metronome(click_config(120, 4), 0);
    rx.controls[0].solo.store(true, Ordering::Relaxed);
    assert!(rx.push(1, &vec![0.8; rx_target_samples(128) + 128]));
    let mut output = [0.0_f32; 256];
    let mut click = [0.0_f32; 128];
    mixer.playback(&mut output, 2, &stats);
    click_only.playback(&mut click, 1, &stats);
    for (frame, sample) in output.chunks_exact(2).zip(click) {
        let expected = (0.8 + sample.atanh()).tanh();
        assert!((frame[0] - expected).abs() < 1e-6);
        assert_eq!(frame[0], frame[1]);
    }
}

#[test]
fn metronome_local_gain_scales_amplitude_without_changing_sample_phase() {
    let (mut rx, mut full) = channels(128);
    let (mut quiet_rx, mut quiet) = channels(128);
    let stats = Stats::default();
    rx.sync_metronome(click_config(300, 3), 0);
    quiet_rx.sync_metronome(click_config(300, 3), 0);
    quiet_rx
        .metronome
        .gain
        .store(0.25_f32.to_bits(), Ordering::Relaxed);
    let mut full_samples = vec![0.0_f32; 10_560];
    let mut quiet_samples = full_samples.clone();
    full.playback(&mut full_samples, 1, &stats);
    quiet.playback(&mut quiet_samples, 1, &stats);
    // After the gain ramp, compare the next complete beat before clipping.
    for (full, quiet) in full_samples[9600..].iter().zip(&quiet_samples[9600..]) {
        assert!((quiet.atanh() - full.atanh() * 0.5).abs() < 1e-6);
    }
    assert_eq!(rx.metronome.current_beat.load(Ordering::Relaxed), 1);
    assert_eq!(quiet_rx.metronome.current_beat.load(Ordering::Relaxed), 1);
}

#[test]
fn metronome_periods_are_sample_exact_without_fractional_bpm_drift() {
    for bpm in [30_u16, 73, 120, 137, 300] {
        let (mut rx, _) = channels(128);
        rx.sync_metronome(click_config(bpm, 3), 0);
        let mut synth = metronome::Metronome::new(Arc::clone(&rx.metronome));
        synth.begin_block();
        let end = (6 * 2_880_000_u64).div_ceil(u64::from(bpm));
        let mut onsets = Vec::new();
        let mut previous = 0.0;
        for sample in 0..end {
            let value = synth.next_sample();
            if previous == 0.0 && value > 0.0 {
                onsets.push(sample);
            }
            previous = value;
        }
        let expected: Vec<_> = (0..6_u64)
            .map(|beat| (beat * 2_880_000).div_ceil(u64::from(bpm)) + 1)
            .collect();
        assert_eq!(onsets, expected, "BPM {bpm}");
    }
}

#[test]
fn metronome_accents_and_waveform_are_independent_of_callback_size() {
    let render = |frames| {
        let (mut rx, mut mixer) = channels(frames);
        rx.sync_metronome(click_config(300, 3), 0);
        let stats = Stats::default();
        let mut result = vec![0.0_f32; 38_400];
        for block in result.chunks_mut(frames) {
            mixer.playback(block, 1, &stats);
        }
        assert_eq!(rx.metronome.current_beat.load(Ordering::Relaxed), 0);
        result
    };
    let samples = render(64);
    assert_eq!(samples, render(128));
    assert_eq!(samples, render(256));
    // 1200 Hz crosses zero after 20 samples, 800 Hz after 30 samples.
    assert!(samples[21] < 0.0);
    assert!(samples[9600 + 21] > 0.0);
    assert!(samples[9600 + 31] < 0.0);
    let peak = |start| {
        samples[start..start + 960]
            .iter()
            .copied()
            .fold(0.0_f32, f32::max)
    };
    assert!(peak(0) > peak(9600));
    assert_eq!(&samples[..960], &samples[28_800..29_760]);
    assert_eq!(&samples[9600..10_560], &samples[19_200..20_160]);
    assert!(samples[960..9600].iter().all(|&s| s == 0.0));
    assert!(
        samples
            .windows(2)
            .all(|pair| (pair[0] - pair[1]).abs() < 0.08)
    );
}

#[test]
fn metronome_bpm_phase_and_disable_changes_preserve_an_in_flight_click() {
    for config in [
        click_config(300, 2),
        click_config(120, 4),
        jam_protocol::MetronomeConfig::default(),
    ] {
        let (mut rx, mut mixer) = channels(128);
        let (mut reference_rx, mut reference) = channels(128);
        let stats = Stats::default();
        rx.sync_metronome(click_config(120, 4), 0);
        reference_rx.sync_metronome(click_config(120, 4), 0);
        let mut prefix = [0.0_f32; 128];
        mixer.playback(&mut prefix, 1, &stats);
        reference.playback(&mut prefix, 1, &stats);
        // Realign exactly onto a new downbeat in the middle of the old pulse.
        rx.sync_metronome(config, 0);
        let mut changed = [0.0_f32; 1024];
        let mut unchanged = changed;
        mixer.playback(&mut changed, 1, &stats);
        reference.playback(&mut unchanged, 1, &stats);
        assert_eq!(changed, unchanged);
        assert!(changed[832..].iter().all(|&s| s == 0.0));
    }
}

#[test]
fn metronome_restarts_at_new_bpm_boundary_and_waits_for_future_anchor() {
    let (mut rx, mut mixer) = channels(128);
    let stats = Stats::default();
    rx.sync_metronome(click_config(120, 4), -10_000); // Anchor in 480 samples.
    let mut output = [0.0_f32; 1440];
    mixer.playback(&mut output, 1, &stats);
    assert!(output[..481].iter().all(|&s| s == 0.0));
    assert!(output[481] > 0.0);
    rx.sync_metronome(click_config(300, 3), 10_000);
    let mut output = vec![0.0_f32; 19_000];
    mixer.playback(&mut output, 1, &stats);
    // 480 samples into the new beat: next starts after 9120 samples.
    assert!(output[..9121].iter().all(|&s| s == 0.0));
    assert!(output[9121] > 0.0);
    assert!(output[18_721] > 0.0);
    assert_eq!(rx.metronome.current_beat.load(Ordering::Relaxed), 2);
}

#[test]
fn metronome_is_local_and_mute_gain_are_smooth_and_do_not_stop_phase() {
    let (mut rx, mut mixer) = channels(128);
    let mut monitor = mixer.enable_local_monitor();
    let (mut mic, mut net) = RingBuffer::new(256);
    let stats = Stats::default();
    rx.sync_metronome(click_config(300, 3), 0);
    let mut output = [0.0_f32; 128];
    crate::audio::capture(
        &[0.0; 128],
        1,
        crate::audio::InputRouting::default(),
        &mut mic,
        Some(&mut monitor),
        &stats,
    );
    mixer.playback(&mut output, 1, &stats);
    assert!(output.iter().any(|&s| s != 0.0));
    assert!((0..128).all(|_| net.pop() == Ok(0.0)));
    assert_eq!(net.slots(), 0);
    let last = output[127];
    rx.metronome.mute.store(true, Ordering::Relaxed);
    mixer.playback(&mut output, 1, &stats);
    assert!((output[0] - last).abs() < 0.08);
    assert!(output[48..].iter().all(|&s| s == 0.0));
    for _ in 0..80 {
        mixer.playback(&mut output, 1, &stats);
        assert!(output.iter().all(|&s| s == 0.0));
    }
    assert_eq!(rx.metronome.current_beat.load(Ordering::Relaxed), 1);
    for gain in [f32::NAN, f32::INFINITY, -1.0] {
        rx.metronome.gain.store(gain.to_bits(), Ordering::Relaxed);
        rx.metronome.mute.store(false, Ordering::Relaxed);
        mixer.playback(&mut output, 1, &stats);
        assert!(output.iter().all(|&s| s == 0.0));
    }
}

#[test]
fn remote_audio_and_local_monitor_fill_every_output_channel() {
    for output_channels in [1, 2, 4] {
        for (remote, local) in [(0.25_f32, 0.0_f32), (0.0, 0.5), (0.25, 0.5)] {
            let (mut rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
            let mut monitor = mixer.enable_local_monitor();
            let stats = Stats::default();
            let (mut mic, _net) = RingBuffer::new(1);
            crate::audio::capture(
                &[local],
                1,
                crate::audio::InputRouting::default(),
                &mut mic,
                Some(&mut monitor),
                &stats,
            );
            rx.push(1, &[remote; RX_TARGET_SAMPLES + 1]);
            let mut out = vec![0.0_f32; output_channels];
            mixer.playback(&mut out, output_channels, &stats);
            assert_eq!(out, vec![(remote + local).tanh(); output_channels]);
            assert!(!stats.has_errors());
        }
    }
}

#[test]
fn dual_local_controls_solos_and_tx_mute_are_independent() {
    let (mut rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
    let mut monitor = mixer.enable_local_sources(true);
    let routing = crate::audio::InputRouting::new(1, Some(2)).unwrap();
    let stats = Stats::default();
    let (mut tx, mut net) = RingBuffer::new(4);
    let mut out = [0.0_f32; 2];
    for (gain, inst_mute, mic_mute, inst_solo, mic_solo, remote_solo, expected) in [
        (1.0_f32, false, false, false, false, false, 0.875_f32),
        (0.5, false, false, false, false, false, 0.625),
        (1.0, true, false, false, false, false, 0.375),
        (1.0, false, true, false, false, false, 0.625),
        (1.0, false, false, true, false, false, 0.5),
        (1.0, false, false, false, true, false, 0.25),
        (1.0, false, false, true, true, false, 0.75),
        (1.0, false, false, false, false, true, 0.125),
        (1.0, false, false, false, true, true, 0.375),
        (1.0, false, true, false, true, false, 0.0),
    ] {
        rx.local_control
            .gain
            .store(gain.to_bits(), Ordering::Relaxed);
        rx.local_control.mute.store(inst_mute, Ordering::Relaxed);
        rx.local_control.solo.store(inst_solo, Ordering::Relaxed);
        rx.local_mic_control.mute.store(mic_mute, Ordering::Relaxed);
        rx.local_mic_control.solo.store(mic_solo, Ordering::Relaxed);
        rx.controls[0].solo.store(remote_solo, Ordering::Relaxed);
        for tx_muted in [false, true] {
            stats.mic_tx_mute.store(tx_muted, Ordering::Relaxed);
            crate::audio::capture(
                &[0.5, 0.25],
                2,
                routing,
                &mut tx,
                Some(&mut monitor),
                &stats,
            );
            assert!(rx.push(1, &[0.125; RX_TARGET_SAMPLES + 1]));
            mixer.playback(&mut out, 2, &stats);
            assert_eq!(out, [expected.tanh(); 2]);
            assert_eq!(net.pop(), Ok(if tx_muted { 0.5 } else { 0.75 }));
        }
    }
    rx.local_control.mute.store(true, Ordering::Relaxed);
    rx.local_mic_control.mute.store(false, Ordering::Relaxed);
    rx.local_mic_control
        .gain
        .store(2.0_f32.to_bits(), Ordering::Relaxed);
    rx.local_mic_control.solo.store(true, Ordering::Relaxed);
    rx.controls[0].solo.store(false, Ordering::Relaxed);
    crate::audio::capture(
        &[0.5, 0.25],
        2,
        routing,
        &mut tx,
        Some(&mut monitor),
        &stats,
    );
    mixer.playback(&mut out, 2, &stats);
    assert_eq!(out, [0.5_f32.tanh(); 2]);
    // All local samples were consumed even while excluded or muted.
    mixer.playback(&mut out, 2, &stats);
    assert_eq!(out, [0.0; 2]);
}

#[test]
fn disabled_microphone_solo_cannot_silence_instrument_or_remote_peers() {
    let (mut rx, mut mixer) = channels(crate::audio::BUFFER_FRAMES);
    let mut monitor = mixer.enable_local_monitor();
    rx.local_mic_control.solo.store(true, Ordering::Relaxed);
    monitor.push_sources(0.5, 0.75);
    monitor.publish();
    assert!(rx.push(1, &[0.25; RX_TARGET_SAMPLES + 1]));
    let mut out = [0.0_f32; 2];
    mixer.playback(&mut out, 2, &Stats::default());
    assert_eq!(out, [0.75_f32.tanh(); 2]);
    let (new_rx, _) = channels(crate::audio::BUFFER_FRAMES);
    assert!(!new_rx.local_mic_control.solo.load(Ordering::Relaxed));
    assert!(!new_rx.local_mic_control.mute.load(Ordering::Relaxed));
    assert_eq!(
        f32::from_bits(new_rx.local_mic_control.gain.load(Ordering::Relaxed)),
        1.0
    );
    assert!(!Stats::default().mic_tx_mute.load(Ordering::Relaxed));
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
        crate::audio::capture(
            &[0.5, -0.9],
            2,
            crate::audio::InputRouting::default(),
            &mut mic,
            Some(&mut monitor),
            &stats,
        );
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
    crate::audio::capture(
        &[0.25, -0.5],
        1,
        crate::audio::InputRouting::default(),
        &mut mic,
        Some(&mut monitor),
        &stats,
    );
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
        crate::audio::InputRouting::default(),
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
            crate::audio::InputRouting::default(),
            &mut mic,
            Some(&mut monitor),
            &stats,
        );
        crate::audio::capture(
            &vec![-0.25; frames],
            1,
            crate::audio::InputRouting::default(),
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
            crate::audio::InputRouting::default(),
            &mut mic,
            Some(&mut monitor),
            &stats,
        );
        crate::audio::capture(
            &vec![-0.25; frames],
            1,
            crate::audio::InputRouting::default(),
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
            crate::audio::InputRouting::default(),
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
        rx.push(id, &vec![0.8; rx_target_samples(128) + 128]);
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
        rx.push(id, &vec![-0.8; rx_target_samples(128) + 128]);
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
    rx.push(8, &vec![0.8; rx_target_samples(128) + 128]);
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
    rx.push(1, &vec![0.8; rx_target_samples(128) + 128]);
    rx.push(1, &[-0.8; 128]);
    rx.push(2, &vec![-0.5; rx_target_samples(128) + 128]);
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
        let chunks = (frames + rx_target_samples(frames)).div_ceil(FRAME_SAMPLES);
        for _ in 0..chunks {
            assert!(rx.push(1, &[0.5; FRAME_SAMPLES]));
        }
        let queued = mixer.rings[0].slots();
        mixer.playback(&mut vec![0.0; frames], 1, &stats);
        assert!(mixer.ready[0]);
        assert_eq!(mixer.rings[0].slots(), queued - frames);
        assert_eq!(stats.telemetry().align_dropped_samples, 0);
        assert_eq!(stats.output_underflows.load(Ordering::Relaxed), 0);
        assert!(rx.rings[0].buffer().capacity() >= frames + rx_target_samples(frames));
    }
}

#[test]
fn remote_prefill_is_per_peer_and_restarts_after_starvation_or_reuse() {
    let (mut rx, mut mixer) = channels(128);
    let stats = Stats::default();
    let mut out = [0.0; 128];
    assert!(rx.push(1, &[0.5; FRAME_SAMPLES]));
    assert!(rx.push(2, &[0.25; 600]));
    mixer.playback(&mut out, 1, &stats);
    assert!(!mixer.ready[0]);
    assert!(mixer.ready[1]);
    assert_eq!(out, [0.25_f32.tanh(); 128]);
    rx.remove(2);
    assert!(rx.push(1, &[0.5; 480]));
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out, [0.5_f32.tanh(); 128]);
    for _ in 0..6 {
        mixer.playback(&mut out, 1, &stats);
    }
    assert!(!mixer.ready[0]);
    assert_eq!(stats.output_underflows.load(Ordering::Relaxed), 120);
    assert!(rx.push(1, &[0.5; FRAME_SAMPLES]));
    mixer.playback(&mut out, 1, &stats);
    assert_eq!(out, [0.0; 128]);
    rx.remove(1);
    mixer.playback(&mut out, 1, &stats);
    assert!(rx.push(1, &[-0.5; 600]));
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
                    // Normal worker cadence: 0.25 to 1.5 ms. Long stalls are tested separately.
                    next_wake += [12, 48, 72, 60, 24][wake % 5];
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
    assert!(rx.push(1, &[0.5; 480]));
    mixer.playback(&mut output, 2, &stats);
    assert_eq!(stats.telemetry().align_dropped_samples, 0);
    for _ in 0..4 {
        mixer.playback(&mut output, 2, &stats);
    }
    assert_eq!(stats.output_underflows.load(Ordering::Relaxed), 40);
    mixer.playback(&mut output, 2, &stats);
    assert_eq!(stats.output_underflows.load(Ordering::Relaxed), 120);
    assert_eq!(stats.telemetry().not_ready_samples, 176);
    let callbacks = stats.telemetry().output_callbacks;
    rx.remove(1);
    mixer.playback(&mut output, 2, &stats);
    assert_eq!(stats.telemetry().not_ready_samples, 176);
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

#[test]
fn isolated_underflow_recovers_without_discarding_a_sparse_queue() {
    for frames in [64, 128, 256] {
        let (mut rx, mut mixer) = channels(frames);
        let stats = Stats::default();
        let mut output = vec![0.0_f32; frames];
        assert!(rx.push(1, &vec![0.5; rx_target_samples(frames) + frames]));
        mixer.playback(&mut output, 1, &stats);
        // Repeated single-sample gaps. Every return includes the delayed sample
        // plus one callback; sparse queues must remain available for playback.
        for cycle in 1..=100 {
            while mixer.rings[0].pop().is_ok() {}
            assert!(rx.push(1, &vec![0.5; frames - 1]));
            mixer.playback(&mut output, 1, &stats);
            assert!(mixer.ready[0]);
            assert!(output[frames - 1] > 0.0);
            assert!(output[frames - 1] < output[frames - 2]);
            let last = output[frames - 1];
            assert!(rx.push(1, &[-0.5]));
            assert!(rx.push(1, &vec![-0.5; frames]));
            mixer.playback(&mut output, 1, &stats);
            assert!((output[0] - last).abs() < 0.022);
            assert!(output.windows(2).all(|v| (v[1] - v[0]).abs() < 0.022));
            assert_eq!(output[frames - 1], (-0.5_f32).tanh());
            assert_eq!(mixer.rings[0].slots(), 1);
            assert_eq!(stats.telemetry().align_dropped_samples, 0);
            assert_eq!(stats.telemetry().not_ready_samples, 0);
            assert_eq!(stats.output_underflows.load(Ordering::Relaxed), cycle);
        }
    }
}

#[test]
fn fade_and_starvation_span_callbacks_and_refill_only_the_starved_peer() {
    let (mut rx, mut mixer) = channels(64);
    let stats = Stats::default();
    assert!(rx.push(1, &[0.5; RX_TARGET_SAMPLES + 64]));
    assert!(rx.push(2, &[0.25; RX_TARGET_SAMPLES + 64]));
    mixer.playback(&mut [0.0; 64], 1, &stats);
    while mixer.rings[0].pop().is_ok() {}
    // Keep the last valid sample near the callback boundary.
    assert!(rx.push(1, &[0.5; 48]));
    rx.controls[1].mute.store(true, Ordering::Relaxed);
    let mut output = [0.0; 64];
    mixer.playback(&mut output, 1, &stats);
    assert!(output[48] > 0.0 && output[48] < output[47]);
    let last = output[63];
    mixer.playback(&mut output, 1, &stats);
    assert!(output[0] > 0.0 && output[0] < last);
    assert!(output.windows(2).all(|v| v[1] <= v[0]));
    assert_eq!(output[31..], [0.0; 33]);
    assert!(mixer.ready[0]); // 80 misses, below one Opus interval.
    mixer.playback(&mut output, 1, &stats);
    assert!(!mixer.ready[0]);
    assert!(mixer.ready[1]);
    assert_eq!(stats.output_underflows.load(Ordering::Relaxed), 120);
    assert!(rx.push(1, &[0.5; FRAME_SAMPLES]));
    mixer.playback(&mut output, 1, &stats);
    assert_eq!(output, [0.0; 64]);
    assert!(!mixer.ready[0]);
    assert!(rx.push(1, &[0.5; RX_TARGET_SAMPLES]));
    mixer.playback(&mut output, 1, &stats);
    assert!(mixer.ready[0]);
    assert!(output[0] > 0.0 && output[0] < 0.02);
    assert!(output.windows(2).all(|v| v[1] >= v[0]));
    assert_eq!(output[47..], [0.5_f32.tanh(); 17]);
}

#[test]
fn partial_64_frame_underflow_recovers_on_next_chunk_without_discard_loop() {
    for missing in [1, 60, 64, 116, 119] {
        let (mut rx, mut mixer) = channels(64);
        let stats = Stats::default();
        assert!(rx.push(1, &[0.5; RX_TARGET_SAMPLES + 64]));
        mixer.playback(&mut [0.0_f32; 128], 2, &stats);
        while mixer.rings[0].pop().is_ok() {}
        assert!(rx.push(1, &vec![0.5; 128 - missing]));
        // Two hardware callbacks produce the partial chunk underflow.
        for _ in 0..2 {
            mixer.playback(&mut [0.0_f32; 128], 2, &stats);
        }
        assert!(mixer.ready[0]);
        assert_eq!(
            stats.output_underflows.load(Ordering::Relaxed),
            missing as u32
        );
        assert!(rx.push(1, &[0.5; FRAME_SAMPLES]));
        let mut output = [0.0_f32; 128];
        mixer.playback(&mut output, 2, &stats);
        assert!(mixer.ready[0]);
        assert_eq!(mixer.continuity[0].missing, 0);
        assert_eq!(output[126], 0.5_f32.tanh());
        // Model exactly 120 samples produced every 2.5 ms, 64 consumed every 1.333 ms.
        let mut produced = 0;
        for callback in 1..=750 {
            while produced * FRAME_SAMPLES < callback * 64 {
                assert!(rx.push(1, &[0.5; FRAME_SAMPLES]));
                produced += 1;
            }
            mixer.playback(&mut output, 2, &stats);
            assert!(mixer.ready[0]);
            assert_eq!(output, [0.5_f32.tanh(); 128]);
        }
        assert_eq!(
            stats.output_underflows.load(Ordering::Relaxed),
            missing as u32
        );
        assert_eq!(stats.telemetry().align_dropped_samples, 0);
    }
}

#[test]
fn isolated_samples_in_partial_blocks_do_not_reset_starvation() {
    let (mut rx, mut mixer) = channels(64);
    let stats = Stats::default();
    assert!(rx.push(1, &[0.5; RX_TARGET_SAMPLES + 64]));
    mixer.playback(&mut [0.0_f32; 128], 2, &stats);
    while mixer.rings[0].pop().is_ok() {}
    for _ in 0..2 {
        assert!(rx.push(1, &[0.5; 4]));
        mixer.playback(&mut [0.0_f32; 128], 2, &stats);
    }
    assert!(!mixer.ready[0]);
    assert_eq!(mixer.continuity[0].missing, STARVATION_SAMPLES);
}
