// SPDX-License-Identifier: MPL-2.0
//! Terrain debug views of the shared colour pass, switchable while the draw is in the renderer
//! (ADR-1447, book `agents/terrain-debug-views.md`).
//!
//! `TerrainSharedDraw::set_debug_view` picks a view before the draw goes into the renderer; after
//! that the draw is a boxed `SharedPassDraw` the host no longer reaches. [`TerrainDebugViewHandle`]
//! is the draw's view as a shared atomic: the draw reads it at every prepare, so a host, or the
//! agent through [`TerrainDebugViewControl`] (`terrain.get_debug_view` / `terrain.set_debug_view`),
//! changes the view on a running frame. [`TERRAIN_DEBUG_VIEWS`] names the views `terrain.wgsl`
//! implements.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use regia_app::prelude::*;

/// One terrain debug view: the `TerrainFrame::debug_view` number `terrain.wgsl` reads, a stable
/// name and what its channels hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerrainDebugView {
    /// The shader's view number (0 = off, the lit terrain).
    pub id: u32,
    /// Stable lower-case name (the agent accepts it in place of the number).
    pub name: &'static str,
    /// What the view prints. On the shared HDR target a value of 1 prints like a white Lambert
    /// surface in full sun (`E_sun / π`), before the frame's exposure.
    pub channels: &'static str,
}

/// Every view the shared colour pass implements (`shade`, `shared_debug` in `terrain.wgsl`).
pub const TERRAIN_DEBUG_VIEWS: &[TerrainDebugView] = &[
    TerrainDebugView {
        id: 0,
        name: "off",
        channels: "the lit terrain",
    },
    TerrainDebugView {
        id: 1,
        name: "lod",
        channels: "the lit terrain tinted by CDLOD level",
    },
    TerrainDebugView {
        id: 2,
        name: "n_dot_l",
        channels: "grey: N·L of the shading normal",
    },
    TerrainDebugView {
        id: 3,
        name: "albedo",
        channels: "rgb: the blended albedo before lighting",
    },
    TerrainDebugView {
        id: 4,
        name: "layer_weights",
        channels: "rgb: normalised weights of the first three layers",
    },
    TerrainDebugView {
        id: 5,
        name: "slope",
        channels: "grey: slope, 90° = 1",
    },
    TerrainDebugView {
        id: 6,
        name: "height",
        channels: "grey: normalised height",
    },
    TerrainDebugView {
        id: 7,
        name: "sim_agreement",
        channels: "green/blue agree with the physics heightfield, red/yellow do not",
    },
    TerrainDebugView {
        id: 8,
        name: "sun_split",
        channels: "R cascade shadow, G heightfield shadow times micro shadow, B N·L, each times the views' scale. R×G equals the lit visibility only when one shadow is 0 or 1",
    },
    TerrainDebugView {
        id: 9,
        name: "indirect_split",
        channels: "unscaled. Per unit albedo, irradiance / π: R sky and ground ambient after occlusion, G planet-shine fill, B terrain bounce before probes replace it",
    },
    TerrainDebugView {
        id: 10,
        name: "near_field",
        channels: "R near-field strength, G icy (crust), B frost coverage",
    },
    TerrainDebugView {
        id: 11,
        name: "cascade_shadow",
        channels: "grey: the cascade shadow alone",
    },
    TerrainDebugView {
        id: 12,
        name: "cascade_occluder",
        channels: "R metres the cascade depth sits in front (÷ 2), G cascade index ÷ 3, B metres behind (÷ 2)",
    },
    TerrainDebugView {
        id: 13,
        name: "far_sun",
        channels: "R heightfield sun visibility to ~2 km, G distance of the first occluder ÷ 2 km",
    },
    TerrainDebugView {
        id: 14,
        name: "occlusion_relief",
        channels: "R heightfield cavity, G baked macro AO, B irradiance-probe coverage",
    },
    TerrainDebugView {
        id: 15,
        name: "occlusion_texture",
        channels: "R layers' ORM occlusion, G screen-space AO, B 1 (the views' scale)",
    },
    TerrainDebugView {
        id: 16,
        name: "normal",
        channels: "rgb: the shading normal as 0.5 + 0.5·n",
    },
];

impl TerrainDebugView {
    /// The view with this number or name (`"8"` and `"sun_split"` both name view 8).
    #[must_use]
    pub fn find(key: &str) -> Option<Self> {
        let key = key.trim();
        TERRAIN_DEBUG_VIEWS
            .iter()
            .find(|v| v.name.eq_ignore_ascii_case(key) || key.parse::<u32>().ok() == Some(v.id))
            .copied()
    }

    /// The view with this number.
    #[must_use]
    pub fn by_id(id: u32) -> Option<Self> {
        TERRAIN_DEBUG_VIEWS.iter().find(|v| v.id == id).copied()
    }
}

/// The shared terrain draw's debug view, shared with whoever may change it while the draw runs
/// (the draw reads it at its next prepare).
#[derive(Debug, Clone, Default)]
pub struct TerrainDebugViewHandle(Arc<AtomicU32>);

impl TerrainDebugViewHandle {
    /// Sets the view number (`TerrainFrame::debug_view`; 0 = off).
    pub fn set(&self, view: u32) {
        self.0.store(view, Ordering::Relaxed);
    }

    /// The view number in effect.
    #[must_use]
    pub fn get(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}

/// The ECS resource a host inserts with its terrain draw's [`TerrainDebugViewHandle`]
/// (`TerrainSharedDraw::debug_view_handle`), so agent `terrain.get_debug_view` /
/// `terrain.set_debug_view` reach the running draw.
#[derive(Resource, Debug, Clone, Default)]
pub struct TerrainDebugViewControl(pub TerrainDebugViewHandle);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_are_found_by_number_and_name_and_are_unique() {
        assert_eq!(TerrainDebugView::find("sun_split").map(|v| v.id), Some(8));
        assert_eq!(
            TerrainDebugView::find(" 9 ").map(|v| v.name),
            Some("indirect_split")
        );
        assert_eq!(
            TerrainDebugView::find("Occlusion_Texture").map(|v| v.id),
            Some(15)
        );
        assert_eq!(TerrainDebugView::by_id(0).map(|v| v.name), Some("off"));
        assert!(TerrainDebugView::find("17").is_none());
        assert!(TerrainDebugView::find("sky").is_none());
        for (i, a) in TERRAIN_DEBUG_VIEWS.iter().enumerate() {
            for b in &TERRAIN_DEBUG_VIEWS[i + 1..] {
                assert!(a.id != b.id && a.name != b.name, "{a:?} / {b:?}");
            }
            assert!(
                a.name
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_'),
                "{}",
                a.name
            );
        }
    }

    #[test]
    fn a_cloned_handle_shares_the_view() {
        let handle = TerrainDebugViewHandle::default();
        assert_eq!(handle.get(), 0);
        let control = TerrainDebugViewControl(handle.clone());
        control.0.set(14);
        assert_eq!(handle.get(), 14);
    }
}
