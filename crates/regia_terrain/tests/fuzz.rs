// SPDX-License-Identifier: MPL-2.0
//! M18 robustness: mutation fuzzing of the terrain layer and scatter loaders. See
//! `docs/evidence/m18/robustness.md`.

// `TerrainError` embeds `ron::SpannedError` (`result_large_err` on the test closure).
#![allow(clippy::result_large_err)]

use regia_terrain::{ScatterRule, TerrainLayers};
use regia_test_utils::fuzz::fuzz_text_loader;

const N: usize = 3_000;

#[test]
fn terrain_layers_ron_never_panics() {
    let fixture = include_str!("fixtures/ground.terrain.layers.ron");
    fuzz_text_loader(fixture, 0x11, N, TerrainLayers::from_ron);
}

#[test]
fn scatter_ron_never_panics() {
    let fixture = include_str!("fixtures/rocks.scatter.ron");
    fuzz_text_loader(fixture, 0x12, N, |source| ScatterRule::from_ron(source, None));
}
