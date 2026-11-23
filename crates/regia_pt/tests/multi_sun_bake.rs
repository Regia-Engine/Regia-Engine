// SPDX-License-Identifier: MPL-2.0
//! More than one sun in a bake description (`.bake.ron` v3, `suns: [...]`, ADR-7818): the probe and
//! lightmap bakes set up from the description integrate every sun. Each test bakes the same floor
//! under sun A alone, sun B alone and both, from description text, and checks that the energies add
//! (the direct light is linear in the lights, and the bounce too: one floor, nothing else to hit).
//! A v2 file's one `sun` migrates to a one-element list that bakes the same bytes.
//!
//! The CPU bakes need no GPU; the GPU probe bake and (feature `lightmap-gpu`) the GPU lightmap bake
//! run on whatever adapter `RenderDevice::headless` picks (lavapipe in CI). No golden: the
//! single-sun bakes are the reference.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regia_asset::baked_gi::{BakedGiVolume, GiComponent};
use regia_asset::lightmap::{Coverage, Lightmap};
use regia_pt::bake_desc::{BAKE_DESC_FORMAT_VERSION, GiBakeDescription, migrate_bake_ron};
use regia_pt::bake_meshes::{BakeMeshLoader, BakeMeshPart, BakeMeshSource};
use regia_pt::cpu_probe::CpuProbeTracer;
use regia_pt::lightmap::{
    LightmapBakeSettings, LightmapScene, MeshLightmapRequest, bake_lightmap, mesh_layout,
};
use regia_pt::probe::{ProbeTracer, bake_gi_volume};
use regia_pt::scenes::quad;
use regia_pt::{PathTracer, PtMesh, PtScene};
use regia_render::RenderDevice;

/// Sun A: 60° from the zenith toward +X, warm.
const SUN_A: &str = "(toward: (0.8660254, 0.5, 0.0), illuminance: 1000.0, color: (1.0, 0.9, 0.8))";
/// Sun B: 30° from the zenith toward −Z, blue, weaker.
const SUN_B: &str = "(toward: (0.0, 0.8660254, -0.5), illuminance: 400.0, color: (0.6, 0.7, 1.0))";

/// A 40 m floor facing up, the only mesh.
fn floor() -> PtMesh {
    let m = quad([
        [-20.0, 0.0, -20.0],
        [-20.0, 0.0, 20.0],
        [20.0, 0.0, 20.0],
        [20.0, 0.0, -20.0],
    ]);
    assert!(m.normals[0][1] > 0.99, "the floor faces up: {:?}", m.normals[0]);
    m
}

struct FloorLoader;

impl BakeMeshLoader for FloorLoader {
    fn load(&mut self, _path: &Path) -> Result<BakeMeshSource, String> {
        Ok(BakeMeshSource {
            parts: vec![BakeMeshPart::plain(floor(), [0.5; 3])],
        })
    }

    fn companions(&self, _path: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
}

/// A current-version description of the floor under `suns` (RON list items), no sky, one brick of
/// probes above the floor.
fn v3(suns: &[&str]) -> String {
    format!(
        "(\n    format_version: {BAKE_DESC_FORMAT_VERSION},\n    suns: [{}],\n    {}",
        suns.join(", "),
        BODY
    )
}

/// The rest of every description: the floor, a brick 1-2 m above it, a small deterministic bake.
const BODY: &str = r#"meshes: [(id: Some("floor"), source: "floor.gltf")],
    layout: (bricks: [(min: (-2.0, 1.0, -2.0), max: (2.0, 2.0, 2.0), spacing: 2.0)],
             moment_resolution: 4, max_distance: Some(4.0)),
    bake: (rays_per_probe: 64, samples: 2, max_bounces: 2, seed: 5, relocation_iterations: 0, filter: false),
)
"#;

/// The same scene as a format v2 file: one `sun`.
fn v2(sun: &str) -> String {
    format!("(\n    format_version: 2,\n    sun: Some({sun}),\n    {BODY}")
}

fn scene(text: &str) -> PtScene {
    let (desc, _) = GiBakeDescription::parse_versioned(text).expect("parses");
    desc.build_scene_with(Path::new("/fixture"), &mut FloorLoader)
        .expect("builds")
        .0
}

fn probe_volume(tracer: &dyn ProbeTracer, text: &str) -> BakedGiVolume {
    let (desc, _) = GiBakeDescription::parse_versioned(text).expect("parses");
    let job = desc
        .build_with(Path::new("/fixture"), &mut FloorLoader)
        .expect("builds");
    bake_gi_volume(tracer, &job.scene, &job.layout, &job.settings, |_, _, _| true)
        .expect("bake")
        .volume
}

