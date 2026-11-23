// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_gizmos/src/config.rs. Copyright (c) Bevy contributors, dual-licensed
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
//! Gizmo configuration: per-group line width, style, joints, depth bias and on-top drawing.
//!
//! Every [`Gizmos`](super::Gizmos) call draws into a *config group* (a marker type implementing
//! [`GizmoConfigGroup`]). Each group carries one [`GizmoConfig`] in the [`GizmoConfigStore`]
//! resource, so a game can show or hide a family of gizmos (the physics colliders, the nav mesh)
//! and style it without touching the code that draws it. Groups have a stable
//! [`GizmoConfigGroup::NAME`] so the agent protocol (`debug.set_gizmos`) can address them.

use std::any::{Any, TypeId};
use std::collections::HashMap;

use regia_ecs::prelude::*;

/// How line joints of a line strip are drawn.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Hash)]
pub enum GizmoLineJoint {
    /// No joints: thick strips show notches at the corners.
    #[default]
    None,
    /// Extends both lines at the joint until they meet in a sharp point.
    Miter,
    /// A round corner made of this many triangles.
    Round(u32),
    /// A straight bevel between the two line ends.
    Bevel,
}

/// The pattern of a gizmo line (like CSS `line-style`).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub enum GizmoLineStyle {
    /// A solid line.
    #[default]
    Solid,
    /// A dotted line.
    Dotted,
    /// A dashed line.
    Dashed {
        /// Length of the gap, in line widths.
        gap_scale: f32,
        /// Length of the visible dash, in line widths.
        line_scale: f32,
    },
}

/// Line settings of a gizmo group.
#[derive(Clone, Debug, PartialEq)]
pub struct GizmoLineConfig {
    /// Line width in pixels. With [`Self::perspective`] it is the width at 1 m from the camera.
    ///
    /// Defaults to `2.0`.
    pub width: f32,
    /// Shrink lines with distance (perspective cameras).
    ///
    /// Defaults to `false`.
    pub perspective: bool,
    /// Solid, dotted or dashed.
    pub style: GizmoLineStyle,
    /// How line strips join.
    pub joints: GizmoLineJoint,
    /// Offset of dots/dashes along the line, in pattern periods (animate it to march the pattern).
    ///
    /// Defaults to `0.0`.
    pub animation_offset: f32,
}

impl Default for GizmoLineConfig {
    fn default() -> Self {
        Self {
            width: 2.0,
            perspective: false,
            style: GizmoLineStyle::Solid,
            joints: GizmoLineJoint::None,
            animation_offset: 0.0,
        }
    }
}

/// The configuration of one gizmo group.
#[derive(Clone, Debug, PartialEq)]
pub struct GizmoConfig {
    /// `false` drops everything the group draws (the draw calls return early).
    ///
    /// Defaults to `true` (the debug overlay groups default to `false`).
    pub enabled: bool,
    /// Line settings.
    pub line: GizmoLineConfig,
    /// Moves the lines towards (negative) or away from (positive) the camera in depth, in
    /// `-1..=1`. `0` keeps the true depth; `-1` puts the lines in front of everything; small
    /// negative values stop wireframes z-fighting with the surface they outline.
    pub depth_bias: f32,
    /// Draw over the scene, ignoring its depth (the line is never hidden by geometry).
    pub on_top: bool,
}

impl Default for GizmoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            line: GizmoLineConfig::default(),
            depth_bias: 0.0,
            on_top: false,
        }
    }
}

/// A gizmo config group: a marker type with its own [`GizmoConfig`] in the [`GizmoConfigStore`].
///
/// Register custom groups with
/// [`AppGizmoBuilder::init_gizmo_group`](super::AppGizmoBuilder::init_gizmo_group).
pub trait GizmoConfigGroup: Default + Send + Sync + 'static {
    /// Stable, unique name (agent protocol `debug.set_gizmos`, settings files).
    const NAME: &'static str;

    /// The configuration the group starts with.
    fn default_config() -> GizmoConfig {
        GizmoConfig::default()
    }
}

