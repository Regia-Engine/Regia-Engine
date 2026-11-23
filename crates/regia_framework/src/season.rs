// SPDX-License-Identifier: MPL-2.0
//! Seasons: one driver for foliage colour, leaf density and leaf fall, with a bare winter tree
//! (ADR-2449, `book/agents/season.md`).
//!
//! [`SeasonState`] is a resource holding the time of year as a fraction of the year. Four anchors
//! sit on it: spring `0.0`, summer `0.25`, autumn `0.5`, winter `0.75`; the year wraps. For each
//! [`FoliageKind`] a [`SeasonLook`] is interpolated between the anchors: a colour tint (multiplies
//! the material's base colour, so a green leaf turns yellow or orange), a leaf density (`1` full
//! crown, `0` bare) and the fraction of the leaves that have fallen.
//!
//! The renderer is not changed: [`SeasonalFoliage`] marks a mesh entity whose material the season
//! drives. The system keeps the entity's original material, and on a season change writes a
//! tinted copy into its [`MeshRenderer`] (shared between entities with the same original, so a
//! forest of one tree material allocates one copy per season change). Leaf density moves the
//! alpha of an alpha-masked material: the cards shrink as the density falls and vanish at zero,
//! in the main pass and, with alpha-tested shadows on, in the shadows. Opaque materials (trunks
//! and branches) only take the tint; blended materials fade.
//!
//! Hosts that scatter instances themselves use [`leaf_present`]: a hash of the instance against
//! the density, so which instances drop out is the same in every run and the set shrinks
//! monotonically as the density falls.
//!
//! Colour keyframes are art-directed values chosen for this engine (no external data used).

use std::collections::HashMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_render::{ALPHA_BLEND, ALPHA_MASK, CpuMaterial, MeshRenderer};

/// The four season anchors on the year, in order.
pub const SEASONS: [&str; 4] = ["spring", "summer", "autumn", "winter"];

/// Which kind of foliage a material is, since a pine does not turn orange.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[reflect(Default, Clone)]
pub enum FoliageKind {
    /// Deciduous leaves: green, then yellow and orange, then bare.
    #[default]
    Broadleaf,
    /// Needles: nearly the same all year.
    Evergreen,
    /// Grass and ground cover: green, then straw, then thin.
    Grass,
}

impl FoliageKind {
    /// The kind's wire name: `broadleaf`, `evergreen` or `grass`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Broadleaf => "broadleaf",
            Self::Evergreen => "evergreen",
            Self::Grass => "grass",
        }
    }

    /// The kind with wire name `name`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "broadleaf" => Some(Self::Broadleaf),
            "evergreen" => Some(Self::Evergreen),
            "grass" => Some(Self::Grass),
            _ => None,
        }
    }

    /// `(tint, density)` at the four anchors: spring, summer, autumn, winter.
    fn keys(self) -> [([f32; 3], f32); 4] {
        match self {
            Self::Broadleaf => [
                ([0.92, 1.12, 0.72], 0.8),
                ([1.0, 1.0, 1.0], 1.0),
                ([2.2, 0.95, 0.38], 0.6),
                ([1.5, 0.9, 0.5], 0.0),
            ],
            Self::Evergreen => [
                ([1.0, 1.06, 0.94], 1.0),
                ([1.0, 1.0, 1.0], 1.0),
                ([1.0, 0.98, 0.95], 1.0),
                ([0.88, 0.95, 1.02], 0.95),
            ],
            Self::Grass => [
                ([0.9, 1.12, 0.78], 0.9),
                ([1.0, 1.0, 1.0], 1.0),
                ([1.75, 1.1, 0.62], 0.8),
                ([1.45, 1.25, 1.0], 0.55),
            ],
        }
    }
}

/// What the season does to one kind of foliage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeasonLook {
    /// Multiplies the base colour (linear RGB).
    pub tint: [f32; 3],
    /// Leaf density `0..=1`: `1` a full crown, `0` bare.
    pub density: f32,
    /// Fraction of the leaves fallen: `1 - density`.
    pub leaf_fall: f32,
}

impl SeasonLook {
    /// Summer: no tint, full density.
    pub const NEUTRAL: Self = Self {
        tint: [1.0, 1.0, 1.0],
        density: 1.0,
        leaf_fall: 0.0,
    };

    /// Whether the look changes nothing.
    pub fn is_neutral(&self) -> bool {
        *self == Self::NEUTRAL
    }

