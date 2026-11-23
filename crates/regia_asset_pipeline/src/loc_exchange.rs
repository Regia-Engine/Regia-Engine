// SPDX-License-Identifier: MPL-2.0
//! Translator exchange of a project's localization through gettext PO files or XLIFF 2.0
//! (`docs/specs/36-localization-exchange-dialogue-subtitles.md` § 3, ADR-1668, ADR-2550, ADR-7677):
//! the project-level half of `regia loc export` / `regia loc import`, shared by the CLI and the
//! editor's Localization panel so both write the same bytes.
//!
//! The pure work (PO text, entry layout, stale detection, translation checks) lives in
//! `regia_ui::loc_po` and `regia_ui::loc_xliff`; this module reads the project's `localization.ron`,
//! its `.ftl` files and the gather scan ([`read_config`], [`read_locales`], [`add_dialogue_keys`]),
//! builds the export text ([`export`]) and plans and writes an import ([`plan_import`],
//! [`write_import`]). Printing stays with the callers. Every write is atomic.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regia_framework::dialogue::DialogueLibrary;
use regia_ui::loc_check::{self, ParsedLocale};
use regia_ui::loc_config::{LOCALIZATION_CONFIG_PATH, LocalizationConfig};
use regia_ui::loc_gather::{self, PlannedFile};
use regia_ui::loc_glossary;
use regia_ui::loc_po::{self, ExportFilter, ExportInput, ImportContext, ImportPlan, PoFile};
use regia_ui::loc_xliff;

/// One locale as read from disk: its parsed messages and its `(path, text)` files.
pub type LoadedLocale = (ParsedLocale, Vec<(String, String)>);

/// The localization directory, relative to the project root.
pub const LOCALIZATION_DIR: &str = "assets/localization";

/// `localization.ron`, or the defaults when the project has none.
///
/// # Errors
///
/// The file exists but cannot be read or does not parse.
pub fn read_config(project: &Path) -> Result<LocalizationConfig> {
    let path = project.join(LOCALIZATION_CONFIG_PATH);
    if !path.is_file() {
        return Ok(LocalizationConfig::default());
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    LocalizationConfig::from_ron(&text).with_context(|| format!("in {}", path.display()))
}

/// Every locale directory under `assets/localization` (plus configured locales without one), each
/// with its `.ftl` files (sorted by name, as the runtime combines them).
///
/// # Errors
///
/// A locale directory or file cannot be read.
pub fn read_locales(project: &Path, config: &LocalizationConfig) -> Result<Vec<LoadedLocale>> {
    let dir = project.join(LOCALIZATION_DIR);
    let mut tags: BTreeSet<String> = config.locales.iter().cloned().collect();
    if dir.is_dir() {
        for entry in std::fs::read_dir(&dir).with_context(|| format!("listing {}", dir.display()))? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with('.') && (config.locales.is_empty() || tags.contains(&name)) {
                    tags.insert(name);
                }
            }
        }
    }
    let mut out = Vec::new();
    for tag in tags {
        let locale_dir = dir.join(&tag);
        let mut names = Vec::new();
        if locale_dir.is_dir() {
            for entry in std::fs::read_dir(&locale_dir)? {
                let name = entry?.file_name().to_string_lossy().into_owned();
                if name.ends_with(".ftl") {
                    names.push(name);
                }
            }
        }
        names.sort();
        let mut files = Vec::new();
        for name in names {
            let rel = format!("{LOCALIZATION_DIR}/{tag}/{name}");
            let text =
                std::fs::read_to_string(project.join(&rel)).with_context(|| format!("reading {rel}"))?;
            files.push((rel, text));
        }
        out.push((loc_check::parse_locale(&tag, &files), files));
    }
    Ok(out)
}

