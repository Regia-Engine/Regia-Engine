// SPDX-License-Identifier: MIT-0
//! `hud_draw2d` (P-06): a generic game HUD drawn with `regia_quick::draw2d` and its dirty
//! regions. A health bar bottom-left, an ammo count bottom-right, a minimap frame top-right, a
//! crosshair in the centre and a kill feed top-left, all through [`HudLayer`].
//!
//! The HUD state changes on a fixed frame script ([`HudState::step`]), so a run is the same on
//! every machine: most frames change nothing (the layer redraws no tile and uploads nothing), and
//! a frame that changes the ammo count redraws only that count's tiles.
//!
//! - `cargo run -p hud_draw2d` opens a window (`--exit-after-frames N` to stop on its own).
//! - `cargo run --release -p hud_draw2d --example hud_draw2d_bench` measures the paint's CPU time
//!   and the layer's GPU time headless (the `hud_paint_cpu_ms_below_2` row of P-06).
//! - Agents read the dirty tiles with `quick.draw2d_stats` (`book/agents/quick-draw2d.md`).

use std::collections::VecDeque;
use std::fmt::Write as _;

use regia_app::prelude::*;
use regia_core::math::{Color, Vec2};
use regia_platform::{PrimaryWindow, Window};
use regia_quick::draw2d::{Draw2d, Draw2dList, Draw2dPlugin, HudAnchor, HudLayer, PxRect, TextAlign};
use regia_ui::Localization;
use regia_ui::text::{ShapedText, Text, TextSpan, TextStyle, TextSystem};

/// The stated bound on the sample HUD's instances in any frame (`hud_sample_instance_count_is_bounded`):
/// crosshair 5, minimap 14, health 10, ammo 10, four kill-feed lines of at most 26 glyphs plus a
/// scrim each (108). A HUD that grows past it is a regression worth a look, not a hard limit.
pub const HUD_INSTANCE_BOUND: u32 = 160;

/// Kill-feed lines kept on screen.
pub const FEED_LINES: usize = 4;

/// Names the kill feed picks from (generic placeholders).
const NAMES: [&str; 8] = ["Ash", "Bram", "Cleo", "Dane", "Edda", "Finn", "Gale", "Hollis"];

/// What the HUD shows. Changes only in [`Self::step`].
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct HudState {
    /// Frames stepped.
    pub frame: u64,
    /// Health, 0-100.
    pub health: u32,
    /// Rounds in the magazine.
    pub ammo: u32,
    /// Magazine size.
    pub magazine: u32,
    /// Rounds in reserve.
    pub reserve: u32,
    /// Kill-feed lines, newest last.
    pub feed: VecDeque<String>,
    /// Minimap heading in radians (0 = up).
    pub heading: f32,
}

impl Default for HudState {
    fn default() -> Self {
        Self {
            frame: 0,
            health: 100,
            ammo: 30,
            magazine: 30,
            reserve: 90,
            feed: VecDeque::with_capacity(FEED_LINES + 1),
            heading: 0.0,
        }
    }
}

/// Frames between shots.
pub const SHOT_EVERY: u64 = 20;
/// Frames between hits taken.
pub const HIT_EVERY: u64 = 90;
/// Frames between kill-feed lines.
pub const FEED_EVERY: u64 = 150;
/// Frames between minimap heading steps.
pub const TURN_EVERY: u64 = 45;

impl HudState {
    /// Advances the fixed script one frame: a shot every [`SHOT_EVERY`] frames (a reload from the
    /// reserve when the magazine is empty), a hit every [`HIT_EVERY`] (health resets below 10), a
    /// kill-feed line every [`FEED_EVERY`] and a heading step every [`TURN_EVERY`].
    pub fn step(&mut self) {
        self.frame += 1;
        let f = self.frame;
        if f.is_multiple_of(SHOT_EVERY) {
            if self.ammo == 0 {
                let take = self.magazine.min(self.reserve);
                self.reserve -= take;
                self.ammo = take;
                if take == 0 {
                    self.reserve = 90;
                }
            } else {
                self.ammo -= 1;
            }
        }
        if f.is_multiple_of(HIT_EVERY) {
            self.health = if self.health < 17 { 100 } else { self.health - 7 };
        }
        if f.is_multiple_of(FEED_EVERY) {
            let n = (f / FEED_EVERY) as usize;
            // The oldest line's buffer is reused once the feed is full.
            let mut line = if self.feed.len() >= FEED_LINES {
                self.feed.pop_front().unwrap_or_default()
            } else {
                String::with_capacity(32)
            };
            line.clear();
            let _ = write!(
                line,
                "{} tagged {}",
                NAMES[n % NAMES.len()],
                NAMES[(n * 3 + 1) % NAMES.len()]
            );
            self.feed.push_back(line);
        }
        if f.is_multiple_of(TURN_EVERY) {
            self.heading = (self.heading + 0.2) % std::f32::consts::TAU;
        }
    }
}

