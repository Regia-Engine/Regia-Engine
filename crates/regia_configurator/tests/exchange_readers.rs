// SPDX-License-Identifier: MPL-2.0
//! The published fixtures against Regia's own readers (ADR-2558; plan `interfaces.md` § 0.6): every
//! recipe, option graph record and shop photo under `REGIA_JW_EXCHANGE` passes its owner's
//! `schema.json`, loads through the reader the viewer uses, and agrees with the other files beside it.
//! Then each product opens in the runtime and every value it offers really is sold.
//!
//! Nothing here names a product: the files are found by their `schema` field at test time, so nothing
//! about them enters the repo. Without `REGIA_JW_EXCHANGE` the tests only say so;
//! `REGIA_JW_CONTRACT_REQUIRED=1` (or `REGIA_JW_REQUIRE_PILOT=1`) makes that a failure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_configurator::content::{Recipe, ShopPhoto};
use regia_configurator::graph::GroupRef;
use regia_configurator::{
    BuildOptions, Configurator, Interface, MemorySource, OpenRequest, Pick, ProductRecord, Schema, SetMode,
    Tenant, check_bytes, load_product, resolve,
};
use serde_json::{Value, json};

/// One published fixture file.
struct Doc {
    path: PathBuf,
    /// Its path below the interface's `fixtures/` folder, `/`-separated.
    relative: String,
    bytes: Vec<u8>,
    value: Value,
}

fn exchange() -> Option<PathBuf> {
    let dir = std::env::var_os("REGIA_JW_EXCHANGE").map(PathBuf::from);
    let found = dir.filter(|d| d.join("option_graph/1/schema.json").is_file());
    if found.is_none() {
        // `REGIA_JW_REQUIRE_PILOT=1` is spec 42 § 5.2's spelling of the same flag.
        let required = ["REGIA_JW_CONTRACT_REQUIRED", "REGIA_JW_REQUIRE_PILOT"]
            .iter()
            .any(|name| std::env::var(name).is_ok_and(|v| v == "1"));
        assert!(
            !required,
            "REGIA_JW_CONTRACT_REQUIRED=1 but no exchange at REGIA_JW_EXCHANGE"
        );
        eprintln!("REGIA_JW_EXCHANGE unset or without option_graph/1/schema.json; nothing to check");
    }
    found
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "json") {
            out.push(p);
        }
    }
}

/// Every fixture of `interface`, by its `schema` field.
fn docs(exchange: &Path, interface: Interface) -> Vec<Doc> {
    docs_named(exchange, interface.wire_name(), &interface.schema_id())
}

/// Every fixture with the `schema` id `id` in `exchange/<wire_name>/1/fixtures`. At least the two
/// fixture pieces.
fn docs_named(exchange: &Path, wire_name: &str, id: &str) -> Vec<Doc> {
    let root = exchange.join(wire_name).join("1").join("fixtures");
    let mut files = Vec::new();
    walk(&root, &mut files);
    let found: Vec<Doc> = files
        .into_iter()
        .filter_map(|path| {
            let bytes = std::fs::read(&path).ok()?;
            let value: Value = serde_json::from_slice(&bytes).ok()?;
            (value.get("schema").and_then(Value::as_str) == Some(id)).then(|| Doc {
                relative: path
                    .strip_prefix(&root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default(),
                path,
                bytes,
                value,
            })
        })
        .collect();
    assert!(
        found.len() >= 2,
        "{id}: {} fixtures under {}",
        found.len(),
        root.display()
    );
    found
}

/// The shop photo's fixtures: an optional document, never a contract of the viewer's own, so it is
/// not in [`Interface`]; its owner's schema and Regia's reader still hold.
fn shop_photos(exchange: &Path) -> Vec<Doc> {
    docs_named(
        exchange,
        "shop_photo",
        regia_configurator::content::SHOP_PHOTO_SCHEMA,
    )
}

fn published(exchange: &Path, interface: Interface) -> Schema {
    published_named(exchange, interface.wire_name())
}

fn published_named(exchange: &Path, wire_name: &str) -> Schema {
    let path = exchange.join(wire_name).join("1").join("schema.json");
    let text = std::fs::read_to_string(&path).expect("published schema");
    Schema::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The part of the option graph schema `pointer` names, compiled on its own.
fn option_graph_part(exchange: &Path, pointer: &str) -> Schema {
    let text = std::fs::read_to_string(exchange.join("option_graph/1/schema.json")).expect("schema");
    let doc: Value = serde_json::from_str(&text).expect("schema json");
    Schema::compile(json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": pointer,
        "$defs": doc["$defs"],
    }))
    .expect("part compiles")
}

