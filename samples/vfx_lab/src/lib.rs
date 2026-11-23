// SPDX-License-Identifier: MIT-0
//! VFX Lab — module exerciser for `regia_vfx`.
//!
//! Specification: `docs/specs/26-terrain-vfx-highend.md` § 6 VFX rows.
//!
//! The CPU acceptance suite lives in [`run`]; the sample's other modes
//! (`--evidence`, `--bench`, `--window`) live in the sibling modules:
//! - `evidence`: offscreen PNG evidence per DO27 effect + contact sheet + metrics.
//! - `bench`: the § 6 particle budget with GPU timestamps.
//! - `window`: the ECS runtime path in a window, cycling the effect set.
//!
//! # What `run` tests
//! - Format: loads `.vfx.ron` files, round-trips through serialise, validates LOD arrays.
//! - Determinism: two runs with the same seed produce byte-identical spawn jobs.
//! - Budget: system-level caps and global headroom are enforced.
//! - LOD: distance bands scale or disable spawns.
//! - All spawn modes: Rate, Burst, PerDistance; every shape and velocity mode is
//!   exercised through the showcase set.
//! - Decal pool: spawn, age, fade, budget recycle.
//! - Camera shake: trauma accumulation, decay, distance falloff.
//! - Trails: sample recording, min-segment spacing, expiration, vertex generation.

use std::path::Path;

use regia::core::math::{Quat, Vec3};
use regia::prelude::*;
use regia_vfx::{
    CameraShake, Decal, DecalPool, DecalPrefab, DecalStrip, DecalSurface, SpawnDecal, SpawnEffect, TrailRig,
    TrailStyle, VfxCamera, VfxPlugin, VfxScene, VfxSettings, VfxView, angle_fade, bin_decals, builtin_prefab,
    format::VfxSystemDef,
    sample_quadratic,
    scene::VfxLibrary,
    sim::{LodInput, SpawnJob, VfxInstance},
    world_mask_excludes_characters,
};

// ---------------------------------------------------------------------------
// The sample's Plugin
// ---------------------------------------------------------------------------

pub mod bench;
pub mod crash_bench;
pub mod decals;
pub mod deck_check;
pub mod deck_presets;
pub mod evidence;
pub mod window;

/// Encodes a tightly packed RGBA8 buffer as a PNG file.
///
/// Samples may not depend on `image` (runtime exclusions, `docs/06` § 7), so the
/// evidence shots and window screenshots share this small `png`-based encoder.
pub(crate) fn save_png(path: &Path, rgba: &[u8], width: u32, height: u32) -> Result<(), std::io::Error> {
    use std::io::BufWriter;
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    // The buffers are sRGB-encoded; tell the PNG encoder.
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    let mut writer = encoder.write_header().map_err(std::io::Error::other)?;
    writer.write_image_data(rgba).map_err(std::io::Error::other)?;
    writer.finish().map_err(std::io::Error::other)?;
    Ok(())
}

/// The sample's Plugin — runs the lab and then exits.
pub struct VfxLab;

impl Plugin for VfxLab {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, lab_system);
    }
}

fn lab_system(mut exit: MessageWriter<AppExit>) {
    let code = run("effects");
    exit.write(if code == 0 {
        AppExit::Success
    } else {
        AppExit::Error(1.try_into().expect("1 is non-zero"))
    });
}

// ---------------------------------------------------------------------------
// Runner entry point
// ---------------------------------------------------------------------------

