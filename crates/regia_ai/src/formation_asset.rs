// SPDX-License-Identifier: MPL-2.0
//! Formation shapes as data (ADR-8358, `.formation.ron` v1, registry `ai-formation`).
//!
//! A file names slot offsets in the anchor's frame (x to the right, z backward), an optional role
//! tag on each slot, a spacing scale and a fill order. [`assign_slots`] places members on those
//! slots by minimum total horizontal distance. The built-in names `ring`, `line` and `wedge` are
//! the same offsets the code shapes emit, as a finite preset.
//!
//! The older [`crate::FormationShape`] path (a member keeps its slot, then the innermost ring or
//! row) is unchanged when no custom asset is set. A custom asset uses this module instead.

use regia_core::math::Vec3;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::formation::{FormationShape, MAX_CANDIDATES, preset_offsets};

/// `.formation.ron` format version. Version 1 is the first published shape.
pub const FORMATION_FORMAT_VERSION: u32 = 1;

/// Hungarian assignment is used at this many rows and below. Above it, assignment is greedy.
///
/// The bound is the one the acceptance test checks against a brute-force minimum (`n <= 8` brute,
/// `n <= 64` Hungarian).
pub const FORMATION_HUNGARIAN_MAX: usize = 64;

/// Why a `.formation.ron` file was refused.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FormationAssetError {
    /// The text is not RON this version understands.
    #[error("parse: {0}")]
    Parse(String),
    /// `format_version` is not [`FORMATION_FORMAT_VERSION`].
    #[error("unsupported version {found} (this build reads version {supported})")]
    UnsupportedVersion {
        /// The version in the file.
        found: u32,
        /// The version this build reads.
        supported: u32,
    },
    /// The file has no slots.
    #[error("empty: a formation needs at least one slot")]
    Empty,
    /// More slots than [`MAX_CANDIDATES`].
    #[error("too_many_slots: {count} slots (at most {max})")]
    TooManySlots {
        /// How many slots the file has.
        count: usize,
        /// The cap.
        max: usize,
    },
    /// A slot name is empty, or the scale is not a positive finite number.
    #[error("bad_value: {0}")]
    BadValue(&'static str),
    /// Two slots share a name or the same offset.
    #[error("duplicate_slot: {0}")]
    DuplicateSlot(String),
    /// An offset or the spacing scale is NaN or infinite.
    #[error("non_finite: {0}")]
    NonFinite(&'static str),
    /// `fill_order` is not a permutation of the slot names.
    #[error("bad_fill: {0}")]
    BadFill(String),
}

impl FormationAssetError {
    /// The stable lowercase reason (`data.reason` on `invalid_asset`).
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Parse(_) => "parse",
            Self::UnsupportedVersion { .. } => "unsupported_version",
            Self::Empty => "empty",
            Self::TooManySlots { .. } => "too_many_slots",
            Self::BadValue(_) => "bad_value",
            Self::DuplicateSlot(_) => "duplicate_slot",
            Self::NonFinite(_) => "non_finite",
            Self::BadFill(_) => "bad_fill",
        }
    }
}

#[derive(Debug, Deserialize)]
struct VersionHeader {
    format_version: u32,
}

/// One slot offset in the file, metres, before [`FormationFile::spacing_scale`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SlotOffset {
    /// Metres to the anchor's right.
    pub x: f32,
    /// Metres up.
    pub y: f32,
    /// Metres backward from the anchor's facing.
    pub z: f32,
}

impl SlotOffset {
    /// An offset of `(x, y, z)`.
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    fn finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }

    fn bits(self) -> [u32; 3] {
        [self.x.to_bits(), self.y.to_bits(), self.z.to_bits()]
    }
}

/// One slot in a `.formation.ron` file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormationSlotFile {
    /// Unique name. `fill_order` refers to it.
    pub name: String,
    /// Offset in the anchor frame, before the spacing scale.
    pub offset: SlotOffset,
    /// When set, only a member whose role equals this tag may take the slot.
    #[serde(default)]
    pub role: Option<String>,
}

