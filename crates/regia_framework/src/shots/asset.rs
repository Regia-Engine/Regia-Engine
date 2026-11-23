// SPDX-License-Identifier: MPL-2.0
//! The shot-sequence asset (`.shots.ron`): the data model, its RON form, validation, and the
//! format version contract (`docs/specs/12-assets.md` § 4a, `docs/formats/shots-ron.md`).
//!
//! A [`ShotSequence`] is an ordered list of [`Shot`]s played back to back. Each shot has its own
//! clock starting at zero: a [`CameraPath`] (position keys, a look-at rule), a field-of-view
//! track, a roll track, optional depth-of-field tracks, and a fade in/out. The boundary between
//! two shots is a hard cut. Everything is plain keyed data (times in seconds of the shot, values
//! in metres, degrees and f-stops), so a hand-written file, an agent and a future curve-less
//! editor panel all edit the same thing.
//!
//! ```
//! use regia_framework::shots::ShotSequence;
//!
//! let sequence = ShotSequence::from_ron(r#"(
//!     version: 1,
//!     shots: [
//!         (
//!             name: "establishing",
//!             duration: 4.0,
//!             camera: (
//!                 path: (
//!                     keys: [(time: 0.0, position: (0.0, 2.0, 10.0)), (time: 4.0, position: (0.0, 2.0, 4.0))],
//!                     look_at: Point((0.0, 1.0, 0.0)),
//!                 ),
//!                 fov: [(time: 0.0, value: 40.0)],
//!             ),
//!             fade_in: 0.5,
//!         ),
//!     ],
//! )"#).expect("valid sequence");
//! assert_eq!(sequence.duration(), 4.0);
//! ```

use regia_core::version::{self, VersionStep};
use serde::{Deserialize, Serialize};

/// Current `.shots.ron` version. v3 (ADR-8640, the v0.6 format bump): a camera path or look-at
/// track may keep keys past its shot's end (a shortened Catmull-Rom shot keeps its tail keys as
/// control keys); the shape is unchanged, the number fences such files off from older builds.
pub const SHOT_SEQUENCE_FORMAT_VERSION: u32 = 3;

/// Vertical field of view, degrees, of a shot with an empty `fov` track (the renderer's default
/// camera).
pub const DEFAULT_FOV_DEGREES: f32 = 45.0;

/// Aperture, f-stops, of a depth-of-field shot with an empty `aperture` track.
pub const DEFAULT_APERTURE: f32 = 2.8;

/// Focus distance, metres, of a depth-of-field shot with an empty `focus_distance` track.
pub const DEFAULT_FOCUS_DISTANCE: f32 = 10.0;

/// Largest coordinate magnitude, metres, of a path key or look-at point. Far past any camera a
/// game places, and small enough that the spline's phantom points (`2 P0 - P1`) and knot
/// distances stay finite in `f32`.
pub const MAX_PATH_COORDINATE: f32 = 1.0e12;

/// Shot sequence errors.
#[derive(Debug, thiserror::Error)]
pub enum ShotSequenceError {
    /// The file is a newer version than this build reads.
    #[error("shot sequence version {found} is newer than supported version {expected}; upgrade the engine")]
    Version {
        /// Version in the file.
        found: u32,
        /// Version this build reads.
        expected: u32,
    },
    /// RON syntax or shape error.
    #[error("shot sequence parse error: {0}")]
    Parse(String),
    /// Semantic error (unsorted keys, bad duration, out-of-range value).
    #[error("invalid shot sequence: {0}")]
    Invalid(String),
    /// A migration step failed.
    #[error("shot sequence migration failed: {0}")]
    Migration(String),
}

fn default_fps() -> f32 {
    30.0
}

fn black() -> [f32; 3] {
    [0.0; 3]
}

/// How a scalar track eases from one key to the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Ease {
    /// Straight line between the two keys.
    #[default]
    Linear,
    /// Holds this key's value until the next key (a cut in the value).
    Step,
    /// Smoothstep (`3u² - 2u³`): zero slope at both keys.
    Smooth,
}

