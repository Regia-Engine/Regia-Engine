// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (tools/tunnels_v2/tunnel_bake.py, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 62-64 and 827-946, mark: adapt (Stage A: unchanged).
// Edits: Rust module `checks`; tolerances stay 0.05 / 0.5 / 0.05; distance helpers stay in `tunnel_bake`; doc comments drop the source file's product names. Spec 121 § 10 and plan § 4.6 checks are applied here (ADR-8359).

//! Cook consistency checks (spec 121 § 10, plan row W-09, ADR-8359).
//!
//! The cook runs every check below and lists each one, passed or failed, in
//! [`TerrainBakeReport`] (`terrain.bake_report`). A failed check refuses the cook. A missing sweep
//! input is `input_missing`, never an empty pass. An authored [`CheckWaiver`] suppresses one named
//! check on a hashed set of cooked triangles; there is no global switch.
//!
//! The collision, nav and render tolerances (0.05 m, 0.5 m, 0.05 m) are the baker's. The flood fill
//! that backs `not_watertight` is original: a 4-connected fill of a small grid that reaches the
//! border is a leak.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::compat::layout::{Junction, TunnelLayout};
use crate::cook::{self, GroundInput};
use crate::cooked::{CookedTriangle, TriKind, WorldGeomCooked};
use crate::meshlayers::{GroundMode, MeshLayersError};
use crate::pitch::PitchSeamReport;
use crate::sections::is_skylight_kind;
use crate::settings::{GeomSettings, SPAN_CAPACITY};
use regia_detsim::float::{det_clamp_f64, det_max_f64, det_min_f64};

/// Collision must sit within this of the render surface, metres (baker tolerance).
pub const COLLISION_TOL_M: f64 = 0.05;
/// A nav sample must sit within this of the collision floor, metres (baker tolerance).
pub const NAV_TOL_M: f64 = 0.5;
/// Drawn solid must sit within this of the collision wall, metres.
pub const DRAWN_SOLID_TOL_M: f64 = 0.5;
/// A ground seam wider than this fails `seam_gap`, metres (spec 121 § 10).
pub const SEAM_M: f64 = 1e-4;
/// On-foot lane required at every station, metres (plan § 4.6).
pub const ONFOOT_LANE_M: f64 = 2.0;
/// Hull radii that must clear, metres, plus the 0.35 m pilot (plan § 4.6).
pub const HULL_RADII_M: [f64; 7] = [2.5, 2.6, 3.6, 3.9, 4.1, 4.5, 0.35];
/// Minimum ceiling clearance, metres (plan § 4.6, body height).
pub const LID_CLEAR_M: f64 = 10.0;
/// Floor-triangle slope (horizontal over vertical of the normal) above this fails
/// `ramp_facet_slope_over_limit`.
///
/// Measured on the Switchbacks cook: the two steepest floor facets are 1.58, near (174, 93) and
/// (-173, 90), and the next are 1.41. 1.50 sits in that gap, so exactly those two fail. The quad
/// split itself stays the pinned diagonal (ADR-8359).
pub const FACET_SLOPE_LIMIT: f64 = 1.50;
/// Spawn ring the flatness check quotes, metres.
pub const SPAWN_RING_M: f64 = 9.0;
/// Rise on that ring above this fails `spawn_not_flat`, metres.
pub const SPAWN_RISE_LIMIT_M: f64 = 2.41;
/// A floor edge longer than this can be a long cap, metres (known issue K6).
pub const LONG_CAP_M: f64 = 8.0;
/// Midpoint deviation above this, with [`LONG_CAP_M`], flags the cap, metres.
pub const CAP_DEVIATION_M: f64 = 0.02;
/// Sim-ground budget for a map up to [`MAP_SMALL_M`], bytes (spec 121 § 11: 35 MB since ADR-8396,
/// 34 MB before; the one budget the cook measures, `pitch::sim_budget_bytes`, ADR-8436).
pub const GROUND_BUDGET_SMALL: u64 = 35_000_000;
/// Sim-ground budget for a larger map, bytes (spec 121 § 11: 64 MB).
pub const GROUND_BUDGET_LARGE: u64 = 64_000_000;
/// Maps at most this wide use [`GROUND_BUDGET_SMALL`], metres.
pub const MAP_SMALL_M: f64 = 2560.0;
/// `|normal_displace| + |noise_amplitude|` above this is `render_off_collision`.
pub const DISPLACE_LIMIT_M: f32 = 0.05;
/// A covered shaft section whose floor is this far above the ramp start still has a lid.
pub const SHAFT_ABOVE_RAMP_M: f64 = 1.0;
/// Floor centroids closer than this in Y are one floor, metres.
pub const COLUMN_GAP_M: f64 = 4.0;
/// Column bucket, metres.
pub const COLUMN_CELL_M: f64 = 2.0;

/// One cook check (spec 121 § 10). `code` is the agent error code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CheckFinding {
    /// Lowercase ascii, digits and `_`.
    pub code: &'static str,
    /// The check passed.
    pub passed: bool,
    /// An authored waiver covers this failure.
    pub waived: bool,
    /// Where it was measured (a passage, a junction, a triangle centroid).
    pub location: String,
    /// The measured value and the limit, as text (no float in the report).
    pub measured: String,
    /// Cooked triangle indices the check names (layer-local). Empty when it names none.
    pub triangles: Vec<u32>,
    /// BLAKE3 hex of those cooked triangles, empty when `triangles` is empty.
    pub triangle_hash: String,
    /// `ground_over_budget` on a `Full` map: the smallest `Nested { max_level }` that fits the budget,
    /// or `None` when none does (and on every other check). Serialized as the mode name, like
    /// [`TerrainBakeReport::ground_mode`]; left out of the JSON when `None` (ADR-8436).
    #[serde(serialize_with = "serialize_fits", skip_serializing_if = "Option::is_none")]
    pub fits: Option<GroundMode>,
    /// `ground_over_budget` on a `Nested` map: the ground tiles held finer than `max_level` to stay
    /// within `pitch_error_m` (ascending, at most [`GROUND_TILES_LISTED`]). Empty on every other check
    /// and left out of the JSON then (ADR-8436).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tiles: Vec<u32>,
}

/// How many held-fine tiles a `ground_over_budget` finding lists at most.
pub const GROUND_TILES_LISTED: usize = 32;

/// serde `serialize_with` for [`CheckFinding::fits`]: the mode name, as `ground_mode` writes it.
fn serialize_fits<S: serde::Serializer>(fits: &Option<GroundMode>, s: S) -> Result<S::Ok, S::Error> {
    match fits {
        Some(mode) => s.serialize_some(&mode_name(*mode)),
        None => s.serialize_none(),
    }
}

/// `terrain.bake_report`: every check the cook ran (lane W-28 reads this; it does not own the type).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TerrainBakeReport {
    /// The layout's map id, or empty when the cook stopped before a layout.
    pub map: String,
    /// No unwaived failure.
    pub passed: bool,
    /// Every check, passing ones included.
    pub checks: Vec<CheckFinding>,
    /// The authored ground mode (`Full` or `Nested { max_level: N }`). The cook never switches it.
    pub ground_mode: String,
    /// The spec 121 § 11 sim-ground bytes of the authored mode (`pitch::sim_ground_bytes`: counts,
    /// overrides and pins, cut bitmaps, canonical triangles, tile levels; a `Full` lattice's counts
    /// alone are `2 * n * n`), or 0 when the cook stopped before the ground. ADR-8436.
    pub memory_bytes: u64,
    /// Canonical `content_hash`, hex, or empty when the cook stopped early.
    pub content_hash: String,
    /// How many failures an authored waiver covers.
    pub waived: u32,
}

