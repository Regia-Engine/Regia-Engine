// SPDX-License-Identifier: MPL-2.0
//! Foliage wind and the dynamic-caster shadow class (`specs/88`, ADR-2028).
//!
//! Two parts:
//!
//! - **Wind.** One scene wind vector (metres per second on world X and Z) that the renderer writes
//!   into every time-reading material graph next to the material clock: graph parameter slot 0
//!   holds `[time, previous time, wind x, wind z]` ([`crate::FrameRenderer::set_foliage_wind`]).
//!   [`FoliageSway`] is a ready-made vertex stage that bends a plant along that wind (a main bend
//!   that grows with height, a gust that varies per instance, a leaf flutter weighted by vertex
//!   colour red), with [`FoliageSway::offset`] as its CPU mirror for tests and tools.
//! - **Dynamic-caster shadow class.** Paged sun cascades and local-light atlas tiles are cached:
//!   a page is redrawn only when a caster's bounds or pose move ([`crate::PageId`]). A wind sway
//!   (or any time-reading vertex stage) moves the geometry without moving the bounds, so the
//!   cached page kept the pose it was drawn with and the shadow stood still under swaying leaves.
//!   Casters whose material has a time-reading vertex stage are **dynamic**: in the cascades the
//!   [`DynamicCasterPolicy`] names, their pages are redrawn every frame, and with
//!   `local_lights` so are the atlas tiles their bounds reach. Elsewhere they cache like any
//!   other caster (a far cascade holds the pose it was last drawn with). Vertex-shader-skinned
//!   casters with a joint-palette bound are dynamic too, on the frames their palette changes
//!   (ADR-5418): the caches page them by that bound, a pose that moves the bound redraws its pages
//!   in every cascade, and a pose that changes inside it is what the class refreshes. A skin without a bound redraws whole cascades
//!   every frame.
//!
//! ## Provenance
//!
//! The bend is Regia Engine's: a height-squared main bend along the wind with a small-angle arc
//! drop (`h²` offset, `amp² h³ / 2H` drop) and a per-vertex flutter along the normal, after the
//! main-plus-detail split described by Sousa, "Vegetation Procedural Animation and Shading",
//! GPU Gems 3 ch. 16 (2007) and the per-blade sway of Pelzer, "Rendering Countless Blades of
//! Waving Grass", GPU Gems ch. 7 (2004). Static and per-frame shadow content split by caster
//! class follows the shadow-caching discussion in Eisemann et al., "Real-Time Shadows" (2011),
//! ch. 10. No code from either source.

use crate::profile::FrameQuality;
use crate::surface_graph::{GRAPH_PARAM_SLOTS, GraphCode, SurfaceGraph};
use crate::wind_natural::{self, WindStyle};

/// Graph parameter slot holding [`FoliageSway`]'s `[stiffness, height_m, flutter_m, gust_period_s]`.
pub const FOLIAGE_SWAY_SLOT: usize = GRAPH_PARAM_SLOTS - 1;

/// Graph parameter slot holding [`FoliageSway`]'s `[max_wind_m_per_s, style, 0, 0]` (`style`:
/// 0 classic, 1 natural; ADR-2446).
pub const FOLIAGE_SWAY_LIMIT_SLOT: usize = GRAPH_PARAM_SLOTS - 2;

/// Fastest wind (m/s) [`crate::FrameRenderer::set_foliage_wind`] accepts; a faster vector is
/// scaled down to this length. 40 m/s is a strong storm (Beaufort 12 starts at 32.7 m/s).
pub const FOLIAGE_WIND_MAX_M_PER_S: f32 = 40.0;

/// The vertex stage [`FoliageSway`] installs (`material_vertex_offset`). Reads the scene wind from
/// slot 0 `zw`, the sway from [`FOLIAGE_SWAY_SLOT`] and the wind limit from
/// [`FOLIAGE_SWAY_LIMIT_SLOT`], and calls the one sway definition, `regia::foliage_sway::offset`
/// (`shaders/foliage_sway.wgsl`), which the visibility path's passes call too (ADR-3916). Mirrored by
/// [`FoliageSway::offset`]; keep the two in step.
pub const FOLIAGE_SWAY_VERTEX: &str = r"
fn material_vertex_offset(in: regia::pbr::surface::VertexOffsetInput) -> vec3<f32> {
    return regia::foliage_sway::offset(
        material.graph[15],
        material.graph[14],
        material.graph[0].zw,
        in.time,
        in.world_position,
        in.world_normal,
        in.vertex_color.r,
        in.object_position,
    );
}
";