/// Adds the keys of every `.dialogue.ron` (transcript and display keys, inline cue keys), every `.subs.ron`
/// (cue keys) and every `.voice.ron` (display names) under `assets/` to a gather, with the line's note and
/// speaker as comments (spec 36 § 7.2: the keys are plain strings the text scan does not find).
///
/// # Errors
///
/// None today; a file that does not parse is reported by `regia loc validate` through the library errors.
pub fn add_dialogue_keys(project: &Path, gathered: &mut regia_ui::loc_gather::Gathered) -> Result<()> {
    let assets = project.join("assets");
    if !assets.is_dir() {
        return Ok(());
    }
    let library = DialogueLibrary::load_dir(&assets);
    let line_of = |rel: &str, needle: &str| -> u32 {
        std::fs::read_to_string(assets.join(rel))
            .ok()
            .and_then(|text| {
                text.lines()
                    .position(|l| l.contains(needle))
                    .and_then(|i| u32::try_from(i + 1).ok())
            })
            .unwrap_or(1)
    };
    let dialogue_files: BTreeMap<String, String> = library
        .dialogues
        .keys()
        .map(|name| (name.clone(), find_file(&assets, &format!("{name}.dialogue.ron"))))
        .collect();
    for (name, dialogue) in &library.dialogues {
        let rel = dialogue_files.get(name).cloned().unwrap_or_default();
        let file = format!("assets/{rel}");
        gathered.files_scanned += 1;
        for line in &dialogue.lines {
            let at = line_of(&rel, &format!("\"{}\"", line.id));
            let mut notes = vec![format!("speaker: {}", line.speaker)];
            if !line.note.is_empty() {
                notes.push(format!("note: {}", line.note));
            }
            gathered.add_key(&line.text_key(name), &file, at, notes.clone());
            if let Some(display) = &line.display_key {
                gathered.add_key(display, &file, at, notes.clone());
            }
            for cue in line.variants.iter().filter_map(|v| v.cues.as_ref()).flatten() {
                if let Some(key) = &cue.text_key {
                    gathered.add_key(key, &file, at, Vec::new());
                }
            }
        }
    }
    for (rel, track) in &library.subtitles {
        gathered.files_scanned += 1;
        for cue in &track.cues {
            if let Some(key) = &cue.text_key {
                gathered.add_key(key, &format!("assets/{rel}"), line_of(rel, key), Vec::new());
            }
        }
    }
    for voice in library.voices.values() {
        let rel = find_file(&assets, &format!("{}.voice.ron", voice.id));
        gathered.files_scanned += 1;
        gathered.add_key(&voice.name_key, &format!("assets/{rel}"), 1, Vec::new());
    }
    Ok(())
}

/// The `assets/`-relative path of the first file named `name` under `assets` (sorted walk), or `name`.
pub fn find_file(assets: &Path, name: &str) -> String {
    let mut stack = vec![assets.to_path_buf()];
    let mut found: Vec<String> = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // `file_type` does not follow. A directory symlink (including a link at `..`)
            // would otherwise walk forever.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == name)
                && let Ok(rel) = path.strip_prefix(assets)
            {
                found.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    found.sort();
    found.into_iter().next().unwrap_or_else(|| name.to_owned())
}

/// Writes `text` to `path` through a fresh temp file and a rename (`regia loc export`, the bundles of
/// `regia loc compile`, and the editor's export). A planted symlink at the temp name is unlinked, never
/// written through.
///
/// # Errors
///
/// The directory, the temp file or the rename fails.
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    // A planted symlink at the predictable temp name must be unlinked, never written through.
    if tmp.symlink_metadata().is_ok() {
        std::fs::remove_file(&tmp).with_context(|| format!("removing {}", tmp.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let written = file
        .write_all(text.as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))
        .and_then(|()| {
            drop(file);
            std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
        });
    if written.is_err() {
        // The target is unharmed; the temporary file must not be left behind either.
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The file format of an exchange file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExchangeFormat {
    /// gettext PO (`<locale>.po`).
    #[default]
    Po,
    /// XLIFF 2.0 (`<locale>.xlf`), placeholders as protected inline codes.
    Xliff,
}

impl ExchangeFormat {
    /// The file extension (`po`, `xlf`).
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Po => "po",
            Self::Xliff => "xlf",
        }
    }

    /// Parses `po` or `xliff` (`xlf` too).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "po" => Some(Self::Po),
            "xliff" | "xlf" => Some(Self::Xliff),
            _ => None,
        }
    }
}

