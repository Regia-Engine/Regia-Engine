// SPDX-License-Identifier: MPL-2.0
//! Per-instance glass control: the own-canopy view mode ([`GlassView`]) and runtime damage
//! ([`GlassDamage`], crack masks stamped at impact points). Format: `docs/formats/glass-crack.md`;
//! agent: `render.glass_view`, `render.glass_damage` (`docs/book/agents/glass.md`).
//!
//! - [`GlassView::viewer_inside`] turns refraction off for that instance and scales its specular
//!   down to an anti-glare level. It draws after (and does not feed) the refraction copy, so
//!   refracting glass seen through it still bends. Crack shading stays on.
//! - [`GlassDamage`] holds a crack set name, a level D0–D3 and an impact list in mesh-local space.
//!   [`apply_glass_damage`] stamps new impacts into the instance's [`GlassMask`] page; the page is
//!   a cache rebuilt from the impact list (saves keep the list, never the pixels).
//!
//! Entities without either component draw exactly as before (byte-identical frames).

pub mod mask;
pub mod procedural;
pub mod space;

use std::collections::HashMap;
use std::sync::Arc;

use regia_asset::glasscrack::{CookedCrackSet, CrackShading};
use regia_core::math::{Mat4, Vec3};
use regia_ecs::prelude::*;
use regia_scene::GlobalTransform;

pub use mask::{GlassMask, StampPlacement};
pub use regia_asset::glasscrack::GlassType;
pub use space::MaskSpace;

use crate::runtime::MeshRenderer;

/// Default specular scale of [`GlassViewMode::ViewerInside`] (anti-glare).
pub const INSIDE_SPECULAR_DEFAULT: f32 = 0.15;

/// How the camera relates to a glass instance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect)]
pub enum GlassViewMode {
    /// Seen from outside: refraction and full specular.
    #[default]
    Exterior,
    /// The viewer sits behind it (own canopy, visor): no refraction, anti-glare specular.
    ViewerInside,
}

/// Per-instance glass view control.
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct GlassView {
    /// View mode.
    pub mode: GlassViewMode,
    /// Specular multiplier in [`GlassViewMode::ViewerInside`] (0..1).
    pub inside_specular: f32,
}

impl Default for GlassView {
    fn default() -> Self {
        Self {
            mode: GlassViewMode::Exterior,
            inside_specular: INSIDE_SPECULAR_DEFAULT,
        }
    }
}

impl GlassView {
    /// Own-canopy mode with the default anti-glare level.
    #[must_use]
    pub fn viewer_inside() -> Self {
        Self {
            mode: GlassViewMode::ViewerInside,
            ..Self::default()
        }
    }
}

/// How the renderer shades the own light (reflection, sun) of [`GlassViewMode::ViewerInside`]
/// glass (v0.1.17, ADR-224). Exterior glass always takes the full forward shader.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect)]
pub enum CanopyShading {
    /// Environment reflection with Fresnel, the sun (one cascade-0 compare), tint and
    /// absorption, crack shading. No local lights, no fitted emitters, no probe GI, no fog.
    #[default]
    Cheap,
    /// The full forward shader (the look before v0.1.17).
    Full,
}

/// Settings of viewer-inside glass ([`set_canopy_glass`]). Build it with
/// `..CanopyGlass::default()`: later versions may add fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect)]
pub struct CanopyGlass {
    /// Cheap (default) or full shading.
    pub shading: CanopyShading,
    /// Cheap path only, on frames with the glass front layer (screen-space refraction on).
    /// When set, a layer behind the nearest transmissive one adds no light of its own (its
    /// tint still applies). Default `false` keeps every sheet's light.
    pub nearest_layer_only: bool,
}

/// `REGIA_CANOPY_FULL=1` restores the full shader on viewer-inside glass, over
/// [`set_canopy_glass`] and [`crate::FrameRenderer::set_canopy_glass`].
pub const CANOPY_FULL_ENV: &str = "REGIA_CANOPY_FULL";

