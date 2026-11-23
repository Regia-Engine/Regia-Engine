// SPDX-License-Identifier: MPL-2.0
//! Audio zones beyond the reverb (`docs/specs/16-audio.md` § 3d, ADR-250): the distance scale a
//! zone gives sound shared inside it, occlusion by the thickness of rock between listener and
//! emitter, and routing a sound out of a cavity through its mouths.
//!
//! Everything here is pure math over a [`ZoneField`] (the zones placed in the world) and an
//! optional [`RockField`] (the ground surface). The audio crate does not know about terrain: a
//! host installs a [`RockField`] on [`AudioRockField`] (a closure over its heightfield and holes
//! mask is enough), and without one no rock is ever reported.
//!
//! - **Distance scale.** An emitter and a listener that both stand in a zone with
//!   `distance_scale = 1.5` hear the emitter as if it were 1.5× closer: [`scaled_range`] stretches
//!   the emitter's `min_distance`, `max_distance` and rolloff so every curve evaluates at
//!   `distance / scale`.
//! - **Rock occlusion.** [`rock_thickness`] marches the segment from listener to emitter and sums
//!   the length below the ground surface that is neither a hole in the terrain nor inside a
//!   cavity zone ([`AudioReverbZone::cavity`]). [`RockTuning`] maps the metres to a gain (dB per
//!   metre, capped) and a low-pass cutoff.
//! - **Mouth routing.** A cavity zone may list [`ZonePortal`] points (its mouths). When the
//!   emitter is inside the zone and the listener is not, [`ZoneField::portal_route`] returns a
//!   virtual source: in the direction of the best mouth as seen from the listener, at the length of
//!   the path through it. A zone without portals routes nothing.

use std::sync::Arc;

use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;

use crate::reverb_zone::{AudioReverbZone, specificity_order};
use crate::spatial::{AudioEmitter, RolloffCurve};

/// A mouth of a cavity zone, in the zone entity's local frame.
#[derive(Reflect, Clone, Copy, Debug, PartialEq)]
pub struct ZonePortal {
    /// Mouth centre, local to the zone entity, metres.
    pub position: Vec3,
    /// Radius within which a listener counts as standing in the mouth, metres.
    pub radius: f32,
}

/// The mouths of the cavity zone on the same entity (supplied by the map bake). Empty or absent
/// means no mouth routing: sound through rock is occluded by thickness only.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct AudioZonePortals {
    /// The mouths.
    pub portals: Vec<ZonePortal>,
}

/// The ground surface as audio sees it: where there is rock under a point.
///
/// A heightfield host implements [`RockField::surface_height`] only. A host with cooked world
/// geometry ([`crate::GeomSoundField`]) also answers the rock on a segment exactly
/// ([`RockField::rock_between`]), routes sound through its baked portals
/// ([`RockField::sound_route`]) and arms the reverb of the section a listener stands in
/// ([`RockField::armed_zone`]); the defaults leave all three to the zones.
pub trait RockField: Send + Sync {
    /// Height of the rock surface at `(x, z)`, metres. `None` where there is no rock to
    /// speak of: outside the terrain or inside a terrain hole (a tunnel mouth cut-out).
    fn surface_height(&self, x: f32, z: f32) -> Option<f32>;

    /// Metres of rock on the segment `from → to` when the field knows it exactly. `None` (the
    /// default) makes [`rock_thickness`] march [`RockField::surface_height`] instead.
    fn rock_between(&self, from: Vec3, to: Vec3) -> Option<f32> {
        let _ = (from, to);
        None
    }

    /// How sound from `emitter` reaches `listener` through the field's own portals. `None` (the
    /// default) leaves routing to the cavity zones' mouths ([`ZoneField::portal_route`]).
    fn sound_route(&self, listener: Vec3, emitter: Vec3) -> Option<FieldRoute> {
        let _ = (listener, emitter);
        None
    }

    /// The reverb zone the place of `listener` arms (a baked section's preset). `None` by default.
    fn armed_zone(&self, listener: Vec3) -> Option<ArmedZone> {
        let _ = listener;
        None
    }

    /// The cooked-geometry field behind this one, for tools and the agent protocol.
    fn as_geom(&self) -> Option<&crate::GeomSoundField> {
        None
    }
}

/// A route from [`RockField::sound_route`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldRoute {
    /// The virtual source through the portals; `None` for a straight (direct or occluded) path.
    pub route: Option<PortalRoute>,
    /// Hearing-range scale for this listener (stands in for the zones' distance scale).
    pub range_scale: f32,
}

