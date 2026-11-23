// SPDX-License-Identifier: MPL-2.0
//! Images and masks named by asset path or icon in `.ui.ron` v9 (ADR-3148): background loads, the
//! placeholder, ready state in a fixed number of updates, the same layout as the raw-id form, one
//! warning and the fallback for a broken reference, nine-slice insets, hot reload and the agent
//! method `ui.image_assets`. Headless: the UI renderer queues images without a GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_asset::{
    AssetId, AssetManifest, AssetServer, AssetServerSettings, AssetTypeTag, COOKED_FORMAT_VERSION,
    CookedHeader, CookedImage, ManifestEntry, ReloadMessage, cooked_cache_dir, cooked_file_name,
    encode_cooked_file,
};
use regia_ui::authoring::{UiLibrary, parse_ui_ron, spawn_ui};
use regia_ui::icons::{IconAtlas, IconPage, IconSheet};
use regia_ui::node::NodeName;
use regia_ui::prelude::*;
use regia_ui::render::SharedUiRenderer;
use regia_ui::ui_image_assets::asset_image_id;
use regia_ui::{
    BackgroundKind, UI_IMAGE_FALLBACK, UI_IMAGE_PLACEHOLDER, UiImageAssets, UiImageHandle, UiImageStatus,
    UiMask, UiMaskHandle, UiMigrators, UiScales,
};
use serde_json::json;

fn ui_app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.update();
    app.insert_resource(UiScales {
        window_size: [320.0, 240.0],
        scale_factor: 1.0,
    });
    app
}

fn rgba(width: u32, height: u32, px: [u8; 4]) -> Vec<u8> {
    px.repeat((width * height) as usize)
}

fn cooked_image(id: AssetId, width: u32, height: u32, px: [u8; 4]) -> Vec<u8> {
    let payload = CookedImage {
        width,
        height,
        rgba8: rgba(width, height, px),
    }
    .to_postcard()
    .expect("payload");
    cooked_file(id, AssetTypeTag::image(), &payload)
}

fn cooked_file(id: AssetId, tag: AssetTypeTag, payload: &[u8]) -> Vec<u8> {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: tag.0,
        asset_id: id,
        importer_version: 1_000_000,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, payload).expect("encode")
}

/// A cooked cache with `panel.png` (4x4), `mask.png` (2x1, left half clear) and `block.png` (not
/// an image), and an asset server over it.
struct Project {
    root: std::path::PathBuf,
    cache: std::path::PathBuf,
    panel: AssetId,
}

impl Project {
    fn new() -> (Self, AssetServer) {
        let root = regia_test_utils::unique_temp_dir("ui-image-assets");
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        let panel = AssetId::new_random();
        let mask = AssetId::new_random();
        let block = AssetId::new_random();
        std::fs::write(
            cache.join(cooked_file_name(panel)),
            cooked_image(panel, 4, 4, [255, 0, 0, 255]),
        )
        .expect("panel");
        let mut half = rgba(2, 1, [255, 255, 255, 255]);
        half[3] = 0;
        let mask_payload = CookedImage {
            width: 2,
            height: 1,
            rgba8: half,
        }
        .to_postcard()
        .expect("mask");
        std::fs::write(
            cache.join(cooked_file_name(mask)),
            cooked_file(mask, AssetTypeTag::image(), &mask_payload),
        )
        .expect("mask");
        std::fs::write(
            cache.join(cooked_file_name(block)),
            cooked_file(block, AssetTypeTag::heightfield(), &[]),
        )
        .expect("block");
        let entry = |path: &str, id| ManifestEntry {
            path: path.to_owned(),
            id,
            type_path: "regia_asset::CookedImage".to_owned(),
        };
        AssetManifest {
            entries: vec![
                entry("assets/ui/panel.png", panel),
                entry("assets/ui/mask.png", mask),
                entry("assets/ui/block.png", block),
            ],
        }
        .write_to_cache(&cache)
        .expect("manifest");
        let server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("server");
        (Self { root, cache, panel }, server)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Icons `gear` and `star` (16 px), white.
fn icons(app: &mut App) {
    let page = IconPage {
        width: 32,
        height: 16,
        rgba8: rgba(32, 16, [255, 255, 255, 255]),
    };
    let sheet = IconSheet::from_grid(page, [16, 16], &["gear@16", "star@16"]).expect("sheet");
    app.world_mut()
        .resource_mut::<IconAtlas>()
        .add_sheet(sheet)
        .expect("add");
}

const NAMED: &str = r##"(version: 9, root: (style: (direction: column), children: [
    (name: "logo", style: (size: (64, 32)), image: (asset: "assets/ui/panel.png", tint: "#ff8080ff")),
    (name: "gear", style: (size: (16, 16)), image: (icon: "gear")),
    (name: "group", style: (size: (96, 48)), group_opacity: 0.5, mask: (asset: "assets/ui/mask.png"), children: [
        (name: "inner", style: (size: (40, 20)), background: "#ff0000ff"),
    ]),
    (name: "starred", style: (size: (24, 24)), mask: (icon: "star"), background: "#ffffffff"),
    (name: "frame", style: (size: (80, 40)), image: (asset: "assets/ui/panel.png", nine_slice: (1, 1, 1, 1))),
]))"##;