/// The host's setting ([`set_canopy_glass`]): bit 0 full, bit 1 nearest layer only.
static CANOPY_BITS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub(crate) fn canopy_bits(settings: CanopyGlass) -> u8 {
    u8::from(settings.shading == CanopyShading::Full) | (u8::from(settings.nearest_layer_only) << 1)
}

fn canopy_from_bits(bits: u8) -> CanopyGlass {
    CanopyGlass {
        shading: if bits & 1 != 0 {
            CanopyShading::Full
        } else {
            CanopyShading::Cheap
        },
        nearest_layer_only: bits & 2 != 0,
    }
}

/// Whether `value` (of [`CANOPY_FULL_ENV`]) asks for the full shader: `1`, `true`, `on`, `yes`.
#[must_use]
pub fn canopy_full_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

/// Whether [`CANOPY_FULL_ENV`] is set to a true value (read once per process).
#[must_use]
pub fn canopy_full_env() -> bool {
    static ENV: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENV.get_or_init(|| regia_core::brand::env_var(CANOPY_FULL_ENV).is_ok_and(|v| canopy_full_value(&v)))
}

/// Sets how every renderer shades viewer-inside glass from the next frame (process-wide, not
/// persisted). A renderer's own [`crate::FrameRenderer::set_canopy_glass`] beats it, and
/// [`CANOPY_FULL_ENV`] beats both.
pub fn set_canopy_glass(settings: CanopyGlass) {
    CANOPY_BITS.store(canopy_bits(settings), std::sync::atomic::Ordering::Relaxed);
}

/// Changes the stored process-wide setting ([`set_canopy_glass`]) with `edit` in one atomic
/// read-modify-write and returns `(before, after)` as stored. Two threads editing different
/// fields at once both keep their change (a [`canopy_glass_setting`] then [`set_canopy_glass`]
/// pair can lose one). `edit` may run more than once under contention: keep it pure.
pub fn update_canopy_glass(mut edit: impl FnMut(CanopyGlass) -> CanopyGlass) -> (CanopyGlass, CanopyGlass) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut bits = CANOPY_BITS.load(Relaxed);
    loop {
        let before = canopy_from_bits(bits);
        let after = edit(before);
        match CANOPY_BITS.compare_exchange_weak(bits, canopy_bits(after), Relaxed, Relaxed) {
            Ok(_) => return (before, after),
            Err(now) => bits = now,
        }
    }
}

/// The process-wide setting in effect: full when [`CANOPY_FULL_ENV`] is set, else the value of
/// [`set_canopy_glass`] (default [`CanopyGlass::default`], the cheap path).
#[must_use]
pub fn canopy_glass() -> CanopyGlass {
    resolve_canopy(None)
}

/// The value of [`set_canopy_glass`] as stored, without [`CANOPY_FULL_ENV`].
#[must_use]
pub fn canopy_glass_setting() -> CanopyGlass {
    canopy_from_bits(CANOPY_BITS.load(std::sync::atomic::Ordering::Relaxed))
}

/// `renderer`'s override, else the process-wide setting; [`CANOPY_FULL_ENV`] beats both.
pub(crate) fn resolve_canopy(renderer: Option<CanopyGlass>) -> CanopyGlass {
    if canopy_full_env() {
        return CanopyGlass {
            shading: CanopyShading::Full,
            nearest_layer_only: false,
        };
    }
    renderer.unwrap_or_else(canopy_glass_setting)
}

/// One impact on a glass instance (mesh-local, so it moves with the vehicle or building).
#[derive(Clone, Copy, Debug, Default, PartialEq, Reflect)]
pub struct GlassImpact {
    /// Hit point in mesh-local space.
    pub point: Vec3,
    /// Impact energy (escalation units; laminate holes need `min_hole_energy`).
    pub energy: f32,
    /// Level whose stamp set it used (1–3); 0 = not applied yet (resolved by escalation).
    pub level: u8,
    /// Variant pick and rotation seed.
    pub seed: u32,
    /// Footprint override in metres (0 = the stamp's authored size).
    pub size_m: f32,
}

