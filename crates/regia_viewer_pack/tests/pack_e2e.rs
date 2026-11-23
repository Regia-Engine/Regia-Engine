// SPDX-License-Identifier: MPL-2.0
//! JW-L6 end to end: write the stand-in kit and recipes, cook every recipe into a package, read
//! it back through the verifying reader, and check what a player relies on.
#![cfg(feature = "cook")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use regia_viewer_pack::cook::{self, CookOptions};
use regia_viewer_pack::{BUDGET_BYTES, MANIFEST_FILE, MESH_FILE, Package, geom, read_dir, standin};
use serde_json::{Value, json};

/// A fresh temp directory per call: pid plus a counter, never a fixed name (the same scheme as
/// `regia_test_utils::unique_temp_dir`, without pulling the engine into this small crate's tests).
fn unique_temp_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "regia-viewer-pack-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

fn cook_all(root: &Path) -> Vec<(PathBuf, cook::CookResult)> {
    let set = standin::write(&root.join("in")).expect("stand-ins");
    set.recipes
        .iter()
        .zip(&set.option_graphs)
        .map(|(recipe, og)| {
            let name = recipe
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("x")
                .to_string();
            let out = root.join("out").join(&name);
            let opts = CookOptions {
                option_graph: Some(og.clone()),
                materials: Some(set.library.clone()),
                ..CookOptions::default()
            };
            let r = cook::pack(&set.kit_dir, recipe, &out, &opts).unwrap_or_else(|e| panic!("{name}: {e}"));
            (out, r)
        })
        .collect()
}

#[test]
fn every_stand_in_cooks_verifies_and_fits_the_budget() {
    let root = unique_temp_dir("e2e");
    for (dir, result) in cook_all(&root) {
        let pkg = read_dir(&dir).expect("package verifies");
        let report = &result.report;
        eprintln!(
            "{}: {} bytes ({} gzip), {} meshes, {} triangles; warnings {:?}",
            dir.display(),
            report.total_bytes,
            report.total_gzip_bytes.unwrap_or(0),
            report.meshes,
            report.triangles,
            result.warnings
        );
        assert!(
            report.within_budget && report.total_bytes < BUDGET_BYTES / 4,
            "{report:?}"
        );
        assert!(report.total_gzip_bytes.is_some_and(|g| g < report.total_bytes));
        // Every mesh a layout, part swap or gem size names is in mesh.rvm.
        let mut named: Vec<String> = Vec::new();
        for p in pkg.options["parts"].as_array().into_iter().flatten() {
            named.push(p["mesh"].as_str().expect("part mesh").to_string());
        }
        for g in pkg.options["groups"].as_array().into_iter().flatten() {
            for d in g["drives"].as_array().into_iter().flatten() {
                for v in d["map"].as_object().into_iter().flat_map(|m| m.values()) {
                    if let Some(m) = v.get("mesh").and_then(Value::as_str) {
                        named.push(m.to_string());
                    }
                }
            }
        }
        for s in pkg.options["gem_slots"].as_array().into_iter().flatten() {
            for size in s["sizes"].as_array().into_iter().flatten() {
                named.push(size["mesh"].as_str().expect("gem mesh").to_string());
            }
        }
        assert!(!named.is_empty());
        for n in &named {
            assert!(pkg.mesh(n).is_some(), "{n} missing from mesh.rvm");
        }
        // Dealer cost never leaves the cook.
        assert!(pkg.options["cost_fields_removed"].as_u64().unwrap_or(0) >= 1);
        let text = std::fs::read_to_string(dir.join("options.json")).expect("options");
        assert!(!text.contains("\"cost\""), "cost leaked");
        // Spec 42 § 8.2: no cost or markup field in any JSON file of the package.
        let mut json_files = 0;
        for entry in std::fs::read_dir(&dir).expect("package dir") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            json_files += 1;
            let text = std::fs::read_to_string(&path).expect("json file");
            for field in ["cost", "cost_cents", "markup", "markup_pct", "dealer_price"] {
                assert!(
                    !text.contains(&format!("\"{field}\"")),
                    "{field} leaked into {}",
                    path.display()
                );
            }
        }
        assert!(
            json_files >= 4,
            "manifest, recipe, options, materials: {json_files}"
        );
        // Material groups map labels to entries of the library, and only those entries ship.
        let mats = pkg.materials["entries"].as_array().expect("entries");
        for g in pkg.options["groups"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|g| g["group"] == "Material")
        {
            for v in g["values"].as_array().into_iter().flatten() {
                let id = v["material_id"].as_str().expect("every metal maps to an entry");
                assert!(mats.iter().any(|m| m["material_id"] == id));
            }
        }
        // The packaged recipe drives the drag view.
        let view = regia_viewer_view::DragView::from_recipe(&pkg.recipe).expect("view range");
        assert_eq!(view.controller.range().tilt_max_deg, 30.0);
    }
}