const RAW: &str = r##"(version: 9, root: (style: (direction: column), children: [
    (name: "logo", style: (size: (64, 32)), image: (id: 5, tint: "#ff8080ff")),
    (name: "gear", style: (size: (16, 16)), image: (id: 6)),
    (name: "group", style: (size: (96, 48)), group_opacity: 0.5, mask: (image: 7), children: [
        (name: "inner", style: (size: (40, 20)), background: "#ff0000ff"),
    ]),
    (name: "starred", style: (size: (24, 24)), mask: (image: 8), background: "#ffffffff"),
    (name: "frame", style: (size: (80, 40)), image: (id: 5, nine_slice: (1, 1, 1, 1))),
]))"##;

fn spawn_doc(app: &mut App, name: &str, text: &str) -> Entity {
    let file = parse_ui_ron(text, name, &UiMigrators::default()).expect("parses");
    app.world_mut()
        .resource_mut::<UiLibrary>()
        .templates
        .insert(name.to_owned(), file);
    spawn_ui(app.world_mut(), name).expect("spawn")
}

/// The named node `name` under `root`.
fn named(app: &mut App, root: Entity, name: &str) -> Entity {
    let mut q = app.world_mut().query::<(Entity, &NodeName)>();
    let found: Vec<Entity> = q
        .iter(app.world())
        .filter(|(_, n)| n.0 == name)
        .map(|(e, _)| e)
        .collect();
    found
        .into_iter()
        .find(|e| {
            let mut cur = Some(*e);
            while let Some(c) = cur {
                if c == root {
                    return true;
                }
                cur = app.world().get::<ChildOf>(c).map(|p| p.0);
            }
            false
        })
        .unwrap_or_else(|| panic!("no node {name}"))
}

fn image_status(app: &App, e: Entity) -> UiImageStatus {
    app.world()
        .get::<UiImageHandle>(e)
        .expect("image handle")
        .0
        .status
}

fn mask_status(app: &App, e: Entity) -> UiImageStatus {
    app.world().get::<UiMaskHandle>(e).expect("mask handle").0.status
}

