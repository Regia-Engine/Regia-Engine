// SPDX-License-Identifier: MPL-2.0
//! The CPU probe tracer (`regia_pt::cpu_probe`, ADR-365) against the GPU probe kernels on the
//! same scene: a Lambertian floor and a closed box under a sun and a uniform sky. Each probe's
//! irradiance per component agrees within Monte Carlo noise and the documented differences (none
//! apply here: every surface is an ideal Lambertian, the sun is a delta, there is no emitter).
//! Runs the real kernels on whatever adapter `RenderDevice::headless` picks (lavapipe in CI).

use std::sync::OnceLock;

use regia_asset::baked_gi::{GiComponent, irradiance};
use regia_core::glam::{Mat4, Vec3};
use regia_pt::cpu_probe::CpuProbeTracer;
use regia_pt::probe::{ProbeTrace, ProbeTraceSettings, ProbeTracer};
use regia_pt::{PathTracer, PtCamera, PtMaterial, PtMesh, PtScene, PtSky, PtSun};
use regia_render::RenderDevice;

fn gpu() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

/// An axis-aligned box `min..max` facing outward, 12 triangles.
fn box_mesh(min: [f32; 3], max: [f32; 3]) -> PtMesh {
    let corner = |i: usize| {
        [
            if i & 1 == 0 { min[0] } else { max[0] },
            if i & 2 == 0 { min[1] } else { max[1] },
            if i & 4 == 0 { min[2] } else { max[2] },
        ]
    };
    let positions: Vec<[f32; 3]> = (0..8).map(corner).collect();
    // Counter-clockwise seen from outside.
    let faces: [[u32; 4]; 6] = [
        [0, 4, 6, 2],
        [1, 3, 7, 5],
        [0, 1, 5, 4],
        [2, 6, 7, 3],
        [0, 2, 3, 1],
        [4, 5, 7, 6],
    ];
    let indices = faces
        .iter()
        .flat_map(|f| [f[0], f[1], f[2], f[0], f[2], f[3]])
        .collect();
    PtMesh {
        normals: vec![[0.0; 3]; positions.len()],
        positions,
        indices,
        ..PtMesh::default()
    }
}

fn scene() -> PtScene {
    scene_with_box_from(0.0)
}

/// `scene()` with the box's bottom face at `bottom` (0: resting on the floor).
fn scene_with_box_from(bottom: f32) -> PtScene {
    let mut scene = PtScene::new(PtCamera::pinhole([0.0, 2.0, 8.0], [0.0; 3], 0.8, 4, 4));
    let floor = scene.add_material(PtMaterial::lambert([0.5; 3]));
    let block = scene.add_material(PtMaterial::lambert([0.7, 0.4, 0.2]));
    let plane = scene.add_mesh(PtMesh {
        positions: vec![
            [-200.0, 0.0, -200.0],
            [200.0, 0.0, -200.0],
            [-200.0, 0.0, 200.0],
            [200.0, 0.0, 200.0],
        ],
        normals: vec![[0.0, 1.0, 0.0]; 4],
        indices: vec![0, 2, 1, 1, 2, 3],
        ..PtMesh::default()
    });
    scene.add_instance(plane, floor, Mat4::IDENTITY);
    let cube = scene.add_mesh(box_mesh([-1.0, bottom, -1.0], [1.0, 2.0, 1.0]));
    scene.add_instance(cube, block, Mat4::IDENTITY);
    scene.sun = Some(PtSun {
        toward: Vec3::new(0.5, 0.75, 0.3).normalize().to_array(),
        color: [1.0, 0.95, 0.9],
        illuminance: 1000.0,
        angular_radius: 0.0,
    });
    scene.sky = PtSky::Uniform([40.0, 60.0, 90.0]);
    scene
}

fn settings() -> ProbeTraceSettings {
    ProbeTraceSettings {
        rays_per_probe: 1024,
        samples: 64,
        max_bounces: 3,
        seed: 3,
        samples_per_submit: 16,
        ..ProbeTraceSettings::default()
    }
}

