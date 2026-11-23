// SPDX-License-Identifier: MIT-0
//! The host's real frame function on a real cooked package and the real renderer, offscreen (lavapipe in
//! CI): the app draws what changed and nothing else, picks change the picture without a new pipeline, a
//! refused pick changes nothing, the panel reaches the screen, and a package that is refused on open leaves
//! the app showing the refusal and usable.

use std::path::Path;

use product_viewer::app::{PanelUi, ViewerConfig, ViewerCore, ViewerState, frame};
use product_viewer::stats::{ACTIVE_WAKES_PER_SECOND, IDLE_WAKES_PER_SECOND};
use product_viewer::surface::{Capture, SurfaceHost};
use product_viewer::viewer_api;
use regia_platform::SafeArea;
use regia_test_utils::unique_temp_dir;
use regia_viewer_pack::standin;
use serde_json::{Value, json};

const SIZE: (u32, u32) = (360, 720);

struct Rig {
    state: ViewerState,
    panel: PanelUi,
    core: ViewerCore,
    dir: std::path::PathBuf,
}

/// Cooks the sold stand-in into `store_dir/slug`.
fn cook_into(work: &Path, store_dir: &Path, slug: &str) {
    let set = standin::write(work).expect("stand-ins");
    let options = regia_viewer_pack::cook::CookOptions {
        materials: Some(set.library.clone()),
        option_graph: set.option_graphs.first().cloned(),
        ..Default::default()
    };
    regia_viewer_pack::cook::pack(&set.kit_dir, &set.recipes[0], &store_dir.join(slug), &options)
        .expect("cook");
}

/// Cooks a stand-in whose option record is not for sale: it cooks and verifies, and the player refuses it
/// (`product_not_orderable`).
fn cook_unsold_into(work: &Path, store_dir: &Path, slug: &str) {
    let set = standin::write(work).expect("stand-ins");
    let record = set.option_graphs.first().expect("an option record");
    let mut graph: Value = serde_json::from_slice(&std::fs::read(record).expect("read")).expect("json");
    graph["status"] = json!("not_orderable");
    let unsold = work.join("unsold-option-record.json");
    std::fs::write(&unsold, serde_json::to_vec(&graph).expect("json")).expect("write");
    let options = regia_viewer_pack::cook::CookOptions {
        materials: Some(set.library.clone()),
        option_graph: Some(unsold),
        ..Default::default()
    };
    regia_viewer_pack::cook::pack(&set.kit_dir, &set.recipes[0], &store_dir.join(slug), &options)
        .expect("cook");
}

/// A store of three packages: `piece` opens, `unsold` is refused by the player, `tampered` by the store.
fn refusal_store(dir: &Path) {
    let store = dir.join("store");
    cook_into(&dir.join("work-piece"), &store, "piece");
    cook_unsold_into(&dir.join("work-unsold"), &store, "unsold");
    cook_into(&dir.join("work-tampered"), &store, "tampered");
    let mesh = store.join("tampered/mesh.rvm");
    let mut bytes = std::fs::read(&mesh).expect("read");
    bytes[10] ^= 0xff;
    std::fs::write(&mesh, bytes).expect("write");
}

impl Rig {
    /// A running app over the store `fill` writes, with nothing open.
    fn over_store(tag: &str, fill: impl FnOnce(&Path)) -> Option<Self> {
        let Some(host) = SurfaceHost::headless(SIZE) else {
            eprintln!("no GPU adapter on this machine: skipped");
            return None;
        };
        eprintln!("adapter: {}", host.adapter().get_info().name);
        let dir = unique_temp_dir(tag);
        fill(&dir);
        let config = ViewerConfig {
            package_roots: vec![dir.join("store")],
            ..ViewerConfig::default()
        };
        let state = ViewerState::new(&config);
        let mut core = ViewerCore::new();
        core.attach(host).expect("attach");
        Some(Self {
            state,
            panel: PanelUi::default(),
            core,
            dir,
        })
    }

    fn new(tag: &str) -> Option<Self> {
        let mut rig = Self::over_store(tag, |dir| {
            cook_into(&dir.join("work"), &dir.join("store"), "piece");
        })?;
        rig.open("piece");
        Some(rig)
    }

