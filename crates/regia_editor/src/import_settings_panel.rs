// SPDX-License-Identifier: MPL-2.0
//! Content ▸ Import settings… (`docs/specs/19-editor.md` § 2 Content Browser, ADR-7469).
//!
//! A typed form over one source's importer settings: the fields of the importer types the asset
//! pipeline re-exports (`TextureImporterSettings`, `GltfImporterSettings`, `ObjImportSettings`,
//! `FbxImportSettings`, `AudioImporterSettings`). The form holds the settings object as ordered
//! field → RON value text (`SettingsObject`), so a field the form has no widget for is carried
//! through byte-for-byte. Apply goes through the same function as `editor.set_import_settings`
//! (`asset_ops_agent::set_import_settings_at`): the same values write the same `.meta` bytes, and
//! one Apply cooks the project once (the cook hashes the settings, so the source re-cooks). Revert
//! drops the unsaved edits.
//!
//! First attempt ported from the never-pushed branch `lane/sprint-content-browser-rename-move`
//! (67e37f8a6, 2d4aeb31f): the field lists, the ranges and the panel's shape. This version keeps
//! the method's settings text instead of a JSON round trip, so both routes share one writer.
//!
//! Agent surface: `editor.import_settings_open`, `editor.import_settings_edit`,
//! `editor.import_settings_apply`, `editor.import_settings_revert` and
//! `editor.import_settings_state` (`book/agents/import-settings.md`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset::import_rules::SettingsObject;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::agent_ext::EditorProject;
use crate::asset_ops_agent::reasoned;
use crate::viewport_agent::Handler;

/// The importer a source's settings belong to (by source extension).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImporterKind {
    /// Images (`TextureImporterSettings`).
    Texture,
    /// glTF / GLB (`GltfImporterSettings`).
    Gltf,
    /// Wavefront OBJ (`ObjImportSettings`, next to the glTF mesh keys).
    Obj,
    /// FBX (`FbxImportSettings`, next to the glTF mesh keys).
    Fbx,
    /// Sounds (`AudioImporterSettings`).
    Audio,
}

impl ImporterKind {
    /// The importer for a source path, by extension; `None` for a file the panel has no form for.
    #[must_use]
    pub fn of_path(path: &str) -> Option<Self> {
        let ext = path.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase())?;
        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "tga" | "bmp" | "hdr" | "exr" => Some(Self::Texture),
            "gltf" | "glb" => Some(Self::Gltf),
            "obj" => Some(Self::Obj),
            "fbx" => Some(Self::Fbx),
            "wav" | "ogg" | "flac" | "mp3" => Some(Self::Audio),
            _ => None,
        }
    }

    /// Stable name (`texture`, `gltf`, `obj`, `fbx`, `audio`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Texture => "texture",
            Self::Gltf => "gltf",
            Self::Obj => "obj",
            Self::Fbx => "fbx",
            Self::Audio => "audio",
        }
    }

    /// The importer called `name` ([`Self::name`]'s inverse).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Texture, Self::Gltf, Self::Obj, Self::Fbx, Self::Audio]
            .into_iter()
            .find(|kind| kind.name() == name)
    }

    /// A source path of this importer's kind (`settings.<ext>`), for checks that go by path.
    #[must_use]
    pub fn sample_path(self) -> &'static str {
        match self {
            Self::Texture => "assets/settings.png",
            Self::Gltf => "assets/settings.glb",
            Self::Obj => "assets/settings.obj",
            Self::Fbx => "assets/settings.fbx",
            Self::Audio => "assets/settings.wav",
        }
    }

    /// The fields the form shows, in order.
    #[must_use]
    pub fn fields(self) -> Vec<&'static FieldSpec> {
        match self {
            Self::Texture => TEXTURE_FIELDS.iter().collect(),
            Self::Gltf => GLTF_FIELDS.iter().collect(),
            Self::Obj => OBJ_FIELDS.iter().chain(GLTF_MESH_FIELDS).collect(),
            Self::Fbx => FBX_FIELDS.iter().chain(GLTF_MESH_FIELDS).collect(),
            Self::Audio => AUDIO_FIELDS.iter().collect(),
        }
    }

    /// The field called `key`.
    #[must_use]
    pub fn field(self, key: &str) -> Option<&'static FieldSpec> {
        self.fields().into_iter().find(|spec| spec.key == key)
    }
}

/// How a field's value is typed (and so which widget edits it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldKind {
    /// `Option<bool>`: absent, `Some(true)` or `Some(false)`.
    OptBool,
    /// `bool`.
    Bool,
    /// `Option<enum>`: absent or `Some(<choice>)`.
    OptChoice(&'static [&'static str]),
    /// An enum (or a fixed set of number tokens): `<choice>`.
    Choice(&'static [&'static str]),
    /// `Option<u32>` inside `min..=max`.
    OptU32 {
        /// Smallest accepted value.
        min: u32,
        /// Largest accepted value.
        max: u32,
    },
    /// `f32` inside `min..=max` (finite).
    F32 {
        /// Smallest accepted value.
        min: f32,
        /// Largest accepted value.
        max: f32,
    },
    /// `Option<f32>` inside `min..=max` (finite).
    OptF32 {
        /// Smallest accepted value.
        min: f32,
        /// Largest accepted value.
        max: f32,
    },
    /// `Option<String>`.
    OptString,
    /// Any RON value (a nested block such as `limits` or `generate_lods`), edited as text.
    Raw,
}

impl FieldKind {
    /// Stable name for the agent state (`opt_bool`, `bool`, `opt_choice`, `choice`, `opt_u32`,
    /// `f32`, `opt_f32`, `opt_string`, `raw`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::OptBool => "opt_bool",
            Self::Bool => "bool",
            Self::OptChoice(_) => "opt_choice",
            Self::Choice(_) => "choice",
            Self::OptU32 { .. } => "opt_u32",
            Self::F32 { .. } => "f32",
            Self::OptF32 { .. } => "opt_f32",
            Self::OptString => "opt_string",
            Self::Raw => "raw",
        }
    }
}

/// One importer setting the form edits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldSpec {
    /// The `.meta` settings key.
    pub key: &'static str,
    /// The form label.
    pub label: &'static str,
    /// The value type.
    pub kind: FieldKind,
    /// What the importer does when the key is absent (shown in the form).
    pub default: &'static str,
}

const fn field(key: &'static str, label: &'static str, kind: FieldKind, default: &'static str) -> FieldSpec {
    FieldSpec {
        key,
        label,
        kind,
        default,
    }
}

const TEXTURE_FIELDS: &[FieldSpec] = &[
    field(
        "usage",
        "Color space / usage",
        FieldKind::OptChoice(&["color", "linear", "normal", "mask", "ui"]),
        "material slot, else file name",
    ),
    field(
        "max_size",
        "Max size (px)",
        FieldKind::OptU32 { min: 1, max: 16384 },
        "source size",
    ),
    field("compress", "Compress", FieldKind::OptBool, "on"),
    field(
        "quality",
        "Quality",
        FieldKind::OptChoice(&["high", "low"]),
        "high",
    ),
    field("stream", "Stream mips", FieldKind::OptBool, "on"),
    field(
        "stream_min_px",
        "Stream floor (px)",
        FieldKind::OptU32 { min: 1, max: 16384 },
        "tier floor",
    ),
    field(
        "hdr_encoding",
        "HDR encoding",
        FieldKind::OptChoice(&["auto", "bc6h", "rgba16f", "astc_hdr"]),
        "auto",
    ),
    field(
        "cube_face_px",
        "Cube face (px)",
        FieldKind::OptU32 { min: 4, max: 8192 },
        "2D equirect",
    ),
    field(
        "normal_green",
        "Normal green",
        FieldKind::OptChoice(&["opengl", "directx"]),
        "opengl",
    ),
    field(
        "orm_channel_order",
        "ORM channel order",
        FieldKind::OptChoice(&["orm", "arm", "rma", "mra"]),
        "orm",
    ),
    field(
        "alpha_cutoff",
        "Alpha coverage cutoff",
        FieldKind::OptF32 { min: 0.0, max: 1.0 },
        "plain box mips",
    ),
];

/// The glTF mesh keys OBJ and FBX share (their scenes are converted and cooked as glTF).
const GLTF_MESH_FIELDS: &[FieldSpec] = &[
    field("generate_lods", "Generate LODs", FieldKind::Raw, "None"),
    field("meshlets", "Meshlets", FieldKind::Bool, "false"),
    field(
        "drop_double_sided_on_closed",
        "Drop double-sided on closed meshes",
        FieldKind::Bool,
        "false",
    ),
    field(
        "vertex_ao",
        "Vertex AO from COLOR_0 alpha",
        FieldKind::Bool,
        "false",
    ),
    field(
        "skin_influences",
        "Skin influences",
        FieldKind::Choice(&["4", "8"]),
        "4",
    ),
    field("fracture", "Fracture", FieldKind::Raw, "None"),
    field(
        "collision",
        "Collision",
        FieldKind::Choice(&["None", "ConvexDecomposition"]),
        "None",
    ),
    field(
        "collision_hulls",
        "Collision hulls",
        FieldKind::Raw,
        "decomposition defaults",
    ),
    field("voxel_far", "Voxel far proxy", FieldKind::Raw, "None"),
];

const GLTF_FIELDS: &[FieldSpec] = &[
    field("generate_lods", "Generate LODs", FieldKind::Raw, "None"),
    field("meshlets", "Meshlets", FieldKind::Bool, "false"),
    field(
        "drop_double_sided_on_closed",
        "Drop double-sided on closed meshes",
        FieldKind::Bool,
        "false",
    ),
    field(
        "vertex_ao",
        "Vertex AO from COLOR_0 alpha",
        FieldKind::Bool,
        "false",
    ),
    field(
        "skin_influences",
        "Skin influences",
        FieldKind::Choice(&["4", "8"]),
        "4",
    ),
    field("fracture", "Fracture", FieldKind::Raw, "None"),
    field(
        "collision",
        "Collision",
        FieldKind::Choice(&["None", "ConvexDecomposition"]),
        "None",
    ),
    field(
        "collision_hulls",
        "Collision hulls",
        FieldKind::Raw,
        "decomposition defaults",
    ),
    field("voxel_far", "Voxel far proxy", FieldKind::Raw, "None"),
    field(
        "forward",
        "Axis",
        FieldKind::Choice(&["Convert", "Keep"]),
        "Convert",
    ),
    field(
        "scale",
        "Scale",
        FieldKind::F32 {
            min: 1.0e-6,
            max: 1.0e6,
        },
        "1.0",
    ),
];

const OBJ_FIELDS: &[FieldSpec] = &[
    field(
        "scale",
        "Scale",
        FieldKind::F32 {
            min: 1.0e-6,
            max: 1.0e6,
        },
        "1.0",
    ),
    field("limits", "Limits", FieldKind::Raw, "importer caps"),
];

const FBX_FIELDS: &[FieldSpec] = &[
    field(
        "scale",
        "Scale",
        FieldKind::F32 {
            min: 1.0e-6,
            max: 1.0e6,
        },
        "1.0",
    ),
    field("import_animation", "Import animation", FieldKind::Bool, "true"),
    field(
        "sample_rate",
        "Curve sample rate (Hz)",
        FieldKind::F32 { min: 1.0, max: 240.0 },
        "30.0",
    ),
    field("uv_set", "UV set", FieldKind::OptString, "the first"),
    field("limits", "Limits", FieldKind::Raw, "importer caps"),
];

const AUDIO_FIELDS: &[FieldSpec] = &[
    field("normalize", "Normalize", FieldKind::OptBool, "on"),
    field(
        "target_lufs",
        "Target loudness (LUFS)",
        FieldKind::OptF32 { min: -70.0, max: 0.0 },
        "project target",
    ),
    field(
        "mode",
        "Ship mode",
        FieldKind::OptChoice(&["keep", "vorbis"]),
        "project mode",
    ),
    field(
        "quality",
        "Vorbis quality (desktop)",
        FieldKind::OptF32 { min: -0.1, max: 1.0 },
        "project quality",
    ),
    field(
        "android_quality",
        "Vorbis quality (Android)",
        FieldKind::OptF32 { min: -0.1, max: 1.0 },
        "project quality",
    ),
    field("force", "Force transcode", FieldKind::OptBool, "project setting"),
    field("impulse_response", "Impulse response", FieldKind::OptBool, "off"),
];

/// `Some(<inner>)` → `<inner>`.
fn strip_some(text: &str) -> Option<&str> {
    text.trim()
        .strip_prefix("Some(")
        .and_then(|rest| rest.strip_suffix(')'))
        .map(str::trim)
}

fn parse_f64(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// A field's RON value text as a typed JSON value (`null` for `None`).
///
/// # Errors
///
/// The text is not a value of the field's type.
pub fn ron_to_json(spec: &FieldSpec, text: &str) -> Result<Value, String> {
    let text = text.trim();
    let wrong = || format!("`{}`: `{text}` is not a {} value", spec.key, spec.kind.name());
    if text == "None" && !matches!(spec.kind, FieldKind::Choice(_) | FieldKind::Raw) {
        return Ok(Value::Null);
    }
    match spec.kind {
        FieldKind::OptBool => match strip_some(text) {
            Some("true") => Ok(json!(true)),
            Some("false") => Ok(json!(false)),
            _ => Err(wrong()),
        },
        FieldKind::Bool => match text {
            "true" => Ok(json!(true)),
            "false" => Ok(json!(false)),
            _ => Err(wrong()),
        },
        FieldKind::OptChoice(_) => strip_some(text).map(|inner| json!(inner)).ok_or_else(wrong),
        FieldKind::Choice(_) | FieldKind::Raw => Ok(json!(text)),
        FieldKind::OptU32 { .. } => strip_some(text)
            .and_then(|inner| inner.parse::<u64>().ok())
            .map(|n| json!(n))
            .ok_or_else(wrong),
        FieldKind::F32 { .. } => parse_f64(text).map(|v| json!(v)).ok_or_else(wrong),
        FieldKind::OptF32 { .. } => strip_some(text)
            .and_then(parse_f64)
            .map(|v| json!(v))
            .ok_or_else(wrong),
        FieldKind::OptString => ron::from_str::<Option<String>>(text)
            .map(|v| json!(v))
            .map_err(|_| wrong()),
    }
}

/// A typed JSON value as the field's RON value text; `null` (or an empty raw text) is `None`:
/// the key is removed and the importer's default applies.
///
/// # Errors
///
/// The value has the wrong type or is outside the field's range (the message names the range).
pub fn json_to_ron(spec: &FieldSpec, value: &Value) -> Result<Option<String>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let key = spec.key;
    let number = |v: &Value| v.as_f64().filter(|x| x.is_finite());
    match spec.kind {
        FieldKind::OptBool => value
            .as_bool()
            .map(|b| Some(format!("Some({b})")))
            .ok_or_else(|| format!("`{key}` must be true, false or null")),
        FieldKind::Bool => value
            .as_bool()
            .map(|b| Some(b.to_string()))
            .ok_or_else(|| format!("`{key}` must be true or false")),
        FieldKind::OptChoice(choices) | FieldKind::Choice(choices) => {
            let token = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => String::new(),
            };
            if !choices.contains(&token.as_str()) {
                return Err(format!("`{key}` must be one of {}", choices.join(", ")));
            }
            Ok(Some(match spec.kind {
                FieldKind::OptChoice(_) => format!("Some({token})"),
                _ => token,
            }))
        }
        FieldKind::OptU32 { min, max } => {
            let n = value
                .as_u64()
                .ok_or_else(|| format!("`{key}` must be a whole number in {min}..={max}"))?;
            if n < u64::from(min) || n > u64::from(max) {
                return Err(format!("`{key}` {n} is outside {min}..={max}"));
            }
            Ok(Some(format!("Some({n})")))
        }
        FieldKind::F32 { min, max } | FieldKind::OptF32 { min, max } => {
            let x = number(value).ok_or_else(|| format!("`{key}` must be a finite number"))?;
            if x < f64::from(min) || x > f64::from(max) {
                return Err(format!("`{key}` {x} is outside {min}..={max}"));
            }
            let text = format!("{:?}", x as f32);
            Ok(Some(match spec.kind {
                FieldKind::OptF32 { .. } => format!("Some({text})"),
                _ => text,
            }))
        }
        FieldKind::OptString => {
            let s = value
                .as_str()
                .ok_or_else(|| format!("`{key}` must be a string or null"))?;
            let quoted = ron::ser::to_string(&s).map_err(|e| format!("`{key}`: {e}"))?;
            Ok(Some(format!("Some({quoted})")))
        }
        FieldKind::Raw => {
            let s = value
                .as_str()
                .ok_or_else(|| format!("`{key}` must be RON value text (a string) or null"))?;
            let s = s.trim();
            if s.is_empty() {
                return Ok(None);
            }
            SettingsObject::parse(&format!("({key}: {s})"))
                .map_err(|e| format!("`{key}` is not a RON value: {e}"))?;
            Ok(Some(s.to_string()))
        }
    }
}

