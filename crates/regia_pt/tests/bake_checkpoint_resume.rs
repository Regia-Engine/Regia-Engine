// SPDX-License-Identifier: MPL-2.0
//! Checkpointed, resumable probe and GPU lightmap bakes (ADR-3095): a bake stopped by the test hook
//! at batch K and resumed writes the bytes of a bake that never stopped. The CPU probe tracer runs
//! anywhere; the GPU probe tracer and the GPU lightmap baker run on whatever adapter
//! `RenderDevice::headless` picks (lavapipe in CI).

use std::path::Path;
use std::sync::OnceLock;

use regia_asset::baked_gi::{DetailBrick, VolumeLayout};
use regia_core::glam::Mat4;
use regia_pt::checkpoint::{
    BAKE_CHECKPOINT_DAMAGED, BAKE_CHECKPOINT_MISMATCH, BAKE_INTERRUPTED, BakeCheckpointPlan,
    bake_checkpoint_error_code,
};
use regia_pt::cpu_probe::CpuProbeTracer;
use regia_pt::probe::{
    GiBakeSettings, ProbeTraceSettings, ProbeTracer, bake_gi_volume, bake_gi_volume_checkpointed,
};
use regia_pt::scenes::uv_sphere;
use regia_pt::{PathTracer, PtCamera, PtMaterial, PtScene, PtSky};
use regia_render::RenderDevice;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("headless device"))
}

fn gpu_tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

/// A ball of radius 0.5 at the origin under a uniform sky: probes inside it relocate.
fn ball_scene() -> PtScene {
    let mut scene = PtScene::new(PtCamera::pinhole([0.0, 0.0, 5.0], [0.0; 3], 0.5, 4, 4));
    scene.sky = PtSky::Uniform([1.0; 3]);
    let ball = scene.add_mesh(uv_sphere(0.5, 24, 12, false));
    let grey = scene.add_material(PtMaterial::lambert([0.5; 3]));
    scene.add_instance(ball, grey, Mat4::IDENTITY);
    scene
}

/// A 4 × 3 × 3 grid of probes 0.4 m apart around the ball.
fn grid() -> VolumeLayout {
    VolumeLayout {
        terrain: None,
        bricks: vec![DetailBrick {
            origin: [-0.6, -0.4, -0.4],
            spacing: [0.4; 3],
            counts: [4, 3, 3],
            first_probe: 0,
            flags: 0,
        }],
        moment_resolution: 4,
        max_distance: 4.0,
    }
}

/// `sample_groups` is a parameter: values above 1 are a GPU-only quality control that the CPU
/// probe tracer refuses (`CpuProbeTracer::trace_streams`), so CPU call sites pass 1.
fn settings(sample_groups: u32) -> GiBakeSettings {
    GiBakeSettings {
        trace: ProbeTraceSettings {
            rays_per_probe: 32,
            samples: 4,
            max_bounces: 2,
            seed: 11,
            samples_per_submit: 2,
            sample_groups,
            ..ProbeTraceSettings::default()
        },
        relocation_samples: 2,
        ..GiBakeSettings::default()
    }
}

fn plan(path: &Path, stop: Option<u64>) -> BakeCheckpointPlan {
    BakeCheckpointPlan {
        min_interval: std::time::Duration::ZERO,
        batch_units: 5,
        stop_after_batches: stop,
        ..BakeCheckpointPlan::new(path)
    }
}

/// Bakes `grid()` with `tracer`, stopping after batch 1 (the relocation) and batch 4 (three main
/// batches), then resuming to the end; returns the resumed volume's bytes and the reference's.
fn stop_twice_and_resume<T: ProbeTracer + ?Sized>(
    tracer: &T,
    name: &str,
    sample_groups: u32,
) -> (Vec<u8>, Vec<u8>) {
    let (scene, layout, s) = (ball_scene(), grid(), settings(sample_groups));
    let reference = bake_gi_volume(tracer, &scene, &layout, &s, |_, _, _| true).expect("reference");
    let dir = regia_test_utils::unique_temp_dir(name);
    let path = dir.join(".ball.fgiv.bakeck");
    for stop in [1, 4] {
        let Err(e) = bake_gi_volume_checkpointed(
            tracer,
            &scene,
            &layout,
            &s,
            None,
            &plan(&path, Some(stop)),
            |_, _, _| true,
        ) else {
            panic!("stopped expected")
        };
        assert_eq!(bake_checkpoint_error_code(&e), Some(BAKE_INTERRUPTED), "{e}");
        let h = regia_asset::bake_checkpoint::read_header(&path).expect("checkpoint");
        assert_eq!((h.stage, h.batches_done), (1, stop));
        assert_eq!(h.units_done, 5 * (stop - 1), "batch 1 is the relocation");
    }
    let mut last = (0, 0);
    let (bake, stats) = bake_gi_volume_checkpointed(
        tracer,
        &scene,
        &layout,
        &s,
        None,
        &plan(&path, None),
        |stage, d, t| {
            if stage == "bake" {
                last = (d, t);
            }
            true
        },
    )
    .expect("resumed");
    assert!(stats.resumed && stats.removed && !path.exists());
    assert_eq!(stats.resumed_units, 15);
    assert!(
        last.0 == last.1 && last.1 > 15,
        "progress counts every run: {last:?}"
    );
    assert_eq!(bake.report.probes, reference.report.probes);
    let _ = std::fs::remove_dir_all(dir);
    (
        bake.volume.to_bytes().expect("encode"),
        reference.volume.to_bytes().expect("encode"),
    )
}