fn lightmap(text: &str) -> Lightmap {
    let scene = LightmapScene::from_pt_scene(&scene(text)).expect("flatten");
    let uv = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let request = MeshLightmapRequest {
        instance: 0,
        name: "floor",
        uv: &uv,
        size: [16, 16],
        padding: 2,
    };
    let settings = LightmapBakeSettings {
        samples: 8,
        bounces: 2,
        seed: 3,
        direct_sun: true,
        filter: false,
        threads: 2,
        ..LightmapBakeSettings::default()
    };
    bake_lightmap(
        &scene,
        mesh_layout(&scene, &request).expect("layout"),
        &[0],
        &settings,
        |_, _| {},
    )
    .expect("bake")
    .0
}

fn gpu_tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device (lavapipe?)");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

/// Every texel both lightmaps baked: `both` = `a + b` within the RGB9E5 rounding (9 mantissa bits
/// of the texel's brightest channel, three roundings).
fn assert_lightmaps_add(a: &Lightmap, b: &Lightmap, both: &Lightmap) {
    let mut texels = 0;
    for y in 0..both.height {
        for x in 0..both.width {
            if both.coverage_at(x, y) != Coverage::BAKED {
                continue;
            }
            texels += 1;
            let (ea, eb, e) = (
                a.irradiance_at(x, y),
                b.irradiance_at(x, y),
                both.irradiance_at(x, y),
            );
            let peak = e.iter().copied().fold(0.0f32, f32::max);
            assert!(peak > 0.0, "texel ({x}, {y}) is lit");
            for k in 0..3 {
                let sum = ea[k] + eb[k];
                assert!(
                    (e[k] - sum).abs() <= 0.01 * peak,
                    "texel ({x}, {y}) channel {k}: both {} vs A + B {sum} ({} + {})",
                    e[k],
                    ea[k],
                    eb[k]
                );
            }
        }
    }
    assert!(texels > 100, "{texels} baked texels");
}

/// Every probe of the volumes: the sun component's downward irradiance (the floor's bounce) of
/// `both` = `a + b` within `tolerance` of the larger value; the other components stay dark.
fn assert_probe_suns_add(a: &BakedGiVolume, b: &BakedGiVolume, both: &BakedGiVolume, tolerance: f32) {
    assert_eq!(both.probe_count(), a.probe_count());
    let mut lit = 0;
    for i in 0..both.probe_count() {
        let down = [0.0, -1.0, 0.0];
        let (ea, eb, e) = (
            a.irradiance(i, GiComponent::SunBounce, down),
            b.irradiance(i, GiComponent::SunBounce, down),
            both.irradiance(i, GiComponent::SunBounce, down),
        );
        for k in 0..3 {
            let sum = ea[k] + eb[k];
            let scale = sum.abs().max(e[k].abs());
            assert!(
                (e[k] - sum).abs() <= tolerance * scale + 1e-3,
                "probe {i} channel {k}: both {} vs A + B {sum} ({} + {})",
                e[k],
                ea[k],
                eb[k]
            );
        }
        if e[1] > 0.0 {
            lit += 1;
        }
        let sky = both.irradiance(i, GiComponent::Sky, down);
        assert_eq!(sky, [0.0; 3], "probe {i}: no sky in these descriptions");
    }
    assert_eq!(lit, both.probe_count(), "every probe sees the lit floor");
}

/// A v3 description lists two suns: the first is the scene's sun, the second an extra sun, in
/// order, and the plan reports both.
#[test]
fn a_two_sun_description_builds_a_scene_with_both_suns() {
    let (desc, found) = GiBakeDescription::parse_versioned(&v3(&[SUN_A, SUN_B])).expect("parses");
    assert_eq!(found, BAKE_DESC_FORMAT_VERSION);
    assert_eq!(desc.suns.len(), 2);
    let job = desc
        .build_with(Path::new("/fixture"), &mut FloorLoader)
        .expect("builds");
    let sun = job.scene.sun.expect("the first sun is the scene's sun");
    assert_eq!((sun.illuminance, sun.color), (1000.0, [1.0, 0.9, 0.8]));
    assert_eq!(job.scene.extra_suns.len(), 1);
    assert_eq!(job.scene.extra_suns[0].illuminance, 400.0);
    assert_eq!(job.plan.lights.sun.as_ref().map(|s| s.illuminance), Some(1000.0));
    assert_eq!(
        job.plan
            .lights
            .extra_suns
            .iter()
            .map(|s| s.illuminance)
            .collect::<Vec<_>>(),
        vec![400.0]
    );
}

