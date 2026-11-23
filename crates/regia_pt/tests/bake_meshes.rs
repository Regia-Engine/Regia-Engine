// SPDX-License-Identifier: MPL-2.0
//! Mesh-aware GI bake (ADR-279) through the real kernels: static meshes placed by
//! `regia_pt::bake_meshes::add_meshes` are occluders for the probe bake. A probe inside a
//! closed box is relocated out of it or marked invalid (the Majercik relocation/validity path);
//! a sealed room's probes see no sky; a mirrored placement is front-facing (winding flipped); a
//! v0 description bakes the same volume as its v1 migration. Runs on whatever adapter
//! `RenderDevice::headless` picks (lavapipe in CI), like `probe_bake.rs`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regia_asset::baked_gi::{GiComponent, ProbeFlags};
use regia_core::glam::{Mat4, Vec3};
use regia_pt::bake_desc::{GiBakeDescription, GiBakeJob, MeshDesc, builtin, migrate_bake_ron};
use regia_pt::bake_meshes::{BakeMeshLoader, BakeMeshPart, BakeMeshSource, add_meshes};
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, bake_gi_volume};
use regia_pt::scenes::{merge, quad};
use regia_pt::{PathTracer, PtMaterial, PtMesh};
use regia_render::RenderDevice;

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

fn settings() -> GiBakeSettings {
    GiBakeSettings {
        trace: ProbeTraceSettings {
            rays_per_probe: 64,
            samples: 2,
            max_bounces: 3,
            seed: 11,
            samples_per_submit: 2,
            ..ProbeTraceSettings::default()
        },
        relocation_iterations: 2,
        filter: false,
        ..GiBakeSettings::default()
    }
}

/// An axis-aligned box of half size `h` around `c`, faces wound outward (or inward for a room
/// seen from inside); `skip` leaves out the face on `(axis, sign)`.
fn box_mesh(c: [f32; 3], h: f32, inward: bool, skip: Option<(usize, f32)>) -> PtMesh {
    let mut m = PtMesh::default();
    for axis in 0..3 {
        for sign in [-1.0f32, 1.0] {
            if skip == Some((axis, sign)) {
                continue;
            }
            let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
            let corner = |a: f32, b: f32| {
                let mut p = c;
                p[axis] += sign * h;
                p[u] += a * h;
                p[v] += b * h;
                p
            };
            let mut q = [
                corner(-1.0, -1.0),
                corner(1.0, -1.0),
                corner(1.0, 1.0),
                corner(-1.0, 1.0),
            ];
            let mut face = quad(q);
            let outward = face.normals[0][axis] * sign > 0.0;
            if outward == inward {
                q.reverse();
                face = quad(q);
            }
            merge(&mut m, &face);
        }
    }
    m
}

struct Fixed(PtMesh);

impl BakeMeshLoader for Fixed {
    fn load(&mut self, _path: &Path) -> Result<BakeMeshSource, String> {
        Ok(BakeMeshSource {
            parts: vec![BakeMeshPart::plain(self.0.clone(), [0.5; 3])],
        })
    }

