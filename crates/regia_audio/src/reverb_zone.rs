// SPDX-License-Identifier: MPL-2.0
//! Reverb zones: rooms, tunnels and canyons that make the world reverberate while the listener
//! is inside them.
//!
//! The default bus tree has a `Reverb` send bus (a Freeverb-style reverb, kira's
//! `ReverbBuilder`) fed by `World` and `Ambience`, with both sends silent
//! ([`crate::bus::SILENT_SEND_DB`]). An [`AudioReverbZone`] is a volume in the world; while the
//! listener (player 0's) is inside, the sends rise to the zone's `send_db` and the reverb takes
//! the zone's room parameters. Over `fade_distance` metres outside the volume the send fades
//! out, so walking into a hangar blends the reverb in instead of switching it. Where zones
//! overlap they blend: layers are applied least specific first (lower `priority`, then larger
//! volume) and each crossfades toward its own parameters by its membership weight — so a room
//! nested inside a hall dominates while the listener is in the room, and equal-weight overlaps
//! are heard as a mix.
//!
//! Zones also carry a [`ReverbPreset`] (tube, mine gallery, open), a `distance_scale` that makes
//! sound carry further between a listener and an emitter that share the zone, and a `cavity`
//! flag marking the zone as air inside rock (the audio occlusion ray does not count rock inside
//! a cavity; see [`crate::audio_zone`]).

use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

/// A named reverb character. [`ReverbPreset::Custom`] is the hand-tuned zone; the others fill
/// the zone's numbers through [`AudioReverbZone::with_preset`].
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReverbPreset {
    /// Hand-tuned values (the default).
    #[default]
    Custom,
    /// A narrow bored tunnel: long, bright decay, narrow image, sound carries far along it.
    Tube,
    /// A mine gallery: shorter, darker decay than the tube, still carrying.
    MineGallery,
    /// Open ground: almost dry, no extra carrying.
    Open,
}

/// The numbers a [`ReverbPreset`] stands for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PresetParams {
    /// Send level into the reverb bus, dB.
    pub send_db: f32,
    /// Reverb feedback 0..1 (decay).
    pub feedback: f32,
    /// High-frequency damping 0..1.
    pub damping: f32,
    /// Stereo width 0..1.
    pub stereo_width: f32,
    /// Blend distance outside the volume, metres.
    pub blend_distance: f32,
    /// Distance-attenuation scale for sound shared inside the zone (> 1 carries further).
    pub distance_scale: f32,
    /// The zone is air inside rock.
    pub cavity: bool,
}

impl ReverbPreset {
    /// The zone numbers of this preset (`Custom` yields the [`AudioReverbZone`] defaults).
    pub fn params(self) -> PresetParams {
        match self {
            Self::Custom => PresetParams {
                send_db: -6.0,
                feedback: 0.85,
                damping: 0.4,
                stereo_width: 1.0,
                blend_distance: 2.0,
                distance_scale: 1.0,
                cavity: false,
            },
            Self::Tube => PresetParams {
                send_db: -3.0,
                feedback: 0.94,
                damping: 0.2,
                stereo_width: 0.6,
                blend_distance: 6.0,
                distance_scale: 1.5,
                cavity: true,
            },
            Self::MineGallery => PresetParams {
                send_db: -5.0,
                feedback: 0.88,
                damping: 0.45,
                stereo_width: 0.8,
                blend_distance: 5.0,
                distance_scale: 1.5,
                cavity: true,
            },
            Self::Open => PresetParams {
                send_db: -18.0,
                feedback: 0.35,
                damping: 0.7,
                stereo_width: 1.0,
                blend_distance: 20.0,
                distance_scale: 1.0,
                cavity: false,
            },
        }
    }

    /// The preset named `name` (`"tube"`, `"mine_gallery"`, `"open"`, `"custom"`).
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "custom" => Self::Custom,
            "tube" => Self::Tube,
            "mine_gallery" | "gallery" => Self::MineGallery,
            "open" => Self::Open,
            _ => return None,
        })
    }

    /// The preset's name (inverse of [`ReverbPreset::from_name`]).
    pub fn name(self) -> &'static str {
        match self {
            Self::Custom => "custom",
            Self::Tube => "tube",
            Self::MineGallery => "mine_gallery",
            Self::Open => "open",
        }
    }
}

