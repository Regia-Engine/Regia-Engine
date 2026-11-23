// SPDX-License-Identifier: MPL-2.0
//! Render-out AOVs and scene-linear EXR frames (ADR-1521), on lavapipe through the agent protocol: a
//! quad slides in front of a still triangle; the depth, id and motion files of every frame are read
//! back and checked against the scene (depths of the two planes, the sky at `+inf`, ids that stay
//! the same on every frame, backward motion equal to the quad's measured shift and zero on frame 0,
//! on the cut frame and on everything that does not move). A `format: "exr"` job writes scene-linear
//! frames and EXR AOVs that read back with the `exr` crate. The quad is gameplay (it moves on the
//! simulation clock), so under a `time_map` its motion follows the map (ADR-1524, ADR-1554), and
//! the manifest (`regia.render_out/2`) lists each AOV's files in `files.aov`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use exr::prelude::{FlatSamples, read_first_flat_layer_from_file};
use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::enable_fixed_step;
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, Environment, LabVertex,
    MeshRenderer, PostSettings, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

/// Largest vertical motion, in pixels, the sideways-moving quad may show. The quad moves right
/// by several pixels per frame; what is asserted is that it does not move vertically. The motion
/// vector is a difference of two projected positions, so a GPU's fused multiply-add and driver
/// rounding leave noise near 1.5e-3 px (RTX 5080, Windows, ADR-1564; lavapipe stays under
/// 1e-3). 1e-2 px is still a hundredth of the smallest real shift asserted.
const VERTICAL_MOTION_EPS: f32 = 1e-2;

/// A hang guard only: nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(900);
const CALL_TIMEOUT_MS: u64 = 60_000;
const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
/// Meters per second the quad slides along +X.
const SPEED: f32 = 6.0;
/// Camera distance to the quad's plane (z = 0) and to the triangle's (z = -1).
const QUAD_DEPTH: f32 = 2.6;
const TRIANGLE_DEPTH: f32 = 3.6;

#[derive(Component)]
struct Slider;

/// Moves the quad on the simulation clock (`Time::sim_elapsed`, the same as `elapsed` without a
/// time map).
fn slide(time: Res<Time>, mut query: Query<(&mut Transform, &mut GlobalTransform), With<Slider>>) {
    let x = -0.6 + SPEED * time.sim_elapsed().as_secs_f32();
    for (mut transform, mut global) in &mut query {
        transform.translation.x = x;
        global.0 = transform.compute_affine();
    }
}

fn vertex(pos: [f32; 3]) -> LabVertex {
    LabVertex::from_pos_normal_uv(pos, [0.0, 0.0, 1.0], [0.0, 0.0])
}

fn quad() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            vertex([-0.3, -0.4, 0.0]),
            vertex([0.3, -0.4, 0.0]),
            vertex([0.3, 0.4, 0.0]),
            vertex([-0.3, 0.4, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
    })
}

fn triangle() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            vertex([-1.2, -0.9, -1.0]),
            vertex([1.2, -0.9, -1.0]),
            vertex([0.0, 0.9, -1.0]),
        ],
        indices: vec![0, 1, 2],
    })
}

fn settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.auto_exposure = false;
    settings.profile.bloom = false;
    settings.profile.depth_prepass = false;
    settings
}