/// One keyed value of a [`Track`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScalarKey {
    /// Seconds since the start of the shot.
    pub time: f32,
    /// The value at `time`.
    pub value: f32,
    /// How the value eases from this key to the next.
    #[serde(default)]
    pub ease: Ease,
}

/// A keyed scalar over shot time. Before the first key it holds the first value, after the last
/// key the last value; an empty track means "use the default" of whatever it drives.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Track(pub Vec<ScalarKey>);

/// How a [`CameraPath`] moves between its position keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PathInterp {
    /// Straight segments.
    Linear,
    /// Smooth curve through every key (centripetal Catmull-Rom).
    #[default]
    CatmullRom,
    /// Holds each key until the next one (hard moves).
    Step,
}

/// One position key of a path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PathKey {
    /// Seconds since the start of the shot.
    pub time: f32,
    /// World position, metres.
    pub position: [f32; 3],
}

/// Where the camera looks.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub enum LookAt {
    /// Along the direction of travel (the path's tangent).
    #[default]
    Heading,
    /// A fixed world point.
    Point([f32; 3]),
    /// A moving target, keyed and interpolated like the path (same `interp`).
    Track(Vec<PathKey>),
}

/// The camera's position over a shot and the rule for where it looks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CameraPath {
    /// How the camera moves between keys.
    #[serde(default)]
    pub interp: PathInterp,
    /// Position keys, strictly increasing in time.
    pub keys: Vec<PathKey>,
    /// Where the camera looks.
    #[serde(default)]
    pub look_at: LookAt,
}

/// Depth-of-field tracks of a shot. Absent (`None`) means no depth of field.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct DepthOfFieldTracks {
    /// Distance to the plane in focus, metres (default 10 when empty). Ignored when
    /// `focus_on_target` is set.
    #[serde(default)]
    pub focus_distance: Track,
    /// Aperture, f-stops (default 2.8 when empty). Smaller is shallower.
    #[serde(default)]
    pub aperture: Track,
    /// Focus on the look-at target: the focus distance is the distance from the eye to the target.
    #[serde(default)]
    pub focus_on_target: bool,
    /// Hexagonal bokeh instead of a Gaussian blur.
    #[serde(default)]
    pub bokeh: bool,
}

/// One orientation key of a shot's rotation track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RotationKey {
    /// Seconds since the start of the shot.
    pub time: f32,
    /// Camera orientation as a unit quaternion `[x, y, z, w]` (looks down −Z, +Y up), before roll.
    /// Normalized on use; the length must not be zero.
    pub rotation: [f32; 4],
    /// How the orientation moves from this key to the next: `Linear` is a spherical interpolation
    /// along the shortest arc, `Smooth` the same with a smoothstep on the parameter, `Step` holds.
    #[serde(default)]
    pub ease: Ease,
}

/// Everything the camera does within one shot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShotCamera {
    /// Position keys and look-at rule.
    pub path: CameraPath,
    /// Vertical field of view, degrees (45 when empty). The range is 1 to 170.
    #[serde(default)]
    pub fov: Track,
    /// Roll about the view axis, degrees (0 when empty). Positive rolls the camera clockwise as
    /// seen from behind it.
    #[serde(default)]
    pub roll: Track,
    /// Depth-of-field tracks, if the shot uses depth of field.
    #[serde(default)]
    pub dof: Option<DepthOfFieldTracks>,
    /// Orientation keys (v2). Empty = the orientation comes from `path.look_at`. With keys, the
    /// keyed orientation replaces the look-at rule for the camera's rotation (before `roll`
    /// is applied); `path.look_at` still gives `ShotSample::target` and the `dof.focus_on_target`
    /// distance. Keys strictly increase in time; before the first key and after the last the
    /// orientation holds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rotation: Vec<RotationKey>,
}