fn records(exchange: &Path) -> BTreeMap<String, ProductRecord> {
    docs(exchange, Interface::OptionGraph)
        .iter()
        .map(|d| {
            let record =
                ProductRecord::from_json(&d.bytes).unwrap_or_else(|e| panic!("{}: {e}", d.path.display()));
            (record.product_key.clone(), record)
        })
        .collect()
}

/// Every published document in the layout [`load_product`] expects, with where each record is (a
/// carried-over product keeps the snapshot it last changed in, so its id need not name its folder).
fn memory_source(exchange: &Path) -> MemorySource {
    let mut source = MemorySource::new();
    for interface in [Interface::OptionGraph, Interface::PieceRecipe] {
        for d in docs(exchange, interface) {
            source.insert(d.relative.clone(), d.bytes.clone());
        }
    }
    for interface in Interface::ALL {
        let path = exchange.join(interface.wire_name()).join("1").join("schema.json");
        if let Ok(bytes) = std::fs::read(&path) {
            source.insert(format!("exchange/{}/1/schema.json", interface.wire_name()), bytes);
        }
    }
    source
}

/// Whether a published recipe names `product_key`.
fn recipe_named(exchange: &Path, product_key: &str) -> bool {
    docs(exchange, Interface::PieceRecipe).iter().any(|d| {
        d.value["product_keys"]
            .as_array()
            .is_some_and(|keys| keys.iter().any(|k| k.as_str() == Some(product_key)))
    })
}

fn open(exchange: &Path, source: &MemorySource, record: &ProductRecord) -> Configurator {
    let path = docs(exchange, Interface::OptionGraph)
        .into_iter()
        .find(|d| d.value.get("product_key").and_then(Value::as_str) == Some(record.product_key.as_str()))
        .map(|d| d.relative);
    let content = load_product(
        source,
        &OpenRequest {
            product_key: record.product_key.clone(),
            option_graph_path: path,
            validate: true,
            ..OpenRequest::default()
        },
    )
    .unwrap_or_else(|e| panic!("{}: {e}", record.product_key));
    // Since option graph 1.2.0 the exchange holds the real pilot records, and recipes for only some
    // of them: a record a published recipe names must load with it; any other opens without one.
    assert_eq!(
        content.recipe.is_some(),
        recipe_named(exchange, &record.product_key),
        "{}: recipe found vs published",
        record.product_key
    );
    Configurator::open(
        content,
        Tenant::new("shop:example-shop", vec![record.house().to_owned()]),
    )
    .unwrap_or_else(|e| panic!("{}: {e}", record.product_key))
}

#[test]
fn recipes_option_graphs_and_shop_photos_pass_their_schema_and_our_readers() {
    let Some(exchange) = exchange() else { return };
    for interface in [Interface::OptionGraph, Interface::PieceRecipe] {
        let schema = published(&exchange, interface);
        for d in docs(&exchange, interface) {
            let checked = check_bytes(&d.bytes, Some(interface), Some(&schema));
            assert_eq!(checked, Ok(interface), "{}", d.path.display());
            let read = match interface {
                Interface::OptionGraph => ProductRecord::from_json(&d.bytes).map(|_| ()),
                _ => Recipe::from_json(&d.bytes).map(|_| ()),
            };
            assert_eq!(read, Ok(()), "{}", d.path.display());
        }
    }
    let schema = published_named(&exchange, "shop_photo");
    for d in shop_photos(&exchange) {
        let violations = schema.validate(&d.value);
        assert!(violations.is_empty(), "{}: {violations:#?}", d.path.display());
        assert_eq!(
            ShopPhoto::from_json(&d.bytes).map(|_| ()),
            Ok(()),
            "{}",
            d.path.display()
        );
    }
}

