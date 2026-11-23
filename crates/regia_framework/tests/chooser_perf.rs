// SPDX-License-Identifier: MPL-2.0
//! Microbenchmarks for spec 51 § 10 (`docs/06-testing-and-ci.md` § 9): curve sampling by handle,
//! chooser evaluation of 64 rows x 8 columns with and without a `Full` trace, a pass over 256
//! unchanged `OnChange` bindings, and loading a 4,096-row table. Every test is `#[ignore]`: the
//! gate never runs them, and they assert only that the work happened, never a time. Budgets are
//! for the min-spec machine and are judged by the lead from the printed numbers. Run with
//!
//! ```text
//! cargo test -p regia_framework --release --test chooser_perf -- --ignored --nocapture
//! ```

use std::hint::black_box;
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_core::math::Rng;
use regia_framework::chooser::{
    ChooserAppExt, ChooserBinding, ChooserContext, ChooserPlugin, ChooserStats, ChooserTable, ChooserValue,
    Choosers, NoChoosers, TraceDetail,
};
use regia_framework::curve_table::{CurveKey, CurveTable, NamedCurve, TableInterp};

fn report(name: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "perf-micro {name}: n={} p50={:?} p99={:?} max={:?}",
        samples.len(),
        at(0.5),
        at(0.99),
        samples[samples.len() - 1]
    );
}

fn measure(name: &str, iterations: usize, mut op: impl FnMut(usize)) {
    for i in 0..iterations.min(100) {
        op(i);
    }
    let samples = (0..iterations)
        .map(|i| {
            let start = Instant::now();
            op(i);
            start.elapsed()
        })
        .collect();
    report(name, samples);
}

/// 64 rows x 8 range columns over one `Int` input; only the last row passes for `v = 7`.
fn table_64x8(mode: &str) -> ChooserTable {
    let columns: Vec<String> = (0..8)
        .map(|i| format!(r#"(id: "c{i}", input: "v", kind: IntRange)"#))
        .collect();
    let mut rows: Vec<String> = (0..63)
        .map(|r| {
            format!(
                r#"(id: "r{r}", cells: {{ "c{}": (100, 200) }}, result: Asset("a/r{r}.clip.ron"))"#,
                r % 8
            )
        })
        .collect();
    rows.push(r#"(id: "last", cells: { "c7": (0, 9) }, result: Asset("a/last.clip.ron"))"#.to_owned());
    ChooserTable::from_ron_str(&format!(
        r#"(format_version: 1, mode: {mode}, result: Asset("clip"), inputs: {{ "v": Int }}, columns: [{}], rows: [{}])"#,
        columns.join(", "),
        rows.join(", ")
    ))
    .expect("table")
}

#[test]
#[ignore = "hand-run microbenchmark"]
fn curve_sample_by_handle() {
    let keys: Vec<CurveKey> = (0..4096)
        .map(|i| CurveKey::new(i as f32, (i as f32).sqrt()))
        .collect();
    let table = CurveTable::new(TableInterp::Cubic, vec![NamedCurve::new("c", keys)]).expect("table");
    let handle = table.handle("c").expect("handle");
    let mut sum = 0.0;
    // One sample = 1,000 lookups; divide by 1,000 for the per-sample budget (<= 50 ns).
    measure("curve_sample_x1000", 2_000, |i| {
        for k in 0..1000 {
            sum += table.sample(handle, black_box(((i * 7 + k * 13) % 4096) as f32 + 0.5));
        }
    });
    assert!(sum > 0.0);
}

#[test]
#[ignore = "hand-run microbenchmark"]
fn chooser_eval_64x8_plain_and_traced() {
    let mut ctx = ChooserContext::default();
    ctx.set("v", ChooserValue::Int(7));
    let mut rng = Rng::new(0);
    for mode in ["First", "Best"] {
        let table = table_64x8(mode);
        measure(&format!("chooser_eval_{mode}"), 5_000, |_| {
            assert!(table.evaluate(&NoChoosers, &mut ctx, &mut rng, None).is_some());
        });
        measure(&format!("chooser_eval_traced_full_{mode}"), 2_000, |_| {
            let (picked, trace) =
                table.evaluate_traced(&NoChoosers, &mut ctx, &mut rng, None, TraceDetail::Full);
            assert!(picked.is_some());
            black_box(trace);
        });
    }
}

fn provider(_: &World, _: Entity, ctx: &mut ChooserContext) {
    ctx.set("v", ChooserValue::Int(7));
}

#[test]
#[ignore = "hand-run microbenchmark"]
fn binding_pass_256_unchanged() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, ChooserPlugin));
    enable_fixed_step(&mut app, 60.0);
    app.register_chooser_provider("p", provider);
    app.world_mut()
        .resource_mut::<Choosers>()
        .insert("t.chooser.ron", table_64x8("First"));
    for _ in 0..256 {
        app.world_mut()
            .spawn(ChooserBinding::new("t.chooser.ron", "p", "s"));
    }
    app.update();
    // Each sample is a whole app update; the binding pass is the `chooser_bindings` span inside it.
    measure("app_update_256_bindings_unchanged", 500, |_| app.update());
    let stats = *app.world().resource::<ChooserStats>();
    assert_eq!(
        stats.evaluations, 256,
        "unchanged bindings are skipped after the first pass"
    );
}

