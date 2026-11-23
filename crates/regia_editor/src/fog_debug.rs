// SPDX-License-Identifier: MPL-2.0
//! The viewport's fog of war debug view (Show ▸ Fog of war, `editor.set_viewport { fog }`; audit
//! gap FW-1).
//!
//! With the flag on, the play world's `regia_framework::fog_texture::FogDebugView` is switched on
//! ([`sync_flag`](crate::fog_debug::sync_flag)), so the game's fog presentation copies its watchers
//! and texels there every frame. The viewport then draws each watcher's sight circle and the
//! grid's outline ([`fog_segments`](crate::fog_debug::fog_segments)), and the Show menu's Fog of war
//! rows show the texture and a viewer team picker that writes `fog.set_viewer`'s request
//! ([`fog_menu`](crate::fog_debug::fog_menu)).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::fog_texture::{FogDebugView, FogTextureStats};
use regia_framework::fog_view::{FogViewControl, FogViewStatus};

/// Most segments the fog overlay draws (watcher circles past it are skipped).
pub const MAX_FOG_SEGMENTS: usize = 8192;
/// Segments per watcher circle.
const CIRCLE_SEGMENTS: usize = 32;
/// Watcher circle colour.
const WATCHER: egui::Color32 = egui::Color32::from_rgb(120, 220, 255);
/// Grid outline colour.
const GRID: egui::Color32 = egui::Color32::from_rgb(255, 200, 80);

/// Switches the world's [`FogDebugView`] on or off to match `on` (no change when it already does).
pub fn sync_flag(world: &mut World, on: bool) {
    if let Some(mut view) = world.get_resource_mut::<FogDebugView>()
        && view.enabled != on
    {
        view.enabled = on;
    }
}

/// The fog overlay's world lines: each watcher's sight circle and the grid's outline, lifted
/// 0.1 m. Empty without a filled [`FogDebugView`].
#[must_use]
pub fn fog_segments(world: &World) -> Vec<(Vec3, Vec3, egui::Color32)> {
    let _span = tracing::trace_span!(target: "regia_editor", "fog_overlay").entered();
    let Some(view) = world.get_resource::<FogDebugView>().filter(|v| v.enabled) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if view.cols > 0 && view.rows > 0 {
        let (x0, z0) = (view.origin[0], view.origin[1]);
        let (x1, z1) = (
            x0 + view.cols as f32 * view.cell_m,
            z0 + view.rows as f32 * view.cell_m,
        );
        let corners = [
            Vec3::new(x0, 0.1, z0),
            Vec3::new(x1, 0.1, z0),
            Vec3::new(x1, 0.1, z1),
            Vec3::new(x0, 0.1, z1),
        ];
        for i in 0..4 {
            out.push((corners[i], corners[(i + 1) % 4], GRID));
        }
    }
    for (at, radius) in &view.watchers {
        if out.len() + CIRCLE_SEGMENTS > MAX_FOG_SEGMENTS {
            break;
        }
        if !(radius.is_finite() && *radius > 0.0) || !at.iter().all(|v| v.is_finite()) {
            continue;
        }
        let center = Vec3::new(at[0], at[1] + 0.1, at[2]);
        let point = |k: usize| {
            let a = k as f32 / CIRCLE_SEGMENTS as f32 * std::f32::consts::TAU;
            center + Vec3::new(a.cos() * radius, 0.0, a.sin() * radius)
        };
        for k in 0..CIRCLE_SEGMENTS {
            out.push((point(k), point(k + 1), WATCHER));
        }
    }
    out
}

/// The texture preview's egui texture and the sync it shows.
#[derive(Resource)]
struct FogDebugTexture {
    handle: egui::TextureHandle,
    syncs: u64,
}

/// The fog texture as an egui image: unexplored black, explored grey, visible white; row 0 (north)
/// at the top.
#[must_use]
pub fn fog_color_image(view: &FogDebugView) -> egui::ColorImage {
    let (w, h) = (view.cols as usize, view.rows as usize);
    let pixels = (0..w * h)
        .map(|i| {
            let v = view.texels.get(i).copied().unwrap_or(0);
            egui::Color32::from_gray(v)
        })
        .collect();
    egui::ColorImage::new([w, h], pixels)
}

/// The Show menu's Fog of war rows (shown while the flag is on): the texture, its counters and a
/// viewer team picker (it queues `fog.set_viewer`'s request).
pub fn fog_menu(ui: &mut egui::Ui, world: &mut World) {
    let Some(status) = world.get_resource::<FogViewStatus>().cloned() else {
        ui.weak("No fog view in this world (play a level with FogViewPlugin).");
        return;
    };
    let mut viewer = status.viewer;
    ui.horizontal(|ui| {
        ui.label("Viewer team");
        let max = status.team_count.max(1);
        if ui
            .add(egui::DragValue::new(&mut viewer).range(0..=max))
            .on_hover_text("0 observes everything (fog.set_viewer)")
            .changed()
        {
            world.get_resource_or_insert_with(FogViewControl::default).viewer = Some(viewer);
        }
    });
    if let Some(stats) = world.get_resource::<FogTextureStats>().copied() {
        ui.weak(format!(
            "{} x {} cells of {:.0} m, {} rows uploaded last frame",
            stats.cols, stats.rows, stats.cell_m, stats.rows_uploaded
        ));
    }
    let Some(view) = world
        .get_resource::<FogDebugView>()
        .filter(|v| v.cols > 0)
        .cloned()
    else {
        return;
    };
    let syncs = world.get_resource::<FogTextureStats>().map_or(0, |s| s.syncs);
    let stale = world
        .get_resource::<FogDebugTexture>()
        .is_none_or(|t| t.syncs != syncs);
    if stale {
        let image = fog_color_image(&view);
        let handle = ui
            .ctx()
            .load_texture("fog_debug_texture", image, egui::TextureOptions::NEAREST);
        world.insert_resource(FogDebugTexture { handle, syncs });
    }
    if let Some(texture) = world.get_resource::<FogDebugTexture>() {
        let scale = 192.0 / view.cols.max(view.rows).max(1) as f32;
        let size = egui::vec2(view.cols as f32 * scale, view.rows as f32 * scale);
        ui.add(egui::Image::new((texture.handle.id(), size)));
        ui.weak(format!("{} watchers", view.watchers.len()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_follow_the_debug_view() {
        let mut world = World::new();
        assert!(fog_segments(&world).is_empty());
        world.insert_resource(FogDebugView {
            enabled: true,
            watchers: vec![([0.0, 0.0, 0.0], 150.0), ([1.0, 0.0, 1.0], f32::NAN)],
            cols: 4,
            rows: 2,
            origin: [-64.0, -32.0],
            cell_m: 32.0,
            texels: vec![0, 128, 255, 0, 0, 0, 255, 255],
        });
        let segments = fog_segments(&world);
        assert_eq!(
            segments.len(),
            4 + CIRCLE_SEGMENTS,
            "the outline and one valid circle"
        );
        assert!(segments[..4].iter().any(|(a, _, _)| a.x == 64.0 && a.z == 32.0));
        let r = (segments[4].0 - Vec3::new(0.0, 0.1, 0.0)).length();
        assert!((r - 150.0).abs() < 1e-3, "{r}");
        sync_flag(&mut world, false);
        assert!(fog_segments(&world).is_empty());
        let image = fog_color_image(world.resource::<FogDebugView>());
        assert_eq!(image.size, [4, 2]);
        assert_eq!(image.pixels[1], egui::Color32::from_gray(128));
    }
}
