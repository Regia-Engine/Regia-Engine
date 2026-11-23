// SPDX-License-Identifier: MPL-2.0
//! Custom data assets: `#[derive(Asset)]` types stored as `.data.ron` files (`docs/specs/10-core.md`
//! § 2.4, `docs/specs/14-scene-and-framework.md` Part C, `docs/formats/data-ron.md`, ADR-7232).
//!
//! A game declares a reflected struct as a data asset with `#[derive(Asset)]` (from
//! `regia_macros`) and registers it with [`RegisterDataAssetExt::register_data_asset`], or with
//! `#[reflect(Asset)]` next to `#[derive(Reflect)]`. Either way the type's registration carries
//! [`ReflectAsset`] type data, which is how tools (the editor, the agent protocol, the cook) find
//! every data asset type and build a default value of one without knowing the Rust type.
//!
//! A `.data.ron` file names its type by path and holds the value as reflected RON:
//!
//! ```ron
//! DataAsset(
//!     format_version: 1,
//!     type: "my_game::WeaponStats",
//!     value: (
//!         damage: 12,
//!         range_m: 40.0,
//!     ),
//! )
//! ```
//!
//! Missing fields take the type's defaults and unknown fields are warnings (the reflected RON rules
//! of spec 10 § 2.3). The cook keeps the text (the cook does not know game types) in a cooked file
//! tagged [`DATA_ASSET_TYPE_PATH`]; [`DataAssets::load_path`] reads it through the
//! [`AssetServer`](crate::AssetServer) and decodes it with the app's type registry.
//!
//! ```
//! use regia_app::prelude::*;
//! use regia_asset::data_asset::{Asset, DataAssetFile, ReflectAsset, RegisterDataAssetExt, data_asset_types};
//! use regia_ecs::reflect::TypePath;
//!
//! #[derive(Reflect, Default, Debug, PartialEq)]
//! #[reflect(Default)]
//! struct WeaponStats {
//!     damage: u32,
//!     range_m: f32,
//! }
//! impl Asset for WeaponStats {}
//!
//! let mut app = App::new();
//! app.register_data_asset::<WeaponStats>();
//! let registry = app.type_registry().read();
//! let path = WeaponStats::type_path();
//! assert!(data_asset_types(&registry).contains(&path));
//!
//! let text = format!("DataAsset(format_version: 1, type: \"{path}\", value: (damage: 12))");
//! let loaded = DataAssetFile::parse(&text, &registry).expect("loads");
//! let stats = loaded.value.downcast_ref::<WeaponStats>().expect("a WeaponStats");
//! assert_eq!(stats, &WeaponStats { damage: 12, range_m: 0.0 });
//!
//! // Written back canonically, it reads the same.
//! let again = DataAssetFile::to_ron(loaded.value.as_ref(), &registry).expect("writes");
//! let back = DataAssetFile::parse(&again, &registry).expect("loads again");
//! assert_eq!(back.value.downcast_ref::<WeaponStats>(), Some(stats));
//! # let _ = ReflectAsset::type_path;
//! ```

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_ecs::reflect::{
    CreateTypeData, FromReflect, GetTypeRegistration, PartialReflect, Reflect, TypePath, TypeRegistry,
};
use regia_ecs::serialize::ReflectCodec;
use serde::Deserialize;

use crate::id::AssetId;

/// The `format_version` this build writes and the newest it reads.
pub const DATA_ASSET_FORMAT_VERSION: u32 = 1;

/// The cooked type path of a `.data.ron` file (its payload is the file's UTF-8 RON text).
pub const DATA_ASSET_TYPE_PATH: &str = "regia_asset::DataAsset";

/// The file name suffix of a data asset.
pub const DATA_ASSET_SUFFIX: &str = ".data.ron";

/// A custom data asset type. Implement it with `#[derive(Asset)]` (`regia_macros`); the bounds are
/// what [`ReflectAsset`] needs: a reflected, `Default`-constructible value with a stable type path.
pub trait Asset:
    Reflect + FromReflect + TypePath + GetTypeRegistration + Default + Send + Sync + 'static
{
}