#[test]
fn hidden_parts_are_cheap_and_gem_sizes_follow_the_drives() {
    let root = unique_temp_dir("sizes");
    let results = cook_all(&root);
    let (dir, _) = &results[0];
    let pkg = read_dir(dir).expect("verifies");
    // The marquise slot is cooked at both offered sizes, as linked pairs (3x1.5 and 4x2 only).
    let sizes: Vec<String> = pkg.options["gem_slots"][0]["sizes"]
        .as_array()
        .expect("sizes")
        .iter()
        .map(|s| s["mesh"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(sizes, vec!["gem:s1@3x1.5", "gem:s1@4x2"]);
    let small = pkg.mesh("gem:s1@3x1.5").expect("small gem");
    let (lo, hi) = small.bounds();
    // A cut's length runs along +y, the kit's gem-local frame (ADR-2615).
    assert!((hi.y - lo.y - 3.0).abs() < 0.05 && (hi.x - lo.x - 1.5).abs() < 0.08);
    let pin = pkg.mesh("part:shared/threadless_pin_0.8x6@1").expect("pin");
    let bezel = pkg.mesh("part:shared/bezel_marquise_3x1.5@1").expect("bezel");
    assert!(
        pin.triangles() * 4 < bezel.triangles(),
        "hidden pin {} vs bezel {}",
        pin.triangles(),
        bezel.triangles()
    );
}

#[test]
fn cooking_twice_gives_identical_bytes() {
    let a = unique_temp_dir("det-a");
    let b = unique_temp_dir("det-b");
    let ra = cook_all(&a);
    let rb = cook_all(&b);
    for ((da, _), (db, _)) in ra.iter().zip(&rb) {
        for f in [
            MANIFEST_FILE,
            MESH_FILE,
            "options.json",
            "materials.json",
            "recipe.json",
        ] {
            assert_eq!(
                std::fs::read(da.join(f)).ok(),
                std::fs::read(db.join(f)).ok(),
                "{f} differs"
            );
        }
    }
}

#[test]
fn a_damaged_or_incomplete_package_is_refused() {
    let root = unique_temp_dir("tamper");
    let (dir, _) = cook_all(&root).remove(1);
    let manifest = std::fs::read(dir.join(MANIFEST_FILE)).expect("manifest");
    let mesh = std::fs::read(dir.join(MESH_FILE)).expect("mesh");
    let others: Vec<(String, Vec<u8>)> = ["recipe.json", "options.json", "materials.json"]
        .iter()
        .map(|f| (f.to_string(), std::fs::read(dir.join(f)).expect("file")))
        .collect();
    let load = |mesh: &[u8], skip: Option<&str>| {
        Package::from_files(&manifest, |p| {
            if Some(p) == skip {
                None
            } else if p == MESH_FILE {
                Some(mesh)
            } else {
                others.iter().find(|(n, _)| n == p).map(|(_, b)| b.as_slice())
            }
        })
    };
    assert!(load(&mesh, None).is_ok());
    let mut bad = mesh.clone();
    let mid = bad.len() / 2;
    bad[mid] ^= 0x40;
    assert_eq!(
        load(&bad, None).map(|_| ()).expect_err("refused").code,
        "pack_hash_mismatch"
    );
    assert_eq!(
        load(&mesh, Some("options.json"))
            .map(|_| ())
            .expect_err("refused")
            .code,
        "pack_missing_file"
    );
    // A manifest edited by hand (one size changed) no longer matches its package_hash.
    let text = String::from_utf8(manifest.clone()).expect("utf-8");
    let edited = text.replacen("\"bytes\":", "\"bytes\":1", 1);
    let e = Package::from_files(edited.as_bytes(), |_| None)
        .map(|_| ())
        .expect_err("refused");
    assert_eq!(e.code, "pack_hash_mismatch");
    let e = Package::from_files(
        text.replace("viewer_package/1", "viewer_package/9").as_bytes(),
        |_| None,
    )
    .map(|_| ())
    .expect_err("refused");
    assert_eq!(e.code, "pack_bad_manifest");
    let e = Package::from_files(
        text.replace("\"format_version\":1", "\"format_version\":2")
            .as_bytes(),
        |_| None,
    )
    .map(|_| ())
    .expect_err("refused");
    assert_eq!(e.code, "pack_version");
}

#[test]
fn recipes_that_break_the_rules_are_refused() {
    let root = unique_temp_dir("refuse");
    let set = standin::write(&root.join("in")).expect("stand-ins");
    let base: Value = serde_json::from_slice(&std::fs::read(&set.recipes[0]).expect("recipe")).expect("json");
    type Edit = Box<dyn Fn(&mut Value)>;
    let cases: Vec<(&str, Edit)> = vec![
        (
            "tilt",
            Box::new(|r: &mut Value| r["view_range"]["tilt_max_deg"] = 120.into()),
        ),
        (
            "part",
            Box::new(|r: &mut Value| r["parts"][0]["part_ref"] = "shared/no_such_part@1".into()),
        ),
        (
            "house",
            Box::new(|r: &mut Value| r["parts"][0]["part_ref"] = "house-z/bezel_marquise_3x1.5@1".into()),
        ),
        (
            "cut",
            Box::new(|r: &mut Value| r["gem_slots"][0]["cut"] = "heart".into()),
        ),
        (
            "schema",
            Box::new(|r: &mut Value| r["schema"] = "jewelry.piece_recipe/2".into()),
        ),
    ];
    for (name, edit) in cases {
        let mut r = base.clone();
        edit(&mut r);
        let p = root.join(format!("{name}.json"));
        std::fs::write(&p, serde_json::to_vec(&r).expect("json")).expect("write");
        let e = cook::pack(
            &set.kit_dir,
            &p,
            &root.join("out").join(name),
            &CookOptions::default(),
        )
        .expect_err("refused");
        assert_eq!(e.code, "pack_bad_recipe", "{name}: {e}");
    }
}

#[test]
fn textures_are_scaled_and_reencoded_for_the_web() {
    // A 1024 x 1024 detail map: noise plus a gradient, like a hammered finish.
    let mut img = image::RgbImage::new(1024, 1024);
    let mut s = 0x1234_5678u32;
    for (x, y, p) in img.enumerate_pixels_mut() {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        let n = (s >> 24) as u8 / 4;
        *p = image::Rgb([(x / 8) as u8 / 2 + n, (y / 8) as u8 / 2 + n, 128 + n]);
    }
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png).expect("png");
    let src = png.into_inner();
    let (normal, ext) = cook::cook_texture(&src, "detail_normal", 512).expect("normal");
    assert_eq!(ext, "png");
    let (color, ext) = cook::cook_texture(&src, "base_color", 512).expect("color");
    assert_eq!(ext, "jpg");
    let decoded = image::load_from_memory(&normal).expect("decode");
    assert_eq!((decoded.width(), decoded.height()), (512, 512));
    eprintln!(
        "texture 1024 src png {} B -> 512 png {} B, 512 jpeg q85 {} B",
        src.len(),
        normal.len(),
        color.len()
    );
    assert!(color.len() < normal.len());
}

/// Point the marquise recipe's gem size at one `(length, width)` pair.
fn only_gem_size(recipe: &mut Value, length: f32, width: f32) {
    recipe["values"]["mq_l"] = json!(length);
    recipe["values"]["mq_w"] = json!(width);
    let label = format!("{length}x{width}mm");
    for drive in recipe["drives"].as_array_mut().expect("drives") {
        let value = if drive["to"]["value"] == "mq_l" {
            Some(length)
        } else if drive["to"]["value"] == "mq_w" {
            Some(width)
        } else {
            None
        };
        if let Some(mm) = value {
            let mut map = serde_json::Map::new();
            map.insert(label.clone(), json!(mm));
            drive["to"]["map"] = Value::Object(map);
        }
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).expect("read")).expect("json")
}

