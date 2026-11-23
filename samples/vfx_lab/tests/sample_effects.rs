// SPDX-License-Identifier: MIT-0
//! The sample effects in `samples/vfx_lab/effects/` (ADR-545): they load, pass the lab's CPU
//! acceptance suite, hold their budgets, replay identically, and (GPU) render the same pixels
//! twice.

use std::path::PathBuf;

use regia_core::math::Vec3;
use regia_vfx::offscreen::VfxHarness;
use regia_vfx::scene::{VfxBudgetStats, VfxLibrary};
use regia_vfx::sim::{LodInput, SpawnJob};
use regia_vfx::{VfxCamera, VfxProfile, VfxScene};

const SAMPLES: [&str; 8] = [
    "sample_budget_storm",
    "sample_debris_cubes",
    "sample_ember_drift",
    "sample_kill_room",
    "sample_mesh_sparks",
    "sample_ribbon_trail",
    "sample_skinned_aura",
    "sample_smoke_stack",
];

fn effects_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("effects")
}

fn library() -> VfxLibrary {
    let library = VfxLibrary::load_dir(&effects_dir()).expect("sample effects dir");
    let mut names: Vec<&str> = library.defs().iter().map(|d| d.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names, SAMPLES,
        "every sample effect loads (a bad file is skipped with an error)"
    );
    library
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn sample_effects_load_and_round_trip() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for def in library().defs() {
        let text = def.to_ron().expect("save");
        let back = regia_vfx::VfxSystemDef::from_ron(&text).expect("reload");
        assert_eq!(&back, def, "{} round-trips", def.name);
    }
}

/// `vfx_lab --effects samples/vfx_lab/effects` checks the directory's effects (it used to
/// load them and then check only the built-ins).
#[test]
fn sample_effects_pass_the_lab_acceptance() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = effects_dir();
    assert_eq!(vfx_lab::run(dir.to_str().expect("utf-8 path")), 0);
}

/// One run of every sample effect at once, at a fixed binary-exact step, on the Mobile
/// budgets. Returns the per-frame budget stats and every spawn job's bits.
fn replay() -> (Vec<VfxBudgetStats>, Vec<u32>) {
    let mut scene = VfxScene::new(library());
    for (i, name) in SAMPLES.iter().enumerate() {
        scene
            .spawn(
                name,
                Vec3::new(i as f32 * 4.0 - 6.0, 0.0, 0.0),
                Vec3::Z,
                27 + i as u64,
            )
            .expect("spawn sample");
    }
    let profile = VfxProfile::mobile_high();
    let lod = LodInput {
        camera_pos: Vec3::new(0.0, 3.0, -14.0),
    };
    let mut stats = Vec::new();
    let mut bits = Vec::new();
    let mut jobs: Vec<SpawnJob> = Vec::new();
    for frame in 0..384 {
        scene.update_budgets(1.0 / 64.0, &lod, profile.particle_capacity, profile.smoke_budget);
        let frame_stats = scene.budget_stats();
        assert!(
            frame_stats.live <= profile.particle_capacity,
            "frame {frame}: pool over capacity"
        );
        assert!(
            frame_stats.live_alpha <= profile.smoke_budget,
            "frame {frame}: smoke over budget"
        );
        for instance in &frame_stats.instances {
            assert!(instance.live <= instance.budget, "frame {frame}: {instance:?}");
        }
        // The host's upload: drain every job.
        for instance in scene.instances_mut() {
            jobs.clear();
            instance.drain_jobs(&mut jobs);
            for job in &jobs {
                bits.extend(job.position.iter().chain(&job.velocity).map(|v| v.to_bits()));
                bits.push(job.lifetime.to_bits());
            }
        }
        stats.push(frame_stats);
    }
    (stats, bits)
}

/// The samples stay inside every budget, the storm reaches its own, and the same inputs
/// replay to the same budgets and bit-identical spawn jobs.
#[test]
fn sample_effects_hold_their_budgets_and_replay() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (stats, bits) = replay();
    let storm_peak = stats
        .iter()
        .filter_map(|s| s.instances.iter().find(|i| i.effect == "sample_budget_storm"))
        .map(|i| i.live)
        .max();
    assert_eq!(storm_peak, Some(1500), "the storm is held at its budget");
    let debris_mesh = stats.iter().map(|s| s.live_mesh).max().unwrap_or(0);
    assert_eq!(debris_mesh, 160, "the debris burst is all mesh particles");
    let (stats_again, bits_again) = replay();
    assert!(stats == stats_again, "budget stats differ between two runs");
    assert!(bits == bits_again, "spawn jobs differ between two runs");
}