/// Runtime cache of a damaged instance (not saved; rebuilt from the impact list).
#[derive(Clone, Debug, Default)]
pub struct GlassDamageCache {
    mask: Option<Arc<GlassMask>>,
    applied: usize,
    set: String,
    space: Option<MaskSpace>,
    mesh: usize,
    /// Local length per page UV unit times the world scale, metres.
    metres_per_uv: f32,
}

/// Per-instance glass damage.
#[derive(Component, Reflect, Clone, Debug)]
#[reflect(Component, Default)]
pub struct GlassDamage {
    /// Crack set name ([`GlassCrackSets`]; built-ins `laminate`, `pressure_pane`).
    pub set: String,
    /// Current level 0–3 (D0 intact). Escalation only raises it.
    pub level: u8,
    /// Accumulated impact energy.
    pub energy: f32,
    /// Impacts in order (saved; the page is re-stamped from them on load).
    pub impacts: Vec<GlassImpact>,
    #[reflect(ignore)]
    cache: GlassDamageCache,
}

impl Default for GlassDamage {
    fn default() -> Self {
        Self::new("laminate")
    }
}

impl GlassDamage {
    /// Intact glass using crack set `set`.
    #[must_use]
    pub fn new(set: &str) -> Self {
        Self {
            set: set.to_string(),
            level: 0,
            energy: 0.0,
            impacts: Vec::new(),
            cache: GlassDamageCache::default(),
        }
    }

    /// Queues an impact at a mesh-local point; its level comes from escalation when applied.
    /// The seed derives from the impact count and the point, so a replay is identical.
    pub fn impact(&mut self, local_point: Vec3, energy: f32) -> usize {
        let seed = impact_seed(self.impacts.len(), local_point);
        self.impacts.push(GlassImpact {
            point: local_point,
            energy: energy.max(0.0),
            level: 0,
            seed,
            size_m: 0.0,
        });
        self.impacts.len() - 1
    }

    /// Queues an impact that uses `level`'s stamps; applying it raises the instance to that level
    /// (and reports the change like an escalation does).
    pub fn impact_at_level(&mut self, local_point: Vec3, energy: f32, level: u8, size_m: f32) -> usize {
        let index = self.impact(local_point, energy);
        let level = level.clamp(1, 3);
        self.impacts[index].level = level;
        self.impacts[index].size_m = size_m.max(0.0);
        self.energy += energy.max(0.0);
        index
    }

    /// The stamped page (`None` until the first impact is applied).
    #[must_use]
    pub fn mask(&self) -> Option<&Arc<GlassMask>> {
        self.cache.mask.as_ref()
    }

    /// The page mapping of the mesh (after the first apply).
    #[must_use]
    pub fn space(&self) -> Option<MaskSpace> {
        self.cache.space
    }

    /// Metres one page UV unit spans (after the first apply).
    #[must_use]
    pub fn metres_per_uv(&self) -> f32 {
        self.cache.metres_per_uv
    }

    /// Impacts already stamped into the page.
    #[must_use]
    pub fn applied(&self) -> usize {
        self.cache.applied
    }
}

fn impact_seed(index: usize, p: Vec3) -> u32 {
    let mut h = 0x811C_9DC5u32 ^ index as u32;
    for v in p.to_array() {
        h = (h ^ v.to_bits()).wrapping_mul(0x0100_0193);
        h ^= h >> 15;
    }
    h
}

/// Loaded crack sets by name. The two built-ins are always present; a game inserts cooked sets
/// (`.glasscrack` from the cooker) under their names, replacing a built-in of the same name.
#[derive(Resource, Clone)]
pub struct GlassCrackSets {
    sets: HashMap<String, Arc<CookedCrackSet>>,
}

