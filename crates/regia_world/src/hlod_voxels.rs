// SPDX-License-Identifier: MPL-2.0
//! The HLOD voxel table of a streamed level (`.wvoxels`, ADR-3253, spec 78 § 11.6): which tree nodes
//! carry a voxel proxy of the terrain scatter under them, where each sits and what it costs.
//!
//! It is a sidecar of the world index, written only when the level's cook saw scatter, so a level
//! without scatter cooks exactly the files it cooked before (the world index format is unchanged).
//! Each voxel proxy itself is an ordinary cooked `VoxelFarProxy` (`regia_render::VoxelFarProxy`,
//! format `voxel-far`), one per node, under [`hlod_voxel_proxy_id`].
//!
//! The reader trusts nothing: a bad table is a typed error, and the runtime then draws the level
//! without voxel proxies.

use regia_asset::{
    AssetId, COOKED_FORMAT_VERSION, CookedHeader, decode_cooked_file, encode_cooked_file,
    pack_importer_version,
};
use regia_core::id::{Guid, TypePathHash};
use regia_scene::level::LevelId;
use serde::{Deserialize, Serialize};

use crate::format::WorldFormatError;

/// The current `.wvoxels` format version.
pub const HLOD_VOXELS_FORMAT_VERSION: u32 = 1;
/// The cooked header's type path of an HLOD voxel table.
pub const HLOD_VOXELS_TYPE_PATH: &str = "regia_world::hlod_voxels::HlodVoxels";
/// The cooked header's type path of one voxel proxy (the `voxel-far` format).
pub const HLOD_VOXEL_PROXY_TYPE_PATH: &str = "regia_render::VoxelFarProxy";
/// Most nodes a table may name: far above any tree the cook builds (a 1024 x 1024-cell world has
/// about 1.4 million nodes), low enough that a corrupt count cannot ask for gigabytes.
pub const MAX_HLOD_VOXEL_NODES: usize = 1 << 22;

/// The voxel proxies of a level's HLOD tree, in tree order (the order of `HlodIndex::node_proxies`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HlodVoxels {
    /// [`HLOD_VOXELS_FORMAT_VERSION`].
    pub format_version: u32,
    /// The level.
    pub level: Guid,
    /// Voxels along the longest axis of a node's scatter at its finest level, as cooked.
    pub resolution: u32,
    /// One entry per tree node; `None` for a node with no scatter under it. May be shorter than the
    /// node list: a missing entry is `None`.
    pub nodes: Vec<Option<HlodVoxelNode>>,
}

/// One node's voxel proxy.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HlodVoxelNode {
    /// The cooked `VoxelFarProxy` ([`hlod_voxel_proxy_id`]).
    pub proxy: AssetId,
    /// Its position in the origin space: the proxy's coordinates are relative to this cell's center.
    pub anchor: [i64; 3],
    /// Scatter instances voxelized into it.
    pub instances: u32,
    /// Occupied voxels at its finest level (the level drawn).
    pub voxels: u32,
    /// Edge of one voxel at its finest level, meters.
    pub voxel_size_m: f32,
    /// Bytes of the cooked proxy file.
    pub bytes: u64,
}

impl HlodVoxels {
    /// Node `index`'s voxel proxy, if it has one.
    #[must_use]
    pub fn node(&self, index: usize) -> Option<&HlodVoxelNode> {
        self.nodes.get(index).and_then(Option::as_ref)
    }

    /// Nodes with a voxel proxy.
    #[must_use]
    pub fn count(&self) -> usize {
        self.nodes.iter().flatten().count()
    }