/// The recipe names records that exist, holds their content hashes, and accounts for their groups.
#[test]
fn recipes_agree_with_their_option_graph() {
    let Some(exchange) = exchange() else { return };
    let records = records(&exchange);
    for d in docs(&exchange, Interface::PieceRecipe) {
        let recipe = Recipe::from_json(&d.bytes).expect("recipe");
        assert!(!recipe.product_keys.is_empty(), "{}", d.path.display());
        for key in &recipe.product_keys {
            let record = records
                .get(key)
                .unwrap_or_else(|| panic!("{}: no record of {key}", d.path.display()));
            assert_eq!(record.design_key(), recipe.design_key);
            assert_eq!(record.house(), recipe.house);
            let hashes = d.value.pointer("/option_graph/content_hashes");
            assert_eq!(
                hashes.and_then(|h| h.get(key)).and_then(Value::as_str),
                record.content_hash.as_deref(),
                "{key}"
            );
            for drive in &recipe.drives {
                assert!(
                    record.group(&drive.group).is_some(),
                    "{key}: drive of the unknown group {}",
                    drive.group
                );
            }
            let groups = record
                .axes
                .iter()
                .map(|a| &a.group)
                .chain(record.addons.iter().map(|a| &a.group));
            for group in groups {
                assert!(
                    recipe.drives_of(group).next().is_some() || recipe.no_visual.contains(group),
                    "{key}: {group} is neither driven nor no_visual"
                );
            }
        }
    }
}

/// A photo's picks resolve to the variant it says it shows.
#[test]
fn shop_photos_resolve_to_their_variant() {
    let Some(exchange) = exchange() else { return };
    let records = records(&exchange);
    for d in shop_photos(&exchange) {
        let photo = ShopPhoto::from_json(&d.bytes).expect("photo");
        let record = records
            .get(&photo.product_key)
            .unwrap_or_else(|| panic!("{}: no record", d.path.display()));
        assert_eq!(record.house(), photo.house);
        let r = resolve(record, &photo.picks);
        assert!(r.orderable, "{}: {:?}", d.path.display(), r.reasons);
        assert_eq!(r.variant_key.as_deref(), Some(photo.variant_key.as_str()));
    }
}

