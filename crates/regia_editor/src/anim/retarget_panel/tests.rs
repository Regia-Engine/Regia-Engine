// SPDX-License-Identifier: MPL-2.0
//! Retarget panel tests (ADR-7801): the fixture pair's auto-map, the saved setup, chain alignment,
//! the batch's equivalence with the runtime retargeter, Cancel, the keyboard path and the agent
//! dispatcher.

use std::cell::Cell;

use regia_animation::retarget_setup::RetargetSetup;
use serde_json::json;

use super::*;

fn fixture_panel() -> RetargetPanel {
    let mut panel = RetargetPanel::default();
    panel.open(
        RetargetRig::builtin("fixture:source").expect("fixture source"),
        RetargetRig::builtin("fixture:target").expect("fixture target"),
    );
    panel
}

/// Rotation difference in degrees.
fn angle_deg(a: Quat, b: Quat) -> f32 {
    // atan2 keeps resolution near zero, where the acos of a dot product rounds to 0.
    let d = a.normalize().conjugate() * b.normalize();
    (2.0 * d.xyz().length().atan2(d.w.abs())).to_degrees()
}

#[test]
fn the_fixture_pair_auto_maps_every_chain_by_chain_and_the_tail_by_name() {
    let source = fixture_source_skeleton();
    let target = fixture_target_skeleton();
    let map = auto_map(&source, &target);
    for chain in RetargetChain::ALL {
        for role in chain.roles() {
            assert!(
                map.source.contains_key(role.name()) && map.target.contains_key(role.name()),
                "{} misses {}: {map:?}",
                chain.name(),
                role.name()
            );
        }
    }
    let pair = |role: &str| (map.source[role].as_str(), map.target[role].as_str());
    assert_eq!(pair("hips"), ("mixamorig:Hips", "pelvis"));
    assert_eq!(pair("spine"), ("mixamorig:Spine", "spine_01"));
    assert_eq!(pair("upperChest"), ("mixamorig:Spine2", "spine_03"));
    assert_eq!(pair("head"), ("mixamorig:Head", "head"));
    assert_eq!(pair("leftShoulder"), ("mixamorig:LeftShoulder", "clavicle_l"));
    assert_eq!(pair("leftUpperArm"), ("mixamorig:LeftArm", "upperarm_l"));
    assert_eq!(pair("rightLowerLeg"), ("mixamorig:RightLeg", "calf_r"));
    assert_eq!(pair("leftToes"), ("mixamorig:LeftToeBase", "ball_l"));
    assert_eq!(map.by_chain.len(), 22, "every humanoid role pairs by chain");
    assert_eq!(map.by_name, vec!["tail_1".to_string(), "tail_2".to_string()]);
    assert_eq!(pair("tail_1"), ("mixamorig:Tail1", "tail_1"));
    assert!(
        !map.target.values().any(|j| j.contains("twist")),
        "twist joints never take a role: {map:?}"
    );
    assert_eq!(name_key("mixamorig:Tail1"), name_key("tail_1"));
}

#[test]
fn rows_group_by_chain_and_edits_are_checked() {
    let mut panel = fixture_panel();
    assert!(panel.rows().is_empty(), "open starts with an empty map");
    panel.auto_map().expect("auto-map");
    let rows = panel.rows();
    assert_eq!(rows.first().map(|r| r.role.as_str()), Some("hips"));
    assert_eq!(rows.last().map(|r| r.chain), Some(None), "custom rows come last");
    let err = panel
        .set_joint("leftHand", Side::Target, Some("no_such_joint"))
        .expect_err("unknown joint");
    assert_eq!(err.code, "retarget_unknown_joint");
    panel.set_joint("leftHand", Side::Target, None).expect("clear");
    let row = panel
        .rows()
        .into_iter()
        .find(|r| r.role == "leftHand")
        .expect("row stays");
    assert_eq!((row.source.is_some(), row.target), (true, None));
    panel
        .set_joint("leftHand", Side::Target, Some("hand_l"))
        .expect("set");
    assert!(panel.dirty);
    let err = RetargetPanel::default().auto_map().expect_err("not open");
    assert_eq!(err.code, "retarget_not_open");
}