/// The file an export writes when the caller names none: `<project>/.regia/loc/<locale>.po` (or `.xlf`).
#[must_use]
pub fn default_export_path(project: &Path, locale: &str, format: ExchangeFormat) -> PathBuf {
    project
        .join(".regia")
        .join("loc")
        .join(format!("{locale}.{}", format.extension()))
}

/// What [`export`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exported {
    /// The file's text (PO or XLIFF).
    pub text: String,
    /// Entries in the file.
    pub entries: usize,
    /// Messages with attributes (attributes are not exported).
    pub skipped_attributes: usize,
    /// Fluent terms of the source locale (not exported).
    pub skipped_terms: usize,
    /// Entries the translation memory pre-filled (flagged for review).
    pub tm_pre_filled: usize,
    /// Near-match notes the translation memory added.
    pub tm_near: usize,
}

/// The export of `locale` (`regia loc export`): every source message the `filter` keeps, with the gather
/// scan's references and the translation memory applied, as PO or XLIFF text. Writes nothing.
///
/// # Errors
///
/// The project's localization cannot be read, the locale is the source locale, or the source locale is
/// unknown.
pub fn export(
    project: &Path,
    locale: &str,
    format: ExchangeFormat,
    filter: ExportFilter,
) -> Result<Exported> {
    let _span = tracing::info_span!("loc_exchange_export", locale, ?format).entered();
    let config = read_config(project)?;
    let locales = read_locales(project, &config)?;
    let source = if config.source.is_empty() {
        "en".to_owned()
    } else {
        config.source.clone()
    };
    let files_of = |tag: &str| {
        locales
            .iter()
            .find(|(p, _)| p.locale == tag)
            .map(|(_, f)| f.clone())
    };
    let Some(source_files) = files_of(&source) else {
        bail!("no source locale `{source}` under {LOCALIZATION_DIR}");
    };
    if locale == source {
        bail!("`{locale}` is the source locale; export a translation locale");
    }
    let target_files = files_of(locale).unwrap_or_default();
    let mut gathered = loc_gather::gather_project(project, &config.gather)?;
    // Dialogue keys are plain strings in `.dialogue.ron` files: add their uses so their entries get
    // `#:` references like every other key (spec 36 §§ 3, 7.2; `regia loc gather` does the same).
    add_dialogue_keys(project, &mut gathered)?;
    let references = gathered
        .keys
        .iter()
        .map(|(k, uses)| {
            (
                k.clone(),
                uses.iter().map(|u| format!("{}:{}", u.file, u.line)).collect(),
            )
        })
        .collect();
    let project_name = project
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "project".to_owned());
    let mut output = loc_po::export_po(&ExportInput {
        project: format!("{project_name} {}", env!("CARGO_PKG_VERSION")),
        source: source.clone(),
        locale: locale.to_owned(),
        source_files,
        target_files,
        references,
        filter,
    });
    // Translation memory: pre-fill exact matches of already-translated source strings and
    // offer near matches as notes (ADR-3359).
    let memory = loc_glossary::build_memory(&files_of(&source).unwrap_or_default(), &locales);
    let (tm_pre_filled, tm_near) = loc_glossary::apply_memory(&mut output, &memory, locale);
    let text = match format {
        ExchangeFormat::Po => loc_po::write_po(&output.file),
        ExchangeFormat::Xliff => loc_xliff::write_xliff(&output.file).map_err(|e| anyhow::anyhow!("{e}"))?,
    };
    Ok(Exported {
        text,
        entries: output.file.entries.len(),
        skipped_attributes: output.skipped_attributes,
        skipped_terms: output.skipped_terms,
        tm_pre_filled,
        tm_near,
    })
}

/// A parsed exchange file a translator returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExchangeFile {
    /// gettext PO.
    Po(PoFile),
    /// XLIFF 2.0.
    Xliff(loc_xliff::XliffImport),
}

