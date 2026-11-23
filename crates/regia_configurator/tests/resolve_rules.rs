// SPDX-License-Identifier: MPL-2.0
//! The rules of `resolve()` the published vectors leave to the reference's text (ADR-2558), on small
//! generic records, so they hold in every test run with no exchange at hand.

use regia_configurator::content::{Recipe, ShopPhoto};
use regia_configurator::resolve::geometry_key;
use regia_configurator::{Pick, Picks, ProductRecord, Resolution, resolve};
use serde_json::{Value, json};

fn value(label: &str, code: Value) -> Value {
    json!({"value_key": format!("x={label}"), "label": label, "code": code, "available": true})
}

/// Two metals by two sizes, a required stone, an optional finish on `v2` only and a multi finish.
fn record(mode: &str) -> Value {
    json!({
        "schema": "jewelry.option_graph/1",
        "snapshot_id": "2026-11-04-000000000000",
        "product_key": "house-a/p1",
        "title": "Piece",
        "status": "orderable",
        "cart": {"platform": "test", "mode": mode},
        "axes": [
            {"group": "Metal", "kind": "metal", "values": [value("Red", json!(null)), value("Blue", json!(null))]},
            {"group": "Size", "kind": "gem_size", "values": [value("S", json!(null)), value("L", json!(null))]},
        ],
        "variants": [
            {"variant_key": "11", "sku": "v1", "picks": {"Metal": "Red", "Size": "S"}},
            {"variant_key": "12", "sku": "v2", "picks": {"Metal": "Blue", "Size": "S"}},
            {"variant_key": "13", "sku": "v3", "picks": {"Metal": "Blue", "Size": "L"}},
        ],
        "addons": [
            {"group": "Stone", "kind": "gem", "required": true, "multi": false, "applies_to": null,
             "values": [value("Clear", json!("51")), value("NoCode", json!(null))]},
            {"group": "Finish", "kind": "finish", "required": false, "multi": false, "applies_to": ["12"],
             "values": [value("Matte", json!("61"))]},
            {"group": "Extras", "kind": "finish", "required": false, "multi": true, "applies_to": null,
             "values": [value("A", json!("71")), value("B", json!("72"))]},
        ],
    })
}

fn product(doc: Value) -> ProductRecord {
    ProductRecord::from_value(doc).expect("record")
}

fn picks(doc: Value) -> Picks {
    serde_json::from_value(doc).expect("picks")
}

fn codes(r: &Resolution) -> Vec<&str> {
    r.reasons.iter().map(|x| x.code).collect()
}

#[test]
fn labels_and_groups_match_ignoring_ascii_case_and_answer_in_the_records_spelling() {
    let p = product(record("shopify_lines"));
    let r = resolve(&p, &picks(json!({"metal": "RED", "STONE": "clear"})));
    assert!(r.orderable, "{:?}", r.reasons);
    assert_eq!(r.variant_key.as_deref(), Some("11"));
    assert_eq!(
        r.picks,
        picks(json!({"Metal": "Red", "Size": "S", "Stone": "Clear", "Extras": []})),
        "the implied axis and the multi group that applies are filled in"
    );
    // Only ASCII folds.
    let mut doc = record("shopify_lines");
    doc["axes"][0]["values"][0]["label"] = json!("Éclat");
    doc["variants"][0]["picks"]["Metal"] = json!("Éclat");
    let r = resolve(
        &product(doc),
        &picks(json!({"Metal": "éclat", "Size": "S", "Stone": "Clear"})),
    );
    assert_eq!(codes(&r), ["unknown_value"]);
}

#[test]
fn the_statement_names_every_dimension_and_a_refusal_has_none() {
    let p = product(record("shopify_lines"));
    let r = resolve(
        &p,
        &picks(
            json!({"Metal": "Blue", "Size": "S", "Stone": "Clear", "Finish": "Matte", "Extras": ["B", "A"]}),
        ),
    );
    assert_eq!(
        r.statement.as_deref(),
        Some("Piece | Metal: Blue | Size: S | Stone: Clear | Finish: Matte | Extras: A, B")
    );
    let lines: Vec<&str> = r.cart_lines.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(
        lines,
        ["12", "51", "61", "71", "72"],
        "record order, not pick order"
    );
    assert_eq!(
        r.picks["Extras"],
        Pick::Many(vec!["A".to_owned(), "B".to_owned()])
    );

    let refused = resolve(&p, &picks(json!({"Metal": "Red", "Size": "L"})));
    assert_eq!(codes(&refused), ["not_offered"]);
    assert_eq!(refused.statement, None);
    assert!(refused.picks.is_empty());
}

