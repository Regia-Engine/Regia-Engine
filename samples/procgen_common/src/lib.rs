// SPDX-License-Identifier: MIT-0
//! Shared host of the procgen samples (`procgen_roads`, `procgen_biomes`, `procgen_open_world`): the
//! engine's renderer, agent server and generators, a procedural terrain with a mesh drawn over it
//! (the samples look at generator output, not at the terrain renderer), simple placeholder meshes,
//! one view, and a runner that drives the app through the agent protocol.
//!
//! Generators run on the main thread (`ProcGenSettings::inline`) with a fixed step, so a script sees
//! the same frames on every run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::prelude::*;
use regia_core::math::{Mat3, Quat, Vec3};
use regia_procgen::draw::{ProcMaterialLibrary, ProcMeshLibrary};
use regia_procgen::world::{ProcGenSettings, ProcGraphSource};
use regia_render::primitives::{box_mesh, uv_sphere};
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, Environment, LabVertex,
    MeshRenderer, PostSettings, RenderDevice, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use regia_terrain::edit::TerrainEdits;
use regia_terrain::{CdlodConfig, Terrain, TerrainLayers, TerrainMap, ridged_fbm};
use serde_json::{Value, json};

/// Longest a script may run: a hang guard only, nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(600);

/// The `timeout_ms` a sample's `seeing.screenshot` passes (ADR-7640): a hang guard only. The first
/// offscreen capture builds its pipelines on the CPU adapter, and on a loaded lavapipe host that
/// passed the server's 30 s default (the v0.5.0 rc4 gate: F0006 after 30 s; reproduced with the
/// test pinned to two cores). Measured on the build server's lavapipe (ADR-7640): 21.5 and 31.0 s
/// unpinned at load 110-119, 48.9 s pinned to two cores, 262 s pinned to one. Kept under
/// [`SCRIPT_TIMEOUT`], which bounds the whole script.
const SHOT_TIMEOUT_MS: u64 = 480_000;

/// A solid opaque material.
pub fn solid(rgb: [f32; 3], roughness: f32) -> Arc<CpuMaterial> {
    Arc::new(CpuMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        roughness,
        metallic: 0.0,
        ..CpuMaterial::default()
    })
}

/// A box centered on the origin.
pub fn cuboid(half: [f32; 3]) -> Arc<CpuMesh> {
    let (vertices, indices) = box_mesh(half);
    Arc::new(CpuMesh { vertices, indices })
}

/// A box standing on the origin.
pub fn standing_box(half: [f32; 3]) -> Arc<CpuMesh> {
    let (vertices, indices) = box_mesh(half);
    let vertices = regia_render::primitives::translate_mesh(&vertices, [0.0, half[1], 0.0]);
    Arc::new(CpuMesh { vertices, indices })
}

/// A lump: a sphere squashed to `height` over a `radius` footprint, standing on the origin.
pub fn lump(radius: f32, height: f32) -> Arc<CpuMesh> {
    let (mut vertices, indices) = uv_sphere(1.0, 10, 6);
    for v in &mut vertices {
        v.position = [
            v.position[0] * radius,
            (v.position[1] + 1.0) * height * 0.5,
            v.position[2] * radius,
        ];
    }
    Arc::new(CpuMesh { vertices, indices })
}

/// A tree: a trunk with a cone-like stack of squashed spheres, about `height` tall.
pub fn tree(height: f32, radius: f32) -> Arc<CpuMesh> {
    let mut vertices: Vec<LabVertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut add = |vs: Vec<LabVertex>, is: Vec<u32>, tint: [f32; 4]| {
        let base = vertices.len() as u32;
        vertices.extend(vs.into_iter().map(|mut v| {
            v.color = tint;
            v
        }));
        indices.extend(is.iter().map(|i| i + base));
    };
    let (trunk, ti) = box_mesh([radius * 0.12, height * 0.2, radius * 0.12]);
    add(
        regia_render::primitives::translate_mesh(&trunk, [0.0, height * 0.2, 0.0]),
        ti,
        [0.3, 0.2, 0.12, 1.0],
    );
    for k in 0..3 {
        let f = k as f32 / 3.0;
        let (mut crown, ci) = uv_sphere(1.0, 10, 6);
        let r = radius * (1.0 - f * 0.65);
        let y = height * (0.42 + 0.22 * k as f32);
        for v in &mut crown {
            v.position = [
                v.position[0] * r,
                v.position[1] * height * 0.2 + y,
                v.position[2] * r,
            ];
        }
        add(crown, ci, [0.16, 0.34, 0.14, 1.0]);
    }
    Arc::new(CpuMesh { vertices, indices })
}

