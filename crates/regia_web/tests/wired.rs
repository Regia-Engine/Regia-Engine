// SPDX-License-Identifier: MPL-2.0
//! The player's product over a real package: the stand-in kit goes through the real cook, the reader and
//! the configurator. Nothing here needs a browser or a GPU.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use regia_viewer_pack::cook::{self, CookOptions};
use regia_viewer_pack::{Package, standin};
use regia_web::package;
use regia_web::player::{Player, PlayerEvent};
use regia_web::product::PackageProduct;
use regia_web::seams::{ProductModel, ViewModel, ViewRequest, ViewValues};
use regia_web::view::ProductView;
use serde_json::{Value, json};

fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "regia-web-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

/// Every stand-in package, cooked for real: `(name, folder)`.
fn cooked(root: &Path) -> Vec<(String, PathBuf)> {
    let set = standin::write(&root.join("in")).expect("stand-ins");
    set.recipes
        .iter()
        .zip(&set.option_graphs)
        .map(|(recipe, graph)| {
            let name = recipe
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("x")
                .to_string();
            let out = root.join("out").join(&name);
            let options = CookOptions {
                option_graph: Some(graph.clone()),
                materials: Some(set.library.clone()),
                ..CookOptions::default()
            };
            cook::pack(&set.kit_dir, recipe, &out, &options).unwrap_or_else(|e| panic!("{name}: {e}"));
            (name, out)
        })
        .collect()
}

fn files_of(dir: &Path) -> (Vec<u8>, BTreeMap<String, Vec<u8>>) {
    let manifest = std::fs::read(dir.join(package::MANIFEST)).expect("manifest");
    let mut files = BTreeMap::new();
    for (name, _) in package::files_to_fetch("x/", &manifest).expect("names") {
        files.insert(name.clone(), std::fs::read(dir.join(&name)).expect("file"));
    }
    (manifest, files)
}

fn loaded(dir: &Path) -> Package {
    let (manifest, files) = files_of(dir);
    package::verify(&manifest, &files).expect("verifies")
}

fn open(dir: &Path) -> PackageProduct {
    PackageProduct::open(loaded(dir), "studio-test").expect("opens")
}

fn marquise(root: &Path) -> PathBuf {
    cooked(root)
        .into_iter()
        .find(|(n, _)| n.contains("marquise"))
        .map(|(_, d)| d)
        .expect("the marquise stand-in")
}

fn group<'a>(options: &'a Value, name: &str) -> &'a Value {
    options
        .as_array()
        .and_then(|g| g.iter().find(|g| g["group"] == name))
        .unwrap_or_else(|| panic!("no group {name} in {options}"))
}

