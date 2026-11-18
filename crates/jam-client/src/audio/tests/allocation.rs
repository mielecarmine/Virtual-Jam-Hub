//! Track allocations on the current test thread, excluding other parallel tests.
#![allow(unsafe_code)]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

use super::super::*;
use rtrb::RingBuffer;

thread_local! {
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

struct CountingAllocator;

// SAFETY: All allocation and deallocation operations delegate to the system allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| {
            if let Some(value) = count.get() {
                count.set(Some(value + 1));
            }
        });
        // SAFETY: The caller supplies a valid allocation layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: The caller supplies a pointer and layout allocated through System above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn audio_paths_do_not_allocate_including_startup_overflow_and_underflow() {
    let stats = Stats::default();
    let (mut mic, mut net_input) = RingBuffer::new(RING_CAPACITY);
    let (mut net_output, mut speaker) = RingBuffer::new(RING_CAPACITY);
    let input = [0.5; BUFFER_FRAMES];
    let mut output = [0.0; BUFFER_FRAMES];
    let mut started = false;
    let integer_input = [1_i32 << 30; PREFILL_SAMPLES];
    let mut integer_output = [0_u16; PREFILL_SAMPLES];

    ALLOCATIONS.with(|count| count.set(Some(0)));
    playback(&mut output, 1, &mut speaker, &mut started, &stats);
    for _ in 0..100 {
        capture(&input, 1, &mut mic, None, &stats);
        bridge(&mut net_input, &mut net_output);
        playback(&mut output, 1, &mut speaker, &mut started, &stats);
    }
    for _ in 0..100 {
        capture(&input, 1, &mut mic, None, &stats);
    }
    for _ in 0..100 {
        playback(&mut output, 1, &mut speaker, &mut started, &stats);
    }
    capture(&input[..64], 1, &mut mic, None, &stats);
    playback(&mut output[..64], 1, &mut speaker, &mut started, &stats);
    capture(&input[..64], 3, &mut mic, None, &stats);
    playback(&mut output[..64], 3, &mut speaker, &mut started, &stats);
    capture(&integer_input, 1, &mut mic, None, &stats);
    bridge(&mut net_input, &mut net_output);
    playback(&mut integer_output, 1, &mut speaker, &mut started, &stats);
    let allocations = ALLOCATIONS.with(|count| count.replace(None));
    assert_eq!(allocations, Some(0));
    assert!(stats.input_overflows.load(Ordering::Relaxed) > 0);
    assert!(stats.output_underflows.load(Ordering::Relaxed) > 0);
    assert!(stats.telemetry().input_callbacks > 0);
    assert!(stats.telemetry().output_callbacks > 0);
    assert_eq!(stats.telemetry().last_output_frames, PREFILL_SAMPLES as u32);
}

#[test]
fn multichannel_mixer_never_allocates_during_playback_or_peer_reuse() {
    let (mut rx, mut mixer) = crate::mixer::channels();
    let controls = std::sync::Arc::clone(&mixer.controls);
    let stats = Stats::default();
    let mut output = [0.0; BUFFER_FRAMES * 2];
    let mut integer_output = [0_u16; BUFFER_FRAMES * 2];
    let chunk = [0.8; BUFFER_FRAMES];
    ALLOCATIONS.with(|count| count.set(Some(0)));
    mixer.playback(&mut output, 2, &stats);
    for _ in 0..40 {
        for id in 1..=8 {
            rx.push(id, &chunk);
        }
    }
    controls[0].gain.store(0.5_f32.to_bits(), Ordering::Relaxed);
    controls[1].mute.store(true, Ordering::Relaxed);
    controls[2].solo.store(true, Ordering::Relaxed);
    mixer.playback(&mut output, 2, &stats);
    for id in 1..=8 {
        rx.remove(id);
        rx.push(id, &chunk);
    }
    mixer.playback(&mut integer_output, 2, &stats);
    for _ in 0..40 {
        mixer.playback(&mut output, 2, &stats);
    }
    mixer.playback(&mut output[..64], 3, &stats);
    let snapshot = stats.telemetry();
    let allocations = ALLOCATIONS.with(|count| count.replace(None));
    assert_eq!(allocations, Some(0));
    assert!(snapshot.not_ready_samples > 0);
    assert!(snapshot.align_dropped_samples > 0);
    assert!(snapshot.output_callbacks > 0);
    assert_eq!(snapshot.last_output_frames, BUFFER_FRAMES as u32);
}

#[test]
fn direct_monitor_capture_and_mix_do_not_allocate_even_when_full_or_muted() {
    let (rx, mut mixer) = crate::mixer::channels();
    let mut monitor = mixer.enable_local_monitor();
    let (mut mic, _net) = RingBuffer::new(1);
    let stats = Stats::default();
    let mut output = [0_u16; BUFFER_FRAMES * 2];
    let input = [1_i16 << 14; BUFFER_FRAMES];
    ALLOCATIONS.with(|count| count.set(Some(0)));
    mixer.playback(&mut output, 2, &stats);
    for _ in 0..100 {
        capture(&input, 1, &mut mic, Some(&mut monitor), &stats);
    }
    mixer.playback(&mut output, 2, &stats);
    for muted in [true, false] {
        rx.local_control.mute.store(muted, Ordering::Relaxed);
        rx.local_control.solo.store(true, Ordering::Relaxed);
        capture(&input, 1, &mut mic, Some(&mut monitor), &stats);
        mixer.playback(&mut output, 2, &stats);
    }
    for _ in 0..100 {
        mixer.playback(&mut output, 2, &stats);
    }
    capture(&input, 3, &mut mic, Some(&mut monitor), &stats);
    mixer.playback(&mut output, 3, &stats);
    let allocations = ALLOCATIONS.with(|count| count.replace(None));
    assert_eq!(allocations, Some(0));
}
