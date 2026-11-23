// SPDX-License-Identifier: MPL-2.0
//! Dynamic GI quality tiers (`specs/79-dynamic-gi.md`, ADR-1679; `specs/27` § 4.2 stage 2a).
//!
//! The GPU irradiance probes (`crate::ddgi`, ADR-132) are traced against the exact terrain
//! heightfield and per-mesh signed-distance bricks, and every shading lookup weighs its probes
//! by their distance moments (Chebyshev visibility), so light does not pass through closed
//! walls. This module sizes that field per quality tier: how many cascades and probes, how
//! often a settled probe re-traces, how many mesh bricks each cascade traces and how large the
//! brick pool is. A tier writes only those fields of [`DdgiSettings`]; biases, hysteresis,
//! emitters and the diagnostics stay what the host set.
//!
//! A renderer starts at [`DynamicGiTier::High`]. Since ADR-5001 High and Ultra space their
//! finest cascade 1 m apart (each coarser one × 3.8, so the reach stays about 550 m) and turn on
//! ADR-2605's seeding, visibility blend and running-average weighting: 2 m probes could not hold
//! a 3–6 m room. [`DdgiSettings::default`] keeps the layout High had before (2 m, × 3, no
//! options) for hosts that build their own settings on it. [`DynamicGiTier::Low`] is the Deck
//! and Desktop Low budget, [`DynamicGiTier::Medium`] sits between, and [`DynamicGiTier::Ultra`]
//! keeps High's probe grid with twice High's mesh bricks (128) and twice its brick pool (64 MB).
//! That is four times Low's 32-brick cascade.
//!
//! Whether the probes run at all is the lighting option (`crate::GiLook`, the runtime switch);
//! the tier only decides what they cost when they run.

use crate::ddgi::{DdgiState, RAYS_PER_PROBE, finest_window_m};
use crate::{DdgiSettings, RenderProfilePreset};

/// How large and how fast the GPU irradiance probe field is (`specs/79` § 2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DynamicGiTier {
    /// Steam Deck and Desktop Low: four cascades of 12 × 8 × 12 probes, a settled probe
    /// re-traces every 16 frames, 32 bricks per cascade, a 16 MB brick pool. Its 2 m probes straddle a
    /// house wall, so a probe past the edge of the point's surface has to see the point without doubt
    /// ([`DdgiSettings::edge_visibility`], ADR-5566).
    Low,
    /// Four cascades of 16 × 8 × 16 probes, every 8 frames, 48 bricks per cascade, 24 MB. Low's 2 m
    /// probes, and its edge visibility with them (ADR-5716).
    Medium,
    /// Desktop High: four cascades of 20 × 12 × 20 probes, the finest 1 m apart (ADR-5001) and
    /// holding 3.5 m over the eye (ADR-5290), every 8 frames, 64 bricks per cascade, 32 MB.
    #[default]
    High,
    /// Desktop Ultra: High's probe grid, 128 bricks per cascade and a 64 MB pool, so a dense
    /// scene keeps more of its meshes in the probes' rays.
    Ultra,
}

/// The seed weight, in updates, of the Low and Medium tiers ([`DdgiSettings::seed_updates`]).
pub const LOW_SEED_UPDATES: u32 = 4;
/// The running-average weighting of the Low and Medium tiers ([`DdgiSettings::recent_weight`]):
/// 60 frames after a camera stopped from 0.5 m a frame, Low read 1.985 luma levels from the held view
/// with 1 and 1.732 with 3 (ADR-2605).
pub const LOW_RECENT_WEIGHT: u32 = 3;

