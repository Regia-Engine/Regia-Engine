// SPDX-License-Identifier: MPL-2.0
//! Where the planet-shine fill on the ground takes the reduced occlusion (ADR-8412).
//!
//! The shared terrain colour pass multiplies the sky body's fill by an occlusion. By default it
//! is the full indirect stack (the heightfield cavity, the baked macro AO, the layers' ORM
//! occlusion and the screen-space AO, `vis` in `terrain.wgsl`). The canyon-scale visibility
//! (`min(macro AO, SSAO)`) suits a shadowless fill from a small disc better, but switched on for
//! the whole map it lifts every fill-lit trench and canyon floor too. [`FillCanyonScope`] picks
//! where it applies; [`fill_visibility`] and [`near_occluder_shade`] are the CPU mirrors of the
//! shader's pick and of its proxy weight (`mesh_occluder_shade`).

use regia_core::glam::{Vec2, Vec3};
use regia_render::{MAX_MESH_OCCLUDERS, MeshOccluderBlock};

/// Where the planet-shine fill's visibility is the canyon-scale occlusion only
/// (`TerrainSharedDraw::set_fill_canyon_scope`). The default, [`Self::Off`], keeps the full
/// indirect occlusion stack everywhere.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FillCanyonScope {
    /// The full indirect occlusion stack everywhere (the default; the look before ADR-8412).
    #[default]
    Off,
    /// The canyon-scale macro AO and the SSAO only, on every fill-lit pixel of the map
    /// (`set_fill_canyon_visibility(true)`).
    MapWide,
    /// The canyon-scale occlusion only in the sun shade of a mesh occluder (a hull box sent as
    /// a `regia_render::MeshOccluder`), in proportion to [`near_occluder_shade`]; the full stack
    /// everywhere else, so trench and canyon floors away from the hulls keep their look.
    NearOccluders,
}

impl FillCanyonScope {
    /// Every scope, in the order of [`Self::NAMES`].
    pub const ALL: [Self; 3] = [Self::Off, Self::MapWide, Self::NearOccluders];

    /// The scopes' protocol names (`terrain.get_ground_look`'s `fill_canyon_scope`).
    pub const NAMES: [&'static str; 3] = ["off", "map_wide", "near_occluders"];

    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => Self::NAMES[0],
            Self::MapWide => Self::NAMES[1],
            Self::NearOccluders => Self::NAMES[2],
        }
    }

    /// The scope named `name` (one of [`Self::NAMES`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The colour pass's `fill_dir.w` with a fill light on: 1 off, 2 map-wide, 3 near
    /// occluders (no uniform layout change; 0 stays "no fill").
    #[must_use]
    pub fn uniform_w(self) -> f32 {
        match self {
            Self::Off => 1.0,
            Self::MapWide => 2.0,
            Self::NearOccluders => 3.0,
        }
    }

    /// The scope stored as a small integer (the agent handle's atomic).
    #[must_use]
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::MapWide => 1,
            Self::NearOccluders => 2,
        }
    }

    /// The inverse of [`Self::to_u8`]; anything else is [`Self::Off`].
    #[must_use]
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::MapWide,
            2 => Self::NearOccluders,
            _ => Self::Off,
        }
    }
}

/// The fill's visibility on one ground pixel, CPU mirror of the colour pass's pick
/// (`terrain.wgsl`, the planet-shine block): `vis` is the full indirect stack, `macro_ao` and
/// `ssao` the canyon-scale terms, `occluder_weight` the pixel's [`near_occluder_shade`].
/// [`FillCanyonScope::Off`] returns `vis`, [`FillCanyonScope::MapWide`] `min(macro_ao, ssao)`,
/// and [`FillCanyonScope::NearOccluders`] moves linearly from `vis` (weight 0) to that
/// (weight 1).
#[must_use]
pub fn fill_visibility(
    scope: FillCanyonScope,
    vis: f32,
    macro_ao: f32,
    ssao: f32,
    occluder_weight: f32,
) -> f32 {
    let reduced = macro_ao.min(ssao);
    match scope {
        FillCanyonScope::Off => vis,
        FillCanyonScope::MapWide => reduced,
        FillCanyonScope::NearOccluders => {
            let w = occluder_weight.clamp(0.0, 1.0);
            if w > 0.0 { vis + (reduced - vis) * w } else { vis }
        }
    }
}

/// Edge softness of a shadow footprint: this share of the polygon's radius outside its edge.
const SHADE_SOFT: f32 = 0.25;