/// A reverb zone's volume, in the zone entity's local frame (scale is not applied; rotation is).
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReverbZoneShape {
    /// A sphere of this radius, metres.
    Sphere {
        /// Radius, metres.
        radius: f32,
    },
    /// An oriented box with these half extents, metres.
    Box {
        /// Half extents along the zone's local X, Y, Z, metres.
        half_extents: Vec3,
    },
    /// A capsule around the local Y axis: the segment from `−half_length` to `+half_length` swept
    /// by `radius` (a tunnel section; rotate the entity to run it along a heading).
    Capsule {
        /// Radius, metres.
        radius: f32,
        /// Half the length of the axis segment (without the end caps), metres.
        half_length: f32,
    },
    /// Everywhere (an outdoor or level-wide reverb). Loses to every finite zone the listener
    /// is inside.
    Global,
}

/// A volume in which the world reverberates (see the module docs).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct AudioReverbZone {
    /// The zone's volume around the entity's `GlobalTransform`.
    pub shape: ReverbZoneShape,
    /// Distance outside the volume over which the reverb fades out, metres.
    pub fade_distance: f32,
    /// Send level into the reverb bus while fully inside, dB.
    pub send_db: f32,
    /// Reverb feedback 0..1: the room size / decay time.
    pub feedback: f32,
    /// High-frequency damping 0..1: soft rooms damp more.
    pub damping: f32,
    /// Stereo width 0..1.
    pub stereo_width: f32,
    /// Overlap tie-break: higher wins.
    pub priority: i32,
    /// The preset this zone was built from (informational once the numbers are set).
    pub preset: ReverbPreset,
    /// Distance-attenuation scale for an emitter and listener that share the zone: 1.5 makes an
    /// emitter audible 1.5× further. Blended by the lower of the two memberships at the zone's
    /// edge; 1 = none.
    pub distance_scale: f32,
    /// The zone is air inside rock: occlusion counts no rock inside it.
    pub cavity: bool,
    /// An impulse response registered with [`crate::Audio::register_impulse_response`] to play
    /// instead of the preset reverb while this is the most specific zone around the listener
    /// (ADR-2721). The low tier, or an IR that is not registered, falls back to the preset.
    pub ir: Option<String>,
}

impl Default for AudioReverbZone {
    fn default() -> Self {
        Self {
            shape: ReverbZoneShape::Box {
                half_extents: Vec3::splat(5.0),
            },
            fade_distance: 2.0,
            send_db: -6.0,
            feedback: 0.85,
            damping: 0.4,
            stereo_width: 1.0,
            priority: 0,
            preset: ReverbPreset::Custom,
            distance_scale: 1.0,
            cavity: false,
            ir: None,
        }
    }
}

impl AudioReverbZone {
    /// A zone of `shape` with a preset's numbers.
    pub fn with_preset(preset: ReverbPreset, shape: ReverbZoneShape) -> Self {
        let p = preset.params();
        Self {
            shape,
            fade_distance: p.blend_distance,
            send_db: p.send_db,
            feedback: p.feedback,
            damping: p.damping,
            stereo_width: p.stereo_width,
            priority: 0,
            preset,
            distance_scale: p.distance_scale,
            cavity: p.cavity,
            ir: None,
        }
    }

    /// A tunnel (see [`ReverbPreset::Tube`]).
    pub fn tube(shape: ReverbZoneShape) -> Self {
        Self::with_preset(ReverbPreset::Tube, shape)
    }

    /// A mine gallery (see [`ReverbPreset::MineGallery`]).
    pub fn mine_gallery(shape: ReverbZoneShape) -> Self {
        Self::with_preset(ReverbPreset::MineGallery, shape)
    }

    /// Open ground (see [`ReverbPreset::Open`]).
    pub fn open(shape: ReverbZoneShape) -> Self {
        Self::with_preset(ReverbPreset::Open, shape)
    }

    /// Wet send level, dB (the zone's `wet`).
    pub fn wet_db(&self) -> f32 {
        self.send_db
    }

    /// Decay 0..1 (the zone's `decay`, the reverb feedback).
    pub fn decay(&self) -> f32 {
        self.feedback
    }

    /// Blend distance outside the volume, metres (the zone's `blend_distance`).
    pub fn blend_distance(&self) -> f32 {
        self.fade_distance
    }

    /// The same zone playing impulse response `ir` (see [`AudioReverbZone::ir`]).
    #[must_use]
    pub fn with_ir(mut self, ir: impl Into<String>) -> Self {
        self.ir = Some(ir.into());
        self
    }

