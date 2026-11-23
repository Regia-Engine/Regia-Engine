// SPDX-License-Identifier: MPL-2.0
//! Cooked asset loaders (`docs/specs/12-assets.md` § 3).

use regia_core::id::TypePathHash;
use thiserror::Error;

use crate::cooked::{CookedImage, CookedMesh, CookedModel, CookedTexture, CpuMesh, Heightfield};
use crate::cooked_header::{CookedHeader, decode_cooked_file};
use crate::id::AssetId;
use crate::material::CookedStandardMaterial;

/// Tag identifying a cooked payload type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssetTypeTag(pub TypePathHash);

impl AssetTypeTag {
    /// Tag for [`CookedMesh`] (primary glTF mesh payload).
    pub fn mesh() -> Self {
        Self(TypePathHash::of("regia_asset::CookedMesh"))
    }

    /// Tag for a GPU-ready mesh payload ([`crate::gpu_mesh`], desktop paks).
    pub fn gpu_mesh() -> Self {
        Self(TypePathHash::of(crate::gpu_mesh::GPU_MESH_TYPE_PATH))
    }

    /// Legacy tag for [`CpuMesh`] (pre–M4 round 3 cooks).
    pub fn mesh_legacy_cpu() -> Self {
        Self(TypePathHash::of("regia_asset::CpuMesh"))
    }

    /// Tag for [`CookedImage`].
    pub fn image() -> Self {
        Self(TypePathHash::of("regia_asset::CookedImage"))
    }

    /// Tag for [`CookedTexture`].
    pub fn texture() -> Self {
        Self(TypePathHash::of("regia_asset::CookedTexture"))
    }

    /// Tag for [`Heightfield`].
    pub fn heightfield() -> Self {
        Self(TypePathHash::of("regia_asset::Heightfield"))
    }

    /// Tag for [`CookedModel`].
    pub fn model() -> Self {
        Self(TypePathHash::of("regia_asset::CookedModel"))
    }

    /// Tag for [`CookedStandardMaterial`].
    pub fn standard_material() -> Self {
        Self(TypePathHash::of("regia_asset::CookedStandardMaterial"))
    }
}

