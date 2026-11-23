// SPDX-License-Identifier: MPL-2.0
//! Biome editor tests (w11-biome-procgen-tooling item 1): ops write the bytes `BiomeDef::to_ron`
//! gives the expected definition; the runtime loader accepts every state; a 50-op random sequence
//! then a full undo gives the original bytes back; renames cascade; refused edits change nothing;
//! the comment-loss warning; coded errors; headless drawing.

use std::path::{Path, PathBuf};

use regia_procgen::biome::def::{
    BiomeChannel, BiomeEntry, BiomeGrass, BiomeLayer, CandidateSource, ChannelRule, ChildRule, EntryFilter,
    EntrySpawn, ScaleRule,
};
use serde_json::{Value, json};

use super::*;
use crate::data_panels::test_ui::draw_frames;

const REL: &str = "assets/biomes/forest.biome.ron";

/// A small valid biome: layer `trees` (channels `steep` and `rest`) with `oak` (children `fern`)
/// and the child-only `fern`; layer `ground` (Poisson); one grass class.
fn fixture() -> BiomeDef {
    let mut oak = BiomeEntry::new("oak", EntrySpawn::Mesh("meshes/oak.glb".into()));
    oak.channels = vec!["rest".into()];
    oak.filters = vec![EntryFilter::Slope {
        min_deg: 0.0,
        max_deg: 30.0,
        feather: 5.0,
    }];
    oak.children = vec![ChildRule {
        entry: "fern".into(),
        count: (1, 3),
        radius_m: (0.5, 2.0),
        rate: 1.0,
        keep_clear_of_parent: regia_procgen::biome::def::KeepClear::Core,
        scale_with_parent: false,
        graph: None,
    }];
    let mut fern = BiomeEntry::new("fern", EntrySpawn::Mesh("meshes/fern.glb".into()));
    fern.weight = 0.0;
    let mut trees = BiomeLayer::new("trees");
    trees.channels = vec![
        BiomeChannel {
            name: "steep".into(),
            rule: ChannelRule::Slope {
                min_deg: 35.0,
                max_deg: 90.0,
            },
        },
        BiomeChannel {
            name: "rest".into(),
            rule: ChannelRule::Rest,
        },
    ];
    trees.entries = vec![oak, fern];
    let mut ground = BiomeLayer::new("ground");
    ground.source = CandidateSource::Poisson { min_dist_m: 1.5 };
    ground.entries = vec![BiomeEntry::new(
        "rock",
        EntrySpawn::Prefab("prefabs/rock.prefab.ron".into()),
    )];
    BiomeDef {
        name: "Forest".into(),
        color: (0.1, 0.6, 0.2),
        priority: 10,
        layers: vec![trees, ground],
        grass: vec![BiomeGrass {
            class: "meadow".into(),
            density: 0.5,
            outside: regia_procgen::biome::def::GrassOutside::Keep,
        }],
        ..BiomeDef::default()
    }
}

fn project(name: &str, text: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(name);
    std::fs::create_dir_all(dir.join("assets/biomes")).expect("dir");
    std::fs::write(dir.join(REL), text).expect("write");
    dir
}

fn world_with(dir: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.to_path_buf()),
    });
    world
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

fn edit(world: &mut World, params: Value) -> Result<Value, RpcError> {
    call(world, "editor.biome_panel_edit", params)
}

fn panel_def(world: &World) -> BiomeDef {
    world
        .resource::<BiomePanel>()
        .def
        .clone()
        .expect("a definition is open")
}

