// SPDX-License-Identifier: MPL-2.0
//! The Motion Matching panel, headless: the candidate table, the bar classes, freezing, the
//! database view and the agent methods, on the procedural locomotion set.

use std::sync::Arc;

use regia_animation::motion_matching::procedural::{arc_trajectory, procedural_locomotion};
use regia_animation::motion_matching::{
    DEFAULT_TRAJECTORY_TIMES, FeatureWeights, LoadedMotionDatabase, MatcherSettings, MotionDatabases,
    MotionMatching,
};

use super::*;

const ASSET: &str = "anim/locomotion.motiondb.ron";

/// A world with one walking character on the registered procedural database, stepped `steps`
/// times at 30 Hz.
fn world(steps: u32) -> (World, Entity) {
    let set = procedural_locomotion();
    let loaded = LoadedMotionDatabase::built(
        set.build().expect("database"),
        set.skeleton.clone(),
        set.entry_clips(),
    );
    let shared: Arc<_> = loaded.database.clone();
    let mut registry = MotionDatabases::default();
    registry.insert(ASSET, loaded);
    let mut world = World::new();
    world.insert_resource(registry);
    let mut matching = MotionMatching::new(shared, MatcherSettings::default());
    matching.trajectory = arc_trajectory(&DEFAULT_TRAJECTORY_TIMES, 1.5, 0.0);
    let entity = world.spawn(matching).id();
    step(&mut world, entity, steps);
    (world, entity)
}

fn step(world: &mut World, entity: Entity, steps: u32) {
    let mut matching = world.get_mut::<MotionMatching>(entity).expect("matcher");
    for _ in 0..steps {
        matching.step(1.0 / 30.0).expect("step");
    }
}

#[test]
fn the_candidate_table_explains_the_latest_search() {
    let (world, entity) = world(30);
    let panel = MotionMatchPanel::default();
    assert_eq!(panel.target(&world), Some(entity), "the only character is picked");
    let snapshot = panel.snapshot(&world).expect("report");
    assert_eq!(snapshot.asset.as_deref(), Some(ASSET));
    assert!(snapshot.searches > 0);
    assert!(!snapshot.rows.is_empty() && snapshot.rows.len() <= DEFAULT_CANDIDATES);
    let (clip, _, _) = snapshot.playing.clone().expect("plays");
    assert!(
        clip == "walk" || clip.starts_with("turn"),
        "a straight 1.5 m/s path walks: {clip}"
    );
    for row in snapshot.rows.iter().chain(snapshot.continuation.iter()) {
        assert_eq!(row.groups.len(), 5, "the five baseline groups");
        let sum: f32 = row.groups.iter().map(|g| g.cost).sum();
        let distance = row.total - row.transition - row.bias - row.extra_bias;
        assert!(
            (sum - distance).abs() <= 1e-3 * distance.abs().max(1.0),
            "{sum} vs {distance}"
        );
    }
    // Rows are cheapest first by distance plus landing bias.
    for pair in snapshot.rows.windows(2) {
        let a = pair[0].total - pair[0].transition - pair[0].extra_bias;
        let b = pair[1].total - pair[1].transition - pair[1].extra_bias;
        assert!(a <= b + 1e-6, "{a} then {b}");
    }
    if let Some(best) = snapshot.rows.iter().find(|r| r.status == "best") {
        assert!(
            best.groups.iter().all(|g| g.class == SegmentClass::Below),
            "the best is never above itself"
        );
    }
    let json = panel.json(&world);
    assert_eq!(json["characters"][0]["asset"], ASSET);
    assert_eq!(json["report"]["rows"][0]["groups"][0]["group"], "root_velocity");
}

#[test]
fn a_zero_weight_group_is_ignored_and_a_dearer_one_is_above() {
    let (mut world, entity) = world(20);
    {
        let mut matching = world.get_mut::<MotionMatching>(entity).expect("matcher");
        let weights = FeatureWeights {
            foot_velocity: 0.0,
            ..matching.database.schema().weights
        };
        matching.set_weight_overlay(Some(weights)).expect("overlay");
    }
    step(&mut world, entity, 10);
    let snapshot = live_snapshot(&world, entity, 16).expect("report");
    assert_eq!(
        snapshot.asset.as_deref(),
        Some(ASSET),
        "the overlay still names the asset"
    );
    let mut above = 0;
    for row in &snapshot.rows {
        let foot = row
            .groups
            .iter()
            .find(|g| g.group == "foot_velocity")
            .expect("group");
        assert_eq!(foot.class, SegmentClass::Ignored);
        above += row
            .groups
            .iter()
            .filter(|g| g.class == SegmentClass::Above)
            .count();
    }
    assert!(above > 0, "some candidate pays more than the best somewhere");
}

#[test]
fn freezing_keeps_the_report_while_the_game_plays_on() {
    let (mut world, entity) = world(15);
    let mut panel = MotionMatchPanel::default();
    panel.freeze(&world, true).expect("freeze");
    let frozen = panel.snapshot(&world).expect("frozen");
    step(&mut world, entity, 90);
    assert_eq!(panel.snapshot(&world).expect("still frozen"), frozen);
    let live = live_snapshot(&world, entity, DEFAULT_CANDIDATES).expect("live");
    assert!(live.searches > frozen.searches, "the game played on");
    panel.freeze(&world, false).expect("unfreeze");
    assert_eq!(
        panel.snapshot(&world).expect("live again").searches,
        live.searches
    );
}

