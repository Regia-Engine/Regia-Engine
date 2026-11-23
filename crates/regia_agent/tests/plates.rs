// SPDX-License-Identifier: MPL-2.0
//! `seeing.capture_plate` and `render.set_reference_overlay` end to end on lavapipe (ADR-2463):
//! the same shot captured twice gives identical files, a different pose a different picture, a
//! `.shot.ron` list names its shots, bad requests answer their documented reasons, and the overlay
//! draws a frame's own picture as a zero difference and splits the frame at the wipe line.
//! No golden image: every check compares captures of this test with each other.

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
/// Most app updates before the first renderable extracted frame (it comes within a few).
const WARM_UP_FRAMES: u32 = 600;

fn vertex(pos: [f32; 3], normal: [f32; 3]) -> LabVertex {
    LabVertex::from_pos_normal_uv(pos, normal, [0.0, 0.0])
}

fn fan_triangle() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            vertex([-1.2, -0.9, 0.0], [-0.8, 0.0, 0.6]),
            vertex([1.2, -0.9, 0.0], [0.8, 0.0, 0.6]),
            vertex([0.0, 0.9, 0.0], [0.0, 0.0, 1.0]),
        ],
        indices: vec![0, 1, 2],
    })
}

fn deterministic_settings(aa: AntiAliasing) -> RenderSettings {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = aa;
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

fn base64_decode(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes().filter(|c| *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

fn rgba(png_bytes: &[u8]) -> Vec<u8> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(png_bytes))
        .read_info()
        .expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    assert_eq!(info.color_type, png::ColorType::Rgba);
    buf.truncate(info.buffer_size());
    buf
}

fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .expect("header")
        .write_image_data(rgba)
        .expect("data");
    out
}