impl ExchangeFile {
    /// Parses PO or XLIFF text; the format is detected from the content. `shown` names the file in
    /// the error.
    ///
    /// # Errors
    ///
    /// The text is not a readable PO or XLIFF file.
    pub fn parse(text: &str, shown: &str) -> Result<Self> {
        if loc_xliff::looks_like_xliff(text) {
            let xliff = loc_xliff::read_xliff(text).map_err(|e| anyhow::anyhow!("{shown}: {e}"))?;
            return Ok(Self::Xliff(xliff));
        }
        let po = loc_po::read_po(text).map_err(|e| anyhow::anyhow!("{shown}: {e}"))?;
        Ok(Self::Po(po))
    }

    /// The format.
    #[must_use]
    pub fn format(&self) -> ExchangeFormat {
        match self {
            Self::Po(_) => ExchangeFormat::Po,
            Self::Xliff(_) => ExchangeFormat::Xliff,
        }
    }

    /// The file's `Language` header.
    #[must_use]
    pub fn language(&self) -> Option<String> {
        match self {
            Self::Po(po) => po.header_value("Language").map(str::to_owned),
            Self::Xliff(x) => x.po.header_value("Language").map(str::to_owned),
        }
    }

    /// The import plan of this file against `ctx`.
    #[must_use]
    pub fn plan(&self, ctx: &ImportContext) -> ImportPlan {
        match self {
            Self::Po(po) => loc_po::plan_import(po, ctx),
            Self::Xliff(x) => loc_xliff::plan_import_xliff(x, ctx),
        }
    }
}