#[test]
fn the_database_view_lists_clips_tags_and_the_tree() {
    let (world, entity) = world(1);
    let view = database_view(&world, entity).expect("view");
    assert_eq!(view["asset"], ASSET);
    let clips = view["clips"].as_array().expect("clips");
    assert_eq!(clips.len(), 6);
    let walk = clips.iter().find(|c| c["name"] == "walk").expect("walk");
    assert_eq!(walk["tags"], json!(["walk"]));
    assert_eq!(walk["looping"], true);
    let stop = clips.iter().find(|c| c["name"] == "stop").expect("stop");
    assert_eq!(stop["distance_matched"], true);
    assert!(view["kd"]["nodes"].as_u64().unwrap_or(0) > 0);
    assert_eq!(view["groups"].as_array().map(Vec::len), Some(5));
}

#[test]
fn the_agent_methods_select_freeze_and_report_codes() {
    let (mut world, entity) = world(10);
    let other = world.spawn_empty().id();
    let out = panel_select(
        &mut world,
        &json!({ "entity": entity.to_bits(), "candidates": 4 }),
    )
    .expect("select");
    assert_eq!(out["selected"], entity.to_bits());
    assert_eq!(out["candidates"], 4);
    assert!(out["report"]["rows"].as_array().is_some_and(|r| r.len() <= 4));
    let code = |r: Result<Value, RpcError>| {
        r.err()
            .and_then(|e| e.data)
            .and_then(|d| d["reason"].as_str().map(str::to_string))
            .unwrap_or_default()
    };
    assert_eq!(
        code(panel_select(&mut world, &json!({ "entity": other.to_bits() }))),
        "mm_panel_not_found"
    );
    assert_eq!(
        code(panel_select(&mut world, &json!({ "candidates": 40 }))),
        "mm_panel_bad_candidates"
    );
    assert_eq!(
        code(panel_select(&mut world, &json!({ "entity": "x" }))),
        "mm_panel_bad_entity"
    );
    assert_eq!(code(panel_freeze(&mut world, &json!({}))), "mm_panel_bad_frozen");
    let frozen = panel_freeze(&mut world, &json!({ "frozen": true })).expect("freeze");
    assert_eq!(frozen["frozen"], true);
    let got = panel_get(&mut world, &json!({})).expect("get");
    assert_eq!(got["report"], frozen["report"]);
    let empty = panel_get(&mut World::new(), &json!({})).expect("empty world");
    assert_eq!(empty["characters"], json!([]));
    assert_eq!(empty["report"], Value::Null);
    let names: Vec<&str> = methods().iter().map(|(m, _)| m.name).collect();
    assert_eq!(
        names,
        [
            "editor.mm_panel_get",
            "editor.mm_panel_select",
            "editor.mm_panel_freeze"
        ]
    );
}

#[test]
fn the_database_tab_sorts_clips_and_tags_and_shows_the_contact_strip() {
    use regia_animation::motion_matching::gait::gait_set;
    // The procedural set has no contact labels: no strip.
    let (mut world, entity) = world(3);
    assert!(MotionMatchPanel::default().json(&world)["strip"].is_null());
    let by_name = database_view_sorted(&world, entity, ClipSort::Name, false).expect("view");
    let names: Vec<&str> = by_name["clips"]
        .as_array()
        .expect("clips")
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    let by_frames = database_view_sorted(&world, entity, ClipSort::Frames, true).expect("view");
    let frames: Vec<u64> = by_frames["tag_frames"]
        .as_array()
        .expect("tags")
        .iter()
        .filter_map(|t| t["frames"].as_u64())
        .collect();
    assert!(
        frames.windows(2).all(|w| w[0] >= w[1]),
        "descending puts the most frames first: {frames:?}"
    );
    // The agent selects the order; a bad one has its code.
    let out = panel_select(&mut world, &json!({ "sort": "duration", "descending": true })).expect("select");
    assert_eq!(out["database"]["sort"], "duration");
    assert_eq!(out["database"]["descending"], true);
    let bad = panel_select(&mut world, &json!({ "sort": "colour" })).expect_err("bad sort");
    assert_eq!(
        bad.data.as_ref().and_then(|d| d["reason"].as_str()),
        Some("mm_panel_bad_sort")
    );

    // The gait set has contacts and phases: the strip of the playing clip.
    let set = gait_set();
    let db = Arc::new(set.build().expect("gait"));
    let mut world = World::new();
    let mut matching = MotionMatching::new(db.clone(), MatcherSettings::default());
    matching.trajectory = arc_trajectory(&DEFAULT_TRAJECTORY_TIMES, 1.5, 0.0);
    let entity = world.spawn(matching).id();
    step(&mut world, entity, 10);
    let json = MotionMatchPanel::default().json(&world);
    let strip = &json["strip"];
    let playing = world
        .get::<MotionMatching>(entity)
        .and_then(|m| m.matcher.playback())
        .expect("playing");
    let range = &db.clips()[playing.clip];
    assert_eq!(strip["contacts"].as_array().map(Vec::len), Some(range.frames));
    assert_eq!(strip["feet"], 2);
    assert_eq!(strip["current"], playing.frame - range.start);
    assert!(strip["phases"][0][0].is_number(), "{strip}");
    assert_eq!(json["database"]["contacts"], true);
}