    /// The cooked file of this table.
    ///
    /// # Errors
    ///
    /// The table does not encode.
    pub fn to_bytes(&self) -> Result<Vec<u8>, WorldFormatError> {
        let payload = postcard::to_allocvec(self).map_err(|e| WorldFormatError::Decode(e.to_string()))?;
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: TypePathHash::of(HLOD_VOXELS_TYPE_PATH),
            asset_id: hlod_voxels_id(LevelId(self.level)),
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: self.nodes.iter().flatten().map(|n| n.proxy).collect(),
            payload_len: payload.len() as u64,
        };
        Ok(encode_cooked_file(&header, &payload)?)
    }

    /// Reads the cooked table of `level`. Never panics on bad bytes.
    ///
    /// # Errors
    ///
    /// The header, asset id, version or payload is wrong.
    pub fn from_bytes(bytes: &[u8], level: LevelId) -> Result<Self, WorldFormatError> {
        let (header, body) = decode_cooked_file(bytes)?;
        if header.type_tag != TypePathHash::of(HLOD_VOXELS_TYPE_PATH) {
            return Err(WorldFormatError::Corrupt(format!(
                "not a {HLOD_VOXELS_TYPE_PATH}"
            )));
        }
        if header.asset_id != hlod_voxels_id(level) {
            return Err(WorldFormatError::Corrupt(format!(
                "file holds asset {:?}, expected {:?}",
                header.asset_id,
                hlod_voxels_id(level)
            )));
        }
        let table = Self::decode_body(body)?;
        if table.level != level.0 {
            return Err(WorldFormatError::Corrupt(format!(
                "payload names level {}, expected {}",
                table.level, level.0
            )));
        }
        Ok(table)
    }

    /// Reads a cooked table of any level (the one the file names), for tools and fuzzing.
    ///
    /// # Errors
    ///
    /// As [`Self::from_bytes`], with the asset id checked against the level the payload names.
    pub fn decode_any(bytes: &[u8]) -> Result<Self, WorldFormatError> {
        let (header, body) = decode_cooked_file(bytes)?;
        if header.type_tag != TypePathHash::of(HLOD_VOXELS_TYPE_PATH) {
            return Err(WorldFormatError::Corrupt(format!(
                "not a {HLOD_VOXELS_TYPE_PATH}"
            )));
        }
        let table = Self::decode_body(body)?;
        if header.asset_id != hlod_voxels_id(LevelId(table.level)) {
            return Err(WorldFormatError::Corrupt(format!(
                "file holds asset {:?}, which is not the voxel table of level {}",
                header.asset_id, table.level
            )));
        }
        Ok(table)
    }

    fn decode_body(body: &[u8]) -> Result<Self, WorldFormatError> {
        let (found, _) = postcard::take_from_bytes::<u32>(body)
            .map_err(|e| WorldFormatError::Decode(format!("format version: {e}")))?;
        if found > HLOD_VOXELS_FORMAT_VERSION {
            return Err(WorldFormatError::Newer {
                found,
                supported: HLOD_VOXELS_FORMAT_VERSION,
            });
        }
        let table: Self = postcard::from_bytes(body).map_err(|e| WorldFormatError::Decode(e.to_string()))?;
        table.validate()?;
        Ok(table)
    }

    /// Checks what the runtime relies on: a bounded node list and finite, positive voxel sizes.
    ///
    /// # Errors
    ///
    /// [`WorldFormatError::Corrupt`] naming the first bad entry.
    pub fn validate(&self) -> Result<(), WorldFormatError> {
        if self.nodes.len() > MAX_HLOD_VOXEL_NODES {
            return Err(WorldFormatError::Corrupt(format!(
                "{} nodes, over the {MAX_HLOD_VOXEL_NODES} limit",
                self.nodes.len()
            )));
        }
        for (index, node) in self.nodes.iter().enumerate() {
            if let Some(node) = node
                && !(node.voxel_size_m.is_finite() && node.voxel_size_m > 0.0)
            {
                return Err(WorldFormatError::Corrupt(format!(
                    "node {index}: voxel size {} is not a positive length",
                    node.voxel_size_m
                )));
            }
        }
        Ok(())
    }
}

/// The asset id of a level's HLOD voxel table.
#[must_use]
pub fn hlod_voxels_id(level: LevelId) -> AssetId {
    AssetId::sub_asset(AssetId(level.0), "hlod_voxels")
}

/// The asset id of node `node`'s voxel proxy.
#[must_use]
pub fn hlod_voxel_proxy_id(level: LevelId, node: u32) -> AssetId {
    AssetId::sub_asset(AssetId(level.0), &format!("hlod/{node}/voxels"))
}