/// A fresh headless app showing a still shaded triangle; runs `script` against its agent with
/// the output root and returns the project dir (removed by the caller).
fn run(tag: &str, script: impl FnOnce(&mut Client, &Path) + Send + 'static) -> PathBuf {
    let project = regia_test_utils::unique_temp_dir(&format!("plates-{tag}"));
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
    app.insert_resource(deterministic_settings(AntiAliasing::None));
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
    let material = Arc::new(CpuMaterial {
        base_color: [0.85, 0.42, 0.12, 1.0],
        roughness: 1.0,
        metallic: 0.0,
        ..CpuMaterial::default()
    });
    let mesh_tf = Transform::default();
    app.world_mut().spawn((
        mesh_tf,
        GlobalTransform(mesh_tf.compute_affine()),
        MeshRenderer::new(fan_triangle(), material),
        DirectionalLight {
            direction: [0.0, 0.0, -1.0],
            color: [1.0, 1.0, 1.0],
            illuminance: 60_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));

    // ADR-7566: run until the app has extracted a renderable frame before the script connects. A
    // first `seeing.screenshot` that reached the agent before the first extraction answered
    // `F0011` "nothing to capture offscreen" (2 of 5 runs on the Windows a test machine, v0.5.0-rc2).
    app.finish_plugins();
    let mut warm = 0;
    while !app
        .world()
        .get_resource::<regia_render::runtime::ExtractedFrame>()
        .is_some_and(regia_render::runtime::ExtractedFrame::is_renderable)
    {
        assert!(warm < WARM_UP_FRAMES, "no renderable frame after {warm} updates");
        app.update();
        warm += 1;
    }

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let thread = std::thread::Builder::new()
        .name(format!("plates-{tag}"))
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

    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !thread.is_finished() && Instant::now() < deadline {
        app.update();
    }
    thread.join().expect("script finished");
    project
}

fn reason(client: &mut Client, method: &str, params: Value) -> String {
    let mut params = params;
    params["timeout_ms"] = json!(CALL_TIMEOUT_MS);
    match client.call(method, params) {
        Ok(v) => panic!("{method} should have failed, answered {v}"),
        Err(err) => {
            let text = format!("{err:?}");
            // The reason is the lowercase word in `data.reason`.
            let marker = "\"reason\":";
            let start = text.find(marker).or_else(|| text.find("reason:"));
            text[start.unwrap_or(0)..].to_string()
        }
    }
}

/// Starts a plate and polls it to the end; returns the finished answer.
fn plate(client: &mut Client, params: Value) -> Value {
    let started = call(client, "seeing.capture_plate", params);
    let id = started["plate"].as_u64().expect("plate id");
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while Instant::now() < deadline {
        let answer = call(client, "seeing.capture_plate", json!({ "plate": id }));
        if answer["status"] == json!("done") {
            return answer;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("plate {id} did not finish");
}

fn file_hash(path: &Path) -> String {
    blake3::hash(&std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .to_hex()
        .to_string()
}

fn shot(name: &str, eye: [f32; 3]) -> Value {
    json!({
        "name": name,
        "camera": { "eye": eye, "target": [0.0, 0.0, 0.0] },
        "width": WIDTH, "height": HEIGHT, "warmup_frames": 3,
    })
}

#[test]
fn the_same_shot_twice_gives_identical_files_and_a_pose_changes_the_picture() {
    let project = run("twice", |client, out| {
        let a = plate(
            client,
            json!({ "shot": shot("tri", [0.0, 0.0, 2.6]), "out": out.display().to_string(), "tag": "a" }),
        );
        let b = plate(
            client,
            json!({ "shot": shot("tri", [0.0, 0.0, 2.6]), "out": out.display().to_string(), "tag": "b" }),
        );
        let far = plate(
            client,
            json!({ "shot": shot("tri", [0.0, 0.0, 6.0]), "out": out.display().to_string(), "tag": "far" }),
        );
        let hash = |stem: &str| file_hash(&out.join(format!("{stem}_000000.png")));
        assert_eq!(hash("tri-a"), hash("tri-b"), "same shot, same file");
        assert_ne!(hash("tri-a"), hash("tri-far"), "another pose, another picture");
        // The sidecar names the file hashes and matches the files on disk.
        for (plate, stem) in [(&a, "tri-a"), (&b, "tri-b"), (&far, "tri-far")] {
            let sidecar = &plate["sidecar"];
            assert_eq!(sidecar["schema"], json!("regia.plate/1"));
            assert!(sidecar["engine"].as_str().is_some_and(|v| !v.is_empty()));
            assert_eq!(sidecar["shot"]["name"], json!("tri"));
            let files = sidecar["files"].as_array().expect("files");
            assert_eq!(files.len(), 1, "{sidecar}");
            assert_eq!(files[0]["file"], json!(format!("{stem}_000000.png")));
            assert_eq!(files[0]["blake3"], json!(hash(stem)));
            assert!(out.join(format!("{stem}.plate.json")).is_file());
            assert!(
                !out.join(format!("{stem}.plate.json.part")).exists(),
                "atomic write leaves no part file"
            );
        }
        assert_eq!(a["sidecar"]["frame_hash"], b["sidecar"]["frame_hash"]);
        assert_eq!(
            a["sidecar"]["files"][0]["blake3"],
            b["sidecar"]["files"][0]["blake3"]
        );
        // The camera as rendered is recorded.
        let eye = &far["sidecar"]["camera"]["eye"];
        assert!((eye[2].as_f64().expect("z") - 6.0).abs() < 1e-3, "{eye}");
        // A polled finished plate answers the same sidecar again.
        let again = call(client, "seeing.capture_plate", json!({ "plate": a["plate"] }));
        assert_eq!(again["sidecar"], a["sidecar"]);
    });
    let _ = std::fs::remove_dir_all(project);
}

#[test]
fn a_shot_list_file_names_its_shots() {
    let project = run("list", |client, out| {
        let project = out.parent().expect("project").to_path_buf();
        std::fs::create_dir_all(&project).expect("dir");
        std::fs::write(
            project.join("review.shot.ron"),
            "(format_version: 1, shots: [(name: \"one\", width: Some(32), height: Some(24), warmup_frames: Some(2)), (name: \"two\", width: Some(32), height: Some(24), warmup_frames: Some(2))])",
        )
        .expect("write list");
        let done = plate(
            client,
            json!({ "shots_file": "review.shot.ron", "name": "two", "out": out.display().to_string() }),
        );
        assert_eq!(done["name"], json!("two"));
        assert!(out.join("two_000000.png").is_file());
        assert!(!out.join("one_000000.png").exists());
        let missing = reason(
            client,
            "seeing.capture_plate",
            json!({ "shots_file": "review.shot.ron", "name": "three" }),
        );
        assert!(missing.contains("shot_not_found"), "{missing}");
        std::fs::write(project.join("bad.shot.ron"), "(format_version: 9, shots: [])").expect("write");
        let bad = reason(
            client,
            "seeing.capture_plate",
            json!({ "shots_file": "bad.shot.ron", "name": "x" }),
        );
        assert!(bad.contains("shot_list_invalid"), "{bad}");
    });
    let _ = std::fs::remove_dir_all(project);
}

#[test]
fn bad_plate_requests_answer_their_reasons() {
    let project = run("errors", |client, out| {
        let out_text = out.display().to_string();
        let cases: Vec<(Value, &str)> = vec![
            (json!({}), "bad_shot"),
            (json!({ "shot": { "name": "Bad Name" } }), "bad_shot"),
            (
                json!({ "shot": { "name": "a", "aovs": ["normals"] } }),
                "bad_shot",
            ),
            (json!({ "shot": { "name": "a", "extra": 1 } }), "bad_shot"),
            (
                json!({ "shot": { "name": "a", "profile": "nope" } }),
                "unknown_profile",
            ),
            (
                json!({ "shot": { "name": "a", "camera": { "name": "no_such_camera" } } }),
                "camera_not_found",
            ),
            (
                json!({ "shot": { "name": "a" }, "out": "../../escape" }),
                "path_outside_project",
            ),
            (json!({ "plate": 999 }), "plate_not_found"),
        ];
        for (mut params, expected) in cases {
            if params.get("out").is_none() && params.get("plate").is_none() {
                params["out"] = json!(out_text);
            }
            let got = reason(client, "seeing.capture_plate", params.clone());
            assert!(got.contains(expected), "{params} -> {got} (wanted {expected})");
        }
        assert!(
            !out.exists() || std::fs::read_dir(out).map_or(true, |mut d| d.next().is_none()),
            "a refused plate writes nothing"
        );
    });
    let _ = std::fs::remove_dir_all(project);
}

fn screenshot_png(client: &mut Client, path: &Path) -> Vec<u8> {
    let shot = call(
        client,
        "seeing.screenshot",
        json!({ "target": "offscreen", "width": WIDTH, "height": HEIGHT, "hud": false, "calibration": "neutral" }),
    );
    let bytes = base64_decode(shot["png_base64"].as_str().expect("png"));
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, &bytes).expect("write png");
    bytes
}

#[test]
fn the_overlay_zeroes_a_frames_own_picture_and_wipes_at_the_split() {
    let project = run("overlay", |client, out| {
        let project = out.parent().expect("project").to_path_buf();
        let reference = project.join("refs/frame.png");
        let own = screenshot_png(client, &reference);
        let plain = rgba(&own);
        assert!(
            plain
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[0] > 8 || p[1] > 8 || p[2] > 8),
            "the frame is not black"
        );

        // Difference against its own picture is zero everywhere.
        let state = call(
            client,
            "render.set_reference_overlay",
            json!({ "mode": "difference", "path": "refs/frame.png" }),
        );
        assert_eq!(state["image"], json!({ "width": WIDTH, "height": HEIGHT }));
        assert_eq!(state["loads"], json!(1));
        let diff = rgba(&screenshot_png(client, &project.join("refs/diff.png")));
        assert!(
            diff.as_chunks::<4>().0.iter().all(|p| p[..3] == [0, 0, 0]),
            "difference of the frame with itself"
        );

        // The same file is not decoded again; wipe at 0.5 splits the frame: left is the live frame,
        // right is the (here identical) reference, so build the reference from a solid colour.
        let again = call(
            client,
            "render.set_reference_overlay",
            json!({ "mode": "difference" }),
        );
        assert_eq!(again["loads"], json!(1), "cached");
        let solid: Vec<u8> = (0..WIDTH * HEIGHT).flat_map(|_| [0u8, 0, 255, 255]).collect();
        let png = encode_rgba(WIDTH, HEIGHT, &solid);
        std::fs::write(project.join("refs/blue.png"), png).expect("write");
        let wipe = call(
            client,
            "render.set_reference_overlay",
            json!({ "mode": "wipe", "path": "refs/blue.png", "split": 0.5 }),
        );
        assert_eq!(wipe["loads"], json!(2));
        let wiped = rgba(&screenshot_png(client, &project.join("refs/wipe.png")));
        let px = |img: &[u8], x: u32, y: u32| {
            let i = ((y * WIDTH + x) * 4) as usize;
            [img[i], img[i + 1], img[i + 2]]
        };
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                if x < WIDTH / 2 {
                    assert_eq!(
                        px(&wiped, x, y),
                        px(&plain, x, y),
                        "left is the frame at ({x},{y})"
                    );
                } else {
                    assert_eq!(
                        px(&wiped, x, y),
                        [0, 0, 255],
                        "right is the reference at ({x},{y})"
                    );
                }
            }
        }

        // Off restores the frame exactly.
        call(client, "render.set_reference_overlay", json!({ "mode": "off" }));
        let off = rgba(&screenshot_png(client, &project.join("refs/off.png")));
        assert_eq!(off, plain);
    });
    let _ = std::fs::remove_dir_all(project);
}