#[test]
fn every_stand_in_opens_and_plans_only_meshes_it_ships() {
    let root = temp_dir("plan");
    for (name, dir) in cooked(&root) {
        let product = open(&dir);
        let plan = product.plan().unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!plan.draws.is_empty(), "{name} draws something");
        for draw in &plan.draws {
            assert!(
                product.meshes().iter().any(|m| m.name == draw.mesh),
                "{name}: {}",
                draw.mesh
            );
            assert!(
                plan.materials.contains_key(&draw.zone),
                "{name}: zone {}",
                draw.zone
            );
            assert!(!draw.transforms.is_empty());
        }
        assert!(
            plan.problems.is_empty(),
            "{name}: every zone maps to a usable look: {:?}",
            plan.problems
        );
        let pivot = product.pivot().expect("pivot");
        assert!(
            pivot.iter().all(|c| c.is_finite() && c.abs() < 100.0),
            "{name}: {pivot:?}"
        );
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn options_have_the_spec_shape() {
    let root = temp_dir("options");
    let product = open(&marquise(&root));
    let options = product.options();
    let groups = options.as_array().expect("an array");
    assert!(groups.len() >= 2, "{options}");
    for g in groups {
        assert!(g["group"].is_string());
        assert!(matches!(g["kind"].as_str(), Some("single" | "multi")), "{g}");
        assert!(g["visual"].is_boolean(), "{g}");
        for v in g["values"].as_array().expect("values") {
            assert!(
                v["value"].is_string() && v["label"].is_string() && v["available"].is_boolean(),
                "{v}"
            );
        }
    }
    std::fs::remove_dir_all(&root).ok();
}

/// Walks every group and value one step away and checks the promise: `available` says exactly whether the
/// pick would pass, and a refusal leaves the build as it was.
#[test]
fn availability_matches_what_set_pick_does_and_refusals_change_nothing() {
    let root = temp_dir("walk");
    for (name, dir) in cooked(&root) {
        let mut product = open(&dir);
        let view = ViewValues::default();
        let groups = product.options();
        for g in groups.as_array().expect("array") {
            let group_name = g["group"].as_str().expect("name");
            if g["kind"] == "multi" {
                continue;
            }
            for v in g["values"].as_array().expect("values") {
                let label = v["value"].as_str().expect("label");
                let before = product.build(&view).expect("build");
                let current = product.options();
                let now = group(&current, group_name)["values"]
                    .as_array()
                    .and_then(|a| a.iter().find(|x| x["value"] == label))
                    .cloned()
                    .expect("the value");
                match product.set_pick(group_name, &json!(label), false) {
                    Ok(_) => {
                        assert_eq!(
                            now["available"], true,
                            "{name}: {group_name}={label} passed but says unavailable"
                        );
                        let build = product.build(&view).expect("a sold build");
                        assert_eq!(build["resolution"]["orderable"], true, "{name}: {build}");
                    }
                    Err(e) => {
                        assert_eq!(
                            now["available"], false,
                            "{name}: {group_name}={label} refused ({e}) but says available"
                        );
                        assert_eq!(
                            product.build(&view).expect("build"),
                            before,
                            "{name}: a refusal changed the build"
                        );
                    }
                }
            }
        }
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn unknown_groups_and_values_get_the_player_codes() {
    let root = temp_dir("codes");
    let mut product = open(&marquise(&root));
    assert_eq!(
        product
            .set_pick("No Such Group", &json!("x"), false)
            .expect_err("unknown")
            .code,
        "pick_unknown_group"
    );
    let options = product.options();
    let first = options[0]["group"].as_str().expect("a group").to_string();
    assert_eq!(
        product
            .set_pick(&first, &json!("No Such Value"), false)
            .expect_err("unknown")
            .code,
        "pick_unknown_value"
    );
    assert_eq!(
        product
            .set_pick(&first, &json!(5), false)
            .expect_err("a number")
            .code,
        "pick_unknown_value"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_material_pick_keeps_the_draws_and_changes_the_material() {
    let root = temp_dir("material");
    let mut product = open(&marquise(&root));
    let plan = product.plan().expect("plan");
    let options = product.options();
    let groups = options.as_array().expect("array");
    let material = groups
        .iter()
        .find(|g| {
            g["values"]
                .as_array()
                .is_some_and(|v| v.iter().filter(|x| x["material_id"].is_string()).count() >= 2)
        })
        .unwrap_or_else(|| panic!("no material group in {options}"));
    let next = material["values"]
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|v| v["available"] == true && v["value"] != material["current"])
        })
        .expect("another sold value");
    let applied = product
        .set_pick(material["group"].as_str().expect("name"), &next["value"], false)
        .expect("sold");
    assert!(
        !applied.geometry_changed,
        "a material pick needs no new geometry: {next}"
    );
    let after = product.plan().expect("plan");
    assert_eq!(plan.draws, after.draws);
    assert_ne!(plan.materials, after.materials, "{next}");
    std::fs::remove_dir_all(&root).ok();
}

/// A recipe writes a round stone's size with one number.
#[test]
fn a_round_stone_sized_by_one_number_finds_its_mesh() {
    let root = temp_dir("round");
    let dir = cooked(&root)
        .into_iter()
        .find(|(n, _)| n.contains("round"))
        .map(|(_, d)| d)
        .expect("the round stand-in");
    let mut pkg = loaded(&dir);
    pkg.recipe["gem_slots"][0]["size_mm"] = json!(["$d"]);
    let product = PackageProduct::open(pkg, "s").expect("opens");
    let plan = product.plan().expect("the plan finds the stone");
    assert!(
        plan.draws.iter().any(|d| d.mesh.starts_with("gem:")),
        "{:?}",
        plan.draws
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_part_placed_by_a_socket_is_reported_not_hidden() {
    let root = temp_dir("socket");
    let mut pkg = loaded(&marquise(&root));
    pkg.options["parts"][0]["at"] = json!({"socket": "base.top"});
    let product = PackageProduct::open(pkg, "s").expect("opens");
    let plan = product.plan().expect("plan");
    assert_eq!(plan.problems.len(), 1, "{:?}", plan.problems);
    assert!(plan.problems[0].contains("base.top"), "{:?}", plan.problems);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_zone_without_a_usable_material_falls_back_and_says_so() {
    let root = temp_dir("fallback");
    let mut pkg = loaded(&marquise(&root));
    pkg.materials["entries"] = json!([]);
    let product = PackageProduct::open(pkg, "s").expect("opens");
    let plan = product.plan().expect("plan");
    assert!(!plan.draws.is_empty());
    assert!(
        plan.materials.len() >= 2,
        "every zone still draws: {:?}",
        plan.materials
    );
    assert!(plan.problems.len() >= 2, "{:?}", plan.problems);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn the_build_has_no_cost_or_client_field() {
    let root = temp_dir("build");
    let product = open(&marquise(&root));
    let build = product
        .build(&ViewValues {
            tilt: [5.0, -3.0],
            ..ViewValues::default()
        })
        .expect("build");
    assert_eq!(build["schema"], "jewelry.build/1");
    assert_eq!(build["camera"]["rig_preset"], "tilt");
    assert!(build["camera"]["tilt_deg"].as_f64().expect("tilt") > 5.0);
    let text = serde_json::to_string(&build).expect("json");
    for forbidden in ["cost", "markup", "dealer_price", "email", "\"ip\""] {
        assert!(!text.contains(forbidden), "{forbidden} in {text}");
    }
    std::fs::remove_dir_all(&root).ok();
}

/// The default view frames the piece: its reach under the widest tilt is 1 / 1.1 of the half view.
#[test]
fn the_default_view_frames_the_piece() {
    let root = temp_dir("fit");
    for (name, dir) in cooked(&root) {
        let mut product = open(&dir);
        let view = ProductView::for_product(&mut product).expect("view");
        let pose = view.pose();
        let eye = glam::Vec3::from(pose.eye);
        let look = glam::Vec3::from(pose.look_at);
        let distance = (eye - look).length();
        assert!(distance > 1.0 && distance < 200.0, "{name}: {distance}");
        let half = distance * (pose.fov_short_deg * 0.5).to_radians().tan();
        let plan = product.plan().expect("plan");
        let mut reach = 0.0_f32;
        for draw in &plan.draws {
            let mesh = product
                .meshes()
                .iter()
                .find(|m| m.name == draw.mesh)
                .expect("mesh");
            let (lo, hi) = mesh.bounds();
            for t in &draw.transforms {
                for p in [lo, hi] {
                    let q = t.transform_point3(p) - look;
                    reach = reach.max(q.x.abs()).max(q.y.abs());
                }
            }
        }
        assert!(
            reach <= half,
            "{name}: the piece reaches {reach} mm, the view shows {half} mm"
        );
        assert!(
            reach > half * 0.2,
            "{name}: the piece fills a real part of the view: {reach} of {half}"
        );
        let build = product.build(&view.shown()).expect("build");
        assert!(
            (build["camera"]["focus_distance_mm"].as_f64().expect("distance") as f32 - distance).abs() < 1e-3,
            "{build}"
        );
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_damaged_or_newer_package_is_refused_with_its_code() {
    let root = temp_dir("damage");
    let dir = marquise(&root);
    let (manifest, files) = files_of(&dir);

    let mut flipped = files.clone();
    flipped.get_mut("mesh.rvm").expect("mesh")[8] ^= 0x40;
    assert_eq!(
        package::verify(&manifest, &flipped).expect_err("flip").code,
        "package_hash_mismatch"
    );

    let mut short = files.clone();
    short.get_mut("options.json").expect("options").pop();
    assert_eq!(
        package::verify(&manifest, &short).expect_err("short").code,
        "package_hash_mismatch"
    );

    let mut missing = files.clone();
    missing.remove("recipe.json");
    assert_eq!(
        package::verify(&manifest, &missing).expect_err("missing").code,
        "package_hash_mismatch"
    );

    let mut newer: Value = serde_json::from_slice(&manifest).expect("json");
    newer["format_version"] = json!(99);
    let newer = serde_json::to_vec(&newer).expect("json");
    assert_eq!(
        package::files_to_fetch("x/", &newer).expect_err("newer").code,
        "package_version_unsupported"
    );

    assert_eq!(
        package::files_to_fetch("x/", b"nonsense").expect_err("junk").code,
        "package_invalid"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_manifest_cannot_send_a_fetch_out_of_its_folder() {
    let root = temp_dir("escape");
    let dir = marquise(&root);
    let (manifest, _) = files_of(&dir);
    let mut doc: Value = serde_json::from_slice(&manifest).expect("json");
    doc["files"][0]["path"] = json!("../../secret");
    let bad = serde_json::to_vec(&doc).expect("json");
    assert!(package::files_to_fetch("https://shop.example/p/", &bad).is_err());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_package_without_an_option_record_does_not_open() {
    let root = temp_dir("norecord");
    let mut pkg = loaded(&marquise(&root));
    pkg.options["option_graph"] = Value::Null;
    let e = PackageProduct::open(pkg, "s").err().expect("refused");
    assert_eq!(e.code, "package_invalid");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn an_unsold_product_does_not_open() {
    let root = temp_dir("unsold");
    let mut pkg = loaded(&marquise(&root));
    let graph = &mut pkg.options["option_graph"];
    graph["status"] = json!("not_orderable");
    let e = PackageProduct::open(pkg, "s").err().expect("refused");
    assert_eq!(e.code, "product_not_orderable", "{e}");
    std::fs::remove_dir_all(&root).ok();
}

/// Spec 42 § 5.2: `duplicate` and `basic` listings do not open either; the player says `product_not_orderable`.
#[test]
fn duplicate_and_basic_listings_do_not_open() {
    let root = temp_dir("duplicate");
    let dir = marquise(&root);
    for status in ["duplicate", "basic"] {
        let mut pkg = loaded(&dir);
        pkg.options["option_graph"]["status"] = json!(status);
        let e = PackageProduct::open(pkg, "s").err().expect("refused");
        assert_eq!(e.code, "product_not_orderable", "{status}: {e}");
    }
    std::fs::remove_dir_all(&root).ok();
}

/// A scene that draws every frame and creates no pipeline.
struct NullScene;

impl regia_web::seams::SceneBackend for NullScene {
    fn sync_product(
        &mut self,
        _product: &dyn ProductModel,
        _geometry: bool,
    ) -> Result<(), regia_web::WebError> {
        Ok(())
    }
    fn resize(&mut self, _width: u32, _height: u32) {}
    fn draw(
        &mut self,
        _pose: &regia_web::seams::CameraPose,
    ) -> Result<regia_web::seams::DrawOutcome, regia_web::WebError> {
        Ok(regia_web::seams::DrawOutcome::Drawn)
    }
    fn pipelines(&self) -> usize {
        0
    }
}

/// Spec 42 § 3.2: a product whose graph status is `incomplete` opens and says `incomplete: true` on `ready`;
/// the same package with a complete listing says `false`.
#[test]
fn an_incomplete_listing_opens_and_says_so_on_ready() {
    let root = temp_dir("incomplete");
    let dir = marquise(&root);
    for (status, expected) in [(None, false), (Some("incomplete"), true)] {
        let mut pkg = loaded(&dir);
        if let Some(status) = status {
            pkg.options["option_graph"]["status"] = json!(status);
        }
        let mut product = PackageProduct::open(pkg, "s").expect("an incomplete listing still opens");
        assert_eq!(product.incomplete(), expected);
        let view = ProductView::for_product(&mut product).expect("view");
        let mut player = Player::new(Box::new(|| 0.0));
        let _ = player.attach_scene(Box::new(NullScene));
        player
            .install(Box::new(product), Box::new(view))
            .expect("installs");
        player.draw_first();
        let ready: Vec<PlayerEvent> = player
            .drain_events()
            .into_iter()
            .filter(|e| matches!(e, PlayerEvent::Ready { .. }))
            .collect();
        assert_eq!(
            ready,
            vec![PlayerEvent::Ready { incomplete: expected }],
            "{status:?}"
        );
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn the_view_clamps_to_the_recipe_through_the_player() {
    let root = temp_dir("view");
    let dir = marquise(&root);
    let mut product = open(&dir);
    let view = ProductView::for_product(&mut product).expect("view");
    let mut player = Player::new(Box::new(|| 0.0));
    player
        .install(Box::new(product), Box::new(view))
        .expect("installs");
    let request: ViewRequest =
        serde_json::from_value(json!({"tilt": [80.0, 0.0], "spin": 45.0, "animate": false})).expect("shape");
    let (report, _) = player.set_view(&request).expect("open");
    assert_eq!(report["clamped"], true, "{report}");
    let tilt = report["view"]["tilt"][0].as_f64().expect("tilt");
    assert!(tilt <= 30.0 + 1e-3, "{report}");
    assert_eq!(report["view"]["spin"], 0.0, "{report}");
    let build = player.build().expect("build");
    assert!(
        build["camera"]["tilt_deg"].as_f64().expect("tilt") <= 30.0 + 1e-3,
        "{build}"
    );
    assert_eq!(player.drain_events(), Vec::<PlayerEvent>::new());
    std::fs::remove_dir_all(&root).ok();
}

/// Audit fix (sprint-fix-audit-jw, finding 13): the tenant comes from the package's manifest, so an option
/// record of another house or another product is refused instead of passing a house check built from the
/// record itself.
#[test]
fn a_package_whose_option_record_is_for_another_house_or_product_is_refused() {
    let root = temp_dir("isolation");
    let dir = marquise(&root);
    let code = |package: Package| match PackageProduct::open(package, "studio-test") {
        Ok(_) => "opens",
        Err(e) => e.code,
    };
    assert_eq!(code(loaded(&dir)), "opens");
    let mut other_house = loaded(&dir);
    other_house.manifest.doc["house"] = json!("house-b");
    assert_eq!(code(other_house), "package_invalid");
    let mut other_product = loaded(&dir);
    other_product.manifest.doc["product_keys"] = json!(["house-a/some-other-piece"]);
    assert_eq!(code(other_product), "package_invalid");
    std::fs::remove_dir_all(&root).ok();
}