#[test]
fn the_shared_format_has_exactly_the_schemas_fields_and_no_cost() {
    let mut doc = record("shopify_lines");
    doc["variants"][0]["cost"] = json!(50.0);
    doc["addons"][0]["values"][0]["price_delta"] = json!(10.0);
    let p = product(doc);
    let r = resolve(&p, &picks(json!({"Metal": "Red", "Stone": "Clear"})));
    let json = r.to_json();
    let mut keys: Vec<&str> = json
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "cart_lines",
            "cart_mode",
            "geometry_key",
            "made_to_order_line",
            "open",
            "orderable",
            "picks",
            "product_key",
            "reasons",
            "sku",
            "statement",
            "variant_key"
        ]
    );
    let refused = resolve(&p, &picks(json!({"Metal": "Blue"})));
    assert_eq!(
        refused.to_json()["reasons"],
        json!([{"code": "axis_open", "group": "Size"}]),
        "reason objects carry no message"
    );
    assert!(refused.to_agent_json()["details"][0]["message"].is_string());
}

#[test]
fn only_live_priced_variants_take_part() {
    let unpriced = |cost: Value| {
        let mut doc = record("shopify_lines");
        doc["variants"][0]["cost"] = cost;
        resolve(&product(doc), &picks(json!({"Metal": "Red", "Stone": "Clear"})))
    };
    assert_eq!(
        codes(&unpriced(json!(null))),
        ["not_offered"],
        "an explicit null is unpriced"
    );
    assert_eq!(codes(&unpriced(json!(0.0))), ["not_offered"]);
    assert!(unpriced(json!(5.0)).orderable);

    let mut stripped = record("shopify_lines");
    stripped["variants"][1]["cost"] = json!(5.0);
    assert!(
        resolve(
            &product(stripped),
            &picks(json!({"Metal": "Red", "Stone": "Clear"}))
        )
        .orderable,
        "a client pack has no cost field at all"
    );

    let mut gone = record("shopify_lines");
    gone["variants"][0]["discontinued"] = json!(true);
    let r = resolve(&product(gone), &picks(json!({"Metal": "Red", "Stone": "Clear"})));
    assert_eq!(codes(&r), ["not_offered"]);

    // A value no longer offered is never accepted, even implied by a variant nobody picked it on.
    let mut sold_out = record("shopify_lines");
    sold_out["axes"][1]["values"][1]["available"] = json!(false);
    let r = resolve(
        &product(sold_out),
        &picks(json!({"Metal": "Blue", "Stone": "Clear"})),
    );
    assert_eq!(
        r.variant_key.as_deref(),
        Some("12"),
        "the size L variant is out, so S is implied"
    );
}

#[test]
fn applies_to_null_is_every_variant_and_an_empty_list_is_none() {
    let mut doc = record("shopify_lines");
    doc["addons"][1]["applies_to"] = json!(null);
    let everywhere = resolve(
        &product(doc),
        &picks(json!({"Metal": "Red", "Stone": "Clear", "Finish": "Matte"})),
    );
    assert!(everywhere.orderable);
    let mut doc = record("shopify_lines");
    doc["addons"][1]["applies_to"] = json!([]);
    let nowhere = resolve(
        &product(doc),
        &picks(json!({"Metal": "Blue", "Size": "S", "Stone": "Clear", "Finish": "Matte"})),
    );
    assert_eq!(
        nowhere.to_json()["reasons"],
        json!([{"code": "addon_not_applicable", "group": "Finish"}])
    );
}