/// A kit `material_slots` entry is `{slot_id, role}`, not a bare string. The cook must keep the id.
#[test]
fn audit_object_material_slot_is_kept() {
    let root = unique_temp_dir("slot");
    let set = standin::write(&root.join("in")).expect("stand-ins");
    let kit_path = set.kit_dir.join("kit.json");
    let mut kit = read_json(&kit_path);
    let parts = kit["parts"].as_array_mut().expect("parts");
    let ball = parts
        .iter_mut()
        .find(|p| p["part_id"] == "ball_3.0")
        .expect("ball");
    ball["material_slots"] = json!([{"slot_id": "stone", "role": "gem"}]);
    std::fs::write(&kit_path, serde_json::to_vec(&kit).expect("json")).expect("write");
    let out = root.join("out");
    cook::pack(&set.kit_dir, &set.recipes[2], &out, &CookOptions::default()).expect("cook");
    let pkg = read_dir(&out).expect("package");
    let ball = pkg.mesh("part:shared/ball_3.0@1").expect("ball mesh");
    assert_eq!(ball.material_slot, "stone");
}

/// A size the kit does not carry is generated in the gem-local frame: length on +y, width on +x.
#[test]
fn audit_generated_cut_uses_gem_local_frame() {
    let root = unique_temp_dir("frame");
    let set = standin::write(&root.join("in")).expect("stand-ins");
    let mut recipe = read_json(&set.recipes[0]);
    only_gem_size(&mut recipe, 5.0, 2.5);
    let path = root.join("recipe.json");
    std::fs::write(&path, serde_json::to_vec(&recipe).expect("json")).expect("write");
    let out = root.join("out");
    let result = cook::pack(&set.kit_dir, &path, &out, &CookOptions::default()).expect("cook");
    assert!(
        result.warnings.iter().any(|w| w.contains("generated")),
        "expected a generated cut, warnings: {:?}",
        result.warnings
    );
    let pkg = read_dir(&out).expect("package");
    let gem = pkg.mesh("gem:s1@5x2.5").expect("generated gem");
    let (lo, hi) = gem.bounds();
    let (dx, dy) = (hi.x - lo.x, hi.y - lo.y);
    assert!((dy - 5.0).abs() < 0.08, "length along +y, dx {dx} dy {dy}");
    assert!((dx - 2.5).abs() < 0.08, "width along +x, dx {dx} dy {dy}");
}

