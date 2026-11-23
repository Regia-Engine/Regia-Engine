// SPDX-License-Identifier: MPL-2.0
//! Debug draw and view modes (`specs/13` § 11).
//!
//! Debug lines and shapes are [`crate::gizmos`]; [`DebugDraw`] is the original single-method
//! queue, kept as a thin shim over the default gizmo group.

use regia_app::prelude::*;

/// Editor / dev visualization modes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum DebugViewMode {
    /// Shaded with lighting.
    #[default]
    Lit,
    /// Base colour only.
    Unlit,
    /// Wireframe overlay (when supported).
    Wireframe,
    /// Interpolated normals as colour.
    Normals,
    /// Base colour channel.
    BaseColor,
    /// Roughness channel.
    Roughness,
    /// Metallic channel.
    Metallic,
    /// Ambient occlusion.
    AmbientOcclusion,
    /// Overdraw heatmap.
    Overdraw,
    /// Lights per cluster.
    LightComplexity,
    /// Cascade colours.
    ShadowCascades,
    /// LOD tint.
    LodColor,
    /// Motion vectors.
    MotionVectors,
    /// Texture mip level.
    MipLevel,
    /// UV checker texture, scaled per mesh so one checker cell spans `tile_m` meters of
    /// surface (`tile_m <= 0`: four cells per UV unit). Stretch shows as non-square cells.
    UvChecker {
        /// Edge of one checker cell, meters.
        tile_m: f32,
    },
    /// Texel density per triangle: blue under the target, green within `tolerance`, red over.
    TexelDensity {
        /// Target density, texture pixels per meter of surface.
        target_px_per_m: f32,
        /// Allowed relative deviation (0.1 = ±10 %).
        tolerance: f32,
    },
    /// A stable false colour per material, named by a legend.
    MaterialId,
    /// The cloud transmittance map (ADR-320): grey by the sun's transmittance through the cloud
    /// layer at every surface, and on the sky where the view ray meets the layer base.
    CloudShadowMap,
    /// Only the image-based light: its diffuse and specular with their occlusion, without the
    /// sun, the local lights and emission, through the frame's exposure.
    ImageLight,
    /// What occludes the image-based light: red = diffuse visibility, green = specular visibility,
    /// blue = the share of the image the ground plane replaced ([`crate::EnvGroundPlane`]).
    ImageLightOcclusion,
    /// Lightmap density: a checker with one cell per lightmap texel, coloured by the entry's density
    /// scale (`specs/lightmaps.md` § 3; [`crate::lightmap_debug`], ADR-2656).
    LightmapDensity,
    /// Only the lightmap's light: receivers as a white rough surface, no sun, no local lights.
    LightmapOnly,
    /// The texels the bake filled from their neighbours, magenta on grey.
    LightmapInvalid,
}

/// One debug line segment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DebugLine {
    /// Start position (world).
    pub from: [f32; 3],
    /// End position (world).
    pub to: [f32; 3],
    /// Linear RGBA colour.
    pub color: [f32; 4],
    /// When false, draws on top of geometry.
    pub depth_test: bool,
    /// Frames the line is drawn for: 0 and 1 draw it in the next frame only (ADR-7211).
    pub duration: u32,
}

/// One world-space debug text label (`specs/13` § 11), drawn with the gizmo stroke font facing the
/// primary camera (ADR-7211).
#[derive(Debug, Clone, PartialEq)]
pub struct DebugText {
    /// Centre of the label (world).
    pub position: [f32; 3],
    /// The text (printable ASCII; `\n` starts a line).
    pub text: String,
    /// Cap height, meters.
    pub size: f32,
    /// Linear RGBA colour.
    pub color: [f32; 4],
    /// When false, draws on top of geometry.
    pub depth_test: bool,
    /// Frames the label is drawn for: 0 and 1 draw it in the next frame only.
    pub duration: u32,
}

/// Immediate-mode debug draw queue (`specs/13` § 11): a thin shim over [`crate::gizmos`].
///
/// Insert it as a resource and push lines and labels; each frame the gizmo pass draws them through
/// the default gizmo group (items with `depth_test: false` on top). An item stays queued for its
/// `duration` in frames (0 or 1: one frame), then leaves the queue. New code should take a
/// [`crate::gizmos::Gizmos`] system parameter instead.
#[derive(Resource, Debug, Default, Clone)]
pub struct DebugDraw {
    /// Pending line segments.
    pub lines: Vec<DebugLine>,
    /// Pending text labels.
    pub texts: Vec<DebugText>,
}

impl DebugDraw {
    /// Adds a line segment for one frame.
    pub fn line(&mut self, from: [f32; 3], to: [f32; 3], color: [f32; 4]) {
        self.line_for(from, to, color, 1);
    }

    /// Adds a line segment drawn for `frames` frames (0 counts as 1).
    pub fn line_for(&mut self, from: [f32; 3], to: [f32; 3], color: [f32; 4], frames: u32) {
        self.lines.push(DebugLine {
            from,
            to,
            color,
            depth_test: true,
            duration: frames,
        });
    }

    /// Adds a text label centred on `position`, `size` meters tall, drawn for `frames` frames (0
    /// counts as 1).
    pub fn text(
        &mut self,
        position: [f32; 3],
        text: impl Into<String>,
        size: f32,
        color: [f32; 4],
        frames: u32,
    ) {
        self.texts.push(DebugText {
            position,
            text: text.into(),
            size,
            color,
            depth_test: true,
            duration: frames,
        });
    }

    /// Counts one drawn frame off every item and drops the items whose duration is over.
    pub fn advance_frame(&mut self) {
        self.lines.retain_mut(|l| {
            l.duration = l.duration.saturating_sub(1);
            l.duration > 0
        });
        self.texts.retain_mut(|t| {
            t.duration = t.duration.saturating_sub(1);
            t.duration > 0
        });
    }

    /// Clears all pending primitives.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.texts.clear();
    }

    /// True when nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.texts.is_empty()
    }
}
