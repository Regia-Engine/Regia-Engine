// SPDX-License-Identifier: MPL-2.0
//! Project-wide import settings rules: `config/import_rules.ron` v1 at the cook, the effective
//! settings of one source, and the `import_rule_unused` / `import_rule_unknown_field` checks
//! (`docs/specs/12-assets.md` § 10, ADR-3321).
//!
//! The format, its parser and the settings merge live in `regia_asset::import_rules` (shared
//! with the agent protocol, which must not depend on this desktop-only crate). This module adds
//! what only the pipeline knows: which importers take which settings fields, the source
//! extension → importer name mapping, and the cook-time resolution every settings-reading import
//! path calls ([`resolve_for_cook`]).
//!
//! Precedence, low to high: the importer's defaults, the file-name inference
//! ([`crate::texture_settings::usage_from_file_name`] for a standalone texture's usage), the
//! matching rules in file order (later rules win field by field), then the explicit fields of the
//! asset's own `.meta`. Rules are resolved at cook time: the `.meta` keeps only explicit
//! overrides, and the matched rules are hashed into the source's cook key
//! (`cook_inputs` hashes them into the cook key) so editing a rule recooks exactly the assets it
//! matches.

use std::borrow::Cow;
use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};

pub use regia_asset::import_rules::{
    IMPORT_RULES_FILE, IMPORT_RULES_FORMAT_VERSION, ImportRule, ImportRulesFile, ImportRulesFileError,
    SettingsObject, import_rules_schema_fingerprint,
};

pub use regia_asset::import_rules::import_rules_migration_sources;

thread_local! {
    /// The last `config/import_rules.ron` this thread parsed. `resolve_for_cook` and
    /// `referenced_settings` run once per source (and once per referenced image), so a cook
    /// used to read and parse the file about twice per source; the `(len, mtime)` stat stamp
    /// skips the read and the parse while the file is untouched and its stamp trusted, see
    /// [`RulesStamp`] (rev-corr-assets, sprint-corr-asset-pipeline-a 3; rev-kimi-5 1).
    static RULES_FILE_CACHE: RefCell<Option<CachedRulesFile>> = const { RefCell::new(None) };
}

/// One entry of [`RULES_FILE_CACHE`]: the file's path, the stamp of the read and the parse.
/// Only trusted stamps are cached.
struct CachedRulesFile {
    path: PathBuf,
    stamp: RulesStamp,
    file: Option<ImportRulesFile>,
}

/// What `config/import_rules.ron` looked like when it was read: the `(len, mtime)` stat taken
/// right before the read, and whether a later identical stat proves the file still reads the
/// same. A stamp is trusted with the same acceptance as the cook's file stamps
/// (`cook_inputs::stamp_trusted`): the mtime is known and at least
/// [`crate::cook_inputs::RACY_MARGIN_NS`] older than the read, so a same-length edit within one
/// mtime tick of the previous write (coarse FAT/exFAT/HFS+/network mtimes, or a tool writing
/// right after the read) cannot keep the stat unmoved (rev-kimi-5 finding 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RulesStamp {
    /// `(len, mtime ns)`, `None` when the file was absent; the mtime is `0` when the file system
    /// did not give one (never trusted, so the file is read again; rev-kimi-5 finding 2).
    stat: Option<(u64, u64)>,
    /// Whether an identical later stat stands for an unchanged file.
    trusted: bool,
}

impl RulesStamp {
    /// Whether `project_root`'s rules file still reads as it did when this stamp was taken,
    /// answered from one stat: `false` means "not proved", not "changed" (compare the content).
    pub(crate) fn still_matches(&self, project_root: &Path) -> bool {
        self.trusted && rules_stat(project_root) == self.stat
    }
}

/// The `(len, mtime)` stat of `project_root`'s `config/import_rules.ron`: `None` when the file
/// is absent, an mtime of `0` when the file system gives none.
fn rules_stat(project_root: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(project_root.join(IMPORT_RULES_FILE)).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .unwrap_or(0);
    Some((meta.len(), mtime))
}

/// Whether a read of the rules file that saw `stat` before it and produced `read`, started at
/// `read_at_ns`, may be reused while the stat stays the same. An absent file is trusted when
/// the read also found none. A present one needs a known mtime at least `RACY_MARGIN_NS`
/// before the read; a stat that disagrees with the read (the file appeared or vanished in
/// between) is never trusted.
fn stamp_trusted(
    stat: Option<(u64, u64)>,
    read: &Result<Option<ImportRulesFile>, ImportRulesFileError>,
    read_at_ns: u64,
) -> bool {
    match (stat, read) {
        (None, Ok(None)) => true,
        (Some((_, mtime_ns)), Ok(Some(_)) | Err(_)) => {
            mtime_ns != 0 && mtime_ns.saturating_add(crate::cook_inputs::RACY_MARGIN_NS) < read_at_ns
        }
        _ => false,
    }
}