/// A locale tag that is one plain directory name: ASCII letters, digits, `-` and `_`, starting
/// with a letter, at most 64 bytes (`fr`, `pt-BR`, `zh_Hant_TW`).
#[must_use]
pub fn is_locale_tag(tag: &str) -> bool {
    tag.len() <= 64
        && tag.starts_with(|c: char| c.is_ascii_alphabetic())
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A planned import: the locale, the plan, and the files it writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedImport {
    /// The locale imported (the caller's, else the file's `Language`).
    pub locale: String,
    /// What the import writes and its report.
    pub plan: ImportPlan,
}

impl PlannedImport {
    /// The project-relative directory of the locale (`assets/localization/<locale>`).
    #[must_use]
    pub fn dir(&self) -> String {
        format!("{LOCALIZATION_DIR}/{}", self.locale)
    }

    /// Whether the plan writes anything: the file was not refused and an entry was accepted.
    #[must_use]
    pub fn writes(&self) -> bool {
        self.plan.report.refused.is_none() && self.plan.report.imported > 0
    }

    /// The files a write of the plan rewrites: `imported.ftl`, then `gathered.ftl` when placeholders
    /// were removed (empty when [`Self::writes`] is false).
    #[must_use]
    pub fn files(&self) -> Vec<PlannedFile> {
        if !self.writes() {
            return Vec::new();
        }
        let dir = self.dir();
        let file = |name: &str, text: &str| PlannedFile {
            locale: self.locale.clone(),
            path: format!("{dir}/{name}"),
            added: Vec::new(),
            text: text.to_owned(),
        };
        let mut out = vec![file("imported.ftl", &self.plan.imported_ftl)];
        if let Some(gathered) = &self.plan.gathered_ftl {
            out.push(file(loc_gather::GATHERED_FILE, gathered));
        }
        out
    }
}

/// Plans an import into `project` (`regia loc import`): `locale` (else `header_locale`, the file's
/// `Language`) names the locale, `plan` decides from the project's context what to write. Writes
/// nothing.
///
/// # Errors
///
/// No locale is known or it is not a locale tag, or the localization files cannot be read.
pub fn plan_import(
    project: &Path,
    header_locale: Option<String>,
    locale: Option<String>,
    plan: impl FnOnce(&ImportContext) -> ImportPlan,
) -> Result<PlannedImport> {
    let _span = tracing::info_span!("loc_exchange_plan_import").entered();
    let locale = locale
        .or(header_locale)
        .context("the file has no Language header; pass --locale")?;
    // The tag names the directory written under `assets/localization`: a translator's file must not
    // pick another one (`Language: ../../x`).
    if !is_locale_tag(&locale) {
        bail!("`{locale}` is not a locale tag (ASCII letters, digits, `-` and `_`); pass --locale");
    }
    let config = read_config(project)?;
    let locales = read_locales(project, &config)?;
    let source = if config.source.is_empty() {
        "en".to_owned()
    } else {
        config.source.clone()
    };
    let Some((_, source_files)) = locales.iter().find(|(p, _)| p.locale == source) else {
        bail!("no source locale `{source}` under {LOCALIZATION_DIR}");
    };
    let dir = format!("{LOCALIZATION_DIR}/{locale}");
    let read_if = |name: &str| std::fs::read_to_string(project.join(format!("{dir}/{name}"))).ok();
    let ctx = ImportContext {
        locale: locale.clone(),
        source,
        source_files: source_files.clone(),
        gathered: read_if(loc_gather::GATHERED_FILE),
        existing_imported: read_if("imported.ftl"),
    };
    let plan = plan(&ctx);
    Ok(PlannedImport { locale, plan })
}

/// Writes a planned import's files ([`PlannedImport::files`]) with the gather tool's atomic writer.
///
/// # Errors
///
/// A file cannot be written.
pub fn write_import(project: &Path, planned: &PlannedImport) -> Result<()> {
    for file in planned.files() {
        loc_gather::write_planned(project, &file).with_context(|| format!("writing {}", file.path))?;
    }
    Ok(())
}

// ------------------------------------------------------------------------------------ validate and compile

/// `assets/localization/glossary.ron` (the glossary file), when the project has one.
///
/// # Errors
///
/// The file exists but cannot be read or does not parse.
pub fn read_glossary(project: &Path) -> Result<Option<loc_glossary::GlossaryFile>> {
    let path = project.join(LOCALIZATION_DIR).join(loc_glossary::GLOSSARY_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(
        loc_glossary::parse_glossary(&text).with_context(|| format!("in {}", path.display()))?,
    ))
}

/// The dialogue validator's issues (`regia loc validate`, spec 36 § 7) as validator findings: clips,
/// duplicate lines, unknown voices, keys the source lacks, sounds missing for a shipping locale.
#[must_use]
pub fn dialogue_findings(
    project: &Path,
    config: &LocalizationConfig,
    locales: &[LoadedLocale],
) -> Vec<loc_check::Finding> {
    let source = if config.source.is_empty() {
        "en"
    } else {
        config.source.as_str()
    };
    let files: Vec<crate::dialogue_validator::LocaleFiles> = locales
        .iter()
        .map(|(p, files)| (p.locale.clone(), files.clone()))
        .collect();
    crate::dialogue_validator::dialogue_issues(project, config, &files)
        .into_iter()
        .map(|issue| loc_check::Finding {
            code: issue.code,
            severity: if issue.error {
                loc_check::Severity::Error
            } else {
                loc_check::Severity::Warning
            },
            locale: issue.locale.unwrap_or_else(|| source.to_owned()),
            key: None,
            file: Some(issue.subject),
            line: None,
            message: issue.message,
        })
        .collect()
}

/// Validates `locales` the way `regia loc validate` does: the locale checks (with `used_keys` when the
/// caller ran the gather scan), the dialogue findings and the glossary findings, sorted by locale, key,
/// code and line.
///
/// # Errors
///
/// The glossary file cannot be read or does not parse.
pub fn validate_locales(
    project: &Path,
    config: &LocalizationConfig,
    locales: Vec<LoadedLocale>,
    used_keys: Option<BTreeSet<String>>,
) -> Result<loc_check::Report> {
    let dialogue = dialogue_findings(project, config, &locales);
    let glossary = read_glossary(project)?
        .map(|g| {
            let source_files = locales
                .iter()
                .find(|(p, _)| p.locale == config.source)
                .map_or_else(Vec::new, |(_, f)| f.clone());
            loc_glossary::glossary_findings(&g, &source_files, &locales)
        })
        .unwrap_or_default();
    let parsed: Vec<ParsedLocale> = locales.into_iter().map(|(p, _)| p).collect();
    let mut report = loc_check::validate(
        &parsed,
        &loc_check::CheckOptions {
            source: config.source.clone(),
            fallbacks: config.fallbacks.clone(),
            used_keys,
        },
    );
    report.findings.extend(dialogue);
    report.findings.extend(glossary);
    report.findings.sort_by(|a, b| {
        (a.locale.as_str(), a.key.as_deref(), a.code, a.line).cmp(&(
            b.locale.as_str(),
            b.key.as_deref(),
            b.code,
            b.line,
        ))
    });
    Ok(report)
}

/// What [`compile_bundles`] did: the validator's report and, when the report did not fail, the bundles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledBundles {
    /// The validation report of the project's locales.
    pub report: loc_check::Report,
    /// The bundle directory and the shipped locales; `None` when validation failed (nothing written).
    pub written: Option<(PathBuf, Vec<String>)>,
}

