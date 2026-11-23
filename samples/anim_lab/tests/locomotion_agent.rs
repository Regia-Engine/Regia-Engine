// SPDX-License-Identifier: MIT-0
//! Rule 8 dogfooding for the anim_lab locomotion scene (`anim_lab::locomotion`): an agent finds
//! the two walkers, steps the paused scene, asserts on the reflected gait and foot-plant state
//! with `play.expect`, and reads `anim.locomotion_state` for both, over the real agent protocol.
//!
//! `locomotion_agent_dogfood` needs no GPU. `locomotion_agent_screenshot` renders the same scene
//! headless (lavapipe in CI) with boxes for the course, the bodies and the feet, lets the scene
//! run until `play.wait_until` sees the hexapod on its slope and takes an offscreen screenshot;
//! no golden image (the lead blesses one later), the check is on the pixels. A machine without
//! any Vulkan device (not even lavapipe) skips that test with a message, like the other samples'
//! render dogfood tests.
//!
//! The clock advances exactly 1/60 s per frame and the scene is paused while the script steps
//! it, so the frames the script sees do not depend on how fast the machine runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anim_lab::locomotion::{self, HEX_SLOPE, HEX_SPEED, HEX_WALK, HEX_WALK_START};
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::App;
use serde_json::{Value, json};

/// A hang guard only; nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(600);
/// The screenshot's `timeout_ms`: a hang guard for the first offscreen frame on a cold driver
/// cache (see `samples/arena/tests/wc_render_agent.rs` for measured times).
const SHOT_TIMEOUT_MS: u64 = 480_000;
const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;

/// One agent call; retries the rate limiter's "wait a second" (`F0007`), nothing asserts on time.
fn call(client: &mut Client, method: &str, params: Value) -> Value {
    for _ in 0..100 {
        match client.call(method, params.clone()) {
            Ok(value) => return value,
            Err(err) if format!("{err:?}").contains("F0007") => {
                std::thread::sleep(Duration::from_millis(200))
            }
            Err(err) => panic!("{method} failed: {err:?}"),
        }
    }
    panic!("{method}: still rate limited after 100 tries");
}

fn expect(client: &mut Client, name: &str, condition: Value) {
    let report = call(
        client,
        "play.expect",
        json!({ "assertions": [{ "name": name, "condition": condition }] }),
    );
    assert_eq!(report["pass"], true, "{name}: {report}");
}

/// The entity ids of every entity with `component`.
fn entities_with(client: &mut Client, component: &str) -> Vec<u64> {
    let listed = call(
        client,
        "scene.query_entities",
        json!({ "component": component, "limit": 16 }),
    );
    listed["entities"]
        .as_array()
        .map(|rows| rows.iter().filter_map(|row| row["entity"].as_u64()).collect())
        .unwrap_or_default()
}

fn number(value: &Value) -> f64 {
    value.as_f64().unwrap_or_else(|| panic!("not a number: {value}"))
}

/// The frame (counted from the first update) at which the hexapod's mover reaches `s` (s = −z).
fn hexapod_frame_at(s: f32) -> u64 {
    u64::from(HEX_WALK_START) + ((s - HEX_WALK[0]) / HEX_SPEED * 60.0) as u64
}

/// Grants the script more than the default 50 mutating calls a second (specs/23 § 5).
fn project_dir(name: &str) -> PathBuf {
    let project = regia_test_utils::unique_temp_dir(name);
    let config = project.join(".regia");
    std::fs::create_dir_all(&config).expect("agent config dir");
    std::fs::write(config.join("agent.toml"), "rate_limit_per_second = 10000\n").expect("agent.toml");
    project
}

