// SPDX-License-Identifier: MPL-2.0
//! Content recipe steps 3-4 end to end (spec 40 § 8 test 20): over the real agent protocol,
//! `play.wait_until` on the content request reaching `Mounted` through the new `Content`
//! predicate resource, then `level.open` and an offscreen `seeing.screenshot`. Steps 1, 2 and 5
//! of the § 5 recipe (chunks list, set_source + request, inject corrupt) are the
//! `regia_agent::content` unit test `request_progress_and_the_inject_recipe`; this file is the
//! missing middle.
//!
//! What is and is not exercised: the on-demand chunk fetch, verification and mount are real
//! (a `dir` source serving a pak). The arena level itself is not in that pak: this headless
//! app has no cook pipeline, so the level is parsed from a `SceneFile` text and registered in
//! the `LevelLibrary` by hand — it stands in for the startup level a packaged game registers
//! from its install chunk. Step 4 therefore proves `level.open` + `scene.query_entities` +
//! `seeing.screenshot` over the protocol with the chunk mounted, not that the level's bytes
//! travelled through the fetched chunk.
//!
//! The app runs headless with the render plugin (offscreen capture), an asset server to mount
//! into, the scene plugin for `level.open`, and a hand-built `ContentManager` stepped by
//! `content_tick` — no window, no network (a `dir` content source).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_asset::content::{ChunkEntry, ContentManager, ContentManifest, Delivery};
use regia_asset::pak::{FPAK_VERSION, PakToc, encode_pak_header};
use regia_asset::{AssetPlugin, AssetServerSettings};
use regia_core::id::Guid;
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, Environment, LabVertex,
    MeshRenderer, PostSettings, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::format::SCENE_FORMAT_VERSION;
use regia_scene::level::{Level, LevelId, LevelLibrary};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

const GUARD: Duration = Duration::from_secs(120);
const CALL_TIMEOUT_MS: u64 = 90_000;

/// Calls one protocol method, unwrapping errors with the method name in the message.
fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

fn read_token(agent_dir: &Path) -> String {
    std::fs::read_to_string(agent_dir.join("token"))
        .expect("token file")
        .trim()
        .to_owned()
}

/// A valid empty pak (public pak API only): zeroed header, content bytes, a postcard TOC,
/// the BLAKE3 content hash, then the encoded header.
fn write_pak(path: &Path) -> u64 {
    const HEADER_LEN: usize = 60;
    let mut data = vec![0u8; HEADER_LEN];
    data.resize(4096, 0);
    data.extend((0..20_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8));
    let toc_offset = data.len() as u64;
    let toc = PakToc {
        entries: Vec::new(),
        manifest: Vec::new(),
        dependencies: Vec::new(),
    };
    let toc_bytes = postcard::to_allocvec(&toc).expect("toc");
    data.extend_from_slice(&toc_bytes);
    let hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
    let header = encode_pak_header(FPAK_VERSION, 0, toc_offset, toc_bytes.len() as u64, &hash);
    data[..HEADER_LEN].copy_from_slice(&header);
    let len = data.len() as u64;
    std::fs::write(path, &data).expect("write pak");
    len
}

fn chunk_entry(name: &str, dir: &Path, file: &str, delivery: Delivery) -> ChunkEntry {
    ChunkEntry {
        name: name.into(),
        size: std::fs::metadata(dir.join(file)).expect("file").len(),
        hash: regia_asset::content::hash_file(&dir.join(file)).expect("hash"),
        file: file.into(),
        delivery,
        requires: Vec::new(),
        overrides: false,
        mount_order: 0,
        assets: 0,
        blocks: Vec::new(),
    }
}

fn triangle() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-0.95, -0.75, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.95, -0.75, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.0, 0.95, 0.0], [0.0, 0.0, 1.0], [0.5, 1.0]),
        ],
        indices: vec![0, 1, 2],
    })
}

