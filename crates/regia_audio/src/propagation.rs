// SPDX-License-Identifier: MPL-2.0
//! The propagation solver: which paths a sound takes to the listener and what each does to it
//! (`docs/specs/16-audio.md` § 3c). Pure functions over plain frames; [`crate::Audio`] runs them
//! per voice and the agent protocol reports them (`audio.propagation`).
//!
//! Two paths per emitter, rendered as up to two voices:
//! - **airborne**: through the planet's air (or a pressurised interior's own air): delay `r / c`,
//!   the emitter's authored rolloff and cone, the medium's level offset, absorption low-pass and
//!   Doppler with the medium's `c`; through a wall it is muffled ([`WALL_TRANSMISSION`]). None on
//!   an airless world outside an interior.
//! - **ground**: injected by emitters with ground coupling, straight-line through the ground
//!   material ([`crate::ground`]), across structure interfaces, and into the ear through the
//!   listener's contact filter — or radiated into an interior's air. Faded by the listener's
//!   contact (jumping fades it out in [`COUPLING_FADE_SECS`]).
//!
//! The listener's own body, bypass buses (radio, UI) and the listener's own vehicle bypass both.

use regia_core::math::{Vec3, lerp};
use serde::{Deserialize, Serialize};

use crate::coupling::{
    AudioPropagation, COUPLING_FADE_SECS, INTERIOR_RADIATION, InteriorFrame, ListenerContact,
    ListenerCoupling, PathClass, WALL_TRANSMISSION,
};
use crate::ground::{ground_arrival_view, impedance_of, interface_transmission, material_view_or};
use crate::medium::{AudioMedium, MAX_CUTOFF_HZ, MediumKind};
use crate::spatial::{AudioEmitter, EmitterFrame, ListenerFrame, db_to_gain, spatialize};

/// Below this amplitude a path is not rendered (−90 dB).
pub const AUDIBLE_GAIN: f32 = 3.0e-5;
/// Pan of a ground arrival relative to the airborne pan (vibration through the feet localises
/// poorly).
pub const GROUND_PAN_SCALE: f32 = 0.5;

/// One path from an emitter to the listener.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PathFrame {
    /// Arrival delay, seconds.
    pub delay_secs: f32,
    /// Amplitude 0..1 (everything: rolloff, medium level, spreading, interfaces, coupling).
    pub gain: f32,
    /// Absorption low-pass cutoff, Hz (20 kHz = unfiltered).
    pub cutoff_hz: f32,
    /// Coupling (boots/hull/floor/wall) low-pass cutoff, Hz, if any.
    pub coupling_cutoff_hz: Option<f32>,
    /// Coda length, seconds (0 = none).
    pub coda_secs: f32,
    /// Coda wet level.
    pub coda_mix: f32,
    /// Stereo pan −1..1.
    pub pan: f32,
    /// Doppler playback-rate ratio.
    pub pitch_ratio: f32,
    /// Haptic-band amplitude (ground paths; 0 for airborne).
    pub felt: f32,
    /// The listener-contact fade already folded into `gain` (1 unless the listener is leaving
    /// or landing on the ground). The mixer applies it after the coda so a jump silences the
    /// ringing too.
    pub contact_weight: f32,
}

impl PathFrame {
    /// The effective low-pass cutoff (the lower of absorption and coupling), Hz.
    #[must_use]
    pub fn effective_cutoff_hz(&self) -> f32 {
        self.coupling_cutoff_hz
            .map_or(self.cutoff_hz, |c| c.min(self.cutoff_hz))
    }

    /// The gain in dB (−100 for silence).
    #[must_use]
    pub fn gain_db(&self) -> f32 {
        crate::spatial::gain_to_db(self.gain)
    }

    /// Whether the path is loud enough to render.
    #[must_use]
    pub fn audible(&self) -> bool {
        self.gain >= AUDIBLE_GAIN
    }
}

/// Every path from one emitter to the listener.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Propagation {
    /// The emitter bypasses the medium (body, radio/UI bus, own vehicle): `airborne` holds the
    /// plain spatialised frame.
    pub bypass: bool,
    /// Which air the airborne path used (`None` when there is no airborne path).
    pub air: Option<MediumKind>,
    /// The airborne path.
    pub airborne: Option<PathFrame>,
    /// The ground-borne path.
    pub ground: Option<PathFrame>,
    /// Ground material the ground path travelled through.
    pub ground_material: Option<String>,
    /// Whether the listener currently picks up ground vibration (contact or interior radiation).
    pub coupled: bool,
    /// Straight-line distance, metres.
    pub distance: f32,
}