/// `size_mm` may be `[length, width, depth]`. The depth must not hide the kit's cut.
#[test]
fn audit_kit_cut_with_depth_is_used() {
    let root = unique_temp_dir("depth");
    let set = standin::write(&root.join("in")).expect("stand-ins");
    let facets = geom::cut_facets("marquise", 5.0, 2.5).expect("facets");
    let kit_path = set.kit_dir.join("kit.json");
    let mut kit = read_json(&kit_path);
    kit["gem_cuts"].as_array_mut().expect("cuts").push(json!({
        "cut_id": "marquise-5x2.5",
        "cut_type": "marquise",
        "size_mm": [5.0, 2.5, 1.2],
        "facets": facets,
    }));
    std::fs::write(&kit_path, serde_json::to_vec(&kit).expect("json")).expect("write");
    let mut recipe = read_json(&set.recipes[0]);
    only_gem_size(&mut recipe, 5.0, 2.5);
    let path = root.join("recipe.json");
    std::fs::write(&path, serde_json::to_vec(&recipe).expect("json")).expect("write");
    let result = cook::pack(&set.kit_dir, &path, &root.join("out"), &CookOptions::default()).expect("cook");
    assert!(
        !result.warnings.iter().any(|w| w.contains("generated")),
        "kit cut ignored, warnings: {:?}",
        result.warnings
    );
}

