// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (tools/tunnels_v2/tunnel_bake.py, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-1014, mark: adapt (Stage A: unchanged).
// Edits: Python rewritten in Rust (AGENTS rule 2; numpy dropped, f64 cook math with the pinned sqrt/atan2): constants 35-87; `validate_description` 286 with `_validate_passage` 135, `_validate_opening` 170, `_validate_looks` 186, `_validate_gameplay` 221, `_validate_crawlway` 238; `sample_centreline` 382 / `sample_layout` 439 replaced by the pinned Stage A sampler where they agree; collision = the pinned mesher, render = `crate::render`; `build_terrain_cut` 675 and `build_nav` 710 as requirements (W-10, W-19/W-20 own them); `check_consistency` 827-946 as W-09's input; `bake` 957, `canonical_bytes` 988 (integer mm replace the 6-digit rounding), `bake_to_dir` 1000; sample step 1..20 m; DO27 file and engine names dropped; plan row W-07, ADR-8175.

//! The one-description bake of a `do27-tunnels-v2/1` tunnel description (plan row W-07, ADR-8175).
//!
//! One description (centrelines, widths, ceilings, openings, junctions, crawlways, looks, gameplay) feeds one bake
//! that writes the render mesh, the collision mesh, the mouth cut and the centreline nav, so they can never
//! disagree; [`check_consistency`] fails the bake when any two outputs do.
//!
//! - [`validate_description`] refuses a bad description with a message that names the fix.
//! - [`sample`] samples it with the pinned Stage A sampler ([`crate::compat::layout::TunnelLayout::sample`]): the
//!   same Catmull-Rom centreline (Catmull and Rom 1974; mirrored ghost end points, 64 sub-steps per span, equal arc
//!   length), cover, junction-wall and cap rules. Unknown overburden (no ground) bakes every segment open.
//! - [`build_meshes`]: the collision mesh is the pinned Stage A mesher's; the render mesh subdivides its faces
//!   ([`crate::render`]), so both share the section planes and the collision triangles never reach the draw list.
//! - [`build_terrain_cut`]: one cut quad and one portal per ramp opening. [`build_nav`]: the centreline graph.
//!
//! Determinism: every output position is integer millimetres (the pinned `m_to_mm`), so [`canonical_bytes`] and
//! [`bake_to_dir`] write the same bytes on every host.

use regia_detsim::float::{det_clamp_f64, det_max_f64, det_min_f64};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::compat::layout::{
    CrawlwayVolume, Junction, LAYOUT_VERSION, Opening, Passage, PassageKind, SampledLayout, TunnelLayout,
};
use crate::compat::sampler::TerrainSampler;
use crate::cook::{WorldTriangleMm, quantize_mesh};
use crate::render::{RenderLayerInputs, cut_corners, outward, render_layer};
use crate::units::m_to_mm;

/// The description format this module reads.
pub const FORMAT: &str = "do27-tunnels-v2/1";

/// Centreline sub-steps per spline span (the pinned sampler's).
pub const SAMPLE_SUB: usize = 64;
/// Rock this far over a ceiling covers it; less is an open cut (the pinned sampler's).
pub const ROCK_COVER_M: f32 = crate::compat::layout::ROCK_COVER_M;
/// Two passage floors within this height join flush (the pinned mesher's).
pub const FLUSH_M: f32 = crate::compat::layout::FLUSH_M;
/// The top this many metres of a ramp is the portal onto the surface (the pinned mesher's).
pub const PORTAL_LENGTH_M: f32 = crate::compat::layout::PORTAL_LENGTH_M;
/// Extra graded lip drawn around every mouth cut, metres.
pub const MOUTH_LIP_M: f32 = crate::render::MOUTH_LIP_M;
/// An opening's top must sit this close to its ramp's last point, metres.
pub const OPENING_TOP_TOL_M: f64 = 1.0;
/// A junction's `at` must sit this close (past the member's half width) to every member centreline, metres.
pub const JUNCTION_AT_TOL_M: f64 = 2.0;
/// Rule `collision_inside_render`: tolerance, metres.
pub const COLLISION_IN_RENDER_TOL_M: f64 = 0.05;
/// Rule `nav_on_collision_floor`: tolerance, metres.
pub const NAV_ON_FLOOR_TOL_M: f64 = 0.5;
/// Rule `render_covers_floor`: tolerance, metres.
pub const RENDER_FLOOR_TOL_M: f64 = 0.05;
/// The smallest sample step, metres (the pinned sampler clamps smaller steps to 1 m).
pub const MIN_SAMPLE_STEP_M: f64 = 1.0;
/// The largest sample step, metres.
pub const MAX_SAMPLE_STEP_M: f64 = 20.0;

/// Passage kinds (the look; only `ramp` changes the collision).
pub const PASSAGE_KINDS: [&str; 5] = ["tube", "gallery", "ramp", "shaft", "chamber"];
/// The kind of a pilot-only side passage (not a passage kind: too narrow, and not in the collision mesh).
pub const CRAWLWAY_KIND: &str = "crawlway";
/// Crawlway width range, metres.
pub const MIN_CRAWLWAY_WIDTH_M: f64 = 0.90;
/// Crawlway width range, metres.
pub const MAX_CRAWLWAY_WIDTH_M: f64 = 2.00;
/// Crawlway height range, metres.
pub const MIN_CRAWLWAY_HEIGHT_M: f64 = 1.90;
/// Crawlway height range, metres.
pub const MAX_CRAWLWAY_HEIGHT_M: f64 = 3.00;
/// Opening kinds (the look).
pub const OPENING_KINDS: [&str; 6] = [
    "skylight_rubble",
    "mine_adit",
    "fracture_ramp",
    "breakdown_ramp",
    "portal",
    "shaft",
];
/// Audio presets of the looks block.
pub const AUDIO_PRESETS: [&str; 3] = ["tube", "gallery", "open"];
/// The render subdivision of [`bake`] (the baker's `detail`).
pub const BAKE_SUBDIVISION: u8 = crate::render::DEFAULT_SUBDIVISION;

/// (`tunnel_description_invalid`) a description failed validation. The message names the fix.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DescriptionError(pub String);

/// Why a bake failed.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum TunnelBakeError {
    /// (`tunnel_description_invalid`) the description.
    #[error(transparent)]
    Description(#[from] DescriptionError),
    /// (`tunnel_bake_disagrees`) the outputs disagreed (strict bake).
    #[error("{message}")]
    Disagrees {
        /// One line per failing rule.
        message: String,
        /// Every finding.
        findings: Vec<Finding>,
    },
}

impl TunnelBakeError {
    /// The agent error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Description(_) => "tunnel_description_invalid",
            Self::Disagrees { .. } => "tunnel_bake_disagrees",
        }
    }
}

fn need(cond: bool, message: impl FnOnce() -> String) -> Result<(), DescriptionError> {
    if cond {
        Ok(())
    } else {
        Err(DescriptionError(message()))
    }
}

fn dist3(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    regia_detsim::math::sqrt_f64(dx * dx + dy * dy + dz * dz)
}

fn hypot(x: f64, z: f64) -> f64 {
    regia_detsim::math::sqrt_f64(x * x + z * z)
}

fn f64p(p: [f32; 3]) -> [f64; 3] {
    p.map(f64::from)
}

// ---------------------------------------------------------------------------------------------------------------
// The description
// ---------------------------------------------------------------------------------------------------------------

/// One passage of a description.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DescPassage {
    /// Id (`^[a-z][a-z0-9_]{0,39}$`).
    pub id: String,
    /// One of [`PASSAGE_KINDS`].
    pub kind: String,
    /// Floor width, 8..80 m.
    pub width_m: f64,
    /// Ceiling over the floor, 4..40 m.
    pub ceiling_m: f64,
    /// 2..512 centreline points `[x, floor y, z]`, metres.
    pub points: Vec<[f64; 3]>,
    /// Optional level tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
}