impl TerrainBakeReport {
    /// The first unwaived failure's code, or `bake_checks_failed` when the report failed without one.
    #[must_use]
    pub fn failure_code(&self) -> &'static str {
        self.checks
            .iter()
            .find(|c| !c.passed && !c.waived)
            .map_or("bake_checks_failed", |c| c.code)
    }
}

impl fmt::Display for TerrainBakeReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut any = false;
        for check in &self.checks {
            if check.passed || check.waived {
                continue;
            }
            if any {
                writeln!(f)?;
            }
            any = true;
            write!(f, "{} at {}: {}", check.code, check.location, check.measured)?;
        }
        if !any {
            write!(f, "bake checks passed ({})", self.checks.len())?;
        }
        Ok(())
    }
}

/// An authored waiver in `<map>.meshlayers.ron`. It names a check and the cooked triangles it
/// covers. The hash is of those triangles' cooked bytes, not of a file name (ADR-8359, known issue K1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckWaiver {
    /// The check code.
    pub check: String,
    /// The layer id, informational. Matching ignores it.
    #[serde(default)]
    pub layer: String,
    /// Triangle indices, layer-local.
    pub triangles: Vec<u32>,
    /// BLAKE3 hex (64 lowercase digits) of those triangles in ascending index order.
    pub triangle_hash: String,
}

/// A stated mouth: the layout point and the file point, metres.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatedMouth {
    /// Mouth id.
    pub id: String,
    /// Layout top, metres.
    pub layout: [f32; 3],
    /// File mouth, metres.
    pub file: [f32; 3],
}

/// A stated portal station, metres along the passage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatedPortal {
    /// Portal id.
    pub id: String,
    /// Station the report names.
    pub report_s: f32,
    /// Station the layout names.
    pub layout_s: f32,
}

/// One manifest row or one file the row should match.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatedFile {
    /// Path as written.
    pub path: String,
    /// Size, bytes.
    pub bytes: u64,
}

/// A spawn ring's flatness (known issue: a 9 m ring).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnRing {
    /// Ring radius, metres.
    pub radius_m: f32,
    /// Rise across the ring, metres.
    pub rise_m: f32,
    /// Rise limit, metres.
    pub limit_m: f32,
    /// Height window the report quotes, metres.
    pub height_window_m: f32,
}

/// Drawn solid against the sim lane and the collision wall.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrawnSolid {
    /// Drawn extent, metres.
    pub extent_m: f32,
    /// Sim lane length, metres.
    pub lane_m: f32,
    /// Distance from the solid to the collision wall, metres.
    pub wall_distance_m: f32,
}

/// A ground sample the author says sits in a span.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpanPoint {
    /// Sample height, metres.
    pub y_m: f32,
    /// Floor, metres.
    pub floor_m: f32,
    /// Lid, metres.
    pub lid_m: f32,
}

/// A nav sample against the floor under it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NavSample {
    /// Sample height, metres.
    pub y_m: f32,
    /// Floor height, metres.
    pub floor_y_m: f32,
}

/// An authored floor edge for the seam and long-cap checks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeamEdge {
    /// Edge length, metres.
    pub length_m: f32,
    /// Midpoint deviation from the ground, metres.
    pub deviation_m: f32,
    /// Opening along the edge, metres (the LOD seam).
    pub gap_m: f32,
    /// Both endpoints are on the ground, so a deep tube floor is not a cap.
    pub near_ground: bool,
}

/// A face the author says is hidden under a floor fused into a wall.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HiddenFace {
    /// Distance from the face to the wall, metres.
    pub distance_m: f32,
    /// How far the face sits under the floor, metres.
    pub below_floor_m: f32,
}

/// Two portal endpoints the integer mouth rule compares, world millimetres.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointPair {
    /// One end.
    pub a: [i32; 3],
    /// The other end.
    pub b: [i32; 3],
}

/// Authored probes the cook checks without a second passages file (empty probes add no failure).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BakeAudit {
    /// Stated cooked triangle count.
    pub triangle_file: Option<u64>,
    /// Stated layout triangle count.
    pub triangle_layout: Option<u64>,
    /// Stated mouths.
    pub stated_mouths: Vec<StatedMouth>,
    /// Stated portal stations.
    pub stated_portals: Vec<StatedPortal>,
    /// Manifest rows.
    pub manifest: Vec<StatedFile>,
    /// Files those rows should match.
    pub files: Vec<StatedFile>,
    /// Spawn rings.
    pub spawn_rings: Vec<SpawnRing>,
    /// Drawn solids.
    pub drawn_solids: Vec<DrawnSolid>,
    /// The cook must have a sweep input. Missing, it is `input_missing`.
    pub require_sweep: bool,
    /// Floors the author says one column holds.
    pub column_floors: Option<u32>,
    /// Ground samples claimed to sit inside a span.
    pub span_intrusions: Vec<SpanPoint>,
    /// Authored neighbouring tile levels (a nested description; the cook still refuses `Nested`).
    pub pitch_neighbors: Vec<(u8, u8)>,
    /// The mouth-cut integers disagree.
    pub mouth_cut_mismatch: bool,
    /// Expected `content_hash` hex. Empty skips the compare.
    pub expected_content_hash: String,
    /// Points that must lie in some zone.
    pub zone_samples: Vec<[f32; 3]>,
    /// Nav samples.
    pub nav_samples: Vec<NavSample>,
    /// Floor edges for `seam_gap`.
    pub seam_edges: Vec<SeamEdge>,
    /// Claimed `max_floors` (a header copy that does not match).
    pub claimed_max_floors: Option<u8>,
    /// Claimed lattice `n`.
    pub claimed_lattice_n: Option<u32>,
    /// Run the leak flood on a planted open grid.
    pub leaky_mesh: bool,
    /// A canonical vertex this far above LOD-0 ground outside a cut, metres.
    pub above_ground_m: Option<f32>,
    /// Hidden faces.
    pub hidden_faces: Vec<HiddenFace>,
    /// Integer endpoint pairs.
    pub endpoint_pairs: Vec<EndpointPair>,
    /// A shaft column still has a lid.
    pub shaft_lid: bool,
    /// A portal joins a cell to itself, or joins nothing.
    pub blocked_portal: bool,
}

/// What [`audit`] reads. The cook builds this after the canonical section checks.
pub struct AuditInputs<'a> {
    /// Map id.
    pub map: &'a str,
    /// Normalized settings.
    pub settings: GeomSettings,
    /// Authored ground mode.
    pub ground_mode: GroundMode,
    /// The heightfield.
    pub ground: &'a GroundInput,
    /// The canonical section.
    pub canonical: &'a WorldGeomCooked,
    /// Layouts the passages layers cooked, in layer order.
    pub layouts: &'a [TunnelLayout],
    /// Authored waivers.
    pub waive: &'a [CheckWaiver],
    /// Authored probes.
    pub audit: &'a BakeAudit,
    /// Canonical `content_hash`, hex.
    pub content_hash: &'a str,
    /// What the cook's `pitch::check_ground_pitch_seam` measured on a `Nested` ground (it passed:
    /// a failure is already `CookError::PitchSeam`). `None` for `Full`. ADR-8436.
    pub pitch_seam: Option<PitchSeamReport>,
}

/// Checks waiver rows and audit floats.
///
/// # Errors
///
/// A waiver code, triangle list or hash is wrong, or an audit float is not finite.
pub fn validate_authored(waive: &[CheckWaiver], audit: &BakeAudit) -> Result<(), String> {
    for (i, waiver) in waive.iter().enumerate() {
        if !valid_code(&waiver.check) {
            return Err(format!(
                "waive[{i}].check must be 1-64 lowercase ascii, digits or '_'"
            ));
        }
        if waiver.triangles.is_empty() {
            return Err(format!("waive[{i}] names no triangles"));
        }
        if !valid_hash(&waiver.triangle_hash) {
            return Err(format!(
                "waive[{i}].triangle_hash must be 64 lowercase hex digits"
            ));
        }
    }
    finite_audit(audit)
}

