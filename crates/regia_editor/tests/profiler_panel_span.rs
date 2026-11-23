// SPDX-License-Identifier: MPL-2.0
//! Spec 83 § 8 / ADR-9740: the Profiler panel reads its source once per repaint, inside the one
//! `profiler_panel` span, even when egui runs the window twice in a frame; the next repaint reads
//! again.
//!
//! One test in its own binary with the frame profiler as the global subscriber: a scoped
//! dispatcher in the library's parallel tests races `tracing`'s per-callsite interest cache with
//! other tests that reach `read_source` with no subscriber, and the span can go unrecorded.

use regia_core::profiling::FrameProfiler;
use regia_ecs::prelude::*;
use regia_editor::profiler_panel::{ProfilerPanel, window};
use regia_editor::profiler_source::ProfilerView;

#[test]
fn the_panel_reads_its_source_once_per_repaint_inside_one_span() {
    use tracing_subscriber::layer::SubscriberExt;
    let profiler = FrameProfiler::new();
    profiler.set_flame_enabled(true);
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(profiler.clone()))
        .expect("the only subscriber of this binary");
    let mut world = World::new();
    world.insert_resource(regia::Profiler(profiler.clone()));
    world.insert_resource(ProfilerPanel {
        open: true,
        status: String::new(),
        ..ProfilerPanel::default()
    });
    let ctx = egui::Context::default();
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        window(ui.ctx(), &mut world);
        window(ui.ctx(), &mut world);
    });
    output.textures_delta.clear();
    profiler.end_frame();
    assert_eq!(world.resource::<ProfilerView>().layout_builds, 1);
    let spans = profiler.frame_spans(0);
    let calls = |name: &str| {
        spans
            .iter()
            .find(|(n, _, _)| *n == name)
            .map(|(_, _, calls)| *calls)
    };
    assert_eq!(calls("profiler_read_source"), Some(1), "{spans:?}");
    assert_eq!(
        calls("profiler_panel"),
        Some(2),
        "one span per window call: {spans:?}"
    );
    let graph = profiler.flame_graph(1);
    let panel = graph
        .roots
        .iter()
        .find(|n| n.name == "profiler_panel")
        .expect("the panel span is a root");
    assert!(
        panel.children.iter().any(|c| c.name == "profiler_read_source"),
        "the read happens inside the panel span"
    );
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| window(ui.ctx(), &mut world));
    output.textures_delta.clear();
    assert_eq!(
        world.resource::<ProfilerView>().layout_builds,
        2,
        "the next repaint reads again"
    );
}
