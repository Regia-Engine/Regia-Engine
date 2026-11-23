// SPDX-License-Identifier: MPL-2.0
//! The cooked convex hull set (`FCHS`): the output of the importer's `collision: ConvexDecomposition`
//! setting, one set per mesh primitive (sub-asset `collision/<node>/primitive_<i>`).
//!
//! Layout, little endian: magic `FCHS`, `format_version: u32`, `hull_count: u32`, then per hull
//! `vertex_count: u32` followed by `vertex_count` times three `f32`. Only vertices are stored; the
//! collider (`ColliderShape::ConvexHull`) takes points, and the triangles are rebuilt on load.

use crate::ColliderShape;
use crate::convex_decomposition::{ConvexHull, compound_shape};
use regia_core::math::Vec3;

/// Type path of the cooked hull set sub-asset.
pub const HULL_SET_TYPE_PATH: &str = "regia_physics::HullSet";
/// Current `FCHS` format version.
pub const HULL_SET_FORMAT_VERSION: u32 = 1;
/// Most hulls a set holds (the decomposition's own limit).
pub const MAX_SET_HULLS: u32 = 64;
/// Most vertices a stored hull holds.
pub const MAX_SET_HULL_VERTICES: u32 = 4096;

const MAGIC: [u8; 4] = *b"FCHS";

/// Why a hull set did not decode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HullSetError {
    /// The bytes are shorter than the layout needs.
    #[error("hull set is truncated")]
    Truncated,
    /// The magic is not `FCHS`.
    #[error("not a hull set (bad magic)")]
    BadMagic,
    /// The format version is not one this build reads.
    #[error("unsupported hull set format version {0}")]
    Version(u32),
    /// A count is over its limit, or a hull has fewer than four vertices.
    #[error("hull set count out of range")]
    Count,
    /// A vertex is not finite.
    #[error("hull set has a non-finite vertex")]
    NonFinite,
    /// Bytes remain after the last hull.
    #[error("hull set has trailing bytes")]
    Trailing,
}

/// Convex hulls of one mesh primitive, in the primitive's object space.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HullSet {
    /// The hulls' vertices, one list per hull.
    pub hulls: Vec<Vec<Vec3>>,
}

impl HullSet {
    /// A set from decomposition hulls.
    pub fn from_hulls(hulls: &[ConvexHull]) -> Self {
        Self {
            hulls: hulls.iter().map(|h| h.vertices.clone()).collect(),
        }
    }

    /// Encodes the set (`FCHS` v1).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&HULL_SET_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&(self.hulls.len() as u32).to_le_bytes());
        for hull in &self.hulls {
            out.extend_from_slice(&(hull.len() as u32).to_le_bytes());
            for v in hull {
                for c in v.to_array() {
                    out.extend_from_slice(&c.to_le_bytes());
                }
            }
        }
        out
    }

    /// Decodes a set, checking every count and vertex.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, HullSetError> {
        let mut at = 0usize;
        let mut take = |n: usize| -> Result<&[u8], HullSetError> {
            let end = at.checked_add(n).ok_or(HullSetError::Truncated)?;
            let s = bytes.get(at..end).ok_or(HullSetError::Truncated)?;
            at = end;
            Ok(s)
        };
        let u32_at = |s: &[u8]| u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
        if take(4)? != MAGIC {
            return Err(HullSetError::BadMagic);
        }
        let version = u32_at(take(4)?);
        if version != HULL_SET_FORMAT_VERSION {
            return Err(HullSetError::Version(version));
        }
        let count = u32_at(take(4)?);
        if count > MAX_SET_HULLS {
            return Err(HullSetError::Count);
        }
        let mut hulls = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let n = u32_at(take(4)?);
            if !(4..=MAX_SET_HULL_VERTICES).contains(&n) {
                return Err(HullSetError::Count);
            }
            let raw = take(n as usize * 12)?;
            let mut verts = Vec::with_capacity(n as usize);
            for v in raw.as_chunks::<12>().0 {
                let f = |i: usize| f32::from_le_bytes([v[i], v[i + 1], v[i + 2], v[i + 3]]);
                let p = Vec3::new(f(0), f(4), f(8));
                if !p.is_finite() {
                    return Err(HullSetError::NonFinite);
                }
                verts.push(p);
            }
            hulls.push(verts);
        }
        if at != bytes.len() {
            return Err(HullSetError::Trailing);
        }
        Ok(Self { hulls })
    }

    /// The set as one collider (`None` when empty; see [`compound_shape`]'s rules).
    pub fn to_collider_shape(&self) -> Option<ColliderShape> {
        let hulls: Vec<ConvexHull> = self
            .hulls
            .iter()
            .map(|vertices| ConvexHull {
                vertices: vertices.clone(),
                triangles: Vec::new(),
            })
            .collect();
        compound_shape(&hulls)
    }
}