/// The reverb a [`RockField`] arms at a listener's place.
#[derive(Clone, Debug, PartialEq)]
pub struct ArmedZone {
    /// The baked section the listener stands in.
    pub section: u32,
    /// The preset id from the bake (`tube`, `gallery` or `open`).
    pub preset_id: String,
    /// The zone that preset makes ([`crate::ReverbPreset::from_name`]), global in extent.
    pub zone: AudioReverbZone,
}

/// A [`RockField`] from a closure.
pub struct FnRockField<F: Fn(f32, f32) -> Option<f32> + Send + Sync>(pub F);

impl<F: Fn(f32, f32) -> Option<f32> + Send + Sync> RockField for FnRockField<F> {
    fn surface_height(&self, x: f32, z: f32) -> Option<f32> {
        (self.0)(x, z)
    }
}

/// The installed [`RockField`] (none by default: no rock occlusion).
#[derive(Resource, Clone, Default)]
pub struct AudioRockField {
    field: Option<Arc<dyn RockField>>,
}

impl AudioRockField {
    /// Installs `field`.
    pub fn set(&mut self, field: Arc<dyn RockField>) {
        self.field = Some(field);
    }

    /// Removes the field: no rock is reported afterwards.
    pub fn clear(&mut self) {
        self.field = None;
    }

    /// The installed field.
    pub fn get(&self) -> Option<&dyn RockField> {
        self.field.as_deref()
    }

    /// The installed field, shared (the `Audio` resource keeps one for its emitter routes).
    pub fn shared(&self) -> Option<Arc<dyn RockField>> {
        self.field.clone()
    }
}

impl std::fmt::Debug for AudioRockField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioRockField")
            .field("installed", &self.field.is_some())
            .finish()
    }
}

/// How rock thickness sounds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RockTuning {
    /// Marching step along the ray, metres (thickness is exact to about this).
    pub sample_step_m: f32,
    /// Samples per ray at most (a long ray gets a coarser step).
    pub max_samples: usize,
    /// Gain lost per metre of rock, dB.
    pub db_per_metre: f32,
    /// Most gain rock can remove, dB.
    pub max_db: f32,
    /// Thickness at which the low-pass reaches `min_cutoff_hz`, metres.
    pub full_cutoff_m: f32,
    /// Low-pass cutoff behind `full_cutoff_m` of rock or more, Hz.
    pub min_cutoff_hz: f32,
}

impl Default for RockTuning {
    fn default() -> Self {
        Self {
            sample_step_m: 1.0,
            max_samples: 256,
            db_per_metre: 2.5,
            max_db: 60.0,
            full_cutoff_m: 6.0,
            min_cutoff_hz: 350.0,
        }
    }
}

impl RockTuning {
    /// Amplitude factor of `thickness_m` of rock.
    pub fn gain(&self, thickness_m: f32) -> f32 {
        let db = (thickness_m.max(0.0) * self.db_per_metre).min(self.max_db.max(0.0));
        let gain = crate::spatial::db_to_gain(-db);
        if gain.is_finite() { gain } else { 1.0 }
    }

    /// Low-pass cutoff (Hz) behind `thickness_m` of rock: 20 kHz open, `min_cutoff_hz` at
    /// `full_cutoff_m`, log-interpolated.
    pub fn cutoff_hz(&self, thickness_m: f32) -> f64 {
        let open = 20_000.0f32.ln();
        // A NaN tuning value must not reach the emitter track's filter (its state would stay NaN).
        let closed_hz = if self.min_cutoff_hz.is_nan() {
            20_000.0
        } else {
            self.min_cutoff_hz.clamp(20.0, 20_000.0)
        };
        let closed = closed_hz.ln();
        let t = (thickness_m.max(0.0) / self.full_cutoff_m.max(1e-3)).clamp(0.0, 1.0);
        f64::from((open + (closed - open) * t).exp())
    }
}

/// One zone placed in the world.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedZone {
    /// The zone.
    pub zone: AudioReverbZone,
    /// World-space centre.
    pub centre: Vec3,
    /// World-space orientation.
    pub rotation: Quat,
    /// World-space mouths with their radii.
    pub portals: Vec<(Vec3, f32)>,
}

