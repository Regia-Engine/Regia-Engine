// SPDX-License-Identifier: MPL-2.0
//! The `cooked-video` target (ADR-2410): the cooked payload of a small WebM. The loader decodes the
//! header and its clip report, validates the media bytes after it the way an import does, and
//! re-encodes the payload (`regia_media::validate`).

use std::io::Cursor;

use regia_media::mux::PatternClip;
use regia_media::validate::{COOKED_VIDEO_VERSION, decode_cooked_video, encode_cooked_video};

use super::{FuzzInput, FuzzTarget};

/// A four-frame pattern clip, cooked as `regia cook` cooks a source: the report of the real file and
/// the file itself. The clip is smaller than the 4 KiB header, so the harness's cut at half the
/// payload's length lands inside the header, which refuses it.
fn fixture() -> Vec<u8> {
    let file = PatternClip::new(16, 16, 4, 25).to_webm();
    let Ok(report) = regia_media::validate(Cursor::new(&file)) else {
        return Vec::new();
    };
    encode_cooked_video(&report, &file).unwrap_or_default()
}

fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let (report, file) = decode_cooked_video(bytes).map_err(|e| e.to_string())?;
    // The header's report is not checked against the file: a damaged file must only refuse cleanly.
    let _ = regia_media::validate(Cursor::new(file));
    encode_cooked_video(&report, file).map_err(|e| e.to_string())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![FuzzTarget {
        name: "cooked-video",
        format: Some("cooked-video"),
        format_version: COOKED_VIDEO_VERSION,
        round_trip: true,
        input: FuzzInput::Bytes { fixture, load },
    }]
}
