// SPDX-License-Identifier: MPL-2.0
//! Impact sites: a symmetric crater, burnt ground, an ejecta blanket and a hull that changes to
//! its battered variant, all scheduled to a frame and driven from the same clock as the debris
//! ([`crate::debris_mesh::DebrisClock`]).
//!
//! An [`ImpactSiteSpec`] is the geometry of one site. It gives
//!
//! - the **height profile** ([`ImpactSiteSpec::height_delta`]): a simple-crater bowl with a raised
//!   rim and an ejecta blanket whose thickness falls off as `r⁻³`. The host adds it to its
//!   terrain height map (`TerrainMap::set_height`) so the pit is real geometry; the decals below
//!   are only the colour on top;
//! - the **decal set** ([`ImpactSiteSpec::decals`]): wide soot, a nearer burn, the blanket, the
//!   crater floor and a black core with an ember that cools in seconds. There is no gouge or
//!   skid mark: a steep fall at terminal speed digs a round pit, not a furrow;
//! - the **buried pose** ([`ImpactSiteSpec::buried_transform`]): where a battered hull rests,
//!   part sunk below the crater floor and leaning a little, with soil around it.
//!
//! [`ImpactSites`] schedules a site at a frame: a small first-contact dust puff a few frames
//! before it (`puff_lead` frames, the hook the blast effect lanes share), the decals at the frame,
//! and, for any entity carrying a [`DamageSwap`], the swap from the intact mesh to the damaged
//! one. The swap is a pure function of the clock (a seek back restores the intact hull), and
//! rewinding past a site retires its decals.
//!
//! **Tiers.** [`ImpactDetail`] picks five decals (Ultra, High), four (Medium: no blanket, no
//! ember) or two (Low, Deck, Mobile: the mid burn and the floor).
//!
//! **Not modelled.** Volume conservation of the crater, and any gouge: the hold on a
//! lifting-body skid was decided as a steep fall (D11), so no decal is aligned along a path.
//!
//! Provenance: the profile follows published crater morphometry (a parabolic bowl of depth `d`,
//! rim height `h` at the crest `R`, Pike 1977 style proportions, and ejecta thickness
//! `h (R/r)³` after McGetchin, Settle and Head 1973); the decal layering is original
//! (`docs/provenance.md`).

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3, det_quat_from_axis_angle, det_sin_cos};
use regia_render::quality::QualityTier;
use regia_render::{CpuMaterial, CpuMesh, MeshRenderer};
use regia_scene::Transform;

use crate::debris_mesh::{DebrisClock, tick_debris_clock};
use crate::decal::{
    CHANNEL_ALBEDO, CHANNEL_EMISSIVE, Decal, DecalId, DecalPool, DecalSurface, LAYER_WORLD, align_to_normal,
};
use crate::plugin::SpawnEffect;
use crate::scene::VfxScene;

/// The effect played at first ground contact, a few frames before the blast
/// ([`ImpactSites::schedule`] `puff_lead`). A host registers a `.vfx.ron` of this name; without
/// one the puff is skipped.
pub const FIRST_CONTACT_PUFF_EFFECT: &str = "impact_first_contact_dust";
/// Most sites alive at once.
pub const MAX_SITES: usize = 8;
/// Default frames the first-contact puff leads the impact by.
pub const DEFAULT_PUFF_LEAD_FRAMES: u64 = 4;
/// Time constant of the ember at the core, seconds.
pub const EMBER_COOLING_S: f32 = 6.0;

/// How many decals a site lays down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImpactDetail {
    /// Mid burn and floor only.
    Minimal,
    /// Far soot, mid burn, floor and core (no blanket, no ember).
    Reduced,
    /// Everything, with the cooling ember.
    Full,
}

impl ImpactDetail {
    /// Lower-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Reduced => "reduced",
            Self::Full => "full",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Minimal, Self::Reduced, Self::Full]
            .into_iter()
            .find(|d| d.name() == name)
    }

    /// The detail for a quality tier.
    #[must_use]
    pub const fn for_tier(tier: QualityTier) -> Self {
        match tier {
            QualityTier::MobileLow
            | QualityTier::MobileHigh
            | QualityTier::Deck
            | QualityTier::Low
            | QualityTier::VeryLow => Self::Minimal,
            QualityTier::Medium => Self::Reduced,
            QualityTier::High | QualityTier::Ultra => Self::Full,
        }
    }
}

/// One decal layer of a site (private builder input).
struct Layer {
    radius: f32,
    depth: f32,
    albedo: [f32; 3],
    opacity: f32,
    atlas_layer: i32,
    priority: i16,
    yaw: f32,
}