/// Type data of a data asset type (`#[reflect(Asset)]`, or added by
/// [`RegisterDataAssetExt::register_data_asset`]).
#[derive(Clone, Copy)]
pub struct ReflectAsset {
    type_path: &'static str,
    default: fn() -> Box<dyn Reflect>,
    from_reflect: fn(&dyn PartialReflect) -> Option<Box<dyn Reflect>>,
}

impl std::fmt::Debug for ReflectAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReflectAsset")
            .field("type_path", &self.type_path)
            .finish()
    }
}

impl ReflectAsset {
    /// The type's full path (the `type` of its `.data.ron` files).
    #[must_use]
    pub fn type_path(&self) -> &'static str {
        self.type_path
    }

    /// A default value of the type (what "New data asset" starts from).
    #[must_use]
    pub fn default_value(&self) -> Box<dyn Reflect> {
        (self.default)()
    }

    /// Builds the concrete type from a (dynamic or concrete) reflected value.
    #[must_use]
    pub fn from_reflect(&self, value: &dyn PartialReflect) -> Option<Box<dyn Reflect>> {
        (self.from_reflect)(value)
    }
}

impl<T: Asset> CreateTypeData<T> for ReflectAsset {
    fn create_type_data(_input: ()) -> Self {
        Self {
            type_path: T::type_path(),
            default: || Box::new(T::default()),
            from_reflect: |value| {
                <T as FromReflect>::from_reflect(value).map(|v| Box::new(v) as Box<dyn Reflect>)
            },
        }
    }
}

/// Registers data asset types on an [`App`].
pub trait RegisterDataAssetExt {
    /// Registers `T` with the type registry and adds its [`ReflectAsset`] type data.
    fn register_data_asset<T: Asset>(&mut self) -> &mut Self;
}

impl RegisterDataAssetExt for App {
    fn register_data_asset<T: Asset>(&mut self) -> &mut Self {
        self.register_type::<T>();
        let registry = self.world().resource::<AppTypeRegistry>().clone();
        registry.write().register_type_data::<T, ReflectAsset>();
        self
    }
}

/// Every registered data asset type path, sorted.
#[must_use]
pub fn data_asset_types(registry: &TypeRegistry) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = registry
        .iter()
        .filter_map(|r| r.data::<ReflectAsset>().map(ReflectAsset::type_path))
        .collect();
    out.sort_unstable();
    out
}

/// Why a data asset did not load. [`Self::code`] is the agent protocol's error code.
#[derive(Debug, thiserror::Error)]
pub enum DataAssetError {
    /// The text is not a `DataAsset(...)` document.
    #[error("not a .data.ron document: {0}")]
    Parse(String),
    /// A `format_version` newer than this build reads.
    #[error("format_version {found} is newer than {supported}")]
    UnsupportedVersion {
        /// Version in the file.
        found: u32,
        /// Newest this build reads.
        supported: u32,
    },
    /// The `type` is not registered, or not as a data asset.
    #[error(
        "`{0}` is not a registered data asset type (register it with register_data_asset or #[reflect(Asset)])"
    )]
    UnknownType(String),
    /// The `value` does not decode as the type.
    #[error("value of `{type_path}`: {reason}")]
    Value {
        /// The type.
        type_path: String,
        /// The decoder's message.
        reason: String,
    },
    /// The value is not a data asset type, or could not be written.
    #[error("cannot write: {0}")]
    Write(String),
    /// The asset server could not provide the file.
    #[error("asset: {0}")]
    Asset(String),
}

impl DataAssetError {
    /// The stable error code (`data_asset_parse`, `data_asset_version`, `data_asset_unknown_type`,
    /// `data_asset_value`, `data_asset_write`, `data_asset_load`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "data_asset_parse",
            Self::UnsupportedVersion { .. } => "data_asset_version",
            Self::UnknownType(_) => "data_asset_unknown_type",
            Self::Value { .. } => "data_asset_value",
            Self::Write(_) => "data_asset_write",
            Self::Asset(_) => "data_asset_load",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename = "DataAsset")]
struct RawFile {
    format_version: u32,
    #[serde(rename = "type")]
    type_path: String,
    value: Box<ron::value::RawValue>,
}