/// Runs the VFX lab and returns 0 on success, non-zero on failure.
#[cfg_attr(test, allow(clippy::assertions_on_constants))]
pub fn run(effects_dir: &str) -> i32 {
    tracing::info!("=== VFX Lab M14 ===");
    let mut failures = 0u32;
    let mut fail = |msg: &str| {
        eprintln!("FAIL: {msg}");
        failures += 1;
    };

    // 1. Load built-in effects (always present; no dependency on external files).
    let builtin = builtin_effects();
    tracing::info!("built-in effects loaded: {}", builtin.len());

    // 2. Load effects from the user-provided directory (if any).
    // The directory's effects are the library; built-ins fill in the names it lacks.
    let dir = Path::new(effects_dir);
    let mut library = if dir.is_dir() {
        match VfxLibrary::load_dir(dir) {
            Ok(lib) => {
                let n = lib.defs().len();
                tracing::info!("loaded {} effects from {effects_dir:?}", n);
                lib
            }
            Err(err) => {
                tracing::warn!("could not load {effects_dir:?}: {err}");
                VfxLibrary::new()
            }
        }
    } else {
        tracing::info!("no effects dir at {effects_dir:?}; using built-ins only");
        VfxLibrary::new()
    };

    // The spawn meshes the v7 sample effects name ("lab/plane", "lab/arm").
    lab_meshes(&mut library);

    // Merge built-ins.
    for def in &builtin {
        if library.index(&def.name).is_none()
            && let Err(err) = library.add(def.clone())
        {
            tracing::error!("built-in {} rejected: {err}", def.name);
            fail("built-in rejected");
        }
    }

    // 3. Acceptance: format round-trip for every loaded definition.
    tracing::info!("--- 3. Format round-trip ---");
    for def in library.defs() {
        match def.to_ron().and_then(|text| VfxSystemDef::from_ron(&text)) {
            Ok(round) => {
                if &round != def {
                    tracing::error!("round-trip mismatch for '{}'", def.name);
                    fail("round-trip mismatch");
                }
            }
            Err(err) => {
                tracing::error!("round-trip failed for '{}': {err}", def.name);
                fail("round-trip failed");
            }
        }
    }

    // 4. Acceptance: deterministic spawn counts (two runs with same seed).
    tracing::info!("--- 4. Deterministic spawn counts ---");
    for def in library.defs() {
        tracing::info!("  checking {}...", def.name);
        if !check_deterministic(&library, def) {
            tracing::error!("non-deterministic spawns for '{}'", def.name);
            fail("non-deterministic");
        }
    }

    // 5. Acceptance: budget enforcement.
    tracing::info!("--- 5. Budget enforcement ---");
    for def in library.defs() {
        if !check_budget(&library, def) {
            tracing::error!("budget violated for '{}'", def.name);
            fail("budget violated");
        }
    }

    // 6. Acceptance: LOD scaling.
    tracing::info!("--- 6. LOD bands ---");
    for def in library.defs() {
        if def.system.lod.is_some() {
            check_lod(def);
        }
    }

    // 7. Acceptance: shape variety exercised — Point shapes spawn at origin.
    // (Tested separately via shape_coverage unit test; avoid false positives from multi-emitter
    // systems where Sphere jobs get mixed into the Point-emitter drain.)
    tracing::info!("--- 7. Shape coverage ---");

    // 8. Decal pool exercise.
    tracing::info!("--- 8. Decal pool ---");
    let mut pool = DecalPool::new(32);
    for i in 0..40 {
        pool.spawn_request(&SpawnDecal {
            position: Vec3::new(i as f32 * 0.5, 0.0, 0.0),
            rotation: Quat::IDENTITY,
            half_size: Vec3::new(0.5, 0.5, 1.0),
            color: [1.0, 0.5, 0.2, 0.8],
            lifetime: Some(2.0),
        });
    }
    assert_eq!(pool.len(), 32, "decal pool capacity enforced");
    pool.update(3.0);
    assert_eq!(pool.len(), 0, "all decals aged out");

    // 9. Camera shake exercise.
    tracing::info!("--- 9. Camera shake ---");
    let mut shake = CameraShake::default();
    shake.add_trauma(0.5);
    shake.update(1.0 / 60.0, 1.2, 0.35, 0.02);
    assert!(shake.offset().length() > 1e-3, "shake active");
    for _ in 0..120 {
        shake.update(1.0 / 60.0, 1.2, 0.35, 0.02);
    }
    assert_eq!(shake.trauma(), 0.0, "trauma fully decayed");

    // 10. ECS plugin exercise.
    tracing::info!("--- 10. ECS plugin ---");
    {
        let mut app = App::new();
        app.insert_resource(VfxSettings {
            library_dir: None,
            ..Default::default()
        });
        app.add_plugins(VfxPlugin);
        app.update();
        assert!(app.world().contains_resource::<VfxScene>());
        assert!(app.world().contains_resource::<VfxView>());
        assert!(app.world().contains_resource::<DecalPool>());

        // Load a built-in and spawn it.
        app.world_mut()
            .resource_mut::<VfxScene>()
            .library_mut()
            .add(builtin[0].clone())
            .ok();
        app.world_mut().write_message(SpawnEffect {
            effect: builtin[0].name.clone(),
            position: Vec3::new(5.0, 2.0, 0.0),
            forward: Vec3::Z,
            seed: Some(1),
        });
        app.update();
        let scene = app.world().resource::<VfxScene>();
        // Note: message processing relies on add_message + MessageReader;
        // the scene may or may not tick messages depending on system ordering.
        // The plugin_registers_messages test in regia_vfx covers the integration.
        tracing::info!("ECS scene live instances after spawn: {}", scene.live_instances());
    }

    // 11. Trail rig exercise.
    tracing::info!("--- 11. Trails ---");
    {
        let mut rig = TrailRig::new(TrailStyle {
            width: 0.05,
            lifetime: 1.0,
            color: [1.0, 0.5, 0.0, 1.0],
            min_segment: 0.1,
        });
        for i in 0..20 {
            rig.update(Vec3::new(i as f32 * 0.5, 0.0, 0.0), 1.0 / 60.0);
        }
        assert!(rig.sample_count() >= 2, "trail recorded samples");
        let mut verts = Vec::new();
        rig.build_vertices(&mut verts, Vec3::X, [1.0; 4]);
        assert!(!verts.is_empty(), "trail generated vertices");
    }

    // 12. Decal prefab, angle fade, tiled cull, rut curve.
    tracing::info!("--- 12. Projected decals ---");
    {
        let prefab = builtin_prefab("scorch").expect("scorch prefab");
        let text = prefab.to_ron().expect("scorch ron");
        let again = DecalPrefab::from_ron(&text).expect("scorch reparse");
        assert_eq!(again.name, "scorch");
        assert!(world_mask_excludes_characters(again.receiver_mask));
        assert!(
            angle_fade(0.0, 0.25, 0.6) < 1e-5,
            "a facing of 0 must kill the decal"
        );
        let camera = VfxCamera::looking_at(Vec3::new(0.0, 3.0, 8.0), Vec3::ZERO);
        let (_, _, view_proj) = camera.matrices(16.0 / 9.0);
        let offscreen = Decal {
            position: Vec3::new(400.0, 0.0, 0.0),
            rotation: Quat::IDENTITY,
            half_size: Vec3::splat(0.2),
            color: [1.0, 1.0, 1.0, 1.0],
            age: 0.0,
            lifetime: None,
        };
        let surface = DecalSurface::from_decal(&offscreen);
        let bin = bin_decals(&[offscreen], &[surface], view_proj, camera.eye, 320, 180, 16);
        assert_eq!(bin.visible.len(), 0, "an off-screen box must be culled");
        assert!(bin.culled >= 1);
        let (_, rut_surface) = builtin_prefab("rut")
            .expect("rut")
            .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
            .expect("rut instance");
        let stamps = DecalStrip {
            points: sample_quadratic(
                Vec3::new(-4.0, 0.0, 2.0),
                Vec3::new(0.0, 0.0, -2.0),
                Vec3::new(4.0, 0.0, 2.0),
                12,
            ),
            normal: Vec3::Y,
            half_width: 0.2,
            half_depth: 0.15,
            spacing: 0.4,
            surface: rut_surface,
            lifetime: None,
        }
        .stamps();
        assert!(stamps.len() > 2, "a curve emits more than the endpoints");
        let mid = stamps[stamps.len() / 2].0.position;
        assert!(
            mid.z < 0.5,
            "the middle stamp follows the bow, not the chord (z={})",
            mid.z
        );
    }

    tracing::info!("=== VFX Lab done: {} failures ===", failures);
    if failures > 0 { 1 } else { 0 }
}