    /// A small, bright room (corridor, bunker).
    pub fn small_room(half_extents: Vec3) -> Self {
        Self {
            shape: ReverbZoneShape::Box { half_extents },
            feedback: 0.7,
            damping: 0.5,
            send_db: -8.0,
            ..Self::default()
        }
    }

    /// A large hall (hangar, cathedral, cave).
    pub fn hall(half_extents: Vec3) -> Self {
        Self {
            shape: ReverbZoneShape::Box { half_extents },
            feedback: 0.92,
            damping: 0.25,
            send_db: -4.0,
            fade_distance: 4.0,
            ..Self::default()
        }
    }

    /// How far `point` lies outside the volume (0 inside), metres. `Global` is never outside.
    pub fn distance_outside(&self, centre: Vec3, rotation: Quat, point: Vec3) -> f32 {
        let local = rotation.inverse() * (point - centre);
        // A non-finite point is outside every finite zone: `max(0.0)` turns a NaN distance into 0,
        // which read as inside (full weight, inside every cavity).
        // Regression: tests/tqs_sweep.rs::the_innermost_cavity_wins_and_a_nan_point_is_in_none_regression
        if !local.is_finite() && !matches!(self.shape, ReverbZoneShape::Global) {
            return f32::INFINITY;
        }
        match self.shape {
            ReverbZoneShape::Sphere { radius } => (local.length() - radius.max(0.0)).max(0.0),
            ReverbZoneShape::Box { half_extents } => {
                let q = local.abs() - half_extents.max(Vec3::ZERO);
                q.max(Vec3::ZERO).length()
            }
            ReverbZoneShape::Capsule { radius, half_length } => {
                let along = local.y.clamp(-half_length.max(0.0), half_length.max(0.0));
                ((local - Vec3::new(0.0, along, 0.0)).length() - radius.max(0.0)).max(0.0)
            }
            ReverbZoneShape::Global => 0.0,
        }
    }

    /// The zone's volume, m³ (`Global` is infinite).
    pub fn volume(&self) -> f32 {
        match self.shape {
            ReverbZoneShape::Sphere { radius } => 4.0 / 3.0 * std::f32::consts::PI * radius.max(0.0).powi(3),
            ReverbZoneShape::Box { half_extents } => {
                let e = half_extents.max(Vec3::ZERO);
                8.0 * e.x * e.y * e.z
            }
            ReverbZoneShape::Capsule { radius, half_length } => {
                let r = radius.max(0.0);
                std::f32::consts::PI * r * r * (2.0 * half_length.max(0.0))
                    + 4.0 / 3.0 * std::f32::consts::PI * r.powi(3)
            }
            ReverbZoneShape::Global => f32::INFINITY,
        }
    }

    /// The zone's weight at `point`: 1 inside, falling linearly to 0 at `fade_distance` outside.
    pub fn weight(&self, centre: Vec3, rotation: Quat, point: Vec3) -> f32 {
        let outside = self.distance_outside(centre, rotation, point);
        if outside <= 0.0 {
            1.0
        } else if self.fade_distance <= 1e-4 {
            0.0
        } else {
            (1.0 - outside / self.fade_distance).clamp(0.0, 1.0)
        }
    }
}

/// The reverb the listener hears this frame: the winning zone's parameters and how much of it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReverbMix {
    /// 0 = no reverb, 1 = fully inside the zone.
    pub weight: f32,
    /// Send level at full weight, dB.
    pub send_db: f32,
    /// Reverb feedback 0..1.
    pub feedback: f32,
    /// Reverb damping 0..1.
    pub damping: f32,
    /// Reverb stereo width 0..1.
    pub stereo_width: f32,
}

impl ReverbMix {
    /// No reverb.
    pub const DRY: Self = Self {
        weight: 0.0,
        send_db: crate::bus::SILENT_SEND_DB,
        feedback: 0.85,
        damping: 0.4,
        stereo_width: 1.0,
    };

    /// The effective send level in dB: the zone's level scaled by the weight in amplitude.
    pub fn effective_send_db(&self) -> f32 {
        let gain = crate::spatial::db_to_gain(self.send_db) * self.weight.clamp(0.0, 1.0);
        crate::spatial::gain_to_db(gain).max(crate::bus::SILENT_SEND_DB)
    }
}

/// Zones ordered from the least to the most specific: lower priority first, then larger volume
/// first (so a room nested in a hall comes after the hall).
pub(crate) fn specificity_order<T>(items: &mut [T], key: impl Fn(&T) -> (i32, f32)) {
    items.sort_by(|a, b| {
        let (pa, va) = key(a);
        let (pb, vb) = key(b);
        pa.cmp(&pb).then(vb.total_cmp(&va))
    });
}

