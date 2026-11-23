// SPDX-License-Identifier: MPL-2.0
//! Live asset reload end to end (`docs/specs/12-assets.md` § 8): a running app with an
//! [`AssetServer`] and a [`LiveCooker`]; each test edits a source on disk and observes the
//! reloaded value through the *same* handle within one second of the write. Cook errors keep the
//! previous version and land in [`AssetWatchStatus`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use image::ImageEncoder;
use regia_app::prelude::*;
use regia_asset::{
    AssetPlugin, AssetReloadInbox, AssetReloaded, AssetServer, AssetServerSettings, AssetWatchStatus,
    WatchOutcome,
};
use regia_asset_pipeline::{CookOptions, LiveCooker, LiveCookerConfig, cook_project, host_platform};

/// The spec's reload budget, asserted in release builds only (see `assert_reload_budget`).
const DEADLINE: Duration = Duration::from_secs(1);

/// Reloaded ids seen by the app (collected from [`AssetReloaded`]).
#[derive(Resource, Default)]
struct Seen(Vec<AssetReloaded>);

fn collect(mut reader: MessageReader<AssetReloaded>, mut seen: ResMut<Seen>) {
    seen.0.extend(reader.read().cloned());
}

struct Harness {
    root: PathBuf,
    app: App,
    _cooker: LiveCooker,
}

impl Harness {
    fn new(files: &[(&str, Vec<u8>)]) -> Self {
        let root = std::env::temp_dir().join(format!("regia_live_reload_{}", uuid::Uuid::new_v4()));
        for (rel, bytes) in files {
            let path = root.join("assets").join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            std::fs::write(path, bytes).expect("write");
        }
        let report = cook_project(CookOptions::for_host(root.clone())).expect("initial cook");
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let (tx, inbox) = AssetReloadInbox::channel();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        // An inbox before the plugin: the plugin's own dev watchers stay off (the editor's setup).
        app.insert_resource(inbox);
        app.add_plugins(AssetPlugin {
            settings: AssetServerSettings {
                project_root: root.clone(),
                platform: host_platform().to_string(),
                content_dir: None,
            },
        });
        app.init_resource::<Seen>().add_systems(Update, collect);
        let cooker = LiveCooker::spawn(LiveCookerConfig::for_host(root.clone()), tx).expect("live cooker");
        app.update();
        Self {
            root,
            app,
            _cooker: cooker,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join("assets").join(rel)
    }

    fn server(&mut self) -> Mut<'_, AssetServer> {
        self.app.world_mut().resource_mut::<AssetServer>()
    }