/// A `.formation.ron` file (format v1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormationFile {
    /// [`FORMATION_FORMAT_VERSION`].
    pub format_version: u32,
    /// A name for tools. May be empty.
    #[serde(default)]
    pub name: String,
    /// Multiplies every slot offset. Finite and greater than 0.
    #[serde(default = "one_scale")]
    pub spacing_scale: f32,
    /// Slot names in fill order. Empty means declaration order.
    #[serde(default)]
    pub fill_order: Vec<String>,
    /// The slots. At least one, at most [`MAX_CANDIDATES`].
    pub slots: Vec<FormationSlotFile>,
}

fn one_scale() -> f32 {
    1.0
}

impl FormationFile {
    /// Parses and validates `text`. A version other than 1 is refused before the body is trusted
    /// beyond its version field.
    pub fn from_ron(text: &str) -> Result<Self, FormationAssetError> {
        let header: VersionHeader =
            ron::from_str(text).map_err(|e| FormationAssetError::Parse(e.to_string()))?;
        if header.format_version != FORMATION_FORMAT_VERSION {
            return Err(FormationAssetError::UnsupportedVersion {
                found: header.format_version,
                supported: FORMATION_FORMAT_VERSION,
            });
        }
        let file: Self = ron::from_str(text).map_err(|e| FormationAssetError::Parse(e.to_string()))?;
        file.validate()?;
        Ok(file)
    }

    /// Canonical text. [`Self::from_ron`] of this text returns an equal file.
    pub fn to_ron(&self) -> String {
        let mut out = String::from("(\n");
        out.push_str(&format!("    format_version: {},\n", self.format_version));
        out.push_str(&format!("    name: {},\n", ron_string(&self.name)));
        out.push_str(&format!("    spacing_scale: {},\n", num(self.spacing_scale)));
        out.push_str("    fill_order: [");
        if self.fill_order.is_empty() {
            out.push_str("],\n");
        } else {
            out.push('\n');
            for name in &self.fill_order {
                out.push_str(&format!("        {},\n", ron_string(name)));
            }
            out.push_str("    ],\n");
        }
        out.push_str("    slots: [\n");
        for slot in &self.slots {
            out.push_str("        (\n");
            out.push_str(&format!("            name: {},\n", ron_string(&slot.name)));
            out.push_str(&format!(
                "            offset: (x: {}, y: {}, z: {}),\n",
                num(slot.offset.x),
                num(slot.offset.y),
                num(slot.offset.z)
            ));
            match &slot.role {
                Some(role) => out.push_str(&format!("            role: Some({}),\n", ron_string(role))),
                None => out.push_str("            role: None,\n"),
            }
            out.push_str("        ),\n");
        }
        out.push_str("    ],\n)\n");
        out
    }

