// SPDX-License-Identifier: MPL-2.0
//! Regression fixtures drive the actual step bodies with deterministic protocol replies.

use super::*;

struct Rig(Driver);

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0.dir);
    }
}

fn rig(mut reply: impl FnMut(&str, Value) -> Result<Value, CallError> + 'static) -> Rig {
    let dir = regia_test_utils::unique_temp_dir("editor-smoke-regressions");
    std::fs::create_dir_all(dir.join("shots")).expect("shots");
    let mut frame = 0;
    Rig(Driver {
        test_call: Some(Box::new(move |method, params| match method {
            "agent.hello" => {
                frame += 10;
                Ok(json!({ "frame": frame }))
            }
            "play.wait_until" => Ok(json!({ "satisfied": true })),
            _ => reply(method, params),
        })),
        session: None,
        spec: LaunchSpec {
            exe: dir.join("unused-editor"),
            project: dir.clone(),
            work: dir.clone(),
            log: dir.join("editor.log"),
            windowed: false,
            headless: false,
            normal_mode: false,
            env: vec![],
            env_remove: vec![],
        },
        only: vec![],
        skip: vec![],
        keep_going: true,
        dir,
        scratch: "smoke-out".into(),
        budget: LAVAPIPE_BUDGET_MS,
        steps: vec![],
        shots: 0,
        restarts: 0,
        stopped: false,
        fingerprint_stable: true,
        doc_players: BTreeSet::new(),
        called: BTreeSet::new(),
        snapshot: vec![],
        examples: Default::default(),
    })
}

fn shot(draw_panel: bool, chrome: u8) -> Value {
    let mut img = image::RgbaImage::from_pixel(16, 16, image::Rgba([32, 32, 32, 255]));
    // A bright viewport/chrome outside the panel must never prove that the panel drew.
    img.put_pixel(0, 0, image::Rgba([chrome, 255, 255, 255]));
    if draw_panel {
        img.put_pixel(8, 8, image::Rgba([255, 255, 255, 255]));
    }
    let mut bytes = std::io::Cursor::new(Vec::new());
    img.write_to(&mut bytes, image::ImageFormat::Png).expect("PNG");
    json!({ "png_base64": regia_agent::base64_encode(bytes.get_ref()) })
}

fn layout() -> Value {
    json!({
        "panels": ["stub"],
        "panel_regions": { "stub": [4.0, 4.0, 8.0, 8.0] },
        "pixels_per_point": 1.0,
        "focused_widget": "abc"
    })
}

#[test]
fn opened_but_blank_panel_fails_despite_varied_window() {
    let mut rig = rig(|method, _| {
        Ok(match method {
            "editor.layout_get" => layout(),
            "play.expect" => json!({ "pass": true }),
            "seeing.screenshot" => shot(false, 200),
            _ => json!({}),
        })
    });
    let body = rig.0.panel_body("stub");
    assert_eq!(body.outcome, Outcome::Fail, "blank panel passed: {}", body.reason);
    assert!(body.reason.contains("flat color"), "{}", body.reason);
}

#[test]
fn painted_panel_passes_and_saves_only_its_body() {
    let mut rig = rig(|method, _| {
        Ok(match method {
            "editor.layout_get" => layout(),
            "play.expect" => json!({ "pass": true }),
            "seeing.screenshot" => shot(true, 200),
            _ => json!({}),
        })
    });
    let body = rig.0.panel_body("stub");
    assert_eq!(body.outcome, Outcome::Pass, "{}", body.reason);
    let saved = image::open(rig.0.dir.join(body.screenshot.expect("shot"))).expect("PNG");
    assert_eq!((saved.width(), saved.height()), (8, 8));
}