/// Runs every check. The cook turns a report with `passed == false` into [`crate::cook::CookError::Checks`].
#[must_use]
pub fn audit(inputs: &AuditInputs<'_>) -> TerrainBakeReport {
    let tris = inputs.canonical.triangles_sorted();
    let mut checks = Vec::new();
    push_drift_triangles(&mut checks, inputs.audit);
    push_drift_manifest(&mut checks, inputs.audit);
    push_drift_mouths(&mut checks, inputs.audit);
    push_drift_portals(&mut checks, inputs.audit);
    push_spawn(&mut checks, inputs.audit);
    push_drawn(&mut checks, inputs.audit);
    push_columns(&mut checks, inputs, &tris);
    push_seams(&mut checks, inputs.audit);
    push_pitch(&mut checks, inputs);
    push_mouth_cut(&mut checks, inputs);
    push_header(&mut checks, inputs);
    push_lattice(&mut checks, inputs);
    push_determinism(&mut checks, inputs);
    push_watertight(&mut checks, inputs);
    push_above_ground(&mut checks, inputs, &tris);
    push_nav(&mut checks, inputs.audit);
    push_zones(&mut checks, inputs);
    push_hidden(&mut checks, inputs.audit);
    push_junctions(&mut checks, inputs);
    push_shaft_lid(&mut checks, inputs, &tris);
    push_span(&mut checks, inputs);
    push_lanes(&mut checks, inputs.layouts);
    push_facets(&mut checks, &tris);
    push_portals(&mut checks, inputs);
    pass(
        &mut checks,
        "render_off_collision",
        "render",
        format!("displacement within {DISPLACE_LIMIT_M} m"),
    );
    pass(
        &mut checks,
        "ground_over_budget",
        "ground",
        format!(
            "{} bytes within {}",
            crate::pitch::sim_ground_bytes(inputs.canonical),
            crate::pitch::sim_budget_bytes(inputs.canonical.lattice.width_mm)
        ),
    );
    pass(
        &mut checks,
        "ground_vs_render",
        "ground",
        "no authored ground/render disagreement",
    );
    pass(&mut checks, "input_missing", "inputs", "named files were read");
    apply_waivers(&mut checks, inputs.waive);
    finish(inputs, checks)
}

/// Horizontal-over-vertical slope of a floor triangle. Vertical is a large slope.
#[must_use]
pub fn floor_slope(world_mm: [[i32; 3]; 3]) -> f64 {
    let edge = |a: [i32; 3], b: [i32; 3]| {
        [
            f64::from(b[0] - a[0]),
            f64::from(b[1] - a[1]),
            f64::from(b[2] - a[2]),
        ]
    };
    let a = edge(world_mm[0], world_mm[1]);
    let b = edge(world_mm[0], world_mm[2]);
    let n = [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ];
    let horiz = regia_detsim::math::sqrt_f64(n[0] * n[0] + n[2] * n[2]);
    let ny = n[1].abs();
    if ny <= 1e-9 { 1.0e6 } else { horiz / ny }
}

/// BLAKE3 hex of one cooked triangle (index, kind, flags, world millimetres).
#[must_use]
pub fn triangle_hash(index: u32, kind: TriKind, flags: u8, world_mm: [[i32; 3]; 3]) -> String {
    let mut buf = Vec::with_capacity(42);
    append_triangle_bytes(&mut buf, index, kind, flags, world_mm);
    hex_bytes(blake3::hash(&buf).as_bytes())
}

/// True when a 4-connected flood from `seed` through open cells (`solid` is a wall) reaches the
/// border. A sealed interior returns false. A bad grid returns true (a leak, not a pass).
#[must_use]
pub fn flood_reaches_border(width: usize, height: usize, solid: &[bool], seed: (usize, usize)) -> bool {
    if width == 0 || height == 0 || solid.len() != width.saturating_mul(height) {
        return true;
    }
    let (sx, sy) = seed;
    if sx >= width || sy >= height || solid[sy * width + sx] {
        return false;
    }
    let mut seen = vec![false; solid.len()];
    let mut queue = vec![(sx, sy)];
    seen[sy * width + sx] = true;
    let mut head = 0;
    while head < queue.len() {
        let (x, y) = queue[head];
        head += 1;
        if x == 0 || y == 0 || x + 1 == width || y + 1 == height {
            return true;
        }
        for (nx, ny) in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] {
            let i = ny * width + nx;
            if !solid[i] && !seen[i] {
                seen[i] = true;
                queue.push((nx, ny));
            }
        }
    }
    false
}

/// Floor-height step at a junction: max minus min of each member's nearest control point, metres.
/// Fewer than two members is 0.
#[must_use]
pub fn member_floor_step_m(layout: &TunnelLayout, junction: &Junction) -> f64 {
    let mut ys = Vec::new();
    for id in &junction.passages {
        let Some(passage) = layout.passages.iter().find(|p| p.id == *id) else {
            continue;
        };
        if let Some(y) = nearest_control_y(&passage.points, junction.at) {
            ys.push(y);
        }
    }
    let Some(first) = ys.first().copied() else {
        return 0.0;
    };
    let (mut lo, mut hi) = (first, first);
    for y in ys.iter().skip(1).copied() {
        lo = det_min_f64(lo, y);
        hi = det_max_f64(hi, y);
    }
    if ys.len() < 2 { 0.0 } else { hi - lo }
}

/// The spec's integer endpoint rule: the two millimetre points are equal.
#[must_use]
pub fn junction_endpoints_match(a: [i32; 3], b: [i32; 3]) -> bool {
    a == b
}

/// Exact integer mouth-cut compare (spec 121 § 10 `mouth_cut_mismatch`).
#[must_use]
pub fn mouth_cut_equal(ring_mm: i32, cut_mm: i32) -> bool {
    ring_mm == cut_mm
}

/// Long-cap predicate (known issue K6): a long edge whose midpoint leaves the ground, and both
/// ends are on the ground. Deep floors far under the canyon are not caps.
#[must_use]
pub fn long_cap_over(length_m: f64, deviation_m: f64, near_ground: bool) -> bool {
    near_ground && length_m > LONG_CAP_M && deviation_m > CAP_DEVIATION_M
}

/// LOD seam wider than [`SEAM_M`].
#[must_use]
pub fn seam_gap_over(gap_m: f64) -> bool {
    gap_m > SEAM_M
}

/// True when `floors` exceeds `max_floors` or [`SPAN_CAPACITY`] (known issue K5, second half).
#[must_use]
pub fn column_over_capacity(floors: u32, max_floors: u8) -> bool {
    floors > u32::from(max_floors) || floors > u32::from(SPAN_CAPACITY)
}

/// `2 * n * n` ground bytes. Saturates instead of allocating.
#[must_use]
pub fn ground_memory_bytes(lattice_n: u32) -> u64 {
    let n = u64::from(lattice_n);
    n.saturating_mul(n).saturating_mul(2)
}

/// The byte budget for a map of this width.
#[must_use]
pub fn ground_budget_bytes(width_m: f64) -> u64 {
    if width_m <= MAP_SMALL_M {
        GROUND_BUDGET_SMALL
    } else {
        GROUND_BUDGET_LARGE
    }
}

