// SPDX-License-Identifier: MIT-0
//! The scripted transform track of one named entity (`track.ron`).
//!
//! Keys are placed by frame number of the fixed 60 Hz step. [`Track::sample`] is a pure function of
//! the frame: before the first key the pose is the first key's, after the last the last key's. No
//! wall clock, no random input.
//!
//! ```text
//! Track(
//!     version: 1,
//!     entity: "pod",
//!     capsule: (radius: 0.9, half_length: 1.2),
//!     mesh: Some("meshes/hardware.gltf"),            // optional glTF instead of the capsule
//!     swap_mesh: Some("meshes/hardware_damaged.gltf"), // optional, needs swap_frame
//!     swap_frame: Some(420),
//!     color: (0.55, 0.57, 0.6),
//!     height_mode: AboveGround,
//!     interpolation: Linear,
//!     keys: [
//!         (frame: 0,   position: (0.0, 600.0, -400.0), euler_deg: (-20.0, 0.0, 0.0)),
//!         (frame: 240, position: (0.0, 3.0, 0.0),      euler_deg: (-5.0, 0.0, 0.0)),
//!     ],
//! )
//! ```

use std::path::{Component, Path};

use regia_core::glam::{EulerRot, Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::{ErrorCode, ShowcaseError};

/// Format version of `track.ron`.
pub const TRACK_VERSION: u32 = 1;

/// Most keys a track may hold.
pub const MAX_KEYS: usize = 100_000;

/// How the key `position.y` is read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeightMode {
    /// `y` is a world height in metres.
    Absolute,
    /// `y` is metres above the terrain surface under the entity.
    #[default]
    AboveGround,
}

/// How poses between two keys are blended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Interpolation {
    /// Straight lines between positions.
    #[default]
    Linear,
    /// A uniform Catmull-Rom spline through the positions (end keys repeat).
    CatmullRom,
}

/// The stand-in mesh of the entity: a capsule standing along its local Y axis.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapsuleSpec {
    /// Radius, metres.
    pub radius: f32,
    /// Half the length of the straight section, metres.
    pub half_length: f32,
}

impl Default for CapsuleSpec {
    fn default() -> Self {
        Self {
            radius: 0.5,
            half_length: 0.5,
        }
    }
}

/// One pose at a frame.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    /// Frame number of the 60 Hz step.
    pub frame: u32,
    /// World position (`y` per [`HeightMode`]).
    pub position: (f32, f32, f32),
    /// Rotation as (pitch about X, yaw about Y, roll about Z) in degrees, applied yaw, pitch, roll.
    #[serde(default)]
    pub euler_deg: (f32, f32, f32),
}

impl Key {
    /// The rotation of this key.
    #[must_use]
    pub fn rotation(&self) -> Quat {
        Quat::from_euler(
            EulerRot::YXZ,
            self.euler_deg.1.to_radians(),
            self.euler_deg.0.to_radians(),
            self.euler_deg.2.to_radians(),
        )
    }

    fn position(&self) -> Vec3 {
        Vec3::new(self.position.0, self.position.1, self.position.2)
    }
}

/// A pose: where the entity is and how it is turned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    /// Position (`y` per the track's [`HeightMode`]).
    pub position: Vec3,
    /// Rotation.
    pub rotation: Quat,
}

/// The track of one entity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Track {
    /// Format version ([`TRACK_VERSION`]).
    pub version: u32,
    /// Name of the entity (`[a-z0-9_-]`, 1 to 64 characters).
    pub entity: String,
    /// The stand-in capsule, drawn when [`Self::mesh`] is absent.
    #[serde(default)]
    pub capsule: CapsuleSpec,
    /// A `.gltf` or `.glb` file, relative to the scene directory, drawn instead of the capsule (its materials
    /// are the file's own; [`Self::color`] only tints the capsule).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh: Option<String>,
    /// A `.gltf` or `.glb` file, relative to the scene directory, that replaces the entity from
    /// [`Self::swap_frame`] on (the damaged variant). Needs [`Self::swap_frame`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_mesh: Option<String>,
    /// The track frame the entity swaps to [`Self::swap_mesh`]. Needs [`Self::swap_mesh`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_frame: Option<u32>,
    /// Linear base colour of the capsule.
    #[serde(default = "default_color")]
    pub color: (f32, f32, f32),
    /// How key heights are read.
    #[serde(default)]
    pub height_mode: HeightMode,
    /// How poses between keys are blended.
    #[serde(default)]
    pub interpolation: Interpolation,
    /// Keys, frames strictly increasing.
    pub keys: Vec<Key>,
}

