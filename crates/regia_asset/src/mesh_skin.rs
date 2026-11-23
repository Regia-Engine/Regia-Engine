// SPDX-License-Identifier: MPL-2.0
//! Skin, vertex colour and sparse morph-target blocks of the cooked mesh (cooked mesh format v2,
//! `docs/formats/cooked-mesh.md`, ADR-226).
//!
//! The packed forms ([`CookedSkin`], [`CookedMorphTarget`], the `unorm16x4` colour stream) live
//! in [`crate::CookedMesh`]; the unpacked forms ([`CpuSkin`], [`CpuMorphTarget`], `[f32; 4]`
//! colours) live in [`crate::CpuMesh`]. Semantics follow glTF 2.0 § 3.7.3 (skins: `JOINTS_n` /
//! `WEIGHTS_n`, four influences per set) and § 3.7.2.2 (morph targets: per-vertex deltas added
//! to the base attributes).

use serde::{Deserialize, Serialize};

use crate::cooked::{CookedMeshValidationError, MeshValidationError};

/// [`crate::CookedMesh::format_version`] written by this build.
///
/// Version 1 is every payload without the field (importer 1.7.0 and earlier, with or without
/// `lod_metric`). Version 2 appends the skin, colour and morph blocks. Version 3 appends
/// [`crate::CookedMesh::skin_influences`] (ADR-1622). A v1 payload loads with none of the v2
/// blocks, and a v1 or v2 payload loads with four influences
/// ([`crate::CookedMesh::from_postcard`]); `regia upgrade-project` rewrites cooked files with
/// [`migrate_cooked_mesh_file`].
pub const COOKED_MESH_FORMAT_VERSION: u32 = 3;

/// The cooked-mesh versions a migration step reads from (`cargo xtask check-formats`).
pub fn cooked_mesh_migration_sources() -> Vec<u32> {
    vec![1, 2]
}

/// Serde shape of the cooked mesh payload, for `cargo xtask check-formats`.
pub fn cooked_mesh_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<crate::CookedMesh>(),
        serde_field_names::<crate::CookedMeshLod>(),
        serde_field_names::<crate::CookedSubmesh>(),
        serde_variant_names::<crate::CookedIndexWidth>(),
        serde_field_names::<CookedSkin>(),
        serde_variant_names::<CookedJointWidth>(),
        serde_field_names::<CookedMorphTarget>(),
    ])
}

/// Width of one joint index in [`CookedSkin`]'s joint streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CookedJointWidth {
    /// `u8x4`: the skin has at most 256 joints.
    U8,
    /// `u16x4`: the skin has more than 256 joints.
    U16,
}

impl CookedJointWidth {
    /// The width a skin with `joint_count` joints is written with.
    #[must_use]
    pub fn for_joint_count(joint_count: usize) -> Self {
        if joint_count <= 256 { Self::U8 } else { Self::U16 }
    }

    /// Bytes per joint index.
    #[must_use]
    pub fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
        }
    }
}

/// Packed skin streams of a cooked mesh.
///
/// Joint indices are positions in [`Self::joint_names`], which the glTF importer writes in the
/// cooked skeleton's joint order (`regia_animation::Skeleton` ids), so a palette built from
/// that skeleton indexes them directly. Each vertex has one set of four influences, or two sets
/// (eight influences) when [`Self::stream_joints1`] is not empty. Weights are `unorm16`; the
/// weights of one vertex (both sets together) sum to exactly 65535. Influence slots keep the
/// authored order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedSkin {
    /// Joint names, one per joint index.
    pub joint_names: Vec<String>,
    /// Width of the joint streams ([`CookedJointWidth::for_joint_count`] of the name count).
    pub joint_width: CookedJointWidth,
    /// Influence set 0 joints: four indices per vertex, little endian.
    pub stream_joints0: Vec<u8>,
    /// Influence set 0 weights: `unorm16x4` per vertex, little endian.
    pub stream_weights0: Vec<u8>,
    /// Influence set 1 joints (influences 5–8), empty for a four-influence skin.
    pub stream_joints1: Vec<u8>,
    /// Influence set 1 weights, empty exactly when [`Self::stream_joints1`] is.
    pub stream_weights1: Vec<u8>,
}