/// One shot: a camera move of fixed duration and its fades.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Shot {
    /// Unique, non-empty name (what agents and editors refer to the shot by).
    pub name: String,
    /// Length in seconds, greater than zero.
    pub duration: f32,
    /// The camera's motion and lens.
    pub camera: ShotCamera,
    /// Seconds spent fading from `fade_color` to the picture at the start of the shot.
    #[serde(default)]
    pub fade_in: f32,
    /// Seconds spent fading from the picture to `fade_color` at the end of the shot.
    #[serde(default)]
    pub fade_out: f32,
    /// Linear RGB of the fade, black by default.
    #[serde(default = "black")]
    pub fade_color: [f32; 3],
}

/// The `.shots.ron` document: shots played back to back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShotSequence {
    /// Format version ([`SHOT_SEQUENCE_FORMAT_VERSION`]).
    pub version: u32,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Authoring frame rate, frames per second. Only converts frame numbers to seconds
    /// (`render.sequence { frame }`); playback is by time.
    #[serde(default = "default_fps")]
    pub fps: f32,
    /// The shots, in playback order. At least one.
    pub shots: Vec<Shot>,
}

impl Default for ShotSequence {
    fn default() -> Self {
        Self {
            version: SHOT_SEQUENCE_FORMAT_VERSION,
            name: String::new(),
            fps: default_fps(),
            shots: Vec::new(),
        }
    }
}

impl ShotSequence {
    /// Parses a `.shots.ron`: version check, migration, validation.
    pub fn from_ron(text: &str) -> Result<Self, ShotSequenceError> {
        let found = version::peek_ron_version(text).map_err(ShotSequenceError::Parse)?;
        if found > SHOT_SEQUENCE_FORMAT_VERSION {
            return Err(ShotSequenceError::Version {
                found,
                expected: SHOT_SEQUENCE_FORMAT_VERSION,
            });
        }
        let mut sequence: Self = ron::from_str(text).map_err(|e| ShotSequenceError::Parse(e.to_string()))?;
        sequence.migrate_to_latest()?;
        sequence.validate()?;
        Ok(sequence)
    }

    /// Serializes to RON with a trailing newline. Re-saving an unchanged sequence is byte-identical.
    pub fn to_ron(&self) -> Result<String, ShotSequenceError> {
        let mut text = ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| ShotSequenceError::Parse(e.to_string()))?;
        text.push('\n');
        Ok(text)
    }

    /// Rejects a newer version and runs the migration chain on an older one
    /// (`docs/specs/12-assets.md` § 4a).
    pub fn migrate_to_latest(&mut self) -> Result<u32, ShotSequenceError> {
        if self.version > SHOT_SEQUENCE_FORMAT_VERSION {
            return Err(ShotSequenceError::Version {
                found: self.version,
                expected: SHOT_SEQUENCE_FORMAT_VERSION,
            });
        }
        if self.version < SHOT_SEQUENCE_FORMAT_VERSION {
            self.version = version::migrate(
                self,
                self.version,
                shot_sequence_migrations(),
                SHOT_SEQUENCE_FORMAT_VERSION,
            )
            .map_err(|e| ShotSequenceError::Migration(e.to_string()))?;
        }
        Ok(self.version)
    }

    /// Checks everything playback relies on: at least one shot with a unique name and a positive
    /// duration; sorted, finite, in-range keys on every path and track; lens values inside
    /// sane ranges; fades that fit in their shot.
    pub fn validate(&self) -> Result<(), ShotSequenceError> {
        let invalid = |m: String| Err(ShotSequenceError::Invalid(m));
        if !(self.fps.is_finite() && self.fps > 0.0 && self.fps <= 1000.0) {
            return invalid(format!("fps {} must be in (0, 1000]", self.fps));
        }
        if self.shots.is_empty() {
            return invalid("a sequence needs at least one shot".into());
        }
        let mut names = std::collections::BTreeSet::new();
        for shot in &self.shots {
            if shot.name.is_empty() {
                return invalid("a shot has an empty name".into());
            }
            if !names.insert(shot.name.as_str()) {
                return invalid(format!("duplicate shot name `{}`", shot.name));
            }
            shot.validate()?;
        }
        Ok(())
    }
}

