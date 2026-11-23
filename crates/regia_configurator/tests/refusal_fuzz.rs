// SPDX-License-Identifier: MPL-2.0
//! Refusal fuzz (ADR-2534): random small option graphs, an independent brute-force list of every
//! combination each one sells, and random picks. Properties:
//!
//! 1. `resolve()` is orderable only for a sold combination, and for every sold combination.
//! 2. A configurator session never holds an unsold build: after every accepted `set_pick`
//!    (strict or adjust) the build resolves to a sold combination, and a refused pick changes
//!    nothing.
//! 3. `options()` marks a value selectable exactly when a strict `set_pick` of it is accepted.
//!
//! Deterministic: a fixed-seed splitmix64, fixed counts.

use std::collections::{BTreeMap, BTreeSet};

use regia_configurator::resolve::{Pick, Picks, resolve};
use regia_configurator::source::ProductContent;
use regia_configurator::{Configurator, ProductRecord, SetMode, Tenant};
use serde_json::{Value, json};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// A random product record (JSON) with 1-3 axes, a random subset of the axis grid as variants
/// (some unpriced or discontinued), and 0-3 add-ons of every shape.
fn random_product(rng: &mut Rng, n: usize) -> Value {
    let axes: Vec<Value> = (0..1 + rng.below(3))
        .map(|a| {
            let values: Vec<Value> = (0..1 + rng.below(3))
                .map(|v| json!({"label": format!("a{a}v{v}"), "code": format!("{}", 100 + a * 10 + v), "available": !rng.chance(10)}))
                .collect();
            json!({"group": format!("Axis {a}"), "kind": "axis", "values": values})
        })
        .collect();
    let mut grid: Vec<BTreeMap<String, String>> = vec![BTreeMap::new()];
    for axis in &axes {
        let mut next = Vec::new();
        for cell in &grid {
            for v in axis["values"].as_array().into_iter().flatten() {
                let mut c = cell.clone();
                c.insert(
                    axis["group"].as_str().unwrap_or_default().to_owned(),
                    v["label"].as_str().unwrap_or_default().to_owned(),
                );
                next.push(c);
            }
        }
        grid = next;
    }
    let mut variants = Vec::new();
    for (i, cell) in grid.iter().enumerate() {
        if rng.chance(35) {
            continue;
        }
        let cost = if rng.chance(10) { 0.0 } else { 10.0 };
        variants.push(
            json!({"variant_key": format!("{}", 1000 + i), "sku": format!("SKU-{i}"), "picks": cell,
                             "cost": cost, "discontinued": rng.chance(8)}),
        );
    }
    let keys: Vec<String> = variants
        .iter()
        .filter_map(|v| v["variant_key"].as_str().map(str::to_owned))
        .collect();
    let addons: Vec<Value> = (0..rng.below(4))
        .map(|a| {
            let values: Vec<Value> = (0..1 + rng.below(3))
                .map(|v| {
                    let code = match rng.below(10) {
                        0 => Value::Null,
                        1 => json!("x-not-numeric"),
                        _ => json!(format!("{}", 500 + a * 10 + v)),
                    };
                    json!({"label": format!("o{a}v{v}"), "code": code, "available": !rng.chance(15)})
                })
                .collect();
            // `null` is every variant; a list names them (an empty list would be none).
            let applies = if rng.chance(50) {
                Value::Null
            } else {
                let some: Vec<&String> = keys.iter().filter(|_| rng.chance(50)).collect();
                if some.is_empty() { Value::Null } else { json!(some) }
            };
            let multi = rng.chance(30);
            json!({"group": format!("Addon {a}"), "kind": "addon", "required": !multi && rng.chance(50), "multi": multi,
                   "applies_to": applies, "values": values})
        })
        .collect();
    let mode = if rng.chance(70) {
        "shopify_lines"
    } else {
        "manual_line"
    };
    json!({"schema": "jewelry.option_graph/1", "snapshot_id": "2026-11-04-000000000000",
           "product_key": format!("house-a/f{n}"), "title": "Fuzz", "status": "orderable",
           "axes": axes, "variants": variants, "addons": addons, "cart": {"mode": mode}})
}

