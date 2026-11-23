// SPDX-License-Identifier: MPL-2.0
//! The configurator runtime on the generic fixtures (ADR-2534): resolve vectors, options and
//! refusals, adjust mode, looks, the build JSON and its round trip, tenancy, and published-schema
//! contract checks on load.
//!
//! Environment (the pilot data on the build server; see `book/agents/configurator.md`):
//! - `REGIA_JW_EXCHANGE=<dir>`: the owner's `option_graph/1/fixtures/resolve_vectors.jsonl` there is
//!   run against the product records beside it and under `REGIA_JW_OUT`.
//! - `REGIA_JW_OUT=<dir>`: `optiongraph/<snapshot>/houses/*/products/*.json`.
//! - `REGIA_JW_CONTRACT_REQUIRED=1` (or `REGIA_JW_REQUIRE_PILOT=1`): a missing vector file fails instead of being reported.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_configurator::content::Recipe;
use regia_configurator::resolve::Pick;
use regia_configurator::runtime::Look;
use regia_configurator::vectors::{parse_vectors, run_vectors};
use regia_configurator::{
    BuildOptions, Camera, Configurator, Interface, MemorySource, OpenRequest, ProductRecord, SetMode, Tenant,
    check, load_product,
};
use serde_json::{Value, json};

const SNAP: &str = "2026-11-04-000000000000";

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runtime")
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every runtime fixture under its layout path, plus the M0 kit and library fixtures.
fn source() -> MemorySource {
    let mut src = MemorySource::new();
    let root = fixture_dir();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).expect("fixture dir") {
            let p = e.expect("entry").path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "json") {
                let rel = p
                    .strip_prefix(&root)
                    .expect("under root")
                    .to_string_lossy()
                    .replace('\\', "/");
                src.insert(rel, read(&p));
            }
        }
    }
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    src.insert(
        "kits/shared/shared-basic/kit.json",
        read(&fixtures.join("jewelry-part-kit/kit-shared-basic.json")),
    );
    src.insert(
        "materials/1/library.json",
        read(&fixtures.join("jewelry-material-library/library-basic.json")),
    );
    src
}

fn request(product_key: &str) -> OpenRequest {
    OpenRequest {
        product_key: product_key.to_owned(),
        snapshot_id: Some(SNAP.to_owned()),
        library_version: Some("1".to_owned()),
        kit_paths: vec!["kits/shared/shared-basic/kit.json".to_owned()],
        validate: true,
        ..OpenRequest::default()
    }
}

fn shop() -> Tenant {
    Tenant::new(
        "shop:example-shop",
        vec!["house-a".to_owned(), "house-b".to_owned()],
    )
}

fn open(product_key: &str) -> Configurator {
    let content = load_product(&source(), &request(product_key)).expect("load");
    Configurator::open(content, shop()).expect("open")
}

fn one(s: &str) -> Option<Pick> {
    Some(Pick::One(s.to_owned()))
}

fn label(c: &Configurator, group: &str) -> String {
    match c.picks().get(group) {
        Some(Pick::One(s)) => s.clone(),
        other => panic!("{group}: {other:?}"),
    }
}

#[test]
fn synthetic_vectors_match_exactly() {
    let text = String::from_utf8(read(&fixture_dir().join("resolve_vectors.jsonl"))).expect("utf8");
    let cases = parse_vectors(&text).expect("parse");
    assert_eq!(cases.len(), 32);
    let src = source();
    let mut products: BTreeMap<String, ProductRecord> = BTreeMap::new();
    for key in [
        "house-a/p0001",
        "house-b/p0002",
        "house-a/p0003",
        "house-a/p0004",
        "house-a/p0005",
        "house-a/p0006",
    ] {
        let content = load_product(
            &src,
            &OpenRequest {
                kit_paths: vec![],
                ..request(key)
            },
        )
        .expect("load");
        products.insert(key.to_owned(), content.product);
    }
    let report = run_vectors(&cases, &|k| products.get(k));
    assert!(report.mismatches.is_empty(), "{:#?}", report.mismatches);
    assert_eq!(report.matched, 32);
}