/// One shaped string, reshaped only when its text changes.
#[derive(Debug, Default)]
struct Slot {
    text: String,
    px: f32,
    run: Option<ShapedText>,
}

impl Slot {
    fn get(&mut self, fonts: &mut TextSystem, text: &str, px: f32) -> &ShapedText {
        if self.run.is_none() || self.text != text || self.px != px {
            self.text.clear();
            self.text.push_str(text);
            self.px = px;
            let t = Text {
                spans: vec![TextSpan::lit(text).with_style(TextStyle {
                    font_px: px,
                    ..Default::default()
                })],
                ..Default::default()
            };
            self.run = Some(fonts.shape(&t, 1.0, None, &Localization::default()));
        }
        self.run.get_or_insert_with(ShapedText::default)
    }
}

/// The HUD's shaped strings: reshaped only when a value changes, so a steady frame shapes
/// nothing.
#[derive(Resource, Debug, Default)]
pub struct HudText {
    hp_label: Slot,
    hp_value: Slot,
    ammo: Slot,
    reserve: Slot,
    north: Slot,
    feed: [Slot; FEED_LINES],
    scratch: String,
}

const INK: Color = Color::new(0.0, 0.0, 0.0, 0.55);
const WHITE: Color = Color::new(1.0, 1.0, 1.0, 0.95);
const GREEN: Color = Color::new(0.2, 0.75, 0.3, 1.0);
const LOW: Color = Color::new(0.85, 0.2, 0.15, 1.0);
const FRAME: Color = Color::new(0.75, 0.85, 1.0, 0.85);