/// Every sold combination, enumerated straight from the record's data (no runtime code):
/// (variant key, picks with every axis and only non-empty add-ons).
fn sold(v: &Value) -> BTreeSet<(String, Picks)> {
    let shopify = v["cart"]["mode"] == "shopify_lines";
    let ok_value = |x: &Value| {
        // A Shopify cart needs a numeric code, a manual line any code.
        x["available"].as_bool().unwrap_or(true)
            && x["code"]
                .as_str()
                .is_some_and(|c| !c.is_empty() && (!shopify || c.bytes().all(|b| b.is_ascii_digit())))
    };
    let axes = v["axes"].as_array().cloned().unwrap_or_default();
    let mut out = BTreeSet::new();
    for variant in v["variants"].as_array().into_iter().flatten() {
        if variant["discontinued"].as_bool().unwrap_or(false)
            || variant["cost"].as_f64().unwrap_or(1.0) <= 0.0
        {
            continue;
        }
        let key = variant["variant_key"].as_str().unwrap_or_default().to_owned();
        // Axis values the variant picks must be available.
        let mut base = Picks::new();
        let mut fine = true;
        for axis in &axes {
            let g = axis["group"].as_str().unwrap_or_default();
            let l = variant["picks"][g].as_str().unwrap_or_default();
            let avail = axis["values"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|x| x["label"] == l && x["available"].as_bool().unwrap_or(true));
            fine &= avail;
            base.insert(g.to_owned(), Pick::One(l.to_owned()));
        }
        if !fine {
            continue;
        }
        let mut combos = vec![base];
        for addon in v["addons"].as_array().into_iter().flatten() {
            let applies = addon["applies_to"]
                .as_array()
                .is_none_or(|l| l.iter().any(|k| k == &json!(key)));
            if !applies {
                continue;
            }
            let g = addon["group"].as_str().unwrap_or_default().to_owned();
            let good: Vec<String> = addon["values"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|x| ok_value(x))
                .filter_map(|x| x["label"].as_str().map(str::to_owned))
                .collect();
            let mut choices: Vec<Option<Pick>> = Vec::new();
            if addon["multi"].as_bool().unwrap_or(false) {
                for mask in 0..(1usize << good.len()) {
                    let set: Vec<String> = good
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| mask & (1 << i) != 0)
                        .map(|(_, l)| l.clone())
                        .collect();
                    choices.push((!set.is_empty()).then_some(Pick::Many(set)));
                }
            } else {
                if !addon["required"].as_bool().unwrap_or(false) {
                    choices.push(None);
                }
                choices.extend(good.iter().map(|l| Some(Pick::One(l.clone()))));
            }
            let mut next = Vec::new();
            for c in &combos {
                for choice in &choices {
                    let mut p = c.clone();
                    if let Some(pick) = choice {
                        p.insert(g.clone(), pick.clone());
                    }
                    next.push(p);
                }
            }
            combos = next;
        }
        out.extend(combos.into_iter().map(|p| (key.clone(), p)));
    }
    out
}

/// The full form of a resolved build's picks: every axis from the variant, non-empty add-ons in
/// the value order of their group.
fn normalized(record: &ProductRecord, variant_key: &str, picks: &Picks) -> Picks {
    let mut out = Picks::new();
    if let Some(v) = record.variants.iter().find(|v| v.variant_key == variant_key) {
        for (g, l) in &v.picks {
            out.insert(g.clone(), Pick::One(l.clone()));
        }
    }
    for addon in &record.addons {
        if let Some(p) = picks.get(&addon.group).filter(|p| !p.is_empty()) {
            let chosen = p.labels();
            out.insert(
                addon.group.clone(),
                match p {
                    Pick::One(l) => Pick::One(l.clone()),
                    Pick::Many(_) => Pick::Many(
                        addon
                            .values
                            .iter()
                            .filter(|v| chosen.contains(&v.label.as_str()))
                            .map(|v| v.label.clone())
                            .collect(),
                    ),
                },
            );
        }
    }
    out
}

fn random_picks(rng: &mut Rng, record: &ProductRecord) -> Picks {
    let mut picks = Picks::new();
    for axis in &record.axes {
        if rng.chance(80) {
            let l = if rng.chance(5) {
                "nope".to_owned()
            } else {
                axis.values[rng.below(axis.values.len())].label.clone()
            };
            picks.insert(
                axis.group.clone(),
                if rng.chance(3) {
                    Pick::Many(vec![l])
                } else {
                    Pick::One(l)
                },
            );
        }
    }
    for addon in &record.addons {
        if rng.chance(60) {
            let labels: Vec<String> = addon
                .values
                .iter()
                .filter(|_| rng.chance(50))
                .map(|v| v.label.clone())
                .collect();
            let pick = if addon.multi || rng.chance(3) {
                Pick::Many(labels)
            } else {
                Pick::One(labels.first().cloned().unwrap_or_default())
            };
            picks.insert(addon.group.clone(), pick);
        }
    }
    if rng.chance(3) {
        picks.insert("Unknown".to_owned(), Pick::One("x".to_owned()));
    }
    picks
}

fn content(record: ProductRecord) -> ProductContent {
    ProductContent {
        product: record,
        recipe: None,
        kits: Vec::new(),
        library: None,
        read: Vec::new(),
    }
}

#[test]
fn resolve_is_orderable_exactly_for_sold_combinations() {
    let mut rng = Rng(0x5eed_0001);
    let mut orderable = 0usize;
    let mut refused = 0usize;
    for n in 0..300 {
        let doc = random_product(&mut rng, n);
        let sold = sold(&doc);
        let record = ProductRecord::from_value(doc).expect("record");
        for _ in 0..200 {
            let picks = random_picks(&mut rng, &record);
            let r = resolve(&record, &picks);
            if r.orderable {
                orderable += 1;
                let key = r.variant_key.clone().expect("variant");
                assert!(r.reasons.is_empty());
                assert!(
                    sold.contains(&(key.clone(), normalized(&record, &key, &picks))),
                    "product {n}: {picks:?} resolved to unsold {key}"
                );
            } else {
                refused += 1;
                assert!(!r.reasons.is_empty() && r.variant_key.is_none() && r.cart_lines.is_empty());
            }
        }
        // Completeness: every sold combination resolves to its variant.
        for (key, picks) in &sold {
            let r = resolve(&record, picks);
            assert!(
                r.orderable,
                "product {n}: sold {key} {picks:?} refused: {:?}",
                r.reasons
            );
            assert_eq!(r.variant_key.as_deref(), Some(key.as_str()));
        }
    }
    assert!(
        orderable > 500 && refused > 500,
        "both outcomes exercised: {orderable} / {refused}"
    );
}