/// [`ImportRulesFile::for_project`] behind a per-thread, single-entry stat-stamp cache, with
/// the stamp of the read it returns: cooks and per-source resolves reuse the parse while
/// `config/import_rules.ron` is untouched. A moved stat or a racily fresh file (see
/// [`RulesStamp`]) is read again, so the cache never hides a rules change. `CookInputs::open`
/// keeps the returned stamp with the returned parse, so the two describe one read.
pub(crate) fn for_project_stamped(
    project_root: &Path,
) -> (Result<Option<ImportRulesFile>, ImportRulesFileError>, RulesStamp) {
    let path = project_root.join(IMPORT_RULES_FILE);
    // Taken before the stat: an edit after it lands at or after this time.
    let read_at_ns = crate::cook_inputs::now_ns();
    let stat = rules_stat(project_root);
    let hit = RULES_FILE_CACHE.with(|c| match c.borrow().as_ref() {
        Some(hit) if hit.path == path && hit.stamp.trusted && hit.stamp.stat == stat => {
            Some((hit.file.clone(), hit.stamp))
        }
        _ => None,
    });
    if let Some((file, stamp)) = hit {
        return (Ok(file), stamp);
    }
    let read = ImportRulesFile::for_project(project_root);
    #[cfg(test)]
    test_seam::after_read();
    let stamp = RulesStamp {
        stat,
        trusted: stamp_trusted(stat, &read, read_at_ns),
    };
    RULES_FILE_CACHE.with(|c| {
        *c.borrow_mut() = match &read {
            Ok(file) if stamp.trusted => Some(CachedRulesFile {
                path,
                stamp,
                file: file.clone(),
            }),
            _ => None,
        };
    });
    (read, stamp)
}

/// [`for_project_stamped`] without the stamp.
pub(crate) fn for_project_cached(
    project_root: &Path,
) -> Result<Option<ImportRulesFile>, ImportRulesFileError> {
    for_project_stamped(project_root).0
}

/// Test seam of the rules cache (fixrev5-ldtk-rules-stamp): counts this thread's reads of the
/// rules file and runs a one-shot hook right after a read, so a test can edit the file between
/// the read and whatever the caller does next.
#[cfg(test)]
pub(crate) mod test_seam {
    use std::cell::{Cell, RefCell};

    thread_local! {
        static READS: Cell<u64> = const { Cell::new(0) };
        static AFTER_READ: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }

    /// Reads of the rules file by this thread so far.
    pub(crate) fn reads() -> u64 {
        READS.with(Cell::get)
    }

    /// Runs `hook` once, right after this thread's next read of the rules file.
    pub(crate) fn on_next_read(hook: impl FnOnce() + 'static) {
        AFTER_READ.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn after_read() {
        READS.with(|r| r.set(r.get() + 1));
        if let Some(hook) = AFTER_READ.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }
}

/// Stable finding code of `regia assets import-rules check`: a rule matches no source in the
/// project.
pub const CODE_RULE_UNUSED: &str = "import_rule_unused";

/// Stable finding code of `regia assets import-rules check`: a rule sets a field the matched
/// asset's importer does not know.
pub const CODE_UNKNOWN_FIELD: &str = "import_rule_unknown_field";

/// The importer name a source extension cooks under (the importer recorded in its `.meta`): the
/// audio importer for sounds, `gltf` for `.glb`, otherwise the extension itself (`png`, `fbx`,
/// `bvh`, ...).
#[must_use]
pub fn importer_for_ext(ext: &str) -> &str {
    if crate::audio_cook::is_audio_extension(ext) {
        "audio"
    } else if matches!(ext, "glb" | "gltf") {
        "gltf"
    } else {
        ext
    }
}

/// The settings fields an importer's `.meta` `settings` object knows, or `None` when the
/// importer takes no import-rule settings (every field of a matching rule is then reported with
/// [`CODE_UNKNOWN_FIELD`]).
#[must_use]
pub fn known_fields(importer: &str) -> Option<&'static [&'static str]> {
    const TEXTURE: &[&str] = &[
        "usage",
        "max_size",
        "compress",
        "quality",
        "stream",
        "stream_min_px",
        "hdr_encoding",
        "cube_face_px",
        "normal_green",
        "orm_channel_order",
        "alpha_cutoff",
        "heightfield",
    ];
    const GLTF: &[&str] = &[
        "generate_lods",
        "meshlets",
        "drop_double_sided_on_closed",
        "vertex_ao",
        "skin_influences",
        "fracture",
        "collision",
        "collision_hulls",
        "voxel_far",
        "forward",
        "scale",
        "lossless_clips",
        "clips",
        // Cooked foot contact curves (ADR-8308).
        "contact_curves",
        // Per-mesh skip and per-material binding (ADR-8376).
        "meshes",
        "materials",
    ];
    const FBX: &[&str] = &[
        "scale",
        "import_animation",
        "sample_rate",
        "uv_set",
        "limits",
        "lossless_clips",
    ];
    const OBJ: &[&str] = &["scale", "limits"];
    const BVH: &[&str] = &[
        "unit",
        "unit_scale",
        "up_axis",
        "keep_forward",
        "root_motion",
        "rotation_tolerance_degrees",
        "translation_tolerance",
        "end_sites",
        "clip_name",
        "retarget",
        "limits",
        "lossless_clips",
    ];
    const AUDIO: &[&str] = &[
        "normalize",
        "target_lufs",
        "mode",
        "quality",
        "android_quality",
        "force",
        "impulse_response",
    ];
    match importer {
        "png" | "jpg" | "jpeg" | "tga" | "bmp" | "dds" | "exr" | "hdr" => Some(TEXTURE),
        "gltf" => Some(GLTF),
        "fbx" => Some(FBX),
        "obj" => Some(OBJ),
        "bvh" => Some(BVH),
        "audio" => Some(AUDIO),
        _ => None,
    }
}