/// One surface opening.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DescOpening {
    /// Id.
    pub id: String,
    /// One of [`OPENING_KINDS`].
    pub kind: String,
    /// The top, on the surface, metres.
    pub top: [f64; 3],
    /// The ramp passage that comes up here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ramp: Option<String>,
}

/// Where passages meet.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DescJunction {
    /// Id.
    pub id: String,
    /// The meeting point, metres.
    pub at: [f64; 3],
    /// At least two member passage ids.
    pub passages: Vec<String>,
}

/// A pilot-only side passage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DescCrawlway {
    /// Id (distinct from every passage and junction id).
    pub id: String,
    /// [`CRAWLWAY_KIND`].
    pub kind: String,
    /// Width, [`MIN_CRAWLWAY_WIDTH_M`]..=[`MAX_CRAWLWAY_WIDTH_M`].
    pub width_m: f64,
    /// Height, [`MIN_CRAWLWAY_HEIGHT_M`]..=[`MAX_CRAWLWAY_HEIGHT_M`].
    pub height_m: f64,
    /// 2..512 floor centreline points, metres.
    pub points: Vec<[f64; 3]>,
    /// Detail seed.
    pub seed: u32,
}

/// The looks block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DescLooks {
    /// Exposure range `[lo, hi]`, EV.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure_ev_range: Option<[f64; 2]>,
    /// Exposure adaptation, 0.1..30 s (default 2.5).
    pub exposure_adapt_s: f64,
    /// One of [`AUDIO_PRESETS`] (default `tube`).
    pub audio_preset: String,
    /// Audio range scale, 1..4 (default 1.5).
    pub audio_range_scale: f64,
    /// Light color, 0..1 each.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light_color: Option<[f64; 3]>,
    /// Floor roughness, 0..1 (default 0.8).
    pub floor_roughness: f64,
}

/// The gameplay block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DescGameplay {
    /// Rock blocks radar (default true).
    pub radar_blocked_by_rock: bool,
    /// AI path cost scale in tunnels, 0.1..2 (default 0.75).
    pub ai_cost_scale: f64,
    /// Flank cost scale, 0.1..2 (default 0.2).
    pub flank_cost_scale: f64,
    /// Whether ground structures fit inside tunnels: always false (the format's key name is kept).
    #[serde(rename = "reclaimer_fits")]
    pub structure_fits: bool,
}

/// A validated description, defaults filled.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Description {
    /// 1..64 passages.
    pub passages: Vec<DescPassage>,
    /// At most 64 openings.
    pub openings: Vec<DescOpening>,
    /// At most 64 junctions.
    pub junctions: Vec<DescJunction>,
    /// At most 64 crawlways (omitted when empty, so a description without them bakes the same bytes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crawlways: Vec<DescCrawlway>,
    /// Sample step, [`MIN_SAMPLE_STEP_M`]..=[`MAX_SAMPLE_STEP_M`] (default 5).
    pub sample_step_m: f64,
    /// Looks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub looks: Option<DescLooks>,
    /// Gameplay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gameplay: Option<DescGameplay>,
}

fn show(v: Option<&Value>) -> String {
    v.map_or_else(|| "None".to_string(), Value::to_string)
}

fn finite_number(v: &Value) -> Option<f64> {
    v.as_f64().filter(|x| x.is_finite())
}

fn number_or(raw: &Map<String, Value>, key: &str, default: f64) -> Option<f64> {
    match raw.get(key) {
        None => Some(default),
        Some(v) => finite_number(v),
    }
}

fn triple(value: Option<&Value>, what: &str) -> Result<[f64; 3], DescriptionError> {
    let list = value.and_then(Value::as_array);
    need(list.is_some_and(|l| l.len() == 3), || {
        format!("{what} must be [x, y, z] in metres")
    })?;
    let list = list.map(Vec::as_slice).unwrap_or_default();
    let nums: Vec<f64> = list.iter().filter_map(finite_number).collect();
    need(nums.len() == 3, || {
        format!("{what} must be finite numbers [x, y, z] in metres")
    })?;
    Ok([nums[0], nums[1], nums[2]])
}

fn id_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

fn level_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'.' | b'-'))
}

fn check_id(value: Option<&Value>, what: &str) -> Result<String, DescriptionError> {
    let s = value.and_then(Value::as_str).filter(|s| id_ok(s));
    need(s.is_some(), || {
        format!(
            "{what} id {} must match ^[a-z][a-z0-9_]{{0,39}}$ (use a lore slug)",
            show(value)
        )
    })?;
    Ok(s.unwrap_or_default().to_string())
}

fn check_no_extra(raw: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), DescriptionError> {
    let extra: Vec<&str> = raw
        .keys()
        .map(String::as_str)
        .filter(|k| !allowed.contains(k))
        .collect();
    let mut allowed: Vec<&str> = allowed.to_vec();
    allowed.sort_unstable();
    need(extra.is_empty(), || {
        format!("{what} has unknown keys {extra:?}; allowed keys are {allowed:?}")
    })
}

fn object<'a>(raw: &'a Value, what: &str) -> Result<&'a Map<String, Value>, DescriptionError> {
    raw.as_object()
        .ok_or_else(|| DescriptionError(format!("{what} must be an object")))
}

fn points(raw: &Map<String, Value>, what: &str, id: &str) -> Result<Vec<[f64; 3]>, DescriptionError> {
    let list = raw.get("points").and_then(Value::as_array);
    need(list.is_some_and(|l| (2..=512).contains(&l.len())), || {
        format!("{what} (id {id:?}) points must be a list of 2..512 [x, y, z] centreline points")
    })?;
    let list = list.map(Vec::as_slice).unwrap_or_default();
    let mut pts = Vec::with_capacity(list.len());
    for (i, p) in list.iter().enumerate() {
        pts.push(triple(Some(p), &format!("{what} (id {id:?}) points[{i}]"))?);
    }
    for (i, w) in pts.windows(2).enumerate() {
        need(dist3(w[0], w[1]) > 1e-6, || {
            format!(
                "{what} (id {id:?}) points[{i}] and points[{}] are the same point; consecutive centreline points must be distinct",
                i + 1
            )
        })?;
    }
    Ok(pts)
}

fn validate_passage(raw: &Value, index: usize) -> Result<DescPassage, DescriptionError> {
    let what = format!("passages[{index}]");
    let raw = object(raw, &what)?;
    check_no_extra(
        raw,
        &["id", "kind", "width_m", "ceiling_m", "points", "level"],
        &what,
    )?;
    let id = check_id(raw.get("id"), &what)?;
    let kind = match raw.get("kind") {
        None => Some("tube"),
        Some(v) => v.as_str(),
    };
    let kind = kind.filter(|k| PASSAGE_KINDS.contains(k));
    need(kind.is_some(), || {
        format!(
            "{what} (id {id:?}) kind {} must be one of {PASSAGE_KINDS:?}",
            show(raw.get("kind"))
        )
    })?;
    let width = number_or(raw, "width_m", 22.0).filter(|w| (8.0..=80.0).contains(w));
    need(width.is_some(), || {
        format!(
            "{what} (id {id:?}) width_m {} must be within 8..80 m",
            show(raw.get("width_m"))
        )
    })?;
    let ceiling = number_or(raw, "ceiling_m", 12.0).filter(|c| (4.0..=40.0).contains(c));
    need(ceiling.is_some(), || {
        format!(
            "{what} (id {id:?}) ceiling_m {} must be within 4..40 m",
            show(raw.get("ceiling_m"))
        )
    })?;
    let pts = points(raw, &what, &id)?;
    let level = match raw.get("level") {
        None => None,
        Some(v) => {
            let s = v.as_str().filter(|s| level_ok(s));
            need(s.is_some(), || {
                format!(
                    "{what} (id {id:?}) level {} must match ^[a-z][a-z0-9_.\\-]{{0,63}}$",
                    show(Some(v))
                )
            })?;
            s.map(str::to_string)
        }
    };
    Ok(DescPassage {
        id,
        kind: kind.unwrap_or_default().to_string(),
        width_m: width.unwrap_or_default(),
        ceiling_m: ceiling.unwrap_or_default(),
        points: pts,
        level,
    })
}

