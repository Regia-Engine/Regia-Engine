// SPDX-License-Identifier: MPL-2.0
//! Render-out frame writing off the render thread (ADR-1412), on lavapipe: the same frames written
//! as PNG at every level and as raw files have the same hashes and the same decoded pixels, for
//! any number of writer threads; the raw manifest block describes the files; a cancel leaves
//! exactly the frames from 0 to the last delivered one, in order, with no partial file.

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

/// A grid of small flat-shaded quads with pseudo-random normals: rendered at 4K every quad covers
/// about 6x6 pixels with its own shade, so the frame compresses like a busy real one, not like the
/// flat test scene. Used by the benchmarks when `REGIA_PERF_NOISE` is set.
fn noise_grid() -> Arc<CpuMesh> {
    let (nx, ny) = (640u32, 360u32);
    let mut seed = 0x9e37_79b9u32;
    let mut next = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / 16_777_216.0
    };
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for j in 0..ny {
        for i in 0..nx {
            let x0 = -2.4 + 4.8 * i as f32 / nx as f32;
            let x1 = -2.4 + 4.8 * (i + 1) as f32 / nx as f32;
            let y0 = -1.35 + 2.7 * j as f32 / ny as f32;
            let y1 = -1.35 + 2.7 * (j + 1) as f32 / ny as f32;
            let n = [next() - 0.5, next() - 0.5, 0.5 + 0.5 * next()];
            let base = vertices.len() as u32;
            for p in [[x0, y0], [x1, y0], [x1, y1], [x0, y1]] {
                vertices.push(vertex([p[0], p[1], 0.0], n));
            }
            indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
    }
    Arc::new(CpuMesh { vertices, indices })
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

/// Polls `render.out_progress` until the job is no longer running or cancelling.
fn wait(client: &mut Client, job: u64) -> Value {
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while Instant::now() < deadline {
        let progress = call(
            client,
            "render.out_progress",
            json!({ "job": job, "hashes": true }),
        );
        if progress["status"] != json!("running") && progress["status"] != json!("cancelling") {
            return progress;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("render-out job {job} did not finish");
}

/// Starts a job and waits for it to end.
fn render(client: &mut Client, params: Value) -> Value {
    let started = call(client, "render.out_start", params);
    wait(client, started["job"].as_u64().expect("job id"))
}

fn hashes(progress: &Value) -> Vec<String> {
    progress["hashes"]["hashes"]
        .as_array()
        .expect("hashes")
        .iter()
        .map(|h| h.as_str().expect("hex").to_string())
        .collect()
}

/// A decoded PNG as plain native-order samples widened to u16.
fn decode(path: &Path) -> Vec<u16> {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .expect("png header");
    let depth = reader.info().bit_depth;
    let mut buf = vec![0; reader.output_buffer_size().expect("size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    let bytes = &buf[..info.buffer_size()];
    match depth {
        png::BitDepth::Sixteen => bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_be_bytes(*b))
            .collect(),
        _ => bytes.iter().map(|b| u16::from(*b)).collect(),
    }
}

/// A raw frame file as samples widened to u16 (`bits` 8, or 16 little-endian).
fn read_raw(path: &Path, bits: u32) -> Vec<u16> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    if bits == 16 {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect()
    } else {
        bytes.iter().map(|b| u16::from(*b)).collect()
    }
}

/// A fresh headless app showing a still shaded triangle; runs `script` against its agent with
/// the output root and returns the project dir (removed by the caller).
fn run(tag: &str, script: impl FnOnce(&mut Client, &Path) + Send + 'static) -> PathBuf {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-frames-{tag}"));
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
        MeshRenderer::new(
            if regia_core::brand::env_var_os("REGIA_PERF_NOISE").is_some() {
                noise_grid()
            } else {
                fan_triangle()
            },
            material,
        ),
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
        .name(format!("render-out-frames-{tag}"))
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

fn manifest(out: &Path, name: &str) -> Value {
    serde_json::from_slice(
        &std::fs::read(out.join(name).join(format!("{name}_render_out.json"))).expect("manifest"),
    )
    .expect("json")
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

/// Runs the same frames as every PNG level and as raw (and with 1 and 4 writers) at `bits`, and
/// checks hashes and decoded pixels agree everywhere.
fn same_frames_every_way(tag: &str, bits: u64) {
    const FRAMES: u64 = 3;
    let project = run(tag, move |client, out| {
        let depth = json!({ "bit_depth": bits });
        let jobs: Vec<(&str, Value)> = vec![
            ("png_fast", json!({ "format": "png", "png_compression": "fast" })),
            (
                "png_balanced",
                json!({ "format": "png", "png_compression": "balanced", "writers": 1 }),
            ),
            (
                "png_best",
                json!({ "format": "png", "png_compression": "best", "writers": 4 }),
            ),
            ("raw_auto", json!({ "format": "raw" })),
            ("raw_one", json!({ "format": "raw", "writers": 1 })),
        ];
        let mut reference: Option<(Vec<String>, Vec<Vec<u16>>)> = None;
        for (name, extra) in &jobs {
            let mut extra = extra.clone();
            extra
                .as_object_mut()
                .expect("object")
                .extend(depth.as_object().expect("object").clone());
            let done = render(client, job_params(out, name, FRAMES, extra));
            assert_eq!(done["status"], json!("done"), "{name}: {done}");
            assert_eq!(done["frame"], json!(FRAMES), "{name}");
            let manifest = manifest(out, name);
            assert_eq!(manifest["delivered"], json!(FRAMES), "{name}");
            assert_eq!(
                manifest["files"]["frames"].as_array().map(Vec::len),
                Some(FRAMES as usize),
                "{name}"
            );
            let raw = manifest["request"]["format"] == json!("raw");
            let ext = if raw { "raw" } else { "png" };
            let pixels: Vec<Vec<u16>> = (0..FRAMES)
                .map(|i| {
                    let path = out.join(name).join(format!("{name}_{i:06}.{ext}"));
                    if raw {
                        read_raw(&path, bits as u32)
                    } else {
                        decode(&path)
                    }
                })
                .collect();
            if raw {
                let layout = &manifest["raw"];
                let bytes = (bits / 8) * 4 * u64::from(WIDTH);
                assert_eq!(layout["width"], json!(WIDTH));
                assert_eq!(layout["height"], json!(HEIGHT));
                assert_eq!(layout["channels"], json!(4));
                assert_eq!(layout["bit_depth"], json!(bits));
                assert_eq!(layout["row_stride_bytes"], json!(bytes));
                assert_eq!(layout["frame_bytes"], json!(bytes * u64::from(HEIGHT)));
                assert_eq!(
                    layout["byte_order"],
                    json!(if bits == 16 { "little" } else { "n/a" })
                );
                assert_eq!(
                    std::fs::metadata(out.join(name).join(format!("{name}_000000.raw")))
                        .expect("meta")
                        .len(),
                    bytes * u64::from(HEIGHT)
                );
            } else {
                assert!(manifest["raw"].is_null());
            }
            let seen = (hashes(&done), pixels);
            match &reference {
                None => reference = Some(seen),
                Some(first) => {
                    assert_eq!(first.0, seen.0, "{name}: hashes differ from png_fast");
                    assert_eq!(first.1, seen.1, "{name}: decoded pixels differ from png_fast");
                }
            }
        }
        // The levels really are different encodings of the same pixels.
        let size = |name: &str| {
            std::fs::metadata(out.join(name).join(format!("{name}_000000.png")))
                .expect("meta")
                .len()
        };
        assert!(
            size("png_best") <= size("png_fast"),
            "best is not larger than fast"
        );
        let echoed = manifest(out, "png_best")["request"].clone();
        assert_eq!(
            (echoed["png_compression"].clone(), echoed["writers"].clone()),
            (json!("best"), json!(4))
        );
    });
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn png_levels_and_raw_have_the_same_hashes_and_pixels_8_bit() {
    same_frames_every_way("same8", 8);
}

#[test]
fn png_levels_and_raw_have_the_same_hashes_and_pixels_16_bit() {
    same_frames_every_way("same16", 16);
}

#[test]
fn a_cancel_leaves_every_frame_up_to_the_last_delivered_one_and_nothing_else() {
    const FRAMES: u64 = 400;
    let project = run("cancel", |client, out| {
        let started = call(
            client,
            "render.out_start",
            job_params(out, "cut", FRAMES, json!({ "writers": 2 })),
        );
        let job = started["job"].as_u64().expect("job");
        // Wait for a few delivered frames, then cancel mid-run.
        loop {
            let progress = call(client, "render.out_progress", json!({ "job": job }));
            if progress["frame"].as_u64().is_some_and(|f| f >= 3) || progress["status"] != json!("running") {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // The job may already be over on a fast machine; the guarantee holds either way.
        let _ = client.call(
            "render.out_cancel",
            json!({ "job": job, "timeout_ms": CALL_TIMEOUT_MS }),
        );
        let done = wait(client, job);
        let delivered = done["frame"].as_u64().expect("frame");
        assert!(delivered >= 3, "{done}");
        assert_eq!(done["files"], json!(delivered), "{done}");
        assert_eq!(hashes(&done).len() as u64, delivered);
        let manifest = manifest(out, "cut");
        assert_eq!(manifest["delivered"], json!(delivered));
        let mut names: Vec<String> = std::fs::read_dir(out.join("cut"))
            .expect("dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("cut_0") || n.ends_with(".part"))
            .collect();
        names.sort();
        let expected: Vec<String> = (0..delivered).map(|i| format!("cut_{i:06}.png")).collect();
        assert_eq!(
            names, expected,
            "exactly frames 0..{delivered}, in order, no partial file"
        );
        for index in 0..delivered {
            let samples = decode(&out.join("cut").join(format!("cut_{index:06}.png")));
            assert_eq!(
                samples.len(),
                (WIDTH * HEIGHT * 4) as usize,
                "frame {index} decodes whole"
            );
        }
    });
    let _ = std::fs::remove_dir_all(&project);
}

/// Process CPU seconds (user + system) and peak resident memory in MB, from `/proc` (Linux only;
/// `None` elsewhere).
fn process_usage() -> Option<(f64, f64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let after = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after.split_whitespace().collect();
    // Fields 14 and 15 of the line (utime, stime) are at 11 and 12 after the closing parenthesis.
    let ticks: f64 = fields.get(11)?.parse::<f64>().ok()? + fields.get(12)?.parse::<f64>().ok()?;
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: f64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some((ticks / 100.0, kb / 1024.0))
}

/// Benchmark, never run by the gate (`docs/06-testing-and-ci.md` § 9): wall time, CPU time and peak
/// memory per delivered 4K frame for each frame format and PNG level, after a warm-up job, on
/// whatever adapter `REGIA_GPU_INDEX` picks. The scene is trivial, so the numbers are the
/// render-out delivery path (readback, hash, encode, write). Asserts only that every job finished.
#[test]
#[ignore = "benchmark: run by hand, see the ADR-1412 lane report"]
fn perf_frame_formats_4k() {
    const FRAMES: u64 = 12;
    let project = run("perf", |client, out| {
        let size = json!({ "width": 3840, "height": 2160 });
        let jobs: Vec<(&str, Value)> = vec![
            ("warm", json!({})),
            ("png_fast", json!({ "png_compression": "fast" })),
            ("png_fast_1w", json!({ "png_compression": "fast", "writers": 1 })),
            ("png_balanced", json!({ "png_compression": "balanced" })),
            ("png_best", json!({ "png_compression": "best" })),
            ("raw", json!({ "format": "raw" })),
            ("raw16", json!({ "format": "raw", "bit_depth": 16 })),
            ("png16_fast", json!({ "bit_depth": 16 })),
        ];
        for (name, extra) in jobs {
            let mut extra = extra;
            extra
                .as_object_mut()
                .expect("object")
                .extend(size.as_object().expect("object").clone());
            let before = process_usage();
            let started = Instant::now();
            let done = render(client, job_params(out, name, FRAMES, extra));
            let seconds = started.elapsed().as_secs_f64();
            assert_eq!(done["status"], json!("done"), "{name}: {done}");
            let after = process_usage();
            let cpu = before.zip(after).map(|(b, a)| (a.0 - b.0) / seconds);
            println!(
                "perf render_out 4k {name}: {:.0} ms per frame, cpu {} cores, peak {} MB",
                seconds * 1000.0 / FRAMES as f64,
                cpu.map_or("?".into(), |c| format!("{c:.1}")),
                after.map_or("?".into(), |a| format!("{:.0}", a.1)),
            );
            // Keep the disk flat: the files are not needed.
            let _ = std::fs::remove_dir_all(out.join(name));
        }
    });
    let _ = std::fs::remove_dir_all(&project);
}