/// Blends the reverb for a listener at `listener` among `(zone, centre, rotation)` triples.
///
/// Zones are layered least specific first (lower priority, then larger volume); each layer
/// crossfades from what is below it by its own weight (1 inside, falling to 0 over its blend
/// distance). So a tunnel mouth blends the open reverb into the tube's over the tube's blend
/// distance, and a nested room fully replaces its hall once the listener is inside.
pub fn reverb_mix<'a>(
    listener: Vec3,
    zones: impl IntoIterator<Item = (&'a AudioReverbZone, Vec3, Quat)>,
) -> ReverbMix {
    let mut layers: Vec<(&AudioReverbZone, f32)> = zones
        .into_iter()
        .map(|(zone, centre, rotation)| (zone, zone.weight(centre, rotation, listener)))
        .filter(|(_, w)| *w > 0.0)
        .collect();
    specificity_order(&mut layers, |(z, _)| (z.priority, z.volume()));
    let mut mix = ReverbMix::DRY;
    // NaN takes the dry default: `NaN.clamp` is NaN, and a NaN room parameter poisons the reverb.
    // Regression: tests/tqs_sweep.rs::reverb_mix_keeps_room_parameters_finite_for_nan_zones_regression
    let unit = |x: f32, default: f32| if x.is_nan() { default } else { x.clamp(0.0, 1.0) };
    for (zone, w) in layers {
        let zone_mix = ReverbMix {
            weight: 1.0,
            send_db: zone.send_db,
            feedback: unit(zone.feedback, ReverbMix::DRY.feedback),
            damping: unit(zone.damping, ReverbMix::DRY.damping),
            stereo_width: unit(zone.stereo_width, ReverbMix::DRY.stereo_width),
        };
        mix = if mix.weight <= 0.0 {
            ReverbMix {
                weight: w,
                ..zone_mix
            }
        } else {
            let lerp = |a: f32, b: f32| a + (b - a) * w;
            // The send crossfades in amplitude (as a single zone's weight scales it), not in dB.
            let send_gain = lerp(
                crate::spatial::db_to_gain(mix.send_db),
                crate::spatial::db_to_gain(zone_mix.send_db),
            );
            ReverbMix {
                weight: mix.weight + (1.0 - mix.weight) * w,
                send_db: crate::spatial::gain_to_db(send_gain).max(crate::bus::SILENT_SEND_DB),
                feedback: lerp(mix.feedback, zone_mix.feedback),
                damping: lerp(mix.damping, zone_mix.damping),
                stereo_width: lerp(mix.stereo_width, zone_mix.stereo_width),
            }
        };
    }
    mix
}

