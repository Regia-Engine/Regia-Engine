// SPDX-License-Identifier: MPL-2.0
//! Video panel tests over the `regia_media` pattern clips: stream info, frame-exact seeking, clamping,
//! error states, the boot list round trip, the skip binding check, the agent methods and headless drawing.

use std::path::Path;

use regia_media::mux::{PatternClip, ToneAudio, pattern_index};
use serde_json::{Value, json};

use super::*;
use crate::data_panels::test_ui::draw_frames;

const W: u32 = 16;
const H: u32 = 16;
const FRAMES: u32 = 40;

fn clip() -> PatternClip {
    let mut c = PatternClip::new(W, H, FRAMES, 25);
    c.audio = Some(ToneAudio {
        rate: 48_000,
        channels: 2,
        freq_hz: 440.0,
        amplitude: 0.5,
    });
    c
}

fn project(tag: &str) -> std::path::PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(root.join("movies")).expect("dir");
    std::fs::write(root.join("Regia.toml"), "[project]\nname = \"g\"\n").expect("manifest");
    let c = clip();
    std::fs::write(root.join("movies/a.webm"), c.to_webm()).expect("clip");
    std::fs::write(
        root.join("movies/subs.webm"),
        c.to_webm_with_cues(&[
            (0, Some(400_000_000), "hello"),
            (800_000_000, Some(400_000_000), "world"),
        ]),
    )
    .expect("clip");
    root
}

fn world_at(root: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    world
}

fn open(root: &Path, source: &str) -> (VideoPanel, Result<(), PanelError>) {
    let mut panel = VideoPanel::default();
    let r = panel.open(root, source, &MediaSettings::default(), (0, 0));
    (panel, r)
}

fn call(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
    let handler = methods()
        .into_iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| h)
        .expect("registered");
    handler(world, &params)
}