#[test]
fn bad_overlay_requests_answer_their_reasons() {
    let project = run("overlay-errors", |client, out| {
        let project = out.parent().expect("project").to_path_buf();
        std::fs::create_dir_all(&project).expect("dir");
        std::fs::write(project.join("not.png"), b"this is not a png").expect("write");
        let cases: Vec<(Value, &str)> = vec![
            (json!({ "mode": "Blend", "path": "x.png" }), "unknown_mode"),
            (json!({ "mode": "split" }), "unknown_mode"),
            (json!({}), "unknown_mode"),
            (json!({ "mode": "blend", "opacity": 2.0 }), "bad_opacity"),
            (json!({ "mode": "wipe", "split": "half" }), "bad_split"),
            (json!({ "mode": "blend" }), "path_required"),
            (
                json!({ "mode": "blend", "path": "missing.png" }),
                "image_not_found",
            ),
            (
                json!({ "mode": "blend", "path": "../../etc/passwd" }),
                "path_outside_project",
            ),
            (json!({ "mode": "blend", "path": "not.png" }), "image_unreadable"),
        ];
        for (params, expected) in cases {
            let got = reason(client, "render.set_reference_overlay", params.clone());
            assert!(got.contains(expected), "{params} -> {got} (wanted {expected})");
        }
        // Off needs no image.
        let off = call(client, "render.set_reference_overlay", json!({ "mode": "off" }));
        assert_eq!(off["mode"], json!("off"));
        assert_eq!(off["image"], Value::Null);
    });
    let _ = std::fs::remove_dir_all(project);
}