    /// Writes `bytes` to `rel`, then updates the app until `done` holds; returns the latency.
    fn edit_until(&mut self, rel: &str, bytes: &[u8], mut done: impl FnMut(&mut App) -> bool) -> Duration {
        let path = self.path(rel);
        let start = Instant::now();
        write_atomically(&path, bytes);
        while start.elapsed() < DEADLINE * 10 {
            self.app.update();
            if done(&mut self.app) {
                return start.elapsed();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("{rel}: no reload within {:?}", DEADLINE * 10);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Write-to-temp + rename, the way most editors save.
fn write_atomically(path: &Path, bytes: &[u8]) {
    let tmp = path.with_file_name(format!(
        ".regia-tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("x")
    ));
    std::fs::write(&tmp, bytes).expect("write tmp");
    std::fs::rename(&tmp, path).expect("rename");
}

#[test]
fn texture_edit_reloads_through_the_same_handle_within_one_second() {
    let mut h = Harness::new(&[("albedo.png", rgba_png(4, 4, [255, 0, 0, 255]))]);
    let id = h.server().id_for_path("assets/albedo.png").expect("id");
    let handle = h.server().load_texture(id).expect("load");
    let before = h.server().textures().get(&handle).expect("texture").ktx2.clone();

    let took = h.edit_until("albedo.png", &rgba_png(4, 4, [0, 255, 0, 255]), |app| {
        let server = app.world().resource::<AssetServer>();
        server.textures().get(&handle).is_some_and(|t| t.ktx2 != before)
    });
    assert_reload_budget(took, "texture reload");
    let seen = &h.app.world().resource::<Seen>().0;
    assert!(
        seen.iter().any(|r| r.id == id),
        "no AssetReloaded for the texture: {seen:?}"
    );
    let status = h.app.world().resource::<AssetWatchStatus>();
    let line = status.last_line().unwrap_or_default();
    assert!(line.starts_with("reloaded assets/albedo.png in "), "{line}");
}

#[test]
fn material_edit_reloads_and_a_broken_file_keeps_the_old_version() {
    let mut h = Harness::new(&[("crate.material.ron", material_ron(0.25).into_bytes())]);
    let id = h.server().id_for_path("assets/crate.material.ron").expect("id");
    let handle = h.server().load_material(id).expect("load");

    let took = h.edit_until("crate.material.ron", material_ron(0.75).as_bytes(), |app| {
        let server = app.world().resource::<AssetServer>();
        server
            .materials()
            .get(&handle)
            .is_some_and(|m| (m.authored.base.base_color[0] - 0.75).abs() < 1e-6)
    });
    assert_reload_budget(took, "material reload");

    // A syntax error: the cook fails, the status says so, the live material is unchanged.
    let errors_before = h.app.world().resource::<AssetWatchStatus>().errors;
    let took = h.edit_until("crate.material.ron", b"( format_version: 3, base: ", |app| {
        app.world().resource::<AssetWatchStatus>().errors > errors_before
    });
    assert_reload_budget(took, "cook error");
    let status = h.app.world().resource::<AssetWatchStatus>();
    let last = status.recent.back().expect("entry");
    assert!(matches!(last.outcome, WatchOutcome::Error(_)), "{last:?}");
    assert!(
        last.toast()
            .starts_with("cook error in assets/crate.material.ron: "),
        "{}",
        last.toast()
    );
    let server = h.app.world().resource::<AssetServer>();
    let base = server
        .materials()
        .get(&handle)
        .expect("still loaded")
        .authored
        .base
        .base_color;
    assert!(
        (base[0] - 0.75).abs() < 1e-6,
        "old material must stay live: {base:?}"
    );
}

#[test]
fn mesh_edit_reloads_through_the_same_handle_within_one_second() {
    let mut h = Harness::new(&[("tri.glb", triangle_glb(1.0))]);
    let id = h.server().id_for_path("assets/tri.glb").expect("id");
    let handle = h.server().load_mesh(id).expect("load");
    let before = h.server().meshes().get(&handle).expect("mesh").positions.clone();

    let took = h.edit_until("tri.glb", &triangle_glb(3.0), |app| {
        let server = app.world().resource::<AssetServer>();
        server
            .meshes()
            .get(&handle)
            .is_some_and(|m| m.positions != before)
    });
    assert_reload_budget(took, "mesh reload");
    let span = h
        .server()
        .meshes()
        .get(&handle)
        .expect("mesh")
        .positions
        .iter()
        .map(|p| p[0].abs().max(p[1].abs()))
        .fold(0.0f32, f32::max);
    assert!(
        (span - 3.0).abs() < 1e-4,
        "reloaded mesh is the edited one: {span}"
    );
}

fn material_ron(red: f32) -> String {
    format!(
        "(\n    format_version: 3,\n    name: \"M_Crate\",\n    shader: Standard,\n    base: (\n        base_color: ({red}, 0.5, 0.5, 1.0),\n        metallic: 0.0,\n        roughness: 0.6,\n    ),\n)\n"
    )
}

fn rgba_png(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
    let pixels: Vec<u8> = pixel
        .iter()
        .copied()
        .cycle()
        .take((width * height * 4) as usize)
        .collect();
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, width, height, image::ExtendedColorType::Rgba8)
        .expect("encode png");
    out
}

/// One triangle of size `s` with POSITION and TEXCOORD_0.
fn triangle_glb(s: f32) -> Vec<u8> {
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0}}],"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0,"TEXCOORD_0":1}}}}]}}],"accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[{s},{s},0]}},{{"bufferView":1,"componentType":5126,"count":3,"type":"VEC2"}}],"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}},{{"buffer":0,"byteOffset":36,"byteLength":24}}],"buffers":[{{"byteLength":60}}]}}"#
    );
    let mut bin = Vec::new();
    for p in [0.0f32, 0.0, 0.0, s, 0.0, 0.0, 0.0, s, 0.0] {
        bin.extend_from_slice(&p.to_le_bytes());
    }
    for uv in [0.0f32, 0.0, 1.0, 0.0, 0.5, 1.0] {
        bin.extend_from_slice(&uv.to_le_bytes());
    }
    let json_bytes = json.as_bytes();
    let json_pad = (4 - json_bytes.len() % 4) % 4;
    let json_len = json_bytes.len() + json_pad;
    let total = 12 + 8 + json_len + 8 + bin.len();
    let mut out = Vec::new();
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_len as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(json_bytes);
    out.extend(std::iter::repeat_n(b' ', json_pad));
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out
}

/// The spec's one-second reload budget, asserted in release builds only: a debug test binary on a
/// loaded machine measures the scheduler, not the reload (audit-v035-flaky H1). Debug runs print it.
fn assert_reload_budget(took: Duration, what: &str) {
    eprintln!("{what} took {took:?}");
    if !cfg!(debug_assertions) {
        assert!(took < DEADLINE, "{what} took {took:?}");
    }
}