impl Propagation {
    /// An inaudible result, before the first solve. No heap allocation.
    pub(crate) fn empty() -> Self {
        Self {
            bypass: false,
            air: None,
            airborne: None,
            ground: None,
            ground_material: None,
            coupled: false,
            distance: 0.0,
        }
    }

    /// Whether any path is audible.
    #[must_use]
    pub fn audible(&self) -> bool {
        self.airborne.is_some_and(|p| p.audible()) || self.ground.is_some_and(|p| p.audible())
    }
}

/// The world state the solver reads.
#[derive(Clone, Copy, Debug)]
pub struct PropagationContext<'a> {
    /// The level's medium.
    pub medium: &'a AudioMedium,
    /// The listener's frame.
    pub listener: &'a ListenerFrame,
    /// The listener's contact.
    pub coupling: &'a ListenerCoupling,
    /// Contact fade 0..1 (1 = in contact; falls to 0 within [`COUPLING_FADE_SECS`] of leaving).
    pub coupling_weight: f32,
    /// The last contact the listener had: while the fade runs after leaving the ground, the
    /// path keeps that contact's filter.
    pub last_contact: ListenerContact,
    /// Interior volumes this frame.
    pub interiors: &'a [InteriorFrame],
}

/// Advances the contact fade by `dt` toward `coupled` (linear, [`COUPLING_FADE_SECS`] end to end).
#[must_use]
pub fn step_coupling_weight(weight: f32, coupled: bool, dt: f32) -> f32 {
    let step = dt.max(0.0) / COUPLING_FADE_SECS;
    if coupled {
        (weight + step).min(1.0)
    } else {
        (weight - step).max(0.0)
    }
}

fn interior_of(interiors: &[InteriorFrame], point: Vec3) -> Option<&InteriorFrame> {
    // The smallest containing volume wins: an airlock inside a hab is its own air, not the
    // hab's (matching `ZoneField::cavity_zone_index`).
    interiors
        .iter()
        .filter(|frame| frame.contains(point))
        .min_by(|a, b| box_volume(a).total_cmp(&box_volume(b)))
}

/// The box's volume (the half-extents product), for the smallest-volume lookup.
fn box_volume(frame: &InteriorFrame) -> f32 {
    let h = frame.volume.half_extents.abs();
    h.x * h.y * h.z
}

/// Solves every path from `emitter` to the listener. `bus_bypass` is the voice's bus flag
/// ([`crate::BusDef::bypass_medium`]).
#[must_use]
pub fn propagate(
    ctx: &PropagationContext<'_>,
    emitter: &EmitterFrame,
    params: &AudioEmitter,
    propagation: &AudioPropagation,
    bus_bypass: bool,
) -> Propagation {
    let mut out = Propagation {
        bypass: false,
        air: None,
        airborne: None,
        ground: None,
        ground_material: None,
        coupled: false,
        distance: 0.0,
    };
    // One interface-name buffer per thread, so a solve that runs through this wrapper (tests,
    // one-shot callers) does not allocate the scratch a second time; the returned ground name
    // stays the only per-solve allocation.
    thread_local! {
        static SCRATCH: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    }
    SCRATCH.with(|cell| {
        let mut scratch = cell.borrow_mut();
        scratch.clear();
        propagate_into(
            ctx,
            emitter,
            params,
            propagation,
            bus_bypass,
            &mut scratch,
            &mut out,
        );
    });
    out
}