#[test]
fn scene_change_without_undo_entry_fails() {
    let mut changed = false;
    let mut rig = rig(move |method, _| {
        Ok(match method {
            "editor.history" => json!({ "undo_depth": 4 }),
            "scene.serialize" => json!({ "ron": if changed { "(value: 2)" } else { "(value: 1)" } }),
            "editor.stub_mutation" => {
                changed = true;
                json!({})
            }
            _ => json!({}),
        })
    });
    let body = rig
        .0
        .mutating_body("editor.stub_mutation", json!({}), "action:stub", false);
    assert_eq!(body.outcome, Outcome::Fail, "{}", body.reason);
    assert!(body.reason.contains("without an undo entry"), "{}", body.reason);
}

/// Lane v06-editor-smoke-fixes (ADR-8702): `route:editor.seq_new` failed "changed the level
/// without an undo entry" because `scene.serialize` lists the panel's document player, an
/// editor-only preview the level never saves (ADR-8583). The player the route answers is left out
/// of the fingerprint, in that step and later ones; any other new entity still fails the step.
#[test]
fn a_sequence_document_player_is_not_a_level_change() {
    fn scene(extra: &[(u64, &str)]) -> String {
        let mut ron = String::from(
            "(\n    entities: [\n        // entity 7 \"Cube\"\n        (\n            \"regia_scene::components::Name\": \"Cube\",\n        ),\n",
        );
        for (id, name) in extra {
            ron.push_str(&format!(
                "        // entity {id} \"{name}\"\n        (\n            \"regia_scene::components::Name\": \"{name}\",\n            \"x::Nested\": (\n                a: 1,\n            ),\n        ),\n"
            ));
        }
        ron.push_str("    ],\n)\n");
        ron
    }
    let spawned = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(u64, &'static str)>::new()));
    let shared = spawned.clone();
    let mut rig = rig(move |method, _| {
        Ok(match method {
            "editor.history" => json!({ "undo_depth": 4 }),
            "scene.serialize" => json!({ "ron": scene(&shared.borrow()) }),
            "editor.seq_new" => {
                shared.borrow_mut().push((9, "sequence:smoke-out/a.seq.ron"));
                json!({ "player": 9, "path": "smoke-out/a.seq.ron" })
            }
            "editor.stub_spawn" => {
                shared.borrow_mut().push((11, "Spawned"));
                json!({})
            }
            _ => json!({}),
        })
    });
    let body = rig
        .0
        .mutating_body("editor.seq_new", json!({}), "route:editor.seq_new", false);
    assert_eq!(body.outcome, Outcome::Pass, "{}", body.reason);
    assert_eq!(rig.0.doc_players, BTreeSet::from([9]));
    // The player lives on; a later level change is still caught.
    let body = rig
        .0
        .mutating_body("editor.stub_spawn", json!({}), "action:stub", false);
    assert_eq!(body.outcome, Outcome::Fail, "{}", body.reason);
    assert!(body.reason.contains("without an undo entry"), "{}", body.reason);
    assert!(!body.reason.contains("sequence:"), "{}", body.reason);
    assert_eq!(spawned.borrow().len(), 2);
}

/// Lane v06-editor-smoke-fixes (ADR-8702): `command:view.content_drawer` left the content drawer
/// open, and the next steps' crops (`scale200:inspector`) showed the drawer instead of their
/// panel. A step's reset closes both drawers after the layout reset.
#[test]
fn a_step_reset_closes_the_drawers() {
    let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(String, Value)>::new()));
    let seen = calls.clone();
    let mut rig = rig(move |method, params| {
        seen.borrow_mut().push((method.to_owned(), params));
        Ok(match method {
            "editor.history" => json!({ "undo_depth": 4 }),
            "scene.serialize" => json!({ "ron": "(value: 1)" }),
            _ => json!({}),
        })
    });
    let body = rig.0.mutating_body(
        "editor.command",
        json!({ "id": "view.content_drawer" }),
        "command:view.content_drawer",
        false,
    );
    assert_eq!(body.outcome, Outcome::Pass, "{}", body.reason);
    let calls = calls.borrow();
    let reset = calls
        .iter()
        .rposition(|(m, _)| m == "editor.layout_reset")
        .expect("a layout reset");
    let closed: Vec<&Value> = calls[reset..]
        .iter()
        .filter(|(m, _)| m == "editor.drawer")
        .map(|(_, p)| p)
        .collect();
    assert_eq!(
        closed,
        [
            &json!({ "drawer": "content", "open": false }),
            &json!({ "drawer": "log", "open": false })
        ]
    );
}