#[test]
fn checks_run_in_the_published_order() {
    let p = product(record("shopify_lines"));
    // Group errors in sorted name order, then the axes in record order; add-ons wait for a variant.
    let r = resolve(
        &p,
        &picks(json!({"Zed": "1", "Abc": "1", "Size": ["S"], "Metal": "Green", "Stone": "Nope"})),
    );
    assert_eq!(
        r.to_json()["reasons"],
        json!([
            {"code": "unknown_group", "group": "Abc"},
            {"code": "unknown_group", "group": "Zed"},
            {"code": "unknown_value", "group": "Metal", "value": "Green"},
            {"code": "wrong_pick_type", "group": "Size"},
        ])
    );
    // No variant: a bad add-on value is not looked at.
    let r = resolve(&p, &picks(json!({"Metal": "Red", "Size": "L", "Stone": "Nope"})));
    assert_eq!(codes(&r), ["not_offered"]);
    // A variant: not-applicable first, then the groups that apply, in record order.
    let r = resolve(
        &p,
        &picks(json!({"Metal": "Red", "Finish": "Nope", "Extras": ["A", "a"]})),
    );
    assert_eq!(
        r.to_json()["reasons"],
        json!([
            {"code": "addon_not_applicable", "group": "Finish"},
            {"code": "addon_required", "group": "Stone"},
            {"code": "duplicate_value", "group": "Extras", "value": "A"},
        ])
    );
    // Cart codes come last.
    let r = resolve(&p, &picks(json!({"Metal": "Red", "Stone": "NoCode"})));
    assert_eq!(
        r.to_json()["reasons"],
        json!([{"code": "no_cart_code", "group": "Stone", "value": "NoCode"}])
    );
}

#[test]
fn a_status_that_never_sells_answers_with_the_status() {
    for status in ["not_orderable", "duplicate"] {
        let mut doc = record("shopify_lines");
        doc["status"] = json!(status);
        let r = resolve(&product(doc), &picks(json!({"Metal": "Red", "Stone": "Clear"})));
        assert_eq!(
            r.to_json()["reasons"],
            json!([{"code": "product_not_orderable", "value": status}])
        );
    }
}

#[test]
fn a_manual_line_needs_a_code_too_and_a_multi_group_holds_a_list() {
    let mut doc = record("manual_line");
    doc["variants"][0]["variant_key"] = json!("900:7");
    doc["addons"][1]["applies_to"] = json!(null);
    let p = product(doc);
    let r = resolve(
        &p,
        &picks(json!({"Metal": "Red", "Stone": "Clear", "Finish": "Matte", "Extras": ["A"]})),
    );
    assert!(r.orderable && r.cart_lines.is_empty(), "{:?}", r.reasons);
    let line = r.made_to_order_line.clone().expect("line");
    assert_eq!(
        (line.variation_id.as_str(), line.metal_code.as_deref()),
        ("900", Some("7"))
    );
    assert_eq!(line.statement, r.statement.clone().expect("statement"));
    assert_eq!(
        serde_json::to_value(&line.attrs).expect("attrs"),
        json!({"Stone": "51", "Finish": "61", "Extras": ["71"]})
    );
    let r = resolve(&p, &picks(json!({"Metal": "Red", "Stone": "NoCode"})));
    assert_eq!(codes(&r), ["no_cart_code"]);
}

#[test]
fn an_add_on_code_that_is_the_variants_own_axis_value_adds_no_line() {
    let mut doc = record("shopify_lines");
    doc["axes"][0]["values"][0]["code"] = json!("51");
    let r = resolve(&product(doc), &picks(json!({"Metal": "Red", "Stone": "Clear"})));
    let lines: Vec<&str> = r.cart_lines.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(lines, ["11"], "catalogue research 9.6 step 4");
}

#[test]
fn variants_no_axis_tells_apart_refuse_with_a_reason() {
    let mut doc = record("shopify_lines");
    doc["variants"][1]["picks"] = json!({"Metal": "Red", "Size": "S"});
    let r = resolve(&product(doc), &picks(json!({"Metal": "Red", "Stone": "Clear"})));
    assert!(!r.orderable && r.open.is_empty());
    assert_eq!(codes(&r), ["not_offered"]);
}

