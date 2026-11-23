// SPDX-License-Identifier: MPL-2.0
//! View ▸ Rendering Debug (ADR-7770, parity row P-084): a floating window over the dock with the
//! active pane's view mode, the per-entity render overrides of the selection (tint, unlit, x-ray,
//! ghost), the last pane render's pass timings and the light-complexity legend.
//!
//! Every control has an agent route: the view mode is `editor.set_viewport { view_mode }`, the
//! overrides are `editor.set_render_override`, the timings are `editor.viewport_stats`, the window
//! itself is the `view.toggle_rendering_debug` command. Every control is a focusable egui widget
//! (Tab reaches it, Space or Enter operates it) with its text as its accessible name.

use regia_ecs::prelude::{Resource, World, bevy_ecs};

use crate::selection::Selection;
use crate::viewport::ViewportStats;
use crate::views::{BUFFER_VIEWS, ViewMode, ViewportLayout};

/// The window's state.
#[derive(Resource, Debug, Clone, Default)]
pub struct RenderDebugPanel {
    /// The window is shown.
    pub open: bool,
}

/// The coverage a ghost override draws the selection at.
pub const GHOST_ALPHA: f32 = 0.35;

/// Lights per cluster at the top of the light-complexity heat ramp (the forward shader's
/// `debug_view_heat(count / 16)`).
pub const LIGHT_COMPLEXITY_FULL: u32 = 16;

/// The light-complexity heat ramp at `t` in 0..=1 (linear RGB), as the forward shader's
/// `debug_view_heat` draws it: blue through green to red, never darker than a dim blue.
#[must_use]
pub fn light_complexity_color(t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    let band = |center: f32| (1.5 - (4.0 * t - center).abs()).clamp(0.0, 1.0);
    [band(3.0).max(0.05), band(2.0).max(0.05), band(1.0).max(0.2)]
}

/// The legend rows: a light count and its swatch color (sRGB-encoded for the UI).
#[must_use]
pub fn light_complexity_legend() -> Vec<(String, egui::Color32)> {
    [0u32, 2, 4, 8, 12, LIGHT_COMPLEXITY_FULL]
        .into_iter()
        .map(|count| {
            let [r, g, b] = light_complexity_color(count as f32 / LIGHT_COMPLEXITY_FULL as f32);
            let label = if count == LIGHT_COMPLEXITY_FULL {
                format!("{count}+ lights")
            } else {
                format!("{count} lights")
            };
            (label, egui::Rgba::from_rgb(r, g, b).into())
        })
        .collect()
}

/// Draws the window when it is open (the shell calls this every frame, over the dock).
pub fn window(ctx: &egui::Context, world: &mut World) {
    let Some(panel) = world.get_resource::<RenderDebugPanel>().cloned() else {
        return;
    };
    if !panel.open {
        return;
    }
    let _span = tracing::info_span!("render_debug_panel").entered();
    let mut open = true;
    egui::Window::new("Rendering Debug")
        .default_pos(crate::tool_windows::default_pos(ctx, "Rendering Debug"))
        .open(&mut open)
        .default_width(360.0)
        .show(ctx, |ui| body(ui, world));
    if !open {
        world.insert_resource(RenderDebugPanel { open: false });
    }
}

/// The window's body.
pub fn body(ui: &mut egui::Ui, world: &mut World) {
    let mut layout = world.get_resource_or_insert_with(ViewportLayout::default).clone();
    let pane = layout.active.min(3);
    let before = layout.clone();
    ui.heading(format!("View mode (pane {})", pane + 1));
    let display = layout.display_mut(pane);
    ui.horizontal_wrapped(|ui| {
        for mode in ViewMode::KEYED {
            ui.selectable_value(&mut display.view_mode, mode, mode.label());
        }
    });
    egui::CollapsingHeader::new("Buffer views")
        .id_salt("render-debug-buffers")
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (name, label) in BUFFER_VIEWS {
                    if let Some(mode) = ViewMode::from_name(name) {
                        ui.selectable_value(&mut display.view_mode, mode, label);
                    }
                }
            });
        });
    if display.view_mode == ViewMode::LightComplexity {
        ui.label("Lights per cluster:");
        for (label, color) in light_complexity_legend() {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 2.0, color);
                ui.label(label);
            });
        }
    }
    ui.separator();
    overrides(ui, world);
    ui.separator();
    ui.heading("Pass timings");
    ui.checkbox(&mut layout.show.stats, "Measure GPU passes (Stats)");
    let stats = world.get_resource::<ViewportStats>();
    match stats.and_then(|s| s.last.as_ref()) {
        Some(info) => {
            let g = &info.gpu;
            egui::Grid::new("render-debug-timings")
                .striped(true)
                .show(ui, |ui| {
                    for (name, ms) in [
                        ("Frame", g.frame_ms),
                        ("Shadows", g.shadows_ms),
                        ("Depth prepass", g.prepass_ms),
                        ("Forward", g.forward_ms),
                        ("Ambient occlusion", g.ssao_ms),
                        ("Reflections", g.ssr_ms),
                        ("Temporal upscale", g.taau_ms),
                        ("Atmosphere", g.atmosphere_ms),
                        ("Clouds", g.clouds_ms),
                        ("Fog", g.fog_ms),
                        ("Post", g.post_ms),
                    ] {
                        ui.label(name);
                        ui.label(format!("{ms:.2} ms"));
                        ui.end_row();
                    }
                    ui.label("CPU");
                    ui.label(format!("{:.2} ms", info.cpu_ms));
                    ui.end_row();
                    ui.label("Internal size");
                    ui.label(format!(
                        "{} x {} ({} %)",
                        info.render_size[0], info.render_size[1], info.screen_percentage
                    ));
                    ui.end_row();
                });
        }
        None => {
            ui.weak("No pane rendered yet.");
        }
    }
    if layout.show != before.show || layout.panes[pane].display != before.panes[pane].display {
        world.insert_resource(layout);
    }
}

