// SPDX-License-Identifier: MPL-2.0
//! Option graph 1.2.0 in `resolve()` and the look (ADR-2693), on small generic records: the geometry
//! key hashes a value's `geometry_pick`, a slotted gem group counts as `slot:<slot>`, and a slot's
//! optional `positions_norm` is read, checked and handed to the viewer. 1.0 and 1.1 records (no
//! `geometry_pick`, no slot on a gem group, no positions) key and load as before.

use regia_configurator::resolve::geometry_key;
use regia_configurator::{Pick, Picks, ProductRecord, resolve};
use serde_json::{Value, json};

fn value(group: &str, label: &str, code: Value) -> Value {
    json!({"value_key": format!("{group}={label}"), "label": label, "code": code, "available": true})
}

fn stone(group: &str, label: &str, code: &str, cabochon: bool, material: &str) -> Value {
    let mut v = value(group, label, json!(code));
    v["gem"] = json!({"material": material, "cut": "round_brilliant", "cabochon": cabochon,
                      "count": 2, "size_mm": [2.0]});
    v
}

/// Two metals, each with its own list of the slot `s1` stones (a gold and a titanium group), two
/// styles that differ only in their stones' material (same `geometry_pick`) and a third with another
/// body.
fn record() -> Value {
    let pick = |body: &str| {
        json!({"body": body, "stones": [{"cabochon": false, "count": 1,
                                                             "cut": "pear", "size_mm": [5.0, 3.0]}]})
    };
    let mut style_a = value("Style", "A | pear | white", json!("s-a"));
    style_a["geometry_pick"] = pick("barbell with pear top");
    let mut style_b = value("Style", "B | pear | rutilated", json!("s-b"));
    style_b["geometry_pick"] = pick("barbell with pear top");
    let mut style_c = value("Style", "C | pear | white | milgrain", json!("s-c"));
    style_c["geometry_pick"] = pick("barbell with pear top and milgrain");
    let gold = "Gem 1 / 2";
    let ti = "Gem 1 / 2 - Titanium";
    json!({
        "schema": "jewelry.option_graph/1",
        "snapshot_id": "2026-11-05-000000000000",
        "product_key": "house-c/p1",
        "design_key": "house-c/D1",
        "title": "Piece",
        "status": "orderable",
        "cart": {"platform": "test", "mode": "shopify_lines"},
        "axes": [
            {"group": "Material", "kind": "metal", "geometry_role": "excluded",
             "values": [value("Material", "Gold", json!(null)), value("Material", "Titanium", json!(null))]},
            {"group": "Style", "kind": "style", "geometry_role": "geometry",
             "values": [style_a, style_b, style_c]},
        ],
        "variants": [
            {"variant_key": "1", "sku": "g-a", "picks": {"Material": "Gold", "Style": "A | pear | white"}},
            {"variant_key": "2", "sku": "t-a", "picks": {"Material": "Titanium", "Style": "A | pear | white"}},
            {"variant_key": "3", "sku": "g-b", "picks": {"Material": "Gold", "Style": "B | pear | rutilated"}},
            {"variant_key": "4", "sku": "g-c", "picks": {"Material": "Gold", "Style": "C | pear | white | milgrain"}},
        ],
        "addons": [
            {"group": gold, "kind": "gem", "geometry_role": "gem", "slot": "s1", "required": true,
             "multi": false, "applies_to": ["1", "3", "4"],
             "values": [stone(gold, "Black", "11", false, "ceramic"),
                        stone(gold, "Garnet", "12", false, "garnet"),
                        stone(gold, "Opal", "13", true, "opal")]},
            {"group": ti, "kind": "gem", "geometry_role": "gem", "slot": "s1", "required": true,
             "multi": false, "applies_to": ["2"],
             "values": [stone(ti, "Jet", "21", false, "ceramic"),
                        stone(ti, "Opal", "23", true, "opal")]},
            {"group": "Anodization", "kind": "finish", "geometry_role": "excluded", "required": false,
             "multi": false, "applies_to": ["2"], "values": [value("Anodization", "Blue", json!("31"))]},
        ],
        "slots": [
            {"slot": "s1", "role": "position_1", "count": 2, "cut": "round_brilliant",
             "positions_norm": [[0.074, 0.627], [0.926, 0.627]]},
            {"slot": "s2", "role": "center", "count": 1},
        ],
    })
}

fn product(doc: Value) -> ProductRecord {
    ProductRecord::from_value(doc).expect("record")
}

fn picks(doc: Value) -> Picks {
    serde_json::from_value(doc).expect("picks")
}

