// SPDX-License-Identifier: MPL-2.0
//! `resolve()` over a real option graph folder (option graph 1.2.0, ADR-2693), read at test time only:
//! nothing of the catalogue (costs, labels, keys) enters the repo.
//!
//! Environment (the pilot data on the build server; see `book/agents/configurator.md`):
//! - `REGIA_JW_GRAPH=<dir>`: one `optiongraph/<snapshot_id>/` folder (`houses/*/products/*.json`).
//! - `REGIA_JW_PILOT=<pilot.json>`: the plan's pilot set; every slot's chosen, primary and alternate
//!   product is resolved exhaustively.
//! - `REGIA_JW_PILOT_VECTORS=<file.jsonl>`: extra resolve vectors (the pilot vectors the option graph's
//!   owner checked against the shop), run against the folder's records; every one must match.
//! - `REGIA_JW_CONTRACT_REQUIRED=1` (or `REGIA_JW_REQUIRE_PILOT=1`): a missing input fails instead of being reported.
//!
//! What it proves, for the whole folder: every record loads through Regia's reader. For every pilot
//! product: each offered variant (with the first acceptable value of each required add-on) resolves
//! orderable exactly when the record can sell it; a metal swap and a gem swap of the same shape keep the
//! `geometry_key`; a cabochon and a faceted stone of one cut and size get different keys.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_configurator::graph::{GroupRef, OptionValue};
use regia_configurator::resolve::{Pick, Picks, geometry_key, resolve};
use regia_configurator::vectors::{parse_vectors, run_vectors};
use regia_configurator::{ProductRecord, Resolution};
use serde_json::{Value, json};

fn required() -> bool {
    // `REGIA_JW_REQUIRE_PILOT=1` is spec 42 § 5.2's spelling of the same flag.
    ["REGIA_JW_CONTRACT_REQUIRED", "REGIA_JW_REQUIRE_PILOT"]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|v| v == "1"))
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn missing(what: &str) {
    assert!(
        !required(),
        "REGIA_JW_CONTRACT_REQUIRED=1 but {what} is not set or not present"
    );
    eprintln!("real catalogue: not present: {what}");
}

/// Every product record of the folder by key, and the files that did not load.
fn load_folder(dir: &Path) -> (BTreeMap<String, ProductRecord>, Vec<String>) {
    let mut records = BTreeMap::new();
    let mut failed = Vec::new();
    let Ok(houses) = std::fs::read_dir(dir.join("houses")) else {
        failed.push(format!("{}: no houses/", dir.display()));
        return (records, failed);
    };
    let mut files = Vec::new();
    for house in houses.flatten() {
        if let Ok(products) = std::fs::read_dir(house.path().join("products")) {
            files.extend(
                products
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "json")),
            );
        }
    }
    files.sort();
    for f in files {
        match std::fs::read(&f)
            .map_err(|e| e.to_string())
            .and_then(|b| ProductRecord::from_json(&b).map_err(|e| e.to_string()))
        {
            Ok(r) => {
                records.insert(r.product_key.clone(), r);
            }
            Err(e) => failed.push(format!("{}: {e}", f.display())),
        }
    }
    (records, failed)
}

/// The pilot set's product keys: each slot's chosen, primary and alternates, without repeats.
fn pilot_keys(pilot: &Value) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut push = |k: Option<&str>| {
        if let Some(k) = k
            && !keys.iter().any(|x| x == k)
        {
            keys.push(k.to_owned());
        }
    };
    for slot in pilot["slots"].as_array().map_or(&[][..], Vec::as_slice) {
        push(slot["chosen"].as_str());
        push(slot["primary"]["product_key"].as_str());
        for alt in slot["alternates"].as_array().map_or(&[][..], Vec::as_slice) {
            push(alt["product_key"].as_str());
        }
    }
    keys
}

/// What a stone value is as geometry: its `geometry_pick` when the record gives one, else the
/// `{cabochon, count, cut, size_mm}` of its gem facts (the material and colour left out).
fn stone_shape(v: &OptionValue) -> Value {
    if let Some(g) = &v.geometry_pick {
        return g.clone();
    }
    let gem = v.gem.as_ref();
    let f = |k: &str| gem.and_then(|g| g.get(k)).cloned().unwrap_or(Value::Null);
    json!({"cabochon": f("cabochon"), "count": f("count"), "cut": f("cut"), "size_mm": f("size_mm")})
}

