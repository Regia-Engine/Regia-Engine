// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pilot_mesh.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 583-613 (two tests), mark: adapt (Stage A: asserts unchanged). Edits: neutral fixture paths in the
// backdrop test (the source's map names dropped), `ON_FOOT_PARAMS` for `slide_move`. Lines 615-746 read the
// game's asset store (a building shell, a shelter, the real maps' grid statistics) and stay the source's:
// `a_door_slab_in_an_authored_shell_opens_its_mouth_and_the_wall_stays` below is the original stand-in
// (requirements: the source's `barracks_shell_blocks_the_wall_and_keeps_a_door`), on an in-memory glTF.

#![allow(clippy::unwrap_used)]

use super::*;
use crate::compat::fixtures::ON_FOOT_PARAMS;

const P: &OnFootParams = &ON_FOOT_PARAMS;

#[test]
fn the_far_backdrop_is_never_a_pilot_collider() {
    let hero = Path::new("maps/ridge/setpieces/hero");
    assert!(!usable_gltf(&hero.join("SM_RidgeBackdrop.gltf")));
    assert!(usable_gltf(&hero.join("SM_RidgeColdStore.gltf")));
    assert!(usable_gltf(&hero.join("SM_RidgeRocks_3_4.gltf")));
    assert!(!usable_gltf(&hero.join("SM_RidgeColdStore.bin")));
}

/// Two shells meeting at a seam (one's slab end under the floor, the other's roof facet overhead)
/// keep two spans in the world grid, whichever file merges first.
#[test]
fn merging_shell_grids_keeps_their_wall_spans_apart_in_any_order() {
    let mut slab = PilotGrid::default();
    slab.stamp_wall(0.1, 0.1, -287.42, -285.98);
    slab.stamp_floor(0.1, 0.1, -286.0);
    let mut roof = PilotGrid::default();
    roof.stamp_wall(0.1, 0.1, -275.0, -270.3);
    let mut one = PilotGrid::default();
    merge_grid(&mut one, slab.clone());
    merge_grid(&mut one, roof.clone());
    let mut two = PilotGrid::default();
    merge_grid(&mut two, roof);
    merge_grid(&mut two, slab);
    assert_eq!(one, two);
    let cell = one.cells[&(0, 0)];
    assert_eq!(cell.spans().len(), 2, "{cell:?}");
    assert!(cell.has_floor && cell.floor_y == -286.0);
    let walked = crate::compat::onfoot::slide_move(P, &one, &[], -1.0, 0.4, 1.5, 0.4, -286.0, 0.35);
    assert!(walked[0] > 1.4, "the seam stops the walker at {walked:?}");
}

/// One named mesh of a test glTF: `(name, positions, triangle indices)` (no indices: a door slab).
type MeshSpec<'a> = (&'a str, Vec<[f32; 3]>, Vec<u32>);

