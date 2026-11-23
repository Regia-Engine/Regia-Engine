// SPDX-License-Identifier: MPL-2.0
//! Untrusted bytes through the WebM demuxer and the AV1, Opus and Vorbis decoders (ADR-1869 § 8).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let _ = regia_media::fuzzing::decode_untrusted(data);
});