/// Checks a canonical settings object against the importer of `rel` (a project-relative source
/// path): every value the form types has its type and range, and the whole object deserializes
/// as the importer's settings type (audio: no problem reported by its parser; meshes: the
/// collision-hull block is inside the decomposition's ranges). A file the panel has no form for
/// is accepted as it is (the object already parsed). Unknown keys are kept: the cook warns.
///
/// # Errors
///
/// A message naming the first refused value; the method answers `asset_settings_invalid`.
pub fn validate_settings(rel: &str, text: &str) -> Result<(), String> {
    let Some(kind) = ImporterKind::of_path(rel) else {
        return Ok(());
    };
    let object = SettingsObject::parse(text).map_err(|e| e.message)?;
    for (key, value) in &object.fields {
        if let Some(spec) = kind.field(key) {
            json_to_ron(spec, &ron_to_json(spec, value)?)?;
        }
    }
    let importer = kind.name();
    let typed =
        |e: ron::error::SpannedError| format!("does not match the {importer} importer's settings: {e}");
    let check_mesh = |text: &str| -> Result<(), String> {
        let settings = ron::from_str::<regia_asset_pipeline::GltfImporterSettings>(text).map_err(typed)?;
        regia_asset_pipeline::collision_cook::params_of(&settings.collision_hulls)
            .validate()
            .map_err(|e| format!("`collision_hulls`: {e}"))
    };
    match kind {
        ImporterKind::Texture => {
            ron::from_str::<regia_asset_pipeline::TextureImporterSettings>(text).map_err(typed)?;
        }
        ImporterKind::Gltf => check_mesh(text)?,
        ImporterKind::Obj => {
            ron::from_str::<regia_asset_pipeline::obj_import::ObjImportSettings>(text).map_err(typed)?;
            check_mesh(text)?;
        }
        ImporterKind::Fbx => {
            ron::from_str::<regia_asset_pipeline::fbx_import::FbxImportSettings>(text).map_err(typed)?;
            check_mesh(text)?;
        }
        ImporterKind::Audio => {
            let (_, problem) =
                regia_asset_pipeline::audio_cook::AudioImporterSettings::from_meta_string(Some(text));
            if let Some(problem) = problem {
                return Err(problem);
            }
        }
    }
    Ok(())
}

/// The Import settings panel (an edit-world resource).
#[derive(Resource, Debug, Default)]
pub struct ImportSettingsPanel {
    /// The window shows.
    pub open: bool,
    /// The project the source belongs to.
    pub root: Option<PathBuf>,
    /// The open source, project-relative (`assets/...`).
    pub path: Option<String>,
    /// Its importer.
    pub kind: Option<ImporterKind>,
    /// The settings in the `.meta` (what Revert returns to).
    pub saved: SettingsObject,
    /// The edited settings.
    pub form: SettingsObject,
    /// Why the `.meta` settings did not parse (the form then starts empty).
    pub problem: Option<String>,
    /// The last refusal: `(code, message)`.
    pub error: Option<(String, String)>,
    /// The last Apply's result (`editor.set_import_settings`'s shape).
    pub last_apply: Option<Value>,
    /// Applies that wrote the `.meta` and cooked, since the panel opened a source.
    pub recooks: u64,
    /// Text-field edit buffers, by key (cleared on open, apply and revert).
    pub buffers: BTreeMap<String, String>,
    /// Fields whose text buffer holds a value the last edit refused: the form keeps the last
    /// accepted value, so Apply is refused until the field is fixed or reverted (rev-claude-57
    /// asset finding 1, ADR-7495). Cleared with the buffers.
    pub invalid: BTreeSet<String>,
    /// The job id of the panel's running Apply (`import_settings:<n>`, ADR-7664); `None` when idle.
    pub applying: Option<String>,
    /// Apply was clicked this frame: [`import_settings_window`] starts the job after drawing.
    pub apply_requested: bool,
    /// The other sources edited together with `path` (multi-source editing, ADR-7744): same
    /// importer, edits and Reset go to every source, a field whose values differ shows as mixed.
    pub others: Vec<ExtraSource>,
    /// The Presets row's "Save as" name buffer (ADR-7904).
    pub preset_name: String,
    /// The preset picked in the Presets row (the last one applied or saved).
    pub preset_choice: Option<String>,
    /// The open importer's preset names, read from `config/import_presets.ron` when the panel
    /// opens a source and after each preset change (`None`: read again on the next draw).
    pub preset_names: Option<Vec<String>>,
}

/// A source edited beside the panel's primary one ([`ImportSettingsPanel::others`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtraSource {
    /// The source, project-relative (`assets/...`).
    pub path: String,
    /// The settings in its `.meta`.
    pub saved: SettingsObject,
    /// The edited settings.
    pub form: SettingsObject,
}

impl ImportSettingsPanel {
    /// Opens `path` (project-relative, an optional `assets/` prefix): reads its `.meta` settings
    /// into the form and shows the window.
    ///
    /// # Errors
    ///
    /// `path_outside_project`, a missing file (`F0003`), `import_settings_unsupported` (no form
    /// for the file type) or `asset_not_imported` (no `.meta`).
    pub fn open_path(&mut self, root: &Path, path: &str) -> Result<(), RpcError> {
        self.open_paths(root, &[path])
    }

    /// Opens several sources of the same importer at once (ADR-7744): the first is the panel's
    /// primary source, the others are edited together with it ([`Self::others`]). Repeated paths
    /// count once.
    ///
    /// # Errors
    ///
    /// As [`Self::open_path`] for any source, `invalid_params` for no path, and
    /// `import_settings_mixed_importers` when the sources do not share an importer.
    pub fn open_paths(&mut self, root: &Path, paths: &[&str]) -> Result<(), RpcError> {
        let mut sources: Vec<(String, ImporterKind, SettingsObject, Option<String>)> = Vec::new();
        for path in paths {
            let source = read_source(root, path)?;
            if sources.iter().all(|(rel, ..)| *rel != source.0) {
                sources.push(source);
            }
        }
        let Some((rel, kind, saved, problem)) = sources.first().cloned() else {
            return Err(RpcError::invalid_params("at least one source path is required"));
        };
        if let Some((other, other_kind, ..)) = sources.iter().find(|(_, k, ..)| *k != kind) {
            return Err(reasoned(
                "import_settings_mixed_importers",
                format!(
                    "{rel} is a {} source and {other} a {} source: edit sources of one importer together",
                    kind.name(),
                    other_kind.name()
                ),
            ));
        }
        let others = sources
            .into_iter()
            .skip(1)
            .map(|(path, _, saved, _)| ExtraSource {
                path,
                form: saved.clone(),
                saved,
            })
            .collect();
        *self = Self {
            open: true,
            root: Some(root.to_path_buf()),
            path: Some(rel),
            kind: Some(kind),
            form: saved.clone(),
            saved,
            problem,
            others,
            ..Self::default()
        };
        Ok(())
    }
}

/// Reads one source's importer and `.meta` settings: `(project-relative path, importer, settings,
/// why the settings did not parse)`.
fn read_source(
    root: &Path,
    path: &str,
) -> Result<(String, ImporterKind, SettingsObject, Option<String>), RpcError> {
    let full = crate::asset_ops_agent::assets_request_path(root, "path", path)?;
    let rel = crate::content::slash_path(full.strip_prefix(root).unwrap_or(&full));
    if !full.is_file() {
        return Err(RpcError::not_found(
            format!("{rel} does not exist or is not a file"),
            "editor.list_assets shows the project's files",
        ));
    }
    let kind = ImporterKind::of_path(&rel).ok_or_else(|| {
        reasoned(
            "import_settings_unsupported",
            format!("{rel}: no import settings form for this file type (images, glTF/GLB, OBJ, FBX, sounds)"),
        )
    })?;
    let meta_path = regia_asset::meta_path_for(&full);
    let bytes = std::fs::read(&meta_path).map_err(|_| {
        reasoned(
            "asset_not_imported",
            format!("{rel} has no .meta: cook it once first (import it, or Reimport)"),
        )
    })?;
    let meta = regia_asset::MetaSidecar::from_ron(&bytes)
        .map_err(|e| RpcError::invalid_params(format!("{} does not parse: {e}", meta_path.display())))?;
    let (saved, problem) = match meta.settings.as_deref() {
        None => (SettingsObject::default(), None),
        Some(text) => match SettingsObject::parse(text) {
            Ok(object) => (object, None),
            Err(e) => (
                SettingsObject::default(),
                Some(format!(
                    "the .meta settings do not parse ({e}); Apply replaces them"
                )),
            ),
        },
    };
    Ok((rel, kind, saved, problem))
}