fn validate_opening(raw: &Value, index: usize, ramp_ids: &[&str]) -> Result<DescOpening, DescriptionError> {
    let what = format!("openings[{index}]");
    let raw = object(raw, &what)?;
    check_no_extra(raw, &["id", "kind", "ramp", "top"], &what)?;
    let id = check_id(raw.get("id"), &what)?;
    let kind = raw
        .get("kind")
        .and_then(Value::as_str)
        .filter(|k| OPENING_KINDS.contains(k));
    need(kind.is_some(), || {
        format!(
            "{what} (id {id:?}) kind {} must be one of {OPENING_KINDS:?}",
            show(raw.get("kind"))
        )
    })?;
    let top = triple(raw.get("top"), &format!("{what} (id {id:?}) top"))?;
    let ramp = match raw.get("ramp") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let r = v.as_str().filter(|r| ramp_ids.contains(r));
            let mut sorted = ramp_ids.to_vec();
            sorted.sort_unstable();
            need(r.is_some(), || {
                format!(
                    "{what} (id {id:?}) ramp {} does not name a ramp passage; ramp passages are {sorted:?}",
                    show(Some(v))
                )
            })?;
            r.map(str::to_string)
        }
    };
    Ok(DescOpening {
        id,
        kind: kind.unwrap_or_default().to_string(),
        top,
        ramp,
    })
}

fn validate_looks(raw: &Value) -> Result<DescLooks, DescriptionError> {
    let raw = object(raw, "looks")?;
    check_no_extra(
        raw,
        &[
            "exposure_ev_range",
            "exposure_adapt_s",
            "audio_preset",
            "audio_range_scale",
            "light_color",
            "floor_roughness",
        ],
        "looks",
    )?;
    let exposure_ev_range = match raw.get("exposure_ev_range") {
        None => None,
        Some(v) => {
            let nums: Vec<f64> = v
                .as_array()
                .map(|l| l.iter().filter_map(finite_number).collect())
                .unwrap_or_default();
            need(
                v.as_array().is_some_and(|l| l.len() == 2) && nums.len() == 2,
                || "looks exposure_ev_range must be [lo_ev, hi_ev]".to_string(),
            )?;
            need(nums[0] <= nums[1], || {
                "looks exposure_ev_range needs lo_ev <= hi_ev".to_string()
            })?;
            Some([nums[0], nums[1]])
        }
    };
    let adapt = number_or(raw, "exposure_adapt_s", 2.5).filter(|a| (0.1..=30.0).contains(a));
    need(adapt.is_some(), || {
        format!(
            "looks exposure_adapt_s {} must be within 0.1..30 s",
            show(raw.get("exposure_adapt_s"))
        )
    })?;
    let preset = match raw.get("audio_preset") {
        None => Some("tube"),
        Some(v) => v.as_str(),
    }
    .filter(|p| AUDIO_PRESETS.contains(p));
    need(preset.is_some(), || {
        format!(
            "looks audio_preset {} must be one of {AUDIO_PRESETS:?}",
            show(raw.get("audio_preset"))
        )
    })?;
    let scale = number_or(raw, "audio_range_scale", 1.5).filter(|s| (1.0..=4.0).contains(s));
    need(scale.is_some(), || {
        format!(
            "looks audio_range_scale {} must be within 1..4",
            show(raw.get("audio_range_scale"))
        )
    })?;
    let light_color = match raw.get("light_color") {
        None => None,
        Some(v) => {
            let nums: Vec<f64> = v
                .as_array()
                .map(|l| l.iter().filter_map(finite_number).collect())
                .unwrap_or_default();
            need(
                v.as_array().is_some_and(|l| l.len() == 3) && nums.len() == 3,
                || "looks light_color must be [r, g, b]".to_string(),
            )?;
            need(nums.iter().all(|c| (0.0..=1.0).contains(c)), || {
                "looks light_color values must be within 0..1".to_string()
            })?;
            Some([nums[0], nums[1], nums[2]])
        }
    };
    let rough = number_or(raw, "floor_roughness", 0.8).filter(|r| (0.0..=1.0).contains(r));
    need(rough.is_some(), || {
        format!(
            "looks floor_roughness {} must be within 0..1",
            show(raw.get("floor_roughness"))
        )
    })?;
    Ok(DescLooks {
        exposure_ev_range,
        exposure_adapt_s: adapt.unwrap_or_default(),
        audio_preset: preset.unwrap_or_default().to_string(),
        audio_range_scale: scale.unwrap_or_default(),
        light_color,
        floor_roughness: rough.unwrap_or_default(),
    })
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn validate_gameplay(raw: &Value) -> Result<DescGameplay, DescriptionError> {
    let raw = object(raw, "gameplay")?;
    check_no_extra(
        raw,
        &[
            "radar_blocked_by_rock",
            "ai_cost_scale",
            "flank_cost_scale",
            "reclaimer_fits",
        ],
        "gameplay",
    )?;
    let radar = raw.get("radar_blocked_by_rock").is_none_or(truthy);
    let mut scales = [0.0f64; 2];
    for (slot, (key, default)) in scales
        .iter_mut()
        .zip([("ai_cost_scale", 0.75), ("flank_cost_scale", 0.2)])
    {
        let v = number_or(raw, key, default).filter(|v| (0.1..=2.0).contains(v));
        need(v.is_some(), || {
            format!("gameplay {key} {} must be within 0.1..2", show(raw.get(key)))
        })?;
        *slot = v.unwrap_or_default();
    }
    need(
        raw.get("reclaimer_fits").is_none_or(|v| *v == Value::Bool(false)),
        || "gameplay reclaimer_fits must be false: no ground structures inside tunnels".to_string(),
    )?;
    Ok(DescGameplay {
        radar_blocked_by_rock: radar,
        ai_cost_scale: scales[0],
        flank_cost_scale: scales[1],
        structure_fits: false,
    })
}

fn validate_crawlway(raw: &Value, index: usize) -> Result<DescCrawlway, DescriptionError> {
    let what = format!("crawlways[{index}]");
    let raw = object(raw, &what)?;
    check_no_extra(
        raw,
        &["id", "kind", "width_m", "height_m", "points", "seed"],
        &what,
    )?;
    let id = check_id(raw.get("id"), &what)?;
    need(
        raw.get("kind").and_then(Value::as_str) == Some(CRAWLWAY_KIND),
        || {
            format!(
                "{what} (id {id:?}) kind {} must be {CRAWLWAY_KIND:?}",
                show(raw.get("kind"))
            )
        },
    )?;
    let width =
        number_or(raw, "width_m", 1.4).filter(|w| (MIN_CRAWLWAY_WIDTH_M..=MAX_CRAWLWAY_WIDTH_M).contains(w));
    need(width.is_some(), || {
        format!(
            "{what} (id {id:?}) width_m {} must be within {MIN_CRAWLWAY_WIDTH_M}..{MAX_CRAWLWAY_WIDTH_M} m",
            show(raw.get("width_m"))
        )
    })?;
    let height = number_or(raw, "height_m", 2.2)
        .filter(|h| (MIN_CRAWLWAY_HEIGHT_M..=MAX_CRAWLWAY_HEIGHT_M).contains(h));
    need(height.is_some(), || {
        format!(
            "{what} (id {id:?}) height_m {} must be within {MIN_CRAWLWAY_HEIGHT_M}..{MAX_CRAWLWAY_HEIGHT_M} m",
            show(raw.get("height_m"))
        )
    })?;
    let pts = points(raw, &what, &id)?;
    let seed = match raw.get("seed") {
        None => Some(0),
        Some(v) => v.as_u64().and_then(|s| u32::try_from(s).ok()),
    };
    need(seed.is_some(), || {
        format!(
            "{what} (id {id:?}) seed {} must be a non-negative integer",
            show(raw.get("seed"))
        )
    })?;
    Ok(DescCrawlway {
        id,
        kind: CRAWLWAY_KIND.to_string(),
        width_m: width.unwrap_or_default(),
        height_m: height.unwrap_or_default(),
        points: pts,
        seed: seed.unwrap_or_default(),
    })
}

fn unique(ids: &[&str]) -> bool {
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    sorted.windows(2).all(|w| w[0] != w[1])
}

/// Validates a description (the JSON value) and returns it with defaults filled.
///
/// # Errors
///
/// [`DescriptionError`] naming the fix.
pub fn validate_description(desc: &Value) -> Result<Description, DescriptionError> {
    let desc = desc
        .as_object()
        .ok_or_else(|| DescriptionError("the tunnel description must be a JSON object".into()))?;
    check_no_extra(
        desc,
        &[
            "passages",
            "openings",
            "junctions",
            "crawlways",
            "sample_step_m",
            "looks",
            "gameplay",
        ],
        "the tunnel description",
    )?;
    let raw_passages = desc.get("passages").and_then(Value::as_array);
    need(raw_passages.is_some_and(|p| (1..=64).contains(&p.len())), || {
        "passages must be a list of 1..64 passages".into()
    })?;
    let mut passages = Vec::new();
    for (i, p) in raw_passages
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        passages.push(validate_passage(p, i)?);
    }
    let ids: Vec<&str> = passages.iter().map(|p| p.id.as_str()).collect();
    need(unique(&ids), || {
        format!("passage ids must be unique; got duplicates in {ids:?}")
    })?;
    let ramp_ids: Vec<&str> = passages
        .iter()
        .filter(|p| p.kind == "ramp")
        .map(|p| p.id.as_str())
        .collect();
    let list = |key: &str, what: &str| -> Result<Vec<Value>, DescriptionError> {
        match desc.get(key) {
            None => Ok(Vec::new()),
            Some(v) => {
                let l = v.as_array().filter(|l| l.len() <= 64);
                need(l.is_some(), || {
                    format!("{key} must be a list of at most 64 {what}")
                })?;
                Ok(l.cloned().unwrap_or_default())
            }
        }
    };
    let mut openings = Vec::new();
    for (i, o) in list("openings", "openings")?.iter().enumerate() {
        openings.push(validate_opening(o, i, &ramp_ids)?);
    }
    let oids: Vec<&str> = openings.iter().map(|o| o.id.as_str()).collect();
    need(unique(&oids), || {
        format!("opening ids must be unique; got duplicates in {oids:?}")
    })?;
    for o in &openings {
        if let Some(ramp) = &o.ramp
            && let Some(r) = passages.iter().find(|p| &p.id == ramp)
            && let Some(last) = r.points.last()
        {
            let d = dist3(o.top, *last);
            need(d <= OPENING_TOP_TOL_M, || {
                format!(
                    "opening {:?} top {:?} is {d:.2} m from the end of ramp {ramp:?}; the top must sit on the ramp end (within 1 m)",
                    o.id, o.top
                )
            })?;
        }
    }
    let mut junctions = Vec::new();
    for (i, raw) in list("junctions", "junctions")?.iter().enumerate() {
        let what = format!("junctions[{i}]");
        let raw = object(raw, &what)?;
        check_no_extra(raw, &["id", "at", "passages"], &what)?;
        let id = check_id(raw.get("id"), &what)?;
        let members: Vec<&str> = raw
            .get("passages")
            .and_then(Value::as_array)
            .map(|l| l.iter().map(|m| m.as_str().unwrap_or("")).collect())
            .unwrap_or_default();
        need(
            raw.get("passages")
                .and_then(Value::as_array)
                .is_some_and(|l| l.len() >= 2),
            || format!("{what} (id {id:?}) must name at least 2 passages"),
        )?;
        need(unique(&members), || {
            format!("{what} (id {id:?}) passages must be unique; got {members:?}")
        })?;
        for m in &members {
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            need(ids.contains(m), || {
                format!("{what} (id {id:?}) names unknown passage {m:?}; passages are {sorted:?}")
            })?;
        }
        let at = triple(raw.get("at"), &format!("{what} (id {id:?}) at"))?;
        junctions.push(DescJunction {
            id,
            at,
            passages: members.iter().map(|m| (*m).to_string()).collect(),
        });
    }
    let jids: Vec<&str> = junctions.iter().map(|j| j.id.as_str()).collect();
    need(unique(&jids), || {
        format!("junction ids must be unique; got duplicates in {jids:?}")
    })?;
    let step =
        number_or(desc, "sample_step_m", 5.0).filter(|s| (MIN_SAMPLE_STEP_M..=MAX_SAMPLE_STEP_M).contains(s));
    need(step.is_some(), || {
        format!(
            "sample_step_m {} must be within {MIN_SAMPLE_STEP_M}..{MAX_SAMPLE_STEP_M} m",
            show(desc.get("sample_step_m"))
        )
    })?;
    let mut crawlways = Vec::new();
    for (i, c) in list("crawlways", "pilot-only volumes")?.iter().enumerate() {
        crawlways.push(validate_crawlway(c, i)?);
    }
    let cids: Vec<&str> = crawlways.iter().map(|c| c.id.as_str()).collect();
    need(unique(&cids), || {
        format!("crawlway ids must be unique; got duplicates in {cids:?}")
    })?;
    for cid in &cids {
        need(!ids.contains(cid) && !jids.contains(cid), || {
            format!("crawlway id {cid:?} collides with a passage or junction id")
        })?;
    }
    let looks = desc.get("looks").map(validate_looks).transpose()?;
    let gameplay = desc.get("gameplay").map(validate_gameplay).transpose()?;
    Ok(Description {
        passages,
        openings,
        junctions,
        crawlways,
        sample_step_m: step.unwrap_or_default(),
        looks,
        gameplay,
    })
}

