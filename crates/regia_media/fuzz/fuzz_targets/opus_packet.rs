// SPDX-License-Identifier: MPL-2.0
//! Untrusted Opus packets through the video path's packet gate and decoder (ADR-1911, ADR-2306).
//! Seeds: `seeds/opus_packet/` (`cargo +nightly fuzz run opus_packet corpus/opus_packet seeds/opus_packet`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let _ = regia_media::fuzzing::opus_packets_untrusted(data);
});
