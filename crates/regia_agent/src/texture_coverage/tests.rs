// SPDX-License-Identifier: MPL-2.0
//! `assets.texture_coverage` and `assets.texpack_dry_run` (ADR-3090).
#![cfg(test)]

use super::*;
use regia_asset::{AssetPlugin, AssetServerSettings};
use regia_asset_pipeline::texture_pack as pipeline;

fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) {
    let file = std::fs::File::create(path).expect("png file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("header");
    writer.write_image_data(rgba).expect("data");
}

fn world_at(root: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(crate::AgentSettings {
        project_dir: Some(root.to_path_buf()),
    });
    world
}

/// The recipes both readers must agree on: name → text.
const RECIPES: &[(&str, &str)] = &[
    (
        "ok.texpack.ron",
        r#"(format_version: 1, sources: {"a": "a.png", "b": "b.png"},
            r: Source(source: "a", channel: "r"), g: Source(source: "b", channel: "luma", invert: true))"#,
    ),
    (
        "small.texpack.ron",
        r#"(format_version: 1, sources: {"a": "a.png", "b": "b.png"}, r: Source(source: "a", channel: "r"),
            size_mismatch: resize_to_smallest)"#,
    ),
    (
        "strict.texpack.ron",
        r#"(format_version: 1, sources: {"a": "a.png", "b": "b.png"}, r: Source(source: "a", channel: "r"),
            size_mismatch: error)"#,
    ),
    (
        "fixed.texpack.ron",
        r#"(format_version: 1, sources: {"a": "a.png"}, r: Source(source: "a", channel: "g"), size: Some((7, 9)))"#,
    ),
    (
        "channel.texpack.ron",
        r#"(format_version: 1, sources: {"a": "a.png"}, r: Source(source: "a", channel: "x"))"#,
    ),
    (
        "unknown.texpack.ron",
        r#"(format_version: 1, sources: {"a": "a.png"}, r: Source(source: "z", channel: "r"))"#,
    ),
    (
        "missing.texpack.ron",
        r#"(format_version: 1, sources: {"a": "gone.png"}, r: Source(source: "a", channel: "r"))"#,
    ),
    (
        "loop_a.texpack.ron",
        r#"(format_version: 1, sources: {"b": "loop_b.texpack.ron"}, r: Source(source: "b", channel: "r"))"#,
    ),
    (
        "loop_b.texpack.ron",
        r#"(format_version: 1, sources: {"a": "loop_a.texpack.ron"}, r: Source(source: "a", channel: "r"))"#,
    ),
    (
        "nested.texpack.ron",
        r#"(format_version: 1, sources: {"inner": "ok.texpack.ron"}, a: Source(source: "inner", channel: "g"))"#,
    ),
    ("consts.texpack.ron", "(format_version: 1, r: Constant(0.5))"),
    ("future.texpack.ron", "(format_version: 2, shiny: true)"),
    (
        "constant.texpack.ron",
        "(format_version: 1, size: Some((4, 4)), b: Constant(2.0))",
    ),
    (
        "five.texpack.ron",
        r#"(format_version: 1, sources: {"a": "a.png", "b": "b.png", "c": "a.png", "d": "b.png", "e": "a.png"})"#,
    ),
];

fn recipe_project(tag: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    write_png(&assets.join("a.png"), 16, 8, &[200u8, 10, 20, 255].repeat(16 * 8));
    write_png(&assets.join("b.png"), 4, 4, &[0u8, 50, 0, 255].repeat(16));
    for (name, text) in RECIPES {
        std::fs::write(assets.join(name), text).expect("recipe");
    }
    root
}

