// SPDX-License-Identifier: MPL-2.0
//! Full-pitch mouth fixtures shared by native ring and hop acceptance.
use regia_world_geom::WorldGeom;
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::layout::{SampledLayout, TunnelLayout};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, CookOutput, GroundInput, cook};
use regia_world_geom::passages::PassagesDocument;
/// The fixture's own acceptance ground, metres (ADR-8246 R1). Switchbacks sits on the canyon
/// floor at -256 m. Cinder Steps and Atacama measure height above a pit-floor datum at y = 0.
#[must_use]
pub fn acceptance_ground_m(map: &str) -> f32 {
    if map == "switchbacks" { -256.0 } else { 0.0 }
}
/// Cook every mouth cut and collar on the 4096-sample lattice pitch, on that map's acceptance ground.
pub fn world(map: &str) -> (CookOutput, WorldGeom, TunnelLayout, SampledLayout) {
    cook_world(map, true)
}

/// The same cook with no hole filled, so every mouth stays closed and the lattice is uncut.
pub fn world_closed(map: &str) -> (CookOutput, WorldGeom, TunnelLayout, SampledLayout) {
    cook_world(map, false)
}

fn cook_world(map: &str, fill: bool) -> (CookOutput, WorldGeom, TunnelLayout, SampledLayout) {
    let terrain = if map == "switchbacks" {
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384)
    } else {
        TerrainSampler::new(HeightGrid::square(4, vec![0; 16]).unwrap(), 1280.0).with_zero_counts(0)
    };
    let layout = for_map(map).unwrap();
    let sampled = layout.sample(Some(&terrain));
    let fills: Vec<_> = if fill {
        regia_world_geom::cut::passage_mouths(&layout, &layout.sample(Some(&terrain)))
            .iter()
            .map(|m| format!("{:?}", m.id))
            .collect()
    } else {
        Vec::new()
    };
    let document = format!(
        "(version:1,layers:[(id:\"rings\",source:Passages(path:\"map.passages.ron\"),fills:[{}])])",
        fills.join(",")
    );
    let passages = PassagesDocument::from_layout(layout.clone()).to_ron().unwrap();
    // offset -512 and count 8192 at 1/32 m is y = -256. A zero count at offset 0 is y = 0.
    let (offset_m, count) = if map == "switchbacks" {
        (-512.0, 8192u16)
    } else {
        (0.0, 0)
    };
    let ground = GroundInput {
        lattice_n: 4096,
        width_m: 1280.0,
        offset_m,
        step_m: 1.0 / 32.0,
        counts: vec![count; 4096 * 4096],
    };
    let read = |_: &str| -> Result<Vec<u8>, String> { Ok(passages.as_bytes().to_vec()) };
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    // ADR-8359: a test cook waives the two known Switchbacks ramp facets by hash and recooks.
    let output = match run(&document) {
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            run(&waive_ramp_facets(&document, &report).unwrap())
        }
        other => other,
    }
    .unwrap();
    let geom = WorldGeom::from_canonical_bytes(
        &regia_world_geom::cooked::encode_canonical(&output.file.canonical).unwrap(),
    )
    .unwrap();
    (output, geom, layout, sampled)
}
