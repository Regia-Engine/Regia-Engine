// SPDX-License-Identifier: MPL-2.0
//! M18 robustness: mutation fuzzing of the `.vfx.ron` loader. See `docs/evidence/m18/robustness.md`.

// `VfxFormatError` embeds `ron::SpannedError` (`result_large_err` on the test closure).
#![allow(clippy::result_large_err)]

use regia_test_utils::fuzz::fuzz_text_loader;
use regia_vfx::format::VfxSystemDef;

const N: usize = 3_000;

#[test]
fn vfx_ron_never_panics() {
    let fixture = include_str!("fixtures/muzzle_flash.vfx.ron");
    fuzz_text_loader(fixture, 0xD1, N, VfxSystemDef::from_ron);
}

/// The v7 additions (mesh shapes, kill volumes, ribbons) as a second seed (ADR-2800).
#[test]
fn vfx_ron_v7_never_panics() {
    let fixture = include_str!("fixtures/shapes_ribbons_v7.vfx.ron");
    VfxSystemDef::from_ron(fixture).expect("the v7 seed is valid");
    fuzz_text_loader(fixture, 0xD7, N, VfxSystemDef::from_ron);
}