    /// Checks the fields [`Self::from_ron`] checks.
    pub fn validate(&self) -> Result<(), FormationAssetError> {
        if self.format_version != FORMATION_FORMAT_VERSION {
            return Err(FormationAssetError::UnsupportedVersion {
                found: self.format_version,
                supported: FORMATION_FORMAT_VERSION,
            });
        }
        if self.slots.is_empty() {
            return Err(FormationAssetError::Empty);
        }
        if self.slots.len() > MAX_CANDIDATES {
            return Err(FormationAssetError::TooManySlots {
                count: self.slots.len(),
                max: MAX_CANDIDATES,
            });
        }
        if !self.spacing_scale.is_finite() {
            return Err(FormationAssetError::NonFinite("spacing_scale"));
        }
        if self.spacing_scale <= 0.0 {
            return Err(FormationAssetError::BadValue(
                "spacing_scale must be a finite number above 0",
            ));
        }
        let mut names = Vec::with_capacity(self.slots.len());
        let mut offsets = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            if slot.name.is_empty() {
                return Err(FormationAssetError::BadValue("a slot name must not be empty"));
            }
            if names.iter().any(|n| n == &slot.name) {
                return Err(FormationAssetError::DuplicateSlot(slot.name.clone()));
            }
            names.push(slot.name.clone());
            if !slot.offset.finite() {
                return Err(FormationAssetError::NonFinite("offset"));
            }
            let bits = slot.offset.bits();
            if offsets.iter().any(|o| o == &bits) {
                return Err(FormationAssetError::DuplicateSlot(slot.name.clone()));
            }
            offsets.push(bits);
            if slot.role.as_ref().is_some_and(|role| role.is_empty()) {
                return Err(FormationAssetError::BadValue(
                    "a slot role must not be an empty string",
                ));
            }
        }
        if !self.fill_order.is_empty() {
            if self.fill_order.len() != self.slots.len() {
                return Err(FormationAssetError::BadFill(
                    "fill_order must list every slot once".into(),
                ));
            }
            let mut seen = Vec::new();
            for name in &self.fill_order {
                if seen.iter().any(|n| n == name) || !names.iter().any(|n| n == name) {
                    return Err(FormationAssetError::BadFill(format!(
                        "unknown or repeated `{name}`"
                    )));
                }
                seen.push(name.clone());
            }
        }
        Ok(())
    }

    /// The runtime asset: offsets already multiplied by the spacing scale, fill ranks applied.
    pub fn to_asset(&self) -> Result<FormationAsset, FormationAssetError> {
        self.validate()?;
        let scale = self.spacing_scale;
        let mut slots = Vec::with_capacity(self.slots.len());
        for (index, slot) in self.slots.iter().enumerate() {
            let fill = if self.fill_order.is_empty() {
                u32::try_from(index).unwrap_or(u32::MAX)
            } else {
                self.fill_order
                    .iter()
                    .position(|name| name == &slot.name)
                    .and_then(|i| u32::try_from(i).ok())
                    .unwrap_or(u32::MAX)
            };
            let index_u = u32::try_from(index).unwrap_or(u32::MAX);
            slots.push(FormationSlotDesc {
                name: slot.name.clone(),
                offset: Vec3::new(
                    slot.offset.x * scale,
                    slot.offset.y * scale,
                    slot.offset.z * scale,
                ),
                role: slot.role.clone(),
                fill,
                index: index_u,
            });
        }
        Ok(FormationAsset {
            name: self.name.clone(),
            spacing_scale: scale,
            slots,
        })
    }
}

/// One validated slot.
#[derive(Debug, Clone, PartialEq)]
pub struct FormationSlotDesc {
    /// The slot's name.
    pub name: String,
    /// Offset in the anchor frame, after the spacing scale. x right, z backward.
    pub offset: Vec3,
    /// Role tag. `None` accepts any member.
    pub role: Option<String>,
    /// Fill rank. Lower ranks are preferred when distances tie.
    pub fill: u32,
    /// Declaration index. Unique inside the asset.
    pub index: u32,
}

/// A validated formation shape.
#[derive(Debug, Clone, PartialEq)]
pub struct FormationAsset {
    /// The file's name, or the preset name.
    pub name: String,
    /// The spacing scale the offsets already include.
    pub spacing_scale: f32,
    /// Slots in declaration order.
    pub slots: Vec<FormationSlotDesc>,
}

impl FormationAsset {
    /// The built-in preset `ring`, `line` or `wedge`, or `None` when `name` is not one of those.
    ///
    /// `ring` is the first ring of `Ring { radius: 3, spacing: 2 }` (the pinned nine places).
    /// `line` is one row of `Line { spacing: 2, row_length: 5 }`. `wedge` is the first four rows
    /// of `Wedge { spacing: 2 }`. Offsets are in the facing frame, not world-fixed.
    pub fn preset(name: &str) -> Option<Self> {
        let shape = match name {
            "ring" => FormationShape::Ring {
                radius: 3.0,
                spacing: 2.0,
            },
            "line" => FormationShape::Line {
                spacing: 2.0,
                row_length: 5,
            },
            "wedge" => FormationShape::Wedge { spacing: 2.0 },
            _ => return None,
        };
        let offsets = preset_offsets(shape);
        let slots = offsets
            .into_iter()
            .enumerate()
            .map(|(index, offset)| FormationSlotDesc {
                name: format!("s{index}"),
                offset,
                role: None,
                fill: u32::try_from(index).unwrap_or(u32::MAX),
                index: u32::try_from(index).unwrap_or(u32::MAX),
            })
            .collect();
        Some(Self {
            name: name.to_string(),
            spacing_scale: 1.0,
            slots,
        })
    }
}

/// The `.formation.ron` migration chain. Empty: version 1 is the first published shape.
pub fn formation_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Serde field-list fingerprint of `.formation.ron` (hashed by `cargo xtask check-formats`).
pub fn formation_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<FormationFile>(),
        serde_field_names::<FormationSlotFile>(),
        serde_field_names::<SlotOffset>(),
    ])
}