/// The selection's render overrides (`editor.set_render_override`).
fn overrides(ui: &mut egui::Ui, world: &mut World) {
    ui.heading("Selection overrides");
    let selected: Vec<_> = world
        .get_resource::<Selection>()
        .map(|s| s.entities().to_vec())
        .unwrap_or_default();
    let Some(&first) = selected.first() else {
        ui.weak("Select entities to tint, ghost or x-ray them.");
        return;
    };
    let current = world
        .get::<regia_render::RenderOverride>(first)
        .copied()
        .unwrap_or_default();
    let mut value = current;
    let mut ghost = value.alpha.is_some() && !value.xray;
    let mut tinted = value.tint.is_some();
    ui.checkbox(&mut value.unlit, "Unlit");
    ui.checkbox(&mut value.xray, "X-ray (through what is in front)");
    ui.checkbox(&mut ghost, "Ghost");
    ui.horizontal(|ui| {
        ui.checkbox(&mut tinted, "Tint");
        let mut rgba = value.tint.unwrap_or([1.0, 0.5, 0.2, 1.0]);
        if tinted {
            let mut rgb = [rgba[0], rgba[1], rgba[2]];
            if ui.color_edit_button_rgb(&mut rgb).changed() {
                rgba = [rgb[0], rgb[1], rgb[2], rgba[3]];
            }
        }
        value.tint = tinted.then_some(rgba);
    });
    value.alpha = match (value.xray, ghost) {
        (true, _) => Some(value.alpha.unwrap_or(GHOST_ALPHA)),
        (false, true) => Some(GHOST_ALPHA),
        (false, false) => None,
    };
    let clear = ui.button("Clear overrides").clicked();
    if clear {
        for &entity in &selected {
            if let Ok(mut entity) = world.get_entity_mut(entity) {
                entity.remove::<regia_render::RenderOverride>();
            }
        }
    } else if value != current {
        for &entity in &selected {
            if let Ok(mut entity) = world.get_entity_mut(entity) {
                entity.insert(value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn frame(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 900.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| body(ui, world));
        });
        output.textures_delta.clear();
    }

    /// The keyboard path (the accessibility bar): Tab reaches the view-mode choices and Space
    /// picks one, with no pointer.
    #[test]
    fn the_keyboard_picks_a_view_mode() {
        let mut world = World::new();
        world.insert_resource(ViewportLayout::default());
        let ctx = egui::Context::default();
        frame(&ctx, &mut world, Vec::new());
        // Tab to the first choice (Lit), Tab to the second (Unlit), Space picks it.
        frame(&ctx, &mut world, vec![key(egui::Key::Tab)]);
        frame(&ctx, &mut world, vec![key(egui::Key::Tab)]);
        frame(&ctx, &mut world, vec![key(egui::Key::Space)]);
        frame(&ctx, &mut world, Vec::new());
        assert_eq!(
            world.resource::<ViewportLayout>().display(0).view_mode,
            ViewMode::Unlit
        );
    }

    /// Drawing the overrides changes nothing by itself, and the legend runs from cold to hot.
    #[test]
    fn overrides_reach_the_selection_and_the_legend_ramps() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        let mut selection = Selection::default();
        selection.select(a, crate::selection::SelectMode::Replace);
        selection.select(b, crate::selection::SelectMode::Add);
        world.insert_resource(selection);
        world.entity_mut(a).insert(regia_render::RenderOverride {
            unlit: true,
            ..Default::default()
        });
        let ctx = egui::Context::default();
        // Drawing alone changes nothing: the window shows the first entity's override.
        frame(&ctx, &mut world, Vec::new());
        assert_eq!(
            world.get::<regia_render::RenderOverride>(a).map(|o| o.unlit),
            Some(true)
        );
        assert!(world.get::<regia_render::RenderOverride>(b).is_none());
        let legend = light_complexity_legend();
        assert_eq!(legend.len(), 6);
        assert_ne!(legend[0].1, legend[5].1, "the ramp runs from cold to hot");
        let cold = light_complexity_color(0.0);
        let hot = light_complexity_color(1.0);
        assert!(cold[2] > cold[0] && hot[0] > hot[2], "{cold:?} {hot:?}");
    }
}