/// The `format_version` of a cooked HLOD voxel table, or `Ok(None)` when the file is a cooked file
/// of another type or its header does not read (for `regia upgrade-project`, which probes every
/// cooked file).
///
/// # Errors
///
/// A voxel table payload has no readable version.
pub fn hlod_voxels_file_version(bytes: &[u8]) -> Result<Option<u32>, WorldFormatError> {
    let Ok((header, body)) = decode_cooked_file(bytes) else {
        return Ok(None);
    };
    if header.type_tag != TypePathHash::of(HLOD_VOXELS_TYPE_PATH) {
        return Ok(None);
    }
    let (found, _) = postcard::take_from_bytes::<u32>(body)
        .map_err(|e| WorldFormatError::Decode(format!("format version: {e}")))?;
    Ok(Some(found))
}

/// The serde field list of `.wvoxels`, for `check-formats`.
#[must_use]
pub fn hlod_voxels_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<HlodVoxels>(),
        regia_core::schema::serde_field_names::<HlodVoxelNode>(),
    ])
}

/// The `from` versions of `.wvoxels`'s migration steps, for `check-formats`: none yet (version 1 is
/// the first).
#[must_use]
pub fn hlod_voxels_migration_sources() -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level() -> LevelId {
        LevelId(Guid::from_u128(0x3253))
    }

    fn table() -> HlodVoxels {
        HlodVoxels {
            format_version: HLOD_VOXELS_FORMAT_VERSION,
            level: level().0,
            resolution: 32,
            nodes: vec![
                None,
                Some(HlodVoxelNode {
                    proxy: hlod_voxel_proxy_id(level(), 1),
                    anchor: [1, 0, -2],
                    instances: 40,
                    voxels: 900,
                    voxel_size_m: 2.0,
                    bytes: 12_345,
                }),
            ],
        }
    }

    #[test]
    fn a_table_round_trips_and_names_its_proxies_as_dependencies() {
        let bytes = table().to_bytes().expect("encode");
        assert_eq!(HlodVoxels::from_bytes(&bytes, level()).expect("decode"), table());
        assert_eq!(HlodVoxels::decode_any(&bytes).expect("any"), table());
        let (header, _) = decode_cooked_file(&bytes).expect("header");
        assert_eq!(header.dependencies, vec![hlod_voxel_proxy_id(level(), 1)]);
        assert_eq!(hlod_voxels_file_version(&bytes).expect("probe"), Some(1));
        assert_eq!(table().count(), 1);
        assert!(table().node(0).is_none() && table().node(5).is_none());
    }

    #[test]
    fn bad_tables_are_typed_errors_never_panics() {
        let bytes = table().to_bytes().expect("encode");
        assert!(HlodVoxels::from_bytes(&bytes, LevelId(Guid::from_u128(1))).is_err());
        for cut in [0, 8, bytes.len() / 2, bytes.len() - 1] {
            assert!(
                HlodVoxels::from_bytes(&bytes[..cut], level()).is_err(),
                "cut {cut}"
            );
        }
        let mut newer = table();
        newer.format_version = HLOD_VOXELS_FORMAT_VERSION + 1;
        let newer = newer.to_bytes().expect("encode");
        assert!(matches!(
            HlodVoxels::from_bytes(&newer, level()),
            Err(WorldFormatError::Newer { .. })
        ));
        let mut bad = table();
        if let Some(Some(node)) = bad.nodes.get_mut(1) {
            node.voxel_size_m = f32::NAN;
        }
        let bad = bad.to_bytes().expect("encode");
        assert!(matches!(
            HlodVoxels::from_bytes(&bad, level()),
            Err(WorldFormatError::Corrupt(_))
        ));
        // Another cooked type is not a table.
        assert_eq!(hlod_voxels_file_version(b"not cooked").expect("probe"), None);
    }

    #[test]
    fn a_payload_that_names_another_level_is_rejected() {
        // `to_bytes` derives the header id from the payload, so build the disagreeing file by hand:
        // the header claims `level()`, the payload names another level (what a bit flip produces).
        let mut other = table();
        other.level = Guid::from_u128(0xdead);
        let payload = postcard::to_allocvec(&other).expect("encode payload");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: TypePathHash::of(HLOD_VOXELS_TYPE_PATH),
            asset_id: hlod_voxels_id(level()),
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        let bytes = encode_cooked_file(&header, &payload).expect("encode file");
        assert!(matches!(
            HlodVoxels::from_bytes(&bytes, level()),
            Err(WorldFormatError::Corrupt(_))
        ));
        assert!(HlodVoxels::decode_any(&bytes).is_err());
    }
}