#[test]
fn without_entities_drops_only_the_named_records() {
    let ron = "(\n    entities: [\n        // entity 1 \"A\"\n        (\n            \"n\": (\n                x: 1,\n            ),\n        ),\n        // entity 12 \"B\", parent 1\n        (\n            \"n\": \"B\",\n        ),\n    ],\n)\n";
    assert_eq!(without_entities(ron, &BTreeSet::new()), ron);
    let kept = without_entities(ron, &BTreeSet::from([1]));
    assert!(!kept.contains("\"A\"") && !kept.contains("x: 1"), "{kept}");
    assert!(
        kept.contains("// entity 12 \"B\", parent 1") && kept.contains("\"n\": \"B\""),
        "{kept}"
    );
    assert!(kept.ends_with("    ],\n)\n"), "{kept}");
    // Entity 1 is not entity 12.
    let kept = without_entities(ron, &BTreeSet::from([12]));
    assert!(kept.contains("x: 1") && !kept.contains("\"B\""), "{kept}");
}

#[test]
fn panel_expect_requires_explicit_success() {
    for case in 0..6 {
        let mut rig = rig(move |method, _| match method {
            "editor.layout_get" => Ok(layout()),
            "seeing.screenshot" => Ok(shot(true, 200)),
            "play.expect" => match case {
                0 => Err(CallError::TimedOut),
                1 => Err(CallError::Rpc(-32601, "missing method".into(), Value::Null)),
                2 => Err(CallError::Gone("closed".into())),
                3 => Ok(json!({})),
                4 => Ok(json!({ "pass": "true" })),
                _ => Ok(json!({ "pass": false })),
            },
            _ => Ok(json!({})),
        });
        let body = rig.0.panel_body("stub");
        assert_eq!(body.outcome, Outcome::Fail, "expect case {case}: {}", body.reason);
        assert!(body.reason.contains("expect"), "{}", body.reason);
    }
}

#[cfg(unix)]
#[test]
fn unchanged_focus_fails_even_when_viewport_pixels_change() {
    let mut chrome = 100;
    let mut rig = rig(move |method, _| {
        Ok(match method {
            "editor.layout_get" => layout(),
            "seeing.screenshot" => {
                chrome += 1;
                shot(true, chrome)
            }
            _ => json!({}),
        })
    });
    for (name, key) in [
        ("tab_focus", xkeys::keysym::TAB),
        ("arrow_focus", xkeys::keysym::DOWN),
    ] {
        let body = rig.0.focus_body(name, key, || Ok(()));
        assert_eq!(body.outcome, Outcome::Fail, "{name}: {}", body.reason);
        assert!(body.reason.contains("focus"), "{}", body.reason);
    }
}

#[test]
fn panel_crop_uses_scale_and_rejects_missing_or_invalid_rectangles() {
    let img = image::RgbaImage::from_pixel(32, 32, image::Rgba([32, 32, 32, 255]));
    let mut reply = layout();
    reply["pixels_per_point"] = json!(2.0);
    let crop = report::panel_image(&img, &reply, "stub").expect("200 percent crop");
    assert_eq!(crop.dimensions(), (16, 16));
    assert!(report::panel_image(&img, &reply, "hidden").is_err());
    for rect in [
        json!([]),
        json!([4, 4, 0, 8]),
        json!([4, 4, -1, 8]),
        json!([-4, 4, 8, 8]),
        json!([30, 30, 8, 8]),
        json!([null, 4, 8, 8]),
    ] {
        reply["panel_regions"]["stub"] = rect;
        assert!(report::panel_image(&img, &reply, "stub").is_err(), "{reply}");
    }
    reply = layout();
    for scale in [Value::Null, json!(0), json!(-1), json!("2")] {
        reply["pixels_per_point"] = scale;
        assert!(report::panel_image(&img, &reply, "stub").is_err());
    }
}

