// SPDX-License-Identifier: MPL-2.0
//! Capsule-sized stair walkability (ADR-8038).
//!
//! [`check_stair`] builds a small navmesh for one stair and asks [`crate::NavQuery::find_path`]
//! whether a capsule can walk from a stand point on the lower landing to a stand point on the
//! upper landing. The capsule is [`crate::NavAgentParams`] (radius, height, max climb, max slope).
//! Voxel size is [`CapsuleWalkSettings`]. A stair the capsule cannot use is flagged.
//!
//! The probe is the stair plus two landings. It is not a whole-building raster. Centre-sampled
//! voxels can miss a tread thinner than `cell_size_m`. A ramp whose chord is steeper than
//! `max_slope` is not a walkable surface. Flat treads connect when the riser is within
//! `max_climb`.

use regia_core::math::{Vec3, det_atan2};
use thiserror::Error;

use crate::config::{NavAgentParams, NavMeshBuildParams};
use crate::query::NavQuery;
use crate::{NavBuildError, NavMeshBuilder};

/// How far a landing extends past the stand point, in cells, so radius erosion leaves the stand.
const LANDING_CELLS: f32 = 3.0;
/// At most this many flat treads. A finer split is refused rather than allocating without bound.
const MAX_TREADS: usize = 64;
/// A stair axis longer than this is refused. The builder also refuses a grid it cannot allocate.
const MAX_AXIS_M: f32 = 64.0;

/// Voxel size and capsule for one stair check. Every field is a setting.
#[derive(Debug, Clone, PartialEq)]
pub struct CapsuleWalkSettings {
    /// The capsule the mesh is built for.
    pub agent: NavAgentParams,
    /// Horizontal voxel size, metres. A tread thinner than this can fall between cell centres.
    pub cell_size_m: f32,
    /// Vertical span-merge gap, metres. Surface heights stay on the triangles.
    pub cell_height_m: f32,
}

impl Default for CapsuleWalkSettings {
    fn default() -> Self {
        Self {
            agent: NavAgentParams::HUMANOID,
            cell_size_m: 0.2,
            cell_height_m: 0.1,
        }
    }
}

impl CapsuleWalkSettings {
    /// Settings with an explicit capsule and the default 0.2 m by 0.1 m voxels.
    #[must_use]
    pub fn with_agent(agent: NavAgentParams) -> Self {
        Self {
            agent,
            ..Self::default()
        }
    }
}

/// Which way the run leaves `(x, z)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StairDirection {
    /// Run increases X.
    PosX,
    /// Run decreases X.
    NegX,
    /// Run increases Z.
    PosZ,
    /// Run decreases Z.
    NegZ,
}

impl StairDirection {
    /// `+x`, `-x`, `+z` or `-z`. Anything else is `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "+x" => Some(Self::PosX),
            "-x" => Some(Self::NegX),
            "+z" => Some(Self::PosZ),
            "-z" => Some(Self::NegZ),
            _ => None,
        }
    }

    /// The same token [`Self::parse`] accepts.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PosX => "+x",
            Self::NegX => "-x",
            Self::PosZ => "+z",
            Self::NegZ => "-z",
        }
    }
}

/// One stair: a run from `(x, y0, z)` along [`StairDirection`], `width_m` across the run.
///
/// `step_rise_m: None` is a single sloped ramp. `Some(h)` is flat treads whose riser is `h`
/// (the last riser is shorter when `rise_m` is not a multiple of `h`).
#[derive(Debug, Clone, PartialEq)]
pub struct StairProbe {
    /// World X of the bottom of the run, metres.
    pub x: f32,
    /// World Z of the bottom of the run, metres.
    pub z: f32,
    /// Which way the run leaves the bottom.
    pub direction: StairDirection,
    /// Width across the run, metres. The landings are at least this wide.
    pub width_m: f32,
    /// World Y of the lower landing, metres.
    pub y0_m: f32,
    /// Height gained over the run, metres.
    pub rise_m: f32,
    /// Horizontal length of the run, metres.
    pub run_m: f32,
    /// Riser of a stepped stair. `None` builds one ramp.
    pub step_rise_m: Option<f32>,
}

/// Why a stair is or is not flagged. The string is the agent `flag` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StairFlag {
    /// The capsule has a path from the lower stand point to the upper one.
    Walkable,
    /// The run is narrower than twice the capsule radius, and no path exists.
    TooNarrow,
    /// The ramp (or the chord, when the steps themselves are climbable) is steeper than
    /// `max_slope`, and no path exists.
    TooSteep,
    /// A stepped riser is taller than `max_climb`, and no path exists.
    RiseExceedsClimb,
    /// The landings do not connect, and none of the reasons above apply.
    NotConnected,
}

