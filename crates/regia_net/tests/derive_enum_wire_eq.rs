// SPDX-License-Identifier: MPL-2.0
//! `#[derive(Replicate)]` on enums compares fields without encoding scratch buffers, and agrees
//! with the byte comparison the default `wire_eq` makes.

use regia_net::codec::{CodecContext, ReplicateCodec, encode_to_vec};
use regia_net::*;

#[derive(Clone, Debug, PartialEq, Replicate)]
enum Pose {
    Idle,
    Run {
        #[replicate(quantize(min = 0.0, max = 10.0, bits = 8))]
        heading: f32,
        clip: String,
        #[replicate(skip)]
        cache: u8,
    },
    Pair(u8, Option<u16>),
}

fn bytes_eq(a: &Pose, b: &Pose) -> bool {
    let cx = CodecContext::detached();
    encode_to_vec(a, &cx) == encode_to_vec(b, &cx)
}

#[test]
fn enum_wire_eq_matches_encoded_bytes() {
    let cx = CodecContext::detached();
    let run = |h: f32, c: &str, k: u8| Pose::Run {
        heading: h,
        clip: c.into(),
        cache: k,
    };
    let all = [
        Pose::Idle,
        run(1.0, "a", 0),
        run(1.0, "a", 9), // skipped field differs: still equal on the wire
        run(1.0, "b", 0),
        run(5.0, "a", 0),
        Pose::Pair(1, None),
        Pose::Pair(1, Some(2)),
        Pose::Pair(2, Some(2)),
    ];
    for a in &all {
        for b in &all {
            assert_eq!(a.wire_eq(b, &cx), bytes_eq(a, b), "{a:?} vs {b:?}");
        }
    }
    assert!(run(1.0, "a", 0).wire_eq(&run(1.0, "a", 9), &cx));
    assert!(!Pose::Idle.wire_eq(&run(1.0, "a", 0), &cx));
}