impl Description {
    /// The description as a Stage A layout (what the pinned sampler and mesher read): `ramp` stays a ramp,
    /// `tube` and `shaft` are tubes, `gallery` and `chamber` galleries (the kind is the look; only a ramp changes
    /// the collision); an opening without a ramp names no passage; crawlways are carried, not meshed.
    #[must_use]
    pub fn to_layout(&self, map_id: &str) -> TunnelLayout {
        let p32 = |p: &[f64; 3]| p.map(|v| v as f32);
        TunnelLayout {
            map_id: map_id.to_string(),
            version: LAYOUT_VERSION,
            sample_step_m: self.sample_step_m as f32,
            passages: self
                .passages
                .iter()
                .map(|p| Passage {
                    id: p.id.clone(),
                    kind: match p.kind.as_str() {
                        "ramp" => PassageKind::Ramp,
                        "gallery" | "chamber" => PassageKind::Gallery,
                        _ => PassageKind::Tube,
                    },
                    width_m: p.width_m as f32,
                    ceiling_m: p.ceiling_m as f32,
                    points: p.points.iter().map(p32).collect(),
                })
                .collect(),
            openings: self
                .openings
                .iter()
                .map(|o| Opening {
                    id: o.id.clone(),
                    kind: o.kind.clone(),
                    ramp: o.ramp.clone().unwrap_or_default(),
                    top: p32(&o.top),
                })
                .collect(),
            junctions: self
                .junctions
                .iter()
                .map(|j| Junction {
                    id: j.id.clone(),
                    at: p32(&j.at),
                    passages: j.passages.clone(),
                })
                .collect(),
            crawlways: self
                .crawlways
                .iter()
                .map(|c| CrawlwayVolume {
                    id: c.id.clone(),
                    points: c.points.iter().map(p32).collect(),
                    width_m: c.width_m as f32,
                    height_m: c.height_m as f32,
                    seed: c.seed,
                })
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------------------------------------------

/// XZ distance from `p` to the segment `a -> b` and the segment parameter.
fn seg_dist_xz(p: [f64; 3], a: [f64; 3], b: [f64; 3]) -> (f64, f64) {
    let (abx, abz) = (b[0] - a[0], b[2] - a[2]);
    let len2 = abx * abx + abz * abz;
    if len2 < 1e-9 {
        return (hypot(p[0] - a[0], p[2] - a[2]), 0.0);
    }
    let t = det_clamp_f64(((p[0] - a[0]) * abx + (p[2] - a[2]) * abz) / len2, 0.0, 1.0);
    (hypot(p[0] - (a[0] + abx * t), p[2] - (a[2] + abz * t)), t)
}

/// Shortest XZ distance from `q` to a sampled centreline, less `half`.
fn footprint_dist(q: [f64; 3], line: &[crate::compat::layout::Sample], half: f64) -> f64 {
    line.windows(2)
        .map(|w| seg_dist_xz(q, f64p(w[0].p), f64p(w[1].p)).0)
        .fold(f64::INFINITY, det_min_f64)
        - half
}

/// Samples a validated description with the pinned Stage A sampler on `ground` (`None`: unknown overburden, every
/// segment an open cut), then applies the description's own sampling checks: no vertical centreline, and every
/// junction on its members.
///
/// # Errors
///
/// [`DescriptionError`] for a vertical centreline or a junction away from a member.
pub fn sample(
    desc: &Description,
    ground: Option<&TerrainSampler>,
) -> Result<SampledLayout, DescriptionError> {
    let mut sampled = desc.to_layout("").sample(ground);
    if ground.is_none() {
        for p in &mut sampled.passages {
            p.covered.iter_mut().for_each(|c| *c = false);
        }
    }
    for p in &sampled.passages {
        let n = p.samples.len();
        for i in 0..n {
            let (a, b) = (p.samples[i.saturating_sub(1)].p, p.samples[(i + 1).min(n - 1)].p);
            need(
                hypot(
                    f64::from(b[0]) - f64::from(a[0]),
                    f64::from(b[2]) - f64::from(a[2]),
                ) > 1e-6,
                || {
                    format!(
                        "passage {:?}: the passage centreline runs vertically; shafts must slant",
                        p.id
                    )
                },
            )?;
        }
    }
    for j in &desc.junctions {
        for m in &j.passages {
            if let (Some(line), Some(spec)) = (sampled.passage(m), desc.passages.iter().find(|p| &p.id == m))
            {
                let d = footprint_dist(j.at, &line.samples, spec.width_m * 0.5);
                need(d <= JUNCTION_AT_TOL_M, || {
                    format!(
                        "junction {:?} at {:?} is {d:.2} m outside passage {m:?}; a junction must sit where its passages meet",
                        j.id, j.at
                    )
                })?;
            }
        }
    }
    Ok(sampled)
}

// ---------------------------------------------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------------------------------------------

/// An indexed triangle mesh, world millimetres.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BakeMesh {
    /// Vertex positions, world millimetres.
    pub vertices: Vec<[i32; 3]>,
    /// Triangles, indices into `vertices`.
    pub faces: Vec<[u32; 3]>,
}

impl BakeMesh {
    fn from_triangles(tris: impl IntoIterator<Item = [[i32; 3]; 3]>) -> Self {
        let mut index: BTreeMap<[i32; 3], u32> = BTreeMap::new();
        let mut mesh = Self::default();
        for t in tris {
            let f = t.map(|p| {
                *index.entry(p).or_insert_with(|| {
                    mesh.vertices.push(p);
                    (mesh.vertices.len() - 1) as u32
                })
            });
            mesh.faces.push(f);
        }
        mesh
    }

    /// An indexed mesh of world-millimetre triangles (vertices in first-use order).
    #[must_use]
    pub fn from_world(tris: &[WorldTriangleMm]) -> Self {
        Self::from_triangles(tris.iter().map(|t| t.v))
    }

    /// The triangles in metres.
    #[must_use]
    pub fn triangles_m(&self) -> Vec<[[f64; 3]; 3]> {
        self.faces
            .iter()
            .filter(|f| f.iter().all(|&i| (i as usize) < self.vertices.len()))
            .map(|f| f.map(|i| self.vertices[i as usize].map(|c| f64::from(c) / 1000.0)))
            .collect()
    }
}

/// Builds `(render, collision)` from the sampled layout: the collision mesh is the pinned Stage A mesher's
/// (quantized to millimetres), the render mesh subdivides each of its faces `subdivision` times
/// ([`crate::render::render_layer`]), so they share every section plane.
///
/// # Errors
///
/// [`DescriptionError`] for a coordinate beyond 2^24 mm of the origin.
pub fn build_meshes(
    desc: &Description,
    sampled: &SampledLayout,
    ground: Option<&TerrainSampler>,
    subdivision: u8,
) -> Result<(BakeMesh, BakeMesh, Vec<WorldTriangleMm>), DescriptionError> {
    let layout = desc.to_layout("");
    let canonical = quantize_mesh(&sampled.triangles(&layout.openings, ground))
        .map_err(|e| DescriptionError(e.to_string()))?;
    let collision = BakeMesh::from_world(&canonical);
    let render = render_layer(
        &RenderLayerInputs {
            layer: 0,
            id: "tunnels",
            material: "",
            uv_tile_m: 4.0,
            subdivision,
            sampled,
            openings: &layout.openings,
        },
        &canonical,
    );
    let render = BakeMesh::from_triangles(render.triangles_mm());
    Ok((render, collision, canonical))
}

/// One mouth cut quad.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouthCut {
    /// The opening id.
    pub opening: String,
    /// The opening kind.
    pub kind: String,
    /// The corners, world millimetres (back left, back right, front right, front left).
    pub corners: [[i32; 3]; 4],
}

/// The portal set piece at one mouth.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouthPortal {
    /// The opening id.
    pub opening: String,
    /// The opening top, world millimetres.
    pub position: [i32; 3],
    /// Yaw of the outward direction, `atan2(out_x, out_z)` in millidegrees.
    pub yaw_mdeg: i32,
    /// The ramp's width, millimetres.
    pub width_mm: i32,
    /// The ramp's ceiling, millimetres.
    pub height_mm: i32,
}