    /// Opens a package the way the panel does and lets the background task finish.
    fn open(&mut self, slug: &str) {
        self.state.start_open(slug).expect("open");
        self.finish_task();
    }

    /// The background read finishes on its own schedule; frames idle until it does.
    fn finish_task(&mut self) {
        while self.state.task_running() {
            self.tick();
            std::thread::yield_now();
        }
        self.tick();
    }

    /// The frame the app last drew by itself, with nothing asking for a capture: offscreen, every frame that
    /// would reach the screen is read back. `None` means nothing would be on the screen.
    fn drawn_by_itself(&mut self) -> Option<Capture> {
        self.settle();
        self.core.last_capture.take()
    }

    /// Taps across the panel until `done` holds. Returns whether it did.
    fn tap_panel_until(&mut self, done: impl Fn(&Self) -> bool) -> bool {
        let panel = self.panel.hit_rect_px(2.0);
        let mut x = 30.0;
        while x < SIZE.0 as f32 {
            let mut y = panel.min.y + 8.0;
            while y < panel.max.y {
                let tap = json!({"events": [
                    {"id": 3, "phase": "down", "x": x, "y": y, "at_ms": 0.0},
                    {"id": 3, "phase": "up", "x": x, "y": y, "at_ms": 30.0},
                ]});
                product_viewer::viewer_api::touch(&mut self.state, panel, 2.0, &tap).expect("touch");
                self.tick();
                self.tick();
                self.finish_task();
                if done(self) {
                    return true;
                }
                y += 16.0;
            }
            x += 30.0;
        }
        false
    }

    fn tick(&mut self) {
        frame(
            &mut self.state,
            &mut self.panel,
            &mut self.core,
            SIZE,
            2.0,
            &SafeArea::default(),
        );
    }

    fn settle(&mut self) {
        for _ in 0..2000 {
            self.tick();
            if self.state.stats.wakes_per_second == IDLE_WAKES_PER_SECOND {
                return;
            }
        }
        panic!("the app never came to rest: {:?}", self.state.stats);
    }

    fn draws(&mut self) -> u64 {
        self.core.player.stats("test")["draws"].as_u64().expect("draws")
    }

    /// The picture after everything settled.
    fn picture(&mut self) -> Capture {
        self.settle();
        let host = self.core.host.clone().expect("host");
        host.borrow_mut().capture_next_frame();
        let _ = self.core.player.invalidate();
        self.tick();
        self.settle();
        self.core.last_capture.take().expect("a captured frame")
    }

    fn other_value(&mut self, group: &str) -> String {
        let options = self.core.player.options().expect("options");
        options
            .as_array()
            .and_then(|groups| groups.iter().find(|g| g["group"] == group))
            .and_then(|g| {
                g["values"]
                    .as_array()?
                    .iter()
                    .find(|v| v["available"] == true && v["value"] != g["current"])
            })
            .and_then(|v| v["value"].as_str().map(str::to_owned))
            .expect("another sold value")
    }

