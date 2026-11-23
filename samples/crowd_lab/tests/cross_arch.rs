// SPDX-License-Identifier: MIT-0
//! Cross-architecture digests (ADR-5176): with `cross-platform-determinism` (glam's scalar math and
//! the pure-Rust libm) the lab gives the same digests on x86_64 and aarch64, Linux, Windows, Android
//! and macOS. The pins were measured on x86_64 Linux; the same test passes unchanged on the other
//! hosts. Without the feature the SIMD paths differ (SSE2 sums a 4-lane dot product as
//! `(x + z) + (y + w)`, NEON as `(x + y) + (z + w)`, and NEON's quaternion product fuses
//! multiply-adds), so the crowd digest is per architecture there; with glam's scalar path but the
//! platform `sinf`/`cosf`, Windows (UCRT) and Linux (glibc) differ.
//!
//! Re-pinned for ADR-5410: fixrev-06-k (`4bd0cad99`) blends the two-bone IK (the crowd's foot IK)
//! with `det::quat_slerp` instead of glam's `slerp`, an intended change of the crowd digest; the nav
//! digest is unchanged. The re-pin was checked on x86_64 Linux and on the aarch64 Android device.
//!
//! Run: `cargo test --release -p crowd_lab --features crowd_lab/cross-platform-determinism --test cross_arch`.
#![cfg(feature = "cross-platform-determinism")]

use crowd_lab::{ClipShape, LabConfig, run};

/// `(shape, digest, nav_digest)` for the default lab (240 agents, 300 steps).
const PINS: [(ClipShape, u64, u64); 3] = [
    (
        ClipShape::RootMotion,
        0x922a_d2a8_a3ac_fa97,
        0x6251_2ac5_793c_2f18,
    ),
    (ClipShape::InPlace, 0x502b_1e71_75bc_a5ba, 0x6251_2ac5_793c_2f18),
    (
        ClipShape::TravellingHips,
        0x0434_11cd_46b9_8276,
        0x6251_2ac5_793c_2f18,
    ),
];

#[test]
fn glam_runs_its_scalar_path() {
    // glam's SIMD `Quat` is 16-byte aligned; the scalar one (feature `scalar-math`) is not.
    assert_eq!(std::mem::align_of::<regia_core::math::Quat>(), 4);
}

#[test]
fn the_digests_match_the_x86_64_pins() {
    let mut wrong = Vec::new();
    for (shape, digest, nav_digest) in PINS {
        let summary = run(&LabConfig {
            shape,
            ..LabConfig::default()
        })
        .expect("lab run");
        if (summary.digest, summary.nav_digest) != (digest, nav_digest) {
            wrong.push(format!(
                "{shape:?}: digest={:016x} nav_digest={:016x}, pinned {digest:016x} {nav_digest:016x}",
                summary.digest, summary.nav_digest
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
