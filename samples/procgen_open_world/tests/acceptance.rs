// SPDX-License-Identifier: MIT-0
//! Spec 66 § 8 acceptance through the agent protocol on the headless renderer (lavapipe here): the
//! camera flies a scripted route in fixed steps, `procgen.tiles_wait` succeeds at each stop,
//! `procgen.tiles` never shows more than the memory cap and no failed cell, and cells behind the
//! camera are released. The hitch report and the Pixel 6 run need real hardware (lead or owner); no
//! golden (`cargo run -p procgen_open_world -- --out <dir>` writes the review shots).

use procgen_common::{call, drive, generator, shot};
use procgen_open_world::{GENERATOR, MEMORY_CAP_MB, SPEED_M_S, route_length};
use regia_test_utils::unique_temp_dir;
use serde_json::json;

#[test]
fn the_camera_flies_the_route_and_cells_follow_it() {
    let project = unique_temp_dir("procgen-open-world");
    let Some(app) = procgen_open_world::app(&project) else {
        return; // no Vulkan device at all
    };
    let review = std::env::var_os("PROCGEN_SAMPLE_SHOTS").map(std::path::PathBuf::from);
    drive(app, &project.clone(), move |client| {
        let e = generator(client, GENERATOR);
        let stops = 6usize;
        let frames = (route_length() / SPEED_M_S / stops as f32 * 60.0) as u32;
        let mut seen = std::collections::BTreeSet::new();
        for k in 0..=stops {
            if k > 0 {
                call(client, "play.step", json!({ "frames": frames }));
            }
            let wait = call(
                client,
                "procgen.tiles_wait",
                json!({ "entity": e, "timeout_ms": 120_000 }),
            );
            assert_eq!(wait["ready"], true, "stop {k}: {wait}");
            let tiles = call(client, "procgen.tiles", json!({ "entity": e }));
            assert!(tiles["error"].is_null(), "{tiles}");
            let memory = tiles["memory_mb"].as_f64().expect("memory");
            assert!(memory <= f64::from(MEMORY_CAP_MB), "stop {k}: {memory} MB");
            let cells = tiles["cells"].as_array().expect("cells");
            assert!(
                !cells.iter().any(|c| c["state"] == "failed"),
                "stop {k}: a failed cell"
            );
            let resident: Vec<(i64, i64, i64)> = cells
                .iter()
                .filter(|c| c["state"] == "resident")
                .map(|c| {
                    (
                        c["tier"].as_i64().unwrap_or(0),
                        c["i"].as_i64().unwrap_or(0),
                        c["j"].as_i64().unwrap_or(0),
                    )
                })
                .collect();
            assert!(resident.len() > 8, "stop {k}: {} resident", resident.len());
            assert!(
                cells.iter().any(|c| c["instances"].as_u64() > Some(100)),
                "stop {k}: cells hold instances"
            );
            seen.extend(resident);
            let name = format!("waypoint_{k}.png");
            let size = shot(
                client,
                review.as_deref().map(|d| d.join(&name)).as_deref(),
                320,
                180,
            );
            assert!(size > 500, "a real image: {size} bytes");
        }
        // The route crosses most of the map: the cells seen far outnumber what is resident at once.
        let last = call(
            client,
            "procgen.tiles",
            json!({ "entity": e, "state": "resident" }),
        );
        let now = last["cells"].as_array().map_or(0, Vec::len);
        assert!(seen.len() > now * 2, "{} seen, {now} resident now", seen.len());
        let draw = call(client, "procgen.list", json!({}))["draw"].clone();
        assert!(draw["drawn"].as_u64() > Some(100), "{draw}");
        assert_eq!(draw["missing_meshes"], json!([]), "{draw}");
    });
    let _ = std::fs::remove_dir_all(&project);
}