/// The agent's reader and the pipeline's give the same code, size and inputs on every recipe.
#[test]
fn the_dry_run_agrees_with_the_pipeline_on_every_recipe() {
    let root = recipe_project("agent-texpack-parity");
    let mut world = world_at(&root);
    let canonical = std::fs::canonicalize(&root).expect("root");
    for (name, _) in RECIPES {
        let rel = format!("assets/{name}");
        let ours = texpack_dry_run(&mut world, &json!({ "path": rel })).expect("dry run");
        let theirs = pipeline::plan_pack(&canonical.join(&rel));
        match theirs {
            Ok(plan) => {
                assert_eq!(ours["ok"], json!(true), "{name}: {ours}");
                assert_eq!(
                    (ours["width"].clone(), ours["height"].clone()),
                    (json!(plan.width), json!(plan.height)),
                    "{name}"
                );
                let inputs: Vec<String> = plan
                    .inputs
                    .iter()
                    .map(|p| {
                        p.strip_prefix(&canonical)
                            .expect("inside")
                            .to_string_lossy()
                            .replace('\\', "/")
                    })
                    .collect();
                assert_eq!(ours["inputs"], json!(inputs), "{name}");
                for (source, planned) in ours["sources"]
                    .as_array()
                    .expect("sources")
                    .iter()
                    .zip(&plan.sources)
                {
                    assert_eq!(source["name"], json!(planned.name), "{name}");
                    assert_eq!(source["resized"], json!(planned.resized), "{name}");
                    assert_eq!(source["recipe"], json!(planned.recipe), "{name}");
                }
            }
            Err(e) => {
                assert_eq!(ours["ok"], json!(false), "{name}: {ours}");
                assert_eq!(ours["code"], json!(e.code), "{name}: {ours} vs {e}");
            }
        }
    }
    // Spot checks of what the answer says.
    let ok = texpack_dry_run(&mut world, &json!({ "path": "assets/ok.texpack.ron" })).expect("ok");
    assert_eq!((ok["width"].clone(), ok["height"].clone()), (json!(16), json!(8)));
    assert_eq!(ok["channels"]["g"], json!("1 - b.luma"));
    assert_eq!(ok["channels"]["a"], json!("1"));
    assert_eq!(ok["usage"], json!("linear"));
    let cycle = texpack_dry_run(&mut world, &json!({ "path": "assets/loop_a.texpack.ron" })).expect("cycle");
    assert_eq!(cycle["code"], json!("texpack_cycle"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_dry_run_refuses_a_bad_path() {
    let root = recipe_project("agent-texpack-params");
    let mut world = world_at(&root);
    let code = |e: RpcError| e.data.and_then(|d| d.get("code").cloned());
    let err = texpack_dry_run(&mut world, &json!({})).expect_err("no path");
    assert_eq!(code(err), Some(json!(DRY_RUN_BAD_PARAM)));
    let err = texpack_dry_run(&mut world, &json!({ "path": "assets/a.png" })).expect_err("not a recipe");
    assert_eq!(code(err), Some(json!(DRY_RUN_BAD_PARAM)));
    assert!(texpack_dry_run(&mut world, &json!({ "path": "../x.texpack.ron" })).is_err());
    assert!(texpack_dry_run(&mut world, &json!({ "path": 3 })).is_err());
    // A recipe file that does not exist is a dry-run finding, not a refusal.
    let gone = texpack_dry_run(&mut world, &json!({ "path": "assets/none.texpack.ron" })).expect("finding");
    assert_eq!(gone["code"], json!("texpack_missing_source"));
    let _ = std::fs::remove_dir_all(&root);
}

/// A sparse canopy card (deterministic): about 90 small leaves at random spots on a transparent
/// background.
fn canopy() -> Vec<u8> {
    let mut state = 0x2545_f491_u32;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    let mut alpha = vec![0f32; 128 * 128];
    for _ in 0..90 {
        let (cx, cy) = ((next() % 128) as f32, (next() % 128) as f32);
        let r = 1.5 + (next() % 1000) as f32 / 500.0;
        for y in 0..128u32 {
            for x in 0..128u32 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let i = (y * 128 + x) as usize;
                alpha[i] = alpha[i].max((r + 0.5 - d).clamp(0.0, 1.0));
            }
        }
    }
    alpha
        .iter()
        .flat_map(|a| [40, 120, 30, (a * 255.0).round() as u8])
        .collect()
}

#[test]
fn coverage_reports_every_level_and_sees_the_setting() {
    let root = regia_test_utils::unique_temp_dir("agent-texture-coverage");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    write_png(&assets.join("plain_albedo.png"), 128, 128, &canopy());
    let covered = assets.join("covered_albedo.png");
    write_png(&covered, 128, 128, &canopy());
    let mut meta = regia_asset::MetaSidecar::new("png", regia_asset::AssetId::new_random());
    meta.settings = Some("(alpha_cutoff: Some(0.5))".into());
    std::fs::write(
        regia_asset::meta_path_for(&covered),
        meta.to_ron_string().expect("ron"),
    )
    .expect("meta");
    write_png(
        &assets.join("hull_normal.png"),
        16,
        16,
        &[128u8, 128, 255, 255].repeat(256),
    );
    let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
        project_root: root.clone(),
        platform: "linux".into(),
        incremental: true,
    })
    .expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(AssetPlugin {
        settings: AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        },
    });
    let world = app.world_mut();
    let plain = texture_coverage(world, &json!({ "asset": "assets/plain_albedo.png" })).expect("plain");
    assert_eq!(plain["cutoff_source"], json!("default"));
    assert_eq!(plain["levels"].as_array().map(Vec::len), Some(8), "128 -> 1");
    assert_eq!(plain["preserved"], json!(false), "{plain}");
    let on = texture_coverage(world, &json!({ "asset": "assets/covered_albedo.png" })).expect("covered");
    assert_eq!(on["cutoff_source"], json!("meta"));
    assert_eq!(on["cutoff"], json!(0.5));
    assert_eq!(on["preserved"], json!(true), "{on}");
    assert_eq!(on["mip0_coverage"], plain["mip0_coverage"]);
    let param = texture_coverage(
        world,
        &json!({ "asset": "assets/covered_albedo.png", "cutoff": 0.25 }),
    )
    .expect("param");
    assert_eq!(param["cutoff_source"], json!("param"));

    let code = |e: RpcError| e.data.and_then(|d| d.get("code").cloned());
    for bad in [json!(0.0), json!(1.5), json!("half")] {
        let err = texture_coverage(
            world,
            &json!({ "asset": "assets/plain_albedo.png", "cutoff": bad }),
        )
        .expect_err("bad cutoff");
        assert_eq!(code(err), Some(json!(COVERAGE_BAD_PARAM)));
    }
    let err = texture_coverage(
        world,
        &json!({ "asset": "assets/plain_albedo.png", "channel": "g" }),
    )
    .expect_err("channel");
    assert_eq!(code(err), Some(json!(COVERAGE_BAD_PARAM)));
    let err = texture_coverage(world, &json!({ "asset": "assets/hull_normal.png" })).expect_err("normal map");
    assert_eq!(code(err), Some(json!(COVERAGE_NOT_TEXTURE)));
    let err = texture_coverage(world, &json!({ "asset": "assets/nothing.png" })).expect_err("unknown");
    assert_eq!(code(err), Some(json!("F0016")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn coverage_without_an_asset_server_is_f0016() {
    let root = regia_test_utils::unique_temp_dir("agent-texture-coverage-bare");
    let mut world = world_at(&root);
    let err = texture_coverage(&mut world, &json!({ "asset": "assets/x.png" })).expect_err("no server");
    assert_eq!(
        err.data.and_then(|d| d.get("code").cloned()),
        Some(json!("F0016"))
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_cutoff_byte_matches_the_cook() {
    for c in [0.01f32, 0.25, 0.5, 0.5001, 0.75, 1.0] {
        assert_eq!(
            Some(cutoff_byte(f64::from(c))),
            regia_asset_pipeline::alpha_coverage::cutoff_byte(c),
            "{c}"
        );
    }
}

/// A `.meta` sidecar over the 1 MiB cap is not read (it counts as no cutoff); one just under the cap
/// is. The sidecar is valid RON either way: trailing whitespace pads it.
#[test]
fn an_oversized_meta_sidecar_is_not_read() {
    let root = regia_test_utils::unique_temp_dir("texture-coverage-meta-cap");
    let source = root.join("assets/big_albedo.png");
    std::fs::create_dir_all(source.parent().expect("parent")).expect("dir");
    let mut meta = regia_asset::MetaSidecar::new("png", regia_asset::AssetId::new_random());
    meta.settings = Some("(alpha_cutoff: Some(0.5))".into());
    let text = meta.to_ron_string().expect("ron");
    let cap = 1usize << 20;
    for (len, expected) in [(cap, Some(0.5)), (cap + 1, None)] {
        let mut padded = text.clone();
        padded.push_str(&" ".repeat(len - text.len()));
        std::fs::write(regia_asset::meta_path_for(&source), &padded).expect("meta");
        assert_eq!(
            meta_cutoff(&root, "assets/big_albedo.png"),
            expected,
            "{len} bytes"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