impl PlacedZone {
    /// Places `zone` with the (local) `portals` at `centre`/`rotation`.
    pub fn new(zone: AudioReverbZone, centre: Vec3, rotation: Quat, portals: &[ZonePortal]) -> Self {
        Self {
            zone,
            centre,
            rotation,
            portals: portals
                .iter()
                .map(|p| (centre + rotation * p.position, p.radius))
                .collect(),
        }
    }

    /// Membership weight of `point` (1 inside, fading over the blend distance).
    pub fn weight(&self, point: Vec3) -> f32 {
        self.zone.weight(self.centre, self.rotation, point)
    }

    /// Whether `point` is strictly inside the volume.
    pub fn contains(&self, point: Vec3) -> bool {
        self.zone.distance_outside(self.centre, self.rotation, point) <= 0.0
    }
}

/// A virtual source standing in for an emitter heard through a mouth.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PortalRoute {
    /// Where the sound enters: the mouth the listener should be tested against.
    pub mouth: Vec3,
    /// The virtual source position: from the listener toward the mouth, at the path length.
    pub position: Vec3,
    /// Path length emitter → mouth → listener, metres.
    pub path_length: f32,
}

/// The zones placed in the world for one frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZoneField {
    /// The zones.
    pub zones: Vec<PlacedZone>,
}

impl ZoneField {
    /// Whether any zone is placed.
    pub fn is_empty(&self) -> bool {
        self.zones.is_empty()
    }

    /// The distance scale for sound between `listener` and `emitter`: each zone contributes the
    /// lower of the two memberships toward its scale, layered least specific first (see
    /// [`crate::reverb_mix`]). 1 when no zone is shared.
    pub fn distance_scale(&self, listener: Vec3, emitter: Vec3) -> f32 {
        let mut layers: Vec<(&PlacedZone, f32)> = self
            .zones
            .iter()
            .map(|z| (z, z.weight(listener).min(z.weight(emitter))))
            .filter(|(z, shared)| {
                *shared > 0.0
                    && z.zone.distance_scale.is_finite()
                    && (z.zone.distance_scale - 1.0).abs() > 1e-6
            })
            .collect();
        specificity_order(&mut layers, |(z, _)| (z.zone.priority, z.zone.volume()));
        let mut scale = 1.0f32;
        for (z, shared) in layers {
            let target = z.zone.distance_scale.clamp(0.1, 10.0);
            scale += (target - scale) * shared;
        }
        scale
    }

    /// Whether `point` is inside a cavity zone (air in rock).
    pub fn in_cavity(&self, point: Vec3) -> bool {
        self.zones.iter().any(|z| z.zone.cavity && z.contains(point))
    }

    /// The cavity zone the point is inside, innermost first (smallest volume).
    pub fn cavity_zone_index(&self, point: Vec3) -> Option<usize> {
        self.zones
            .iter()
            .enumerate()
            .filter(|(_, z)| z.zone.cavity && z.contains(point))
            .min_by(|(_, a), (_, b)| a.zone.volume().total_cmp(&b.zone.volume()))
            .map(|(i, _)| i)
    }

    /// Routes `emitter` through a mouth of the cavity zone it is in when `listener` is not in
    /// that zone. `None` when the emitter is in no cavity, the listener shares it, or the zone
    /// has no mouths (the no-portal path: nothing changes).
    pub fn portal_route(&self, listener: Vec3, emitter: Vec3) -> Option<PortalRoute> {
        let index = self.cavity_zone_index(emitter)?;
        let zone = &self.zones[index];
        if zone.portals.is_empty() || zone.contains(listener) {
            return None;
        }
        let path = |mouth: Vec3| (mouth, emitter.distance(mouth) + mouth.distance(listener));
        // A listener standing in a mouth (within its radius) hears through that mouth, even when
        // another mouth gives a shorter path; otherwise the shortest path wins. Returning `None`
        // here would send the sound through the rock instead (the unrouted path), muting it.
        let standing_in = zone
            .portals
            .iter()
            .filter(|(mouth, radius)| listener.distance(*mouth) <= *radius)
            .min_by(|a, b| listener.distance(a.0).total_cmp(&listener.distance(b.0)))
            .map(|(mouth, _)| path(*mouth));
        let (mouth, path_length) = match standing_in {
            Some(best) => best,
            None => zone
                .portals
                .iter()
                .map(|(mouth, _)| path(*mouth))
                .min_by(|a, b| a.1.total_cmp(&b.1))?,
        };
        // On the mouth centre the listener-to-mouth direction is undefined: the sound comes
        // out of the mouth from the cavity, so it arrives from the emitter's side.
        let heading = match (mouth - listener).try_normalize() {
            Some(heading) => heading,
            None => (emitter - listener).normalize_or_zero(),
        };
        if heading == Vec3::ZERO {
            return None;
        }
        Some(PortalRoute {
            mouth,
            position: listener + heading * path_length,
            path_length,
        })
    }
}