/// Each published product opens; every value the runtime offers is accepted and every other one
/// refused; what it writes passes the published formats and carries no cost.
#[test]
fn published_products_open_and_only_sell_what_resolves() {
    let Some(exchange) = exchange() else { return };
    let source = memory_source(&exchange);
    let records = records(&exchange);
    let resolution_part = option_graph_part(&exchange, "#/$defs/resolution");
    let build_schema = published(&exchange, Interface::Build);
    assert!(records.len() >= 2);
    let mut built = 0;
    for (key, record) in &records {
        let c = open(&exchange, &source, record);
        let start = c.resolve();
        assert!(start.orderable, "{key}: {:?}", start.reasons);
        let view = c.options();
        assert_eq!(view.product_key, *key);
        for group in &view.groups {
            for value in &group.values {
                let pick = if group.multi {
                    Pick::Many(vec![value.label.clone()])
                } else {
                    Pick::One(value.label.clone())
                };
                let mut strict = c.clone();
                let accepted = strict
                    .set_pick(&group.group, Some(pick.clone()), SetMode::Strict)
                    .is_ok();
                assert_eq!(
                    accepted, value.selectable,
                    "{key}: {}={}",
                    group.group, value.label
                );
                if accepted {
                    let r = strict.resolve();
                    assert!(
                        r.orderable,
                        "{key}: {}={}: {:?}",
                        group.group, value.label, r.reasons
                    );
                    let shape = resolution_part.validate(&r.to_json());
                    assert!(shape.is_empty(), "{key}: {shape:#?}");
                }
                let mut adjust = c.clone();
                if adjust.set_pick(&group.group, Some(pick), SetMode::Adjust).is_ok() {
                    assert!(adjust.resolve().orderable, "{key}: adjust {}", group.group);
                } else {
                    assert!(!value.selectable, "{key}: {}={}", group.group, value.label);
                }
            }
        }
        // Every record carries staff costs; no answer of any record, with a recipe or not, shows them.
        // The message names the field only, so a failure never writes a cost into a log.
        let text = serde_json::to_string(&start.to_agent_json()).expect("json");
        for field in ["\"cost\"", "\"retail\"", "\"price_delta\""] {
            assert!(!text.contains(field), "{key}: {field} in the answer");
        }
        if c.content().recipe.is_none() {
            // No published recipe: resolve and the options work, a build is refused by its code.
            let refused = c.build(&BuildOptions::default()).err().map(|e| e.reason);
            assert_eq!(refused, Some("configurator_no_recipe"), "{key}");
            continue;
        }
        built += 1;
        let build = c.build(&BuildOptions::default()).expect("build");
        let shape = build_schema.validate(&build);
        assert!(shape.is_empty(), "{key}: {shape:#?}");
        assert_eq!(build["resolution"]["statement"], json!(start.statement));
        assert_eq!(
            build["geometry_key"],
            json!(start.geometry_key),
            "{key}: one key everywhere"
        );
        assert!(!serde_json::to_string(&build).expect("json").contains("\"cost\""));
        let mut back = open(&exchange, &source, record);
        let report = back.load_build(&build).expect("reload");
        assert!(report.orderable_now && !report.snapshot_changed && !report.recipe_changed);
    }
    let named = records.keys().filter(|k| recipe_named(&exchange, k)).count();
    assert!(
        built >= 1 && built == named,
        "{built} built, {named} with a recipe"
    );
}

/// The vectors' resolutions, as Regia writes them, pass the published `resolution` definition, and a
/// refused one has neither a statement nor a cart.
#[test]
fn refused_and_sold_resolutions_fit_the_published_format() {
    let Some(exchange) = exchange() else { return };
    let records = records(&exchange);
    let part = option_graph_part(&exchange, "#/$defs/resolution");
    let text = std::fs::read_to_string(exchange.join("option_graph/1/fixtures/resolve_vectors.jsonl"))
        .expect("vectors");
    let cases = regia_configurator::vectors::parse_vectors(&text).expect("parse");
    assert!(!cases.is_empty());
    for case in &cases {
        let record = records.get(&case.product_key).expect("record");
        let r = resolve(record, &case.picks);
        let shape = part.validate(&r.to_json());
        assert!(shape.is_empty(), "line {}: {shape:#?}", case.line);
        if !r.orderable {
            assert!(r.statement.is_none() && r.cart_lines.is_empty() && r.picks.is_empty());
            assert!(!r.reasons.is_empty(), "line {}", case.line);
        }
    }
}