#[test]
fn panel_crop_rounds_inward_to_exclude_neighbor_chrome() {
    let mut img = image::RgbaImage::from_pixel(16, 16, image::Rgba([32, 32, 32, 255]));
    img.put_pixel(4, 4, image::Rgba([255, 255, 255, 255]));
    let mut reply = layout();
    reply["panel_regions"]["stub"] = json!([4.25, 4.25, 8.0, 8.0]);
    let crop = report::panel_image(&img, &reply, "stub").expect("fractional rect");
    assert_eq!(crop.dimensions(), (7, 7));
    assert!(report::is_uniform(crop.as_raw(), 2));
}

#[cfg(unix)]
#[test]
fn changed_focus_passes_and_missing_focus_evidence_fails() {
    for (before, after, expected) in [
        (json!("a"), json!("b"), Outcome::Pass),
        (Value::Null, json!("b"), Outcome::Pass),
        (json!("a"), Value::Null, Outcome::Fail),
        (Value::Null, Value::Null, Outcome::Fail),
        (json!("a"), json!(false), Outcome::Fail),
    ] {
        let mut reads = 0;
        let mut rig = rig(move |method, _| {
            Ok(match method {
                "editor.layout_get" => {
                    reads += 1;
                    json!({ "focused_widget": if reads == 1 { &before } else { &after } })
                }
                "seeing.screenshot" => shot(true, 200),
                _ => json!({}),
            })
        });
        let body = rig.0.focus_body("tab_focus", xkeys::keysym::TAB, || Ok(()));
        assert_eq!(body.outcome, expected, "{}", body.reason);
    }
    for transport_error in [false, true] {
        let mut rig = rig(move |method, _| match method {
            "editor.layout_get" if transport_error => Err(CallError::TimedOut),
            "seeing.screenshot" => Ok(shot(true, 200)),
            _ => Ok(json!({})),
        });
        let body = rig.0.focus_body("tab_focus", xkeys::keysym::TAB, || Ok(()));
        assert_eq!(body.outcome, Outcome::Fail, "{}", body.reason);
    }
}

#[test]
fn a_scene_change_with_undo_restores_the_original_fingerprint() {
    let mut changed = false;
    let mut rig = rig(move |method, _| {
        Ok(match method {
            "editor.history" => json!({ "undo_depth": if changed { 5 } else { 4 } }),
            "scene.serialize" => json!({ "ron": if changed { "(value: 2)" } else { "(value: 1)" } }),
            "editor.stub_mutation" => {
                changed = true;
                json!({})
            }
            "editor.undo" => {
                changed = false;
                json!({})
            }
            _ => json!({}),
        })
    });
    let body = rig
        .0
        .mutating_body("editor.stub_mutation", json!({}), "action:stub", false);
    assert_eq!(body.outcome, Outcome::Pass, "{}", body.reason);
    assert!(body.reason.contains("undone (1 entry)"), "{}", body.reason);
}

#[test]
fn missing_or_decreasing_undo_depth_cannot_hide_a_scene_change() {
    for depth in [Value::Null, json!(3)] {
        let mut changed = false;
        let mut rig = rig(move |method, _| {
            Ok(match method {
                "editor.history" => json!({ "undo_depth": if changed { depth.clone() } else { json!(4) } }),
                "scene.serialize" => json!({ "ron": if changed { "(value: 2)" } else { "(value: 1)" } }),
                "editor.stub_mutation" => {
                    changed = true;
                    json!({})
                }
                _ => json!({}),
            })
        });
        // Initial instability must not turn an observed untracked mutation into a pass.
        rig.0.fingerprint_stable = false;
        let body = rig
            .0
            .mutating_body("editor.stub_mutation", json!({}), "action:stub", false);
        assert_eq!(body.outcome, Outcome::Fail, "{}", body.reason);
    }
}

