// SPDX-License-Identifier: MPL-2.0
//! Build a small level through the agent protocol (`editor.*` + `scene.*`) and capture the
//! viewport (`docs/specs/19` § 8 M11). Screenshots are checked as PNG bytes; a person reviews
//! the pictures.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_editor::agent_ext::EditorProject;
use regia_editor::level_io;
use regia_editor::material_editor::render_material_preview;
use regia_editor::pie::{PieRequest, PieSession};
use regia_editor::project;
use regia_editor::render::capture_viewport;
use regia_editor::viewport::{self, EditorCamera};
use regia_scene::level::LEVEL_SETTINGS_PATH;
use regia_scene::spawn::SnapshotFilter;
use serde_json::{Value, json};

/// Liveness bound for the app coming up. Not a performance budget: the run is ordered by [`Gate`],
/// and only an app that never starts fails on this.
const APP_START_LIMIT: Duration = Duration::from_secs(240);

/// `params.timeout_ms` of every agent call: the same kind of bound. A frame blocks for as long as
/// the kernel takes to page this binary in, which the merge gate saw exceed the server's default
/// 30 s (F0006) on a disk under load.
const CALL_TIMEOUT_MS: u64 = 180_000;

struct Gate {
    want: AtomicU8,
    ack: AtomicU8,
    error: Mutex<Option<String>>,
    /// `edit_app` returned, so `AgentPlugin::finish` has advertised the endpoint.
    app_up: AtomicBool,
}

/// The agent client with [`CALL_TIMEOUT_MS`] on every call.
struct Agent(regia_agent::client::Client);

impl Agent {
    fn call(&mut self, method: &str, mut params: Value) -> Result<Value, regia_agent::client::ClientError> {
        params["timeout_ms"] = json!(CALL_TIMEOUT_MS);
        self.0.call(method, params)
    }
}

fn fail(gate: &Gate, message: String) -> ! {
    if let Ok(mut slot) = gate.error.lock() {
        *slot = Some(message.clone());
    }
    panic!("{message}");
}

fn must(gate: &Gate, result: Result<Value, regia_agent::client::ClientError>) -> Value {
    match result {
        Ok(value) => value,
        Err(err) => fail(gate, err.to_string()),
    }
}

