// SPDX-License-Identifier: MPL-2.0
//! Acceptance pins of `compat::onfoot` (plan row W-03, spec 121 § 4.1 row `compat::onfoot`).
//!
//! Original tests (requirements: the T3 on-foot rule of the source's `pilot_mesh.rs` 157-232, the
//! skylight-prop rule of its `pilot_floor_y`, and the span cap of `pilot_collide.rs` 39-47). The
//! fixtures build their own walls, decks, props and buildings: no game data enters the crate.

#![allow(clippy::unwrap_used)]

use regia_world_geom::compat::fixtures::{ON_FOOT_PARAMS, switchbacks};
use regia_world_geom::compat::mesh::synthetic_gallery;
use regia_world_geom::compat::onfoot::mesh::{GltfMemory, load_gltf_grid_on};
use regia_world_geom::compat::onfoot::{
    Blocker, MAX_WALL_SPANS, OnFootSolids, PILOT_STEP_M, PilotCell, PilotGrid, PilotSolids, cell_index,
    pilot_floor_y,
};
use regia_world_geom::compat::sampler::{HEIGHT_METERS_PER_COUNT, HeightGrid, TerrainSampler};
use regia_world_geom::compat::state::TunnelsState;
use regia_world_geom::compat::terrain_patch::TerrainPatch;

/// A host with a static grid and nothing else (no live buildings, no loose or fixed boxes, no rocks).
struct GridOnly(PilotSolids);

impl OnFootSolids for GridOnly {
    fn pilot_solids(&self) -> &PilotSolids {
        &self.0
    }
    fn for_each_building(&self, _f: &mut dyn FnMut(&str, [f32; 3], f32)) {}
    fn for_each_loose_blocker(&self, _f: &mut dyn FnMut(Blocker)) {}
    fn for_each_fixed_blocker(&self, _f: &mut dyn FnMut(Blocker)) {}
    fn rock_push_out(&self, x: f32, z: f32, _feet_y: f32, _radius: f32) -> [f32; 2] {
        [x, z]
    }
}

/// Acceptance 1: the cap of four disjoint wall spans per cell is a watched limit. A column of five
/// disjoint walls under one cell keeps four spans (the closest pair merges, as the source does) and
/// the grid reports the merge; four walls report nothing.
#[test]
fn cells_at_span_cap_is_reported() {
    let mut grid = PilotGrid::default();
    for k in 0..5u8 {
        let lo = f32::from(k) * 3.0;
        grid.stamp_wall(0.3, 0.3, lo, lo + 1.0);
    }
    let cell = grid.cells[&(cell_index(0.3), cell_index(0.3))];
    assert_eq!(cell.spans().len(), MAX_WALL_SPANS, "{cell:?}");
    assert_eq!(
        grid.span_cap_hits, 1,
        "the fifth span merged: the grid must report it"
    );

    let mut four = PilotGrid::default();
    for k in 0..4u8 {
        let lo = f32::from(k) * 3.0;
        four.stamp_wall(0.3, 0.3, lo, lo + 1.0);
    }
    assert_eq!(four.span_cap_hits, 0, "four spans fit the cap");

    // The cell-level call says so too, and a span that joins an existing one is not a cap hit.
    let mut cell = PilotCell::default();
    for k in 0..4u8 {
        let lo = f32::from(k) * 3.0;
        assert!(!cell.add_wall(lo, lo + 1.0));
    }
    assert!(!cell.add_wall(0.5, 1.05), "an overlapping span joins, no cap");
    assert!(cell.add_wall(20.0, 21.0), "a fifth disjoint span hits the cap");
}

fn flat_terrain(side: usize, world_width: f32, raw_height: u16) -> TerrainSampler {
    let heights = vec![raw_height; side * side];
    TerrainSampler::new(HeightGrid::square(side, heights).unwrap(), world_width)
}