fn rows_text(n: usize) -> String {
    let rows: Vec<String> = (0..n)
        .map(|r| {
            format!(
                r#"(id: "r{r}", cells: {{ "c": ({r}, {}) }}, result: Asset("a/r.clip.ron"))"#,
                r + 1
            )
        })
        .collect();
    format!(
        r#"(format_version: 1, result: Asset("clip"), inputs: {{ "v": Int }}, columns: [(id: "c", input: "v", kind: IntRange)], rows: [{}])"#,
        rows.join(",\n        ")
    )
}

#[test]
#[ignore = "hand-run microbenchmark"]
fn load_and_validate_4096_rows() {
    for n in [1024, 4096] {
        let text = rows_text(n);
        measure(&format!("ron_value_parse_{n}_rows"), 10, |_| {
            black_box(ron::from_str::<ron::Value>(&text).expect("ron"));
        });
        measure(&format!("chooser_load_{n}_rows"), 10, |_| {
            let table = ChooserTable::from_ron_str(&text).expect("table");
            assert_eq!(table.rows.len(), n);
        });
        let table = ChooserTable::from_ron_str(&text).expect("table");
        measure(&format!("chooser_validate_{n}_rows"), 10, |_| {
            black_box(table.validate(&NoChoosers));
        });
        // The path a shipped game takes (ADR-2012): decode and validate, the `chooser_load` budget.
        let cooked = table.to_cooked().expect("cook");
        println!("perf-micro chooser_cooked_{n}_rows: {} bytes", cooked.len());
        measure(&format!("chooser_load_cooked_{n}_rows"), 50, |_| {
            let loaded = ChooserTable::from_cooked(&cooked).expect("load");
            assert_eq!(loaded.rows.len(), n);
        });
        // What the startup load does: decode, validate, then the slot's check against other tables.
        measure(&format!("choosers_load_cooked_{n}_rows"), 50, |_| {
            let mut choosers = Choosers::default();
            assert_eq!(choosers.load_cooked("t.chooser.ron", &cooked), Ok(1));
        });
    }
}

/// A table closer to a real one than [`rows_text`]: 8 columns of several kinds, every cell set.
#[test]
#[ignore = "hand-run microbenchmark"]
fn load_cooked_4096_rows_of_8_columns() {
    let columns = r#"
        (id: "a", input: "n", kind: IntRange), (id: "b", input: "f", kind: FloatRange),
        (id: "c", input: "on", kind: Bool), (id: "d", input: "who", kind: Name),
        (id: "e", input: "t", kind: Tags), (id: "g", input: "f", kind: FloatDistance(max_distance: 10.0)),
        (id: "w", kind: Weight), (id: "r", output: "rate", kind: SetFloat)"#;
    let rows: Vec<String> = (0..4096)
        .map(|r| {
            format!(
                r#"(id: "row_{r}", cells: {{ "a": ({r}, {}), "b": (0.0, 1.5), "c": True, "d": "name_{r}",
                   "e": AllOf([Has("A.B"), NoneOf([Has("C")])]), "g": 2.5, "w": 1.5, "r": 1.25 }},
                   result: Asset("anim/hit_{r}.montage.ron"), note: "row")"#,
                r + 1
            )
        })
        .collect();
    let text = format!(
        r#"(format_version: 1, mode: Weighted, result: Asset("montage"),
            inputs: {{ "n": Int, "f": Float, "on": Bool, "who": Name, "t": Tags }},
            outputs: {{ "rate": Float }}, columns: [{columns}], rows: [{}])"#,
        rows.join(",\n")
    );
    let table = ChooserTable::from_ron_str(&text).expect("table");
    let cooked = table.to_cooked().expect("cook");
    println!(
        "perf-micro chooser_8col_4096: text {} bytes, cooked {} bytes",
        text.len(),
        cooked.len()
    );
    measure("chooser_load_cooked_8col_4096_rows", 50, |_| {
        let loaded = ChooserTable::from_cooked(&cooked).expect("load");
        assert_eq!(loaded.rows.len(), 4096);
    });
}

/// 4,096 curves of 32 keys, the cooked path; text is hand-authored and this size is rare.
#[test]
#[ignore = "hand-run microbenchmark"]
fn load_cooked_4096_curves() {
    let curves: Vec<NamedCurve> = (0..4096)
        .map(|c| {
            let keys = (0..32)
                .map(|k| CurveKey::new(k as f32, (k as f32 * 0.3 + c as f32).sin()))
                .collect();
            NamedCurve::new(&format!("curve_{c}"), keys)
        })
        .collect();
    let table = CurveTable::new(TableInterp::Cubic, curves).expect("table");
    let cooked = table.to_cooked().expect("cook");
    println!("perf-micro curves_4096: cooked {} bytes", cooked.len());
    measure("curve_table_load_cooked_4096_curves", 50, |_| {
        assert_eq!(CurveTable::from_cooked(&cooked).expect("load").len(), 4096);
    });
    let text = table.to_ron_string();
    measure("curve_table_load_text_4096_curves", 5, |_| {
        assert_eq!(CurveTable::from_ron_str(&text).expect("load").len(), 4096);
    });
}
