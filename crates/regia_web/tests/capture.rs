// SPDX-License-Identifier: MPL-2.0
//! JW-L7's deterministic mode on the real player, the real renderer and a real cooked package, offscreen (lavapipe
//! on the farm): the same script of `setPick` and `setView` calls makes the same frames, byte for byte, on two
//! separate devices. Skips when the machine offers no adapter.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use regia_viewer::BakedEnv;
use regia_viewer_pack::cook::{self, CookOptions};
use regia_viewer_pack::standin;
use regia_web::capture::{
    CaptureRig, CaptureRigError, CaptureScript, Ease, ScriptCamera, ScriptPick, ScriptViewKey, check_script,
    frame_hash, view_at, view_at_size,
};
use regia_web::product::PackageProduct;
use regia_web::seams::{PointerClass, ProductModel, ViewModel, ViewRequest};
use serde_json::{Value, json};

const SIZE: u32 = 96;
const STEP_MS: f64 = 1000.0 / 60.0;

fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "regia-web-capture-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

fn cook_marquise(root: &Path) -> PathBuf {
    let set = standin::write(&root.join("in")).expect("stand-ins");
    for (recipe, graph) in set.recipes.iter().zip(&set.option_graphs) {
        let name = recipe.file_stem().and_then(|s| s.to_str()).unwrap_or("x");
        if name.contains("marquise") {
            let out = root.join("out");
            let options = CookOptions {
                option_graph: Some(graph.clone()),
                materials: Some(set.library.clone()),
                ..CookOptions::default()
            };
            cook::pack(&set.kit_dir, recipe, &out, &options).expect("cooks");
            return out;
        }
    }
    panic!("no marquise stand-in");
}

fn product(dir: &Path) -> PackageProduct {
    PackageProduct::open(regia_viewer_pack::read_dir(dir).expect("package"), "studio-test").expect("opens")
}

fn rig(dir: &Path) -> Option<CaptureRig> {
    match CaptureRig::new(
        product(dir),
        [SIZE, SIZE],
        [SIZE, SIZE],
        &BakedEnv::studio_default(),
    ) {
        Ok(rig) => Some(rig),
        Err(CaptureRigError::Device(regia_viewer::ViewerError::NoAdapter(e))) => {
            assert!(
                std::env::var_os("REGIA_VIEWER_REQUIRE_ADAPTER").is_none(),
                "REGIA_VIEWER_REQUIRE_ADAPTER is set and there is no adapter: {e}"
            );
            println!("skipped: no adapter: {e}");
            None
        }
        Err(e) => panic!("no rig: {e}"),
    }
}

/// A scripted run: an animated view, swaps, a part change, a drag, on a fixed clock.
fn run_script(rig: &mut CaptureRig) -> Vec<String> {
    let mut hashes = Vec::new();
    let mut frame = |rig: &mut CaptureRig| {
        let drawn = rig.draw(STEP_MS).expect("a frame");
        hashes.push(frame_hash(&drawn.still.rgba));
    };
    let player = rig.player();
    player
        .set_view(&ViewRequest {
            tilt: Some([0.0, 0.0]),
            zoom: Some(1.0),
            animate: Some(false),
            ..ViewRequest::default()
        })
        .expect("a product is open");
    frame(rig);
    rig.player()
        .set_view(&ViewRequest {
            tilt: Some([12.0, -7.0]),
            zoom: Some(1.3),
            ..ViewRequest::default()
        })
        .expect("view");
    for _ in 0..5 {
        frame(rig);
    }
    rig.player()
        .set_pick("Material", &json!("14K Rose Gold"), false)
        .expect("sold");
    frame(rig);
    rig.player()
        .set_pick("Gem", &json!("Blue Sapphire (synthetic)"), false)
        .expect("sold");
    frame(rig);
    rig.player()
        .set_pick("Gem Size", &json!("4x2mm"), false)
        .expect("sold");
    frame(rig);
    // A scripted drag: the times given here are ignored for the script's clock.
    let player = rig.player();
    player.pointer_down(7, PointerClass::Touch, 40.0, 40.0, 123_456.0);
    player.pointer_move(7, 52.0, 44.0, 987_654.0);
    frame(rig);
    let player = rig.player();
    player.pointer_move(7, 70.0, 50.0, 1.0);
    frame(rig);
    rig.player().pointer_up(7, 2.0);
    for _ in 0..4 {
        frame(rig);
    }
    hashes
}