/// The CPU probe tracer: stopped after the relocation and after three main batches, resumed, the
/// volume is byte-identical to an uninterrupted bake.
#[test]
fn a_resumed_cpu_probe_bake_is_byte_identical() {
    let (resumed, reference) = stop_twice_and_resume(&CpuProbeTracer::with_threads(2), "pt_ck_gi_cpu", 1);
    assert_eq!(resumed, reference);
}

/// The GPU probe tracer on the test adapter: each probe keys its streams by its index, so the
/// resumed volume is byte-identical on the same device and driver.
#[test]
fn a_resumed_gpu_probe_bake_is_byte_identical() {
    let (resumed, reference) = stop_twice_and_resume(gpu_tracer(), "pt_ck_gi_gpu", 2);
    assert_eq!(resumed, reference);
}

/// A checkpoint of another seed, of the other backend, or a damaged one, is refused with its code.
#[test]
fn another_bakes_checkpoint_is_refused() {
    let (scene, layout, s) = (ball_scene(), grid(), settings(1));
    let dir = regia_test_utils::unique_temp_dir("pt_ck_gi_refuse");
    let path = dir.join(".ball.fgiv.bakeck");
    let cpu = CpuProbeTracer::with_threads(2);
    let _ = bake_gi_volume_checkpointed(
        &cpu,
        &scene,
        &layout,
        &s,
        None,
        &plan(&path, Some(2)),
        |_, _, _| true,
    );
    assert!(path.exists());
    let mut reseeded = s;
    reseeded.trace.seed = 12;
    let Err(e) = bake_gi_volume_checkpointed(
        &cpu,
        &scene,
        &layout,
        &reseeded,
        None,
        &plan(&path, None),
        |_, _, _| true,
    ) else {
        panic!("seed expected")
    };
    assert_eq!(bake_checkpoint_error_code(&e), Some(BAKE_CHECKPOINT_MISMATCH));
    let Err(e) = bake_gi_volume_checkpointed(
        gpu_tracer(),
        &scene,
        &layout,
        &s,
        None,
        &plan(&path, None),
        |_, _, _| true,
    ) else {
        panic!("backend expected")
    };
    assert_eq!(bake_checkpoint_error_code(&e), Some(BAKE_CHECKPOINT_MISMATCH));
    let mut bytes = std::fs::read(&path).expect("read");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x40;
    std::fs::write(&path, &bytes).expect("damage");
    let Err(e) =
        bake_gi_volume_checkpointed(&cpu, &scene, &layout, &s, None, &plan(&path, None), |_, _, _| {
            true
        })
    else {
        panic!("damaged expected")
    };
    assert_eq!(bake_checkpoint_error_code(&e), Some(BAKE_CHECKPOINT_DAMAGED));
    let _ = std::fs::remove_dir_all(dir);
}

/// The GPU lightmap baker: stopped at a chunk and resumed, the lightmap's bytes match an
/// uninterrupted GPU bake.
#[cfg(feature = "lightmap-gpu")]
#[test]
fn a_resumed_gpu_lightmap_bake_is_byte_identical() {
    use regia_pt::lightmap::{
        LightmapBakeSettings, LightmapGpuControl, LightmapGpuOptions, LightmapScene, MeshLightmapRequest,
        bake_lightmap_gpu_checkpointed, bake_lightmap_gpu_with, mesh_layout, quad_box_scene,
    };
    let q = quad_box_scene();
    let scene = LightmapScene::from_pt_scene(&q.scene).expect("scene");
    let request = MeshLightmapRequest {
        instance: q.floor,
        name: "floor",
        uv: &q.floor_uv,
        size: [24, 24],
        padding: 1,
    };
    let layout = mesh_layout(&scene, &request).expect("layout");
    let receivers = vec![q.floor];
    let s = LightmapBakeSettings {
        samples: 8,
        bounces: 2,
        ..LightmapBakeSettings::default()
    };
    let opts = LightmapGpuOptions::default();
    let go = |_: &_| LightmapGpuControl::Continue;
    let (reference, _, _) =
        bake_lightmap_gpu_with(device(), &scene, layout.clone(), &receivers, &s, &opts, go)
            .expect("reference");
    let dir = regia_test_utils::unique_temp_dir("pt_ck_lightmap_gpu");
    let path = dir.join(".quad.flmap.bakeck");
    let mut p = BakeCheckpointPlan {
        batch_units: 64,
        ..plan(&path, Some(3))
    };
    let Err(e) =
        bake_lightmap_gpu_checkpointed(device(), &scene, layout.clone(), &receivers, &s, &opts, &p, go)
    else {
        panic!("stopped expected")
    };
    assert_eq!(bake_checkpoint_error_code(&e), Some(BAKE_INTERRUPTED), "{e}");
    p.stop_after_batches = None;
    let (map, _, _, stats) =
        bake_lightmap_gpu_checkpointed(device(), &scene, layout, &receivers, &s, &opts, &p, go)
            .expect("resumed");
    assert!(stats.resumed && stats.removed);
    assert_eq!(stats.resumed_units, 192);
    assert_eq!(
        map.to_bytes().expect("encode"),
        reference.to_bytes().expect("encode")
    );
    let _ = std::fs::remove_dir_all(dir);
}