/// One sparse morph target of a cooked mesh.
///
/// Only the vertices the target moves are stored: [`Self::vertex_indices`] is strictly
/// increasing, and delta `i` belongs to vertex `vertex_indices[i]`. Deltas are `f16x3`, little
/// endian (6 bytes each). The normal and tangent streams are empty when the source target has
/// no such attribute.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedMorphTarget {
    /// Target name (glTF `mesh.extras.targetNames`, or `target_<n>`).
    pub name: String,
    /// Affected vertices, strictly increasing.
    pub vertex_indices: Vec<u32>,
    /// Position deltas (metres, mesh space), `f16x3` per affected vertex.
    pub position_deltas: Vec<u8>,
    /// Normal deltas, `f16x3` per affected vertex, or empty.
    pub normal_deltas: Vec<u8>,
    /// Tangent (xyz) deltas, `f16x3` per affected vertex, or empty.
    pub tangent_deltas: Vec<u8>,
    /// Component-wise minimum of the stored position deltas (zero when there are none).
    pub delta_min: [f32; 3],
    /// Component-wise maximum of the stored position deltas.
    pub delta_max: [f32; 3],
}

/// Unpacked skin of a [`crate::CpuMesh`]: one entry per vertex in every non-empty array.
///
/// Build one with `..Default::default()` to stay source compatible when fields are added.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CpuSkin {
    /// Joint names, one per joint index (see [`CookedSkin::joint_names`]).
    pub joint_names: Vec<String>,
    /// Influence set 0 joint indices.
    pub joints: Vec<[u16; 4]>,
    /// Influence set 0 weights, `unorm16` (both sets of a vertex sum to 65535).
    pub weights: Vec<[u16; 4]>,
    /// Influence set 1 joint indices (influences 5–8), empty for a four-influence skin.
    pub joints1: Vec<[u16; 4]>,
    /// Influence set 1 weights, empty exactly when [`Self::joints1`] is.
    pub weights1: Vec<[u16; 4]>,
    /// The mesh opted into drawing all eight influences ([`crate::CookedMesh::skin_influences`]
    /// 8, ADR-1622). False by default: renderers draw the four strongest ([`Self::top4`]). True
    /// needs a second influence set. Not part of the packed [`CookedSkin`]: the cook writes it to
    /// the cooked mesh, and [`crate::CookedMesh::to_cpu_mesh`] sets it.
    pub eight_influences: bool,
}

impl CpuSkin {
    /// Influences stored per vertex: 4, or 8 with a second set.
    #[must_use]
    pub fn influences_per_vertex(&self) -> usize {
        if self.joints1.is_empty() { 4 } else { 8 }
    }

    /// The (up to) four strongest influences of `vertex` as joint ids and float weights that
    /// sum to 1, for a renderer limited to four influences (`specs/13` § 8). A four-influence
    /// skin returns its slots in stored order; an eight-influence vertex keeps its four largest
    /// weights (earlier slot wins a tie) and renormalises them. `None` when `vertex` is out of
    /// range or its weights are all zero.
    #[must_use]
    pub fn top4(&self, vertex: usize) -> Option<([u32; 4], [f32; 4])> {
        let joints = self.joints.get(vertex)?;
        let weights = self.weights.get(vertex)?;
        let mut slots = [(0u16, 0u16); 8];
        for (i, slot) in slots.iter_mut().take(4).enumerate() {
            *slot = (joints[i], weights[i]);
        }
        if let (Some(j1), Some(w1)) = (self.joints1.get(vertex), self.weights1.get(vertex)) {
            for (i, slot) in slots.iter_mut().skip(4).enumerate() {
                *slot = (j1[i], w1[i]);
            }
            // Stable: equal weights keep their slot order.
            slots.sort_by_key(|slot| std::cmp::Reverse(slot.1));
        }
        let slots = &slots[..4];
        let sum: u32 = slots.iter().map(|s| u32::from(s.1)).sum();
        if sum == 0 {
            return None;
        }
        let mut out_joints = [0u32; 4];
        let mut out_weights = [0.0f32; 4];
        for (i, (joint, weight)) in slots.iter().enumerate() {
            out_joints[i] = u32::from(*joint);
            out_weights[i] = (f64::from(*weight) / f64::from(sum)) as f32;
        }
        Some((out_joints, out_weights))
    }