impl StairFlag {
    /// `walkable`, `too_narrow`, `too_steep`, `rise_exceeds_climb` or `not_connected`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Walkable => "walkable",
            Self::TooNarrow => "too_narrow",
            Self::TooSteep => "too_steep",
            Self::RiseExceedsClimb => "rise_exceeds_climb",
            Self::NotConnected => "not_connected",
        }
    }
}

/// The stair check's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StairVerdict {
    /// `flag` is not [`StairFlag::Walkable`].
    pub flagged: bool,
    /// Why the stair was flagged, or [`StairFlag::Walkable`].
    pub flag: StairFlag,
    /// A path exists between the two stand points.
    pub connected: bool,
}

/// Why [`check_stair`] refused to answer.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum WalkCheckError {
    /// The stair's numbers are missing, non-finite, non-positive or too large.
    #[error("stair probe is not usable: {0}")]
    BadStair(&'static str),
    /// The capsule or the voxel size is not usable.
    #[error("capsule walk settings are not usable: {0}")]
    BadAgent(&'static str),
    /// The navmesh builder rejected the probe geometry.
    #[error(transparent)]
    Build(#[from] NavBuildError),
}

/// Flags a stair the capsule in `settings` cannot walk.
///
/// A path is [`StairFlag::Walkable`]. Otherwise a run narrower than twice the radius is
/// [`StairFlag::TooNarrow`] (that wins over a steep slope). A ramp steeper than `max_slope` is
/// [`StairFlag::TooSteep`]. A stepped riser above `max_climb` is [`StairFlag::RiseExceedsClimb`].
/// A chord that is still too steep after a climbable riser is [`StairFlag::TooSteep`]. Anything
/// else disconnected is [`StairFlag::NotConnected`].
///
/// # Errors
///
/// [`WalkCheckError::BadStair`] when the probe is non-finite, non-positive or larger than
/// 64 m on an axis, or would need more than 64 treads.
/// [`WalkCheckError::BadAgent`] when the capsule or the voxel size is not usable.
/// [`WalkCheckError::Build`] when the builder finds no geometry or no walkable cell (the landings
/// were eroded away) or refuses the grid.
pub fn check_stair(
    settings: &CapsuleWalkSettings,
    stair: &StairProbe,
) -> Result<StairVerdict, WalkCheckError> {
    validate_settings(settings)?;
    validate_stair(stair)?;
    let (margin, stand, pad_len) = landings(settings);
    let overlap = (settings.cell_size_m * 0.25).min(0.05);
    let pad_half = (stair.width_m * 0.5).max(margin);
    let y0 = stair.y0_m;
    let y1 = y0 + stair.rise_m;
    let run = stair.run_m;

    let mut tris = Vec::new();
    push_flat(&mut tris, stair, y0, -pad_len, overlap, -pad_half, pad_half);
    match stair.step_rise_m {
        None => push_ramp(&mut tris, stair, y0, run, stair.rise_m),
        Some(step) => push_treads(&mut tris, stair, y0, run, stair.rise_m, step),
    }
    push_flat(
        &mut tris,
        stair,
        y1,
        run - overlap,
        run + pad_len,
        -pad_half,
        pad_half,
    );

    let params = NavMeshBuildParams {
        cell_size: settings.cell_size_m,
        cell_height: settings.cell_height_m,
        border_size: 0.0,
        agent: settings.agent.clone(),
    };
    let mesh = NavMeshBuilder::new(params).build(&tris)?;
    let query = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    let extents = Vec3::new(1.0, settings.agent.height, 1.0);
    let start = place(stair, -stand, y0, 0.0);
    let end = place(stair, run + stand, y1, 0.0);
    let connected = query.find_path(start, end, extents).is_some();
    let flag = classify(settings, stair, connected);
    Ok(StairVerdict {
        flagged: flag != StairFlag::Walkable,
        flag,
        connected,
    })
}

fn classify(settings: &CapsuleWalkSettings, stair: &StairProbe, connected: bool) -> StairFlag {
    if connected {
        return StairFlag::Walkable;
    }
    if stair.width_m < settings.agent.radius * 2.0 {
        return StairFlag::TooNarrow;
    }
    let slope = det_atan2(stair.rise_m, stair.run_m);
    let steep = slope > settings.agent.max_slope;
    if stair.step_rise_m.is_none() && steep {
        return StairFlag::TooSteep;
    }
    if let Some(step) = stair.step_rise_m
        && step > settings.agent.max_climb
    {
        return StairFlag::RiseExceedsClimb;
    }
    if steep {
        return StairFlag::TooSteep;
    }
    StairFlag::NotConnected
}

fn validate_settings(settings: &CapsuleWalkSettings) -> Result<(), WalkCheckError> {
    if !(settings.cell_size_m.is_finite() && settings.cell_size_m > 0.0) {
        return Err(WalkCheckError::BadAgent(
            "cell_size_m must be finite and positive",
        ));
    }
    if !(settings.cell_height_m.is_finite() && settings.cell_height_m > 0.0) {
        return Err(WalkCheckError::BadAgent(
            "cell_height_m must be finite and positive",
        ));
    }
    let agent = &settings.agent;
    if !(agent.radius.is_finite() && agent.radius >= 0.0) {
        return Err(WalkCheckError::BadAgent("radius must be finite and non-negative"));
    }
    if !(agent.height.is_finite() && agent.height > 0.0) {
        return Err(WalkCheckError::BadAgent("height must be finite and positive"));
    }
    if !(agent.max_climb.is_finite() && agent.max_climb >= 0.0) {
        return Err(WalkCheckError::BadAgent(
            "max_climb must be finite and non-negative",
        ));
    }
    if !(agent.max_slope.is_finite()
        && agent.max_slope >= 0.0
        && agent.max_slope < std::f32::consts::FRAC_PI_2)
    {
        return Err(WalkCheckError::BadAgent(
            "max_slope must be finite and within [0, pi/2) radians",
        ));
    }
    Ok(())
}

fn validate_stair(stair: &StairProbe) -> Result<(), WalkCheckError> {
    if ![stair.x, stair.z, stair.y0_m].into_iter().all(f32::is_finite) {
        return Err(WalkCheckError::BadStair("x, z and y0_m must be finite"));
    }
    for value in [stair.width_m, stair.rise_m, stair.run_m] {
        if !(value.is_finite() && value > 0.0) {
            return Err(WalkCheckError::BadStair(
                "width_m, rise_m and run_m must be finite and positive",
            ));
        }
        if value > MAX_AXIS_M {
            return Err(WalkCheckError::BadStair(
                "width_m, rise_m and run_m must be at most 64 m",
            ));
        }
    }
    if let Some(step) = stair.step_rise_m {
        if !(step.is_finite() && step > 0.0) {
            return Err(WalkCheckError::BadStair(
                "step_rise_m must be finite and positive",
            ));
        }
        if tread_count(stair.rise_m, step) > MAX_TREADS {
            return Err(WalkCheckError::BadStair(
                "step_rise_m produces more than 64 treads",
            ));
        }
    }
    Ok(())
}

fn landings(settings: &CapsuleWalkSettings) -> (f32, f32, f32) {
    let margin = settings.agent.radius + settings.cell_size_m * LANDING_CELLS;
    let stand = margin + settings.cell_size_m;
    let pad_len = stand + margin;
    (margin, stand, pad_len)
}

fn tread_count(rise: f32, step: f32) -> usize {
    let n = (rise / step).ceil();
    let whole = if n.is_finite() { n as usize } else { usize::MAX };
    whole.max(1)
}

fn place(stair: &StairProbe, along: f32, y: f32, across: f32) -> Vec3 {
    let (x, z) = match stair.direction {
        StairDirection::PosX => (stair.x + along, stair.z + across),
        StairDirection::NegX => (stair.x - along, stair.z + across),
        StairDirection::PosZ => (stair.x + across, stair.z + along),
        StairDirection::NegZ => (stair.x - across, stair.z - along),
    };
    Vec3::new(x, y, z)
}

fn push_flat(tris: &mut Vec<[Vec3; 3]>, stair: &StairProbe, y: f32, x0: f32, x1: f32, z0: f32, z1: f32) {
    let a = place(stair, x0, y, z0);
    let b = place(stair, x1, y, z0);
    let c = place(stair, x1, y, z1);
    let d = place(stair, x0, y, z1);
    tris.push([a, b, c]);
    tris.push([a, c, d]);
}

fn push_ramp(tris: &mut Vec<[Vec3; 3]>, stair: &StairProbe, y0: f32, run: f32, rise: f32) {
    let half = stair.width_m * 0.5;
    let y1 = y0 + rise;
    let a = place(stair, 0.0, y0, -half);
    let b = place(stair, run, y1, -half);
    let c = place(stair, run, y1, half);
    let d = place(stair, 0.0, y0, half);
    tris.push([a, b, c]);
    tris.push([a, c, d]);
}

fn push_treads(tris: &mut Vec<[Vec3; 3]>, stair: &StairProbe, y0: f32, run: f32, rise: f32, step: f32) {
    let half = stair.width_m * 0.5;
    let n = tread_count(rise, step);
    let n_f = n as f32;
    for i in 0..n {
        let x0 = run * (i as f32) / n_f;
        let x1 = run * ((i + 1) as f32) / n_f;
        let y = y0 + (step * (i + 1) as f32).min(rise);
        push_flat(tris, stair, y, x0, x1, -half, half);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(rise: f32, run: f32, width: f32, step: Option<f32>) -> StairProbe {
        StairProbe {
            x: 0.0,
            z: 0.0,
            direction: StairDirection::PosX,
            width_m: width,
            y0_m: 0.0,
            rise_m: rise,
            run_m: run,
            step_rise_m: step,
        }
    }

    #[test]
    fn unwalkable_stair_flagged() {
        let verdict = check_stair(&CapsuleWalkSettings::default(), &probe(2.0, 0.5, 2.0, None))
            .expect("landings stay walkable");
        assert!(verdict.flagged, "{verdict:?}");
        assert_eq!(verdict.flag, StairFlag::TooSteep);
        assert!(!verdict.connected);
        assert_eq!(verdict.flag.as_str(), "too_steep");
    }

    #[test]
    fn a_walkable_stair_is_not_flagged() {
        let verdict =
            check_stair(&CapsuleWalkSettings::default(), &probe(0.4, 2.0, 2.0, None)).expect("gentle ramp");
        assert!(!verdict.flagged, "{verdict:?}");
        assert_eq!(verdict.flag, StairFlag::Walkable);
        assert!(verdict.connected);
    }

    #[test]
    fn a_stair_narrower_than_the_capsule_is_flagged() {
        let verdict =
            check_stair(&CapsuleWalkSettings::default(), &probe(0.4, 2.0, 0.4, None)).expect("narrow run");
        assert!(verdict.flagged, "{verdict:?}");
        assert_eq!(verdict.flag, StairFlag::TooNarrow);
        assert!(!verdict.connected);
    }

    #[test]
    fn a_stepped_riser_above_max_climb_is_flagged() {
        let mut settings = CapsuleWalkSettings::default();
        settings.agent.max_climb = 0.4;
        let verdict = check_stair(&settings, &probe(1.6, 3.2, 2.0, Some(0.8))).expect("steps");
        assert_eq!(verdict.flag, StairFlag::RiseExceedsClimb, "{verdict:?}");
        assert!(verdict.flagged);
        assert!(!verdict.connected);
    }

    #[test]
    fn a_climbable_step_is_not_flagged() {
        let verdict = check_stair(&CapsuleWalkSettings::default(), &probe(0.5, 2.0, 2.0, Some(0.25)))
            .expect("short risers");
        assert_eq!(verdict.flag, StairFlag::Walkable, "{verdict:?}");
        assert!(verdict.connected);
    }

    #[test]
    fn the_same_stair_flags_the_same_way_twice() {
        let settings = CapsuleWalkSettings::default();
        let stair = probe(2.0, 0.5, 2.0, None);
        let a = check_stair(&settings, &stair).expect("first");
        let b = check_stair(&settings, &stair).expect("second");
        assert_eq!(a, b);
    }

    #[test]
    fn a_steep_stair_is_flagged_on_every_axis() {
        for direction in [
            StairDirection::PosX,
            StairDirection::NegX,
            StairDirection::PosZ,
            StairDirection::NegZ,
        ] {
            let mut stair = probe(2.0, 0.5, 2.0, None);
            stair.direction = direction;
            stair.x = 3.0;
            stair.z = -2.0;
            let verdict = check_stair(&CapsuleWalkSettings::default(), &stair)
                .unwrap_or_else(|err| panic!("{direction:?}: {err}"));
            assert_eq!(verdict.flag, StairFlag::TooSteep, "{direction:?} {verdict:?}");
        }
    }

    #[test]
    fn a_non_finite_or_non_positive_stair_is_refused() {
        let settings = CapsuleWalkSettings::default();
        let mut stair = probe(0.4, 2.0, 2.0, None);
        stair.rise_m = f32::NAN;
        assert!(matches!(
            check_stair(&settings, &stair),
            Err(WalkCheckError::BadStair(_))
        ));
        stair.rise_m = 0.0;
        assert!(matches!(
            check_stair(&settings, &stair),
            Err(WalkCheckError::BadStair(_))
        ));
        stair.rise_m = 0.4;
        stair.step_rise_m = Some(-0.1);
        assert!(matches!(
            check_stair(&settings, &stair),
            Err(WalkCheckError::BadStair(_))
        ));
        let bad = CapsuleWalkSettings {
            cell_size_m: 0.0,
            ..CapsuleWalkSettings::default()
        };
        assert!(matches!(
            check_stair(&bad, &probe(0.4, 2.0, 2.0, None)),
            Err(WalkCheckError::BadAgent(_))
        ));
    }

    #[test]
    fn direction_tokens_round_trip() {
        for token in ["+x", "-x", "+z", "-z"] {
            let parsed = StairDirection::parse(token).expect(token);
            assert_eq!(parsed.as_str(), token);
        }
        assert!(StairDirection::parse("up").is_none());
    }
}
