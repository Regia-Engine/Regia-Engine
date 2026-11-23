// SPDX-License-Identifier: MIT-0
//! `hud_draw2d` acceptance (P-06), CPU only: the sample HUD's instance count stays under its
//! stated bound over a whole script cycle, and a frame that changes only the ammo count dirties
//! one run of tiles in one tile row (counted by `Draw2dTiles`, the tracker the layer uses).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hud_draw2d::{FEED_EVERY, HUD_INSTANCE_BOUND, HudSamplePlugin, HudState, HudText, SHOT_EVERY, paint_hud};
use regia_app::prelude::*;
use regia_quick::draw2d::{Draw2dList, Draw2dStats, Draw2dTiles};
use regia_ui::text::TextSystem;

const W: u32 = 1920;
const H: u32 = 1080;

fn paint(list: &mut Draw2dList, fonts: &mut TextSystem, text: &mut HudText, state: &HudState) {
    list.begin_frame();
    paint_hud(list, fonts, text, state, W, H, 1.0);
    list.end_frame();
}

#[test]
fn hud_sample_instance_count_is_bounded() {
    let mut fonts = TextSystem::embedded_only();
    let mut text = HudText::default();
    let mut list = Draw2dList::new();
    let mut state = HudState::default();
    let mut most = 0;
    // Past a full feed and a reload: every element at its largest.
    for _ in 0..(FEED_EVERY * 6) {
        state.step();
        paint(&mut list, &mut fonts, &mut text, &state);
        most = most.max(list.stats().instances);
    }
    assert_eq!(state.feed.len(), hud_draw2d::FEED_LINES);
    assert!(
        most <= HUD_INSTANCE_BOUND,
        "{most} instances > {HUD_INSTANCE_BOUND}"
    );
    assert!(most >= 60, "the HUD drew its elements: {most}");
    assert_eq!(list.stats().glyphs_refused, 0);
}

#[test]
fn hud_sample_ammo_change_redraws_one_tile_row() {
    let mut fonts = TextSystem::embedded_only();
    let mut text = HudText::default();
    let mut list = Draw2dList::new();
    let mut tiles = Draw2dTiles::default();
    let mut state = HudState::default();
    // Step to just before a shot that is not also a hit, feed or turn frame.
    let mut changed = 0;
    let mut steady = 0;
    for _ in 0..(SHOT_EVERY * 12) {
        let before = state.clone();
        state.step();
        paint(&mut list, &mut fonts, &mut text, &state);
        let runs = tiles.update(list.instances(), W, H).to_vec();
        if state.frame == 1 {
            continue;
        }
        let only_ammo = state
            == HudState {
                ammo: state.ammo,
                frame: state.frame,
                ..before.clone()
            };
        let unchanged = before
            == HudState {
                frame: before.frame,
                ..state.clone()
            };
        if unchanged {
            assert!(
                runs.is_empty(),
                "frame {}: nothing changed, {runs:?}",
                state.frame
            );
            steady += 1;
        } else if only_ammo && before.ammo != state.ammo {
            assert_eq!(runs.len(), 1, "frame {}: {runs:?}", state.frame);
            assert!(runs[0].end - runs[0].start <= 2, "{runs:?}");
            changed += 1;
        }
    }
    assert!(changed >= 5, "{changed} ammo-only frames");
    assert!(steady >= 100, "{steady} steady frames");
}

#[test]
fn the_plugin_paints_every_frame_under_the_bound() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins).add_plugins(HudSamplePlugin);
    app.finish_plugins();
    for _ in 0..200 {
        app.update();
    }
    let stats = app.world().resource::<Draw2dStats>();
    assert_eq!(stats.frame, 200);
    assert!(stats.list.instances > 0 && stats.list.instances <= HUD_INSTANCE_BOUND);
    assert_eq!(app.world().resource::<HudState>().frame, 200);
}
