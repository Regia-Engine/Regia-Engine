// SPDX-License-Identifier: MPL-2.0
//! The large-mesh proxy WGSL (`regia_render::mesh_occluders`) is duplicated in the terrain
//! shader, which is plain WGSL without the engine's module composer: both copies must agree.
//! On the forward side the structs and pure helpers are the `mesh occluder geometry` block of
//! the `regia::forward::occluder_geometry` module (ADR-1552) and the functions that read the
//! occluder uniform are the `mesh occluders` block of `lab_forward.wgsl`; terrain.wgsl holds a
//! copy of each.

/// The text from `// BEGIN <name>` up to `// END <name>`; each marker occurs exactly once.
fn block<'a>(source: &'a str, name: &str) -> &'a str {
    let begin = format!("// BEGIN {name}");
    let end = format!("// END {name}");
    assert_eq!(source.matches(begin.as_str()).count(), 1, "one `{begin}` marker");
    assert_eq!(source.matches(end.as_str()).count(), 1, "one `{end}` marker");
    let start = source.find(begin.as_str()).expect("begin marker");
    let stop = source.find(end.as_str()).expect("end marker");
    assert!(start < stop, "`{begin}` comes before `{end}`");
    &source[start..stop]
}

/// The structs and pure form factors (`regia::forward::occluder_geometry`).
const GEOMETRY: &str = "mesh occluder geometry";
/// The functions that read the `mesh_occ` uniform (`lab_forward.wgsl`).
const OCCLUDERS: &str = "mesh occluders";

#[test]
fn occluder_wgsl_copies_match() {
    let geometry = include_str!("../../regia_render/shaders/forward_occluder_geometry.wgsl");
    let forward = include_str!("../../regia_render/shaders/lab_forward.wgsl");
    let terrain = include_str!("../src/gpu/terrain.wgsl");
    assert_eq!(block(geometry, GEOMETRY), block(terrain, GEOMETRY));
    assert_eq!(block(forward, OCCLUDERS), block(terrain, OCCLUDERS));
    // The forward shader reaches the geometry block only through the module import.
    assert!(forward.contains("#import regia::forward::occluder_geometry::"));
}
