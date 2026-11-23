// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/map_pack.rs, commit d6ed37677), ADR-8012.
// Lines 26-35, 58-99, 101-121, 255-263, 529-617, 973-1153, 1179-1208, 1282-1288, 1367-1423, mark: as-is.
// Portions copied from Direct Order 27 (ir-core/src/core.rs, commit d6ed37677), ADR-8012.
// Lines 2417-2431 (`load_tunnels`), mark: as-is (the match); the sim state it built is W-02's, so the
// two arms are the caller's closures here (spec 121 § 4.2 seam: the map registry is the host's).
// Stage A (spec 121 § 4, ADR-8050): arithmetic unchanged. Edits: module paths, doc comments (DO27
// file and engine names dropped, missing docs added), lint attributes. `tunnel_mouth_issues` reads a
// `MouthCheckMap` (the four fields it used of DO27's loaded map package) instead of the package.
// Regia addition (ADR-8166, review rev-b L6): `check_tunnels_bounds` and the `MAX_TUNNEL_*` bounds, called at
// the end of `parse_tunnels`; validation only, no value of a file the bake wrote moves.

//! The baked tunnel file (`do27-tunnels/1`), its loader and the mouth checks against a map package.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::compat::layout::{MouthHole, SampledLayout, TunnelLayout};
use crate::compat::sampler::TerrainSampler;

/// Stamped ground (`ground_y_m`, piece `position_m` y) must sit this close
/// to the heightfield. A few centimetres: one raw count is 1/32 m.
pub const GROUND_TOLERANCE_M: f32 = 0.05;

/// A tunnel mouth top is the ramp's last control point. The sim treats it as
/// on the ground within a metre (`every_opening_is_within_the_hover_limit_and_on_open_ground`).
pub const MOUTH_GROUND_TOLERANCE_M: f32 = 1.0;

/// Points this far past the heightfield half-extent are still on the border.
pub const MAP_EDGE_TOLERANCE_M: f32 = 0.05;

const TUNNELS_FORMAT: &str = "do27-tunnels/1";

/// One failed check against a loaded map. `object_id` is the layout id
/// (`west`, `team_start:1`, `scrap_pool:0:piece:3`) or the manifest path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapIssue {
    /// The map the issue is on.
    pub map_id: String,
    /// The check that failed (`tunnel_mouth`, `inside_map`, `on_heightfield`, `unique_id`).
    pub check: &'static str,
    /// The object the issue names.
    pub object_id: String,
    /// What is wrong.
    pub detail: String,
}

impl std::fmt::Display for MapIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} {}: {}",
            self.map_id, self.check, self.object_id, self.detail
        )
    }
}

impl MapIssue {
    fn new(
        map_id: &str,
        check: &'static str,
        object_id: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            map_id: map_id.to_string(),
            check,
            object_id: object_id.into(),
            detail: detail.into(),
        }
    }
}

/// `assets_regia/maps/<map_id>`.
pub fn map_dir(root: &Path, map_id: &str) -> PathBuf {
    root.join("assets_regia/maps").join(map_id)
}

/// Parse JSON. Truncation and `NaN` / `Infinity` outside strings are errors
/// with those words in the message. Never panics.
pub fn parse_map_json(text: &str) -> Result<serde_json::Value, String> {
    if let Some(bad) = find_non_finite(text) {
        return Err(bad);
    }
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => Ok(value),
        Err(err) => {
            if err.is_eof() {
                Err(format!("truncated JSON: {err}"))
            } else {
                Err(format!("not valid JSON: {err}"))
            }
        }
    }
}

fn parse_named(file: &str, text: &str) -> Result<serde_json::Value, String> {
    parse_map_json(text).map_err(|err| format!("{file}: {err}"))
}

/// One `tunnel_openings` row from `layout.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutOpening {
    /// Opening id.
    pub id: String,
    /// Opening look (`skylight_rubble`, `mine_adit`, ...).
    pub kind: String,
    /// The ramp passage that comes up here.
    pub ramp: String,
    /// Centre of the ramp's top edge (sim metres).
    pub top: [f32; 3],
    /// Ground height the package stamped at the top, metres.
    pub ground_y: f32,
}