/// Whether `field` is a glTF importer key that `importer` honours because its sources cook
/// through the glTF importer (FBX and OBJ are converted to glTF first; their own settings structs
/// list only their own keys).
fn converted_field(importer: &str, field: &str) -> bool {
    matches!(importer, "fbx" | "obj") && known_fields("gltf").is_some_and(|gltf| gltf.contains(&field))
}

/// Where one effective settings field came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldSourceKind {
    /// The asset's own `.meta` `settings` (an explicit override: wins over every rule).
    Meta,
    /// A matching rule; later rules overwrite earlier ones field by field.
    Rule {
        /// The rule's index in `config/import_rules.ron` (0-based).
        index: usize,
        /// The rule's glob (diagnostics).
        glob: String,
    },
}

/// One field of a source's effective settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldSource {
    /// The field name (`compress`, `meshlets`, ...).
    pub field: String,
    /// The field's raw RON value text.
    pub value: String,
    /// Which layer set it.
    pub source: FieldSourceKind,
}

/// The effective settings of one source: the merged RON object and the source of each field.
/// Fields no layer sets are absent (the importer's default applies; a standalone texture's usage
/// additionally falls back to its file name).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectiveSettings {
    /// The merged settings object text (`()` when nothing sets any field).
    pub settings: String,
    /// The fields that a rule or the `.meta` sets, in merged order, each with its source.
    pub fields: Vec<FieldSource>,
    /// The indices of the rules that matched this source, in file order.
    pub matched_rules: Vec<usize>,
}

/// The effective settings of the source at `rel_path` (project-relative, `/`-separated) under
/// `project_root`, for the editor's import-settings panel and `regia assets import-rules check`.
///
/// `explicit` is the source's `.meta` `settings` string; `None` when the meta has none (or there
/// is no meta yet). A missing `config/import_rules.ron` is not an error: the explicit settings
/// pass through. A malformed rules file or settings string is reported as an error string and the
/// other layer still applies.
pub fn effective_settings(
    project_root: &Path,
    rel_path: &str,
    importer: &str,
    explicit: Option<&str>,
) -> Result<EffectiveSettings, String> {
    let mut out = EffectiveSettings::default();
    let file = match for_project_cached(project_root) {
        Ok(file) => file,
        Err(e) => return Err(e.to_string()),
    };
    let Some(file) = file else {
        out.settings = explicit
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("()")
            .to_string();
        out.fields = explicit_fields(explicit);
        return Ok(out);
    };
    let matched = file.matched_rules(rel_path, importer);
    out.matched_rules = matched.iter().map(|(index, _)| *index).collect();
    let merged = SettingsObject::parse(
        &regia_asset::import_rules::resolve_settings(explicit, &matched).unwrap_or_else(|| "()".into()),
    )
    .map_err(|e| format!("{rel_path}: merged settings do not parse: {e}"))?;
    out.settings = merged.to_ron();
    out.fields = merged
        .fields
        .iter()
        .map(|(name, value)| FieldSource {
            field: name.clone(),
            value: value.clone(),
            source: field_source(name, explicit, &matched),
        })
        .collect();
    Ok(out)
}

/// The provenance of one merged field: `Meta` when the explicit `.meta` settings set it (explicit
/// fields win over every rule), else the last matching rule that sets it (later rules win). Only
/// fields a layer sets reach the merged object.
fn field_source(name: &str, explicit: Option<&str>, matched: &[(usize, &ImportRule)]) -> FieldSourceKind {
    if let Some(obj) = explicit
        .filter(|s| !s.trim().is_empty())
        .and_then(|text| SettingsObject::parse(text).ok())
        && obj.get(name).is_some()
    {
        return FieldSourceKind::Meta;
    }
    for (index, rule) in matched.iter().rev() {
        if SettingsObject::parse(&rule.settings)
            .ok()
            .is_some_and(|obj| obj.get(name).is_some())
        {
            return FieldSourceKind::Rule {
                index: *index,
                glob: rule.glob.clone(),
            };
        }
    }
    FieldSourceKind::Meta
}

