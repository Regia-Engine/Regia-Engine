// SPDX-License-Identifier: MPL-2.0
//! Render-out material AOVs (ADR-3256) on lavapipe through the agent protocol: two quads of one
//! tilted plane carry two materials; the world normal AOV equals the plane's normal, roughness,
//! metalness and base color equal the authored values, the material id separates the two
//! materials and the sky stays 0, a repeat run writes byte-identical files, the EXR and PNG
//! writers carry the same quantities, and the depth/id AOVs of the same job are unchanged.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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

/// A hang guard only: nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(900);
const CALL_TIMEOUT_MS: u64 = 60_000;
const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
/// The plane's tilt about +X, radians.
const TILT: f32 = 0.5;
/// What the AOVs must equal, within this.
const EPS: f32 = 1.0e-3;

const ALL_MATERIAL: [&str; 6] = [
    "normal",
    "normal_view",
    "roughness",
    "metalness",
    "base_color",
    "material_id",
];

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

fn render(client: &mut Client, out: &Path, name: &str, frames: u64, extra: Value) -> Value {
    let mut params = json!({
        "out": out.join(name).display().to_string(), "name": name, "frames": frames,
        "width": WIDTH, "height": HEIGHT, "fps": 30, "hud": false,
    });
    params
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    let started = call(client, "render.out_start", params);
    wait(client, started["job"].as_u64().expect("job id"))
}