    fn pick(&mut self, group: &str, value: Value) {
        self.state.queue_pick(group.to_owned(), value);
        self.tick();
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn differing(a: &Capture, b: &Capture) -> usize {
    let (left, _) = a.pixels.as_chunks::<4>();
    let (right, _) = b.pixels.as_chunks::<4>();
    left.iter().zip(right).filter(|(x, y)| x != y).count()
}

/// Pixels in the status line's refusal color (panel.rs), so the words are on the frame and not only in a field.
fn refusal_colored(capture: &Capture) -> usize {
    let (pixels, _) = capture.pixels.as_chunks::<4>();
    pixels
        .iter()
        .filter(|p| p[0] > 230 && (140..200).contains(&p[1]) && (90..150).contains(&p[2]))
        .count()
}

/// The top left pixel is the backdrop, well lit: not a black screen.
fn backdrop_is_lit(capture: &Capture) -> bool {
    capture.pixels[..3].iter().all(|channel| *channel > 150)
}

#[test]
fn an_idle_app_draws_nothing_and_wakes_slowly() {
    let Some(mut rig) = Rig::new("product-viewer-idle") else {
        return;
    };
    rig.settle();
    let draws = rig.draws();
    for _ in 0..500 {
        rig.tick();
    }
    assert_eq!(rig.draws(), draws, "no draw while nothing changes");
    assert_eq!(rig.state.stats.wakes_per_second, IDLE_WAKES_PER_SECOND);
}

#[test]
fn picks_change_the_picture_without_a_new_pipeline_and_a_refusal_changes_nothing() {
    let Some(mut rig) = Rig::new("product-viewer-picks") else {
        return;
    };
    let first = rig.picture();
    assert_eq!((first.width, first.height), SIZE);
    let pipelines = rig.core.player.stats("test")["pipelines"].clone();
    assert!(pipelines.as_u64().unwrap_or(0) > 0, "{pipelines}");

    let metal = rig.other_value("Material");
    rig.pick("Material", json!(metal));
    assert!(!rig.state.status_is_error, "{}", rig.state.status);
    let metal_picture = rig.picture();
    assert!(differing(&first, &metal_picture) > 50, "the metal shows");

    let gem = rig.other_value("Gem");
    rig.pick("Gem", json!(gem));
    let gem_picture = rig.picture();
    assert!(differing(&metal_picture, &gem_picture) > 10, "the gem shows");

    let size = rig.other_value("Gem Size");
    rig.pick("Gem Size", json!(size));
    let sized = rig.picture();
    assert!(differing(&gem_picture, &sized) > 10, "the size shows");
    assert_eq!(
        rig.core.player.stats("test")["pipelines"],
        pipelines,
        "no pipeline after open"
    );

    // A pick the snapshot does not sell: titanium with the large stone.
    rig.pick("Material", json!("Titanium"));
    assert!(
        rig.state.status_is_error,
        "the refusal is said: {}",
        rig.state.status
    );
    assert_eq!(rig.core.player.stats("test")["refused"], 1);
    let refused = rig.picture();
    // The status line changed; the piece above the panel did not.
    let rows = SIZE.1 as usize / 3;
    let top = 4 * SIZE.0 as usize * rows;
    assert_eq!(
        &sized.pixels[..top],
        &refused.pixels[..top],
        "the piece is the same"
    );
}

#[test]
fn a_drag_animates_then_comes_to_rest_inside_the_limits() {
    let Some(mut rig) = Rig::new("product-viewer-drag") else {
        return;
    };
    let front = rig.picture();
    let draws = rig.draws();
    let panel = rig.panel.hit_rect_px(2.0);
    let events = json!({"events": [
        {"id": 1, "phase": "down", "x": 100.0, "y": 100.0, "at_ms": 0.0},
        {"id": 1, "phase": "move", "x": 180.0, "y": 100.0, "at_ms": 16.0},
        {"id": 1, "phase": "move", "x": 260.0, "y": 100.0, "at_ms": 32.0},
        {"id": 1, "phase": "up", "x": 260.0, "y": 100.0, "at_ms": 500.0},
    ]});
    product_viewer::viewer_api::touch(&mut rig.state, panel, 2.0, &events).expect("touch");
    rig.tick();
    assert_eq!(
        rig.state.stats.wakes_per_second, ACTIVE_WAKES_PER_SECOND,
        "the view is moving"
    );
    rig.settle();
    assert!(rig.draws() > draws + 1, "the motion drew frames");
    let view = rig.core.player.view().expect("view");
    let tilt = view["tilt"].as_array().expect("tilt");
    let (x, y) = (tilt[0].as_f64().unwrap_or(0.0), tilt[1].as_f64().unwrap_or(0.0));
    assert!(x.abs() > 1.0, "{view}");
    assert!(x.hypot(y) <= 30.001, "inside the recipe's cone: {view}");
    assert!(differing(&front, &rig.picture()) > 100, "the view turned");
}

#[test]
fn a_tap_in_the_panel_picks_and_a_drag_in_the_panel_does_not_turn_the_view() {
    let Some(mut rig) = Rig::new("product-viewer-panel") else {
        return;
    };
    rig.settle();
    let panel = rig.panel.hit_rect_px(2.0);
    assert!(panel.min.y > 0.0, "the panel is below the piece: {panel:?}");
    let before = rig.core.player.build().expect("build")["picks"].clone();
    let y = panel.min.y + panel.height() * 0.5;
    let drag = json!({"events": [
        {"id": 2, "phase": "down", "x": 300.0, "y": y, "at_ms": 0.0},
        {"id": 2, "phase": "move", "x": 100.0, "y": y, "at_ms": 16.0},
        {"id": 2, "phase": "up", "x": 100.0, "y": y, "at_ms": 32.0},
    ]});
    product_viewer::viewer_api::touch(&mut rig.state, panel, 2.0, &drag).expect("touch");
    rig.settle();
    assert_eq!(
        rig.core.player.view().expect("view")["tilt"],
        json!([0.0, 0.0]),
        "the panel owns that gesture"
    );
    // Sweep the panel with taps until one lands on a chip of another value.
    let mut changed = false;
    let mut x = 40.0;
    while x < SIZE.0 as f32 && !changed {
        let mut y = panel.min.y + 8.0;
        while y < panel.max.y && !changed {
            let tap = json!({"events": [
                {"id": 3, "phase": "down", "x": x, "y": y, "at_ms": 0.0},
                {"id": 3, "phase": "up", "x": x, "y": y, "at_ms": 30.0},
            ]});
            product_viewer::viewer_api::touch(&mut rig.state, panel, 2.0, &tap).expect("touch");
            rig.tick();
            rig.tick();
            changed = rig.core.player.build().expect("build")["picks"] != before;
            y += 24.0;
        }
        x += 60.0;
    }
    assert!(changed, "some tap in the panel changed a pick");
    rig.settle();
}

/// Both ways a package can be refused with nothing open: the panel is on the screen with the code on its
/// status line, over a plain backdrop, nothing keeps drawing, and another product still opens from the list.
#[test]
fn a_package_refused_with_nothing_open_shows_the_refusal_and_the_app_stays_usable() {
    for (slug, code) in [
        ("unsold", "product_not_orderable"),
        ("tampered", "pack_hash_mismatch"),
    ] {
        let Some(mut rig) = Rig::over_store("product-viewer-refused", refusal_store) else {
            return;
        };
        rig.open(slug);
        let shown = rig.drawn_by_itself();
        assert!(rig.state.status_is_error, "{slug}: {}", rig.state.status);
        assert!(rig.state.status.contains(code), "{slug}: {}", rig.state.status);
        assert!(!rig.core.player.has_product(), "{slug}");
        assert!(rig.state.showing_products, "{slug}: the list is showing");

        let shown = shown.unwrap_or_else(|| panic!("{slug}: nothing reached the screen"));
        assert!(backdrop_is_lit(&shown), "{slug}: a black backdrop");
        assert!(
            refusal_colored(&shown) > 10,
            "{slug}: the refusal is not on the frame"
        );

        // The panel's frame is one frame, not a loop.
        for _ in 0..100 {
            rig.tick();
        }
        assert!(
            rig.core.last_capture.is_none(),
            "{slug}: still drawing while idle"
        );

        let opened = rig.tap_panel_until(|rig| rig.state.open_slug.as_deref() == Some("piece"));
        assert!(opened, "{slug}: the list did not open the other product");
        assert!(rig.core.player.has_product(), "{slug}");
        assert!(!rig.state.status_is_error, "{slug}: {}", rig.state.status);
        assert!(rig.draws() > 0, "{slug}: the player draws the product");
        let piece = rig.drawn_by_itself().expect("the product's frame");
        assert!(differing(&shown, &piece) > 100, "{slug}: the product shows");
    }
}

/// A rotation with nothing open: the panel moves to the side on a new frame of the new size, and still says why.
/// (A phone will not rotate over its keyguard, so a locked test phone cannot show this.)
#[test]
fn the_refusal_survives_a_rotation() {
    let Some(mut rig) = Rig::over_store("product-viewer-refused-rotate", refusal_store) else {
        return;
    };
    rig.open("unsold");
    rig.drawn_by_itself().expect("the refusal's frame");
    let sideways = (SIZE.1, SIZE.0);
    let mut shown = None;
    for _ in 0..200 {
        frame(
            &mut rig.state,
            &mut rig.panel,
            &mut rig.core,
            sideways,
            2.0,
            &SafeArea::default(),
        );
        shown = rig.core.last_capture.take().or(shown);
        if rig.state.stats.wakes_per_second == IDLE_WAKES_PER_SECOND {
            break;
        }
    }
    let shown = shown.expect("a frame after the rotation");
    assert_eq!((shown.width, shown.height), sideways);
    assert!(backdrop_is_lit(&shown), "a black backdrop");
    assert!(refusal_colored(&shown) > 10, "the refusal is not on the frame");
    assert!(
        rig.panel.hit_rect_px(2.0).min.x > 0.0,
        "the panel is beside the backdrop"
    );
}

/// A refusal while a product is open keeps that product: on the screen and still answering picks.
#[test]
fn a_package_refused_with_a_product_open_keeps_that_product() {
    let Some(mut rig) = Rig::over_store("product-viewer-refused-open", refusal_store) else {
        return;
    };
    rig.open("piece");
    rig.settle();
    let draws = rig.draws();
    rig.open("unsold");
    let shown = rig.drawn_by_itself().expect("the refusal's frame");
    assert!(
        rig.state.status.contains("product_not_orderable"),
        "{}",
        rig.state.status
    );
    assert!(refusal_colored(&shown) > 10, "the refusal is not on the frame");
    assert_eq!(rig.state.open_slug.as_deref(), Some("piece"));
    assert!(rig.draws() > draws, "the player's own frame carries the panel");

    rig.state.showing_products = false;
    let before = rig.core.player.build().expect("build")["picks"].clone();
    let metal = rig.other_value("Material");
    rig.pick("Material", json!(metal));
    assert!(!rig.state.status_is_error, "{}", rig.state.status);
    assert_ne!(rig.core.player.build().expect("build")["picks"], before);
}

/// A first start with nothing on the phone: the list (empty, with the copy hint) is on the screen.
#[test]
fn a_start_with_no_packages_shows_the_panel_and_draws_once() {
    let Some(mut rig) = Rig::over_store("product-viewer-empty", |dir| {
        std::fs::create_dir_all(dir.join("store")).expect("store");
    }) else {
        return;
    };
    let shown = rig.drawn_by_itself().expect("nothing reached the screen");
    assert!(backdrop_is_lit(&shown), "a black backdrop");
    assert!(rig.panel.hit_rect_px(2.0).is_positive(), "no panel");
    assert_eq!(rig.draws(), 0, "there is no product for the player to draw");
    for _ in 0..100 {
        rig.tick();
    }
    assert!(rig.core.last_capture.is_none(), "still drawing while idle");
}

/// JW-L7 on the app: a script of agent calls on the app's own frame function, on the player's fixed clock, panel
/// included. Two rigs, two devices, the same frames.
fn scripted_run(rig: &mut Rig) -> Vec<String> {
    let mut panel = std::mem::take(&mut rig.panel);
    let safe = SafeArea::default();
    viewer_api::set_deterministic(&mut panel, &mut rig.core, &json!({"on": true})).expect("on");
    let mut hashes = Vec::new();
    let mut step = |rig: &mut Rig, panel: &mut PanelUi, extra_app_frames: usize| {
        // App frames between two steps must change nothing, however many the host runs.
        for _ in 0..extra_app_frames {
            frame(&mut rig.state, panel, &mut rig.core, SIZE, 2.0, &safe);
        }
        let answer = viewer_api::step(
            &mut rig.state,
            panel,
            &mut rig.core,
            SIZE,
            2.0,
            &safe,
            &json!({"dt_ms": 1000.0 / 60.0, "force": true, "capture": "hash"}),
        )
        .expect("a step");
        assert_eq!(answer["drew"], json!(true), "{answer}");
        hashes.push(answer["hash"].as_str().expect("a hash").to_owned());
    };
    viewer_api::set_view(
        &mut rig.core,
        &json!({"tilt": [0.0, 0.0], "zoom": 1.0, "animate": false}),
    )
    .expect("view");
    step(rig, &mut panel, 0);
    viewer_api::set_view(&mut rig.core, &json!({"tilt": [9.0, -4.0], "zoom": 1.2})).expect("view");
    for extra in 0..4 {
        step(rig, &mut panel, extra);
    }
    let other = rig.other_value("Material");
    viewer_api::set_pick(
        &mut rig.state,
        &mut rig.core,
        &json!({"group": "Material", "value": other}),
    )
    .expect("sold");
    step(rig, &mut panel, 1);
    step(rig, &mut panel, 3);
    rig.panel = panel;
    hashes
}

#[test]
fn a_scripted_run_makes_the_same_frames_on_two_devices_panel_included() {
    let Some(mut first) = Rig::new("scripted-a") else {
        return;
    };
    let Some(mut second) = Rig::new("scripted-b") else {
        return;
    };
    let a = scripted_run(&mut first);
    let b = scripted_run(&mut second);
    assert_eq!(a, b, "byte-identical frames");
    assert_ne!(a[0], a[4], "the view moved the picture");
    assert_ne!(a[4], a[5], "the pick shows on its frame");
    assert_eq!(first.core.player.script_clock().0, a.len() as u64);
}

#[test]
fn a_step_out_of_scripted_mode_is_refused_and_leaving_it_brings_the_lazy_loop_back() {
    let Some(mut rig) = Rig::new("scripted-off") else {
        return;
    };
    let safe = SafeArea::default();
    let mut panel = std::mem::take(&mut rig.panel);
    let error = viewer_api::step(
        &mut rig.state,
        &mut panel,
        &mut rig.core,
        SIZE,
        2.0,
        &safe,
        &json!({"dt_ms": 16.0}),
    )
    .expect_err("not on");
    assert_eq!(error.code, "bad_argument");
    viewer_api::set_deterministic(&mut panel, &mut rig.core, &json!({"on": true})).expect("on");
    let before = rig.core.player.stats("test")["draws"].as_u64().expect("draws");
    for _ in 0..5 {
        frame(&mut rig.state, &mut panel, &mut rig.core, SIZE, 2.0, &safe);
    }
    assert_eq!(
        rig.core.player.stats("test")["draws"].as_u64().expect("draws"),
        before,
        "app frames draw nothing while the script holds the clock"
    );
    let lazy = viewer_api::step(
        &mut rig.state,
        &mut panel,
        &mut rig.core,
        SIZE,
        2.0,
        &safe,
        &json!({"dt_ms": 16.0, "capture": "hash"}),
    )
    .expect("a lazy step");
    assert_eq!(lazy["frame"], json!(1));
    viewer_api::set_deterministic(&mut panel, &mut rig.core, &json!({"on": false})).expect("off");
    rig.panel = panel;
    let other = rig.other_value("Material");
    rig.pick("Material", json!(other));
    assert!(
        rig.core.player.stats("test")["draws"].as_u64().expect("draws") > before,
        "the lazy loop draws again"
    );
}

/// A device, a store with `piece`, and the pipeline cache in `cache_dir`: one launch of the app, up to the first
/// frame. Returns what the attach reported, the picture, and whether the device keeps pipeline caches at all
/// (`Features::PIPELINE_CACHE`: Vulkan only in wgpu 30; Metal, DX12 and GL have none).
fn launch(dir: &Path, cache_dir: &Path, tag: &str) -> Option<(Value, Capture, bool)> {
    let host = SurfaceHost::headless(SIZE)?;
    let caches = host.device().features().contains(wgpu::Features::PIPELINE_CACHE);
    let config = ViewerConfig {
        package_roots: vec![dir.join("store")],
        ..ViewerConfig::default()
    };
    let mut core = ViewerCore::new();
    core.pipeline_cache_dir = Some(cache_dir.to_path_buf());
    core.attach(host).expect("attach");
    let report = core.gpu_start.clone().expect("the attach reports");
    let mut rig = Rig {
        state: ViewerState::new(&config),
        panel: PanelUi::default(),
        core,
        dir: dir.join(tag),
    };
    rig.open("piece");
    let picture = rig.drawn_by_itself().expect("the first frame reaches the screen");
    Some((report, picture, caches))
}

/// On a backend without pipeline caches the viewer keeps none: nothing loaded, nothing saved, no
/// folder made. Returns true there, so the caller skips the checks that need a cache file.
fn no_pipeline_caches(report: &Value, caches: bool, cache_dir: &Path) -> bool {
    if caches {
        return false;
    }
    assert_eq!(report["pipeline_cache_loaded_bytes"], 0, "{report}");
    assert_eq!(report["pipeline_cache_saved_bytes"], 0, "{report}");
    assert!(!cache_dir.exists(), "no cache folder without pipeline caches");
    eprintln!("this backend has no pipeline caches: the cache-file checks are skipped");
    true
}

#[test]
fn a_second_launch_starts_from_the_pipelines_the_first_one_kept() {
    let dir = unique_temp_dir("product_viewer_pipeline_cache");
    cook_into(&dir.join("work"), &dir.join("store"), "piece");
    let cache_dir = dir.join("pipeline-cache");
    let Some((first, first_picture, caches)) = launch(&dir, &cache_dir, "first") else {
        eprintln!("no GPU adapter on this machine: skipped");
        return;
    };
    if no_pipeline_caches(&first, caches, &cache_dir) {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    assert_eq!(first["pipeline_cache_loaded_bytes"], 0, "{first}");
    let files: Vec<_> = std::fs::read_dir(&cache_dir)
        .expect("the cache folder")
        .flatten()
        .collect();
    assert_eq!(files.len(), 1, "one file, no temp file left behind");
    let kept = std::fs::metadata(files[0].path()).expect("the file").len();
    assert!(kept > 0);
    assert_eq!(first["pipeline_cache_saved_bytes"], kept, "{first}");

    let (second, second_picture, _) = launch(&dir, &cache_dir, "second").expect("adapter");
    assert_eq!(second["pipeline_cache_loaded_bytes"], kept, "{second}");
    assert_eq!(
        second_picture.pixels, first_picture.pixels,
        "the cache must not change the picture"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_broken_pipeline_cache_file_costs_a_compile_and_nothing_else() {
    let dir = unique_temp_dir("product_viewer_pipeline_cache_broken");
    cook_into(&dir.join("work"), &dir.join("store"), "piece");
    let cache_dir = dir.join("pipeline-cache");
    let Some((good, good_picture, caches)) = launch(&dir, &cache_dir, "good") else {
        eprintln!("no GPU adapter on this machine: skipped");
        return;
    };
    if no_pipeline_caches(&good, caches, &cache_dir) {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let file = std::fs::read_dir(&cache_dir)
        .expect("folder")
        .flatten()
        .next()
        .expect("file")
        .path();
    let whole = std::fs::read(&file).expect("read");
    let mut flipped = whole.clone();
    flipped[4] ^= 0xff;
    let broken: [&[u8]; 3] = [b"", b"not a cache", &whole[..whole.len() / 2]];
    for (case, bytes) in broken.into_iter().chain([flipped.as_slice()]).enumerate() {
        std::fs::write(&file, bytes).expect("write");
        let (report, picture, _) = launch(&dir, &cache_dir, &format!("broken{case}")).expect("adapter");
        assert_eq!(picture.pixels, good_picture.pixels, "case {case}: {report}");
        let replaced = std::fs::read(&file).expect("read");
        assert_eq!(
            replaced.len(),
            whole.len(),
            "case {case}: the next launch has a good file: {report}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stats_say_how_the_gpu_started_and_a_forced_sample_count_is_used() {
    let Some(host) = SurfaceHost::headless(SIZE) else {
        eprintln!("no GPU adapter on this machine: skipped");
        return;
    };
    let config = ViewerConfig::default();
    let state = ViewerState::new(&config);
    let mut core = ViewerCore::new();
    core.samples = Some(1);
    core.attach(host).expect("attach");
    let stats = product_viewer::viewer_api::stats(&state, &core);
    assert_eq!(stats["gpu_start"]["samples"], 1, "{stats}");
    assert_eq!(stats["gpu_start"]["pipeline_cache_loaded_bytes"], 0, "{stats}");
}