/// A file already at [`FORMATION_FORMAT_VERSION`] is returned unchanged. Any other version is
/// [`FormationAssetError::UnsupportedVersion`].
pub fn migrate_formation_ron(text: &str) -> Result<String, FormationAssetError> {
    let header: VersionHeader = ron::from_str(text).map_err(|e| FormationAssetError::Parse(e.to_string()))?;
    match header.format_version {
        FORMATION_FORMAT_VERSION => Ok(text.to_string()),
        found => Err(FormationAssetError::UnsupportedVersion {
            found,
            supported: FORMATION_FORMAT_VERSION,
        }),
    }
}

/// A member to place.
#[derive(Debug, Clone, PartialEq)]
pub struct AssignMember {
    /// Stable id. Ties break toward the lower id.
    pub id: u64,
    /// World position. Only XZ is used.
    pub position: Vec3,
    /// Role name. A tagged slot accepts only this string.
    pub role: Option<String>,
}

/// A slot to fill.
#[derive(Debug, Clone, PartialEq)]
pub struct AssignSlot {
    /// Stable index reported on the assignment.
    pub index: u32,
    /// Fill rank. Lower ranks win a distance tie.
    pub fill: u32,
    /// World position. Only XZ is used.
    pub position: Vec3,
    /// When set, only a member with this role may take the slot.
    pub role: Option<String>,
}

/// One member placed on one slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotAssignment {
    /// [`AssignMember::id`].
    pub member: u64,
    /// [`AssignSlot::index`].
    pub slot: u32,
}

/// Horizontal distance in micrometres, rounded, capped so a cost cell fits in the assignment
/// matrix. Non-finite input returns the cap.
pub fn horizontal_distance_um(a: Vec3, b: Vec3) -> i64 {
    let dx = f64::from(a.x) - f64::from(b.x);
    let dz = f64::from(a.z) - f64::from(b.z);
    if !dx.is_finite() || !dz.is_finite() {
        return DIST_CAP_UM;
    }
    let um = dx.hypot(dz) * 1_000_000.0;
    if !um.is_finite() {
        return DIST_CAP_UM;
    }
    um.round().clamp(0.0, DIST_CAP_UM as f64) as i64
}

/// One kilometre, in micrometres. Farther pairs share this cost so the matrix stays in range.
const DIST_CAP_UM: i64 = 1_000_000_000_000;

/// Minimum total horizontal distance, deterministic.
///
/// Members are ordered by id and slots by `(fill, index)` before the matrix is built, so the
/// input order does not change the result. A slot with a role tag accepts only a member whose
/// role equals that tag. An untagged slot accepts anyone. A pair that fails the tag is left
/// unassigned.
///
/// When `max(members, slots) <= 64` the assignment is the Hungarian method (Kuhn 1955), with the
/// cost of a feasible pair `distance_um * (n² + 1) + slot_rank * n + member_rank` so a shorter
/// total distance always wins and, on a tie, the lower fill rank then the lower member id wins.
/// Above 64 the pairs are taken greedily in order `(distance_um, member id, slot index)`.
///
/// `n` here is `max(member_count, slot_count)` after that sort, not the caller's vector order.
pub fn assign_slots(members: &[AssignMember], slots: &[AssignSlot]) -> Vec<SlotAssignment> {
    let _span = tracing::debug_span!(
        target: "regia_ai",
        "ai_formation_assign",
        members = members.len(),
        slots = slots.len()
    )
    .entered();
    if members.is_empty() || slots.is_empty() {
        return Vec::new();
    }
    let mut members: Vec<AssignMember> = members.to_vec();
    members.sort_by(|a, b| {
        a.id.cmp(&b.id)
            .then_with(|| a.position.x.total_cmp(&b.position.x))
    });
    let mut slots: Vec<AssignSlot> = slots.to_vec();
    slots.sort_by(|a, b| a.fill.cmp(&b.fill).then(a.index.cmp(&b.index)));
    let n = members.len().max(slots.len());
    let assigned = if n <= FORMATION_HUNGARIAN_MAX {
        hungarian_assign(&members, &slots).unwrap_or_else(|| greedy_assign(&members, &slots))
    } else {
        greedy_assign(&members, &slots)
    };
    let mut assigned = assigned;
    assigned.sort_by(|a, b| a.member.cmp(&b.member).then(a.slot.cmp(&b.slot)));
    assigned
}