fn reason(e: &RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// The pattern frame number the picture on screen carries.
fn shown(panel: &VideoPanel) -> u32 {
    let core = panel.state().core();
    pattern_index(&core.presented().expect("a picture").planes[0])
}

#[test]
fn opening_shows_the_streams() {
    let root = project("video_panel_info");
    let (panel, r) = open(&root, "movies/a.webm");
    r.expect("opens");
    assert!(panel.is_open());
    let s = panel.summary().expect("summary");
    assert_eq!((s.width, s.height), (W, H));
    assert_eq!(s.fps, Some(25.0));
    assert_eq!(s.total_frames, Some(u64::from(FRAMES)));
    assert!((s.duration_s.expect("duration") - 1.6).abs() < 1e-6);
    assert_eq!(s.audio.len(), 1);
    assert_eq!((s.audio[0].channels, s.audio[0].sample_rate), (2, 48_000));
    assert!(s.text.is_empty());
    assert_eq!(panel.frame_index(), Some(0));
    assert_eq!(shown(&panel), 0);
    // A paused session decodes nothing, so it holds no pool slot.
    assert_eq!(
        panel.pool(),
        PoolUse {
            in_use: 0,
            max: MediaSettings::default().max_decoders
        }
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn caption_track_lists_its_cues() {
    let root = project("video_panel_cues");
    let (mut panel, r) = open(&root, "movies/subs.webm");
    r.expect("opens");
    for _ in 0..FRAMES {
        panel.step(1).expect("step");
    }
    let s = panel.summary().expect("summary");
    assert_eq!(s.text.len(), 1);
    assert_eq!(s.text[0].codec, "S_TEXT/UTF8");
    let texts: Vec<&str> = s.text[0].cues.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(texts, ["hello", "world"]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn seeking_to_frame_k_shows_frame_k() {
    let root = project("video_panel_seek");
    let (mut panel, r) = open(&root, "movies/a.webm");
    r.expect("opens");
    for k in [13u64, 4, 27, 0, 39] {
        assert_eq!(panel.seek_frame(k).expect("seek"), k);
        assert_eq!(panel.frame_index(), Some(k));
        assert_eq!(u64::from(shown(&panel)), k, "the picture carries its index");
        assert_eq!(panel.play_state(), PlayState::Paused);
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn stepping_goes_both_ways_and_stays_on_the_ends() {
    let root = project("video_panel_step");
    let (mut panel, r) = open(&root, "movies/a.webm");
    r.expect("opens");
    assert_eq!(panel.step(3).expect("step"), Some(3));
    assert_eq!(panel.step(-1).expect("back"), Some(2));
    assert_eq!(u64::from(shown(&panel)), 2);
    assert_eq!(panel.step(-100).expect("back"), Some(0));
    panel.seek_frame(39).expect("seek");
    assert_eq!(panel.step(1).expect("past the end"), Some(39));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn scrubbing_past_the_end_clamps() {
    let root = project("video_panel_clamp");
    let (mut panel, r) = open(&root, "movies/a.webm");
    r.expect("opens");
    assert_eq!(panel.seek_frame(10_000).expect("clamps"), 39);
    assert_eq!(panel.seek_frame(u64::MAX).expect("clamps"), 39);
    assert_eq!(u64::from(shown(&panel)), 39);
    assert!(panel.seek_time(1e9).is_ok());
    assert_eq!(panel.frame_index(), Some(39));
    assert!(panel.seek_time(-5.0).is_ok());
    assert_eq!(panel.frame_index(), Some(0));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn playing_advances_and_pausing_holds() {
    let root = project("video_panel_play");
    let (mut panel, r) = open(&root, "movies/a.webm");
    r.expect("opens");
    panel.play().expect("play");
    for _ in 0..30 {
        panel.tick(16_666_667);
    }
    let at = panel.frame_index().expect("frame");
    assert!(at > 0, "30 steps of 1/60 s move the picture");
    panel.pause().expect("pause");
    panel.tick(16_666_667);
    assert_eq!(panel.frame_index(), Some(at));
    assert_eq!(panel.play_state(), PlayState::Paused);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn missing_and_corrupt_assets_give_an_error_state() {
    let root = project("video_panel_errors");
    let (panel, r) = open(&root, "movies/nope.webm");
    let e = r.expect_err("missing");
    assert_eq!(e.code, "video_panel_source_not_found");
    assert!(!panel.is_open());
    assert_eq!(panel.json(Some(&root))["state"], "error");

    std::fs::write(root.join("movies/junk.webm"), b"this is not matroska at all").expect("junk");
    let (panel, r) = open(&root, "movies/junk.webm");
    let e = r.expect_err("corrupt");
    assert!(e.code.starts_with("video_panel_"), "{e:?}");
    assert!(!panel.is_open());
    assert_eq!(panel.error(), Some(&e));

    let bytes = clip().to_webm();
    std::fs::write(root.join("movies/cut.webm"), &bytes[..bytes.len() / 3]).expect("cut");
    let (mut panel, r) = open(&root, "movies/cut.webm");
    // A truncated file may open and fail while decoding, or fail to open: neither panics.
    if r.is_ok() {
        for k in 0..FRAMES.into() {
            let _ = panel.seek_frame(k);
        }
    }

    // ADR-7566: the Windows shapes are refused on every host (`/etc/x.webm` and `\x.webm` are not
    // absolute on Windows, and `C:x.webm` is drive-relative, so a `Path` check let them through).
    for bad in [
        "../x.webm",
        "/etc/x.webm",
        "\\x.webm",
        "C:x.webm",
        "C:\\x.webm",
        "movies\\..\\..\\x.webm",
        "movies/CON.webm",
        "movies/a.mp4",
        "",
    ] {
        let (_, r) = open(&root, bad);
        assert_eq!(r.expect_err(bad).code, "video_panel_bad_path", "{bad}");
    }
    let mut closed = VideoPanel::default();
    assert_eq!(
        closed.seek_frame(1).expect_err("not open").code,
        "video_panel_not_open"
    );
    assert_eq!(closed.play().expect_err("not open").code, "video_panel_not_open");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_full_decode_pool_refuses_to_open() {
    let root = project("video_panel_pool");
    let mut panel = VideoPanel::default();
    let settings = MediaSettings {
        max_decoders: 1,
        ..MediaSettings::default()
    };
    let e = panel
        .open(&root, "movies/a.webm", &settings, (1, 1))
        .expect_err("pool full");
    assert_eq!(e.code, "video_panel_limit_exceeded");
    panel
        .open(&root, "movies/a.webm", &settings, (0, 0))
        .expect("an empty pool opens");
    assert_eq!(panel.pool(), PoolUse { in_use: 0, max: 1 });
    panel.play().expect("play");
    assert_eq!(
        panel.pool(),
        PoolUse { in_use: 1, max: 1 },
        "playing holds the slot"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn boot_reorder_round_trips_through_the_manifest() {
    let root = project("video_panel_boot");
    let manifest = "# the game\n[project]\nname = \"g\"\n\n[boot]\nmovies = [\"movies/a.webm\", \"movies/subs.webm\", \"movies/c.video.ron\"]\nskippable_after = 2.5\n";
    std::fs::write(root.join("Regia.toml"), manifest).expect("manifest");
    let mut boot = BootListModel::load(&root).expect("loads");
    assert_eq!(boot.movies.len(), 3);
    assert!(!boot.dirty());
    assert!(!boot.save(&root).expect("no-op save"));
    assert_eq!(
        std::fs::read_to_string(root.join("Regia.toml")).expect("read"),
        manifest
    );

    boot.reorder(2, 0).expect("reorder");
    assert!(boot.dirty());
    assert!(boot.reorder(0, 3).is_err());
    assert!(boot.save(&root).expect("saves"));
    assert!(!boot.dirty());

    let config = BootConfig::load(&root).expect("the engine's own reader");
    assert_eq!(
        config.movies,
        ["movies/c.video.ron", "movies/a.webm", "movies/subs.webm"]
    );
    assert_eq!(config.skippable_after, Some(2.5));
    let text = std::fs::read_to_string(root.join("Regia.toml")).expect("read");
    assert!(text.contains("name = \"g\""), "other keys are kept: {text}");
    assert_eq!(BootListModel::load(&root).expect("again").movies, boot.movies);

    boot.remove(0).expect("remove");
    boot.add("movies/d.webm").expect("add");
    assert!(boot.add("../escape.webm").is_err());
    assert!(boot.add("movies/x.png").is_err());
    boot.save(&root).expect("saves");
    assert_eq!(
        BootConfig::load(&root).expect("reads").movies,
        ["movies/a.webm", "movies/subs.webm", "movies/d.webm"]
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn boot_list_checks_entries_and_the_skip_binding() {
    let root = project("video_panel_skip");
    std::fs::write(
        root.join("Regia.toml"),
        "[boot]\nmovies = [\"movies/a.webm\", \"movies/gone.webm\"]\n",
    )
    .expect("manifest");
    let boot = BootListModel::load(&root).expect("loads");
    let checks = boot.check_entries(&root);
    assert_eq!(checks[0].problem, None);
    assert_eq!(checks[1].problem.as_deref(), Some("source_not_found"));

    assert_eq!(SkipBinding::check(&root), SkipBinding::Default);
    std::fs::create_dir_all(root.join("config")).expect("dir");
    std::fs::write(root.join("config/input.ron"), "not ron (").expect("bad");
    let bad = SkipBinding::check(&root);
    assert_eq!(bad, SkipBinding::Unreadable);
    assert!(!bad.ok());
    let bound = regia_input::InputConfig::new().with_skip_video_default().to_ron();
    std::fs::write(root.join("config/input.ron"), bound).expect("good");
    assert_eq!(SkipBinding::check(&root), SkipBinding::Project);
    let unbound = regia_input::InputConfig::new().to_ron();
    std::fs::write(root.join("config/input.ron"), unbound).expect("none");
    assert_eq!(SkipBinding::check(&root), SkipBinding::Default);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn agent_methods_open_seek_and_report() {
    let root = project("video_panel_agent");
    let mut world = world_at(&root);

    let none = call(&mut world, "editor.video_state", json!({})).expect("state");
    assert_eq!(none["open"], false);

    let e = call(
        &mut world,
        "editor.video_open",
        json!({ "path": "movies/nope.webm" }),
    )
    .expect_err("missing");
    assert_eq!(reason(&e), "video_panel_source_not_found");
    let state = call(&mut world, "editor.video_state", json!({})).expect("state");
    assert_eq!(state["state"], "error");
    assert_eq!(state["error"]["code"], "video_panel_source_not_found");

    let e = call(&mut world, "editor.video_seek", json!({ "frame": 1 })).expect_err("nothing open");
    assert_eq!(
        reason(&e),
        "video_panel_source_not_found",
        "the error state is kept"
    );

    let opened = call(
        &mut world,
        "editor.video_open",
        json!({ "path": "movies/a.webm" }),
    )
    .expect("opens");
    assert_eq!(opened["open"], true);
    assert_eq!(opened["info"]["width"], W);
    assert_eq!(opened["info"]["total_frames"], FRAMES);
    assert_eq!(opened["info"]["audio"].as_array().map(Vec::len), Some(1));
    assert_eq!(opened["frame"], 0);
    assert_eq!(opened["pool"]["in_use"], 0);

    let sought = call(&mut world, "editor.video_seek", json!({ "frame": 17 })).expect("seek");
    assert_eq!(sought["frame"], 17);
    let clamped = call(&mut world, "editor.video_seek", json!({ "frame": 99_999 })).expect("clamp");
    assert_eq!(clamped["frame"], 39);
    let stepped = call(&mut world, "editor.video_seek", json!({ "step": -2 })).expect("step");
    assert_eq!(stepped["frame"], 37);
    let timed = call(&mut world, "editor.video_seek", json!({ "time": 0.2 })).expect("time");
    assert_eq!(timed["frame"], 5);
    let playing = call(
        &mut world,
        "editor.video_seek",
        json!({ "transport": "play", "ticks": 30 }),
    )
    .expect("play");
    assert!(playing["frame"].as_u64().expect("frame") > 5);
    assert_eq!(playing["stats"]["frames_dropped"], 0);
    let paused = call(&mut world, "editor.video_seek", json!({ "transport": "pause" })).expect("pause");
    assert_eq!(paused["state"], "paused");

    for bad in [
        json!({}),
        json!({ "frame": 1, "time": 1.0 }),
        json!({ "transport": "rewind" }),
        json!({ "ticks": 99_999 }),
    ] {
        let e = call(&mut world, "editor.video_seek", bad.clone()).expect_err("bad params");
        assert_eq!(e.code, regia_agent::protocol::INVALID_PARAMS, "{bad}");
    }
    let e = call(&mut world, "editor.video_open", json!({})).expect_err("needs a path");
    assert_eq!(e.code, regia_agent::protocol::INVALID_PARAMS);

    let boot = &paused["boot"];
    assert_eq!(boot["skip_binding"], "default");
    assert_eq!(boot["skip_binding_ok"], true);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn opening_without_a_project_is_coded() {
    let mut world = World::new();
    let e = call(
        &mut world,
        "editor.video_open",
        json!({ "path": "movies/a.webm" }),
    )
    .expect_err("no project");
    assert_eq!(reason(&e), "video_panel_no_project");
}

#[test]
fn the_panel_draws_headless() {
    let root = project("video_panel_draw");
    let mut world = world_at(&root);
    std::fs::write(
        root.join("Regia.toml"),
        "[boot]\nmovies = [\"movies/a.webm\", \"movies/gone.webm\"]\n",
    )
    .expect("manifest");
    assert!(!draw_frames(&mut world, 2, video_panel).is_empty());
    open_video(&mut world, "movies/subs.webm").expect("opens");
    let shapes = draw_frames(&mut world, 3, video_panel);
    assert!(!shapes.is_empty());
    let panel = world.resource::<VideoPanel>();
    assert!(panel.is_open());
    assert_eq!(panel.boot.as_ref().map(|b| b.movies.len()), Some(2));
    let _ = std::fs::remove_dir_all(root);
}

fn clock_ns(panel: &VideoPanel) -> i64 {
    panel.state().core().position_ns()
}

/// The panel feeds each UI frame's own delta. A lockstep `Step` multiplies the step count by
/// that call's delta, so a long frame jumps the playhead and the next short frame rewinds it.
#[test]
fn variable_frame_deltas_do_not_rewind_the_playhead() {
    let root = project("video_panel_variable_dt");
    let (mut panel, r) = open(&root, "movies/a.webm");
    r.expect("opens");
    panel.play().expect("play");
    panel.tick(10_000_000);
    panel.tick(10_000_000);
    panel.tick(10_000_000);
    let after_three = clock_ns(&panel);
    panel.tick(100_000_000);
    let after_long = clock_ns(&panel);
    panel.tick(10_000_000);
    let after_short = clock_ns(&panel);
    assert!(
        after_short > after_long,
        "a shorter frame rewound the playhead: after three 10 ms ticks {after_three} ns, after a 100 ms tick {after_long} ns, after the next 10 ms tick {after_short} ns"
    );
    assert_eq!(
        after_short - after_three,
        110_000_000,
        "the 100 ms frame and the following 10 ms frame must add only their own deltas (from {after_three} ns to {after_short} ns)"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Stepping backward while playback is running pauses on the chosen frame. The next tick must
/// not walk off it.
#[test]
fn stepping_back_while_playing_pauses() {
    let root = project("video_panel_step_back");
    let (mut panel, r) = open(&root, "movies/a.webm");
    r.expect("opens");
    panel.seek_frame(10).expect("seek");
    panel.play().expect("play");
    assert_eq!(panel.play_state(), PlayState::Playing);
    assert_eq!(panel.step(-1).expect("back"), Some(9));
    assert_eq!(
        panel.play_state(),
        PlayState::Paused,
        "a backward step pauses, got {:?}",
        panel.play_state()
    );
    panel.tick(100_000_000);
    assert_eq!(
        panel.frame_index(),
        Some(9),
        "the paused frame holds across a tick"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Stepping past the last picture pauses on that picture. `play` continues from there.
#[test]
fn stepping_past_the_end_pauses_on_the_last_frame() {
    let root = project("video_panel_step_end");
    let (mut panel, r) = open(&root, "movies/a.webm");
    r.expect("opens");
    assert_eq!(panel.step(100).expect("past the end"), Some(39));
    assert_eq!(
        panel.play_state(),
        PlayState::Paused,
        "stepping past the end pauses on the last frame, got {:?}",
        panel.play_state()
    );
    panel.play().expect("play");
    assert_ne!(
        panel.frame_index(),
        Some(0),
        "play from the last frame must not restart the clip"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Audit LOW (sprint-corr-editor-b #22): `contains("..")` refused a legit name like `a..b.webm`,
/// and `C:x.webm` (a drive-relative path on Windows) passed. The list takes what the runtime's
/// project-path rule takes.
#[test]
fn boot_add_follows_the_project_path_rule() {
    let root = project("video_panel_boot_names");
    let mut boot = BootListModel::load(&root).expect("loads");
    boot.add("movies/a..b.webm")
        .expect("a double dot inside a name is a name");
    for bad in [
        "../x.webm",
        "movies/../../x.webm",
        "C:/x.webm",
        "C:x.webm",
        "/x.webm",
        "movies/CON.webm",
    ] {
        assert!(boot.add(bad).is_err(), "{bad}");
    }
    assert_eq!(boot.movies, ["movies/a..b.webm"]);
    let _ = std::fs::remove_dir_all(root);
}

/// `editor.video_boot` does the panel's boot list Up / Down, Remove, Add and Save (ADR-7307).
#[test]
fn video_boot_edits_and_saves_the_boot_list() {
    let root = project("video_panel_boot_agent");
    std::fs::write(
        root.join("Regia.toml"),
        "[project]\nname = \"g\"\n\n[boot]\nmovies = [\"movies/a.webm\", \"movies/subs.webm\"]\n",
    )
    .expect("manifest");
    let mut world = world_at(&root);
    let out = call(&mut world, "editor.video_boot", json!({ "move": [1, 0] })).expect("move");
    assert_eq!(out["boot"]["movies"][0]["path"], "movies/subs.webm", "{out}");
    assert_eq!(out["boot"]["dirty"], true);
    let out = call(
        &mut world,
        "editor.video_boot",
        json!({ "remove": 1, "add": "movies/c.webm", "save": true }),
    )
    .expect("edit and save");
    assert_eq!(out["boot"]["dirty"], false, "{out}");
    let saved = BootListModel::load(&root).expect("reload");
    assert_eq!(
        saved.movies,
        vec!["movies/subs.webm".to_owned(), "movies/c.webm".to_owned()]
    );
    let bad = call(&mut world, "editor.video_boot", json!({ "remove": 9 })).expect_err("out of range");
    assert_eq!(reason(&bad), "video_panel_bad_index", "{bad:?}");
    let bad = call(&mut world, "editor.video_boot", json!({ "add": "../x.webm" })).expect_err("outside");
    assert_eq!(reason(&bad), "video_panel_bad_path", "{bad:?}");
    let _ = std::fs::remove_dir_all(&root);
}