impl ImportSettingsPanel {
    /// Whether any source's form differs from its `.meta`.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.form != self.saved || self.others.iter().any(|o| o.form != o.saved)
    }

    /// Every source the panel edits, project-relative, the primary first.
    #[must_use]
    pub fn sources(&self) -> Vec<&str> {
        self.path
            .iter()
            .map(String::as_str)
            .chain(self.others.iter().map(|o| o.path.as_str()))
            .collect()
    }

    /// Whether the sources hold different values for `key` (multi-source editing): the field then
    /// shows as mixed until an edit or Reset sets every source to one value.
    #[must_use]
    pub fn mixed(&self, key: &str) -> bool {
        let first = self.form.get(key);
        self.others.iter().any(|o| o.form.get(key) != first)
    }

    /// Sets one field back to the importer's default in every source (what the field's Reset
    /// button and `editor.import_settings_edit { reset: true }` do).
    ///
    /// # Errors
    ///
    /// As [`Self::edit`].
    pub fn reset_field(&mut self, key: &str) -> Result<(), RpcError> {
        self.edit(key, &Value::Null)
    }

    fn open_kind(&self) -> Result<ImporterKind, RpcError> {
        match (&self.path, self.kind) {
            (Some(_), Some(kind)) => Ok(kind),
            _ => Err(reasoned(
                "import_settings_not_open",
                "no source is open in the Import settings panel: call editor.import_settings_open first",
            )),
        }
    }

    /// Sets one field from a typed JSON value (`null` removes it: the importer's default).
    ///
    /// # Errors
    ///
    /// `import_settings_not_open`, `import_settings_unknown_field` (not a field of this
    /// importer's form) or `asset_settings_invalid` (wrong type, out of range).
    pub fn edit(&mut self, key: &str, value: &Value) -> Result<(), RpcError> {
        self.set_value(key, value)?;
        // An accepted edit from outside the field (an agent, a choice box) supersedes whatever
        // the field's text buffer and refused mark hold (rev-claude-63 finding B).
        self.buffers.remove(key);
        self.accepted(key);
        Ok(())
    }

    /// A field's value was accepted: drops its refused mark. The refusal message clears only when
    /// no other field is still invalid, so fixing one field never hides another's (rev-claude-63
    /// finding C).
    fn accepted(&mut self, key: &str) {
        self.invalid.remove(key);
        if self.invalid.is_empty() {
            self.error = None;
        }
    }

    /// Checks `value` against the field `key` and sets it in the form. Touches neither the text
    /// buffers nor the refused marks: a keystroke keeps the text the user is typing (`1.`, a
    /// trailing space) in the field's buffer while the form holds the parsed value.
    fn set_value(&mut self, key: &str, value: &Value) -> Result<(), RpcError> {
        let kind = self.open_kind()?;
        let spec = kind.field(key).ok_or_else(|| {
            let known: Vec<&str> = kind.fields().iter().map(|spec| spec.key).collect();
            reasoned(
                "import_settings_unknown_field",
                format!(
                    "`{key}` is not a {} import setting ({})",
                    kind.name(),
                    known.join(", ")
                ),
            )
        })?;
        let text = json_to_ron(spec, value).map_err(|message| reasoned("asset_settings_invalid", message))?;
        for other in &mut self.others {
            set_field(&mut other.form, key, text.clone());
        }
        set_field(&mut self.form, key, text);
        Ok(())
    }

    /// Drops the unsaved edits.
    pub fn revert(&mut self) {
        self.form = self.saved.clone();
        for other in &mut self.others {
            other.form = other.saved.clone();
        }
        self.buffers.clear();
        self.invalid.clear();
        self.error = None;
    }

    /// Whether Apply may write the form: it differs from the `.meta` and no text field holds a
    /// refused value (`invalid`).
    #[must_use]
    pub fn can_apply(&self) -> bool {
        self.dirty() && self.invalid.is_empty()
    }

    /// Writes the form into the `.meta` through `editor.set_import_settings`'s writer and cooks
    /// once. An empty form clears the override. On success the form is the saved state.
    ///
    /// # Errors
    ///
    /// `import_settings_not_open`, or what `editor.set_import_settings` refuses
    /// (`asset_settings_invalid`, `asset_not_imported`, ...).
    pub fn apply(&mut self) -> Result<Value, RpcError> {
        let (root, path, text) = self.check_apply()?;
        if let Some(lock) = cook_lock_holder(&root) {
            // The cook below would wait for the lock on the caller's thread (the UI thread): a
            // live cook, a Content ▸ Import worker or `regia cook` holds it. Refuse at once.
            let message = format!(
                "a cook holds {}: Apply once it ends (the form is kept)",
                lock.display()
            );
            self.error = Some(("tools_busy".into(), message.clone()));
            return Err(busy_error(&message));
        }
        let extras = self.extra_writes();
        match write_sources_and_cook(&root, &path, text.as_deref(), &extras) {
            Ok(out) => {
                self.saved = self.form.clone();
                for other in &mut self.others {
                    other.saved = other.form.clone();
                }
                self.recooks += 1;
                self.problem = None;
                self.error = None;
                self.buffers.clear();
                self.invalid.clear();
                self.last_apply = Some(out.clone());
                Ok(out)
            }
            Err(error) => {
                let code = error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("apply_failed")
                    .to_string();
                self.error = Some((code, error.message.clone()));
                Err(error)
            }
        }
    }

    /// The other sources an Apply writes (the ones whose form changed) with the settings text
    /// each gets (`None` clears the override).
    fn extra_writes(&self) -> Vec<(String, Option<String>)> {
        self.others
            .iter()
            .filter(|o| o.form != o.saved)
            .map(|o| {
                (
                    (o.path.clone()),
                    (!o.form.fields.is_empty()).then(|| o.form.to_ron()),
                )
            })
            .collect()
    }

    /// What every Apply checks first: a source is open and no field holds a refused value.
    /// Answers the root, the source and the settings text to write (`None` clears the override).
    fn check_apply(&mut self) -> Result<(PathBuf, String, Option<String>), RpcError> {
        self.open_kind()?;
        let (Some(root), Some(path)) = (self.root.clone(), self.path.clone()) else {
            return Err(reasoned("import_settings_not_open", "no source is open"));
        };
        if let Some(key) = self.invalid.iter().next() {
            // The form holds the last accepted value of this field, not what the field shows:
            // writing it would apply a value the user never confirmed.
            let error = reasoned(
                "asset_settings_invalid",
                format!("`{key}` holds a value that was refused: fix it or Revert before Apply"),
            );
            self.error = Some(("asset_settings_invalid".into(), error.message.clone()));
            return Err(error);
        }
        let text = (!self.form.fields.is_empty()).then(|| self.form.to_ron());
        Ok((root, path, text))
    }

    /// The panel as the agent sees it (`editor.import_settings_state`).
    #[must_use]
    pub fn state_json(&self) -> Value {
        let fields: Vec<Value> = self
            .kind
            .map(|kind| {
                kind.fields()
                    .into_iter()
                    .map(|spec| {
                        let mixed = self.mixed(spec.key);
                        let raw = self.form.get(spec.key).filter(|_| !mixed);
                        let value = raw.map_or(Value::Null, |text| {
                            ron_to_json(spec, text).unwrap_or_else(|_| json!(text))
                        });
                        let mut row = json!({
                            "key": spec.key,
                            "label": spec.label,
                            "kind": spec.kind.name(),
                            "set": raw.is_some(),
                            "value": value,
                            "mixed": mixed,
                            "default": spec.default,
                        });
                        if let (FieldKind::OptChoice(choices) | FieldKind::Choice(choices), Some(object)) =
                            (spec.kind, row.as_object_mut())
                        {
                            object.insert("choices".into(), json!(choices));
                        }
                        row
                    })
                    .collect()
            })
            .unwrap_or_default();
        let known: Vec<&str> = self
            .kind
            .map(|kind| kind.fields().iter().map(|spec| spec.key).collect())
            .unwrap_or_default();
        let other: Vec<Value> = self
            .form
            .fields
            .iter()
            .filter(|(key, _)| !known.contains(&key.as_str()))
            .map(|(key, value)| json!({ "key": key, "value": value }))
            .collect();
        json!({
            "open": self.open,
            "path": self.path,
            "sources": self.sources(),
            "importer": self.kind.map(ImporterKind::name),
            "dirty": self.dirty(),
            "settings": (!self.form.fields.is_empty()).then(|| self.form.to_ron()),
            "saved": (!self.saved.fields.is_empty()).then(|| self.saved.to_ron()),
            "fields": fields,
            "other_fields": other,
            "problem": self.problem,
            "error": self.error.as_ref().map(|(code, message)| json!({ "code": code, "message": message })),
            "invalid": self.invalid.iter().collect::<Vec<_>>(),
            "can_apply": self.can_apply(),
            "recooks": self.recooks,
            "last_apply": self.last_apply,
            "applying": self.applying,
        })
    }
}

/// Sets (`Some`) or removes (`None`) one field, keeping the object's order.
fn set_field(object: &mut SettingsObject, key: &str, text: Option<String>) {
    match text {
        Some(text) => match object.fields.iter_mut().find(|(name, _)| name == key) {
            Some((_, value)) => *value = text,
            None => object.fields.push((key.to_string(), text)),
        },
        None => object.fields.retain(|(name, _)| name != key),
    }
}

/// Why Apply cannot cook now, from the editor's job rows: a package, Clear caches or agent
/// cook/package/clean job ([`crate::package_job::busy_reason`]) or a Content ▸ Import job (its
/// worker cooks the project under the cook lock). Apply is disabled and
/// `editor.import_settings_apply` refuses with `tools_busy` while this is `Some`.
pub(crate) fn apply_busy_reason(world: &World) -> Option<String> {
    crate::package_job::busy_reason(world).or_else(|| {
        world
            .get_resource::<regia_agent::jobs::JobRegistry>()?
            .running("import")
            .map(|running| format!("{running} is still running; Apply once it ends"))
    })
}

/// The project's cook lock (`<state>/cook.lock`) when another holder has it right now: the
/// editor's live cooker, a Content ▸ Import worker or another process (`regia cook`). Never
/// waits. `None` when it is free, was never created, or the volume has no file locks (the cook
/// then runs unlocked anyway), and for a lock only a child the editor is spawning still shares
/// (`regia_asset_pipeline::cook_lock_holder`, ADR-8366).
fn cook_lock_holder(root: &Path) -> Option<PathBuf> {
    regia_asset_pipeline::cook_lock_holder(root)
}

// ---------------------------------------------------------------------------------------------
// The Apply job (ADR-7664)
// ---------------------------------------------------------------------------------------------

/// The job kind of an import-settings write (`import_settings:<n>` in `agent.job_status`).
pub const APPLY_JOB_KIND: &str = "import_settings";
/// The background-task id of the running write (`editor.cancel_task { id: "import_settings" }`).
pub const APPLY_TASK: &str = "import_settings";
/// The refusal while a write runs: a second Apply (or `editor.set_import_settings`) is refused,
/// not queued.
pub const APPLY_RUNNING: &str = "import_settings_apply_running";
/// How often a write that waits for another holder's cook lock checks it again (and its cancel).
const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// The cook an import-settings write runs after it wrote the `.meta`: the project's incremental
/// cook by default. A test installs a counted fake gated on a channel it releases.
pub type SettingsCookFn =
    std::sync::Arc<dyn Fn(&Path) -> Result<regia_asset_pipeline::CookReport, String> + Send + Sync>;

/// The cook import-settings writes run (an edit-world resource; absent means the project cook).
#[derive(Resource, Clone)]
pub struct SettingsCook(pub SettingsCookFn);

impl Default for SettingsCook {
    fn default() -> Self {
        Self(std::sync::Arc::new(crate::asset_ops_agent::cook_for_job))
    }
}

impl std::fmt::Debug for SettingsCook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SettingsCook")
    }
}

/// Who started an import-settings write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOrigin {
    /// The panel's Apply (button or `editor.import_settings_apply`): the panel's saved state and
    /// recook count follow the write.
    Panel,
    /// `editor.set_import_settings`.
    Method,
}

/// How an import-settings worker ended.
#[derive(Debug, Clone, PartialEq)]
pub enum ApplyEnd {
    /// Written and cooked: the `editor.set_import_settings` result.
    Applied(Value),
    /// Cancelled: the old `.meta` is back. `recooked`: the cancel came during the cook, which
    /// cannot stop, so the old settings were cooked again (the cooked output is the old one).
    Cancelled {
        /// The old settings were cooked again.
        recooked: bool,
    },
    /// Refused or failed: a lowercase code and a message. A failed cook puts the old `.meta` back.
    Failed {
        /// The code (`import_settings_io`, `import_settings_cook_failed`).
        reason: &'static str,
        /// What went wrong.
        message: String,
    },
}

/// The running import-settings write (an edit-world resource; one at a time).
#[derive(Resource, Debug)]
pub struct ImportSettingsApplyJob {
    /// The worker job (`import_settings:<n>`).
    pub job: crate::editor_jobs::WorkerJob<ApplyEnd>,
    /// The source, project-relative.
    pub path: String,
    /// Who started it.
    pub origin: ApplyOrigin,
    /// The form the panel applied ([`ApplyOrigin::Panel`]): the saved state once it is written.
    pub form: SettingsObject,
    /// The other sources the same write covers (multi-source editing, ADR-7744), each with the
    /// form that becomes its saved state.
    pub others: Vec<(String, SettingsObject)>,
}

/// What the worker does: wait (cancellably) while another holder has the cook lock, write the
/// `.meta`, cook, and put the old `.meta` back on a cancel (cooking it again when the cancel came
/// during the cook) or a failed cook. Checks the cancel before the write, before the cook and
/// after it.
fn run_write(
    cx: &crate::editor_jobs::WorkerContext,
    plans: &[crate::asset_ops_agent::SettingsPlan],
    cook: &SettingsCookFn,
) -> ApplyEnd {
    let Some(first) = plans.first() else {
        return ApplyEnd::Failed {
            reason: "import_settings_failed",
            message: "no source to write".into(),
        };
    };
    // Puts every `.meta` back (the last written first); the first failure is the one reported.
    let restore = || -> Result<(), String> {
        let mut failed = None;
        for plan in plans.iter().rev() {
            if let Err(e) = crate::atomic_write::write_atomic(&plan.meta_path, &plan.old_meta) {
                failed.get_or_insert_with(|| format!("{}: {e}", plan.meta_path.display()));
            }
        }
        failed.map_or(Ok(()), Err)
    };
    let mut announced = false;
    while let Some(lock) = cook_lock_holder(&first.root) {
        if cx.cancelled() {
            return ApplyEnd::Cancelled { recooked: false };
        }
        if !announced {
            cx.progress(
                Some(0.0),
                &format!("waiting for the cook lock ({})", lock.display()),
            );
            announced = true;
        }
        std::thread::sleep(LOCK_POLL);
    }
    if cx.cancelled() {
        return ApplyEnd::Cancelled { recooked: false };
    }
    for plan in plans {
        cx.progress(Some(0.1), &format!("writing {}.meta", plan.rel));
        if let Err(e) = crate::atomic_write::write_atomic(&plan.meta_path, &plan.new_meta) {
            // A source written before this one goes back too.
            let back = restore().map_or_else(
                |e| format!("; the old .meta did not go back: {e}"),
                |()| String::new(),
            );
            return ApplyEnd::Failed {
                reason: "import_settings_io",
                message: format!("cannot write {}: {e}{back}", plan.meta_path.display()),
            };
        }
    }
    if cx.cancelled() {
        return match restore() {
            Ok(()) => ApplyEnd::Cancelled { recooked: false },
            Err(e) => ApplyEnd::Failed {
                reason: "import_settings_io",
                message: format!("cancelled, but the old .meta did not go back: {e}"),
            },
        };
    }
    cx.progress(Some(0.2), &format!("cooking {}", first.rel));
    let report = match cook(&first.root) {
        Ok(report) => report,
        Err(message) => {
            let back = restore().map_or_else(
                |e| format!("; the old .meta did not go back: {e}"),
                |()| String::new(),
            );
            return ApplyEnd::Failed {
                reason: "import_settings_cook_failed",
                message: format!("{message}{back}"),
            };
        }
    };
    crate::asset_ops_agent::IMPORT_SETTINGS_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if cx.cancelled() {
        // The cook cannot stop part-way: the cancel puts the old settings back and cooks them
        // again, so the cooked output is the old one.
        cx.progress(Some(0.6), "cancelled: cooking the old settings again");
        if let Err(e) = restore() {
            return ApplyEnd::Failed {
                reason: "import_settings_io",
                message: format!("cancelled, but the old .meta did not go back: {e}"),
            };
        }
        return match cook(&first.root) {
            Ok(_) => {
                crate::asset_ops_agent::IMPORT_SETTINGS_WRITES
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                ApplyEnd::Cancelled { recooked: true }
            }
            Err(message) => ApplyEnd::Failed {
                reason: "import_settings_cook_failed",
                message: format!("cancelled, but the old settings did not cook again: {message}"),
            },
        };
    }
    cx.progress(Some(1.0), "cooked");
    let mut out = first.result(&report);
    if plans.len() > 1
        && let Some(object) = out.as_object_mut()
    {
        object.insert(
            "paths".into(),
            json!(plans.iter().map(|p| p.rel.as_str()).collect::<Vec<_>>()),
        );
    }
    ApplyEnd::Applied(out)
}

/// The synchronous Apply's write ([`ImportSettingsPanel::apply`], review finding S5). One source
/// goes through `editor.set_import_settings`'s writer. With several, every source is checked
/// before any `.meta` is written, the project cooks once, and a failed write or cook puts every
/// written `.meta` back, as the Apply job (`run_write`) does.
pub(crate) fn write_sources_and_cook(
    root: &Path,
    path: &str,
    text: Option<&str>,
    extras: &[(String, Option<String>)],
) -> Result<Value, RpcError> {
    if extras.is_empty() {
        // One source: `editor.set_import_settings`'s own writer.
        return crate::asset_ops_agent::set_import_settings_at(root, path, text);
    }
    let mut plans = vec![crate::asset_ops_agent::plan_import_settings(root, path, text)?];
    for (extra, text) in extras {
        plans.push(crate::asset_ops_agent::plan_import_settings(
            root,
            extra,
            text.as_deref(),
        )?);
    }
    // Puts the first `written` sources' `.meta` back (the last written first); answers the first
    // failure as a message suffix.
    let restore = |written: usize| -> String {
        let mut failed = String::new();
        for plan in plans[..written].iter().rev() {
            if let Err(e) = crate::atomic_write::write_atomic(&plan.meta_path, &plan.old_meta)
                && failed.is_empty()
            {
                failed = format!(
                    "; the old .meta did not go back: {}: {e}",
                    plan.meta_path.display()
                );
            }
        }
        failed
    };
    for (index, plan) in plans.iter().enumerate() {
        if let Err(e) = crate::atomic_write::write_atomic(&plan.meta_path, &plan.new_meta) {
            let back = restore(index);
            return Err(RpcError::internal(format!(
                "cannot write {}: {e}{back}",
                plan.meta_path.display()
            )));
        }
    }
    let report = match crate::asset_ops_agent::cook_for_job(root) {
        Ok(report) => report,
        Err(message) => {
            let back = restore(plans.len());
            return Err(RpcError::internal(format!("{message}{back}")));
        }
    };
    crate::asset_ops_agent::IMPORT_SETTINGS_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut out = plans[0].result(&report);
    if plans.len() > 1
        && let Some(object) = out.as_object_mut()
    {
        object.insert(
            "paths".into(),
            json!(plans.iter().map(|p| p.rel.as_str()).collect::<Vec<_>>()),
        );
    }
    Ok(out)
}