/// A plant's response to the scene wind, as the [`FOLIAGE_SWAY_VERTEX`] stage reads it.
///
/// The stage bends every vertex above the object's origin along the wind: the horizontal offset
/// is `amp · h²` and the drop `amp² · h³ / (2 · height_m)` (the small-angle arc of a stem bent
/// over `height_m`), with `h` the vertex's height above the origin over `height_m` (clamped to
/// 0..1) and `amp = min(stiffness · speed · gust, height_m / 2)`. The gust is
/// `0.7 + 0.2 sin(ωt + φ) + 0.1 sin(3ωt + 1.7φ)` (0.4..1.0), `ω = 2π / gust_period_s`, with the
/// phase `φ` from the object's origin so neighbours do not move in step. Leaves flutter along
/// their normal by `flutter_m · red · speed / (speed + 5)` at seven times the gust frequency.
/// A gust period that divides 3600 s loops across the material clock's wrap (ADR-1238).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FoliageSway {
    /// Bend per metre per second of wind, metres (0.02 is a shrub, 0.05 a reed).
    pub stiffness: f32,
    /// Height of the plant above its origin, metres: full bend at the top.
    pub height_m: f32,
    /// Leaf flutter amplitude along the normal at vertex colour red = 1, metres.
    pub flutter_m: f32,
    /// Gust period, seconds. Pick one that divides 3600 (2, 3, 4, 5, 6, 8, 10, 12, 15 ...).
    pub gust_period_s: f32,
    /// Fastest wind the plant follows, m/s. The culling bounds grow by the offset this allows.
    pub max_wind_m_per_s: f32,
    /// [`WindStyle::Classic`] (the default) is the periodic gust above; [`WindStyle::Natural`]
    /// takes the gust from a travelling noise field with calm spells, slows the sway of tall
    /// plants, varies the phase across space and gives each plant (by stiffness and height) its
    /// own flutter band (ADR-2446). Stored in graph slot 14 `y`.
    pub style: WindStyle,
}

impl Default for FoliageSway {
    /// A two-metre shrub: 0.03 m per m/s, 2 cm flutter, a 4 s gust, up to 20 m/s.
    fn default() -> Self {
        Self {
            stiffness: 0.03,
            height_m: 2.0,
            flutter_m: 0.02,
            gust_period_s: 4.0,
            max_wind_m_per_s: 20.0,
            style: WindStyle::Classic,
        }
    }
}

impl FoliageSway {
    /// The vertex-stage code: [`FOLIAGE_SWAY_VERTEX`] next to `surface_wgsl`.
    #[must_use]
    pub fn graph_code(surface_wgsl: &str) -> GraphCode {
        GraphCode {
            surface: surface_wgsl.into(),
            vertex: Some(FOLIAGE_SWAY_VERTEX.into()),
            uv1: false,
        }
    }

    /// A surface graph for `surface_wgsl` with this sway installed ([`Self::apply`]).
    #[must_use]
    pub fn graph(&self, surface_wgsl: &str) -> SurfaceGraph {
        let mut graph = SurfaceGraph::from_code(&Self::graph_code(surface_wgsl));
        self.apply(&mut graph);
        graph
    }

    /// Writes the sway into `graph`'s parameter slots, marks it time-reading (so the renderer
    /// writes the clock and the wind) and sets its offset bound ([`Self::max_offset`]). The
    /// graph's code is not changed: pair it with [`Self::graph_code`].
    pub fn apply(&self, graph: &mut SurfaceGraph) {
        let s = self.sanitized();
        graph.params[FOLIAGE_SWAY_SLOT] = [s.stiffness, s.height_m, s.flutter_m, s.gust_period_s];
        graph.params[FOLIAGE_SWAY_LIMIT_SLOT] = [s.max_wind_m_per_s, s.style.gpu_word() as f32, 0.0, 0.0];
        graph.uses_time = true;
        graph.max_vertex_offset = graph.max_vertex_offset.max(s.max_offset());
    }