fn key(p: &ProductRecord, doc: Value) -> String {
    let r = resolve(p, &picks(doc));
    assert!(r.orderable, "{:?}", r.reasons);
    let k = r.geometry_key.clone().expect("a sold build has a key");
    assert_eq!(
        k,
        geometry_key(p, &r.picks),
        "the resolution's key is geometry_key(picks)"
    );
    k
}

#[test]
fn two_groups_of_one_slot_keep_both_entries_in_the_key() {
    // corr-configurator listed: the slot-collision fallback inserted the second group under its
    // own name without checking that name was free, so a group literally named `slot:<id>` (the
    // key the first group took) was overwritten and two different builds shared one geometry key.
    let mut doc = record();
    doc["addons"].as_array_mut().expect("addons").push(json!({
        "group": "slot:s1", "kind": "gem", "geometry_role": "gem", "slot": "s1",
        "required": true, "multi": false, "applies_to": ["1"],
        "values": [stone("slot:s1", "Sapphire", "41", false, "sapphire")],
    }));
    let p = product(doc);
    let key_of = |gem12: &str| {
        let r = resolve(
            &p,
            &picks(json!({
                "Material": "Gold",
                "Style": "A | pear | white",
                "Gem 1 / 2": gem12,
                "slot:s1": "Sapphire",
            })),
        );
        assert!(r.orderable, "{:?}", r.reasons);
        r.geometry_key.expect("a sold build has a key")
    };
    assert_ne!(
        key_of("Garnet"),
        key_of("Opal"),
        "the first group's stone shape stands in the key alongside the fallback group"
    );
}

#[test]
fn a_pure_stone_swap_keeps_the_key() {
    let p = product(record());
    let black = key(
        &p,
        json!({"Material": "Gold", "Style": "A | pear | white", "Gem 1 / 2": "Black"}),
    );
    let garnet = key(
        &p,
        json!({"Material": "Gold", "Style": "A | pear | white", "Gem 1 / 2": "Garnet"}),
    );
    assert_eq!(black, garnet, "same cut, size, count and cabochon: one model");
}

#[test]
fn a_style_whose_stones_differ_in_material_only_keeps_the_key() {
    let p = product(record());
    let a = key(
        &p,
        json!({"Material": "Gold", "Style": "A | pear | white", "Gem 1 / 2": "Black"}),
    );
    let b = key(
        &p,
        json!({"Material": "Gold", "Style": "B | pear | rutilated", "Gem 1 / 2": "Black"}),
    );
    let c = key(
        &p,
        json!({"Material": "Gold", "Style": "C | pear | white | milgrain", "Gem 1 / 2": "Black"}),
    );
    assert_eq!(a, b, "the geometry_pick is hashed, not the label");
    assert_ne!(a, c, "another body is another model");
}

#[test]
fn a_metal_swap_across_per_metal_gem_groups_keeps_the_key() {
    let p = product(record());
    let gold = key(
        &p,
        json!({"Material": "Gold", "Style": "A | pear | white", "Gem 1 / 2": "Black"}),
    );
    let ti = key(
        &p,
        json!({"Material": "Titanium", "Style": "A | pear | white", "Gem 1 / 2 - Titanium": "Jet",
               "Anodization": "Blue"}),
    );
    assert_eq!(gold, ti, "both groups fill slot s1, so both count as `slot:s1`");
}

#[test]
fn a_cabochon_and_a_faceted_stone_are_two_models() {
    let p = product(record());
    let faceted = key(
        &p,
        json!({"Material": "Gold", "Style": "A | pear | white", "Gem 1 / 2": "Black"}),
    );
    let cab = key(
        &p,
        json!({"Material": "Gold", "Style": "A | pear | white", "Gem 1 / 2": "Opal"}),
    );
    assert_ne!(faceted, cab);
    let ti_cab = key(
        &p,
        json!({"Material": "Titanium", "Style": "A | pear | white", "Gem 1 / 2 - Titanium": "Opal"}),
    );
    assert_eq!(cab, ti_cab, "a cabochon in either metal's list is the same model");
}

