// SPDX-License-Identifier: MPL-2.0
//! The product layer on the documents other tools publish (`docs/specs/42-product-viewer.md` § 5): read at test
//! time from `REGIA_JW_EXCHANGE` and never kept in this repo. There are no meshes yet, so this walks options,
//! refusals and builds. It reports a skip without the folder; `REGIA_JW_REQUIRE_PILOT=1` makes that a failure.

use std::path::{Path, PathBuf};

use regia_viewer_pack::{Manifest, Package};
use regia_web::product::PackageProduct;
use regia_web::seams::{ProductModel, ViewValues};
use serde_json::{Value, json};

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_under(&path, out);
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
}

fn read(path: &Path) -> Value {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn package(recipe: &Value, record: &Value, library: &Value) -> Package {
    Package {
        manifest: Manifest {
            doc: json!({}),
            files: Vec::new(),
            package_hash: "blake3:fixture".to_string(),
        },
        meshes: Vec::new(),
        recipe: recipe.clone(),
        options: json!({"schema": "regia.viewer_options/1", "option_graph": record, "parts": [], "gem_slots": [], "groups": []}),
        materials: library.clone(),
        textures: Vec::new(),
    }
}

#[test]
fn published_records_open_and_their_availability_holds() {
    let required = std::env::var("REGIA_JW_REQUIRE_PILOT").as_deref() == Ok("1");
    let Some(root) = std::env::var_os("REGIA_JW_EXCHANGE").map(PathBuf::from) else {
        assert!(
            !required,
            "REGIA_JW_REQUIRE_PILOT=1 and REGIA_JW_EXCHANGE is not set"
        );
        println!("skipped: set REGIA_JW_EXCHANGE");
        return;
    };
    let mut recipes = Vec::new();
    files_under(&root.join("piece_recipe/1/fixtures/recipes"), &mut recipes);
    let mut records = Vec::new();
    files_under(&root.join("option_graph/1/fixtures/optiongraph"), &mut records);
    let library = read(&root.join("material_library/1/fixtures/library-a1-b1.json"));
    let records: Vec<Value> = records
        .iter()
        .map(|p| read(p))
        .filter(|r| r.get("product_key").is_some())
        .collect();
    assert!(
        !recipes.is_empty() && !records.is_empty(),
        "no fixtures under {}",
        root.display()
    );

    let mut opened = 0;
    for path in &recipes {
        let recipe = read(path);
        let keys = recipe["product_keys"].as_array().cloned().unwrap_or_default();
        for record in records.iter().filter(|r| keys.contains(&r["product_key"])) {
            let key = record["product_key"].as_str().unwrap_or("?").to_string();
            let mut product = PackageProduct::open(package(&recipe, record, &library), "studio-test")
                .unwrap_or_else(|e| panic!("{key}: {e}"));
            opened += 1;
            let view = ViewValues::default();
            let first = product
                .build(&view)
                .unwrap_or_else(|e| panic!("{key}: first build: {e}"));
            assert_eq!(
                first["resolution"]["orderable"], true,
                "{key}: it starts on a sold build: {first}"
            );
            let text = serde_json::to_string(&first).expect("json");
            assert!(!text.contains("\"cost"), "{key}: a build never carries cost");

            let mut checked = 0;
            let groups = product.options();
            for g in groups.as_array().expect("array") {
                let name = g["group"].as_str().expect("name").to_string();
                if g["kind"] == "multi" || g["applies"] == false {
                    continue;
                }
                for v in g["values"].as_array().expect("values") {
                    let label = v["value"].clone();
                    let now = product.options();
                    let row = now
                        .as_array()
                        .and_then(|a| a.iter().find(|x| x["group"] == name.as_str()))
                        .and_then(|x| x["values"].as_array())
                        .and_then(|a| a.iter().find(|x| x["value"] == label))
                        .cloned();
                    let Some(row) = row else {
                        continue;
                    };
                    let before = product.build(&view).expect("build");
                    match product.set_pick(&name, &label, false) {
                        Ok(_) => {
                            assert_eq!(
                                row["available"], true,
                                "{key}: {name}={label} passed but says unavailable"
                            );
                            let after = product.build(&view).expect("a sold build");
                            assert_eq!(
                                after["resolution"]["orderable"], true,
                                "{key}: {name}={label}: {after}"
                            );
                        }
                        Err(e) => {
                            assert_eq!(
                                row["available"], false,
                                "{key}: {name}={label} refused ({e}) but says available"
                            );
                            assert_eq!(
                                product.build(&view).expect("build"),
                                before,
                                "{key}: a refusal changed the build"
                            );
                        }
                    }
                    checked += 1;
                }
            }
            println!("{key}: {checked} one-step picks agree with options()");
            assert!(checked > 0, "{key}: nothing to walk");
        }
    }
    assert!(opened > 0, "no record matched a recipe");
}