/// CPU lightmap bake: both suns = sun A alone + sun B alone, texel by texel (direct and bounce).
#[test]
fn two_suns_add_in_the_cpu_lightmap_bake() {
    let a = lightmap(&v3(&[SUN_A]));
    let b = lightmap(&v3(&[SUN_B]));
    let both = lightmap(&v3(&[SUN_A, SUN_B]));
    // The direct light at the floor's centre: E = Σ illuminance · color · cos θ.
    let (w, h) = (both.width / 2, both.height / 2);
    let e = both.irradiance_at(w, h);
    let want = [
        1000.0 * 1.0 * 0.5 + 400.0 * 0.6 * 0.866_025_4,
        1000.0 * 0.9 * 0.5 + 400.0 * 0.7 * 0.866_025_4,
        1000.0 * 0.8 * 0.5 + 400.0 * 1.0 * 0.866_025_4,
    ];
    for k in 0..3 {
        assert!(
            (e[k] - want[k]).abs() < 0.02 * want[k],
            "channel {k}: {} vs {}",
            e[k],
            want[k]
        );
    }
    assert_lightmaps_add(&a, &b, &both);
}

/// CPU probe bake: the probes' bounced sun of both suns = each alone, summed.
#[test]
fn two_suns_add_in_the_cpu_probe_bake() {
    let cpu = CpuProbeTracer::with_threads(2);
    let a = probe_volume(&cpu, &v3(&[SUN_A]));
    let b = probe_volume(&cpu, &v3(&[SUN_B]));
    let both = probe_volume(&cpu, &v3(&[SUN_A, SUN_B]));
    assert_probe_suns_add(&a, &b, &both, 0.01);
}

/// GPU probe bake (the path tracer's probe kernel): the same sum.
#[test]
fn two_suns_add_in_the_gpu_probe_bake() {
    let gpu = gpu_tracer();
    let a = probe_volume(gpu, &v3(&[SUN_A]));
    let b = probe_volume(gpu, &v3(&[SUN_B]));
    let both = probe_volume(gpu, &v3(&[SUN_A, SUN_B]));
    assert_probe_suns_add(&a, &b, &both, 0.01);
}

/// A v2 file's one `sun` and its on-disk migration (a one-element `suns`) bake byte-identical
/// probe volumes (CPU and GPU) and lightmaps; the one-element v3 file written by hand does too.
#[test]
fn a_v2_sun_and_its_migration_bake_the_same_bytes() {
    let old = v2(SUN_A);
    let migrated = migrate_bake_ron(&old).expect("migrates");
    assert!(migrated.contains("suns: ["), "{migrated}");
    assert!(!migrated.contains("sun: Some"), "{migrated}");
    let by_hand = v3(&[SUN_A]);
    let cpu = CpuProbeTracer::with_threads(2);
    let bytes = |tracer: &dyn ProbeTracer, text: &str| probe_volume(tracer, text).to_bytes().expect("encode");
    let cpu_old = bytes(&cpu, &old);
    assert_eq!(cpu_old, bytes(&cpu, &migrated), "CPU probe bake");
    assert_eq!(cpu_old, bytes(&cpu, &by_hand), "CPU probe bake, v3 by hand");
    let gpu_old = bytes(gpu_tracer(), &old);
    assert_eq!(gpu_old, bytes(gpu_tracer(), &migrated), "GPU probe bake");
    let map = |text: &str| lightmap(text).to_bytes().expect("encode");
    assert_eq!(map(&old), map(&migrated), "CPU lightmap bake");
    assert_eq!(
        scene(&old).content_hash(),
        scene(&migrated).content_hash(),
        "the scene hash a volume records"
    );
}

/// GPU lightmap bake (feature `lightmap-gpu`): both suns = each alone, summed.
#[cfg(feature = "lightmap-gpu")]
#[test]
fn two_suns_add_in_the_gpu_lightmap_bake() {
    use regia_pt::lightmap::bake_lightmap_gpu;
    let device = RenderDevice::headless().expect("headless device (lavapipe?)");
    let gpu_map = |text: &str| {
        let scene = LightmapScene::from_pt_scene(&scene(text)).expect("flatten");
        let uv = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let request = MeshLightmapRequest {
            instance: 0,
            name: "floor",
            uv: &uv,
            size: [16, 16],
            padding: 2,
        };
        let settings = LightmapBakeSettings {
            samples: 8,
            bounces: 2,
            seed: 3,
            direct_sun: true,
            filter: false,
            threads: 2,
            ..LightmapBakeSettings::default()
        };
        bake_lightmap_gpu(
            &device,
            &scene,
            mesh_layout(&scene, &request).expect("layout"),
            &[0],
            &settings,
            |_, _| {},
        )
        .expect("gpu bake")
        .0
    };
    let a = gpu_map(&v3(&[SUN_A]));
    let b = gpu_map(&v3(&[SUN_B]));
    let both = gpu_map(&v3(&[SUN_A, SUN_B]));
    assert_lightmaps_add(&a, &b, &both);
}