    /// The look of `kind` at `year_fraction` (wraps; NaN reads as summer).
    pub fn at(kind: FoliageKind, year_fraction: f32) -> Self {
        if !year_fraction.is_finite() {
            return Self::NEUTRAL;
        }
        let t = year_fraction.rem_euclid(1.0) * 4.0;
        let i = (t.floor() as usize).min(3);
        let f = t - i as f32;
        // Ease inside each quarter so an anchor holds a little and the change is gentle.
        let f = f * f * (3.0 - 2.0 * f);
        let keys = kind.keys();
        let (a, da) = keys[i];
        let (b, db) = keys[(i + 1) % 4];
        let tint = [
            a[0] + (b[0] - a[0]) * f,
            a[1] + (b[1] - a[1]) * f,
            a[2] + (b[2] - a[2]) * f,
        ];
        let density = (da + (db - da) * f).clamp(0.0, 1.0);
        Self {
            tint,
            density,
            leaf_fall: 1.0 - density,
        }
    }

    /// `material` with this look applied: the base colour tinted; an alpha-masked material's
    /// alpha scaled so its cards shrink with the density and vanish at zero; a blended one faded.
    /// Opaque materials only take the tint. A neutral look returns the material unchanged.
    pub fn apply(&self, material: &CpuMaterial) -> CpuMaterial {
        let mut out = material.clone();
        if self.is_neutral() {
            return out;
        }
        for (c, t) in out.base_color[..3].iter_mut().zip(self.tint) {
            *c *= t;
        }
        match out.alpha_mode {
            ALPHA_MASK => {
                let cutoff = out.alpha_cutoff.clamp(0.0, 0.999);
                let multiplier = if self.density <= 0.0 {
                    0.0
                } else {
                    // At density 1 the card is whole; as it falls the multiplier approaches the
                    // cutoff, where only the card's core clears the test; at 0, nothing does.
                    cutoff + (1.0 - cutoff) * self.density
                };
                out.base_color[3] *= multiplier;
            }
            ALPHA_BLEND => out.base_color[3] *= self.density,
            _ => {}
        }
        out
    }
}

/// Whether the instance with hash `instance_hash` still has its leaves at `density`. The
/// instances that drop out are a fixed subset in every run, and shrink monotonically as the
/// density falls: an instance present at density `d` is present at every higher one.
pub fn leaf_present(instance_hash: u64, density: f32) -> bool {
    if !density.is_finite() || density <= 0.0 {
        return false;
    }
    // splitmix64 finalizer, then 24 bits to [0, 1).
    let mut x = instance_hash.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    let unit = (x >> 40) as f32 / 16_777_216.0;
    unit < density.min(1.0)
}

/// The time of year, global to the scene.
///
/// # Example
///
/// ```
/// use regia_framework::season::{FoliageKind, SeasonLook, SeasonState};
///
/// let mut season = SeasonState::default();
/// assert!(SeasonLook::at(FoliageKind::Broadleaf, season.year_fraction()).is_neutral());
/// season.set_named("winter");
/// assert_eq!(SeasonLook::at(FoliageKind::Broadleaf, season.year_fraction()).density, 0.0);
/// ```
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct SeasonState {
    year_fraction: f32,
    /// Year fractions per second of game time (`0`: the season holds still).
    pub rate_per_s: f32,
    /// Foliage kinds to leave at their summer look (an editor override per kind).
    pub hold_summer: [bool; 3],
}

impl Default for SeasonState {
    /// Summer, holding still: a scene that never sets a season looks as it always did.
    fn default() -> Self {
        Self {
            year_fraction: 0.25,
            rate_per_s: 0.0,
            hold_summer: [false; 3],
        }
    }
}

impl SeasonState {
    /// The time of year, `0..1`.
    pub fn year_fraction(&self) -> f32 {
        self.year_fraction
    }

    /// Sets the time of year (wraps into `0..1`); returns `false` for a non-finite value.
    pub fn set_year_fraction(&mut self, fraction: f32) -> bool {
        if !fraction.is_finite() {
            return false;
        }
        self.year_fraction = fraction.rem_euclid(1.0);
        true
    }

    /// Jumps to a named anchor (`spring`, `summer`, `autumn`, `winter`); `false` if unknown.
    pub fn set_named(&mut self, name: &str) -> bool {
        match SEASONS.iter().position(|s| *s == name) {
            Some(i) => {
                self.year_fraction = i as f32 * 0.25;
                true
            }
            None => false,
        }
    }

    /// The name of the nearest anchor at or before the current time of year.
    pub fn name(&self) -> &'static str {
        SEASONS[((self.year_fraction * 4.0).floor() as usize).min(3)]
    }

    /// Advances by `dt` seconds of game time at [`Self::rate_per_s`].
    pub fn advance(&mut self, dt: f32) {
        if dt.is_finite() && self.rate_per_s.is_finite() && self.rate_per_s != 0.0 {
            self.year_fraction = (self.year_fraction + self.rate_per_s * dt).rem_euclid(1.0);
        }
    }

    /// The look of `kind` now (neutral when that kind is held at summer).
    pub fn look(&self, kind: FoliageKind) -> SeasonLook {
        if self.hold_summer[kind as usize] {
            SeasonLook::NEUTRAL
        } else {
            SeasonLook::at(kind, self.year_fraction)
        }
    }
}