#[test]
fn the_shop_photo_reader_keeps_files_in_the_folder() {
    let photo = |file: &str, schema: &str| {
        serde_json::to_vec(&json!({
            "schema": schema, "house": "house-a", "product_key": "house-a/p1", "variant_key": "11",
            "picks": {"Metal": "Red"}, "taken_at": "2026-11-04T12:00:00Z", "operator": "shop",
            "files": [{"file": file, "view": "top"}], "measurements_mm": {"width": 2.5}
        }))
        .expect("json")
    };
    let read = ShopPhoto::from_json(&photo("top.jpg", "jewelry.shop_photo/1")).expect("photo");
    assert_eq!(read.files[0].view, "top");
    assert_eq!(read.measurements_mm["width"], 2.5);
    for bad in ["../top.jpg", "a/top.jpg", "/top.jpg", "a\\top.jpg"] {
        assert!(
            ShopPhoto::from_json(&photo(bad, "jewelry.shop_photo/1")).is_err(),
            "{bad}"
        );
    }
    assert!(ShopPhoto::from_json(&photo("top.jpg", "jewelry.other/1")).is_err());
    assert!(ShopPhoto::from_json(b"not json").is_err());
}

/// `bake` is neither required nor read (book/agents/configurator.md).
#[test]
fn a_recipe_loads_with_or_without_a_bake_block() {
    let recipe = |bake: Option<Value>| {
        let mut doc = json!({"schema": "jewelry.piece_recipe/1", "design_key": "house-a/d1"});
        if let Some(b) = bake {
            doc["bake"] = b;
        }
        Recipe::from_value(doc)
    };
    assert!(recipe(None).is_ok());
    let on = recipe(Some(json!({"transport_tables": true, "probes": true}))).expect("on");
    let off = recipe(Some(json!({"transport_tables": false, "probes": false}))).expect("off");
    assert_ne!(
        on.hash, off.hash,
        "it still counts toward the hash, like every field"
    );
}

#[test]
fn the_geometry_key_follows_each_groups_role() {
    // Each hash is of the exact text the reference hashes, written out by hand (sorted keys, no
    // spaces, numbers as parsed) and hashed outside this crate.
    let mut doc = record("shopify_lines");
    doc["design_key"] = json!("house-a/D1");
    doc["axes"][0]["geometry_role"] = json!("excluded");
    doc["addons"][0]["geometry_role"] = json!("gem");
    doc["addons"][0]["values"][0]["gem"] =
        json!({"cut": "round", "size_mm": [3.0, 1.5], "count": 1, "cabochon": false, "material": "x"});
    doc["addons"][2]["geometry_role"] = json!("excluded");
    let p = product(doc);
    let r = resolve(
        &p,
        &picks(json!({"Metal": "Red", "Stone": "Clear", "Extras": ["A"]})),
    );
    // sha256 of {"design_key":"house-a/D1","picks":{"Size":"S","Stone":{"cabochon":false,"count":1,"cut":"round","size_mm":[3.0,1.5]}}}
    let want = "f451e9e918a22233071758eedc3aa3d8860b34c9df31965bb42556671e4a4ad6";
    assert_eq!(
        r.geometry_key.as_deref(),
        Some(format!("sha256:{}", want).as_str())
    );

    // An excluded group's swap keeps the key; a geometry group's moves it.
    let other = resolve(
        &p,
        &picks(json!({"Metal": "Blue", "Size": "S", "Stone": "Clear"})),
    );
    assert_eq!(other.geometry_key, r.geometry_key);
    let large = resolve(
        &p,
        &picks(json!({"Metal": "Blue", "Size": "L", "Stone": "Clear"})),
    );
    assert_ne!(large.geometry_key, r.geometry_key);

    // A multi group adds its list only when not empty; a role left out is `geometry`.
    let p = product(record("shopify_lines"));
    let none = geometry_key(&p, &picks(json!({"Metal": "Red", "Extras": []})));
    // sha256 of {"design_key":"house-a/p1","picks":{"Metal":"Red"}}
    let want = "7866be483224020514882824478af336e0715b44679a10538b6763c02de77fd4";
    assert_eq!(none, format!("sha256:{}", want));
    let two = geometry_key(&p, &picks(json!({"Extras": ["B", "A"]})));
    // sha256 of {"design_key":"house-a/p1","picks":{"Extras":["A","B"]}}
    let want = "42110cce275098bcb32ebfb0b7ffdbb3f001376727496ef7810aef4730a234c7";
    assert_eq!(two, format!("sha256:{}", want));

    // A refused build has no key.
    assert_eq!(resolve(&p, &picks(json!({"Metal": "Blue"}))).geometry_key, None);

    // A gem value without gem facets counts by its label.
    let mut doc = record("shopify_lines");
    doc["addons"][0]["geometry_role"] = json!("gem");
    let r = resolve(&product(doc), &picks(json!({"Metal": "Red", "Stone": "Clear"})));
    // sha256 of {"design_key":"house-a/p1","picks":{"Metal":"Red","Size":"S","Stone":"Clear"}}
    let want = "d8946e79d9df58c4a6006990af4bdcd19d99e73f1a08cece4abae567b8f58a9b";
    assert_eq!(r.geometry_key, Some(format!("sha256:{}", want)));
}