/// The fields an explicit settings string sets (all `Meta`).
fn explicit_fields(explicit: Option<&str>) -> Vec<FieldSource> {
    explicit
        .filter(|s| !s.trim().is_empty())
        .and_then(|text| SettingsObject::parse(text).ok())
        .map(|obj| {
            obj.fields
                .into_iter()
                .map(|(field, value)| FieldSource {
                    field,
                    value,
                    source: FieldSourceKind::Meta,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The settings string a cook path parses: `config/import_rules.ron` merged under the explicit
/// `.meta` fields. Borrows `explicit` untouched when no rule matches, so a project without rules
/// (the common case) costs one file existence check per source.
#[must_use]
pub fn resolve_for_cook<'a>(
    project_root: &Path,
    rel_path: &str,
    importer: &str,
    explicit: Option<&'a str>,
) -> Cow<'a, str> {
    let Ok(Some(file)) = for_project_cached(project_root) else {
        return Cow::Borrowed(explicit.unwrap_or_default());
    };
    let matched = file.matched_rules(rel_path, importer);
    if matched.is_empty() {
        return Cow::Borrowed(explicit.unwrap_or_default());
    }
    match regia_asset::import_rules::resolve_settings(explicit, &matched) {
        Some(merged) => Cow::Owned(merged),
        None => Cow::Borrowed(explicit.unwrap_or_default()),
    }
}

thread_local! {
    /// The project of the cook running on this thread ([`with_cook_project`]).
    static COOK_PROJECT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Runs `f` with `project_root` as the project of the cook on this thread, so the files an import
/// reads besides its source (a glTF's images) get the project's rules too
/// ([`referenced_settings`]). Restores the previous project afterwards, also on a panic.
pub(crate) fn with_cook_project<R>(project_root: &Path, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            COOK_PROJECT.with(|p| *p.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(COOK_PROJECT.with(|p| p.borrow_mut().replace(project_root.to_path_buf())));
    f()
}

/// `path` relative to `root`, `/`-separated, with `.` and `..` folded lexically; `None` when it
/// leaves `root`.
pub(crate) fn project_rel(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts: Vec<String> = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

/// The settings string of a file an import references (a glTF's image at `path`): its `.meta`
/// settings merged over the project's matching import rules, as [`resolve_for_cook`] gives a
/// standalone source. `None` outside a cook ([`with_cook_project`]), for a file outside the
/// project, or when no rule matches: the caller then reads the `.meta` alone, as before.
pub(crate) fn referenced_settings(path: &Path) -> Option<String> {
    let root = COOK_PROJECT.with(|p| p.borrow().clone())?;
    let rel = project_rel(&root, path)?;
    let file = for_project_cached(&root).ok().flatten()?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let importer = importer_for_ext(&ext);
    if file.matched_rules(&rel, importer).is_empty() {
        return None;
    }
    let explicit = std::fs::read(regia_asset::meta_path_for(path))
        .ok()
        .and_then(|bytes| regia_asset::MetaSidecar::from_ron(&bytes).ok())
        .and_then(|meta| meta.settings);
    Some(resolve_for_cook(&root, &rel, importer, explicit.as_deref()).into_owned())
}

/// The canonical key text of one matched rule: the cook key hashes these, so editing a rule (or
/// adding/removing one that matches) changes the key of exactly the sources it matches.
#[must_use]
pub fn rule_key_text(rule: &ImportRule) -> String {
    format!(
        "{}|{}|{}",
        rule.glob,
        rule.importer.as_deref().unwrap_or("*"),
        rule.settings
    )
}

/// One finding of [`check_project`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleFinding {
    /// [`CODE_RULE_UNUSED`] or [`CODE_UNKNOWN_FIELD`].
    pub code: &'static str,
    /// The rule's index in `config/import_rules.ron` (0-based).
    pub rule_index: usize,
    /// The rule's glob.
    pub glob: String,
    /// The source path (`""` for an unused rule).
    pub path: String,
    /// The field the importer does not know (`""` for an unused rule).
    pub field: String,
}

/// One source's row of [`check_project`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetReport {
    /// Project-relative source path.
    pub path: String,
    /// The importer the cook records for the source.
    pub importer: String,
    /// The effective settings this source cooks with.
    pub effective: EffectiveSettings,
    /// The merged settings do not parse as this importer's settings struct (the cook would log a
    /// warning and cook with the defaults for the fields that do not parse); the parse error.
    pub value_problem: Option<String>,
}

/// Parse `settings` as `importer`'s settings struct, exactly as the cook's `from_meta_string`
/// does; `Some(error)` when the value shape is wrong (an unknown *field* is a finding instead,
/// [`CODE_UNKNOWN_FIELD`]).
fn value_problem(importer: &str, settings: &str) -> Option<String> {
    if settings.trim().is_empty() || settings.trim() == "()" {
        return None;
    }
    let parsed: Result<(), String> = match importer {
        "png" | "jpg" | "jpeg" | "tga" | "bmp" | "dds" | "exr" | "hdr" => {
            ron::from_str::<crate::texture_settings::TextureImporterSettings>(settings)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        "gltf" => crate::gltf_settings::GltfImporterSettings::try_from_meta_string(Some(settings))
            .map(|_| ())
            .map_err(|e| e.to_string()),
        "fbx" => crate::fbx_import::FbxImportSettings::try_from_meta_string(Some(settings))
            .map(|_| ())
            .map_err(|e| e.to_string()),
        "obj" => crate::obj_import::ObjImportSettings::try_from_meta_string(Some(settings))
            .map(|_| ())
            .map_err(|e| e.to_string()),
        "bvh" => crate::bvh_import::BvhImportSettings::try_from_meta_string(Some(settings))
            .map(|_| ())
            .map_err(|e| e.to_string()),
        "audio" => match crate::audio_cook::AudioImporterSettings::from_meta_string(Some(settings)) {
            (_, Some(problem)) => Err(problem),
            (_, None) => Ok(()),
        },
        _ => return None,
    };
    parsed.err()
}

/// The report of `check_project`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// The rules in the file (0 when there is none).
    pub rules: usize,
    /// The findings: unused rules, then per-source unknown fields.
    pub findings: Vec<RuleFinding>,
    /// The sources with at least one matched rule or explicit settings.
    pub assets: Vec<AssetReport>,
    /// The configuration could not be read (the cook would report `invalid_config`).
    pub config_error: Option<String>,
}

/// What `regia assets import-rules check` reports: walk the project's sources, resolve each
/// one's effective settings, and collect the unused-rule and unknown-field findings.
///
/// `path_filter` (a [`regia_asset::import_rules::glob_match`] glob) limits the reported assets;
/// findings and rule usage still count the whole project. A missing `config/import_rules.ron`
/// gives an empty report (no rules is not an error); a malformed one sets
/// [`CheckReport::config_error`].
#[must_use]
pub fn check_project(project_root: &Path, path_filter: Option<&str>) -> CheckReport {
    let mut report = CheckReport::default();
    let file = match for_project_cached(project_root) {
        Ok(file) => file,
        Err(e) => {
            report.config_error = Some(e.to_string());
            return report;
        }
    };
    let Some(file) = file else {
        return report;
    };
    report.rules = file.rules.len();
    let assets_root = project_root.join("assets");
    let sources = crate::db::AssetDatabase::discover_sources(&assets_root);
    let mut matched_any = vec![false; file.rules.len()];
    let filter = path_filter.map(str::to_ascii_lowercase);
    for source in sources {
        let Ok(rel) = source.strip_prefix(project_root) else {
            continue;
        };
        let rel_path = rel.to_string_lossy().replace('\\', "/");
        let ext = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let meta = std::fs::read(regia_asset::meta_path_for(&source))
            .ok()
            .and_then(|bytes| regia_asset::MetaSidecar::from_ron(&bytes).ok());
        let (importer, explicit) = match &meta {
            Some(meta) => (meta.importer.as_str(), meta.settings.as_deref()),
            None => (importer_for_ext(&ext), None),
        };
        let matched = file.matched_rules(&rel_path, importer);
        for (index, _) in &matched {
            matched_any[*index] = true;
        }
        for (index, rule) in &matched {
            let known = known_fields(importer);
            if let Ok(settings) = SettingsObject::parse(&rule.settings) {
                for (field, _) in &settings.fields {
                    // Unknown when the importer is not rule-aware at all, or does not know the
                    // field. (A bad field *value* is not checked here: the importer reports it
                    // at cook time like a malformed .meta settings string.)
                    if known.is_none_or(|fields| !fields.contains(&field.as_str()))
                        && !converted_field(importer, field)
                    {
                        report.findings.push(RuleFinding {
                            code: CODE_UNKNOWN_FIELD,
                            rule_index: *index,
                            glob: rule.glob.clone(),
                            path: rel_path.clone(),
                            field: field.clone(),
                        });
                    }
                }
            }
        }
        let has_explicit = explicit.is_some_and(|s| !s.trim().is_empty());
        if matched.is_empty() && !has_explicit {
            continue;
        }
        if filter
            .as_deref()
            .is_some_and(|f| !regia_asset::import_rules::glob_match(f, &rel_path.to_ascii_lowercase()))
        {
            continue;
        }
        let (effective, problem) = match effective_settings(project_root, &rel_path, importer, explicit) {
            Ok(effective) => {
                let problem = value_problem(importer, &effective.settings);
                (effective, problem)
            }
            // The merge failed (a mid-check edit, or a rule whose settings string does not
            // parse): surface that error instead of RON-parsing the error text as settings.
            Err(err) => (
                EffectiveSettings {
                    settings: "()".to_string(),
                    ..EffectiveSettings::default()
                },
                Some(format!("the effective settings could not be resolved: {err}")),
            ),
        };
        report.assets.push(AssetReport {
            path: rel_path,
            importer: importer.to_string(),
            effective,
            value_problem: problem,
        });
    }
    for (index, any) in matched_any.iter().enumerate() {
        if !any {
            report.findings.push(RuleFinding {
                code: CODE_RULE_UNUSED,
                rule_index: index,
                glob: file.rules[index].glob.clone(),
                path: String::new(),
                field: String::new(),
            });
        }
    }
    report
        .findings
        .sort_by_key(|f| (f.rule_index, f.code, f.path.clone()));
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(rules: Option<&str>, files: &[(&str, Option<&str>)]) -> std::path::PathBuf {
        let root = regia_test_utils::unique_temp_dir("import-rules-pipe");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        if let Some(rules) = rules {
            std::fs::create_dir_all(root.join("config")).expect("config");
            std::fs::write(root.join(IMPORT_RULES_FILE), rules).expect("rules");
        }
        for (path, settings) in files {
            let file = root.join(path);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).expect("parent dir");
            }
            std::fs::write(&file, b"x").expect("source");
            if let Some(settings) = settings {
                let id = regia_asset::AssetId::new_random();
                let mut meta = regia_asset::MetaSidecar::new(
                    importer_for_ext(path.rsplit('.').next().unwrap_or("")),
                    id,
                );
                meta.settings = Some((*settings).to_string());
                std::fs::write(
                    regia_asset::meta_path_for(&file),
                    meta.to_ron_string().expect("meta"),
                )
                .expect("meta write");
            }
        }
        root
    }

    #[test]
    fn the_known_field_tables_match_the_importer_settings_structs() {
        // The table is what `check` judges rules against; the serde reflection of each settings
        // struct is the truth. They must not drift.
        use regia_core::schema::serde_field_names;
        let joined = |fields: Vec<String>| fields.join(",");
        let table = |importer: &str| known_fields(importer).expect("fields").join(",");
        assert_eq!(
            joined(serde_field_names::<
                crate::texture_settings::TextureImporterSettings,
            >()),
            table("png"),
        );
        assert_eq!(
            joined(serde_field_names::<crate::gltf_settings::GltfImporterSettings>()),
            table("gltf"),
        );
        assert_eq!(
            joined(serde_field_names::<crate::audio_cook::AudioImporterSettings>()),
            table("audio"),
        );
        assert_eq!(
            joined(serde_field_names::<crate::fbx_import::FbxImportSettings>()),
            table("fbx"),
        );
        assert_eq!(
            joined(serde_field_names::<crate::obj_import::ObjImportSettings>()),
            table("obj"),
        );
        assert_eq!(
            joined(serde_field_names::<crate::bvh_import::BvhImportSettings>()),
            table("bvh"),
        );
        assert!(known_fields("material_ron").is_none());
        assert_eq!(importer_for_ext("wav"), "audio");
        assert_eq!(importer_for_ext("glb"), "gltf");
        assert_eq!(importer_for_ext("fbx"), "fbx");
    }

    #[test]
    fn effective_settings_merge_rules_under_meta_with_provenance() {
        let root = project(
            Some(
                r#"(rules: [
                (glob: "assets/ui/**", importer: "png", settings: "(compress: Some(false), max_size: Some(256))"),
                (glob: "assets/**", importer: "png", settings: "(quality: Some(Low))"),
            ])"#,
            ),
            &[("assets/ui/icon.png", Some("(max_size: Some(1024))"))],
        );
        let eff = effective_settings(&root, "assets/ui/icon.png", "png", Some("(max_size: Some(1024))"))
            .expect("resolve");
        assert_eq!(eff.matched_rules, vec![0, 1]);
        let field = |name: &str| eff.fields.iter().find(|f| f.field == name).expect("field");
        assert_eq!(field("compress").value, "Some(false)");
        assert!(matches!(
            field("compress").source,
            FieldSourceKind::Rule { index: 0, .. }
        ));
        assert_eq!(field("quality").value, "Some(Low)");
        assert!(matches!(
            field("quality").source,
            FieldSourceKind::Rule { index: 1, .. }
        ));
        // The explicit .meta field beats both rules' values for it.
        assert_eq!(field("max_size").value, "Some(1024)");
        assert_eq!(field("max_size").source, FieldSourceKind::Meta);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn effective_settings_without_rules_passthrough_meta() {
        let root = project(None, &[]);
        let eff =
            effective_settings(&root, "assets/a.png", "png", Some("(compress: false)")).expect("resolve");
        assert!(eff.matched_rules.is_empty());
        assert_eq!(eff.fields.len(), 1);
        assert_eq!(eff.fields[0].source, FieldSourceKind::Meta);
        let eff = effective_settings(&root, "assets/a.png", "png", None).expect("resolve");
        assert_eq!(eff.settings, "()");
        assert!(eff.fields.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Corr-low (pipeline-a #23): an FBX or OBJ source cooks through the glTF importer, which
    /// honours the glTF keys (`meshlets`, `voxel_far`, …; see `FbxImportSettings`), so a rule
    /// setting one for those sources is not an unknown field. A key neither importer knows still is.
    #[test]
    fn corr_low_gltf_keys_are_known_to_converted_fbx_and_obj_sources() {
        let root = project(
            Some(
                r#"(rules: [
                (glob: "assets/**", importer: "fbx", settings: "(meshlets: true, scale: 0.01)"),
                (glob: "assets/**", importer: "obj", settings: "(generate_lods: Some((count: 3)))"),
                (glob: "assets/**", importer: "fbx", settings: "(target_lufs: Some(-23.0))"),
            ])"#,
            ),
            &[("assets/props/tree.fbx", None), ("assets/props/rock.obj", None)],
        );
        let report = check_project(&root, None);
        let codes = |i: usize| {
            report
                .findings
                .iter()
                .filter(|f| f.rule_index == i)
                .map(|f| f.code)
                .collect::<Vec<_>>()
        };
        assert!(
            codes(0).is_empty(),
            "meshlets reaches a converted FBX: {:?}",
            report.findings
        );
        assert!(
            codes(1).is_empty(),
            "generate_lods reaches a converted OBJ: {:?}",
            report.findings
        );
        assert_eq!(
            codes(2),
            vec![CODE_UNKNOWN_FIELD],
            "target_lufs is still unknown to fbx"
        );
    }

    /// Corr-low (pipeline-a #24d): a rule whose `settings` string does not parse used to land its
    /// error TEXT in `effective.settings`, which `value_problem` then RON-parsed as the
    /// importer's settings — a garbage secondary message. The merge error is reported as itself.
    #[test]
    fn corr_low_a_rule_with_unparsable_settings_reports_the_merge_error() {
        // A rules file with an unparsable settings string is refused when it loads
        // (`config_error`), so the per-source merge fails only when the file changes between
        // the check's own read and a source's merge: edit it right after that first read.
        let root = project(
            Some(r#"(rules: [(glob: "assets/**", importer: "png", settings: "(compress: Some(false))")])"#),
            &[("assets/ui/icon.png", None)],
        );
        let rules = root.join(IMPORT_RULES_FILE);
        test_seam::on_next_read(move || {
            std::fs::write(
                &rules,
                r#"(rules: [(glob: "assets/**", importer: "png", settings: "(compress: ")])"#,
            )
            .expect("edit rules");
        });
        let report = check_project(&root, None);
        assert!(report.config_error.is_none(), "{:?}", report.config_error);
        let asset = report
            .assets
            .iter()
            .find(|a| a.path == "assets/ui/icon.png")
            .expect("the matched source is reported");
        let problem = asset.value_problem.as_ref().expect("a value problem");
        assert!(
            problem.contains("could not be resolved"),
            "the merge error is surfaced, not RON-parsed: {problem}"
        );
        assert_eq!(asset.effective.settings, "()");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn check_reports_unused_rules_and_unknown_fields() {
        let root = project(
            Some(
                r#"(rules: [
                (glob: "assets/ui/**", importer: "png", settings: "(compress: Some(false))"),
                (glob: "assets/nothing/**", settings: "(quality: Some(Low))"),
                (glob: "assets/**", importer: "gltf", settings: "(meshlets: tru)"),
                (glob: "assets/**", importer: "png", settings: "(meshlets: true)"),
                (glob: "assets/**", importer: "material_ron", settings: "(anything: 1)"),
                (glob: "assets/**", settings: "(target_lufs: Some(-23.0))"),
            ])"#,
            ),
            &[("assets/ui/icon.png", None), ("assets/props/crate.gltf", None)],
        );
        let report = check_project(&root, None);
        assert_eq!(report.rules, 6);
        let codes = |i: usize| {
            report
                .findings
                .iter()
                .filter(|f| f.rule_index == i)
                .map(|f| f.code)
                .collect::<Vec<_>>()
        };
        assert!(codes(0).is_empty(), "compress is a real png field");
        assert_eq!(codes(1), vec![CODE_RULE_UNUSED]);
        assert!(
            codes(2).is_empty(),
            "meshlets is a real gltf field: a bad value is not an unknown field"
        );
        assert_eq!(codes(3), vec![CODE_UNKNOWN_FIELD]);
        assert_eq!(
            codes(4),
            vec![CODE_RULE_UNUSED],
            "no material_ron source exists, so the rule matches nothing"
        );
        assert_eq!(
            codes(5),
            vec![CODE_UNKNOWN_FIELD, CODE_UNKNOWN_FIELD],
            "the filterless rule reaches both sources; target_lufs is unknown to both importers"
        );
        // The reported assets carry their effective settings and provenance.
        let ui = report
            .assets
            .iter()
            .find(|a| a.path == "assets/ui/icon.png")
            .expect("ui row");
        assert_eq!(ui.importer, "png");
        assert_eq!(
            ui.effective.settings,
            "(compress: Some(false), meshlets: true, target_lufs: Some(-23.0))"
        );
        assert!(ui.value_problem.is_none());
        // The bad glTF value (meshlets: tru) is a value problem on the crate, not a finding.
        let krate = report
            .assets
            .iter()
            .find(|a| a.path == "assets/props/crate.gltf")
            .expect("crate row");
        let problem = krate.value_problem.as_deref().expect("the bad value is reported");
        assert!(!problem.is_empty(), "{problem}");
        // A path filter limits the reported rows, not the findings.
        let filtered = check_project(&root, Some("assets/props/**"));
        assert!(
            filtered
                .assets
                .iter()
                .all(|a| a.path.starts_with("assets/props/"))
        );
        assert_eq!(filtered.findings, report.findings);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_malformed_rules_file_sets_the_config_error() {
        let root = project(Some("(rules: [(glob: \"\", settings: \"()\")])"), &[]);
        let report = check_project(&root, None);
        assert!(report.config_error.is_some());
        assert!(
            report
                .config_error
                .as_deref()
                .is_some_and(|e| e.contains("rule 0"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_for_cook_borrows_without_rules_and_merges_with() {
        let root = project(
            Some("(rules: [(glob: \"assets/ui/**\", settings: \"(compress: false)\")])"),
            &[],
        );
        let borrowed = resolve_for_cook(&root, "assets/props/a.png", "png", Some("(quality: \"low\")"));
        assert!(
            matches!(borrowed, Cow::Borrowed(_)),
            "no matching rule: untouched"
        );
        assert_eq!(borrowed, "(quality: \"low\")");
        let owned = resolve_for_cook(&root, "assets/ui/a.png", "png", None);
        assert_eq!(owned, "(compress: false)");
        let none_rules = project(None, &[]);
        assert_eq!(resolve_for_cook(&none_rules, "assets/ui/a.png", "png", None), "");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&none_rules);
    }

    /// rev-corr-assets (sprint-corr-asset-pipeline-a 3): the per-thread parse cache must never
    /// hide a rules edit — an edited file (a moved `(len, mtime)` stamp) re-parses.
    #[test]
    fn for_project_cached_reparses_an_edited_rules_file() {
        let root = project(
            Some(r#"(rules: [(glob: "assets/**", settings: "(compress: Some(false))")])"#),
            &[],
        );
        let first = for_project_cached(&root).expect("read").expect("rules");
        assert_eq!(first.rules.len(), 1);
        let again = for_project_cached(&root).expect("read").expect("rules");
        assert_eq!(again.rules, first.rules, "an untouched file reuses the parse");
        std::fs::write(
            root.join(IMPORT_RULES_FILE),
            r#"(rules: [(glob: "assets/**", settings: "(compress: Some(true))"), (glob: "other/**", settings: "()")])"#,
        )
        .expect("edit rules");
        let edited = for_project_cached(&root).expect("read").expect("rules");
        assert_eq!(edited.rules.len(), 2, "the edit is visible, not the cached parse");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// fixrev5-ldtk-rules-stamp (rev-kimi-5 findings 1 and 2): a rules stamp is trusted only
    /// with a known mtime well before the read, and only when the stat agrees with what the read
    /// found; an unavailable mtime (`0`) is never trusted, and absent is not "mtime unknown".
    #[test]
    fn a_rules_stamp_is_trusted_only_for_an_old_known_mtime_matching_the_read() {
        use crate::cook_inputs::RACY_MARGIN_NS;
        let present = Ok(Some(ImportRulesFile::default()));
        let absent: Result<Option<ImportRulesFile>, ImportRulesFileError> = Ok(None);
        let bad = Err(ImportRulesFileError::Parse("bad".into()));
        let read_at = 100 * RACY_MARGIN_NS;
        let old = read_at - 2 * RACY_MARGIN_NS;
        assert!(stamp_trusted(Some((10, old)), &present, read_at));
        assert!(
            stamp_trusted(Some((10, old)), &bad, read_at),
            "a bad file is still a file"
        );
        assert!(
            stamp_trusted(None, &absent, read_at),
            "absent at the stat and the read"
        );
        // Modified within the margin of (or after) the read: racily clean, read again.
        let racy = read_at - RACY_MARGIN_NS;
        assert!(!stamp_trusted(Some((10, racy)), &present, read_at));
        assert!(!stamp_trusted(Some((10, read_at + 1)), &present, read_at));
        // No mtime from the file system: present, but never trusted.
        assert!(!stamp_trusted(Some((10, 0)), &present, read_at));
        // The file appeared or vanished between the stat and the read.
        assert!(!stamp_trusted(None, &present, read_at));
        assert!(!stamp_trusted(Some((10, old)), &absent, read_at));
        // A clock before the epoch (`now_ns` = 0) trusts nothing present.
        assert!(!stamp_trusted(Some((10, old)), &present, 0));
    }

    #[test]
    fn the_fingerprint_and_key_text_are_stable() {
        assert!(import_rules_schema_fingerprint().contains("glob"));
        let rule = ImportRule {
            glob: "assets/**".into(),
            importer: None,
            settings: "(compress: false)".into(),
        };
        assert_eq!(rule_key_text(&rule), "assets/**|*|(compress: false)");
    }
}