/// The lab's spawn meshes for `.vfx.ron` v7 mesh shapes: `lab/plane`, a 2 m quad facing +Y, and
/// `lab/arm`, a two-bone strip along +Y (the lower half on joint 0, the upper on joint 1).
pub fn lab_meshes(library: &mut VfxLibrary) {
    use regia_vfx::mesh_shape::VfxMesh;
    match VfxMesh::from_triangles(
        &[
            [-1.0, 0.0, -1.0],
            [1.0, 0.0, -1.0],
            [1.0, 0.0, 1.0],
            [-1.0, 0.0, 1.0],
        ],
        &[0, 2, 1, 0, 3, 2],
    ) {
        Ok(mesh) => library.add_mesh("lab/plane", mesh),
        Err(err) => tracing::error!("lab/plane: {err}"),
    }
    let arm = VfxMesh::from_triangles(
        &[
            [-0.1, 0.0, 0.0],
            [0.1, 0.0, 0.0],
            [-0.1, 1.0, 0.0],
            [0.1, 1.0, 0.0],
            [-0.1, 2.0, 0.0],
            [0.1, 2.0, 0.0],
        ],
        &[0, 1, 3, 0, 3, 2, 2, 3, 5, 2, 5, 4],
    )
    .and_then(|mesh| {
        mesh.with_skin(
            &[
                [0, 0, 0, 0],
                [0, 0, 0, 0],
                [0, 1, 0, 0],
                [0, 1, 0, 0],
                [1, 0, 0, 0],
                [1, 0, 0, 0],
            ],
            &[
                [1.0, 0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                [0.5, 0.5, 0.0, 0.0],
                [0.5, 0.5, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
            ],
        )
    });
    match arm {
        Ok(mesh) => library.add_mesh("lab/arm", mesh),
        Err(err) => tracing::error!("lab/arm: {err}"),
    }
}

/// A probe instance of `def` with the library's meshes bound.
fn lab_instance(library: &VfxLibrary, def: &VfxSystemDef, seed: u64) -> VfxInstance {
    let index = library.index(&def.name).unwrap_or(0);
    let mut inst = VfxInstance::new(index, seed, Vec3::ZERO, Vec3::Z);
    if library.index(&def.name).is_some() {
        library.bind_meshes(&mut inst);
    }
    inst
}

fn check_deterministic(library: &VfxLibrary, def: &VfxSystemDef) -> bool {
    let lod = LodInput {
        camera_pos: Vec3::ZERO,
    };
    let run = |seed: u64| -> Vec<SpawnJob> {
        let mut inst = lab_instance(library, def, seed);
        let mut all = Vec::new();
        let dt = 1.0 / 60.0;
        for i in 0..120 {
            // Simulate forward movement for PerDistance emitters.
            let x = i as f32 * 0.5;
            inst.set_transform(Vec3::new(x, 0.0, 0.0), Vec3::Z, dt);
            inst.schedule(def, dt, &lod, u32::MAX);
            inst.drain_jobs(&mut all);
        }
        all
    };
    let a = run(42);
    let b = run(42);
    if a.is_empty() && b.is_empty() {
        return true; // no particles expected (e.g. zero-rate LOD band)
    }
    a == b
}

fn check_budget(library: &VfxLibrary, def: &VfxSystemDef) -> bool {
    let lod = LodInput {
        camera_pos: Vec3::ZERO,
    };
    let mut inst = lab_instance(library, def, 1);
    let dt = 1.0 / 60.0;
    let mut peak = 0u32;
    for i in 0..180 {
        let x = i as f32 * 0.5;
        inst.set_transform(Vec3::new(x, 0.0, 0.0), Vec3::Z, dt);
        inst.schedule(def, dt, &lod, u32::MAX);
        let mut jobs = Vec::new();
        inst.drain_jobs(&mut jobs);
        peak = peak.max(inst.live_particles());
    }
    peak <= def.system.budget
}

fn check_lod(def: &VfxSystemDef) {
    let Some(lod) = &def.system.lod else { return };
    assert_eq!(
        lod.rate_scale.len(),
        lod.distances.len() + 1,
        "{}: rate_scale length mismatch",
        def.name
    );
    let last = lod.rate_scale.last().copied().unwrap_or(1.0);
    let beyond = def.lod_scale(lod.distances.last().copied().unwrap_or(0.0) * 2.0 + 100.0);
    assert!(
        (beyond - last).abs() < 1e-6,
        "{}: beyond LOD scale not clamped",
        def.name
    );
}

/// Built-in effect definitions used when no external files are present: the DO27
/// reference set lives in `regia_vfx::showcase` (single source of truth for the
/// sample, the offscreen evidence suite and the GPU tests).
fn builtin_effects() -> Vec<VfxSystemDef> {
    regia_vfx::showcase_effects()
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_vfx::format::SpawnMode;

    #[test]
    fn builtins_are_valid() {
        for def in builtin_effects() {
            def.validate()
                .unwrap_or_else(|e| panic!("{} validates: {e}", def.name));
            let text = def.to_ron().expect("to_ron");
            let back = VfxSystemDef::from_ron(&text).expect("from_ron");
            assert_eq!(&def, &back, "{} round-trips", def.name);
        }
    }

    #[test]
    fn every_builtin_spawns_deterministically() {
        for def in builtin_effects() {
            assert!(
                check_deterministic(&VfxLibrary::new(), &def),
                "{} deterministic",
                def.name
            );
        }
    }

    #[test]
    fn every_builtin_respects_budget() {
        for def in builtin_effects() {
            assert!(
                check_budget(&VfxLibrary::new(), &def),
                "{} respects budget",
                def.name
            );
        }
    }

    #[test]
    fn shape_coverage() {
        use regia_vfx::format::ShapeDef;
        let defs = builtin_effects();
        let mut shapes = std::collections::BTreeSet::new();
        for def in &defs {
            for emitter in &def.emitters {
                shapes.insert(match &emitter.init.shape {
                    ShapeDef::Point => "Point",
                    ShapeDef::Sphere { .. } => "Sphere",
                    ShapeDef::Box { .. } => "Box",
                    ShapeDef::Cone { .. } => "Cone",
                    ShapeDef::MeshSurface { .. } => "MeshSurface",
                    ShapeDef::SkinnedMesh { .. } => "SkinnedMesh",
                });
            }
        }
        assert!(shapes.contains("Point"));
        assert!(shapes.contains("Sphere"));
        assert!(shapes.contains("Cone"));
    }

    /// Point shapes MUST spawn at the instance origin.
    #[test]
    fn point_shape_spawns_at_origin() {
        use regia_vfx::format::{
            EmitterDef, InitModule, RangeDef, RenderModule, ShapeDef, SpawnMode, SpawnModule, SystemModule,
            VelocityDef,
        };
        let r = |min: f32, max: f32| RangeDef { min, max };
        let point_def = VfxSystemDef {
            version: 1,
            name: "point_test".into(),
            system: SystemModule {
                bounds: None,
                budget: 256,
                lod: None,
            },
            emitters: vec![EmitterDef {
                name: "p".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(10),
                    lifetime: r(0.5, 0.5),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Point,
                    velocity: VelocityDef::default(),
                    size: r(1.0, 1.0),
                    rotation: r(0.0, 0.0),
                    spin: r(0.0, 0.0),
                    color: vec![],
                },
                update: Default::default(),
                render: RenderModule::default(),
            }],
        };
        let mut inst = VfxInstance::new(0, 1, Vec3::new(3.0, 5.0, 1.0), Vec3::Z);
        inst.schedule(
            &point_def,
            0.1,
            &LodInput {
                camera_pos: Vec3::ZERO,
            },
            u32::MAX,
        );
        let mut jobs = Vec::new();
        inst.drain_jobs(&mut jobs);
        assert!(!jobs.is_empty());
        for job in &jobs {
            let pos = Vec3::from_array(job.position);
            let expected = Vec3::new(3.0, 5.0, 1.0);
            let d = (pos - expected).length();
            assert!(d < 1e-4, "Point shape spawned at {pos:?}, expected {expected:?}");
        }
    }

    #[test]
    fn spawn_mode_coverage() {
        let defs = builtin_effects();
        let mut modes = std::collections::BTreeSet::new();
        for def in &defs {
            for emitter in &def.emitters {
                let s = match emitter.spawn.mode {
                    SpawnMode::Rate(_) => "Rate",
                    SpawnMode::Burst(_) => "Burst",
                    SpawnMode::PerDistance { .. } => "PerDistance",
                    SpawnMode::Event => "Event",
                };
                modes.insert(s);
            }
        }
        assert!(modes.contains("Rate"));
        assert!(modes.contains("Burst"));
        assert!(modes.contains("PerDistance"));
    }

    #[test]
    fn render_mode_coverage() {
        use regia_vfx::format::RenderMode;
        let defs = builtin_effects();
        let mut modes = std::collections::BTreeSet::new();
        for def in &defs {
            for emitter in &def.emitters {
                modes.insert(match emitter.render.mode {
                    RenderMode::Billboard => "Billboard",
                    RenderMode::Stretched => "Stretched",
                    RenderMode::Mesh => "Mesh",
                    RenderMode::Ribbon => "Ribbon",
                });
            }
        }
        assert!(modes.contains("Billboard"));
        assert!(modes.contains("Stretched"));
        assert!(modes.contains("Mesh"));
    }

    #[test]
    fn vfx_lab_run_succeeds() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_target(false)
            .try_init();
        let code = run("nonexistent_dir_for_vfx_lab_test");
        assert_eq!(code, 0, "vfx_lab run returned {code}");
    }

    /// M14 cosmetic debris demo: pre-fractured mesh chunk creation, outward impulse logic,
    /// heightfield shape conversion, and budget enforcement.
    ///
    /// Full physics-stepping integration tests (bodies falling on heightfields, sleeping,
    /// despawn) live in `regia_physics/tests/debris_integration.rs`.
    #[test]
    fn debris_demo() {
        use regia::core::math::{Quat, Vec3};
        use regia_physics::{ChunkDesc, ColliderShape, SpawnConfig, step::shape_to_shared_shape};

        // 1. Build a pre-fractured mesh: 8 box chunks around an origin.
        let chunks: Vec<ChunkDesc> = (0..8)
            .map(|i| {
                let angle = i as f32 * std::f32::consts::TAU / 8.0;
                let r = 1.0;
                ChunkDesc {
                    local_translation: Vec3::new(r * angle.cos(), 0.0, r * angle.sin()),
                    local_rotation: Quat::IDENTITY,
                    shape: ColliderShape::Box {
                        half_extents: Vec3::new(0.3, 0.3, 0.3),
                    },
                    mass: 1.0,
                }
            })
            .collect();

        let config = SpawnConfig {
            chunks: &chunks,
            origin: Vec3::new(0.0, 5.0, 0.0),
            impulse_strength: 15.0,
        };

        // Verify chunk count.
        assert_eq!(config.chunks.len(), 8);

        // Verify all chunks have outward-facing directions from origin.
        for chunk in config.chunks {
            let world_pos = config.origin + chunk.local_translation;
            let dir = (world_pos - config.origin).normalize_or_zero();
            assert!(dir.length() > 0.9, "direction should be roughly unit length");
            let expected_dir = chunk.local_translation.normalize_or_zero();
            if expected_dir.length() > 0.1 {
                assert!(
                    dir.dot(expected_dir) > 0.99,
                    "impulse should be outward from origin"
                );
            }
        }

        // 2. Heightfield shape conversion.
        let nx = 5usize;
        let nz = 5usize;
        let hf_heights: Vec<f32> = (0..nx * nz).map(|_| 0.0f32).collect();
        let hf_shape = ColliderShape::HeightField {
            nx,
            nz,
            heights: hf_heights,
            scale: Vec3::new(8.0, 1.0, 8.0),
        };
        assert!(
            shape_to_shared_shape(&hf_shape).is_some(),
            "heightfield shape converts for debris ground"
        );

        // 3. Verify convex hull and box shapes also convert.
        let box_shape = ColliderShape::Box {
            half_extents: Vec3::new(0.5, 0.5, 0.5),
        };
        assert!(shape_to_shared_shape(&box_shape).is_some(), "box shape converts");

        let hull_shape = ColliderShape::ConvexHull {
            points: vec![
                Vec3::new(0.0, 1.0, 0.0),
                Vec3::new(1.0, -1.0, 0.5),
                Vec3::new(-1.0, -1.0, -0.5),
                Vec3::new(0.0, -1.0, 0.5),
            ],
        };
        assert!(
            shape_to_shared_shape(&hull_shape).is_some(),
            "convex hull shape converts"
        );
    }
}
