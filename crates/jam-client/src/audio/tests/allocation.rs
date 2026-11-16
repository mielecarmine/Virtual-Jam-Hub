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
        capture(&input, 1, &mut mic, &stats);
        bridge(&mut net_input, &mut net_output);
        playback(&mut output, 1, &mut speaker, &mut started, &stats);
    }
    for _ in 0..100 {
        capture(&input, 1, &mut mic, &stats);
    }
    for _ in 0..100 {
        playback(&mut output, 1, &mut speaker, &mut started, &stats);
    }
    capture(&input[..64], 1, &mut mic, &stats);
    playback(&mut output[..64], 1, &mut speaker, &mut started, &stats);
    capture(&input[..64], 3, &mut mic, &stats);
    playback(&mut output[..64], 3, &mut speaker, &mut started, &stats);
    capture(&integer_input, 1, &mut mic, &stats);
    bridge(&mut net_input, &mut net_output);
    playback(&mut integer_output, 1, &mut speaker, &mut started, &stats);
    let allocations = ALLOCATIONS.with(|count| count.replace(None));
    assert_eq!(allocations, Some(0));
    assert!(stats.input_overflows.load(Ordering::Relaxed) > 0);
    assert!(stats.output_underflows.load(Ordering::Relaxed) > 0);
}