#[test]
fn a_vector_that_disagrees_is_reported_field_by_field() {
    let src = source();
    let content = load_product(&src, &request("house-a/p0001")).expect("load");
    let line = r#"{"product_key":"house-a/p0001","picks":{"Material":"Metal A"},"expect":{"orderable":true,"open":["Threading"],"retail":9.5,"new_field":1}}"#;
    let cases = parse_vectors(line).expect("parse");
    let report = run_vectors(&cases, &|_| Some(&content.product));
    let fields: Vec<&str> = report.mismatches.iter().map(|m| m.field.as_str()).collect();
    // `retail` is a pricing field (ignored); an unknown field is a mismatch, never skipped.
    assert_eq!(fields, ["new_field", "open", "orderable"]);
    let missing = run_vectors(&cases, &|_| None);
    assert_eq!(missing.mismatches[0].field, "product");
}

/// Every `.json` under `dir` whose `schema` is an option graph record.
fn product_records(dir: &Path, depth: usize, out: &mut BTreeMap<String, ProductRecord>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() && depth > 0 {
            product_records(&p, depth - 1, out);
        } else if p.extension().is_some_and(|x| x == "json")
            && let Ok(bytes) = std::fs::read(&p)
            && let Ok(record) = ProductRecord::from_json(&bytes)
        {
            out.insert(record.product_key.clone(), record);
        }
    }
}

#[test]
fn published_vectors_match_when_present() {
    // `REGIA_JW_REQUIRE_PILOT=1` is spec 42 § 5.2's spelling of the same flag.
    let required = ["REGIA_JW_CONTRACT_REQUIRED", "REGIA_JW_REQUIRE_PILOT"]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|v| v == "1"));
    let exchange = std::env::var_os("REGIA_JW_EXCHANGE").map(PathBuf::from);
    let vectors = exchange
        .as_ref()
        .map(|d| d.join("option_graph/1/fixtures/resolve_vectors.jsonl"))
        .filter(|p| p.is_file());
    let Some(path) = vectors else {
        assert!(
            !required,
            "REGIA_JW_CONTRACT_REQUIRED=1 but no option_graph/1/fixtures/resolve_vectors.jsonl under REGIA_JW_EXCHANGE"
        );
        eprintln!(
            "no published resolve vectors (REGIA_JW_EXCHANGE unset or without the file); synthetic set only"
        );
        return;
    };
    let mut products = BTreeMap::new();
    if let Some(dir) = path.parent() {
        // fixtures/optiongraph/<snapshot>/houses/<house>/products/<product>.json
        product_records(dir, 5, &mut products);
    }
    if let Some(out) = std::env::var_os("REGIA_JW_OUT") {
        product_records(&PathBuf::from(out).join("optiongraph"), 5, &mut products);
    }
    let text = std::fs::read_to_string(&path).expect("vectors");
    let cases = parse_vectors(&text).expect("parse");
    let report = run_vectors(&cases, &|k| products.get(k));
    eprintln!("{} published vectors, {} matched", report.cases, report.matched);
    assert!(report.cases > 0, "{} holds no vectors", path.display());
    assert!(
        report.mismatches.is_empty(),
        "{:#?}",
        &report.mismatches[..report.mismatches.len().min(20)]
    );
}

#[test]
fn open_starts_from_the_first_sold_combination() {
    let c = open("house-a/p0001");
    let r = c.resolve();
    assert!(r.orderable, "{r:?}");
    assert_eq!(r.variant_key.as_deref(), Some("1001"));
    assert_eq!(label(&c, "Gem"), "Clear A");
    assert!(
        !c.picks().contains_key("Engraving"),
        "Engraving applies to 1005 only"
    );
}