/// Decode a cooked file's payload into a known type.
#[derive(Debug, Error)]
pub enum AssetLoadError {
    /// Cooked wrapper error.
    #[error("cooked file error: {0}")]
    Cooked(#[from] crate::cooked_header::CookedIoError),

    /// Type tag mismatch.
    #[error("asset {id} has type tag {found:#x}, expected {expected:#x}")]
    TypeMismatch {
        /// Asset id.
        id: AssetId,
        /// Tag in file.
        found: u64,
        /// Expected tag.
        expected: u64,
    },

    /// Mesh decode/validation.
    #[error("mesh load error: {0}")]
    Mesh(#[from] crate::cooked::MeshLoadError),

    /// Cooked mesh decode/validation.
    #[error("cooked mesh load error: {0}")]
    CookedMesh(#[from] crate::cooked::CookedMeshLoadError),

    /// GPU-ready mesh payload decode/validation.
    #[error("GPU-ready mesh load error: {0}")]
    GpuMesh(#[from] crate::gpu_mesh::GpuMeshError),

    /// Cooked mesh unpack validation.
    #[error("cooked mesh validation error: {0}")]
    CookedMeshValidation(#[from] crate::cooked::CookedMeshValidationError),

    /// Image decode/validation.
    #[error("image load error: {0}")]
    Image(#[from] crate::cooked::ImageLoadError),

    /// Texture decode/validation.
    #[error("texture load error: {0}")]
    Texture(#[from] crate::cooked::TextureLoadError),

    /// Heightfield decode/validation.
    #[error("heightfield load error: {0}")]
    Heightfield(#[from] crate::cooked::HeightfieldLoadError),

    /// Cooked model decode.
    #[error("model load error: {0}")]
    Model(#[from] crate::cooked::CookedModelLoadError),

    /// Material decode.
    #[error("material load error: {0}")]
    Material(#[from] crate::material::MaterialLoadError),

    /// Postcard decode for payloads that do not have their own load error.
    ///
    /// Skeleton and clip loaders convert `postcard::Error` through this variant.
    /// Standard materials use [`Self::Material`].
    #[error("postcard error: {0}")]
    Postcard(#[from] postcard::Error),

    /// The payload decoded but breaks its format's rules.
    #[error("invalid payload: {0}")]
    Invalid(String),

    /// A content error while opening the server: a pak the content manifest lists failed its
    /// size or BLAKE3 check at startup (`chunk_integrity_mismatch`), or could not be read. The
    /// agent code is [`crate::content::ContentError::reason`] (ADR-7979).
    #[error("content error: {0}")]
    Content(#[from] crate::content::ContentError),

    /// IO while loading.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The path manifest lists no asset at this path ([`crate::AssetServer::load_path`]).
    #[error("no asset at path `{0}` (asset_not_found)")]
    NotFound(String),

    /// No [`crate::AssetLoader`] is registered for this asset type ([`crate::AssetServer::load`]).
    #[error("no loader registered for `{0}` (asset_no_loader)")]
    NoLoader(String),
}

impl AssetLoadError {
    /// A stable snake-case code for the error (agent protocol and logs): `asset_not_found`,
    /// `asset_no_loader`, `asset_type_mismatch`, `asset_io`, or `asset_decode` for the payload
    /// errors.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "asset_not_found",
            Self::NoLoader(_) => "asset_no_loader",
            Self::TypeMismatch { .. } => "asset_type_mismatch",
            Self::Io(_) | Self::Cooked(crate::cooked_header::CookedIoError::Io(_)) => "asset_io",
            _ => "asset_decode",
        }
    }
}

/// Loaded mesh with header metadata.
pub struct LoadedMesh {
    /// Header from the cooked file.
    pub header: CookedHeader,
    /// Decoded mesh.
    pub mesh: CpuMesh,
}

/// Load a mesh cooked file from bytes.
pub fn load_mesh_bytes(bytes: &[u8]) -> Result<LoadedMesh, AssetLoadError> {
    let (header, payload) = decode_cooked_file(bytes)?;
    let mesh = if header.type_tag == AssetTypeTag::mesh_legacy_cpu().0 {
        CpuMesh::from_postcard(payload)?
    } else if header.type_tag == AssetTypeTag::mesh().0 {
        CookedMesh::from_postcard(payload)?.to_cpu_mesh()?
    } else if header.type_tag == AssetTypeTag::gpu_mesh().0 {
        crate::gpu_mesh::GpuMeshView::parse(payload)?.to_cpu_mesh()?
    } else {
        return Err(AssetLoadError::TypeMismatch {
            id: header.asset_id,
            found: header.type_tag.get(),
            expected: AssetTypeTag::mesh().0.get(),
        });
    };
    Ok(LoadedMesh { header, mesh })
}

/// Load a texture cooked file from bytes.
pub fn load_texture_bytes(bytes: &[u8]) -> Result<(CookedHeader, CookedTexture), AssetLoadError> {
    let (header, payload) = decode_cooked_file(bytes)?;
    if header.type_tag != AssetTypeTag::texture().0 {
        return Err(AssetLoadError::TypeMismatch {
            id: header.asset_id,
            found: header.type_tag.get(),
            expected: AssetTypeTag::texture().0.get(),
        });
    }
    let tex = CookedTexture::from_postcard(payload)?;
    Ok((header, tex))
}

/// [`load_texture_bytes`] without copying the KTX2 bytes: the texture's [`CookedTexture::ktx2`]
/// is a view of `bytes` (for a pak entry stored uncompressed, of the mapped pak itself).
///
/// # Errors
///
/// As [`load_texture_bytes`].
pub fn load_texture_shared(
    bytes: &crate::pak::PakPayload,
) -> Result<(CookedHeader, CookedTexture), AssetLoadError> {
    let (header, payload) = decode_cooked_file(bytes)?;
    if header.type_tag != AssetTypeTag::texture().0 {
        return Err(AssetLoadError::TypeMismatch {
            id: header.asset_id,
            found: header.type_tag.get(),
            expected: AssetTypeTag::texture().0.get(),
        });
    }
    let payload = bytes
        .subslice(payload)
        .unwrap_or_else(|| crate::pak::PakPayload::from(payload));
    let tex = CookedTexture::from_postcard_shared(&payload)?;
    Ok((header, tex))
}

/// Load an image cooked file from bytes.
pub fn load_image_bytes(bytes: &[u8]) -> Result<(CookedHeader, CookedImage), AssetLoadError> {
    let (header, payload) = decode_cooked_file(bytes)?;
    if header.type_tag != AssetTypeTag::image().0 {
        return Err(AssetLoadError::TypeMismatch {
            id: header.asset_id,
            found: header.type_tag.get(),
            expected: AssetTypeTag::image().0.get(),
        });
    }
    let image = CookedImage::from_postcard(payload)?;
    Ok((header, image))
}

/// Load a heightfield cooked file from bytes.
pub fn load_heightfield_bytes(bytes: &[u8]) -> Result<(CookedHeader, Heightfield), AssetLoadError> {
    let (header, payload) = decode_cooked_file(bytes)?;
    if header.type_tag != AssetTypeTag::heightfield().0 {
        return Err(AssetLoadError::TypeMismatch {
            id: header.asset_id,
            found: header.type_tag.get(),
            expected: AssetTypeTag::heightfield().0.get(),
        });
    }
    let hf = Heightfield::from_postcard(payload)?;
    Ok((header, hf))
}

/// Load a standard material cooked file from bytes.
pub fn load_material_bytes(bytes: &[u8]) -> Result<(CookedHeader, CookedStandardMaterial), AssetLoadError> {
    let (header, payload) = decode_cooked_file(bytes)?;
    if header.type_tag != AssetTypeTag::standard_material().0 {
        return Err(AssetLoadError::TypeMismatch {
            id: header.asset_id,
            found: header.type_tag.get(),
            expected: AssetTypeTag::standard_material().0.get(),
        });
    }
    let mat = CookedStandardMaterial::from_postcard(payload)?;
    Ok((header, mat))
}

/// Load a cooked model (an imported glTF's `model` sub-asset) from bytes.
pub fn load_model_bytes(bytes: &[u8]) -> Result<(CookedHeader, CookedModel), AssetLoadError> {
    let (header, payload) = decode_cooked_file(bytes)?;
    if header.type_tag != AssetTypeTag::model().0 {
        return Err(AssetLoadError::TypeMismatch {
            id: header.asset_id,
            found: header.type_tag.get(),
            expected: AssetTypeTag::model().0.get(),
        });
    }
    let model = CookedModel::from_postcard(payload)?;
    Ok((header, model))
}

/// Cooked file name for an asset id in the cache directory.
pub fn cooked_file_name(id: AssetId) -> String {
    format!("{}.fckd", id)
}

#[cfg(test)]
mod model_tests {
    use super::*;
    use crate::cooked::{COOKED_MODEL_VERSION, CookedModelLoadError, CookedModelPart};
    use crate::cooked_header::{COOKED_FORMAT_VERSION, encode_cooked_file, pack_importer_version};

    fn model() -> CookedModel {
        CookedModel {
            version: COOKED_MODEL_VERSION,
            parts: vec![
                CookedModelPart {
                    mesh: AssetId::new_random(),
                    material: Some(AssetId::new_random()),
                    node: "Hull".into(),
                },
                CookedModelPart {
                    mesh: AssetId::new_random(),
                    material: None,
                    node: "Antenna".into(),
                },
            ],
        }
    }

    fn file_of(tag: TypePathHash, payload: &[u8]) -> Vec<u8> {
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: tag,
            asset_id: AssetId::new_random(),
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        encode_cooked_file(&header, payload).expect("encode")
    }

    #[test]
    fn a_cooked_model_round_trips_through_postcard() {
        let model = model();
        let bytes = model.to_postcard().expect("serialize");
        assert_eq!(CookedModel::from_postcard(&bytes).expect("decode"), model);
    }

    #[test]
    fn a_future_model_version_is_rejected_before_the_parts_are_read() {
        let mut future = model();
        future.version = COOKED_MODEL_VERSION + 1;
        let bytes = future.to_postcard().expect("serialize");
        match CookedModel::from_postcard(&bytes) {
            Err(CookedModelLoadError::UnsupportedVersion { found, supported }) => {
                assert_eq!(found, COOKED_MODEL_VERSION + 1);
                assert_eq!(supported, COOKED_MODEL_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
        // A version-only prefix is enough to be refused with the version error, not a decode error.
        let prefix = postcard::to_allocvec(&(COOKED_MODEL_VERSION + 1)).expect("prefix");
        assert!(matches!(
            CookedModel::from_postcard(&prefix),
            Err(CookedModelLoadError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn truncated_model_bytes_are_a_postcard_error() {
        let bytes = model().to_postcard().expect("serialize");
        assert!(matches!(
            CookedModel::from_postcard(&bytes[..bytes.len() - 3]),
            Err(CookedModelLoadError::Postcard(_))
        ));
        assert!(matches!(
            CookedModel::from_postcard(&[]),
            Err(CookedModelLoadError::Postcard(_))
        ));
    }

    #[test]
    fn load_model_bytes_returns_the_header_and_the_model() {
        let model = model();
        let file = file_of(AssetTypeTag::model().0, &model.to_postcard().expect("serialize"));
        let (header, loaded) = load_model_bytes(&file).expect("loads");
        assert_eq!(header.type_tag, AssetTypeTag::model().0);
        assert_eq!(loaded, model);
    }

    #[test]
    fn load_model_bytes_refuses_another_type_tag() {
        let payload = model().to_postcard().expect("serialize");
        let file = file_of(AssetTypeTag::mesh().0, &payload);
        match load_model_bytes(&file) {
            Err(AssetLoadError::TypeMismatch { found, expected, .. }) => {
                assert_eq!(found, AssetTypeTag::mesh().0.get());
                assert_eq!(expected, AssetTypeTag::model().0.get());
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn load_model_bytes_reports_a_newer_model_payload_and_a_bad_wrapper() {
        let mut future = model();
        future.version = COOKED_MODEL_VERSION + 1;
        let file = file_of(AssetTypeTag::model().0, &future.to_postcard().expect("serialize"));
        assert!(matches!(
            load_model_bytes(&file),
            Err(AssetLoadError::Model(
                CookedModelLoadError::UnsupportedVersion { .. }
            ))
        ));
        assert!(matches!(
            load_model_bytes(b"nope"),
            Err(AssetLoadError::Cooked(_))
        ));
    }
}