/// `emitter` with every distance-based parameter stretched by `scale`, so spatialization at
/// distance `d` equals the original at `d / scale`.
pub fn scaled_range(emitter: &AudioEmitter, scale: f32) -> AudioEmitter {
    let mut out = emitter.clone();
    if (scale - 1.0).abs() < 1e-6 || !scale.is_finite() || scale <= 0.0 {
        return out;
    }
    out.min_distance *= scale;
    out.max_distance *= scale;
    if let RolloffCurve::Custom(points) = &mut out.rolloff {
        for point in points.iter_mut() {
            point.0 *= scale;
        }
    }
    out
}

/// Metres of rock on the segment `from → to`: the length below the surface of `rock` that is
/// outside every cavity zone of `zones`.
pub fn rock_thickness(
    rock: &dyn RockField,
    zones: &ZoneField,
    tuning: &RockTuning,
    from: Vec3,
    to: Vec3,
) -> f32 {
    let span = to - from;
    let length = span.length();
    if !length.is_finite() || length < 1e-4 {
        return 0.0;
    }
    if let Some(exact) = rock.rock_between(from, to) {
        return if exact.is_finite() { exact.max(0.0) } else { 0.0 };
    }
    let samples =
        ((length / tuning.sample_step_m.max(0.05)).ceil() as usize).clamp(1, tuning.max_samples.max(1));
    let step = length / samples as f32;
    let mut inside = 0usize;
    for i in 0..samples {
        let p = from + span * ((i as f32 + 0.5) / samples as f32);
        if rock.surface_height(p.x, p.z).is_some_and(|h| p.y < h) && !zones.in_cavity(p) {
            inside += 1;
        }
    }
    inside as f32 * step
}

/// What a listener hears of an emitter through the zones, before any smoothing: the geometry
/// behind `Audio`'s per-frame emitter update, usable without an app (tools, the agent protocol,
/// tests).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoneHearing {
    /// Distance scale in effect (1 when routed or no zone is shared; the field's range scale when
    /// a [`RockField::sound_route`] answers).
    pub distance_scale: f32,
    /// The mouth route, when the sound comes out of a cavity through a mouth.
    pub route: Option<PortalRoute>,
    /// Distance the attenuation curve sees, metres (the path length when routed).
    pub distance: f32,
    /// Metres of rock on the ray to the emitter (or to its mouth); `None` without a rock field.
    pub rock_m: Option<f32>,
    /// Distance attenuation times rock gain, linear amplitude.
    pub gain: f32,
}