#[test]
fn options_mark_only_sold_values_selectable() {
    let mut c = open("house-a/p0001");
    let o = c.options();
    assert_eq!(o.listing, "complete");
    let group = |o: &regia_configurator::runtime::OptionsView, g: &str| {
        o.groups.iter().find(|x| x.group == g).cloned().expect("group")
    };
    let sel = |o: &regia_configurator::runtime::OptionsView, g: &str, l: &str| {
        group(o, g)
            .values
            .iter()
            .find(|v| v.label == l)
            .expect("value")
            .selectable
    };
    assert!(sel(&o, "Material", "Metal B"));
    assert!(!sel(&o, "Material", "Metal C"), "unavailable");
    assert!(!sel(&o, "Gauge", "18g"), "Threadless + 18g is unpriced");
    assert!(
        !sel(&o, "Anodization", "Color 1"),
        "anodize applies to Metal B only"
    );
    assert!(!sel(&o, "Gem", "Tinted") && !sel(&o, "Gem", "Opal-like"));
    assert!(!sel(&o, "Extras", "Reverse Set") && sel(&o, "Extras", "Sandblast"));
    assert!(!group(&o, "Anodization").applies && !group(&o, "Engraving").applies);
    assert!(group(&o, "Material").look_only && !group(&o, "Gem Size").look_only);
    assert!(group(&o, "Gauge").no_visual);
    assert_eq!(
        group(&o, "Material").values[0].material_id.as_deref(),
        Some("grey-metal-a")
    );
    c.set_pick("Material", one("Metal B"), SetMode::Strict)
        .expect("metal B");
    let o = c.options();
    assert!(sel(&o, "Anodization", "Color 1") && group(&o, "Anodization").applies);
    // Every selectable value is accepted strictly, every other one refused.
    for g in &o.groups {
        for v in &g.values {
            let mut t = c.clone();
            let pick = if g.multi {
                Some(Pick::Many(vec![v.label.clone()]))
            } else {
                one(&v.label)
            };
            assert_eq!(
                t.set_pick(&g.group, pick, SetMode::Strict).is_ok(),
                v.selectable,
                "{}={}",
                g.group,
                v.label
            );
        }
    }
}

#[test]
fn strict_refuses_and_adjust_drops_the_conflicts() {
    let mut c = open("house-a/p0001");
    let before = c.picks().clone();
    let e = c
        .set_pick("Gauge", one("18g"), SetMode::Strict)
        .expect_err("not sold");
    assert_eq!(e.reason, "configurator_not_sold");
    assert_eq!(c.picks(), &before, "a refusal changes nothing");
    let out = c.set_pick("Gauge", one("18g"), SetMode::Adjust).expect("adjust");
    let dropped: Vec<&str> = out.dropped.keys().map(String::as_str).collect();
    assert_eq!(dropped, ["Gem Size", "Material", "Threading"]);
    assert_eq!(c.resolve().variant_key.as_deref(), Some("1006"));
    assert_eq!(label(&c, "Threading"), "Threaded");
    assert_eq!(label(&c, "Gem"), "Clear A", "the gem pick survives");
    for (g, v, code) in [
        ("Material", "Metal C", "configurator_value_unavailable"),
        ("Material", "Metal Z", "configurator_unknown_value"),
        ("Gem", "Opal-like", "configurator_not_sold"),
        ("Colour", "Teal", "configurator_unknown_group"),
    ] {
        for mode in [SetMode::Strict, SetMode::Adjust] {
            let e = c.set_pick(g, one(v), mode).expect_err("refused");
            assert_eq!(e.reason, code, "{g}={v} {mode:?}");
        }
    }
    let e = c
        .set_pick("Extras", one("Sandblast"), SetMode::Strict)
        .expect_err("shape");
    assert_eq!(e.reason, "configurator_pick_shape");
    assert_eq!(
        c.set_pick("Gem", None, SetMode::Strict)
            .expect_err("required")
            .reason,
        "configurator_pick_required"
    );
    assert_eq!(
        c.set_pick("Material", None, SetMode::Strict)
            .expect_err("axis")
            .reason,
        "configurator_pick_required"
    );
    c.set_pick("Anodization", one("Color 2"), SetMode::Strict)
        .expect("anodize");
    c.set_pick("Anodization", None, SetMode::Strict)
        .expect("clear optional");
    assert!(!c.picks().contains_key("Anodization"));
}

#[test]
fn switching_into_a_required_addon_fills_it() {
    let mut c = open("house-a/p0001");
    c.set_pick("Gem Size", one("4x2mm"), SetMode::Strict)
        .expect("size");
    let out = c
        .set_pick("Threading", one("Threaded"), SetMode::Strict)
        .expect("threaded");
    assert_eq!(out.filled.get("Engraving"), Some(&Pick::One("Plain".to_owned())));
    assert_eq!(c.resolve().variant_key.as_deref(), Some("1005"));
}