/// The geometry of one impact site.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImpactSiteSpec {
    /// Where the object struck, on the pre-impact ground (world metres).
    pub center: Vec3,
    /// Radius of the crater at the rim crest, metres (20–35 for a 40–70 m crater).
    pub radius: f32,
    /// Depth of the floor below the pre-impact ground, metres (10–20).
    pub depth: f32,
    /// Height of the rim crest above the pre-impact ground, metres (3–5).
    pub rim_height: f32,
    /// Radius the ejecta blanket reaches, metres (about 150).
    pub blanket_radius: f32,
    /// Radius the soot and stripped ground reaches, metres (about 300).
    pub scorch_radius: f32,
    /// Seed for the decal yaws.
    pub seed: u64,
}

impl Default for ImpactSiteSpec {
    fn default() -> Self {
        Self {
            center: Vec3::ZERO,
            radius: 27.0,
            depth: 14.0,
            rim_height: 4.0,
            blanket_radius: 150.0,
            scorch_radius: 300.0,
            seed: 0x517e,
        }
    }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge1 <= edge0 {
        return if x >= edge1 { 1.0 } else { 0.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl ImpactSiteSpec {
    /// Whether every dimension is finite and ordered (`radius < blanket_radius <
    /// scorch_radius`, positive depth).
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let finite = [
            self.center.x,
            self.center.y,
            self.center.z,
            self.radius,
            self.depth,
            self.rim_height,
            self.blanket_radius,
            self.scorch_radius,
        ]
        .iter()
        .all(|v| v.is_finite());
        finite
            && self.radius > 0.5
            && self.depth > 0.0
            && self.rim_height >= 0.0
            && self.blanket_radius > self.radius
            && self.scorch_radius >= self.blanket_radius
    }

    /// Height change at distance `r` from the centre, metres (negative inside the pit).
    ///
    /// Inside the crest the bowl is `-d + (d + h)(r/R)²` (floor `-d`, crest `+h`); outside it
    /// is the blanket `h (R/r)³`, eased to zero over its outer 40 %.
    #[must_use]
    pub fn height_delta(&self, r: f32) -> f32 {
        let r = r.max(0.0);
        if r <= self.radius {
            let t = r / self.radius;
            return -self.depth + (self.depth + self.rim_height) * t * t;
        }
        if r >= self.blanket_radius {
            return 0.0;
        }
        let falloff = 1.0 - smoothstep(0.6 * self.blanket_radius, self.blanket_radius, r);
        self.rim_height * (self.radius / r).powi(3) * falloff
    }

    /// [`Self::height_delta`] at world `(x, z)`.
    #[must_use]
    pub fn displacement_at(&self, x: f32, z: f32) -> f32 {
        let (dx, dz) = (x - self.center.x, z - self.center.z);
        self.height_delta((dx * dx + dz * dz).sqrt())
    }

    /// Height of the crater floor, world metres.
    #[must_use]
    pub fn floor_y(&self) -> f32 {
        self.center.y - self.depth
    }

    /// Where a battered hull rests: centred over the crater, `sink` of its height below the
    /// floor (`0.5` puts its middle at the floor, half buried), leaning by `tilt` radians about
    /// a horizontal axis chosen by the seed, turned by `yaw`.
    #[must_use]
    pub fn buried_transform(&self, hull_half_height: f32, sink: f32, tilt: f32, yaw: f32) -> Transform {
        let sink = sink.clamp(0.0, 1.0);
        // The lean axis and the yaw run on the deterministic trig kernel: the same quaternion
        // bits on every platform (platform libm sin/cos last digits would rest the wreck
        // differently per machine, and the swap writes this pose every frame while swapped).
        let lean_dir = {
            let a = (self.seed % 6283) as f32 * 1e-3;
            let (sin, cos) = det_sin_cos(a);
            Vec3::new(cos, 0.0, sin)
        };
        let lean = det_quat_from_axis_angle(lean_dir.cross(Vec3::Y).normalize_or(Vec3::X), tilt);
        // `Quat::from_rotation_y(yaw)` from the kernel: (0, sin(yaw/2), 0, cos(yaw/2)).
        let (yaw_sin, yaw_cos) = det_sin_cos(yaw * 0.5);
        Transform {
            translation: Vec3::new(
                self.center.x,
                self.floor_y() + hull_half_height * (1.0 - 2.0 * sink),
                self.center.z,
            ),
            rotation: lean * Quat::from_xyzw(0.0, yaw_sin, 0.0, yaw_cos),
            scale: Vec3::ONE,
        }
    }

    /// The decals of this site at `detail`, in draw order (all project down onto the world
    /// layers, none fade by lifetime).
    #[must_use]
    pub fn decals(&self, detail: ImpactDetail) -> Vec<(Decal, DecalSurface)> {
        let mut yaw = self.seed;
        let mut next_yaw = || {
            yaw = yaw
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (yaw >> 40) as f32 / (1u64 << 24) as f32 * std::f32::consts::TAU
        };
        let reach = self.depth + self.rim_height + 4.0;
        let mut out = Vec::new();
        let mut push =
            |radius: f32, depth: f32, albedo: [f32; 3], opacity: f32, atlas_layer: i32, priority: i16| {
                out.push(self.layer(Layer {
                    radius,
                    depth,
                    albedo,
                    opacity,
                    atlas_layer,
                    priority,
                    yaw: next_yaw(),
                }));
            };
        if detail == ImpactDetail::Full || detail == ImpactDetail::Reduced {
            // Soot and stripped ground: wide, soft, dark.
            push(
                self.scorch_radius,
                (self.scorch_radius * 0.25).clamp(2.0, 60.0),
                [0.045, 0.04, 0.036],
                0.45,
                0,
                -3,
            );
        }
        push(
            self.scorch_radius * 0.4,
            (self.scorch_radius * 0.1).clamp(2.0, 40.0),
            [0.04, 0.034, 0.03],
            0.6,
            0,
            -2,
        );
        if detail == ImpactDetail::Full {
            push(
                self.blanket_radius,
                (self.blanket_radius * 0.2).clamp(2.0, 40.0),
                [0.13, 0.09, 0.062],
                0.5,
                3,
                -1,
            );
        }
        push(self.radius * 1.05, reach, [0.03, 0.025, 0.02], 0.85, 0, 1);
        let mut decals = out;
        if detail != ImpactDetail::Minimal {
            let (mut core, mut surface) = self.layer(Layer {
                radius: self.radius * 0.45,
                depth: reach,
                albedo: [0.008, 0.007, 0.006],
                opacity: 0.92,
                atlas_layer: 0,
                priority: 2,
                yaw: 0.0,
            });
            if detail == ImpactDetail::Full {
                // A bright ember on the core that cools like hot ground, in seconds.
                surface.emissive = [1.6, 0.35, 0.05];
                surface.channels |= CHANNEL_EMISSIVE;
                surface.cooling_s = EMBER_COOLING_S;
            }
            core.color[3] = surface.opacity;
            decals.push((core, surface));
        }
        decals
    }

    fn layer(&self, l: Layer) -> (Decal, DecalSurface) {
        let Layer {
            radius,
            depth,
            albedo,
            opacity,
            atlas_layer,
            priority,
            yaw,
        } = l;
        let decal = Decal {
            position: self.center,
            rotation: align_to_normal(Vec3::Y, yaw),
            half_size: Vec3::new(radius, radius, depth),
            color: [albedo[0], albedo[1], albedo[2], opacity],
            age: 0.0,
            lifetime: None,
        };
        let surface = DecalSurface {
            albedo,
            opacity,
            roughness: 1.0,
            normal_xy: [0.0, 0.0],
            normal_strength: 0.0,
            emissive: [0.0; 3],
            // The crater walls are steep: keep the soot on them.
            angle_fade: [0.0, 0.0],
            distance_fade: [0.0, 0.0],
            priority,
            receiver_mask: LAYER_WORLD,
            atlas_layer,
            channels: CHANNEL_ALBEDO,
            edge_softness: 1.0,
            blend: 0.0,
            cooling_s: 0.0,
            metallic: 0.0,
            shading: crate::decal::DecalShading::Auto,
        };
        (decal, surface)
    }
}

/// One scheduled site.
#[derive(Clone, Debug)]
pub struct ImpactSite {
    /// Id.
    pub id: u64,
    /// Geometry.
    pub spec: ImpactSiteSpec,
    /// Frame the decals are laid and the swaps happen.
    pub at_frame: u64,
    /// Frames before `at_frame` the first-contact puff plays.
    pub puff_lead: u64,
    /// Decal detail.
    pub detail: ImpactDetail,
    placed: Vec<DecalId>,
    is_placed: bool,
    puffed: bool,
}

impl ImpactSite {
    /// Whether the decals are down.
    #[must_use]
    pub fn is_placed(&self) -> bool {
        self.is_placed
    }

    /// Whether the first-contact puff has played.
    #[must_use]
    pub fn puffed(&self) -> bool {
        self.puffed
    }
}

/// Why a site could not be scheduled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImpactSiteError {
    /// The spec has a non-finite or mis-ordered dimension.
    Invalid,
    /// [`MAX_SITES`] sites are alive.
    TooMany,
}

impl std::fmt::Display for ImpactSiteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid => write!(f, "the impact site dimensions must be finite and ordered"),
            Self::TooMany => write!(f, "{MAX_SITES} impact sites are already scheduled"),
        }
    }
}