    /// Checks array lengths against `vertex_count` and every joint index against the name list.
    pub fn validate(&self, vertex_count: usize) -> Result<(), MeshValidationError> {
        let bad = |reason: String| Err(MeshValidationError::InvalidSkin { reason });
        if self.joint_names.is_empty() {
            return bad("skin has no joints".into());
        }
        if self.joint_names.len() > usize::from(u16::MAX) + 1 {
            return bad(format!("{} joints (at most 65536)", self.joint_names.len()));
        }
        if self.joints.len() != vertex_count || self.weights.len() != vertex_count {
            return bad(format!(
                "set 0 has {} joints / {} weights for {vertex_count} vertices",
                self.joints.len(),
                self.weights.len()
            ));
        }
        let second = self.joints1.len();
        if second != self.weights1.len() || (second != 0 && second != vertex_count) {
            return bad(format!(
                "set 1 has {second} joints / {} weights for {vertex_count} vertices",
                self.weights1.len()
            ));
        }
        if self.eight_influences && second == 0 {
            return bad("eight influences need a second influence set".into());
        }
        let count = self.joint_names.len();
        for set in [&self.joints, &self.joints1] {
            if let Some(index) = set.iter().flatten().find(|j| usize::from(**j) >= count) {
                return bad(format!("joint index {index} out of range ({count} joints)"));
            }
        }
        Ok(())
    }

    /// Packs into [`CookedSkin`] streams.
    pub fn pack(&self, vertex_count: usize) -> Result<CookedSkin, MeshValidationError> {
        self.validate(vertex_count)?;
        let width = CookedJointWidth::for_joint_count(self.joint_names.len());
        Ok(CookedSkin {
            joint_names: self.joint_names.clone(),
            joint_width: width,
            stream_joints0: pack_joints(&self.joints, width),
            stream_weights0: pack_u16x4(&self.weights),
            stream_joints1: pack_joints(&self.joints1, width),
            stream_weights1: pack_u16x4(&self.weights1),
        })
    }
}

impl CookedSkin {
    /// Checks stream sizes against `vertex_count` and every joint index against the name list.
    pub fn validate(&self, vertex_count: usize) -> Result<(), CookedMeshValidationError> {
        let bad = |reason: String| Err(CookedMeshValidationError::Skin { reason });
        if self.joint_names.is_empty() {
            return bad("skin has no joints".into());
        }
        if CookedJointWidth::for_joint_count(self.joint_names.len()) != self.joint_width {
            return bad(format!(
                "joint width {:?} does not fit {} joints",
                self.joint_width,
                self.joint_names.len()
            ));
        }
        let joint_bytes = vertex_count.saturating_mul(4 * self.joint_width.bytes());
        let weight_bytes = vertex_count.saturating_mul(8);
        if self.stream_joints0.len() != joint_bytes || self.stream_weights0.len() != weight_bytes {
            return bad("influence set 0 stream size does not match the vertex count".into());
        }
        let second = !self.stream_joints1.is_empty() || !self.stream_weights1.is_empty();
        if second && (self.stream_joints1.len() != joint_bytes || self.stream_weights1.len() != weight_bytes)
        {
            return bad("influence set 1 stream size does not match the vertex count".into());
        }
        let w0 = unpack_u16x4(&self.stream_weights0);
        let w1 = unpack_u16x4(&self.stream_weights1);
        for (i, a) in w0.iter().enumerate() {
            let extra: u32 = w1.get(i).map_or(0, |b| b.iter().map(|w| u32::from(*w)).sum());
            let sum = a.iter().map(|w| u32::from(*w)).sum::<u32>() + extra;
            if sum != 65535 {
                return bad(format!("vertex {i} weights sum to {sum}, not 65535"));
            }
        }
        let count = self.joint_names.len();
        for stream in [&self.stream_joints0, &self.stream_joints1] {
            let out_of_range = match self.joint_width {
                CookedJointWidth::U8 => stream
                    .iter()
                    .find(|j| usize::from(**j) >= count)
                    .map(|j| usize::from(*j)),
                CookedJointWidth::U16 => stream
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| usize::from(u16::from_le_bytes(*c)))
                    .find(|j| *j >= count),
            };
            if let Some(index) = out_of_range {
                return bad(format!("joint index {index} out of range ({count} joints)"));
            }
        }
        Ok(())
    }

    /// Unpacks to [`CpuSkin`] and validates it.
    pub fn unpack(&self, vertex_count: usize) -> Result<CpuSkin, CookedMeshValidationError> {
        self.validate(vertex_count)?;
        let skin = CpuSkin {
            joint_names: self.joint_names.clone(),
            joints: unpack_joints(&self.stream_joints0, self.joint_width),
            weights: unpack_u16x4(&self.stream_weights0),
            joints1: unpack_joints(&self.stream_joints1, self.joint_width),
            weights1: unpack_u16x4(&self.stream_weights1),
            eight_influences: false,
        };
        skin.validate(vertex_count)
            .map_err(|e| CookedMeshValidationError::Skin {
                reason: e.to_string(),
            })?;
        Ok(skin)
    }
}