#[test]
fn opening_refuses_what_is_not_for_sale_or_not_for_this_tenant() {
    let src = source();
    let cases = [
        ("house-a/p0003", shop(), "configurator_not_orderable"),
        ("house-a/p0004", shop(), "configurator_not_modelled"),
        ("house-a/p0005", shop(), "configurator_duplicate"),
        (
            "house-a/p0001",
            Tenant::new("house:house-b", vec![]),
            "configurator_house_not_allowed",
        ),
        (
            "house-a/p0001",
            Tenant::new("shop:other", vec!["house-b".to_owned()]),
            "configurator_house_not_allowed",
        ),
    ];
    for (key, tenant, code) in cases {
        let content = load_product(
            &src,
            &OpenRequest {
                kit_paths: vec![],
                ..request(key)
            },
        )
        .expect("load");
        let e = Configurator::open(content, tenant).expect_err("refused");
        assert_eq!(e.reason, code, "{key}");
    }
    assert!(
        Configurator::open(
            load_product(&src, &request("house-a/p0001")).expect("load"),
            Tenant::new("house:house-a", vec![])
        )
        .is_ok()
    );
    let missing = load_product(&src, &request("house-a/p9999")).expect_err("missing");
    assert_eq!(missing.reason, "configurator_missing_file");
    let bad = load_product(
        &src,
        &OpenRequest {
            recipe_path: Some("../x.json".to_owned()),
            ..request("house-a/p0001")
        },
    )
    .expect_err("bad path");
    assert_eq!(bad.reason, "configurator_bad_path");
}

/// spec 42 § 3.2: an incomplete listing's build carries `incomplete` in
/// `build().resolution.reasons` (it is a listing caveat, not a refusal: the
/// build can still be orderable).
#[test]
fn an_incomplete_listing_carries_incomplete_in_the_build_reasons() {
    let mut src = source();
    let record_path = "optiongraph/2026-11-04-000000000000/houses/house-a/products/p0001.json";
    let mut record: Value =
        serde_json::from_slice(&read(&fixture_dir().join(record_path))).expect("record json");
    record["status"] = json!("incomplete");
    src.insert(record_path, serde_json::to_vec(&record).expect("encode"));
    let content = load_product(&src, &request("house-a/p0001")).expect("load");
    let c = Configurator::open(content, shop()).expect("open");
    let build = c.build(&BuildOptions::default()).expect("build");
    assert_eq!(
        build["resolution"]["reasons"],
        json!([{ "code": "incomplete" }]),
        "{build}"
    );
    assert_eq!(
        build["resolution"]["orderable"], true,
        "a truncated listing still resolves what it lists"
    );
}

/// spec 42 § 5.1: a recipe that drives an option group the product does not
/// have is refused with `recipe_drive_unknown`.
#[test]
fn a_recipe_driving_an_unknown_group_is_refused() {
    let mut src = source();
    let recipe_path = "recipes/house-a/d0001.json";
    let mut recipe: Value =
        serde_json::from_slice(&read(&fixture_dir().join(recipe_path))).expect("recipe json");
    recipe["drives"]
        .as_array_mut()
        .expect("drives")
        .push(json!({ "group": "No Such Group", "to": { "material_zone": "body" } }));
    src.insert(recipe_path, serde_json::to_vec(&recipe).expect("encode"));
    let err = load_product(&src, &request("house-a/p0001")).expect_err("refused");
    assert_eq!(err.reason, "recipe_drive_unknown");
    assert!(err.message.contains("No Such Group"), "{err:?}");
}

#[test]
fn a_truncated_listing_shows_as_incomplete() {
    let mut c = open("house-b/p0002");
    let o = c.options();
    assert_eq!(o.listing, "incomplete");
    assert_eq!(o.flags, ["TRUNCATED_VARIATIONS"]);
    assert!(c.resolve().orderable, "its listed variants are still sold");
    assert_eq!(
        c.build(&BuildOptions::default()).expect_err("no recipe").reason,
        "configurator_no_recipe"
    );
    assert!(
        c.set_pick("Metal", one("Metal B"), SetMode::Strict).is_ok(),
        "S1 has metal B"
    );
    assert_eq!(
        c.set_pick("Style", one("S5"), SetMode::Strict)
            .expect_err("S5 is A only")
            .reason,
        "configurator_not_sold"
    );
    let line = c.resolve().made_to_order_line.expect("manual line");
    assert_eq!(line.variation_id, "2001");
}