impl Shot {
    /// Validates one shot (see [`ShotSequence::validate`]).
    pub fn validate(&self) -> Result<(), ShotSequenceError> {
        let name = &self.name;
        let invalid = |m: String| Err(ShotSequenceError::Invalid(format!("shot `{name}`: {m}")));
        if !(self.duration.is_finite() && self.duration > 0.0) {
            return invalid(format!("duration {} must be greater than zero", self.duration));
        }
        for (label, fade) in [("fade_in", self.fade_in), ("fade_out", self.fade_out)] {
            if !(fade.is_finite() && fade >= 0.0) {
                return invalid(format!("{label} {fade} must be zero or more"));
            }
        }
        if self.fade_in + self.fade_out > self.duration {
            return invalid(format!(
                "fade_in {} + fade_out {} is longer than the shot ({})",
                self.fade_in, self.fade_out, self.duration
            ));
        }
        if self.fade_color.iter().any(|c| !(0.0..=1.0).contains(c)) {
            return invalid(format!("fade_color {:?} must be in [0, 1]", self.fade_color));
        }
        let camera = &self.camera;
        check_path_keys(name, "path.keys", &camera.path.keys, self.duration)?;
        match &camera.path.look_at {
            LookAt::Heading => {}
            LookAt::Point(p) => {
                if p.iter().any(|v| !v.is_finite()) {
                    return invalid(format!("look_at point {p:?} is not finite"));
                }
                if p.iter().any(|v| v.abs() > MAX_PATH_COORDINATE) {
                    return invalid(format!(
                        "look_at point {p:?} is farther than {MAX_PATH_COORDINATE} m"
                    ));
                }
            }
            LookAt::Track(keys) => check_path_keys(name, "look_at track", keys, self.duration)?,
        }
        check_rotation_keys(name, &camera.rotation, self.duration)?;
        check_track(name, "fov", &camera.fov, self.duration, 1.0, 170.0)?;
        check_track(name, "roll", &camera.roll, self.duration, -3600.0, 3600.0)?;
        if let Some(dof) = &camera.dof {
            check_track(
                name,
                "dof.focus_distance",
                &dof.focus_distance,
                self.duration,
                1.0e-3,
                1.0e6,
            )?;
            check_track(name, "dof.aperture", &dof.aperture, self.duration, 0.1, 128.0)?;
        }
        Ok(())
    }
}

fn check_path_keys(
    shot: &str,
    what: &str,
    keys: &[PathKey],
    _duration: f32,
) -> Result<(), ShotSequenceError> {
    let invalid = |m: String| Err(ShotSequenceError::Invalid(format!("shot `{shot}`: {what} {m}")));
    if keys.is_empty() {
        return invalid("needs at least one key".into());
    }
    let mut last = f32::NEG_INFINITY;
    for key in keys {
        // A key may sit past the shot's end (ADR-8640): a shortened Catmull-Rom path keeps its
        // keys there as control keys so the curve before the cut does not bend; the clock never
        // reaches them.
        if !key.time.is_finite() || key.time < 0.0 {
            return invalid(format!("key time {} is negative or not finite", key.time));
        }
        if key.time <= last {
            return invalid(format!(
                "key times must strictly increase (found {} after {last})",
                key.time
            ));
        }
        if key.position.iter().any(|v| !v.is_finite()) {
            return invalid(format!("key at {} has a non-finite position", key.time));
        }
        if key.position.iter().any(|v| v.abs() > MAX_PATH_COORDINATE) {
            return invalid(format!(
                "key at {} is farther than {MAX_PATH_COORDINATE} m",
                key.time
            ));
        }
        last = key.time;
    }
    Ok(())
}

fn check_rotation_keys(shot: &str, keys: &[RotationKey], duration: f32) -> Result<(), ShotSequenceError> {
    let invalid = |m: String| Err(ShotSequenceError::Invalid(format!("shot `{shot}`: rotation {m}")));
    let mut last = f32::NEG_INFINITY;
    for key in keys {
        if !key.time.is_finite() || key.time < 0.0 || key.time > duration {
            return invalid(format!("key time {} is outside 0..={duration}", key.time));
        }
        if key.time <= last {
            return invalid(format!(
                "key times must strictly increase (found {} after {last})",
                key.time
            ));
        }
        let length_squared: f32 = key.rotation.iter().map(|v| v * v).sum();
        if key.rotation.iter().any(|v| !v.is_finite())
            || !(length_squared > 1.0e-12 && length_squared.is_finite())
        {
            return invalid(format!(
                "key at {} has a zero or non-finite quaternion {:?}",
                key.time, key.rotation
            ));
        }
        last = key.time;
    }
    Ok(())
}