/// The default gizmo group ([`Gizmos`](super::Gizmos) without a type argument).
#[derive(Default, Debug, Clone, Copy)]
pub struct DefaultGizmoConfigGroup;

impl GizmoConfigGroup for DefaultGizmoConfigGroup {
    const NAME: &'static str = "default";
}

/// The type-erased group of retained gizmos ([`GizmoAsset`](super::GizmoAsset)); retained
/// gizmos carry their own line settings.
#[derive(Default, Debug, Clone, Copy)]
pub struct ErasedGizmoConfigGroup;

impl GizmoConfigGroup for ErasedGizmoConfigGroup {
    const NAME: &'static str = "retained";
}

macro_rules! debug_group {
    ($(#[$doc:meta])* $ty:ident, $name:literal, $bias:expr) => {
        $(#[$doc])*
        ///
        /// Disabled by default; turn it on with [`GizmoConfigStore::set_enabled`] or the agent
        /// method `debug.set_gizmos`.
        #[derive(Default, Debug, Clone, Copy)]
        pub struct $ty;

        impl GizmoConfigGroup for $ty {
            const NAME: &'static str = $name;

            fn default_config() -> GizmoConfig {
                GizmoConfig {
                    enabled: false,
                    depth_bias: $bias,
                    ..GizmoConfig::default()
                }
            }
        }
    };
}

debug_group!(
    /// Debug overlay: physics collider shapes.
    PhysicsGizmos,
    "physics",
    -0.002
);
debug_group!(
    /// Debug overlay: the navigation mesh (polygons, off-mesh links).
    NavMeshGizmos,
    "nav_mesh",
    -0.01
);
debug_group!(
    /// Debug overlay: audio emitters (attenuation spheres, cones).
    AudioGizmos,
    "audio",
    0.0
);
debug_group!(
    /// Debug overlay: AI perception (sight cones, hearing radii).
    AiPerceptionGizmos,
    "ai_perception",
    0.0
);
debug_group!(
    /// Debug overlay: smart-object slots (available / claimed / disabled markers and facing rays).
    SmartObjectGizmos,
    "smart_object",
    0.0
);
debug_group!(
    /// Debug overlay: data-driven camera rigs (pose frustums tinted by weight, boom line, collision
    /// probe, aim point, layer markers).
    CameraRigGizmos,
    "camera_rig",
    0.0
);
debug_group!(
    /// Debug overlay: motion matching (desired trajectory with facing ticks, the matched frame's
    /// own trajectory in gray, foot positions).
    MotionMatchingGizmos,
    "motion_matching",
    0.0
);
debug_group!(
    /// Debug overlay: post volumes (shape and blend band, `crate::post_volume_gizmos`; ADR-2323).
    PostVolumeGizmos,
    "post_volumes",
    0.0
);
debug_group!(
    /// Debug overlay: formation slots, member assignments and the column fallback (ADR-8358).
    FormationGizmos,
    "ai_formation",
    0.0
);
debug_group!(
    /// Debug overlay: behavior trees, state trees, plans, squads, formations, move targets and
    /// blackboard keys (ADR-8463).
    AiDebugGizmos,
    "ai_debug",
    0.0
);

struct StoreEntry {
    name: &'static str,
    config: GizmoConfig,
    ext: Box<dyn Any + Send + Sync>,
}

/// Every registered group's [`GizmoConfig`] and group value, keyed by group type.
#[derive(Resource, Default)]
pub struct GizmoConfigStore {
    store: HashMap<TypeId, StoreEntry>,
}

impl GizmoConfigStore {
    /// The config and group value of `G`, when the group is registered.
    #[must_use]
    pub fn config<G: GizmoConfigGroup>(&self) -> Option<(&GizmoConfig, &G)> {
        let entry = self.store.get(&TypeId::of::<G>())?;
        Some((&entry.config, entry.ext.downcast_ref::<G>()?))
    }

    /// Mutable config and group value of `G`, when the group is registered.
    pub fn config_mut<G: GizmoConfigGroup>(&mut self) -> Option<(&mut GizmoConfig, &mut G)> {
        let entry = self.store.get_mut(&TypeId::of::<G>())?;
        Some((&mut entry.config, entry.ext.downcast_mut::<G>()?))
    }

    /// `true` when `G` is registered and enabled.
    #[must_use]
    pub fn is_enabled<G: GizmoConfigGroup>(&self) -> bool {
        self.store
            .get(&TypeId::of::<G>())
            .is_some_and(|entry| entry.config.enabled)
    }

    /// The config of the group named `name` ([`GizmoConfigGroup::NAME`]).
    #[must_use]
    pub fn config_by_name(&self, name: &str) -> Option<&GizmoConfig> {
        self.store
            .values()
            .find(|entry| entry.name == name)
            .map(|entry| &entry.config)
    }

    /// Mutable config of the group named `name`.
    pub fn config_by_name_mut(&mut self, name: &str) -> Option<&mut GizmoConfig> {
        self.store
            .values_mut()
            .find(|entry| entry.name == name)
            .map(|entry| &mut entry.config)
    }

    /// Enables or disables the group named `name`. Returns `false` when no such group exists.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        match self.config_by_name_mut(name) {
            Some(config) => {
                config.enabled = enabled;
                true
            }
            None => false,
        }
    }

    /// Every registered group as `(name, config)`, sorted by name.
    #[must_use]
    pub fn iter(&self) -> Vec<(&'static str, &GizmoConfig)> {
        let mut out: Vec<_> = self
            .store
            .values()
            .map(|entry| (entry.name, &entry.config))
            .collect();
        out.sort_by_key(|(name, _)| *name);
        out
    }

    /// Inserts (or replaces) the config and group value of `G`.
    pub fn insert<G: GizmoConfigGroup>(&mut self, config: GizmoConfig, ext: G) {
        self.store.insert(
            TypeId::of::<G>(),
            StoreEntry {
                name: G::NAME,
                config,
                ext: Box::new(ext),
            },
        );
    }

    /// Registers `G` with [`GizmoConfigGroup::default_config`] unless it is already present.
    pub fn register<G: GizmoConfigGroup>(&mut self) {
        if !self.store.contains_key(&TypeId::of::<G>()) {
            self.insert(G::default_config(), G::default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_groups_start_disabled_and_toggle_by_name() {
        let mut store = GizmoConfigStore::default();
        store.register::<DefaultGizmoConfigGroup>();
        store.register::<PhysicsGizmos>();
        store.register::<NavMeshGizmos>();
        assert!(store.is_enabled::<DefaultGizmoConfigGroup>());
        assert!(!store.is_enabled::<PhysicsGizmos>());
        assert!(store.set_enabled("physics", true));
        assert!(store.is_enabled::<PhysicsGizmos>());
        assert!(!store.set_enabled("no_such_group", true));
        let names: Vec<_> = store.iter().into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["default", "nav_mesh", "physics"]);
        assert!(
            store
                .config_by_name("nav_mesh")
                .is_some_and(|c| c.depth_bias < 0.0)
        );
    }

    #[test]
    fn register_keeps_an_existing_config() {
        let mut store = GizmoConfigStore::default();
        store.insert(
            GizmoConfig {
                depth_bias: -0.5,
                ..GizmoConfig::default()
            },
            DefaultGizmoConfigGroup,
        );
        store.register::<DefaultGizmoConfigGroup>();
        assert_eq!(
            store
                .config::<DefaultGizmoConfigGroup>()
                .map(|(config, _)| config.depth_bias),
            Some(-0.5)
        );
    }
}