fn gem_group(p: &ProductRecord, at: GroupRef) -> bool {
    let role = match at {
        GroupRef::Axis(i) => p.axes[i].geometry_role.as_deref(),
        GroupRef::Addon(i) => p.addons[i].geometry_role.as_deref(),
    };
    role == Some("gem")
}

/// The picks of one variant: its axes, then the first acceptable value of every required add-on
/// that applies.
fn variant_picks(p: &ProductRecord, vi: usize) -> Picks {
    let v = &p.variants[vi];
    let mut picks: Picks = v
        .picks
        .iter()
        .map(|(g, l)| (g.clone(), Pick::One(l.clone())))
        .collect();
    for a in p
        .addons
        .iter()
        .filter(|a| a.required && a.applies(&v.variant_key))
    {
        if let Some(x) = a.values.iter().find(|x| p.addon_value_acceptable(x)) {
            picks.insert(a.group.clone(), Pick::One(x.label.clone()));
        }
    }
    picks
}

#[derive(Default, Debug)]
struct Counts {
    products: usize,
    variants: usize,
    orderable: usize,
    refused_unsellable: usize,
    metal_swaps_same_key: usize,
    gem_swaps_same_key: usize,
    shape_changes_new_key: usize,
    cab_vs_faceted_new_key: usize,
    slots_with_positions: usize,
}

fn key_of(r: &Resolution) -> Option<&str> {
    r.geometry_key.as_deref()
}

/// Resolve every offered variant of one product and check the geometry key rules on it.
fn exhaust(p: &ProductRecord, counts: &mut Counts, problems: &mut Vec<String>) {
    counts.products += 1;
    counts.slots_with_positions += p.slots.iter().filter(|s| s.positions_norm.is_some()).count();
    let mut sold: Vec<(usize, Picks, String)> = Vec::new();
    for vi in 0..p.variants.len() {
        if !p.variant_offered(&p.variants[vi]) {
            continue;
        }
        counts.variants += 1;
        let picks = variant_picks(p, vi);
        let r = resolve(p, &picks);
        let sellable = p.variant_sellable(&p.variants[vi]);
        match (r.orderable, sellable) {
            (true, true) => {
                counts.orderable += 1;
                if r.variant_key.as_deref() != Some(p.variants[vi].variant_key.as_str()) {
                    problems.push(format!(
                        "{}: {picks:?} resolved to {:?}",
                        p.product_key, r.variant_key
                    ));
                }
                if key_of(&r) != Some(geometry_key(p, &r.picks).as_str()) {
                    problems.push(format!("{}: key differs from geometry_key(picks)", p.product_key));
                }
                sold.push((vi, r.picks.clone(), r.geometry_key.clone().unwrap_or_default()));
            }
            (false, false) => counts.refused_unsellable += 1,
            (false, true) => {
                // A non-numeric add-on code, an add-on code the axes carry, ... : let the reasons say.
                let codes: Vec<&str> = r.reasons.iter().map(|x| x.code).collect();
                if !codes.iter().all(|c| *c == "no_cart_code") {
                    problems.push(format!("{}: {picks:?} refused {codes:?}", p.product_key));
                }
            }
            (true, false) => problems.push(format!("{}: {picks:?} sold but not sellable", p.product_key)),
        }
    }
    metal_swaps(p, &sold, counts, problems);
    gem_swaps(p, &sold, counts, problems);
}