/// A published build holds what Regia's own `resolve()` answers for its picks: the same resolution,
/// `geometry_key`, top-level `sku` and `house_variant_id`. So the kit builder's records and vectors and
/// Regia's builds describe one piece, and a republished option graph that moves a key shows up here
/// instead of as a build that points at the wrong 3D asset. A refused build's key follows the same rule
/// from the picks it asked for (`Configurator::geometry_key`), since its resolution has none.
#[test]
fn published_builds_are_what_regia_resolves() {
    let Some(exchange) = exchange() else { return };
    let records = records(&exchange);
    let builds = docs(&exchange, Interface::Build);
    assert!(
        builds
            .iter()
            .any(|d| d.value["resolution"]["orderable"] == json!(true))
    );
    assert!(
        builds
            .iter()
            .any(|d| d.value["resolution"]["orderable"] == json!(false))
    );
    for d in &builds {
        let name = &d.relative;
        let key = d.value["product_key"].as_str().expect("product_key");
        let record = records
            .get(key)
            .unwrap_or_else(|| panic!("{name}: no record for {key}"));
        assert_eq!(
            d.value["snapshot_id"],
            json!(record.snapshot_id),
            "{name}: this build was cut from another catalog snapshot, so it needs refreshing"
        );
        let picks: regia_configurator::resolve::Picks =
            serde_json::from_value(d.value["picks"].clone()).unwrap_or_else(|e| panic!("{name}: picks: {e}"));
        let r = resolve(record, &picks);
        assert_eq!(d.value["resolution"], r.to_build_json(), "{name}: resolution");
        let geometry = r
            .geometry_key
            .clone()
            .unwrap_or_else(|| regia_configurator::resolve::geometry_key(record, &picks));
        assert_eq!(d.value["geometry_key"], json!(geometry), "{name}: geometry_key");
        assert_eq!(d.value["sku"], json!(r.sku), "{name}: sku");
        assert_eq!(
            d.value["house_variant_id"],
            json!(r.variant_key),
            "{name}: variant"
        );
    }
}

/// A recipe's `bake` block changes nothing the viewer does: left out, all on or all off, the file
/// loads and the look is the same. `interfaces.md` v1 still lists it; the v2 viewer has no use for it.
#[test]
fn the_bake_block_is_optional_and_changes_no_look() {
    let Some(exchange) = exchange() else { return };
    let records = records(&exchange);
    for d in docs(&exchange, Interface::PieceRecipe) {
        let recipe = Recipe::from_value(d.value.clone()).expect("recipe");
        let Some(key) = recipe.product_keys.first() else {
            continue;
        };
        let Some(record) = records.get(key) else {
            continue;
        };
        let look = |recipe_value: Value| {
            let content = regia_configurator::source::ProductContent {
                product: record.clone(),
                recipe: Some(Recipe::from_value(recipe_value).expect("recipe")),
                kits: Vec::new(),
                library: None,
                read: Vec::new(),
            };
            let tenant = Tenant::new("shop:example-shop", vec![record.house().to_owned()]);
            let c = Configurator::open(content, tenant).expect("open");
            (c.look(), c.geometry_key(), c.resolve().to_json())
        };
        let base = look(d.value.clone());
        let mut without = d.value.clone();
        if let Some(o) = without.as_object_mut() {
            o.remove("bake");
        }
        assert_eq!(look(without), base, "{}: bake left out", d.path.display());
        let mut on = d.value.clone();
        on["bake"] = json!({"transport_tables": true, "probes": true, "front_still": true});
        assert_eq!(look(on), base, "{}: bake on", d.path.display());
    }
}

/// Matching ignores ASCII case at every entry point, and the state holds the record's spelling.
#[test]
fn set_pick_takes_any_ascii_case_and_keeps_the_records_spelling() {
    let Some(exchange) = exchange() else { return };
    let source = memory_source(&exchange);
    let records = records(&exchange);
    for (key, record) in &records {
        let c = open(&exchange, &source, record);
        let view = c.options();
        let Some((group, value)) = view.groups.iter().find_map(|g| {
            (!g.multi)
                .then(|| {
                    g.values
                        .iter()
                        .find(|v| v.selectable && !v.picked)
                        .map(|v| (g, v))
                })
                .flatten()
        }) else {
            continue;
        };
        let mut loud = c.clone();
        loud.set_pick(
            &group.group.to_ascii_uppercase(),
            Some(Pick::One(value.label.to_ascii_lowercase())),
            SetMode::Strict,
        )
        .unwrap_or_else(|e| panic!("{key}: {e}"));
        let mut plain = c.clone();
        plain
            .set_pick(
                &group.group,
                Some(Pick::One(value.label.clone())),
                SetMode::Strict,
            )
            .expect("plain");
        assert_eq!(loud.picks(), plain.picks(), "{key}");
        assert!(matches!(
            record.group_folded(&group.group.to_ascii_lowercase()),
            Some(GroupRef::Axis(_) | GroupRef::Addon(_))
        ));
    }
}