/// Softness floor, metres.
const SHADE_SOFT_MIN: f32 = 0.1;

/// Least box height (metres) the vertical fade uses.
const SHADE_MIN_HEIGHT: f32 = 0.5;

/// How much the ground point `p` lies in a mesh occluder's sun shade, `0..=1`: CPU mirror of
/// `mesh_occluder_shade` in `terrain.wgsl`. Per occluder with a sun-shadow polygon (three or
/// more vertices: the sun at least ~3° up), 1 inside the polygon on its ground plane, falling
/// to 0 over a quarter of the polygon's radius (at least 0.1 m) outside its edge, and over one
/// to two box heights (top above the ground plane, at least 0.5 m) above or below the plane;
/// the largest over the occluders. 0 with no occluders or far from all of them.
#[must_use]
pub fn near_occluder_shade(block: &MeshOccluderBlock, p: [f32; 3]) -> f32 {
    let p = Vec3::from_array(p);
    let count = block.count().min(MAX_MESH_OCCLUDERS);
    let listed = block.cell_mask(p);
    let mut cover = 0.0f32;
    for i in 0..count {
        if listed & (1 << i) == 0 {
            continue;
        }
        let [sx, sz, radius, ground_y] = block.shadow_bounds[i];
        if radius < 0.0 {
            continue;
        }
        let soft = (SHADE_SOFT * radius).max(SHADE_SOFT_MIN);
        let q = Vec2::new(p.x, p.z);
        if q.distance(Vec2::new(sx, sz)) > radius + soft {
            continue;
        }
        let occ = &block.items[i];
        let depth: f32 = (0..3).map(|k| occ.axes[k][1].abs() * occ.axes[k][3]).sum();
        let tall = (occ.center[1] + depth - ground_y).max(SHADE_MIN_HEIGHT);
        let vertical = 1.0 - smoothstep(tall, 2.0 * tall, (p.y - ground_y).abs());
        if vertical <= 0.0 {
            continue;
        }
        let verts = ((occ.faces[1][3] + 0.5) as usize).min(6);
        let vertex = |j: usize| {
            let row = occ.shadow[j / 2];
            Vec2::new(row[(j % 2) * 2], row[(j % 2) * 2 + 1])
        };
        let mut sd = f32::MAX;
        for j in 0..verts {
            let (a, b) = (vertex(j), vertex((j + 1) % verts));
            let e = b - a;
            let inward = Vec2::new(-e.y, e.x) / e.length().max(1e-6);
            sd = sd.min((q - a).dot(inward));
        }
        cover = cover.max(smoothstep(-soft, 0.0, sd) * vertical);
    }
    cover
}