impl ZoneHearing {
    /// Evaluates the hearing of `emitter` (with `params`) at `listener`.
    pub fn evaluate(
        field: &ZoneField,
        rock: Option<&dyn RockField>,
        tuning: &RockTuning,
        params: &AudioEmitter,
        listener: Vec3,
        emitter: Vec3,
    ) -> Self {
        let field_route = rock.and_then(|r| r.sound_route(listener, emitter));
        let route = match field_route {
            Some(f) => f.route,
            None => field.portal_route(listener, emitter),
        };
        let heard_from = route.map_or(emitter, |r| r.position);
        let distance_scale = match field_route {
            Some(f) => f.range_scale,
            None if route.is_some() => 1.0,
            None => field.distance_scale(listener, emitter),
        };
        let scaled = scaled_range(params, distance_scale);
        let distance = listener.distance(heard_from);
        let attenuation = crate::spatial::distance_gain_faded(
            &scaled.rolloff,
            scaled.min_distance,
            scaled.max_distance,
            distance,
            scaled.fade_start,
        );
        let target = route.map_or(emitter, |r| r.mouth);
        let rock_m = rock.map(|r| rock_thickness(r, field, tuning, listener, target));
        Self {
            distance_scale,
            route,
            distance,
            rock_m,
            gain: attenuation * rock_m.map_or(1.0, |m| tuning.gain(m)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reverb_zone::ReverbZoneShape;

    /// Flat ground at height 0 with a hole around x ∈ [−10, 10] at z ∈ [−5, 5].
    fn ground() -> impl RockField {
        FnRockField(|x: f32, z: f32| {
            (!((-10.0..=10.0).contains(&x) && (-5.0..=5.0).contains(&z))).then_some(0.0)
        })
    }

    /// A tunnel along X at y = −10, radius 3, from x = −100 to 100 (a capsule rotated about Z).
    fn tunnel() -> PlacedZone {
        PlacedZone::new(
            AudioReverbZone::tube(ReverbZoneShape::Capsule {
                radius: 3.0,
                half_length: 100.0,
            }),
            Vec3::new(0.0, -10.0, 0.0),
            Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            &[],
        )
    }

    #[test]
    fn a_nan_distance_scale_is_ignored_and_does_not_wipe_later_zones() {
        let zone = |scale: f32, priority: i32| {
            let mut z = AudioReverbZone::open(ReverbZoneShape::Global);
            z.distance_scale = scale;
            z.priority = priority;
            PlacedZone::new(z, Vec3::ZERO, Quat::IDENTITY, &[])
        };
        let field = ZoneField {
            zones: vec![zone(f32::NAN, 0), zone(2.0, 1)],
        };
        let scale = field.distance_scale(Vec3::ZERO, Vec3::X);
        assert!((scale - 2.0).abs() < 1e-5, "{scale}");
    }

    #[test]
    fn capsule_distance_and_membership() {
        let z = tunnel();
        assert!(z.contains(Vec3::new(50.0, -10.0, 0.0)));
        assert!(z.contains(Vec3::new(50.0, -8.0, 2.0)));
        assert!(!z.contains(Vec3::new(50.0, -5.0, 0.0)));
        // Past the end cap: 3 m beyond the sphere's edge.
        let out = z
            .zone
            .distance_outside(z.centre, z.rotation, Vec3::new(106.0, -10.0, 0.0));
        assert!((out - 3.0).abs() < 1e-4, "{out}");
    }

    #[test]
    fn presets_map_to_reverb_parameters() {
        let tube = AudioReverbZone::tube(ReverbZoneShape::Global);
        let gallery = AudioReverbZone::mine_gallery(ReverbZoneShape::Global);
        let open = AudioReverbZone::open(ReverbZoneShape::Global);
        assert!(tube.feedback > gallery.feedback && gallery.feedback > open.feedback);
        assert_eq!(
            (tube.distance_scale, gallery.distance_scale, open.distance_scale),
            (1.5, 1.5, 1.0)
        );
        assert!(tube.cavity && gallery.cavity && !open.cavity);
        assert!(open.send_db < tube.send_db, "open ground is nearly dry");
        assert_eq!(tube.preset, crate::ReverbPreset::Tube);
        for p in [
            crate::ReverbPreset::Custom,
            crate::ReverbPreset::Tube,
            crate::ReverbPreset::MineGallery,
            crate::ReverbPreset::Open,
        ] {
            assert_eq!(crate::ReverbPreset::from_name(p.name()), Some(p));
        }
        assert_eq!(crate::ReverbPreset::from_name("cathedral"), None);
        assert_eq!(AudioReverbZone::default().distance_scale, 1.0);
    }

    #[test]
    fn mouth_blends_the_open_reverb_into_the_tube() {
        let open = AudioReverbZone::open(ReverbZoneShape::Global);
        let mut tube = AudioReverbZone::tube(ReverbZoneShape::Capsule {
            radius: 3.0,
            half_length: 100.0,
        });
        tube.fade_distance = 6.0;
        let rot = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
        let c = Vec3::new(0.0, -10.0, 0.0);
        let zones = [(&open, Vec3::ZERO, Quat::IDENTITY), (&tube, c, rot)];
        let inside = crate::reverb_mix(c, zones);
        let outside = crate::reverb_mix(Vec3::new(0.0, 20.0, 0.0), zones);
        // 3 m beyond the wall (half the blend distance).
        let edge = crate::reverb_mix(Vec3::new(0.0, -10.0 + 3.0 + 3.0, 0.0), zones);
        assert!((inside.feedback - tube.feedback).abs() < 1e-5);
        assert!((outside.feedback - open.feedback).abs() < 1e-5);
        let mid = (tube.feedback + open.feedback) / 2.0;
        assert!((edge.feedback - mid).abs() < 1e-4, "{} vs {mid}", edge.feedback);
        assert_eq!(inside.weight, 1.0);
    }

    #[test]
    fn distance_scale_needs_a_shared_zone_and_blends_at_the_edge() {
        let field = ZoneField {
            zones: vec![tunnel()],
        };
        let a = Vec3::new(-30.0, -10.0, 0.0);
        let b = Vec3::new(30.0, -10.0, 0.0);
        let surface = Vec3::new(30.0, 5.0, 0.0);
        assert!((field.distance_scale(a, b) - 1.5).abs() < 1e-5);
        assert_eq!(
            field.distance_scale(a, surface),
            1.0,
            "the surface listener is outside"
        );
        // Listener half-way into the blend distance: shared membership 0.5 → scale 1.25.
        let mid = Vec3::new(30.0, -10.0 + 3.0 + 3.0, 0.0);
        assert!((field.distance_scale(a, mid) - 1.25).abs() < 1e-4);
        assert_eq!(ZoneField::default().distance_scale(a, b), 1.0);
    }

    #[test]
    fn scaled_range_matches_evaluating_at_distance_over_scale() {
        let mut emitter = AudioEmitter {
            min_distance: 2.0,
            max_distance: 100.0,
            ..AudioEmitter::default()
        };
        for rolloff in [
            RolloffCurve::Inverse,
            RolloffCurve::Linear,
            RolloffCurve::Log { db_per_doubling: 6.0 },
            RolloffCurve::Custom(vec![(2.0, 1.0), (50.0, 0.25), (100.0, 0.0)]),
        ] {
            emitter.rolloff = rolloff.clone();
            let scaled = scaled_range(&emitter, 1.5);
            for d in [0.5f32, 3.0, 20.0, 60.0, 99.0, 140.0, 160.0] {
                let a = crate::distance_gain(&scaled.rolloff, scaled.min_distance, scaled.max_distance, d);
                let b = crate::distance_gain(&rolloff, 2.0, 100.0, d / 1.5);
                assert!((a - b).abs() < 1e-4, "{rolloff:?} d={d}: {a} vs {b}");
            }
        }
        assert_eq!(scaled_range(&emitter, 1.0), emitter);
        assert_eq!(scaled_range(&emitter, f32::NAN), emitter);
    }

    #[test]
    fn rock_thickness_counts_only_rock_outside_cavities_and_holes() {
        let rock = ground();
        let tuning = RockTuning::default();
        let field = ZoneField {
            zones: vec![tunnel()],
        };
        // Straight down 7 m from the surface (x = 40, outside the hole) to the tunnel's top (−7).
        let thick = rock_thickness(
            &rock,
            &field,
            &tuning,
            Vec3::new(40.0, 0.5, 0.0),
            Vec3::new(40.0, -10.0, 0.0),
        );
        assert!((thick - 7.0).abs() <= 1.0, "{thick}");
        // Through the hole at the mouth: no rock at all.
        let mouth = rock_thickness(
            &rock,
            &field,
            &tuning,
            Vec3::new(0.0, 0.5, 0.0),
            Vec3::new(0.0, -10.0, 0.0),
        );
        assert_eq!(mouth, 0.0);
        // Inside the tunnel along its length: cavity, no rock.
        let along = rock_thickness(
            &rock,
            &field,
            &tuning,
            Vec3::new(-50.0, -10.0, 0.0),
            Vec3::new(50.0, -10.0, 0.0),
        );
        assert_eq!(along, 0.0);
        // Without the cavity zone the same underground ray is all rock.
        let solid = rock_thickness(
            &rock,
            &ZoneField::default(),
            &tuning,
            Vec3::new(-50.0, -10.0, 0.0),
            Vec3::new(50.0, -10.0, 0.0),
        );
        assert!(
            (solid - 80.0).abs() <= 2.0,
            "hole spans 20 m of x but is air above only: {solid}"
        );
        // Above ground: nothing.
        assert_eq!(
            rock_thickness(
                &rock,
                &field,
                &tuning,
                Vec3::new(0.0, 5.0, 0.0),
                Vec3::new(90.0, 8.0, 0.0)
            ),
            0.0
        );
        // Degenerate rays.
        assert_eq!(
            rock_thickness(&rock, &field, &tuning, Vec3::ZERO, Vec3::ZERO),
            0.0
        );
    }

    #[test]
    fn thickness_maps_to_gain_and_cutoff() {
        let t = RockTuning::default();
        assert_eq!(t.gain(0.0), 1.0);
        assert!((t.gain(4.0) - crate::spatial::db_to_gain(-10.0)).abs() < 1e-6);
        assert!(
            t.gain(1000.0) <= crate::spatial::db_to_gain(-60.0) + 1e-9,
            "capped at max_db"
        );
        assert!((t.cutoff_hz(0.0) - 20_000.0).abs() < 1.0);
        assert!((t.cutoff_hz(6.0) - 350.0).abs() < 1.0);
        assert!((t.cutoff_hz(60.0) - 350.0).abs() < 1.0);
        assert!(t.cutoff_hz(3.0) > 350.0 && t.cutoff_hz(3.0) < 20_000.0);
    }

    #[test]
    fn portal_routing_is_a_no_op_without_mouths() {
        let no_portals = ZoneField {
            zones: vec![tunnel()],
        };
        let inside = Vec3::new(50.0, -10.0, 0.0);
        assert_eq!(no_portals.portal_route(Vec3::new(0.0, 5.0, 40.0), inside), None);

        let mut zone = tunnel();
        zone.portals = vec![
            (Vec3::new(-100.0, -5.0, 0.0), 4.0),
            (Vec3::new(100.0, -5.0, 0.0), 4.0),
        ];
        let field = ZoneField { zones: vec![zone] };
        let listener = Vec3::new(120.0, 2.0, 0.0);
        let route = field
            .portal_route(listener, inside)
            .expect("routed through the east mouth");
        assert_eq!(route.mouth, Vec3::new(100.0, -5.0, 0.0));
        let expected = inside.distance(route.mouth) + route.mouth.distance(listener);
        assert!((route.path_length - expected).abs() < 1e-3);
        assert!((route.position.distance(listener) - expected).abs() < 1e-3);
        // A listener inside the tunnel is not routed; nor an emitter outside it.
        assert_eq!(field.portal_route(Vec3::new(-20.0, -10.0, 0.0), inside), None);
        assert_eq!(field.portal_route(listener, Vec3::new(0.0, 5.0, 0.0)), None);
    }

    #[test]
    fn a_listener_within_a_mouth_radius_is_routed_through_that_mouth() {
        let mut zone = tunnel();
        let east = Vec3::new(100.0, -5.0, 0.0);
        zone.portals = vec![(east, 4.0)];
        let field = ZoneField { zones: vec![zone] };
        let inside = Vec3::new(50.0, -10.0, 0.0);
        // On the mouth centre and 3 m out (inside the 4 m radius): standing in the mouth, the
        // sound comes out of it, so it is routed (an unrouted listener would hear through rock).
        for listener in [east, Vec3::new(103.0, -5.0, 0.0)] {
            let route = field.portal_route(listener, inside).expect("routed in the mouth");
            assert_eq!(route.mouth, east);
            let expected = inside.distance(east) + east.distance(listener);
            assert!((route.path_length - expected).abs() < 1e-3);
            assert!((route.position.distance(listener) - expected).abs() < 1e-3);
            assert!(route.position.is_finite());
        }
        // On the centre the sound arrives from the cavity side (toward the emitter).
        let centre = field.portal_route(east, inside).expect("routed");
        assert!((centre.position - east).dot(inside - east) > 0.0);
        // 5 m out (beyond the radius): routed too.
        assert!(field.portal_route(Vec3::new(105.0, -5.0, 0.0), inside).is_some());
    }

    #[test]
    fn the_mouth_a_listener_stands_in_wins_over_a_shorter_path() {
        let mut zone = tunnel();
        let west = Vec3::new(-100.0, -5.0, 0.0);
        let east = Vec3::new(100.0, -5.0, 0.0);
        // A wide east mouth: a listener 30 m out of it, nearer the emitter's west side, still
        // stands in it.
        zone.portals = vec![(west, 4.0), (east, 40.0)];
        let field = ZoneField { zones: vec![zone] };
        let emitter = Vec3::new(-90.0, -10.0, 0.0);
        let listener = Vec3::new(100.0, -5.0, 30.0);
        let shortest_west = emitter.distance(west) + west.distance(listener);
        let through_east = emitter.distance(east) + east.distance(listener);
        assert!(shortest_west < through_east, "the west mouth is the shorter path");
        let route = field.portal_route(listener, emitter).expect("routed");
        assert_eq!(route.mouth, east);
        assert!((route.path_length - through_east).abs() < 1e-3);
    }
}