/// Rolling hills as a square heightfield: `n × n` samples over `world_m`, `amplitude_m` high.
pub fn hills(n: u32, world_m: f32, amplitude_m: f32, seed: u32) -> TerrainMap {
    let heights: Vec<f32> = (0..n * n)
        .map(|i| {
            let (c, r) = ((i % n) as f32, (i / n) as f32);
            let s = world_m / (n - 1) as f32 / 180.0;
            ridged_fbm(c * s, r * s, seed, 4) * amplitude_m
        })
        .collect();
    TerrainMap::from_heights(n, n, heights, world_m, amplitude_m * 2.0, -amplitude_m * 0.5)
        .unwrap_or_else(|e| panic!("the sample heightfield is valid: {e}"))
}

/// A `Terrain` component over `map` (the layer stack is the sample's).
pub fn terrain_of(map: &TerrainMap, layers: TerrainLayers) -> Terrain {
    let levels = (map.width().max(2) - 1).ilog2().saturating_sub(3).max(1);
    Terrain::new(
        &map.to_heightfield(),
        map.world_width(),
        map.height_scale(),
        map.height_offset(),
        CdlodConfig::default(),
        levels,
        layers,
    )
    .unwrap_or_else(|e| panic!("the sample terrain is valid: {e}"))
}

/// The heightfield as a triangle mesh, every `step` samples, with its true normals.
pub fn ground_mesh(map: &TerrainMap, step: u32) -> CpuMesh {
    let (w, h) = (map.width(), map.height());
    let cols = (w - 1) / step + 1;
    let rows = (h - 1) / step + 1;
    let mut vertices = Vec::with_capacity((cols * rows) as usize);
    let sample = |c: u32, r: u32| {
        let [x, z] = map.sample_to_world(c, r);
        Vec3::new(x, map.height_at_sample(i64::from(c), i64::from(r)), z)
    };
    for r in 0..rows {
        for c in 0..cols {
            let (sc, sr) = (c * step, r * step);
            let p = sample(sc, sr);
            let dx = sample((sc + step).min(w - 1), sr) - sample(sc.saturating_sub(step), sr);
            let dz = sample(sc, (sr + step).min(h - 1)) - sample(sc, sr.saturating_sub(step));
            let n = dz.cross(dx).normalize_or(Vec3::Y);
            vertices.push(LabVertex::from_pos_normal_uv(
                p.to_array(),
                n.to_array(),
                [p.x * 0.05, p.z * 0.05],
            ));
        }
    }
    let mut indices = Vec::with_capacity(((cols - 1) * (rows - 1) * 6) as usize);
    for r in 0..rows - 1 {
        for c in 0..cols - 1 {
            let i = |c: u32, r: u32| r * cols + c;
            indices.extend([
                i(c, r),
                i(c, r + 1),
                i(c + 1, r),
                i(c + 1, r),
                i(c, r + 1),
                i(c + 1, r + 1),
            ]);
        }
    }
    CpuMesh { vertices, indices }
}

/// Marks the entity that draws the terrain, and the edit generation it was built from.
#[derive(Component)]
pub struct GroundView {
    /// Heightfield samples between mesh vertices.
    pub step: u32,
    generation: u64,
}

/// Spawns the ground mesh for the world's `Terrain` with `material`.
pub fn spawn_ground(world: &mut World, step: u32, material: Arc<CpuMaterial>) {
    let mesh = {
        let mut q = world.query::<&Terrain>();
        let Some(t) = q.iter(world).next() else {
            return;
        };
        Arc::new(ground_mesh(&t.map, step))
    };
    let tf = Transform::default();
    world.spawn((
        tf,
        GlobalTransform(tf.compute_affine()),
        MeshRenderer::new(mesh, material),
        GroundView { step, generation: 0 },
    ));
}