/// The finest probe spacing of the High and Ultra tiers, metres (ADR-5001). With 2 m probes a 3 m
/// tunnel read 0.75 and a 6 × 3 × 6 m room 1.5 times the path tracer's indirect light once the
/// probes' sun shadow ray stopped shadowing its own floor; 1 m read 0.83 and 1.28.
pub const HIGH_SPACING_M: f32 = 1.0;
/// How much wider each coarser High and Ultra cascade is than the last (ADR-5001): 3.8 keeps the
/// coarsest at 54.9 m, the reach the 2 m × 3 layout had.
pub const HIGH_SPACING_RATIO: f32 = 3.8;
/// Probe layers of the High and Ultra cascades (ADR-5290). Ten 1 m layers held 2 m over the eye and
/// 2.5 m under it, so a 6 m room's ceiling (4.4 m over a standing eye) lay past the finest cascade's
/// blend window and the 3.8 m cascade lit it alone: 2.16 % of the sunlit ground. Twelve hold
/// [`HIGH_FINEST_HEADROOM_M`] over the eye and 3 m under it: the ceiling reads 0.0002 %.
pub const HIGH_LAYERS: u32 = 12;
/// How far over the eye the High and Ultra finest cascade is placed to hold at full weight, metres
/// ([`DdgiSettings::finest_headroom_m`], ADR-5290; the lookup holds half a cell more, ADR-5909:
/// [`DynamicGiBudget::finest_above_m`]). Low and Medium take `LOW_FINEST_HEADROOM_M`.
pub const HIGH_FINEST_HEADROOM_M: f32 = 3.5;
/// How far over the eye the Low and Medium finest cascade is placed to hold at full weight, metres
/// ([`DdgiSettings::finest_headroom_m`], ADR-5909): a metre more than [`crate::FINEST_HEADROOM_M`]. With the
/// finest cascade's quarter-cell blend margin the lookup holds half a cell (a metre) more at each end of the
/// placement's window; placed for 2 m over the eye that was 3 m over it and 4 m under it. A view up a 3 m shaft
/// spans 6 m over the eye and read the 6 m cascade's two probes above the window (0.117 against the path
/// tracer). Placed for 3 m the lookup holds 4 m over the eye and the 3 m under it that it held before
/// ADR-5909: the shaft reads 0.102, and the trench, the color room, the door room and the tunnel read what
/// they read (`gi_acceptance` `REGIA_GI_X="headroom=3.0"`; 4 m of headroom reads the shaft 0.100).
const LOW_FINEST_HEADROOM_M: f32 = 3.0;

/// Bytes of the probe field's buffers that scale with the tier (`specs/79` § 7).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DynamicGiBudget {
    /// Probe slots in all cascades.
    pub probes: u64,
    /// Rays a settled field traces per frame on average when every probe is active: a probe
    /// of cascade `c` casts `RAYS_PER_PROBE` rays every `update_divisor << min(c, 3)` frames
    /// (coarser cascades re-trace half as often per level); a new probe traces every frame.
    pub rays_per_frame: u64,
    /// Probe records, irradiance and distance tiles, ray results and the update queue, bytes.
    /// The half-resolution screen lookup region is not counted. Its size is per tier; see
    /// [`Self::screen_bytes`].
    pub probe_bytes: u64,
    /// Signed-distance brick pool, bytes.
    pub sdf_pool_bytes: u64,
    /// Horizontal half extent of the coarsest cascade, metres: no probe light beyond it.
    pub reach_m: f32,
    /// How far above the eye the finest cascade holds at full weight, metres. A surface higher
    /// than this reads a coarser cascade, whose spacing may not resolve a thin roof.
    pub finest_above_m: f32,
    /// The same below the eye, metres.
    pub finest_below_m: f32,
    /// Whether this layout runs the room rule (`specs/79` § 9.1): a cascade is too coarse to
    /// resolve a room, so the finest one holds the room around the eye and a ray hit keeps to its
    /// own cascade. True on every tier; a host layout of fine cascades does not need it.
    pub room_rule: bool,
    /// Bytes of the screen lookup region ([`DdgiSettings::screen_max`]), which `probe_bytes` does
    /// not count: 66.4 MB on High and Ultra, 16.6 MB on Low, 29.5 MB on Medium.
    pub screen_bytes: u64,
    /// Most young probes traced in one frame ([`DdgiSettings::young_budget`], ADR-7870); 0 = every young
    /// probe every frame (every tier).
    pub young_budget: u32,
    /// Frames after which a field that restarted (a start-up, a light cut) has taken every probe's sixteen
    /// young updates, at most ([`DdgiSettings::young_settle_frames`]): 16 without a budget.
    pub young_settle_frames: u32,
}

