// SPDX-License-Identifier: MPL-2.0
//! Plan row W-21 (ADR-8371): the VisCell runtime loads the `<map>.viscells.ron` the mesh-layer
//! cook emits for the Switchbacks fixture. No hand-authored cells.

#![allow(clippy::unwrap_used)]

use regia_core::math::Vec3;
use regia_render::viscells::VisCells;
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, GroundInput, cook};
use regia_world_geom::cooked::CookedZoneShape;
use regia_world_geom::passages::PassagesDocument;

/// Switchbacks on the flat canyon floor, the same cook as the section tests.
fn cook_switchbacks() -> regia_world_geom::cook::CookOutput {
    let terrain =
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384);
    let ground = GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; 16],
    };
    let passages = PassagesDocument::from_layout(for_map("switchbacks").unwrap())
        .to_ron()
        .unwrap();
    let read = |_: &str| Ok(passages.as_bytes().to_vec());
    let doc = "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"sb.passages.ron\"))])";
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    // The Switchbacks test cook authors ADR-8359's ramp-facet waiver, as the other test cooks do (ADR-8436).
    match run(doc) {
        Ok(out) => out,
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            run(&waive_ramp_facets(doc, &report).unwrap()).unwrap()
        }
        Err(e) => panic!("{e} ({})", e.code()),
    }
}

/// The emitted file loads, lists one cell per section plus the outdoors, and `locate` hits a section box.
#[test]
fn switchbacks_viscells_load_in_the_runtime() {
    let out = cook_switchbacks();
    let cells = VisCells::from_ron(&out.viscells_ron).expect("the runtime loads the emitted file");
    assert_eq!(cells.cells.len(), out.file.canonical.sections.len() + 1);
    assert_eq!(cells.cells[0].id, 0);
    assert_eq!(cells.portals.len(), out.report.vis_portals.len());
    assert_eq!(cells.locate(Vec3::new(1.0e5, 1.0e5, 1.0e5)), 0);
    let mut hits = 0u32;
    for section in &out.file.canonical.sections {
        let zone = out
            .file
            .canonical
            .zones
            .iter()
            .find(|z| z.section == section.id)
            .unwrap();
        let CookedZoneShape::OrientedBox { center_mm, .. } = zone.shape else {
            continue;
        };
        let point = Vec3::new(
            center_mm[0] as f32 / 1000.0,
            center_mm[1] as f32 / 1000.0,
            center_mm[2] as f32 / 1000.0,
        );
        if cells.locate(point) == section.id + 1 {
            hits += 1;
        }
    }
    assert!(hits >= 1, "no section zone centre located to that section's cell");
}
