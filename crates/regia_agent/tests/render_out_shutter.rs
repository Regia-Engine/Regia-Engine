// SPDX-License-Identifier: MPL-2.0
//! Render-out shutter accumulation and the 16-bit readout (ADR-1404), on lavapipe: two fresh
//! headless runs of the same jobs give the same frame hashes and files; a fast-moving quad is
//! blurred along its motion with `subframes` (the classic frame has hard edges); a 16-bit frame of a
//! shaded gradient agrees with the 8-bit frame to within one 8-bit code and holds more levels.

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
/// Speed of the moving quad, world units per second of game time: about 4 px of streak per
/// 180-degree frame at 30 fps in the 64x48 view (19 px per unit at the camera's distance). Both
/// jobs of a run together cover about 0.2 s, so the quad stays in view.
const SPEED: f32 = 12.0;
const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
/// Size of every job of a run except the perf benchmark's.
const SMALL: (u32, u32) = (WIDTH, HEIGHT);

/// What the scene shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Look {
    /// A flat-lit quad sliding left to right (motion blur).
    Moving,
    /// A still triangle whose vertex normals fan out, so its shading is a smooth ramp (bit depth).
    Gradient,
}

#[derive(Component)]
struct Slider;

fn slide(time: Res<Time>, mut query: Query<(&mut Transform, &mut GlobalTransform), With<Slider>>) {
    let x = -1.2 + SPEED * time.elapsed_secs();
    for (mut transform, mut global) in &mut query {
        transform.translation.x = x;
        global.0 = transform.compute_affine();
    }
}

fn vertex(pos: [f32; 3], normal: [f32; 3]) -> LabVertex {
    LabVertex::from_pos_normal_uv(pos, normal, [0.0, 0.0])
}