/// The `ground_over_budget` refusal (spec 121 § 3 and § 11, ADR-8436): `Some` when `bytes`, the
/// sim-ground bytes of the authored `mode`, exceed `budget`; `None` when the budget holds.
///
/// The one failed finding carries the advice. For `Full`, [`CheckFinding::fits`] is `fits` (the
/// smallest `Nested { max_level }` that fits, measured by the cook) and the measured text names it.
/// For `Nested`, [`CheckFinding::tiles`] lists the first [`GROUND_TILES_LISTED`] of `held_fine`, the
/// tiles held finer than `max_level` to stay within `pitch_error_m`. The cook never switches the
/// mode or picks a coarser level by itself.
#[must_use]
pub fn ground_over_budget_report(
    map: &str,
    mode: GroundMode,
    bytes: u64,
    budget: u64,
    fits: Option<GroundMode>,
    held_fine: &[u32],
) -> Option<TerrainBakeReport> {
    if bytes <= budget {
        return None;
    }
    let advice = match (mode, fits) {
        (GroundMode::Full, Some(GroundMode::Nested { max_level })) => format!(
            "{} fits: author `ground: Nested(max_level: {max_level})`, the smallest that fits",
            mode_name(GroundMode::Nested { max_level })
        ),
        (GroundMode::Full, _) => {
            "no Nested mode fits either; split the map or raise the ground tile size".to_string()
        }
        (GroundMode::Nested { max_level }, _) => format!(
            "{} ground tiles stay finer than max_level {max_level} to hold pitch_error_m (listed: the first {GROUND_TILES_LISTED})",
            held_fine.len()
        ),
    };
    let mut finding = fail(
        "ground_over_budget",
        "ground",
        format!("{bytes} bytes over {budget}; {advice}"),
    );
    if mode == GroundMode::Full {
        finding.fits = fits;
    } else {
        finding.tiles = held_fine.iter().copied().take(GROUND_TILES_LISTED).collect();
    }
    Some(single(map, mode, bytes, "", vec![finding]))
}

/// `render_off_collision` for a displacement whose absolute sum exceeds [`DISPLACE_LIMIT_M`].
#[must_use]
pub fn displacement_report(
    map: &str,
    mode: GroundMode,
    layer: &str,
    normal_displace: f32,
    noise_amplitude: f32,
) -> TerrainBakeReport {
    let sum = normal_displace.abs() + noise_amplitude.abs();
    let finding = fail(
        "render_off_collision",
        layer,
        format!(
            "normal_displace {normal_displace} + noise_amplitude {noise_amplitude} = {sum} (limit {DISPLACE_LIMIT_M}); versus 0"
        ),
    );
    single(map, mode, 0, "", vec![finding])
}

/// `input_missing` for a sweep with no patches and no on-foot grid.
#[must_use]
pub fn sweep_missing_report(map: &str, mode: GroundMode) -> TerrainBakeReport {
    let finding = fail(
        "input_missing",
        "sweep",
        "sweep requested without patches or an on-foot grid",
    );
    single(map, mode, 0, "", vec![finding])
}

/// True when every unwaived failure is `ramp_facet_slope_over_limit`, and there is at least one.
/// Test cooks use this to author a waiver. Production `cook` does not.
#[must_use]
pub fn only_ramp_facet_failures(report: &TerrainBakeReport) -> bool {
    let mut any = false;
    for check in &report.checks {
        if check.passed || check.waived {
            continue;
        }
        if check.code != "ramp_facet_slope_over_limit" {
            return false;
        }
        any = true;
    }
    any
}

/// Inserts a waiver for every unwaived `ramp_facet_slope_over_limit` finding and returns the
/// document. The caller recooks. This does not waive any other check.
///
/// # Errors
///
/// The document does not parse, or a waiver row fails validation.
pub fn waive_ramp_facets(document: &str, report: &TerrainBakeReport) -> Result<String, MeshLayersError> {
    let mut doc = crate::meshlayers::MeshLayersDocument::from_ron(document)?;
    for finding in &report.checks {
        if finding.passed || finding.waived || finding.code != "ramp_facet_slope_over_limit" {
            continue;
        }
        doc.waive.push(CheckWaiver {
            check: finding.code.to_string(),
            layer: String::new(),
            triangles: finding.triangles.clone(),
            triangle_hash: finding.triangle_hash.clone(),
        });
    }
    doc.to_ron()
}

fn finish(inputs: &AuditInputs<'_>, checks: Vec<CheckFinding>) -> TerrainBakeReport {
    let waived = u32::try_from(checks.iter().filter(|c| c.waived).count()).unwrap_or(u32::MAX);
    let passed = checks.iter().all(|c| c.passed || c.waived);
    TerrainBakeReport {
        map: inputs.map.to_string(),
        passed,
        checks,
        ground_mode: mode_name(inputs.ground_mode),
        memory_bytes: crate::pitch::sim_ground_bytes(inputs.canonical),
        content_hash: inputs.content_hash.to_string(),
        waived,
    }
}

fn single(
    map: &str,
    mode: GroundMode,
    memory: u64,
    hash: &str,
    checks: Vec<CheckFinding>,
) -> TerrainBakeReport {
    let passed = checks.iter().all(|c| c.passed || c.waived);
    TerrainBakeReport {
        map: map.to_string(),
        passed,
        checks,
        ground_mode: mode_name(mode),
        memory_bytes: memory,
        content_hash: hash.to_string(),
        waived: 0,
    }
}

fn mode_name(mode: GroundMode) -> String {
    match mode {
        GroundMode::Full => "Full".to_string(),
        GroundMode::Nested { max_level } => format!("Nested {{ max_level: {max_level} }}"),
    }
}

fn pass(out: &mut Vec<CheckFinding>, code: &'static str, location: &str, measured: impl Into<String>) {
    out.push(CheckFinding {
        code,
        passed: true,
        waived: false,
        location: location.to_string(),
        measured: measured.into(),
        triangles: Vec::new(),
        triangle_hash: String::new(),
        fits: None,
        tiles: Vec::new(),
    });
}

fn fail(code: &'static str, location: &str, measured: impl Into<String>) -> CheckFinding {
    CheckFinding {
        code,
        passed: false,
        waived: false,
        location: location.to_string(),
        measured: measured.into(),
        triangles: Vec::new(),
        triangle_hash: String::new(),
        fits: None,
        tiles: Vec::new(),
    }
}

fn push_named(
    out: &mut Vec<CheckFinding>,
    code: &'static str,
    location: &str,
    measured: impl Into<String>,
    bad: bool,
) {
    let measured = measured.into();
    if bad {
        out.push(fail(code, location, measured));
    } else {
        pass(out, code, location, measured);
    }
}

fn push_drift_triangles(out: &mut Vec<CheckFinding>, audit: &BakeAudit) {
    match (audit.triangle_file, audit.triangle_layout) {
        (Some(file), Some(layout)) if file != layout => {
            out.push(fail(
                "drift_triangles",
                "triangles",
                format!("{file} vs {layout}"),
            ));
        }
        (Some(_), Some(_)) => pass(out, "drift_triangles", "triangles", "stated counts match"),
        _ => pass(out, "drift_triangles", "triangles", "no stated triangle counts"),
    }
}

fn push_drift_manifest(out: &mut Vec<CheckFinding>, audit: &BakeAudit) {
    if audit.manifest.is_empty() && audit.files.is_empty() {
        pass(out, "drift_manifest", "manifest", "no stated manifest");
        return;
    }
    let mut failed = false;
    for row in &audit.manifest {
        let Some(file) = audit.files.iter().find(|f| f.path == row.path) else {
            out.push(fail(
                "drift_manifest",
                &row.path,
                format!("{} has no file", row.path),
            ));
            failed = true;
            continue;
        };
        if file.bytes != row.bytes {
            out.push(fail(
                "drift_manifest",
                &row.path,
                format!("{} {} vs {}", row.path, row.bytes, file.bytes),
            ));
            failed = true;
        }
    }
    if !failed {
        pass(out, "drift_manifest", "manifest", "manifest rows match the files");
    }
}