impl Default for GlassCrackSets {
    fn default() -> Self {
        let mut sets = HashMap::new();
        for set in [procedural::laminate_set(), procedural::pressure_pane_set()] {
            sets.insert(set.manifest.name.clone(), Arc::new(set));
        }
        Self { sets }
    }
}

impl GlassCrackSets {
    /// A set by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<CookedCrackSet>> {
        self.sets.get(name)
    }

    /// Adds or replaces a set (keyed by its manifest name).
    pub fn insert(&mut self, set: CookedCrackSet) {
        self.sets.insert(set.manifest.name.clone(), Arc::new(set));
    }

    /// Set names, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.sets.keys().cloned().collect();
        names.sort();
        names
    }
}

/// A glass instance reached a new damage level (for chunk bursts, breaches, audio).
#[derive(Message, Clone, Debug, PartialEq)]
pub struct GlassLevelChanged {
    /// The glass entity.
    pub entity: Entity,
    /// Previous level.
    pub from: u8,
    /// New level.
    pub to: u8,
    /// Glass type of its crack set.
    pub glass_type: GlassType,
    /// World position of the impact that raised it.
    pub point: Vec3,
    /// World normal of the glass there (toward the side the page's +normal faces).
    pub normal: Vec3,
    /// The crack set (for chunk-burst parameters).
    pub set: String,
}

/// Stamps every pending impact of every [`GlassDamage`] into its page (and rebuilds pages whose
/// set or mesh changed). Emits [`GlassLevelChanged`] when escalation raises a level.
#[tracing::instrument(skip_all)]
pub fn apply_glass_damage(
    sets: Option<Res<GlassCrackSets>>,
    mut glass: Query<(Entity, &mut GlassDamage, &MeshRenderer, Option<&GlobalTransform>)>,
    mut changed: MessageWriter<GlassLevelChanged>,
) {
    let _span = tracing::debug_span!("apply_glass_damage").entered();
    let fallback;
    let sets = match sets.as_deref() {
        Some(sets) => sets,
        None => {
            fallback = GlassCrackSets::default();
            &fallback
        }
    };
    for (entity, mut damage, renderer, transform) in &mut glass {
        let mesh_id = crate::runtime::mesh_key(&renderer.mesh) as usize;
        let up_to_date = damage.cache.mask.is_some()
            && damage.cache.applied == damage.impacts.len()
            && damage.cache.set == damage.set
            && damage.cache.mesh == mesh_id;
        if up_to_date || damage.impacts.is_empty() && damage.cache.mask.is_none() {
            continue;
        }
        let Some(set) = sets.get(&damage.set).cloned() else {
            if damage.cache.set != damage.set {
                tracing::warn!(target: "regia_render", "glass crack set `{}` is not loaded", damage.set);
                damage.cache.set = damage.set.clone();
            }
            continue;
        };
        let model = transform.map_or(Mat4::IDENTITY, |t| Mat4::from(t.0));
        let events = apply_one(
            &mut damage,
            &set,
            &renderer.mesh.vertices,
            &renderer.mesh.indices,
            mesh_id,
            model,
        );
        for (from, to, local) in events {
            let point = model.transform_point3(local);
            let normal = damage
                .cache
                .space
                .map(|s| {
                    let (u, v) = s.world_axes(model);
                    u.cross(v).normalize_or_zero()
                })
                .unwrap_or(Vec3::Z);
            changed.write(GlassLevelChanged {
                entity,
                from,
                to,
                glass_type: set.manifest.glass_type,
                point,
                normal,
                set: set.manifest.name.clone(),
            });
        }
    }
}

