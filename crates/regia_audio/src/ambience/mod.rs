// SPDX-License-Identifier: MPL-2.0
//! Ambience that matches the picture (ADR-2442, `book/agents/audio-ambience.md`): a looping bed
//! whose wind, gust, rustle, rain and drip layers follow the scene's wind and weather
//! ([`AmbienceBed`]), point ambiences such as water and birds heard with distance gain, a
//! distance low-pass and occlusion ([`AmbientSpot`]), and surface footsteps with randomized
//! variants that never repeat back to back, scaled by speed and weight ([`FootstepBank`]).
//!
//! Everything is opt-in: nothing runs until a host (or `audio.set_ambience`,
//! `audio.add_ambient_spot`, `audio.set_footsteps`) inserts an [`AmbienceBed`], spawns an
//! [`AmbientSpot`] or inserts a [`FootstepBank`].
//!
//! # Provenance
//!
//! Level-follows-parameter beds and randomized containers are standard game-audio practice
//! (Collins, "Game Sound", MIT Press 2008, ch. 8; Stevens and Raybould, "The Game Audio
//! Tutorial", Focal Press 2011, ch. 2 "ambience" and "footsteps"). The rustle layer follows the
//! leaf-flutter factor of Regia Engine's own sway stage (`regia_render::foliage_wind`). Distance
//! gain, the distance low-pass and occlusion are the existing spec 16 formulas. Uniform disc
//! sampling by `r = R√u` is textbook (Pharr, Jakob and Humphreys, "Physically Based Rendering",
//! 4th ed., § A.5.1). No code from any source.

mod bed;
mod file;
mod footsteps;
mod spots;
mod system;

pub use bed::{
    AmbienceBed, AmbienceInputs, BedDrive, BedLayer, BedLayerState, TEST_TONE_PREFIX, foliage_flutter,
    test_tone_loopgens,
};
pub use file::{
    AMBIENCE_VERSION, AmbienceCall, AmbienceDef, AmbienceError, AmbienceOneShot, AmbiencePlayer,
    MAX_AMBIENCE_DISTANCE_M, MAX_AMBIENCE_INTERVAL_SECS, MIN_AMBIENCE_INTERVAL_SECS,
    ambience_migration_sources, ambience_schema_fingerprint,
};
pub use footsteps::{FOOTSTEP_LOG_LEN, FootstepBank, FootstepEvent, FootstepPlay, FootstepSet};
pub use spots::{
    AmbientCall, AmbientSpot, AmbientSpotKind, SpotHearing, SpotSchedule, call_position, next_interval,
    pick_variant, spot_hearing,
};
pub(crate) use system::ambience_bed_cleanup;
pub use system::{
    AmbienceSpotState, ambience_bed_sync, ambience_file_sync, ambient_spot_sync, stop_bed_voices,
};