fn look_of(c: &Configurator) -> Look {
    c.look()
}

#[test]
fn picks_drive_the_look() {
    let mut c = open("house-a/p0001");
    let l = look_of(&c);
    assert_eq!(l.zones["body"].material_id.as_deref(), Some("grey-metal-a"));
    assert_eq!(
        l.zones["gem:s1"].material_id.as_deref(),
        Some("clear-high-dispersion")
    );
    assert_eq!(l.gems[0].material_id.as_deref(), Some("clear-high-dispersion"));
    assert_eq!(l.gems[0].size_mm, Some(vec![3.0, 1.5]));
    assert_eq!(l.gems[0].cut_id, None, "the kit has no 3x1.5 marquise");
    assert!(l.unmapped.contains(&"cut:s1:marquise".to_owned()));
    assert_eq!(l.parts[0].kind.as_deref(), Some("end_base"));
    c.set_pick("Gem Size", one("4x2mm"), SetMode::Strict)
        .expect("size");
    c.set_pick("Material", one("Metal B"), SetMode::Strict)
        .expect("metal");
    c.set_pick("Anodization", one("Color 1"), SetMode::Strict)
        .expect("anodize");
    c.set_pick(
        "Extras",
        Some(Pick::Many(vec!["Sandblast".to_owned()])),
        SetMode::Strict,
    )
    .expect("finish");
    c.set_pick("Gem", one("Clear B"), SetMode::Strict).expect("gem");
    let l = look_of(&c);
    assert_eq!(l.gems[0].cut_id.as_deref(), Some("marquise-4x2"));
    assert_eq!(l.values["mq_l"], 4.0);
    assert_eq!(
        l.zones["body"].material_id.as_deref(),
        Some("grey-metal-a-film-1"),
        "anodize drives after the metal"
    );
    assert_eq!(l.gems[0].material_id.as_deref(), Some("clear-low-index"));
    assert!(
        l.unmapped.contains(&"Extras=Sandblast".to_owned()),
        "{:?}",
        l.unmapped
    );
    c.set_pick("Threading", one("Threaded"), SetMode::Adjust)
        .expect("threaded");
    let l = look_of(&c);
    assert_eq!(l.parts[0].part_ref, "shared/prong-head-4@1");
    assert_eq!(l.parts[0].kind.as_deref(), Some("prong_head"));
}

#[test]
fn metal_and_gem_swaps_keep_the_geometry_key() {
    let mut c = open("house-a/p0001");
    c.set_pick("Gem Size", one("4x2mm"), SetMode::Strict)
        .expect("size");
    let k = c.geometry_key();
    c.set_pick("Material", one("Metal B"), SetMode::Strict)
        .expect("metal");
    c.set_pick("Gem", one("Clear B"), SetMode::Strict).expect("gem");
    c.set_pick("Anodization", one("Color 2"), SetMode::Strict)
        .expect("anodize");
    assert_eq!(c.geometry_key(), k);
    c.set_pick("Gem Size", one("3x1.5mm"), SetMode::Strict)
        .expect("size");
    assert_ne!(c.geometry_key(), k);
    assert!(k.starts_with("sha256:") && k.len() == 71);
}

fn no_pricing_or_client_fields(v: &Value, at: &str) {
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                for banned in [
                    "cost", "retail", "price", "markup", "customer", "email", "ip", "address", "name",
                ] {
                    assert_ne!(k, banned, "{at}/{k}");
                }
                no_pricing_or_client_fields(x, &format!("{at}/{k}"));
            }
        }
        Value::Array(a) => a
            .iter()
            .enumerate()
            .for_each(|(i, x)| no_pricing_or_client_fields(x, &format!("{at}/{i}"))),
        _ => {}
    }
}

