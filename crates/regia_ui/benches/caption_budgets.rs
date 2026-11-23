// SPDX-License-Identifier: MPL-2.0
//! Spec 36 § 12 budgets on the caption board (ADR-1882): one board step with 16 live cue sets
//! (budget 20 us), and the layout of a 200-character cue on a measure-cache miss (budget 100 us)
//! and a hit (about 0). Headless, no GPU; the shaped runs use the embedded font.
//! Run with `cargo bench -p regia_ui --bench caption_budgets`.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use regia_app::caption::{
    CaptionId, CaptionKind, CaptionPiece, CaptionSource, CaptionStart, CaptionStyleHint, CaptionTime,
};
use regia_ui::captions::{
    BlockLayout, BlockMetrics, CaptionBoard, CaptionOptions, CaptionPolicy, FixedLayout, MeasureCache,
    ShapedLayout, paginate,
};
use regia_ui::{Localization, TextSystem};

const SETS: u64 = 16;
const SIZE_PX: f32 = 36.0;
const STEP_US: u64 = 16_667;
const PLANNED_US: u64 = 1 << 40;
const CUE: &str = "The convoy leaves at first light, so check your gear tonight and keep the radio on; \
                   if the bridge is out we take the river road, and nobody stops for anything until \
                   we reach the depot at the far end. Go.";

fn set(id: u64) -> CaptionStart {
    CaptionStart {
        id: CaptionId(id),
        source: CaptionSource::Game("bench".into()),
        kind: CaptionKind::Speech,
        locale: "en".into(),
        pieces: vec![CaptionPiece::literal(0, "One short line for the cue set.")],
        speaker: None,
        priority: 0,
        start: CaptionTime::AtMicros(0),
        planned_us: PLANNED_US,
        keep_breaks: false,
        hint: CaptionStyleHint::default(),
    }
}

fn board_step(c: &mut Criterion) {
    let opts = CaptionOptions::default();
    let mut layout = FixedLayout {
        metrics: BlockMetrics::default(),
    };
    let mut board = CaptionBoard::new(CaptionPolicy {
        max_entries: SETS as usize,
        ..CaptionPolicy::default()
    });
    for id in 1..=SETS {
        board.push(set(id), 0, &opts);
    }
    let mut now = 0;
    board.advance(now, &opts, &mut layout);
    assert_eq!(board.len(), SETS as usize, "16 live sets");
    c.bench_function("caption_board_step_16_sets", |b| {
        b.iter(|| {
            now += STEP_US;
            board.advance(black_box(now), &opts, &mut layout);
        });
    });
}

/// The board's work for a cue that appears: wrap and page it, then measure the rows.
fn lay_out(text: &str, layout: &mut dyn BlockLayout) -> f32 {
    let pages = paginate(text, 5_000_000, 1190.0, 2, false, SIZE_PX, layout);
    pages
        .iter()
        .flat_map(|page| &page.lines)
        .map(|line| layout.width(line, SIZE_PX, false))
        .fold(0.0, f32::max)
}

fn layout_cue(c: &mut Criterion) {
    let text = &CUE[..200];
    let loc = Localization::default();
    let mut shaper = TextSystem::embedded_only();
    let mut cache = MeasureCache::default();
    let mut group = c.benchmark_group("caption_layout_200_chars");
    group.bench_function("em_measure", |b| {
        let mut layout = FixedLayout {
            metrics: BlockMetrics::default(),
        };
        b.iter(|| black_box(lay_out(text, &mut layout)));
    });
    group.bench_function("shaped_miss", |b| {
        b.iter(|| {
            cache.clear();
            let mut layout = ShapedLayout {
                text: &mut shaper,
                loc: &loc,
                cache: &mut cache,
                metrics: BlockMetrics::default(),
            };
            black_box(lay_out(text, &mut layout))
        });
    });
    group.bench_function("shaped_hit", |b| {
        b.iter(|| {
            let mut layout = ShapedLayout {
                text: &mut shaper,
                loc: &loc,
                cache: &mut cache,
                metrics: BlockMetrics::default(),
            };
            black_box(lay_out(text, &mut layout))
        });
    });
    group.finish();
}

criterion_group!(benches, board_step, layout_cue);
criterion_main!(benches);