    /// Non-finite or negative fields as zero, the gust period at least 0.1 s, the wind limit
    /// within [`FOLIAGE_WIND_MAX_M_PER_S`].
    #[must_use]
    pub fn sanitized(&self) -> Self {
        let pos = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
        Self {
            stiffness: pos(self.stiffness),
            height_m: pos(self.height_m),
            flutter_m: pos(self.flutter_m),
            gust_period_s: pos(self.gust_period_s).max(0.1),
            max_wind_m_per_s: pos(self.max_wind_m_per_s).min(FOLIAGE_WIND_MAX_M_PER_S),
            style: self.style,
        }
    }

    /// Largest offset (metres) the stage can produce for any wind, time and vertex: the main bend
    /// at the top, its drop and the flutter.
    #[must_use]
    pub fn max_offset(&self) -> f32 {
        let s = self.sanitized();
        if s.height_m <= 1.0e-4 {
            return 0.0;
        }
        let amp = (s.stiffness * s.max_wind_m_per_s).min(0.5 * s.height_m);
        let drop = amp * amp / (2.0 * s.height_m);
        (amp * amp + drop * drop).sqrt() + s.flutter_m
    }

    /// The `natural` branch of [`Self::offset`] (`s` sanitized, `dir` the unit wind).
    fn natural_offset(
        &self,
        (world, normal, red): ([f32; 3], [f32; 3], f32),
        origin: [f32; 3],
        t: f32,
        (speed, dir): (f32, [f32; 2]),
    ) -> [f32; 3] {
        // The material clock wraps at 3600 s (ADR-1238): evaluate the field on one lap so the
        // wrap is seamless whatever the gust period, the height factor or the flutter band
        // (their cycle counts over an hour are not integers in general).
        let t = t.rem_euclid(crate::material_clock::MATERIAL_CLOCK_PERIOD_SECS as f32);
        let s = self;
        let o = [origin[0], origin[2]];
        let intensity = wind_natural::gust_intensity(o, dir, t);
        let phase = wind_natural::natural_phase(o, 0.0);
        let period = s.gust_period_s.max(0.1);
        let nw =
            std::f32::consts::TAU * 0.55 * wind_natural::height_frequency_factor(s.height_m) * t / period;
        let ngust = intensity * (0.55 + 0.45 * (nw + phase).sin());
        let amp = (s.stiffness * speed * ngust).min(0.5 * s.height_m);
        let h = ((world[1] - origin[1]) / s.height_m).clamp(0.0, 1.0);
        let bend = amp * h * h;
        let drop = amp * amp * h * h * h / (2.0 * s.height_m);
        let band = wind_natural::flutter_band(s.species_seed());
        let detail = world[0] * 1.7 + world[1] * 0.9 + world[2] * 2.3;
        let flutter = s.flutter_m
            * red.clamp(0.0, 1.0)
            * (speed / (speed + 5.0))
            * intensity
            * (std::f32::consts::TAU * 2.0 * band * t / period + detail).sin();
        [
            dir[0] * bend + normal[0] * flutter,
            -drop + normal[1] * flutter,
            dir[1] * bend + normal[2] * flutter,
        ]
    }

    /// The seed that picks this plant's flutter band: its stiffness and height bits (the stage
    /// reads the same two slots).
    fn species_seed(&self) -> u32 {
        self.stiffness.to_bits() ^ self.height_m.to_bits().wrapping_mul(0x9E37_79B1)
    }

