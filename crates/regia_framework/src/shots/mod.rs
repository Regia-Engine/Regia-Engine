// SPDX-License-Identifier: MPL-2.0
//! Camera paths and shot sequences: keyed camera moves, cuts, fades, field of view and depth of
//! field, played back deterministically from a time value (book `agents/camera-paths.md`).
//!
//! - [`asset`]: the `.shots.ron` format ([`ShotSequence`]), versioned and validated
//!   (`docs/formats/shots-ron.md`).
//! - [`sample`]: [`ShotSequence::sample`], the pure function from a time to a [`ShotSample`]:
//!   eye, target, orientation, lens, depth of field and fade. No clocks, no state.
//! - [`spline`]: the centripetal Catmull-Rom curve behind [`PathInterp::CatmullRom`].
//! - [`player`]: [`ShotPlayer`] and [`ShotPlugin`], which move a camera entity from the sample
//!   each frame on the engine's gameplay time.
//!
//! The data model is plain keyed values (seconds, metres, degrees, f-stops), so a hand-written
//! file, an agent (`camera.set_path`, `render.sequence`) and a future editor panel all edit the
//! same thing. There is no curve editor here.

pub mod asset;
#[cfg(feature = "client")]
pub mod player;
pub mod sample;
pub mod spline;

pub use asset::{
    CameraPath, DEFAULT_APERTURE, DEFAULT_FOCUS_DISTANCE, DEFAULT_FOV_DEGREES, DepthOfFieldTracks, Ease,
    LookAt, MAX_PATH_COORDINATE, PathInterp, PathKey, RotationKey, SHOT_SEQUENCE_FORMAT_VERSION, ScalarKey,
    Shot, ShotCamera, ShotSequence, ShotSequenceError, Track, shot_sequence_migration_sources,
    shot_sequence_schema_fingerprint,
};
#[cfg(feature = "client")]
pub use player::{ShotDriveSet, ShotFadeLayer, ShotPlayer, ShotPlugin, fade_state};
pub use sample::{DofSample, ShotSample, look_rotation, look_rotation_along, sample_rotation, time_of_frame};