/// Renders the alpha and mesh samples for `frames` frames on a fresh harness; returns the
/// last frame's pixels.
fn render_sorted_samples(device: &regia_render::RenderDevice, frames: u32) -> Vec<u8> {
    let mut library = VfxLibrary::new();
    for def in library_defs(&["sample_smoke_stack", "sample_debris_cubes"]) {
        library.add(def).expect("sample");
    }
    let mut scene = VfxScene::new(library);
    scene
        .spawn("sample_smoke_stack", Vec3::new(-0.6, 0.0, 0.0), Vec3::Z, 3)
        .expect("smoke");
    scene
        .spawn("sample_debris_cubes", Vec3::new(0.6, 0.2, 0.0), Vec3::Z, 4)
        .expect("debris");
    let mut harness = VfxHarness::new(device, VfxProfile::desktop_high()).expect("harness");
    let camera = VfxCamera::looking_at(Vec3::new(0.0, 2.5, -9.0), Vec3::new(0.0, 1.5, 0.0));
    let lod = LodInput {
        camera_pos: camera.eye,
    };
    let dt = 1.0 / 64.0;
    let mut pixels = Vec::new();
    for _ in 0..frames {
        scene.update_budgets(dt, &lod, 100_000, 8_192);
        scene.upload(harness.gpu());
        pixels = harness
            .render(device, &camera, 320, 180, dt)
            .expect("frame")
            .pixels;
    }
    pixels
}

fn library_defs(names: &[&str]) -> Vec<regia_vfx::VfxSystemDef> {
    let library = library();
    names
        .iter()
        .map(|name| {
            let index = library.index(name).expect("sample loaded");
            library.def(index).clone()
        })
        .collect()
}

/// Sorted lists (alpha smoke at equal depths, overlapping mesh debris) draw the same pixels
/// on two fresh runs: no order comes from GPU thread scheduling. Additive effects are left
/// out (their sum order is not fixed; see `book/agents/vfx.md`).
#[test]
fn sorted_samples_render_identically_twice() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let a = render_sorted_samples(&device, 48);
    let b = render_sorted_samples(&device, 48);
    assert!(!a.is_empty());
    let differing = a
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(x, y)| x != y)
        .count();
    assert_eq!(
        differing, 0,
        "{differing} pixels differ between two identical runs"
    );
}

/// Spec 26 § 6: "`samples/vfx_lab` with every module". Every module the spec names in § 3 is
/// used by at least one effect the lab runs (its sample directory plus the built-in DO27 set).
/// CPU only: it reads the definitions, no device.
#[test]
fn the_lab_effects_use_every_module_of_the_spec() {
    use regia_vfx::format::{RenderMode, ShapeDef, SpawnMode};
    let mut defs: Vec<regia_vfx::VfxSystemDef> = library().defs().to_vec();
    defs.extend(regia_vfx::showcase_effects());
    let emitters: Vec<_> = defs.iter().flat_map(|d| d.emitters.iter()).collect();
    let any = |f: &dyn Fn(&&regia_vfx::format::EmitterDef) -> bool| emitters.iter().any(f);
    let checks: [(&str, bool); 27] = [
        ("spawn rate", any(&|e| matches!(e.spawn.mode, SpawnMode::Rate(_)))),
        (
            "spawn burst",
            any(&|e| matches!(e.spawn.mode, SpawnMode::Burst(_))),
        ),
        (
            "spawn per distance",
            any(&|e| matches!(e.spawn.mode, SpawnMode::PerDistance { .. })),
        ),
        ("init point", any(&|e| matches!(e.init.shape, ShapeDef::Point))),
        (
            "init sphere",
            any(&|e| matches!(e.init.shape, ShapeDef::Sphere { .. })),
        ),
        ("init box", any(&|e| matches!(e.init.shape, ShapeDef::Box { .. }))),
        (
            "init cone",
            any(&|e| matches!(e.init.shape, ShapeDef::Cone { .. })),
        ),
        (
            "init mesh surface",
            any(&|e| matches!(e.init.shape, ShapeDef::MeshSurface { .. })),
        ),
        (
            "init skinned mesh",
            any(&|e| matches!(e.init.shape, ShapeDef::SkinnedMesh { .. })),
        ),
        ("init color", any(&|e| !e.init.color.is_empty())),
        (
            "init rotation",
            any(&|e| e.init.rotation.min != 0.0 || e.init.rotation.max != 0.0),
        ),
        ("gravity", any(&|e| e.update.gravity != (0.0, 0.0, 0.0))),
        ("drag", any(&|e| e.update.drag > 0.0)),
        ("curl noise", any(&|e| e.update.curl.is_some())),
        ("vortex", any(&|e| e.update.vortex.is_some())),
        ("attractor", any(&|e| e.update.attractor.is_some())),
        ("size over life", any(&|e| !e.update.size_over_life.is_empty())),
        ("color over life", any(&|e| !e.update.color_over_life.is_empty())),
        ("alpha over life", any(&|e| !e.update.alpha_over_life.is_empty())),
        ("depth collision", any(&|e| e.update.depth_collision.is_some())),
        ("kill volume", any(&|e| e.update.kill_volume.is_some())),
        ("billboard", any(&|e| e.render.mode == RenderMode::Billboard)),
        ("flipbook", any(&|e| e.render.flipbook.is_some())),
        ("soft particles", any(&|e| e.render.softness > 0.0)),
        (
            "stretched billboard",
            any(&|e| e.render.mode == RenderMode::Stretched),
        ),
        ("mesh particles", any(&|e| e.render.mode == RenderMode::Mesh)),
        ("ribbons", any(&|e| e.render.mode == RenderMode::Ribbon)),
    ];
    let missing: Vec<&str> = checks
        .iter()
        .filter(|(_, used)| !used)
        .map(|(name, _)| *name)
        .collect();
    assert!(
        any(&|e| e.render.light.is_some()),
        "no lab effect has light-emitting particles"
    );
    assert!(missing.is_empty(), "modules no lab effect uses: {missing:?}");
}