/// A path image, an atlas icon and a masked group: placeholder first, ready after the loads in
/// exactly two updates, and laid out like the raw-id form of the same document.
#[test]
fn named_pictures_reach_ready_in_two_updates_and_lay_out_like_raw_ids() {
    let (project, server) = Project::new();
    let mut app = ui_app();
    app.insert_resource(server);
    icons(&mut app);
    let named_root = spawn_doc(&mut app, "named", NAMED);
    let raw_root = spawn_doc(&mut app, "raw", RAW);
    let logo = named(&mut app, named_root, "logo");
    let gear = named(&mut app, named_root, "gear");
    let group = named(&mut app, named_root, "group");
    let starred = named(&mut app, named_root, "starred");
    let frame = named(&mut app, named_root, "frame");

    // Spawned with the placeholder, before any update.
    assert_eq!(
        app.world().get::<UiImage>(logo).map(|i| i.id),
        Some(UI_IMAGE_PLACEHOLDER)
    );

    // Update 1: the asset loads start; the icons resolve at once.
    app.update();
    assert_eq!(image_status(&app, logo), UiImageStatus::Loading);
    assert_eq!(
        app.world().get::<UiImage>(logo).map(|i| i.id),
        Some(UI_IMAGE_PLACEHOLDER)
    );
    assert_eq!(mask_status(&app, group), UiImageStatus::Loading);
    assert!(
        app.world().get::<UiMask>(group).is_none(),
        "unmasked until the mask loads"
    );
    assert_eq!(image_status(&app, gear), UiImageStatus::Ready);
    assert_eq!(mask_status(&app, starred), UiImageStatus::Ready);
    let stats = app.world().resource::<UiImageAssets>().stats();
    assert_eq!(
        stats.loads_started, 2,
        "panel.png once for two nodes, mask.png once"
    );

    // The loads finish off the frame; update 2 applies them.
    app.world_mut().resource_mut::<UiImageAssets>().finish_loads();
    app.update();
    for e in [logo, gear, frame] {
        assert_eq!(image_status(&app, e), UiImageStatus::Ready);
    }
    for e in [group, starred] {
        assert_eq!(mask_status(&app, e), UiImageStatus::Ready);
    }
    let panel_image = asset_image_id(project.panel);
    let logo_image = *app.world().get::<UiImage>(logo).expect("image");
    assert_eq!(logo_image.id, panel_image);
    assert_eq!(logo_image.tint, UiColor::from_hex("#ff8080ff").expect("hex"));
    let renderer = app.world().resource::<SharedUiRenderer>().clone();
    assert_eq!(renderer.lock().image_size(panel_image), Some([4, 4]));
    // The icon draws its page and rect; the icon mask too.
    let gear_image = *app.world().get::<UiImage>(gear).expect("icon image");
    let gear_variant = app.world().resource::<IconAtlas>().variants("gear")[0];
    assert_eq!(
        (gear_image.id, gear_image.uv),
        (gear_variant.image, gear_variant.uv)
    );
    let star_mask = *app.world().get::<UiMask>(starred).expect("icon mask");
    let star_variant = app.world().resource::<IconAtlas>().variants("star")[0];
    assert_eq!(
        (star_mask.image, star_mask.uv),
        (star_variant.image, star_variant.uv)
    );
    assert!(app.world().get::<UiMask>(group).is_some(), "the group is masked");
    // Nine-slice: the picture is the node's background with the insets.
    assert!(app.world().get::<UiImage>(frame).is_none());
    match app.world().get::<Background>(frame).map(|b| b.kind.clone()) {
        Some(BackgroundKind::Image { id, nine_slice, .. }) => {
            assert_eq!(id, panel_image);
            assert_eq!(nine_slice, Some([1.0; 4]));
        }
        other => panic!("frame background: {other:?}"),
    }

    // The same layout as the raw-id document.
    for name in ["logo", "gear", "group", "inner", "starred", "frame"] {
        let a = named(&mut app, named_root, name);
        let b = named(&mut app, raw_root, name);
        let ra = app.world().get::<ComputedNode>(a).expect("laid out").rect;
        let rb = app.world().get::<ComputedNode>(b).expect("laid out").rect;
        assert_eq!(ra, rb, "{name}");
    }

    // A steady frame changes nothing and starts nothing.
    app.update();
    app.update();
    assert_eq!(app.world().resource::<UiImageAssets>().stats().loads_started, 2);
    assert_eq!(app.world().resource::<UiImageAssets>().in_flight(), 0);
}

/// A missing path, an asset that is not a picture, an unknown icon and a missing mask: each
/// logs once, draws the fallback (masks: none) and never fails the frame.
#[test]
fn broken_references_log_once_and_draw_the_fallback() {
    let (_project, server) = Project::new();
    let mut app = ui_app();
    app.insert_resource(server);
    icons(&mut app);
    let root = spawn_doc(
        &mut app,
        "broken",
        r#"(version: 9, root: (children: [
            (name: "gone", style: (size: (10, 10)), image: (asset: "assets/ui/gone.png")),
            (name: "block", style: (size: (10, 10)), image: (asset: "assets/ui/block.png")),
            (name: "nope", style: (size: (10, 10)), image: (icon: "nope")),
            (name: "masked", style: (size: (10, 10)), mask: (asset: "assets/ui/gone-mask.png")),
        ]))"#,
    );
    app.update();
    app.world_mut().resource_mut::<UiImageAssets>().finish_loads();
    for _ in 0..10 {
        app.update();
    }
    let expect = [
        ("gone", "ui_image_not_found"),
        ("block", "ui_image_wrong_kind"),
        ("nope", "ui_image_icon_missing"),
    ];
    for (name, code) in expect {
        let e = named(&mut app, root, name);
        let handle = app.world().get::<UiImageHandle>(e).expect("handle").0.clone();
        assert_eq!(handle.status, UiImageStatus::Failed, "{name}");
        assert_eq!(handle.code, Some(code), "{name}");
        assert_eq!(
            app.world().get::<UiImage>(e).map(|i| i.id),
            Some(UI_IMAGE_FALLBACK),
            "{name}"
        );
    }
    let masked = named(&mut app, root, "masked");
    assert_eq!(mask_status(&app, masked), UiImageStatus::Failed);
    assert!(app.world().get::<UiMask>(masked).is_none());
    assert_eq!(
        app.world().resource::<UiImageAssets>().warnings_logged(),
        4,
        "one warning per broken picture, not one per frame"
    );
    // No asset server: a clear code, still no panic.
    let mut bare = ui_app();
    let root = spawn_doc(
        &mut bare,
        "bare",
        r#"(version: 9, root: (children: [(name: "a", image: (asset: "assets/ui/panel.png"))]))"#,
    );
    bare.update();
    let a = named(&mut bare, root, "a");
    assert_eq!(
        bare.world().get::<UiImageHandle>(a).expect("handle").0.code,
        Some("ui_image_no_asset_server")
    );
}