impl DynamicGiTier {
    /// Every tier, cheapest first.
    pub const ALL: [Self; 4] = [Self::Low, Self::Medium, Self::High, Self::Ultra];

    /// The protocol and settings name: `low`, `medium`, `high` or `ultra`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Ultra => "ultra",
        }
    }

    /// The tier named `name` (see [`Self::name`]), ASCII case-insensitive.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|t| t.name().eq_ignore_ascii_case(name))
    }

    /// The tier a preset ships with. Mobile presets get [`Self::Low`]: Android never runs the
    /// probes (`crate::GiLook::depth_available`), so the value only matters if that changes.
    #[must_use]
    pub const fn of_preset(preset: RenderProfilePreset) -> Self {
        match preset {
            RenderProfilePreset::MobileLow
            | RenderProfilePreset::MobileHigh
            | RenderProfilePreset::DesktopLow
            | RenderProfilePreset::Deck => Self::Low,
            RenderProfilePreset::DesktopHigh => Self::High,
            RenderProfilePreset::DesktopUltra | RenderProfilePreset::Cinematic => Self::Ultra,
        }
    }

    /// Writes this tier's fields into `settings` (cascades, probes per axis, spacing and its
    /// growth, update divisor, bricks per cascade, brick pool, the probe seeding, the visibility
    /// blend, the screen lookup region, the running-average weighting, the finest cascade's
    /// headroom, the front weights, which every tier turns on, and the edge visibility, which Low
    /// and Medium turn on, ADR-5566 and ADR-5716) and leaves every other field.
    ///
    /// Every tier seeds probes that start over and keeps a point on the finest cascade where a
    /// coarser one cannot see it (ADR-2605). Low and Medium space their finest cascade 2 m apart
    /// (× 3); High and Ultra 1 m (× 3.8), so a small room holds finest probes (ADR-5001; an owner
    /// look decision at the GI review, `specs/79` § 9.2).
    pub fn apply(self, settings: &mut DdgiSettings) {
        let (cascades, dims, update_divisor, bricks, pool_mb) = match self {
            Self::Low => (4, [12, 8, 12], 16, 32, 16),
            Self::Medium => (4, [16, 8, 16], 8, 48, 24),
            Self::High => (4, [20, HIGH_LAYERS, 20], 8, 64, 32),
            Self::Ultra => (4, [20, HIGH_LAYERS, 20], 8, 128, 64),
        };
        let (spacing, spacing_ratio, screen_max, headroom) = match self {
            // A 1920 × 1080 frame at half resolution (the Deck's 1280 × 800 needs 640 × 400).
            Self::Low => (2.0, 3.0, [960, 540], LOW_FINEST_HEADROOM_M),
            // 2560 × 1440 at half resolution.
            Self::Medium => (2.0, 3.0, [1280, 720], LOW_FINEST_HEADROOM_M),
            // 1, 3.8, 14.4 and 54.9 m: the reach of 2, 6, 18 and 54 m, with finest probes that fit
            // a 3 m room (ADR-5001) and reach a 6 m room's ceiling (ADR-5290).
            Self::High | Self::Ultra => (
                HIGH_SPACING_M,
                HIGH_SPACING_RATIO,
                crate::ddgi::SCREEN_LOOKUP_MAX,
                HIGH_FINEST_HEADROOM_M,
            ),
        };
        let (seed_updates, visibility_blend, recent_weight) = (LOW_SEED_UPDATES, true, LOW_RECENT_WEIGHT);
        settings.cascades = cascades;
        settings.dims = dims;
        settings.spacing = spacing;
        settings.spacing_ratio = spacing_ratio;
        settings.update_divisor = update_divisor;
        settings.max_instances_per_cascade = bricks;
        settings.sdf_pool_bytes = pool_mb << 20;
        settings.seed_updates = seed_updates;
        settings.visibility_blend = visibility_blend;
        settings.screen_max = screen_max;
        settings.recent_weight = recent_weight;
        settings.finest_headroom_m = headroom;
        settings.front_weights = true;
        settings.edge_visibility = matches!(self, Self::Low | Self::Medium);
    }

    /// [`DdgiSettings::default`] with this tier applied.
    #[must_use]
    pub fn settings(self) -> DdgiSettings {
        let mut s = DdgiSettings::default();
        self.apply(&mut s);
        s
    }

    /// Whether `settings` holds exactly this tier's fields (a host that called
    /// `FrameRenderer::set_ddgi_settings` with its own sizes no longer does).
    #[must_use]
    pub fn matches(self, settings: &DdgiSettings) -> bool {
        let mut tiered = settings.clone();
        self.apply(&mut tiered);
        tiered == *settings
    }

    /// What this tier's probe field holds and traces.
    #[must_use]
    pub fn budget(self) -> DynamicGiBudget {
        budget_of(&self.settings())
    }
}