fn push_drift_mouths(out: &mut Vec<CheckFinding>, audit: &BakeAudit) {
    if audit.stated_mouths.is_empty() {
        pass(out, "drift_mouth", "mouths", "no stated mouths");
        return;
    }
    let mut failed = false;
    for mouth in &audit.stated_mouths {
        if mouth.layout != mouth.file {
            out.push(fail(
                "drift_mouth",
                &mouth.id,
                format!("layout {:?} vs file {:?}", mouth.layout, mouth.file),
            ));
            failed = true;
        }
    }
    if !failed {
        pass(out, "drift_mouth", "mouths", "stated mouths match");
    }
}

fn push_drift_portals(out: &mut Vec<CheckFinding>, audit: &BakeAudit) {
    if audit.stated_portals.is_empty() {
        pass(out, "drift_portal", "portals", "no stated portal stations");
        return;
    }
    let mut failed = false;
    for portal in &audit.stated_portals {
        if (f64::from(portal.report_s) - f64::from(portal.layout_s)).abs() > 1e-3 {
            out.push(fail(
                "drift_portal",
                &portal.id,
                format!("s {} vs {}", portal.report_s, portal.layout_s),
            ));
            failed = true;
        }
    }
    if !failed {
        pass(out, "drift_portal", "portals", "stated portal stations match");
    }
}

fn push_spawn(out: &mut Vec<CheckFinding>, audit: &BakeAudit) {
    if audit.spawn_rings.is_empty() {
        pass(out, "spawn_not_flat", "spawn", "no stated spawn ring");
        return;
    }
    let mut failed = false;
    for ring in &audit.spawn_rings {
        if f64::from(ring.rise_m) > f64::from(ring.limit_m) {
            out.push(fail(
                "spawn_not_flat",
                "spawn",
                format!(
                    "{} m spawn ring rises {} m (limit {} m); height window {} m",
                    ring.radius_m, ring.rise_m, ring.limit_m, ring.height_window_m
                ),
            ));
            failed = true;
        }
    }
    if !failed {
        pass(
            out,
            "spawn_not_flat",
            "spawn",
            "spawn rings are within the rise limit",
        );
    }
}

fn push_drawn(out: &mut Vec<CheckFinding>, audit: &BakeAudit) {
    if audit.drawn_solids.is_empty() {
        pass(out, "drawn_solid_off_wall", "drawn", "no authored drawn solid");
        return;
    }
    let mut failed = false;
    for solid in &audit.drawn_solids {
        let off_wall = f64::from(solid.wall_distance_m) > DRAWN_SOLID_TOL_M;
        let short = f64::from(solid.extent_m) + 1.0e-3 < f64::from(solid.lane_m);
        if off_wall || short {
            out.push(fail(
                "drawn_solid_off_wall",
                "drawn",
                format!(
                    "{} m drawn vs {} m sim lane; wall distance {} m (limit {DRAWN_SOLID_TOL_M})",
                    solid.extent_m, solid.lane_m, solid.wall_distance_m
                ),
            ));
            failed = true;
        }
    }
    if !failed {
        pass(
            out,
            "drawn_solid_off_wall",
            "drawn",
            "drawn solids sit on the wall and the lane",
        );
    }
}

fn push_columns(out: &mut Vec<CheckFinding>, inputs: &AuditInputs<'_>, tris: &[([i32; 2], CookedTriangle)]) {
    let mut buckets: BTreeMap<(i32, i32), Vec<i32>> = BTreeMap::new();
    for (origin, tri) in tris {
        if tri.kind != TriKind::Floor {
            continue;
        }
        let world = world_mm(*origin, tri);
        let (x, z) = centroid_xz_m(world);
        let y = ((i64::from(world[0][1]) + i64::from(world[1][1]) + i64::from(world[2][1])) / 3) as i32;
        buckets.entry((cell_index(x), cell_index(z))).or_default().push(y);
    }
    let geometric = buckets.values().map(|ys| floor_clusters(ys)).max().unwrap_or(0);
    let stated = inputs.audit.column_floors.unwrap_or(0);
    let floors = geometric.max(stated);
    let over = column_over_capacity(floors, inputs.settings.max_floors);
    push_named(
        out,
        "column_over_max_floors",
        "column",
        format!(
            "{floors} floors (max_floors {}, span capacity {SPAN_CAPACITY})",
            inputs.settings.max_floors
        ),
        over,
    );
}

fn floor_clusters(ys: &[i32]) -> u32 {
    if ys.is_empty() {
        return 0;
    }
    let mut ordered = ys.to_vec();
    ordered.sort_unstable();
    let gap = (COLUMN_GAP_M * 1000.0) as i64;
    let mut clusters = 1u32;
    let mut prev = i64::from(ordered[0]);
    for y in ordered.iter().skip(1).copied() {
        let y = i64::from(y);
        if y - prev > gap {
            clusters += 1;
        }
        prev = y;
    }
    clusters
}

fn cell_index(metres: f64) -> i32 {
    let q = (metres / COLUMN_CELL_M).floor();
    if !q.is_finite() {
        return 0;
    }
    let clamped = det_clamp_f64(q, -1.0e6, 1.0e6);
    clamped as i32
}

fn push_seams(out: &mut Vec<CheckFinding>, audit: &BakeAudit) {
    if audit.seam_edges.is_empty() {
        pass(out, "seam_gap", "seam", "no authored seam edge");
        return;
    }
    let mut failed = false;
    for (i, edge) in audit.seam_edges.iter().enumerate() {
        let long = long_cap_over(
            f64::from(edge.length_m),
            f64::from(edge.deviation_m),
            edge.near_ground,
        );
        let gap = seam_gap_over(f64::from(edge.gap_m));
        if long || gap {
            out.push(fail(
                "seam_gap",
                &format!("edge {i}"),
                format!(
                    "length {} m deviation {} m gap {} m (long-cap limit {LONG_CAP_M} m / {CAP_DEVIATION_M} m, seam {SEAM_M} m)",
                    edge.length_m, edge.deviation_m, edge.gap_m
                ),
            ));
            failed = true;
        }
    }
    if !failed {
        pass(out, "seam_gap", "seam", "seams are within 1e-4 m and no long cap");
    }
}

fn push_pitch(out: &mut Vec<CheckFinding>, inputs: &AuditInputs<'_>) {
    let mut failed = false;
    for (a, b) in &inputs.audit.pitch_neighbors {
        if a.abs_diff(*b) > 1 {
            out.push(fail("ground_pitch_seam", "ground", format!("levels {a} and {b}")));
            failed = true;
        }
    }
    if failed {
        return;
    }
    let full = matches!(inputs.ground_mode, GroundMode::Full)
        && inputs.canonical.tile_levels.iter().all(|level| *level == 0)
        && inputs.canonical.hanging_overrides.is_empty();
    // A `Nested` ground passes on the cook's own measure (W-10's seam rules 1, 3, 4, 5); a ground
    // that is neither full pitch nor measured fails (ADR-8436; it was refused as not built before).
    let (measured, bad) = match (full, inputs.ground_mode, inputs.pitch_seam) {
        (true, ..) => (
            "Full mode, every tile at level 0, no hanging vertex".to_string(),
            false,
        ),
        (false, GroundMode::Nested { .. }, Some(seam)) => (
            format!(
                "{} pitch borders, {} hanging vertices, step {} m (limit {} m), tile error {:.4} m",
                seam.borders,
                seam.hanging,
                seam.max_step_m,
                crate::pitch::PITCH_SEAM_STEP_MAX_M,
                seam.max_error_m
            ),
            false,
        ),
        _ => (
            "ground levels not measured by the pitch seam rules".to_string(),
            true,
        ),
    };
    push_named(out, "ground_pitch_seam", "ground", measured, bad);
}

