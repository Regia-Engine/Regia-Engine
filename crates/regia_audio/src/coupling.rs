// SPDX-License-Identifier: MPL-2.0
//! Who touches what: how emitters inject vibration into the ground and how the listener picks it
//! up (`docs/specs/16-audio.md` § 3c).
//!
//! Gameplay owns contact (it knows the physics contacts, the vehicle the pilot sits in, the
//! building they stand in); it writes it into three components the `AudioSync` pass reads:
//! - [`AudioPropagation`] on an emitter: its path class (world, own body, bypass), how strongly it
//!   couples into the ground and how that coupling fades with height above the surface;
//! - [`ListenerCoupling`] on the listener: boots on the ground, sitting in a grounded vehicle,
//!   standing in a structure, or touching nothing (jumping, flying, floating);
//! - [`AudioInteriorVolume`] on a structure: a box of its own (pressurised) air.
//!
//! The listener's coupling filter models the path from the ground into the ear through boots and a
//! suit, a seat and hull, or a floor: a heavy low-pass (bone/structure conduction carries little
//! above a few hundred hertz) and a gain.

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::ground::Spreading;

/// Time for the ground path to fade out when the listener leaves the ground (or back in on
/// landing), seconds.
pub const COUPLING_FADE_SECS: f32 = 0.05;

/// What the listener's body is in contact with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Reflect)]
#[reflect(Default)]
pub enum ListenerContact {
    /// Touching nothing: airborne, jumping, floating. The ground path fades out.
    None,
    /// Standing or walking on the ground (boots and suit).
    #[default]
    Boots,
    /// Seated in a vehicle that is on the ground (seat, frame and hull).
    Vehicle,
    /// Standing in or on a structure connected to the ground (floor and foundation).
    Structure,
}

impl ListenerContact {
    /// Lower-case name for the agent protocol.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Boots => "boots",
            Self::Vehicle => "vehicle",
            Self::Structure => "structure",
        }
    }

    /// Whether the body touches something that carries ground vibration.
    #[must_use]
    pub fn coupled(self) -> bool {
        self != Self::None
    }

    /// The conduction filter into the ear for this contact (`None` when not coupled).
    #[must_use]
    pub fn filter(self) -> Option<CouplingFilter> {
        match self {
            Self::None => None,
            // Bone conduction through boots and suit: the thump, not the crunch.
            Self::Boots => Some(CouplingFilter {
                cutoff_hz: 500.0,
                gain_db: -6.0,
            }),
            // A heavy hull and seat: low, strong rumble.
            Self::Vehicle => Some(CouplingFilter {
                cutoff_hz: 250.0,
                gain_db: -3.0,
            }),
            // A floor slab radiating into the room and up through the feet.
            Self::Structure => Some(CouplingFilter {
                cutoff_hz: 700.0,
                gain_db: -9.0,
            }),
        }
    }
}

/// A conduction path into the ear: low-pass cutoff and gain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CouplingFilter {
    /// Low-pass cutoff, Hz.
    pub cutoff_hz: f32,
    /// Gain, dB.
    pub gain_db: f32,
}

/// Exterior vibration radiated by a structure's walls into its pressurised interior air when the
/// listener is inside but not touching anything (floating in a hab): heavily muffled.
pub const INTERIOR_RADIATION: CouplingFilter = CouplingFilter {
    cutoff_hz: 300.0,
    gain_db: -18.0,
};

/// Airborne sound passing through a structure's wall (both sides have air): muffled.
pub const WALL_TRANSMISSION: CouplingFilter = CouplingFilter {
    cutoff_hz: 400.0,
    gain_db: -35.0,
};

/// The listener's contact with the world. Put it on the [`crate::AudioListener`] entity and keep
/// it current from gameplay (physics contacts, vehicle seat, building). Without one the listener
/// stands on the level's default ground in boots.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct ListenerCoupling {
    /// What the body touches now.
    pub contact: ListenerContact,
    /// Ground material underfoot (`None` = the level's default ground).
    pub ground: Option<String>,
    /// The vehicle the listener sits inside: emitters of this vehicle reach the listener
    /// directly (hull-internal path, the own cockpit), never through the medium.
    pub vehicle: Option<Entity>,
    /// Structure materials crossed from the ground to the listener (e.g. `["concrete"]` for a
    /// foundation, `["concrete", "steel"]` for a deck on a foundation). Each change of material
    /// loses energy at the interface.
    pub structure: Vec<String>,
}

/// Which way an emitter's sound reaches the listener.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Reflect)]
#[reflect(Default)]
pub enum PathClass {
    /// A sound in the world: airborne and/or ground-borne through the medium.
    #[default]
    World,
    /// The listener's own body: breathing, suit, own footsteps by bone conduction. Always heard,
    /// directly.
    Body,
    /// Bypasses the medium entirely (radio, comms, UI, the own cockpit interior).
    Bypass,
}