#[test]
fn an_unknown_geometry_role_is_refused_on_load() {
    let mut doc = record("shopify_lines");
    doc["axes"][0]["geometry_role"] = json!("shape");
    assert!(ProductRecord::from_value(doc).is_err());
}

/// A record using what option graph 1.2.0 added: `geometry_pick` on values and gem groups with a `slot`.
fn geometry_1_2_record() -> Value {
    let value = |label: &str, extra: Value| {
        let mut v =
            json!({"value_key": format!("x={label}"), "label": label, "code": "51", "available": true});
        if let (Some(v), Some(extra)) = (v.as_object_mut(), extra.as_object()) {
            v.extend(extra.clone());
        }
        v
    };
    let gem = |cut: &str, count: u32| json!({"gem": {"material": "cz", "class": "stone", "cut": cut, "cabochon": false, "size_mm": [2.25], "count": count}});
    let variant = |key: &str, style: &str| json!({"variant_key": key, "sku": key, "picks": {"Style": style}, "cost": 1.0});
    json!({
        "schema": "jewelry.option_graph/1",
        "snapshot_id": "2026-11-04-000000000000",
        "product_key": "house-a/p9",
        "house": "house-a",
        "design_key": "house-a/p9",
        "title": "Piece",
        "status": "orderable",
        "cart": {"platform": "test", "mode": "manual_line"},
        "axes": [{"group": "Style", "kind": "other", "values": [
            value("Prong Alexandrite", json!({"geometry_pick": {"model": "prong", "stones": [{"cut": "round", "size_mm": 2.25, "count": 1}]}})),
            value("Prong White CZ", json!({"geometry_pick": {"stones": [{"count": 1, "size_mm": 2.25, "cut": "round"}], "model": "prong"}})),
            value("Bezel", json!({"geometry_pick": {"model": "bezel", "stones": []}})),
            value("Plain", json!({})),
        ]}],
        "variants": [
            variant("1", "Prong Alexandrite"),
            variant("2", "Prong White CZ"),
            variant("3", "Bezel"),
            variant("4", "Plain"),
        ],
        "addons": [
            {"group": "Gem 1", "kind": "gem", "required": false, "multi": false, "applies_to": null, "slot": "s1", "geometry_role": "gem",
             "values": [value("Clear 2", gem("round", 1)), value("Clear 3", gem("round", 2))]},
            {"group": "Gem 1 - Titanium", "kind": "gem", "required": false, "multi": false, "applies_to": null, "slot": "s1", "geometry_role": "gem",
             "values": [value("Ti Clear 2", gem("round", 1))]},
            {"group": "Gem 2", "kind": "gem", "required": false, "multi": false, "applies_to": null, "slot": null, "geometry_role": "gem",
             "values": [value("Plain gem", gem("princess", 1))]},
            {"group": "Add", "kind": "other", "required": false, "multi": true, "applies_to": null,
             "values": [value("Tall", json!({"geometry_pick": {"h": 3}})), value("Wide", json!({}))]},
        ],
    })
}