/// corr-agent finding 15 (ADR-5895): the sidecar took the camera when the plate was first polled
/// done, not when its frame was rendered. Plate A renders at z = 2.6; before A is polled, plate B
/// moves the camera to z = 6.0 and renders; A's sidecar then said 6.0. It now records the camera
/// as the frame was delivered.
#[test]
fn a_plate_polled_after_another_plate_records_its_own_camera() {
    let project = run("late-poll", |client, out| {
        let started = call(
            client,
            "seeing.capture_plate",
            json!({ "shot": shot("tri", [0.0, 0.0, 2.6]), "out": out.display().to_string(), "tag": "near" }),
        );
        let near = started["plate"].as_u64().expect("plate id");
        let job = started["job"].as_u64().expect("render-out job");
        // Wait for A's render-out job without polling the plate (a poll would write its sidecar).
        let deadline = Instant::now() + SCRIPT_TIMEOUT;
        loop {
            let progress = call(client, "render.out_progress", json!({ "job": job }));
            if progress["status"] == json!("done") {
                break;
            }
            assert!(Instant::now() < deadline, "plate {near}'s job did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
        let far = plate(
            client,
            json!({ "shot": shot("tri", [0.0, 0.0, 6.0]), "out": out.display().to_string(), "tag": "far" }),
        );
        let far_eye = &far["sidecar"]["camera"]["eye"];
        assert!((far_eye[2].as_f64().expect("z") - 6.0).abs() < 1e-3, "{far_eye}");
        let answer = call(client, "seeing.capture_plate", json!({ "plate": near }));
        assert_eq!(answer["status"], json!("done"));
        let eye = &answer["sidecar"]["camera"]["eye"];
        assert!(
            (eye[2].as_f64().expect("z") - 2.6).abs() < 1e-3,
            "plate {near} was rendered at z 2.6, its sidecar says {eye}"
        );
    });
    let _ = std::fs::remove_dir_all(project);
}
