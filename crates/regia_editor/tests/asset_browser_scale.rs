// SPDX-License-Identifier: MPL-2.0
//! The Content Browser at scale (ADR-1329): the grid view asks the thumbnail cache only for the
//! tiles on screen, a large project is searched and refreshed through the agent methods with the
//! index's counters as the cost measure, and an ignored bench times 50 000 assets on disk.
//! No GPU and no clock in the checked tests.

use std::path::Path;

use regia_ecs::prelude::*;
use regia_editor::content::{ContentBrowserState, ContentView, content_browser_panel};
use regia_editor::content_index::ContentIndex;
use regia_editor::thumb_cache::ThumbCache;
use serde_json::{Value, json};

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is installed"));
    handler(world, &params)
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    regia_test_utils::write_atomic(path, bytes).expect("write");
}

fn tiny_png() -> Vec<u8> {
    use image::ImageEncoder as _;
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(
            &[200u8, 40, 40, 255].repeat(16),
            4,
            4,
            image::ExtendedColorType::Rgba8,
        )
        .expect("png encode");
    bytes
}

/// Text drawn in one egui frame.
fn frame_text(ctx: &egui::Context, mut draw: impl FnMut(&mut egui::Ui)) -> String {
    fn walk(shape: &egui::Shape, out: &mut String) {
        match shape {
            egui::Shape::Text(text) => {
                out.push_str(text.galley.text());
                out.push('\n');
            }
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
            _ => {}
        }
    }
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1200.0, 900.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw, |ui| {
        egui::CentralPanel::default().show(ui, |ui| draw(ui));
    });
    output.textures_delta.clear();
    let mut out = String::new();
    for clipped in &output.shapes {
        walk(&clipped.shape, &mut out);
    }
    out
}

fn browser_world(root: &Path, folder: &str) -> (World, ContentBrowserState) {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let mut world = std::mem::take(app.world_mut());
    let open = regia_editor::level_io::new_level(&mut world);
    world.insert_resource(open);
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    let state = ContentBrowserState {
        project_root: Some(root.to_path_buf()),
        folder: folder.to_string(),
        ..Default::default()
    };
    (world, state)
}

fn draw(world: &mut World, state: &mut ContentBrowserState, ctx: &egui::Context) -> String {
    let mut open = world
        .remove_resource::<regia_editor::level_io::OpenLevel>()
        .expect("open level");
    let text = frame_text(ctx, |ui| content_browser_panel(ui, world, state, &mut open));
    world.insert_resource(open);
    text
}