/// `tunnels.json` (`do27-tunnels/1`). `holes` is the baked T2 list when the
/// export carries one; older files omit it and the loader bakes the list
/// from `layout`.
#[derive(Debug, Clone, PartialEq)]
pub struct TunnelsFile {
    /// The description.
    pub layout: TunnelLayout,
    /// The sampled centrelines the sim's collision mesh and nav overlay are
    /// built from (`do27-tunnels/1` `sampled`). Loaded (not just validated) so
    /// the sim can arm its tunnels straight from the baked file instead of
    /// re-sampling the splines (T1 "one bake").
    pub sampled: SampledLayout,
    /// The baked hole list, when the file carries one.
    pub holes: Option<Vec<MouthHole>>,
    /// The triangle count the export recorded, when present.
    pub triangle_count: Option<u64>,
}

impl TunnelsFile {
    /// The sim's collision mesh from the baked file's sampled block and
    /// openings. Byte-identical to `TunnelLayout::mesh` on the same terrain
    /// (the one-bake guarantee; `switchbacks_baked_file_builds_the_byte_identical_sim`).
    pub fn collision_mesh(&self, terrain: &TerrainSampler) -> crate::compat::mesh::TunnelCollisionMesh {
        self.sampled.mesh(&self.layout.openings, Some(terrain))
    }
}

/// Parse a baked tunnel file (`do27-tunnels/1`): the description and the sampled block. Errors name the field.
pub fn parse_tunnels(text: &str) -> Result<TunnelsFile, String> {
    let value = parse_named("tunnels.json", text)?;
    let format = req_str(&value, "format", "tunnels.json")?;
    if format != TUNNELS_FORMAT {
        return Err(format!("tunnels.json format `{format}` is not {TUNNELS_FORMAT}"));
    }
    let layout_value = value.get("layout").ok_or("tunnels.json missing layout")?;
    let layout: TunnelLayout =
        serde_json::from_value(layout_value.clone()).map_err(|err| format!("tunnels.json layout: {err}"))?;
    if layout.map_id.is_empty() {
        return Err("tunnels.json layout.map_id is empty".into());
    }
    if layout.passages.is_empty() {
        return Err("tunnels.json layout has no passages".into());
    }
    // The sampled block is the one-bake description of the sim's collision
    // mesh: it is loaded (not just validated) so the sim can arm its tunnels
    // straight from the baked file. A truncated or drifted sample fails here.
    let sampled: SampledLayout = match value.get("sampled") {
        Some(raw) => {
            serde_json::from_value(raw.clone()).map_err(|err| format!("tunnels.json sampled: {err}"))?
        }
        None => return Err("tunnels.json missing sampled".into()),
    };
    let holes = match value.get("holes") {
        None | Some(serde_json::Value::Null) => None,
        Some(raw) => Some(
            serde_json::from_value::<Vec<MouthHole>>(raw.clone())
                .map_err(|err| format!("tunnels.json holes: {err}"))?,
        ),
    };
    let triangle_count = match value.get("triangle_count") {
        None | Some(serde_json::Value::Null) => None,
        Some(raw) => Some(raw.as_u64().ok_or("tunnels.json triangle_count is not a count")?),
    };
    let file = TunnelsFile {
        layout,
        sampled,
        holes,
        triangle_count,
    };
    check_tunnels_bounds(&file)?;
    Ok(file)
}

/// Every number in a baked tunnel file is finite and at most this large in magnitude (review rev-b L6).
/// JSON numbers past `f32::MAX` narrow to an infinity on load, so they fail here too.
pub const MAX_TUNNEL_NUMBER: f32 = 1.0e6;

/// Every position in a baked tunnel file (control points, mouth tops, junctions, centreline samples, hole
/// centres) lies within this many metres of the map origin on each axis. Maps are centred on the origin
/// (`TerrainSampler`); the largest shipped map is 2.56 km wide. This bounds the collision mesh's XZ bucket
/// grid at about a million buckets.
pub const MAX_TUNNEL_HALF_EXTENT_M: f32 = 8192.0;

/// A passage or crawlway width, ceiling or clear height in a baked tunnel file is in `0..=` this (m).
pub const MAX_PASSAGE_SIZE_M: f32 = 256.0;

/// Centreline samples in a baked tunnel file, at most: the `sampled` block, and the samples the
/// description would give if a host re-samples it (all passages together).
pub const MAX_TUNNEL_SAMPLES: usize = 1 << 17;

/// Spline control points in a baked tunnel file's description, at most (passages and crawlways together).
pub const MAX_TUNNEL_CONTROL_POINTS: usize = 1 << 14;