/// Unpacked sparse morph target of a [`crate::CpuMesh`] (see [`CookedMorphTarget`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CpuMorphTarget {
    /// Target name.
    pub name: String,
    /// Affected vertices, strictly increasing.
    pub vertex_indices: Vec<u32>,
    /// Position deltas, one per affected vertex.
    pub position_deltas: Vec<[f32; 3]>,
    /// Normal deltas, one per affected vertex, or empty.
    pub normal_deltas: Vec<[f32; 3]>,
    /// Tangent deltas, one per affected vertex, or empty.
    pub tangent_deltas: Vec<[f32; 3]>,
}

impl CpuMorphTarget {
    /// Checks the index list (strictly increasing, in range) and the delta counts.
    pub fn validate(&self, vertex_count: usize) -> Result<(), MeshValidationError> {
        let bad = |reason: String| {
            Err(MeshValidationError::InvalidMorphTarget {
                name: self.name.clone(),
                reason,
            })
        };
        let n = self.vertex_indices.len();
        if self.position_deltas.len() != n {
            return bad(format!(
                "{} position deltas for {n} vertices",
                self.position_deltas.len()
            ));
        }
        for (what, deltas) in [("normal", &self.normal_deltas), ("tangent", &self.tangent_deltas)] {
            if !deltas.is_empty() && deltas.len() != n {
                return bad(format!("{} {what} deltas for {n} vertices", deltas.len()));
            }
        }
        let mut previous: Option<u32> = None;
        for &index in &self.vertex_indices {
            if index as usize >= vertex_count {
                return bad(format!("vertex {index} out of range ({vertex_count} vertices)"));
            }
            if previous.is_some_and(|p| p >= index) {
                return bad("vertex indices are not strictly increasing".into());
            }
            previous = Some(index);
        }
        Ok(())
    }

    /// Packs to `f16x3` deltas and computes the position-delta bounds (of the packed values).
    pub fn pack(&self, vertex_count: usize) -> Result<CookedMorphTarget, MeshValidationError> {
        self.validate(vertex_count)?;
        let position_deltas = pack_f16x3(&self.position_deltas);
        let mut delta_min = [0.0f32; 3];
        let mut delta_max = [0.0f32; 3];
        for (i, delta) in unpack_f16x3(&position_deltas).iter().enumerate() {
            for axis in 0..3 {
                if i == 0 || delta[axis] < delta_min[axis] {
                    delta_min[axis] = delta[axis];
                }
                if i == 0 || delta[axis] > delta_max[axis] {
                    delta_max[axis] = delta[axis];
                }
            }
        }
        Ok(CookedMorphTarget {
            name: self.name.clone(),
            vertex_indices: self.vertex_indices.clone(),
            position_deltas,
            normal_deltas: pack_f16x3(&self.normal_deltas),
            tangent_deltas: pack_f16x3(&self.tangent_deltas),
            delta_min,
            delta_max,
        })
    }
}