/// The mouth cuts and portals (lane W-10 owns the lattice cut that replaces the quads).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerrainCut {
    /// One per ramp opening.
    pub cuts: Vec<MouthCut>,
    /// One per ramp opening, same order.
    pub portals: Vec<MouthPortal>,
}

/// The portal yaw keeps the source baker's bare libm `atan2f` (signed zeros kept, KI-WD-029): its bytes must
/// match the source game's tool, so it calls `atan2_libm`, not the canonical `math::atan2` (ADR-8876).
fn yaw_mdeg(out: [f64; 2]) -> i32 {
    let rad = f64::from(regia_detsim::math::atan2_libm(out[0] as f32, out[1] as f32));
    (rad.to_degrees() * 1000.0).round_ties_even() as i32
}

/// One cut quad plus one portal per opening on a ramp, from the ramp's sampled end.
///
/// # Errors
///
/// [`DescriptionError`] when an opening's top is off its ramp's sampled end, or the ramp's last two samples share
/// an XZ (no outward direction).
pub fn build_terrain_cut(
    desc: &Description,
    sampled: &SampledLayout,
) -> Result<TerrainCut, DescriptionError> {
    let mut cut = TerrainCut::default();
    for o in &desc.openings {
        let Some(ramp_id) = &o.ramp else {
            continue;
        };
        let (Some(spec), Some(line)) = (
            desc.passages.iter().find(|p| &p.id == ramp_id),
            sampled.passage(ramp_id),
        ) else {
            continue;
        };
        let end = line.samples.last().map(|s| f64p(s.p)).unwrap_or_default();
        need(
            hypot(o.top[0] - end[0], o.top[2] - end[2]) <= OPENING_TOP_TOL_M,
            || format!("opening {:?} top is not on the end of ramp {ramp_id:?}", o.id),
        )?;
        let out = outward(&line.samples).ok_or_else(|| {
            DescriptionError(
                "a ramp's last two samples share the same XZ; it has no outward direction".into(),
            )
        })?;
        let top = o.top.map(|v| v as f32);
        let corners = cut_corners(top, out, spec.width_m as f32);
        cut.cuts.push(MouthCut {
            opening: o.id.clone(),
            kind: o.kind.clone(),
            corners: corners.map(|c| c.map(|v| m_to_mm(v as f32))),
        });
        cut.portals.push(MouthPortal {
            opening: o.id.clone(),
            position: top.map(m_to_mm),
            yaw_mdeg: yaw_mdeg(out),
            width_mm: m_to_mm(spec.width_m as f32),
            height_mm: m_to_mm(spec.ceiling_m as f32),
        });
    }
    Ok(cut)
}