fn default_color() -> (f32, f32, f32) {
    (0.6, 0.6, 0.62)
}

fn finite3(v: (f32, f32, f32)) -> bool {
    v.0.is_finite() && v.1.is_finite() && v.2.is_finite()
}

impl Track {
    /// Parses and validates `text`.
    ///
    /// # Errors
    /// [`ErrorCode::TrackMalformed`] naming `path`.
    pub fn from_ron(text: &str, path: &Path) -> Result<Self, ShowcaseError> {
        let bad = |message: String| ShowcaseError::new(ErrorCode::TrackMalformed, path, message);
        let track: Track = ron::from_str(text).map_err(|e| bad(e.to_string()))?;
        track.validate().map_err(bad)?;
        Ok(track)
    }

    /// Checks every field.
    ///
    /// # Errors
    /// A message naming the field.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != TRACK_VERSION {
            return Err(format!(
                "version {} (this build reads {TRACK_VERSION})",
                self.version
            ));
        }
        let name_ok = !self.entity.is_empty()
            && self.entity.len() <= 64
            && self
                .entity
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if !name_ok {
            return Err("entity must be 1 to 64 characters of a-z, 0-9, _ or -".into());
        }
        if self.keys.is_empty() {
            return Err("a track needs at least one key".into());
        }
        if self.keys.len() > MAX_KEYS {
            return Err(format!("{} keys (at most {MAX_KEYS})", self.keys.len()));
        }
        for (i, key) in self.keys.iter().enumerate() {
            if !finite3(key.position) || !finite3(key.euler_deg) {
                return Err(format!("key {i} holds a non-finite number"));
            }
            if i > 0 && key.frame <= self.keys[i - 1].frame {
                return Err(format!("key {i}: frames must increase (frame {})", key.frame));
            }
        }
        let c = self.capsule;
        if !(c.radius.is_finite() && c.radius > 0.0 && c.half_length.is_finite() && c.half_length >= 0.0) {
            return Err("capsule radius must be > 0 and half_length >= 0".into());
        }
        if !finite3(self.color) {
            return Err("color is not finite".into());
        }
        for (field, path) in [("mesh", &self.mesh), ("swap_mesh", &self.swap_mesh)] {
            if let Some(path) = path {
                let lower = path.to_ascii_lowercase();
                if !(lower.ends_with(".gltf") || lower.ends_with(".glb")) {
                    return Err(format!("{field} '{path}' must name a .gltf or .glb file"));
                }
                let escapes = Path::new(path).components().any(|c| {
                    matches!(
                        c,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                });
                if escapes {
                    return Err(format!(
                        "{field} '{path}' must be relative and stay inside the scene directory"
                    ));
                }
            }
        }
        match (&self.swap_mesh, self.swap_frame) {
            (Some(_), None) => return Err("swap_mesh needs swap_frame".into()),
            (None, Some(_)) => return Err("swap_frame needs swap_mesh".into()),
            _ => {}
        }
        Ok(())
    }

    /// The last key's frame.
    #[must_use]
    pub fn last_frame(&self) -> u32 {
        self.keys.last().map_or(0, |k| k.frame)
    }

    /// The pose at `frame` (frames before the first key hold the first pose, frames after the last
    /// hold the last). A track always has a key once validated; an empty one answers the origin.
    #[must_use]
    pub fn sample(&self, frame: i64) -> Pose {
        let keys = &self.keys;
        let (Some(first), Some(last)) = (keys.first(), keys.last()) else {
            return Pose {
                position: Vec3::ZERO,
                rotation: Quat::IDENTITY,
            };
        };
        if frame <= i64::from(first.frame) {
            return Pose {
                position: first.position(),
                rotation: first.rotation(),
            };
        }
        if frame >= i64::from(last.frame) {
            return Pose {
                position: last.position(),
                rotation: last.rotation(),
            };
        }
        // `upper` is the first key after `frame`; the segment is `upper - 1 .. upper`.
        let upper = keys.partition_point(|k| i64::from(k.frame) <= frame);
        let (a, b) = (&keys[upper - 1], &keys[upper]);
        let span = (b.frame - a.frame) as f32;
        let t = (frame - i64::from(a.frame)) as f32 / span;
        let position = match self.interpolation {
            Interpolation::Linear => a.position().lerp(b.position(), t),
            Interpolation::CatmullRom => {
                let p0 = keys[upper.saturating_sub(2)].position();
                let p3 = keys[(upper + 1).min(keys.len() - 1)].position();
                catmull_rom(p0, a.position(), b.position(), p3, t)
            }
        };
        Pose {
            position,
            rotation: a.rotation().slerp(b.rotation(), t),
        }
    }
}

