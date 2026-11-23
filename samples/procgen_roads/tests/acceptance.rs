// SPDX-License-Identifier: MIT-0
//! Spec 64 § 9 acceptance through the agent protocol on the headless renderer (lavapipe here):
//! `procgen.run` → ready, `procgen.report` lists 2 junctions, the meshes draw, the terrain was
//! flattened, a bake reloads, `seeing.screenshot` answers an image. No golden (lead TODO if one is
//! wanted): `cargo run -p procgen_roads -- --out <dir>` writes the review shot.

use procgen_common::{call, drive, generator, shot};
use regia_test_utils::unique_temp_dir;
use serde_json::json;

#[test]
fn the_road_network_runs_bakes_and_draws_through_the_agent() {
    let project = unique_temp_dir("procgen-roads");
    let bake_dir = project.join("level.procgen");
    let Some(app) = procgen_roads::app(&project, Some(bake_dir.clone())) else {
        return; // no Vulkan device at all
    };
    let review = std::env::var_os("PROCGEN_SAMPLE_SHOTS").map(std::path::PathBuf::from);
    drive(app, &project.clone(), move |client| {
        let e = generator(client, procgen_roads::GENERATOR);
        let run = call(client, "procgen.run", json!({ "entity": e, "wait": true }));
        assert_eq!(run["state"], "ready", "{run}");

        let report = call(client, "procgen.report", json!({ "entity": e }));
        assert_eq!(report["totals"]["junctions"], 2, "{}", report["totals"]);
        let roads = report["roads"].as_array().expect("roads");
        assert_eq!(roads.len(), 3, "{roads:?}");
        assert!(roads.iter().all(|r| r["length_m"].as_f64() > Some(100.0)));
        assert_eq!(
            roads.iter().filter_map(|r| r["junctions"].as_u64()).sum::<u64>(),
            4,
            "a crossing touches two roads, the T two: {roads:?}"
        );
        assert!(
            roads.iter().all(|r| r["flatten_rect"].is_array()),
            "every road flattened: {roads:?}"
        );

        call(client, "play.step", json!({ "frames": 2 }));
        let list = call(client, "procgen.list", json!({}));
        let draw = &list["draw"];
        assert!(draw["drawn"].as_u64() > Some(500), "{draw}");
        assert!(draw["generated_meshes"].as_u64() >= Some(3), "{draw}");
        assert_eq!(draw["missing_meshes"], json!([]), "{draw}");
        assert_eq!(draw["missing_materials"], json!([]), "{draw}");

        let size = shot(
            client,
            review.as_deref().map(|d| d.join("roads.png")).as_deref(),
            480,
            270,
        );
        assert!(size > 1000, "a real image: {size} bytes");

        // The bake writes meshes and the terrain edit; verify re-runs against the original ground.
        let bake = call(client, "procgen.bake", json!({ "entity": e }));
        assert_eq!(bake["ok"], true, "{bake}");
        let verify = call(client, "procgen.verify", json!({ "entity": e }));
        assert_eq!(verify["ok"], true, "{verify}");
        let cleared = call(client, "procgen.clear", json!({ "entity": e }));
        assert_eq!(cleared["cleared"], true);
    });
    assert!(bake_dir.is_dir());
    let _ = std::fs::remove_dir_all(&project);
}