/// Applies pending impacts of one instance; returns level changes `(from, to, local point)`.
pub fn apply_one(
    damage: &mut GlassDamage,
    set: &CookedCrackSet,
    vertices: &[crate::LabVertex],
    indices: &[u32],
    mesh_id: usize,
    model: Mat4,
) -> Vec<(u8, u8, Vec3)> {
    let m = &set.manifest;
    let rebuild = damage.cache.mask.as_ref().is_none_or(|p| p.size != m.page_size)
        || damage.cache.set != damage.set
        || damage.cache.mesh != mesh_id;
    if rebuild {
        let space = MaskSpace::from_mesh(vertices, m.damage_space);
        let scale = model
            .x_axis
            .truncate()
            .length()
            .max(model.y_axis.truncate().length())
            .max(model.z_axis.truncate().length())
            .max(1e-6);
        damage.cache = GlassDamageCache {
            mask: Some(Arc::new(GlassMask::new(m.page_size))),
            applied: 0,
            set: damage.set.clone(),
            space: Some(space),
            mesh: mesh_id,
            metres_per_uv: space.extent * scale,
        };
    }
    let Some(space) = damage.cache.space else {
        return Vec::new();
    };
    let world_scale = damage.cache.metres_per_uv / space.extent.max(1e-6);
    let mut mask = damage
        .cache
        .mask
        .take()
        .map(Arc::unwrap_or_clone)
        .unwrap_or_else(|| GlassMask::new(m.page_size));
    let page = m.page_size as f32;
    let mut events = Vec::new();
    for index in damage.cache.applied..damage.impacts.len() {
        let mut impact = damage.impacts[index];
        if impact.level == 0 {
            let from = damage.level;
            damage.energy += impact.energy;
            // Any recorded impact is at least D1, even when the energy is still under the
            // D1 threshold (a light calibre is 0.6; the laminate's D1 is 1).
            damage.level = damage.level.max(m.escalation.level_for(damage.energy)).max(1);
            impact.level = damage.level;
            damage.impacts[index] = impact;
            if damage.level != from {
                events.push((from, damage.level, impact.point));
            }
        } else if impact.level > damage.level {
            // An explicit-level impact (a replay already holds its level, so it reports nothing).
            events.push((damage.level, impact.level, impact.point));
            damage.level = impact.level;
        }
        let Some((uv, local_per_uv)) = space.hit_uv(vertices, indices, impact.point) else {
            continue;
        };
        let metres_per_uv = local_per_uv * world_scale;
        let range = set.level_range(impact.level);
        let decls = m.stamps(impact.level);
        if decls.is_empty() {
            continue;
        }
        let pick = weighted_pick(decls.iter().map(|d| d.weight), impact.seed);
        let (decl, px) = (&decls[pick], &set.stamps[range.start + pick]);
        let size = if impact.size_m > 0.0 {
            let aspect = decl.size_m.1 / decl.size_m.0.max(1e-6);
            (impact.size_m, impact.size_m * aspect)
        } else {
            decl.size_m
        };
        let to_texels = page / metres_per_uv.max(1e-6);
        let rotation = if decl.random_rotation {
            (impact.seed >> 8) as f32 / (1u32 << 24) as f32 * std::f32::consts::TAU
        } else {
            0.0
        };
        let (hole_radius, hole_scale, halo_radius) = match m.laminate {
            Some(rules) => (
                rules.max_hole_radius_m * to_texels,
                if impact.level == 3 && impact.energy >= rules.min_hole_energy {
                    1.0
                } else {
                    0.0
                },
                if impact.level >= 2 {
                    rules.halo_radius_m * to_texels
                } else {
                    0.0
                },
            ),
            None => (f32::INFINITY, 1.0, 0.0),
        };
        mask.stamp(
            px,
            &StampPlacement {
                center: [uv.x * page, uv.y * page],
                footprint: [size.0 * to_texels, size.1 * to_texels],
                pivot: [decl.pivot.0, decl.pivot.1],
                rotation,
                hole_radius,
                hole_scale,
                halo_radius,
            },
        );
    }
    damage.cache.applied = damage.impacts.len();
    damage.cache.mask = Some(Arc::new(mask));
    events
}