/// Passages, openings, junctions, crawlways and holes in a baked tunnel file, at most (together).
pub const MAX_TUNNEL_ITEMS: usize = 4096;

/// The `triangle_count` a baked tunnel file may record, at most.
pub const MAX_TUNNEL_TRIANGLES: u64 = 1 << 22;

/// A side normal component (`Sample::left`) is a unit-vector component: at most 1 plus rounding.
const MAX_UNIT_COMPONENT: f32 = 1.0 + 1.0e-3;

/// Review rev-b L6: the bounds a baked file must meet before a host builds a mesh from it. Validation
/// only: a file the bake wrote passes unchanged and no value is moved.
fn check_tunnels_bounds(file: &TunnelsFile) -> Result<(), String> {
    let layout = &file.layout;
    let items = layout.passages.len()
        + layout.openings.len()
        + layout.junctions.len()
        + layout.crawlways.len()
        + file.holes.as_ref().map_or(0, Vec::len);
    if items > MAX_TUNNEL_ITEMS {
        return Err(format!(
            "tunnels.json has {items} passages, openings, junctions, crawlways and holes (at most {MAX_TUNNEL_ITEMS})"
        ));
    }
    let control_points: usize = layout.passages.iter().map(|p| p.points.len()).sum::<usize>()
        + layout.crawlways.iter().map(|c| c.points.len()).sum::<usize>();
    if control_points > MAX_TUNNEL_CONTROL_POINTS {
        return Err(format!(
            "tunnels.json layout has {control_points} control points (at most {MAX_TUNNEL_CONTROL_POINTS})"
        ));
    }
    let samples: usize = file.sampled.passages.iter().map(|p| p.samples.len()).sum();
    if samples > MAX_TUNNEL_SAMPLES {
        return Err(format!(
            "tunnels.json sampled samples: {samples} (at most {MAX_TUNNEL_SAMPLES})"
        ));
    }
    if let Some(count) = file.triangle_count
        && count > MAX_TUNNEL_TRIANGLES
    {
        return Err(format!(
            "tunnels.json triangle_count {count} is over {MAX_TUNNEL_TRIANGLES}"
        ));
    }
    number("layout.sample_step_m", layout.sample_step_m)?;
    for (i, p) in layout.passages.iter().enumerate() {
        size(&format!("layout.passages[{i}].width_m"), p.width_m)?;
        size(&format!("layout.passages[{i}].ceiling_m"), p.ceiling_m)?;
        for (k, q) in p.points.iter().enumerate() {
            position(&format!("layout.passages[{i}].points[{k}]"), q)?;
        }
    }
    for (i, o) in layout.openings.iter().enumerate() {
        position(&format!("layout.openings[{i}].top"), &o.top)?;
    }
    for (i, j) in layout.junctions.iter().enumerate() {
        position(&format!("layout.junctions[{i}].at"), &j.at)?;
    }
    for (i, c) in layout.crawlways.iter().enumerate() {
        size(&format!("layout.crawlways[{i}].width_m"), c.width_m)?;
        size(&format!("layout.crawlways[{i}].height_m"), c.height_m)?;
        for (k, q) in c.points.iter().enumerate() {
            position(&format!("layout.crawlways[{i}].points[{k}]"), q)?;
        }
    }
    // A host may re-sample the description (`TunnelLayout::sample`); bound the samples that would give.
    let resampled: usize = layout
        .passages
        .iter()
        .map(|p| crate::compat::layout::centreline_sample_bound(&p.points, layout.sample_step_m))
        .fold(0usize, usize::saturating_add);
    if resampled > MAX_TUNNEL_SAMPLES {
        return Err(format!(
            "tunnels.json layout passages would sample to about {resampled} points (at most {MAX_TUNNEL_SAMPLES})"
        ));
    }
    for (i, p) in file.sampled.passages.iter().enumerate() {
        size(&format!("sampled.passages[{i}].width_m"), p.width_m)?;
        size(&format!("sampled.passages[{i}].ceiling_m"), p.ceiling_m)?;
        for (k, q) in p.samples.iter().enumerate() {
            position(&format!("sampled.passages[{i}].samples[{k}].p"), &q.p)?;
            for c in q.left {
                if !c.is_finite() || c.abs() > MAX_UNIT_COMPONENT {
                    return Err(format!(
                        "tunnels.json sampled.passages[{i}].samples[{k}].left: {c} is not a unit-vector component"
                    ));
                }
            }
            number(&format!("sampled.passages[{i}].samples[{k}].s"), q.s)?;
        }
        // The mesher reads one flag per segment from each list.
        let segments = p.samples.len().saturating_sub(1);
        for (name, len) in [
            ("covered", p.covered.len()),
            ("wall_left", p.wall_left.len()),
            ("wall_right", p.wall_right.len()),
        ] {
            if len != segments {
                return Err(format!(
                    "tunnels.json sampled.passages[{i}].{name} has {len} entries for {segments} segments"
                ));
            }
        }
    }
    for (i, h) in file.holes.iter().flatten().enumerate() {
        for (k, &c) in h.center.iter().enumerate() {
            let field = format!("holes[{i}].center[{k}]");
            number(&field, c)?;
            if c.abs() > MAX_TUNNEL_HALF_EXTENT_M {
                return Err(format!(
                    "tunnels.json {field}: {c} m is outside the map (more than {MAX_TUNNEL_HALF_EXTENT_M} m from the origin)"
                ));
            }
        }
        number(&format!("holes[{i}].half[0]"), h.half[0])?;
        number(&format!("holes[{i}].half[1]"), h.half[1])?;
        number(&format!("holes[{i}].yaw"), h.yaw)?;
    }
    Ok(())
}