impl CookedMorphTarget {
    /// Checks stream sizes and the index list against `vertex_count`.
    pub fn validate(&self, vertex_count: usize) -> Result<(), CookedMeshValidationError> {
        let n = self.vertex_indices.len();
        let bytes = n.saturating_mul(6);
        let sizes_ok = self.position_deltas.len() == bytes
            && (self.normal_deltas.is_empty() || self.normal_deltas.len() == bytes)
            && (self.tangent_deltas.is_empty() || self.tangent_deltas.len() == bytes);
        if !sizes_ok {
            return Err(CookedMeshValidationError::Morph {
                name: self.name.clone(),
                reason: "delta stream size does not match the vertex index count".into(),
            });
        }
        let mut previous: Option<u32> = None;
        for &index in &self.vertex_indices {
            if index as usize >= vertex_count || previous.is_some_and(|p| p >= index) {
                return Err(CookedMeshValidationError::Morph {
                    name: self.name.clone(),
                    reason: format!("vertex index {index} is out of range or out of order"),
                });
            }
            previous = Some(index);
        }
        Ok(())
    }

    /// Unpacks to [`CpuMorphTarget`].
    pub fn unpack(&self, vertex_count: usize) -> Result<CpuMorphTarget, CookedMeshValidationError> {
        self.validate(vertex_count)?;
        Ok(CpuMorphTarget {
            name: self.name.clone(),
            vertex_indices: self.vertex_indices.clone(),
            position_deltas: unpack_f16x3(&self.position_deltas),
            normal_deltas: unpack_f16x3(&self.normal_deltas),
            tangent_deltas: unpack_f16x3(&self.tangent_deltas),
        })
    }
}

/// Quantises one vertex's skin weights (one or both influence sets, in slot order) to `unorm16`
/// values that sum to exactly 65535.
///
/// Negative and non-finite weights count as 0. The weights are normalised to sum 1 and scaled
/// to 65535 in `f64`; each slot takes the floor, and the units left over go to the slots with
/// the largest fractional parts (largest-remainder rounding; the earlier slot wins a tie), so
/// every weight is within 1/65535 of its normalised value. All-zero weights give the whole
/// weight to slot 0 (glTF 2.0 § 3.7.3.3 requires a non-zero sum; this keeps such a vertex on a
/// joint instead of collapsing it).
#[must_use]
pub fn quantize_skin_weights<const N: usize>(weights: [f32; N]) -> [u16; N] {
    let clean = weights.map(|w| {
        if w.is_finite() && w > 0.0 {
            f64::from(w)
        } else {
            0.0
        }
    });
    let sum: f64 = clean.iter().sum();
    let mut out = [0u16; N];
    if N == 0 {
        return out;
    }
    if sum <= 0.0 {
        out[0] = u16::MAX;
        return out;
    }
    let scaled = clean.map(|w| w / sum * 65535.0);
    let mut total: u32 = 0;
    for (slot, value) in scaled.iter().enumerate() {
        let floor = value.floor().min(65535.0);
        out[slot] = floor as u16;
        total += u32::from(out[slot]);
    }
    let mut order: Vec<usize> = (0..N).collect();
    order.sort_by(|&a, &b| {
        let fa = scaled[a] - scaled[a].floor();
        let fb = scaled[b] - scaled[b].floor();
        fb.total_cmp(&fa).then(a.cmp(&b))
    });
    let mut left = 65535u32.saturating_sub(total);
    for &slot in order.iter().cycle() {
        if left == 0 {
            break;
        }
        if out[slot] < u16::MAX {
            out[slot] += 1;
            left -= 1;
        }
    }
    out
}

/// Normalised `unorm16` weights from integer sources (glTF `unorm8` / `unorm16` `WEIGHTS_n`):
/// kept bit for bit when they already sum to 65535 (`unorm8` is widened by ×257, which keeps a
/// 255 sum at 65535); otherwise requantised with [`quantize_skin_weights`].
#[must_use]
pub fn normalize_unorm16_weights<const N: usize>(weights: [u16; N]) -> [u16; N] {
    let sum: u32 = weights.iter().map(|w| u32::from(*w)).sum();
    if sum == 65535 {
        return weights;
    }
    quantize_skin_weights(weights.map(|w| f32::from(w) / 65535.0))
}