/// Marks a mesh whose material the season drives. Keeps the original material.
#[derive(Component, Debug, Clone)]
pub struct SeasonalFoliage {
    /// What kind of foliage the mesh is.
    pub kind: FoliageKind,
    original: Option<Arc<CpuMaterial>>,
    /// The year fraction last applied (`NaN`: never).
    applied: f32,
}

impl SeasonalFoliage {
    /// A seasonal mesh of `kind`.
    pub fn new(kind: FoliageKind) -> Self {
        Self {
            kind,
            original: None,
            applied: f32::NAN,
        }
    }
}

/// `PostUpdate`: advances the season and rewrites the materials of seasonal meshes when it changed.
#[tracing::instrument(skip_all, target = "regia_framework", name = "season_apply")]
fn apply_season(
    time: Res<Time>,
    state: Option<ResMut<SeasonState>>,
    mut meshes: Query<(&mut MeshRenderer, &mut SeasonalFoliage)>,
) {
    let Some(mut state) = state else {
        return;
    };
    if state.rate_per_s != 0.0 {
        let dt = time.delta_secs();
        state.advance(dt);
    }
    let mut shared: HashMap<(usize, FoliageKind), Arc<CpuMaterial>> = HashMap::new();
    for (mut mesh, mut seasonal) in &mut meshes {
        let held = state.hold_summer[seasonal.kind as usize];
        let target = if held { 0.25 } else { state.year_fraction };
        if seasonal.applied == target {
            continue;
        }
        let original = seasonal
            .original
            .get_or_insert_with(|| Arc::clone(&mesh.material))
            .clone();
        let look = SeasonLook::at(seasonal.kind, target);
        let material = if look.is_neutral() {
            original
        } else {
            let key = (Arc::as_ptr(&original) as usize, seasonal.kind);
            Arc::clone(
                shared
                    .entry(key)
                    .or_insert_with(|| Arc::new(look.apply(&original))),
            )
        };
        mesh.material = material;
        seasonal.applied = target;
    }
}

/// Registers the resource and system (add to the app that owns the scene).
#[derive(Debug, Default, Clone, Copy)]
pub struct SeasonPlugin;