#[test]
fn the_build_passes_its_contract_and_round_trips() {
    let mut c = open("house-a/p0001");
    c.set_pick("Material", one("Metal B"), SetMode::Strict)
        .expect("metal");
    c.set_pick("Anodization", one("Color 2"), SetMode::Strict)
        .expect("anodize");
    c.set_wear("flipped").expect("wear");
    let mut cam = Camera::default_for(None);
    cam.tilt_deg = 80.0;
    cam.spin_deg = 25.0;
    cam.pan_mm = [5.0, -1.0];
    c.set_camera(cam);
    let build = c.build(&BuildOptions::default()).expect("build");
    assert!(check(&build, Some(Interface::Build), None).is_ok());
    no_pricing_or_client_fields(&build, "");
    assert_eq!(build["camera"]["tilt_deg"], json!(30.0));
    assert_eq!(build["camera"]["spin_deg"], json!(0.0));
    assert_eq!(build["camera"]["pan_mm"], json!([3.0, -1.0]));
    assert_eq!(build["wear"]["mirror"], json!(true));
    assert_eq!(build["resolution"]["variant_key"], "1003");
    assert_eq!(
        build["resolution"]["cart_lines"],
        json!([{"id": "1003", "quantity": 1}, {"id": "51", "quantity": 1}, {"id": "62", "quantity": 1}])
    );
    assert_eq!(build["tenant_id"], "shop:example-shop");
    // Spec 42 § 5.4: the viewer's tier, studio and gamut, and no depth of field.
    assert_eq!(build["tier"], "interactive");
    assert_eq!(build["output_gamut"], "srgb");
    assert_eq!(build["studio_id"], BuildOptions::default().studio_id.as_str());
    assert_eq!(build["camera"]["dof"], json!(false));
    assert_eq!(
        build,
        c.build(&BuildOptions::default()).expect("again"),
        "deterministic"
    );

    let mut back = open("house-a/p0001");
    let report = back.load_build(&build).expect("reopen");
    assert!(
        report.was_orderable && report.orderable_now && !report.snapshot_changed && !report.recipe_changed
    );
    assert_eq!(
        back.build(&BuildOptions::default()).expect("rebuild"),
        build,
        "round trip"
    );

    let recipe = back.content().recipe.clone().expect("recipe");
    let hash = recipe.hash.clone();
    let mut v: Value =
        serde_json::from_slice(&read(&fixture_dir().join("recipes/house-a/d0001.json"))).expect("json");
    v["recipe_version"] = json!(3);
    assert_ne!(Recipe::from_value(v).expect("recipe").hash, hash);
}

#[test]
fn a_saved_build_that_no_longer_resolves_is_marked() {
    let c = open("house-a/p0001");
    let build = c.build(&BuildOptions::default()).expect("build");
    // The next snapshot retires variant 1001.
    let path = fixture_dir().join(format!("optiongraph/{SNAP}/houses/house-a/products/p0001.json"));
    let mut v: Value = serde_json::from_slice(&read(&path)).expect("json");
    v["snapshot_id"] = json!("2026-11-05-000000000001");
    v["variants"][0]["discontinued"] = json!(true);
    let mut src = source();
    src.insert(
        format!("optiongraph/{SNAP}/houses/house-a/products/p0001.json"),
        serde_json::to_vec(&v).expect("bytes"),
    );
    let mut later = Configurator::open(
        load_product(&src, &request("house-a/p0001")).expect("load"),
        shop(),
    )
    .expect("open");
    let report = later.load_build(&build).expect("reopen");
    assert!(report.was_orderable && !report.orderable_now && report.snapshot_changed);
    assert_eq!(report.resolution["reasons"], json!([{ "code": "not_offered" }]));
    let other = open("house-b/p0002")
        .load_build(&build)
        .expect_err("other product");
    assert_eq!(other.reason, "configurator_wrong_product");
}

#[test]
fn documents_are_checked_against_published_schemas() {
    let mut src = source();
    // A published option-graph schema that demands a field the fixture lacks.
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["schema", "product_key", "extractor_version"],
    });
    src.insert(
        "exchange/option_graph/1/schema.json",
        serde_json::to_vec(&schema).expect("bytes"),
    );
    let e = load_product(&src, &request("house-a/p0001")).expect_err("contract");
    assert_eq!(e.reason, "configurator_contract");
    assert!(
        load_product(
            &src,
            &OpenRequest {
                validate: false,
                ..request("house-a/p0001")
            }
        )
        .is_ok()
    );
    // A kit from another house is refused.
    let mut src = source();
    let mut kit: Value = serde_json::from_slice(&read(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/jewelry-part-kit/kit-shared-basic.json"),
    ))
    .expect("kit");
    kit["house"] = json!("house-b");
    src.insert(
        "kits/house-b/k/kit.json",
        serde_json::to_vec(&kit).expect("bytes"),
    );
    let e = load_product(
        &src,
        &OpenRequest {
            kit_paths: vec!["kits/house-b/k/kit.json".to_owned()],
            validate: false,
            ..request("house-a/p0001")
        },
    )
    .expect_err("other house");
    assert_eq!(e.reason, "configurator_house_not_allowed");
}