/// `f32` → IEEE 754 binary16 bits, round to nearest even, subnormals kept.
///
/// The overflow to infinity is at 65520 (round-to-nearest-even from the 2^15 binade). A value in
/// `(65504, 65520)` stays the largest finite half (`0x7bff`); 65520 and above become infinity.
/// NaN stays NaN. An input of exactly 65504 stays `0x7bff`.
#[must_use]
pub fn f32_to_f16_round(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let abs = bits & 0x7fff_ffff;
    if abs >= 0x7f80_0000 {
        let nan = if abs > 0x7f80_0000 { 0x0200 } else { 0 };
        return sign | 0x7c00 | nan;
    }
    if abs >= 0x4780_0000 {
        // ≥ 65536 overflows.
        return sign | 0x7c00;
    }
    if abs < 0x3880_0000 {
        // Below the smallest normal half (2^-14): subnormal half or zero.
        if abs < 0x3300_0000 {
            // Below 2^-25 rounds to zero (2^-25 itself ties to even = 0).
            return sign;
        }
        let exp = (abs >> 23) as i32;
        let mant = (abs & 0x7f_ffff) | 0x80_0000;
        let shift = (126 - exp) as u32;
        let mut half = mant >> shift;
        let rem = mant & ((1 << shift) - 1);
        let halfway = 1 << (shift - 1);
        if rem > halfway || (rem == halfway && half & 1 == 1) {
            half += 1;
        }
        return sign | half as u16;
    }
    let exp = (abs >> 23) as i32 - 112;
    let mant = abs & 0x7f_ffff;
    let mut half = ((exp as u32) << 10) | (mant >> 13);
    let rem = mant & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && half & 1 == 1) {
        // A carry into the exponent is the correct next binade (or infinity).
        half += 1;
    }
    sign | half as u16
}

/// IEEE 754 binary16 bits → `f32` (exact, subnormals included).
#[must_use]
pub fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exp = u32::from((bits >> 10) & 0x1f);
    let mant = u32::from(bits & 0x03ff);
    match exp {
        0 => {
            // Subnormal (or zero): mant × 2^-24, exact in f32.
            let magnitude = mant as f32 * f32::from_bits(0x3380_0000);
            f32::from_bits(sign | magnitude.to_bits())
        }
        0x1f => f32::from_bits(sign | 0x7f80_0000 | (mant << 13)),
        _ => f32::from_bits(sign | ((exp + 112) << 23) | (mant << 13)),
    }
}

/// `[f32; 4]` linear colours → `unorm16x4` bytes (each channel clamped to 0..=1, rounded).
pub fn pack_colors_unorm16(colors: &[[f32; 4]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(colors.len() * 8);
    for color in colors {
        for channel in color {
            let c = if channel.is_finite() {
                channel.clamp(0.0, 1.0)
            } else {
                0.0
            };
            let q = (f64::from(c) * 65535.0).round() as u16;
            out.extend_from_slice(&q.to_le_bytes());
        }
    }
    out
}

/// `unorm16x4` bytes → `[f32; 4]` colours.
pub fn unpack_colors_unorm16(bytes: &[u8]) -> Vec<[f32; 4]> {
    unpack_u16x4(bytes)
        .into_iter()
        .map(|c| c.map(|v| f32::from(v) / 65535.0))
        .collect()
}

fn pack_joints(joints: &[[u16; 4]], width: CookedJointWidth) -> Vec<u8> {
    let mut out = Vec::with_capacity(joints.len() * 4 * width.bytes());
    for set in joints {
        for &joint in set {
            match width {
                // `CpuSkin::validate` bounds every index by the joint count, which is ≤ 256 here.
                CookedJointWidth::U8 => out.push(joint as u8),
                CookedJointWidth::U16 => out.extend_from_slice(&joint.to_le_bytes()),
            }
        }
    }
    out
}

fn unpack_joints(bytes: &[u8], width: CookedJointWidth) -> Vec<[u16; 4]> {
    match width {
        CookedJointWidth::U8 => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| c.map(u16::from))
            .collect(),
        CookedJointWidth::U16 => unpack_u16x4(bytes),
    }
}

fn pack_u16x4(values: &[[u16; 4]]) -> Vec<u8> {
    values.iter().flatten().flat_map(|v| v.to_le_bytes()).collect()
}