impl Plugin for SeasonPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SeasonState>()
            .add_systems(PostUpdate, apply_season.before(PostUpdateSet::TransformPropagate));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{CpuMesh, MeshRenderer};

    fn leaf_material() -> CpuMaterial {
        CpuMaterial {
            base_color: [0.2, 0.35, 0.1, 1.0],
            alpha_mode: ALPHA_MASK,
            alpha_cutoff: 0.5,
            ..CpuMaterial::default()
        }
    }

    #[test]
    fn summer_is_neutral_and_the_year_wraps() {
        for kind in [FoliageKind::Broadleaf, FoliageKind::Evergreen, FoliageKind::Grass] {
            assert!(SeasonLook::at(kind, 0.25).is_neutral(), "{kind:?}");
            assert_eq!(SeasonLook::at(kind, 1.25), SeasonLook::at(kind, 0.25));
            assert_eq!(SeasonLook::at(kind, -0.75), SeasonLook::at(kind, 0.25));
        }
        assert!(SeasonLook::at(FoliageKind::Broadleaf, f32::NAN).is_neutral());
    }

    #[test]
    fn a_broadleaf_turns_warm_in_autumn_and_is_bare_in_winter() {
        let autumn = SeasonLook::at(FoliageKind::Broadleaf, 0.5);
        assert!(autumn.tint[0] > 1.5 && autumn.tint[2] < 0.5, "{autumn:?}");
        let winter = SeasonLook::at(FoliageKind::Broadleaf, 0.75);
        assert_eq!(winter.density, 0.0);
        assert_eq!(winter.leaf_fall, 1.0);
        // Density falls monotonically from summer through autumn to winter.
        let mut last = 1.0;
        for i in 0..=100 {
            let d = SeasonLook::at(FoliageKind::Broadleaf, 0.25 + 0.5 * i as f32 / 100.0).density;
            assert!(d <= last + 1e-6, "{i}: {d} > {last}");
            last = d;
        }
    }

    #[test]
    fn evergreens_stay_green() {
        for i in 0..100 {
            let look = SeasonLook::at(FoliageKind::Evergreen, i as f32 / 100.0);
            assert!(look.density >= 0.95);
            assert!(look.tint.iter().all(|c| (0.85..=1.1).contains(c)), "{look:?}");
        }
    }

    #[test]
    fn a_bare_look_makes_masked_cards_vanish_and_a_full_one_is_unchanged() {
        let leaf = leaf_material();
        let winter = SeasonLook::at(FoliageKind::Broadleaf, 0.75).apply(&leaf);
        assert_eq!(winter.base_color[3], 0.0);
        assert_eq!(SeasonLook::NEUTRAL.apply(&leaf).base_color, leaf.base_color);
        let half = SeasonLook {
            density: 0.5,
            leaf_fall: 0.5,
            ..SeasonLook::NEUTRAL
        };
        let m = half.apply(&leaf).base_color[3];
        assert!(m > 0.5 && m < 1.0, "{m}");
    }

    #[test]
    fn opaque_materials_only_take_the_tint() {
        let bark = CpuMaterial {
            base_color: [0.3, 0.2, 0.1, 1.0],
            ..CpuMaterial::default()
        };
        let winter = SeasonLook::at(FoliageKind::Broadleaf, 0.75).apply(&bark);
        assert_eq!(winter.base_color[3], 1.0);
    }

    /// Instanced scatter classes take their answer from `regia_terrain`, which cannot depend on this
    /// crate: the two rules must stay one (ADR-3146).
    #[test]
    fn the_scatter_classes_rule_is_this_rule() {
        let density_steps = (0..=128).map(|s| s as f32 / 128.0);
        for density in density_steps.chain([1.0e-6, 0.333_333_34, 7.0, -1.0, f32::NAN, f32::INFINITY]) {
            for i in 0..4096u64 {
                let hash = i.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (i << 7);
                assert_eq!(
                    leaf_present(hash, density),
                    regia_terrain::leaf_present(hash, density),
                    "hash {hash:#x}, density {density}"
                );
            }
        }
    }

    #[test]
    fn leaf_present_is_deterministic_and_monotonic() {
        let mut counts = Vec::new();
        for density in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let present: Vec<bool> = (0..4000_u64).map(|h| leaf_present(h, density)).collect();
            counts.push(present.iter().filter(|p| **p).count());
            let again: Vec<bool> = (0..4000_u64).map(|h| leaf_present(h, density)).collect();
            assert_eq!(present, again);
        }
        assert_eq!(counts[0], 0);
        assert_eq!(counts[4], 4000);
        assert!((counts[2] as i64 - 2000).abs() < 150, "{counts:?}");
        for h in 0..4000_u64 {
            if leaf_present(h, 0.3) {
                assert!(leaf_present(h, 0.6));
            }
        }
        assert!(!leaf_present(1, f32::NAN));
    }

    #[test]
    fn named_seasons_and_names() {
        let mut state = SeasonState::default();
        assert_eq!(state.name(), "summer");
        for (i, name) in SEASONS.iter().enumerate() {
            assert!(state.set_named(name));
            assert_eq!(state.year_fraction(), i as f32 * 0.25);
            assert_eq!(state.name(), *name);
        }
        assert!(!state.set_named("monsoon"));
        assert!(!state.set_year_fraction(f32::INFINITY));
        assert!(state.set_year_fraction(1.5));
        assert_eq!(state.year_fraction(), 0.5);
    }

    #[test]
    fn the_system_tints_shares_and_restores_materials() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SeasonPlugin));
        let material = Arc::new(leaf_material());
        let mesh = Arc::new(CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        let spawn = |app: &mut App| {
            app.world_mut()
                .spawn((
                    MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
                    SeasonalFoliage::new(FoliageKind::Broadleaf),
                ))
                .id()
        };
        let a = spawn(&mut app);
        let b = spawn(&mut app);
        app.update();
        // Summer: the original material, untouched.
        assert!(Arc::ptr_eq(
            &app.world().get::<MeshRenderer>(a).expect("a").material,
            &material
        ));
        app.world_mut().resource_mut::<SeasonState>().set_named("autumn");
        app.update();
        let ma = Arc::clone(&app.world().get::<MeshRenderer>(a).expect("a").material);
        let mb = Arc::clone(&app.world().get::<MeshRenderer>(b).expect("b").material);
        assert!(Arc::ptr_eq(&ma, &mb), "one copy shared");
        assert!(ma.base_color[0] > material.base_color[0]);
        app.world_mut().resource_mut::<SeasonState>().set_named("winter");
        app.update();
        assert_eq!(
            app.world().get::<MeshRenderer>(a).expect("a").material.base_color[3],
            0.0
        );
        app.world_mut().resource_mut::<SeasonState>().set_named("summer");
        app.update();
        assert!(Arc::ptr_eq(
            &app.world().get::<MeshRenderer>(a).expect("a").material,
            &material
        ));
    }
}
