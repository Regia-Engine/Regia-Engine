// SPDX-License-Identifier: MPL-2.0
//! Spec 51 § 8 test 15 (allocation half): a plain `ChooserTable::evaluate` builds no trace. Its
//! allocation count does not grow with the number of rows scanned (64 rows cost what 4 rows
//! cost), while a traced evaluation allocates per row.
//!
//! Its own test binary: it installs a counting global allocator.

use regia_core::math::Rng;
use regia_framework::chooser::{ChooserContext, ChooserTable, ChooserValue, NoChoosers, TraceDetail};

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests and live bytes.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

fn allocations() -> usize {
    alloc_counter::thread_allocs() as usize
}

/// `rows` rows over 8 gate columns; only the last row passes for `v = 7`.
fn table(rows: usize) -> ChooserTable {
    let columns: Vec<String> = (0..8)
        .map(|i| format!(r#"(id: "c{i}", input: "v", kind: IntRange)"#))
        .collect();
    let mut body: Vec<String> = (0..rows - 1)
        .map(|r| format!(r#"(id: "r{r}", cells: {{ "c0": (100, 200) }}, result: Asset("a/r{r}.clip.ron"))"#))
        .collect();
    body.push(r#"(id: "last", cells: { "c7": (0, 9) }, result: Asset("a/last.clip.ron"))"#.to_owned());
    ChooserTable::from_ron_str(&format!(
        r#"(format_version: 1, result: Asset("clip"), inputs: {{ "v": Int }}, columns: [{}], rows: [{}])"#,
        columns.join(", "),
        body.join(", ")
    ))
    .expect("table")
}

fn count(f: impl FnOnce()) -> usize {
    let before = allocations();
    f();
    allocations() - before
}

#[test]
fn a_plain_evaluation_allocates_no_trace() {
    let small = table(4);
    let large = table(64);
    let mut ctx = ChooserContext::default();
    ctx.set("v", ChooserValue::Int(7));
    let mut rng = Rng::new(0);
    // Warm up (lazy statics in tracing and the like).
    for t in [&small, &large] {
        let _ = t.evaluate(&NoChoosers, &mut ctx, &mut rng, None);
    }
    let plain_small = count(|| {
        let picked = small.evaluate(&NoChoosers, &mut ctx, &mut rng, None);
        assert!(picked.is_some());
    });
    let plain_large = count(|| {
        let picked = large.evaluate(&NoChoosers, &mut ctx, &mut rng, None);
        assert_eq!(picked.and_then(|p| p.row).as_deref(), Some("last"));
    });
    let traced_large = count(|| {
        let (_, trace) = large.evaluate_traced(&NoChoosers, &mut ctx, &mut rng, None, TraceDetail::Full);
        assert_eq!(trace.rows.len(), 64);
    });
    assert_eq!(plain_large, plain_small, "no per-row allocation without a trace");
    assert!(
        traced_large > plain_large + 64,
        "a trace allocates per row ({traced_large} vs {plain_large})"
    );
}

/// Loads a 4,096-row table with `columns` filled cells per row; the heap it holds and its cooked size.
fn heap_of_4096_rows(columns: usize) -> (isize, usize) {
    let names: Vec<String> = (0..columns)
        .map(|i| format!(r#"(id: "c{i}", input: "v", kind: IntRange)"#))
        .collect();
    let rows: Vec<String> = (0..4096)
        .map(|r| {
            let cells: Vec<String> = (0..columns)
                .map(|i| format!(r#""c{i}": ({r}, {})"#, r + 1))
                .collect();
            format!(
                r#"(id: "r{r}", cells: {{ {} }}, result: Asset("a/r{r}.clip.ron"))"#,
                cells.join(", ")
            )
        })
        .collect();
    let text = format!(
        r#"(format_version: 1, result: Asset("clip"), inputs: {{ "v": Int }}, columns: [{}], rows: [{}])"#,
        names.join(", "),
        rows.join(", ")
    );
    let cooked = ChooserTable::from_ron_str(&text)
        .expect("table")
        .to_cooked()
        .expect("cook");
    let before = alloc_counter::thread_live_bytes() as isize;
    let table = ChooserTable::from_cooked(&cooked).expect("load");
    let held = alloc_counter::thread_live_bytes() as isize - before;
    assert_eq!(table.rows.len(), 4096);
    (held, cooked.len())
}

/// Spec 51 § 10's memory line, by the byte, on a table of one cell per row. A filled cell costs about
/// 66 bytes more, so the budget holds up to about one cell per row (ADR-2012).
#[test]
fn a_loaded_4096_row_table_of_one_cell_rows_fits_the_memory_budget() {
    let (held, _) = heap_of_4096_rows(1);
    assert!(held <= 1024 * 1024, "{held} bytes");
}

/// The same with every cell filled, printed for the lead: above the budget, see ADR-2012.
#[test]
#[ignore = "hand-run measurement"]
fn the_heap_of_a_loaded_4096_row_table() {
    for columns in [1, 8] {
        let (held, cooked) = heap_of_4096_rows(columns);
        println!(
            "perf-micro chooser_heap_4096_rows {columns} column(s): {held} bytes loaded, {cooked} bytes cooked"
        );
    }
}
