// SPDX-License-Identifier: MPL-2.0
//! Times the configurator on a pilot-sized synthetic product (ADR-2534; the plan's option-switch
//! budget is 100 ms including the redraw). Run:
//!
//! ```text
//! cargo run --release -p regia_configurator --example configurator_bench
//! ```
//!
//! The product: 7 metals × 3 threads × 3 gauges × 4 gem sizes (about 200 live variants), a
//! required gem picker of 70 stones, a 13-colour anodize that applies to one metal, and a
//! 4-value multi finish group. Prints the mean of each call over many runs.

use std::time::Instant;

use regia_configurator::resolve::Pick;
use regia_configurator::source::ProductContent;
use regia_configurator::{BuildOptions, Configurator, ProductRecord, SetMode, Tenant};
use serde_json::{Value, json};

fn product() -> Value {
    let metals: Vec<String> = (0..7).map(|i| format!("Metal {i}")).collect();
    let threads = ["Threadless", "Threaded 14g", "Threaded 16g"];
    let gauges = ["14g", "16g", "18g"];
    let sizes = ["2mm", "2.5mm", "3mm", "4mm"];
    let vals = |g: &str, labels: &[String], base: usize| -> Vec<Value> {
        labels
            .iter()
            .enumerate()
            .map(|(i, l)| json!({"value_key": format!("{g}={l}"), "label": l, "code": format!("{}", base + i)}))
            .collect()
    };
    let s = |a: &[&str]| a.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
    let mut variants = Vec::new();
    let mut ti = Vec::new();
    let mut n = 0usize;
    for m in &metals {
        for t in threads {
            for g in gauges {
                for z in sizes {
                    n += 1;
                    if n.is_multiple_of(6) {
                        continue;
                    }
                    let key = format!("{}", 100_000 + n);
                    if m == "Metal 0" {
                        ti.push(key.clone());
                    }
                    variants.push(
                        json!({"variant_key": key, "sku": format!("SKU-{n}"), "cost": 40.0,
                        "picks": {"Material": m, "Threading": t, "Gauge": g, "Gem Size": z}}),
                    );
                }
            }
        }
    }
    let gems: Vec<String> = (0..70).map(|i| format!("Stone {i}")).collect();
    let colours: Vec<String> = (0..13).map(|i| format!("Colour {i}")).collect();
    json!({
        "schema": "jewelry.option_graph/1", "snapshot_id": "2026-11-04-000000000000",
        "product_key": "house-a/bench", "title": "Bench piece", "status": "orderable",
        "axes": [
            {"group": "Material", "kind": "metal", "values": vals("Material", &metals, 1)},
            {"group": "Threading", "kind": "thread", "values": vals("Threading", &s(&threads), 20)},
            {"group": "Gauge", "kind": "gauge", "values": vals("Gauge", &s(&gauges), 30)},
            {"group": "Gem Size", "kind": "gem_size", "values": vals("Gem Size", &s(&sizes), 40)}],
        "variants": variants,
        "addons": [
            {"group": "Gem", "kind": "gem", "required": true, "values": vals("Gem", &gems, 1000)},
            {"group": "Anodization", "kind": "anodize", "applies_to": ti, "values": vals("Anodization", &colours, 2000)},
            {"group": "Finish", "kind": "finish", "multi": true,
             "values": vals("Finish", &s(&["Sandblast", "Reverse Set", "Hammered", "Brushed"]), 3000)}],
        "cart": {"mode": "shopify_lines"}
    })
}

fn time<T>(label: &str, runs: u32, mut f: impl FnMut() -> T) {
    let start = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(f());
    }
    let per = start.elapsed().as_secs_f64() * 1000.0 / f64::from(runs);
    println!("{label:<34} {per:>9.4} ms");
}

fn main() {
    let record = match ProductRecord::from_value(product()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("bench product: {e}");
            return;
        }
    };
    println!("variants: {}", record.variants.len());
    let content = ProductContent {
        product: record,
        recipe: None,
        kits: Vec::new(),
        library: None,
        read: Vec::new(),
    };
    let tenant = Tenant::new("house:house-a", vec![]);
    time("open (sellable scan + defaults)", 50, || {
        Configurator::open(content.clone(), tenant.clone()).is_ok()
    });
    let Ok(mut c) = Configurator::open(content, tenant) else {
        eprintln!("bench product did not open");
        return;
    };
    time("options() (every value checked)", 50, || c.options().groups.len());
    time("resolve()", 2000, || c.resolve().orderable);
    let mut flip = false;
    time("set_pick metal (strict)", 500, || {
        flip = !flip;
        let m = if flip { "Metal 3" } else { "Metal 4" };
        c.set_pick("Material", Some(Pick::One(m.to_owned())), SetMode::Strict)
            .is_ok()
    });
    let mut i = 0;
    time("set_pick gem (strict)", 500, || {
        i = (i + 1) % 70;
        c.set_pick("Gem", Some(Pick::One(format!("Stone {i}"))), SetMode::Strict)
            .is_ok()
    });
    let mut j = 0;
    time("set_pick anodize (adjust)", 500, || {
        j = (j + 1) % 13;
        c.set_pick(
            "Anodization",
            Some(Pick::One(format!("Colour {j}"))),
            SetMode::Adjust,
        )
        .is_ok()
    });
    time("set_pick metal + options()", 50, || {
        flip = !flip;
        let m = if flip { "Metal 5" } else { "Metal 6" };
        let ok = c
            .set_pick("Material", Some(Pick::One(m.to_owned())), SetMode::Adjust)
            .is_ok();
        (ok, c.options().groups.len())
    });
    time("geometry_key()", 2000, || c.geometry_key());
    // No recipe in the bench product: build() refuses quickly; the look is empty.
    time("build() (refused: no recipe)", 2000, || {
        c.build(&BuildOptions::default()).is_err()
    });
}
