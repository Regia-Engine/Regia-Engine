// SPDX-License-Identifier: MPL-2.0
//! Dense forests a camera can fly through (ADR-1503, `book/agents/forest-flythrough.md`).
//!
//! The scatter field already streams, culls, LOD-selects and fades hundreds of thousands of
//! instances (`super::field`). This module adds what a fast flight through trunks and crowns
//! needs on top, each part opt-in and deterministic:
//!
//! - [`shape`]: a trunk capsule and a crown spheroid per scatter mesh, measured from its geometry;
//! - [`query`]: scatter instances in a box or along a segment, straight from placement (nothing
//!   has to be resident);
//! - [`corridor`]: per-segment clearance of a camera path to trunks, crowns and the ground, canopy
//!   cover overhead, and a carve that opens the corridor with `.scatter.ron` exclusions;
//! - [`gaps`]: canopy gap density maps (power-law gap sizes);
//! - [`motion`]: speed-aware LOD (lookahead along the eye's travel, a full-detail radius) and a
//!   check of a set's fade and streaming ranges against a flight speed;
//! - [`wake`]: the camera's air wake as gusts, and a damped sway of the trees it passes, applied
//!   to the emitted draws (motion vectors follow, the renderer keys previous models by object);
//! - [`pops`]: a pop meter over the emitted draws (hard LOD switches, pop-ins and vanishes on
//!   screen);
//! - [`stress`]: a synthetic dense forest and a scripted flythrough with per-frame counters;
//! - [`flight`]: the [`flight::ForestFlight`] resource the agent methods `terrain.forest_*` read.

pub mod corridor;
pub mod flight;
pub mod gaps;
pub mod motion;
pub mod pops;
pub mod query;
pub mod shape;
pub mod stress;
pub mod wake;

pub use corridor::{
    CameraPath, ClassCarve, CorridorCarve, CorridorReport, CorridorSpec, NearestObstacle, SegmentClearance,
    carve_corridor, corridor_report,
};
pub use flight::{ForestFlight, ForestFlightPlugin, ForestFlightStats, forest_flight_system};
pub use gaps::{CanopyGaps, Gap, GapMap, canopy_gap_map};
pub use motion::{
    BandChange, ClassTiming, FlightCheck, FlightPlan, MotionLod, flight_check, tune_fade_bands,
};
pub use pops::{PopEvent, PopKind, PopMeter, PopStats, SwitchRecord};
pub use query::{ScatterHit, ScatterQuery, SegmentHit};
pub use shape::{ObstaclePart, ObstacleShape, WorldObstacle};
pub use stress::{
    FlythroughFrame, FlythroughReport, FlythroughSpec, ForestSpec, PathSampler, StandinTree, SyntheticForest,
    TreeKind, run_flythrough, standin_tree, synthetic_forest,
};
pub use wake::{GustField, StillAir, SwayField, SwaySettings, SwayStats, TrackWake, WakeModel, WakeSample};

use regia_core::glam::Vec3;

/// The eye's step for motion LOD: `eye − last` while motion LOD is on and the step is not
/// longer than its `max_step_m` (a camera cut); `None` otherwise.
pub(crate) fn motion_step(motion: Option<MotionLod>, last: Option<[f32; 3]>, eye: [f32; 3]) -> Option<Vec3> {
    let motion = motion?;
    let step = Vec3::from_array(eye) - Vec3::from_array(last?);
    (step.length() <= motion.max_step_m).then_some(step)
}
