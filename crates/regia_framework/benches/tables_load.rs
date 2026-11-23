// SPDX-License-Identifier: MPL-2.0
//! Spec 51 § 10 and test 21 (ADR-2132): the table micro-benchmarks, as criterion benches so the numbers
//! come with confidence intervals. Budgets (min-spec class): a 4,096-row table loads and validates in
//! <= 20 ms on the shipped (cooked) path; a curve sample by handle <= 50 ns; a 64 x 8 chooser evaluation
//! <= 15 us plain and <= 150 us with a `Full` trace. Benches assert nothing about time: the lead reads the
//! numbers on the min-spec machine. Run with
//!
//! ```text
//! cargo bench -p regia_framework --bench tables_load
//! ```
//!
//! The text loads are measured too, to show what the cook saves (a 4,096-row chooser in RON text is an
//! authoring-time cost, ADR-2012).

use std::hint::black_box;
use std::path::PathBuf;
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use regia_app::prelude::*;
use regia_asset::data_table::{CookedDataTable, DataTable, TableFormat};
use regia_core::math::Rng;
use regia_framework::chooser::{
    ChooserContext, ChooserTable, ChooserValue, Choosers, NoChoosers, TraceDetail,
};
use regia_framework::curve_table::{CurveKey, CurveTable, NamedCurve, TableInterp};

const ROWS: usize = 4096;

/// A chooser of `n` rows over 8 columns of several kinds, every cell set (as `tests/chooser_perf.rs`).
fn chooser_text(n: usize) -> String {
    let columns = r#"
        (id: "a", input: "n", kind: IntRange), (id: "b", input: "f", kind: FloatRange),
        (id: "c", input: "on", kind: Bool), (id: "d", input: "who", kind: Name),
        (id: "e", input: "t", kind: Tags), (id: "g", input: "f", kind: FloatDistance(max_distance: 10.0)),
        (id: "w", kind: Weight), (id: "r", output: "rate", kind: SetFloat)"#;
    let rows: Vec<String> = (0..n)
        .map(|r| {
            format!(
                r#"(id: "row_{r}", cells: {{ "a": ({r}, {}), "b": (0.0, 1.5), "c": True, "d": "name_{r}",
                   "e": AllOf([Has("A.B"), NoneOf([Has("C")])]), "g": 2.5, "w": 1.5, "r": 1.25 }},
                   result: Asset("anim/hit_{r}.montage.ron"), note: "row")"#,
                r + 1
            )
        })
        .collect();
    format!(
        r#"(format_version: 1, mode: Weighted, result: Asset("montage"),
            inputs: {{ "n": Int, "f": Float, "on": Bool, "who": Name, "t": Tags }},
            outputs: {{ "rate": Float }}, columns: [{columns}], rows: [{}])"#,
        rows.join(",\n")
    )
}

/// 64 rows x 8 range columns over one `Int` input; only the last row passes for `v = 7`.
fn chooser_64x8() -> Option<ChooserTable> {
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
        r#"(format_version: 1, mode: First, result: Asset("clip"), inputs: {{ "v": Int }}, columns: [{}], rows: [{}])"#,
        columns.join(", "),
        rows.join(", ")
    ))
    .ok()
}

#[derive(Reflect, serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
enum Kind {
    #[default]
    Rifle,
    Burst(u8),
}

#[derive(Reflect, serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
struct Offset {
    x: i32,
    y: i32,
}

/// A typed data-table row of eight fields of several kinds.
#[derive(Reflect, serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
struct WeaponRow {
    damage: u32,
    spread: f32,
    kind: Kind,
    icon: String,
    tags: Vec<String>,
    muzzle: Option<Offset>,
    sound: PathBuf,
    weight: f64,
}

fn data_table_text(n: usize) -> String {
    let mut text = String::from("(\n    format_version: 1,\n    row_type: \"WeaponRow\",\n    rows: {\n");
    for i in 0..n {
        text.push_str(&format!(
            "        \"w{i:05}\": (damage: {d}, spread: {s}.5, kind: Burst({b}), icon: \"ui/w{i}.png\", tags: [\"t{t}\"], \
             muzzle: Some((x: {i}, y: -1)), sound: \"sfx/{i}.wav\", weight: 1.25),\n",
            d = i % 97,
            s = i % 7,
            b = i % 5,
            t = i % 3,
        ));
    }
    text.push_str("    },\n)\n");
    text
}