#[test]
fn a_session_never_holds_an_unsold_build() {
    let mut rng = Rng(0x5eed_0002);
    let tenant = Tenant::new("house:house-a", vec![]);
    let mut sessions = 0usize;
    let mut accepted = 0usize;
    let mut refusals = 0usize;
    let mut not_sold_with = 0usize;
    for n in 0..300 {
        let doc = random_product(&mut rng, n);
        let sold = sold(&doc);
        let record = ProductRecord::from_value(doc).expect("record");
        let opened = Configurator::open(content(record.clone()), tenant.clone());
        let mut c = match opened {
            Ok(c) => c,
            Err(e) => {
                assert_eq!(e.reason, "configurator_not_orderable");
                assert!(
                    sold.is_empty(),
                    "product {n} sells {} combinations but did not open",
                    sold.len()
                );
                continue;
            }
        };
        assert!(!sold.is_empty());
        sessions += 1;
        for step in 0..60 {
            let groups: Vec<(String, Vec<String>, bool)> = record
                .axes
                .iter()
                .map(|a| {
                    (
                        a.group.clone(),
                        a.values.iter().map(|v| v.label.clone()).collect(),
                        false,
                    )
                })
                .chain(record.addons.iter().map(|a| {
                    (
                        a.group.clone(),
                        a.values.iter().map(|v| v.label.clone()).collect(),
                        a.multi,
                    )
                }))
                .collect();
            let (group, labels, multi) = &groups[rng.below(groups.len())];
            let value = if rng.chance(8) {
                None
            } else if *multi {
                Some(Pick::Many(
                    labels.iter().filter(|_| rng.chance(50)).cloned().collect(),
                ))
            } else {
                Some(Pick::One(labels[rng.below(labels.len())].clone()))
            };
            let mode = if rng.chance(50) {
                SetMode::Strict
            } else {
                SetMode::Adjust
            };
            let before = c.picks().clone();
            match c.set_pick(group, value, mode) {
                Ok(_) => {
                    accepted += 1;
                    let r = c.resolve();
                    assert!(
                        r.orderable,
                        "product {n} step {step}: accepted pick left {:?} unorderable: {:?}",
                        c.picks(),
                        r.reasons
                    );
                    let key = r.variant_key.clone().expect("variant");
                    assert!(
                        sold.contains(&(key.clone(), normalized(&record, &key, c.picks()))),
                        "product {n} step {step}: unsold {key}"
                    );
                }
                Err(_) => {
                    refusals += 1;
                    assert_eq!(c.picks(), &before, "a refusal changes nothing");
                }
            }
        }
        // Selectable means strictly accepted, and nothing else.
        let options = c.options();
        for g in &options.groups {
            for v in &g.values {
                let mut t = c.clone();
                let pick = if g.multi {
                    let mut list = match c.picks().get(&g.group) {
                        Some(Pick::Many(l)) => l.clone(),
                        _ => Vec::new(),
                    };
                    if !list.contains(&v.label) {
                        list.push(v.label.clone());
                    }
                    Pick::Many(list)
                } else {
                    Pick::One(v.label.clone())
                };
                assert_eq!(
                    t.set_pick(&g.group, Some(pick.clone()), SetMode::Strict).is_ok(),
                    v.selectable,
                    "product {n} {}={}",
                    g.group,
                    v.label
                );
                // An adjust-mode pick of a `not_sold_with` value drops exactly its `conflicts`
                // and lands on a sold build; any other unselectable value is refused.
                let mut a = c.clone();
                let adjusted = a.set_pick(&g.group, Some(pick), SetMode::Adjust);
                match v.reason {
                    None => assert!(adjusted.is_ok()),
                    Some("not_sold_with") => {
                        let out = adjusted.expect("adjust accepts a not_sold_with value");
                        let dropped: Vec<&String> = out.dropped.keys().collect();
                        let conflicts: Vec<&String> = v.conflicts.iter().collect();
                        assert_eq!(dropped, conflicts, "product {n} {}={}", g.group, v.label);
                        assert!(a.resolve().orderable);
                        not_sold_with += 1;
                    }
                    Some(_) => assert!(adjusted.is_err(), "product {n} {}={}", g.group, v.label),
                }
            }
        }
    }
    assert!(
        not_sold_with > 50,
        "{not_sold_with} not_sold_with values exercised"
    );
    assert!(
        sessions > 100 && accepted > 1000 && refusals > 100,
        "{sessions} sessions, {accepted} accepted, {refusals} refused"
    );
}