/// Validates the project's locales and writes the shipping bundles to `<out>/loc/` (spec 36 § 4;
/// `regia loc compile`, `regia package` and the Localization panel's Compile): the `requested` locales
/// (default: `locales` of `localization.ron`, else every locale found) plus their fallback chains and the
/// source. A failing validation (`strict`: warnings fail too) writes nothing.
///
/// # Errors
///
/// The localization files cannot be read, the source locale has no files, or a bundle cannot be written.
pub fn compile_bundles(
    project: &Path,
    out: &Path,
    requested: &[String],
    strict: bool,
) -> Result<CompiledBundles> {
    use regia_ui::loc_bundle::{self, BundleIndex, BundleLocale};
    let config = read_config(project)?;
    let source = if config.source.is_empty() {
        "en".to_owned()
    } else {
        config.source.clone()
    };
    let all = read_locales(project, &config)?;
    let report = validate_locales(project, &config, all, None)?;
    if report.fails(strict) {
        return Ok(CompiledBundles {
            report,
            written: None,
        });
    }
    // `locales` filters what is read; the source locale always ships (spec 36 § 4), listed or not.
    let mut reading = config.clone();
    if !reading.locales.is_empty() && !reading.locales.contains(&source) {
        reading.locales.push(source.clone());
    }
    let locales = read_locales(project, &reading)?;
    let available: BTreeSet<String> = locales.iter().map(|(p, _)| p.locale.clone()).collect();
    let requested = if requested.is_empty() {
        config.locales.clone()
    } else {
        requested.to_vec()
    };
    let requested = if requested.is_empty() {
        available.iter().cloned().collect()
    } else {
        requested
    };
    let ship = loc_bundle::locales_to_ship(&requested, &available, &config.fallbacks, &source)?;
    let out_dir = out.join("loc");
    let mut entries = Vec::new();
    for tag in &ship {
        let Some((_, files)) = locales.iter().find(|(p, _)| &p.locale == tag) else {
            bail!("locale `{tag}` is in the shipping set but has no files");
        };
        let bundle = loc_bundle::normalize_locale(files);
        write_atomic(&out_dir.join(format!("{tag}.ftl")), &bundle.text)?;
        entries.push(BundleLocale {
            tag: tag.clone(),
            file: format!("loc/{tag}.ftl"),
            messages: bundle.messages,
            hash: regia_asset::content_hash_hex(bundle.text.as_bytes()),
        });
    }
    let fallbacks = config
        .fallbacks
        .iter()
        .filter(|(k, _)| ship.contains(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let index = BundleIndex {
        format_version: loc_bundle::BUNDLE_INDEX_VERSION,
        source,
        locales: entries,
        fallbacks,
    };
    write_atomic(
        &out_dir.join("index.ron"),
        &index.to_ron().map_err(|e| anyhow::anyhow!(e))?,
    )?;
    Ok(CompiledBundles {
        report,
        written: Some((out_dir, ship)),
    })
}