fn table_loads(c: &mut Criterion) {
    let mut group = c.benchmark_group("table_load_4096");
    group.sample_size(20).measurement_time(Duration::from_secs(4));

    // Choosers: the shipped path (decode, validate, the slot's checks) and the text path.
    let text = chooser_text(ROWS);
    let Ok(table) = ChooserTable::from_ron_str(&text) else {
        return;
    };
    let Ok(cooked) = table.to_cooked() else {
        return;
    };
    group.bench_function("chooser_cooked", |b| {
        b.iter(|| {
            let mut choosers = Choosers::default();
            black_box(choosers.load_cooked("t.chooser.ron", black_box(&cooked)).is_ok());
        });
    });
    group.bench_function("chooser_validate", |b| {
        b.iter(|| black_box(table.validate(&NoChoosers).len()))
    });
    group.sample_size(10);
    group.bench_function("chooser_text", |b| {
        b.iter(|| black_box(ChooserTable::from_ron_str(black_box(&text)).is_ok()));
    });
    group.sample_size(20);

    // Curve tables: 4,096 curves of 32 keys.
    let curves: Vec<NamedCurve> = (0..ROWS)
        .map(|c| {
            let keys = (0..32)
                .map(|k| CurveKey::new(k as f32, (k as f32 * 0.3 + c as f32).sin()))
                .collect();
            NamedCurve::new(&format!("curve_{c}"), keys)
        })
        .collect();
    if let Ok(curves) = CurveTable::new(TableInterp::Cubic, curves)
        && let Ok(cooked) = curves.to_cooked()
    {
        group.bench_function("curves_cooked", |b| {
            b.iter(|| black_box(CurveTable::from_cooked(black_box(&cooked)).is_ok()));
        });
    }

    // Typed data tables (content-data § 3): text and cooked, each with the typed rows and validation.
    let text = data_table_text(ROWS);
    group.bench_function("data_table_text", |b| {
        b.iter(|| {
            let table = DataTable::<WeaponRow>::from_ron_str(black_box(&text));
            black_box(table.map(|t| t.validate().len()).is_ok())
        });
    });
    if let Ok(cooked) = CookedDataTable::from_text(&text, TableFormat::Ron).map(|t| t.to_bytes()) {
        let cooked = cooked.unwrap_or_default();
        group.bench_function("data_table_cooked", |b| {
            b.iter(|| {
                let table = DataTable::<WeaponRow>::from_cooked(black_box(&cooked));
                black_box(table.map(|t| t.validate().len()).is_ok())
            });
        });
    }
    group.finish();
}

fn hot_paths(c: &mut Criterion) {
    let curves = CurveTable::new(
        TableInterp::Cubic,
        vec![NamedCurve::new(
            "big",
            (0..ROWS)
                .map(|k| CurveKey::new(k as f32, (k as f32).sqrt()))
                .collect(),
        )],
    );
    if let Ok(curves) = curves
        && let Some(handle) = curves.handle("big")
    {
        let mut rng = Rng::new(3);
        c.bench_function("curve_sample_by_handle_4096_keys", |b| {
            b.iter_batched(
                || rng.f32() * ROWS as f32,
                |x| black_box(curves.sample(handle, x)),
                BatchSize::SmallInput,
            );
        });
    }
    let Some(table) = chooser_64x8() else {
        return;
    };
    let mut ctx = ChooserContext::default();
    ctx.set("v", ChooserValue::Int(7));
    c.bench_function("chooser_eval_64x8_plain", |b| {
        b.iter(|| {
            let mut rng = Rng::new(0);
            black_box(table.evaluate(&NoChoosers, &mut ctx, &mut rng, None).is_some())
        });
    });
    c.bench_function("chooser_eval_64x8_full_trace", |b| {
        b.iter(|| {
            let mut rng = Rng::new(0);
            let (picked, trace) =
                table.evaluate_traced(&NoChoosers, &mut ctx, &mut rng, None, TraceDetail::Full);
            black_box((picked.is_some(), trace.rows.len()))
        });
    });
}

criterion_group!(benches, table_loads, hot_paths);
criterion_main!(benches);