fn call(client: &mut Client, method: &str, mut params: Value) -> Value {
    params["timeout_ms"] = json!(CALL_TIMEOUT_MS);
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

fn read_token(agent_dir: &Path) -> String {
    std::fs::read_to_string(agent_dir.join("token"))
        .expect("token file")
        .trim()
        .to_string()
}

/// Polls `render.out_progress` until the job is no longer running or cancelling.
fn wait(client: &mut Client, job: u64) -> Value {
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while Instant::now() < deadline {
        let progress = call(client, "render.out_progress", json!({ "job": job }));
        if progress["status"] != json!("running") && progress["status"] != json!("cancelling") {
            return progress;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("render-out job {job} did not finish");
}

fn render(client: &mut Client, params: Value) -> Value {
    let started = call(client, "render.out_start", params);
    wait(client, started["job"].as_u64().expect("job id"))
}

fn job_params(out: &Path, name: &str, frames: u64, extra: Value) -> Value {
    let mut params = json!({
        "out": out.join(name).display().to_string(), "name": name, "frames": frames,
        "width": WIDTH, "height": HEIGHT, "fps": 30, "hud": false,
    });
    params
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    params
}

fn manifest(out: &Path, name: &str) -> Value {
    serde_json::from_slice(
        &std::fs::read(out.join(name).join(format!("{name}_render_out.json"))).expect("manifest"),
    )
    .expect("json")
}

/// A fresh headless app with the sliding quad in front of the still triangle; runs `script`
/// against its agent and returns the project dir (removed by the caller).
fn run(tag: &str, script: impl FnOnce(&mut Client, &Path) + Send + 'static) -> PathBuf {
    run_with(tag, |_| {}, script)
}

/// [`run`] with `setup` applied to the app before it starts.
fn run_with(
    tag: &str,
    setup: impl FnOnce(&mut App),
    script: impl FnOnce(&mut Client, &Path) + Send + 'static,
) -> PathBuf {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-aov-{tag}"));
    let out = project.join("out");
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    });
    enable_fixed_step(&mut app, 60.0);
    // Paused until a job takes the clock, so nothing depends on the client's speed.
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    app.insert_resource(settings());
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 18_000.0,
        double_sided: true,
    });
    app.add_systems(Update, slide);
    let camera = Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, QUAD_DEPTH));
    app.world_mut().spawn((
        camera,
        GlobalTransform(camera.compute_affine()),
        Camera {
            exposure_ev: Some(14.0),
            fov_y: 0.9,
            clear: ClearMode::Color([0.05, 0.07, 0.12, 1.0]),
            ..Camera::default()
        },
    ));
    let material = Arc::new(CpuMaterial {
        base_color: [0.85, 0.42, 0.12, 1.0],
        roughness: 1.0,
        metallic: 0.0,
        ..CpuMaterial::default()
    });
    let still = Transform::default();
    app.world_mut().spawn((
        still,
        GlobalTransform(still.compute_affine()),
        MeshRenderer::new(triangle(), material.clone()),
        DirectionalLight {
            direction: [0.0, 0.0, -1.0],
            color: [1.0, 1.0, 1.0],
            illuminance: 60_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));
    app.world_mut().spawn((
        still,
        GlobalTransform(still.compute_affine()),
        MeshRenderer::new(quad(), material),
        Slider,
    ));

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let thread = std::thread::Builder::new()
        .name(format!("render-out-aov-{tag}"))
        .spawn(move || {
            let deadline = Instant::now() + SCRIPT_TIMEOUT;
            let mut client = None;
            while Instant::now() < deadline {
                if let Ok((advertised, _)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &read_token(&agent_dir))
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut client) = client else {
                panic!("no endpoint at {}", agent_dir.display());
            };
            script(&mut client, &out);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");
    setup(&mut app);
    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !thread.is_finished() && Instant::now() < deadline {
        app.update();
    }
    thread.join().expect("script finished");
    project
}

fn f32s(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

fn u32s(bytes: &[u8]) -> Vec<u32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect()
}

/// One frame's raw AOVs.
struct Frame {
    depth: Vec<f32>,
    /// `(instance, class)` per pixel.
    ids: Vec<(u32, u32)>,
    /// `(u, v)` per pixel.
    motion: Vec<(f32, f32)>,
}

fn read_frame(dir: &Path, name: &str, index: u64) -> Frame {
    let file = |aov: &str| {
        let path = dir.join(format!("{name}_{aov}_{index:06}.raw"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    };
    let pixels = (WIDTH * HEIGHT) as usize;
    let depth = f32s(&file("depth"));
    let ids = u32s(&file("id"));
    let motion = f32s(&file("motion"));
    assert_eq!(
        (depth.len(), ids.len(), motion.len()),
        (pixels, 2 * pixels, 2 * pixels)
    );
    Frame {
        depth,
        ids: ids.as_chunks::<2>().0.iter().map(|p| (p[0], p[1])).collect(),
        motion: motion.as_chunks::<2>().0.iter().map(|p| (p[0], p[1])).collect(),
    }
}

/// Mean pixel x (center) of the pixels with instance id `id`.
fn centroid_x(frame: &Frame, id: u32) -> f32 {
    let xs: Vec<f32> = frame
        .ids
        .iter()
        .enumerate()
        .filter(|(_, (instance, _))| *instance == id)
        .map(|(i, _)| (i as u32 % WIDTH) as f32 + 0.5)
        .collect();
    assert!(!xs.is_empty(), "no pixel has id {id}");
    xs.iter().sum::<f32>() / xs.len() as f32
}

#[test]
fn aovs_report_depth_stable_ids_and_backward_motion() {
    const FRAMES: u64 = 4;
    let project = run("raw", |client, out| {
        let done = render(
            client,
            job_params(
                out,
                "plate",
                FRAMES,
                json!({ "format": "raw", "aovs": ["motion", "id", "depth"], "aov_format": "raw", "cuts": [3] }),
            ),
        );
        assert_eq!(done["status"], json!("done"), "{done}");
        let dir = out.join("plate");
        let manifest = manifest(out, "plate");
        let aov = &manifest["aov"];
        assert_eq!(manifest["request"]["aovs"], json!(["depth", "id", "motion"]));
        assert_eq!(aov["encodings"]["channels"]["depth"]["unit"], json!("meter"));
        assert_eq!(aov["motion_reset_frames"], json!([0, 3]));
        assert_eq!(aov["files"], json!(FRAMES * 3));
        assert_eq!(manifest["schema"], json!("regia.render_out/2"));
        let listed = manifest["files"]["aov"].as_array().expect("files.aov");
        assert_eq!(listed.len(), 3, "one entry per AOV");
        for (entry, name) in listed.iter().zip(["depth", "id", "motion"]) {
            assert_eq!(entry["aov"], json!(name));
            let files = entry["files"].as_array().expect("files");
            assert_eq!(files.len() as u64, FRAMES, "{name}: one file per delivered frame");
            for (i, file) in files.iter().enumerate() {
                let end = format!("plate_{name}_{i:06}.raw");
                assert!(file.as_str().is_some_and(|f| f.ends_with(&end)), "{file}");
            }
        }
        assert_eq!(
            manifest["files"]["frames"].as_array().map(Vec::len),
            Some(FRAMES as usize)
        );
        let instances = aov["ids"]["instances"].as_array().expect("instances");
        assert_eq!(instances.len(), 2, "the triangle and the quad: {instances:?}");
        assert_eq!(aov["ids"]["classes"].as_array().map(Vec::len), Some(2));
        for name in ["depth", "id", "motion"] {
            assert_eq!(
                aov["frame_hashes"][name].as_array().map(Vec::len),
                Some(FRAMES as usize),
                "{name}"
            );
        }
        let frames: Vec<Frame> = (0..FRAMES).map(|i| read_frame(&dir, "plate", i)).collect();
        // Ids 1 and 2 are the two entities in the order of their bits; tell them apart by depth
        // on frame 0, then they must keep their ids (and classes) on every frame.
        let id_at_depth = |depth: f32| {
            frames[0]
                .ids
                .iter()
                .zip(&frames[0].depth)
                .find(|(_, d)| (**d - depth).abs() < 1e-3)
                .map(|(ids, _)| *ids)
                .expect("a pixel at that depth")
        };
        let (triangle, triangle_class) = id_at_depth(TRIANGLE_DEPTH);
        let (quad, quad_class) = id_at_depth(QUAD_DEPTH);
        let mut seen = [triangle, quad];
        seen.sort_unstable();
        assert_eq!(seen, [1, 2], "two instance ids, numbered from 1");
        assert_ne!(triangle_class, quad_class, "two meshes, two classes");
        let corner = 0;
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(frame.ids[corner], (0, 0), "frame {index}: the corner is sky");
            assert_eq!(frame.depth[corner], f32::INFINITY, "frame {index}: sky depth");
            for (i, (instance, class)) in frame.ids.iter().enumerate() {
                match *instance {
                    0 => assert_eq!(frame.depth[i], f32::INFINITY),
                    id if id == triangle => {
                        assert_eq!(*class, triangle_class);
                        assert!(
                            (frame.depth[i] - TRIANGLE_DEPTH).abs() < 1e-3,
                            "triangle depth {}",
                            frame.depth[i]
                        );
                        // Held (the same model and view as the last frame): exactly 0, not the
                        // rounding of projecting one position twice (ADR-1965).
                        let (u, v) = frame.motion[i];
                        assert!(
                            u == 0.0 && v == 0.0,
                            "frame {index}: the triangle does not move ({u}, {v})"
                        );
                    }
                    id if id == quad => {
                        assert_eq!(*class, quad_class);
                        assert!(
                            (frame.depth[i] - QUAD_DEPTH).abs() < 1e-3,
                            "quad depth {}",
                            frame.depth[i]
                        );
                    }
                    other => panic!("unexpected instance id {other}"),
                }
            }
        }
        // Motion: zero on frame 0 and on the cut frame; on frames 1 and 2 the quad's pixels point
        // back to where it was, the shift of its id mask.
        for reset in [0usize, 3] {
            assert!(
                frames[reset].motion.iter().all(|(u, v)| *u == 0.0 && *v == 0.0),
                "frame {reset} has no motion"
            );
        }
        for index in [1usize, 2] {
            let shift = centroid_x(&frames[index - 1], quad) - centroid_x(&frames[index], quad);
            assert!(shift < -1.0, "the quad moves right on screen ({shift})");
            let quad_motion: Vec<(f32, f32)> = frames[index]
                .ids
                .iter()
                .zip(&frames[index].motion)
                .filter(|((instance, _), _)| *instance == quad)
                .map(|(_, m)| *m)
                .collect();
            for (u, v) in &quad_motion {
                assert!(
                    (u - shift).abs() < 1.0,
                    "frame {index}: u {u} against the mask shift {shift}"
                );
                assert!(
                    v.abs() < VERTICAL_MOTION_EPS,
                    "frame {index}: no vertical motion ({v})"
                );
            }
        }
    });
    let _ = std::fs::remove_dir_all(project);
}

fn read_exr(path: &Path) -> Vec<(String, FlatSamples)> {
    let image = read_first_flat_layer_from_file(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    image
        .layer_data
        .channel_data
        .list
        .into_iter()
        .map(|channel| (channel.name.to_string(), channel.sample_data))
        .collect()
}

#[test]
fn exr_frames_are_scene_linear_and_exr_aovs_read_back() {
    let project = run("exr", |client, out| {
        let done = render(
            client,
            job_params(
                out,
                "hdr",
                2,
                json!({ "format": "exr", "aovs": ["depth", "id"], "supersample": 2 }),
            ),
        );
        assert_eq!(done["status"], json!("done"), "{done}");
        let manifest = manifest(out, "hdr");
        assert_eq!(manifest["color"]["channels"], json!(["R", "G", "B"]));
        assert_eq!(manifest["aov"]["encodings"]["format"], json!("exr"));
        let dir = out.join("hdr");
        let pixels = (WIDTH * HEIGHT) as usize;
        let frame = read_exr(&dir.join("hdr_000001.exr"));
        let names: Vec<&str> = frame.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["B", "G", "R"]);
        let ids = read_exr(&dir.join("hdr_id_000001.exr"));
        let instance = match &ids.iter().find(|(n, _)| n == "id.instance").expect("instance").1 {
            FlatSamples::U32(values) => values.clone(),
            other => panic!("ids are u32, got {other:?}"),
        };
        let depth = match &read_exr(&dir.join("hdr_depth_000001.exr"))[0].1 {
            FlatSamples::F32(values) => values.clone(),
            other => panic!("depth is f32, got {other:?}"),
        };
        assert_eq!((instance.len(), depth.len()), (pixels, pixels));
        let red = match &frame.iter().find(|(n, _)| n == "R").expect("R").1 {
            FlatSamples::F32(values) => values.clone(),
            other => panic!("R is f32, got {other:?}"),
        };
        assert!(
            red.iter().all(|v| v.is_finite() && *v >= 0.0),
            "scene-linear values are finite"
        );
        // The lit orange surfaces are brighter in red than the dark clear colour.
        let lit: Vec<f32> = (0..pixels)
            .filter(|i| instance[*i] != 0)
            .map(|i| red[i])
            .collect();
        let sky: Vec<f32> = (0..pixels)
            .filter(|i| instance[*i] == 0)
            .map(|i| red[i])
            .collect();
        assert!(!lit.is_empty() && !sky.is_empty());
        let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
        assert!(
            mean(&lit) > mean(&sky),
            "lit {} against sky {}",
            mean(&lit),
            mean(&sky)
        );
        for (i, id) in instance.iter().enumerate() {
            if *id == 0 {
                assert_eq!(depth[i], f32::INFINITY);
            } else {
                assert!(depth[i] > 2.0 && depth[i] < 4.0, "depth {}", depth[i]);
            }
        }
        // Requests the AOVs and EXR frames cannot serve are refused before anything is created.
        for bad in [
            json!({ "format": "ffv1", "aovs": ["depth"] }),
            json!({ "format": "exr", "bit_depth": 16 }),
            json!({ "aovs": ["normals"] }),
            json!({ "aov_format": "raw" }),
        ] {
            let mut params = job_params(out, "refused", 1, bad.clone());
            params["timeout_ms"] = json!(CALL_TIMEOUT_MS);
            let err = client.call("render.out_start", params).expect_err("refused");
            assert!(format!("{err:?}").contains("-32602"), "{bad}: {err:?}");
        }
        assert!(!out.join("refused").exists());
    });
    let _ = std::fs::remove_dir_all(project);
}

/// Renders `frames` raw id + motion frames with `extra` params in a fresh app and returns them.
fn motion_job(tag: &str, frames: u64, extra: Value) -> Vec<Frame> {
    let result = Arc::new(std::sync::Mutex::new(None));
    let slot = result.clone();
    let project = run(tag, move |client, out| {
        let mut params = json!({ "format": "raw", "aovs": ["depth", "id", "motion"], "aov_format": "raw" });
        params
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        let done = render(client, job_params(out, "motion", frames, params));
        assert_eq!(done["status"], json!("done"), "{done}");
        let dir = out.join("motion");
        let read: Vec<Frame> = (0..frames).map(|i| read_frame(&dir, "motion", i)).collect();
        *slot.lock().expect("slot") = Some(read);
    });
    let _ = std::fs::remove_dir_all(project);
    let taken = result.lock().expect("slot").take();
    taken.expect("the script ran")
}

/// The quad's id: the instance whose pixels sit at the quad's depth on `frame`.
fn quad_id(frame: &Frame) -> u32 {
    frame
        .ids
        .iter()
        .zip(&frame.depth)
        .find(|(_, d)| (**d - QUAD_DEPTH).abs() < 1e-3)
        .map(|((instance, _), _)| *instance)
        .expect("a quad pixel")
}

#[test]
fn aov_motion_follows_the_simulation_clock_under_a_time_map() {
    // The quad moves on the simulation clock: at scale 0.5 it covers half the distance per
    // delivered frame, and the motion AOV reports that half shift, not the render step's.
    let plain = motion_job("plain", 3, json!({}));
    let slow = motion_job("slow", 3, json!({ "time_map": [{ "frame": 0, "scale": 0.5 }] }));
    for index in [1usize, 2] {
        let shift = |frames: &[Frame]| {
            let quad = quad_id(&frames[index]);
            centroid_x(&frames[index - 1], quad) - centroid_x(&frames[index], quad)
        };
        let (full, half) = (shift(&plain), shift(&slow));
        assert!(full < -2.0, "frame {index}: the quad moves right ({full})");
        assert!(
            (half - full / 2.0).abs() < 0.75,
            "frame {index}: half speed shifts {half}, full speed {full}"
        );
        let quad = quad_id(&slow[index]);
        for ((instance, _), (u, v)) in slow[index].ids.iter().zip(&slow[index].motion) {
            if *instance == quad {
                assert!(
                    (u - half).abs() < 1.0,
                    "frame {index}: u {u} against the half-speed shift {half}"
                );
                assert!(
                    v.abs() < VERTICAL_MOTION_EPS,
                    "frame {index}: no vertical motion ({v})"
                );
            }
        }
    }
}

/// Red-Rouge v0.2.8 (ADR-1965): with the world held (`time_map` scale 0) and the camera still,
/// two jobs in one app repeat their motion AOV. Every pixel's motion is exactly 0 (nothing moved
/// since the last delivered frame), so no rounding noise can differ between the jobs, and the
/// manifest's `input_hashes` (what the pass uploaded) repeat frame by frame.
#[test]
fn a_held_world_repeats_its_motion_exactly_in_one_app() {
    const FRAMES: u64 = 6;
    let project = run("held-repeat", move |client, out| {
        let mut manifests = Vec::new();
        for take in 0..2 {
            let name = format!("held{take}");
            let done = render(
                client,
                job_params(
                    out,
                    &name,
                    FRAMES,
                    json!({
                        "format": "raw", "aovs": ["depth", "id", "motion"], "aov_format": "raw",
                        "warmup_frames": 2, "time_map": [{ "frame": 0, "scale": 0 }],
                    }),
                ),
            );
            assert_eq!(done["status"], json!("done"), "{done}");
            let dir = out.join(&name);
            for index in 0..FRAMES {
                let frame = read_frame(&dir, &name, index);
                assert!(
                    frame.motion.iter().all(|(u, v)| *u == 0.0 && *v == 0.0),
                    "{name} frame {index}: a held world has no motion"
                );
                assert!(
                    frame.ids.iter().any(|(instance, _)| *instance != 0),
                    "{name}: a scene was drawn"
                );
            }
            manifests.push(manifest(out, &name));
        }
        let aov = |m: &Value, key: &str| m["aov"][key].clone();
        let inputs = aov(&manifests[0], "input_hashes");
        assert_eq!(inputs.as_array().map(Vec::len), Some(FRAMES as usize), "{inputs}");
        assert_eq!(
            inputs,
            aov(&manifests[1], "input_hashes"),
            "the pass's inputs repeat"
        );
        assert_eq!(
            aov(&manifests[0], "frame_hashes"),
            aov(&manifests[1], "frame_hashes"),
            "every AOV repeats"
        );
        // Held: from frame 1 on every frame uploads what the one before it did.
        let list = inputs.as_array().cloned().unwrap_or_default();
        assert!(list[1..].windows(2).all(|w| w[0] == w[1]), "{inputs}");
    });
    let _ = std::fs::remove_dir_all(project);
}

/// Renders two raw frames with every AOV in a fresh app whose shot player holds a one-shot
/// sequence at time 0 with the camera where the scene's camera is; with `fade` the shot fades in
/// over its whole length, so the compositor's fade layer is (all but) fully black on both frames.
/// Returns the manifest and frame 0's pixels.
fn fade_job(tag: &str, fade: bool) -> (Value, Vec<u8>) {
    let result = Arc::new(std::sync::Mutex::new(None));
    let slot = result.clone();
    let setup = move |app: &mut App| {
        let fade_in = if fade { 100.0 } else { 0.0 };
        let fov = 0.9_f32.to_degrees();
        let ron = format!(
            "(version: 2, name: \"fade\", fps: 30.0, shots: [(name: \"hold\", duration: 100.0, camera: (path: (keys: [(time: 0.0, position: (0.0, 0.0, {QUAD_DEPTH}))], look_at: Point((0.0, 0.0, 0.0))), fov: [(time: 0.0, value: {fov})]), fade_in: {fade_in})])"
        );
        let sequence = regia_framework::shots::ShotSequence::from_ron(&ron).expect("a valid sequence");
        app.world_mut()
            .resource_mut::<regia_framework::shots::ShotPlayer>()
            .load(sequence);
    };
    let project = run_with(tag, setup, move |client, out| {
        let done = render(
            client,
            job_params(
                out,
                "fade",
                2,
                json!({ "format": "raw", "aovs": ["depth", "id", "motion"], "aov_format": "raw" }),
            ),
        );
        assert_eq!(done["status"], json!("done"), "{done}");
        let pixels = std::fs::read(out.join("fade").join("fade_000000.raw")).expect("frame 0");
        *slot.lock().expect("slot") = Some((manifest(out, "fade"), pixels));
    });
    let _ = std::fs::remove_dir_all(project);
    let taken = result.lock().expect("slot").take();
    taken.expect("the script ran")
}

#[test]
fn the_shot_fade_is_in_the_frame_and_never_in_the_aovs() {
    // The fade is composited by the compositor after the UI; the AOVs are drawn by their own pass
    // and never carry it (design constraint 5, ADR-1554).
    let (plain, plain_pixels) = fade_job("no-fade", false);
    let (faded, faded_pixels) = fade_job("fade", true);
    let brightest = |pixels: &[u8]| {
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| p[..3].iter().copied())
            .max()
            .unwrap_or(0)
    };
    assert!(
        brightest(&plain_pixels) > 16,
        "the scene is visible without the fade"
    );
    assert!(
        brightest(&faded_pixels) <= 1,
        "a black fade at amount >= 0.999 covers the frame ({})",
        brightest(&faded_pixels)
    );
    assert_ne!(plain["frame_hashes"], faded["frame_hashes"]);
    assert_eq!(
        plain["aov"]["frame_hashes"], faded["aov"]["frame_hashes"],
        "depth, ids and motion are the same with and without the fade"
    );
    assert_eq!(plain["fade"]["frames"], json!([]));
    assert_eq!(faded["fade"]["in_frames"], json!(true));
    assert_eq!(faded["fade"]["in_aovs"], json!(false));
    let frames = faded["fade"]["frames"].as_array().expect("fade frames");
    assert_eq!(frames.len(), 2, "both delivered frames: {frames:?}");
    assert_eq!(frames[1]["frame"], json!(1));
    for frame in frames {
        let amount = frame["amount"].as_f64().expect("amount");
        assert!(amount >= 0.999, "{frame}");
        assert_eq!(frame["color"], json!([0.0, 0.0, 0.0]));
    }
}