fn check_track(
    shot: &str,
    what: &str,
    track: &Track,
    duration: f32,
    min: f32,
    max: f32,
) -> Result<(), ShotSequenceError> {
    let invalid = |m: String| Err(ShotSequenceError::Invalid(format!("shot `{shot}`: {what} {m}")));
    let mut last = f32::NEG_INFINITY;
    for key in &track.0 {
        if !key.time.is_finite() || key.time < 0.0 || key.time > duration {
            return invalid(format!("key time {} is outside 0..={duration}", key.time));
        }
        if key.time <= last {
            return invalid(format!(
                "key times must strictly increase (found {} after {last})",
                key.time
            ));
        }
        if !(key.value.is_finite() && key.value >= min && key.value <= max) {
            return invalid(format!(
                "value {} at {} is outside {min}..={max}",
                key.value, key.time
            ));
        }
        last = key.time;
    }
    Ok(())
}

/// The `.shots.ron` migration chain. v1 → v2 only adds the optional `rotation` track (empty =
/// the v1 look-at behavior), and v2 → v3 only allows path and look-at keys past a shot's end
/// (ADR-8640), so both steps change nothing but the version. Validation runs after the chain, so
/// a v2 file a v0.6 dev build wrote with tail keys loads too.
fn shot_sequence_migrations() -> &'static [VersionStep<ShotSequence>] {
    &[
        VersionStep {
            from: 1,
            to: 2,
            migrate: |_| Ok(()),
        },
        VersionStep {
            from: 2,
            to: 3,
            migrate: |_| Ok(()),
        },
    ]
}

/// The `from` versions of the `.shots.ron` migration steps (for `cargo xtask check-formats`).
#[must_use]
pub fn shot_sequence_migration_sources() -> Vec<u32> {
    shot_sequence_migrations().iter().map(|s| s.from).collect()
}

/// The serde shape fingerprint of `.shots.ron` (hashed by `cargo xtask check-formats`): the field
/// list of every struct in the tree, then the variant spellings of every enum.
#[must_use]
pub fn shot_sequence_schema_fingerprint() -> String {
    use regia_core::schema::{serde_field_names, serde_variant_names};
    regia_core::schema::join_field_lists(&[
        serde_field_names::<ShotSequence>(),
        serde_field_names::<Shot>(),
        serde_field_names::<ShotCamera>(),
        serde_field_names::<RotationKey>(),
        serde_field_names::<CameraPath>(),
        serde_field_names::<PathKey>(),
        serde_field_names::<ScalarKey>(),
        serde_field_names::<DepthOfFieldTracks>(),
        serde_variant_names::<Ease>(),
        serde_variant_names::<PathInterp>(),
        serde_variant_names::<LookAt>(),
    ])
}