#[test]
fn the_saved_map_is_a_current_setup_that_builds_the_panels_retargeter() {
    let dir = regia_test_utils::unique_temp_dir("retarget-save");
    let mut panel = fixture_panel();
    panel.auto_map().expect("auto-map");
    panel
        .set_chain_offset(RetargetChain::LeftArm, Vec3::new(0.0, 0.0, 12.5))
        .expect("offset");
    let path = dir.join("fixture.retarget.ron");
    panel.save(&path).expect("save");
    let text = std::fs::read_to_string(&path).expect("read back");
    assert!(
        text.contains("format_version: 2") && text.contains("Explicit"),
        "{text}"
    );
    let setup = RetargetSetup::from_ron_str(&text).expect("the existing loader reads it");
    for role in RetargetChain::LeftArm.roles() {
        assert_eq!(
            setup.roles[role.name()].rest_offset_deg,
            Vec3::new(0.0, 0.0, 12.5)
        );
    }
    let saved = setup
        .build(&fixture_source_skeleton(), &fixture_target_skeleton())
        .expect("builds");
    let ours = panel.retargeter().expect("panel retargeter");
    let source = panel.source.clone().expect("source");
    let mut pose = source.skeleton.bind_pose_pose();
    source.clip("Stride").expect("stride").sample_pose(0.4, &mut pose);
    let (mut a, mut b) = (Pose::default(), Pose::default());
    saved.retarget_pose(&pose, &mut a);
    ours.retarget_pose(&pose, &mut b);
    assert_eq!(a, b, "the file and the panel retarget identically");

    // Loading it back gives the same rows and offsets.
    let mut reopened = fixture_panel();
    reopened.load_setup(&text, Some(path)).expect("load");
    assert_eq!(reopened.rows(), panel.rows());
    assert_eq!(
        reopened.chain_offset(RetargetChain::LeftArm),
        Vec3::new(0.0, 0.0, 12.5)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn aligning_an_arm_to_the_t_pose_levels_it_and_the_a_pose_keeps_the_source() {
    let mut panel = fixture_panel();
    panel.auto_map().expect("auto-map");
    let target = fixture_target_skeleton();
    let joint = |n: &str| target.joint_index(n).expect("joint") as usize;
    let arm_dir = |panel: &mut RetargetPanel| {
        let r = panel.retargeter().expect("retargeter");
        let mut out = Pose::default();
        r.retarget_pose(&fixture_source_skeleton().bind_pose_pose(), &mut out);
        let g = target.globals(&out, None);
        (g[joint("lowerarm_l")].w_axis.truncate() - g[joint("upperarm_l")].w_axis.truncate()).normalize()
    };
    // Without an offset the T-pose target copies the source's A-pose at rest (its left is +X).
    let before = arm_dir(&mut panel);
    let down = Vec3::new(1.0, -1.0, 0.0).normalize();
    assert!(before.angle_between(down).to_degrees() < 0.1, "{before}");
    let offset = panel
        .align_chain(RetargetChain::LeftArm, ReferencePose::TPose)
        .expect("align");
    let after = arm_dir(&mut panel);
    let error = after.angle_between(Vec3::X).to_degrees();
    assert!(error < 0.1, "aligned arm {after} is {error}° off level");
    assert!((offset.length() - 45.0).abs() < 0.5, "a 45° turn: {offset}");
    // The source already stands in an A-pose: aligning to it sets no turn.
    let a = panel
        .align_chain(RetargetChain::LeftArm, ReferencePose::APose)
        .expect("align a");
    assert!(a.length() < 0.01, "{a}");
    let err = panel
        .align_chain(RetargetChain::Root, ReferencePose::TPose)
        .expect_err("the root has no aim bone");
    assert_eq!(err.code, "retarget_cannot_align");
}

fn batch_input(panel: &mut RetargetPanel, out: &Path) -> BatchInput {
    panel.batch_input(&[], out.to_path_buf()).expect("batch input")
}

#[test]
fn a_baked_clips_end_pose_matches_the_runtime_retargeter_for_the_same_map() {
    let dir = regia_test_utils::unique_temp_dir("retarget-batch-eq");
    let mut panel = fixture_panel();
    panel.auto_map().expect("auto-map");
    panel
        .align_chain(RetargetChain::RightArm, ReferencePose::TPose)
        .expect("align");
    let setup_path = dir.join("map.retarget.ron");
    panel.save(&setup_path).expect("save");
    let input = batch_input(&mut panel, &dir.join("clips"));
    let report = run_batch(&input, &|| false, &|_, _| {}).expect("batch");
    assert_eq!(report.written.len(), 4, "{report:?}");
    // The runtime side: a Retargeter built from the saved file by the runtime loader.
    let text = std::fs::read_to_string(&setup_path).expect("setup");
    let runtime = RetargetSetup::from_ron_str(&text)
        .expect("parse")
        .build(&fixture_source_skeleton(), &fixture_target_skeleton())
        .expect("build");
    let source = panel.source.clone().expect("source");
    let (mut worst_rot, mut worst_pos) = (0.0f32, 0.0f32);
    let (mut mid_rot, mut mid_pos) = (0.0f32, 0.0f32);
    for (clip, file) in source.clips.iter().zip(&report.written) {
        let bytes = std::fs::read(file).expect("clip file");
        let (header, payload) = regia_asset::decode_cooked_file(&bytes).expect("cooked container");
        assert_eq!(
            header.type_tag,
            regia_core::id::TypePathHash::of("regia_animation::AnimationClip")
        );
        let baked = AnimationClip::from_cooked_payload(payload).expect("clip payload");
        assert_eq!(baked.name, clip.name);
        let end = clip.duration;
        let mut src = source.skeleton.bind_pose_pose();
        clip.sample_pose(end, &mut src);
        let mut expected = Pose::default();
        runtime.retarget_pose(&src, &mut expected);
        let mut got = runtime.target().bind_pose_pose();
        baked.sample_pose(baked.duration, &mut got);
        for j in 0..expected.len() {
            worst_rot = worst_rot.max(angle_deg(expected.rotations[j], got.rotations[j]));
            worst_pos = worst_pos.max(expected.translations[j].distance(got.translations[j]));
        }
        // Mid-clip, for scale: the compression error between kept keys.
        let mid = end * 0.5;
        let mut src_mid = source.skeleton.bind_pose_pose();
        clip.sample_pose(mid, &mut src_mid);
        let mut expected_mid = Pose::default();
        runtime.retarget_pose(&src_mid, &mut expected_mid);
        let mut got_mid = runtime.target().bind_pose_pose();
        baked.sample_pose(mid, &mut got_mid);
        for j in 0..expected_mid.len() {
            mid_rot = mid_rot.max(angle_deg(expected_mid.rotations[j], got_mid.rotations[j]));
            mid_pos = mid_pos.max(expected_mid.translations[j].distance(got_mid.translations[j]));
        }
        // Before compression the bake is the runtime pose itself.
        let raw = runtime.retarget_clip(clip);
        let mut raw_end = runtime.target().bind_pose_pose();
        raw.sample_pose(raw.duration, &mut raw_end);
        for j in 0..expected.len() {
            assert!(
                angle_deg(expected.rotations[j], raw_end.rotations[j]) < 1e-3,
                "{} joint {j}",
                clip.name
            );
            assert!(expected.translations[j].distance(raw_end.translations[j]) < 1e-5);
        }
    }
    eprintln!(
        "retarget batch end pose vs runtime Retargeter: worst rotation {worst_rot:.3e} deg, worst translation {worst_pos:.3e} m; mid-clip worst {mid_rot:.3e} deg, {mid_pos:.3e} m"
    );
    // The bar is the cook compressor's own per-joint tolerance (review sprint-w12-rev-g7 #8):
    // `CompressionSettings::default()`, 0.005 rad and 5 mm.
    let limits = regia_animation::compress::CompressionSettings::default();
    assert!(
        worst_rot < limits.rotation_tolerance.to_degrees(),
        "worst rotation error {worst_rot}° (compressor tolerance {}°)",
        limits.rotation_tolerance.to_degrees()
    );
    assert!(
        worst_pos < limits.position_tolerance,
        "worst translation error {worst_pos} m (compressor tolerance {} m)",
        limits.position_tolerance
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cancel_leaves_no_clip_and_no_partial_file() {
    let dir = regia_test_utils::unique_temp_dir("retarget-batch-cancel");
    let mut panel = fixture_panel();
    panel.auto_map().expect("auto-map");
    let out = dir.join("clips");
    let input = batch_input(&mut panel, &out);
    // Cancel after two of four clips are baked, then on the last check before the move.
    for cancel_after in [2usize, 4] {
        let done = Cell::new(0usize);
        let report = run_batch(&input, &|| done.get() >= cancel_after, &|n, _| done.set(n)).expect("batch");
        assert!(report.cancelled && report.written.is_empty(), "{report:?}");
        let left: Vec<_> = std::fs::read_dir(&out)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "cancel after {cancel_after} left {left:?}");
    }
    let report = run_batch(&input, &|| false, &|_, _| {}).expect("batch");
    assert_eq!(report.written.len(), 4);
    let names: BTreeSet<String> = std::fs::read_dir(&out)
        .expect("out")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        ["Nod.cooked", "Stride.cooked", "Turn.cooked", "Wave.cooked"]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>(),
        "only the clips, no staging folder"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review sprint-w12-rev-g7 #7: a move that fails after some clips moved leaves no partial set.
#[test]
fn a_failed_move_leaves_no_clip_in_the_output_folder() {
    let dir = regia_test_utils::unique_temp_dir("retarget-batch-move-fails");
    let mut panel = fixture_panel();
    panel.auto_map().expect("auto-map");
    let out = dir.join("clips");
    let input = batch_input(&mut panel, &out);
    // The last clip's destination is a folder that is not empty: its rename fails.
    let last = input.clips.last().expect("clips");
    let blocker = out.join(clip_file_name(&last.name));
    std::fs::create_dir_all(blocker.join("keep")).expect("blocker");
    let err = run_batch(&input, &|| false, &|_, _| {}).expect_err("the last move fails");
    assert!(err.contains("no clip written"), "{err}");
    let left: BTreeSet<String> = std::fs::read_dir(&out)
        .expect("out")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        left,
        [clip_file_name(&last.name)].into_iter().collect::<BTreeSet<_>>(),
        "only the blocking folder is left: no moved clip, no staging folder"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review sprint-w12-rev-g7 #6: the panel's open, save and batch paths follow the agent's rule.
#[test]
fn panel_paths_outside_the_project_are_refused() {
    let root = regia_test_utils::unique_temp_dir("retarget-panel-paths");
    std::fs::create_dir_all(root.join("assets")).expect("root");
    let inside = panel_path(&root, "path", "assets/rigs/a.retarget.ron").expect("inside");
    assert!(
        inside.ends_with("assets/rigs/a.retarget.ron"),
        "{}",
        inside.display()
    );
    for outside in [
        "../outside.retarget.ron",
        "../../clips",
        "assets/../../x.retarget.ron",
    ] {
        let err = panel_path(&root, "out_dir", outside).expect_err(outside);
        assert!(!err.is_empty(), "{outside}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Review sprint-w12-rev-g7 #9: Play/Pause written on both passes keeps the first pass's flip.
#[test]
fn play_pause_flips_once_per_frame() {
    // Pass one: off -> on. Pass two of the same frame sees the click again: on -> off, refused.
    let on = gate_toggle(false, true, true);
    assert!(on);
    assert!(gate_toggle(on, false, false), "the second pass keeps it on");
    assert!(!gate_toggle(true, false, true), "the next frame's click pauses");
}

#[test]
fn clip_file_names_are_safe() {
    assert_eq!(clip_file_name("Walk Fwd/01"), "Walk_Fwd_01.cooked");
    assert_eq!(clip_file_name(".."), "clip.cooked");
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    let (_, handler) = methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is listed"));
    handler(world, &params)
}

fn reason(error: &RpcError) -> Option<String> {
    error
        .data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[test]
fn the_agent_methods_map_preview_save_and_batch() {
    let dir = regia_test_utils::unique_temp_dir("retarget-agent");
    let mut world = World::new();
    let err = call(&mut world, "editor.retarget_auto_map", json!({})).expect_err("not open");
    assert_eq!(reason(&err).as_deref(), Some("retarget_not_open"));
    let state = call(
        &mut world,
        "editor.retarget_open",
        json!({ "source": "fixture:source", "target": "fixture:target" }),
    )
    .expect("open");
    assert_eq!(state["source"]["clips"].as_array().map(Vec::len), Some(4));
    let mapped = call(&mut world, "editor.retarget_auto_map", json!({})).expect("auto");
    for chain in mapped["chains"].as_array().expect("chains") {
        assert_eq!(chain["mapped"], chain["roles"], "{chain}");
    }
    assert_eq!(mapped["by_name"], json!(["tail_1", "tail_2"]));
    let err = call(
        &mut world,
        "editor.retarget_map_set",
        json!({ "role": "leftHand", "target": "nope" }),
    )
    .expect_err("bad joint");
    assert_eq!(reason(&err).as_deref(), Some("retarget_unknown_joint"));
    let err = call(
        &mut world,
        "editor.retarget_map_set",
        json!({ "chain": "tail", "align": "t_pose" }),
    )
    .expect_err("bad chain");
    assert_eq!(reason(&err).as_deref(), Some("retarget_unknown_chain"));
    let set = call(
        &mut world,
        "editor.retarget_map_set",
        json!({ "chain": "left_leg", "rest_offset_deg": [5.0, 0.0, 0.0] }),
    )
    .expect("offset");
    assert_eq!(set["changed"], json!(["rest_offset_deg"]));
    call(
        &mut world,
        "editor.retarget_map_set",
        json!({ "chain": "left_leg", "rest_offset_deg": [0.0, 0.0, 0.0] }),
    )
    .expect("offset back");
    let preview = call(
        &mut world,
        "editor.retarget_preview",
        json!({ "clip": "Stride", "time": 0.3 }),
    )
    .expect("preview");
    assert_eq!(
        preview["facing_yaw_deg"]
            .as_f64()
            .map(f64::abs)
            .map(|v| v.round()),
        Some(180.0)
    );
    for chain in preview["chains"].as_array().expect("chains") {
        let angle = chain["angle_deg"].as_f64().expect("angle");
        assert!(angle < 0.1, "unaligned chains follow the source: {chain}");
    }
    let err = call(&mut world, "editor.retarget_preview", json!({ "clip": "Dance" })).expect_err("clip");
    assert_eq!(reason(&err).as_deref(), Some("retarget_unknown_clip"));
    let path = dir.join("agent.retarget.ron");
    let saved = call(
        &mut world,
        "editor.retarget_save",
        json!({ "path": path.display().to_string() }),
    )
    .expect("save");
    assert!(saved["text"].as_str().is_some_and(|t| t.contains("Explicit")));
    let out = dir.join("baked");
    let done = call(
        &mut world,
        "editor.retarget_batch",
        json!({ "out_dir": out.display().to_string(), "clips": ["Wave", "Nod"], "wait": true }),
    )
    .expect("batch");
    assert_eq!(done["result"]["count"], 2, "{done}");
    assert!(out.join("Wave.cooked").is_file() && out.join("Nod.cooked").is_file());
    let job = done["job"].as_str().expect("job id").to_string();
    let state = world
        .resource::<crate::editor_jobs::EditorJobs>()
        .get(&job)
        .map(|h| h.state());
    assert_eq!(state, Some(regia_agent::jobs::JobState::Done));
    // Reopening with the saved setup restores the map.
    let state = call(
        &mut world,
        "editor.retarget_open",
        json!({ "source": "fixture:source", "target": "fixture:target", "setup": path.display().to_string() }),
    )
    .expect("reopen");
    assert_eq!(state["chains"][3]["mapped"], 4, "{state}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_retarget_methods_answer_through_the_agent_dispatcher() {
    let mut app = crate::engine::app::App::new();
    app.add_plugins(crate::engine::app::MinimalPlugins);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(std::env::temp_dir()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    crate::agent_ext::install(app.world_mut());
    app.finish_plugins();
    let (sender, inbox) = crossbeam_channel::unbounded();
    app.insert_resource(regia_agent::AgentInbox(inbox));
    let mut call = |method: &str, params: Value| {
        let (reply, answer) = crossbeam_channel::bounded(1);
        sender
            .send(regia_agent::server::Inbound {
                request: regia_agent::protocol::Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("agent inbox");
        app.update();
        let response = answer.try_recv().expect("dispatch answers in one frame");
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.expect("agent result")
    };
    call(
        "editor.retarget_open",
        json!({ "source": "fixture:source", "target": "fixture:target" }),
    );
    let mapped = call("editor.retarget_auto_map", json!({}));
    assert_eq!(mapped["by_chain"].as_array().map(Vec::len), Some(22));
    let preview = call("editor.retarget_preview", json!({ "clip": "Wave", "time": 0.25 }));
    assert_eq!(preview["clip"], "Wave");
}

#[test]
fn the_panel_is_operable_from_the_keyboard() {
    use crate::canvas_keys::test_input::{frame, key, tab_to};
    let mut world = World::new();
    world.insert_resource(fixture_panel());
    let ctx = egui::Context::default();
    let size = [1200.0, 900.0];
    let mut draw = |ui: &mut egui::Ui| {
        egui::CentralPanel::default().show(ui, |ui| retarget_panel(ui, &mut world));
    };
    frame(&ctx, size, vec![], &mut draw);
    tab_to(&ctx, size, "Auto-map", false, 20, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
        &mut draw,
    );
    let update = frame(&ctx, size, vec![], &mut draw);
    drop(draw);
    let panel = world.resource::<RetargetPanel>();
    assert_eq!(
        panel.rows().len(),
        24,
        "Enter on the focused Auto-map button maps the rigs"
    );
    // The painted preview is a named canvas; the joint pickers are named comboboxes.
    let labels: Vec<String> = update
        .nodes
        .iter()
        .filter_map(|(_, n)| n.label().map(str::to_string))
        .collect();
    assert!(
        labels.iter().any(|l| l.starts_with("Retarget preview")),
        "{labels:?}"
    );
    assert!(labels.iter().any(|l| l == "Source joint for hips"), "{labels:?}");
}

#[test]
fn ik_goals_edit_save_and_preview_the_bake() {
    let mut panel = fixture_panel();
    panel.auto_map().expect("auto-map");
    panel.clip = Some("Stride".into());
    let plain = panel.preview_globals(0.4).expect("plain preview");
    // Goals without the preview toggle leave the preview a plain pose transfer.
    for goal in regia_animation::retarget::IkGoalSpec::humanoid_feet() {
        panel.set_goal(None, goal).expect("goal");
    }
    assert_eq!(panel.preview_globals(0.4).expect("preview"), plain);
    // With the toggle the preview is the bake with its goals solved.
    panel.goal_preview = true;
    let (_, with_goals) = panel.preview_globals(0.4).expect("goal preview");
    let retargeter = panel.retargeter().expect("retargeter");
    let source = panel.source.clone().expect("source");
    let baked = retargeter.retarget_clip(source.clip("Stride").expect("stride"));
    let mut pose = retargeter.target().bind_pose_pose();
    baked.sample_pose(0.4, &mut pose);
    assert_eq!(with_goals, retargeter.target().globals(&pose, None));
    let checks = panel.goal_checks().expect("checks");
    assert_eq!(checks.len(), 2);
    assert!(checks.iter().all(|c| c.resolved), "{checks:?}");
    // The saved file carries the goals and loads back with them.
    let text = panel.setup_text().expect("text");
    assert!(text.contains("ik_goals") && text.contains("Contacts"), "{text}");
    let mut reopened = fixture_panel();
    reopened.load_setup(&text, None).expect("load");
    assert_eq!(reopened.setup.ik_goals, panel.setup.ik_goals);
    // Edits are checked.
    let mut bad = regia_animation::retarget::IkGoalSpec::humanoid_hands().remove(0);
    bad.weight = 1.5;
    assert_eq!(
        panel.set_goal(None, bad).expect_err("weight").code,
        "retarget_bad_goal"
    );
    assert_eq!(
        panel.remove_goal(7).expect_err("index").code,
        "retarget_unknown_goal"
    );
    assert_eq!(panel.remove_goal(1).expect("remove").end_role(), "rightFoot");
    assert_eq!(panel.setup.ik_goals.len(), 1);
    assert!(panel.goal_roles().contains(&"leftFoot".to_string()));
}

#[test]
fn retarget_goal_set_edits_goals_through_the_agent() {
    let mut world = World::new();
    world.insert_resource(fixture_panel());
    world
        .resource_mut::<RetargetPanel>()
        .auto_map()
        .expect("auto-map");
    let out = call(
        &mut world,
        "editor.retarget_goal_set",
        json!({ "preset": "feet", "preview": true }),
    )
    .expect("preset");
    assert_eq!(out["ik_goals"].as_array().map(Vec::len), Some(2), "{out}");
    assert_eq!(out["goal_preview"], json!(true));
    assert_eq!(out["checks"].as_array().map(Vec::len), Some(2), "{out}");
    assert_eq!(out["checks"][0]["resolved"], json!(true), "{out}");
    let out = call(
        &mut world,
        "editor.retarget_goal_set",
        json!({ "roles": ["rightUpperArm", "rightLowerArm", "rightHand"], "weight": 0.5 }),
    )
    .expect("add");
    assert_eq!(out["ik_goals"][2]["mode"], json!("ScaledWorld"), "{out}");
    assert_eq!(out["ik_goals"][2]["weight"], json!(0.5), "{out}");
    let out = call(
        &mut world,
        "editor.retarget_goal_set",
        json!({ "index": 2, "mode": "Contacts" }),
    )
    .expect("mode");
    assert_eq!(out["ik_goals"][2]["mode"], json!("Contacts"), "{out}");
    let out = call(
        &mut world,
        "editor.retarget_goal_set",
        json!({ "index": 2, "remove": true }),
    )
    .expect("remove");
    assert_eq!(out["ik_goals"].as_array().map(Vec::len), Some(2), "{out}");
    for (params, code) in [
        (json!({ "roles": ["a", "b"] }), "retarget_bad_goal"),
        (json!({ "index": 0, "mode": "Sideways" }), "retarget_bad_goal"),
        (json!({ "index": 0, "weight": -1.0 }), "retarget_bad_goal"),
        (json!({ "preset": "tails" }), "retarget_bad_goal"),
        (json!({ "index": 9, "remove": true }), "retarget_unknown_goal"),
        (json!({ "remove": true }), "retarget_unknown_goal"),
    ] {
        let err = call(&mut world, "editor.retarget_goal_set", params.clone()).expect_err("refused");
        assert_eq!(reason(&err).as_deref(), Some(code), "{params}");
    }
    let preview = call(
        &mut world,
        "editor.retarget_preview",
        json!({ "clip": "Stride", "time": 0.2 }),
    )
    .expect("preview");
    assert_eq!(preview["goals"], json!(true), "{preview}");
    let mut empty = World::new();
    let err = call(
        &mut empty,
        "editor.retarget_goal_set",
        json!({ "preset": "feet" }),
    )
    .expect_err("not open");
    assert_eq!(reason(&err).as_deref(), Some("retarget_not_open"));
}