/// Preset emitter categories (DO27's fixture list) and their ground coupling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Reflect)]
pub enum EmitterCategory {
    /// A projectile or debris hitting the ground.
    Impact,
    /// An explosion on or near the ground.
    Explosion,
    /// A wreck or building collapsing.
    Collapse,
    /// Another pilot's footsteps.
    Footstep,
    /// A pilot landing from a jump.
    Landing,
    /// Deployed-structure machinery (turrets, generators, drills).
    Machinery,
    /// A hull touching down.
    HullLanding,
    /// A hull scraping along the ground.
    HullScrape,
    /// A hull in normal hover: fans and engine. Couples weakly, only within ~1 m of the ground.
    HullHover,
    /// Weapon fire from a weapon planted on the ground.
    PlantedWeapon,
    /// A projectile in flight: air only.
    Projectile,
    /// Radio and comms (chat, wingman, AI callouts, announcer, companion): bypasses the medium.
    Radio,
    /// UI and HUD: bypasses the medium.
    Ui,
    /// The listener's own vehicle interior (cockpit): reaches the listener directly when they sit
    /// in it (set [`AudioPropagation::vehicle`] and [`ListenerCoupling::vehicle`]).
    Cockpit,
    /// The listener's own body (breathing, suit, own footsteps): always heard.
    Body,
}

/// How an emitter's sound propagates. Optional: an emitter without one is a plain world sound
/// with no ground coupling.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct AudioPropagation {
    /// Path class.
    pub class: PathClass,
    /// How much of the source goes into the ground, 0..1 (0 = air only).
    pub ground_coupling: f32,
    /// Height above the surface at which the coupling has faded to zero, metres. 0 = the source
    /// is planted (no height dependence).
    pub coupling_height: f32,
    /// The emitter's height above the surface now, metres (gameplay keeps it current; only used
    /// when `coupling_height > 0`).
    pub height_above_ground: f32,
    /// Level of the ground-borne sound relative to the airborne sound at 1 m, dB.
    pub ground_level_db: f32,
    /// Size of the source region, metres: spreading starts beyond it.
    pub source_size_m: f32,
    /// Surface or body-wave spreading.
    pub spreading: Spreading,
    /// Material the emitter touches (`None` = the level's default ground).
    pub ground: Option<String>,
    /// The vehicle this emitter belongs to (cockpit sounds; see [`ListenerCoupling::vehicle`]).
    pub vehicle: Option<Entity>,
    /// Beyond this the ground path is not rendered, metres.
    pub ground_max_distance: f32,
}

impl Default for AudioPropagation {
    fn default() -> Self {
        Self {
            class: PathClass::World,
            ground_coupling: 0.0,
            coupling_height: 0.0,
            height_above_ground: 0.0,
            ground_level_db: -12.0,
            source_size_m: 1.0,
            spreading: Spreading::Surface,
            ground: None,
            vehicle: None,
            ground_max_distance: 3_000.0,
        }
    }
}

impl AudioPropagation {
    /// The preset of a category.
    #[must_use]
    pub fn for_category(category: EmitterCategory) -> Self {
        let world = |coupling: f32, level_db: f32, size: f32| Self {
            ground_coupling: coupling,
            ground_level_db: level_db,
            source_size_m: size,
            ..Self::default()
        };
        match category {
            EmitterCategory::Impact => world(1.0, -6.0, 2.0),
            EmitterCategory::Explosion => Self {
                ground_max_distance: 6_000.0,
                ..world(1.0, -3.0, 10.0)
            },
            EmitterCategory::Collapse => Self {
                ground_max_distance: 5_000.0,
                ..world(1.0, -3.0, 8.0)
            },
            EmitterCategory::Footstep => Self {
                ground_max_distance: 300.0,
                ..world(0.6, -12.0, 1.0)
            },
            EmitterCategory::Landing => Self {
                ground_max_distance: 500.0,
                ..world(0.8, -9.0, 1.0)
            },
            EmitterCategory::Machinery => world(0.7, -15.0, 3.0),
            EmitterCategory::HullLanding => world(1.0, -6.0, 4.0),
            EmitterCategory::HullScrape => world(0.9, -9.0, 3.0),
            EmitterCategory::HullHover => Self {
                coupling_height: 1.0,
                ..world(0.25, -18.0, 3.0)
            },
            EmitterCategory::PlantedWeapon => world(0.5, -12.0, 1.0),
            EmitterCategory::Projectile | EmitterCategory::Cockpit => world(0.0, -12.0, 1.0),
            EmitterCategory::Radio | EmitterCategory::Ui => Self {
                class: PathClass::Bypass,
                ..Self::default()
            },
            EmitterCategory::Body => Self {
                class: PathClass::Body,
                ..Self::default()
            },
        }
    }