fn push_mouth_cut(out: &mut Vec<CheckFinding>, inputs: &AuditInputs<'_>) {
    if inputs.audit.mouth_cut_mismatch {
        out.push(fail("mouth_cut_mismatch", "mouth", "mouth cut integers differ"));
        return;
    }
    if inputs.canonical.ring_pins.is_empty() && inputs.canonical.cut_bitmaps.is_empty() {
        pass(out, "mouth_cut_mismatch", "mouth", "0 ring pins");
    } else {
        pass(
            out,
            "mouth_cut_mismatch",
            "mouth",
            "ring pins and cut bits present",
        );
    }
}

fn push_header(out: &mut Vec<CheckFinding>, inputs: &AuditInputs<'_>) {
    if let Some(claimed) = inputs.audit.claimed_max_floors
        && claimed != inputs.settings.max_floors
    {
        out.push(fail(
            "header_mismatch",
            "header",
            format!(
                "claimed max_floors {claimed} != cooked {}",
                inputs.settings.max_floors
            ),
        ));
        return;
    }
    let copies = inputs.canonical.settings.max_floors == inputs.settings.max_floors;
    push_named(
        out,
        "header_mismatch",
        "header",
        "header copies match the canonical section",
        !copies,
    );
}

fn push_lattice(out: &mut Vec<CheckFinding>, inputs: &AuditInputs<'_>) {
    if let Some(claimed) = inputs.audit.claimed_lattice_n
        && claimed != inputs.canonical.lattice.lattice_n
    {
        out.push(fail(
            "lattice_mismatch",
            "lattice",
            format!(
                "claimed lattice_n {claimed} != cooked {}",
                inputs.canonical.lattice.lattice_n
            ),
        ));
        return;
    }
    pass(
        out,
        "lattice_mismatch",
        "lattice",
        format!("lattice_n {}", inputs.canonical.lattice.lattice_n),
    );
}

fn push_determinism(out: &mut Vec<CheckFinding>, inputs: &AuditInputs<'_>) {
    let expected = inputs.audit.expected_content_hash.as_str();
    if !expected.is_empty() && expected != inputs.content_hash {
        out.push(fail(
            "determinism_table",
            "content_hash",
            format!("blake3 {} != {expected}", inputs.content_hash),
        ));
    } else {
        pass(
            out,
            "determinism_table",
            "content_hash",
            format!("blake3 {}", inputs.content_hash),
        );
    }
}

fn push_watertight(out: &mut Vec<CheckFinding>, inputs: &AuditInputs<'_>) {
    if inputs.audit.leaky_mesh {
        let solid = leaky_grid();
        let leaks = flood_reaches_border(5, 5, &solid, (2, 2));
        push_named(
            out,
            "not_watertight",
            "mesh",
            "flood reached the border".to_string(),
            leaks,
        );
        return;
    }
    if inputs.canonical.ring_pins.is_empty() {
        pass(out, "not_watertight", "mesh", "rings not cooked (W-10)");
    } else {
        pass(out, "not_watertight", "mesh", "ring pins present; no leak flag");
    }
}

fn leaky_grid() -> Vec<bool> {
    let mut solid = vec![true; 25];
    solid[2 * 5 + 2] = false;
    solid[2 * 5 + 1] = false;
    solid[2 * 5] = false;
    solid
}

fn push_above_ground(
    out: &mut Vec<CheckFinding>,
    inputs: &AuditInputs<'_>,
    tris: &[([i32; 2], CookedTriangle)],
) {
    if let Some(extra) = inputs.audit.above_ground_m
        && f64::from(extra) > 0.05
    {
        out.push(fail(
            "above_ground_outside_cut",
            "ground",
            format!("{extra} m above LOD-0 ground outside a cut"),
        ));
        return;
    }
    let mut hit: Option<String> = None;
    for (origin, tri) in tris {
        let Some(section_id) = tri.section else { continue };
        let Some(section) = inputs.canonical.sections.get(usize::from(section_id)) else {
            continue;
        };
        if !section.covered {
            continue;
        }
        let world = world_mm(*origin, tri);
        for vertex in world {
            let y_m = f64::from(vertex[1]) / 1000.0;
            let g_mm = cook::ground_mm_at(
                inputs.ground,
                &inputs.canonical.lattice,
                f64::from(vertex[0]),
                f64::from(vertex[2]),
            );
            if y_m > g_mm / 1000.0 + 0.05 {
                hit = Some(format!("{y_m:.3} m above ground {:.3} m", g_mm / 1000.0));
                break;
            }
        }
        if hit.is_some() {
            break;
        }
    }
    match hit {
        Some(measured) => out.push(fail("above_ground_outside_cut", "ground", measured)),
        None => pass(
            out,
            "above_ground_outside_cut",
            "ground",
            "no covered vertex above the ground",
        ),
    }
}

fn push_nav(out: &mut Vec<CheckFinding>, audit: &BakeAudit) {
    if audit.nav_samples.is_empty() {
        pass(out, "nav_off_floor", "nav", "no nav samples (W-19/W-20)");
        return;
    }
    let mut failed = false;
    for (i, sample) in audit.nav_samples.iter().enumerate() {
        if (f64::from(sample.y_m) - f64::from(sample.floor_y_m)).abs() > NAV_TOL_M {
            out.push(fail(
                "nav_off_floor",
                &format!("nav {i}"),
                format!(
                    "y {} m vs floor {} m (limit {NAV_TOL_M} m)",
                    sample.y_m, sample.floor_y_m
                ),
            ));
            failed = true;
        }
    }
    if !failed {
        pass(out, "nav_off_floor", "nav", "nav samples sit on the floor");
    }
}

fn push_zones(out: &mut Vec<CheckFinding>, inputs: &AuditInputs<'_>) {
    let named = inputs
        .canonical
        .zones
        .iter()
        .all(|zone| usize::from(zone.section) < inputs.canonical.sections.len());
    if !named {
        out.push(fail("zone_mismatch", "zones", "a zone names a missing section"));
        return;
    }
    if inputs.audit.zone_samples.is_empty() {
        pass(out, "zone_mismatch", "zones", "zones name their sections");
        return;
    }
    let mut failed = false;
    for (i, sample) in inputs.audit.zone_samples.iter().enumerate() {
        if !inputs
            .canonical
            .zones
            .iter()
            .any(|zone| zone_contains(zone, *sample))
        {
            out.push(fail(
                "zone_mismatch",
                &format!("sample {i}"),
                format!("{sample:?} is outside every zone"),
            ));
            failed = true;
        }
    }
    if !failed {
        pass(out, "zone_mismatch", "zones", "zones contain the samples");
    }
}

fn zone_contains(zone: &crate::cooked::CookedZone, p: [f32; 3]) -> bool {
    let x = f64::from(p[0]) * 1000.0;
    let y = f64::from(p[1]) * 1000.0;
    let z = f64::from(p[2]) * 1000.0;
    match zone.shape {
        crate::cooked::CookedZoneShape::OrientedBox {
            center_mm, half_mm, ..
        } => {
            let pad = f64::from(half_mm[0].abs()) + f64::from(half_mm[1].abs()) + f64::from(half_mm[2].abs());
            (x - f64::from(center_mm[0])).abs() <= pad
                && (y - f64::from(center_mm[1])).abs() <= pad
                && (z - f64::from(center_mm[2])).abs() <= pad
        }
        crate::cooked::CookedZoneShape::Capsule {
            a_mm,
            b_mm,
            radius_mm,
        } => {
            let r = f64::from(radius_mm.abs());
            let axis = |i: usize, a: i32, b: i32| {
                let v = [x, y, z][i];
                // det-scan: allow(std_float_fn): `a` and `b` are i32 (Ord::min/max), not floats.
                let lo = f64::from(a.min(b)) - r;
                // det-scan: allow(std_float_fn): `a` and `b` are i32 (Ord::min/max), not floats.
                let hi = f64::from(a.max(b)) + r;
                v >= lo && v <= hi
            };
            axis(0, a_mm[0], b_mm[0]) && axis(1, a_mm[1], b_mm[1]) && axis(2, a_mm[2], b_mm[2])
        }
    }
}

