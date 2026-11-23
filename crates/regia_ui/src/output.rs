// SPDX-License-Identifier: MPL-2.0
//! Render-out layout: the HUD laid out for the *output* resolution and orientation (16:9 or
//! native 9:16), not the window's.
//!
//! Hosts author one [`crate::UiRoot`] per layout and tag it with [`UiVariant`]; roots without the tag
//! show in every layout. The engine renders the roots of the active [`UiLayoutVariant`]: the
//! window's orientation normally (so a portrait phone window picks the portrait roots), or the
//! chosen one during a render-out ([`begin_render_out`] / [`end_render_out`], installed as
//! `regia_render`'s [`RenderOutHud`] so the agent's offscreen screenshot and video render-out
//! hosts include the HUD). Roots that opt into [`crate::SafeAreaPadding`] are inset by
//! [`default_safe_area`] during a render-out unless the host sets [`UiOutput::safe_area`].

use regia_app::prelude::*;
use regia_platform::SafeArea;
use regia_render::{HudLayout, RenderOutHud, RenderOutSpec};

use crate::extract::{UiDamage, UiDamageSet};
use crate::layout::{LayoutDirty, log_layout_error, run_ui_layout};
use crate::scale::{ScaleMode, UiScales};

/// A HUD layout orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UiLayoutVariant {
    /// 16:9 (any width ≥ height).
    #[default]
    Landscape,
    /// Native 9:16 (any height > width).
    Portrait,
}

impl UiLayoutVariant {
    /// The orientation of a `width`×`height` output.
    #[must_use]
    pub fn for_size(width: f32, height: f32) -> Self {
        if height > width {
            Self::Portrait
        } else {
            Self::Landscape
        }
    }

    /// Resolves a render-out layout choice for an output of `width`×`height`.
    #[must_use]
    pub fn resolve(layout: HudLayout, width: f32, height: f32) -> Self {
        match layout {
            HudLayout::Landscape => Self::Landscape,
            HudLayout::Portrait => Self::Portrait,
            HudLayout::Auto => Self::for_size(width, height),
        }
    }
}

/// Put on a [`crate::UiRoot`]: the root is laid out and drawn only while its layout is active.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiVariant(pub UiLayoutVariant);

/// The output size and layout a render-out forces.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ForcedOutput {
    /// Output size in pixels.
    pub size: [f32; 2],
    /// The layout to show.
    pub variant: UiLayoutVariant,
    /// Supersampling factor of the render (1 = none): constant-pixel-size roots grow by it.
    pub pixel_scale: f32,
}

/// Output state of the UI: the render-out override and the host's safe-area choice.
#[derive(Resource, Debug, Default)]
pub struct UiOutput {
    /// Set between [`begin_render_out`] and [`end_render_out`].
    pub forced: Option<ForcedOutput>,
    /// Safe area (pixels) for [`crate::SafeAreaPadding`] roots during a render-out; `None` uses
    /// [`default_safe_area`].
    pub safe_area: Option<SafeArea>,
    /// The window size before the override (restored by [`end_render_out`]).
    saved_window_size: Option<[f32; 2]>,
    /// The window's OS scale factor, saved while a render-out forces it to 1.0.
    saved_scale_factor: Option<f64>,
    /// The variant the last layout ran with (a flip marks the layout dirty).
    last_variant: Option<UiLayoutVariant>,
}

/// The layout currently shown.
#[must_use]
pub fn active_variant(world: &World) -> UiLayoutVariant {
    if let Some(forced) = world.get_resource::<UiOutput>().and_then(|o| o.forced) {
        return forced.variant;
    }
    let size = world
        .get_resource::<UiScales>()
        .map_or([1.0, 1.0], |s| s.window_size);
    UiLayoutVariant::for_size(size[0], size[1])
}

/// The extra scale a render-out gives a root with `mode`: a supersampled frame has `pixel_scale`×
/// the pixels per UI pixel, which [`ScaleMode::ConstantPixelSize`] would otherwise not follow
/// (the other modes derive their scale from the frame size or the scale factor, both of which
/// carry the factor). 1 outside a render-out.
#[must_use]
pub fn render_out_boost(world: &World, mode: &ScaleMode) -> f32 {
    match (mode, world.get_resource::<UiOutput>().and_then(|o| o.forced)) {
        (ScaleMode::ConstantPixelSize, Some(forced)) => forced.pixel_scale.max(1.0),
        _ => 1.0,
    }
}

/// True when `root` is shown in the active layout (untagged roots always are).
#[must_use]
pub fn root_active(world: &World, root: Entity, active: UiLayoutVariant) -> bool {
    world.get::<UiVariant>(root).is_none_or(|v| v.0 == active)
}

/// The default render-out safe area for `variant` at `size` (pixels).
///
/// Landscape is the broadcast title-safe area, 5 % per edge (EBU R 95). Portrait keeps clear of
/// the controls vertical-video apps draw over the picture: 12 % at the top, 20 % at the bottom,
/// 5 % left and 10 % right (convention of the short-video platforms' published safe-zone
/// guides; hosts with other needs set [`UiOutput::safe_area`]).
#[must_use]
pub fn default_safe_area(variant: UiLayoutVariant, size: [f32; 2]) -> SafeArea {
    let px = |fraction: f32, extent: f32| (fraction * extent).round() as u32;
    match variant {
        UiLayoutVariant::Landscape => SafeArea {
            left: px(0.05, size[0]),
            top: px(0.05, size[1]),
            right: px(0.05, size[0]),
            bottom: px(0.05, size[1]),
        },
        UiLayoutVariant::Portrait => SafeArea {
            left: px(0.05, size[0]),
            top: px(0.12, size[1]),
            right: px(0.10, size[0]),
            bottom: px(0.20, size[1]),
        },
    }
}