/// A glTF with one mesh of axis quads (`[min_x, max_x, min_z, max_z, y]` each), in memory.
fn deck_gltf(quads: &[[f32; 5]]) -> GltfMemory {
    let mut bin = Vec::new();
    for q in quads {
        for (x, z) in [(q[0], q[2]), (q[1], q[2]), (q[1], q[3]), (q[0], q[3])] {
            for c in [x, q[4], z] {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
    }
    let pos_len = bin.len();
    let mut count = 0u32;
    for i in 0..quads.len() as u32 {
        // Counter-clockwise seen from above (+y normal): 0, 2, 1 and 0, 3, 2.
        for idx in [0, 2, 1, 0, 3, 2] {
            bin.extend_from_slice(&(i * 4 + idx).to_le_bytes());
            count += 1;
        }
    }
    let text = format!(
        r#"{{"buffers":[{{"uri":"deck.bin","byteLength":{total}}}],
        "bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":{pos_len}}},{{"buffer":0,"byteOffset":{pos_len},"byteLength":{idx_len}}}],
        "accessors":[{{"bufferView":0,"componentType":5126,"type":"VEC3","count":{verts}}},{{"bufferView":1,"componentType":5125,"type":"SCALAR","count":{count}}}],
        "meshes":[{{"name":"Deck","primitives":[{{"attributes":{{"POSITION":0}},"indices":1}}]}}],
        "nodes":[{{"mesh":0}}]}}"#,
        total = bin.len(),
        idx_len = bin.len() - pos_len,
        verts = quads.len() * 4,
    );
    GltfMemory::new("deck.gltf", text).with_buffer("deck.bin", bin)
}

/// Acceptance 3 (T3): a set-piece deck drawn 0.2 m over the gallery's tube floor is the tube floor,
/// so the grid drops it and the cell keeps one floor (the sim's); a deck 1.0 m up is a real deck and
/// stays. Without the mesh layers the same 0.2 m deck is stamped (the heightfield is 4.8 m away).
#[test]
fn a_drawn_floor_within_30cm_of_a_sim_floor_is_dropped() {
    let terrain = flat_terrain(4, 40.0, (8.0 / HEIGHT_METERS_PER_COUNT) as u16);
    assert!(terrain.height_at_world(0.0, 0.0).abs() < 1e-4);
    let tunnels = TunnelsState::<()>::with_mesh(true, synthetic_gallery(10.0, 10.0, -5.0, 1.0, true));
    let near = tunnels.nearest_floor(Some(&terrain), -2.0, 0.0, -4.8);
    assert!(
        (near + 5.0).abs() < 1e-4,
        "the gallery floor is the sim floor there: {near}"
    );

    let gltf = deck_gltf(&[[-3.0, -1.0, -1.0, 1.0, -4.8], [1.0, 3.0, -1.0, 1.0, -4.0]]);
    let grid = load_gltf_grid_on(&ON_FOOT_PARAMS, &gltf, true, Some(&terrain), Some(&tunnels)).unwrap();
    let low = grid.cells.get(&(cell_index(-2.0), cell_index(0.0)));
    assert!(
        low.is_none_or(|cell| !cell.has_floor),
        "the drawn deck 0.2 m over the tube floor is dropped: {low:?}"
    );
    let high = grid.cells[&(cell_index(2.0), cell_index(0.0))];
    assert!(high.has_floor && (high.floor_y + 4.0).abs() < 1e-5, "{high:?}");

    // One floor in the cell: a walker on the tube floor stays on it, and steps up onto the real deck.
    let solids = GridOnly(PilotSolids {
        world: grid,
        ..PilotSolids::default()
    });
    let on_tube = pilot_floor_y(&solids, Some(&terrain), &ON_FOOT_PARAMS, -2.0, 0.0, -5.0, -5.0);
    assert_eq!(on_tube, -5.0);
    let on_deck = pilot_floor_y(&solids, Some(&terrain), &ON_FOOT_PARAMS, 2.0, 0.0, -4.5, -5.0);
    assert!((on_deck + 4.0).abs() < 1e-5, "{on_deck}");

    // Control: on the heightfield alone the 0.2 m deck is a drawn floor and is stamped.
    let plain = load_gltf_grid_on::<()>(&ON_FOOT_PARAMS, &gltf, true, Some(&terrain), None).unwrap();
    let kept = plain.cells[&(cell_index(-2.0), cell_index(0.0))];
    assert!(kept.has_floor && (kept.floor_y + 4.8).abs() < 1e-5, "{kept:?}");
}