/// One nav node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavNode {
    /// `<passage>:<sample>` or the junction id.
    pub id: String,
    /// The passage (absent on a junction node).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passage: Option<String>,
    /// A junction node.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub junction: bool,
    /// Position, world millimetres.
    pub p: [i32; 3],
    /// Arc length, millimetres.
    pub s_mm: i32,
    /// A pilot-only (crawlway) node.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pilot_only: bool,
}

/// One nav edge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavEdge {
    /// From node id.
    pub a: String,
    /// To node id.
    pub b: String,
    /// Cost: metres times the AI cost scale, in millimetres.
    pub cost_mm: i64,
    /// A pilot-only (crawlway) edge.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pilot_only: bool,
}

/// The centreline nav graph (lanes W-19/W-20 own the navmesh that replaces it).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Nav {
    /// Nodes.
    pub nodes: Vec<NavNode>,
    /// Edges.
    pub edges: Vec<NavEdge>,
    /// The AI cost scale used.
    pub ai_cost_scale: f64,
}

fn mm3(p: [f64; 3]) -> [i32; 3] {
    p.map(|v| m_to_mm(v as f32))
}

fn mm1(v: f64) -> i64 {
    (v * 1000.0).round_ties_even() as i64
}

/// The centreline graph on the collision floor, joined at junctions; crawlways are pilot-only chains of their own
/// points, not joined to the hull graph, costed at their raw length.
#[must_use]
pub fn build_nav(desc: &Description, sampled: &SampledLayout) -> Nav {
    let scale = desc.gameplay.as_ref().map_or(0.75, |g| g.ai_cost_scale);
    let mut nav = Nav {
        ai_cost_scale: scale,
        ..Nav::default()
    };
    for p in &sampled.passages {
        for (i, q) in p.samples.iter().enumerate() {
            nav.nodes.push(NavNode {
                id: format!("{}:{i}", p.id),
                passage: Some(p.id.clone()),
                junction: false,
                p: q.p.map(m_to_mm),
                s_mm: m_to_mm(q.s),
                pilot_only: false,
            });
            if i > 0 {
                nav.edges.push(NavEdge {
                    a: format!("{}:{}", p.id, i - 1),
                    b: format!("{}:{i}", p.id),
                    cost_mm: mm1(dist3(f64p(p.samples[i - 1].p), f64p(q.p)) * scale),
                    pilot_only: false,
                });
            }
        }
    }
    for j in &desc.junctions {
        nav.nodes.push(NavNode {
            id: j.id.clone(),
            passage: None,
            junction: true,
            p: mm3(j.at),
            s_mm: 0,
            pilot_only: false,
        });
        for m in &j.passages {
            let Some(line) = sampled.passage(m) else {
                continue;
            };
            let mut best = (f64::INFINITY, 0usize);
            for (i, q) in line.samples.iter().enumerate() {
                let d = dist3(f64p(q.p), j.at);
                if d < best.0 {
                    best = (d, i);
                }
            }
            nav.edges.push(NavEdge {
                a: j.id.clone(),
                b: format!("{m}:{}", best.1),
                cost_mm: mm1(best.0 * scale),
                pilot_only: false,
            });
        }
    }
    for cw in &desc.crawlways {
        let mut arc = 0.0f64;
        for (i, p) in cw.points.iter().enumerate() {
            if i > 0 {
                let step = dist3(cw.points[i - 1], *p);
                arc += step;
                nav.edges.push(NavEdge {
                    a: format!("{}:{}", cw.id, i - 1),
                    b: format!("{}:{i}", cw.id),
                    cost_mm: mm1(step),
                    pilot_only: true,
                });
            }
            nav.nodes.push(NavNode {
                id: format!("{}:{i}", cw.id),
                passage: Some(cw.id.clone()),
                junction: false,
                p: mm3(*p),
                s_mm: mm1(arc) as i32,
                pilot_only: true,
            });
        }
    }
    nav
}

// ---------------------------------------------------------------------------------------------------------------
// Consistency checks (lane W-09's input; the rules of the baker)
// ---------------------------------------------------------------------------------------------------------------

/// A rule's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    /// The rule holds.
    Pass,
    /// The rule fails.
    Fail,
}

/// One rule's finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// `collision_inside_render`, `nav_on_collision_floor`, `mouth_cut_matches_collision`,
    /// `junctions_join_members` or `render_covers_floor`.
    pub rule: String,
    /// The verdict.
    pub status: FindingStatus,
    /// What was measured.
    pub detail: String,
}

/// The check report.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BakeReport {
    /// One finding per rule.
    pub findings: Vec<Finding>,
    /// Every rule passed.
    pub pass: bool,
}

/// A whole bake.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Baked {
    /// [`FORMAT`].
    pub format: String,
    /// The validated description.
    pub description: Description,
    /// The sampled layout.
    pub sampled: SampledLayout,
    /// The render mesh.
    pub render_mesh: BakeMesh,
    /// The collision mesh.
    pub collision_mesh: BakeMesh,
    /// The mouth cuts and portals.
    pub terrain_cut: TerrainCut,
    /// The centreline nav.
    pub nav: Nav,
    /// The checks (empty until [`finish`]).
    pub report: BakeReport,
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Squared distance from `p` to triangle `t` (the plane projection when inside, else the nearest edge).
#[must_use]
pub fn point_triangle_dist2(p: [f64; 3], t: &[[f64; 3]; 3]) -> f64 {
    let a = t[0];
    let ab = sub(t[1], a);
    let ac = sub(t[2], a);
    let n = cross(ab, ac);
    let denom = dot(n, n);
    let mut best = f64::INFINITY;
    if denom > 1e-24 {
        let s = dot(n, sub(p, a)) / denom;
        let q = [p[0] - n[0] * s, p[1] - n[1] * s, p[2] - n[2] * s];
        let aq = sub(q, a);
        let (d00, d01, d11) = (dot(ab, ab), dot(ab, ac), dot(ac, ac));
        let (d20, d21) = (dot(aq, ab), dot(aq, ac));
        let bd = d00 * d11 - d01 * d01;
        if bd.abs() > 1e-24 {
            let v = (d11 * d20 - d01 * d21) / bd;
            let w = (d00 * d21 - d01 * d20) / bd;
            if v >= 0.0 && w >= 0.0 && v + w <= 1.0 {
                let d = sub(q, p);
                best = dot(d, d);
            }
        }
    }
    for (e0, e1) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
        let e = sub(e1, e0);
        let len2 = dot(e, e);
        let s = if len2 > 1e-24 {
            det_clamp_f64(dot(sub(p, e0), e) / len2, 0.0, 1.0)
        } else {
            0.0
        };
        let c = [e0[0] + e[0] * s, e0[1] + e[1] * s, e0[2] + e[2] * s];
        let d = sub(c, p);
        best = det_min_f64(best, dot(d, d));
    }
    best
}