fn connect(project: &Path) -> Client {
    let agent_dir = endpoint::agent_dir(project);
    for _ in 0..3000 {
        if let Ok((advertised, token)) = endpoint::read(&agent_dir)
            && let Ok(connected) = Client::connect(&advertised.url, &token)
        {
            return connected;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("no endpoint at {}", agent_dir.display());
}

/// Runs `script` on a client thread against `app`, updating the app until the script ends.
fn converse(mut app: App, project: PathBuf, name: &str, script: fn(&mut Client)) {
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    })
    .add_plugins(regia_agent::AgentPlugin);
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let paused = Arc::new(AtomicBool::new(false));
    let paused_flag = paused.clone();
    let script_project = project.clone();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut client = connect(&script_project);
            call(
                &mut client,
                "agent.hello",
                json!({ "client": "locomotion-lab", "model": "worker" }),
            );
            call(&mut client, "play.pause", json!({}));
            paused_flag.store(true, Ordering::Relaxed);
            script(&mut client);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("script thread");
    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !thread.is_finished() && Instant::now() < deadline {
        app.update();
        // Until the client has paused the scene, update at about 50 Hz so only a few script
        // frames pass while it connects (the scripts step relative to the frame they find).
        if !paused.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let joined = thread.join();
    let _ = std::fs::remove_dir_all(&project);
    assert!(joined.is_ok(), "the agent script panicked (see above)");
    assert!(done.load(Ordering::Relaxed), "the conversation did not finish");
}

/// The script frame of `walker`'s current pose (`ScriptedWalker.frame`).
fn script_frame(agent: &mut Client, walker: u64) -> u64 {
    let read = call(
        agent,
        "scene.get_entity",
        json!({ "entity": walker, "components": ["ScriptedWalker"] }),
    );
    read["components"]["ScriptedWalker"]["value"]["frame"]
        .as_u64()
        .unwrap_or_else(|| panic!("ScriptedWalker.frame missing: {read}"))
}

/// Steps the paused scene until `walker`'s pose is script frame `target` (no step when it is
/// already there or past it).
fn step_to(agent: &mut Client, walker: u64, target: u64) {
    let now = script_frame(agent, walker);
    if target > now {
        call(agent, "play.step", json!({ "frames": target - now }));
    }
    assert!(script_frame(agent, walker) >= target, "stepped to frame {target}");
}

/// The two walkers: (hexapod, biped).
fn find_walkers(agent: &mut Client) -> (u64, u64) {
    let walkers = entities_with(agent, "ScriptedWalker");
    assert_eq!(walkers.len(), 2, "two scripted walkers: {walkers:?}");
    let rigs = entities_with(agent, "GaitRig");
    assert_eq!(rigs.len(), 1, "one GaitRig: {rigs:?}");
    let planters = entities_with(agent, "FootPlacement");
    assert_eq!(planters.len(), 1, "one FootPlacement: {planters:?}");
    assert!(walkers.contains(&rigs[0]) && walkers.contains(&planters[0]));
    (rigs[0], planters[0])
}

fn dogfood(agent: &mut Client) {
    let (hexapod, biped) = find_walkers(agent);
    let start = script_frame(agent, hexapod);
    assert!(
        start < hexapod_frame_at(1.0),
        "the scene ran {start} frames before the pause"
    );

    // Walk onto the steps: three feet down at least, nothing forced, nothing replanted.
    step_to(agent, hexapod, hexapod_frame_at(2.0));
    for path in ["forced_steps", "replants"] {
        expect(
            agent,
            path,
            json!({ "entity": hexapod, "component": "GaitState", "path": path, "op": "eq", "value": 0 }),
        );
    }
    expect(
        agent,
        "the rig stepped without errors",
        json!({ "entity": hexapod, "component": "GaitRigStatus", "path": "step_errors", "op": "eq", "value": 0 }),
    );
    expect(
        agent,
        "the biped's planted feet hold (under 1 cm of slide)",
        json!({ "entity": biped, "component": "FootPlantReport", "path": "max_slide", "op": "lte", "value": 0.01 }),
    );
    expect(
        agent,
        "the script ran the frames the agent stepped",
        json!({ "entity": hexapod, "component": "ScriptedWalker", "path": "frame", "op": "gte", "value": hexapod_frame_at(2.0) }),
    );

    // The one-call view of each walker.
    let hex = call(agent, "anim.locomotion_state", json!({ "entity": hexapod }));
    eprintln!("hexapod on the steps: {hex}");
    let gait = &hex["gait"];
    assert_eq!(gait["mode"], "driven", "{hex}");
    assert_eq!(gait["legs"], 6, "{hex}");
    assert!(gait["planted_count"].as_u64().is_some_and(|n| n >= 3), "{hex}");
    assert_eq!(gait["forced_steps"], 0, "{hex}");
    assert_eq!(gait["replants"], 0, "{hex}");
    assert!(
        hex["foot_plant"].is_null(),
        "the hexapod has no foot planter: {hex}"
    );
    let feet = gait["feet"].as_array().expect("feet");
    assert_eq!(feet.len(), 6, "{hex}");
    assert!(feet.iter().all(|f| f["mode"] != "hanging"), "{hex}");

    let bip = call(agent, "anim.locomotion_state", json!({ "entity": biped }));
    eprintln!("biped on the stairs: {bip}");
    assert!(bip["gait"].is_null(), "the biped has no gait stepper: {bip}");
    let plant = &bip["foot_plant"];
    assert_eq!(plant["heel_gap"].as_array().map(Vec::len), Some(2), "{bip}");
    assert!(number(&plant["max_slide"]) <= 0.01, "{bip}");
    for leg in 0..2 {
        if number(&plant["planted"][leg]) > 0.5 {
            let (heel, toe) = (number(&plant["heel_gap"][leg]), number(&plant["toe_gap"][leg]));
            assert!((-1.0e-3..=0.01).contains(&heel), "leg {leg} heel {heel}: {bip}");
            assert!((-1.0e-3..=0.01).contains(&toe), "leg {leg} toe {toe}: {bip}");
        }
    }
    assert_eq!(bip["foot_placement"]["probe_installed"], true, "{bip}");
    assert_eq!(bip["foot_placement"]["legs_resolved"], true, "{bip}");
    assert_eq!(bip["foot_placement"]["lod"], "full", "{bip}");

    // Up the slope: the body pitches nose up.
    let middle = 0.5 * (HEX_SLOPE[0] + HEX_SLOPE[1]);
    step_to(agent, hexapod, hexapod_frame_at(middle));
    let hex = call(agent, "anim.locomotion_state", json!({ "entity": hexapod }));
    let pitch = number(&hex["gait"]["attitude"]["pitch"]);
    assert!(pitch > 0.1, "the body pitches on the 10° slope: {hex}");
    assert_eq!(hex["gait"]["forced_steps"], 0, "{hex}");

    // To the end of the script (stop, turn in place, stop): still no forced step, feet settled.
    step_to(agent, hexapod, u64::from(locomotion::FRAMES));
    expect(
        agent,
        "feet settled after the turn",
        json!({ "entity": hexapod, "component": "GaitState", "path": "settled", "op": "eq", "value": true }),
    );
    expect(
        agent,
        "no forced steps over the whole course",
        json!({ "entity": hexapod, "component": "GaitState", "path": "forced_steps", "op": "eq", "value": 0 }),
    );
    // A wrong entity is refused with a code the page lists.
    let err = agent
        .call("anim.locomotion_state", json!({ "entity": 1 }))
        .expect_err("refused");
    let text = format!("{err:?}");
    assert!(
        text.contains("no_such_entity") || text.contains("no_locomotion"),
        "{text}"
    );
}

#[test]
fn locomotion_agent_dogfood() {
    let (app, _scene) = locomotion::headless_app().expect("the locomotion scene");
    converse(
        app,
        project_dir("anim-lab-locomotion-agent"),
        "locomotion-agent",
        dogfood,
    );
}

mod view {
    //! Boxes for the course, the bodies and the feet, so the screenshot shows the walkers.

    use std::sync::Arc;

    use anim_lab::locomotion::{
        BIPED_UP_X, Course, HEX_SLOPE, HEX_SLOPE_DEGREES, HEX_STEP_RISE, HEX_STEP_RUN, HEX_STEPS,
        HEX_STEPS_START, HEXAPOD_X, STAIR_RISE, STAIR_RUN, STAIRS, ScriptedWalker, WalkerKind,
    };
    use regia_animation::gait::GaitState;
    use regia_app::prelude::*;
    use regia_core::math::{Quat, Vec3};
    use regia_render::{Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, LabVertex, MeshRenderer};
    use regia_scene::Transform;

    /// A box that follows a hexapod foot target (`Some(leg)`) or a walker's root.
    #[derive(Component, Clone, Copy)]
    pub struct Marker {
        pub walker: WalkerKind,
        pub foot: Option<usize>,
    }

    fn material(color: [f32; 4]) -> Arc<CpuMaterial> {
        Arc::new(CpuMaterial {
            base_color: color,
            metallic: 0.0,
            roughness: 0.9,
            ..Default::default()
        })
    }

    fn box_mesh(half: Vec3) -> Arc<CpuMesh> {
        const FACES: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
            ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
            ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        ];
        let mut vertices = Vec::with_capacity(24);
        let mut indices: Vec<u32> = Vec::with_capacity(36);
        for (normal, u, v) in FACES {
            let base = vertices.len() as u32;
            let (normal, u, v) = (Vec3::from(normal), Vec3::from(u), Vec3::from(v));
            for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                vertices.push(LabVertex {
                    position: ((normal + u * su + v * sv) * half).to_array(),
                    normal: normal.to_array(),
                    uv: [(su + 1.0) * 0.5, (sv + 1.0) * 0.5],
                    tangent: [u.x, u.y, u.z, 1.0],
                    color: [1.0; 4],
                    joints: [0; 4],
                    weights: [0.0; 4],
                });
            }
            indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        Arc::new(CpuMesh { vertices, indices })
    }

    fn block(world: &mut World, center: Vec3, half: Vec3, rotation: Quat, color: [f32; 4]) {
        world.spawn((
            Transform {
                translation: center,
                rotation,
                scale: Vec3::ONE,
            },
            MeshRenderer::new(box_mesh(half), material(color)),
        ));
    }

    /// The sun, a camera looking up the course from behind, the course and the markers.
    pub fn setup(world: &mut World) {
        world.spawn((
            DirectionalLight {
                direction: [-0.4, -1.0, -0.3],
                color: [1.0, 0.96, 0.9],
                illuminance: 20000.0,
                cast_shadows: false,
                static_shadow: false,
            },
            Transform::default(),
        ));
        world.spawn((
            Transform {
                translation: Vec3::new(0.0, 4.5, 5.0),
                rotation: Quat::from_rotation_x(-0.55),
                scale: Vec3::ONE,
            },
            Camera {
                fov_y: 60.0_f32.to_radians(),
                near: 0.1,
                far: 200.0,
                exposure_ev: Some(12.7),
                clear: ClearMode::Color([0.45, 0.62, 0.85, 1.0]),
                ..Camera::default()
            },
        ));
        let ground = [0.30, 0.32, 0.30, 1.0];
        let step = [0.55, 0.50, 0.42, 1.0];
        // Hexapod half: the flat start, the steps, the landing, the slope and the top.
        let x = HEXAPOD_X;
        block(
            world,
            Vec3::new(x, -0.05, 2.0),
            Vec3::new(1.2, 0.05, 2.0 + HEX_STEPS_START / 2.0),
            Quat::IDENTITY,
            ground,
        );
        for i in 0..HEX_STEPS as u32 {
            let top = HEX_STEP_RISE * (i as f32 + 1.0);
            let s = HEX_STEPS_START + HEX_STEP_RUN * i as f32;
            let end = if i + 1 == HEX_STEPS as u32 {
                HEX_SLOPE[0]
            } else {
                s + HEX_STEP_RUN
            };
            block(
                world,
                Vec3::new(x, top / 2.0, -(s + end) / 2.0),
                Vec3::new(1.2, top / 2.0, (end - s) / 2.0),
                Quat::IDENTITY,
                step,
            );
        }
        let angle = HEX_SLOPE_DEGREES.to_radians();
        let length = (HEX_SLOPE[1] - HEX_SLOPE[0]) / angle.cos();
        let mid_s = 0.5 * (HEX_SLOPE[0] + HEX_SLOPE[1]);
        let mid_y = Course::height(x, -mid_s);
        block(
            world,
            Vec3::new(x, mid_y - 0.05, -mid_s),
            Vec3::new(1.2, 0.05, length / 2.0),
            Quat::from_rotation_x(angle),
            step,
        );
        // Biped half: the floor below the stairs, the stairs, the landing.
        let bx = BIPED_UP_X;
        block(
            world,
            Vec3::new(bx, -0.05, 1.5),
            Vec3::new(1.0, 0.05, 1.5),
            Quat::IDENTITY,
            ground,
        );
        for i in 0..STAIRS as u32 {
            let top = STAIR_RISE * (i as f32 + 1.0);
            let s = STAIR_RUN * i as f32;
            let end = if i + 1 == STAIRS as u32 {
                s + 3.0
            } else {
                s + STAIR_RUN
            };
            block(
                world,
                Vec3::new(bx, top / 2.0, -(s + end) / 2.0),
                Vec3::new(1.0, top / 2.0, (end - s) / 2.0),
                Quat::IDENTITY,
                step,
            );
        }
        let red = material([0.80, 0.15, 0.10, 1.0]);
        let blue = material([0.12, 0.35, 0.80, 1.0]);
        let foot = box_mesh(Vec3::splat(0.06));
        for leg in 0..6 {
            world.spawn((
                Transform::default(),
                MeshRenderer::new(foot.clone(), red.clone()),
                Marker {
                    walker: WalkerKind::Hexapod,
                    foot: Some(leg),
                },
            ));
        }
        world.spawn((
            Transform::default(),
            MeshRenderer::new(box_mesh(Vec3::new(0.3, 0.08, 0.6)), red),
            Marker {
                walker: WalkerKind::Hexapod,
                foot: None,
            },
        ));
        world.spawn((
            Transform::default(),
            MeshRenderer::new(box_mesh(Vec3::new(0.18, 0.5, 0.12)), blue),
            Marker {
                walker: WalkerKind::Biped,
                foot: None,
            },
        ));
    }

    /// Puts each marker on its foot target or body.
    pub fn follow(
        walkers: Query<(&ScriptedWalker, &Transform, Option<&GaitState>), Without<Marker>>,
        mut markers: Query<(&Marker, &mut Transform)>,
    ) {
        for (walker, root, gait) in &walkers {
            for (marker, mut transform) in &mut markers {
                if marker.walker != walker.kind {
                    continue;
                }
                transform.rotation = root.rotation;
                transform.translation = match (marker.foot, gait) {
                    (Some(leg), Some(gait)) => Vec3::from_array(gait.feet[leg].position) + Vec3::Y * 0.06,
                    (None, Some(gait)) => Vec3::from_array(gait.position),
                    _ => root.translation + Vec3::Y * 0.9,
                };
            }
        }
    }
}

