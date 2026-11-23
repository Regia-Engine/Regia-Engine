// SPDX-License-Identifier: MPL-2.0
//! IK level of detail for the locomotion components ([`FootPlacement`](crate::foot_ik::FootPlacement)
//! and [`GaitRig`](crate::gait_rig::GaitRig)).
//!
//! A rig picks one [`IkLod`] per frame from its distance to the LOD camera
//! ([`AnimLodCamera`](crate::AnimLodCamera)) or from a host-set value
//! ([`IkLodSettings::force`]):
//!
//! * [`IkLod::Full`] within [`IkLodSettings::full_within`]: probe the ground and solve every frame.
//! * [`IkLod::Reduced`] within [`IkLodSettings::reduced_within`]: probe the ground every
//!   [`IkLodSettings::reduced_interval`] frames and reuse the last hits in between; the plan and
//!   the leg solve still run every frame, so the feet do not stutter.
//! * [`IkLod::Off`] beyond: no probes and no leg solve. Foot placement fades out at its blend-out
//!   rate on the cached hits, then leaves the clip pose untouched.
//!
//! Without a camera and without `force`, a rig is at full detail, so a host that never sets either
//! keeps the old behavior. Presentation only: nothing here is read by a simulation checksum.

use regia_ecs::prelude::*;

/// How much IK work a rig does this frame. Reflected on the components' status (`lod`).
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[reflect(Default)]
pub enum IkLod {
    /// Probe and solve every frame.
    #[default]
    Full,
    /// Probe every [`IkLodSettings::reduced_interval`] frames, reuse the last hits, solve every frame.
    Reduced,
    /// No probes and no solve: the clip pose (after a short fade for foot placement).
    Off,
}

impl IkLod {
    /// The lowercase name used in the agent guide and the status (`full`, `reduced`, `off`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Reduced => "reduced",
            Self::Off => "off",
        }
    }
}

/// When a rig drops to [`IkLod::Reduced`] and [`IkLod::Off`]. Distances are meters from the
/// [`AnimLodCamera`](crate::AnimLodCamera) position to the rig entity's translation.
#[derive(Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Default)]
pub struct IkLodSettings {
    /// Full detail within this distance. Default 30. A non-finite value (or `f32::INFINITY`)
    /// keeps the rig at full detail at any distance.
    pub full_within: f32,
    /// Reduced detail beyond `full_within` and within this distance; off beyond. Default 80. A value
    /// at or below `full_within` has no reduced band (full, then off).
    pub reduced_within: f32,
    /// Frames between ground probes at reduced detail. Default 4. 0 and 1 probe every frame.
    pub reduced_interval: u32,
    /// A host-set level that wins over the distance (a cutscene, a budget manager). Default `None`.
    pub force: Option<IkLod>,
}

impl Default for IkLodSettings {
    fn default() -> Self {
        Self {
            full_within: 30.0,
            reduced_within: 80.0,
            reduced_interval: 4,
            force: None,
        }
    }
}

impl IkLodSettings {
    /// The level for a rig `distance` meters from the camera; `None` (no camera) or a non-finite
    /// distance is [`IkLod::Full`]. [`Self::force`] wins.
    #[must_use]
    pub fn select(&self, distance: Option<f32>) -> IkLod {
        if let Some(lod) = self.force {
            return lod;
        }
        let Some(distance) = distance.filter(|d| d.is_finite()) else {
            return IkLod::Full;
        };
        let full = if self.full_within.is_nan() {
            f32::INFINITY
        } else {
            self.full_within.abs()
        };
        let reduced = if self.reduced_within.is_nan() {
            full
        } else {
            self.reduced_within.abs().max(full)
        };
        if distance <= full {
            IkLod::Full
        } else if distance <= reduced {
            IkLod::Reduced
        } else {
            IkLod::Off
        }
    }

    /// Frames between probes at reduced detail, at least 1.
    #[must_use]
    pub fn interval(&self) -> u32 {
        self.reduced_interval.max(1)
    }
}

/// Probe cadence of one rig: counts reduced frames, staggered by a per-rig phase so a crowd that
/// crosses the boundary together does not probe on the same frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LodCadence {
    frame: u32,
}

impl LodCadence {
    /// Whether this frame probes. `lod` is this frame's level, `phase` a per-rig stagger (the
    /// entity index), `cached` whether earlier hits exist to reuse. Full always probes, off never;
    /// reduced probes on every `interval`-th frame, and whenever nothing is cached yet.
    pub(crate) fn probe_now(
        &mut self,
        lod: IkLod,
        settings: &IkLodSettings,
        phase: u32,
        cached: bool,
    ) -> bool {
        match lod {
            IkLod::Full => {
                self.frame = 0;
                true
            }
            IkLod::Off => false,
            IkLod::Reduced => {
                let interval = settings.interval();
                let due = self.frame.wrapping_add(phase).is_multiple_of(interval);
                self.frame = self.frame.wrapping_add(1);
                due || !cached
            }
        }
    }
}

/// The LOD camera's distance to `position`, `None` without a camera.
pub(crate) fn camera_distance(
    camera: Option<regia_core::math::Vec3>,
    position: regia_core::math::Vec3,
) -> Option<f32> {
    camera.map(|camera| camera.distance(position))
}

/// Register the reflected types.
pub(crate) fn register(app: &mut regia_app::App) {
    app.register_type::<IkLod>()
        .register_type::<IkLodSettings>()
        .register_type::<Option<IkLod>>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_by_distance_and_force() {
        let s = IkLodSettings::default();
        assert_eq!(s.select(None), IkLod::Full);
        assert_eq!(s.select(Some(f32::NAN)), IkLod::Full);
        assert_eq!(s.select(Some(30.0)), IkLod::Full);
        assert_eq!(s.select(Some(30.5)), IkLod::Reduced);
        assert_eq!(s.select(Some(80.0)), IkLod::Reduced);
        assert_eq!(s.select(Some(80.5)), IkLod::Off);
        let forced = IkLodSettings {
            force: Some(IkLod::Off),
            ..s
        };
        assert_eq!(forced.select(None), IkLod::Off);
        let no_band = IkLodSettings {
            full_within: 10.0,
            reduced_within: 5.0,
            ..s
        };
        assert_eq!(no_band.select(Some(11.0)), IkLod::Off);
        let always = IkLodSettings {
            full_within: f32::INFINITY,
            reduced_within: f32::NAN,
            ..s
        };
        assert_eq!(always.select(Some(1.0e9)), IkLod::Full);
    }

    #[test]
    fn reduced_cadence_probes_every_interval_frames() {
        let s = IkLodSettings::default();
        for phase in 0..5 {
            let mut cadence = LodCadence::default();
            let probes = (0..40)
                .filter(|_| cadence.probe_now(IkLod::Reduced, &s, phase, true))
                .count();
            assert_eq!(probes, 10, "phase {phase}");
        }
        let mut cadence = LodCadence::default();
        assert!(
            cadence.probe_now(IkLod::Reduced, &s, 1, false),
            "nothing cached yet"
        );
        assert!(!cadence.probe_now(IkLod::Off, &s, 0, false));
        assert!(cadence.probe_now(IkLod::Full, &s, 3, true));
    }
}
