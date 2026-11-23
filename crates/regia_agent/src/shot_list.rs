// SPDX-License-Identifier: MPL-2.0
//! The shot-list file (`.shot.ron`): the named shots `seeing.capture_plate` renders (spec 27 § 5,
//! ADR-2463), versioned and validated (`docs/formats/shot-ron.md`).
//!
//! A shot is a camera, a time of day, a render profile, a size, a warm-up and an AOV list; a list
//! holds the twelve (or any number of) shots of a look review. This module is only the data and
//! its checks; nothing here touches a world, a file or a clock.
//!
//! ```
//! use regia_agent::shot_list::ShotList;
//!
//! let list = ShotList::from_ron(r#"(
//!     format_version: 1,
//!     name: "review",
//!     shots: [
//!         (name: "wide_noon", camera: Some(Pose(eye: (0.0, 2.0, 8.0), target: (0.0, 1.0, 0.0))),
//!          time_of_day: Some((hour: 12.0)), width: Some(1280), height: Some(720)),
//!     ],
//! )"#).expect("valid list");
//! assert_eq!(list.shots[0].name, "wide_noon");
//! ```

use regia_core::version::{self, VersionStep};
use serde::{Deserialize, Serialize};

/// Current `.shot.ron` version.
pub const SHOT_LIST_FORMAT_VERSION: u32 = 1;

/// Most shots one list may hold.
pub const MAX_SHOTS: usize = 256;

/// Most AOVs one shot may name.
pub const MAX_SHOT_AOVS: usize = 3;

/// Largest side of a plate, pixels.
pub const MAX_PLATE_SIDE: u32 = 8192;

/// Most warm-up frames a shot may ask for.
pub const MAX_WARMUP_FRAMES: u32 = 600;

/// Shot-list errors.
#[derive(Debug, thiserror::Error)]
pub enum ShotListError {
    /// The file is a newer version than this build reads.
    #[error("shot list version {found} is newer than supported version {expected}; upgrade the engine")]
    Version {
        /// Version in the file.
        found: u32,
        /// Version this build reads.
        expected: u32,
    },
    /// RON syntax or shape error.
    #[error("shot list parse error: {0}")]
    Parse(String),
    /// Semantic error (no shots, a duplicate or bad name, a size out of range).
    #[error("shot list invalid: {0}")]
    Invalid(String),
    /// A migration step failed.
    #[error("shot list migration failed: {0}")]
    Migration(String),
}

/// The camera of a shot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ShotCamera {
    /// A pose: the camera at `eye` looking at `target`.
    Pose {
        /// Camera position, world space.
        eye: (f32, f32, f32),
        /// Point looked at, world space.
        target: (f32, f32, f32),
        /// Vertical field of view in degrees; the camera's own when absent.
        #[serde(default)]
        fov_y_deg: Option<f32>,
    },
    /// The camera entity with this `Name`, left where it is.
    Named(String),
}

/// The time of day of a shot (local solar time, as `regia_render::TimeOfDay`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShotTime {
    /// Hours since local solar midnight, `0..24`.
    pub hour: f32,
    /// Day of year, `1..=365`; the scene's own when absent.
    #[serde(default)]
    pub day_of_year: Option<u16>,
    /// Latitude in degrees, `-89..=89`; the scene's own when absent.
    #[serde(default)]
    pub latitude_deg: Option<f32>,
}

/// One shot of a list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Shot {
    /// Name; becomes the file stem of the plate (`a-z`, `0-9`, `_`, `-`, up to 64).
    pub name: String,
    /// The level the shot is taken in (project-relative `.level.ron`). Recorded in the sidecar;
    /// the host loads levels, so this must already be the running level.
    #[serde(default)]
    pub level: Option<String>,
    /// The camera; the scene's camera as it stands when absent.
    #[serde(default)]
    pub camera: Option<ShotCamera>,
    /// The time of day; left as it stands when absent.
    #[serde(default)]
    pub time_of_day: Option<ShotTime>,
    /// The render profile preset (`render.set_profile` names); left as it stands when absent.
    #[serde(default)]
    pub profile: Option<String>,
    /// Plate width in pixels (default 1280).
    #[serde(default)]
    pub width: Option<u32>,
    /// Plate height in pixels (default 720).
    #[serde(default)]
    pub height: Option<u32>,
    /// Frames stepped before the plate (default 8): lets streaming and history settle.
    #[serde(default)]
    pub warmup_frames: Option<u32>,
    /// AOV files written next to the plate: `depth`, `id`, `motion`.
    #[serde(default)]
    pub aovs: Vec<String>,
    /// `png` (default) or `exr` (scene-linear).
    #[serde(default)]
    pub format: Option<String>,
}