/// Uniform Catmull-Rom between `p1` and `p2` at `t` in `0..=1` (`p0`, `p3` are the neighbours).
fn catmull_rom(p0: Vec3, p1: Vec3, p2: Vec3, p3: Vec3, t: f32) -> Vec3 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * ((2.0 * p1)
        + (p2 - p0) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (3.0 * p1 - p0 - 3.0 * p2 + p3) * t3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(interpolation: Interpolation) -> Track {
        Track {
            version: TRACK_VERSION,
            entity: "pod".into(),
            capsule: CapsuleSpec::default(),
            mesh: None,
            swap_mesh: None,
            swap_frame: None,
            color: default_color(),
            height_mode: HeightMode::Absolute,
            interpolation,
            keys: vec![
                Key {
                    frame: 10,
                    position: (0.0, 0.0, 0.0),
                    euler_deg: (0.0, 0.0, 0.0),
                },
                Key {
                    frame: 20,
                    position: (10.0, 20.0, 30.0),
                    euler_deg: (0.0, 90.0, 0.0),
                },
                Key {
                    frame: 40,
                    position: (10.0, 20.0, 70.0),
                    euler_deg: (0.0, 90.0, 0.0),
                },
            ],
        }
    }

    fn near(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-4
    }

    #[test]
    fn holds_before_the_first_and_after_the_last_key() {
        let t = track(Interpolation::Linear);
        assert!(near(t.sample(-50).position, Vec3::ZERO));
        assert!(near(t.sample(0).position, Vec3::ZERO));
        assert!(near(t.sample(10).position, Vec3::ZERO));
        assert!(near(t.sample(40).position, Vec3::new(10.0, 20.0, 70.0)));
        assert!(near(t.sample(10_000).position, Vec3::new(10.0, 20.0, 70.0)));
    }

    #[test]
    fn linear_interpolation_is_exact_at_keys_and_between() {
        let t = track(Interpolation::Linear);
        assert!(near(t.sample(20).position, Vec3::new(10.0, 20.0, 30.0)));
        assert!(near(t.sample(15).position, Vec3::new(5.0, 10.0, 15.0)));
        assert!(near(t.sample(30).position, Vec3::new(10.0, 20.0, 50.0)));
        // A quarter of the way through the first segment (2.5 frames is not a frame; 12.5 neither):
        // frame 12 is 0.2 of the way.
        assert!(near(t.sample(12).position, Vec3::new(2.0, 4.0, 6.0)));
    }

    #[test]
    fn rotation_slerps_between_keys() {
        let t = track(Interpolation::Linear);
        let mid = t.sample(15).rotation;
        let want = Quat::from_rotation_y(45.0_f32.to_radians());
        assert!(mid.angle_between(want) < 1e-4, "{mid:?} vs {want:?}");
        assert!(
            t.sample(20)
                .rotation
                .angle_between(Quat::from_rotation_y(FRAC_90))
                < 1e-4
        );
    }

    const FRAC_90: f32 = std::f32::consts::FRAC_PI_2;

    #[test]
    fn catmull_rom_passes_through_the_keys() {
        let t = track(Interpolation::CatmullRom);
        for key in &t.keys {
            assert!(near(t.sample(i64::from(key.frame)).position, key.position()));
        }
        // Between keys it is smooth: no corner at the middle key, so the step lengths on both
        // sides of frame 20 are close.
        let before = (t.sample(20).position - t.sample(19).position).length();
        let after = (t.sample(21).position - t.sample(20).position).length();
        assert!(before > 0.0 && after > 0.0);
        assert!(!near(t.sample(15).position, Vec3::new(5.0, 10.0, 15.0)));
    }

    #[test]
    fn sampling_is_a_pure_function_of_the_frame() {
        let t = track(Interpolation::CatmullRom);
        let a: Vec<Pose> = (0..60).map(|f| t.sample(f)).collect();
        let b: Vec<Pose> = (0..60).map(|f| t.sample(f)).collect();
        assert_eq!(a, b);
    }

    #[test]
    fn a_single_key_track_holds_still() {
        let mut t = track(Interpolation::Linear);
        t.keys.truncate(1);
        assert!(near(t.sample(-3).position, Vec3::ZERO));
        assert!(near(t.sample(500).position, Vec3::ZERO));
        assert_eq!(t.last_frame(), 10);
    }

    #[test]
    fn parses_the_documented_file() {
        let text = r#"Track(
            version: 1,
            entity: "pod",
            capsule: (radius: 0.9, half_length: 1.2),
            height_mode: AboveGround,
            keys: [
                (frame: 0, position: (0.0, 600.0, -400.0), euler_deg: (-20.0, 0.0, 0.0)),
                (frame: 240, position: (0.0, 3.0, 0.0)),
            ],
        )"#;
        let t = Track::from_ron(text, Path::new("track.ron")).expect("parses");
        assert_eq!(t.entity, "pod");
        assert_eq!(t.keys.len(), 2);
        assert_eq!(t.height_mode, HeightMode::AboveGround);
        assert_eq!(t.keys[1].euler_deg, (0.0, 0.0, 0.0));
    }

    #[test]
    fn parses_a_mesh_and_a_swap() {
        let text = r#"Track(
            version: 1,
            entity: "hardware",
            mesh: Some("meshes/hw.gltf"),
            swap_mesh: Some("meshes/hw_damaged.glb"),
            swap_frame: Some(420),
            keys: [(frame: 0, position: (0.0, 0.0, 0.0))],
        )"#;
        let t = Track::from_ron(text, Path::new("track.ron")).expect("parses");
        assert_eq!(t.mesh.as_deref(), Some("meshes/hw.gltf"));
        assert_eq!(t.swap_mesh.as_deref(), Some("meshes/hw_damaged.glb"));
        assert_eq!(t.swap_frame, Some(420));
    }

    #[test]
    fn rejects_bad_tracks_with_the_track_code() {
        let p = Path::new("track.ron");
        let key = "(frame: 0, position: (0.0, 0.0, 0.0))";
        let cases = [
            "not ron at all".to_string(),
            format!(r#"Track(version: 2, entity: "a", keys: [{key}])"#),
            format!(r#"Track(version: 1, entity: "", keys: [{key}])"#),
            format!(r#"Track(version: 1, entity: "Pod", keys: [{key}])"#),
            r#"Track(version: 1, entity: "a", keys: [])"#.to_string(),
            format!(
                r#"Track(version: 1, entity: "a", keys: [{key}, (frame: 0, position: (1.0, 0.0, 0.0))])"#
            ),
            r#"Track(version: 1, entity: "a", keys: [(frame: 0, position: (nan, 0.0, 0.0))])"#.to_string(),
            format!(r#"Track(version: 1, entity: "a", capsule: (radius: 0.0), keys: [{key}])"#),
            format!(r#"Track(version: 1, entity: "a", extra: 1, keys: [{key}])"#),
            format!(r#"Track(version: 1, entity: "a", mesh: Some("hw.png"), keys: [{key}])"#),
            format!(r#"Track(version: 1, entity: "a", swap_mesh: Some("hw.glb"), keys: [{key}])"#),
            format!(r#"Track(version: 1, entity: "a", swap_frame: Some(3), keys: [{key}])"#),
        ];
        for text in &cases {
            let err = Track::from_ron(text, p).expect_err(text);
            assert_eq!(err.code, ErrorCode::TrackMalformed, "{text}: {err}");
        }
    }
}