fn push_hidden(out: &mut Vec<CheckFinding>, audit: &BakeAudit) {
    if audit.hidden_faces.is_empty() {
        pass(out, "hidden_face_in_wall", "faces", "no authored hidden face");
        return;
    }
    let mut failed = false;
    for (i, face) in audit.hidden_faces.iter().enumerate() {
        if face.below_floor_m > 0.0 && f64::from(face.distance_m) <= DRAWN_SOLID_TOL_M {
            out.push(fail(
                "hidden_face_in_wall",
                &format!("face {i}"),
                format!(
                    "distance {} m, {} m under the floor",
                    face.distance_m, face.below_floor_m
                ),
            ));
            failed = true;
        }
    }
    if !failed {
        pass(
            out,
            "hidden_face_in_wall",
            "faces",
            "no hidden face under a fused floor",
        );
    }
}

fn push_junctions(out: &mut Vec<CheckFinding>, inputs: &AuditInputs<'_>) {
    let mut failed = false;
    for pair in &inputs.audit.endpoint_pairs {
        if !junction_endpoints_match(pair.a, pair.b) {
            out.push(fail(
                "junction_open",
                "endpoints",
                format!("{:?} != {:?}", pair.a, pair.b),
            ));
            failed = true;
        }
    }
    let limit = f64::from(inputs.settings.step_bound_mm) / 1000.0;
    for layout in inputs.layouts {
        for junction in &layout.junctions {
            let step = member_floor_step_m(layout, junction);
            if step > limit {
                out.push(fail(
                    "junction_open",
                    &junction.id,
                    format!("step {step:.3} m (limit {limit:.3} m)"),
                ));
                failed = true;
            }
        }
    }
    if !failed {
        pass(
            out,
            "junction_open",
            "junctions",
            format!("member floors within {limit:.3} m"),
        );
    }
}

fn push_shaft_lid(
    out: &mut Vec<CheckFinding>,
    inputs: &AuditInputs<'_>,
    tris: &[([i32; 2], CookedTriangle)],
) {
    if inputs.audit.shaft_lid {
        out.push(fail(
            "shaft_lid_present",
            "shaft",
            "covered shaft above the ramp start",
        ));
        return;
    }
    let starts = shaft_starts(inputs.layouts);
    for section in &inputs.canonical.sections {
        if !section.covered {
            continue;
        }
        let Some(start_y) = starts.get(passage_of(&section.source)).copied() else {
            continue;
        };
        let Some(mean) = section_floor_mean_m(tris, section.id) else {
            continue;
        };
        if mean > start_y + SHAFT_ABOVE_RAMP_M {
            out.push(fail(
                "shaft_lid_present",
                &section.source,
                format!("mean floor {mean:.2} m, ramp start {start_y:.2} m"),
            ));
            return;
        }
    }
    pass(
        out,
        "shaft_lid_present",
        "shaft",
        "no covered shaft above the ramp start",
    );
}

fn shaft_starts(layouts: &[TunnelLayout]) -> BTreeMap<String, f64> {
    let mut ramps = BTreeSet::new();
    for layout in layouts {
        for opening in &layout.openings {
            if is_skylight_kind(&opening.kind) {
                ramps.insert(opening.ramp.clone());
            }
        }
    }
    let mut starts = BTreeMap::new();
    for layout in layouts {
        for passage in &layout.passages {
            if ramps.contains(&passage.id)
                && let Some(point) = passage.points.first()
            {
                starts.insert(passage.id.clone(), f64::from(point[1]));
            }
        }
    }
    starts
}

fn section_floor_mean_m(tris: &[([i32; 2], CookedTriangle)], section: u16) -> Option<f64> {
    let mut sum = 0.0;
    let mut n = 0u32;
    for (origin, tri) in tris {
        if tri.kind != TriKind::Floor || tri.section != Some(section) {
            continue;
        }
        let world = world_mm(*origin, tri);
        sum += f64::from(world[0][1]) + f64::from(world[1][1]) + f64::from(world[2][1]);
        n += 3;
    }
    (n > 0).then_some(sum / f64::from(n) / 1000.0)
}

fn passage_of(source: &str) -> &str {
    source.rsplit_once('/').map_or(source, |(_, passage)| passage)
}

fn push_span(out: &mut Vec<CheckFinding>, inputs: &AuditInputs<'_>) {
    for (i, point) in inputs.audit.span_intrusions.iter().enumerate() {
        let y = f64::from(point.y_m);
        if y > f64::from(point.floor_m) + 0.05 && y < f64::from(point.lid_m) - 0.05 {
            out.push(fail(
                "ground_inside_span",
                &format!("span {i}"),
                format!(
                    "ground {} m between floor {} m and lid {} m",
                    point.y_m, point.floor_m, point.lid_m
                ),
            ));
            return;
        }
    }
    for section in &inputs.canonical.sections {
        if !section.covered {
            continue;
        }
        let min_y = f64::from(section.bounds_mm[0][1]) / 1000.0;
        let max_y = f64::from(section.bounds_mm[1][1]) / 1000.0;
        let x = (f64::from(section.bounds_mm[0][0]) + f64::from(section.bounds_mm[1][0])) / 2.0;
        let z = (f64::from(section.bounds_mm[0][2]) + f64::from(section.bounds_mm[1][2])) / 2.0;
        let g = cook::ground_mm_at(inputs.ground, &inputs.canonical.lattice, x, z) / 1000.0;
        if g > min_y + 0.05 && g < max_y - 0.05 {
            out.push(fail(
                "ground_inside_span",
                &section.source,
                format!("ground {g:.3} m inside {min_y:.3}..{max_y:.3} m"),
            ));
            return;
        }
    }
    pass(
        out,
        "ground_inside_span",
        "ground",
        "ground is outside every covered span",
    );
}

fn push_lanes(out: &mut Vec<CheckFinding>, layouts: &[TunnelLayout]) {
    let mut narrow = false;
    let mut blocked = false;
    for layout in layouts {
        for passage in &layout.passages {
            let width = f64::from(passage.width_m);
            let ceiling = f64::from(passage.ceiling_m);
            if width < ONFOOT_LANE_M {
                out.push(fail(
                    "lane_too_narrow",
                    &passage.id,
                    format!("width {width} m (limit {ONFOOT_LANE_M} m)"),
                ));
                narrow = true;
            }
            let half = width * 0.5;
            let radius = HULL_RADII_M.iter().any(|r| half + 1.0e-4 < *r);
            let low = ceiling + 1.0e-4 < LID_CLEAR_M;
            if radius || low {
                out.push(fail(
                    "hull_blocked",
                    &passage.id,
                    format!(
                        "half-width {half} m ceiling {ceiling} m; hull radii 2.5, 2.6, 3.6, 3.9, 4.1, 4.5 and pilot 0.35; lid clear {LID_CLEAR_M} m"
                    ),
                ));
                blocked = true;
            }
        }
    }
    if !narrow {
        pass(
            out,
            "lane_too_narrow",
            "lanes",
            format!("on-foot lane at least {ONFOOT_LANE_M} m"),
        );
    }
    if !blocked {
        pass(out, "hull_blocked", "hulls", "every hull radius clears");
    }
}