/// Probes in the open, next to the box (its lit and shaded sides) and on top of it.
const PROBES: [[f32; 3]; 4] = [
    [0.0, 3.5, 0.0],
    [2.0, 0.6, 0.0],
    [-2.0, 0.6, -0.5],
    [6.0, 1.0, 4.0],
];

fn traces(tracer: &dyn ProbeTracer) -> Vec<ProbeTrace> {
    tracer
        .trace(&scene(), &PROBES, &settings(), &mut |_, _| true)
        .expect("trace probes")
}

#[test]
fn cpu_and_gpu_probes_agree_on_a_lit_floor_and_box() {
    let (g, c) = (traces(gpu()), traces(&CpuProbeTracer::new()));
    assert_eq!(gpu().backend(), "gpu");
    assert_eq!(CpuProbeTracer::new().backend(), "cpu");
    for (k, (g, c)) in g.iter().zip(&c).enumerate() {
        assert_eq!((g.rejected, c.rejected), (0, 0), "probe {k}");
        assert_eq!(g.rays, c.rays, "probe {k}");
        let hit_g = g.hits as f32 / g.rays as f32;
        let hit_c = c.hits as f32 / c.rays as f32;
        assert!(
            (hit_g - hit_c).abs() < 0.02,
            "probe {k}: hit fraction {hit_g} vs {hit_c}"
        );
        assert!(
            g.back_face_fraction() < 0.01 && c.back_face_fraction() < 0.01,
            "probe {k}"
        );
        for comp in [GiComponent::SunBounce, GiComponent::Sky] {
            for n in [
                [0.0, 1.0, 0.0],
                [0.0, -1.0, 0.0],
                [1.0, 0.0, 0.0],
                [-1.0, 0.0, 0.0],
            ] {
                let eg = irradiance(&g.sh[comp as usize], n);
                let ec = irradiance(&c.sh[comp as usize], n);
                for ch in 0..3 {
                    // 65 536 rays per probe: a few percent of noise on the bounced parts, plus an
                    // absolute floor for the near-zero ones. (At 4096 rays the near-zero side of a
                    // probe lit from one side scattered by ±1 from seed to seed: the L1 of the
                    // ~400 lux side nearly cancels there.)
                    let tol = 0.06 * eg[ch].abs().max(ec[ch].abs()) + 0.5;
                    assert!(
                        (eg[ch] - ec[ch]).abs() <= tol,
                        "probe {k} {comp:?} n {n:?} channel {ch}: gpu {} cpu {}",
                        eg[ch],
                        ec[ch]
                    );
                }
            }
        }
        for comp in [GiComponent::Fill, GiComponent::Emissive] {
            assert!(
                c.sh[comp as usize].iter().flatten().all(|v| *v == 0.0),
                "probe {k}"
            );
        }
    }
}

/// Inside the box both tracers see only back faces, so relocation and validity treat the probe the
/// same way. The box is lifted 1 cm here: resting on the floor its bottom face and the floor are
/// coplanar, and which of two faces at the same distance a ray reports is each BVH's tie-break
/// (the GPU's picked the box bottom for most of those rays, the CPU's the floor: 0.96 and 0.86).
#[test]
fn a_probe_inside_the_box_sees_back_faces_on_both() {
    let inside = [[0.0, 1.0, 0.0]];
    let scene = scene_with_box_from(0.01);
    let g = gpu()
        .trace(&scene, &inside, &settings(), &mut |_, _| true)
        .expect("gpu");
    let c = CpuProbeTracer::new()
        .trace(&scene, &inside, &settings(), &mut |_, _| true)
        .expect("cpu");
    assert!(g[0].back_face_fraction() > 0.9, "{}", g[0].back_face_fraction());
    assert!(c[0].back_face_fraction() > 0.9, "{}", c[0].back_face_fraction());
}