/// The safe area the layout uses for a forced output, if one is forced.
#[must_use]
pub fn forced_safe_area(world: &World) -> Option<SafeArea> {
    let output = world.get_resource::<UiOutput>()?;
    let forced = output.forced?;
    Some(
        output
            .safe_area
            .unwrap_or_else(|| default_safe_area(forced.variant, forced.size)),
    )
}

/// `PreUpdate`, after window tracking: a change of the window's orientation switches the
/// layout, so the new variant's roots are laid out and drawn.
pub fn track_variant(world: &mut World) {
    let active = active_variant(world);
    let Some(mut output) = world.get_resource_mut::<UiOutput>() else {
        return;
    };
    if output.last_variant == Some(active) {
        return;
    }
    output.last_variant = Some(active);
    mark_all_dirty(world);
}

fn mark_all_dirty(world: &mut World) {
    if let Some(mut dirty) = world.get_resource_mut::<LayoutDirty>() {
        dirty.mark();
    }
    if let Some(mut damage) = world.get_resource_mut::<UiDamage>() {
        damage.mark();
    }
    if let Some(mut set) = world.get_resource_mut::<UiDamageSet>() {
        set.full = true;
    }
}

/// Lays the UI out, shapes its text and fills the draw list, outside the frame schedules.
fn relayout_and_extract(world: &mut World) {
    use bevy_ecs::system::RunSystemOnce;
    // The same font locale the frame loop uses, before the offscreen pass lays out and shapes.
    if let Err(error) = world.run_system_once(crate::text::sync_font_locale) {
        tracing::warn!(target: "regia_ui", "font locale sync failed: {error}");
    }
    log_layout_error(In(run_ui_layout(world)));
    if let Err(error) = world.run_system_once(crate::text::shape_text) {
        tracing::warn!(target: "regia_ui", "render-out text shaping failed: {error}");
    }
    if let Err(error) = world.run_system_once(crate::extract::detect_ui_damage) {
        tracing::warn!(target: "regia_ui", "render-out damage pass failed: {error}");
    }
    crate::extract::extract_ui(world);
}

/// Lays the UI out for `spec`'s output and fills the overlay's draw list; the caller then draws
/// the overlay layers into the output and calls [`end_render_out`].
pub fn begin_render_out(world: &mut World, spec: &RenderOutSpec) {
    let _span = tracing::trace_span!("ui_render_out_begin").entered();
    let size = [spec.width.max(1) as f32, spec.height.max(1) as f32];
    let variant = UiLayoutVariant::resolve(spec.layout, size[0], size[1]);
    let (previous, previous_factor) = {
        let mut scales = world.resource_mut::<UiScales>();
        let previous = (scales.window_size, scales.scale_factor);
        scales.window_size = size;
        // The output is in physical pixels of its own: the window's OS scale must not leak in.
        // A supersampled frame has that many times the pixels per device-independent pixel.
        scales.scale_factor = f64::from(spec.supersample.max(1));
        previous
    };
    {
        let mut output = world.get_resource_or_insert_with(UiOutput::default);
        if output.forced.is_none() {
            output.saved_window_size = Some(previous);
            output.saved_scale_factor = Some(previous_factor);
        }
        output.forced = Some(ForcedOutput {
            size,
            variant,
            pixel_scale: spec.supersample.max(1) as f32,
        });
    }
    mark_all_dirty(world);
    relayout_and_extract(world);
}

/// Ends a render-out: restores the window's size and layout and rebuilds the window's draw list.
pub fn end_render_out(world: &mut World, _spec: &RenderOutSpec) {
    let _span = tracing::trace_span!("ui_render_out_end").entered();
    let Some(mut output) = world.get_resource_mut::<UiOutput>() else {
        return;
    };
    output.forced = None;
    let restored = output.saved_window_size.take();
    let restored_factor = output.saved_scale_factor.take();
    output.last_variant = None;
    if let Some(size) = restored {
        world.resource_mut::<UiScales>().window_size = size;
    }
    if let Some(factor) = restored_factor {
        world.resource_mut::<UiScales>().scale_factor = factor;
    }
    mark_all_dirty(world);
    relayout_and_extract(world);
}

/// Installs the [`RenderOutHud`] hooks (called from the plugin).
pub(crate) fn install(app: &mut App) {
    app.init_resource::<UiOutput>();
    app.insert_resource(RenderOutHud::new(
        std::sync::Arc::new(begin_render_out),
        std::sync::Arc::new(end_render_out),
    ));
    app.add_systems(PreUpdate, track_variant.after(crate::scale::track_window_size));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_follows_orientation() {
        assert_eq!(
            UiLayoutVariant::for_size(1920.0, 1080.0),
            UiLayoutVariant::Landscape
        );
        assert_eq!(
            UiLayoutVariant::for_size(1080.0, 1920.0),
            UiLayoutVariant::Portrait
        );
        assert_eq!(
            UiLayoutVariant::for_size(1000.0, 1000.0),
            UiLayoutVariant::Landscape
        );
        assert_eq!(
            UiLayoutVariant::resolve(HudLayout::Portrait, 1920.0, 1080.0),
            UiLayoutVariant::Portrait
        );
    }

    #[test]
    fn portrait_safe_area_clears_platform_controls() {
        let safe = default_safe_area(UiLayoutVariant::Portrait, [1080.0, 1920.0]);
        assert_eq!(
            safe,
            SafeArea {
                left: 54,
                top: 230,
                right: 108,
                bottom: 384
            }
        );
        let landscape = default_safe_area(UiLayoutVariant::Landscape, [1920.0, 1080.0]);
        assert_eq!((landscape.left, landscape.top), (96, 54));
    }
}
