// SPDX-License-Identifier: MIT-0
//! Spec 65 § 12 acceptance through the agent protocol on the headless renderer (lavapipe here):
//! `procgen.run` → ready, `biome.stats` shows placed counts per entry and removals per filter,
//! `biome.claim` names the expected biome at three known points, and the road is clear of every
//! instance. No golden (owner look review): `cargo run -p procgen_biomes -- --out <dir>`.

use procgen_common::{call, drive, generator, shot};
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

fn owner(client: &mut regia_agent::client::Client, e: u64, point: [f32; 3]) -> String {
    let c = call(client, "biome.claim", json!({ "point": point, "generator": e }));
    c["owner"]["biome"].as_str().unwrap_or_default().to_string()
}

fn counter(entry: &Value, name: &str) -> u64 {
    entry[name].as_u64().unwrap_or(0)
}

#[test]
fn three_biomes_place_filter_and_clear_the_road_through_the_agent() {
    let project = unique_temp_dir("procgen-biomes");
    let Some(app) = procgen_biomes::app(&project) else {
        return; // no Vulkan device at all
    };
    let review = std::env::var_os("PROCGEN_SAMPLE_SHOTS").map(std::path::PathBuf::from);
    drive(app, &project.clone(), move |client| {
        let e = generator(client, procgen_biomes::GENERATOR);
        let run = call(client, "procgen.run", json!({ "entity": e, "wait": true }));
        assert_eq!(run["state"], "ready", "{run}");

        let stats = call(client, "biome.stats", json!({ "generator": e }));
        let entries = stats["entries"].as_object().expect("entries");
        for name in [
            "Pine forest/canopy/pine",
            "Pine forest/canopy/fern",
            "Meadow/flowers/flower",
            "Meadow/shrubs/shrub",
            "Riverbed/stones/stone",
        ] {
            let entry = &entries.get(name).unwrap_or_else(|| panic!("no {name}: {stats}"));
            assert!(counter(entry, "placed") > 0, "{name}: {entry}");
        }
        let pine = &entries["Pine forest/canopy/pine"];
        let removed: u64 = pine["removed_by_filter"]
            .as_object()
            .map_or(0, |o| o.values().filter_map(Value::as_u64).sum());
        assert!(removed > 0, "the pine's filters removed points: {pine}");
        assert!(counter(pine, "excluded") > 0, "the road cleared pines: {pine}");

        assert_eq!(owner(client, e, procgen_biomes::IN_FOREST), "Pine forest");
        assert_eq!(owner(client, e, procgen_biomes::IN_MEADOW), "Meadow");
        assert_eq!(owner(client, e, procgen_biomes::ON_RIVER), "Riverbed");

        call(client, "play.step", json!({ "frames": 2 }));
        let draw = call(client, "procgen.list", json!({}))["draw"].clone();
        assert!(draw["drawn"].as_u64() > Some(1000), "{draw}");
        assert_eq!(
            draw["generated_meshes"], 2,
            "the road carriageway and its shoulders: {draw}"
        );
        assert_eq!(draw["missing_meshes"], json!([]), "{draw}");
        let size = shot(
            client,
            review.as_deref().map(|d| d.join("biomes.png")).as_deref(),
            480,
            270,
        );
        assert!(size > 1000, "a real image: {size} bytes");
    });
    let _ = std::fs::remove_dir_all(&project);
}