/// `Update`: rebuilds the ground mesh after a terrain edit (road flattening).
pub fn sync_ground(
    terrain: Query<(&Terrain, Option<&TerrainEdits>)>,
    mut ground: Query<(&mut GroundView, &mut MeshRenderer)>,
) {
    let Some((t, edits)) = terrain.iter().next() else {
        return;
    };
    let generation = edits.map_or(0, TerrainEdits::generation);
    for (mut view, mut renderer) in &mut ground {
        if view.generation != generation {
            view.generation = generation;
            renderer.mesh = Arc::new(ground_mesh(&t.map, view.step));
        }
    }
}

/// The rotation that points a camera (-Z forward) from `eye` at `target`, Y up.
pub fn look_rotation(eye: Vec3, target: Vec3) -> Quat {
    let forward = (target - eye).normalize_or(Vec3::NEG_Z);
    let right = forward.cross(Vec3::Y).normalize_or(Vec3::X);
    Quat::from_mat3(&Mat3::from_cols(right, right.cross(forward), -forward))
}

/// Spawns the view and the sun. Returns the camera entity.
pub fn spawn_view(world: &mut World, eye: Vec3, target: Vec3, far: f32) -> Entity {
    let camera = Transform {
        rotation: look_rotation(eye, target),
        ..Transform::from_translation(eye)
    };
    let e = world
        .spawn((
            camera,
            GlobalTransform(camera.compute_affine()),
            Camera {
                exposure_ev: Some(14.0),
                fov_y: 0.9,
                far,
                clear: ClearMode::Color([0.45, 0.6, 0.8, 1.0]),
                ..Camera::default()
            },
        ))
        .id();
    world.spawn((
        Transform::default(),
        DirectionalLight {
            direction: [0.4, -0.8, 0.35],
            color: [1.0, 0.96, 0.9],
            illuminance: 60_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));
    world.insert_resource(Environment {
        ibl: false,
        ambient_lux: 18_000.0,
        double_sided: true,
    });
    e
}

/// Render settings for reviewable, repeatable shots.
pub fn review_settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.auto_exposure = false;
    settings.profile.bloom = false;
    settings
}

/// Generators read `assets` and evaluate on the main thread, in the editor's rules (agents re-run them).
pub fn procgen_settings(assets: PathBuf, bake_dir: Option<PathBuf>, terrain_edits: bool) -> ProcGenSettings {
    ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Dir(assets),
        bake_dir,
        terrain_edits,
        ..ProcGenSettings::default()
    }
}

/// A sample's meshes and materials by the asset paths its graphs name.
pub fn install_library(
    world: &mut World,
    meshes: Vec<(&str, Arc<CpuMesh>, Arc<CpuMaterial>)>,
    materials: Vec<(&str, Arc<CpuMaterial>)>,
) {
    let mut lib = ProcMeshLibrary(BTreeMap::new());
    for (path, mesh, material) in meshes {
        lib.insert(path, mesh, material);
    }
    let mut mats = ProcMaterialLibrary::default();
    for (path, material) in materials {
        mats.insert(path, material);
    }
    world.insert_resource(lib);
    world.insert_resource(mats);
}

/// The headless app every sample shares: renderer without a window, agent server advertised in
/// `project`, a paused fixed-step clock, generators from `assets`. `None` when the machine has no
/// Vulkan device at all (not even lavapipe).
pub fn headless_app(project: &Path, settings: ProcGenSettings) -> Option<App> {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(e) => {
            eprintln!("procgen sample: no render device: {e}");
            return None;
        }
    };
    let mut app = App::new();
    app.insert_resource(settings);
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_procgen::ProcGenPlugin,
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.to_path_buf()),
    })
    .insert_resource(device)
    .insert_resource(review_settings())
    .add_systems(Update, sync_ground);
    regia_app::enable_fixed_step(&mut app, 60.0);
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    Some(app)
}