/// A description of the `FCHS` payload layout (it is not serde), for `cargo xtask check-formats`:
/// bump [`HULL_SET_FORMAT_VERSION`] when this changes.
#[must_use]
pub fn hull_set_schema_fingerprint() -> String {
    format!(
        "FCHS;v{HULL_SET_FORMAT_VERSION};u32 hull_count(<={MAX_SET_HULLS});\
         hull:[u32 vertex_count(4..={MAX_SET_HULL_VERTICES});f32x3 vertices]"
    )
}

/// The `from` versions of the hull set's migration steps (none: v1 is the first version, and a
/// hull set is derived data the importer re-cooks).
#[must_use]
pub fn hull_set_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The payload version of a cooked hull-set file (`FCKD` wrapper), `Ok(None)` when the file is
/// another cooked type or not a cooked file (for `regia upgrade-project`).
///
/// # Errors
/// A hull-set file whose payload is shorter than its header or has a bad magic.
pub fn cooked_hull_set_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let Ok((header, payload)) = regia_asset::decode_cooked_file(bytes) else {
        return Ok(None);
    };
    if header.type_tag != regia_core::id::TypePathHash::of(HULL_SET_TYPE_PATH) {
        return Ok(None);
    }
    match payload {
        [m0, m1, m2, m3, v0, v1, v2, v3, ..] if [*m0, *m1, *m2, *m3] == MAGIC => {
            Ok(Some(u32::from_le_bytes([*v0, *v1, *v2, *v3])))
        }
        _ => Err("cooked hull set has a bad magic".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tetra(offset: f32) -> Vec<Vec3> {
        vec![
            Vec3::new(offset, 0.0, 0.0),
            Vec3::new(offset + 1.0, 0.0, 0.0),
            Vec3::new(offset, 1.0, 0.0),
            Vec3::new(offset, 0.0, 1.0),
        ]
    }

    #[test]
    fn round_trips() {
        let set = HullSet {
            hulls: vec![tetra(0.0), tetra(2.0)],
        };
        assert_eq!(HullSet::from_bytes(&set.to_bytes()), Ok(set));
    }

    #[test]
    fn rejects_bad_input() {
        let good = HullSet {
            hulls: vec![tetra(0.0)],
        }
        .to_bytes();
        assert_eq!(
            HullSet::from_bytes(&good[..good.len() - 1]),
            Err(HullSetError::Truncated)
        );
        let mut bad = good.clone();
        bad[0] = b'X';
        assert_eq!(HullSet::from_bytes(&bad), Err(HullSetError::BadMagic));
        let mut bad = good.clone();
        bad[4] = 9;
        assert_eq!(HullSet::from_bytes(&bad), Err(HullSetError::Version(9)));
        let mut bad = good.clone();
        bad.push(0);
        assert_eq!(HullSet::from_bytes(&bad), Err(HullSetError::Trailing));
        let mut bad = good.clone();
        bad[8..12].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(HullSet::from_bytes(&bad), Err(HullSetError::Count));
        let mut bad = good.clone();
        bad[16..20].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(HullSet::from_bytes(&bad), Err(HullSetError::NonFinite));
    }

    #[test]
    fn the_cooked_file_probe_reads_the_payload_version_and_skips_other_types() {
        let set = HullSet {
            hulls: vec![tetra(0.0)],
        };
        let cook = |type_path: &str, payload: &[u8]| {
            let header = regia_asset::CookedHeader {
                format_version: regia_asset::COOKED_FORMAT_VERSION,
                type_tag: regia_core::id::TypePathHash::of(type_path),
                asset_id: regia_asset::AssetId(regia_core::id::Guid::from_u128(7)),
                importer_version: 1,
                dependencies: Vec::new(),
                payload_len: payload.len() as u64,
            };
            regia_asset::encode_cooked_file(&header, payload).expect("encode")
        };
        let bytes = set.to_bytes();
        assert_eq!(
            cooked_hull_set_file_version(&cook(HULL_SET_TYPE_PATH, &bytes)),
            Ok(Some(1))
        );
        assert_eq!(
            cooked_hull_set_file_version(&cook("regia_asset::Mesh", &bytes)),
            Ok(None)
        );
        assert_eq!(cooked_hull_set_file_version(b"not a cooked file"), Ok(None));
        let broken = cook(HULL_SET_TYPE_PATH, b"XXXX");
        assert!(cooked_hull_set_file_version(&broken).is_err());
        assert!(hull_set_schema_fingerprint().starts_with("FCHS;v1;"));
        assert!(hull_set_migration_sources().is_empty());
    }

    #[test]
    fn one_hull_is_a_hull_shape_and_many_are_a_compound() {
        let one = HullSet {
            hulls: vec![tetra(0.0)],
        };
        assert!(matches!(
            one.to_collider_shape(),
            Some(ColliderShape::ConvexHull { .. })
        ));
        let two = HullSet {
            hulls: vec![tetra(0.0), tetra(2.0)],
        };
        assert!(matches!(
            two.to_collider_shape(),
            Some(ColliderShape::Compound { .. })
        ));
        assert!(HullSet::default().to_collider_shape().is_none());
    }
}