fn signal(gate: &Gate, step: u8) {
    gate.want.store(step, Ordering::SeqCst);
    let started = Instant::now();
    while gate.ack.load(Ordering::SeqCst) != step {
        if let Some(message) = gate.error.lock().ok().and_then(|slot| slot.clone()) {
            panic!("{message}");
        }
        if started.elapsed() > Duration::from_secs(120) {
            fail(gate, format!("editor did not acknowledge step {step}"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn edit_app(project_dir: PathBuf) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project_dir),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    app.init_resource::<regia_editor::command::UndoStack>()
        .init_resource::<regia_editor::pie::PieRequests>()
        .init_resource::<regia_editor::pie::PieStatus>()
        .init_resource::<regia_editor::prefab_edit::PrefabEditMode>()
        .insert_resource(EditorProject { root: None });
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.finish_plugins();
    app.update();
    regia_editor::visibility::mark_internal_entities(app.world_mut());
    app
}

fn snapshot_text(app: &mut App) -> String {
    regia_editor::visibility::ensure_scene_identity(app.world_mut());
    let filter = SnapshotFilter {
        level: None,
        resources: vec![LEVEL_SETTINGS_PATH.to_string()],
    };
    let file = regia_scene::prelude::snapshot_scene(app.world_mut(), &filter).expect("snapshot");
    file.to_ron_string().expect("ron")
}

fn drain(app: &mut App, session: &mut Option<PieSession>) {
    let requests: Vec<PieRequest> = std::mem::take(
        &mut app
            .world_mut()
            .resource_mut::<regia_editor::pie::PieRequests>()
            .queue,
    );
    for request in requests {
        let _ = regia_editor::pie::apply_request(app.world_mut(), session, request, None);
    }
    regia_editor::pie::sync_status(app.world_mut(), session.as_ref());
}

fn names_entity(world: &mut World, name: &str) -> Entity {
    let found: Vec<Entity> = world
        .iter_entities()
        .map(|entity| entity.id())
        .filter(|entity| regia_editor::outliner::display_name(world, *entity) == name)
        .collect();
    assert_eq!(found.len(), 1, "one {name}");
    found[0]
}

fn names_of(world: &World) -> Vec<String> {
    // The outliner label covers both name components: scene files store one, `scene.spawn` the other.
    let mut found: Vec<String> = world
        .iter_entities()
        .map(|entity| regia_editor::outliner::display_name(world, entity.id()))
        .collect();
    found.sort();
    found
}

fn shot_dir() -> PathBuf {
    let path = regia_core::brand::env_var("REGIA_M11_SHOTS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("regia-m11-shots"));
    std::fs::create_dir_all(&path).expect("shot dir");
    path
}

/// Writes `image` as PNG and checks the file header.
fn write_png(image: &egui::ColorImage, path: &std::path::Path) {
    let mut bytes = Vec::with_capacity(image.pixels.len() * 4);
    for pixel in &image.pixels {
        bytes.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
    }
    let buffer =
        image::RgbaImage::from_raw(image.width() as u32, image.height() as u32, bytes).expect("size");
    buffer.save(path).expect("png write");
    let stored = std::fs::read(path).expect("png read");
    assert!(stored.starts_with(&[0x89, b'P', b'N', b'G']), "png magic");
    let report = viewport::measure_viewport(image);
    eprintln!(
        "shot {} {}x{} sky={:.3} bias={:.3} peak={:.3} range={:.3} grid={} gizmo={} highlight={}",
        path.display(),
        image.width(),
        image.height(),
        report.sky_luminance,
        report.sky_blue_bias,
        report.lit_peak,
        report.shading_range,
        report.grid_pixels,
        report.gizmo_axes,
        report.selection_highlight,
    );
}

/// Standard base64 (padding optional). The agent image module is crate-private.
fn base64_decode(text: &str) -> Vec<u8> {
    fn value(c: u8) -> u8 {
        match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0xff,
        }
    }
    let bytes: Vec<u8> = text
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let n = chunk
            .iter()
            .fold(0u32, |acc, b| (acc << 6) | u32::from(value(*b)));
        let n = n << (6 * (4 - chunk.len()));
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    out
}

fn decode_png_base64(text: &str) -> egui::ColorImage {
    let bytes = base64_decode(text);
    let img = image::load_from_memory(&bytes).expect("png").to_rgba8();
    egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())
}

/// Standard base64 with padding (RFC 4648 § 4), for the glTF data URI.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A slatted crate as a glTF 2.0 file (positions, normals, UVs, one material): a core box,
/// four corner posts and rims round the top and bottom edges. Written by the test so the level
/// places a real imported mesh, not a primitive.
fn write_crate_gltf(path: &std::path::Path) {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut add_box = |min: [f32; 3], max: [f32; 3]| {
        let faces: [([f32; 3], [usize; 3], f32); 6] = [
            ([1.0, 0.0, 0.0], [0, 2, 1], 1.0),
            ([-1.0, 0.0, 0.0], [0, 2, 1], 0.0),
            ([0.0, 1.0, 0.0], [1, 0, 2], 1.0),
            ([0.0, -1.0, 0.0], [1, 0, 2], 0.0),
            ([0.0, 0.0, 1.0], [2, 0, 1], 1.0),
            ([0.0, 0.0, -1.0], [2, 0, 1], 0.0),
        ];
        for (normal, [n_axis, u_axis, v_axis], side) in faces {
            let base = positions.len() as u32;
            for (u, v) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
                let mut p = [0.0f32; 3];
                p[n_axis] = if side > 0.5 { max[n_axis] } else { min[n_axis] };
                p[u_axis] = min[u_axis] + (max[u_axis] - min[u_axis]) * u;
                p[v_axis] = min[v_axis] + (max[v_axis] - min[v_axis]) * v;
                positions.push(p);
                normals.push(normal);
                uvs.push([u, v]);
            }
            // Counter-clockwise seen from outside along `normal`.
            let e1: Vec<f32> = (0..3)
                .map(|i| positions[base as usize + 1][i] - positions[base as usize][i])
                .collect();
            let e2: Vec<f32> = (0..3)
                .map(|i| positions[base as usize + 2][i] - positions[base as usize][i])
                .collect();
            let cross = [
                e1[1] * e2[2] - e1[2] * e2[1],
                e1[2] * e2[0] - e1[0] * e2[2],
                e1[0] * e2[1] - e1[1] * e2[0],
            ];
            let dot = cross[0] * normal[0] + cross[1] * normal[1] + cross[2] * normal[2];
            if dot >= 0.0 {
                indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
            } else {
                indices.extend_from_slice(&[base, base + 2, base + 1, base, base + 3, base + 2]);
            }
        }
    };
    add_box([-0.44, -0.44, -0.44], [0.44, 0.44, 0.44]);
    for (x, z) in [(-0.43, -0.43), (0.43, -0.43), (-0.43, 0.43), (0.43, 0.43)] {
        add_box([x - 0.07, -0.5, z - 0.07], [x + 0.07, 0.5, z + 0.07]);
    }
    for y in [-0.43f32, 0.43] {
        add_box([-0.5, y - 0.06, -0.5], [0.5, y + 0.06, -0.38]);
        add_box([-0.5, y - 0.06, 0.38], [0.5, y + 0.06, 0.5]);
        add_box([-0.5, y - 0.06, -0.5], [-0.38, y + 0.06, 0.5]);
        add_box([0.38, y - 0.06, -0.5], [0.5, y + 0.06, 0.5]);
    }
    let mut bytes = Vec::new();
    for p in &positions {
        for c in p {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
    }
    let normal_offset = bytes.len();
    for n in &normals {
        for c in n {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
    }
    let uv_offset = bytes.len();
    for uv in &uvs {
        for c in uv {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
    }
    let index_offset = bytes.len();
    for i in &indices {
        bytes.extend_from_slice(&i.to_le_bytes());
    }
    let (count, icount) = (positions.len(), indices.len());
    let gltf = json!({
        "asset": { "version": "2.0", "generator": "regia level_build test" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0, "name": "crate" }],
        "materials": [{ "name": "crate_wood", "pbrMetallicRoughness": { "baseColorFactor": [0.55, 0.38, 0.2, 1.0], "metallicFactor": 0.0, "roughnessFactor": 0.7 } }],
        "meshes": [{ "primitives": [{ "attributes": { "POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2 }, "indices": 3, "material": 0 }] }],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": count, "type": "VEC3", "min": [-0.5, -0.5, -0.5], "max": [0.5, 0.5, 0.5] },
            { "bufferView": 1, "componentType": 5126, "count": count, "type": "VEC3" },
            { "bufferView": 2, "componentType": 5126, "count": count, "type": "VEC2" },
            { "bufferView": 3, "componentType": 5125, "count": icount, "type": "SCALAR" }
        ],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": normal_offset },
            { "buffer": 0, "byteOffset": normal_offset, "byteLength": uv_offset - normal_offset },
            { "buffer": 0, "byteOffset": uv_offset, "byteLength": index_offset - uv_offset },
            { "buffer": 0, "byteOffset": index_offset, "byteLength": bytes.len() - index_offset }
        ],
        "buffers": [{ "byteLength": bytes.len(), "uri": format!("data:application/octet-stream;base64,{}", base64_encode(&bytes)) }]
    });
    std::fs::write(path, serde_json::to_string_pretty(&gltf).expect("gltf json")).expect("gltf write");
}

fn camera_of(world: &World) -> EditorCamera {
    world.get_resource::<EditorCamera>().cloned().unwrap_or_default()
}

/// Mean absolute channel delta, 0..255.
fn mean_abs_delta(a: &egui::ColorImage, b: &egui::ColorImage) -> f64 {
    assert_eq!(
        a.pixels.len(),
        b.pixels.len(),
        "play and edit captures differ in size"
    );
    let mut sum = 0u64;
    for (left, right) in a.pixels.iter().zip(b.pixels.iter()) {
        sum += u64::from(left.r().abs_diff(right.r()));
        sum += u64::from(left.g().abs_diff(right.g()));
        sum += u64::from(left.b().abs_diff(right.b()));
    }
    sum as f64 / (a.pixels.len().max(1) as f64 * 3.0)
}

fn capture_world(device: &regia_render::RenderDevice, world: &mut World) -> egui::ColorImage {
    let camera = camera_of(world);
    capture_viewport(device, world, &camera, [320.0, 240.0]).expect("viewport capture")
}

/// Protocol capture, decoded from the PNG bytes (the JSON measurements are not trusted alone).
fn capture_protocol(gate: &Gate, client: &mut Agent) -> egui::ColorImage {
    let shot = must(
        gate,
        client.call("editor.capture_viewport", json!({ "width": 320, "height": 240 })),
    );
    assert_eq!(shot["mime"], "image/png", "{shot}");
    assert_eq!(shot["taau"], false, "editing must not jitter: {shot}");
    assert_eq!(shot["profile"], "editor", "{shot}");
    let png = shot["png_base64"].as_str().unwrap_or("");
    assert!(!png.is_empty(), "png_base64 is empty: {shot}");
    let image = decode_png_base64(png);
    assert_eq!((image.width(), image.height()), (320, 240));
    image
}

fn run_script(gate: &Gate, parent: &std::path::Path) {
    // The app coming up is the signal, not a time budget: `edit_app` advertises the endpoint before
    // it returns, and nothing else here can tell a slow start from a broken one.
    let started = Instant::now();
    while !gate.app_up.load(Ordering::SeqCst) {
        if started.elapsed() > APP_START_LIMIT {
            fail(gate, format!("edit_app still running after {APP_START_LIMIT:?}"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let agent_dir = regia_agent::endpoint::agent_dir(parent);
    if let Err(err) = regia_agent::endpoint::read(&agent_dir) {
        fail(
            gate,
            format!("no agent endpoint in {}: {err}", agent_dir.display()),
        );
    }
    let mut client = match regia_agent::client::Client::connect_project(parent) {
        Ok(client) => client,
        Err(err) => fail(gate, format!("connect: {err}")),
    };
    // Past the connection's own deadline (`timeout_ms` plus its grace), so an F0006 is what arrives.
    let read_limit = Duration::from_millis(CALL_TIMEOUT_MS) + Duration::from_secs(30);
    if let Err(err) = client.set_read_timeout(Some(read_limit)) {
        fail(gate, format!("read timeout: {err}"));
    }
    let mut client = Agent(client);
    let created = must(
        gate,
        client.call(
            "editor.new_project",
            json!({ "name": "M11 Yard", "path": parent, "template": "blank" }),
        ),
    );
    assert!(created["warning"].is_null(), "startup level: {created}");
    // Level files store `regia_scene::Name`. `scene.query_entities` matches the ECS
    // `Name` that `scene.spawn` inserts, so the blank marker is found by its pose.
    let marker = must(
        gate,
        client.call(
            "scene.query_entities",
            json!({
                "component": "Transform",
                "box": { "min": [-0.2, 0.5, -0.2], "max": [0.2, 1.5, 0.2] }
            }),
        ),
    );
    let marker_id = marker["entities"][0]["entity"].as_u64().unwrap_or(0);
    assert_ne!(marker_id, 0, "blank marker: {marker}");
    must(
        gate,
        client.call("editor.select", json!({ "entities": [marker_id], "mode": "set" })),
    );
    let blank = capture_protocol(gate, &mut client);
    let blank_report = viewport::measure_viewport(&blank);
    assert!(
        blank_report.failures().is_empty(),
        "blank template viewport: {:?} {blank_report:?}",
        blank_report.failures()
    );
    must(
        gate,
        client.call("editor.select", json!({ "entities": [], "mode": "set" })),
    );
    // The template's ground and marker get real meshes (built-in primitives).
    let ground = must(
        gate,
        client.call(
            "scene.query_entities",
            json!({ "component": "Transform", "box": { "min": [-0.2, -0.2, -0.2], "max": [0.2, 0.2, 0.2] } }),
        ),
    );
    let ground_id = ground["entities"][0]["entity"].as_u64().unwrap_or(0);
    assert_ne!(ground_id, 0, "ground: {ground}");
    for (id, mesh) in [(ground_id, "builtin:cube"), (marker_id, "builtin:sphere")] {
        must(
            gate,
            client.call(
                "scene.add_component",
                json!({ "entity": id, "component": "MeshSource", "value": { "mesh": mesh, "material": "" } }),
            ),
        );
    }
    signal(gate, 1);

    let material = must(
        gate,
        client.call(
            "editor.create_asset",
            json!({ "kind": "material", "path": "assets/materials/crate.material.ron" }),
        ),
    );
    let edited = must(
        gate,
        client.call(
            "editor.edit_material",
            json!({
                "path": material["path"].as_str().unwrap_or("assets/materials/crate.material.ron"),
                "base_color": [0.85, 0.35, 0.10, 1.0],
                "metallic": 0.05,
                "roughness": 0.55,
            }),
        ),
    );
    // Colours are stored as f32; JSON numbers come back as f64, so compare within an ulp budget.
    let close = |value: &Value, expected: f32| {
        value
            .as_f64()
            .is_some_and(|got| (got - f64::from(expected)).abs() < 1e-5)
    };
    assert!(
        close(&edited["base_color"][0], 0.85)
            && close(&edited["base_color"][1], 0.35)
            && close(&edited["base_color"][2], 0.10)
            && close(&edited["base_color"][3], 1.0)
            && close(&edited["metallic"], 0.05)
            && close(&edited["roughness"], 0.55),
        "{edited}"
    );

    // A real mesh: a crate glTF written outside the project, imported through the editor.
    let source_dir = parent.join("sources");
    std::fs::create_dir_all(&source_dir).expect("sources dir");
    let crate_source = source_dir.join("crate.gltf");
    write_crate_gltf(&crate_source);
    let imported = must(
        gate,
        client.call(
            "editor.import",
            json!({ "sources": [crate_source.display().to_string()], "folder": "props" }),
        ),
    );
    assert_eq!(imported["imported"][0], "assets/props/crate.gltf", "{imported}");
    let viewed = must(
        gate,
        client.call(
            "editor.view_asset",
            json!({ "path": "assets/props/crate.gltf", "width": 256, "height": 256 }),
        ),
    );
    assert_eq!(viewed["info"]["kind"], "mesh", "{viewed}");
    assert!(
        viewed["info"]["triangles"].as_u64().unwrap_or(0) >= 12 * 13,
        "{viewed}"
    );
    let crate_mesh =
        json!({ "mesh": "assets/props/crate.gltf", "material": "assets/materials/crate.material.ron" });
    must(
        gate,
        client.call(
            "editor.create_asset",
            json!({ "kind": "prefab", "path": "assets/props/crate.prefab.ron" }),
        ),
    );
    must(
        gate,
        client.call(
            "editor.open_prefab",
            json!({ "path": "assets/props/crate.prefab.ron" }),
        ),
    );
    let queried = must(
        gate,
        client.call("scene.query_entities", json!({ "name": "crate" })),
    );
    let entity = queried["entities"][0]["entity"].as_u64().unwrap_or(0);
    assert_ne!(entity, 0, "prefab root: {queried}");
    must(
        gate,
        client.call(
            "scene.add_component",
            json!({ "entity": entity, "component": "MeshSource", "value": crate_mesh }),
        ),
    );
    must(
        gate,
        client.call(
            "editor.set_transform",
            json!({ "entity": entity, "scale": [1.4, 1.4, 1.4] }),
        ),
    );
    must(
        gate,
        client.call("editor.select", json!({ "entities": [entity], "mode": "set" })),
    );
    let prefab = capture_protocol(gate, &mut client);
    let prefab_report = viewport::measure_viewport(&prefab);
    assert!(
        prefab_report.sky_visible() && prefab_report.shading_visible(),
        "prefab edit viewport: {prefab_report:?}"
    );
    let saved = must(gate, client.call("editor.save_prefab", json!({})));
    assert_eq!(saved["saved"], true, "{saved}");
    must(gate, client.call("editor.close_prefab", json!({ "save": false })));
    must(
        gate,
        client.call(
            "editor.spawn_prefab",
            json!({ "path": "assets/props/crate.prefab.ron", "translation": [1.5, 0.5, 0.0] }),
        ),
    );
    for (name, position) in [("CrateA", [0.0, 0.6, 1.5]), ("CrateB", [-1.6, 0.6, 0.4])] {
        must(
            gate,
            client.call(
                "scene.spawn",
                json!({
                    "name": name,
                    "components": {
                        "Transform": {
                            "translation": position,
                            "rotation": [0.0, 0.0, 0.0, 1.0],
                            "scale": [1.0, 1.0, 1.0],
                        },
                        "MeshSource": crate_mesh,
                    }
                }),
            ),
        );
    }
    for (name, position, scale) in [
        ("KeyLight", [3.0, 4.0, 2.0], [0.4, 0.4, 0.4]),
        ("Camera", [0.0, 2.0, 6.0], [0.3, 0.3, 0.3]),
    ] {
        let spawned = must(
            gate,
            client.call(
                "scene.spawn",
                json!({
                    "name": name,
                    "components": {
                        "Transform": {
                            "translation": position,
                            "rotation": [0.0, 0.0, 0.0, 1.0],
                            "scale": scale,
                        }
                    }
                }),
            ),
        );
        // The light and the camera are real components (lit by, and possessable in play).
        let id = spawned["entity"].as_u64().unwrap_or(0);
        assert_ne!(id, 0, "{spawned}");
        let component = if name == "KeyLight" {
            "PointLight"
        } else {
            "Camera"
        };
        must(
            gate,
            client.call(
                "scene.add_component",
                json!({ "entity": id, "component": component }),
            ),
        );
    }
    let assets = must(gate, client.call("editor.list_assets", json!({})));
    let listed = assets["assets"].as_array().cloned().unwrap_or_default();
    assert!(
        listed
            .iter()
            .any(|entry| entry["path"] == "assets/materials/crate.material.ron"),
        "{assets}"
    );
    must(
        gate,
        client.call("editor.select", json!({ "entities": [], "mode": "set" })),
    );
    signal(gate, 2);
    let crate_a = must(
        gate,
        client.call("scene.query_entities", json!({ "name": "CrateA" })),
    );
    let crate_id = crate_a["entities"][0]["entity"].as_u64().unwrap_or(0);
    assert_ne!(crate_id, 0, "CrateA: {crate_a}");
    must(
        gate,
        client.call("editor.select", json!({ "entities": [crate_id], "mode": "set" })),
    );
    let edit = capture_protocol(gate, &mut client);
    let edit_report = viewport::measure_viewport(&edit);
    assert!(
        edit_report.failures().is_empty(),
        "built level viewport: {:?} {edit_report:?}",
        edit_report.failures()
    );
    write_png(&edit, &shot_dir().join("edit.png"));
    must(gate, client.call("editor.play", json!({})));
    signal(gate, 3);
    let state = must(gate, client.call("editor.play_state", json!({})));
    assert_eq!(state["mode"], "playing", "{state}");
    // No wall-clock bound here: `start_elapsed_ms` measures real time, which parallel test load
    // (cargo test --workspace) inflates arbitrarily — the one observed flake. The PIE-start
    // budget is gated where docs/06 § 4 puts it, on reference hardware through the budget
    // benches; here we assert play mode actually reached a settled frame.
    let frames = state["frames"].as_u64().unwrap_or(0);
    assert!(frames >= 1, "PIE has not run a settled frame: {state}");
    eprintln!(
        "level PIE start_elapsed_ms={}",
        state["start_elapsed_ms"].as_u64().unwrap_or(0)
    );
    must(gate, client.call("editor.stop", json!({})));
    signal(gate, 4);
    signal(gate, 9);
}

#[test]
fn agent_protocol_builds_a_level_and_plays_it() {
    let parent = std::env::temp_dir().join(format!("regia-m11-level-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir_all(&parent).expect("parent");
    project::set_recent_file_for_tests(parent.join("recents.txt"));

    let gate = Arc::new(Gate {
        want: AtomicU8::new(0),
        ack: AtomicU8::new(0),
        error: Mutex::new(None),
        app_up: AtomicBool::new(false),
    });
    let script_gate = Arc::clone(&gate);
    let script_parent = parent.clone();
    std::thread::spawn(move || {
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_script(&script_gate, &script_parent);
        }));
        if let Err(payload) = caught {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_string()))
                .unwrap_or_else(|| "agent script panicked".to_string());
            if let Ok(mut slot) = script_gate.error.lock()
                && slot.is_none()
            {
                *slot = Some(message);
            }
        }
    });

    let mut app = edit_app(parent.clone());
    gate.app_up.store(true, Ordering::SeqCst);
    let mut session = None;
    let mut before = String::new();
    let mut neutral: Option<egui::ColorImage> = None;
    let mut device_slot: Option<regia_render::RenderDevice> = None;
    let shots = shot_dir();
    let started = Instant::now();
    while gate.ack.load(Ordering::SeqCst) != 9 {
        if let Some(message) = gate.error.lock().ok().and_then(|slot| slot.clone()) {
            panic!("{message}");
        }
        if started.elapsed() > Duration::from_secs(180) {
            panic!(
                "agent level build timed out at step {}",
                gate.want.load(Ordering::SeqCst)
            );
        }
        app.update();
        let want = gate.want.load(Ordering::SeqCst);
        if want != gate.ack.load(Ordering::SeqCst) {
            match want {
                2 => {
                    let found = names_of(app.world_mut());
                    for required in [
                        "Ground", "Marker", "crate", "CrateA", "CrateB", "KeyLight", "Camera",
                    ] {
                        assert!(
                            found.iter().any(|name| name == required),
                            "missing {required} in {found:?}"
                        );
                    }
                    before = snapshot_text(&mut app);
                    let device = app
                        .world()
                        .get_resource::<regia_render::RenderDevice>()
                        .cloned()
                        .or_else(|| regia_render::RenderDevice::headless().ok())
                        .expect("headless GPU (lavapipe)");
                    if app.world().get_resource::<regia_render::RenderDevice>().is_none() {
                        app.world_mut().insert_resource(device.clone());
                    }
                    let image = capture_world(&device, app.world_mut());
                    let report = viewport::measure_viewport(&image);
                    // The level renders its real meshes through the runtime path: ground, marker,
                    // the prefab crate and the two placed crates; the light is a real light.
                    let camera = camera_of(app.world());
                    let frame = regia_editor::render::render_pane(
                        &device,
                        app.world_mut(),
                        &regia_editor::render::PaneRequest {
                            pane: 0,
                            kind: regia_editor::views::ViewKind::Perspective,
                            camera,
                            width: 320,
                            height: 240,
                            ids: true,
                            warm_frames: 0,
                            debug_view: regia_render::DebugViewMode::Lit,
                            reference: None,
                        },
                    )
                    .expect("pane render");
                    eprintln!("built level pane: {:?}", frame.info);
                    assert!(frame.info.meshes >= 5, "real meshes: {:?}", frame.info);
                    assert_eq!(frame.info.local_lights, 1, "KeyLight is a point light");
                    let crate_a = names_entity(app.world_mut(), "CrateA");
                    let size = regia_core::math::Vec2::new(320.0, 240.0);
                    let crate_px = frame
                        .view
                        .project(regia_core::math::Vec3::new(0.0, 0.6, 1.5), size)
                        .expect("CrateA on screen");
                    assert_eq!(
                        viewport::pick_in_frame(app.world(), &frame, crate_px).map(|hit| hit.0),
                        Some(crate_a),
                        "GPU pick selects the crate mesh"
                    );
                    write_png(&frame.image, &shots.join("edit_raw.png"));
                    assert!(report.sky_visible(), "neutral sky: {report:?}");
                    assert!(report.shading_visible(), "neutral shading: {report:?}");
                    assert!(report.grid_visible(), "neutral grid: {report:?}");
                    assert!(
                        !report.selection_highlight,
                        "neutral has no selection: {report:?}"
                    );
                    neutral = Some(image);
                    device_slot = Some(device);
                }
                3 => {
                    drain(&mut app, &mut session);
                    let device = device_slot.clone().expect("device from the edit capture");
                    let play_world = session
                        .as_mut()
                        .map(|session| session.app.world_mut())
                        .expect("play session");
                    // Ejected: the editor camera over the play world.
                    let mut layout = play_world
                        .get_resource::<regia_editor::views::ViewportLayout>()
                        .cloned()
                        .unwrap_or_default();
                    layout.possessed = false;
                    play_world.insert_resource(layout.clone());
                    let play = capture_world(&device, play_world);
                    let report = viewport::measure_viewport(&play);
                    assert!(report.sky_visible(), "play sky: {report:?}");
                    assert!(report.shading_visible(), "play shading: {report:?}");
                    assert!(report.grid_visible(), "play grid: {report:?}");
                    assert_eq!(report.gizmo_axes, 0, "play has no selection gizmo: {report:?}");
                    let delta = mean_abs_delta(neutral.as_ref().expect("neutral edit capture"), &play);
                    eprintln!("play vs edit mean abs channel delta={delta:.3}");
                    assert!(
                        delta < 1.0,
                        "play renders the edit world from the same camera, delta {delta:.3}"
                    );
                    write_png(&play, &shots.join("play.png"));
                    // Possessed: the level's camera, the game's picture only.
                    let play_world = session
                        .as_mut()
                        .map(|session| session.app.world_mut())
                        .expect("play session");
                    layout.possessed = true;
                    play_world.insert_resource(layout);
                    let possessed = capture_world(&device, play_world);
                    let possessed_report = viewport::measure_viewport(&possessed);
                    assert_eq!(possessed_report.grid_pixels, 0, "possessed: no grid");
                    let moved = mean_abs_delta(&play, &possessed);
                    assert!(
                        moved > 2.0,
                        "possession looks through the level camera: {moved:.3}"
                    );
                    write_png(&possessed, &shots.join("play_possessed.png"));
                    let play_names = session.as_mut().map(|session| names_of(session.app.world_mut()));
                    let play_names = play_names.unwrap_or_default();
                    assert!(
                        play_names.iter().any(|name| name == "CrateA"),
                        "play shows the placed crate: {play_names:?}"
                    );
                }
                4 => {
                    drain(&mut app, &mut session);
                    assert!(session.is_none(), "stop drops the play world");
                    let after = snapshot_text(&mut app);
                    assert_eq!(after, before, "stop restores the edit world byte for byte");
                }
                _ => {}
            }
            gate.ack.store(want, Ordering::SeqCst);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let device = device_slot.expect("GPU device from the edit capture");
    let preview = render_material_preview(
        &device,
        &regia_asset::MaterialAssetRon::default_standard(),
        [320.0, 240.0],
    )
    .expect("material preview");
    let preview_report = viewport::measure_viewport(&preview);
    assert!(
        preview_report.sky_visible(),
        "material preview sky: {preview_report:?}"
    );
    assert!(
        preview_report.shading_visible(),
        "material preview shading: {preview_report:?}"
    );
    write_png(&preview, &shots.join("material_preview.png"));
    eprintln!("m11 shots in {}", shots.display());
    let _ = std::fs::remove_dir_all(&parent);
}
