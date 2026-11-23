// SPDX-License-Identifier: MPL-2.0
//! `.vfx.ron` v8 mesh-asset particles (ADR-7210) on a headless adapter (lavapipe in the gate):
//! a `Mesh` emitter whose `render.mesh` names a registered mesh draws that mesh (its index count
//! and its pixel coverage differ from the built-in cube control), and a mesh that is missing or
//! refused is reported (`missing_mesh` / `bad_mesh`) and draws nothing. A `lit` mesh is shaded
//! by the frame's sun and ambient (ADR-7261): brighter on its sun side, while the unlit control
//! ignores the sun. With several registered meshes each mesh's draw covers only its own
//! instances. No golden image: every number comes from the run itself, against a no-particle
//! backdrop frame and the unlit or cube control.

use std::sync::OnceLock;

use regia_core::math::Vec3;
use regia_render::RenderDevice;
use regia_vfx::format::{VFX_FORMAT_VERSION, VfxSystemDef};
use regia_vfx::gpu::VfxLighting;
use regia_vfx::gpu::mesh_draw::RenderMeshDraw;
use regia_vfx::mesh_shape::VfxMesh;
use regia_vfx::offscreen::VfxHarness;
use regia_vfx::scene::{VfxLibrary, VfxScene};
use regia_vfx::sim::LodInput;
use regia_vfx::{VfxCamera, VfxProfile};

const W: u32 = 160;
const H: u32 = 120;
const DT: f32 = 1.0 / 60.0;
const FRAMES: u32 = 4;
/// Where the particle sits: 2.5 m above the backdrop's ground plane (y = 0), so the ground
/// never hides part of it.
const AT: Vec3 = Vec3::new(0.0, 2.5, 0.0);

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    })
}

fn profile() -> VfxProfile {
    VfxProfile {
        particle_capacity: 1024,
        soft_particle_res: regia_vfx::gpu::SoftParticleRes::Full,
        ..VfxProfile::desktop_high()
    }
}

/// One still mesh particle at `AT`, 2 m across; `mesh` is the `render.mesh` value.
fn effect(mesh: &str) -> VfxSystemDef {
    effect_lit(mesh, false)
}

/// [`effect`] with the render module's `lit` flag.
fn effect_lit(mesh: &str, lit: bool) -> VfxSystemDef {
    VfxSystemDef::from_ron(&format!(
        r##"(
    version: {VFX_FORMAT_VERSION},
    name: "shard",
    emitters: [(
        name: "e",
        spawn: (mode: Burst(1), lifetime: (min: 30.0, max: 30.0)),
        init: (size: (min: 2.0, max: 2.0), color: [(t: 0.0, color: "#ffffff")]),
        render: (mode: Mesh, blend: Alpha, lit: {lit}, mesh: {mesh}),
    )],
)"##
    ))
    .expect("test effect parses")
}

/// A regular octahedron (vertices on the axes at 0.5), every face wound counter-clockwise seen
/// from outside (glTF's front face), checked here by the face normal pointing away from the
/// center.
fn octahedron() -> VfxMesh {
    let positions = [
        [0.5, 0.0, 0.0],
        [-0.5, 0.0, 0.0],
        [0.0, 0.5, 0.0],
        [0.0, -0.5, 0.0],
        [0.0, 0.0, 0.5],
        [0.0, 0.0, -0.5],
    ];
    let mut indices = Vec::new();
    for x in [0u32, 1] {
        for y in [2u32, 3] {
            for z in [4u32, 5] {
                let [a, b, c] = [x, y, z].map(|i| Vec3::from_array(positions[i as usize]));
                let outward = (b - a).cross(c - a).dot(a + b + c) > 0.0;
                indices.extend_from_slice(&if outward { [x, y, z] } else { [x, z, y] });
            }
        }
    }
    VfxMesh::from_triangles(&positions, &indices).expect("octahedron")
}

/// A regular tetrahedron inside the unit cube's corners (4 triangles, 12 indices).
fn tetrahedron() -> VfxMesh {
    VfxMesh::from_triangles(
        &[
            [0.5, 0.5, 0.5],
            [-0.5, -0.5, 0.5],
            [-0.5, 0.5, -0.5],
            [0.5, -0.5, -0.5],
        ],
        &[0, 1, 2, 0, 3, 1, 0, 2, 3, 1, 3, 2],
    )
    .expect("tetrahedron")
}