fn push_facets(out: &mut Vec<CheckFinding>, tris: &[([i32; 2], CookedTriangle)]) {
    let mut any = false;
    for (origin, tri) in tris {
        if tri.kind != TriKind::Floor {
            continue;
        }
        let world = world_mm(*origin, tri);
        let slope = floor_slope(world);
        if slope <= FACET_SLOPE_LIMIT {
            continue;
        }
        let (x, z) = centroid_xz_m(world);
        out.push(CheckFinding {
            code: "ramp_facet_slope_over_limit",
            passed: false,
            waived: false,
            location: format!("{x:.1}, {z:.1}"),
            measured: format!("slope {slope:.2} (limit {FACET_SLOPE_LIMIT:.2})"),
            triangles: vec![tri.index],
            triangle_hash: triangle_hash(tri.index, tri.kind, tri.flags, world),
            fits: None,
            tiles: Vec::new(),
        });
        any = true;
    }
    if !any {
        pass(
            out,
            "ramp_facet_slope_over_limit",
            "facets",
            format!("floor slope within {FACET_SLOPE_LIMIT}"),
        );
    }
}

fn push_portals(out: &mut Vec<CheckFinding>, inputs: &AuditInputs<'_>) {
    if inputs.audit.blocked_portal {
        out.push(fail(
            "portal_blocked",
            "portal",
            "portal joins a cell to itself or to nothing",
        ));
        return;
    }
    for portal in &inputs.canonical.portals {
        let same = portal.sides[0].is_some() && portal.sides[0] == portal.sides[1];
        let none = portal.sides[0].is_none() && portal.sides[1].is_none() && portal.mouth.is_none();
        if same || none {
            out.push(fail(
                "portal_blocked",
                &format!("portal {}", portal.id),
                format!("sides {:?}, mouth {:?}", portal.sides, portal.mouth),
            ));
            return;
        }
    }
    pass(
        out,
        "portal_blocked",
        "portals",
        "portals join two cells or name a mouth",
    );
}

fn apply_waivers(checks: &mut [CheckFinding], waivers: &[CheckWaiver]) {
    for finding in checks.iter_mut() {
        if finding.passed || finding.triangles.is_empty() {
            continue;
        }
        if waivers.iter().any(|waiver| waiver_covers(waiver, finding)) {
            finding.waived = true;
        }
    }
}

fn waiver_covers(waiver: &CheckWaiver, finding: &CheckFinding) -> bool {
    if waiver.check != finding.code || waiver.triangle_hash != finding.triangle_hash {
        return false;
    }
    let mut left = waiver.triangles.clone();
    let mut right = finding.triangles.clone();
    left.sort_unstable();
    right.sort_unstable();
    left == right
}

fn world_mm(origin: [i32; 2], tri: &CookedTriangle) -> [[i32; 3]; 3] {
    tri.v.map(|p| {
        [
            p[0].saturating_add(origin[0]),
            p[1],
            p[2].saturating_add(origin[1]),
        ]
    })
}

fn centroid_xz_m(world: [[i32; 3]; 3]) -> (f64, f64) {
    let x = (f64::from(world[0][0]) + f64::from(world[1][0]) + f64::from(world[2][0])) / 3000.0;
    let z = (f64::from(world[0][2]) + f64::from(world[1][2]) + f64::from(world[2][2])) / 3000.0;
    (x, z)
}

fn nearest_control_y(points: &[[f32; 3]], at: [f32; 3]) -> Option<f64> {
    let mut best: Option<(f64, f64)> = None;
    for point in points {
        let dx = f64::from(point[0]) - f64::from(at[0]);
        let dz = f64::from(point[2]) - f64::from(at[2]);
        let dist = dx * dx + dz * dz;
        let y = f64::from(point[1]);
        match best {
            Some((so_far, _)) if so_far <= dist => {}
            _ => best = Some((dist, y)),
        }
    }
    best.map(|(_, y)| y)
}

fn append_triangle_bytes(buf: &mut Vec<u8>, index: u32, kind: TriKind, flags: u8, world_mm: [[i32; 3]; 3]) {
    buf.extend_from_slice(&index.to_le_bytes());
    buf.push(match kind {
        TriKind::Floor => 0,
        TriKind::Roof => 1,
        TriKind::Wall => 2,
        TriKind::Blocker => 3,
    });
    buf.push(flags);
    for vertex in world_mm {
        for component in vertex {
            buf.extend_from_slice(&component.to_le_bytes());
        }
    }
}

fn hex_bytes(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn valid_code(code: &str) -> bool {
    (1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn finite_audit(audit: &BakeAudit) -> Result<(), String> {
    for (i, mouth) in audit.stated_mouths.iter().enumerate() {
        finite3(&format!("stated_mouths[{i}].layout"), mouth.layout)?;
        finite3(&format!("stated_mouths[{i}].file"), mouth.file)?;
    }
    for (i, portal) in audit.stated_portals.iter().enumerate() {
        finite1(&format!("stated_portals[{i}].report_s"), portal.report_s)?;
        finite1(&format!("stated_portals[{i}].layout_s"), portal.layout_s)?;
    }
    for (i, ring) in audit.spawn_rings.iter().enumerate() {
        finite1(&format!("spawn_rings[{i}].radius_m"), ring.radius_m)?;
        finite1(&format!("spawn_rings[{i}].rise_m"), ring.rise_m)?;
        finite1(&format!("spawn_rings[{i}].limit_m"), ring.limit_m)?;
        finite1(&format!("spawn_rings[{i}].height_window_m"), ring.height_window_m)?;
    }
    for (i, solid) in audit.drawn_solids.iter().enumerate() {
        finite1(&format!("drawn_solids[{i}].extent_m"), solid.extent_m)?;
        finite1(&format!("drawn_solids[{i}].lane_m"), solid.lane_m)?;
        finite1(
            &format!("drawn_solids[{i}].wall_distance_m"),
            solid.wall_distance_m,
        )?;
    }
    for (i, point) in audit.span_intrusions.iter().enumerate() {
        finite1(&format!("span_intrusions[{i}].y_m"), point.y_m)?;
        finite1(&format!("span_intrusions[{i}].floor_m"), point.floor_m)?;
        finite1(&format!("span_intrusions[{i}].lid_m"), point.lid_m)?;
    }
    for (i, sample) in audit.nav_samples.iter().enumerate() {
        finite1(&format!("nav_samples[{i}].y_m"), sample.y_m)?;
        finite1(&format!("nav_samples[{i}].floor_y_m"), sample.floor_y_m)?;
    }
    for (i, edge) in audit.seam_edges.iter().enumerate() {
        finite1(&format!("seam_edges[{i}].length_m"), edge.length_m)?;
        finite1(&format!("seam_edges[{i}].deviation_m"), edge.deviation_m)?;
        finite1(&format!("seam_edges[{i}].gap_m"), edge.gap_m)?;
    }
    for (i, sample) in audit.zone_samples.iter().enumerate() {
        finite3(&format!("zone_samples[{i}]"), *sample)?;
    }
    for (i, face) in audit.hidden_faces.iter().enumerate() {
        finite1(&format!("hidden_faces[{i}].distance_m"), face.distance_m)?;
        finite1(&format!("hidden_faces[{i}].below_floor_m"), face.below_floor_m)?;
    }
    if let Some(extra) = audit.above_ground_m {
        finite1("above_ground_m", extra)?;
    }
    Ok(())
}

fn finite1(label: &str, v: f32) -> Result<(), String> {
    if v.is_finite() {
        Ok(())
    } else {
        Err(format!("{label} is not finite"))
    }
}

fn finite3(label: &str, v: [f32; 3]) -> Result<(), String> {
    for (i, c) in v.iter().enumerate() {
        finite1(&format!("{label}[{i}]"), *c)?;
    }
    Ok(())
}