/// The `import_settings_apply_running` refusal when a write runs.
fn running_error(world: &World) -> Option<RpcError> {
    let job = world.get_resource::<ImportSettingsApplyJob>()?;
    Some(reasoned(
        APPLY_RUNNING,
        format!(
            "{} is still writing {}: wait for it (agent.job_status) or cancel it (editor.cancel_job)",
            job.job.id, job.path
        ),
    ))
}

/// Starts an import-settings write as job `import_settings:<n>` (ADR-7664): the `.meta` write and
/// the cook run on a worker thread; [`tick_apply_job`] applies the end to the world.
///
/// # Errors
///
/// `import_settings_apply_running` while another write runs (refused, not queued), or
/// `job_start_failed` when the worker thread cannot start.
pub(crate) fn start_write_job(
    world: &mut World,
    plan: crate::asset_ops_agent::SettingsPlan,
    origin: ApplyOrigin,
    form: SettingsObject,
) -> Result<String, RpcError> {
    start_write_jobs(world, vec![plan], origin, form, Vec::new())
}

/// [`start_write_job`] for several sources (multi-source editing, ADR-7744): every `.meta` is
/// written, the project cooks once, and a cancel or a failed cook puts every old `.meta` back.
/// `plans[0]` is the panel's primary source; `others` carry the forms of the rest.
pub(crate) fn start_write_jobs(
    world: &mut World,
    plans: Vec<crate::asset_ops_agent::SettingsPlan>,
    origin: ApplyOrigin,
    form: SettingsObject,
    others: Vec<(String, SettingsObject)>,
) -> Result<String, RpcError> {
    if let Some(error) = running_error(world) {
        return Err(error);
    }
    let cook = world
        .get_resource::<SettingsCook>()
        .cloned()
        .unwrap_or_default()
        .0;
    let path = plans.first().map(|plan| plan.rel.clone()).unwrap_or_default();
    let job = crate::editor_jobs::WorkerJob::spawn(
        world,
        APPLY_JOB_KIND,
        APPLY_TASK,
        format!("applying import settings: {path}"),
        "regia-import-settings",
        move |cx| run_write(&cx, &plans, &cook),
    )
    .map_err(|message| reasoned("job_start_failed", message))?;
    let id = job.id.clone();
    world.insert_resource(ImportSettingsApplyJob {
        job,
        path,
        origin,
        form,
        others,
    });
    if origin == ApplyOrigin::Panel {
        with_panel(world, |panel| {
            panel.applying = Some(id.clone());
            panel.error = None;
        });
    }
    Ok(id)
}

/// The panel's Apply as a job (the button, `editor.import_settings_apply { async: true }`): checks
/// what [`ImportSettingsPanel::apply`] checks, then starts `start_write_job`. A cook lock another
/// holder has is waited for on the worker (cancellably), never on the caller's thread. A refusal
/// is also kept in `panel.error`.
///
/// # Errors
///
/// `import_settings_not_open`, `asset_settings_invalid` (a refused field, or a value the importer
/// refuses), `tools_busy` (a package, Clear caches, agent cook or Content ▸ Import job runs),
/// `import_settings_apply_running`, or what `editor.set_import_settings` refuses.
pub fn start_panel_apply(world: &mut World) -> Result<String, RpcError> {
    let busy = apply_busy_reason(world);
    let running = running_error(world);
    let checked = with_panel(world, |panel| {
        let checked = panel.check_apply().and_then(|(root, path, text)| {
            if let Some(message) = &busy {
                return Err(busy_error(&format!(
                    "a cook, package, clean or import job holds the project: {message}"
                )));
            }
            if let Some(error) = running.clone() {
                return Err(error);
            }
            let mut plans = vec![crate::asset_ops_agent::plan_import_settings(
                &root,
                &path,
                text.as_deref(),
            )?];
            for (extra, text) in panel.extra_writes() {
                plans.push(crate::asset_ops_agent::plan_import_settings(
                    &root,
                    &extra,
                    text.as_deref(),
                )?);
            }
            let others = panel
                .others
                .iter()
                .map(|o| (o.path.clone(), o.form.clone()))
                .collect();
            Ok((plans, panel.form.clone(), others))
        });
        if let Err(error) = &checked {
            panel.error = Some((error_code(error), error.message.clone()));
        }
        checked
    })?;
    let (plans, form, others) = checked;
    start_write_jobs(world, plans, ApplyOrigin::Panel, form, others).inspect_err(|error| {
        with_panel(world, |panel| {
            panel.error = Some((error_code(error), error.message.clone()));
        });
    })
}

/// The `data.reason` of a refusal (`apply_failed` without one).
fn error_code(error: &RpcError) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("apply_failed")
        .to_string()
}

/// Applies an ended write to the world and ends its job: on success the content index is
/// invalidated (the cook changed `assets/`' cooked state) and a panel Apply's form becomes the
/// saved state (`recooks` +1); a cancel or a failure is kept in `panel.error`. Answers the
/// method result (`Err` for a cancel or a failure).
fn finish_write(
    world: &mut World,
    job: &ImportSettingsApplyJob,
    end: Result<ApplyEnd, String>,
) -> Result<Value, RpcError> {
    let end = end.unwrap_or_else(|message| ApplyEnd::Failed {
        reason: "import_settings_failed",
        message,
    });
    let panel_owned = job.origin == ApplyOrigin::Panel
        && world
            .get_resource::<ImportSettingsPanel>()
            .is_some_and(|panel| panel.path.as_deref() == Some(job.path.as_str()));
    if let Some(mut panel) = world.get_resource_mut::<ImportSettingsPanel>()
        && panel.applying.as_deref() == Some(job.job.id.as_str())
    {
        panel.applying = None;
    }
    match end {
        ApplyEnd::Applied(out) => {
            crate::content_index::invalidate(world);
            if panel_owned {
                with_panel(world, |panel| {
                    panel.saved = job.form.clone();
                    for (path, form) in &job.others {
                        if let Some(other) = panel.others.iter_mut().find(|o| o.path == *path) {
                            other.saved = form.clone();
                        }
                    }
                    panel.recooks += 1;
                    panel.problem = None;
                    panel.error = None;
                    if panel.form == job.form {
                        panel.buffers.clear();
                        panel.invalid.clear();
                    }
                    panel.last_apply = Some(out.clone());
                });
            }
            job.job.end(world, Ok((out.clone(), false)));
            Ok(out)
        }
        ApplyEnd::Cancelled { recooked } => {
            if recooked {
                crate::content_index::invalidate(world);
            }
            let message = format!(
                "the write of {}'s import settings was cancelled: its .meta and cooked output are as before",
                job.path
            );
            if panel_owned {
                with_panel(world, |panel| {
                    panel.error = Some(("import_settings_cancelled".into(), message.clone()));
                });
            }
            job.job.end(
                world,
                Ok((
                    json!({ "path": job.path, "cancelled": true, "recooked": recooked }),
                    true,
                )),
            );
            Err(reasoned("import_settings_cancelled", message))
        }
        ApplyEnd::Failed { reason, message } => {
            if panel_owned {
                with_panel(world, |panel| {
                    panel.error = Some((reason.to_string(), message.clone()));
                });
            }
            job.job.end(world, Err((reason, message.clone())));
            Err(if reason == "import_settings_cook_failed" {
                // `editor.set_import_settings`' old internal error for a failed cook, with a reason.
                let mut error = RpcError::internal(format!("cook failed: {message}"));
                if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
                    data.insert("reason".into(), json!(reason));
                }
                error
            } else {
                reasoned(reason, message)
            })
        }
    }
}

/// Polls the running import-settings write: copies its progress into its task row and, once the
/// worker has returned, applies its end (`finish_write`). Never waits. The editor runs it every
/// frame ([`crate::editor_jobs::tick`]).
pub fn tick_apply_job(world: &mut World) {
    let Some(mut job) = world.remove_resource::<ImportSettingsApplyJob>() else {
        return;
    };
    match job.job.take() {
        Some(end) => {
            let _ = finish_write(world, &job, end);
        }
        None => {
            let fraction = job.job.context().fraction();
            job.job.sync_row(
                world,
                &format!("applying import settings: {}", job.path),
                fraction,
            );
            world.insert_resource(job);
        }
    }
}

/// Waits for the running write and applies its end: the blocking form of the methods (no
/// `async`). Answers the method result.
fn finish_write_now(world: &mut World) -> Result<Value, RpcError> {
    let Some(mut job) = world.remove_resource::<ImportSettingsApplyJob>() else {
        return Err(RpcError::internal("the import settings job is gone"));
    };
    let end = job
        .job
        .join()
        .unwrap_or_else(|| Err("the worker's end was already taken".into()));
    finish_write(world, &job, end)
}

/// `editor.set_import_settings` after its parameters are read (ADR-7664): the write is job
/// `import_settings:<n>`; with `async` the method answers `{ job }` at once, without it waits for
/// the job and answers its result plus `job`. A blocking call never waits on the cook lock: while
/// another holder has it the call is refused with `tools_busy` (an `async` job waits for it).
///
/// # Errors
///
/// `import_settings_apply_running`, `tools_busy`, or what [`crate::asset_ops_agent`]'s checks
/// refuse; a blocking call also its job's failure (`import_settings_cook_failed` as an internal
/// error) or cancel (`import_settings_cancelled`).
pub(crate) fn set_import_settings_job(
    world: &mut World,
    root: &Path,
    path: &str,
    settings: Option<&str>,
    asynchronous: bool,
) -> Result<Value, RpcError> {
    if let Some(error) = running_error(world) {
        return Err(error);
    }
    if !asynchronous && let Some(lock) = cook_lock_holder(root) {
        return Err(busy_error(&format!(
            "a cook holds {}: call again once it ends, or pass async: true to wait for it in a job",
            lock.display()
        )));
    }
    let plan = crate::asset_ops_agent::plan_import_settings(root, path, settings)?;
    let id = start_write_job(world, plan, ApplyOrigin::Method, SettingsObject::default())?;
    if asynchronous {
        return Ok(json!({ "job": id }));
    }
    let mut out = finish_write_now(world)?;
    if let Some(object) = out.as_object_mut() {
        object.insert("job".into(), json!(id));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------------------------

/// Draws the Import settings window when it is open (the Content Browser calls it each frame).
pub fn import_settings_window(ctx: &egui::Context, world: &mut World) {
    // The running write's end lands here too (the editor's frame tick also polls it).
    tick_apply_job(world);
    // Apply cooks the project, so it must not run while a package, a Clear caches, an agent
    // cook or a Content ▸ Import job holds the cooked cache, the asset database and the paks:
    // the button is disabled then (rev-claude-63 finding A), as Tools ▸ Rebuild asset database
    // is. The write itself runs as a job (ADR-7664): a cook lock another holder has is waited
    // for on its worker, never on the UI thread, and one write runs at a time.
    let busy = apply_busy_reason(world).is_some();
    let running = world
        .get_resource::<ImportSettingsApplyJob>()
        .map(|job| (job.job.id.clone(), job.path.clone(), job.job.context().message()));
    let Some(mut panel) = world.remove_resource::<ImportSettingsPanel>() else {
        return;
    };
    let mut cancel = false;
    if panel.open {
        let mut open = true;
        egui::Window::new("Import settings")
            .id(egui::Id::new("import-settings-window"))
            .open(&mut open)
            .resizable(true)
            .default_width(420.0)
            .show(ctx, |ui| {
                import_settings_ui(ui, &mut panel, busy || running.is_some());
                if let Some((id, path, message)) = &running {
                    cancel = apply_progress_ui(ui, id, path, message);
                }
            });
        panel.open = open && panel.open;
    }
    let requested = std::mem::take(&mut panel.apply_requested);
    world.insert_resource(panel);
    if cancel && let Some(job) = world.get_resource::<ImportSettingsApplyJob>() {
        job.job.cancel();
    }
    if requested {
        // A refusal is kept in `panel.error` and shown in the window.
        let _ = start_panel_apply(world);
    }
}

/// The running write's row in the window: a spinner, what it does, and Cancel
/// (`editor.cancel_job { id }`). Answers whether Cancel was clicked.
fn apply_progress_ui(ui: &mut egui::Ui, id: &str, path: &str, message: &str) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.spinner();
        ui.weak(format!("{id}: {path}: {message}"));
        clicked = crate::agent_parity::action_button(ui, &crate::agent_parity::IMPORT_SETTINGS_APPLY_CANCEL)
            .clicked();
    });
    clicked
}

/// The window's contents. `busy`: a package, Clear caches or agent cook job runs, so Apply is
/// disabled even when the form is applicable (the cook would block the UI thread on the project
/// cook lock).
pub fn import_settings_ui(ui: &mut egui::Ui, panel: &mut ImportSettingsPanel, busy: bool) {
    let (Some(path), Some(kind)) = (panel.path.clone(), panel.kind) else {
        ui.weak("Right-click an image, mesh or sound in the Content Browser: Import settings…");
        return;
    };
    if panel.others.is_empty() {
        ui.strong(&path);
    } else {
        ui.strong(format!("{} sources", panel.others.len() + 1));
        for source in panel.sources() {
            ui.weak(source);
        }
    }
    ui.weak(format!("{} importer", kind.name()));
    if let Some(problem) = &panel.problem {
        ui.colored_label(ui.visuals().warn_fg_color, problem);
    }
    settings_form_ui(ui, panel);
    // Named presets per importer (ADR-7904), after the form so Tab reaches the fields first.
    ui.separator();
    crate::import_presets::presets_ui(ui, panel);
    apply_row_ui(ui, panel, busy);
}