/// The budget of any probe settings (a tier's, or a host's own).
#[must_use]
pub fn budget_of(settings: &DdgiSettings) -> DynamicGiBudget {
    let s = settings.clone().clamped();
    let probes = s.probe_count();
    // Words before the screen lookup region: the probe records and their packed copy.
    let (_, _, _, _, words_before_screen, _) = DdgiState::data_words(&s);
    let rays = u64::from(RAYS_PER_PROBE);
    let coarsest = s.spacing_of(s.cascades.saturating_sub(1));
    let (finest_above_m, finest_below_m) = finest_window_m(&s);
    DynamicGiBudget {
        probes,
        rays_per_frame: settled_rays_per_frame(&s),
        probe_bytes: words_before_screen * 4 + probes * rays * 16 + probes * 4,
        sdf_pool_bytes: s.sdf_pool_values() * 4,
        reach_m: 0.5 * coarsest * s.dims[0].max(s.dims[2]) as f32,
        finest_above_m,
        finest_below_m,
        room_rule: s.needs_room_rule(),
        screen_bytes: s.screen_bytes(),
        young_budget: s.young_budget,
        young_settle_frames: s.young_settle_frames(),
    }
}

/// Rays a settled, active field of `s` (already clamped) traces per frame on average, as
/// `ddgi_trace.wgsl` `classify` queues them: cascade `c` re-traces every
/// `update_divisor << min(c, 3)` frames (the divisor clamped like `ctrl.z`). Counted over one
/// period of the slowest cascade, so the per-cascade shares add without rounding each.
fn settled_rays_per_frame(s: &DdgiSettings) -> u64 {
    let cascades = s.cascades.max(1);
    let per_cascade = s.probe_count() / u64::from(cascades);
    let divisor = u64::from(s.update_divisor.clamp(1, crate::ddgi::DDGI_MAX_UPDATE_DIVISOR));
    let period = divisor << (cascades - 1).min(3);
    let traces: u64 = (0..cascades)
        .map(|c| per_cascade * (period / (divisor << c.min(3))))
        .sum();
    traces * u64::from(RAYS_PER_PROBE) / period
}

/// What `FrameRenderer::dynamic_gi_status` reports (agent `render.get_dynamic_gi`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DynamicGiStatus {
    /// The tier last applied ([`DynamicGiTier::High`] until one is set).
    pub tier: DynamicGiTier,
    /// The probe settings no longer match the tier: the host set its own sizes since.
    pub custom: bool,
    /// The probes ran on the last frame.
    pub active: bool,
    /// The host's on/off override (`FrameRenderer::set_ddgi_enabled`); `None` follows the
    /// lighting option.
    pub enabled_override: Option<bool>,
    /// The budget of the settings in use.
    pub budget: DynamicGiBudget,
    /// Frames the traced scene has been unchanged; `u32::MAX` when the probes are off.
    pub settled_frames: u32,
    /// Mesh bricks still baking (those meshes do not occlude probe rays yet).
    pub pending_bakes: usize,
    /// Emitters in the probes' emitter list on the last frame.
    pub emitters: usize,
    /// GPU time of the probe update plus the screen lookup on the last timed frame, ms (0
    /// without GPU timing, `FrameRenderer::set_gpu_timing`).
    pub gpu_ms: f32,
    /// Its stages: trace, relocate, irradiance blend, distance blend, screen lookup (ms).
    pub gpu_stages_ms: [f32; 5],
    /// The heightfield max pyramid's rebuilds (ADR-7659).
    pub pyramid: DdgiPyramidStats,
}