/// Builders shared by this crate's shot tests.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// A path key on the X axis.
    pub(crate) fn key(time: f32, x: f32) -> PathKey {
        PathKey {
            time,
            position: [x, 0.0, 0.0],
        }
    }

    /// A shot moving along +X from 0 to 10 m over `duration`, looking at a fixed point.
    pub(crate) fn shot(name: &str, duration: f32) -> Shot {
        Shot {
            name: name.into(),
            duration,
            camera: ShotCamera {
                path: CameraPath {
                    interp: PathInterp::Linear,
                    keys: vec![key(0.0, 0.0), key(duration, 10.0)],
                    look_at: LookAt::Point([0.0, 0.0, -10.0]),
                },
                fov: Track::default(),
                roll: Track::default(),
                dof: None,
                rotation: Vec::new(),
            },
            fade_in: 0.0,
            fade_out: 0.0,
            fade_color: black(),
        }
    }

    /// A sequence of the given shots with default metadata.
    pub(crate) fn sequence(shots: Vec<Shot>) -> ShotSequence {
        ShotSequence {
            shots,
            ..ShotSequence::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{key, sequence, shot};
    use super::*;

    #[test]
    fn a_key_too_far_for_the_spline_is_refused_and_sampling_stays_finite() {
        // The audit's document: a Catmull-Rom phantom point `2 * 2e38 - 0` is infinite (ADR-1660).
        let text = r#"(version: 2, shots: [(name: "s", duration: 1.0, camera: (path: (
            keys: [(time: 0.0, position: (2.0e38, 0.0, 0.0)), (time: 1.0, position: (0.0, 0.0, 0.0))],
            look_at: Point((0.0, 0.0, 0.0)))))])"#;
        let e = ShotSequence::from_ron(text).expect_err("too far");
        assert!(e.to_string().contains("farther than"), "{e}");
        let point = text
            .replace("(2.0e38, 0.0, 0.0)", "(1.0, 0.0, 0.0)")
            .replace("Point((0.0, 0.0, 0.0))", "Point((0.0, -2.0e38, 0.0))");
        let e = ShotSequence::from_ron(&point).expect_err("look-at too far");
        assert!(e.to_string().contains("farther than"), "{e}");
        // At the bound, every sample of a Catmull-Rom path is finite.
        let edge = text.replace("2.0e38", &format!("{MAX_PATH_COORDINATE:e}"));
        let seq = ShotSequence::from_ron(&edge).expect("at the bound");
        for i in 0..=8 {
            let s = seq.sample(f64::from(i) / 8.0).expect("a sample");
            assert!(
                s.eye.is_finite() && s.rotation.is_finite(),
                "t {i}/8: {:?}",
                s.eye
            );
        }
    }

    #[test]
    fn a_valid_sequence_round_trips_byte_identically() {
        let mut seq = sequence(vec![shot("a", 2.0), shot("b", 3.0)]);
        seq.name = "demo".into();
        seq.shots[0].fade_in = 0.5;
        seq.shots[0].camera.fov = Track(vec![
            ScalarKey {
                time: 0.0,
                value: 30.0,
                ease: Ease::Smooth,
            },
            ScalarKey {
                time: 2.0,
                value: 50.0,
                ease: Ease::Linear,
            },
        ]);
        seq.shots[1].camera.dof = Some(DepthOfFieldTracks {
            focus_on_target: true,
            bokeh: true,
            ..DepthOfFieldTracks::default()
        });
        seq.shots[1].camera.path.look_at = LookAt::Track(vec![key(0.0, 1.0), key(3.0, 2.0)]);
        seq.validate().expect("valid");
        let text = seq.to_ron().expect("serializes");
        let back = ShotSequence::from_ron(&text).expect("parses");
        assert_eq!(back, seq);
        assert_eq!(back.to_ron().expect("serializes again"), text, "canonical text");
    }

    #[test]
    fn defaults_fill_in_for_a_minimal_hand_written_file() {
        let seq = ShotSequence::from_ron(
            r#"(version: 1, shots: [(name: "s", duration: 1.0,
                camera: (path: (keys: [(time: 0.0, position: (1.0, 2.0, 3.0))])))])"#,
        )
        .expect("minimal file");
        assert_eq!(seq.fps, 30.0);
        let shot = &seq.shots[0];
        assert_eq!(shot.camera.path.interp, PathInterp::CatmullRom);
        assert_eq!(shot.camera.path.look_at, LookAt::Heading);
        assert!(shot.camera.fov.0.is_empty() && shot.camera.dof.is_none());
        assert_eq!(shot.fade_color, [0.0; 3]);
    }

    #[test]
    fn a_v1_file_migrates_to_v2_and_a_rotation_track_round_trips() {
        let v1 = r#"(version: 1, shots: [(name: "s", duration: 2.0,
            camera: (path: (keys: [(time: 0.0, position: (0.0, 0.0, 0.0))])))])"#;
        let mut seq = ShotSequence::from_ron(v1).expect("v1 loads");
        assert_eq!(seq.version, SHOT_SEQUENCE_FORMAT_VERSION);
        assert!(seq.shots[0].camera.rotation.is_empty());
        let saved = seq.to_ron().expect("serializes");
        assert!(
            saved.contains(&format!("version: {SHOT_SEQUENCE_FORMAT_VERSION}"))
                && !saved.contains("rotation"),
            "{saved}"
        );

        seq.shots[0].camera.rotation = vec![
            RotationKey {
                time: 0.0,
                rotation: [0.0, 0.0, 0.0, 1.0],
                ease: Ease::Linear,
            },
            RotationKey {
                time: 2.0,
                rotation: [
                    0.0,
                    std::f32::consts::FRAC_1_SQRT_2,
                    0.0,
                    std::f32::consts::FRAC_1_SQRT_2,
                ],
                ease: Ease::Smooth,
            },
        ];
        let text = seq.to_ron().expect("serializes");
        let back = ShotSequence::from_ron(&text).expect("re-parses");
        assert_eq!(back, seq);
        assert_eq!(
            back.to_ron().expect("serializes"),
            text,
            "re-saving is byte-identical"
        );
    }

    #[test]
    fn rotation_validation_names_the_shot_and_the_problem() {
        let bad = |keys: Vec<RotationKey>| {
            let mut seq = sequence(vec![shot("a", 2.0)]);
            seq.shots[0].camera.rotation = keys;
            seq.validate().expect_err("invalid").to_string()
        };
        let key = |time: f32, rotation: [f32; 4]| RotationKey {
            time,
            rotation,
            ease: Ease::Linear,
        };
        let unit = [0.0, 0.0, 0.0, 1.0];
        assert!(bad(vec![key(0.0, unit), key(0.0, unit)]).contains("strictly increase"));
        assert!(bad(vec![key(3.0, unit)]).contains("outside"));
        assert!(bad(vec![key(0.0, [0.0; 4])]).contains("quaternion"));
        assert!(bad(vec![key(0.0, [f32::NAN, 0.0, 0.0, 1.0])]).contains("quaternion"));
        assert!(bad(vec![key(0.0, [0.0; 4])]).contains("shot `a`"));
    }

    #[test]
    fn a_newer_version_is_rejected_before_the_body_is_read() {
        let err = ShotSequence::from_ron("(version: 99, something_new: true)").expect_err("too new");
        assert!(
            matches!(err, ShotSequenceError::Version { found: 99, .. }),
            "{err}"
        );
    }

    #[test]
    fn the_published_example_parses_and_validates() {
        let text = include_str!("../../../../docs/formats/shots.example.ron");
        let seq = ShotSequence::from_ron(text).expect("docs/formats/shots.example.ron parses");
        assert_eq!(seq.shots.len(), 3);
        assert!(seq.shots[1].camera.dof.is_some());
        let again = ShotSequence::from_ron(&seq.to_ron().expect("serializes")).expect("re-parses");
        assert_eq!(again, seq);
    }

    #[test]
    fn validation_names_the_shot_and_the_problem() {
        let bad = |edit: fn(&mut ShotSequence)| {
            let mut seq = sequence(vec![shot("a", 2.0), shot("b", 2.0)]);
            edit(&mut seq);
            seq.validate().expect_err("invalid").to_string()
        };
        assert!(bad(|s| s.shots.clear()).contains("at least one shot"));
        assert!(bad(|s| s.shots[1].name = "a".into()).contains("duplicate shot name `a`"));
        assert!(bad(|s| s.shots[0].name.clear()).contains("empty name"));
        assert!(bad(|s| s.shots[1].duration = 0.0).contains("shot `b`: duration"));
        assert!(bad(|s| s.shots[0].duration = f32::NAN).contains("duration"));
        assert!(bad(|s| s.shots[0].camera.path.keys.clear()).contains("at least one key"));
        assert!(bad(|s| s.shots[0].camera.path.keys[1].time = 0.0).contains("strictly increase"));
        // A negative key time is refused; a key past the shot's end validates (ADR-8640: a
        // shortened Catmull-Rom path keeps its tail keys as control keys).
        assert!(bad(|s| s.shots[0].camera.path.keys[0].time = -1.0).contains("negative"));
        let mut tail = sequence(vec![shot("a", 2.0), shot("b", 2.0)]);
        tail.shots[0].camera.path.keys[1].time = 9.0;
        tail.validate()
            .expect("a key past the shot's end is a control key");
        assert!(bad(|s| s.shots[0].camera.path.keys[0].position[1] = f32::INFINITY).contains("non-finite"));
        assert!(
            bad(|s| {
                s.shots[0].fade_in = 1.5;
                s.shots[0].fade_out = 1.0;
            })
            .contains("longer than the shot")
        );
        assert!(bad(|s| s.shots[0].fade_color = [2.0, 0.0, 0.0]).contains("fade_color"));
        assert!(
            bad(|s| {
                s.shots[0].camera.fov = Track(vec![ScalarKey {
                    time: 0.0,
                    value: 200.0,
                    ease: Ease::Linear,
                }]);
            })
            .contains("fov")
        );
        assert!(bad(|s| s.fps = 0.0).contains("fps"));
    }

    #[test]
    fn the_fingerprint_lists_every_struct_and_enum() {
        let fp = shot_sequence_schema_fingerprint();
        for needle in [
            "fade_in",
            "look_at",
            "focus_on_target",
            "Linear,Step,Smooth",
            "Heading",
        ] {
            assert!(fp.contains(needle), "{needle} missing from {fp}");
        }
        assert!(fp.contains("rotation"), "rotation missing from {fp}");
        assert_eq!(
            shot_sequence_migration_sources(),
            [1, 2],
            "v2 migrates from v1, v3 from v2"
        );
    }

    /// w11-seq-preview-fixes item 4 (ADR-8640, the v0.6 format bump): a v2 file (with or without
    /// tail keys) loads as v3, the upgrade is idempotent, a v3 file with a Catmull-Rom path key
    /// and a look-at key past the shot's end loads, and an older build's refusal names the version.
    #[test]
    fn a_v2_file_upgrades_to_v3_and_a_v3_file_with_tail_keys_loads() {
        let v2 = r#"(version: 2, shots: [(name: "s", duration: 2.0,
            camera: (path: (keys: [(time: 0.0, position: (0.0, 0.0, 0.0)), (time: 2.0, position: (1.0, 0.0, 0.0))])))])"#;
        let seq = ShotSequence::from_ron(v2).expect("v2 loads");
        assert_eq!(seq.version, 3, "a v2 file migrates to v3");
        let saved = seq.to_ron().expect("serializes");
        assert!(saved.contains("version: 3"), "{saved}");
        let again = ShotSequence::from_ron(&saved).expect("v3 re-parses");
        assert_eq!(again, seq);
        assert_eq!(again.to_ron().expect("serializes"), saved, "idempotent");
        // A v2 file a v0.6 dev build wrote after ADR-8640 (a tail key at 3 s in a 2 s shot).
        let v2_tail = v2.replace("(time: 2.0, position: (1.0", "(time: 3.0, position: (1.0");
        let seq = ShotSequence::from_ron(&v2_tail).expect("a v2 file with tail keys loads");
        assert_eq!(seq.version, 3);
        // The corpus v3 file: a Catmull-Rom path key and a look-at key past the shot's end.
        let v3 = include_str!("../../../regia_agent/tests/fixtures/format-corpus/shots/v3.shots.ron");
        let seq = ShotSequence::from_ron(v3).expect("v3 with tail keys loads");
        let shot = &seq.shots[0];
        assert_eq!(shot.camera.path.interp, PathInterp::CatmullRom);
        assert!(
            shot.camera.path.keys.iter().any(|k| k.time > shot.duration),
            "the fixture has a path key past the end"
        );
        // An older build (supported version 2) refuses the file by its version, not by a key time.
        let refusal = ShotSequenceError::Version {
            found: 3,
            expected: 2,
        }
        .to_string();
        assert_eq!(
            refusal,
            "shot sequence version 3 is newer than supported version 2; upgrade the engine"
        );
    }
}