/// The settings grid of `panel`'s importer: a widget and a Reset per field, then the keys the
/// form keeps as written. The Import settings window and the import dialog draw it.
pub fn settings_form_ui(ui: &mut egui::Ui, panel: &mut ImportSettingsPanel) {
    let Some(kind) = panel.kind else {
        return;
    };
    egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
        egui::Grid::new("import-settings-grid")
            .num_columns(2)
            .striped(true)
            .show(ui, |ui| {
                for spec in kind.fields() {
                    let label = ui
                        .label(spec.label)
                        .on_hover_text(format!("`{}` (default: {})", spec.key, spec.default));
                    ui.horizontal(|ui| {
                        field_widget(ui, panel, spec, label.id);
                        // Back to the importer's default, in every source (ADR-7744).
                        if (panel.form.get(spec.key).is_some() || panel.mixed(spec.key))
                            && crate::agent_parity::action_button_text(
                                ui,
                                &crate::agent_parity::IMPORT_SETTINGS_FIELD_RESET,
                                "Reset",
                            )
                            .on_hover_text(format!("Reset `{}` to the importer default", spec.key))
                            .clicked()
                            && let Err(error) = panel.reset_field(spec.key)
                        {
                            panel.error = Some(("asset_settings_invalid".into(), error.message));
                        }
                    });
                    ui.end_row();
                }
            });
        let known: Vec<&str> = kind.fields().iter().map(|spec| spec.key).collect();
        let other: Vec<String> = panel
            .form
            .fields
            .iter()
            .filter(|(key, _)| !known.contains(&key.as_str()))
            .map(|(key, value)| format!("{key}: {value}"))
            .collect();
        if !other.is_empty() {
            ui.weak(format!("Kept as written: {}", other.join(", ")));
        }
    });
}

/// The window's Apply / Revert row and the last result or refusal.
fn apply_row_ui(ui: &mut egui::Ui, panel: &mut ImportSettingsPanel, busy: bool) {
    ui.separator();
    ui.horizontal(|ui| {
        let dirty = panel.dirty();
        if crate::agent_parity::action_enabled(
            ui,
            panel.can_apply() && !busy,
            &crate::agent_parity::IMPORT_SETTINGS_APPLY,
            None,
        )
        .clicked()
        {
            // The window starts the Apply job after drawing (ADR-7664); a refusal is kept in
            // `panel.error` and shown below.
            panel.apply_requested = true;
        }
        if panel.applying.is_some() {
            ui.weak("applying…");
        } else if busy {
            ui.weak("a package, import or cook runs");
        }
        if crate::agent_parity::action_enabled(ui, dirty, &crate::agent_parity::IMPORT_SETTINGS_REVERT, None)
            .clicked()
        {
            panel.revert();
        }
        if dirty {
            ui.weak("unsaved");
        }
    });
    if let Some((code, message)) = &panel.error {
        ui.colored_label(ui.visuals().error_fg_color, crate::data_panels::sentence(message))
            .on_hover_text(format!("Agent error code: {code}"));
    } else if let Some(out) = &panel.last_apply {
        ui.weak(format!(
            "Applied: cooked {}, skipped {}, {} error(s)",
            out["cooked"],
            out["skipped"],
            out["errors"].as_array().map_or(0, Vec::len)
        ));
    }
}

/// One field's widget: combos for bools and enums, a text field for numbers, strings and blocks.
fn field_widget(ui: &mut egui::Ui, panel: &mut ImportSettingsPanel, spec: &FieldSpec, label: egui::Id) {
    // Sources that disagree show no value ("mixed") until an edit or Reset sets them all.
    let mixed = panel.mixed(spec.key);
    let current = panel
        .form
        .get(spec.key)
        .filter(|_| !mixed)
        .map(|text| ron_to_json(spec, text).unwrap_or_else(|_| json!(text)));
    let mut choice = |ui: &mut egui::Ui, options: Vec<(String, Value)>| {
        let shown = match &current {
            None if mixed => "mixed".to_string(),
            None => format!("default ({})", spec.default),
            Some(Value::Bool(true)) => "on".to_string(),
            Some(Value::Bool(false)) => "off".to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        };
        let selected = options.iter().position(|(_, value)| match (&current, value) {
            _ if mixed => false,
            (None, Value::Null) => true,
            (Some(a), b) => a == b,
            _ => false,
        });
        let mut index = selected;
        egui::ComboBox::new(("import-settings", spec.key), "")
            .selected_text(shown)
            .show_ui(ui, |ui| {
                for (i, (text, _)) in options.iter().enumerate() {
                    ui.selectable_value(&mut index, Some(i), text.as_str());
                }
            })
            .response
            .labelled_by(label);
        if index != selected
            && let Some((_, value)) = index.and_then(|i| options.get(i))
            && let Err(error) = panel.edit(spec.key, value)
        {
            panel.error = Some(("asset_settings_invalid".into(), error.message));
        }
    };
    match spec.kind {
        FieldKind::OptBool => choice(
            ui,
            vec![
                (format!("default ({})", spec.default), Value::Null),
                ("on".into(), json!(true)),
                ("off".into(), json!(false)),
            ],
        ),
        FieldKind::Bool => choice(ui, vec![("on".into(), json!(true)), ("off".into(), json!(false))]),
        FieldKind::OptChoice(choices) => {
            let mut options = vec![(format!("default ({})", spec.default), Value::Null)];
            options.extend(choices.iter().map(|c| ((*c).to_string(), json!(c))));
            choice(ui, options);
        }
        FieldKind::Choice(choices) => {
            choice(ui, choices.iter().map(|c| ((*c).to_string(), json!(c))).collect())
        }
        FieldKind::OptU32 { .. }
        | FieldKind::F32 { .. }
        | FieldKind::OptF32 { .. }
        | FieldKind::OptString
        | FieldKind::Raw => {
            let shown = match &current {
                None => String::new(),
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
            };
            let buffer = panel.buffers.entry(spec.key.to_string()).or_insert(shown);
            let response = ui
                .add(egui::TextEdit::singleline(buffer).hint_text(if mixed {
                    "mixed".to_owned()
                } else {
                    format!("default: {}", spec.default)
                }))
                .labelled_by(label);
            if response.changed() {
                let text = buffer.clone();
                panel.text_edited(spec, &text);
            }
        }
    }
}

impl ImportSettingsPanel {
    /// A text field's new contents (one keystroke): parsed by the field's kind and set in the
    /// form when accepted. A refused value keeps the form's last accepted one and marks the field
    /// `invalid`, so Apply cannot write a value the field no longer shows (ADR-7495).
    pub(crate) fn text_edited(&mut self, spec: &FieldSpec, text: &str) {
        let text = text.trim();
        let value = if text.is_empty() {
            Value::Null
        } else {
            match spec.kind {
                FieldKind::OptU32 { .. } => text.parse::<u64>().map_or(json!(text), |n| json!(n)),
                FieldKind::F32 { .. } | FieldKind::OptF32 { .. } => {
                    text.parse::<f64>().map_or(json!(text), |x| json!(x))
                }
                _ => json!(text),
            }
        };
        // Not `edit`: the buffer holds what the user is typing and must survive an accepted
        // keystroke (only an edit from outside the field replaces it).
        match self.set_value(spec.key, &value) {
            Ok(()) => self.accepted(spec.key),
            Err(error) => {
                self.invalid.insert(spec.key.to_string());
                self.error = Some(("asset_settings_invalid".into(), error.message));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Agent methods
// ---------------------------------------------------------------------------------------------

/// The project root from the editor's resources.
fn project_root(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| {
            RpcError::not_found(
                "no project is open",
                "start the editor with --project <dir> (or open one in the project browser)",
            )
        })
}

fn with_panel<T>(world: &mut World, f: impl FnOnce(&mut ImportSettingsPanel) -> T) -> T {
    let mut panel = world.get_resource_or_insert_with(ImportSettingsPanel::default);
    f(&mut panel)
}

/// `editor.import_settings_open`.
fn import_settings_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // `paths` opens several sources of one importer to edit together (ADR-7744); `path` is one.
    let mut paths: Vec<&str> = match params.get("paths") {
        Some(Value::Array(list)) => list
            .iter()
            .map(|p| {
                p.as_str().filter(|p| !p.is_empty()).ok_or_else(|| {
                    RpcError::invalid_params(
                        "`paths` holds non-empty strings (project-relative, under assets/)",
                    )
                })
            })
            .collect::<Result<_, _>>()?,
        Some(Value::Null) | None => Vec::new(),
        Some(_) => return Err(RpcError::invalid_params("`paths` is an array of source paths")),
    };
    if let Some(path) = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
    {
        paths.insert(0, path);
    }
    if paths.is_empty() {
        return Err(RpcError::invalid_params(
            "`path` (project-relative, under assets/) or `paths` is required",
        ));
    }
    let root = project_root(world)?;
    with_panel(world, |panel| {
        panel.open_paths(&root, &paths)?;
        Ok(panel.state_json())
    })
}

/// `editor.import_settings_edit`.
fn import_settings_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let field = params
        .get("field")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`field` (a settings key) is required"))?;
    // `reset: true` is the field's Reset button: the importer's default, in every source.
    let reset = params.get("reset").and_then(Value::as_bool) == Some(true);
    let value = match params.get("value") {
        Some(value) if !reset => value,
        _ if reset => &Value::Null,
        _ => {
            return Err(RpcError::invalid_params(
                "`value` is required (null = the importer's default), or pass `reset: true`",
            ));
        }
    };
    with_panel(world, |panel| {
        panel.edit(field, value)?;
        Ok(panel.state_json())
    })
}

/// The `tools_busy` refusal, the same code Tools ▸ Rebuild asset database uses: a package, a
/// Clear caches, an agent cook or a Content ▸ Import job runs, or another holder has the cook
/// lock, so the cook cannot start now.
fn busy_error(message: &str) -> RpcError {
    let mut error = RpcError::new(
        regia_agent::protocol::INVALID_PARAMS,
        "F0002",
        message.to_string(),
        "see book/agents/import-settings.md",
    );
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), Value::String("tools_busy".into()));
    }
    error
}

/// `editor.import_settings_apply { async? }`: the panel's Apply as job `import_settings:<n>`
/// (ADR-7664). With `async` it answers `{ job, state }` at once; without it, it waits for the job
/// (refusing with `tools_busy` first while another holder has the cook lock, so the wait is never
/// on the lock) and answers `{ result, state, job }`.
fn import_settings_apply(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    tick_apply_job(world);
    if let Some(running) = apply_busy_reason(world) {
        let message = format!("a cook, package, clean or import job holds the project: {running}");
        with_panel(world, |panel| {
            panel.error = Some(("tools_busy".into(), message.clone()));
        });
        return Err(busy_error(&message));
    }
    let asynchronous = params.get("async").and_then(Value::as_bool) == Some(true);
    if !asynchronous && running_error(world).is_none() {
        let lock = world
            .get_resource::<ImportSettingsPanel>()
            .and_then(|panel| panel.root.clone())
            .and_then(|root| cook_lock_holder(&root));
        if let Some(lock) = lock {
            let message = format!(
                "a cook holds {}: Apply once it ends (the form is kept), or pass async: true to wait for it in a job",
                lock.display()
            );
            with_panel(world, |panel| {
                panel.error = Some(("tools_busy".into(), message.clone()));
            });
            return Err(busy_error(&message));
        }
    }
    let id = start_panel_apply(world)?;
    if asynchronous {
        let state = with_panel(world, |panel| panel.state_json());
        return Ok(json!({ "job": id, "state": state }));
    }
    let result = finish_write_now(world)?;
    let state = with_panel(world, |panel| panel.state_json());
    Ok(json!({ "result": result, "state": state, "job": id }))
}

/// `editor.import_settings_revert`.
fn import_settings_revert(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel| {
        panel.open_kind()?;
        panel.revert();
        Ok(panel.state_json())
    })
}

/// `editor.import_settings_state` (a running Apply's end is applied first).
fn import_settings_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    tick_apply_job(world);
    Ok(with_panel(world, |panel| panel.state_json()))
}