/// A finite number of at most [`MAX_TUNNEL_NUMBER`] magnitude.
fn number(field: &str, v: f32) -> Result<(), String> {
    if v.is_finite() && v.abs() <= MAX_TUNNEL_NUMBER {
        Ok(())
    } else {
        Err(format!(
            "tunnels.json {field}: {v} is not a finite number of at most {MAX_TUNNEL_NUMBER}"
        ))
    }
}

/// A size in `0..=MAX_PASSAGE_SIZE_M`.
fn size(field: &str, v: f32) -> Result<(), String> {
    if v.is_finite() && (0.0..=MAX_PASSAGE_SIZE_M).contains(&v) {
        Ok(())
    } else {
        Err(format!(
            "tunnels.json {field}: {v} m is not a size in 0..={MAX_PASSAGE_SIZE_M} m"
        ))
    }
}

/// A position within [`MAX_TUNNEL_HALF_EXTENT_M`] of the origin on each axis.
fn position(field: &str, p: &[f32; 3]) -> Result<(), String> {
    for &c in p {
        number(field, c)?;
        if c.abs() > MAX_TUNNEL_HALF_EXTENT_M {
            return Err(format!(
                "tunnels.json {field}: {c} m is outside the map (more than {MAX_TUNNEL_HALF_EXTENT_M} m from the origin)"
            ));
        }
    }
    Ok(())
}

/// Read and parse the one baked tunnel file for `map_id` under `root`
/// (`assets_regia/maps/<map_id>/tunnels.json`, `do27-tunnels/1`). `None`
/// when the map has no baked file; a file that is present but unreadable or
/// drifted is logged and treated as absent so the caller falls back to the
/// spline description (T1 one-bake: the baked file is the runtime's source,
/// the splines are the authoring source).
pub fn load_tunnels_file(root: &Path, map_id: &str) -> Option<TunnelsFile> {
    let path = map_dir(root, map_id).join("tunnels.json");
    let text = std::fs::read_to_string(&path).ok()?;
    match parse_tunnels(&text) {
        Ok(file) => Some(file),
        Err(err) => {
            tracing::warn!("tunnels: {} ignored ({err})", path.display());
            None
        }
    }
}