#[test]
fn unselectable_values_say_why_and_name_their_conflicts() {
    let c = open("house-a/p0001");
    let o = c.options();
    let value = |g: &str, l: &str| {
        o.groups
            .iter()
            .find(|x| x.group == g)
            .and_then(|x| x.values.iter().find(|v| v.label == l))
            .cloned()
            .expect("value")
    };
    let v = value("Gauge", "18g");
    assert_eq!(
        (v.reason, v.conflicts.as_slice()),
        (
            Some("not_sold_with"),
            &[
                "Gem Size".to_owned(),
                "Material".to_owned(),
                "Threading".to_owned()
            ][..]
        )
    );
    let v = value("Anodization", "Color 1");
    assert_eq!(
        (v.reason, v.conflicts.as_slice()),
        (Some("not_sold_with"), &["Material".to_owned()][..])
    );
    assert_eq!(value("Material", "Metal C").reason, Some("unavailable"));
    assert_eq!(value("Gem", "Opal-like").reason, Some("not_orderable"));
    assert_eq!(value("Material", "Metal B").reason, None);
}

#[test]
fn adjust_changes_the_fewest_other_picks() {
    let mut c = open("house-a/p0001");
    let out = c
        .set_pick("Anodization", one("Color 1"), SetMode::Adjust)
        .expect("adjust");
    assert_eq!(out.dropped.keys().collect::<Vec<_>>(), ["Material"]);
    assert_eq!(
        c.resolve().variant_key.as_deref(),
        Some("1003"),
        "Threadless, 16g and 3x1.5mm kept"
    );
    assert_eq!(label(&c, "Material"), "Metal B");
}

#[test]
fn a_product_package_folder_opens() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut src = MemorySource::new();
    src.insert(
        "options.json",
        read(&fixture_dir().join(format!("optiongraph/{SNAP}/houses/house-a/products/p0001.json"))),
    );
    src.insert(
        "recipe.json",
        read(&fixture_dir().join("recipes/house-a/d0001.json")),
    );
    src.insert(
        "materials.json",
        read(&fixtures.join("jewelry-material-library/library-basic.json")),
    );
    let content = load_product(&src, &OpenRequest::package("house-a/p0001")).expect("package");
    assert_eq!(content.read, ["options.json", "recipe.json", "materials.json"]);
    let c = Configurator::open(content, Tenant::new("house:house-a", vec![])).expect("open");
    assert_eq!(
        c.look().zones["body"].material_id.as_deref(),
        Some("grey-metal-a")
    );
}

#[test]
fn builds_pass_the_published_build_schema_when_present() {
    let Some(exchange) = std::env::var_os("REGIA_JW_EXCHANGE").map(PathBuf::from) else {
        eprintln!(
            "REGIA_JW_EXCHANGE unset; the shipped build contract is checked in the_build_passes_its_contract_and_round_trips"
        );
        return;
    };
    let path = exchange.join("build/1/schema.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        let required = std::env::var("REGIA_JW_CONTRACT_REQUIRED").is_ok_and(|v| v == "1");
        assert!(!required, "no {}", path.display());
        return;
    };
    let schema = regia_configurator::Schema::parse(&text).expect("published schema compiles");
    for adjust in [false, true] {
        let mut c = open("house-a/p0001");
        if adjust {
            c.set_pick("Gauge", one("18g"), SetMode::Adjust).expect("adjust");
        }
        let build = c.build(&BuildOptions::default()).expect("build");
        let violations = schema.validate(&build);
        assert!(violations.is_empty(), "{violations:#?}");
    }
}