/// Rule 8 as of option graph 1.2.0. The keys are the owner's reference `resolve()` run on this same record,
/// so a change on either side shows up here with no exchange at hand.
#[test]
fn geometry_keys_follow_the_1_2_rule_as_the_reference_computes_them() {
    let p = product(geometry_1_2_record());
    for (name, asked, want) in [
        (
            "a value's geometry_pick stands in for its label",
            json!({"Style": "Prong Alexandrite"}),
            "4e216bde0a3a105142c165ef4276464f5e9f1a31f434e8b1bf20fb9a227f7cb8",
        ),
        (
            "the same pick in another key order is the same model",
            json!({"Style": "Prong White CZ"}),
            "4e216bde0a3a105142c165ef4276464f5e9f1a31f434e8b1bf20fb9a227f7cb8",
        ),
        (
            "another pick is another model",
            json!({"Style": "Bezel"}),
            "2a1545d30097486930df8d769cb7025b2643d46363ffe3f56d7947ecb77a7870",
        ),
        (
            "a value with no geometry_pick still adds its label",
            json!({"Style": "Plain"}),
            "009485def72faccaff759033707e4befee8d9cc308c669211a96e3c981ca2ef5",
        ),
        (
            "a gem group with a slot enters as slot:<slot>",
            json!({"Style": "Plain", "Gem 1": "Clear 2"}),
            "0398ef1e34327ecb322a26ba4d9e6c1590d8cec9d6ff9cd8a014ce8dc9944067",
        ),
        (
            "so a per-metal copy of that group is one position",
            json!({"Style": "Plain", "Gem 1 - Titanium": "Ti Clear 2"}),
            "0398ef1e34327ecb322a26ba4d9e6c1590d8cec9d6ff9cd8a014ce8dc9944067",
        ),
        (
            "another stone count is another model",
            json!({"Style": "Plain", "Gem 1": "Clear 3"}),
            "2df718dcfb5b39fa5aa868519ef170bea74d0473540f719378544f8b736d160a",
        ),
        (
            "a second group on a taken slot keeps its own name",
            json!({"Style": "Plain", "Gem 1": "Clear 2", "Gem 1 - Titanium": "Ti Clear 2"}),
            "9aa07309c519664fd4d2aaa310d387c3e8808bbf3d70bd872fbf501815c5c622",
        ),
        (
            "a gem group with a null slot enters under its name",
            json!({"Style": "Plain", "Gem 2": "Plain gem"}),
            "022a0ccf371beb380f373eb67ffa57e113bd94613975c3b2ec2fa7d19f15bb76",
        ),
        (
            "a multi group lists its geometry picks and labels in record order",
            json!({"Style": "Plain", "Add": ["Wide", "Tall"]}),
            "2b69cebd10c23a6aed5cb40430f13e58d42a37a843f262f04f13d457bdfd2fbd",
        ),
    ] {
        let r = resolve(&p, &picks(asked));
        assert!(r.orderable, "{name}: {:?}", r.reasons);
        assert_eq!(
            r.geometry_key.as_deref(),
            Some(format!("sha256:{want}").as_str()),
            "{name}"
        );
    }
}

/// The schema types `geometry_pick` as an object, and the reference refuses anything else on load.
#[test]
fn a_geometry_pick_that_is_not_an_object_refuses_the_record() {
    let mut doc = geometry_1_2_record();
    doc["axes"][0]["values"][0]["geometry_pick"] = json!("prong");
    let err = ProductRecord::from_value(doc).expect_err("not an object");
    assert!(err.to_string().contains("geometry_pick"), "{err}");

    let mut explicit_null = geometry_1_2_record();
    explicit_null["axes"][0]["values"][3]["geometry_pick"] = json!(null);
    let same = resolve(&product(explicit_null), &picks(json!({"Style": "Plain"})));
    assert_eq!(
        same.geometry_key.as_deref(),
        Some("sha256:009485def72faccaff759033707e4befee8d9cc308c669211a96e3c981ca2ef5"),
        "a null geometry_pick means none"
    );
}

// Audit fixes (sprint-fix-audit-jw, findings 1 and 13, ADR-2685).