/// Two sold variants that differ only in `excluded` axes (metal, gem colour, finish), with each
/// gem slot's stone mapped to the same-shaped value of the group that fills the slot for the other
/// variant, must share a key: the per-metal gem groups count as `slot:<slot>`.
fn metal_swaps(
    p: &ProductRecord,
    sold: &[(usize, Picks, String)],
    counts: &mut Counts,
    problems: &mut Vec<String>,
) {
    let excluded = |g: &str| {
        p.axes
            .iter()
            .any(|a| a.group == g && a.geometry_role.as_deref() == Some("excluded"))
    };
    for (i, (va, picks_a, key_a)) in sold.iter().enumerate() {
        for (vb, _, _) in sold.iter().skip(i + 1).take(64) {
            let a = &p.variants[*va];
            let b = &p.variants[*vb];
            // An axis whose two values have the same `geometry_pick` (a style that differs in its
            // stones' material only) swaps like a metal.
            let same_pick = |g: &str, la: &str, lb: Option<&String>| {
                let axis = p.axes.iter().find(|x| x.group == g);
                let pick_of = |l: &str| {
                    axis.and_then(|x| x.values.iter().find(|v| v.label == l))
                        .and_then(|v| v.geometry_pick.as_ref())
                };
                lb.is_some_and(|lb| pick_of(la).is_some() && pick_of(la) == pick_of(lb))
            };
            let only_excluded = a
                .picks
                .iter()
                .all(|(g, l)| b.picks.get(g) == Some(l) || excluded(g) || same_pick(g, l, b.picks.get(g)));
            if !only_excluded || a.picks == b.picks {
                continue;
            }
            // b's picks: its axes, and for every applicable add-on of b a value shaped like a's.
            let mut picks_b: Picks = b
                .picks
                .iter()
                .map(|(g, l)| (g.clone(), Pick::One(l.clone())))
                .collect();
            let mut mapped = true;
            for addon in p.addons.iter().filter(|x| x.applies(&b.variant_key)) {
                // The add-on of a filling the same slot (or the same group).
                let twin = p.addons.iter().find(|x| {
                    x.applies(&a.variant_key)
                        && picks_a.contains_key(&x.group)
                        && (x.group == addon.group || (x.slot.is_some() && x.slot == addon.slot))
                });
                let Some(twin) = twin else {
                    if addon.required {
                        mapped = false;
                    }
                    continue;
                };
                let Some(Pick::One(label)) = picks_a.get(&twin.group) else {
                    mapped = false;
                    break;
                };
                let Some(from) = twin.values.iter().find(|v| &v.label == label) else {
                    mapped = false;
                    break;
                };
                let shape = stone_shape(from);
                let same = addon.values.iter().find(|v| {
                    p.addon_value_acceptable(v)
                        && if addon.geometry_role.as_deref() == Some("gem") {
                            stone_shape(v) == shape
                        } else {
                            v.label == from.label
                        }
                });
                match same {
                    Some(v) => {
                        picks_b.insert(addon.group.clone(), Pick::One(v.label.clone()));
                    }
                    None => {
                        mapped = false;
                        break;
                    }
                }
            }
            if !mapped {
                continue;
            }
            let rb = resolve(p, &picks_b);
            if !rb.orderable {
                continue;
            }
            if key_of(&rb) == Some(key_a.as_str()) {
                counts.metal_swaps_same_key += 1;
            } else {
                problems.push(format!(
                    "{}: metal swap {} -> {} moved the geometry key",
                    p.product_key, a.variant_key, b.variant_key
                ));
            }
        }
    }
}

/// In one sold build, swapping a gem group's stone for another acceptable value keeps the key when
/// the shapes match and changes it when they do not (a cabochon and a faceted stone are two models).
fn gem_swaps(
    p: &ProductRecord,
    sold: &[(usize, Picks, String)],
    counts: &mut Counts,
    problems: &mut Vec<String>,
) {
    for (_, picks, key) in sold.iter().take(16) {
        for (group, pick) in picks {
            let Some(at) = p.group(group) else { continue };
            let GroupRef::Addon(ai) = at else { continue };
            if !gem_group(p, at) {
                continue;
            }
            let Pick::One(label) = pick else { continue };
            let addon = &p.addons[ai];
            let Some(from) = addon.values.iter().find(|v| &v.label == label) else {
                continue;
            };
            let shape = stone_shape(from);
            for v in addon
                .values
                .iter()
                .filter(|v| &v.label != label && p.addon_value_acceptable(v))
                .take(48)
            {
                let mut swapped = picks.clone();
                swapped.insert(group.clone(), Pick::One(v.label.clone()));
                let r = resolve(p, &swapped);
                if !r.orderable {
                    continue;
                }
                let other = stone_shape(v);
                let same_key = key_of(&r) == Some(key.as_str());
                if other == shape {
                    if same_key {
                        counts.gem_swaps_same_key += 1;
                    } else {
                        problems.push(format!("{}: gem swap `{group}` moved the key", p.product_key));
                    }
                } else if same_key {
                    problems.push(format!(
                        "{}: `{group}` {shape} and {other} share a key",
                        p.product_key
                    ));
                } else {
                    counts.shape_changes_new_key += 1;
                    let cab = |s: &Value| s.get("cabochon").cloned().unwrap_or(Value::Null);
                    let mut a = shape.clone();
                    let mut b = other.clone();
                    if let (Some(a), Some(b)) = (a.as_object_mut(), b.as_object_mut()) {
                        a.remove("cabochon");
                        b.remove("cabochon");
                    }
                    if a == b && cab(&shape) != cab(&other) {
                        counts.cab_vs_faceted_new_key += 1;
                    }
                }
            }
        }
    }
}