/// Counters of the probes' heightfield max pyramid (ADR-7659): a new heights generation
/// re-reduces only the texels its changed samples reach, or the whole pyramid on a whole-map
/// change. Counted when the probe update records the reduction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DdgiPyramidStats {
    /// Texels of the whole pyramid (every level) for the current map; 0 without a heightfield.
    pub full_texels: u64,
    /// Whole-pyramid rebuilds so far (first build, a new map or epoch, a whole-map change, a
    /// missed generation step).
    pub full_rebuilds: u64,
    /// Regional rebuilds so far (only the edited rectangles' texels at every level).
    pub regional_rebuilds: u64,
    /// Texels re-reduced so far, every level, both kinds.
    pub texels_reduced: u64,
    /// Texels the last rebuild re-reduced.
    pub last_texels: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-5001: High is the probes' defaults with the finer layout, ADR-2605's options and the
    /// front weights every tier takes (ADR-5716); the defaults themselves keep the layout High had
    /// before.
    #[test]
    fn high_is_the_probes_defaults_with_a_finer_finest_cascade() {
        let high = DynamicGiTier::High.settings();
        let expect = DdgiSettings {
            dims: [20, HIGH_LAYERS, 20],
            spacing: HIGH_SPACING_M,
            spacing_ratio: HIGH_SPACING_RATIO,
            finest_headroom_m: HIGH_FINEST_HEADROOM_M,
            seed_updates: LOW_SEED_UPDATES,
            visibility_blend: true,
            recent_weight: LOW_RECENT_WEIGHT,
            front_weights: true,
            ..DdgiSettings::default()
        };
        assert_eq!(high, expect);
        assert!(!DynamicGiTier::High.matches(&DdgiSettings::default()));
        assert_eq!(DynamicGiTier::default(), DynamicGiTier::High);
        let legacy = DdgiSettings::default();
        assert!(
            (legacy.spacing - 2.0).abs() < f32::EPSILON && (legacy.spacing_ratio - 3.0).abs() < f32::EPSILON
        );
    }

    #[test]
    fn a_tier_writes_only_its_own_fields() {
        let mut host = DdgiSettings {
            hysteresis: 0.9,
            normal_bias: 0.25,
            emitters: false,
            bake_on_caller: true,
            ..DdgiSettings::default()
        };
        DynamicGiTier::Low.apply(&mut host);
        assert_eq!(host.dims, [12, 8, 12]);
        assert_eq!(host.update_divisor, 16);
        assert!((host.hysteresis - 0.9).abs() < f32::EPSILON);
        assert!((host.normal_bias - 0.25).abs() < f32::EPSILON);
        assert!(!host.emitters);
        assert!(host.bake_on_caller);
        assert!(DynamicGiTier::Low.matches(&host));
        assert!(!DynamicGiTier::High.matches(&host));
    }

    #[test]
    fn tiers_cost_less_going_down_and_reach_far_enough() {
        let b = DynamicGiTier::ALL.map(DynamicGiTier::budget);
        for pair in b.windows(2) {
            assert!(pair[0].rays_per_frame <= pair[1].rays_per_frame, "{pair:?}");
            assert!(pair[0].probe_bytes <= pair[1].probe_bytes, "{pair:?}");
            assert!(pair[0].sdf_pool_bytes <= pair[1].sdf_pool_bytes, "{pair:?}");
        }
        // The Deck traces at most a quarter of High's rays and holds at most half its probe memory.
        assert!(
            b[0].rays_per_frame * 4 <= b[2].rays_per_frame,
            "{:?} vs {:?}",
            b[0],
            b[2]
        );
        assert!(
            b[0].probe_bytes * 2 <= b[2].probe_bytes,
            "{:?} vs {:?}",
            b[0],
            b[2]
        );
        // Every tier lights at least a 250 m radius around the eye.
        assert!(b.iter().all(|x| x.reach_m >= 250.0), "{b:?}");
        // Every tier fits the per-frame brick instance list (MAX_INSTANCES over all cascades).
        for t in DynamicGiTier::ALL {
            let s = t.settings();
            assert!(
                u64::from(s.max_instances_per_cascade) * u64::from(s.cascades)
                    <= crate::ddgi::MAX_INSTANCES as u64,
                "{t:?}"
            );
            assert_eq!(s.clone().clamped(), s, "{t:?} is inside the clamp");
        }
    }

    #[test]
    fn ultra_bricks_and_pool_are_twice_high() {
        let high = DynamicGiTier::High.settings();
        let ultra = DynamicGiTier::Ultra.settings();
        assert_eq!(high.max_instances_per_cascade, 64);
        assert_eq!(ultra.max_instances_per_cascade, 128);
        assert_eq!(
            ultra.max_instances_per_cascade,
            high.max_instances_per_cascade * 2
        );
        assert_eq!(high.sdf_pool_bytes, 32 << 20);
        assert_eq!(ultra.sdf_pool_bytes, 64 << 20);
        assert_eq!(ultra.sdf_pool_bytes, high.sdf_pool_bytes * 2);
        let low = DynamicGiTier::Low.budget().screen_bytes;
        let medium = DynamicGiTier::Medium.budget().screen_bytes;
        let high_screen = DynamicGiTier::High.budget().screen_bytes;
        let ultra_screen = DynamicGiTier::Ultra.budget().screen_bytes;
        assert!(low < medium && medium < high_screen);
        assert_eq!(high_screen, ultra_screen);
    }

    #[test]
    fn the_budget_numbers_are_the_ones_the_spec_quotes() {
        // specs/79 § 7 quotes these; a change here is a spec change.
        let low = DynamicGiTier::Low.budget();
        let high = DynamicGiTier::High.budget();
        assert_eq!(low.probes, 4_608);
        assert_eq!(high.probes, 19_200);
        // Settled and active: cascade c re-traces every `update_divisor << min(c, 3)` frames
        // (`ddgi_trace.wgsl` `classify`; audit render10 #3). Low: 1 152 probes per cascade
        // at divisors 16, 32, 64, 128. High and Ultra: 4 800 per cascade at 8, 16, 32, 64.
        assert_eq!(low.rays_per_frame, 8_640);
        assert_eq!(DynamicGiTier::Medium.budget().rays_per_frame, 30_720);
        assert_eq!(high.rays_per_frame, 72_000);
        assert_eq!(DynamicGiTier::Ultra.budget().rays_per_frame, 72_000);
        // 526 words of probe record and tiles, 64 rays × 16 bytes, one queue word.
        assert_eq!(low.probe_bytes, 4_608 * 3_132);
        assert_eq!(high.probe_bytes, 19_200 * 3_132);
        assert!((low.reach_m - 324.0).abs() < 1e-3, "{low:?}");
        assert!((high.reach_m - 548.72).abs() < 1e-2, "{high:?}");
        // The finest cascade's full weight around the eye, `specs/79` § 7.
        // The window the volumes are placed for (3 m over and 2 m under the eye on Low, ADR-5909; 3.5 m
        // and 3 m on High) plus the half cell the lookup holds past it at each end since ADR-5909.
        assert!((low.finest_above_m - 4.0).abs() < 1e-3, "{low:?}");
        assert!((low.finest_below_m - 3.0).abs() < 1e-3, "{low:?}");
        assert!((high.finest_above_m - 4.0).abs() < 1e-3, "{high:?}");
        assert!((high.finest_below_m - 3.5).abs() < 1e-3, "{high:?}");
        assert!(low.room_rule && high.room_rule);
        let fine = DdgiSettings {
            cascades: 2,
            spacing: 1.0,
            spacing_ratio: 3.0,
            ..DdgiSettings::default()
        };
        assert!(!budget_of(&fine).room_rule, "1 and 3 m cascades resolve a room");
    }

    /// `rays_per_frame` is what `ddgi_trace.wgsl` `classify` queues for a settled, active field:
    /// probe `p` of cascade `c` traces on frames where `(p + frame) % (divisor << min(c, 3)) == 0`.
    /// Counted over one full period of the slowest cascade, for every tier and a host's own
    /// settings (more than four cascades, a divisor of 1, one past the shader's clamp).
    #[test]
    fn rays_per_frame_matches_the_classify_cadence() {
        let mut settings: Vec<DdgiSettings> = DynamicGiTier::ALL.iter().map(|t| t.settings()).collect();
        settings.push(DdgiSettings {
            cascades: 6,
            update_divisor: 1,
            ..DdgiSettings::default()
        });
        settings.push(DdgiSettings {
            cascades: 1,
            update_divisor: crate::ddgi::DDGI_MAX_UPDATE_DIVISOR + 5,
            ..DdgiSettings::default()
        });
        for s in settings {
            let c = s.clone().clamped();
            let per_cascade = c.probe_count() / u64::from(c.cascades.max(1));
            let divisor = c.update_divisor.clamp(1, crate::ddgi::DDGI_MAX_UPDATE_DIVISOR);
            let period = u64::from(divisor) << c.cascades.saturating_sub(1).min(3);
            let mut traced = 0u64;
            for frame in 0..period {
                for probe in 0..c.probe_count() {
                    let cascade = (probe / per_cascade) as u32;
                    let d = u64::from(divisor << cascade.min(3));
                    if (probe + frame).is_multiple_of(d) {
                        traced += 1;
                    }
                }
            }
            let average = traced * u64::from(RAYS_PER_PROBE) / period;
            assert_eq!(budget_of(&s).rays_per_frame, average, "{s:?}");
        }
    }

    /// ADR-2605: Low and Medium seed probes, blend by visibility and reserve a smaller lookup
    /// region; since ADR-5001 High and Ultra seed and blend too, on the default lookup region.
    #[test]
    fn low_and_medium_seed_and_blend_by_visibility_on_their_own_lookup_region() {
        for (tier, region) in [
            (DynamicGiTier::Low, [960, 540]),
            (DynamicGiTier::Medium, [1280, 720]),
        ] {
            let s = tier.settings();
            assert_eq!(s.seed_updates, LOW_SEED_UPDATES, "{tier:?}");
            assert_eq!(s.recent_weight, LOW_RECENT_WEIGHT, "{tier:?}");
            assert!(s.visibility_blend, "{tier:?}");
            assert_eq!(s.screen_max, region, "{tier:?}");
            assert_eq!(
                tier.budget().screen_bytes,
                u64::from(region[0]) * u64::from(region[1]) * 32
            );
        }
        for tier in [DynamicGiTier::High, DynamicGiTier::Ultra] {
            let s = tier.settings();
            assert_eq!(s.seed_updates, LOW_SEED_UPDATES);
            assert_eq!(s.recent_weight, LOW_RECENT_WEIGHT);
            assert!(s.visibility_blend);
            assert_eq!(s.screen_max, crate::ddgi::SCREEN_LOOKUP_MAX);
        }
        // The Deck's region is a quarter of the default's.
        assert_eq!(
            DynamicGiTier::Low.budget().screen_bytes * 4,
            DynamicGiTier::High.budget().screen_bytes
        );
        // A host's own seed weight is replaced by the tier's, like its sizes.
        let mut host = DdgiSettings {
            seed_updates: 9,
            ..DdgiSettings::default()
        };
        DynamicGiTier::High.apply(&mut host);
        assert_eq!(host, DynamicGiTier::High.settings());
    }

    /// ADR-5566, ADR-5716: every tier weighs its probes by which side of the point's surface they stand
    /// on; the 2 m tiers (Low, Medium) also take the edge visibility. A switch to another tier writes both
    /// fields, like every tier field.
    #[test]
    fn every_tier_takes_the_front_weights_and_the_2_m_tiers_the_edge_visibility() {
        for tier in DynamicGiTier::ALL {
            assert!(tier.settings().front_weights, "{tier:?}");
            assert_eq!(
                tier.settings().edge_visibility,
                matches!(tier, DynamicGiTier::Low | DynamicGiTier::Medium),
                "{tier:?}"
            );
        }
        assert!(!DdgiSettings::default().front_weights);
        assert!(!DdgiSettings::default().edge_visibility);
        let mut host = DynamicGiTier::Low.settings();
        assert!(host.front_weights && host.edge_visibility);
        DynamicGiTier::High.apply(&mut host);
        assert_eq!(host, DynamicGiTier::High.settings());
        assert!(host.front_weights && !host.edge_visibility);
        let mut host = DdgiSettings {
            edge_visibility: true,
            ..DdgiSettings::default()
        };
        assert!(!DynamicGiTier::High.matches(&host));
        DynamicGiTier::High.apply(&mut host);
        assert!(!host.edge_visibility);
        let mut host = DdgiSettings::default();
        assert!(!DynamicGiTier::Medium.matches(&host));
        DynamicGiTier::Medium.apply(&mut host);
        assert!(host.front_weights && host.edge_visibility);
        // The option costs no rays, probes or bytes: the tier's budget is what the spec quotes.
        assert_eq!(DynamicGiTier::Low.budget().rays_per_frame, 8_640);
        assert_eq!(DynamicGiTier::Low.budget().probes, 4_608);
    }

    #[test]
    fn names_round_trip_and_presets_map() {
        for t in DynamicGiTier::ALL {
            assert_eq!(DynamicGiTier::parse(t.name()), Some(t));
        }
        assert_eq!(DynamicGiTier::parse("ULTRA"), Some(DynamicGiTier::Ultra));
        assert_eq!(DynamicGiTier::parse("epic"), None);
        assert_eq!(
            DynamicGiTier::of_preset(RenderProfilePreset::Deck),
            DynamicGiTier::Low
        );
        assert_eq!(
            DynamicGiTier::of_preset(RenderProfilePreset::DesktopHigh),
            DynamicGiTier::High
        );
        assert_eq!(
            DynamicGiTier::of_preset(RenderProfilePreset::DesktopUltra),
            DynamicGiTier::Ultra
        );
    }

    /// ADR-7870: no tier writes the young-probe budget (every tier ships it off), a host's budget survives a tier
    /// change without making the settings custom, and the budget reports it with the frames a restart takes.
    #[test]
    fn the_young_budget_is_off_on_every_tier_and_a_host_budget_survives_a_tier() {
        for t in DynamicGiTier::ALL {
            assert_eq!(t.settings().young_budget, 0, "{}", t.name());
            let b = t.budget();
            assert_eq!((b.young_budget, b.young_settle_frames), (0, 16), "{}", t.name());
        }
        let mut host = DdgiSettings {
            young_budget: 4096,
            ..DdgiSettings::default()
        };
        DynamicGiTier::High.apply(&mut host);
        assert_eq!(host.young_budget, 4096);
        assert!(
            DynamicGiTier::High.matches(&host),
            "the budget is the host's, like the biases"
        );
        DynamicGiTier::Low.apply(&mut host);
        assert_eq!(host.young_budget, 4096);
        let b = budget_of(&host);
        assert_eq!(b.young_budget, 4096);
        // 4 608 probes: 16 · 4 608 / 4 096 = 18 frames at the budget, then at most 16 more.
        assert_eq!(b.young_settle_frames, 18 + 16);
        // What the budget meters is not what a settled frame costs.
        assert_eq!(b.rays_per_frame, DynamicGiTier::Low.budget().rays_per_frame);
        assert_eq!(b.probe_bytes, DynamicGiTier::Low.budget().probe_bytes);
    }
}