/// The panel's agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.import_settings_open",
                tool: "import_settings_open",
                description: "Content ▸ Import settings…: open a source's importer settings (image, \
                    glTF/GLB, OBJ, FBX, sound) in the typed panel. The source needs a .meta \
                    (asset_not_imported otherwise). Returns the panel state.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "Project-relative source path under assets/. Required unless paths is given.",
                    },
                    ParamSchema {
                        name: "paths",
                        kind: "array",
                        required: false,
                        description: "Several sources of one importer to edit together (after path, if both are given): edits and resets go to every source, a field whose values differ reads `mixed: true` with a null value, and Apply writes each changed .meta and cooks once. Sources of different importers are refused (import_settings_mixed_importers).",
                    },
                ],
                result: "{ open, path, importer, dirty, settings, saved, fields: [{key, label, kind, set, value, default, choices?}], other_fields, problem, error, invalid, can_apply, recooks, last_apply, applying }",
                mutating: true,
                permission: Permission::Assets,
            },
            import_settings_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.import_settings_edit",
                tool: "import_settings_edit",
                description: "Set one field of the open Import settings panel (not written until \
                    editor.import_settings_apply). value is typed by the field's kind: bool, a \
                    choice string, a number, a string, or RON text for raw blocks; null removes \
                    the key (the importer's default). Out of range is asset_settings_invalid. A \
                    successful edit also clears the field's refused mark and text buffer, so the \
                    panel agrees with what the UI shows.",
                params: &[
                    ParamSchema {
                        name: "field",
                        kind: "string",
                        required: true,
                        description: "A settings key from the state's fields (max_size, collision, ...).",
                    },
                    ParamSchema {
                        name: "value",
                        kind: "any",
                        required: false,
                        description: "The typed value, or null for the default. Required unless reset is true.",
                    },
                    ParamSchema {
                        name: "reset",
                        kind: "boolean",
                        required: false,
                        description: "true is the field's Reset button: the importer's default in every edited source (same as value: null).",
                    },
                ],
                result: "the panel state",
                mutating: true,
                permission: Permission::Assets,
            },
            import_settings_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.import_settings_apply",
                tool: "import_settings_apply",
                description: "The panel's Apply: write the form into the source's .meta exactly as \
                    editor.set_import_settings would with the same settings, then cook once \
                    (recooks counts it). An empty form clears the override. The write and the \
                    cook run as job import_settings:<n> (agent.job_status; editor.cancel_job \
                    cancels it and puts the old .meta and cooked output back); with async: true \
                    the method answers { job, state } at once, otherwise it waits for the job. \
                    Refused with `tools_busy` while a package, Clear caches, agent cook or \
                    Content Import job runs (Apply is disabled then) or, without async, while \
                    another holder has the project cook lock (an async job waits for it), and \
                    with `import_settings_apply_running` while another write runs.",
                params: &[ParamSchema {
                    name: "async",
                    kind: "boolean",
                    required: false,
                    description: "true answers { job, state } at once; the job's end shows in agent.job_status and editor.import_settings_state.",
                }],
                result: "{ result: { path, settings, cooked, skipped, errors }, state, job } (async: { job, state })",
                mutating: true,
                permission: Permission::Assets,
            },
            import_settings_apply as Handler,
        ),
        (
            MethodSchema {
                name: "editor.import_settings_revert",
                tool: "import_settings_revert",
                description: "The panel's Revert: drop the unsaved edits (the form becomes the .meta's settings).",
                params: &[],
                result: "the panel state",
                mutating: true,
                permission: Permission::Assets,
            },
            import_settings_revert as Handler,
        ),
        (
            MethodSchema {
                name: "editor.import_settings_state",
                tool: "import_settings_state",
                description: "The Import settings panel's state: the open source, its importer, \
                    the typed field values, dirty, the fields whose last edit was refused \
                    (`invalid`), whether Apply would run (`can_apply`), the last refusal and the \
                    recook count.",
                params: &[],
                result: "the panel state (see editor.import_settings_open)",
                mutating: false,
                permission: Permission::Read,
            },
            import_settings_state as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png() -> Vec<u8> {
        use image::ImageEncoder;
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&[255u8, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .expect("png encode");
        bytes
    }

    /// A scratch project with one imported (cooked) PNG at `assets/incoming.png`.
    fn project(label: &str) -> (World, PathBuf) {
        let root = regia_test_utils::unique_temp_dir(label);
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        let source = root.join("incoming.png");
        std::fs::write(&source, tiny_png()).expect("png");
        crate::content::import_files(&root, "", std::slice::from_ref(&source), Some("test-agent"))
            .expect("import");
        let mut world = World::new();
        world.insert_resource(EditorProject {
            root: Some(root.clone()),
        });
        (world, root)
    }

    /// Copies a directory tree (a scratch project, its `.meta` files and dev cache).
    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).expect("dir");
        for entry in std::fs::read_dir(from).expect("read_dir").flatten() {
            let target = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).expect("copy");
            }
        }
    }

    /// Calls a method through the registered extension, as dispatch does.
    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut extension = world
            .get_resource::<regia_agent::extension::AgentExtension>()
            .cloned()
            .unwrap_or_default();
        for (schema, handler) in methods().into_iter().chain(crate::asset_ops_agent::methods()) {
            extension.register(schema, handler);
        }
        world.insert_resource(extension);
        let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
        let (_, handler) = extension
            .method(method)
            .unwrap_or_else(|| panic!("{method} is not registered"));
        handler(world, &params)
    }

    fn reason(result: Result<Value, RpcError>) -> String {
        match result {
            Ok(value) => panic!("expected an error, got {value}"),
            Err(error) => error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("<no reason>")
                .to_string(),
        }
    }

    #[test]
    fn importer_kinds_follow_extensions() {
        assert_eq!(ImporterKind::of_path("assets/a.PNG"), Some(ImporterKind::Texture));
        assert_eq!(ImporterKind::of_path("assets/a.glb"), Some(ImporterKind::Gltf));
        assert_eq!(ImporterKind::of_path("assets/a.fbx"), Some(ImporterKind::Fbx));
        assert_eq!(ImporterKind::of_path("assets/a.obj"), Some(ImporterKind::Obj));
        assert_eq!(ImporterKind::of_path("assets/a.ogg"), Some(ImporterKind::Audio));
        assert_eq!(ImporterKind::of_path("assets/a.level.ron"), None);
        // Every field of every form is a key the importer type reads: a one-field object with
        // the field's default-ish value deserializes.
        for kind in [
            ImporterKind::Texture,
            ImporterKind::Gltf,
            ImporterKind::Obj,
            ImporterKind::Fbx,
            ImporterKind::Audio,
        ] {
            let keys: Vec<&str> = kind.fields().iter().map(|s| s.key).collect();
            let mut unique = keys.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), keys.len(), "{kind:?} lists a key twice");
        }
    }

    #[test]
    fn typed_values_round_trip_and_refuse_out_of_range() {
        let spec = ImporterKind::Texture.field("max_size").expect("max_size");
        assert_eq!(json_to_ron(spec, &json!(512)), Ok(Some("Some(512)".into())));
        assert_eq!(ron_to_json(spec, "Some(512)"), Ok(json!(512)));
        assert!(json_to_ron(spec, &json!(0)).is_err());
        assert_eq!(json_to_ron(spec, &Value::Null), Ok(None));
        let usage = ImporterKind::Texture.field("usage").expect("usage");
        assert_eq!(
            json_to_ron(usage, &json!("linear")),
            Ok(Some("Some(linear)".into()))
        );
        assert!(json_to_ron(usage, &json!("srgb")).is_err());
        let scale = ImporterKind::Gltf.field("scale").expect("scale");
        assert_eq!(json_to_ron(scale, &json!(2.5)), Ok(Some("2.5".into())));
        assert!(json_to_ron(scale, &json!(0.0)).is_err());
        let uv = ImporterKind::Fbx.field("uv_set").expect("uv_set");
        let text = json_to_ron(uv, &json!("map \"2\"")).expect("uv").expect("set");
        assert_eq!(ron_to_json(uv, &text), Ok(json!("map \"2\"")));
        let raw = ImporterKind::Gltf.field("generate_lods").expect("lods");
        assert!(json_to_ron(raw, &json!("Some((count: 3")).is_err());
        // Whole-object checks: the method's validator.
        assert!(validate_settings("assets/a.png", "(max_size: Some(64))").is_ok());
        assert!(validate_settings("assets/a.png", "(max_size: Some(0))").is_err());
        assert!(validate_settings("assets/a.png", "(usage: Some(purple))").is_err());
        assert!(validate_settings("assets/a.glb", "(collision_hulls: (max_hulls: 0))").is_err());
        assert!(validate_settings("assets/a.wav", "(target_lufs: Some(12.0))").is_err());
        assert!(validate_settings("assets/a.level.ron", "(anything: 1)").is_ok());
        // Keys the form has no widget for are not refused (the cook warns about them).
        assert!(validate_settings("assets/a.png", "(future_key: 3)").is_ok());
    }

    #[test]
    fn panel_apply_writes_the_method_bytes_and_recooks_once() {
        // ADR-7469 acceptance: Apply from the panel writes the same `.meta` bytes as
        // `editor.set_import_settings` with the same values, and triggers one recook.
        let (mut world, root) = project("import-settings-apply");
        let meta_path = root.join("assets/incoming.png.meta");
        let original = std::fs::read(&meta_path).expect("meta");
        let twin = regia_test_utils::unique_temp_dir("import-settings-apply-twin");
        copy_tree(&root, &twin);

        let state = call(
            &mut world,
            "editor.import_settings_open",
            json!({ "path": "assets/incoming.png" }),
        )
        .expect("open");
        assert_eq!(state["importer"], "texture");
        assert_eq!(state["dirty"], false);
        assert_eq!(state["recooks"], 0);
        call(
            &mut world,
            "editor.import_settings_edit",
            json!({ "field": "max_size", "value": 64 }),
        )
        .expect("max_size");
        let state = call(
            &mut world,
            "editor.import_settings_edit",
            json!({ "field": "compress", "value": false }),
        )
        .expect("compress");
        assert_eq!(state["dirty"], true);
        let settings = state["settings"].as_str().expect("settings text").to_string();
        assert_eq!(settings, "(max_size: Some(64), compress: Some(false))");
        // The state reports the panel's values, typed.
        let field = |state: &Value, key: &str| {
            state["fields"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["key"] == key))
                .cloned()
                .unwrap_or(Value::Null)
        };
        assert_eq!(field(&state, "max_size")["value"], 64);
        assert_eq!(field(&state, "compress")["value"], false);
        assert_eq!(field(&state, "usage")["set"], false);

        let applied = call(&mut world, "editor.import_settings_apply", json!({})).expect("apply");
        assert!(
            applied["result"]["cooked"].as_u64().is_some_and(|c| c >= 1),
            "the apply cooked the source: {applied}"
        );
        assert_eq!(applied["state"]["recooks"], 1, "one Apply, one recook");
        assert_eq!(applied["state"]["dirty"], false);
        let from_panel = std::fs::read(&meta_path).expect("meta after apply");
        assert_ne!(from_panel, original, "the apply changed the .meta");

        // The method with the same values, in a copy of the project taken before the apply
        // (same sources, `.meta` id and dev cache).
        let mut twin_world = World::new();
        twin_world.insert_resource(EditorProject {
            root: Some(twin.clone()),
        });
        let out = call(
            &mut twin_world,
            "editor.set_import_settings",
            json!({ "path": "assets/incoming.png", "settings": settings }),
        )
        .expect("set_import_settings");
        assert_eq!(out["settings"], applied["result"]["settings"]);
        assert!(
            out["cooked"].as_u64().is_some_and(|c| c >= 1),
            "the method cooked too: {out}"
        );
        let from_method = std::fs::read(twin.join("assets/incoming.png.meta")).expect("meta after method");
        assert_eq!(
            String::from_utf8_lossy(&from_panel),
            String::from_utf8_lossy(&from_method),
            "the panel and the method write the same .meta"
        );

        // The state survives a re-read: it reports what the panel holds.
        let state = call(&mut world, "editor.import_settings_state", json!({})).expect("state");
        assert_eq!(field(&state, "max_size")["value"], 64);
        assert_eq!(state["recooks"], 1);
    }

    /// Imports a second PNG into a `project()` (as `assets/second.png`).
    fn add_second_source(root: &Path) {
        let source = root.join("second.png");
        std::fs::write(&source, tiny_png()).expect("png");
        crate::content::import_files(root, "", std::slice::from_ref(&source), Some("test-agent"))
            .expect("import second");
    }

    fn meta_settings(root: &Path, rel: &str) -> Option<String> {
        let bytes = std::fs::read(root.join(rel)).expect("meta");
        regia_asset::MetaSidecar::from_ron(&bytes)
            .expect("parse")
            .settings
    }

    fn field_row(state: &Value, key: &str) -> Value {
        state["fields"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["key"] == key))
            .cloned()
            .unwrap_or(Value::Null)
    }

    #[test]
    fn a_field_reset_returns_it_to_the_importer_default() {
        let (mut world, _root) = project("import-settings-reset");
        call(
            &mut world,
            "editor.import_settings_open",
            json!({ "path": "assets/incoming.png" }),
        )
        .expect("open");
        let state = call(
            &mut world,
            "editor.import_settings_edit",
            json!({ "field": "max_size", "value": 64 }),
        )
        .expect("edit");
        assert_eq!(field_row(&state, "max_size")["set"], true);
        assert_eq!(state["dirty"], true);
        let state = call(
            &mut world,
            "editor.import_settings_edit",
            json!({ "field": "max_size", "reset": true }),
        )
        .expect("reset");
        assert_eq!(field_row(&state, "max_size")["set"], false, "{state}");
        assert_eq!(
            state["dirty"], false,
            "back to the saved (default) state: {state}"
        );
        // The panel model's own entry point.
        let mut panel = ImportSettingsPanel::default();
        let root = match world.resource::<EditorProject>().root.clone() {
            Some(root) => root,
            None => unreachable!("project() sets the root"),
        };
        panel.open_path(&root, "assets/incoming.png").expect("open");
        panel.edit("compress", &json!(false)).expect("edit");
        assert!(panel.dirty());
        panel.reset_field("compress").expect("reset");
        assert!(!panel.dirty());
        let err = call(
            &mut world,
            "editor.import_settings_edit",
            json!({ "field": "max_size" }),
        )
        .expect_err("value or reset");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
    }

    #[test]
    fn two_sources_edit_together_and_mixed_values_show() {
        let (mut world, root) = project("import-settings-multi");
        add_second_source(&root);
        // The second source starts with its own max_size.
        let second_meta = root.join("assets/second.png.meta");
        let mut meta =
            regia_asset::MetaSidecar::from_ron(&std::fs::read(&second_meta).expect("meta")).expect("parse");
        meta.settings = Some("(max_size: Some(128))".to_owned());
        std::fs::write(&second_meta, meta.to_ron_string().expect("ron")).expect("write meta");

        let state = call(
            &mut world,
            "editor.import_settings_open",
            json!({ "path": "assets/incoming.png", "paths": ["assets/second.png"] }),
        )
        .expect("open both");
        assert_eq!(
            state["sources"],
            json!(["assets/incoming.png", "assets/second.png"])
        );
        let max = field_row(&state, "max_size");
        assert_eq!(max["mixed"], true, "{state}");
        assert_eq!(max["value"], Value::Null, "a mixed field reads no value: {state}");
        assert_eq!(field_row(&state, "compress")["mixed"], false);
        assert_eq!(state["dirty"], false);

        // One edit sets every source; the field is no longer mixed.
        let state = call(
            &mut world,
            "editor.import_settings_edit",
            json!({ "field": "max_size", "value": 64 }),
        )
        .expect("edit");
        let max = field_row(&state, "max_size");
        assert_eq!(
            (max["mixed"].clone(), max["value"].clone()),
            (json!(false), json!(64)),
            "{state}"
        );
        assert_eq!(state["dirty"], true);
        // Apply writes both sources' .meta and cooks once.
        let applied = call(&mut world, "editor.import_settings_apply", json!({})).expect("apply");
        assert_eq!(applied["state"]["recooks"], 1, "{applied}");
        assert_eq!(
            applied["result"]["paths"],
            json!(["assets/incoming.png", "assets/second.png"]),
            "{applied}"
        );
        assert_eq!(
            meta_settings(&root, "assets/incoming.png.meta").as_deref(),
            Some("(max_size: Some(64))")
        );
        assert_eq!(
            meta_settings(&root, "assets/second.png.meta").as_deref(),
            Some("(max_size: Some(64))")
        );
        assert_eq!(applied["state"]["dirty"], false, "{applied}");

        // Reset clears the field in both sources, and Apply writes both back to the default.
        let state = call(
            &mut world,
            "editor.import_settings_edit",
            json!({ "field": "max_size", "reset": true }),
        )
        .expect("reset");
        assert_eq!(state["dirty"], true, "{state}");
        call(&mut world, "editor.import_settings_apply", json!({})).expect("apply reset");
        assert_eq!(meta_settings(&root, "assets/incoming.png.meta"), None);
        assert_eq!(meta_settings(&root, "assets/second.png.meta"), None);
    }

    /// Review finding S5: the synchronous Apply over several sources checks every source before
    /// it writes any `.meta`; a refused later source leaves the first one untouched.
    #[test]
    fn a_refused_source_leaves_every_meta_unwritten() {
        let (_world, root) = project("import-settings-multi-refused");
        add_second_source(&root);
        let mut panel = ImportSettingsPanel::default();
        panel
            .open_paths(&root, &["assets/incoming.png", "assets/second.png"])
            .expect("open");
        panel.edit("max_size", &json!(64)).expect("edit");
        let first_before = std::fs::read(root.join("assets/incoming.png.meta")).expect("meta");
        // The second source loses its .meta after the panel opened it.
        std::fs::remove_file(root.join("assets/second.png.meta")).expect("remove meta");
        let err = panel.apply().expect_err("the second source is refused");
        assert_eq!(error_code(&err), "asset_not_imported", "{err:?}");
        assert_eq!(
            std::fs::read(root.join("assets/incoming.png.meta")).expect("meta"),
            first_before,
            "the first source's .meta is not written"
        );
        assert!(panel.dirty(), "the form is kept for another Apply");
    }

    #[test]
    fn revert_drops_every_sources_edits_and_other_importers_are_refused() {
        let (mut world, root) = project("import-settings-multi-revert");
        add_second_source(&root);
        let mut panel = ImportSettingsPanel::default();
        panel
            .open_paths(
                &root,
                &["assets/incoming.png", "assets/second.png", "assets/incoming.png"],
            )
            .expect("open");
        assert_eq!(
            panel.sources(),
            vec!["assets/incoming.png", "assets/second.png"],
            "repeats count once"
        );
        panel.edit("max_size", &json!(32)).expect("edit");
        assert!(panel.others.iter().all(|o| o.form != o.saved));
        panel.revert();
        assert!(!panel.dirty());
        assert!(panel.others.iter().all(|o| o.form == o.saved));

        // A sound is another importer.
        std::fs::write(root.join("assets/tone.wav"), b"RIFF").expect("wav");
        let wav = regia_asset::meta_path_for(&root.join("assets/tone.wav"));
        let meta = std::fs::read(root.join("assets/incoming.png.meta")).expect("png meta");
        std::fs::write(wav, meta).expect("wav meta");
        let err = call(
            &mut world,
            "editor.import_settings_open",
            json!({ "path": "assets/incoming.png", "paths": ["assets/tone.wav"] }),
        )
        .expect_err("mixed importers");
        assert_eq!(error_code(&err), "import_settings_mixed_importers", "{err:?}");
        let err = call(&mut world, "editor.import_settings_open", json!({ "paths": [] })).expect_err("empty");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
    }

    #[test]
    fn invalid_values_are_refused_with_the_method_code_and_revert_drops_edits() {
        let (mut world, root) = project("import-settings-invalid");
        call(
            &mut world,
            "editor.import_settings_open",
            json!({ "path": "incoming.png" }),
        )
        .expect("open");
        assert_eq!(
            reason(call(
                &mut world,
                "editor.import_settings_edit",
                json!({ "field": "max_size", "value": 0 })
            )),
            "asset_settings_invalid"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.set_import_settings",
                json!({ "path": "assets/incoming.png", "settings": "(max_size: Some(0))" })
            )),
            "asset_settings_invalid",
            "the method refuses the same value with the same code"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.import_settings_edit",
                json!({ "field": "scale", "value": 2.0 })
            )),
            "import_settings_unknown_field"
        );
        // A raw value the importer type refuses passes the field check but not Apply.
        let meta_before = std::fs::read(root.join("assets/incoming.png.meta")).expect("meta");
        world
            .resource_mut::<ImportSettingsPanel>()
            .form
            .fields
            .push(("usage".into(), "Some(purple)".into()));
        assert_eq!(
            reason(call(&mut world, "editor.import_settings_apply", json!({}))),
            "asset_settings_invalid"
        );
        let state = call(&mut world, "editor.import_settings_state", json!({})).expect("state");
        assert_eq!(state["error"]["code"], "asset_settings_invalid");
        assert_eq!(state["recooks"], 0, "a refused apply does not cook");
        assert_eq!(
            std::fs::read(root.join("assets/incoming.png.meta")).expect("meta"),
            meta_before,
            "a refused apply writes nothing"
        );
        let state = call(&mut world, "editor.import_settings_revert", json!({})).expect("revert");
        assert_eq!(state["dirty"], false);
        assert_eq!(state["settings"], Value::Null);

        // Refusals of open.
        std::fs::write(root.join("assets/raw.png"), tiny_png()).expect("png");
        assert_eq!(
            reason(call(
                &mut world,
                "editor.import_settings_open",
                json!({ "path": "assets/raw.png" })
            )),
            "asset_not_imported"
        );
        std::fs::write(root.join("assets/notes.txt"), b"x").expect("txt");
        assert_eq!(
            reason(call(
                &mut world,
                "editor.import_settings_open",
                json!({ "path": "assets/notes.txt" })
            )),
            "import_settings_unsupported"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.import_settings_open",
                json!({ "path": "../incoming.png" })
            )),
            "path_outside_project"
        );
        let mut fresh = World::new();
        fresh.insert_resource(EditorProject {
            root: Some(root.clone()),
        });
        assert_eq!(
            reason(call(&mut fresh, "editor.import_settings_apply", json!({}))),
            "import_settings_not_open"
        );
    }

    /// The window's controls all have accessible names (the editor's AccessKit audit), and
    /// rev-claude-57 asset finding 1 (ADR-7495): typing `max_size` 20000 passes through the
    /// accepted keystrokes 2, 20, 200, 2000 before 20000 is refused. The form keeps 2000, so the
    /// field is marked invalid and Apply refuses until it is fixed or reverted; the `.meta` is not
    /// written with the value the field no longer shows.
    #[test]
    fn a_refused_keystroke_blocks_apply_until_the_field_is_fixed() {
        let (_world, root) = project("import-settings-refused-keystroke");
        let meta_path = root.join("assets/incoming.png.meta");
        let original = std::fs::read(&meta_path).expect("meta");
        let mut panel = ImportSettingsPanel::default();
        panel.open_path(&root, "assets/incoming.png").expect("open");
        let spec = ImporterKind::Texture.field("max_size").expect("max_size");
        for typed in ["2", "20", "200", "2000", "20000"] {
            panel.text_edited(spec, typed);
        }
        assert_eq!(
            panel.form.get("max_size"),
            Some("Some(2000)"),
            "the last accepted value"
        );
        assert!(panel.dirty());
        assert!(!panel.can_apply(), "the field shows a refused value");
        let err = panel.apply().expect_err("refused");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str),
            Some("asset_settings_invalid"),
            "{err:?}"
        );
        assert_eq!(
            std::fs::read(&meta_path).expect("meta"),
            original,
            "nothing written"
        );
        assert_eq!(panel.recooks, 0);
        // Fixing the field (a backspace to 2000) makes Apply possible again.
        panel.text_edited(spec, "2000");
        assert!(panel.can_apply());
        // Revert clears a refused field too.
        panel.text_edited(spec, "0");
        assert!(!panel.can_apply());
        panel.revert();
        assert!(panel.invalid.is_empty());
        assert!(!panel.dirty());
    }

    /// rev-claude-63 finding A: Apply cooks the project, so it must not run while a package, a
    /// Clear caches or an agent cook job holds the cooked cache and the paks: the method refuses
    /// at once with `tools_busy` (the UI button is disabled at the same time), never blocking the
    /// UI thread on the project cook lock.
    #[test]
    fn apply_is_refused_while_a_cook_runs() {
        let (mut world, _root) = project("import-settings-apply-busy");
        call(
            &mut world,
            "editor.import_settings_open",
            json!({ "path": "assets/incoming.png" }),
        )
        .expect("open");
        call(
            &mut world,
            "editor.import_settings_edit",
            json!({ "field": "max_size", "value": 64 }),
        )
        .expect("edit");
        // A cook job runs (in production it holds the project cook lock).
        let mut registry = regia_agent::jobs::JobRegistry::default();
        let (_id, _handle) = registry.start("cook", "cook: the project", true);
        world.insert_resource(registry);

        assert_eq!(
            reason(call(&mut world, "editor.import_settings_apply", json!({}))),
            "tools_busy"
        );
        let state = call(&mut world, "editor.import_settings_state", json!({})).expect("state");
        assert_eq!(state["error"]["code"], json!("tools_busy"));
        assert_eq!(state["recooks"], json!(0), "a refused apply does not cook");
        assert_eq!(
            state["can_apply"],
            json!(true),
            "the form is still applicable once the job ends"
        );
    }

    /// rev-claude-63 finding B: an agent edit of a field the user typed a refused value into
    /// clears the field's `invalid` mark and its text buffer, so the panel, the UI and Apply agree
    /// again; `state_json` reports both the refused fields and `can_apply`.
    #[test]
    fn an_agent_edit_clears_the_fields_invalid_mark_and_buffer() {
        let (_world, root) = project("import-settings-agent-edit");
        let mut panel = ImportSettingsPanel::default();
        panel.open_path(&root, "assets/incoming.png").expect("open");
        let spec = ImporterKind::Texture.field("max_size").expect("max_size");
        panel.text_edited(spec, "20000");
        assert!(panel.invalid.contains("max_size"));
        assert!(!panel.can_apply());
        let state = panel.state_json();
        assert_eq!(state["can_apply"], json!(false));
        assert_eq!(state["invalid"], json!(["max_size"]));

        panel.edit("max_size", &json!(1024)).expect("agent edit");
        assert!(
            panel.invalid.is_empty(),
            "the agent's valid value clears the refused mark"
        );
        assert!(
            !panel.buffers.contains_key("max_size"),
            "the UI re-seeds the field's text from the form"
        );
        assert!(panel.can_apply());
        let state = panel.state_json();
        assert_eq!(state["can_apply"], json!(true));
        assert_eq!(state["invalid"], json!([]));
    }

    /// rev-claude-63 finding C: a successful edit of one field keeps the refused message of
    /// another still-invalid field (Apply is disabled; without the message the panel shows nothing).
    #[test]
    fn a_successful_edit_keeps_another_fields_refusal_message() {
        let (_world, root) = project("import-settings-error-kept");
        let mut panel = ImportSettingsPanel::default();
        panel.open_path(&root, "assets/incoming.png").expect("open");
        let spec = ImporterKind::Texture.field("max_size").expect("max_size");
        panel.text_edited(spec, "20000");
        assert!(panel.error.is_some(), "the refused keystroke shows a message");
        panel.edit("compress", &json!(false)).expect("edit another field");
        assert!(
            panel.error.is_some(),
            "the still-invalid field's message stays visible"
        );
        assert!(!panel.can_apply());
        let state = panel.state_json();
        assert_eq!(state["error"]["code"], json!("asset_settings_invalid"));
        // Fixing the refused field clears the message again.
        panel.text_edited(spec, "2000");
        assert!(
            panel.error.is_none(),
            "no refusal is left once every field is fixed"
        );
    }

    /// Review rev63-followups-verify (finding A, the holders `busy_reason` does not see): Apply
    /// never waits on the project cook lock. The editor's live cooker, a Content ▸ Import job's
    /// worker and `regia cook` in a terminal hold it without any package/cook job row; Apply
    /// refuses with `tools_busy` at once instead of freezing the UI thread until they end.
    #[test]
    fn apply_never_waits_on_a_cook_lock_another_holder_has() {
        let (_world, root) = project("import-settings-apply-locked");
        let mut panel = ImportSettingsPanel::default();
        panel.open_path(&root, "assets/incoming.png").expect("open");
        panel.edit("max_size", &json!(64)).expect("edit");
        let state = regia_core::brand::project_state_dir(&root);
        std::fs::create_dir_all(&state).expect("state dir");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(state.join("cook.lock"))
            .expect("lock file");
        lock.lock().expect("hold the cook lock");
        let (sent, answered) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = panel.apply();
            sent.send(()).ok();
            (panel, result)
        });
        // A hang guard, not a time budget: a fixed Apply answers without waiting; one that
        // waits on the lock gets it once the guard releases it, cooks, and fails below.
        answered.recv_timeout(std::time::Duration::from_secs(120)).ok();
        lock.unlock().expect("unlock");
        let (mut panel, result) = worker.join().expect("apply thread");
        assert_eq!(reason(result), "tools_busy");
        assert_eq!(
            panel.error.as_ref().map(|(code, _)| code.as_str()),
            Some("tools_busy")
        );
        assert_eq!(panel.recooks, 0, "a refused Apply does not cook");
        assert!(panel.can_apply(), "the form is kept for the next Apply");
        panel.apply().expect("Apply runs once the lock is free");
        assert_eq!(panel.recooks, 1);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Review rev63-followups-verify (finding A): a Content ▸ Import job cooks the project under
    /// the cook lock on its worker, so `editor.import_settings_apply` refuses with `tools_busy`
    /// while one runs, as it does for a package or an agent cook.
    #[test]
    fn apply_is_refused_while_a_content_import_runs() {
        let (mut world, root) = project("import-settings-apply-import-job");
        call(
            &mut world,
            "editor.import_settings_open",
            json!({ "path": "assets/incoming.png" }),
        )
        .expect("open");
        call(
            &mut world,
            "editor.import_settings_edit",
            json!({ "field": "max_size", "value": 64 }),
        )
        .expect("edit");
        let mut registry = regia_agent::jobs::JobRegistry::default();
        let (_id, handle) = registry.start("import", "import 1 file(s)", true);
        world.insert_resource(registry);
        assert_eq!(
            reason(call(&mut world, "editor.import_settings_apply", json!({}))),
            "tools_busy"
        );
        let state = call(&mut world, "editor.import_settings_state", json!({})).expect("state");
        assert_eq!(state["recooks"], json!(0), "a refused apply does not cook");
        handle.finish(json!({}));
        call(&mut world, "editor.import_settings_apply", json!({})).expect("applies once the import ended");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Review rev63-followups-verify: the user's keystrokes stay in the field. An accepted
    /// keystroke sets the form but must not drop the field's text buffer (only an edit from
    /// outside the field does, finding B): typing `-14.5` passes through `-14.`, which parses,
    /// and re-seeding the field from the form at that point would turn the text into `-14.0`.
    #[test]
    fn typing_a_number_keeps_the_typed_text() {
        use crate::canvas_keys::test_input::{frame, key};
        let (_world, root) = project("import-settings-typing");
        std::fs::write(root.join("assets/clip.ogg"), b"not decoded by this test").expect("ogg");
        std::fs::copy(
            root.join("assets/incoming.png.meta"),
            root.join("assets/clip.ogg.meta"),
        )
        .expect("meta");
        let mut panel = ImportSettingsPanel::default();
        panel.open_path(&root, "assets/clip.ogg").expect("open");
        let ctx = egui::Context::default();
        let size = [600.0, 900.0];
        {
            let mut draw = |ui: &mut egui::Ui| {
                egui::CentralPanel::default().show(ui, |ui| import_settings_ui(ui, &mut panel, false));
            };
            frame(&ctx, size, vec![], &mut draw);
            // Tab to the form's first text input: the audio form's `target_lufs` (the field is
            // named through `labelled_by`, so `tab_to`'s label match does not see it).
            let mut focused_text = false;
            for _ in 0..40 {
                frame(
                    &ctx,
                    size,
                    vec![key(egui::Key::Tab, egui::Modifiers::NONE)],
                    &mut draw,
                );
                let update = frame(&ctx, size, vec![], &mut draw);
                focused_text = update
                    .nodes
                    .iter()
                    .any(|(id, node)| *id == update.focus && node.role() == egui::accesskit::Role::TextInput);
                if focused_text {
                    break;
                }
            }
            assert!(focused_text, "Tab reaches a text field");
            for typed in ["-", "1", "4", ".", "5"] {
                frame(&ctx, size, vec![egui::Event::Text(typed.into())], &mut draw);
                frame(&ctx, size, vec![], &mut draw);
            }
        }
        assert_eq!(
            panel.buffers.get("target_lufs").map(String::as_str),
            Some("-14.5"),
            "the field shows what was typed"
        );
        assert!(panel.invalid.is_empty(), "{:?}", panel.invalid);
        let state = panel.state_json();
        let field = state["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|f| f["key"] == "target_lufs")
            .cloned()
            .expect("target_lufs field");
        assert_eq!(field["value"], json!(-14.5), "{field}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Apply and Revert are named by their registered labels.
    #[test]
    fn the_panel_names_every_control() {
        use crate::canvas_keys::test_input::frame;
        let (_world, root) = project("import-settings-a11y");
        let mut panel = ImportSettingsPanel::default();
        panel.open_path(&root, "assets/incoming.png").expect("open");
        panel.edit("max_size", &json!(64)).expect("edit");
        let ctx = egui::Context::default();
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| import_settings_ui(ui, &mut panel, false));
        };
        frame(&ctx, [600.0, 900.0], vec![], &mut draw);
        let update = frame(&ctx, [600.0, 900.0], vec![], &mut draw);
        assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
        let names = crate::a11y_audit::widget_names(&update);
        for label in ["Apply", "Revert"] {
            assert!(
                names.iter().any(|(_, name)| name == label),
                "{label} in {names:?}"
            );
        }
        assert!(panel.dirty(), "drawing does not touch the form");
    }

    // ---- ADR-7664: Apply as an editor job --------------------------------------------------

    /// A counted fake cook gated on a channel the test releases: `entered` gets one message as
    /// each cook starts; the cook then waits for `release`.
    struct GatedCook {
        cooks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        entered: std::sync::mpsc::Receiver<()>,
        release: std::sync::mpsc::Sender<()>,
    }

    fn gated_cook(world: &mut World) -> GatedCook {
        let cooks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        let counted = std::sync::Arc::clone(&cooks);
        world.insert_resource(SettingsCook(std::sync::Arc::new(move |_root: &Path| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            entered_tx.send(()).ok();
            gate.lock().map_err(|_| "gate".to_string())?.recv().ok();
            Ok(regia_asset_pipeline::CookReport {
                cooked: 1,
                ..Default::default()
            })
        })));
        GatedCook {
            cooks,
            entered,
            release,
        }
    }

    impl GatedCook {
        fn count(&self) -> usize {
            self.cooks.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Waits for the running write's worker to return (the editor's poll then applies it).
    fn wait_for_worker(world: &mut World) {
        world.resource_mut::<ImportSettingsApplyJob>().job.wait();
    }

    fn job_state(world: &World, id: &str) -> regia_agent::jobs::JobState {
        world
            .resource::<crate::editor_jobs::EditorJobs>()
            .get(id)
            .map(regia_agent::jobs::JobHandle::state)
            .expect("the job is registered")
    }

    fn open_and_edit(world: &mut World) {
        call(
            world,
            "editor.import_settings_open",
            json!({ "path": "assets/incoming.png" }),
        )
        .expect("open");
        call(
            world,
            "editor.import_settings_edit",
            json!({ "field": "max_size", "value": 64 }),
        )
        .expect("edit");
    }

    /// Item (1): the UI Apply no longer cooks on the UI thread. Clicking Apply (keyboard, through
    /// the real window) returns while the cook is inside the fake and gated; the window keeps
    /// drawing; the job is `import_settings:<n>` with a task row; once the cook is released the
    /// frame's poll ends the job, the panel's form becomes the saved state (one recook) and the
    /// content index is invalidated (the UI path never did that before).
    #[test]
    fn the_ui_apply_runs_as_a_job_and_invalidates_the_content_index() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let (mut world, root) = project("import-settings-ui-job");
        open_and_edit(&mut world);
        let meta_path = root.join("assets/incoming.png.meta");
        let original = std::fs::read(&meta_path).expect("meta");
        let cook = gated_cook(&mut world);
        world.insert_resource(crate::content_index::ContentIndex::default());
        world
            .resource_mut::<crate::content_index::ContentIndex>()
            .ensure(Some(&root));
        assert_eq!(world.resource::<crate::content_index::ContentIndex>().scans, 1);

        let ctx = egui::Context::default();
        let size = [700.0, 900.0];
        tab_to(&ctx, size, "Apply", false, 80, |ui| {
            import_settings_window(ui.ctx(), &mut world)
        });
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            |ui| {
                import_settings_window(ui.ctx(), &mut world);
            },
        );
        // The click returned. The worker reaches the fake cook (a channel, not a clock) and stays
        // there; the UI keeps drawing frames meanwhile.
        cook.entered.recv().expect("the cook started");
        for _ in 0..3 {
            frame(&ctx, size, vec![], |ui| {
                import_settings_window(ui.ctx(), &mut world)
            });
        }
        assert_eq!(cook.count(), 1);
        let id = world
            .resource::<ImportSettingsPanel>()
            .applying
            .clone()
            .expect("the panel names its running Apply");
        assert!(id.starts_with("import_settings:"), "{id}");
        assert_eq!(job_state(&world, &id), regia_agent::jobs::JobState::Running);
        assert!(
            world
                .resource::<crate::tasks::BackgroundTasks>()
                .rows()
                .iter()
                .any(|row| row.id == APPLY_TASK),
            "the job has a task row"
        );
        assert_eq!(
            world.resource::<ImportSettingsPanel>().recooks,
            0,
            "not applied yet"
        );
        assert_ne!(
            std::fs::read(&meta_path).expect("meta"),
            original,
            "the .meta was written"
        );

        cook.release.send(()).expect("release");
        wait_for_worker(&mut world);
        frame(&ctx, size, vec![], |ui| {
            import_settings_window(ui.ctx(), &mut world)
        });
        assert_eq!(job_state(&world, &id), regia_agent::jobs::JobState::Done);
        let panel = world.resource::<ImportSettingsPanel>();
        assert_eq!(panel.applying, None);
        assert_eq!(panel.recooks, 1, "one Apply, one cook");
        assert!(!panel.dirty(), "the applied form is the saved state");
        assert_eq!(panel.error, None);
        assert_eq!(cook.count(), 1);
        assert!(
            world
                .resource::<crate::tasks::BackgroundTasks>()
                .rows()
                .iter()
                .all(|row| row.id != APPLY_TASK),
            "the row goes with the job"
        );
        // The index was invalidated: the next look rescans.
        world
            .resource_mut::<crate::content_index::ContentIndex>()
            .ensure(Some(&root));
        assert_eq!(world.resource::<crate::content_index::ContentIndex>().scans, 2);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Item (1): a second Apply (or `editor.set_import_settings`) while one runs is refused with
    /// `import_settings_apply_running`, not queued; nothing else is written or cooked.
    #[test]
    fn a_second_apply_while_one_runs_is_refused() {
        let (mut world, root) = project("import-settings-second-apply");
        open_and_edit(&mut world);
        let cook = gated_cook(&mut world);
        let out = call(
            &mut world,
            "editor.import_settings_apply",
            json!({ "async": true }),
        )
        .expect("first");
        let id = out["job"].as_str().expect("job id").to_string();
        assert_eq!(out["state"]["applying"], json!(id));
        cook.entered.recv().expect("the cook started");
        assert_eq!(
            reason(call(
                &mut world,
                "editor.import_settings_apply",
                json!({ "async": true })
            )),
            APPLY_RUNNING
        );
        assert_eq!(
            reason(call(&mut world, "editor.import_settings_apply", json!({}))),
            APPLY_RUNNING
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.set_import_settings",
                json!({ "path": "assets/incoming.png", "settings": "(compress: Some(false))" })
            )),
            APPLY_RUNNING
        );
        assert_eq!(
            world
                .resource::<ImportSettingsPanel>()
                .error
                .as_ref()
                .map(|(c, _)| c.as_str()),
            Some(APPLY_RUNNING),
            "the panel shows the refusal"
        );
        cook.release.send(()).expect("release");
        wait_for_worker(&mut world);
        let state = call(&mut world, "editor.import_settings_state", json!({})).expect("state");
        assert_eq!(state["applying"], Value::Null, "{state}");
        assert_eq!(state["recooks"], 1);
        assert_eq!(cook.count(), 1, "the refused calls cooked nothing");
        assert_eq!(job_state(&world, &id), regia_agent::jobs::JobState::Done);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Item (1): Cancel leaves the old cooked output. A cancel during the cook (which cannot stop
    /// part-way) puts the old `.meta` back and cooks it again; the job ends `cancelled`, the panel
    /// keeps its form (still applicable) and shows `import_settings_cancelled`.
    #[test]
    fn a_cancel_during_the_cook_puts_the_old_settings_back() {
        let (mut world, root) = project("import-settings-cancel-cook");
        open_and_edit(&mut world);
        let meta_path = root.join("assets/incoming.png.meta");
        let original = std::fs::read(&meta_path).expect("meta");
        let cook = gated_cook(&mut world);
        let out = call(
            &mut world,
            "editor.import_settings_apply",
            json!({ "async": true }),
        )
        .expect("apply");
        let id = out["job"].as_str().expect("job id").to_string();
        cook.entered.recv().expect("the cook started");
        let answer = crate::editor_jobs::cancel_job(&mut world, &json!({ "id": id })).expect("cancel");
        assert_eq!(answer["requested"], true);
        // The new settings' cook finishes; the old settings' cook follows.
        cook.release.send(()).expect("release the first cook");
        cook.entered.recv().expect("the old settings cook again");
        cook.release.send(()).expect("release the second cook");
        wait_for_worker(&mut world);
        crate::editor_jobs::tick(&mut world);
        assert_eq!(job_state(&world, &id), regia_agent::jobs::JobState::Cancelled);
        assert_eq!(
            cook.count(),
            2,
            "the new settings' cook, then the old settings' cook"
        );
        assert_eq!(
            std::fs::read(&meta_path).expect("meta"),
            original,
            "the old .meta is back"
        );
        let panel = world.resource::<ImportSettingsPanel>();
        assert_eq!(panel.recooks, 0, "nothing was applied");
        assert!(panel.can_apply(), "the form is kept for another Apply");
        assert_eq!(
            panel.error.as_ref().map(|(c, _)| c.as_str()),
            Some("import_settings_cancelled")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Item (1): a cancel before the write (the worker waits for a cook lock another holder has,
    /// off the UI thread) writes and cooks nothing; the Cancel of the task row reaches it.
    #[test]
    fn a_cancel_while_waiting_for_the_cook_lock_writes_nothing() {
        let (mut world, root) = project("import-settings-cancel-lock");
        open_and_edit(&mut world);
        let meta_path = root.join("assets/incoming.png.meta");
        let original = std::fs::read(&meta_path).expect("meta");
        let cook = gated_cook(&mut world);
        let state = regia_core::brand::project_state_dir(&root);
        std::fs::create_dir_all(&state).expect("state dir");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(state.join("cook.lock"))
            .expect("lock file");
        lock.lock().expect("hold the cook lock");
        // Without async the method refuses at once instead of waiting on the lock.
        assert_eq!(
            reason(call(&mut world, "editor.import_settings_apply", json!({}))),
            "tools_busy"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.set_import_settings",
                json!({ "path": "assets/incoming.png", "settings": "(max_size: Some(64))" })
            )),
            "tools_busy"
        );
        let id = start_panel_apply(&mut world).expect("the job starts and waits for the lock");
        assert!(crate::tasks::cancel(&world, APPLY_TASK), "the task row's Cancel");
        wait_for_worker(&mut world);
        crate::editor_jobs::tick(&mut world);
        lock.unlock().expect("unlock");
        assert_eq!(job_state(&world, &id), regia_agent::jobs::JobState::Cancelled);
        assert_eq!(cook.count(), 0, "nothing cooked");
        assert_eq!(
            std::fs::read(&meta_path).expect("meta"),
            original,
            "nothing written"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Item (1): the agent methods name their job like `editor.import { async: true }`: `async`
    /// answers `{ job }` at once; the blocking form waits and adds `job` to its result. Both
    /// invalidate the content index on success (the agent path did before; the job keeps it).
    #[test]
    fn blocking_methods_name_their_job() {
        let (mut world, root) = project("import-settings-job-ids");
        world.insert_resource(crate::content_index::ContentIndex::default());
        world
            .resource_mut::<crate::content_index::ContentIndex>()
            .ensure(Some(&root));
        let out = call(
            &mut world,
            "editor.set_import_settings",
            json!({ "path": "assets/incoming.png", "settings": "(max_size: Some(64))" }),
        )
        .expect("set_import_settings");
        assert!(
            out["job"]
                .as_str()
                .is_some_and(|job| job.starts_with("import_settings:")),
            "{out}"
        );
        assert!(out["cooked"].as_u64().is_some_and(|c| c >= 1), "{out}");
        world
            .resource_mut::<crate::content_index::ContentIndex>()
            .ensure(Some(&root));
        assert_eq!(world.resource::<crate::content_index::ContentIndex>().scans, 2);

        call(
            &mut world,
            "editor.import_settings_open",
            json!({ "path": "assets/incoming.png" }),
        )
        .expect("open");
        call(
            &mut world,
            "editor.import_settings_edit",
            json!({ "field": "compress", "value": false }),
        )
        .expect("edit");
        let out = call(&mut world, "editor.import_settings_apply", json!({})).expect("apply");
        assert!(
            out["job"]
                .as_str()
                .is_some_and(|job| job.starts_with("import_settings:")),
            "{out}"
        );
        assert_eq!(out["state"]["recooks"], 1, "{out}");
        world
            .resource_mut::<crate::content_index::ContentIndex>()
            .ensure(Some(&root));
        assert_eq!(world.resource::<crate::content_index::ContentIndex>().scans, 3);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Item (1): `editor.set_import_settings { async: true }` answers `{ job }` at once while the
    /// cook is gated, and the job ends `done` with the method's result.
    #[test]
    fn set_import_settings_async_answers_its_job_at_once() {
        let (mut world, root) = project("import-settings-method-async");
        let cook = gated_cook(&mut world);
        let out = call(
            &mut world,
            "editor.set_import_settings",
            json!({ "path": "assets/incoming.png", "settings": "(max_size: Some(32))", "async": true }),
        )
        .expect("async");
        let id = out["job"].as_str().expect("job id").to_string();
        cook.entered.recv().expect("the cook started");
        assert_eq!(job_state(&world, &id), regia_agent::jobs::JobState::Running);
        cook.release.send(()).expect("release");
        wait_for_worker(&mut world);
        crate::editor_jobs::tick(&mut world);
        assert_eq!(job_state(&world, &id), regia_agent::jobs::JobState::Done);
        let meta = std::fs::read_to_string(root.join("assets/incoming.png.meta")).expect("meta");
        assert!(meta.contains("max_size: Some(32)"), "{meta}");
        std::fs::remove_dir_all(&root).ok();
    }
}