/// [`propagate`] into `out`, reusing `out.ground_material` and `scratch`.
///
/// `scratch` holds the previous interface name. A steady chain clears and rewrites it only when
/// the next link differs, and only grows it the first time a longer name appears.
pub(crate) fn propagate_into(
    ctx: &PropagationContext<'_>,
    emitter: &EmitterFrame,
    params: &AudioEmitter,
    propagation: &AudioPropagation,
    bus_bypass: bool,
    scratch: &mut String,
    out: &mut Propagation,
) {
    let spatial = spatialize(ctx.listener, emitter, params);
    let distance = spatial.distance;
    let own_vehicle = propagation.vehicle.is_some() && propagation.vehicle == ctx.coupling.vehicle;
    if bus_bypass || own_vehicle || propagation.class != PathClass::World {
        out.bypass = true;
        out.air = None;
        out.airborne = Some(PathFrame {
            delay_secs: 0.0,
            gain: spatial.gain,
            cutoff_hz: spatial.cutoff_hz.map_or(MAX_CUTOFF_HZ, |c| c as f32),
            coupling_cutoff_hz: None,
            coda_secs: 0.0,
            coda_mix: 0.0,
            pan: spatial.pan,
            pitch_ratio: spatial.pitch_ratio,
            felt: 0.0,
            contact_weight: 1.0,
        });
        out.ground = None;
        out.ground_material = None;
        out.coupled = false;
        out.distance = distance;
        return;
    }

    let emitter_volume = interior_of(ctx.interiors, emitter.position);
    let listener_volume = interior_of(ctx.interiors, ctx.listener.position);
    let same_volume = matches!((emitter_volume, listener_volume), (Some(a), Some(b)) if a.entity == b.entity);
    // Nested volumes: one end's point also lies inside the other end's box (a pressurised room
    // inside a pressurised hab, the usual station authoring). The pair shares air through the
    // inner volume's one wall on any medium; the airless no-path rule below applies to disjoint
    // volumes only (ADR-5731).
    let nested = match (emitter_volume, listener_volume) {
        (Some(e), Some(l)) if e.entity != l.entity => {
            e.contains(ctx.listener.position) || l.contains(emitter.position)
        }
        _ => false,
    };

    // ---- airborne
    // The interior preset allocates a ground name. Build it only when a volume is pressurised,
    // and then share the one static value.
    let air_at = |volume: Option<&InteriorFrame>| -> Option<&AudioMedium> {
        match volume {
            Some(v) if v.volume.pressurised => Some(AudioMedium::interior_shared()),
            _ => ctx.medium.carries_sound().then_some(ctx.medium),
        }
    };
    let (air, walls) = if same_volume {
        (air_at(emitter_volume), 0)
    } else {
        match (air_at(emitter_volume), air_at(listener_volume)) {
            (Some(a), Some(b)) => {
                if nested {
                    // Nested pressurised volumes share the inner air through one wall.
                    (Some(a), 1)
                } else if !ctx.medium.carries_sound() && emitter_volume.is_some() && listener_volume.is_some()
                {
                    // Two different interiors with no shared air: on a world whose outside
                    // carries no sound there is vacuum between them, so no airborne path at all
                    // (the airless rule above). The solver has no shared-wall test, so a docked
                    // bulkhead looks like open space; silence matches the documented rule.
                    (None, 0)
                } else {
                    let walls = i32::from(emitter_volume.is_some()) + i32::from(listener_volume.is_some());
                    // Through a wall the longer stretch is outside when one end is outdoors.
                    let outside = if emitter_volume.is_none() { a } else { b };
                    (Some(outside), walls)
                }
            }
            _ => (None, 0),
        }
    };
    let airborne = air.map(|m| {
        let blend = params.spatial_blend.clamp(0.0, 1.0);
        let wall_gain = db_to_gain(WALL_TRANSMISSION.gain_db).powi(walls);
        // Moving source and listener (OpenAL 1.1 § Doppler Shift), at the medium's speed.
        let doppler = if params.doppler_factor > 0.0 && distance > 1e-4 {
            let line = (ctx.listener.position - emitter.position) / distance;
            m.doppler_ratio_moving(
                emitter.velocity.dot(line),
                ctx.listener.velocity.dot(line),
                params.doppler_factor,
            )
        } else {
            1.0
        };
        let mut cutoff = m.absorption_cutoff_hz(distance);
        if let Some(authored) = spatial.cutoff_hz {
            cutoff = cutoff.min(authored as f32);
        }
        PathFrame {
            delay_secs: m.delay_secs(distance),
            gain: (spatial.gain * db_to_gain(m.level_offset_db) * wall_gain).clamp(0.0, 1.0),
            cutoff_hz: cutoff,
            coupling_cutoff_hz: (walls > 0).then_some(WALL_TRANSMISSION.cutoff_hz),
            coda_secs: 0.0,
            coda_mix: 0.0,
            pan: spatial.pan,
            pitch_ratio: lerp(1.0, doppler, blend),
            felt: 0.0,
            contact_weight: 1.0,
        }
    });

    // ---- ground
    let contact = if ctx.coupling.contact.coupled() {
        ctx.coupling.contact
    } else {
        ctx.last_contact
    };
    let in_air_volume = listener_volume.is_some_and(|v| v.volume.pressurised);
    let (ear, weight) = match contact.filter() {
        Some(filter) if ctx.coupling_weight > 0.0 => (Some(filter), ctx.coupling_weight),
        // Leaving contact inside a hab: the walls still radiate into the air around you.
        _ if in_air_volume => (Some(INTERIOR_RADIATION), 1.0),
        _ => (None, 0.0),
    };
    let coupling = propagation.effective_coupling();
    let ground = match ear {
        Some(ear) if coupling > 0.0 && distance <= propagation.ground_max_distance => {
            // The material between: what the emitter touches, else the structure both are in,
            // else the level's ground.
            let path_name = match (&propagation.ground, same_volume, emitter_volume) {
                (Some(name), ..) => name.as_str(),
                (None, true, Some(v)) => v.volume.wall.as_str(),
                _ => ctx.medium.ground.as_str(),
            };
            let material = material_view_or(path_name, &ctx.medium.ground);
            // Interfaces from the source to the ear. `scratch` is the previous link's name.
            let mut interfaces = 1.0f32;
            let mut started = false;
            if !same_volume && let Some(v) = emitter_volume {
                fold_interface(
                    scratch,
                    &v.volume.wall,
                    material.name,
                    &mut interfaces,
                    &mut started,
                );
            }
            fold_interface(
                scratch,
                material.name,
                material.name,
                &mut interfaces,
                &mut started,
            );
            if !same_volume {
                match listener_volume {
                    Some(v) => {
                        fold_interface(
                            scratch,
                            &v.volume.wall,
                            material.name,
                            &mut interfaces,
                            &mut started,
                        );
                    }
                    None => {
                        if let Some(underfoot) = &ctx.coupling.ground {
                            fold_interface(scratch, underfoot, material.name, &mut interfaces, &mut started);
                        }
                    }
                }
            }
            for layer in &ctx.coupling.structure {
                fold_interface(scratch, layer, material.name, &mut interfaces, &mut started);
            }
            if contact == ListenerContact::Vehicle && ctx.coupling_weight > 0.0 {
                fold_interface(scratch, "steel", material.name, &mut interfaces, &mut started);
            }
            let arrival = ground_arrival_view(
                material,
                distance,
                propagation.spreading,
                propagation.source_size_m,
                coupling * db_to_gain(propagation.ground_level_db),
            );
            set_ground_name(&mut out.ground_material, material.name);
            // The level is clamped before the contact fade: the path applies the fade after
            // the voice handle's own unity cap, so the stored gain must equal what is heard.
            let level = (arrival.amplitude * interfaces * db_to_gain(ear.gain_db)).clamp(0.0, 1.0);
            Some(PathFrame {
                delay_secs: arrival.delay_secs,
                gain: level * weight,
                cutoff_hz: arrival.cutoff_hz,
                coupling_cutoff_hz: Some(ear.cutoff_hz),
                coda_secs: arrival.coda_secs,
                coda_mix: arrival.coda_mix,
                pan: spatial.pan * GROUND_PAN_SCALE,
                pitch_ratio: 1.0,
                felt: (arrival.felt * interfaces * weight).clamp(0.0, 1.0),
                contact_weight: weight,
            })
        }
        _ => {
            out.ground_material = None;
            None
        }
    };

    out.bypass = false;
    out.air = air.map(|m| m.kind);
    out.airborne = airborne;
    out.ground = ground;
    out.coupled = weight > 0.0;
    out.distance = distance;
}

