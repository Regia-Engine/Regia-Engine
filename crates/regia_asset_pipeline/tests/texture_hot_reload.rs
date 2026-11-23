// SPDX-License-Identifier: MPL-2.0
//! Texture hot reload: rewrite source PNG, recook, observe reloaded asset within 1 s (specs/12 § 8–9).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use image::ImageEncoder;
use regia_asset::hot_reload::spawn_cooked_watcher;
use regia_asset::{AssetServer, AssetServerSettings, ReloadMessage};
use regia_asset_pipeline::{CookOptions, cook_project, host_platform};

#[test]
fn texture_hot_reload_within_one_second() {
    let root = std::env::temp_dir().join(format!("regia_hot_reload_{}", uuid::Uuid::new_v4()));
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    let png_path = assets.join("albedo.png");
    std::fs::write(&png_path, rgba_png(4, 4, [255, 0, 0, 255])).expect("write png");

    let options = CookOptions::for_host(root.clone());
    let report = cook_project(options.clone()).expect("initial cook");
    assert_eq!(report.cooked, 1);

    let platform = host_platform();
    let cache = root.join(".regia").join("cooked").join(platform);
    let (reload_tx, reload_rx) = mpsc::channel();
    let _watcher = spawn_cooked_watcher(&cache, reload_tx).expect("watcher");

    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: platform.to_string(),
        content_dir: None,
    })
    .expect("server");
    let id = server.id_for_path("assets/albedo.png").expect("manifest id");
    let handle = server.load_texture(id).expect("load");
    let before = server.textures().get(&handle).expect("texture").ktx2.clone();

    std::fs::write(&png_path, rgba_png(4, 4, [0, 255, 0, 255])).expect("rewrite png");
    let report2 = cook_project(options).expect("recook");
    assert_eq!(report2.cooked, 1, "expected one asset recooked");

    // A functional deadline, left as soon as the reload lands (audit-v035-flaky H1: the whole wait
    // used to be one second, so a slow watcher thread on a loaded machine failed it).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_reload = false;
    let mut reloaded = false;
    while Instant::now() < deadline && !reloaded {
        while let Ok(ReloadMessage::Reload(rid)) = reload_rx.try_recv() {
            saw_reload = true;
            server.reload_id(rid).expect("reload");
            let handle = server.load_texture(id).expect("load after reload");
            if server.textures().get(&handle).is_some_and(|t| t.ktx2 != before) {
                reloaded = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(saw_reload, "cooked-cache watcher did not notify within 10 s");
    assert!(reloaded, "reloaded texture payload did not change");
    let _ = std::fs::remove_dir_all(&root);
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
