// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-core/tests/tunnels_switchbacks.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 43-79 (`switchbacks_baked_file_builds_the_byte_identical_sim`), mark: adapt (Stage A: asserts unchanged).
// Edits: the inputs. The source reads the map's prediction terrain and its committed baked file from
// the game's content tree; here the terrain is the flat canyon floor fixture and the baked file is the
// layout's own export (`TunnelLayout::export_json`, the same writer), parsed by `compat::pack`. The
// navmesh half of the pin arrives with lane W-04 (`TunnelNavBake`). The shell twin below is original
// (requirements: the source's `a_mortar_shell_bursts_on_the_layer_it_is_in`, ir-sim/src/state.rs 23182-23244).

//! The Switchbacks pins of `compat::state` (spec 121 § 13.1, plan row W-02 acceptance 3).

#![allow(clippy::unwrap_used)]

use regia_detsim::float::{det_max, det_min};
use regia_world_geom::compat::fixtures::switchbacks;
use regia_world_geom::compat::pack::parse_tunnels;
use regia_world_geom::compat::sampler::{FloorSurface, HeightGrid, TerrainSampler};
use regia_world_geom::compat::state::TunnelsState;

/// The canyon floor, flat at -256 m, as the sim heightfield sees it.
fn canyon() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).expect("grid"), 1280.0).with_zero_counts(16384)
}

/// T1 "one bake": the baked file the runtime loads (`do27-tunnels/1`) rebuilds the sim's collision
/// mesh and nav overlay byte-identically to the splines, so the Switchbacks replay checksums cannot
/// move when the runtime arms its tunnels from the baked file instead of re-sampling the splines.
#[test]
fn switchbacks_baked_file_builds_the_byte_identical_sim() {
    let terrain = canyon();
    let text = switchbacks().export_json(Some(&terrain));
    let baked = parse_tunnels(&text).expect("the baked file parses");
    let from_spline = TunnelsState::<()>::for_layout(switchbacks(), Some(&terrain));
    let from_baked = TunnelsState::<()>::from_baked(&baked, &terrain);
    assert_eq!(
        from_baked.mesh, from_spline.mesh,
        "the baked file rebuilds the same collision mesh (byte-identical triangles)"
    );
    assert_eq!(
        from_baked.nav, from_spline.nav,
        "the baked file rebuilds the same nav overlay"
    );
    assert_eq!(
        from_baked.layout, from_spline.layout,
        "the baked file carries the same description and sampled centrelines"
    );
    assert_eq!(
        baked.triangle_count.map(|n| n as usize),
        Some(from_spline.mesh.as_ref().expect("mesh").triangles.len()),
        "triangle_count in the file agrees with the sim mesh"
    );
    // T3 (added here): the welded support floors, which the triangle equality above does not compare.
    assert_eq!(
        from_baked.mesh.as_ref().unwrap().support_floors(),
        from_spline.mesh.as_ref().unwrap().support_floors(),
        "the baked file rebuilds the same support floors"
    );
}

/// A shell flown on `TunnelsState::support` from `(-300, y0, 150)`: straight up and back down at
/// 60 ticks a second, bursting when it reaches the floor or the lid of the span it is in. Returns the
/// lowest and highest heights and where it burst.
fn fly(tunnels: &TunnelsState, terrain: &TerrainSampler, y0: f32) -> (f32, f32, f32, FloorSurface) {
    let (dt, g) = (1.0f32 / 60.0, 9.81f32);
    let (mut x, mut y, z, mut vy) = (-300.0f32, y0, 150.0f32, 14.0f32);
    let (mut lowest, mut highest) = (y, y);
    for _ in 0..60 * 30 {
        x += 2.0 * dt;
        vy -= g * dt;
        y += vy * dt;
        lowest = det_min(lowest, y);
        highest = det_max(highest, y);
        let span = tunnels.support(Some(terrain), x, z, y);
        if y <= span.floor.point[1] {
            return (lowest, highest, span.floor.point[1], span.floor.surface);
        }
        if span.ceiling.is_some_and(|lid| y >= lid) {
            return (lowest, highest, y, span.floor.surface);
        }
    }
    panic!("the shell never burst");
}

/// The Regia twin of the source's mortar pin on the one query: where the tube runs 30 m under the
/// canyon floor, a shell fired on the canyon floor bursts on the canyon floor (it never falls through
/// to the tube), and one fired in the tube never leaves it.
#[test]
fn a_shell_bursts_on_the_layer_it_is_in() {
    let terrain = canyon();
    let tunnels = TunnelsState::for_layout(switchbacks(), Some(&terrain));
    let (lowest, highest, burst, surface) = fly(&tunnels, &terrain, -255.0);
    assert!(highest > -254.0, "the shell rose ({highest})");
    assert!(
        lowest > -258.0,
        "the shell fell through the canyon floor toward the tube: lowest {lowest}"
    );
    assert_eq!(surface, FloorSurface::Heightfield, "burst on the canyon floor");
    assert!(
        (burst + 256.0).abs() < 0.05,
        "the shell burst on the canyon floor: {burst}"
    );
    // In the tube (floor -286, lid -274): the lid stops it on the way up.
    let (lowest, highest, burst, surface) = fly(&tunnels, &terrain, -285.0);
    assert!(
        highest < -273.0,
        "the shell left the tube through its lid: highest {highest}"
    );
    assert!(
        lowest > -288.0,
        "the shell left the tube through its floor: lowest {lowest}"
    );
    assert_eq!(surface, FloorSurface::TunnelFloor, "in the tube's span");
    assert!(burst > -288.0 && burst < -273.0, "burst inside the tube: {burst}");
}
