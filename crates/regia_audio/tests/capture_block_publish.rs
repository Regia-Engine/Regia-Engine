// SPDX-License-Identifier: MPL-2.0
//! One microphone callback publishes one block, not one sample.
//!
//! The device callback is not opened here. [`AudioCapture::push_callback_block`] is the same
//! publish path the callback uses.

#![allow(unsafe_code)]

include!("support/alloc_count.rs");

use regia_audio::{AudioCapture, VoiceCaptureSettings};

#[test]
fn a_callback_block_is_one_publish_and_allocates_nothing() {
    let settings = VoiceCaptureSettings {
        buffer_secs: 1.0,
        ..VoiceCaptureSettings::default()
    };
    let mut capture = AudioCapture::headless(&settings);
    let block = vec![0.25_f32; 512];
    let published_before = capture.callback_blocks_published();
    let before = allocations();
    capture.push_callback_block(&block);
    let published = capture.callback_blocks_published() - published_before;
    let allocs = allocations() - before;
    assert_eq!(
        published, 1,
        "a 512-sample callback published {published} times (one block, not one sample)"
    );
    assert_eq!(allocs, 0, "publishing the block allocated {allocs} times");
    let published_after = capture.callback_blocks_published();
    let drained = capture.drain(1.0 / 60.0);
    assert_eq!(drained.len(), 512, "the block is still delivered whole");
    assert_eq!(
        capture.callback_blocks_published(),
        published_after,
        "draining must not publish again"
    );
}

/// `buffer_secs` is the ring's capacity whatever the device's callback length. The ring once had
/// one 4096-sample slot per 4096 frames of buffer and spent a whole slot on every callback, so
/// the default 2 s held 24 callbacks: 240 ms of 10 ms callbacks, 48 ms of Android's 96-frame
/// bursts. A game-thread stall longer than that dropped the player's voice.
#[test]
fn the_ring_holds_buffer_secs_of_short_callbacks() {
    let settings = VoiceCaptureSettings::default();
    assert_eq!((settings.buffer_secs, settings.sample_rate), (2.0, 48_000));
    for callback in [96usize, 480, 1_000, 4_096] {
        let mut capture = AudioCapture::headless(&settings);
        let block: Vec<f32> = (0..callback).map(|i| i as f32 / callback as f32).collect();
        // 1.9 s of callbacks with no drain (a long stall): under the 2 s the settings ask for.
        let blocks = 48_000 * 19 / 10 / callback;
        for _ in 0..blocks {
            capture.push_callback_block(&block);
        }
        assert_eq!(
            capture.dropped_frames(),
            0,
            "{callback}-frame callbacks: {blocks} blocks fit in 2 s"
        );
        let drained = capture.drain(1.0 / 60.0);
        assert_eq!(
            drained.len(),
            blocks * callback,
            "{callback}-frame callbacks: every sample is kept"
        );
        assert_eq!(&drained[..callback], &block[..], "in order");
    }
}

/// Past the capacity the oldest samples go, and each one is counted.
#[test]
fn an_overfull_ring_drops_the_oldest_and_counts_them() {
    let settings = VoiceCaptureSettings {
        buffer_secs: 0.1,
        ..VoiceCaptureSettings::default()
    };
    let mut capture = AudioCapture::headless(&settings);
    let pushed = 4_800 * 4;
    for i in 0..pushed / 96 {
        capture.push_callback_block(&[i as f32; 96]);
    }
    let drained = capture.drain(1.0 / 60.0);
    assert!(
        drained.len() >= 4_800,
        "at least buffer_secs is kept: {}",
        drained.len()
    );
    assert_eq!(
        drained.len() as u64 + capture.dropped_frames(),
        pushed as u64,
        "every sample is kept or counted"
    );
    assert_eq!(
        drained.last().copied(),
        Some((pushed / 96 - 1) as f32),
        "the newest are kept"
    );
}