fn weighted_pick(weights: impl Iterator<Item = f32> + Clone, seed: u32) -> usize {
    let total: f32 = weights.clone().sum();
    let mut t = (seed & 0xFFFF) as f32 / 65536.0 * total;
    let mut last = 0;
    for (i, w) in weights.enumerate() {
        last = i;
        if t < w {
            return i;
        }
        t -= w;
    }
    last
}

/// Everything the renderer needs to draw one glass instance specially.
#[derive(Clone, Debug)]
pub struct GlassInstance {
    /// [`crate::FrameMesh::object_key`] of the instance.
    pub object_key: u64,
    /// [`GlassViewMode::ViewerInside`]: no refraction, specular × [`Self::specular_scale`].
    pub inside: bool,
    /// Specular multiplier (1 exterior).
    pub specular_scale: f32,
    /// Crack page and shading, when damaged.
    pub damage: Option<GlassDamageDraw>,
}

/// The damage part of a [`GlassInstance`].
#[derive(Clone, Debug)]
pub struct GlassDamageDraw {
    /// The page (re-uploaded when its version changes).
    pub mask: Arc<GlassMask>,
    /// Sample the page with the mesh's UV0 (else the planar mapping below).
    pub uv0: bool,
    /// World → page UV (xy), column-major (planar).
    pub mask_from_world: [[f32; 4]; 4],
    /// World direction of page +U.
    pub axis_u: [f32; 3],
    /// World direction of page +V (image down).
    pub axis_v: [f32; 3],
    /// Metres per page UV unit.
    pub metres_per_uv: f32,
    /// Channel shading.
    pub shading: CrackShading,
    /// Glass type (pressure panes add the clear inner pane).
    pub glass_type: GlassType,
    /// Pressure pane: outer-to-inner distance, metres.
    pub pane_gap_m: f32,
    /// Pressure pane: inner pane tint (sRGB 0..1).
    pub inner_tint: [f32; 3],
}