/// A set of triangles with their boxes, for nearest-distance queries.
struct TriSet {
    tris: Vec<[[f64; 3]; 3]>,
    boxes: Vec<([f64; 3], [f64; 3])>,
}

impl TriSet {
    fn new(tris: Vec<[[f64; 3]; 3]>) -> Self {
        let boxes = tris
            .iter()
            .map(|t| {
                let mut lo = t[0];
                let mut hi = t[0];
                for p in &t[1..] {
                    for k in 0..3 {
                        lo[k] = det_min_f64(lo[k], p[k]);
                        hi[k] = det_max_f64(hi[k], p[k]);
                    }
                }
                (lo, hi)
            })
            .collect();
        Self { tris, boxes }
    }

    /// Shortest distance from `p` to any triangle (infinity for none).
    fn dist(&self, p: [f64; 3]) -> f64 {
        let mut best = f64::INFINITY;
        for (t, (lo, hi)) in self.tris.iter().zip(&self.boxes) {
            let mut box2 = 0.0;
            for k in 0..3 {
                let d = det_max_f64(det_max_f64(lo[k] - p[k], p[k] - hi[k]), 0.0);
                box2 += d * d;
            }
            if box2 >= best {
                continue;
            }
            best = det_min_f64(best, point_triangle_dist2(p, t));
            if best == 0.0 {
                break;
            }
        }
        regia_detsim::math::sqrt_f64(best)
    }
}

/// The collision floor height under `(x, z)`: the nearest passage floor within 4 m of `y` (inside its half width
/// plus 0.5 m), or a junction slab's height.
fn floor_height_at(desc: &Description, sampled: &SampledLayout, q: [f64; 3]) -> Option<f64> {
    let mut best: Option<f64> = None;
    for p in &sampled.passages {
        let half = f64::from(p.width_m) * 0.5;
        for w in p.samples.windows(2) {
            let (a, b) = (f64p(w[0].p), f64p(w[1].p));
            let (d, t) = seg_dist_xz(q, a, b);
            if d <= half + 0.5 {
                let floor = a[1] + (b[1] - a[1]) * t;
                if (q[1] - floor).abs() < 4.0
                    && best.is_none_or(|bf| (q[1] - floor).abs() < (q[1] - bf).abs())
                {
                    best = Some(floor);
                }
            }
        }
    }
    for j in &desc.junctions {
        let extent = j
            .passages
            .iter()
            .filter_map(|m| desc.passages.iter().find(|p| &p.id == m))
            .map(|p| p.width_m * 0.5)
            .fold(f64::NEG_INFINITY, det_max_f64)
            + 1.0;
        if (q[0] - j.at[0]).abs() <= extent
            && (q[2] - j.at[2]).abs() <= extent
            && best.is_none_or(|bf| (q[1] - j.at[1]).abs() < (q[1] - bf).abs())
        {
            best = Some(j.at[1]);
        }
    }
    best
}

fn record(findings: &mut Vec<Finding>, rule: &str, passed: bool, detail: String) {
    findings.push(Finding {
        rule: rule.to_string(),
        status: if passed {
            FindingStatus::Pass
        } else {
            FindingStatus::Fail
        },
        detail,
    });
}

/// Cross-checks every output against every other; one finding per rule.
#[must_use]
pub fn check_consistency(baked: &Baked) -> Vec<Finding> {
    let _span = tracing::info_span!("tunnel_bake_check").entered();
    let desc = &baked.description;
    let sampled = &baked.sampled;
    let render = TriSet::new(baked.render_mesh.triangles_m());
    let mut findings = Vec::new();

    // 1. Collision inside render within tolerance.
    let (mut worst, mut worst_i) = (0.0f64, -1i64);
    for (i, v) in baked.collision_mesh.vertices.iter().enumerate() {
        let d = render.dist(v.map(|c| f64::from(c) / 1000.0));
        if d > worst {
            (worst, worst_i) = (d, i as i64);
        }
    }
    record(
        &mut findings,
        "collision_inside_render",
        worst <= COLLISION_IN_RENDER_TOL_M,
        format!(
            "worst collision vertex {worst_i} is {worst:.4} m from the render mesh (tolerance {COLLISION_IN_RENDER_TOL_M} m)"
        ),
    );

    // 2. Nav nodes sit on the collision floor (crawlway nodes are not on it).
    let (mut bad, mut worst_nav) = (0usize, 0.0f64);
    for node in baked.nav.nodes.iter().filter(|n| !n.pilot_only) {
        let p = node.p.map(|c| f64::from(c) / 1000.0);
        let floor = floor_height_at(desc, sampled, p);
        let gap = floor.map_or(f64::INFINITY, |f| (p[1] - f).abs());
        worst_nav = det_max_f64(worst_nav, if gap.is_finite() { gap } else { 1e9 });
        if floor.is_none() || gap > NAV_ON_FLOOR_TOL_M {
            bad += 1;
        }
    }
    record(
        &mut findings,
        "nav_on_collision_floor",
        bad == 0,
        format!(
            "{bad} of {} nav nodes off the collision floor (worst gap {worst_nav:.3} m, tolerance {NAV_ON_FLOOR_TOL_M} m)",
            baked.nav.nodes.len()
        ),
    );

    // 3. Mouth cuts match the collision ends they open: the cut quad in the ramp-end frame, the ramp-end floor
    // inside it, the portal facing outward with the ramp's section size, and no cap on the ramp end.
    let mut bad_mouths: Vec<String> = Vec::new();
    for (cut, portal) in baked.terrain_cut.cuts.iter().zip(&baked.terrain_cut.portals) {
        let opening = desc.openings.iter().find(|o| o.id == cut.opening);
        let ramp = opening
            .and_then(|o| o.ramp.as_ref())
            .and_then(|r| Some((desc.passages.iter().find(|p| &p.id == r)?, sampled.passage(r)?)));
        let (Some(o), Some((spec, line))) = (opening, ramp) else {
            bad_mouths.push(cut.opening.clone());
            continue;
        };
        let mut ok = portal.opening == o.id;
        let out = outward(&line.samples);
        ok &= out.is_some();
        let out = out.unwrap_or([0.0, 0.0]);
        let lat = [-out[1], out[0]];
        let top = o.top;
        let half = spec.width_m * 0.5 + f64::from(MOUTH_LIP_M);
        let back = f64::from(PORTAL_LENGTH_M) + f64::from(MOUTH_LIP_M);
        let fwd = f64::from(MOUTH_LIP_M);
        let top_mm = mm3(top);
        let frame = |x: f64, z: f64| {
            let (dx, dz) = (x - top[0], z - top[2]);
            (dx * out[0] + dz * out[1], dx * lat[0] + dz * lat[1])
        };
        let mut want = vec![(-back, half), (-back, -half), (fwd, -half), (fwd, half)];
        for c in &cut.corners {
            ok &= c[1] == top_mm[1];
            let (along, across) = frame(f64::from(c[0]) / 1000.0, f64::from(c[2]) / 1000.0);
            match want
                .iter()
                .position(|(wa, wc)| (along - wa).abs() <= 0.002 && (across - wc).abs() <= 0.002)
            {
                Some(i) => {
                    want.swap_remove(i);
                }
                None => ok = false,
            }
        }
        ok &= want.is_empty();
        if let Some(last) = line.samples.last() {
            let end = f64p(last.p);
            let (lx, lz) = (f64::from(last.left[0]), f64::from(last.left[1]));
            for side in [-1.0, 1.0] {
                let r = half - f64::from(MOUTH_LIP_M);
                let (along, across) = frame(end[0] + lx * side * r, end[2] + lz * side * r);
                ok &= (-back - OPENING_TOP_TOL_M..=fwd + OPENING_TOP_TOL_M).contains(&along);
                ok &= (-half - OPENING_TOP_TOL_M..=half + OPENING_TOP_TOL_M).contains(&across);
            }
        }
        ok &= portal.position == top_mm;
        ok &= portal.yaw_mdeg == yaw_mdeg(out);
        ok &= portal.width_mm == m_to_mm(spec.width_m as f32);
        ok &= portal.height_mm == m_to_mm(spec.ceiling_m as f32);
        if !ok || line.cap_end {
            bad_mouths.push(o.id.clone());
        }
    }
    record(
        &mut findings,
        "mouth_cut_matches_collision",
        bad_mouths.is_empty(),
        format!(
            "mouths disagreeing with their ramp ends: {}",
            if bad_mouths.is_empty() {
                "none".to_string()
            } else {
                format!("{bad_mouths:?}")
            }
        ),
    );

    // 4. Junctions join their members (geometry near, nav linked).
    let mut bad_j: Vec<String> = Vec::new();
    for j in &desc.junctions {
        let mut ok = baked.nav.nodes.iter().any(|n| n.id == j.id);
        for m in &j.passages {
            let near = sampled.passage(m).map_or(f64::INFINITY, |line| {
                line.samples
                    .iter()
                    .map(|q| dist3(f64p(q.p), j.at))
                    .fold(f64::INFINITY, det_min_f64)
            });
            let width = desc
                .passages
                .iter()
                .find(|p| &p.id == m)
                .map_or(0.0, |p| p.width_m);
            ok &= near <= width * 0.5 + JUNCTION_AT_TOL_M + 5.0;
            let prefix = format!("{m}:");
            ok &= baked
                .nav
                .edges
                .iter()
                .any(|e| e.a == j.id && e.b.starts_with(&prefix));
        }
        if !ok {
            bad_j.push(j.id.clone());
        }
    }
    record(
        &mut findings,
        "junctions_join_members",
        bad_j.is_empty(),
        format!(
            "junctions failing the join check: {}",
            if bad_j.is_empty() {
                "none".to_string()
            } else {
                format!("{bad_j:?}")
            }
        ),
    );

    // 5. The render mesh covers every sampled floor rect.
    let mut worst_floor = 0.0f64;
    for p in &sampled.passages {
        let half = f64::from(p.width_m) * 0.5 - 0.25;
        for q in &p.samples {
            for side in [-1.0, 0.0, 1.0] {
                let probe = [
                    f64::from(q.p[0]) + f64::from(q.left[0]) * half * side,
                    f64::from(q.p[1]),
                    f64::from(q.p[2]) + f64::from(q.left[1]) * half * side,
                ];
                worst_floor = det_max_f64(worst_floor, render.dist(probe));
            }
        }
    }
    record(
        &mut findings,
        "render_covers_floor",
        worst_floor <= RENDER_FLOOR_TOL_M,
        format!(
            "worst floor probe {worst_floor:.4} m from the render mesh (tolerance {RENDER_FLOOR_TOL_M} m)"
        ),
    );
    findings
}