    /// CPU mirror of [`FOLIAGE_SWAY_VERTEX`]: the world offset of a vertex at `world` (rest pose)
    /// with unit `normal` and vertex colour red `red`, on an object whose origin is `origin`, at
    /// material-clock `time` under scene `wind` (m/s on X and Z).
    #[must_use]
    pub fn offset(
        &self,
        world: [f32; 3],
        normal: [f32; 3],
        red: f32,
        origin: [f32; 3],
        time: f32,
        wind: [f32; 2],
    ) -> [f32; 3] {
        let s = self.sanitized();
        let raw = (wind[0] * wind[0] + wind[1] * wind[1]).sqrt();
        let speed = raw.min(s.max_wind_m_per_s);
        if !raw.is_finite() || !time.is_finite() || speed <= 1.0e-4 || s.height_m <= 1.0e-4 {
            return [0.0; 3];
        }
        let dir = [wind[0] / raw, wind[1] / raw];
        if s.style == WindStyle::Natural {
            return s.natural_offset((world, normal, red), origin, time, (speed, dir));
        }
        let lap = (time / s.gust_period_s).rem_euclid(1.0);
        let w = std::f32::consts::TAU * lap;
        let phase = origin[0] * 0.37 + origin[2] * 0.61;
        let gust = 0.7 + 0.2 * (w + phase).sin() + 0.1 * (3.0 * w + 1.7 * phase).sin();
        let amp = (s.stiffness * speed * gust).min(0.5 * s.height_m);
        let h = ((world[1] - origin[1]) / s.height_m).clamp(0.0, 1.0);
        let bend = amp * h * h;
        let drop = amp * amp * h * h * h / (2.0 * s.height_m);
        let detail = world[0] * 1.7 + world[1] * 0.9 + world[2] * 2.3;
        let flutter = s.flutter_m * red.clamp(0.0, 1.0) * (speed / (speed + 5.0)) * (7.0 * w + detail).sin();
        [
            dir[0] * bend + normal[0] * flutter,
            -drop + normal[1] * flutter,
            dir[1] * bend + normal[2] * flutter,
        ]
    }
}

/// The wind vector the renderer writes (m/s on world X and Z): non-finite components as zero,
/// longer than [`FOLIAGE_WIND_MAX_M_PER_S`] scaled down to it.
#[must_use]
pub fn sanitize_wind(wind: [f32; 2]) -> [f32; 2] {
    let w = wind.map(|v| if v.is_finite() { v } else { 0.0 });
    let len = (w[0] * w[0] + w[1] * w[1]).sqrt();
    if len > FOLIAGE_WIND_MAX_M_PER_S {
        let k = FOLIAGE_WIND_MAX_M_PER_S / len;
        [w[0] * k, w[1] * k]
    } else {
        w
    }
}

/// Runtime switch of the dynamic-caster shadow class
/// ([`crate::FrameRenderer::set_dynamic_caster_shadows`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DynamicCasterShadows {
    /// The frame quality decides ([`DynamicCasterPolicy::for_quality`]). The default.
    #[default]
    Tier,
    /// Animated casters cache like static ones: their shadows hold the pose a page was drawn with
    /// until their bounds or model move (a skinned pose inside its palette bound, a sway inside
    /// the inflated bound, stays as drawn).
    Off,
    /// Redraw animated casters' pages in the first `cascades` sun cascades (1..=4) every frame,
    /// and their local-light atlas tiles when `local_lights` is set.
    On {
        /// Sun cascades, from the nearest, whose dynamic pages are redrawn every frame.
        cascades: u8,
        /// Also redraw the local-light atlas tiles an animated caster's bounds reach.
        local_lights: bool,
    },
}

impl DynamicCasterShadows {
    /// The mode's name on the agent protocol: `"tier"`, `"off"` or `"on"`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tier => "tier",
            Self::Off => "off",
            Self::On { .. } => "on",
        }
    }

    /// What this mode does on a frame with quality `quality`.
    #[must_use]
    pub fn resolve(self, quality: &FrameQuality) -> DynamicCasterPolicy {
        if !quality.paged_sun_shadows {
            // Unpaged cascades and atlas tiles are redrawn every frame: animated casters are
            // already right, and there is no cache to refresh.
            return DynamicCasterPolicy::UNPAGED;
        }
        match self {
            Self::Tier => DynamicCasterPolicy::for_quality(quality),
            Self::Off => DynamicCasterPolicy::OFF,
            Self::On {
                cascades,
                local_lights,
            } => DynamicCasterPolicy::cascades(cascades, local_lights),
        }
    }
}

/// What the dynamic-caster class does on one frame (resolved from [`DynamicCasterShadows`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DynamicCasterPolicy {
    /// Sun cascades whose dynamic pages are redrawn every frame (bit `i` = cascade `i`).
    pub cascade_mask: u8,
    /// Local-light atlas tiles reached by an animated caster are redrawn every frame.
    pub local_lights: bool,
    /// The frame's sun shadows are not paged, so every cascade is redrawn anyway.
    pub unpaged: bool,
}

impl DynamicCasterPolicy {
    /// Nothing refreshed: animated casters cache like static ones.
    pub const OFF: Self = Self {
        cascade_mask: 0,
        local_lights: false,
        unpaged: false,
    };