    fn companions(&self, _path: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
}

fn mesh_entry(transform: Mat4) -> MeshDesc {
    MeshDesc {
        id: Some("fixture".into()),
        source: PathBuf::from("fixture.gltf"),
        transform: transform.to_cols_array(),
        instances: None,
        layer: Some("setpiece".into()),
        is_static: true,
        material: None,
        approximate: false,
    }
}

/// The furnace built-in (uniform sky of radiance 1, a 3 × 3 × 3 brick over −1..1) with `mesh`
/// placed through the bake's mesh path.
fn furnace_with(mesh: PtMesh) -> GiBakeJob {
    let mut job = builtin("furnace").expect("furnace");
    add_meshes(
        &mut job.scene,
        &[mesh_entry(Mat4::IDENTITY)],
        Path::new("/fixture"),
        &mut Fixed(mesh),
        None,
        [0.5; 3],
        "meshes",
    )
    .expect("place the fixture");
    job
}

/// Index of the brick's centre probe (1, 1, 1) at the origin.
const CENTRE: usize = 13;

#[test]
fn a_probe_inside_a_closed_box_is_relocated_out_or_invalid() {
    // A 0.6 m box: the centre probe can step out (relocation reaches 0.45 m of its 1 m cell).
    let job = furnace_with(box_mesh([0.0; 3], 0.3, false, None));
    let bake = bake_gi_volume(tracer(), &job.scene, &job.layout, &settings(), |_, _, _| true).expect("bake");
    let f = bake.raw[CENTRE].meta.flags;
    assert!(
        f & (ProbeFlags::RELOCATED | ProbeFlags::INSIDE_GEOMETRY) != 0,
        "flags {f:#x}"
    );
    if f & ProbeFlags::VALID != 0 {
        let p = bake.positions[CENTRE];
        assert!(
            p.iter().any(|v| v.abs() > 0.3),
            "a valid probe left the box: {p:?}"
        );
        assert!(f & ProbeFlags::RELOCATED != 0);
    }
    // The probes around it are outside the box and stay valid.
    for (i, p) in bake.raw.iter().enumerate() {
        if i != CENTRE {
            assert!(p.meta.is_valid(), "probe {i}: {:#x}", p.meta.flags);
        }
    }

    // A 1.6 m box: too deep to leave, so the centre probe is invalid.
    let job = furnace_with(box_mesh([0.0; 3], 0.8, false, None));
    let bake = bake_gi_volume(tracer(), &job.scene, &job.layout, &settings(), |_, _, _| true).expect("bake");
    let f = bake.raw[CENTRE].meta.flags;
    assert!(f & ProbeFlags::INSIDE_GEOMETRY != 0, "flags {f:#x}");
    assert!(f & ProbeFlags::VALID == 0);
    assert_eq!(bake.report.inside_geometry, 1);
}

#[test]
fn a_sealed_rooms_probes_see_no_sky() {
    let sky = |job: &GiBakeJob| {
        let bake =
            bake_gi_volume(tracer(), &job.scene, &job.layout, &settings(), |_, _, _| true).expect("bake");
        let l00: Vec<f32> = bake
            .raw
            .iter()
            .map(|p| {
                p.sh[GiComponent::Sky as usize]
                    .iter()
                    .map(|ch| ch[0])
                    .sum::<f32>()
            })
            .collect();
        (bake, l00)
    };
    // Open: the furnace's sky reaches every probe.
    let (_, open) = sky(&builtin("furnace").expect("furnace"));
    // A room open on +Y: sky through the ceiling.
    let (_, roofless) = sky(&furnace_with(box_mesh([0.0; 3], 1.5, true, Some((1, 1.0)))));
    // Sealed: walls, floor and ceiling face inward around every probe.
    let (bake, sealed) = sky(&furnace_with(box_mesh([0.0; 3], 1.5, true, None)));
    assert_eq!(
        bake.report.valid, 27,
        "inward walls are front faces: every probe is valid"
    );
    let reference = open[CENTRE];
    assert!(reference > 0.0);
    assert!(
        roofless[CENTRE] > 0.05 * reference,
        "{} vs {reference}",
        roofless[CENTRE]
    );
    for (i, s) in sealed.iter().enumerate() {
        assert!(
            s.abs() <= 1e-4 * reference,
            "probe {i} sees sky {s} (open {reference})"
        );
    }
}

#[test]
fn a_mirrored_placement_is_front_facing_to_the_tracer() {
    // A 20 m quad in the XY plane facing +Z, mirrored about X (DO27's set pieces, det −1).
    let q = quad([
        [-10.0, -10.0, 0.0],
        [10.0, -10.0, 0.0],
        [10.0, 10.0, 0.0],
        [-10.0, 10.0, 0.0],
    ]);
    let mirror = Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0));
    let probe = [[0.0, 0.0, 1.0]];
    let s = ProbeTraceSettings {
        rays_per_probe: 128,
        samples: 1,
        max_bounces: 1,
        ..ProbeTraceSettings::default()
    };
    let mut job = builtin("furnace").expect("furnace");
    add_meshes(
        &mut job.scene,
        &[mesh_entry(mirror)],
        Path::new("/fixture"),
        &mut Fixed(q.clone()),
        None,
        [0.5; 3],
        "meshes",
    )
    .expect("place");
    let t = tracer()
        .trace_probes(&job.scene, &probe, &s, |_, _| true)
        .expect("trace");
    assert!(t[0].hits > 0);
    assert_eq!(t[0].back_faces, 0, "the flipped copy faces the probe");

    // The same mirror on the unflipped mesh shows its back to the probe.
    let mut raw = builtin("furnace").expect("furnace");
    let mesh = raw.scene.add_mesh(q);
    let material = raw.scene.add_material(PtMaterial::lambert([0.5; 3]));
    raw.scene.add_instance(mesh, material, mirror);
    let t = tracer()
        .trace_probes(&raw.scene, &probe, &s, |_, _| true)
        .expect("trace");
    assert!(t[0].back_faces > 0);
}

#[test]
fn a_v0_description_bakes_the_same_volume_as_its_v1_migration() {
    let dir = regia_test_utils::unique_temp_dir("regia_pt_bake_v0_v1");
    let samples: Vec<u8> = (0..17u16 * 17).flat_map(|k| (k * 13).to_le_bytes()).collect();
    std::fs::write(dir.join("map.r16"), samples).expect("write");
    let v0 = r#"(
        terrain: Some((heights: "map.r16", width: 17, depth: 17, world_width: 32.0, height_scale: 6.0)),
        sun: Some((toward: (0.3, 0.8, 0.2), illuminance: 1000.0)),
        sky: Some((1.0, 1.0, 1.0)),
        layout: (terrain_spacing: Some((8.0, 8.0)), levels: [1.0, 3.0],
                 bricks: [(min: (0.0, 5.0, 0.0), max: (2.0, 7.0, 2.0), spacing: 1.0)]),
        bake: (rays_per_probe: 16, samples: 1, max_bounces: 2, seed: 3, relocation_iterations: 1, filter: false),
    )"#;
    let v1 = migrate_bake_ron(v0).expect("migrate");
    let bake = |text: &str| {
        let job = GiBakeDescription::from_ron(text)
            .expect("parse")
            .build(&dir)
            .expect("build");
        bake_gi_volume(tracer(), &job.scene, &job.layout, &job.settings, |_, _, _| true)
            .expect("bake")
            .volume
            .to_bytes()
            .expect("encode")
    };
    assert_eq!(bake(v0), bake(&v1));
    let _ = std::fs::remove_dir_all(&dir);
}