/// Tunnel mouths (layout openings and `tunnels.json` openings) must sit in
/// the T2 hole list baked from that description. A file that already carries
/// `holes` must match the bake.
pub fn tunnel_mouth_issues(map: &MouthCheckMap) -> Vec<MapIssue> {
    let mut issues = Vec::new();
    let map_id = map.map_id.as_str();
    let Some(tunnels) = map.tunnels.as_ref() else {
        for opening in &map.layout.openings {
            issues.push(MapIssue::new(
                map_id,
                "tunnel_mouth",
                opening.id.clone(),
                "layout lists a mouth but tunnels.json is missing",
            ));
        }
        return issues;
    };
    flag_duplicate_ids(
        map_id,
        "passage",
        tunnels.layout.passages.iter().map(|p| p.id.as_str()),
        &mut issues,
    );
    flag_duplicate_ids(
        map_id,
        "opening",
        tunnels.layout.openings.iter().map(|o| o.id.as_str()),
        &mut issues,
    );
    flag_duplicate_ids(
        map_id,
        "junction",
        tunnels.layout.junctions.iter().map(|j| j.id.as_str()),
        &mut issues,
    );
    for passage in &tunnels.layout.passages {
        for (i, p) in passage.points.iter().enumerate() {
            if p[0].abs() > map.terrain.half_x + MAP_EDGE_TOLERANCE_M
                || p[2].abs() > map.terrain.half_z + MAP_EDGE_TOLERANCE_M
            {
                issues.push(MapIssue::new(
                    map_id,
                    "inside_map",
                    passage.id.clone(),
                    format!("control point {i} ({}, {}) is outside the map", p[0], p[2]),
                ));
            }
        }
    }
    let holes = baked_holes(tunnels, &map.terrain);
    for opening in &tunnels.layout.openings {
        let own: Vec<&MouthHole> = holes.iter().filter(|h| h.opening == opening.id).collect();
        if own.is_empty() {
            issues.push(MapIssue::new(
                map_id,
                "tunnel_mouth",
                opening.id.clone(),
                "mouth has no T2 hole",
            ));
            continue;
        }
        if !own.iter().any(|h| h.contains(opening.top[0], opening.top[2])) {
            issues.push(MapIssue::new(
                map_id,
                "tunnel_mouth",
                opening.id.clone(),
                format!(
                    "mouth top ({}, {}) is outside every T2 hole for this opening ({} holes)",
                    opening.top[0],
                    opening.top[2],
                    own.len()
                ),
            ));
        }
        let ground = map.terrain.grid_height_at_world(opening.top[0], opening.top[2]);
        let delta = (ground - opening.top[1]).abs();
        if delta > MOUTH_GROUND_TOLERANCE_M {
            issues.push(MapIssue::new(
                map_id,
                "on_heightfield",
                opening.id.clone(),
                format!(
                    "mouth top y {} m is {delta} m off the heightfield ({ground} m)",
                    opening.top[1]
                ),
            ));
        }
        match map.layout.openings.iter().find(|row| row.id == opening.id) {
            None => issues.push(MapIssue::new(
                map_id,
                "tunnel_mouth",
                opening.id.clone(),
                "mouth is missing from layout.json tunnel_openings",
            )),
            Some(row) => {
                let dx = row.top[0] - opening.top[0];
                let dz = row.top[2] - opening.top[2];
                let dy = row.top[1] - opening.top[1];
                if dx.abs() > GROUND_TOLERANCE_M
                    || dz.abs() > GROUND_TOLERANCE_M
                    || dy.abs() > GROUND_TOLERANCE_M
                {
                    issues.push(MapIssue::new(
                        map_id,
                        "tunnel_mouth",
                        opening.id.clone(),
                        format!(
                            "layout.json top {:?} does not match tunnels.json mouth {:?}",
                            row.top, opening.top
                        ),
                    ));
                }
            }
        }
    }
    for row in &map.layout.openings {
        if !tunnels.layout.openings.iter().any(|o| o.id == row.id) {
            issues.push(MapIssue::new(
                map_id,
                "tunnel_mouth",
                row.id.clone(),
                "layout.json opening has no tunnels.json mouth",
            ));
        }
    }
    for hole in &holes {
        if !tunnels.layout.openings.iter().any(|o| o.id == hole.opening) {
            issues.push(MapIssue::new(
                map_id,
                "tunnel_mouth",
                hole.opening.clone(),
                "T2 hole names a mouth that is not in the layout",
            ));
        }
    }
    if let Some(file_holes) = &tunnels.holes {
        if file_holes.len() != holes.len() {
            issues.push(MapIssue::new(
                map_id,
                "tunnel_mouth",
                "tunnels.json",
                format!(
                    "T2 holes list has {} shapes, the bake has {}",
                    file_holes.len(),
                    holes.len()
                ),
            ));
        }
        for (i, file_hole) in file_holes.iter().enumerate() {
            let matched = holes.iter().any(|baked| holes_close(baked, file_hole));
            if !matched {
                let id = if file_hole.opening.is_empty() {
                    format!("holes[{i}]")
                } else {
                    file_hole.opening.clone()
                };
                issues.push(MapIssue::new(
                    map_id,
                    "tunnel_mouth",
                    id,
                    format!(
                        "T2 holes list shape {i} at {:?} half {:?} does not match the bake",
                        file_hole.center, file_hole.half
                    ),
                ));
            }
        }
    }
    issues
}