    /// The coupling into the ground now: `ground_coupling`, faded linearly to 0 at
    /// `coupling_height` above the surface.
    #[must_use]
    pub fn effective_coupling(&self) -> f32 {
        let base = self.ground_coupling.clamp(0.0, 1.0);
        if self.coupling_height > 0.0 {
            base * (1.0 - self.height_above_ground.max(0.0) / self.coupling_height).clamp(0.0, 1.0)
        } else {
            base
        }
    }
}

/// A box of its own air (a pressurised hab, a building, a cockpit): sources inside are airborne
/// inside whatever the planet, exterior impacts reach it through the structure. The box is the
/// entity's transform (translation and rotation) with these half extents.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct AudioInteriorVolume {
    /// Half extents of the box, metres (local axes).
    pub half_extents: Vec3,
    /// Whether the volume holds air (false: an unpressurised shell, airborne sound stays off).
    pub pressurised: bool,
    /// Structure material of the walls and floor (interface losses with the ground).
    pub wall: String,
}

impl Default for AudioInteriorVolume {
    fn default() -> Self {
        Self {
            half_extents: Vec3::splat(5.0),
            pressurised: true,
            wall: "concrete".into(),
        }
    }
}

/// An interior volume placed in the world for one sync step.
#[derive(Clone, Debug, PartialEq)]
pub struct InteriorFrame {
    /// Entity of the volume (identity across frames).
    pub entity: Entity,
    /// World translation of the box centre.
    pub center: Vec3,
    /// World rotation of the box.
    pub rotation: regia_core::math::Quat,
    /// The volume's settings.
    pub volume: AudioInteriorVolume,
}

impl InteriorFrame {
    /// Whether `point` lies inside the box.
    #[must_use]
    pub fn contains(&self, point: Vec3) -> bool {
        let local = self.rotation.inverse() * (point - self.center);
        let h = self.volume.half_extents.abs();
        local.x.abs() <= h.x && local.y.abs() <= h.y && local.z.abs() <= h.z
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_coupling_fades_with_height() {
        let mut hover = AudioPropagation::for_category(EmitterCategory::HullHover);
        hover.height_above_ground = 0.0;
        assert!((hover.effective_coupling() - 0.25).abs() < 1e-6);
        hover.height_above_ground = 0.5;
        assert!((hover.effective_coupling() - 0.125).abs() < 1e-6);
        hover.height_above_ground = 3.0;
        assert_eq!(hover.effective_coupling(), 0.0, "normal hover is air-only");
        let mut boom = AudioPropagation::for_category(EmitterCategory::Explosion);
        boom.height_above_ground = 3.0;
        assert_eq!(boom.effective_coupling(), 1.0, "planted sources ignore height");
    }

    #[test]
    fn categories_cover_the_do27_fixture_list() {
        use EmitterCategory::*;
        for on in [
            Impact,
            Explosion,
            Collapse,
            Footstep,
            Landing,
            Machinery,
            HullLanding,
            HullScrape,
        ] {
            assert!(AudioPropagation::for_category(on).ground_coupling > 0.0, "{on:?}");
        }
        assert_eq!(AudioPropagation::for_category(Projectile).ground_coupling, 0.0);
        for bypass in [Radio, Ui] {
            assert_eq!(AudioPropagation::for_category(bypass).class, PathClass::Bypass);
        }
        assert_eq!(AudioPropagation::for_category(Body).class, PathClass::Body);
    }

    #[test]
    fn contact_filters_are_heavy_low_passes() {
        for contact in [
            ListenerContact::Boots,
            ListenerContact::Vehicle,
            ListenerContact::Structure,
        ] {
            let f = contact.filter().expect("coupled");
            assert!((200.0..=800.0).contains(&f.cutoff_hz), "{contact:?}");
        }
        assert!(ListenerContact::None.filter().is_none());
    }

    #[test]
    fn interior_box_respects_rotation() {
        let frame = InteriorFrame {
            entity: Entity::PLACEHOLDER,
            center: Vec3::new(10.0, 0.0, 0.0),
            rotation: regia_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            volume: AudioInteriorVolume {
                half_extents: Vec3::new(4.0, 2.0, 1.0),
                ..AudioInteriorVolume::default()
            },
        };
        // Rotated 90° about Y: the long local X axis lies along world Z.
        assert!(frame.contains(Vec3::new(10.0, 0.0, 3.5)));
        assert!(!frame.contains(Vec3::new(13.5, 0.0, 0.0)));
    }
}
