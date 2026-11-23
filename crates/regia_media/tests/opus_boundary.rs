// SPDX-License-Identifier: MPL-2.0
//! The `opus` module's own `unsafe` without a decode: state creation, a move to another thread, and
//! the destroy. The rest of the library stops Miri (`docs/06-testing-and-ci.md` § 14, ADR-2086,
//! ADR-2726), and this is the part of the wrapper Miri can reach.

#![cfg(feature = "opus")]

use regia_media::opus::{OpusDecoder, OpusHead};

fn head(channels: u8) -> OpusHead {
    let mut bytes = b"OpusHead".to_vec();
    bytes.extend_from_slice(&[1, channels, 0x38, 0x01, 0x80, 0xbb, 0, 0, 0, 0, 0]);
    OpusHead::parse(&bytes).expect("a valid head")
}

#[test]
fn a_decoder_opens_moves_to_another_thread_and_closes() {
    for channels in [1, 2] {
        let decoder = OpusDecoder::from_head(head(channels)).expect("the library opens the layout");
        std::thread::spawn(move || drop(decoder))
            .join()
            .expect("the thread ends");
    }
}

/// A layout the library refuses (a stream count it rejects) is an error, and nothing is destroyed.
#[test]
fn a_refused_layout_is_an_error() {
    let mut bad = head(2);
    bad.coupled = 2;
    assert!(OpusDecoder::from_head(bad).is_err());
}