#[test]
fn appearance_passes_start_from_edit_mode() {
    let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let seen = calls.clone();
    let mut r = rig(move |method, _| {
        seen.borrow_mut().push(method.to_owned());
        match method {
            // Nothing playing: the editor refuses; the harness goes on.
            "editor.stop" => Err(CallError::Rpc(-32000, "not playing".into(), Value::Null)),
            _ => Ok(json!({})),
        }
    });
    r.0.end_play();
    assert!(calls.borrow().iter().any(|m| m == "editor.stop"));
}

/// One flat color, like the possessed game view of an empty level.
fn flat_shot() -> Value {
    let img = image::RgbaImage::from_pixel(16, 16, image::Rgba([0, 0, 0, 255]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    img.write_to(&mut bytes, image::ImageFormat::Png).expect("PNG");
    json!({ "png_base64": regia_agent::base64_encode(bytes.get_ref()) })
}

/// A scripted editor with a play session: `editor.play` starts it, `editor.stop` ends it (and
/// refuses when nothing plays, like the editor), `editor.play_state` reports it, and a screenshot
/// is one flat color while it runs (the headless capture looks through the possessed game cameras
/// of a level with nothing to see). `stops` says whether `editor.stop` really stops it. Every
/// method name is recorded in `calls`.
fn play_rig(stops: bool, calls: std::rc::Rc<std::cell::RefCell<Vec<String>>>) -> Rig {
    let mut playing = false;
    rig(move |method, _| {
        calls.borrow_mut().push(method.to_owned());
        match method {
            "editor.play" | "editor.simulate" => {
                playing = true;
                Ok(json!({ "queued": true }))
            }
            "editor.stop" if !playing => Err(CallError::Rpc(-32000, "not playing".into(), Value::Null)),
            "editor.stop" => {
                playing = !stops;
                Ok(json!({ "queued": true }))
            }
            "editor.play_state" => Ok(json!({
                "mode": if playing { "playing" } else { "stopped" },
                "players": if playing { 2 } else { 1 },
            })),
            "seeing.screenshot" => Ok(if playing { flat_shot() } else { shot(true, 200) }),
            _ => Ok(json!({})),
        }
    })
}

fn players_step(r: &mut Rig) {
    r.0.step("menu", "Play/Players/2 players", "menus", |d| {
        d.mutating_body(
            "editor.play",
            json!({ "players": 2 }),
            "menu:Play/Players/2 players",
            true,
        )
    });
}

fn viewport_step(r: &mut Rig) {
    r.0.step("command", "window.open_viewport", "dialogs", |d| {
        d.mutating_body(
            "editor.command",
            json!({ "run": "window.open_viewport" }),
            "command:window.open_viewport",
            true,
        )
    });
}

/// Lane v07-editor-smoke-residue (ADR-8847, plan E-08 item 1): `menu:Play/Players/4 players` left
/// its split-screen play session running through every later menu and command step, so their
/// headless shots were the possessed game view of the empty sample level, one flat color (78 of
/// the 83 FAILs of the v0.6.0 card runs). A step that is not a play control starts in edit mode:
/// the harness stops a session an earlier step left running, and the next shot is not flat.
#[test]
fn a_play_session_an_earlier_step_left_running_ends_before_the_next_step() {
    let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut r = play_rig(true, calls.clone());
    players_step(&mut r);
    viewport_step(&mut r);
    let viewport = &r.0.steps[1];
    assert_eq!(viewport.outcome, Outcome::Pass, "{}", viewport.reason);
    assert!(
        viewport.reason.contains("stopped the play session"),
        "the step says it ended the session: {}",
        viewport.reason
    );
    let calls = calls.borrow();
    let stop = calls
        .iter()
        .rposition(|m| m == "editor.stop")
        .expect("editor.stop");
    let shot = calls
        .iter()
        .rposition(|m| m == "seeing.screenshot")
        .expect("screenshot");
    assert!(stop < shot, "the session ended before the next shot: {calls:?}");
}

/// The Play menu's own items keep their session: `⏸ Pause` after `▶ Play` must find it running,
/// so the harness never stops play between two play-control steps.
#[test]
fn play_control_steps_keep_the_session_between_them() {
    let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut r = play_rig(true, calls.clone());
    r.0.step("menu", "Play/▶ Play", "menus", |d| {
        d.mutating_body("editor.play", json!({}), "menu:Play/▶ Play", false)
    });
    r.0.step("menu", "Play/⏸ Pause", "menus", |d| {
        d.mutating_body("editor.pause", json!({}), "menu:Play/⏸ Pause", false)
    });
    r.0.step("action", "play.step", "other", |d| {
        d.mutating_body("editor.step", json!({}), "action:play.step", false)
    });
    assert!(
        !calls.borrow().iter().any(|m| m == "editor.stop"),
        "no stop between play controls: {:?}",
        calls.borrow()
    );
    for name in ["Play/Players/1 player", "play.start_camera", "Play/⏹ Stop"] {
        assert!(is_play_step(&format!("menu:{name}")) || is_play_step(&format!("action:{name}")));
    }
    for key in [
        "route:editor.play",
        "route:editor.pie_net_run",
        "route:editor.stop",
    ] {
        assert!(is_play_step(key), "{key}");
    }
    for key in [
        "menu:Help/About Regia Engine",
        "command:window.open_viewport",
        "route:editor.player_list",
        "action:layers.bind",
    ] {
        assert!(!is_play_step(key), "{key}");
    }
}

/// A session that will not stop fails the next step (its shot would show the play view), with
/// the play state in the reason.
#[test]
fn a_play_session_that_will_not_stop_fails_the_next_step() {
    let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut r = play_rig(false, calls);
    players_step(&mut r);
    viewport_step(&mut r);
    let viewport = &r.0.steps[1];
    assert_eq!(viewport.outcome, Outcome::Fail, "{}", viewport.reason);
    assert!(viewport.reason.contains("did not stop"), "{}", viewport.reason);
}

/// ADR-8847 (plan E-08 item 2): when an undo leaves an entity behind, the step names all of its
/// components through `scene.get_entity`, the ones `scene.serialize` cannot show (not reflected)
/// included, so the report says what the leftover is.
#[test]
fn an_entity_an_undo_left_behind_is_named_with_all_its_components() {
    let mut created = false;
    let mut rig = rig(move |method, params| {
        Ok(match method {
            "editor.history" => json!({ "undo_depth": if created { 5 } else { 4 } }),
            "scene.serialize" => json!({ "ron": if created {
                "(\n    entities: [\n        // entity 7 \"\"\n        (\n            {\n                \"a::Kept\": (1),\n            }\n        ),\n        // entity 9 \"\"\n        (\n            {\n                \"a::Left\": (2),\n            }\n        ),\n    ],\n)"
            } else {
                "(\n    entities: [\n        // entity 7 \"\"\n        (\n            {\n                \"a::Kept\": (1),\n            }\n        ),\n    ],\n)"
            } }),
            "editor.stub_mutation" => {
                created = true;
                json!({})
            }
            "scene.get_entity" => {
                assert_eq!(params["entity"], json!(9), "only the leftover is looked up");
                json!({ "components": {
                    "Left": { "type": "a::Left", "value": 2 },
                    "Marker": { "type": "b::Marker", "reflected": false },
                } })
            }
            _ => json!({}),
        })
    });
    let body = rig
        .0
        .mutating_body("editor.stub_mutation", json!({}), "action:stub", false);
    assert_eq!(body.outcome, Outcome::Fail, "{}", body.reason);
    assert!(
        body.reason.contains("entity 9 left behind: a::Left, b::Marker"),
        "{}",
        body.reason
    );
}
