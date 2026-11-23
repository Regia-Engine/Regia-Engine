// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_gizmos/src/retained.rs. Copyright (c) Bevy contributors, dual-licensed
// MIT OR Apache-2.0; used here under the MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//! Retained gizmos: line geometry built once and drawn every frame from a component.
//!
//! Bevy stores the geometry as an asset behind a handle; Regia Engine shares it through an [`Arc`]
//! (no asset server round trip for debug geometry).

use std::sync::Arc;

use regia_ecs::prelude::*;

use super::buffer::GizmoBuffer;
use super::config::{ErasedGizmoConfigGroup, GizmoLineConfig};

/// Retained gizmo geometry: build it with the usual [`GizmoBuffer`] calls, wrap it in an
/// [`Arc`] and attach it with a [`Gizmo`] component.
pub type GizmoAsset = GizmoBuffer<ErasedGizmoConfigGroup>;

/// Draws a [`GizmoAsset`] every frame at the entity's `GlobalTransform` (scale applies), or at
/// the world origin when the entity has none.
///
/// ```
/// use std::sync::Arc;
/// use regia_core::math::{Color, Vec3};
/// use regia_render::gizmos::{Gizmo, GizmoAsset};
///
/// let mut asset = GizmoAsset::new();
/// asset.sphere(Vec3::ZERO, 1.0, Color::GREEN);
/// let gizmo = Gizmo::new(Arc::new(asset));
/// assert!(!gizmo.asset.is_empty());
/// ```
#[derive(Component, Clone, Debug)]
pub struct Gizmo {
    /// The geometry, in the entity's local space.
    pub asset: Arc<GizmoAsset>,
    /// Line settings.
    pub line: GizmoLineConfig,
    /// Depth bias (see [`GizmoConfig::depth_bias`](super::GizmoConfig::depth_bias)).
    pub depth_bias: f32,
    /// Draw over the scene, ignoring its depth.
    pub on_top: bool,
}

impl Gizmo {
    /// A retained gizmo with default line settings.
    #[must_use]
    pub fn new(asset: Arc<GizmoAsset>) -> Self {
        Self {
            asset,
            line: GizmoLineConfig::default(),
            depth_bias: 0.0,
            on_top: false,
        }
    }
}