/// The impulse response the listener at `listener` hears: the one the most specific zone with a
/// weight above zero names (`None` when that zone names none, so a nested preset room inside an
/// IR hall uses its preset). Same ordering as [`reverb_mix`].
pub fn zone_ir<'a>(
    listener: Vec3,
    zones: impl IntoIterator<Item = (&'a AudioReverbZone, Vec3, Quat)>,
) -> Option<&'a str> {
    let mut layers: Vec<&AudioReverbZone> = zones
        .into_iter()
        .filter(|(zone, centre, rotation)| zone.weight(*centre, *rotation, listener) > 0.0)
        .map(|(zone, _, _)| zone)
        .collect();
    specificity_order(&mut layers, |z| (z.priority, z.volume()));
    layers.last().and_then(|zone| zone.ir.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_most_specific_zone_names_the_ir() {
        let hall = AudioReverbZone::hall(Vec3::splat(10.0)).with_ir("hall_ir");
        let room = AudioReverbZone::small_room(Vec3::splat(2.0));
        let zones = [
            (&hall, Vec3::ZERO, Quat::IDENTITY),
            (&room, Vec3::new(5.0, 0.0, 0.0), Quat::IDENTITY),
        ];
        assert_eq!(zone_ir(Vec3::ZERO, zones), Some("hall_ir"));
        // In the preset room nested in the IR hall: the room's preset.
        assert_eq!(zone_ir(Vec3::new(5.0, 0.0, 0.0), zones), None);
        assert_eq!(zone_ir(Vec3::splat(100.0), zones), None);
    }

    #[test]
    fn weight_is_one_inside_and_fades_outside() {
        let zone = AudioReverbZone {
            shape: ReverbZoneShape::Box {
                half_extents: Vec3::new(4.0, 3.0, 4.0),
            },
            fade_distance: 2.0,
            ..Default::default()
        };
        let c = Vec3::new(10.0, 0.0, 0.0);
        assert_eq!(zone.weight(c, Quat::IDENTITY, c), 1.0);
        assert_eq!(
            zone.weight(c, Quat::IDENTITY, c + Vec3::X * 4.0),
            1.0,
            "on the wall"
        );
        assert!((zone.weight(c, Quat::IDENTITY, c + Vec3::X * 5.0) - 0.5).abs() < 1e-5);
        assert_eq!(zone.weight(c, Quat::IDENTITY, c + Vec3::X * 7.0), 0.0);
        // Rotation is honoured: a 90° yaw swaps the X and Z extents.
        let narrow = AudioReverbZone {
            shape: ReverbZoneShape::Box {
                half_extents: Vec3::new(1.0, 1.0, 8.0),
            },
            fade_distance: 0.0,
            ..Default::default()
        };
        let yaw = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        assert_eq!(narrow.weight(Vec3::ZERO, Quat::IDENTITY, Vec3::X * 5.0), 0.0);
        assert_eq!(narrow.weight(Vec3::ZERO, yaw, Vec3::X * 5.0), 1.0);
    }

    #[test]
    fn the_innermost_zone_wins_and_global_is_the_fallback() {
        let global = AudioReverbZone {
            shape: ReverbZoneShape::Global,
            feedback: 0.3,
            ..Default::default()
        };
        let hall = AudioReverbZone::hall(Vec3::splat(10.0));
        let room = AudioReverbZone::small_room(Vec3::splat(2.0));
        let zones = [
            (&global, Vec3::ZERO, Quat::IDENTITY),
            (&hall, Vec3::ZERO, Quat::IDENTITY),
            (&room, Vec3::new(5.0, 0.0, 0.0), Quat::IDENTITY),
        ];
        // At the hall's centre: the hall (finite) beats the global zone (infinite volume).
        assert_eq!(reverb_mix(Vec3::ZERO, zones).feedback, hall.feedback);
        // Inside the room nested in the hall: the smaller room wins.
        assert_eq!(
            reverb_mix(Vec3::new(5.0, 0.0, 0.0), zones).feedback,
            room.feedback
        );
        // Far outside every finite zone: the global one.
        assert_eq!(reverb_mix(Vec3::splat(100.0), zones).feedback, global.feedback);
        // No zones: dry.
        assert_eq!(reverb_mix(Vec3::ZERO, []), ReverbMix::DRY);
    }

    #[test]
    fn overlapping_zones_crossfade_send_in_amplitude() {
        let open = AudioReverbZone {
            shape: ReverbZoneShape::Global,
            send_db: -18.0,
            ..Default::default()
        };
        let room = AudioReverbZone {
            shape: ReverbZoneShape::Box {
                half_extents: Vec3::new(4.0, 3.0, 4.0),
            },
            fade_distance: 2.0,
            send_db: -3.0,
            ..Default::default()
        };
        let c = Vec3::new(10.0, 0.0, 0.0);
        let zones = [(&open, Vec3::ZERO, Quat::IDENTITY), (&room, c, Quat::IDENTITY)];
        // Halfway through the room's blend distance: weight 0.5.
        let mix = reverb_mix(c + Vec3::X * 5.0, zones);
        let expected = crate::spatial::gain_to_db(
            0.5 * crate::spatial::db_to_gain(-18.0) + 0.5 * crate::spatial::db_to_gain(-3.0),
        );
        assert!(
            (mix.effective_send_db() - expected).abs() < 0.4,
            "{} vs {expected}",
            mix.effective_send_db()
        );
        // The dB lerp would give -10.5 dB, 3 dB under the amplitude blend.
        assert!(mix.effective_send_db() > -9.0, "{}", mix.effective_send_db());
    }

    #[test]
    fn effective_send_scales_with_weight() {
        let mut mix = ReverbMix {
            weight: 1.0,
            send_db: -6.0,
            ..ReverbMix::DRY
        };
        assert!((mix.effective_send_db() + 6.0).abs() < 1e-4);
        mix.weight = 0.5;
        assert!((mix.effective_send_db() + 12.02).abs() < 0.05);
        mix.weight = 0.0;
        assert_eq!(mix.effective_send_db(), crate::bus::SILENT_SEND_DB);
    }
}