/// The `.shot.ron` document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShotList {
    /// Format version ([`SHOT_LIST_FORMAT_VERSION`]).
    pub format_version: u32,
    /// List name (informational).
    #[serde(default)]
    pub name: String,
    /// The shots, in review order.
    pub shots: Vec<Shot>,
}

/// True for a name usable as a plate file stem.
#[must_use]
pub fn valid_shot_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
}

impl Shot {
    /// Checks one shot.
    ///
    /// # Errors
    /// [`ShotListError::Invalid`] naming the problem.
    pub fn validate(&self) -> Result<(), ShotListError> {
        let bad = |m: String| Err(ShotListError::Invalid(format!("shot `{}`: {m}", self.name)));
        if !valid_shot_name(&self.name) {
            return bad("the name must be 1..=64 of a-z, 0-9, `_`, `-` and not start with `-`".into());
        }
        for (key, value) in [("width", self.width), ("height", self.height)] {
            if value.is_some_and(|v| !(1..=MAX_PLATE_SIDE).contains(&v)) {
                return bad(format!("`{key}` must be in 1..={MAX_PLATE_SIDE}"));
            }
        }
        if self.warmup_frames.is_some_and(|w| w > MAX_WARMUP_FRAMES) {
            return bad(format!("`warmup_frames` must be at most {MAX_WARMUP_FRAMES}"));
        }
        if self.aovs.len() > MAX_SHOT_AOVS {
            return bad(format!("at most {MAX_SHOT_AOVS} aovs"));
        }
        if let Some(a) = self
            .aovs
            .iter()
            .find(|a| !matches!(a.as_str(), "depth" | "id" | "motion"))
        {
            return bad(format!("unknown aov `{a}` (depth, id, motion)"));
        }
        if let Some(f) = self.format.as_deref().filter(|f| !matches!(*f, "png" | "exr")) {
            return bad(format!("unknown format `{f}` (png, exr)"));
        }
        if let Some(t) = &self.time_of_day {
            if !(t.hour.is_finite() && (0.0..24.0).contains(&t.hour)) {
                return bad("`time_of_day.hour` must be in 0..24".into());
            }
            if t.day_of_year.is_some_and(|d| !(1..=365).contains(&d)) {
                return bad("`time_of_day.day_of_year` must be in 1..=365".into());
            }
            if t.latitude_deg
                .is_some_and(|l| !(l.is_finite() && (-89.0..=89.0).contains(&l)))
            {
                return bad("`time_of_day.latitude_deg` must be in -89..=89".into());
            }
        }
        match &self.camera {
            Some(ShotCamera::Pose {
                eye,
                target,
                fov_y_deg,
            }) => {
                let finite = [eye.0, eye.1, eye.2, target.0, target.1, target.2]
                    .iter()
                    .all(|v| v.is_finite());
                // The view direction must normalize (ADR-5597): eyes 1e-30 apart, or a difference
                // that overflows f32, gave a zero basis and a non-unit camera rotation.
                let along = [target.0 - eye.0, target.1 - eye.1, target.2 - eye.2];
                let length_sq: f32 = along.iter().map(|d| d * d).sum();
                if !finite || !length_sq.is_finite() || length_sq < 1e-12 {
                    return bad("a camera pose needs finite, distinct `eye` and `target`".into());
                }
                if fov_y_deg.is_some_and(|f| !(f.is_finite() && f > 1.0 && f < 170.0)) {
                    return bad("`fov_y_deg` must be in (1, 170)".into());
                }
            }
            Some(ShotCamera::Named(name)) if name.is_empty() => {
                return bad("a named camera needs a name".into());
            }
            _ => {}
        }
        Ok(())
    }
}

impl ShotList {
    /// Parses a `.shot.ron`: version check, migration, validation.
    ///
    /// # Errors
    /// [`ShotListError`] for a newer version, bad RON or an invalid list.
    pub fn from_ron(text: &str) -> Result<Self, ShotListError> {
        let found = version::peek_ron_version(text).map_err(ShotListError::Parse)?;
        if found > SHOT_LIST_FORMAT_VERSION {
            return Err(ShotListError::Version {
                found,
                expected: SHOT_LIST_FORMAT_VERSION,
            });
        }
        let mut list: Self = ron::from_str(text).map_err(|e| ShotListError::Parse(e.to_string()))?;
        list.migrate_to_latest()?;
        list.validate()?;
        Ok(list)
    }