/// Pushes the HUD for `state` into `list` (between the list's `begin_frame` and `end_frame`) for a
/// `width` × `height` pixel target at UI scale `scale`.
pub fn paint_hud(
    list: &mut Draw2dList,
    fonts: &mut TextSystem,
    text: &mut HudText,
    state: &HudState,
    width: u32,
    height: u32,
    scale: f32,
) {
    let _span = tracing::info_span!(target: "hud_draw2d", "paint_hud").entered();
    let mut hud = HudLayer::new(list, width, height).with_scale(scale);
    let s = hud.scale();
    let screen = hud.screen();

    // Crosshair.
    hud.crosshair(screen * 0.5, 5.0 * s, 9.0 * s, 2.0 * s, 1.5 * s, WHITE);

    // Minimap frame, top-right: panel, outline, corner brackets, north tick and the player arrow.
    let map = hud.place(HudAnchor::TopRight, Vec2::splat(16.0), Vec2::splat(180.0));
    let centre = Vec2::new(map.x + map.w * 0.5, map.y + map.h * 0.5);
    hud.panel(map, Color::new(0.02, 0.04, 0.06, 0.6), 10.0 * s);
    hud.outline(map, 2.0 * s, FRAME, 10.0 * s);
    hud.bracket(centre, map.w * 0.5 - 6.0 * s, 14.0 * s, 2.0 * s, FRAME);
    let north = Vec2::new(state.heading.sin(), -state.heading.cos());
    hud.line(
        centre + north * (map.w * 0.5 - 22.0 * s),
        centre + north * (map.w * 0.5 - 12.0 * s),
        2.0 * s,
        WHITE,
    );
    hud.pointer(centre - Vec2::Y * 6.0 * s, -Vec2::Y, 14.0 * s, 2.5 * s, GREEN);
    let n_run = text.north.get(fonts, "N", 12.0 * s);
    let n_at = centre + north * (map.w * 0.5 - 34.0 * s) - Vec2::new(0.0, n_run.size[1] * 0.5);
    hud.text(fonts, n_run, n_at, TextAlign::Center, WHITE);

    // Health, bottom-left: scrim, label, bar, value.
    let panel = hud.place(HudAnchor::BottomLeft, Vec2::splat(20.0), Vec2::new(300.0, 40.0));
    hud.scrim(panel, INK, 12.0 * s);
    let label = text.hp_label.get(fonts, "HP", 16.0 * s);
    let mid = panel.y + panel.h * 0.5;
    hud.text(
        fonts,
        label,
        Vec2::new(panel.x + 12.0 * s, mid - label.size[1] * 0.5),
        TextAlign::Left,
        WHITE,
    );
    let frac = state.health as f32 / 100.0;
    hud.bar(
        PxRect::new(panel.x + 48.0 * s, mid - 7.0 * s, 190.0 * s, 14.0 * s),
        frac,
        if state.health < 30 { LOW } else { GREEN },
        Color::new(0.0, 0.0, 0.0, 0.6),
    );
    text.scratch.clear();
    let _ = write!(text.scratch, "{}", state.health);
    let value = text.hp_value.get(fonts, &text.scratch, 16.0 * s);
    hud.text(
        fonts,
        value,
        Vec2::new(panel.x + panel.w - 12.0 * s, mid - value.size[1] * 0.5),
        TextAlign::Right,
        WHITE,
    );

    // Ammo, bottom-right: scrim, count, reserve.
    // At 1080p the count's glyphs sit inside the bottom tile row (y 1024..1080), so a shot
    // redraws one run of tiles.
    let ammo = hud.place(
        HudAnchor::BottomRight,
        Vec2::new(20.0, 10.0),
        Vec2::new(170.0, 48.0),
    );
    hud.scrim(ammo, INK, 12.0 * s);
    text.scratch.clear();
    let _ = write!(text.scratch, "{}", state.ammo);
    let count = text.ammo.get(fonts, &text.scratch, 32.0 * s);
    let right = ammo.x + ammo.w - 70.0 * s;
    hud.text(
        fonts,
        count,
        Vec2::new(right, ammo.y + (ammo.h - count.size[1]) * 0.5),
        TextAlign::Right,
        if state.ammo == 0 { LOW } else { WHITE },
    );
    text.scratch.clear();
    let _ = write!(text.scratch, "/ {}", state.reserve);
    let reserve = text.reserve.get(fonts, &text.scratch, 16.0 * s);
    hud.text(
        fonts,
        reserve,
        Vec2::new(right + 8.0 * s, ammo.y + ammo.h * 0.5 - 2.0 * s),
        TextAlign::Left,
        Color::new(1.0, 1.0, 1.0, 0.7),
    );

    // Kill feed, top-left: one scrim and one line per entry, newest at the bottom.
    for (i, (line, slot)) in state.feed.iter().zip(&mut text.feed).enumerate() {
        let row = hud.place(
            HudAnchor::TopLeft,
            Vec2::new(16.0, 16.0 + i as f32 * 28.0),
            Vec2::new(240.0, 24.0),
        );
        hud.scrim(row, INK, 8.0 * s);
        let run = slot.get(fonts, line, 16.0 * s);
        hud.text(
            fonts,
            run,
            Vec2::new(row.x + 10.0 * s, row.y + (row.h - run.size[1]) * 0.5),
            TextAlign::Left,
            WHITE,
        );
    }
}

/// The sample as a plugin: [`Draw2dPlugin`] (dirty regions on), the HUD state and a system that
/// steps the script and paints the HUD each frame. Add after the render plugin.
#[derive(Default)]
pub struct HudSamplePlugin;

impl Plugin for HudSamplePlugin {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<TextSystem>() {
            app.insert_resource(TextSystem::embedded_only());
        }
        app.add_plugins(Draw2dPlugin)
            .init_resource::<HudState>()
            .init_resource::<HudText>()
            .add_systems(Update, hud_system);
    }
}

/// The primary window's physical size and scale factor (1280 × 720 at 1 before a window exists).
fn screen(windows: &Query<&Window, With<PrimaryWindow>>) -> (u32, u32, f32) {
    windows
        .iter()
        .next()
        .filter(|w| w.physical_size.0 > 0 && w.physical_size.1 > 0)
        .map_or((1280, 720, 1.0), |w| {
            (w.physical_size.0, w.physical_size.1, w.scale_factor as f32)
        })
}

fn hud_system(
    mut draw: ResMut<Draw2d>,
    mut fonts: ResMut<TextSystem>,
    mut text: ResMut<HudText>,
    mut state: ResMut<HudState>,
    windows: Query<&Window, With<PrimaryWindow>>,
) {
    state.step();
    let (w, h, scale) = screen(&windows);
    paint_hud(&mut draw, &mut fonts, &mut text, &state, w, h, scale);
}