/// The parts of a loaded map package that [`tunnel_mouth_issues`] reads. Stage A seam: DO27 passed its
/// whole loaded package; the package format is the host's.
#[derive(Debug, Clone)]
pub struct MouthCheckMap {
    /// The map id the issues name.
    pub map_id: String,
    /// The package's own list of tunnel mouths.
    pub layout: MouthCheckLayout,
    /// The map's terrain, for the on-heightfield and hole checks.
    pub terrain: TerrainSampler,
    /// The baked tunnel file, when the package has one.
    pub tunnels: Option<TunnelsFile>,
}

/// The package's tunnel-mouth rows (DO27's `layout.json` `tunnel_openings`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MouthCheckLayout {
    /// One row per mouth.
    pub openings: Vec<LayoutOpening>,
}

/// The T2 hole list for this description, baked against the heightfield grid.
pub fn baked_holes(tunnels: &TunnelsFile, terrain: &TerrainSampler) -> Vec<MouthHole> {
    let sampled = tunnels.layout.sample(Some(terrain));
    sampled.mouth_holes(&tunnels.layout.openings)
}

fn holes_close(a: &MouthHole, b: &MouthHole) -> bool {
    a.opening == b.opening
        && (a.center[0] - b.center[0]).abs() <= GROUND_TOLERANCE_M
        && (a.center[1] - b.center[1]).abs() <= GROUND_TOLERANCE_M
        && (a.half[0] - b.half[0]).abs() <= GROUND_TOLERANCE_M
        && (a.half[1] - b.half[1]).abs() <= GROUND_TOLERANCE_M
        && (a.yaw - b.yaw).abs() <= 1e-3
}

fn flag_duplicate_ids<'a>(
    map_id: &str,
    kind: &str,
    ids: impl Iterator<Item = &'a str>,
    issues: &mut Vec<MapIssue>,
) {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for id in ids {
        *seen.entry(id).or_insert(0) += 1;
    }
    for (id, count) in seen {
        if count > 1 {
            issues.push(MapIssue::new(
                map_id,
                "unique_id",
                id,
                format!("{kind} id repeated {count} times"),
            ));
        }
    }
}

fn req_str(value: &serde_json::Value, key: &str, ctx: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("{ctx} missing `{key}`"))
}

/// `NaN` / `Infinity` tokens outside JSON strings. serde_json rejects them
/// as a generic syntax error; callers need the word NaN in the message.
fn find_non_finite(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        for token in [
            b"NaN".as_slice(),
            b"+Infinity".as_slice(),
            b"-Infinity".as_slice(),
            b"Infinity".as_slice(),
        ] {
            if token_at(bytes, i, token) {
                let word = if token == b"NaN".as_slice() {
                    "NaN"
                } else {
                    "Infinity"
                };
                return Some(format!(
                    "non-finite number ({word}); heights and sizes must be finite"
                ));
            }
        }
        i += 1;
    }
    None
}

fn token_at(bytes: &[u8], i: usize, token: &[u8]) -> bool {
    let end = i + token.len();
    if end > bytes.len() || &bytes[i..end] != token {
        return false;
    }
    if i > 0 {
        let prev = bytes[i - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return false;
        }
    }
    if end < bytes.len() {
        let next = bytes[end];
        if next.is_ascii_alphanumeric() || next == b'_' {
            return false;
        }
    }
    true
}

/// The map's armed tunnels, from the one baked file when it is present and parses
/// (`<root>/assets_regia/maps/<map_id>/tunnels.json`, see [`load_tunnels_file`]), else from the spline
/// description. Byte-identical either way on the same terrain when the file is the current export
/// (`the_runtime_loader_arms_every_map_byte_identically`), so a sim and a client never disagree with the
/// shipped bake.
///
/// `from_baked` builds the host's tunnel state from the file; `for_map` builds it from the host's own
/// layout registry. A baked file is only used with a terrain (it meshes against the ground).
pub fn load_tunnels<T>(
    root: &Path,
    map_id: &str,
    terrain: Option<&TerrainSampler>,
    from_baked: impl FnOnce(&TunnelsFile, &TerrainSampler) -> T,
    for_map: impl FnOnce(&str, Option<&TerrainSampler>) -> Option<T>,
) -> Option<T> {
    match (load_tunnels_file(root, map_id), terrain) {
        (Some(file), Some(terrain)) => Some(from_baked(&file, terrain)),
        _ => for_map(map_id, terrain),
    }
}