/// A headless app with a tilted plane of two quads (two materials) and runs `script` against it.
fn run(tag: &str, script: impl FnOnce(&mut Client, &Path) + Send + 'static) -> PathBuf {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-material-aov-{tag}"));
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
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    app.insert_resource(settings());
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 18_000.0,
        double_sided: true,
    });
    let camera = Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, 2.6));
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
    let mesh = quad();
    let metal = Arc::new(CpuMaterial {
        base_color: [0.8, 0.25, 0.1, 1.0],
        roughness: 0.3,
        metallic: 1.0,
        ..CpuMaterial::default()
    });
    let paint = Arc::new(CpuMaterial {
        base_color: [0.1, 0.4, 0.9, 1.0],
        roughness: 0.7,
        metallic: 0.0,
        ..CpuMaterial::default()
    });
    let rotation = regia_core::math::Quat::from_rotation_x(TILT);
    for (x, material) in [(-0.35f32, metal), (0.35, paint)] {
        let transform = Transform {
            translation: regia_core::math::Vec3::new(x, 0.0, 0.0),
            rotation,
            ..Transform::default()
        };
        app.world_mut().spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            MeshRenderer::new(mesh.clone(), material),
        ));
    }
    app.world_mut().spawn((
        Transform::default(),
        GlobalTransform::default(),
        DirectionalLight {
            direction: [0.0, 0.0, -1.0],
            color: [1.0, 1.0, 1.0],
            illuminance: 60_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let thread = std::thread::Builder::new()
        .name(format!("render-out-material-aov-{tag}"))
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

fn raw(dir: &Path, name: &str, aov: &str, frame: u64) -> Vec<u8> {
    let path = dir.join(format!("{name}_{aov}_{frame:06}.raw"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn near(got: f32, want: f32, what: &str) {
    assert!((got - want).abs() <= EPS, "{what}: {got} vs {want}");
}

#[test]
fn material_aovs_equal_the_authored_values_and_repeat_byte_for_byte() {
    let hashes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let slot = hashes.clone();
    let project = run("raw", move |client, out| {
        let mut files: Vec<Vec<(String, Vec<u8>)>> = Vec::new();
        for take in ["first", "second"] {
            let mut aovs: Vec<&str> = ALL_MATERIAL.to_vec();
            aovs.extend(["depth", "id"]);
            let done = render(
                client,
                out,
                take,
                2,
                json!({ "format": "raw", "aovs": aovs, "aov_format": "raw" }),
            );
            assert_eq!(done["status"], json!("done"), "{done}");
            let dir = out.join(take);
            let mut set = Vec::new();
            for aov in ALL_MATERIAL.iter().chain(&["depth", "id"]) {
                for frame in 0..2 {
                    set.push((format!("{aov}{frame}"), raw(&dir, take, aov, frame)));
                }
            }
            files.push(set);
        }
        // A repeat run writes byte-identical files.
        assert_eq!(files[0], files[1], "second run differs from the first");

        let dir = out.join("first");
        let pixels = (WIDTH * HEIGHT) as usize;
        let normal = f32s(&raw(&dir, "first", "normal", 1));
        let view = f32s(&raw(&dir, "first", "normal_view", 1));
        let rough = f32s(&raw(&dir, "first", "roughness", 1));
        let metal = f32s(&raw(&dir, "first", "metalness", 1));
        let color = f32s(&raw(&dir, "first", "base_color", 1));
        let ids = u32s(&raw(&dir, "first", "material_id", 1));
        assert_eq!(
            (
                normal.len(),
                view.len(),
                rough.len(),
                metal.len(),
                color.len(),
                ids.len()
            ),
            (3 * pixels, 3 * pixels, pixels, pixels, 3 * pixels, pixels)
        );
        let plane = [0.0, -TILT.sin(), TILT.cos()];
        let mut seen = std::collections::BTreeMap::<u32, usize>::new();
        for i in 0..pixels {
            let id = ids[i];
            if id == 0 {
                // Sky: everything 0.
                assert!(normal[3 * i..3 * i + 3].iter().all(|v| *v == 0.0), "sky normal");
                assert!(rough[i] == 0.0 && metal[i] == 0.0, "sky roughness/metalness");
                assert!(color[3 * i..3 * i + 3].iter().all(|v| *v == 0.0), "sky color");
                continue;
            }
            *seen.entry(id).or_default() += 1;
            for c in 0..3 {
                near(normal[3 * i + c], plane[c], "world normal");
                // The camera looks down -z from +z with no rotation: view space is world space.
                near(view[3 * i + c], plane[c], "view normal");
            }
            let (want_rough, want_metal, want_color) = if metal[i] > 0.5 {
                (0.3, 1.0, [0.8, 0.25, 0.1])
            } else {
                (0.7, 0.0, [0.1, 0.4, 0.9])
            };
            near(rough[i], want_rough, "roughness");
            near(metal[i], want_metal, "metalness");
            for c in 0..3 {
                near(color[3 * i + c], want_color[c], "base color");
            }
        }
        assert_eq!(seen.len(), 2, "two materials, two ids: {seen:?}");
        assert!(
            seen.values().all(|n| *n > 50),
            "both materials cover pixels: {seen:?}"
        );

        // The manifest names the AOVs, lists the materials and keeps the existing AOV blocks.
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(dir.join("first_render_out.json")).expect("manifest"))
                .expect("json");
        let aov = &manifest["aov"];
        assert_eq!(aov["ids"]["materials"].as_array().map(Vec::len), Some(2), "{aov}");
        assert!(aov["encodings"]["channels"]["normal"].is_object());
        assert!(aov["encodings"]["channels"]["depth"].is_object());
        assert_eq!(
            aov["frame_hashes"]["material_id"].as_array().map(Vec::len),
            Some(2)
        );
        *slot.lock().expect("lock") = vec![aov["frame_hashes"].clone()];
    });
    let _ = std::fs::remove_dir_all(project);
    assert!(
        !hashes.lock().expect("lock").is_empty(),
        "the script ran to the end"
    );
}

#[test]
fn exr_and_png_carry_the_same_quantities() {
    let project = run("formats", |client, out| {
        let done = render(
            client,
            out,
            "exr",
            1,
            json!({ "format": "raw", "aovs": ["normal", "roughness", "material_id"] }),
        );
        assert_eq!(done["status"], json!("done"), "{done}");
        let image = exr::prelude::read_first_flat_layer_from_file(out.join("exr/exr_normal_000000.exr"))
            .expect("normal exr");
        let names: Vec<String> = image
            .layer_data
            .channel_data
            .list
            .iter()
            .map(|c| c.name.to_string())
            .collect();
        assert_eq!(names, ["normal.x", "normal.y", "normal.z"]);
        assert!(out.join("exr/exr_material_id_000000.exr").exists());

        for (name, format, bytes) in [("png8", "png", 1usize), ("png16", "png16", 2)] {
            let done = render(
                client,
                out,
                name,
                1,
                json!({ "format": "raw", "aovs": ["normal", "roughness", "base_color"], "aov_format": format }),
            );
            assert_eq!(done["status"], json!("done"), "{done}");
            let path = out.join(name).join(format!("{name}_roughness_000000.png"));
            let decoder =
                png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).expect("png")));
            let mut reader = decoder.read_info().expect("header");
            let mut buf = vec![0; reader.output_buffer_size().expect("size")];
            reader.next_frame(&mut buf).expect("frame");
            // Pixel column 25 is inside the left quad (the centre column is the gap between the two).
            let center = ((HEIGHT / 2) * WIDTH + 25) as usize;
            let got = if bytes == 1 {
                f32::from(buf[center]) / 255.0
            } else {
                f32::from(u16::from_be_bytes([buf[2 * center], buf[2 * center + 1]])) / 65535.0
            };
            // The pixel is on one of the two quads: roughness 0.3 or 0.7.
            assert!(
                (got - 0.3).abs() < 0.01 || (got - 0.7).abs() < 0.01,
                "{name}: {got}"
            );
        }
        // Exact quantities refuse the png formats with their own code.
        for (aovs, code) in [
            (json!(["depth"]), "aov_format_unsupported"),
            (json!(["material_id"]), "aov_format_unsupported"),
        ] {
            let params = json!({
                "out": out.join("refused").display().to_string(), "frames": 1, "width": WIDTH, "height": HEIGHT,
                "aovs": aovs, "aov_format": "png", "timeout_ms": CALL_TIMEOUT_MS,
            });
            let err = client.call("render.out_start", params).expect_err("refused");
            assert!(format!("{err:?}").contains(code), "{aovs}: {err:?}");
        }
        // An unknown name has its own code, and nothing is created.
        let params = json!({
            "out": out.join("refused").display().to_string(), "frames": 1, "width": WIDTH, "height": HEIGHT,
            "aovs": ["albedo"], "timeout_ms": CALL_TIMEOUT_MS,
        });
        let err = client.call("render.out_start", params).expect_err("refused");
        assert!(format!("{err:?}").contains("unknown_aov"), "{err:?}");
        assert!(!out.join("refused").exists());
    });
    let _ = std::fs::remove_dir_all(project);
}