impl std::error::Error for ImpactSiteError {}

/// The scheduled sites, and the effect names that schedule one when a game fires them.
#[derive(Resource, Debug, Clone)]
pub struct ImpactSites {
    sites: Vec<ImpactSite>,
    next_id: u64,
    /// Detail for sites scheduled from an effect event.
    pub detail: ImpactDetail,
    /// A [`SpawnEffect`] with one of these names schedules a default site at its position, at
    /// the current clock frame.
    pub trigger_effects: Vec<String>,
    /// Template for sites scheduled from an effect event (the centre is replaced).
    pub template: ImpactSiteSpec,
}

impl Default for ImpactSites {
    fn default() -> Self {
        Self {
            sites: Vec::new(),
            next_id: 0,
            detail: ImpactDetail::Full,
            trigger_effects: vec!["impact_site".into()],
            template: ImpactSiteSpec::default(),
        }
    }
}

impl ImpactSites {
    /// Moves every scheduled site by `-shift` (spec 78 § 12.2). Decals already placed live in the
    /// [`crate::DecalPool`], which shifts on its own.
    pub fn shift_origin(&mut self, shift: Vec3) {
        for site in &mut self.sites {
            site.spec.center -= shift;
        }
    }

    /// Schedules a site. Returns its id.
    ///
    /// # Errors
    ///
    /// [`ImpactSiteError`].
    pub fn schedule(
        &mut self,
        spec: ImpactSiteSpec,
        at_frame: u64,
        puff_lead: u64,
        detail: ImpactDetail,
    ) -> Result<u64, ImpactSiteError> {
        if !spec.is_valid() {
            return Err(ImpactSiteError::Invalid);
        }
        if self.sites.len() >= MAX_SITES {
            return Err(ImpactSiteError::TooMany);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.sites.push(ImpactSite {
            id,
            spec,
            at_frame,
            puff_lead,
            detail,
            placed: Vec::new(),
            is_placed: false,
            puffed: false,
        });
        Ok(id)
    }

    /// The scheduled sites.
    #[must_use]
    pub fn sites(&self) -> &[ImpactSite] {
        &self.sites
    }

    /// Removes a site (its decals retire on the next `Update`). False for an unknown id or one already
    /// removed.
    pub fn remove(&mut self, id: u64) -> bool {
        let Some(site) = self.sites.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        // Already pulled out by an earlier call and waiting for the driver to drop it.
        if site.at_frame == u64::MAX {
            return false;
        }
        // Pull the schedule to the far future so the driver retires the decals, then drop it.
        site.at_frame = u64::MAX;
        site.puff_lead = 0;
        true
    }
}

/// The hull's intact state, captured when its swap first applies.
#[derive(Clone, Debug)]
struct Intact {
    mesh: Arc<CpuMesh>,
    material: Arc<CpuMaterial>,
    transform: Transform,
}

/// Swaps an entity's mesh, material and pose to a damaged variant at a frame.
///
/// While the clock is at or past `at_frame` the entity wears the damaged mesh and pose (the pose
/// is rewritten every frame, so a scripted track that keeps moving the intact hull cannot drag
/// the wreck); before it, or after a seek back, the intact mesh returns and the pose it had at
/// the swap is restored.
#[derive(Component, Clone, Debug)]
pub struct DamageSwap {
    /// Frame on [`DebrisClock`] the swap happens.
    pub at_frame: u64,
    /// The battered mesh.
    pub damaged_mesh: Arc<CpuMesh>,
    /// The battered material.
    pub damaged_material: Arc<CpuMaterial>,
    /// Where the wreck rests (typically [`ImpactSiteSpec::buried_transform`]).
    pub damaged_transform: Transform,
    intact: Option<Intact>,
    swapped: bool,
}

impl DamageSwap {
    /// A swap at `at_frame` to the given mesh, material and rest pose.
    #[must_use]
    pub fn new(
        at_frame: u64,
        damaged_mesh: Arc<CpuMesh>,
        damaged_material: Arc<CpuMaterial>,
        damaged_transform: Transform,
    ) -> Self {
        Self {
            at_frame,
            damaged_mesh,
            damaged_material,
            damaged_transform,
            intact: None,
            swapped: false,
        }
    }