    /// Unpaged shadows: nothing to refresh, everything is drawn every frame.
    pub const UNPAGED: Self = Self {
        cascade_mask: 0,
        local_lights: false,
        unpaged: true,
    };

    /// The first `count` cascades (clamped to 0..=4), with or without the atlas.
    #[must_use]
    pub fn cascades(count: u8, local_lights: bool) -> Self {
        let count = count.min(crate::forward::CASCADE_COUNT as u8);
        Self {
            cascade_mask: ((1u16 << count) - 1) as u8,
            local_lights,
            unpaged: false,
        }
    }

    /// The tier default. Four-cascade tiers (Desktop High, Ultra, Cinematic) refresh cascades 0
    /// and 1 and the local atlas; two-cascade paged tiers (Desktop Low, the Deck) cascade 0
    /// only; unpaged tiers (both Mobile presets) need nothing.
    #[must_use]
    pub fn for_quality(quality: &FrameQuality) -> Self {
        if !quality.paged_sun_shadows {
            return Self::UNPAGED;
        }
        if quality.csm_cascade_count >= 4 {
            Self::cascades(2, quality.local_shadow_atlas_size > 0)
        } else {
            Self::cascades(1, false)
        }
    }

    /// Animated casters are refreshed somewhere this frame.
    #[must_use]
    pub const fn refreshes(self) -> bool {
        self.cascade_mask != 0 || self.local_lights
    }

    /// The number of refreshed cascades (the highest set bit + 1).
    #[must_use]
    pub const fn cascade_count(self) -> u32 {
        8 - self.cascade_mask.leading_zeros()
    }
}