fn unpack_u16x4(bytes: &[u8]) -> Vec<[u16; 4]> {
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| {
            [
                u16::from_le_bytes([c[0], c[1]]),
                u16::from_le_bytes([c[2], c[3]]),
                u16::from_le_bytes([c[4], c[5]]),
                u16::from_le_bytes([c[6], c[7]]),
            ]
        })
        .collect()
}

fn pack_f16x3(values: &[[f32; 3]]) -> Vec<u8> {
    values
        .iter()
        .flatten()
        .flat_map(|v| f32_to_f16_round(v.clamp(-65504.0, 65504.0)).to_le_bytes())
        .collect()
}

fn unpack_f16x3(bytes: &[u8]) -> Vec<[f32; 3]> {
    bytes
        .as_chunks::<6>()
        .0
        .iter()
        .map(|c| {
            [
                f16_bits_to_f32(u16::from_le_bytes([c[0], c[1]])),
                f16_bits_to_f32(u16::from_le_bytes([c[2], c[3]])),
                f16_bits_to_f32(u16::from_le_bytes([c[4], c[5]])),
            ]
        })
        .collect()
}

/// Rewrites a cooked mesh file (`FCKD` wrapper around a `regia_asset::CookedMesh` payload) at
/// [`COOKED_MESH_FORMAT_VERSION`]: the payload is decoded through every older layout
/// ([`crate::CookedMesh::from_postcard`]) and re-encoded; the header keeps every field but the
/// payload length. Returns `Ok(None)` when the file is not a cooked mesh or is already current.
///
/// A v1 mesh gains no skin, colour or morph data, and a v1 or v2 mesh gains
/// `skin_influences` 4, so it draws exactly as it loaded before.
pub fn migrate_cooked_mesh_file(bytes: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let Some(version) = cooked_mesh_file_version(bytes)? else {
        return Ok(None);
    };
    if version >= COOKED_MESH_FORMAT_VERSION {
        return Ok(None);
    }
    let (mut header, payload) = crate::decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    let mesh = crate::CookedMesh::from_postcard(payload).map_err(|e| e.to_string())?;
    let out = mesh.to_postcard().map_err(|e| e.to_string())?;
    header.payload_len = out.len() as u64;
    crate::encode_cooked_file(&header, &out)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// The cooked-mesh payload version of a cooked file: `Ok(None)` when the file's type tag is not
/// [`crate::AssetTypeTag::mesh`] (that includes the legacy `CpuMesh` payload, which carries its own
/// [`crate::AssetTypeTag::mesh_legacy_cpu`] tag and no version), an error when it is not a cooked
/// file or the mesh payload does not decode.
pub fn cooked_mesh_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let (header, payload) = crate::decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    if header.type_tag != crate::AssetTypeTag::mesh().0 {
        return Ok(None);
    }
    crate::CookedMesh::payload_version(payload)
        .map(Some)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooked_skin_rejects_weights_that_do_not_sum_to_65535() {
        let skin = CpuSkin {
            joint_names: vec!["a".into(), "b".into()],
            joints: vec![[0, 1, 0, 0]; 2],
            weights: vec![[40000, 25535, 0, 0]; 2],
            joints1: Vec::new(),
            weights1: Vec::new(),
            eight_influences: false,
        };
        let mut packed = skin.pack(2).expect("pack");
        assert!(packed.validate(2).is_ok());
        // Zero the second vertex's weights.
        packed.stream_weights0[8..16].fill(0);
        assert!(packed.validate(2).is_err());
        assert!(packed.unpack(2).is_err());
    }

    #[test]
    fn quantized_weights_sum_to_one_and_stay_close() {
        let cases: [[f32; 4]; 6] = [
            [1.0, 0.0, 0.0, 0.0],
            [0.5, 0.5, 0.0, 0.0],
            [1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0, 0.0],
            [0.7, 0.2, 0.05, 0.05],
            [0.25, 0.25, 0.25, 0.25],
            [2.0, 1.0, 1.0, 0.0],
        ];
        for weights in cases {
            let q = quantize_skin_weights(weights);
            let sum: u32 = q.iter().map(|v| u32::from(*v)).sum();
            assert_eq!(sum, 65535, "{weights:?} -> {q:?}");
            let total: f32 = weights.iter().sum();
            for slot in 0..4 {
                let want = f64::from(weights[slot] / total);
                let got = f64::from(q[slot]) / 65535.0;
                assert!((want - got).abs() <= 1.0 / 65535.0, "{weights:?} slot {slot}");
            }
        }
    }

    #[test]
    fn degenerate_weights_go_to_slot_zero() {
        assert_eq!(quantize_skin_weights([0.0f32; 4]), [65535, 0, 0, 0]);
        assert_eq!(
            quantize_skin_weights([f32::NAN, -1.0, 0.0, 0.0]),
            [65535, 0, 0, 0]
        );
        assert_eq!(quantize_skin_weights([0.0, f32::NAN, 2.0, 0.0]), [0, 0, 65535, 0]);
    }

    #[test]
    fn eight_influences_share_one_sum() {
        let q = quantize_skin_weights([0.3f32, 0.2, 0.1, 0.1, 0.1, 0.1, 0.05, 0.05]);
        assert_eq!(q.iter().map(|v| u32::from(*v)).sum::<u32>(), 65535);
    }

    #[test]
    fn integer_sources_are_kept_bit_for_bit() {
        let unorm16 = [40000u16, 20000, 5535, 0];
        assert_eq!(normalize_unorm16_weights(unorm16), unorm16);
        // unorm8 widened by 257 keeps its 255 sum.
        let unorm8 = [200u16 * 257, 55 * 257, 0, 0];
        assert_eq!(normalize_unorm16_weights(unorm8), unorm8);
        // A source that does not sum to one is normalised.
        let short = normalize_unorm16_weights([30000u16, 0, 0, 0]);
        assert_eq!(short, [65535, 0, 0, 0]);
    }

    #[test]
    fn f16_round_trips_every_finite_half() {
        for bits in 0..=u16::MAX {
            let exp = (bits >> 10) & 0x1f;
            if exp == 0x1f {
                continue;
            }
            assert_eq!(f32_to_f16_round(f16_bits_to_f32(bits)), bits, "{bits:#06x}");
        }
    }

    #[test]
    fn f16_rounds_to_nearest_even() {
        // 1 + 2^-11 is halfway between 1 and the next half (1 + 2^-10): ties to even (1).
        assert_eq!(f32_to_f16_round(1.0 + 2f32.powi(-11)), 0x3c00);
        // Just above the halfway point rounds up.
        assert_eq!(f32_to_f16_round(1.0 + 2f32.powi(-11) + 2f32.powi(-20)), 0x3c01);
        assert_eq!(f32_to_f16_round(65504.0), 0x7bff);
        assert_eq!(f32_to_f16_round(65512.0), 0x7bff);
        assert_eq!(f32_to_f16_round(65520.0), 0x7c00);
        assert_eq!(f32_to_f16_round(1.0e6), 0x7c00);
        assert_eq!(f32_to_f16_round(-0.0), 0x8000);
        // Smallest subnormal half.
        assert_eq!(f32_to_f16_round(2f32.powi(-24)), 0x0001);
        assert_eq!(f32_to_f16_round(2f32.powi(-26)), 0x0000);
    }

    #[test]
    fn joint_width_follows_the_joint_count() {
        assert_eq!(CookedJointWidth::for_joint_count(67), CookedJointWidth::U8);
        assert_eq!(CookedJointWidth::for_joint_count(256), CookedJointWidth::U8);
        assert_eq!(CookedJointWidth::for_joint_count(257), CookedJointWidth::U16);
    }

    #[test]
    fn top4_keeps_the_strongest_of_eight() {
        let skin = CpuSkin {
            joint_names: (0..8).map(|i| format!("j{i}")).collect(),
            joints: vec![[0, 1, 2, 3]],
            weights: vec![[1000, 20000, 1000, 1000]],
            joints1: vec![[4, 5, 6, 7]],
            weights1: vec![[30000, 1000, 10535, 1000]],
            eight_influences: false,
        };
        let (joints, weights) = skin.top4(0).expect("weights");
        assert_eq!(joints, [4, 1, 6, 0]);
        let sum: f32 = weights.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
    }
}