#[test]
fn the_same_script_makes_the_same_frames_on_two_devices() {
    let root = temp_dir("repeat");
    let dir = cook_marquise(&root);
    let Some(mut first) = rig(&dir) else { return };
    let Some(mut second) = rig(&dir) else { return };
    let a = run_script(&mut first);
    let b = run_script(&mut second);
    assert_eq!(a, b, "byte-identical frames");
    assert!(a.iter().all(|hash| hash.len() == 16), "{a:?}");
    let distinct = a.iter().collect::<std::collections::BTreeSet<_>>().len();
    assert!(distinct >= a.len() / 2, "the script moved the picture: {a:?}");
    assert_ne!(a[5], a[6], "a metal pick shows on its frame");
    assert_ne!(a[6], a[7], "a gem pick shows on its frame");
    assert_ne!(a[7], a[8], "a size pick shows on its frame");
    assert_eq!(first.player().script_clock().0, a.len() as u64);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_script_over_a_reused_player_repeats_after_the_clock_restarts() {
    let root = temp_dir("reuse");
    let dir = cook_marquise(&root);
    let Some(mut rig) = rig(&dir) else { return };
    let start = rig.player().build().expect("a build");
    let a = run_script(&mut rig);
    // Back to the first picks, the clock back to 0: the second run is the first.
    rig.player().set_deterministic(false);
    rig.player().set_deterministic(true);
    for group in ["Gem Size", "Gem", "Material"] {
        rig.player()
            .set_pick(group, &start["picks"][group], false)
            .expect("the first picks");
    }
    rig.draw(0.0).expect("a frame");
    rig.player().set_deterministic(false);
    rig.player().set_deterministic(true);
    assert_eq!(rig.player().script_clock(), (0, 0.0));
    let b = run_script(&mut rig);
    assert_eq!(a, b);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn the_players_clock_does_not_reach_a_scripted_run() {
    let root = temp_dir("clock");
    let dir = cook_marquise(&root);
    let Some(mut rig) = rig(&dir) else { return };
    rig.player()
        .set_pick("Material", &json!("14K Rose Gold"), false)
        .expect("sold");
    rig.draw(40.0).expect("a frame");
    let stats = rig.player().stats("test");
    assert_eq!(stats["deterministic"], json!(true));
    assert_eq!(stats["scriptFrame"], json!(1));
    assert_eq!(stats["scriptClockMs"], json!(40.0));
    assert_eq!(
        stats["lastSwitchMs"],
        json!(40.0),
        "a switch's time on the script's clock, the same every run"
    );
    std::fs::remove_dir_all(&root).ok();
}

fn script(frames: u64) -> CaptureScript {
    CaptureScript {
        frames,
        picks: vec![
            ScriptPick {
                frame: 2,
                group: "Material".into(),
                value: json!("14K Rose Gold"),
                adjust: false,
            },
            ScriptPick {
                frame: 4,
                group: "Gem Size".into(),
                value: json!("4x2mm"),
                adjust: false,
            },
        ],
        camera: ScriptCamera::View(vec![ScriptViewKey {
            frame: 4,
            tilt: Some([10.0, -6.0]),
            spin: None,
            pan: None,
            zoom: Some(1.2),
            ease: Ease::Linear,
        }]),
    }
}

#[test]
fn a_script_walked_leaves_the_product_at_its_start_and_reports_the_picks() {
    let root = temp_dir("walk");
    let dir = cook_marquise(&root);
    let mut open = product(&dir);
    let before = open.build(&Default::default()).expect("a build");
    let view = view_at_size(&mut open, [SIZE, SIZE]).expect("view");
    let checked = check_script(&mut open, &view, None, &script(6)).expect("a sold script");
    assert_eq!(checked.picks.len(), 2);
    assert_eq!(checked.picks[0].frame, 2);
    assert_eq!(checked.picks[1].group, "Gem Size");
    assert_eq!(checked.picks[0].orderable, json!(true));
    assert_ne!(checked.picks[0].build_hash, checked.picks[1].build_hash);
    assert_eq!(checked.picks_past_end, 0);
    assert_eq!(checked.problems, Vec::<String>::new());
    assert_eq!(
        open.build(&Default::default()).expect("a build")["picks"],
        before["picks"],
        "every pick of the walk undone"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_script_a_player_would_refuse_is_refused_before_a_frame() {
    let root = temp_dir("refuse");
    let dir = cook_marquise(&root);
    let mut open = product(&dir);
    let view = view_at_size(&mut open, [SIZE, SIZE]).expect("view");

    let mut unknown = script(6);
    unknown.picks[1].value = json!("no such size");
    let refusal = check_script(&mut open, &view, None, &unknown).expect_err("an unknown value");
    assert_eq!(refusal.kind, "pick");
    assert!(refusal.message.contains("frame 4"), "{refusal}");
    assert!(refusal.message.contains("no such size"), "{refusal}");

    let mut past_tilt = script(6);
    past_tilt.camera = ScriptCamera::View(vec![ScriptViewKey {
        frame: 3,
        tilt: Some([80.0, 0.0]),
        spin: None,
        pan: None,
        zoom: None,
        ease: Ease::Linear,
    }]);
    let refusal = check_script(&mut open, &view, None, &past_tilt).expect_err("outside the cone");
    assert_eq!(refusal.kind, "view");
    assert!(refusal.message.contains("frame"), "{refusal}");

    let mut spun = script(6);
    spun.camera = ScriptCamera::View(vec![ScriptViewKey {
        frame: 3,
        tilt: None,
        spin: Some(90.0),
        pan: None,
        zoom: None,
        ease: Ease::Hold,
    }]);
    let refusal = check_script(&mut open, &view, None, &spun).expect_err("this recipe has no spin");
    assert_eq!(refusal.kind, "view");
    assert!(refusal.message.contains("frame 3"), "{refusal}");

    let mut late = script(3);
    late.camera = ScriptCamera::Hold;
    let checked = check_script(&mut open, &view, None, &late).expect("the late pick is dropped");
    assert_eq!(checked.picks.len(), 1);
    assert_eq!(checked.picks_past_end, 1);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_build_starts_the_timeline_and_a_wrong_or_broken_one_is_refused() {
    let root = temp_dir("build");
    let dir = cook_marquise(&root);
    let mut source = product(&dir);
    source
        .set_pick("Material", &json!("14K Rose Gold"), false)
        .expect("sold");
    let saved_view = regia_web::seams::ViewValues {
        tilt: [6.0, 8.0],
        ..Default::default()
    };
    let build = source.build(&saved_view).expect("a build");

    let mut open = product(&dir);
    let view = view_at_size(&mut open, [SIZE, SIZE]).expect("view");
    let hold = CaptureScript {
        frames: 2,
        ..CaptureScript::default()
    };
    let checked = check_script(&mut open, &view, Some(&build), &hold).expect("its own build");
    assert_eq!(checked.start_build["picks"]["Material"], "14K Rose Gold");
    assert!(
        (checked.start_view.tilt[0] - 6.0).abs() < 1e-3,
        "{:?}",
        checked.start_view
    );
    assert!(
        (checked.start_view.tilt[1] - 8.0).abs() < 1e-3,
        "{:?}",
        checked.start_view
    );
    assert_eq!(
        open.build(&Default::default()).expect("a build")["picks"]["Material"],
        "14K Rose Gold",
        "the product stands at the build"
    );

    let mut other = build.clone();
    other["product_key"] = json!("elsewhere/p1");
    let refusal = check_script(&mut open, &view, Some(&other), &hold).expect_err("another product");
    assert_eq!(refusal.kind, "build");

    let mut unsold = build.clone();
    unsold["picks"]["Material"] = Value::String("no such metal".into());
    let refusal = check_script(&mut open, &view, Some(&unsold), &hold).expect_err("not sold");
    assert_eq!(refusal.kind, "build");
    assert!(refusal.message.contains("no longer orderable"), "{refusal}");

    let refusal =
        check_script(&mut open, &view, Some(&json!({"schema": "x"})), &hold).expect_err("not a build");
    assert_eq!(refusal.kind, "build");
    std::fs::remove_dir_all(&root).ok();
}

/// A sampled camera path is one pose per frame. A short list used to pass the check and then draw the
/// drag camera for every frame past the list.
#[test]
fn audit_camera_path_length() {
    let root = temp_dir("path-len");
    let dir = cook_marquise(&root);
    let mut open = product(&dir);
    let view = view_at_size(&mut open, [SIZE, SIZE]).expect("view");
    let pose = view.pose();
    let short = CaptureScript {
        frames: 2,
        picks: Vec::new(),
        camera: ScriptCamera::Poses(vec![pose]),
    };
    let refusal = check_script(&mut open, &view, None, &short).expect_err("short path");
    assert_eq!(refusal.kind, "view");
    assert!(
        refusal.message.contains("1 poses") && refusal.message.contains("2 frames"),
        "{refusal}"
    );
    let matched = CaptureScript {
        frames: 1,
        picks: Vec::new(),
        camera: ScriptCamera::Poses(vec![pose]),
    };
    check_script(&mut open, &view, None, &matched).expect("one pose per frame");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_rig_frame_of_a_script_is_what_the_player_draws_for_the_same_calls() {
    let root = temp_dir("same");
    let dir = cook_marquise(&root);
    let Some(mut by_script) = rig(&dir) else { return };
    let Some(mut by_hand) = rig(&dir) else { return };
    let script = script(6);
    let mut open = product(&dir);
    let view = view_at_size(&mut open, [SIZE, SIZE]).expect("view");
    let checked = check_script(&mut open, &view, None, &script).expect("a sold script");

    let mut scripted = Vec::new();
    for frame in 0..script.frames {
        let drawn = by_script
            .script_frame(&script, &checked, frame, STEP_MS)
            .expect("a frame");
        scripted.push(frame_hash(&drawn.still.rgba));
    }
    let mut manual = Vec::new();
    let keys = match &script.camera {
        ScriptCamera::View(keys) => keys.clone(),
        other => panic!("{other:?}"),
    };
    for frame in 0..script.frames {
        let shown = view_at(&keys, checked.start_view, frame);
        let player = by_hand.player();
        if frame == 2 {
            player
                .set_pick("Material", &json!("14K Rose Gold"), false)
                .expect("sold");
        }
        if frame == 4 {
            player.set_pick("Gem Size", &json!("4x2mm"), false).expect("sold");
        }
        player
            .set_view(&ViewRequest {
                tilt: Some(shown.tilt),
                spin: Some(shown.spin),
                pan: Some(shown.pan),
                zoom: Some(shown.zoom),
                animate: Some(false),
            })
            .expect("view");
        manual.push(frame_hash(&by_hand.draw(STEP_MS).expect("a frame").still.rgba));
    }
    assert_eq!(scripted, manual);
    assert_ne!(scripted[1], scripted[2], "the metal pick lands on frame 2");
    std::fs::remove_dir_all(&root).ok();
}

/// Audit LOW (sprint-corr-web-viewer): a render size past the device's largest texture made wgpu
/// panic in `CaptureRig::new` (an uncaptured validation error); it is refused as `bad_argument`.
#[test]
fn corr_a_render_past_the_largest_texture_is_refused_not_a_panic() {
    let root = temp_dir("corr-rig-size");
    let dir = cook_marquise(&root);
    let result = CaptureRig::new(product(&dir), [65536, 4], [65536, 4], &BakedEnv::studio_default());
    match result {
        Err(CaptureRigError::Scene(e)) => assert_eq!(e.code, "bad_argument", "{e}"),
        Err(CaptureRigError::Device(regia_viewer::ViewerError::NoAdapter(e))) => {
            assert!(
                std::env::var_os("REGIA_VIEWER_REQUIRE_ADAPTER").is_none(),
                "REGIA_VIEWER_REQUIRE_ADAPTER is set and there is no adapter: {e}"
            );
            println!("skipped: no adapter: {e}");
        }
        Err(e) => panic!("refused for another reason: {e}"),
        Ok(_) => panic!("a 65536-wide render was accepted"),
    }
    std::fs::remove_dir_all(&root).ok();
}