#[test]
fn the_real_graph_loads_and_the_pilot_products_resolve() {
    let Some(dir) = env_path("REGIA_JW_GRAPH").filter(|d| d.is_dir()) else {
        return missing("REGIA_JW_GRAPH (an optiongraph/<snapshot_id> folder)");
    };
    let (records, failed) = load_folder(&dir);
    eprintln!(
        "real catalogue: {} records loaded, {} refused by the reader",
        records.len(),
        failed.len()
    );
    assert!(
        failed.is_empty(),
        "records the reader refused:\n{}",
        failed.join("\n")
    );
    assert!(!records.is_empty(), "{} holds no product records", dir.display());

    // The whole folder: every offered variant of every record resolves as the record says.
    let mut all = Counts::default();
    let mut refused_by: BTreeMap<&str, usize> = BTreeMap::new();
    let mut problems = Vec::new();
    for p in records.values() {
        all.products += 1;
        for vi in 0..p.variants.len() {
            if !p.variant_offered(&p.variants[vi]) {
                continue;
            }
            all.variants += 1;
            let r = resolve(p, &variant_picks(p, vi));
            if r.orderable {
                all.orderable += 1;
            } else {
                if !p.variant_sellable(&p.variants[vi]) {
                    all.refused_unsellable += 1;
                }
                let code = r.reasons.first().map_or("none", |x| x.code);
                *refused_by.entry(code).or_insert(0usize) += 1;
            }
        }
        all.slots_with_positions += p.slots.iter().filter(|s| s.positions_norm.is_some()).count();
    }
    eprintln!(
        "real catalogue (all records): {all:?}; refused offered variants by first reason: {refused_by:?}"
    );

    let Some(pilot) = env_path("REGIA_JW_PILOT") else {
        return missing("REGIA_JW_PILOT (the plan's pilot.json)");
    };
    let pilot: Value =
        serde_json::from_slice(&std::fs::read(&pilot).expect("pilot.json")).expect("pilot.json is JSON");
    let keys = pilot_keys(&pilot);
    let mut counts = Counts::default();
    let mut absent = Vec::new();
    for key in &keys {
        match records.get(key) {
            Some(p) => exhaust(p, &mut counts, &mut problems),
            None => absent.push(key.clone()),
        }
    }
    eprintln!(
        "real catalogue (pilot): {} keys, {} absent {absent:?}: {counts:?}",
        keys.len(),
        absent.len()
    );
    let chosen: Vec<&str> = pilot["slots"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(|s| s["chosen"].as_str())
        .collect();
    for key in chosen {
        assert!(records.contains_key(key), "pilot product {key} has no record");
    }
    assert!(
        problems.is_empty(),
        "{} problems:\n{}",
        problems.len(),
        problems[..problems.len().min(30)].join("\n")
    );
}

#[test]
fn the_pilot_vectors_match_on_the_real_graph() {
    let Some(dir) = env_path("REGIA_JW_GRAPH").filter(|d| d.is_dir()) else {
        return missing("REGIA_JW_GRAPH (an optiongraph/<snapshot_id> folder)");
    };
    let Some(path) = env_path("REGIA_JW_PILOT_VECTORS").filter(|p| p.is_file()) else {
        return missing("REGIA_JW_PILOT_VECTORS (the pilot resolve vectors)");
    };
    let (records, _) = load_folder(&dir);
    let text = std::fs::read_to_string(&path).expect("vectors");
    let cases = parse_vectors(&text).expect("parse");
    let report = run_vectors(&cases, &|k| records.get(k));
    eprintln!(
        "pilot vectors: {} read, {} matched, {} mismatched fields",
        report.cases,
        report.matched,
        report.mismatches.len()
    );
    assert!(report.cases > 0, "{} holds no vectors", path.display());
    assert!(
        report.mismatches.is_empty(),
        "{:#?}",
        &report.mismatches[..report.mismatches.len().min(20)]
    );
}