#[test]
fn an_empty_list_never_sells_a_required_multi_add_on() {
    let mut doc = record("shopify_lines");
    doc["addons"][2]["required"] = json!(true);
    let p = product(doc);
    let base = json!({"Metal": "Red", "Size": "S", "Stone": "Clear"});
    let mut empty = base.clone();
    empty["Extras"] = json!([]);
    for case in [base, empty] {
        let r = resolve(&p, &picks(case.clone()));
        assert!(!r.orderable, "{case}: {r:?}");
        assert_eq!(codes(&r), ["addon_required"], "{case}");
        assert_eq!(r.reasons[0].group.as_deref(), Some("Extras"));
        assert!(r.sku.is_none() && r.statement.is_none() && r.geometry_key.is_none());
    }
    let mut one = json!({"Metal": "Red", "Size": "S", "Stone": "Clear"});
    one["Extras"] = json!(["B"]);
    let r = resolve(&p, &picks(one));
    assert!(r.orderable, "{:?}", r.reasons);
    assert_eq!(r.picks.get("Extras"), Some(&Pick::Many(vec!["B".into()])));
}

#[test]
fn an_empty_label_is_no_pick_on_axes_and_add_ons() {
    let p = product(record("shopify_lines"));
    // An empty axis label leaves the axis open instead of naming an unknown value.
    let r = resolve(&p, &picks(json!({"Metal": "", "Size": "S", "Stone": "Clear"})));
    assert_eq!(codes(&r), ["axis_open"], "{:?}", r.reasons);
    assert_eq!(r.open, ["Metal"]);
    // An empty label for an add-on that does not apply to the variant picks nothing there.
    let r = resolve(
        &p,
        &picks(json!({"Metal": "Red", "Size": "S", "Stone": "Clear", "Finish": ""})),
    );
    assert!(r.orderable, "{:?}", r.reasons);
    assert!(!r.picks.contains_key("Finish"));
    // An empty label for a required add-on is still missing.
    let r = resolve(&p, &picks(json!({"Metal": "Red", "Size": "S", "Stone": ""})));
    assert_eq!(codes(&r), ["addon_required"]);
    // An empty list for a single-choice group is still the wrong shape.
    let r = resolve(&p, &picks(json!({"Metal": "Red", "Size": "S", "Stone": []})));
    assert_eq!(codes(&r), ["wrong_pick_type"]);
}

#[test]
fn two_spellings_of_one_group_must_agree() {
    let p = product(record("shopify_lines"));
    let r = resolve(
        &p,
        &picks(json!({"Metal": "Red", "metal": "Blue", "Size": "S", "Stone": "Clear"})),
    );
    assert!(!r.orderable, "the later spelling must not win silently: {r:?}");
    assert_eq!(codes(&r), ["duplicate_value"]);
    assert_eq!(r.reasons[0].group.as_deref(), Some("Metal"));
    let r = resolve(
        &p,
        &picks(json!({"Metal": "Red", "METAL": "red", "Size": "S", "Stone": "Clear"})),
    );
    assert!(r.orderable, "the same value twice is one pick: {:?}", r.reasons);
    assert_eq!(r.variant_key.as_deref(), Some("11"));
    let r = resolve(
        &p,
        &picks(
            json!({"Metal": "Red", "Size": "S", "Stone": "Clear", "Extras": ["A", "B"], "extras": ["b", "a"]}),
        ),
    );
    assert!(
        r.orderable,
        "a list in another order is the same pick: {:?}",
        r.reasons
    );
}

#[test]
fn incomplete_is_a_build_caveat_not_a_refusal_code() {
    // RESOLVE_REASONS is the refusal-code list (resolve.rs: every refusal carries a stable
    // code from it, and the list mirrors the shared option-graph schema's reason enum).
    // `incomplete` is a build caveat emitted by the build path for a truncated listing
    // (spec 42 § 3.2), not a refusal, and the shared schema's enum does not list it — a
    // consumer validating build reasons against that enum rejects codes outside it.
    let codes: Vec<&str> = regia_configurator::resolve::RESOLVE_REASONS
        .iter()
        .map(|(code, _)| *code)
        .collect();
    assert!(
        !codes.contains(&"incomplete"),
        "`incomplete` must not be advertised as a refusal code: {codes:?}"
    );
}