// ---------------------------------------------------------------------------------------------------------------
// The bake
// ---------------------------------------------------------------------------------------------------------------

/// Validates, samples and builds every output, without the checks (`report` empty). [`finish`] checks it.
///
/// # Errors
///
/// [`TunnelBakeError::Description`].
pub fn bake_unchecked(desc: &Value, ground: Option<&TerrainSampler>) -> Result<Baked, TunnelBakeError> {
    let _span = tracing::info_span!("tunnel_bake").entered();
    let description = validate_description(desc)?;
    let sampled = sample(&description, ground)?;
    let (render_mesh, collision_mesh, _) = build_meshes(&description, &sampled, ground, BAKE_SUBDIVISION)?;
    let terrain_cut = build_terrain_cut(&description, &sampled)?;
    let nav = build_nav(&description, &sampled);
    Ok(Baked {
        format: FORMAT.to_string(),
        description,
        sampled,
        render_mesh,
        collision_mesh,
        terrain_cut,
        nav,
        report: BakeReport::default(),
    })
}

/// Runs the checks into `baked.report`; with `strict`, fails when any rule fails.
///
/// # Errors
///
/// [`TunnelBakeError::Disagrees`] (strict only), carrying every finding.
pub fn finish(mut baked: Baked, strict: bool) -> Result<Baked, TunnelBakeError> {
    let findings = check_consistency(&baked);
    let pass = findings.iter().all(|f| f.status == FindingStatus::Pass);
    baked.report = BakeReport { findings, pass };
    if strict && !pass {
        let bad: Vec<String> = baked
            .report
            .findings
            .iter()
            .filter(|f| f.status != FindingStatus::Pass)
            .map(|f| format!("{}: {}", f.rule, f.detail))
            .collect();
        return Err(TunnelBakeError::Disagrees {
            message: format!("the tunnel bake disagrees with itself: {}", bad.join("; ")),
            findings: baked.report.findings,
        });
    }
    Ok(baked)
}

/// The one bake: description in, render + collision + mouth cut + nav out, checked.
///
/// # Errors
///
/// [`TunnelBakeError::Description`] for a bad description; [`TunnelBakeError::Disagrees`] when the outputs disagree
/// and `strict` is set.
pub fn bake(desc: &Value, ground: Option<&TerrainSampler>, strict: bool) -> Result<Baked, TunnelBakeError> {
    finish(bake_unchecked(desc, ground)?, strict)
}

/// Deterministic bytes for a bake: JSON in field order, integer millimetres, one trailing newline.
#[must_use]
pub fn canonical_bytes(baked: &Baked) -> Vec<u8> {
    let mut out = serde_json::to_vec(baked).unwrap_or_default();
    out.push(b'\n');
    out
}

/// The files [`bake_to_dir`] writes, in order.
pub const FILES: [&str; 7] = [
    "description.json",
    "sampled.json",
    "render_mesh.json",
    "collision_mesh.json",
    "terrain_cut.json",
    "nav.json",
    "report.json",
];

/// Why [`bake_to_dir`] failed.
#[derive(Debug, thiserror::Error)]
pub enum BakeDirError {
    /// The bake.
    #[error(transparent)]
    Bake(#[from] TunnelBakeError),
    /// (`io`) a file could not be written.
    #[error("{path}: {source}")]
    Io {
        /// The path.
        path: PathBuf,
        /// The error.
        source: std::io::Error,
    },
}

/// Bakes (strict) and writes one canonical JSON file per output ([`FILES`]), each through a temporary file and a
/// rename. Returns the paths.
///
/// # Errors
///
/// [`BakeDirError`].
pub fn bake_to_dir(
    desc: &Value,
    dir: &Path,
    ground: Option<&TerrainSampler>,
) -> Result<Vec<PathBuf>, BakeDirError> {
    let baked = bake(desc, ground, true)?;
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| BakeDirError::Io { path, source }
    };
    std::fs::create_dir_all(dir).map_err(io(dir))?;
    let parts: [Result<Vec<u8>, serde_json::Error>; 7] = [
        serde_json::to_vec(&baked.description),
        serde_json::to_vec(&baked.sampled),
        serde_json::to_vec(&baked.render_mesh),
        serde_json::to_vec(&baked.collision_mesh),
        serde_json::to_vec(&baked.terrain_cut),
        serde_json::to_vec(&baked.nav),
        serde_json::to_vec(&baked.report),
    ];
    let mut written = Vec::with_capacity(FILES.len());
    for (name, bytes) in FILES.iter().zip(parts) {
        let path = dir.join(name);
        let mut bytes = bytes.map_err(|e| BakeDirError::Io {
            path: path.clone(),
            source: std::io::Error::other(e),
        })?;
        bytes.push(b'\n');
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, &bytes).map_err(io(&tmp))?;
        std::fs::rename(&tmp, &path).map_err(io(&path))?;
        written.push(path);
    }
    Ok(written)
}