/// Keeps an existing material name and rewrites it in place when `name` changes.
fn set_ground_name(slot: &mut Option<String>, name: &str) {
    match slot {
        Some(existing) if existing == name => {}
        Some(existing) => {
            existing.clear();
            existing.push_str(name);
        }
        None => *slot = Some(name.to_string()),
    }
}

/// One link of the ground chain. The first link only stores its name; each later change of
/// material multiplies the normal-incidence transmission into `interfaces`.
fn fold_interface(
    scratch: &mut String,
    name: &str,
    fallback: &str,
    interfaces: &mut f32,
    started: &mut bool,
) {
    if !*started {
        if scratch != name {
            scratch.clear();
            scratch.push_str(name);
        }
        *started = true;
        return;
    }
    if scratch == name {
        return;
    }
    let z_prev = {
        let view = material_view_or(scratch, fallback);
        impedance_of(view.density_kg_m3, view.wave_speed_mps)
    };
    let next = material_view_or(name, fallback);
    *interfaces *=
        interface_transmission(z_prev, impedance_of(next.density_kg_m3, next.wave_speed_mps)).sqrt();
    scratch.clear();
    scratch.push_str(name);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coupling::{AudioInteriorVolume, EmitterCategory};
    use regia_ecs::prelude::Entity;

    fn far_emitter() -> AudioEmitter {
        AudioEmitter {
            min_distance: 1.0,
            max_distance: 5_000.0,
            ..AudioEmitter::default()
        }
    }

    fn at(x: f32) -> EmitterFrame {
        EmitterFrame {
            position: Vec3::new(x, 0.0, 0.0),
            ..EmitterFrame::default()
        }
    }

    fn solve(
        medium: &AudioMedium,
        coupling: &ListenerCoupling,
        weight: f32,
        interiors: &[InteriorFrame],
        frame: &EmitterFrame,
        category: EmitterCategory,
    ) -> Propagation {
        let listener = ListenerFrame::looking_at(Vec3::ZERO, -Vec3::Z);
        let ctx = PropagationContext {
            medium,
            listener: &listener,
            coupling,
            coupling_weight: weight,
            last_contact: ListenerContact::Boots,
            interiors,
        };
        propagate(
            &ctx,
            frame,
            &far_emitter(),
            &AudioPropagation::for_category(category),
            false,
        )
    }

    fn standing() -> ListenerCoupling {
        ListenerCoupling::default()
    }

    fn floating() -> ListenerCoupling {
        ListenerCoupling {
            contact: ListenerContact::None,
            ..ListenerCoupling::default()
        }
    }

    #[test]
    fn airless_and_floating_hears_nothing() {
        let p = solve(
            &AudioMedium::airless(),
            &floating(),
            0.0,
            &[],
            &at(50.0),
            EmitterCategory::Explosion,
        );
        assert!(p.airborne.is_none() && p.ground.is_none());
        assert!(!p.audible() && !p.coupled);
    }

    #[test]
    fn airless_grounded_on_ice_hears_the_ground_wave() {
        let p = solve(
            &AudioMedium::airless(),
            &standing(),
            1.0,
            &[],
            &at(500.0),
            EmitterCategory::Explosion,
        );
        assert!(p.airborne.is_none());
        let g = p.ground.expect("ground path");
        assert!((g.delay_secs - 500.0 / 3_800.0).abs() < 1e-5);
        assert!(
            g.effective_cutoff_hz() < 200.0,
            "low-passed {}",
            g.effective_cutoff_hz()
        );
        assert!(g.audible() && g.felt > 0.0);
        assert_eq!(p.ground_material.as_deref(), Some("ice"));
    }

    #[test]
    fn mars_is_quieter_later_and_darker_than_earth() {
        let earth = solve(
            &AudioMedium::earth(),
            &standing(),
            1.0,
            &[],
            &at(200.0),
            EmitterCategory::Projectile,
        );
        let mars = solve(
            &AudioMedium::mars(),
            &standing(),
            1.0,
            &[],
            &at(200.0),
            EmitterCategory::Projectile,
        );
        let (e, m) = (
            earth.airborne.expect("earth air"),
            mars.airborne.expect("mars air"),
        );
        assert!(
            (e.gain_db() - m.gain_db() - 20.0).abs() < 0.1,
            "{} vs {}",
            e.gain_db(),
            m.gain_db()
        );
        assert!(m.delay_secs > e.delay_secs);
        assert!(m.cutoff_hz < e.cutoff_hz * 0.25);
    }

    #[test]
    fn explosions_on_earth_arrive_through_the_ground_first() {
        let p = solve(
            &AudioMedium::earth(),
            &standing(),
            1.0,
            &[],
            &at(300.0),
            EmitterCategory::Explosion,
        );
        let (air, ground) = (p.airborne.expect("air"), p.ground.expect("ground"));
        assert!(
            ground.delay_secs < air.delay_secs,
            "{} vs {}",
            ground.delay_secs,
            air.delay_secs
        );
        assert!(ground.felt > 0.0);
    }

    #[test]
    fn a_boosted_ground_level_scales_with_the_contact_fade() {
        let listener = ListenerFrame::looking_at(Vec3::ZERO, -Vec3::Z);
        let medium = AudioMedium::airless();
        let mut propagation = AudioPropagation::for_category(EmitterCategory::Explosion);
        propagation.ground_level_db = 40.0; // pre-fade level far above unity
        let gain_at = |coupling: &ListenerCoupling, weight: f32| {
            let ctx = PropagationContext {
                medium: &medium,
                listener: &listener,
                coupling,
                coupling_weight: weight,
                last_contact: ListenerContact::Boots,
                interiors: &[],
            };
            propagate(&ctx, &at(1.0), &far_emitter(), &propagation, false)
                .ground
                .expect("ground path")
                .gain
        };
        let full = gain_at(&standing(), 1.0);
        let half = gain_at(&floating(), 0.5);
        assert_eq!(full, 1.0, "clamped at full contact");
        assert!(
            (half - 0.5).abs() < 1e-6,
            "half contact is exactly half, got {half}"
        );
    }

    #[test]
    fn the_fade_takes_fifty_milliseconds() {
        let mut w = 1.0;
        for _ in 0..3 {
            w = step_coupling_weight(w, false, 1.0 / 60.0);
        }
        assert_eq!(w, 0.0, "three 60 Hz frames ≥ 50 ms");
        assert!((step_coupling_weight(0.0, true, 0.025) - 0.5).abs() < 1e-6);
        // Mid-fade the path keeps the last contact's filter at the faded level; at 0 it is gone.
        let full = solve(
            &AudioMedium::airless(),
            &standing(),
            1.0,
            &[],
            &at(20.0),
            EmitterCategory::Footstep,
        );
        let half = solve(
            &AudioMedium::airless(),
            &floating(),
            0.5,
            &[],
            &at(20.0),
            EmitterCategory::Footstep,
        );
        let (full, half) = (full.ground.expect("standing"), half.ground.expect("fading"));
        assert!((half.gain - 0.5 * full.gain).abs() < 1e-6);
        let gone = solve(
            &AudioMedium::airless(),
            &floating(),
            0.0,
            &[],
            &at(20.0),
            EmitterCategory::Footstep,
        );
        assert!(gone.ground.is_none(), "after the fade the ground path is gone");
    }

    #[test]
    fn bypass_classes_ignore_the_medium() {
        for category in [EmitterCategory::Radio, EmitterCategory::Body] {
            let p = solve(&AudioMedium::airless(), &floating(), 0.0, &[], &at(5.0), category);
            assert!(p.bypass);
            assert!(p.airborne.expect("direct").audible());
        }
    }

    #[test]
    fn interiors_have_their_own_air_and_muffle_the_outside() {
        let hab = InteriorFrame {
            entity: Entity::from_raw_u32(7).expect("entity"),
            center: Vec3::ZERO,
            rotation: regia_core::math::Quat::IDENTITY,
            volume: AudioInteriorVolume::default(),
        };
        let interiors = [hab];
        // A source inside the hab on an airless world is airborne inside.
        let p = solve(
            &AudioMedium::airless(),
            &floating(),
            0.0,
            &interiors,
            &at(3.0),
            EmitterCategory::Machinery,
        );
        assert_eq!(p.air, Some(MediumKind::Interior));
        assert!(p.airborne.expect("inside air").audible());
        // An exterior impact reaches the floating listener only through the structure.
        let p = solve(
            &AudioMedium::airless(),
            &floating(),
            0.0,
            &interiors,
            &at(40.0),
            EmitterCategory::Impact,
        );
        assert!(p.airborne.is_none());
        let g = p.ground.expect("radiated through the walls");
        assert_eq!(g.coupling_cutoff_hz, Some(INTERIOR_RADIATION.cutoff_hz));
        assert!(p.coupled);
        // On Earth the outside air also gets in, through the wall, muffled.
        let p = solve(
            &AudioMedium::earth(),
            &standing(),
            1.0,
            &interiors,
            &at(40.0),
            EmitterCategory::Impact,
        );
        let air = p.airborne.expect("through the wall");
        assert_eq!(air.coupling_cutoff_hz, Some(WALL_TRANSMISSION.cutoff_hz));
    }

    /// Two pressurised interiors are different entities with vacuum between them on an airless
    /// world: no airborne path, however loud the double wall would have been (correctness audit
    /// finding 1). On a world that carries sound the same geometry joins through the outside air.
    #[test]
    fn separate_interiors_on_an_airless_world_have_no_airborne_path() {
        let volume = AudioInteriorVolume {
            half_extents: Vec3::splat(2.0),
            ..AudioInteriorVolume::default()
        };
        let hab = |entity: u32, x: f32| InteriorFrame {
            entity: Entity::from_raw_u32(entity).expect("entity"),
            center: Vec3::new(x, 0.0, 0.0),
            rotation: regia_core::math::Quat::IDENTITY,
            volume: volume.clone(),
        };
        // Listener at the origin inside hab 7 (spans x −4..=4), emitter 8 m away inside hab 9.
        // The double wall (−70 dB) at 8 m stays just above the audibility floor, so the path
        // would render if the solver built it.
        let interiors = [hab(7, 0.0), hab(9, 8.0)];
        let p = solve(
            &AudioMedium::airless(),
            &floating(),
            0.0,
            &interiors,
            &at(8.0),
            EmitterCategory::Machinery,
        );
        assert!(p.airborne.is_none(), "vacuum between the habs: no airborne path");
        // (The floating listener inside the pressurised hab still hears wall radiation through
        // the ground path; only the airborne path is the bug.)
        // The same volumes on Earth connect through the outside air, through both walls.
        let p = solve(
            &AudioMedium::earth(),
            &standing(),
            1.0,
            &interiors,
            &at(8.0),
            EmitterCategory::Machinery,
        );
        let air = p.airborne.expect("outside air joins the interiors");
        assert_eq!(air.coupling_cutoff_hz, Some(WALL_TRANSMISSION.cutoff_hz));
    }

    /// A pressurised room inside a pressurised hab is one interior nested in another: the
    /// emitter sits in both boxes, the listener in the outer one, and the pair shares air
    /// through the room's single wall. The airless no-path rule applies to *disjoint* volumes
    /// only, so the hab hears the room on an airless world too, through exactly one wall
    /// (fixrev2 review of the airless rule; the disjoint case above stays silent).
    #[test]
    fn nested_pressurised_room_shares_air_through_one_wall() {
        let hab = InteriorFrame {
            entity: Entity::from_raw_u32(7).expect("entity"),
            center: Vec3::new(10.0, 0.0, 0.0),
            rotation: regia_core::math::Quat::IDENTITY,
            volume: AudioInteriorVolume {
                half_extents: Vec3::splat(20.0),
                ..AudioInteriorVolume::default()
            },
        };
        let room = InteriorFrame {
            entity: Entity::from_raw_u32(9).expect("entity"),
            center: Vec3::new(10.0, 0.0, 0.0),
            rotation: regia_core::math::Quat::IDENTITY,
            volume: AudioInteriorVolume {
                half_extents: Vec3::splat(2.0),
                ..AudioInteriorVolume::default()
            },
        };
        let interiors = [hab, room];
        // Reference: the same emitter at the same distance with the hab alone is a same-volume
        // path (zero walls); the nested path must carry exactly one WALL_TRANSMISSION factor.
        let hab_only = [interiors[0].clone()];
        for medium in [&AudioMedium::airless(), &AudioMedium::earth()] {
            let p = solve(
                medium,
                &floating(),
                0.0,
                &interiors,
                &at(10.0),
                EmitterCategory::Machinery,
            );
            let air = p
                .airborne
                .expect("nested pressurised volumes share air through one wall");
            assert_eq!(air.coupling_cutoff_hz, Some(WALL_TRANSMISSION.cutoff_hz));
            let flat = solve(
                medium,
                &floating(),
                0.0,
                &hab_only,
                &at(10.0),
                EmitterCategory::Machinery,
            );
            let open = flat.airborne.expect("the hab alone is one volume");
            let expected = open.gain * db_to_gain(WALL_TRANSMISSION.gain_db);
            assert!(
                (air.gain - expected).abs() <= 1e-7,
                "exactly one wall: {} vs {expected}",
                air.gain
            );
        }
    }

    /// The smallest containing volume classifies a point: an airlock inside a hab is its own
    /// (unpressurised) shell no matter which order the volumes are listed in (correctness audit
    /// finding 2).
    #[test]
    fn interior_lookup_prefers_the_smallest_containing_volume() {
        let hab = InteriorFrame {
            entity: Entity::from_raw_u32(7).expect("entity"),
            center: Vec3::new(10.0, 0.0, 0.0),
            rotation: regia_core::math::Quat::IDENTITY,
            volume: AudioInteriorVolume {
                half_extents: Vec3::splat(20.0),
                ..AudioInteriorVolume::default()
            },
        };
        let airlock = InteriorFrame {
            entity: Entity::from_raw_u32(9).expect("entity"),
            center: Vec3::new(10.0, 0.0, 0.0),
            rotation: regia_core::math::Quat::IDENTITY,
            volume: AudioInteriorVolume {
                half_extents: Vec3::splat(2.0),
                pressurised: false,
                ..AudioInteriorVolume::default()
            },
        };
        // Listener at the origin (in the hab only), emitter inside the airlock: on an airless
        // world the emitter's end matches the unpressurised airlock either way the slice is
        // ordered, so there is no air at the emitter and no airborne path.
        for (order, interiors) in [
            ("hab first", &[hab.clone(), airlock.clone()][..]),
            ("airlock first", &[airlock.clone(), hab.clone()][..]),
        ] {
            let p = solve(
                &AudioMedium::airless(),
                &floating(),
                0.0,
                interiors,
                &at(10.0),
                EmitterCategory::Machinery,
            );
            assert!(
                p.airborne.is_none(),
                "the unpressurised airlock has no air ({order})"
            );
        }
        // Both pressurised: the emitter is in the inner room and the listener in the outer hab —
        // nested volumes (the emitter's point lies in both boxes), so the pair shares air
        // through the room's wall only: one wall, on any medium (ADR-5731). Listing order no
        // longer changes the wall count, whatever the first-match lookup would have done.
        let room = InteriorFrame {
            volume: AudioInteriorVolume {
                pressurised: true,
                ..airlock.volume.clone()
            },
            ..airlock.clone()
        };
        let interiors = [hab, room];
        let p = solve(
            &AudioMedium::earth(),
            &standing(),
            1.0,
            &interiors,
            &at(10.0),
            EmitterCategory::Machinery,
        );
        let air = p.airborne.expect("the rooms share air through the room's wall");
        assert_eq!(
            air.coupling_cutoff_hz,
            Some(WALL_TRANSMISSION.cutoff_hz),
            "the nested path crosses the room's wall only (one wall)"
        );
    }

    #[test]
    fn own_vehicle_is_hull_internal() {
        let hull = Entity::from_raw_u32(3).expect("entity");
        let coupling = ListenerCoupling {
            contact: ListenerContact::Vehicle,
            vehicle: Some(hull),
            ..ListenerCoupling::default()
        };
        let listener = ListenerFrame::looking_at(Vec3::ZERO, -Vec3::Z);
        let ctx = PropagationContext {
            medium: &AudioMedium::airless(),
            listener: &listener,
            coupling: &coupling,
            coupling_weight: 1.0,
            last_contact: ListenerContact::Vehicle,
            interiors: &[],
        };
        let cockpit = AudioPropagation {
            vehicle: Some(hull),
            ..AudioPropagation::for_category(EmitterCategory::Cockpit)
        };
        let p = propagate(&ctx, &at(1.0), &far_emitter(), &cockpit, false);
        assert!(p.bypass && p.audible());
    }

    #[test]
    fn interfaces_cost_energy() {
        let plain = solve(
            &AudioMedium::airless(),
            &standing(),
            1.0,
            &[],
            &at(100.0),
            EmitterCategory::Impact,
        );
        let on_deck = ListenerCoupling {
            contact: ListenerContact::Structure,
            structure: vec!["concrete".into(), "steel".into()],
            ..ListenerCoupling::default()
        };
        let deck = solve(
            &AudioMedium::airless(),
            &on_deck,
            1.0,
            &[],
            &at(100.0),
            EmitterCategory::Impact,
        );
        let (a, b) = (plain.ground.expect("boots"), deck.ground.expect("deck"));
        assert!(
            b.felt < a.felt,
            "two interfaces lose energy: {} vs {}",
            b.felt,
            a.felt
        );
    }
}
