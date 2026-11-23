// SPDX-License-Identifier: MPL-2.0
//! Headless checks that editor viewports request frames only while their content needs them.

use regia_ecs::prelude::World;
use regia_editor::viewport::{ViewportPanelState, ViewportSettings, ViewportStats, viewport_panel};
use regia_editor::views::{ShowFlags, ViewportLayout};

const SIZE: egui::Vec2 = egui::vec2(800.0, 600.0);

struct ViewportHarness {
    ctx: egui::Context,
    world: World,
    state: ViewportPanelState,
}

impl ViewportHarness {
    fn new(realtime: bool, stats: bool) -> Self {
        let mut world = World::new();
        world.insert_resource(ViewportSettings {
            realtime,
            ..Default::default()
        });
        world.insert_resource(ViewportLayout {
            show: ShowFlags {
                stats,
                ..Default::default()
            },
            ..Default::default()
        });
        Self {
            ctx: egui::Context::default(),
            world,
            state: ViewportPanelState::default(),
        }
    }

    fn frame(&mut self, time: f64) -> bool {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SIZE)),
            time: Some(time),
            ..Default::default()
        };
        let mut output = self.ctx.run_ui(input, |ui| {
            viewport_panel(ui, &mut self.world, None, &mut self.state);
        });
        output.textures_delta.clear();
        self.ctx.has_requested_repaint()
    }

    fn settle(&mut self) {
        for frame in 0..5 {
            self.frame(f64::from(frame) / 60.0);
        }
    }
}

#[test]
fn idle_viewport_settles_when_realtime_and_stats_are_off() {
    let mut viewport = ViewportHarness::new(false, false);
    // Let egui initialize fonts and the viewport lay out before checking settled frames.
    viewport.settle();
    for frame in 5..9 {
        assert!(
            !viewport.frame(f64::from(frame) / 60.0),
            "idle frame {frame} requested another repaint"
        );
    }
}

#[test]
fn realtime_viewport_requests_repaints() {
    let mut viewport = ViewportHarness::new(true, false);
    viewport.frame(0.0);
    assert!(viewport.frame(1.0 / 60.0));
    assert!(viewport.frame(2.0 / 60.0));
}

#[test]
fn stable_stats_overlay_does_not_request_repaints() {
    let mut viewport = ViewportHarness::new(false, true);
    viewport.world.insert_resource(ViewportStats {
        frames: 60,
        frame_times: std::iter::repeat_n(1.0 / 60.0, 60).collect(),
        ..Default::default()
    });
    viewport.settle();
    for frame in 5..9 {
        let requested = viewport.frame(f64::from(frame) / 60.0);
        assert!(!requested, "stable stats frame {frame} requested another repaint");
    }
}

#[test]
fn changed_displayed_stats_request_a_repaint() {
    let mut viewport = ViewportHarness::new(false, true);
    viewport.world.insert_resource(ViewportStats {
        frames: 60,
        frame_times: std::iter::repeat_n(1.0 / 60.0, 60).collect(),
        ..Default::default()
    });
    viewport.settle();
    viewport.world.resource_mut::<ViewportStats>().hitches += 1;
    assert!(viewport.frame(5.0 / 60.0));
}