fn quad() -> Arc<CpuMesh> {
    let n = [0.0, 0.0, 1.0];
    Arc::new(CpuMesh {
        vertices: vec![
            vertex([-0.3, -0.4, 0.0], n),
            vertex([0.3, -0.4, 0.0], n),
            vertex([0.3, 0.4, 0.0], n),
            vertex([-0.3, 0.4, 0.0], n),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
    })
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

/// Starts a job and polls `render.out_progress` until it is no longer running.
fn render(client: &mut Client, params: Value) -> Value {
    let started = call(client, "render.out_start", params);
    let job = started["job"].as_u64().expect("job id");
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while Instant::now() < deadline {
        let progress = call(
            client,
            "render.out_progress",
            json!({ "job": job, "hashes": true }),
        );
        if progress["status"] != json!("running") {
            assert_eq!(progress["status"], json!("done"), "{progress}");
            return progress;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("render-out job {job} did not finish");
}

fn hashes(progress: &Value) -> Vec<String> {
    progress["hashes"]["hashes"]
        .as_array()
        .expect("hashes")
        .iter()
        .map(|h| h.as_str().expect("hex").to_string())
        .collect()
}

/// A decoded PNG: bit depth and the samples widened to u16.
struct Image {
    depth: png::BitDepth,
    samples: Vec<u16>,
}

fn decode(path: &Path) -> Image {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .expect("png header");
    let depth = reader.info().bit_depth;
    let mut buf = vec![0; reader.output_buffer_size().expect("size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    let bytes = &buf[..info.buffer_size()];
    let samples = match depth {
        png::BitDepth::Sixteen => bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_be_bytes(*b))
            .collect(),
        _ => bytes.iter().map(|b| u16::from(*b)).collect(),
    };
    Image { depth, samples }
}

/// Everything one run produced. `hashes`, `files` and `echoed` are compared across runs;
/// `seconds` (wall time per job, seen by the client) is printed by the benchmark only.
#[derive(Clone, Debug)]
struct Outcome {
    hashes: Vec<Vec<String>>,
    files: Vec<Vec<u8>>,
    echoed: Vec<Value>,
    seconds: Vec<f64>,
}

impl Outcome {
    /// Everything but the timings.
    fn same_output(&self, other: &Self) -> bool {
        self.hashes == other.hashes && self.files == other.files && self.echoed == other.echoed
    }
}

/// How the jobs of one run are made.
#[derive(Clone, Copy)]
struct Shape {
    size: (u32, u32),
    frames: u64,
    /// Read the PNGs back into the outcome (off for the large benchmark frames).
    read_files: bool,
    /// The anti-aliasing of the app's profile.
    aa: AntiAliasing,
}

/// The jobs of one run: a list of `(name, extra params)`.
fn script(client: &mut Client, out: &Path, jobs: &[(&str, Value)], shape: Shape) -> Outcome {
    let Shape {
        size: (width, height),
        frames,
        read_files,
        ..
    } = shape;
    let mut outcome = Outcome {
        hashes: Vec::new(),
        files: Vec::new(),
        echoed: Vec::new(),
        seconds: Vec::new(),
    };
    for (name, extra) in jobs {
        let mut params = json!({
            "out": out.join(name).display().to_string(), "name": name, "frames": frames,
            "width": width, "height": height, "fps": 30, "hud": false,
        });
        params
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        let started = Instant::now();
        let done = render(client, params);
        outcome.seconds.push(started.elapsed().as_secs_f64());
        outcome.hashes.push(hashes(&done));
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(out.join(name).join(format!("{name}_render_out.json"))).expect("manifest"),
        )
        .expect("json");
        outcome.echoed.push(json!({
            "subframes": manifest["request"]["subframes"],
            "shutter_angle": manifest["request"]["shutter_angle"],
            "bit_depth": manifest["request"]["bit_depth"],
        }));
        for index in (0..frames).filter(|_| read_files) {
            let file: PathBuf = out.join(name).join(format!("{name}_{index:06}.png"));
            outcome
                .files
                .push(std::fs::read(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display())));
        }
    }
    outcome
}

/// The binary's one device (ADR-7557), inserted into every app. Without one, the agent opened
/// a device per app at its first capture (`seeing::headless_render_device`, a new device outside
/// regia_agent's own unit tests) and dropped it with the app, so a device was destroyed while
/// other test threads named objects: the Vulkan loader list race of ADR-2113 (this binary FAILED
/// on the Intel Arc in integ-v050rc3 1483c18a7). Every app still has its own renderer, so "fresh
/// run" still means a fresh app and renderer.
fn shared_device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// Whether this host has a headless adapter. Without one the GPU tests skip with a printed line, as they did
/// before the shared device; a run that expects a GPU sets `REGIA_REQUIRE_ADAPTER` (ADR-7613) and then
/// `RenderDevice::shared_headless` panics on its own (regia_render's `require_adapter`), so a lost card
/// stays a failure. Any other device error is still a failure.
fn adapter_available() -> bool {
    match regia_render::RenderDevice::shared_headless() {
        Ok(_) => true,
        Err(regia_render::RenderError::NoAdapter(err)) => {
            // Straight to stderr: libtest's capture only holds `eprintln!` of a passing test.
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "skipped: no headless GPU adapter on this host ({err}); set REGIA_REQUIRE_ADAPTER=1 to fail instead"
            );
            false
        }
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A fresh headless app showing `look`; runs `jobs` through the agent and returns the outcome and
/// the project dir (removed by the caller once the files are read).
fn run(tag: &str, look: Look, jobs: Vec<(&'static str, Value)>, shape: Shape) -> (Outcome, PathBuf) {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-shutter-{tag}"));
    let out = project.join("out");

    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    })
    .insert_resource(shared_device());
    enable_fixed_step(&mut app, 60.0);
    // Paused until a job takes the clock, so nothing depends on the client's speed.
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    app.insert_resource(deterministic_settings(shape.aa));
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 18_000.0,
        double_sided: true,
    });
    app.add_systems(Update, slide);

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
    let mesh = match look {
        Look::Moving => quad(),
        Look::Gradient => fan_triangle(),
    };
    let mut entity = app.world_mut().spawn((
        mesh_tf,
        GlobalTransform(mesh_tf.compute_affine()),
        MeshRenderer::new(mesh, material),
        DirectionalLight {
            direction: [0.0, 0.0, -1.0],
            color: [1.0, 1.0, 1.0],
            illuminance: 60_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));
    if look == Look::Moving {
        entity.insert(Slider);
    }

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let slot = Arc::new(std::sync::Mutex::new(None));
    let result = slot.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script_out = out.clone();
    let thread = std::thread::Builder::new()
        .name(format!("render-out-shutter-{tag}"))
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
            *result.lock().expect("slot") = Some(script(&mut client, &script_out, &jobs, shape));
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !thread.is_finished() && Instant::now() < deadline {
        app.update();
    }
    thread.join().expect("script finished");
    let outcome = slot.lock().expect("slot").take().expect("scenario ran");
    (outcome, project)
}

/// The largest step between horizontally adjacent pixels (sum of the RGB differences) in the
/// middle rows, which cross the quad's left and right edges: the full edge contrast for a hard edge,
/// a fraction of it when the edge is spread over a motion streak. The background (sky or clear
/// colour) changes smoothly, so it never decides the maximum.
fn edge_step(image: &Image) -> u32 {
    let w = WIDTH as usize;
    let mid = HEIGHT as usize / 2;
    let mut steps = Vec::new();
    for y in mid - 2..=mid + 2 {
        let px = |x: usize| &image.samples[(y * w + x) * 4..][..3];
        let step = (0..w - 1)
            .map(|x| {
                px(x)
                    .iter()
                    .zip(px(x + 1))
                    .map(|(a, b)| u32::from(a.abs_diff(*b)))
                    .sum::<u32>()
            })
            .max()
            .unwrap_or(0);
        steps.push(step);
    }
    steps.sort_unstable();
    steps[steps.len() / 2]
}

/// The red channel of the image's middle row.
fn middle_row(image: &Image) -> Vec<u16> {
    let w = WIDTH as usize;
    (0..w)
        .map(|x| image.samples[(HEIGHT as usize / 2 * w + x) * 4])
        .collect()
}

/// Columns strictly between the row's darkest and brightest values (a hard edge has none).
fn ramp_columns(row: &[u16]) -> usize {
    let (low, high) = (row.iter().min(), row.iter().max());
    row.iter().filter(|v| Some(*v) > low && Some(*v) < high).count()
}

/// The first column brighter than the background.
fn lit_column(row: &[u16]) -> Option<usize> {
    let low = row.iter().min();
    row.iter().position(|v| Some(v) > low)
}

fn moving_jobs() -> Vec<(&'static str, Value)> {
    vec![
        ("classic", json!({})),
        ("blur", json!({ "subframes": 6, "shutter_angle": 180 })),
    ]
}

#[test]
fn shutter_frames_blur_motion_and_two_fresh_runs_match() {
    if !adapter_available() {
        return;
    }
    let shape = Shape {
        size: SMALL,
        frames: 3,
        read_files: true,
        aa: AntiAliasing::None,
    };
    let (a, dir_a) = run("blur-a", Look::Moving, moving_jobs(), shape);
    // `loader_list_lock_count` counts every instance and device creation: the second fresh run
    // renders on the binary's one device and creates neither (ADR-7557).
    let creations = regia_render::loader_list_lock_count();
    let (b, dir_b) = run("blur-b", Look::Moving, moving_jobs(), shape);
    assert_eq!(
        regia_render::loader_list_lock_count(),
        creations,
        "the second fresh run created an instance or a device"
    );
    assert_eq!(a.hashes, b.hashes, "frame hashes");
    assert_eq!(a.files, b.files, "PNG bytes");
    assert_eq!(
        a.echoed[1],
        json!({ "subframes": 6, "shutter_angle": 180.0, "bit_depth": 8 })
    );
    assert_eq!(
        a.echoed[0],
        json!({ "subframes": 1, "shutter_angle": 180.0, "bit_depth": 8 })
    );
    let (classic, blur) = (&a.hashes[0], &a.hashes[1]);
    assert_eq!(classic.len(), 3);
    assert_eq!(blur.len(), 3);
    assert!(
        classic.iter().zip(blur).all(|(c, s)| c != s),
        "the shutter changes every frame"
    );
    assert_ne!(blur[0], blur[2], "the quad moved between delivered frames");
    for frame in 0..3usize {
        let classic = decode(&dir_a.join("out/classic").join(format!("classic_{frame:06}.png")));
        let blur = decode(&dir_a.join("out/blur").join(format!("blur_{frame:06}.png")));
        assert_eq!(blur.depth, png::BitDepth::Eight);
        let (hard, soft) = (edge_step(&classic), edge_step(&blur));
        assert!(
            hard >= 150,
            "frame {frame}: the classic edge is a hard step ({hard})"
        );
        // The two jobs show the same frames now (a job's clock starts at zero, ADR-1881), so the
        // blur is measured against the very edge it spreads. The steepest step of the ramp depends
        // on where the subframes fall in a pixel (sRGB makes the dark end steep): what the shutter
        // owes is the ramp itself, 3.8 px on each of the quad's two edges.
        assert!(
            soft * 4 <= hard * 3,
            "frame {frame}: the shutter spreads the edge (largest step {soft} vs {hard} without)"
        );
        let (hard_row, soft_row) = (middle_row(&classic), middle_row(&blur));
        assert_eq!(ramp_columns(&hard_row), 0, "frame {frame}: {hard_row:?}");
        assert!(ramp_columns(&soft_row) >= 4, "frame {frame}: {soft_row:?}");
        assert_eq!(
            lit_column(&hard_row),
            lit_column(&soft_row),
            "frame {frame}: both jobs are at the same game time\n{hard_row:?}\n{soft_row:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

#[test]
fn sixteen_bit_frames_match_the_8_bit_frame_and_hold_more_levels() {
    if !adapter_available() {
        return;
    }
    let jobs = [("eight", json!({})), ("deep", json!({ "bit_depth": 16 }))];
    let shape = Shape {
        size: SMALL,
        frames: 1,
        read_files: true,
        aa: AntiAliasing::None,
    };
    // Two fresh apps, so both jobs render frame 0 at the same game time.
    let (eight, dir_eight) = run("deep-8", Look::Gradient, vec![jobs[0].clone()], shape);
    let (deep, dir_deep) = run("deep-16", Look::Gradient, vec![jobs[1].clone()], shape);
    let (again, dir_again) = run("deep-16b", Look::Gradient, vec![jobs[1].clone()], shape);
    assert!(deep.same_output(&again), "16-bit frames are deterministic");
    assert_eq!(eight.echoed[0]["bit_depth"], json!(8));
    assert_eq!(
        deep.echoed[0],
        json!({ "subframes": 1, "shutter_angle": 180.0, "bit_depth": 16 })
    );
    let lo = decode(&dir_eight.join("out/eight/eight_000000.png"));
    let hi = decode(&dir_deep.join("out/deep/deep_000000.png"));
    assert_eq!(lo.depth, png::BitDepth::Eight);
    assert_eq!(hi.depth, png::BitDepth::Sixteen);
    let mut worst = 0u32;
    for (h, l) in hi.samples.iter().zip(&lo.samples) {
        // 16-bit code / 257 is the same scale as the 8-bit code.
        let diff = (f64::from(*h) / 257.0 - f64::from(*l)).abs();
        worst = worst.max((diff * 100.0).round() as u32);
    }
    assert!(
        worst <= 100,
        "16-bit and 8-bit differ by {} codes",
        f64::from(worst) / 100.0
    );
    let levels = |image: &Image| {
        let mut set: Vec<u16> = image.samples.iter().step_by(4).copied().collect();
        set.sort_unstable();
        set.dedup();
        set.len()
    };
    let (l8, l16) = (levels(&lo), levels(&hi));
    assert!(l16 >= 2 * l8, "16-bit red levels {l16}, 8-bit {l8}");
    for dir in [dir_eight, dir_deep, dir_again] {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Benchmark, never run by the gate (`docs/06-testing-and-ci.md` § 9): wall time per delivered
/// frame of the classic frame, 8 subframes, 16 bits and both, at 1440p and 4K, after two warm-up
/// jobs (pipeline compiles), on whatever adapter
/// `REGIA_GPU_INDEX` picks (lavapipe without a GPU). The scene is trivial, so the numbers are the
/// feature's own overhead (accumulation, readback, PNG) on top of N renders. Prints a table; asserts
/// only that every job finished.
#[test]
#[ignore = "benchmark: run by hand, see the ADR-1404 lane report"]
fn perf_shutter_and_deep_cost() {
    if !adapter_available() {
        return;
    }
    let frames = 4;
    for size in [(2560u32, 1440u32), (3840, 2160)] {
        // The first two jobs only warm up (pipeline compiles for the 8- and 16-bit targets) and
        // are not printed.
        let jobs = vec![
            ("warm8", json!({})),
            ("warm16", json!({ "bit_depth": 16 })),
            ("classic", json!({})),
            ("sub8", json!({ "subframes": 8 })),
            ("deep", json!({ "bit_depth": 16 })),
            ("deep_sub8", json!({ "bit_depth": 16, "subframes": 8 })),
        ];
        let shape = Shape {
            size,
            frames,
            read_files: false,
            aa: AntiAliasing::None,
        };
        let (outcome, dir) = run(&format!("perf-{}", size.0), Look::Moving, jobs.clone(), shape);
        for ((name, _), seconds) in jobs.iter().zip(&outcome.seconds).skip(2) {
            println!(
                "perf render_out {}x{} {name}: {:.1} ms per delivered frame ({frames} frames, {:.2} s)",
                size.0,
                size.1,
                seconds * 1000.0 / frames as f64,
                seconds
            );
        }
        assert_eq!(outcome.hashes.len(), jobs.len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Evidence frames for the lane report, never run by the gate: with
/// `REGIA_RENDER_OUT_EVIDENCE=<dir>` it renders 640x480 frames of the moving quad (classic, 180
/// and 360 degree shutters with 16 subframes) and of the gradient (8 and 16 bit) and copies the
/// PNGs and manifests into `<dir>`. Without the variable it does nothing.
#[test]
#[ignore = "evidence frames: run by hand with REGIA_RENDER_OUT_EVIDENCE=<dir>"]
fn evidence_frames() {
    let Some(dest) = regia_core::brand::env_var_os("REGIA_RENDER_OUT_EVIDENCE").map(PathBuf::from) else {
        return;
    };
    std::fs::create_dir_all(&dest).expect("evidence dir");
    let shape = Shape {
        size: (640, 480),
        frames: 1,
        read_files: false,
        aa: AntiAliasing::None,
    };
    let looks = [
        (
            Look::Moving,
            vec![
                ("classic", json!({})),
                ("blur180", json!({ "subframes": 16 })),
                ("blur360", json!({ "subframes": 16, "shutter_angle": 360 })),
            ],
        ),
        (
            Look::Gradient,
            vec![("eight", json!({})), ("deep", json!({ "bit_depth": 16 }))],
        ),
    ];
    for (look, jobs) in looks {
        let tag = format!("{look:?}").to_lowercase();
        let (_, dir) = run(&format!("evidence-{tag}"), look, jobs.clone(), shape);
        for (name, _) in &jobs {
            let from = dir.join("out").join(name);
            for file in [format!("{name}_000000.png"), format!("{name}_render_out.json")] {
                std::fs::copy(from.join(&file), dest.join(format!("{tag}_{file}")))
                    .unwrap_or_else(|e| panic!("{file}: {e}"));
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A 16-bit frame through every anti-aliasing path of the post stack, against the 8-bit frame of
/// the same scene, time and AA. Where nothing quantizes to 8 bits (FXAA, TAA, MSAA) the 16-bit
/// frame holds more distinct red values than the 8-bit one; SMAA blends in 8 bits by design
/// (ADR-1404), so its 16-bit frame holds at most 256, but it must still render.
#[test]
fn sixteen_bit_frames_render_through_every_anti_aliasing_path() {
    if !adapter_available() {
        return;
    }
    let red_levels = |image: &Image| {
        let mut red: Vec<u16> = image.samples.iter().step_by(4).copied().collect();
        red.sort_unstable();
        red.dedup();
        red.len()
    };
    for aa in [
        AntiAliasing::Fxaa,
        AntiAliasing::Taa,
        AntiAliasing::Msaa4x,
        AntiAliasing::Smaa,
    ] {
        let shape = Shape {
            size: (256, 192),
            frames: 2,
            read_files: true,
            aa,
        };
        let tag = format!("aa-{aa:?}").to_lowercase();
        let mut levels = Vec::new();
        for (name, extra) in [("eight", json!({})), ("deep", json!({ "bit_depth": 16 }))] {
            // A fresh app per depth, so both render frame 1 at the same game time.
            let (outcome, dir) = run(
                &format!("{tag}-{name}"),
                Look::Gradient,
                vec![(name, extra)],
                shape,
            );
            assert_eq!(outcome.hashes[0].len(), 2, "{aa:?} {name}");
            let image = decode(&dir.join("out").join(name).join(format!("{name}_000001.png")));
            levels.push(red_levels(&image));
            let _ = std::fs::remove_dir_all(&dir);
        }
        let (eight, deep) = (levels[0], levels[1]);
        println!("{aa:?}: 8-bit frame {eight} red levels, 16-bit frame {deep}");
        if aa == AntiAliasing::Smaa {
            assert!(deep <= 256, "SMAA blends in 8 bits: {deep} red values");
        } else {
            assert!(deep > eight, "{aa:?}: 16-bit {deep} red levels, 8-bit {eight}");
        }
    }
}