/// The serde field-list fingerprint of `.data.ron` (hashed by `cargo xtask check-formats`): the
/// document's own fields. The value's fields are the game type's, not part of the format.
#[must_use]
pub fn data_asset_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<RawFile>()])
}

/// `.data.ron` has no migration steps yet (version 1 is the first).
#[must_use]
pub fn data_asset_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The header of a `.data.ron` file, read without the type registry (what the cook checks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataAssetHeader {
    /// `format_version` of the file.
    pub format_version: u32,
    /// The value's type path.
    pub type_path: String,
}

/// A decoded `.data.ron` file.
#[derive(Debug)]
pub struct DataAssetFile {
    /// The value's type path.
    pub type_path: String,
    /// The value, as the concrete registered type.
    pub value: Box<dyn Reflect>,
    /// Decoder warnings (unknown fields).
    pub warnings: Vec<String>,
}

impl DataAssetFile {
    /// Reads the header (version and type) of `.data.ron` text; checks the value is RON but not its
    /// type, so it needs no registry.
    ///
    /// # Errors
    ///
    /// [`DataAssetError::Parse`] or [`DataAssetError::UnsupportedVersion`].
    pub fn header(text: &str) -> Result<DataAssetHeader, DataAssetError> {
        let raw = Self::raw(text)?;
        if raw.type_path.trim().is_empty() {
            return Err(DataAssetError::Parse("`type` is empty".into()));
        }
        Ok(DataAssetHeader {
            format_version: raw.format_version,
            type_path: raw.type_path,
        })
    }

    fn raw(text: &str) -> Result<RawFile, DataAssetError> {
        let raw: RawFile = ron::from_str(text).map_err(|e| DataAssetError::Parse(e.to_string()))?;
        if raw.format_version > DATA_ASSET_FORMAT_VERSION {
            return Err(DataAssetError::UnsupportedVersion {
                found: raw.format_version,
                supported: DATA_ASSET_FORMAT_VERSION,
            });
        }
        Ok(raw)
    }

    /// Decodes `.data.ron` text with `registry`.
    ///
    /// # Errors
    ///
    /// Any [`DataAssetError`] but `Write` and `Asset`.
    pub fn parse(text: &str, registry: &TypeRegistry) -> Result<Self, DataAssetError> {
        let raw = Self::raw(text)?;
        let registration = registry
            .get_with_type_path(&raw.type_path)
            .filter(|r| r.data::<ReflectAsset>().is_some())
            .ok_or_else(|| DataAssetError::UnknownType(raw.type_path.clone()))?;
        let mut codec = ReflectCodec::new(registry);
        let value = codec
            .from_ron_typed(raw.value.get_ron(), registration)
            .map_err(|e| DataAssetError::Value {
                type_path: raw.type_path.clone(),
                reason: e.to_string(),
            })?;
        Ok(Self {
            type_path: raw.type_path,
            value,
            warnings: codec.take_warnings(),
        })
    }

    /// Canonical `.data.ron` text of `value` (a registered data asset type), LF newlines.
    ///
    /// # Errors
    ///
    /// [`DataAssetError::Write`] when the type is not a registered data asset type or does not
    /// serialize.
    pub fn to_ron(value: &dyn PartialReflect, registry: &TypeRegistry) -> Result<String, DataAssetError> {
        let type_path = value
            .get_represented_type_info()
            .map(|info| info.type_path())
            .ok_or_else(|| DataAssetError::Write("the value has no type info".into()))?;
        let is_asset = registry
            .get_with_type_path(type_path)
            .is_some_and(|r| r.data::<ReflectAsset>().is_some());
        if !is_asset {
            return Err(DataAssetError::Write(format!(
                "`{type_path}` is not a registered data asset type"
            )));
        }
        let body = ReflectCodec::new(registry)
            .to_ron_typed(value)
            .map_err(|e| DataAssetError::Write(e.to_string()))?;
        let body = body.replace("\r\n", "\n");
        let mut value_text = String::new();
        for (n, line) in body.trim_end().lines().enumerate() {
            if n > 0 {
                value_text.push_str("\n    ");
            }
            value_text.push_str(line);
        }
        let quoted = ron::to_string(type_path).map_err(|e| DataAssetError::Write(e.to_string()))?;
        Ok(format!(
            "DataAsset(\n    format_version: {DATA_ASSET_FORMAT_VERSION},\n    type: {quoted},\n    value: {value_text},\n)\n"
        ))
    }
}

/// One loaded data asset.
#[derive(Debug)]
pub struct LoadedDataAsset {
    /// Its asset path (as loaded), if it came from a path.
    pub path: Option<String>,
    /// The value's type path.
    pub type_path: String,
    /// The value.
    pub value: Box<dyn Reflect>,
}

/// Loaded data assets by id (`DataAssets::load_path`, [`DataAssets::insert`]).
#[derive(Resource, Debug, Default)]
pub struct DataAssets {
    assets: BTreeMap<AssetId, LoadedDataAsset>,
}

impl DataAssets {
    /// Adds or replaces the asset `id`.
    pub fn insert(&mut self, id: AssetId, asset: LoadedDataAsset) {
        self.assets.insert(id, asset);
    }

    /// Loads (or reloads) the `.data.ron` at `path` through `server` and decodes it with
    /// `registry`. Returns its id.
    ///
    /// # Errors
    ///
    /// [`DataAssetError::Asset`] when the server has no such cooked asset or it is not a data
    /// asset; the decode errors of [`DataAssetFile::parse`].
    pub fn load_path(
        &mut self,
        server: &mut crate::AssetServer,
        path: &str,
        registry: &TypeRegistry,
    ) -> Result<AssetId, DataAssetError> {
        let id = server
            .id_for_path(path)
            .ok_or_else(|| DataAssetError::Asset(format!("{path}: not in the asset manifest")))?;
        let (bytes, _) = server
            .read_cooked(id)
            .map_err(|e| DataAssetError::Asset(format!("{path}: {e}")))?;
        let text = cooked_text(&bytes, DATA_ASSET_TYPE_PATH)
            .map_err(|e| DataAssetError::Asset(format!("{path}: {e}")))?;
        let file = DataAssetFile::parse(text, registry)?;
        for warning in &file.warnings {
            tracing::warn!(target: "regia_asset", path, "data asset: {warning}");
        }
        self.insert(
            id,
            LoadedDataAsset {
                path: Some(path.to_owned()),
                type_path: file.type_path,
                value: file.value,
            },
        );
        Ok(id)
    }

    /// The asset `id`, reflected.
    #[must_use]
    pub fn get_reflect(&self, id: AssetId) -> Option<&LoadedDataAsset> {
        self.assets.get(&id)
    }

    /// The asset `id` as `T`, if it is one.
    #[must_use]
    pub fn get<T: Asset>(&self, id: AssetId) -> Option<&T> {
        self.assets.get(&id)?.value.downcast_ref::<T>()
    }

    /// Every loaded asset, by id.
    pub fn iter(&self) -> impl Iterator<Item = (AssetId, &LoadedDataAsset)> {
        self.assets.iter().map(|(id, a)| (*id, a))
    }

    /// How many are loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.assets.len()
    }

    /// Whether none is loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }
}

/// The UTF-8 text payload of a cooked file whose type is `type_path` (a `.data.ron` or another
/// text-cooked kind such as `.netprofile.ron`).
///
/// # Errors
///
/// The wrapper does not decode, the type tag is another type, or the payload is not UTF-8.
pub fn cooked_text<'a>(cooked_file: &'a [u8], type_path: &str) -> Result<&'a str, crate::AssetLoadError> {
    let (header, payload) = crate::decode_cooked_file(cooked_file)?;
    let expected = regia_core::id::TypePathHash::of(type_path);
    if header.type_tag != expected {
        return Err(crate::AssetLoadError::TypeMismatch {
            id: header.asset_id,
            found: header.type_tag.get(),
            expected: expected.get(),
        });
    }
    std::str::from_utf8(payload)
        .map_err(|e| crate::AssetLoadError::Invalid(format!("payload is not UTF-8: {e}")))
}