fn role_ok(member: &AssignMember, slot: &AssignSlot) -> bool {
    match slot.role.as_deref() {
        None => true,
        Some(tag) => member.role.as_deref() == Some(tag),
    }
}

fn greedy_assign(members: &[AssignMember], slots: &[AssignSlot]) -> Vec<SlotAssignment> {
    let mut pairs = Vec::new();
    for member in members {
        for slot in slots {
            if !role_ok(member, slot) {
                continue;
            }
            pairs.push((
                horizontal_distance_um(member.position, slot.position),
                member.id,
                slot.index,
            ));
        }
    }
    pairs.sort_unstable();
    let mut used_members = Vec::new();
    let mut used_slots = Vec::new();
    let mut out = Vec::new();
    for (dist_ignored, member, slot) in pairs {
        let _ = dist_ignored;
        if used_members.contains(&member) || used_slots.contains(&slot) {
            continue;
        }
        used_members.push(member);
        used_slots.push(slot);
        out.push(SlotAssignment { member, slot });
    }
    out
}

fn hungarian_assign(members: &[AssignMember], slots: &[AssignSlot]) -> Option<Vec<SlotAssignment>> {
    let n = members.len().max(slots.len());
    let n_i = i128::try_from(n).ok()?;
    let scale = n_i * n_i + 1;
    // A real pair always costs less than leaving the member unplaced, and an infeasible pair
    // costs more than every unplaced pad so it is never chosen while a pad remains.
    let unplaced = i128::from(DIST_CAP_UM) * scale + n_i * n_i + 8;
    let infeasible = unplaced.saturating_mul(4);
    let mut cost = vec![vec![0i128; n]; n];
    for i in 0..n {
        for j in 0..n {
            let real_m = i < members.len();
            let real_s = j < slots.len();
            cost[i][j] = match (real_m, real_s) {
                (true, true) if role_ok(&members[i], &slots[j]) => {
                    let d = i128::from(horizontal_distance_um(members[i].position, slots[j].position));
                    d * scale + i128::try_from(j).unwrap_or(0) * n_i + i128::try_from(i).unwrap_or(0)
                }
                (true, true) => infeasible,
                (false, false) => 0,
                _ => unplaced,
            };
        }
    }
    let choice = hungarian(&cost)?;
    let mut out = Vec::new();
    for (i, j) in choice.into_iter().enumerate() {
        if i < members.len() && j < slots.len() && cost[i][j] < unplaced {
            out.push(SlotAssignment {
                member: members[i].id,
                slot: slots[j].index,
            });
        }
    }
    Some(out)
}

/// Minimum-cost assignment for a square `cost` matrix. `Ok` index `i` is the column chosen for
/// row `i`. `None` when the search does not finish (the caller uses the greedy assignment).
fn hungarian(cost: &[Vec<i128>]) -> Option<Vec<usize>> {
    let n = cost.len();
    if n == 0 || cost.iter().any(|row| row.len() != n) {
        return None;
    }
    let mut u = vec![0i128; n + 1];
    let mut v = vec![0i128; n + 1];
    let mut p = vec![0usize; n + 1];
    let mut way = vec![0usize; n + 1];
    let guard_limit = n
        .saturating_mul(n)
        .saturating_mul(n)
        .saturating_add(n)
        .saturating_add(8);
    for i in 1..=n {
        p[0] = i;
        let mut j0 = 0usize;
        let mut minv = vec![i128::MAX / 4; n + 1];
        let mut used = vec![false; n + 1];
        let mut guard = 0usize;
        loop {
            guard += 1;
            if guard > guard_limit {
                return None;
            }
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = i128::MAX / 4;
            let mut j1 = 0usize;
            let mut found = false;
            for j in 1..=n {
                if used[j] {
                    continue;
                }
                let cur = cost[i0 - 1][j - 1] - u[i0] - v[j];
                if cur < minv[j] {
                    minv[j] = cur;
                    way[j] = j0;
                }
                if minv[j] < delta {
                    delta = minv[j];
                    j1 = j;
                    found = true;
                }
            }
            if !found {
                return None;
            }
            for j in 0..=n {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] -= delta;
                }
            }
            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }
        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }
    let mut assignment = vec![0usize; n];
    for j in 1..=n {
        if p[j] != 0 {
            assignment[p[j] - 1] = j - 1;
        }
    }
    Some(assignment)
}