    /// Whether the damaged variant is showing.
    #[must_use]
    pub fn is_swapped(&self) -> bool {
        self.swapped
    }
}

fn drive_damage_swaps(
    clock: Res<DebrisClock>,
    mut hulls: Query<(&mut DamageSwap, &mut MeshRenderer, &mut Transform)>,
) {
    let _span = tracing::debug_span!("drive_damage_swaps").entered();
    for (mut swap, mut renderer, mut transform) in &mut hulls {
        let want = clock.frame >= swap.at_frame;
        if want && !swap.swapped {
            let intact = Intact {
                mesh: Arc::clone(&renderer.mesh),
                material: Arc::clone(&renderer.material),
                transform: *transform,
            };
            swap.intact = Some(intact);
            renderer.mesh = Arc::clone(&swap.damaged_mesh);
            renderer.material = Arc::clone(&swap.damaged_material);
            swap.swapped = true;
        } else if !want && swap.swapped {
            if let Some(intact) = swap.intact.clone() {
                renderer.mesh = intact.mesh;
                renderer.material = intact.material;
                *transform = intact.transform;
            }
            swap.swapped = false;
        }
        if swap.swapped {
            *transform = swap.damaged_transform;
        }
    }
}

fn drive_impact_sites(
    clock: Res<DebrisClock>,
    mut sites: ResMut<ImpactSites>,
    pool: Option<ResMut<DecalPool>>,
    scene: Option<ResMut<VfxScene>>,
) {
    let frame = clock.frame;
    let mut pool = pool;
    let mut scene = scene;
    for site in &mut sites.sites {
        let puff_frame = site.at_frame.saturating_sub(site.puff_lead);
        let want_puff = frame >= puff_frame && site.at_frame != u64::MAX;
        if want_puff && !site.puffed {
            if let Some(scene) = scene.as_mut() {
                let seed = site.spec.seed ^ 0x9e37_79b9;
                let _ = scene.spawn(FIRST_CONTACT_PUFF_EFFECT, site.spec.center, Vec3::Y, seed);
            }
            site.puffed = true;
        } else if !want_puff {
            site.puffed = false;
        }
        let want_placed = frame >= site.at_frame;
        let Some(pool) = pool.as_mut() else {
            continue;
        };
        if want_placed && !site.is_placed {
            let _span = tracing::info_span!("impact_site_decals").entered();
            for (decal, surface) in site.spec.decals(site.detail) {
                site.placed.push(pool.spawn_with(decal, surface));
            }
            site.is_placed = true;
        } else if !want_placed && site.is_placed {
            for id in site.placed.drain(..) {
                pool.remove(id);
            }
            site.is_placed = false;
        }
    }
    // A removed site (scheduled at u64::MAX) is dropped once its decals are retired.
    sites.sites.retain(|s| s.at_frame != u64::MAX || s.is_placed);
}

fn route_impact_effects(
    mut requests: MessageReader<SpawnEffect>,
    mut sites: ResMut<ImpactSites>,
    clock: Res<DebrisClock>,
) {
    let _span = tracing::debug_span!("route_impact_effects").entered();
    for request in requests.read() {
        if !sites.trigger_effects.contains(&request.effect) {
            continue;
        }
        let mut spec = sites.template;
        spec.center = request.position;
        if let Some(seed) = request.seed {
            spec.seed = seed;
        }
        let detail = sites.detail;
        let result = sites.schedule(
            spec,
            clock.frame,
            DEFAULT_PUFF_LEAD_FRAMES.min(clock.frame),
            detail,
        );
        if let Err(err) = result {
            tracing::warn!(target: "regia_vfx", "impact site from effect '{}' refused: {err}", request.effect);
        }
    }
}

/// Adds [`ImpactSites`] and the systems that drive sites and [`DamageSwap`]s from
/// [`DebrisClock`]. Add [`crate::debris_mesh::DebrisPlugin`] too (it owns the clock) and
/// [`crate::VfxPlugin`] for the decal pool and the puff.
#[derive(Debug, Default, Clone, Copy)]
pub struct ImpactSitePlugin;

impl Plugin for ImpactSitePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DebrisClock>()
            .init_resource::<ImpactSites>()
            .add_message::<SpawnEffect>()
            .add_systems(
                Update,
                (route_impact_effects, drive_impact_sites, drive_damage_swaps)
                    .chain()
                    .after(tick_debris_clock),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ImpactSiteSpec {
        ImpactSiteSpec {
            center: Vec3::new(100.0, 12.0, -40.0),
            ..ImpactSiteSpec::default()
        }
    }

    #[test]
    fn profile_meets_the_acceptance_numbers() {
        let s = spec();
        // 40-70 m across, 10-20 m deep, 3-5 m rim.
        assert!((40.0..=70.0).contains(&(2.0 * s.radius)));
        assert!((10.0..=20.0).contains(&s.depth));
        assert!((3.0..=5.0).contains(&s.rim_height));
        assert!((s.height_delta(0.0) + s.depth).abs() < 1e-5, "floor at -depth");
        assert!(
            (s.height_delta(s.radius) - s.rim_height).abs() < 1e-4,
            "crest at +rim"
        );
    }

    #[test]
    fn profile_is_continuous_and_symmetric() {
        let s = spec();
        let mut prev = s.height_delta(0.0);
        let mut r = 0.0;
        while r < s.blanket_radius + 10.0 {
            r += 0.05;
            let h = s.height_delta(r);
            assert!((h - prev).abs() < 0.2, "jump {prev} -> {h} at r={r}");
            prev = h;
        }
        let a = s.displacement_at(s.center.x + 12.0, s.center.z);
        let b = s.displacement_at(s.center.x, s.center.z - 12.0);
        assert!((a - b).abs() < 1e-5);
    }

    #[test]
    fn blanket_falls_off_as_r_cubed_and_ends_at_the_edge() {
        let s = spec();
        let near = s.height_delta(1.5 * s.radius);
        let far = s.height_delta(3.0 * s.radius);
        // Within the undamped part the ratio is 2^3.
        assert!(((near / far) - 8.0).abs() < 0.5, "{near} / {far}");
        assert_eq!(s.height_delta(s.blanket_radius), 0.0);
        assert_eq!(s.height_delta(s.blanket_radius * 2.0), 0.0);
        assert!(
            s.height_delta(s.radius * 1.5) > 0.0,
            "the blanket raises the ground"
        );
    }

    #[test]
    fn invalid_specs_are_refused() {
        let mut bad = spec();
        bad.radius = f32::NAN;
        assert!(!bad.is_valid());
        let mut inverted = spec();
        inverted.blanket_radius = inverted.radius * 0.5;
        assert!(!inverted.is_valid());
        let mut sites = ImpactSites::default();
        assert_eq!(
            sites.schedule(inverted, 0, 0, ImpactDetail::Full),
            Err(ImpactSiteError::Invalid)
        );
    }

    #[test]
    fn detail_levels_lay_the_expected_decals() {
        let s = spec();
        assert_eq!(s.decals(ImpactDetail::Full).len(), 5);
        assert_eq!(s.decals(ImpactDetail::Reduced).len(), 4);
        assert_eq!(s.decals(ImpactDetail::Minimal).len(), 2);
        for detail in [ImpactDetail::Full, ImpactDetail::Reduced, ImpactDetail::Minimal] {
            for (decal, surface) in s.decals(detail) {
                assert_eq!(decal.position, s.center);
                assert!(decal.lifetime.is_none(), "a crater does not fade");
                assert_eq!(surface.receiver_mask, LAYER_WORLD);
                assert!(decal.half_size.min_element() > 0.0);
            }
        }
    }

    #[test]
    fn only_the_full_core_carries_an_ember() {
        let s = spec();
        let ember = |d| {
            s.decals(d)
                .iter()
                .filter(|(_, surf)| surf.channels & CHANNEL_EMISSIVE != 0)
                .count()
        };
        assert_eq!(ember(ImpactDetail::Full), 1);
        assert_eq!(ember(ImpactDetail::Reduced), 0);
        assert_eq!(ember(ImpactDetail::Minimal), 0);
    }

    #[test]
    fn decal_footprints_match_the_acceptance_radii() {
        let s = spec();
        let radii: Vec<f32> = s
            .decals(ImpactDetail::Full)
            .iter()
            .map(|(d, _)| d.half_size.x)
            .collect();
        assert!(radii.iter().any(|r| (*r - 300.0).abs() < 1e-3), "soot to ~300 m");
        assert!(
            radii.iter().any(|r| (*r - 150.0).abs() < 1e-3),
            "blanket to ~150 m"
        );
    }

    #[test]
    fn decals_are_deterministic() {
        assert_eq!(
            spec().decals(ImpactDetail::Full),
            spec().decals(ImpactDetail::Full)
        );
    }

    #[test]
    fn tiers_map_to_detail() {
        assert_eq!(
            ImpactDetail::for_tier(QualityTier::MobileLow),
            ImpactDetail::Minimal
        );
        assert_eq!(ImpactDetail::for_tier(QualityTier::Deck), ImpactDetail::Minimal);
        assert_eq!(ImpactDetail::for_tier(QualityTier::Medium), ImpactDetail::Reduced);
        assert_eq!(ImpactDetail::for_tier(QualityTier::Ultra), ImpactDetail::Full);
        for d in [ImpactDetail::Minimal, ImpactDetail::Reduced, ImpactDetail::Full] {
            assert_eq!(ImpactDetail::from_name(d.name()), Some(d));
        }
    }

    #[test]
    fn buried_hull_sits_half_sunk_at_the_floor() {
        let s = spec();
        let t = s.buried_transform(2.0, 0.5, 0.2, 0.7);
        assert_eq!(t.translation.x, s.center.x);
        assert!(
            (t.translation.y - s.floor_y()).abs() < 1e-5,
            "half sunk: middle at the floor"
        );
        let mostly = s.buried_transform(2.0, 0.8, 0.0, 0.0);
        assert!(mostly.translation.y < s.floor_y());
        let lean = t.rotation * Vec3::Y;
        assert!(lean.y < 1.0 && lean.y > 0.95, "leans a little: {lean:?}");
    }

    /// The wreck rest pose is bit-pinned: the lean axis and the yaw run on the deterministic
    /// trig kernel, so the same seed/sink/tilt/yaw rest pose holds on every platform (the
    /// DamageSwap driver writes this transform every frame while swapped).
    #[test]
    fn buried_transform_quat_bits_are_pinned() {
        let t = spec().buried_transform(2.0, 0.5, 0.2, 0.7);
        assert_eq!(
            t.rotation.to_array().map(f32::to_bits),
            BURIED_QUAT_PINNED_BITS,
            "buried hull rotation bits changed"
        );
        // A different seed leans a different way.
        let other = ImpactSiteSpec {
            seed: 424_242,
            ..spec()
        };
        assert_ne!(
            other
                .buried_transform(2.0, 0.5, 0.2, 0.7)
                .rotation
                .to_array()
                .map(f32::to_bits),
            t.rotation.to_array().map(f32::to_bits)
        );
    }

    /// The lean quaternion is built on the deterministic kernel, not glam's `from_axis_angle`
    /// (platform `sin_cos`): for any tilt the rotation is exactly the shared
    /// `det_quat_from_axis_angle` construction, so the pinned bits above hold on every platform
    /// (corr-vfx C2). The sweep compares against the shared helper itself (it divides by the
    /// axis length), not a hand-built `axis * sin(tilt / 2)`, so the two cannot drift apart.
    #[test]
    fn buried_transform_lean_uses_the_deterministic_kernel_for_every_tilt() {
        let s = spec();
        let a = (s.seed % 6283) as f32 * 1e-3;
        let (sin, cos) = det_sin_cos(a);
        let axis = Vec3::new(cos, 0.0, sin).cross(Vec3::Y).normalize_or(Vec3::X);
        let mut mismatched = Vec::new();
        for i in 0..256 {
            let tilt = i as f32 * 0.0123 - 1.4;
            let lean = det_quat_from_axis_angle(axis, tilt);
            let (yaw_sin, yaw_cos) = det_sin_cos(0.0);
            let want = lean * Quat::from_xyzw(0.0, yaw_sin, 0.0, yaw_cos);
            let got = s.buried_transform(2.0, 0.5, tilt, 0.0).rotation;
            if got.to_array().map(f32::to_bits) != want.to_array().map(f32::to_bits) {
                mismatched.push(tilt);
            }
        }
        assert!(mismatched.is_empty(), "tilts off the kernel: {mismatched:?}");
    }

    const BURIED_QUAT_PINNED_BITS: [u32; 4] = [3180303942, 1051635652, 3180432902, 1064257325]; // pinned 2026-11-09

    /// Review fix-vfx2-determinism finding 2: the lean quaternion must come from the
    /// deterministic kernel, not `Quat::from_axis_angle` (whose half-angle `sin_cos` is the
    /// platform libm): on a platform whose libm differs in the last digit the pre-fix pose
    /// bits moved, so the pin above held a portability the code did not have.
    #[test]
    fn buried_transform_lean_quat_matches_the_kernel() {
        let s = spec();
        let t = s.buried_transform(2.0, 0.5, 0.2, 0.7);
        let a = (s.seed % 6283) as f32 * 1e-3;
        let (sin, cos) = det_sin_cos(a);
        let lean_dir = Vec3::new(cos, 0.0, sin);
        let lean = det_quat_from_axis_angle(lean_dir.cross(Vec3::Y).normalize_or(Vec3::X), 0.2);
        let (yaw_sin, yaw_cos) = det_sin_cos(0.7 * 0.5);
        let want = lean * Quat::from_xyzw(0.0, yaw_sin, 0.0, yaw_cos);
        assert_eq!(
            t.rotation.to_array().map(f32::to_bits),
            want.to_array().map(f32::to_bits),
            "the lean must come from the deterministic kernel, not the platform libm"
        );
    }

    fn mesh() -> Arc<CpuMesh> {
        Arc::new(CpuMesh {
            vertices: vec![],
            indices: vec![],
        })
    }

    #[test]
    fn swap_is_a_pure_function_of_the_clock() {
        let mut app = App::new();
        app.add_plugins(ImpactSitePlugin);
        app.world_mut().resource_mut::<DebrisClock>().auto = false;
        let intact = mesh();
        let battered = mesh();
        let intact_pose = Transform::from_translation(Vec3::new(5.0, 50.0, 5.0));
        let wreck_pose = Transform::from_translation(Vec3::new(5.0, -2.0, 5.0));
        let hull = app
            .world_mut()
            .spawn((
                intact_pose,
                MeshRenderer::new(Arc::clone(&intact), Arc::new(CpuMaterial::default())),
                DamageSwap::new(
                    10,
                    Arc::clone(&battered),
                    Arc::new(CpuMaterial::default()),
                    wreck_pose,
                ),
            ))
            .id();
        let showing = |app: &App| {
            let renderer = app.world().get::<MeshRenderer>(hull).expect("renderer");
            let pose = *app.world().get::<Transform>(hull).expect("pose");
            (Arc::ptr_eq(&renderer.mesh, &battered), pose.translation)
        };
        app.world_mut().resource_mut::<DebrisClock>().frame = 9;
        app.update();
        assert_eq!(showing(&app), (false, intact_pose.translation));
        app.world_mut().resource_mut::<DebrisClock>().frame = 10;
        app.update();
        assert_eq!(showing(&app), (true, wreck_pose.translation));
        // A host track moving the intact hull cannot drag the wreck.
        app.world_mut()
            .entity_mut(hull)
            .insert(Transform::from_translation(Vec3::new(9.0, 9.0, 9.0)));
        app.update();
        assert_eq!(showing(&app), (true, wreck_pose.translation));
        // Seek back: the intact hull returns.
        app.world_mut().resource_mut::<DebrisClock>().frame = 3;
        app.update();
        let (battered_showing, _) = showing(&app);
        assert!(!battered_showing);
    }

    #[test]
    fn sites_lay_decals_at_the_frame_and_retire_them_on_a_seek_back() {
        let mut app = App::new();
        app.add_plugins(ImpactSitePlugin);
        app.insert_resource(DecalPool::new(32));
        app.world_mut().resource_mut::<DebrisClock>().auto = false;
        let id = app
            .world_mut()
            .resource_mut::<ImpactSites>()
            .schedule(spec(), 20, 4, ImpactDetail::Full)
            .expect("schedule");
        app.world_mut().resource_mut::<DebrisClock>().frame = 19;
        app.update();
        assert!(app.world().resource::<DecalPool>().is_empty());
        app.world_mut().resource_mut::<DebrisClock>().frame = 20;
        app.update();
        assert_eq!(app.world().resource::<DecalPool>().len(), 5);
        app.update();
        assert_eq!(app.world().resource::<DecalPool>().len(), 5, "laid once");
        app.world_mut().resource_mut::<DebrisClock>().frame = 2;
        app.update();
        assert!(app.world().resource::<DecalPool>().is_empty());
        assert!(!app.world().resource::<ImpactSites>().sites()[0].is_placed());
        assert!(app.world_mut().resource_mut::<ImpactSites>().remove(id));
        app.update();
        assert!(app.world().resource::<ImpactSites>().sites().is_empty());
    }

    #[test]
    fn removing_a_site_twice_reports_false_the_second_time() {
        let mut sites = ImpactSites::default();
        let id = sites
            .schedule(spec(), 20, 4, ImpactDetail::Full)
            .expect("schedule");
        assert!(sites.remove(id));
        assert!(
            !sites.remove(id),
            "already removed, waiting for the driver to drop it"
        );
        assert!(!sites.remove(id + 1), "unknown id");
    }

    #[test]
    fn an_effect_event_schedules_a_site_at_its_position() {
        let mut app = App::new();
        app.add_message::<SpawnEffect>();
        app.add_plugins(ImpactSitePlugin);
        app.insert_resource(DecalPool::new(32));
        app.world_mut().resource_mut::<DebrisClock>().auto = false;
        app.world_mut().resource_mut::<DebrisClock>().frame = 7;
        app.world_mut().write_message(SpawnEffect {
            effect: "impact_site".into(),
            position: Vec3::new(3.0, 1.0, 4.0),
            forward: Vec3::Y,
            seed: Some(11),
        });
        app.world_mut().write_message(SpawnEffect {
            effect: "explosion".into(),
            position: Vec3::ZERO,
            forward: Vec3::Y,
            seed: None,
        });
        app.update();
        let sites = app.world().resource::<ImpactSites>();
        assert_eq!(sites.sites().len(), 1, "only the named effect schedules a site");
        assert_eq!(sites.sites()[0].spec.center, Vec3::new(3.0, 1.0, 4.0));
        assert_eq!(sites.sites()[0].spec.seed, 11);
        assert_eq!(
            app.world().resource::<DecalPool>().len(),
            5,
            "laid on the same frame"
        );
    }
}