/// A mesh with more triangles than a particle mesh may draw (the GPU refuses it).
fn oversized() -> VfxMesh {
    let n = regia_vfx::gpu::mesh_draw::MAX_RENDER_MESH_TRIANGLES + 1;
    let mut positions = Vec::with_capacity(n * 3);
    let mut indices = Vec::with_capacity(n * 3);
    for i in 0..n {
        let x = i as f32 * 1e-4;
        let base = positions.len() as u32;
        positions.extend_from_slice(&[[x, 0.0, 0.0], [x + 1e-4, 0.0, 0.0], [x, 1e-4, 0.0]]);
        indices.extend_from_slice(&[base, base + 1, base + 2]);
    }
    VfxMesh::from_triangles(&positions, &indices).expect("many triangles")
}

/// What one run produced.
struct Run {
    pixels: Vec<u8>,
    /// Mesh-list particles the GPU counted on the last frame.
    mesh_particles: u32,
    render_meshes: u32,
    index_count: Option<u32>,
    issues: Vec<(String, &'static str)>,
}

/// Plays `def` (or nothing) for a few frames with `meshes` registered and returns the last frame.
fn run(def: Option<VfxSystemDef>, meshes: Vec<(&str, VfxMesh)>) -> Run {
    run_with(def, meshes, None).0
}

/// [`run`] under `lighting` (the harness default when `None`); also returns the per-mesh draws
/// the GPU recorded for the last frame.
fn run_with(
    def: Option<VfxSystemDef>,
    meshes: Vec<(&str, VfxMesh)>,
    lighting: Option<VfxLighting>,
) -> (Run, Vec<RenderMeshDraw>) {
    let mut library = VfxLibrary::new();
    for (name, mesh) in meshes {
        library.add_mesh(name, mesh);
    }
    let spawn = def.as_ref().map(|d| d.name.clone());
    if let Some(def) = def {
        library.add(def).expect("valid effect");
    }
    let mut scene = VfxScene::new(library);
    if let Some(name) = spawn {
        scene.spawn(&name, AT, Vec3::Z, 3).expect("spawned");
    }
    let camera = VfxCamera::looking_at(AT + Vec3::new(0.0, 0.0, -6.0), AT);
    let mut harness = VfxHarness::new(device(), profile()).expect("harness");
    if let Some(lighting) = lighting {
        harness.set_lighting(lighting);
    }
    let mut last = None;
    for _ in 0..FRAMES {
        scene.update(
            DT,
            &LodInput {
                camera_pos: camera.eye,
            },
            1024,
        );
        scene.upload(harness.gpu());
        last = Some(harness.render(device(), &camera, W, H, DT).expect("frame"));
    }
    let frame = last.expect("frames ran");
    let gpu = harness.gpu();
    let draws = gpu.read_render_mesh_draws();
    let run = Run {
        pixels: frame.pixels,
        mesh_particles: frame.counters[2],
        render_meshes: gpu.render_mesh_count(),
        index_count: gpu.render_mesh_index_count(1),
        issues: scene
            .render_mesh_issues()
            .iter()
            .map(|(name, (code, _))| (name.clone(), *code))
            .collect(),
    };
    (run, draws)
}

/// Pixels that differ from `backdrop` by more than a rounding step in any colour channel.
fn coverage(frame: &[u8], backdrop: &[u8]) -> usize {
    frame
        .as_chunks::<4>()
        .0
        .iter()
        .zip(backdrop.as_chunks::<4>().0)
        .filter(|(a, b)| (0..3).any(|c| a[c].abs_diff(b[c]) > 4))
        .count()
}

#[test]
fn a_registered_mesh_particle_draws_the_mesh_not_the_cube() {
    let backdrop = run(None, Vec::new());
    let cube = run(Some(effect("None")), Vec::new());
    let tetra = run(Some(effect(r#"Some("tetra")"#)), vec![("tetra", tetrahedron())]);
    assert_eq!(
        (cube.mesh_particles, tetra.mesh_particles),
        (1, 1),
        "one mesh particle each"
    );
    assert_eq!(cube.render_meshes, 0, "the cube control uploads no mesh");
    assert_eq!(tetra.render_meshes, 1);
    assert_eq!(
        tetra.index_count,
        Some(12),
        "the tetrahedron's indices, not the cube's 36"
    );
    assert!(tetra.issues.is_empty(), "{:?}", tetra.issues);
    let cube_px = coverage(&cube.pixels, &backdrop.pixels);
    let tetra_px = coverage(&tetra.pixels, &backdrop.pixels);
    println!("coverage: cube {cube_px} px, tetrahedron {tetra_px} px");
    assert!(cube_px > 200, "the cube control is visible: {cube_px} px");
    assert!(tetra_px > 50, "the tetrahedron is visible: {tetra_px} px");
    // Same particle (seed, size, rotation): the tetrahedron inside the cube's corners covers
    // well under the cube's silhouette.
    assert!(
        (tetra_px as f64) < 0.85 * cube_px as f64,
        "the tetrahedron ({tetra_px} px) must cover less than the cube ({cube_px} px)"
    );
    assert_ne!(tetra.pixels, cube.pixels);
}

#[test]
fn a_missing_mesh_is_reported_and_draws_nothing() {
    let backdrop = run(None, Vec::new());
    let missing = run(Some(effect(r#"Some("nowhere")"#)), vec![("tetra", tetrahedron())]);
    assert_eq!(missing.mesh_particles, 1, "the particle still simulates");
    assert_eq!(missing.render_meshes, 0, "nothing uploaded for a missing mesh");
    assert_eq!(missing.issues, vec![("nowhere".to_string(), "missing_mesh")]);
    assert_eq!(
        coverage(&missing.pixels, &backdrop.pixels),
        0,
        "a missing mesh draws nothing"
    );
}

#[test]
fn a_refused_mesh_is_reported_as_bad_and_draws_nothing() {
    let backdrop = run(None, Vec::new());
    let refused = run(Some(effect(r#"Some("huge")"#)), vec![("huge", oversized())]);
    assert_eq!(refused.render_meshes, 0, "the GPU refused the mesh");
    assert_eq!(refused.issues, vec![("huge".to_string(), "bad_mesh")]);
    assert_eq!(
        coverage(&refused.pixels, &backdrop.pixels),
        0,
        "a refused mesh draws nothing"
    );
}

/// A sun straight along `x` (world), white, with a low ambient.
fn side_sun(x: f32) -> VfxLighting {
    VfxLighting {
        sun_direction: [x, 0.0, 0.0],
        sun_color: [1.0, 1.0, 1.0],
        ambient: 0.2,
        exposure: 1.0,
    }
}

/// Mean luminance (0..=255, Rec. 709 weights) of the covered pixels left and right of the
/// frame's center column, and how many pixels each side has.
fn side_luma(frame: &[u8], backdrop: &[u8]) -> ([f64; 2], [usize; 2]) {
    let mut sum = [0.0f64; 2];
    let mut n = [0usize; 2];
    for (i, (a, b)) in frame
        .as_chunks::<4>()
        .0
        .iter()
        .zip(backdrop.as_chunks::<4>().0)
        .enumerate()
    {
        if !(0..3).any(|c| a[c].abs_diff(b[c]) > 4) {
            continue;
        }
        let x = i as u32 % W;
        if x == W / 2 {
            continue;
        }
        let side = usize::from(x > W / 2);
        sum[side] += 0.2126 * f64::from(a[0]) + 0.7152 * f64::from(a[1]) + 0.0722 * f64::from(a[2]);
        n[side] += 1;
    }
    let mean = |s: usize| if n[s] == 0 { 0.0 } else { sum[s] / n[s] as f64 };
    ([mean(0), mean(1)], n)
}

#[test]
fn a_lit_mesh_particle_is_brighter_on_its_sun_side() {
    let backdrop = run(None, Vec::new());
    let shade = |lit: bool, x: f32| {
        run_with(
            Some(effect_lit(r#"Some("octa")"#, lit)),
            vec![("octa", octahedron())],
            Some(side_sun(x)),
        )
        .0
    };
    // Unlit control: the sun does not reach it, so moving the sun changes nothing.
    let unlit_east = shade(false, 1.0);
    let unlit_west = shade(false, -1.0);
    assert!(
        coverage(&unlit_east.pixels, &backdrop.pixels) > 200,
        "the unlit octahedron is visible"
    );
    assert!(
        unlit_east.pixels == unlit_west.pixels,
        "an unlit mesh particle ignores the sun (the frames differ)"
    );
    // Lit: the half facing the sun is brighter than the half facing away, and moving the sun to
    // the other side swaps them.
    let lit_east = shade(true, 1.0);
    let lit_west = shade(true, -1.0);
    assert!(lit_east.issues.is_empty(), "{:?}", lit_east.issues);
    let (east, east_n) = side_luma(&lit_east.pixels, &backdrop.pixels);
    let (west, west_n) = side_luma(&lit_west.pixels, &backdrop.pixels);
    let (unlit, _) = side_luma(&unlit_east.pixels, &backdrop.pixels);
    println!(
        "luma left/right: lit sun +x {east:?} ({east_n:?} px), lit sun -x {west:?} ({west_n:?} px), unlit {unlit:?}"
    );
    assert!(
        east_n[0] > 50 && east_n[1] > 50,
        "both halves covered: {east_n:?}"
    );
    assert!(
        lit_east.pixels != unlit_east.pixels,
        "lighting changes the lit particle (the lit and unlit frames are identical)"
    );
    let east_gap = east[0] - east[1];
    let west_gap = west[0] - west[1];
    assert!(
        east_gap.abs() > 20.0,
        "the lit octahedron must have a bright sun side and a dark back: {east:?}"
    );
    assert!(
        east_gap.signum() != west_gap.signum() && west_gap.abs() > 20.0,
        "moving the sun across must move the bright side: sun +x {east:?}, sun -x {west:?}"
    );
    assert!(
        (unlit[0] - unlit[1]).abs() < 4.0,
        "the unlit control is even across: {unlit:?}"
    );
}

/// Burst `n` still mesh particles of `mesh` (a `render.mesh` value), 2 m apart along `x`.
fn emitter_ron(name: &str, n: u32, mesh: &str) -> String {
    format!(
        r##"(
        name: "{name}",
        spawn: (mode: Burst({n}), lifetime: (min: 30.0, max: 30.0)),
        init: (shape: Box(half_extents: (3.0, 0.5, 0.5)), size: (min: 0.5, max: 0.5), color: [(t: 0.0, color: "#ffffff")]),
        render: (mode: Mesh, blend: Alpha, mesh: {mesh}),
    )"##
    )
}

#[test]
fn each_registered_mesh_draws_only_its_own_instances() {
    let def = VfxSystemDef::from_ron(&format!(
        "(version: {VFX_FORMAT_VERSION}, name: \"pair\", emitters: [{}, {}, {}])",
        emitter_ron("tetras", 3, r#"Some("tetra")"#),
        emitter_ron("octas", 5, r#"Some("octa")"#),
        emitter_ron("cubes", 2, "None"),
    ))
    .expect("pair effect parses");
    let (run, mut draws) = run_with(
        Some(def),
        vec![("tetra", tetrahedron()), ("octa", octahedron())],
        None,
    );
    assert!(run.issues.is_empty(), "{:?}", run.issues);
    assert_eq!(
        run.mesh_particles, 10,
        "3 tetrahedra, 5 octahedra and 2 cubes simulate"
    );
    assert_eq!(run.render_meshes, 2);
    draws.sort_by(|a, b| a.mesh.cmp(&b.mesh));
    let summary: Vec<(&str, u32, u32)> = draws
        .iter()
        .map(|d| (d.mesh.as_str(), d.indices, d.instances))
        .collect();
    println!("per-mesh draws: {summary:?}");
    assert_eq!(
        summary,
        vec![("octa", 24, 5), ("tetra", 12, 3)],
        "one draw per mesh over only that mesh's live instances (the cubes draw in the cube pass)"
    );
}