fn num(v: f32) -> String {
    if !v.is_finite() {
        return "0.0".to_string();
    }
    let v = if v == 0.0 { 0.0 } else { v };
    let mut s = format!("{v}");
    if !s.contains('.') && !s.contains('e') && !s.contains('E') {
        s.push_str(".0");
    }
    s
}

fn ron_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: u64, x: f32, z: f32) -> AssignMember {
        AssignMember {
            id,
            position: Vec3::new(x, 0.0, z),
            role: None,
        }
    }

    fn slot(index: u32, x: f32, z: f32) -> AssignSlot {
        AssignSlot {
            index,
            fill: index,
            position: Vec3::new(x, 0.0, z),
            role: None,
        }
    }

    #[test]
    fn crossed_pair_takes_the_zero_distance_matching() {
        let members = vec![member(1, 0.0, 0.0), member(2, 5.0, 0.0)];
        let slots = vec![slot(0, 5.0, 0.0), slot(1, 0.0, 0.0)];
        let got = assign_slots(&members, &slots);
        assert_eq!(
            got,
            vec![
                SlotAssignment { member: 1, slot: 1 },
                SlotAssignment { member: 2, slot: 0 },
            ]
        );
    }

    #[test]
    fn preset_ring_has_the_nine_pinned_places() {
        let ring = FormationAsset::preset("ring").expect("ring");
        assert_eq!(ring.slots.len(), 9);
        assert!(FormationAsset::preset("nope").is_none());
    }

    #[test]
    fn nan_duplicate_and_overflow_are_refused() {
        let bad = r#"(
            format_version: 1,
            name: "x",
            spacing_scale: 1.0,
            slots: [(name: "a", offset: (x: NaN, y: 0.0, z: 0.0), role: None)],
        )"#;
        assert_eq!(
            FormationFile::from_ron(bad).expect_err("refused").reason(),
            "non_finite"
        );
        let dup = r#"(
            format_version: 1,
            name: "x",
            spacing_scale: 1.0,
            slots: [
                (name: "a", offset: (x: 1.0, y: 0.0, z: 0.0), role: None),
                (name: "a", offset: (x: 2.0, y: 0.0, z: 0.0), role: None),
            ],
        )"#;
        assert_eq!(
            FormationFile::from_ron(dup).expect_err("refused").reason(),
            "duplicate_slot"
        );
        let mut slots = String::from("[\n");
        for i in 0..=MAX_CANDIDATES {
            slots.push_str(&format!(
                "(name: \"s{i}\", offset: (x: {i}.0, y: 0.0, z: 0.0), role: None),\n"
            ));
        }
        slots.push(']');
        let text = format!("(format_version: 1, name: \"x\", spacing_scale: 1.0, slots: {slots})");
        assert_eq!(
            FormationFile::from_ron(&text).expect_err("refused").reason(),
            "too_many_slots"
        );
    }

    #[test]
    fn canonical_text_round_trips() {
        let text = r#"(
            format_version: 1,
            name: "wedge-custom",
            spacing_scale: 1.0,
            fill_order: ["left", "right"],
            slots: [
                (name: "left", offset: (x: -1.0, y: 0.0, z: 2.0), role: Some("flanker")),
                (name: "right", offset: (x: 1.0, y: 0.0, z: 2.0), role: None),
            ],
        )"#;
        let file = FormationFile::from_ron(text).expect("parses");
        let again = FormationFile::from_ron(&file.to_ron()).expect("canonical parses");
        assert_eq!(file, again);
        assert_eq!(again.to_ron(), file.to_ron());
        assert!(migrate_formation_ron(text).is_ok());
        let future = text.replace("format_version: 1", "format_version: 2");
        let err = FormationFile::from_ron(&future).expect_err("refused");
        assert_eq!(err.reason(), "unsupported_version");
        assert!(err.to_string().to_ascii_lowercase().contains("version"));
    }
}