/// The key is `sha256:` of the compact JSON of `{design_key, picks}`, the slotted gem group under
/// `slot:<slot>` and the style under its `geometry_pick` (pinned, so a change of the rule is seen).
#[test]
fn the_key_hashes_the_documented_body() {
    let p = product(record());
    let r = resolve(
        &p,
        &picks(json!({"Material": "Gold", "Style": "A | pear | white", "Gem 1 / 2": "Black"})),
    );
    // sha256 (computed outside Regia) of:
    // {"design_key":"house-c/D1","picks":{"Style":{"body":"barbell with pear top",
    //  "stones":[{"cabochon":false,"count":1,"cut":"pear","size_mm":[5.0,3.0]}]},
    //  "slot:s1":{"cabochon":false,"count":2,"cut":"round_brilliant","size_mm":[2.0]}}}
    // (one line, no spaces).
    assert_eq!(
        r.geometry_key.as_deref(),
        Some("sha256:dcfbcb90204a9d23225ab00d0cc839081f61c25609efc60491f20b1b0da23f17")
    );
}

/// Two picked groups of one build that claim the same slot: the first in record order keys as
/// `slot:<slot>`, the later one under its own name, so neither stone drops out of the key.
#[test]
fn two_groups_on_one_slot_keep_both_stones_in_the_key() {
    let mut doc = record();
    // A second gold list on slot s1 that also applies to variant 1 (one position in two sizes).
    let mut second = doc["addons"][0].clone();
    second["group"] = json!("Gem 1 / 3");
    second["applies_to"] = json!(["1"]);
    for v in second["values"].as_array_mut().expect("values") {
        v["value_key"] = json!(format!("Gem 1 / 3={}", v["label"].as_str().unwrap_or_default()));
        v["gem"]["size_mm"] = json!([3.0]);
    }
    doc["addons"].as_array_mut().expect("addons").insert(1, second);
    let p = product(doc);
    let base = json!({"Material": "Gold", "Style": "A | pear | white", "Gem 1 / 2": "Black",
                      "Gem 1 / 3": "Black"});
    let k = key(&p, base.clone());
    let mut first = base.clone();
    first["Gem 1 / 2"] = json!("Opal");
    let mut later = base;
    later["Gem 1 / 3"] = json!("Opal");
    assert_ne!(key(&p, first.clone()), k, "the first group's stone is in the key");
    assert_ne!(key(&p, later.clone()), k, "the later group's stone is in the key");
    assert_ne!(key(&p, first), key(&p, later), "and they are told apart");
}

#[test]
fn a_record_without_the_1_2_fields_keys_by_group_name_and_label() {
    let mut doc = record();
    for v in doc["axes"][1]["values"].as_array_mut().expect("values") {
        v.as_object_mut().expect("value").remove("geometry_pick");
    }
    for a in doc["addons"].as_array_mut().expect("addons") {
        a.as_object_mut().expect("addon").remove("slot");
    }
    doc.as_object_mut().expect("record").remove("slots");
    let p = product(doc);
    assert!(p.slots.is_empty());
    let a = key(
        &p,
        json!({"Material": "Gold", "Style": "A | pear | white", "Gem 1 / 2": "Black"}),
    );
    let b = key(
        &p,
        json!({"Material": "Gold", "Style": "B | pear | rutilated", "Gem 1 / 2": "Black"}),
    );
    assert_ne!(a, b, "without a geometry_pick the label is the geometry");
    let ti = key(
        &p,
        json!({"Material": "Titanium", "Style": "A | pear | white", "Gem 1 / 2 - Titanium": "Jet"}),
    );
    assert_ne!(a, ti, "without a slot each group keys under its own name");
}

#[test]
fn slot_positions_reach_the_record_and_bad_ones_refuse_the_load() {
    let p = product(record());
    let s1 = p.slot("s1").expect("s1");
    assert_eq!(
        s1.positions_norm.as_deref(),
        Some(&[[0.074, 0.627], [0.926, 0.627]][..])
    );
    assert_eq!(p.slot("s2").map(|s| s.positions_norm.is_none()), Some(true));
    let mut bad = record();
    bad["slots"][0]["positions_norm"] = json!([[0.5, -0.1]]);
    let e = ProductRecord::from_value(bad).expect_err("outside the unit square");
    assert_eq!(e.reason, "configurator_bad_document");
}

#[test]
fn picks_resolve_the_same_with_or_without_the_new_fields() {
    let with = product(record());
    let mut doc = record();
    doc.as_object_mut().expect("record").remove("slots");
    let without = product(doc);
    let p =
        picks(json!({"Material": "Titanium", "Style": "A | pear | white", "Gem 1 / 2 - Titanium": "Jet"}));
    let (a, b) = (resolve(&with, &p), resolve(&without, &p));
    assert_eq!(
        (a.orderable, &a.variant_key, &a.sku, &a.statement),
        (b.orderable, &b.variant_key, &b.sku, &b.statement)
    );
    assert_eq!(
        a.picks.get("Anodization"),
        None::<&Pick>,
        "an optional add-on stays unpicked"
    );
}