/// Audit (sprint-corr-low-editor): a host/data field the caller passed (a build option or the
/// tenant id) used to reach the contract check and fail as `configurator_build_invalid` — "an
/// engine bug". Caller input is now refused as `configurator_bad_options` before the build JSON
/// is composed; the check-failure path stays for what it names.
#[test]
fn a_bad_host_or_option_field_is_a_parameter_error_not_an_engine_bug() {
    let c = open("house-a/p0001");
    let bad_studio = BuildOptions {
        studio_id: "Bad Studio!".to_owned(),
        ..BuildOptions::default()
    };
    let err = c.build(&bad_studio).expect_err("a bad studio_id is refused");
    assert_eq!(
        err.reason, "configurator_bad_options",
        "caller input, not an engine bug: {}",
        err.message
    );
    let bad_id = BuildOptions {
        build_id: Some("not-a-uuid".to_owned()),
        ..BuildOptions::default()
    };
    assert_eq!(
        c.build(&bad_id).expect_err("a bad build_id is refused").reason,
        "configurator_bad_options"
    );
    let long = BuildOptions {
        content_build_id: "x".repeat(129),
        ..BuildOptions::default()
    };
    assert_eq!(
        c.build(&long)
            .expect_err("an over-long content_build_id is refused")
            .reason,
        "configurator_bad_options"
    );
    // Good values still build.
    let good = BuildOptions {
        studio_id: "studio-hero".to_owned(),
        build_id: Some("01234567-89ab-cdef-0123-456789abcdef".to_owned()),
        content_build_id: "cb-1".to_owned(),
        ..BuildOptions::default()
    };
    c.build(&good).expect("valid options build");
}

/// Audit (sprint-corr-configurator): a camera or wear outside the `jewelry.build/1` bounds was
/// stored as given, so that build and every later one failed its own contract check with
/// `configurator_build_invalid` ("an engine bug"). Lens numbers, pan and wear turns now stay inside
/// the schema; a saved build with a wear preset id the schema refuses is refused on reopen.
#[test]
fn an_out_of_range_camera_or_wear_never_poisons_later_builds() {
    let mut c = open("house-a/p0001");
    let mut cam = Camera::default_for(None);
    cam.focal_length_mm = 0.0;
    cam.sensor_mm = 300.0;
    cam.f_stop = 300.0;
    cam.focus_distance_mm = -1.0;
    c.set_camera(cam);
    let build = c.build(&BuildOptions::default()).expect("build after a bad lens");
    assert!(check(&build, Some(Interface::Build), None).is_ok());
    assert_eq!(build["camera"]["focal_length_mm"], json!(100.0));
    assert_eq!(build["camera"]["sensor_mm"], json!(200.0));
    assert_eq!(build["camera"]["f_stop"], json!(256.0));
    assert_eq!(build["camera"]["focus_distance_mm"], json!(60.0));

    // Pan stays within ±1000 mm even when the recipe's pan bounds are wider.
    let wide = regia_configurator::content::ViewRange {
        pan_bounds_mm: Some([[-5000.0, 5000.0], [-5000.0, 5000.0]]),
        ..Default::default()
    };
    let mut far = Camera::default_for(None);
    far.pan_mm = [3000.0, -4000.0];
    assert_eq!(far.clamped(&wide).pan_mm, [1000.0, -1000.0]);

    // A saved build with a 400° wear turn reopens and builds again.
    let mut saved = build.clone();
    saved["wear"]["rotation_deg"] = json!([400.0, -725.0, 0.0]);
    let mut back = open("house-a/p0001");
    back.load_build(&saved).expect("reopen");
    let rebuilt = back
        .build(&BuildOptions::default())
        .expect("build after a 400° wear");
    assert!(check(&rebuilt, Some(Interface::Build), None).is_ok());
    assert_eq!(rebuilt["wear"]["rotation_deg"], json!([40.0, -5.0, 0.0]));
    // A wear handed to `build` directly is held to the same bounds.
    let options = BuildOptions {
        wear: Some(regia_configurator::Wear {
            preset_id: "default".to_owned(),
            rotation_deg: [0.0, 361.0, 0.0],
            mirror: false,
        }),
        ..BuildOptions::default()
    };
    let direct = back.build(&options).expect("build with a wide wear");
    assert!(check(&direct, Some(Interface::Build), None).is_ok());

    saved["wear"]["preset_id"] = json!("Left Ear");
    let refused = open("house-a/p0001")
        .load_build(&saved)
        .expect_err("a preset id the build schema refuses");
    assert_eq!(refused.reason, "configurator_bad_document");
}