#[test]
fn content_recipe_steps_3_and_4_wait_until_level_open_and_screenshot() {
    let root = regia_test_utils::unique_temp_dir("agent-content-recipe");
    let project = root.join("project");
    let content = root.join("content");
    let cdn = root.join("cdn");
    std::fs::create_dir_all(project.join("assets")).expect("assets");
    std::fs::create_dir_all(project.join(".regia")).expect(".regia");
    std::fs::create_dir_all(&content).expect("content");
    std::fs::create_dir_all(&cdn).expect("cdn");
    write_pak(&content.join("base.fpak"));
    let arena_size = write_pak(&cdn.join("arena.fpak"));

    let mut manifest = ContentManifest::new("1-1-x", "linux");
    manifest
        .chunks
        .push(chunk_entry("base", &content, "base.fpak", Delivery::Install));
    manifest
        .chunks
        .push(chunk_entry("arena", &cdn, "arena.fpak", Delivery::OnDemand));
    let manager = ContentManager::from_manifest(manifest, &content, &root.join("install"));

    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        AssetPlugin {
            settings: AssetServerSettings {
                project_root: project.clone(),
                platform: "linux".to_owned(),
                content_dir: None,
            },
        },
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
        regia_scene::ScenePlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    })
    .insert_resource(manager)
    .add_message::<regia_asset::content::ChunkStateChanged>()
    .add_systems(PreUpdate, regia_asset::content::content_tick);
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.auto_exposure = false;
    settings.profile.bloom = false;
    settings.profile.depth_prepass = false;
    app.insert_resource(settings);
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 18_000.0,
        double_sided: true,
    });
    // The arena level: one named entity, registered before play like a startup level.
    let arena_id = LevelId(Guid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_a4e1));
    {
        let text = format!(
            r#"SceneFile(format_version: {SCENE_FORMAT_VERSION}, entities: [
                (guid: "00000000-0000-0000-0000-0000000000a1", name: "arena_prop", components: {{}}),
            ])"#
        );
        app.world_mut()
            .resource_mut::<LevelLibrary>()
            .insert(Level::from_ron_str(arena_id, &text).expect("level parses"));
    }
    // Something to see in the screenshot (the level entity is bare).
    let camera = Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, 2.6));
    let mesh_tf = Transform::default();
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
    app.world_mut().spawn((
        mesh_tf,
        GlobalTransform(mesh_tf.compute_affine()),
        MeshRenderer::new(
            triangle(),
            Arc::new(CpuMaterial {
                base_color: [0.85, 0.42, 0.12, 1.0],
                roughness: 0.55,
                metallic: 0.0,
                ..CpuMaterial::default()
            }),
        ),
        DirectionalLight {
            direction: [0.15, -0.45, -1.0],
            color: [1.0, 0.96, 0.9],
            illuminance: 90_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));

    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let cdn_for_script = cdn.clone();
    let level_ref = format!("asset:{}", arena_id.0);

    let script = std::thread::Builder::new()
        .name("agent-content-recipe".into())
        .spawn(move || {
            let deadline = Instant::now() + GUARD;
            let mut client = None;
            while Instant::now() < deadline {
                if let Ok((advertised, _token)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &read_token(&agent_dir))
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let mut client = client.expect("connects to the agent");

            // Recipe step 1 (sanity): the arena chunk is listed, not installed.
            let listed = call(&mut client, "content.chunks", json!({}));
            assert_eq!(listed["chunks"][1]["name"], "arena", "{listed}");
            assert_eq!(listed["chunks"][1]["state"], "NotInstalled", "{listed}");

            // Recipe step 2: a dir source and the request, mounting when done.
            call(
                &mut client,
                "content.set_source",
                json!({ "kind": "dir", "path": cdn_for_script.to_string_lossy() }),
            );
            let requested = call(
                &mut client,
                "content.request",
                json!({ "chunks": ["arena"], "mount_when_done": true, "timeout_ms": CALL_TIMEOUT_MS }),
            );
            assert_eq!(requested["total_bytes"], arena_size, "{requested}");

            // Recipe step 3: wait_until the request is Mounted (the Content predicate resource).
            let waited = call(
                &mut client,
                "play.wait_until",
                json!({
                    "condition": { "resource": "Content", "path": "requests.0.state", "op": "eq", "value": "Mounted" },
                    "within_frames": 600,
                    "timeout_ms": CALL_TIMEOUT_MS,
                }),
            );
            assert_eq!(waited["satisfied"], true, "{waited}");
            let progress = call(
                &mut client,
                "content.progress",
                json!({ "request": requested["request"] }),
            );
            assert_eq!(progress["state"], "Mounted", "{progress}");

            // Recipe step 4: open the arena level (registered in the LevelLibrary by hand, like
            // a startup level — the fetched chunk is an empty pak, see the file doc) and take a
            // screenshot.
            let opened = call(
                &mut client,
                "level.open",
                json!({ "level": level_ref, "timeout_ms": CALL_TIMEOUT_MS }),
            );
            assert_eq!(opened["state"], "active", "{opened}");
            assert_eq!(opened["current"], level_ref, "{opened}");
            let members = call(&mut client, "scene.query_entities", json!({ "name": "arena_prop" }));
            assert_eq!(members["count"], 1, "{members}");

            let stepped = call(
                &mut client,
                "play.step",
                json!({ "frames": 3, "timeout_ms": CALL_TIMEOUT_MS }),
            );
            assert!(stepped["frame"].as_u64().is_some(), "{stepped}");
            let shot = call(
                &mut client,
                "seeing.screenshot",
                json!({ "target": "offscreen", "width": 96, "height": 96, "timeout_ms": CALL_TIMEOUT_MS }),
            );
            let bytes = base64_decode(shot["png_base64"].as_str().expect("png_base64")).expect("decode");
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "PNG magic");
            assert!(frame_not_blank(&bytes), "screenshot is blank");

            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + GUARD;
    while !done.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    script.join().expect("script finished");
    assert!(done.load(Ordering::Relaxed), "the recipe did not complete");
    let _ = std::fs::remove_dir_all(&root);
}

/// Decodes the standard-alphabet base64 of the protocol.
fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in bytes {
        let v = match b {
            b'A'..=b'Z' => u32::from(b - b'A'),
            b'a'..=b'z' => u32::from(b - b'a') + 26,
            b'0'..=b'9' => u32::from(b - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return Err(format!("bad base64 byte {b}")),
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

/// Mean/variance over luma: a clear colour plus the triangle is neither black nor uniform.
fn frame_not_blank(png_bytes: &[u8]) -> bool {
    let pixels = decode_rgba(png_bytes);
    let n = pixels.len() / 4;
    if n == 0 {
        return false;
    }
    let mut sum = 0.0f64;
    let mut sum2 = 0.0f64;
    for i in 0..n {
        let y = 0.2126 * pixels[i * 4] as f64
            + 0.7152 * pixels[i * 4 + 1] as f64
            + 0.0722 * pixels[i * 4 + 2] as f64;
        sum += y;
        sum2 += y * y;
    }
    let mean = sum / n as f64;
    let variance = (sum2 / n as f64) - mean * mean;
    (4.0..240.0).contains(&mean) && variance > 1.0
}

fn decode_rgba(png_bytes: &[u8]) -> Vec<u8> {
    use std::io::Cursor;
    let decoder = png::Decoder::new(Cursor::new(png_bytes));
    let mut reader = decoder.read_info().expect("png info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    buf[..info.buffer_size()].to_vec()
}