/// The renderer-facing description of one entity's glass state (`None` when it has neither
/// component, or a damage component without a page and the exterior view).
#[must_use]
pub fn glass_instance(
    object_key: u64,
    view: Option<&GlassView>,
    damage: Option<&GlassDamage>,
    sets: &GlassCrackSets,
    model: Mat4,
) -> Option<GlassInstance> {
    let inside = view.is_some_and(|v| v.mode == GlassViewMode::ViewerInside);
    let damage = damage.and_then(|d| {
        let mask = d.cache.mask.clone()?;
        let space = d.cache.space?;
        let set = sets.get(&d.cache.set)?;
        let (u, v) = space.world_axes(model);
        let pp = set.manifest.pressure_pane.clone().unwrap_or_default();
        Some(GlassDamageDraw {
            mask,
            uv0: space.mode == regia_asset::glasscrack::DamageSpace::Uv0,
            mask_from_world: space.mask_from_world(model).to_cols_array_2d(),
            axis_u: u.to_array(),
            axis_v: v.to_array(),
            metres_per_uv: d.cache.metres_per_uv,
            shading: set.manifest.shading,
            glass_type: set.manifest.glass_type,
            pane_gap_m: pp.pane_gap_m,
            inner_tint: pp.inner_tint,
        })
    });
    if !inside && damage.is_none() {
        return None;
    }
    Some(GlassInstance {
        object_key,
        inside,
        specular_scale: if inside {
            view.map_or(INSIDE_SPECULAR_DEFAULT, |v| v.inside_specular.clamp(0.0, 1.0))
        } else {
            1.0
        },
        damage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_layer_only_defaults_to_keeping_every_sheet() {
        assert!(!CanopyGlass::default().nearest_layer_only);
    }

    fn pane() -> (Vec<crate::LabVertex>, Vec<u32>) {
        let (v, i) = crate::box_mesh([1.0, 1.0, 0.005]);
        (v, i)
    }

    #[test]
    fn escalation_raises_levels_and_reports_them() {
        let set = procedural::laminate_set();
        let (v, i) = pane();
        let mut d = GlassDamage::new("laminate");
        d.impact(Vec3::new(0.2, 0.1, 0.0), 1.0);
        let ev = apply_one(&mut d, &set, &v, &i, 1, Mat4::IDENTITY);
        assert_eq!(d.level, 1);
        assert_eq!(ev, vec![(0, 1, Vec3::new(0.2, 0.1, 0.0))]);
        for k in 0..9 {
            d.impact(Vec3::new(-0.5 + k as f32 * 0.1, 0.0, 0.0), 1.0);
        }
        let ev = apply_one(&mut d, &set, &v, &i, 1, Mat4::IDENTITY);
        assert_eq!(d.level, 3);
        assert_eq!(ev.iter().map(|e| e.1).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(d.applied(), 10);
    }

    /// A light calibre (0.6) is under the laminate D1 energy (1) and is still stamped D1.
    #[test]
    fn a_light_hit_is_at_least_d1() {
        let set = procedural::laminate_set();
        let (v, i) = pane();
        let mut d = GlassDamage::new("laminate");
        d.impact(Vec3::new(0.2, 0.1, 0.0), 0.6);
        let ev = apply_one(&mut d, &set, &v, &i, 1, Mat4::IDENTITY);
        assert_eq!(d.level, 1);
        assert_eq!(d.impacts[0].level, 1);
        assert!((d.energy - 0.6).abs() < 1e-6);
        assert!((d.impacts[0].energy - 0.6).abs() < 1e-6);
        assert_eq!(ev, vec![(0, 1, Vec3::new(0.2, 0.1, 0.0))]);
    }

    #[test]
    fn replaying_the_saved_impact_list_rebuilds_the_same_page() {
        let set = procedural::pressure_pane_set();
        let (v, i) = pane();
        let mut d = GlassDamage::new("pressure_pane");
        d.impact(Vec3::new(0.3, -0.2, 0.0), 2.0);
        d.impact_at_level(Vec3::new(-0.4, 0.4, 0.0), 1.0, 2, 0.0);
        apply_one(&mut d, &set, &v, &i, 7, Mat4::IDENTITY);
        let page = d.mask().expect("page").rgba.clone();
        // A save keeps the reflected fields only.
        let mut loaded = GlassDamage {
            set: d.set.clone(),
            level: d.level,
            energy: d.energy,
            impacts: d.impacts.clone(),
            cache: GlassDamageCache::default(),
        };
        let ev = apply_one(&mut loaded, &set, &v, &i, 7, Mat4::IDENTITY);
        assert!(ev.is_empty(), "a replay must not re-escalate");
        assert_eq!(loaded.energy, d.energy);
        assert_eq!(loaded.mask().expect("page").rgba, page);
    }

    #[test]
    fn laminate_d3_holes_stay_within_the_radius_and_only_at_hard_hits() {
        // Only the punching D3 variant, so every hard hit tries to cut a hole.
        let mut set = procedural::laminate_set();
        let d3 = set.level_range(3);
        set.stamps.truncate(d3.start + 1);
        set.manifest.d3.truncate(1);
        let rules = set.manifest.laminate.expect("laminate rules");
        let (v, i) = pane();
        let mut d = GlassDamage::new("laminate");
        // Hard hits (energy 6 >= 3) and soft ones (1), all at D3, oversized stamps included.
        let hard = [
            Vec3::new(-0.5, 0.3, 0.0),
            Vec3::new(0.4, -0.2, 0.0),
            Vec3::new(0.0, 0.6, 0.0),
        ];
        let soft = [Vec3::new(0.6, 0.6, 0.0), Vec3::new(-0.6, -0.6, 0.0)];
        for p in hard {
            d.impact_at_level(p, 6.0, 3, 1.5);
        }
        for p in soft {
            d.impact_at_level(p, 1.0, 3, 1.5);
        }
        apply_one(&mut d, &set, &v, &i, 1, Mat4::IDENTITY);
        let space = d.space().expect("space");
        let mask = d.mask().expect("page");
        let n = mask.size;
        let texels_per_m = n as f32 / d.metres_per_uv();
        let centres: Vec<(f32, f32)> = hard
            .iter()
            .map(|p| {
                let uv = space.planar_uv(*p);
                (uv.x * n as f32, uv.y * n as f32)
            })
            .collect();
        let limit = rules.max_hole_radius_m * texels_per_m + 1.0;
        let mut holes = 0;
        for y in 0..n {
            for x in 0..n {
                if mask.texel(x, y)[2] == 0 {
                    continue;
                }
                holes += 1;
                let near = centres.iter().any(|(cx, cy)| {
                    ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt() <= limit
                });
                assert!(
                    near,
                    "hole texel ({x}, {y}) beyond {limit} texels of every hard hit"
                );
            }
        }
        assert!(holes > 0, "hard D3 hits must punch small holes");
    }

    #[test]
    fn canopy_settings_round_trip_and_parse_the_env_value() {
        for shading in [CanopyShading::Cheap, CanopyShading::Full] {
            for nearest_layer_only in [false, true] {
                let s = CanopyGlass {
                    shading,
                    nearest_layer_only,
                };
                assert_eq!(canopy_from_bits(canopy_bits(s)), s);
            }
        }
        assert_eq!(canopy_from_bits(0), CanopyGlass::default());
        assert_eq!(CanopyGlass::default().shading, CanopyShading::Cheap);
        for on in ["1", "true", " ON ", "yes"] {
            assert!(canopy_full_value(on), "{on}");
        }
        for off in ["", "0", "false", "off", "full"] {
            assert!(!canopy_full_value(off), "{off}");
        }
        // A renderer override beats the process-wide value (no env in unit tests).
        if !canopy_full_env() {
            let full = CanopyGlass {
                shading: CanopyShading::Full,
                ..CanopyGlass::default()
            };
            assert_eq!(resolve_canopy(Some(full)), full);
        }
    }

    #[test]
    fn update_canopy_glass_edits_one_field_and_reports_both_sides() {
        // The only unit test that writes the process-wide value; it puts the old one back.
        let previous = canopy_glass_setting();
        set_canopy_glass(CanopyGlass {
            shading: CanopyShading::Full,
            nearest_layer_only: false,
        });
        let (before, after) = update_canopy_glass(|s| CanopyGlass {
            nearest_layer_only: true,
            ..s
        });
        set_canopy_glass(previous);
        assert_eq!(before.shading, CanopyShading::Full);
        assert!(!before.nearest_layer_only);
        assert_eq!(after.shading, CanopyShading::Full, "the other field is kept");
        assert!(after.nearest_layer_only);
    }

    #[test]
    fn instances_only_for_glass_components() {
        let sets = GlassCrackSets::default();
        assert!(glass_instance(1, None, None, &sets, Mat4::IDENTITY).is_none());
        assert!(glass_instance(1, Some(&GlassView::default()), None, &sets, Mat4::IDENTITY).is_none());
        let inside = glass_instance(1, Some(&GlassView::viewer_inside()), None, &sets, Mat4::IDENTITY)
            .expect("inside");
        assert!(inside.inside && (inside.specular_scale - 0.15).abs() < 1e-6);
        // Damage without an applied impact draws normally.
        assert!(
            glass_instance(
                1,
                None,
                Some(&GlassDamage::new("laminate")),
                &sets,
                Mat4::IDENTITY
            )
            .is_none()
        );
    }
}