/// WGSL `smoothstep`.
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{MeshOccluder, OccluderLighting};

    #[test]
    fn off_keeps_the_full_stack() {
        for (vis, macro_ao, ssao, w) in [(0.31, 0.9, 1.0, 0.0), (0.31, 0.9, 1.0, 1.0), (0.8, 0.2, 0.5, 0.5)] {
            assert_eq!(fill_visibility(FillCanyonScope::Off, vis, macro_ao, ssao, w), vis);
        }
    }

    #[test]
    fn map_wide_is_the_canyon_scale_min_everywhere() {
        for w in [0.0, 0.4, 1.0] {
            assert_eq!(fill_visibility(FillCanyonScope::MapWide, 0.31, 0.9, 0.7, w), 0.7);
            assert_eq!(fill_visibility(FillCanyonScope::MapWide, 0.31, 0.6, 0.7, w), 0.6);
        }
    }

    #[test]
    fn near_occluders_moves_from_the_stack_to_the_reduced_term_with_the_weight() {
        let (vis, macro_ao, ssao) = (0.31, 0.9, 0.8);
        let at = |w: f32| fill_visibility(FillCanyonScope::NearOccluders, vis, macro_ao, ssao, w);
        assert_eq!(at(0.0), vis, "no occluder: the full stack, bit for bit");
        assert_eq!(at(-1.0), vis, "a negative weight is none");
        assert!((at(1.0) - 0.8).abs() < 1e-6, "under a hull: the reduced term");
        assert!((at(2.0) - 0.8).abs() < 1e-6, "the weight saturates at 1");
        let mut last = at(0.0);
        for k in 1..=20 {
            let v = at(k as f32 / 20.0);
            assert!(
                v >= last,
                "not monotonic at w = {}: {v} < {last}",
                k as f32 / 20.0
            );
            last = v;
        }
        // The reduced term may be the darker one (multi-bounce can lift `vis` above it): still
        // monotonic, now downward.
        let down = |w: f32| fill_visibility(FillCanyonScope::NearOccluders, 0.9, 0.5, 1.0, w);
        assert!(down(0.5) < down(0.0) && down(1.0) < down(0.5));
    }

    #[test]
    fn names_and_uniform_codes_round_trip() {
        for scope in FillCanyonScope::ALL {
            assert_eq!(FillCanyonScope::from_name(scope.name()), Some(scope));
            assert_eq!(FillCanyonScope::from_u8(scope.to_u8()), scope);
        }
        assert_eq!(FillCanyonScope::from_name("on"), None);
        assert_eq!(FillCanyonScope::default(), FillCanyonScope::Off);
        assert_eq!(FillCanyonScope::Off.uniform_w(), 1.0);
        assert_eq!(FillCanyonScope::MapWide.uniform_w(), 2.0);
        assert_eq!(FillCanyonScope::NearOccluders.uniform_w(), 3.0);
    }

    /// A 4 × 2 × 2 m hull standing on y = 0 at the origin, sun 45° up from +X.
    fn hull_block() -> MeshOccluderBlock {
        let hull = MeshOccluder {
            center: [0.0, 1.0, 0.0],
            axes: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            half_extents: [2.0, 1.0, 1.0],
            albedo: [0.3, 0.3, 0.3],
            ground_y: 0.0,
            far_band: false,
        };
        let s = std::f32::consts::FRAC_1_SQRT_2;
        MeshOccluderBlock::build(
            &[hull],
            &OccluderLighting {
                toward_sun: [s, s, 0.0],
                sun: [0.0; 3],
                sky_ambient: [[0.0; 3]; 2],
                fill: Some([[-0.6, 0.6, 0.53], [1600.0; 3]]),
                eye: [0.0, 10.0, -20.0],
            },
        )
    }

    #[test]
    fn the_shade_weight_covers_the_hulls_sun_shadow_and_nothing_far() {
        let block = hull_block();
        assert_eq!(block.count(), 1);
        // Under the hull and on its shadow side (-X: the sun is toward +X).
        assert_eq!(near_occluder_shade(&block, [0.0, 0.0, 0.0]), 1.0);
        assert_eq!(near_occluder_shade(&block, [-3.0, 0.0, 0.0]), 1.0);
        // Sunlit ground on the sun side, far ground, and a trench floor 20 m down under the
        // shadow: none.
        assert_eq!(near_occluder_shade(&block, [6.0, 0.0, 0.0]), 0.0);
        assert_eq!(near_occluder_shade(&block, [60.0, 0.0, 40.0]), 0.0);
        assert_eq!(near_occluder_shade(&block, [-3.0, -20.0, 0.0]), 0.0);
        // No occluders: none.
        assert_eq!(
            near_occluder_shade(&MeshOccluderBlock::default(), [0.0, 0.0, 0.0]),
            0.0
        );
    }

    #[test]
    fn the_shade_weight_fades_monotonically_past_the_polygon_edge() {
        let block = hull_block();
        // Walk out along +Z from the hull's middle: the shadow's side edge is at z = 1.
        let mut last = 1.0;
        let mut saw_partial = false;
        for k in 0..=40 {
            let z = 0.5 + k as f32 * 0.05;
            let w = near_occluder_shade(&block, [-1.0, 0.0, z]);
            assert!(w <= last + 1e-6, "rises at z = {z}: {w} > {last}");
            saw_partial |= w > 0.0 && w < 1.0;
            last = w;
        }
        assert!(saw_partial, "no soft edge");
        assert_eq!(last, 0.0, "still shaded 2.5 m out");
    }

    #[test]
    fn a_low_sun_casts_no_polygon_and_no_weight() {
        let hull = MeshOccluder {
            center: [0.0, 1.0, 0.0],
            axes: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            half_extents: [2.0, 1.0, 1.0],
            albedo: [0.3; 3],
            ground_y: 0.0,
            far_band: false,
        };
        let block = MeshOccluderBlock::build(
            &[hull],
            &OccluderLighting {
                toward_sun: [1.0, 0.0, 0.0],
                sun: [0.0; 3],
                sky_ambient: [[0.0; 3]; 2],
                fill: None,
                eye: [0.0, 10.0, -20.0],
            },
        );
        assert_eq!(near_occluder_shade(&block, [0.0, 0.0, 0.0]), 0.0);
    }
}