/// A hot reload of the held asset swaps the picture under the same image id.
#[test]
fn hot_reload_swaps_the_image() {
    let (project, server) = Project::new();
    let mut app = ui_app();
    app.insert_resource(server);
    let root = spawn_doc(
        &mut app,
        "reload",
        r#"(version: 9, root: (children: [(name: "a", style: (size: (8, 8)), image: (asset: "assets/ui/panel.png"))]))"#,
    );
    app.update();
    app.world_mut().resource_mut::<UiImageAssets>().finish_loads();
    app.update();
    let a = named(&mut app, root, "a");
    let image = asset_image_id(project.panel);
    let renderer = app.world().resource::<SharedUiRenderer>().clone();
    assert_eq!(renderer.lock().image_size(image), Some([4, 4]));

    // The live cooker rewrites the cooked file and sends a reload.
    let tmp = project.cache.join("panel.tmp");
    std::fs::write(&tmp, cooked_image(project.panel, 8, 2, [0, 255, 0, 255])).expect("new");
    std::fs::rename(&tmp, project.cache.join(cooked_file_name(project.panel))).expect("rename");
    regia_asset::apply_reload_messages(app.world_mut(), vec![ReloadMessage::Reload(project.panel)]);
    app.update();
    assert_eq!(app.world().resource::<UiImageAssets>().stats().reloads, 1);
    app.world_mut().resource_mut::<UiImageAssets>().finish_loads();
    app.update();
    assert_eq!(renderer.lock().image_size(image), Some([8, 2]), "the new pixels");
    let handle = app.world().get::<UiImageHandle>(a).expect("handle").0.clone();
    assert_eq!(handle.status, UiImageStatus::Ready);
    assert_eq!(handle.reloads, 1);
    assert_eq!(
        app.world().get::<UiImage>(a).map(|i| i.id),
        Some(image),
        "same id"
    );
}

/// `ui.image_assets` lists the nodes with their state; a bad filter is a documented error.
#[test]
fn agent_lists_named_pictures() {
    use regia_ui::agent_bridge::call;
    let (_project, server) = Project::new();
    let mut app = ui_app();
    app.insert_resource(server);
    icons(&mut app);
    spawn_doc(&mut app, "named", NAMED);
    app.update();
    app.world_mut().resource_mut::<UiImageAssets>().finish_loads();
    app.update();
    let all = call(app.world_mut(), "ui.image_assets", &json!({})).expect("list");
    let nodes = all["nodes"].as_array().expect("nodes");
    assert_eq!(nodes.len(), 5, "{all}");
    let logo = nodes.iter().find(|n| n["name"] == "logo").expect("logo");
    assert_eq!(logo["kind"], "asset");
    assert_eq!(logo["source"], "assets/ui/panel.png");
    assert_eq!(logo["state"]["status"], "ready");
    assert_eq!(all["loads_started"], 2);
    assert_eq!(all["in_flight"], 0);
    let masks = nodes.iter().filter(|n| n["field"] == "mask").count();
    assert_eq!(masks, 2);
    let failed = call(app.world_mut(), "ui.image_assets", &json!({ "status": "failed" })).expect("filter");
    assert_eq!(failed["nodes"].as_array().map(Vec::len), Some(0));
    let err = call(app.world_mut(), "ui.image_assets", &json!({ "status": "odd" })).expect_err("bad");
    assert_eq!(err.reason, "ui_image_status_unknown");
}