fn render_app() -> Result<App, String> {
    let (mut app, _scene) = locomotion::headless_app()?;
    // ScenePlugin propagates transforms (the renderer draws GlobalTransform) and registers them.
    app.add_plugins((
        regia_scene::ScenePlugin,
        regia_render::RenderPlugin { headless: true },
    ))
    .add_systems(regia_app::Startup, view::setup)
    .add_systems(regia_app::PostUpdate, view::follow);
    app.world_mut().spawn(regia_platform::PrimaryWindow);
    Ok(app)
}

/// Standard base64 (the screenshot's `png_base64`) to bytes.
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

/// An offscreen screenshot as RGB pixels; `ANIM_LAB_LOCOMOTION_PNG=<dir>` keeps the file.
fn screenshot(agent: &mut Client, name: &str) -> Vec<[u8; 3]> {
    let shot = call(
        agent,
        "seeing.screenshot",
        json!({ "target": "offscreen", "width": WIDTH, "height": HEIGHT, "hud": false, "timeout_ms": SHOT_TIMEOUT_MS }),
    );
    let bytes = base64_decode(shot["png_base64"].as_str().expect("png_base64"));
    if let Some(dir) = std::env::var_os("ANIM_LAB_LOCOMOTION_PNG") {
        let _ = std::fs::write(Path::new(&dir).join(name), &bytes);
    }
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("a PNG");
    let mut buf = vec![0; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("a frame");
    assert_eq!((info.width, info.height), (WIDTH, HEIGHT));
    let channels = info.color_type.samples();
    buf.truncate(info.buffer_size());
    buf.chunks(channels).map(|p| [p[0], p[1], p[2]]).collect()
}

fn screenshot_script(agent: &mut Client) {
    let (hexapod, _biped) = find_walkers(agent);
    // Step to just before the slope, then let the scene run until `play.wait_until` sees the
    // script reach the slope, and pause there.
    let on_slope = hexapod_frame_at(HEX_SLOPE[0] + 0.5);
    step_to(agent, hexapod, on_slope - 30);
    call(agent, "play.resume", json!({}));
    let waited = call(
        agent,
        "play.wait_until",
        json!({
            "condition": { "entity": hexapod, "component": "ScriptedWalker", "path": "frame", "op": "gte", "value": on_slope },
            "timeout_ms": 120_000
        }),
    );
    call(agent, "play.pause", json!({}));
    assert_eq!(waited["satisfied"], true, "{waited}");
    let pixels = screenshot(agent, "locomotion.png");
    // TODO(lead): bless a golden of this frame on the build server lavapipe reference (it also needs
    // a start-paused hook: a few script frames pass before the first pause).
    let luma: Vec<f64> = pixels
        .iter()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .collect();
    let mean = luma.iter().sum::<f64>() / luma.len() as f64;
    let deviation = (luma.iter().map(|l| (l - mean).powi(2)).sum::<f64>() / luma.len() as f64).sqrt();
    // The course and the camera stand still; only the walkers move. One second later the frame
    // differs where they are.
    call(agent, "play.step", json!({ "frames": 60 }));
    let later = screenshot(agent, "locomotion_later.png");
    let moved = pixels
        .iter()
        .zip(&later)
        .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 24))
        .count();
    eprintln!(
        "screenshot: luma mean {mean:.1}, deviation {deviation:.1}; {moved} of {} pixels changed in 60 frames",
        pixels.len()
    );
    assert!(deviation > 8.0, "a flat frame: luma deviation {deviation}");
    assert!(
        moved > 30,
        "the walkers did not show: only {moved} pixels changed"
    );
    assert!(moved < pixels.len() / 3, "the whole view changed: {moved} pixels");
}

#[test]
fn locomotion_agent_screenshot() {
    if let Err(err) = regia_render::RenderDevice::headless() {
        eprintln!("skipped: no Vulkan device for the offscreen capture ({err})");
        return;
    }
    let app = render_app().expect("the locomotion scene");
    converse(
        app,
        project_dir("anim-lab-locomotion-shot"),
        "locomotion-shot",
        screenshot_script,
    );
}