    /// Serializes to RON with a trailing newline.
    ///
    /// # Errors
    /// [`ShotListError::Parse`] if serialization fails.
    pub fn to_ron(&self) -> Result<String, ShotListError> {
        let mut text = ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| ShotListError::Parse(e.to_string()))?;
        text.push('\n');
        Ok(text)
    }

    /// Runs the migration chain on an older version (`docs/specs/12-assets.md` § 4a). There is no
    /// older version yet.
    ///
    /// # Errors
    /// A newer version, or a failing step.
    pub fn migrate_to_latest(&mut self) -> Result<u32, ShotListError> {
        if self.format_version > SHOT_LIST_FORMAT_VERSION {
            return Err(ShotListError::Version {
                found: self.format_version,
                expected: SHOT_LIST_FORMAT_VERSION,
            });
        }
        if self.format_version < SHOT_LIST_FORMAT_VERSION {
            self.format_version = version::migrate(
                self,
                self.format_version,
                shot_list_migrations(),
                SHOT_LIST_FORMAT_VERSION,
            )
            .map_err(|e| ShotListError::Migration(e.to_string()))?;
        }
        Ok(self.format_version)
    }

    /// Checks the list: at least one shot, at most [`MAX_SHOTS`], unique valid names.
    ///
    /// # Errors
    /// [`ShotListError::Invalid`] naming the problem.
    pub fn validate(&self) -> Result<(), ShotListError> {
        if self.shots.is_empty() || self.shots.len() > MAX_SHOTS {
            return Err(ShotListError::Invalid(format!(
                "a list holds 1..={MAX_SHOTS} shots, this has {}",
                self.shots.len()
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for shot in &self.shots {
            shot.validate()?;
            if !seen.insert(shot.name.as_str()) {
                return Err(ShotListError::Invalid(format!(
                    "duplicate shot name `{}`",
                    shot.name
                )));
            }
        }
        Ok(())
    }

    /// The shot named `name`.
    #[must_use]
    pub fn shot(&self, name: &str) -> Option<&Shot> {
        self.shots.iter().find(|s| s.name == name)
    }
}

/// The `.shot.ron` migration chain: empty at v1.
fn shot_list_migrations() -> &'static [VersionStep<ShotList>] {
    &[]
}

/// The `from` versions of the `.shot.ron` migration steps (for `cargo xtask check-formats`).
#[must_use]
pub fn shot_list_migration_sources() -> Vec<u32> {
    shot_list_migrations().iter().map(|s| s.from).collect()
}

/// The serde shape fingerprint of `.shot.ron` (hashed by `cargo xtask check-formats`): the field
/// list of every struct in the tree, then the variant spellings of every enum.
#[must_use]
pub fn shot_list_schema_fingerprint() -> String {
    use regia_core::schema::{serde_field_names, serde_variant_names};
    regia_core::schema::join_field_lists(&[
        serde_field_names::<ShotList>(),
        serde_field_names::<Shot>(),
        serde_field_names::<ShotTime>(),
        serde_variant_names::<ShotCamera>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"(
        format_version: 1,
        shots: [
            (name: "a", camera: Some(Pose(eye: (0.0, 1.0, 5.0), target: (0.0, 1.0, 0.0), fov_y_deg: Some(50.0))),
             time_of_day: Some((hour: 7.5, day_of_year: Some(172))), aovs: ["depth"], format: Some("exr")),
            (name: "b", camera: Some(Named("cam_b")), profile: Some("desktop")),
        ],
    )"#;

    #[test]
    fn a_good_list_round_trips() {
        let list = ShotList::from_ron(GOOD).expect("parses");
        assert_eq!(list.shots.len(), 2);
        assert_eq!(
            list.shot("b").map(|s| s.camera.clone()),
            Some(Some(ShotCamera::Named("cam_b".into())))
        );
        let again = ShotList::from_ron(&list.to_ron().expect("ron")).expect("re-parses");
        assert_eq!(list, again);
    }

    #[test]
    fn bad_lists_are_refused() {
        let cases = [
            "(format_version: 2, shots: [(name: \"a\")])",
            "(format_version: 1, shots: [])",
            "(format_version: 1, shots: [(name: \"A\")])",
            "(format_version: 1, shots: [(name: \"a\"), (name: \"a\")])",
            "(format_version: 1, shots: [(name: \"a\", width: Some(0))])",
            "(format_version: 1, shots: [(name: \"a\", aovs: [\"normals\"])])",
            "(format_version: 1, shots: [(name: \"a\", format: Some(\"tga\"))])",
            "(format_version: 1, shots: [(name: \"a\", time_of_day: Some((hour: 24.0)))])",
            "(format_version: 1, shots: [(name: \"a\", camera: Some(Pose(eye: (1.0, 1.0, 1.0), target: (1.0, 1.0, 1.0))))])",
            "(shots: [(name: \"a\")])",
        ];
        for case in cases {
            assert!(ShotList::from_ron(case).is_err(), "{case} should be refused");
        }
    }

    #[test]
    fn the_fingerprint_names_the_shapes_and_v1_has_no_migrator() {
        let print = shot_list_schema_fingerprint();
        for needle in ["format_version", "warmup_frames", "latitude_deg", "Pose", "Named"] {
            assert!(print.contains(needle), "{needle} missing from {print}");
        }
        assert!(shot_list_migration_sources().is_empty());
    }
}