/// The canyon floor, flat at -256 m.
fn canyon() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384)
}

/// The rubble pile at the mid-east skylight of the Switchbacks fixture, on its ramp (about one third
/// of the way up from the tube): a 1.8 m cone, 6 m across, drawn as a 0.5 m `*_skylight` patch whose
/// heights are the ramp floor under each cell plus the cone. Returns the patched terrain, the tunnel
/// state and the cone centre.
fn ramp_with_skylight_cone() -> (TerrainSampler, TunnelsState, f32, f32) {
    let canyon = canyon();
    let tunnels = TunnelsState::for_layout(switchbacks(), Some(&canyon));
    let (cx, cz) = (16.0f32, -126.0f32);
    let (cols, rows) = (25usize, 25usize);
    let origin = [cx - 6.0, cz - 6.0];
    let ramp_y = tunnels.move_floor(Some(&canyon), cx, cz, -270.0).point[1];
    let heights = (0..rows * cols)
        .map(|i| {
            let (x, z) = (
                origin[0] + (i % cols) as f32 * 0.5,
                origin[1] + (i / cols) as f32 * 0.5,
            );
            let floor = tunnels.move_floor(Some(&canyon), x, z, ramp_y + 1.0).point[1];
            let d = regia_detsim::math::sqrt((x - cx) * (x - cx) + (z - cz) * (z - cz));
            floor + 1.8 * (1.0 - regia_detsim::float::det_min(d / 6.0, 1.0))
        })
        .collect();
    let patch = TerrainPatch {
        id: "mid_east_skylight".into(),
        origin,
        spacing: 0.5,
        cols,
        rows,
        heights,
    };
    (canyon.with_patches(vec![patch]), tunnels, cx, cz)
}

/// Acceptance 4 (T3 addendum § 1): a walker on the ramp beside the mid-east skylight's rubble stands
/// on the cone top when it is no more than one step (0.55 m) plus 0.02 m over the feet, and keeps the
/// ramp floor when the cone is higher than that.
#[test]
fn pilot_floor_y_takes_the_skylight_prop() {
    let (terrain, tunnels, cx, cz) = ramp_with_skylight_cone();
    let solids = GridOnly(PilotSolids::default());
    let mut took = 0;
    let mut kept = 0;
    for k in 0..=20u8 {
        let x = cx - 5.0 + f32::from(k) * 0.25;
        let feet = tunnels.move_floor(Some(&terrain), x, cz, -270.0).point[1];
        let top = terrain.height_at_world(x, cz);
        assert!(terrain.shaft_at(x, cz), "inside the skylight patch at {x}");
        let got = pilot_floor_y(&solids, Some(&terrain), &ON_FOOT_PARAMS, x, cz, feet, feet);
        if top <= feet + PILOT_STEP_M + 0.02 && top > feet {
            assert_eq!(
                got, top,
                "x {x}: the cone top {top} is one step over the feet {feet}"
            );
            took += 1;
        } else {
            assert_eq!(
                got, feet,
                "x {x}: the cone top {top} is out of reach of the feet {feet}"
            );
            if top > feet {
                kept += 1;
            }
        }
    }
    assert!(
        took >= 3 && kept >= 3,
        "the walk crosses both bands: took {took}, kept {kept}"
    );
    // Standing on the rubble a walker climbs the next step of it.
    let x = cx - 1.5;
    let feet = terrain.height_at_world(cx - 2.0, cz);
    let top = terrain.height_at_world(x, cz);
    assert!(top > feet && top <= feet + PILOT_STEP_M + 0.02);
    let ramp = tunnels.move_floor(Some(&terrain), x, cz, -270.0).point[1];
    assert_eq!(
        pilot_floor_y(&solids, Some(&terrain), &ON_FOOT_PARAMS, x, cz, feet, ramp),
        top
    );
    // Without the patch (no terrain) the prop is gone.
    assert_eq!(
        pilot_floor_y(&solids, None, &ON_FOOT_PARAMS, x, cz, feet, ramp),
        ramp
    );
}