/// A glTF of named meshes, one buffer, in memory.
fn shell_gltf(meshes: &[MeshSpec<'_>]) -> GltfMemory {
    let mut bin = Vec::new();
    let mut views = Vec::new();
    let mut accessors = Vec::new();
    let mut mesh_json = Vec::new();
    let mut nodes = Vec::new();
    for (i, (name, positions, indices)) in meshes.iter().enumerate() {
        let start = bin.len();
        for p in positions {
            for c in p {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        views.push(format!(
            r#"{{"buffer":0,"byteOffset":{start},"byteLength":{}}}"#,
            bin.len() - start
        ));
        accessors.push(format!(
            r#"{{"bufferView":{},"componentType":5126,"type":"VEC3","count":{}}}"#,
            views.len() - 1,
            positions.len()
        ));
        let pos = accessors.len() - 1;
        let prim = if indices.is_empty() {
            format!(r#"{{"attributes":{{"POSITION":{pos}}}}}"#)
        } else {
            let start = bin.len();
            for idx in indices {
                bin.extend_from_slice(&idx.to_le_bytes());
            }
            views.push(format!(
                r#"{{"buffer":0,"byteOffset":{start},"byteLength":{}}}"#,
                bin.len() - start
            ));
            accessors.push(format!(
                r#"{{"bufferView":{},"componentType":5125,"type":"SCALAR","count":{}}}"#,
                views.len() - 1,
                indices.len()
            ));
            format!(
                r#"{{"attributes":{{"POSITION":{pos}}},"indices":{}}}"#,
                accessors.len() - 1
            )
        };
        mesh_json.push(format!(r#"{{"name":"{name}","primitives":[{prim}]}}"#));
        nodes.push(format!(r#"{{"mesh":{i}}}"#));
    }
    let text = format!(
        r#"{{"buffers":[{{"uri":"shell.bin","byteLength":{}}}],"bufferViews":[{}],"accessors":[{}],"meshes":[{}],"nodes":[{}]}}"#,
        bin.len(),
        views.join(","),
        accessors.join(","),
        mesh_json.join(","),
        nodes.join(",")
    );
    GltfMemory::new("shell.gltf", text).with_buffer("shell.bin", bin)
}

/// The eight corners of an axis box and its twelve triangles.
fn box_mesh(min: [f32; 3], max: [f32; 3]) -> (Vec<[f32; 3]>, Vec<u32>) {
    let mut v = Vec::new();
    for i in 0..8u8 {
        v.push([
            if i & 1 == 0 { min[0] } else { max[0] },
            if i & 2 == 0 { min[1] } else { max[1] },
            if i & 4 == 0 { min[2] } else { max[2] },
        ]);
    }
    let faces: [[u32; 4]; 6] = [
        [0, 1, 3, 2],
        [4, 6, 7, 5],
        [0, 4, 5, 1],
        [2, 3, 7, 6],
        [0, 2, 6, 4],
        [1, 5, 7, 3],
    ];
    let mut idx = Vec::new();
    for [a, b, c, d] in faces {
        idx.extend_from_slice(&[a, b, c, a, c, d]);
    }
    (v, idx)
}

/// Original (requirements: the source's building-shell test): an authored wall 8 m wide with a
/// person-height door leaf in it. The leaf is not stamped and its slab carves the mouth: the doorway
/// stays open and the wall beside it blocks.
#[test]
fn a_door_slab_in_an_authored_shell_opens_its_mouth_and_the_wall_stays() {
    let (wall_v, wall_i) = box_mesh([-4.0, 0.0, -0.3], [4.0, 2.4, 0.3]);
    let (door_v, _) = box_mesh([-0.55, 0.0, -0.08], [0.55, 2.1, 0.08]);
    let gltf = shell_gltf(&[("Shell_LOD2", wall_v, wall_i), ("Door_Leaf", door_v, Vec::new())]);
    let grid = load_gltf_grid(P, &gltf, false, None).unwrap();
    assert!(
        grid.cells.len() > 20,
        "the shell stamps more than a handful of cells, got {}",
        grid.cells.len()
    );
    assert_eq!(grid.door_cuts.len(), 1, "{:?}", grid.door_cuts);
    let door = grid.slide(P, 0.0, 0.0, 0.0, 0.35);
    assert!(
        door[0].abs() < 0.05 && door[1].abs() < 0.05,
        "the doorway at x = 0 stays open, got {door:?}"
    );
    let wall = grid.slide(P, 2.4, 0.0, 0.0, 0.35);
    assert!(
        (wall[0] - 2.4).abs() > 0.2 || wall[1].abs() > 0.2,
        "the wall beside the door must move the capsule out of the solid, got {wall:?}"
    );
    // Through `load`, under the host's key: the building grid is kept; an empty source is skipped.
    let empty = shell_gltf(&[("Nothing", Vec::new(), Vec::new())]);
    let solids = load::<GltfMemory, ()>(
        P,
        &[],
        &[("hall".to_string(), gltf.clone()), ("ghost".to_string(), empty)],
        None,
        None,
    );
    assert_eq!(solids.buildings.keys().collect::<Vec<_>>(), ["hall"]);
    assert_eq!(solids.buildings["hall"], grid);
}

/// Original: a missing buffer is an error, not a panic, and a file source reads the same bytes as
/// the memory source.
#[test]
fn sources_report_missing_bytes_and_a_file_reads_like_memory() {
    let gltf = GltfMemory::new("bad.gltf", r#"{"buffers":[{"uri":"gone.bin"}],"meshes":[]}"#);
    assert!(gltf.buffer_range("gone.bin", 0, 4).is_err());
    assert!(load_gltf_grid(P, &GltfMemory::new("x", "{}"), false, None).is_err());

    let (wall_v, wall_i) = box_mesh([-4.0, 0.0, -0.3], [4.0, 2.4, 0.3]);
    let mem = shell_gltf(&[("Shell", wall_v, wall_i)]);
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "regia_onfoot_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("shell.gltf"), &mem.text).unwrap();
    std::fs::write(dir.join("shell.bin"), &mem.buffers["shell.bin"]).unwrap();
    let from_file = load_gltf_grid(P, &GltfFile::new(dir.join("shell.gltf")), false, None);
    let mut found = Vec::new();
    gltfs_in(&dir, &mut found);
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(from_file.unwrap(), load_gltf_grid(P, &mem, false, None).unwrap());
    assert_eq!(found, [dir.join("shell.gltf")]);
}
