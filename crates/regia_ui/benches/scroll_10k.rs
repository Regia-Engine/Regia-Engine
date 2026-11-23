// SPDX-License-Identifier: MPL-2.0
//! Scroll cost over 10,000 rows (ADR-520, UI design series d-12): one scrolled frame of a plain
//! clipping container with 10,000 row nodes (scroll-only layout walk plus extract, no flexbox pass)
//! and of a virtualised `ListView` of 10,000 items (only the visible rows exist). Headless, no GPU.
//! Run with `cargo bench -p regia_ui --bench scroll_10k`; compare against the HUD budget.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_ui::prelude::*;
use regia_ui::style::FlexDirection;
use regia_ui::{BackgroundKind, ItemCount, UiLayoutStats, UiPointer, UiScales, scroll_container};

const ROWS: usize = 10_000;
const ROW_H: f32 = 24.0;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    app.world_mut().resource_mut::<UiScales>().window_size = [800.0, 600.0];
    app
}

fn root(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Points(800.0), Dimension::Points(600.0)],
                ..Default::default()
            }),
        ))
        .id()
}

fn viewport_style() -> Style {
    Style {
        size: [Dimension::Points(400.0), Dimension::Points(600.0)],
        direction: FlexDirection::Column,
        ..Default::default()
    }
}

/// Scrolls one frame, alternating down and up by `step` px so the view stays in range. No pointer
/// position: the wheel reaches every scroll view (there is one).
fn bench_scroll(c: &mut Criterion, name: &str, app: &mut App, step: f32) {
    for _ in 0..3 {
        app.update();
    }
    let mut down = false;
    c.bench_function(name, |b| {
        b.iter(|| {
            down = !down;
            app.world_mut().resource_mut::<UiPointer>().wheel = [0.0, if down { -step } else { step }];
            app.update();
            black_box(app.world().resource::<UiLayoutStats>().scroll_runs);
        });
    });
}

fn plain_rows(c: &mut Criterion) {
    let mut app = app();
    let root = root(&mut app);
    let world = app.world_mut();
    let list = world
        .spawn((
            scroll_container(viewport_style(), ScrollView::vertical()),
            ChildOf(root),
        ))
        .id();
    let color = UiColor([0.2, 0.3, 0.4, 1.0]);
    for _ in 0..ROWS {
        world.spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(400.0), Dimension::Points(ROW_H)],
                flex_shrink: 0.0,
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(color),
            },
            ChildOf(list),
        ));
    }
    bench_scroll(c, "scroll_frame_10k_plain_rows", &mut app, 4.0);
}

fn virtual_list(c: &mut Criterion) {
    let mut app = app();
    let root = root(&mut app);
    app.world_mut().spawn((
        scroll_container(viewport_style(), ScrollView::vertical()),
        ListView::new(ROW_H, 2),
        ItemCount(ROWS),
        ChildOf(root),
    ));
    // Two rows per frame: each frame rebinds rows.
    bench_scroll(c, "scroll_frame_10k_virtual_list", &mut app, 2.0 * ROW_H);
}

criterion_group!(benches, plain_rows, virtual_list);
criterion_main!(benches);