#[test]
fn the_grid_asks_only_for_the_tiles_on_screen() {
    let root = regia_test_utils::unique_temp_dir("asset_browser_grid");
    let png = tiny_png();
    for i in 0..3_000 {
        write(&root.join(format!("assets/tex/t_{i:04}.png")), &png);
    }
    let folder = "assets/tex".to_string(); // the index stores `/` paths on every platform
    let (mut world, mut state) = browser_world(&root, &folder);
    world.insert_resource(ContentView {
        grid: true,
        tile: 96.0,
    });
    let ctx = egui::Context::default();

    let text = draw(&mut world, &mut state, &ctx);
    assert!(
        text.contains("3000 assets"),
        "the header counts the folder:\n{text}"
    );
    let tiles = text.lines().filter(|line| line.starts_with("t_")).count();
    assert!(
        tiles > 0 && tiles < 300,
        "only the rows on screen are drawn: {tiles}"
    );
    let cache = world.resource::<ThumbCache>();
    // One request per tile drawn, wherever it is now: still queued, with the worker, or already
    // answered (egui may run a frame in more than one pass, and each pass pumps the cache).
    let answered = cache.stats().made + cache.stats().disk_hits;
    assert_eq!(
        cache.queued() as u64 + cache.in_flight() as u64 + answered,
        tiles as u64,
        "one request per tile on screen, not 3000"
    );
    let first = cache.stats();
    assert!(first.requests >= tiles as u64, "{first:?}");

    // Let the worker finish what the frame asked for, then draw again: the same tiles come from
    // memory and nothing else was made.
    world.resource_mut::<ThumbCache>().finish();
    let made = world.resource::<ThumbCache>().stats().made;
    assert_eq!(made, tiles as u64, "each tile on screen made once");
    draw(&mut world, &mut state, &ctx);
    let second = world.resource::<ThumbCache>().stats();
    assert!(
        second.memory_hits > 0,
        "the second frame draws from memory: {second:?}"
    );
    assert_eq!(second.made, made, "no new pictures for the same view");
    assert_eq!(
        world.resource::<ContentIndex>().scans,
        1,
        "two frames, one walk of the tree"
    );
    let stored = std::fs::read_dir(regia_editor::thumb_cache::cache_dir(&root))
        .expect("thumbs dir")
        .count() as u64;
    assert_eq!(stored, made, "each made picture is stored once");

    // The agent sees the same cache and its counters.
    let reply = call(
        &mut world,
        "editor.asset_thumbnails",
        json!({ "paths": ["assets/tex/t_0000.png"] }),
    )
    .expect("editor.asset_thumbnails");
    assert_eq!(reply["thumbnails"][0]["state"], "ready", "{reply}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_large_project_is_searched_and_refreshed_by_counters() {
    let root = regia_test_utils::unique_temp_dir("asset_browser_large");
    for folder in 0..50 {
        for item in 0..100 {
            let i = folder * 100 + item;
            write(&root.join(format!("assets/set_{folder:02}/prop_{i:04}.glb")), b"");
        }
    }
    let (mut world, _state) = browser_world(&root, "");

    let reply = call(&mut world, "editor.content_index", json!({ "search": "prop_1" })).expect("index");
    assert_eq!(reply["assets"], 5_000);
    assert_eq!(reply["dirs_read"], 51, "assets/ and its 50 folders");
    assert_eq!(reply["visible"], 1_000, "prop_1000..prop_1999");
    assert_eq!(reply["search_tested"], 5_000);

    let reply = call(&mut world, "editor.content_index", json!({ "search": "prop_12" })).expect("index");
    assert_eq!(reply["visible"], 100);
    assert_eq!(
        reply["search_tested"],
        5_000 + 1_000,
        "the narrowed search tests the last hits"
    );
    assert_eq!(reply["searches_narrowed"], 1);

    let reply = call(
        &mut world,
        "editor.content_index",
        json!({ "search": "Prop_12 TYPE:mesh", "limit": 3 }),
    )
    .expect("index");
    assert_eq!(reply["visible"], 100, "any case; every hit is a mesh");
    assert_eq!(reply["searches_narrowed"], 2);
    let first: Vec<&str> = reply["paths"]
        .as_array()
        .expect("paths")
        .iter()
        .filter_map(|row| row["path"].as_str())
        .collect();
    assert_eq!(first.len(), 3);
    assert!(
        first[0].ends_with("prop_1200.glb"),
        "path order within a rank: {first:?}"
    );

    // A watcher report re-lists one folder, not 51.
    write(&root.join("assets/set_07/added.glb"), b"");
    let relisted =
        regia_editor::content_index::note_changed(&mut world, &[root.join("assets/set_07/added.glb")]);
    assert_eq!(relisted, 1);
    let reply = call(&mut world, "editor.content_index", json!({ "search": "added" })).expect("index");
    assert_eq!(reply["visible"], 1);
    assert_eq!(reply["dirs_read"], 52);
    assert_eq!(reply["scans"], 1, "no second walk");
    // Plain words also match `.meta` fields: each `.meta` is read once, and only the re-listed
    // folder's again (its 100 assets plus the new one).
    assert_eq!(reply["meta_reads"], 5_000 + 101, "{reply}");

    // The editor's own file actions (import, create, move) rescan the tree: the facts of folders
    // whose time did not change are kept, so the next keystroke reads no `.meta` again.
    regia_editor::content_index::invalidate(&mut world);
    let reply = call(&mut world, "editor.content_index", json!({ "search": "prop_1" })).expect("index");
    assert_eq!(reply["scans"], 2, "{reply}");
    assert_eq!(reply["visible"], 1_000);
    assert_eq!(
        reply["meta_reads"],
        5_000 + 101,
        "a rescan with no changed folder reads no .meta: {reply}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Times 50 000 assets on disk. Run by hand (it writes 50 000 files):
/// `cargo test -p regia_editor --test asset_browser_scale -- --ignored --nocapture`.
#[test]
#[ignore = "bench: writes 50 000 files and prints timings"]
fn bench_fifty_thousand_assets_on_disk() {
    use std::time::Instant;
    let root = regia_test_utils::unique_temp_dir("asset_browser_bench");
    let started = Instant::now();
    for i in 0..50_000 {
        let ext = if i % 10 == 0 { "png" } else { "glb" };
        write(
            &root.join(format!("assets/set_{:03}/prop_{i:05}.{ext}", i / 100)),
            b"",
        );
    }
    eprintln!("write 50k files: {:?}", started.elapsed());

    let mut index = ContentIndex::default();
    let started = Instant::now();
    index.ensure(Some(&root));
    eprintln!(
        "full scan: {:?} ({} assets, {} folders)",
        started.elapsed(),
        index.assets().len(),
        index.folders().len()
    );
    let folder = "assets/set_250".to_string();
    let started = Instant::now();
    let view = index.visible(&folder, "");
    eprintln!("folder view ({} assets): {:?}", view.len(), started.elapsed());
    for text in [
        "p",
        "pr",
        "prop_1",
        "prop_12",
        "prop_123",
        "type:texture prop_123",
    ] {
        let started = Instant::now();
        let view = index.visible("", text);
        eprintln!("search {text:?} ({} hits): {:?}", view.len(), started.elapsed());
    }
    let started = Instant::now();
    let stale = index.refresh_stale();
    eprintln!(
        "stamp check, nothing changed (stale {stale}): {:?}",
        started.elapsed()
    );
    write(&root.join("assets/set_300/new.glb"), b"");
    let started = Instant::now();
    let relisted = index.refresh_paths(&[root.join("assets/set_300/new.glb")]);
    eprintln!("refresh one folder ({relisted} listed): {:?}", started.elapsed());
    let mut fresh = ContentIndex::default();
    let started = Instant::now();
    fresh.ensure(Some(&root));
    eprintln!("second full scan (warm cache): {:?}", started.elapsed());
    assert_eq!(
        fresh.assets().len(),
        index.assets().len(),
        "the refresh found the new file"
    );
    let _ = std::fs::remove_dir_all(&root);
}