/// What the dynamic-caster class did on the last main-view frame
/// ([`crate::FrameRenderer::dynamic_caster_report`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DynamicCasterReport {
    /// The policy the frame ran with.
    pub policy: DynamicCasterPolicy,
    /// Shadow casters in the dynamic class, refreshed or not: those drawn with a time-reading
    /// vertex stage, and bounded skinned casters whose pose changed ([`Self::skinned_casters`]).
    pub animated_casters: u32,
    /// Of [`Self::animated_casters`], the vertex-shader-skinned casters with a joint-palette bound
    /// whose palette changed since the last frame, or whose material sways (ADR-5418): their pose can move inside that
    /// bound, so the class refreshes them like swaying plants. A held pose (the palette it drew
    /// with last frame) is not counted and caches like a static caster. Skins without a bound and
    /// compute-skinned twins are not counted either: they have no bound, so every frame redraws
    /// whole cascades for them.
    pub skinned_casters: u32,
    /// Boxes of swaying shared-draw content (terrain grass) that joined the class this frame
    /// (`SharedPassDraw::dynamic_shadow_boxes`; counted only on frames that refresh).
    pub shared_boxes: u32,
    /// Sun pages redrawn because a dynamic caster covers them (before merging with pages other
    /// casters dirtied).
    pub dynamic_pages: u32,
    /// Local-light atlas tiles redrawn because a dynamic caster reaches them.
    pub dynamic_atlas_tiles: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SURFACE: &str = crate::surface_graph::SAMPLE_SURFACE;

    #[test]
    fn the_stage_composes_in_every_host() {
        crate::surface_graph::validate_graph_code(&FoliageSway::graph_code(SURFACE))
            .expect("foliage sway in forward, shadow and velocity");
    }

    #[test]
    fn the_stage_reads_the_slots_it_documents() {
        assert_eq!(FOLIAGE_SWAY_SLOT, 15);
        assert_eq!(FOLIAGE_SWAY_LIMIT_SLOT, 14);
        assert!(FOLIAGE_SWAY_VERTEX.contains("material.graph[15]"));
        assert!(FOLIAGE_SWAY_VERTEX.contains("material.graph[14]"));
        assert!(FOLIAGE_SWAY_VERTEX.contains("material.graph[0].zw"));
    }

    /// One definition of the sway (ADR-3916): the stage calls the shared module and holds no motion
    /// of its own, and every cluster pass that moves a vertex goes through the same module.
    #[test]
    fn every_path_calls_the_one_sway_definition() {
        assert!(FOLIAGE_SWAY_VERTEX.contains("regia::foliage_sway::offset("));
        for token in ["sin(", "fract(", "fsw_"] {
            assert!(
                !FOLIAGE_SWAY_VERTEX.contains(token),
                "the stage computes motion itself: {token}"
            );
        }
        for (name, source) in [
            ("vis_raster", include_str!("visbuffer/vis_raster.wgsl")),
            ("vis_software", include_str!("visbuffer/vis_software.wgsl")),
            (
                "vis_shadow_raster",
                include_str!("visbuffer/vis_shadow_raster.wgsl"),
            ),
            ("vis_resolve", include_str!("visbuffer/vis_resolve.wgsl")),
            ("vis_velocity", include_str!("visbuffer/vis_velocity.wgsl")),
        ] {
            assert!(
                source.contains("regia::foliage_sway::cluster_offset("),
                "{name} does not call the shared sway"
            );
            assert!(!source.contains("fsw_"), "{name} has its own sway code");
        }
    }

    #[test]
    fn calm_air_and_the_origin_do_not_move() {
        let sway = FoliageSway::default();
        let top = [1.0, 2.0, 3.0];
        assert_eq!(
            sway.offset(top, [0.0, 1.0, 0.0], 1.0, [1.0, 0.0, 3.0], 1.25, [0.0, 0.0]),
            [0.0; 3]
        );
        // The root (h = 0) with no flutter weight stays put in any wind.
        let root = sway.offset(
            [1.0, 0.0, 3.0],
            [0.0, 1.0, 0.0],
            0.0,
            [1.0, 0.0, 3.0],
            1.25,
            [9.0, 4.0],
        );
        assert!(root.iter().all(|v| v.abs() < 1.0e-6), "{root:?}");
    }

    #[test]
    fn the_top_bends_downwind_and_drops() {
        let sway = FoliageSway {
            flutter_m: 0.0,
            ..FoliageSway::default()
        };
        let o = sway.offset([0.0, 2.0, 0.0], [0.0, 1.0, 0.0], 0.0, [0.0; 3], 0.5, [8.0, 0.0]);
        assert!(o[0] > 0.1, "{o:?}");
        assert!(o[1] < 0.0, "{o:?}");
        assert!(o[2].abs() < 1.0e-6, "{o:?}");
        let o = sway.offset([0.0, 2.0, 0.0], [0.0, 1.0, 0.0], 0.0, [0.0; 3], 0.5, [0.0, -8.0]);
        assert!(o[2] < -0.1 && o[0].abs() < 1.0e-6, "{o:?}");
    }

    #[test]
    fn no_offset_exceeds_the_declared_bound() {
        let sway = FoliageSway {
            stiffness: 0.08,
            height_m: 3.0,
            flutter_m: 0.05,
            gust_period_s: 3.0,
            max_wind_m_per_s: 25.0,
            ..FoliageSway::default()
        };
        let bound = sway.max_offset();
        let mut worst = 0.0f32;
        for i in 0..400 {
            let t = i as f32 * 0.137;
            let wind = [(i as f32 * 0.7).sin() * 60.0, (i as f32 * 0.3).cos() * 60.0];
            for y in [0.0, 1.0, 2.5, 3.0, 9.0] {
                let p = [i as f32 * 0.31, y, -(i as f32) * 0.17];
                let o = sway.offset(p, [0.6, 0.0, 0.8], 1.0, [p[0], 0.0, p[2]], t, wind);
                worst = worst.max((o[0] * o[0] + o[1] * o[1] + o[2] * o[2]).sqrt());
            }
        }
        assert!(worst <= bound + 1.0e-5, "worst {worst} > bound {bound}");
        assert!(
            worst > 0.5 * bound,
            "the bound is far too loose: {worst} of {bound}"
        );
    }

    #[test]
    fn a_dividing_gust_period_loops_across_the_clock_wrap() {
        let sway = FoliageSway::default();
        let at = |t: f32| sway.offset([0.4, 1.6, 0.2], [0.0, 0.0, 1.0], 1.0, [0.0; 3], t, [6.0, 2.0]);
        // 3600 s is a whole number of 4 s laps: the frame after the wrap continues the motion.
        let before = at(3599.98);
        let after = at(-0.02);
        for axis in 0..3 {
            assert!(
                (before[axis] - after[axis]).abs() < 2.0e-3,
                "{before:?} vs {after:?}"
            );
        }
    }

    /// The natural style evaluates its gust field on one lap of the material clock, so the wrap
    /// is seamless whatever the period, the height factor and the flutter band.
    #[test]
    fn natural_wind_loops_across_the_clock_wrap() {
        let sway = natural();
        let at = |t: f32| sway.offset([0.4, 1.6, 0.2], [0.0, 0.0, 1.0], 1.0, [0.0; 3], t, [6.0, 2.0]);
        let before = at(3599.98);
        let after = at(-0.02);
        for axis in 0..3 {
            assert!(
                (before[axis] - after[axis]).abs() < 2.0e-3,
                "axis {axis}: {before:?} vs {after:?}"
            );
        }
    }

    #[test]
    fn a_non_finite_material_time_does_not_move_the_plant() {
        for sway in [FoliageSway::default(), natural()] {
            for time in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                assert_eq!(
                    sway.offset([0.4, 1.6, 0.2], [0.0, 0.0, 1.0], 1.0, [0.0; 3], time, [6.0, 2.0]),
                    [0.0; 3],
                    "{time} {sway:?}"
                );
            }
        }
    }

    #[test]
    fn the_same_inputs_give_the_same_offset() {
        let sway = FoliageSway::default();
        let a = sway.offset(
            [3.0, 1.0, 4.0],
            [0.0, 1.0, 0.0],
            0.5,
            [3.0, 0.0, 4.0],
            12.5,
            [5.0, -1.0],
        );
        let b = sway.offset(
            [3.0, 1.0, 4.0],
            [0.0, 1.0, 0.0],
            0.5,
            [3.0, 0.0, 4.0],
            12.5,
            [5.0, -1.0],
        );
        assert_eq!(a.map(f32::to_bits), b.map(f32::to_bits));
    }

    #[test]
    fn neighbours_gust_out_of_step() {
        let sway = FoliageSway {
            flutter_m: 0.0,
            ..FoliageSway::default()
        };
        let top = |x: f32| {
            sway.offset(
                [x, 2.0, 0.0],
                [0.0, 1.0, 0.0],
                0.0,
                [x, 0.0, 0.0],
                1.0,
                [8.0, 0.0],
            )[0]
        };
        assert!((top(0.0) - top(3.0)).abs() > 1.0e-3);
    }

    #[test]
    fn apply_marks_the_graph_and_sizes_its_bound() {
        let sway = FoliageSway::default();
        let graph = sway.graph(SURFACE);
        assert!(graph.uses_time);
        assert!(graph.vertex_wgsl.is_some());
        assert_eq!(graph.params[FOLIAGE_SWAY_SLOT], [0.03, 2.0, 0.02, 4.0]);
        assert_eq!(graph.params[FOLIAGE_SWAY_LIMIT_SLOT][0], 20.0);
        assert!((graph.max_vertex_offset - sway.max_offset()).abs() < 1.0e-6);
        // The same sway on another surface shares nothing but the stage; the key changes.
        assert_ne!(graph.key, FoliageSway::default().graph("fn x() {}").key);
    }

    fn natural() -> FoliageSway {
        FoliageSway {
            style: WindStyle::Natural,
            ..FoliageSway::default()
        }
    }

    #[test]
    fn natural_style_rests_in_calm_spells_and_stays_in_bounds() {
        let sway = natural();
        let bound = sway.max_offset();
        let (mut still, mut total, mut worst) = (0, 0, 0.0f32);
        for p in 0..8 {
            let origin = [p as f32 * 41.0 - 150.0, 0.0, p as f32 * -29.0];
            for i in 0..1500 {
                let o = sway.offset(
                    [origin[0], 1.5, origin[2]],
                    [0.0, 1.0, 0.0],
                    1.0,
                    origin,
                    i as f32 * 0.6,
                    [6.0, 2.0],
                );
                total += 1;
                still += usize::from(o == [0.0; 3]);
                worst = worst.max((o[0] * o[0] + o[1] * o[1] + o[2] * o[2]).sqrt());
            }
        }
        let calm = still as f32 / total as f32;
        assert!((0.1..0.6).contains(&calm), "calm fraction {calm}");
        assert!(worst > 0.0 && worst <= bound + 1.0e-5, "{worst} vs {bound}");
    }

    #[test]
    fn natural_style_is_off_by_default_and_the_classic_mirror_is_unchanged() {
        assert_eq!(FoliageSway::default().style, WindStyle::Classic);
        let classic = FoliageSway::default();
        let a = classic.offset([1.0, 1.0, 1.0], [0.0, 1.0, 0.0], 1.0, [0.0; 3], 0.7, [5.0, 0.0]);
        assert_ne!(a, [0.0; 3]);
        let b = natural().offset([1.0, 1.0, 1.0], [0.0, 1.0, 0.0], 1.0, [0.0; 3], 0.7, [5.0, 0.0]);
        assert_ne!(a, b);
        let mut graph = classic.graph("fn x() {}");
        assert_eq!(graph.params[FOLIAGE_SWAY_LIMIT_SLOT][1], 0.0);
        natural().apply(&mut graph);
        assert_eq!(graph.params[FOLIAGE_SWAY_LIMIT_SLOT][1], 1.0);
    }

    #[test]
    fn bad_numbers_are_sanitized() {
        let s = FoliageSway {
            stiffness: f32::NAN,
            height_m: -1.0,
            flutter_m: f32::INFINITY,
            gust_period_s: 0.0,
            max_wind_m_per_s: 1.0e9,
            ..FoliageSway::default()
        }
        .sanitized();
        assert_eq!(s.stiffness, 0.0);
        assert_eq!(s.height_m, 0.0);
        assert_eq!(s.flutter_m, 0.0);
        assert_eq!(s.gust_period_s, 0.1);
        assert_eq!(s.max_wind_m_per_s, FOLIAGE_WIND_MAX_M_PER_S);
        assert_eq!(sanitize_wind([f32::NAN, 3.0]), [0.0, 3.0]);
        let fast = sanitize_wind([300.0, 400.0]);
        assert!((fast[0] - 24.0).abs() < 1.0e-4 && (fast[1] - 32.0).abs() < 1.0e-4);
    }

    #[test]
    fn tiers_resolve_as_documented() {
        use crate::profile::{RenderProfile, RenderProfilePreset};
        let q = |p| FrameQuality::from_profile(&RenderProfile::from_preset(p));
        let high = DynamicCasterShadows::Tier.resolve(&q(RenderProfilePreset::DesktopHigh));
        assert_eq!(high.cascade_mask, 0b11);
        assert!(high.local_lights && !high.unpaged);
        let ultra = DynamicCasterShadows::Tier.resolve(&q(RenderProfilePreset::DesktopUltra));
        assert_eq!(ultra.cascade_mask, 0b11);
        // Spec 88 § 2.3's table names the offline Cinematic preset with the four-cascade tiers.
        let cinematic = DynamicCasterShadows::Tier.resolve(&q(RenderProfilePreset::Cinematic));
        assert_eq!(cinematic.cascade_mask, 0b11);
        assert!(!cinematic.unpaged);
        for p in [RenderProfilePreset::DesktopLow, RenderProfilePreset::Deck] {
            let policy = DynamicCasterShadows::Tier.resolve(&q(p));
            assert_eq!(policy.cascade_mask, 0b1, "{p:?}");
            assert!(!policy.local_lights, "{p:?}");
        }
        for p in [RenderProfilePreset::MobileLow, RenderProfilePreset::MobileHigh] {
            assert_eq!(
                DynamicCasterShadows::Tier.resolve(&q(p)),
                DynamicCasterPolicy::UNPAGED,
                "{p:?}"
            );
            // An unpaged frame redraws everything whatever the switch says.
            assert_eq!(
                DynamicCasterShadows::Off.resolve(&q(p)),
                DynamicCasterPolicy::UNPAGED,
                "{p:?}"
            );
        }
        let high_q = q(RenderProfilePreset::DesktopHigh);
        assert_eq!(
            DynamicCasterShadows::Off.resolve(&high_q),
            DynamicCasterPolicy::OFF
        );
        let on = DynamicCasterShadows::On {
            cascades: 9,
            local_lights: false,
        }
        .resolve(&high_q);
        assert_eq!(on.cascade_mask, 0b1111);
        assert_eq!(on.cascade_count(), 4);
        assert_eq!(DynamicCasterPolicy::cascades(0, false).cascade_count(), 0);
        assert!(!DynamicCasterPolicy::OFF.refreshes());
    }
}