/// Runs a sample in a window with the engine's default plugins (the agent server included), for
/// `play.hitch_report`, a device, or a look around. Generators evaluate on the worker pool here.
/// `exit_after` ends the app after that many frames. `setup` fills the app and queues its runs.
pub fn run_windowed(
    mut settings: ProcGenSettings,
    project: &Path,
    exit_after: Option<u64>,
    setup: impl FnOnce(&mut App) + Send + 'static,
) -> AppExit {
    settings.inline = false;
    let project = project.to_path_buf();
    regia::run_with(ExitAfter(exit_after), move |app| {
        app.insert_resource(settings)
            .insert_resource(regia_agent::AgentSettings {
                project_dir: Some(project),
            })
            .insert_resource(review_settings())
            .add_systems(Update, sync_ground);
        setup(app);
    })
}

/// Ends the app after this many frames (`None`: runs until closed).
struct ExitAfter(Option<u64>);

impl Plugin for ExitAfter {
    fn build(&self, app: &mut App) {
        if let Some(frames) = self.0 {
            app.add_systems(
                Update,
                move |mut seen: Local<u64>, mut exit: MessageWriter<AppExit>| {
                    *seen += 1;
                    if *seen >= frames {
                        exit.write(AppExit::Success);
                    }
                },
            );
        }
    }
}

/// `--flag value` of the command line.
pub fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// Steps `app` while `script` drives it through the agent protocol; returns the script's result.
///
/// # Panics
///
/// When the agent endpoint never appears, or the script panics.
pub fn drive<R: Send + 'static>(
    mut app: App,
    project: &Path,
    script: impl FnOnce(&mut Client) -> R + Send + 'static,
) -> R {
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let agent_dir = endpoint::agent_dir(project);
    let thread = std::thread::Builder::new()
        .name("procgen-sample-script".to_string())
        .spawn(move || {
            let deadline = Instant::now() + SCRIPT_TIMEOUT;
            let mut client = None;
            while Instant::now() < deadline && client.is_none() {
                if let Ok((advertised, _)) = endpoint::read(&agent_dir)
                    && let Ok(token) = std::fs::read_to_string(agent_dir.join("token"))
                {
                    client = Client::connect(&advertised.url, token.trim()).ok();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut client) = client else {
                panic!("no agent endpoint at {}", agent_dir.display());
            };
            let result = script(&mut client);
            flag.store(true, Ordering::Relaxed);
            result
        })
        .unwrap_or_else(|e| panic!("cannot start the script thread: {e}"));
    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !done.load(Ordering::Relaxed) && !thread.is_finished() && Instant::now() < deadline {
        app.update();
    }
    match thread.join() {
        Ok(r) => r,
        Err(e) => std::panic::resume_unwind(e),
    }
}

/// One agent call; a protocol error is a script failure.
///
/// # Panics
///
/// When the call answers an error.
pub fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(v) => v,
        Err(e) => panic!("{method} failed: {e:?}"),
    }
}

/// The entity bits of the generator named `name` (`procgen.list`).
///
/// # Panics
///
/// When no generator has that name.
pub fn generator(client: &mut Client, name: &str) -> u64 {
    let list = call(client, "procgen.list", json!({}));
    list["generators"]
        .as_array()
        .and_then(|g| g.iter().find(|x| x["name"] == name))
        .and_then(|x| x["entity"].as_u64())
        .unwrap_or_else(|| panic!("no generator `{name}` in {list}"))
}

/// Decodes standard base64 (what `seeing.screenshot` answers); invalid characters are skipped.
fn base64_decode(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut bits, mut acc) = (0u32, 0u32);
    for v in text.bytes().filter_map(value) {
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// Takes an offscreen screenshot, writes the PNG to `path` when given, returns its pixels' byte size.
///
/// # Panics
///
/// When the capture or the write fails.
pub fn shot(client: &mut Client, path: Option<&Path>, width: u32, height: u32) -> usize {
    let started = Instant::now();
    let answer = call(
        client,
        "seeing.screenshot",
        json!({
            "target": "offscreen",
            "width": width,
            "height": height,
            "hud": false,
            "timeout_ms": SHOT_TIMEOUT_MS,
        }),
    );
    // Printed for the record (nothing asserts on time): how long lavapipe took on this host.
    eprintln!(
        "seeing.screenshot {width}x{height} answered in {} ms",
        started.elapsed().as_millis()
    );
    let png = base64_decode(answer["png_base64"].as_str().unwrap_or(""));
    if let Some(path) = path {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
        }
        std::fs::write(path, &png).unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
    }
    png.len()
}