/// Acceptance: the bytes the ops write are `to_ron` of the definition the same changes give when
/// made on the struct, and the runtime loader reads them back to it.
#[test]
fn ops_write_the_bytes_of_the_expected_definition() {
    let dir = project("biome_panel_bytes", &fixture().to_ron());
    let mut world = world_with(&dir);
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open");
    let ops = [
        json!({ "op": "set", "path": "name", "value": "Old forest" }),
        json!({ "op": "set", "path": "color", "value": [0.2, 0.5, 0.1] }),
        json!({ "op": "set", "path": "map_key", "value": [10, 200, 30] }),
        json!({ "op": "set", "path": "priority", "value": 25 }),
        json!({ "op": "set", "path": "layers/trees/rank", "value": 2 }),
        json!({ "op": "set", "path": "layers/trees/source/Ground/spacing_m", "value": 6.0 }),
        json!({ "op": "set", "path": "layers/trees/source/Ground/noise", "value": { "scale_m": 80.0, "min": 0.2 } }),
        json!({ "op": "set", "path": "layers/trees/entries/oak/weight", "value": 2.5 }),
        json!({ "op": "set", "path": "layers/trees/entries/oak/filters/0/Slope/max_deg", "value": 25.0 }),
        json!({ "op": "add", "path": "layers/trees/entries/oak/filters", "value": { "Altitude": { "min": 10.0, "max": 400.0, "feather": 20.0 } } }),
        json!({ "op": "set", "path": "layers/trees/entries/oak/placement/scale", "value": { "Range": [0.8, 1.3] } }),
        json!({ "op": "set", "path": "layers/trees/entries/oak/render/cast_shadows", "value": false }),
        json!({ "op": "set", "path": "layers/trees/entries/oak/footprint/core_m", "value": 0.6 }),
        json!({ "op": "set", "path": "layers/trees/entries/oak/children/0/count", "value": [2, 4] }),
        json!({ "op": "set", "path": "layers/trees/channels/steep/rule", "value": { "Altitude": { "min": 500.0 } } }),
        json!({ "op": "add", "path": "layers", "value": { "name": "shrubs" }, "index": 1 }),
        json!({ "op": "add", "path": "layers/shrubs/entries", "value": { "name": "gorse", "spawn": { "Mesh": "meshes/gorse.glb" } } }),
        json!({ "op": "move", "path": "layers/ground", "to": 0 }),
        json!({ "op": "remove", "path": "grass/meadow" }),
        json!({ "op": "add", "path": "grass", "value": { "class": "moss", "density": 1.5, "outside": "Clear" } }),
    ];
    for op in &ops {
        edit(&mut world, op.clone()).unwrap_or_else(|e| panic!("{op}: {e:?}"));
    }
    let mut expected = fixture();
    expected.name = "Old forest".into();
    expected.color = (0.2, 0.5, 0.1);
    expected.map_key = Some((10, 200, 30));
    expected.priority = 25;
    {
        let trees = &mut expected.layers[0];
        trees.rank = 2;
        trees.source = CandidateSource::Ground {
            spacing_m: 6.0,
            jitter: 0.8,
            noise: Some(regia_procgen::biome::def::SourceNoise {
                scale_m: 80.0,
                octaves: 3,
                min: 0.2,
                max: 1.0,
            }),
        };
        let oak = &mut trees.entries[0];
        oak.weight = 2.5;
        oak.filters[0] = EntryFilter::Slope {
            min_deg: 0.0,
            max_deg: 25.0,
            feather: 5.0,
        };
        oak.filters.push(EntryFilter::Altitude {
            min: 10.0,
            max: 400.0,
            feather: 20.0,
        });
        oak.placement.scale = ScaleRule::Range(0.8, 1.3);
        oak.render.cast_shadows = false;
        oak.footprint.core_m = Some(0.6);
        oak.children[0].count = (2, 4);
        trees.channels[0].rule = ChannelRule::Altitude {
            min: 500.0,
            max: 1.0e9,
        };
    }
    let mut shrubs = BiomeLayer::new("shrubs");
    shrubs.entries.push(BiomeEntry::new(
        "gorse",
        EntrySpawn::Mesh("meshes/gorse.glb".into()),
    ));
    expected.layers.insert(1, shrubs);
    let ground = expected.layers.remove(2);
    expected.layers.insert(0, ground);
    expected.grass = vec![BiomeGrass {
        class: "moss".into(),
        density: 1.5,
        outside: regia_procgen::biome::def::GrassOutside::Clear,
    }];
    assert_eq!(panel_def(&world), expected);
    let state = call(&mut world, "editor.biome_panel_get", json!({})).expect("get");
    assert_eq!(state["dirty"], json!(true));
    assert_eq!(state["undo_depth"], json!(ops.len()));
    let saved = call(&mut world, "editor.biome_panel_save", json!({})).expect("save");
    assert_eq!(saved["written"], json!(REL));
    let bytes = std::fs::read_to_string(dir.join(REL)).expect("saved file");
    assert_eq!(
        bytes,
        expected.to_ron(),
        "the op bytes equal to_ron of the expected definition"
    );
    assert_eq!(
        BiomeDef::from_ron(&bytes),
        Ok(expected),
        "the runtime loader reads it back"
    );
    assert_eq!(
        call(&mut world, "editor.biome_panel_get", json!({})).expect("get")["dirty"],
        json!(false)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A small deterministic generator (xorshift64*), so the sequence is the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % (n.max(1) as u64)).unwrap_or(0)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// One random edit of `def` (valid or not: refused ones must change nothing).
fn random_op(rng: &mut Rng, def: &BiomeDef, k: usize) -> Value {
    let li = rng.below(def.layers.len());
    let layer = def.layers.get(li);
    let lp = layer.map_or_else(|| "layers/0".to_owned(), |l| format!("layers/{}", l.name));
    let entry = layer.and_then(|l| l.entries.get(rng.below(l.entries.len())));
    let ep = entry.map_or_else(
        || format!("{lp}/entries/0"),
        |e| format!("{lp}/entries/{}", e.name),
    );
    match rng.below(16) {
        0 => json!({ "op": "set", "path": "name", "value": format!("biome {k}") }),
        1 => json!({ "op": "set", "path": "priority", "value": rng.below(100) as i64 - 50 }),
        2 => json!({ "op": "set", "path": "color", "value": [rng.unit(), rng.unit(), rng.unit()] }),
        3 => json!({ "op": "add", "path": "layers", "value": { "name": format!("layer {k}") } }),
        4 => json!({ "op": "remove", "path": lp }),
        5 => {
            json!({ "op": "add", "path": format!("{lp}/entries"), "value": { "name": format!("entry {k}") } })
        }
        6 => json!({ "op": "set", "path": format!("{ep}/weight"), "value": rng.unit() * 3.0 }),
        7 => json!({ "op": "set", "path": format!("{ep}/name"), "value": format!("renamed {k}") }),
        8 => {
            json!({ "op": "add", "path": format!("{ep}/filters"), "value": { "Altitude": { "min": rng.unit() * 10.0, "max": 100.0 } } })
        }
        9 => json!({ "op": "set", "path": format!("{lp}/rank"), "value": rng.below(5) }),
        10 => {
            json!({ "op": "set", "path": format!("{lp}/sharing"), "value": if rng.below(2) == 0 { "Shared" } else { "Exclusive" } })
        }
        11 => {
            json!({ "op": "set", "path": format!("{lp}/source"), "value": { "Poisson": { "min_dist_m": 0.5 + rng.unit() } } })
        }
        12 => json!({ "op": "move", "path": lp, "to": rng.below(def.layers.len() + 1) }),
        13 => json!({ "op": "set", "path": format!("{ep}/placement/upright"), "value": rng.unit() * 1.5 }),
        14 => {
            json!({ "op": "add", "path": "grass", "value": { "class": format!("grass {k}"), "density": rng.unit() } })
        }
        _ => json!({ "op": "set", "path": format!("{ep}/weight"), "value": "heavy" }),
    }
}

/// Acceptance: a 50-op random sequence: every accepted state saves to a text the runtime loader
/// (`BiomeDef::from_ron`) accepts, a refused op changes nothing, and undoing everything gives the
/// original bytes back (and redoing everything the final ones).
#[test]
fn fifty_random_ops_then_full_undo_return_the_original_bytes() {
    let original = fixture().to_ron();
    let dir = project("biome_panel_random", &original);
    let mut world = world_with(&dir);
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open");
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut applied, mut refused, mut k) = (0usize, 0usize, 0usize);
    while applied < 50 {
        k += 1;
        assert!(k < 10_000, "the generator keeps producing accepted ops");
        let before = panel_def(&world);
        let depth = world.resource::<BiomePanel>().undo_depth();
        let op = random_op(&mut rng, &before, k);
        match edit(&mut world, op.clone()) {
            Ok(_) => {
                let now = panel_def(&world);
                if now != before {
                    applied += 1;
                }
                let text = now.to_ron();
                assert_eq!(
                    BiomeDef::from_ron(&text).as_ref(),
                    Ok(&now),
                    "after {op}: the runtime loader accepts the state"
                );
            }
            Err(e) => {
                refused += 1;
                assert!(!reason(&e).is_empty(), "{op}: a refusal has a code: {e:?}");
                assert_eq!(panel_def(&world), before, "{op}: a refused op changes nothing");
                assert_eq!(world.resource::<BiomePanel>().undo_depth(), depth);
            }
        }
    }
    assert!(refused > 0, "the sequence also exercised refusals");
    let last = panel_def(&world).to_ron();
    let mut undone = 0;
    while edit(&mut world, json!({ "op": "undo" })).is_ok() {
        undone += 1;
    }
    assert_eq!(undone, 50);
    assert!(!world.resource::<BiomePanel>().dirty(), "back to the file");
    // Saving the fully undone definition writes the original bytes.
    call(&mut world, "editor.biome_panel_save", json!({})).expect("save");
    assert_eq!(std::fs::read_to_string(dir.join(REL)).expect("file"), original);
    while edit(&mut world, json!({ "op": "redo" })).is_ok() {}
    assert_eq!(panel_def(&world).to_ron(), last, "redo replays every step");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Renaming an entry renames the children that name it; renaming a channel renames it in the
/// layer's entries; the result stays valid.
#[test]
fn renames_follow_into_children_and_channels() {
    let dir = project("biome_panel_rename", &fixture().to_ron());
    let mut world = world_with(&dir);
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open");
    edit(
        &mut world,
        json!({ "op": "set", "path": "layers/trees/entries/fern/name", "value": "bracken" }),
    )
    .expect("rename entry");
    edit(
        &mut world,
        json!({ "op": "set", "path": "layers/trees/channels/rest/name", "value": "flat" }),
    )
    .expect("rename channel");
    let def = panel_def(&world);
    let oak = &def.layers[0].entries[0];
    assert_eq!(oak.children[0].entry, "bracken");
    assert_eq!(oak.channels, vec!["flat".to_string()]);
    assert!(def.validate().is_empty(), "{:?}", def.validate());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Refused edits change nothing and answer their codes.
#[test]
fn refused_edits_change_nothing_and_answer_their_codes() {
    let dir = project("biome_panel_refused", &fixture().to_ron());
    let mut world = world_with(&dir);
    let none = edit(&mut world, json!({ "op": "undo" })).expect_err("nothing open");
    assert_eq!(reason(&none), "biome_panel_no_biome");
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open");
    let before = panel_def(&world);
    let cases = [
        (json!({ "op": "spin" }), "biome_panel_bad_op"),
        (
            json!({ "op": "set", "path": "layers/trees/entries/oak/weight", "value": "x" }),
            "biome_panel_bad_value",
        ),
        (
            json!({ "op": "set", "path": "layers/nope/rank", "value": 1 }),
            "biome_panel_bad_field",
        ),
        (
            json!({ "op": "set", "path": "format_version", "value": 2 }),
            "biome_panel_read_only",
        ),
        (
            json!({ "op": "add", "path": "name", "value": "x" }),
            "biome_panel_not_a_list",
        ),
        (
            json!({ "op": "set", "path": "layers/trees/entries/oak/weight", "value": -1.0 }),
            "biome_invalid",
        ),
        (
            json!({ "op": "add", "path": "layers", "value": { "name": "trees" } }),
            "biome_invalid",
        ),
        (
            json!({ "op": "add", "path": "layers/trees/entries/oak/children", "value": { "entry": "ghost" } }),
            "biome_entry_unknown",
        ),
        (
            json!({ "op": "move", "path": "layers/trees", "to": 9 }),
            "biome_panel_bad_value",
        ),
        (json!({ "op": "redo" }), "biome_panel_nothing_to_redo"),
        (json!({ "op": "undo" }), "biome_panel_nothing_to_undo"),
    ];
    for (op, code) in cases {
        let e = edit(&mut world, op.clone()).expect_err("refused");
        assert_eq!(reason(&e), code, "{op}: {e:?}");
        assert_eq!(panel_def(&world), before, "{op} changed nothing");
    }
    let state = call(&mut world, "editor.biome_panel_get", json!({})).expect("get");
    assert_eq!(state["error"]["code"], json!("biome_panel_nothing_to_undo"));
    assert_eq!(state["undo_depth"], json!(0));
    // Paths outside the project or of another kind are refused.
    for path in ["../x.biome.ron", "assets/biomes/x.ron", "/tmp/a.biome.ron"] {
        let e = call(&mut world, "editor.biome_panel_open", json!({ "path": path })).expect_err("bad path");
        assert_eq!(reason(&e), "biome_panel_bad_path", "{path}");
    }
    let e = call(
        &mut world,
        "editor.biome_panel_open",
        json!({ "path": "assets/biomes/missing.biome.ron" }),
    )
    .expect_err("missing");
    assert_eq!(reason(&e), "biome_panel_file_not_found");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A drag (`merge` on the same path) is one undo step; another path starts a new one.
#[test]
fn a_drag_is_one_undo_step() {
    let dir = project("biome_panel_merge", &fixture().to_ron());
    let mut world = world_with(&dir);
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open");
    for v in [1.1, 1.2, 1.3, 1.4] {
        edit(
            &mut world,
            json!({ "op": "set", "path": "layers/trees/entries/oak/weight", "value": v, "merge": true }),
        )
        .expect("drag");
    }
    assert_eq!(world.resource::<BiomePanel>().undo_depth(), 1);
    edit(
        &mut world,
        json!({ "op": "set", "path": "priority", "value": 3, "merge": true }),
    )
    .expect("another field");
    assert_eq!(world.resource::<BiomePanel>().undo_depth(), 2);
    edit(&mut world, json!({ "op": "undo" })).expect("undo");
    edit(&mut world, json!({ "op": "undo" })).expect("undo");
    assert_eq!(panel_def(&world), fixture(), "one undo per drag");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file with RON comments opens with the warning that a save loses them; after the save the
/// warning is gone. A file with rule issues opens with them listed and saves only once fixed.
#[test]
fn comments_are_counted_and_warned_and_issues_block_the_save() {
    let commented = format!(
        "// Forest biome, hand tuned\n{}\n/* end */\n",
        fixture()
            .to_ron()
            .replacen("name: \"Forest\"", "name: \"Forest // not a comment\"", 1)
    );
    let dir = project("biome_panel_comments", &commented);
    let mut world = world_with(&dir);
    let state = call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open");
    assert_eq!(state["comments"], json!(2), "{state}");
    assert_eq!(state["canonical"], json!(false));
    assert!(
        state["warning"]
            .as_str()
            .is_some_and(|w| w.contains("2 RON comments")),
        "{state}"
    );
    edit(
        &mut world,
        json!({ "op": "set", "path": "priority", "value": 11 }),
    )
    .expect("edit");
    call(&mut world, "editor.biome_panel_save", json!({})).expect("save");
    let state = call(&mut world, "editor.biome_panel_get", json!({})).expect("get");
    assert_eq!(state["comments"], json!(0));
    assert_eq!(state["warning"], Value::Null);
    // An invalid file opens (issues listed) and refuses to save until fixed.
    let mut bad = fixture();
    bad.layers[0].entries[0].weight = -2.0;
    std::fs::write(dir.join(REL), bad.to_ron()).expect("write");
    let state = call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open invalid");
    assert_eq!(state["issues"][0]["code"], json!("biome_invalid"), "{state}");
    let e = call(&mut world, "editor.biome_panel_save", json!({})).expect_err("issues");
    assert_eq!(reason(&e), "biome_invalid");
    // An edit elsewhere is allowed (it adds no issue); the fix clears it and the save goes through.
    edit(&mut world, json!({ "op": "set", "path": "priority", "value": 4 })).expect("unrelated edit");
    edit(
        &mut world,
        json!({ "op": "set", "path": "layers/trees/entries/oak/weight", "value": 1.0 }),
    )
    .expect("fix");
    call(&mut world, "editor.biome_panel_save", json!({})).expect("save");
    assert!(BiomeDef::from_ron(&std::fs::read_to_string(dir.join(REL)).expect("file")).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ron_comments_outside_strings_are_counted() {
    assert_eq!(ron_comment_count("(a: 1)"), 0);
    assert_eq!(
        ron_comment_count("// one\n(a: \"// no\", /* two */ b: 2) // three"),
        3
    );
    assert_eq!(ron_comment_count("(a: \"\\\" // still a string\")"), 0);
    assert_eq!(ron_comment_count("/* outer /* nested */ still */ (a: 1)"), 1);
}

/// `new` starts an unsaved biome named after the file; the save writes a file the loader reads.
#[test]
fn a_new_biome_saves_a_loadable_file() {
    let dir = regia_test_utils::unique_temp_dir("biome_panel_new");
    let mut world = world_with(&dir);
    let state = call(
        &mut world,
        "editor.biome_panel_open",
        json!({ "path": "assets/biomes/alpine.biome.ron", "new": true }),
    )
    .expect("new");
    assert_eq!(state["dirty"], json!(true));
    assert_eq!(state["def"]["name"], json!("alpine"));
    edit(
        &mut world,
        json!({ "op": "add", "path": "layers", "value": { "name": "pines" } }),
    )
    .expect("layer");
    call(&mut world, "editor.biome_panel_save", json!({})).expect("save");
    let text = std::fs::read_to_string(dir.join("assets/biomes/alpine.biome.ron")).expect("file");
    let def = BiomeDef::from_ron(&text).expect("loads");
    assert_eq!(def.layers[0].name, "pines");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Content ▸ New ▸ Biome writes a file named after itself that loads and opens in the panel.
#[test]
fn content_new_biome_creates_a_loadable_file() {
    use crate::content::{NewAssetKind, create_asset};
    let dir = regia_test_utils::unique_temp_dir("biome_panel_content_new");
    assert!(NewAssetKind::ALL.contains(&NewAssetKind::Biome));
    assert_eq!(NewAssetKind::parse("biome"), Ok(NewAssetKind::Biome));
    let made = create_asset(&dir, NewAssetKind::Biome, "assets/biomes/marsh.biome.ron").expect("create");
    assert_eq!(made, "assets/biomes/marsh.biome.ron");
    let text = std::fs::read_to_string(dir.join(&made)).expect("file");
    let def = BiomeDef::from_ron(&text).expect("the runtime loader reads it");
    assert_eq!(def.name, "marsh");
    assert_eq!(text, def.to_ron(), "written in the canonical form");
    assert!(create_asset(&dir, NewAssetKind::Biome, "assets/biomes/x.level.ron").is_err());
    let mut world = world_with(&dir);
    call(&mut world, "editor.biome_panel_open", json!({ "path": made })).expect("opens");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The panel draws headless (empty, open, with issues) and its buttons' edits apply in the frame.
#[test]
fn the_panel_draws_headless() {
    let dir = project("biome_panel_draw", &fixture().to_ron());
    let mut world = world_with(&dir);
    let shapes = draw_frames(&mut world, 2, biome_panel);
    assert!(!shapes.is_empty());
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open");
    let shapes = draw_frames(&mut world, 3, biome_panel);
    assert!(!shapes.is_empty());
    assert_eq!(panel_def(&world), fixture(), "drawing changes nothing");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review B3 (fail-first): Open and New over unsaved edits are refused (`biome_panel_unsaved`,
/// nothing changes) unless `discard`; the form asks for a second click, and `KeepEdits` keeps them.
#[test]
fn open_and_new_over_unsaved_edits_need_discard() {
    let dir = project("biome_panel_unsaved", &fixture().to_ron());
    std::fs::write(dir.join("assets/biomes/other.biome.ron"), fixture().to_ron()).expect("other");
    let mut world = world_with(&dir);
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open");
    edit(
        &mut world,
        json!({ "op": "set", "path": "priority", "value": 77 }),
    )
    .expect("edit");
    let edited = panel_def(&world);
    for params in [
        json!({ "path": "assets/biomes/other.biome.ron" }),
        json!({ "path": "assets/biomes/alpine.biome.ron", "new": true }),
        json!({ "path": REL }),
    ] {
        let e = call(&mut world, "editor.biome_panel_open", params.clone()).expect_err("unsaved");
        assert_eq!(reason(&e), "biome_panel_unsaved", "{params}");
        let panel = world.resource::<BiomePanel>();
        assert_eq!(panel_def(&world), edited, "{params}: the edits survive");
        assert_eq!(panel.path.as_deref(), Some(REL));
        assert_eq!(panel.undo_depth(), 1);
    }
    // The form: the first click is refused and offers the second; Keep editing forgets it.
    let project_dir = Some(dir.as_path());
    let mut panel = world.remove_resource::<BiomePanel>().expect("panel");
    let other = "assets/biomes/other.biome.ron".to_string();
    panel.apply_form_frame(project_dir, FormAction::Open(other.clone(), false), &[], false);
    assert_eq!(panel.def.as_ref(), Some(&edited));
    assert_eq!(panel.pending_discard, Some((other.clone(), false)));
    assert_eq!(
        panel.error.as_ref().map(|e| e.code.as_str()),
        Some("biome_panel_unsaved")
    );
    panel.path_input.clone_from(&other);
    panel.apply_form_frame(project_dir, FormAction::KeepEdits, &[], false);
    assert_eq!(panel.pending_discard, None);
    assert_eq!(panel.path_input, REL, "the picker shows the open file again");
    assert_eq!(panel.def.as_ref(), Some(&edited));
    // The second click discards and opens.
    panel.apply_form_frame(project_dir, FormAction::New(other.clone(), false), &[], false);
    assert_eq!(panel.pending_discard, Some((other.clone(), true)));
    panel.apply_form_frame(project_dir, FormAction::Open(other.clone(), true), &[], false);
    assert_eq!(panel.path.as_deref(), Some(other.as_str()));
    assert_eq!(panel.def.as_ref(), Some(&fixture()));
    assert_eq!(panel.pending_discard, None);
    assert!(panel.error.is_none());
    world.insert_resource(panel);
    // A clean panel opens without discard; the agent's discard drops edits.
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("clean open");
    edit(&mut world, json!({ "op": "set", "path": "priority", "value": 5 })).expect("edit");
    call(
        &mut world,
        "editor.biome_panel_open",
        json!({ "path": REL, "discard": true }),
    )
    .expect("discard");
    assert_eq!(panel_def(&world), fixture());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review B4 (fail-first): with layers named `1` and `0`, `#<index>` (the form's paths) names the
/// element by index, and a plain name still names by name.
#[test]
fn index_paths_are_not_taken_for_numeric_names() {
    let mut def = fixture();
    def.layers[0].name = "1".into();
    def.layers[1].name = "0".into();
    let dir = project("biome_panel_numeric_names", &def.to_ron());
    let mut world = world_with(&dir);
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open");
    // The form's path of the second layer (index 1, named `0`): `index_seg` (checked by `the_form_addresses_list_elements_by_index_segment`).
    let second = "layers/#1".to_string();
    edit(
        &mut world,
        json!({ "op": "set", "path": format!("{second}/rank"), "value": 7 }),
    )
    .expect("set by index");
    let now = panel_def(&world);
    assert_eq!(now.layers[1].rank, 7, "the layer at index 1 changed");
    assert_eq!(now.layers[0].rank, def.layers[0].rank, "not the layer named `1`");
    edit(&mut world, json!({ "op": "remove", "path": second })).expect("remove by index");
    let now = panel_def(&world);
    assert_eq!(
        now.layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
        vec!["1"],
        "the layer at index 1 (named `0`) went"
    );
    // A plain segment is still the name first.
    edit(
        &mut world,
        json!({ "op": "set", "path": "layers/1/rank", "value": 3 }),
    )
    .expect("by name");
    assert_eq!(panel_def(&world).layers[0].rank, 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review B5 (fail-first): two form drags on one field, with the pointer released between them,
/// are two undo steps; a drag across frames with the pointer held is one.
#[test]
fn two_form_drags_on_one_field_are_two_undo_steps() {
    let dir = project("biome_panel_two_drags", &fixture().to_ron());
    let mut world = world_with(&dir);
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open");
    let mut panel = world.remove_resource::<BiomePanel>().expect("panel");
    let drag = |v: f64| {
        [json!({ "op": "set", "path": "layers/trees/entries/oak/weight", "value": v, "merge": true })]
    };
    let project_dir = Some(dir.as_path());
    // Drag 1: three frames with the pointer down, then the release frame.
    for v in [1.1, 1.2, 1.3] {
        panel.apply_form_frame(project_dir, FormAction::None, &drag(v), true);
    }
    panel.apply_form_frame(project_dir, FormAction::None, &[], false);
    assert_eq!(panel.undo_depth(), 1, "one drag, one step");
    // Drag 2 on the same field.
    for v in [1.6, 1.7] {
        panel.apply_form_frame(project_dir, FormAction::None, &drag(v), true);
    }
    panel.apply_form_frame(project_dir, FormAction::None, &[], false);
    assert_eq!(panel.undo_depth(), 2, "the second drag is its own step");
    panel.edit(&json!({ "op": "undo" })).expect("undo");
    assert!(
        (panel.def.as_ref().expect("def").layers[0].entries[0].weight - 1.3).abs() < 1e-9,
        "undo returns to the end of the first drag"
    );
    world.insert_resource(panel);
    // An agent merge chain is not ended by form frames.
    edit(
        &mut world,
        json!({ "op": "set", "path": "priority", "value": 1, "merge": true }),
    )
    .expect("agent drag");
    let mut panel = world.remove_resource::<BiomePanel>().expect("panel");
    panel.apply_form_frame(project_dir, FormAction::None, &[], false);
    world.insert_resource(panel);
    let depth = world.resource::<BiomePanel>().undo_depth();
    edit(
        &mut world,
        json!({ "op": "set", "path": "priority", "value": 2, "merge": true }),
    )
    .expect("agent drag");
    assert_eq!(
        world.resource::<BiomePanel>().undo_depth(),
        depth,
        "the agent chain still merges"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review B6 (fail-first): in a file opened with an issue, renaming the bad entry or moving the
/// bad filter changes the issue's path but adds no issue, so it is allowed; a new issue is still
/// refused.
#[test]
fn renaming_or_moving_an_existing_issue_is_not_a_new_issue() {
    let mut bad = fixture();
    bad.layers[0].entries[0].weight = -2.0;
    bad.layers[0].entries[0].filters.push(EntryFilter::Slope {
        min_deg: 0.0,
        max_deg: 10.0,
        feather: 1.0,
    });
    bad.layers[0].entries[0].filters[0] = EntryFilter::Slope {
        min_deg: 40.0,
        max_deg: 20.0,
        feather: 5.0,
    };
    let issues = bad.validate();
    assert!(
        issues.len() >= 2,
        "the fixture has a bad weight and a bad filter: {issues:?}"
    );
    let dir = project("biome_panel_issue_paths", &bad.to_ron());
    let mut world = world_with(&dir);
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open with issues");
    edit(
        &mut world,
        json!({ "op": "set", "path": "layers/trees/entries/oak/name", "value": "old oak" }),
    )
    .expect("renaming the bad entry adds no issue");
    edit(
        &mut world,
        json!({ "op": "move", "path": "layers/trees/entries/old oak/filters/#0", "to": 1 }),
    )
    .expect("moving the bad filter adds no issue");
    let now = panel_def(&world);
    assert_eq!(now.layers[0].entries[0].name, "old oak");
    assert_eq!(
        now.layers[0].entries[0].children[0].entry, "fern",
        "its child untouched"
    );
    assert_eq!(now.validate().len(), issues.len(), "{:?}", now.validate());
    let e = edit(
        &mut world,
        json!({ "op": "set", "path": "layers/trees/entries/fern/weight", "value": -1.0 }),
    )
    .expect_err("a new issue");
    assert_eq!(reason(&e), "biome_invalid");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The form addresses list elements by `#<index>` (`index_seg`), which `element_index` never takes
/// for a name.
#[test]
fn the_form_addresses_list_elements_by_index_segment() {
    assert_eq!(index_seg(1), "#1");
    let list = vec![json!({ "name": "1" }), json!({ "name": "0" })];
    assert_eq!(element_index(&list, &index_seg(1)), Some(1));
    assert_eq!(element_index(&list, &index_seg(0)), Some(0));
    assert_eq!(
        element_index(&list, "1"),
        Some(0),
        "a plain segment is a name first"
    );
    assert_eq!(element_index(&list, &index_seg(2)), None);
    assert_eq!(element_index(&list, "#x"), None);
}

/// An editor world: the project and the shared undo history.
fn hosted_world(dir: &Path) -> World {
    let mut world = world_with(dir);
    world.insert_resource(crate::command::UndoStack::new());
    world
}

fn shared_undo_depth(world: &World) -> usize {
    world.resource::<crate::command::UndoStack>().undo_depth()
}

/// Global undo (P-322): in an editor world a biome edit is an entry of the shared history, which
/// the editor's Undo and Redo (and the panel's own) step; a merge chain is one entry; a refused
/// edit adds none.
#[test]
fn an_edit_in_biome_panel_undoes_from_the_global_stack() {
    let dir = project("biome_panel_global_undo", &fixture().to_ron());
    let mut world = hosted_world(&dir);
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open");
    edit(
        &mut world,
        json!({ "op": "set", "path": "priority", "value": 42 }),
    )
    .expect("set");
    assert_eq!(shared_undo_depth(&world), 1, "the edit is on the shared history");
    for v in [1.1, 1.2, 1.3] {
        edit(
            &mut world,
            json!({ "op": "set", "path": "layers/trees/entries/oak/weight", "value": v, "merge": true }),
        )
        .expect("drag");
    }
    assert_eq!(shared_undo_depth(&world), 2, "a merge chain is one shared entry");
    edit(
        &mut world,
        json!({ "op": "set", "path": "layers/trees/max_child_depth", "value": -1 }),
    )
    .expect_err("refused");
    assert_eq!(shared_undo_depth(&world), 2, "a refused edit adds no entry");
    let state = call(&mut world, "editor.biome_panel_get", json!({})).expect("get");
    assert_eq!(state["undo_depth"], json!(2), "{state}");
    // The editor's Undo (Edit ▸ Undo, Ctrl+Z, editor.undo) reverts the whole drag.
    assert!(crate::command::undo_world(&mut world));
    assert!(
        (panel_def(&world).layers[0].entries[0].weight - fixture().layers[0].entries[0].weight).abs() < 1e-9
    );
    assert_eq!(panel_def(&world).priority, 42);
    // The panel's own Undo steps the same history.
    let state = edit(&mut world, json!({ "op": "undo" })).expect("panel undo");
    assert_eq!(panel_def(&world).priority, fixture().priority);
    assert_eq!(state["undo_depth"], json!(0), "{state}");
    assert_eq!(state["redo_depth"], json!(2), "{state}");
    assert_eq!(state["dirty"], json!(false));
    assert_eq!(
        reason(&edit(&mut world, json!({ "op": "undo" })).expect_err("nothing left")),
        "biome_panel_nothing_to_undo"
    );
    assert!(crate::command::redo_world(&mut world));
    assert_eq!(panel_def(&world).priority, 42);
    edit(&mut world, json!({ "op": "redo" })).expect("panel redo");
    assert!((panel_def(&world).layers[0].entries[0].weight - 1.3).abs() < 1e-9);
    assert_eq!(world.resource::<crate::command::UndoStack>().redo_depth(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Global undo (P-322): the Undo History panel lists a biome edit with its file and field; undoing
/// it after another biome was opened reopens the edited one with its saved baseline.
#[test]
fn an_edit_in_biome_panel_appears_in_undo_history() {
    let dir = project("biome_panel_global_history", &fixture().to_ron());
    let other = "assets/biomes/other.biome.ron";
    std::fs::write(dir.join(other), fixture().to_ron()).expect("other");
    let mut world = hosted_world(&dir);
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open");
    edit(&mut world, json!({ "op": "set", "path": "priority", "value": 7 })).expect("set");
    let history = world.resource::<crate::command::UndoStack>().history();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].documents, vec![REL.to_owned()], "{history:?}");
    assert_eq!(history[0].touched[0].property.as_deref(), Some("priority"));
    assert!(history[0].description.contains(REL), "{}", history[0].description);
    assert!(
        !world.resource::<crate::command::UndoStack>().is_dirty(),
        "a biome edit does not dirty the level"
    );
    call(&mut world, "editor.biome_panel_save", json!({})).expect("save");
    call(&mut world, "editor.biome_panel_open", json!({ "path": other })).expect("open other");
    assert!(crate::command::undo_world(&mut world));
    let state = call(&mut world, "editor.biome_panel_get", json!({})).expect("get");
    assert_eq!(state["path"], json!(REL), "undo reopens the edited biome");
    assert_eq!(state["def"]["priority"], json!(fixture().priority));
    assert_eq!(state["dirty"], json!(true), "the saved file holds the edit");
    assert!(crate::command::redo_world(&mut world));
    let state = call(&mut world, "editor.biome_panel_get", json!({})).expect("get");
    assert_eq!(state["def"]["priority"], json!(7));
    assert_eq!(state["dirty"], json!(false));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Global undo (P-322), the form path: the panel's Undo button steps the shared history once per
/// frame, though egui may run the panel twice in a frame.
#[test]
fn the_biome_forms_undo_steps_the_shared_history_once_per_frame() {
    let dir = project("biome_panel_global_form", &fixture().to_ron());
    let mut world = hosted_world(&dir);
    call(&mut world, "editor.biome_panel_open", json!({ "path": REL })).expect("open");
    edit(&mut world, json!({ "op": "set", "path": "priority", "value": 1 })).expect("set");
    edit(&mut world, json!({ "op": "set", "path": "priority", "value": 2 })).expect("set");
    assert_eq!(shared_undo_depth(&world), 2);
    // Two passes of one frame each ask for an undo.
    for _ in 0..2 {
        with_shared(&mut world, Some(9), |panel, project| {
            panel.apply_form_frame(project, FormAction::None, &[json!({ "op": "undo" })], false);
        });
    }
    assert_eq!(shared_undo_depth(&world), 1, "one undo for the frame");
    assert_eq!(panel_def(&world).priority, 1);
    let _ = std::fs::remove_dir_all(&dir);
}