/// A zero or negative gem size is not a stone. The cook refuses it.
#[test]
fn audit_non_positive_gem_size_is_refused() {
    let root = unique_temp_dir("neg");
    let set = standin::write(&root.join("in")).expect("stand-ins");
    let mut recipe = read_json(&set.recipes[0]);
    recipe["gem_slots"][0]["size_mm"] = json!([0.0, -1.5]);
    let path = root.join("recipe.json");
    std::fs::write(&path, serde_json::to_vec(&recipe).expect("json")).expect("write");
    let err =
        cook::pack(&set.kit_dir, &path, &root.join("out"), &CookOptions::default()).expect_err("refused");
    assert_eq!(err.code, "pack_bad_recipe", "{err}");
}

/// Audit LOW (sprint-corr-web-viewer): two gem slots with one `slot_id` cooked two meshes named
/// `gem:<id>@<size>`; the player takes the first of a name, so the second slot drew the first
/// slot's cut. A recipe that repeats a slot id is refused.
#[test]
fn corr_a_gem_slot_listed_twice_is_refused() {
    let root = unique_temp_dir("dupslot");
    let set = standin::write(&root.join("in")).expect("stand-ins");
    let mut recipe: Value =
        serde_json::from_slice(&std::fs::read(&set.recipes[0]).expect("recipe")).expect("json");
    let twin = recipe["gem_slots"][0].clone();
    recipe["gem_slots"].as_array_mut().expect("gem slots").push(twin);
    let p = root.join("twin.json");
    std::fs::write(&p, serde_json::to_vec(&recipe).expect("json")).expect("write");
    // The full options cook the unchanged recipe (`cook_all`), so only the twin slot can refuse.
    let opts = CookOptions {
        option_graph: Some(set.option_graphs[0].clone()),
        materials: Some(set.library.clone()),
        ..CookOptions::default()
    };
    let e = cook::pack(&set.kit_dir, &p, &root.join("out"), &opts).expect_err("refused");
    assert_eq!(e.code, "pack_bad_recipe", "{e}");
    std::fs::remove_dir_all(&root).ok();
}

/// Audit LOW (sprint-corr-web-viewer): every `symlink_metadata` error read as "absent", so a
/// package whose folder cannot be searched (permission denied) said `pack_missing_file`.
#[cfg(unix)]
#[test]
fn corr_a_file_that_cannot_be_examined_is_an_io_error_not_missing() {
    use std::os::unix::fs::PermissionsExt;
    let root = unique_temp_dir("noperm");
    let out = root.join("pkg");
    let set = standin::write(&root.join("in")).expect("stand-ins");
    let opts = CookOptions {
        option_graph: Some(set.option_graphs[0].clone()),
        materials: Some(set.library.clone()),
        ..CookOptions::default()
    };
    cook::pack(&set.kit_dir, &set.recipes[0], &out, &opts).expect("cook");
    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let searchable = std::fs::symlink_metadata(out.join(MANIFEST_FILE)).is_ok();
    let result = read_dir(&out);
    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
    if searchable {
        println!("skipped: this user can search a 0o600 folder (root?)");
    } else {
        let e = result.expect_err("refused");
        assert_eq!(e.code, "pack_io", "{e}");
    }
    std::fs::remove_dir_all(&root).ok();
}
