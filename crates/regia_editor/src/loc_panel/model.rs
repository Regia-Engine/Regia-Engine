// SPDX-License-Identifier: MPL-2.0
//! The Localization panel's model (ADR-3260): the project's Fluent files read as the CLI reads them, the
//! key-by-locale table with its filters, and the plan of a one-entry edit. Pure apart from
//! [`read_project`]; the panel, the agent methods and the tests share it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use regia_ui::loc_check::{self, CheckOptions};
use regia_ui::loc_config::{LOCALIZATION_CONFIG_PATH, LocalizationConfig};
use regia_ui::loc_gather::is_message_id;
use regia_ui::loc_po::{self, FtlBlock, LocaleCoverage, PLACEHOLDER_MARKER, SOURCE_HASH_PREFIX};

use crate::data_panels::PanelError;

/// Where the locale directories live, relative to the project root (`docs/specs/content-data.md` § 1.2).
pub const LOCALIZATION_DIR: &str = "assets/localization";
/// The default ratio of the `long` filter: a translation longer than 1.5 times its source text.
pub const DEFAULT_LONG_RATIO: f32 = 1.5;
/// Rows per page when a caller does not say.
pub const DEFAULT_PAGE_SIZE: usize = 50;
/// The most rows one page may hold.
pub const MAX_PAGE_SIZE: usize = 500;

pub(crate) fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("loc_panel_{code}"), message)
}

/// One locale's `.ftl` files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocaleFiles {
    /// The locale tag.
    pub locale: String,
    /// `(project-relative path, text)` in runtime order (sorted by file name; later files win).
    pub files: Vec<(String, String)>,
}

/// The project's localization as the CLI reads it (`regia loc validate`, `export`, `gather`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocProject {
    /// `localization.ron`, or its defaults.
    pub config: LocalizationConfig,
    /// The source locale (`config.source`, `en` when empty).
    pub source: String,
    /// Every locale, sorted by tag.
    pub locales: Vec<LocaleFiles>,
}

impl LocProject {
    /// The files of `locale`.
    #[must_use]
    pub fn files(&self, locale: &str) -> Option<&[(String, String)]> {
        self.locales
            .iter()
            .find(|l| l.locale == locale)
            .map(|l| l.files.as_slice())
    }

    /// The locale tags.
    #[must_use]
    pub fn tags(&self) -> Vec<String> {
        self.locales.iter().map(|l| l.locale.clone()).collect()
    }
}

/// Reads `localization.ron` and every locale directory under [`LOCALIZATION_DIR`] the way `regia loc`
/// does: the configured locales (or, with none configured, every directory), plus the source locale, each
/// with its `.ftl` files sorted by name.
///
/// # Errors
///
/// `loc_panel_bad_config` when `localization.ron` does not parse, `loc_panel_io_failed` when a directory or
/// file cannot be read.
pub fn read_project(root: &Path) -> Result<LocProject, PanelError> {
    let _span = tracing::debug_span!(target: "regia_editor", "loc_panel_read").entered();
    let io = |what: &Path, e: std::io::Error| err("io_failed", format!("{}: {e}", what.display()));
    let config_path = root.join(LOCALIZATION_CONFIG_PATH);
    let config = if config_path.is_file() {
        let text = std::fs::read_to_string(&config_path).map_err(|e| io(&config_path, e))?;
        LocalizationConfig::from_ron(&text)
            .map_err(|e| err("bad_config", format!("{LOCALIZATION_CONFIG_PATH}: {e}")))?
    } else {
        LocalizationConfig::default()
    };
    let source = if config.source.is_empty() {
        "en".to_owned()
    } else {
        config.source.clone()
    };
    let dir = root.join(LOCALIZATION_DIR);
    let mut tags: BTreeSet<String> = config.locales.iter().cloned().collect();
    if dir.is_dir() {
        for entry in std::fs::read_dir(&dir).map_err(|e| io(&dir, e))? {
            let entry = entry.map_err(|e| io(&dir, e))?;
            let is_dir = entry.file_type().map_err(|e| io(&entry.path(), e))?.is_dir();
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_dir
                && !name.starts_with('.')
                && (config.locales.is_empty() || tags.contains(&name) || name == source)
            {
                tags.insert(name);
            }
        }
    }
    let mut locales = Vec::new();
    for tag in tags {
        let locale_dir = dir.join(&tag);
        let mut names = Vec::new();
        if locale_dir.is_dir() {
            for entry in std::fs::read_dir(&locale_dir).map_err(|e| io(&locale_dir, e))? {
                let name = entry
                    .map_err(|e| io(&locale_dir, e))?
                    .file_name()
                    .to_string_lossy()
                    .into_owned();
                if name.ends_with(".ftl") {
                    names.push(name);
                }
            }
        }
        names.sort();
        let mut files = Vec::new();
        for name in names {
            let rel = format!("{LOCALIZATION_DIR}/{tag}/{name}");
            let full = root.join(&rel);
            let text = std::fs::read_to_string(&full).map_err(|e| io(&full, e))?;
            files.push((rel, text));
        }
        locales.push(LocaleFiles { locale: tag, files });
    }
    Ok(LocProject {
        config,
        source,
        locales,
    })
}

/// What a cell of the table holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CellState {
    /// The source locale's own text.
    Source,
    /// A translation made against the current source text (or with no recorded source).
    Translated,
    /// A translation whose recorded source hash (`# regia-source:`) differs from the current source text.
    Stale,
    /// Only the gather tool's `# TODO translate` placeholder.
    Placeholder,
    /// The locale's files do not define the key.
    Missing,
}

impl CellState {
    /// The state's name in the agent protocol.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Translated => "translated",
            Self::Stale => "stale",
            Self::Placeholder => "placeholder",
            Self::Missing => "missing",
        }
    }
}

/// One locale's entry for one key.
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    /// What it holds.
    pub state: CellState,
    /// The value pattern as written (continuation indent removed); `None` when missing.
    pub text: Option<String>,
    /// The file that defines it (the last definition in runtime order).
    pub file: Option<String>,
    /// The 1-based line of `key =` (0 when missing).
    pub line: u32,
    /// The text is longer than the source text by more than the table's ratio.
    pub long: bool,
}

/// One key of the table.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// The message key.
    pub key: String,
    /// Whether the source locale defines it.
    pub in_source: bool,
    /// The translator context: the `#` comment lines above the message (the source locale's, else the
    /// first locale that defines the key), without tool markers (`regia_ui::loc_po::FtlBlock::context`).
    pub context: Vec<String>,
    /// One cell per locale of the table, by tag.
    pub cells: BTreeMap<String, Cell>,
}

/// Which rows the table shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    /// Every key.
    #[default]
    All,
    /// Keys a target locale does not define at all.
    Missing,
    /// Keys a target locale has no real translation for (missing, or only a gather placeholder).
    Untranslated,
    /// Keys whose translation was made against an older source text.
    Stale,
    /// Keys whose translation is longer than the source text by more than the ratio.
    Long,
}

impl Filter {
    /// Every filter, in menu order.
    pub const ALL: [Filter; 5] = [
        Self::All,
        Self::Missing,
        Self::Untranslated,
        Self::Stale,
        Self::Long,
    ];

    /// The filter's name in the agent protocol.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Missing => "missing",
            Self::Untranslated => "untranslated",
            Self::Stale => "stale",
            Self::Long => "long",
        }
    }

    /// The filter named `name`.
    ///
    /// # Errors
    ///
    /// `loc_panel_bad_filter` for an unknown name.
    pub fn parse(name: &str) -> Result<Self, PanelError> {
        Self::ALL.into_iter().find(|f| f.as_str() == name).ok_or_else(|| {
            err(
                "bad_filter",
                format!("unknown filter `{name}` (all, missing, untranslated, stale, long)"),
            )
        })
    }

    fn matches(self, cell: &Cell) -> bool {
        match self {
            Self::All => true,
            Self::Missing => cell.state == CellState::Missing,
            Self::Untranslated => matches!(cell.state, CellState::Missing | CellState::Placeholder),
            Self::Stale => cell.state == CellState::Stale,
            Self::Long => cell.long,
        }
    }
}

/// What the table is asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    /// Only this target locale (`None`: every locale).
    pub locale: Option<String>,
    /// Which rows.
    pub filter: Filter,
    /// Case-insensitive text matched against the key, the source text and the shown translations.
    pub search: String,
    /// The `long` filter's ratio (translation chars over source chars).
    pub long_ratio: f32,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            locale: None,
            filter: Filter::All,
            search: String::new(),
            long_ratio: DEFAULT_LONG_RATIO,
        }
    }
}

/// A parse error or duplicate key found while reading a locale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIssue {
    /// The validator's code (`loc_parse_error`, `loc_duplicate_key`).
    pub code: String,
    /// The locale.
    pub locale: String,
    /// The file.
    pub file: Option<String>,
    /// The 1-based line.
    pub line: Option<u32>,
    /// A sentence for people.
    pub message: String,
}

/// The whole table: every key, every locale, before filtering.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocTable {
    /// The source locale.
    pub source: String,
    /// The locale columns: the source first, then the targets sorted.
    pub locales: Vec<String>,
    /// Every key any locale defines, sorted.
    pub rows: Vec<Row>,
    /// Coverage of every target locale (`regia_ui::loc_po::coverage`, the `localization.coverage` numbers).
    pub coverage: Vec<LocaleCoverage>,
    /// Parse errors and duplicate keys.
    pub issues: Vec<FileIssue>,
    /// The `long` ratio the cells' `long` flags were computed with.
    pub long_ratio: f32,
}

/// The effective block of each key in `files`: the last definition in runtime order, with its file.
fn effective_blocks(files: &[(String, String)]) -> BTreeMap<String, (String, FtlBlock)> {
    let mut out = BTreeMap::new();
    for (path, text) in files {
        for block in loc_po::ftl_blocks(text) {
            out.insert(block.key.clone(), (path.clone(), block));
        }
    }
    out
}

/// Builds the table of `project`; `long_ratio` sets the cells' `long` flags.
#[must_use]
pub fn build_table(project: &LocProject, long_ratio: f32) -> LocTable {
    let _span = tracing::debug_span!(target: "regia_editor", "loc_panel_table").entered();
    let source = project.source.clone();
    let source_files = project.files(&source).unwrap_or(&[]);
    let mut locales = vec![source.clone()];
    locales.extend(project.tags().into_iter().filter(|t| *t != source));
    let blocks: BTreeMap<String, BTreeMap<String, (String, FtlBlock)>> = project
        .locales
        .iter()
        .map(|l| (l.locale.clone(), effective_blocks(&l.files)))
        .collect();
    let empty = BTreeMap::new();
    let source_blocks = blocks.get(&source).unwrap_or(&empty);
    let keys: BTreeSet<&String> = blocks.values().flat_map(BTreeMap::keys).collect();
    let mut rows = Vec::with_capacity(keys.len());
    for key in keys {
        let source_block = source_blocks.get(key).map(|(_, b)| b);
        let source_len = source_block.map_or(0, |b| b.pattern.chars().count());
        let source_hash = source_block.map(|b| loc_po::source_hash(&b.pattern));
        let mut cells = BTreeMap::new();
        for tag in &locales {
            let have = blocks.get(tag).and_then(|b| b.get(key));
            let cell = match have {
                None => Cell {
                    state: CellState::Missing,
                    text: None,
                    file: None,
                    line: 0,
                    long: false,
                },
                Some((file, block)) => {
                    let state = if *tag == source {
                        CellState::Source
                    } else if block.placeholder {
                        CellState::Placeholder
                    } else if block
                        .source_hash
                        .as_deref()
                        .is_some_and(|h| source_hash.as_deref().is_some_and(|s| s != h))
                    {
                        CellState::Stale
                    } else {
                        CellState::Translated
                    };
                    let translated = matches!(state, CellState::Translated | CellState::Stale);
                    // Message lengths stay far below 2^24 chars, so the conversions are exact.
                    let long = translated
                        && source_len > 0
                        && block.pattern.chars().count() as f32 > source_len as f32 * long_ratio;
                    Cell {
                        state,
                        text: Some(block.pattern.clone()),
                        file: Some(file.clone()),
                        line: block.line,
                        long,
                    }
                }
            };
            cells.insert(tag.clone(), cell);
        }
        let context = source_block
            .or_else(|| {
                locales
                    .iter()
                    .find_map(|t| blocks.get(t).and_then(|b| b.get(key)).map(|(_, b)| b))
            })
            .map(|b| b.context.clone())
            .unwrap_or_default();
        rows.push(Row {
            key: key.clone(),
            in_source: source_block.is_some(),
            context,
            cells,
        });
    }
    let coverage = locales
        .iter()
        .filter(|t| **t != source)
        .map(|t| loc_po::coverage(t, source_files, project.files(t).unwrap_or(&[])))
        .collect();
    let mut issues = Vec::new();
    for l in &project.locales {
        for f in loc_check::parse_locale(&l.locale, &l.files).findings {
            issues.push(FileIssue {
                code: f.code.to_owned(),
                locale: f.locale,
                file: f.file,
                line: f.line,
                message: f.message,
            });
        }
    }
    LocTable {
        source,
        locales,
        rows,
        coverage,
        issues,
        long_ratio,
    }
}

impl LocTable {
    /// The target locales a query looks at.
    fn targets<'a>(&'a self, query: &'a Query) -> impl Iterator<Item = &'a String> + 'a {
        self.locales
            .iter()
            .filter(move |t| **t != self.source && query.locale.as_ref().is_none_or(|l| l == *t))
    }

    /// The rows `query` selects, in key order.
    #[must_use]
    pub fn select(&self, query: &Query) -> Vec<&Row> {
        let needle = query.search.trim().to_lowercase();
        self.rows
            .iter()
            .filter(|row| {
                let filtered = query.filter == Filter::All
                    || self
                        .targets(query)
                        .filter_map(|t| row.cells.get(t))
                        .any(|c| query.filter.matches(c));
                if !filtered {
                    return false;
                }
                if needle.is_empty() {
                    return true;
                }
                let text_of = |t: &String| {
                    row.cells
                        .get(t)
                        .and_then(|c| c.text.as_deref())
                        .is_some_and(|s| s.to_lowercase().contains(&needle))
                };
                row.key.to_lowercase().contains(&needle)
                    || text_of(&self.source)
                    || self.targets(query).any(text_of)
            })
            .collect()
    }
}

/// A file a write replaces: its text before (`None`: the file did not exist) and after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// Project-relative path.
    pub path: String,
    /// The locale it belongs to.
    pub locale: String,
    /// The text before the write; `None` when the write creates the file.
    pub before: Option<String>,
    /// The text after the write.
    pub after: String,
}

/// A planned translation edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedSet {
    /// The one file it rewrites.
    pub change: FileChange,
    /// Validator findings on the new text that do not refuse it (`loc_variable_mismatch`,
    /// `loc_plural_missing`): `(code, message)`.
    pub warnings: Vec<(String, String)>,
}

/// The newline a file uses (`\r\n` when its first line ends so).
fn newline_of(text: &str) -> &'static str {
    match text.find('\n') {
        Some(i) if i > 0 && text.as_bytes()[i - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

/// `key = text` with continuation lines indented by four spaces, each line ended by `nl`.
fn message_lines(key: &str, text: &str, nl: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 1 {
        return format!("{key} = {text}{nl}");
    }
    let mut out = format!("{key} ={nl}");
    for line in lines {
        if line.is_empty() {
            out.push_str(nl);
        } else {
            out.push_str("    ");
            out.push_str(line);
            out.push_str(nl);
        }
    }
    out
}

/// Whether `line` (without its terminator) is an attribute line of a message (`    .title = ...`).
fn is_attribute(line: &str) -> bool {
    let t = line.trim_start();
    line.starts_with([' ', '\t']) && t.starts_with('.') && t[1..].contains('=')
}

/// Checks the new text of `key` in `locale`: it must parse as a Fluent pattern on its own (no attribute or
/// stray message), and (outside the source locale) its variables are compared with the source message.
fn check_text(
    project: &LocProject,
    key: &str,
    locale: &str,
    text: &str,
) -> Result<Vec<(String, String)>, PanelError> {
    if text.trim().is_empty() {
        return Err(err("empty_text", "the text is empty; a message needs a value"));
    }
    let candidate = message_lines(key, text, "\n");
    let files = vec![("edit.ftl".to_owned(), candidate)];
    let parsed = loc_check::parse_locale(locale, &files);
    if let Some(f) = parsed.findings.iter().find(|f| f.code == "loc_parse_error") {
        return Err(err(
            "bad_text",
            format!("the text is not a valid Fluent pattern: {}", f.message),
        ));
    }
    let one = parsed.messages.len() == 1
        && parsed
            .messages
            .get(key)
            .is_some_and(|m| m.has_value && m.attributes.is_empty());
    if !one {
        return Err(err(
            "bad_text",
            "the text must be one value: a line may not start with `.` (an attribute) or define another message",
        ));
    }
    if locale == project.source {
        return Ok(Vec::new());
    }
    let source_files = project.files(&project.source).unwrap_or(&[]);
    let source_parsed = loc_check::parse_locale(&project.source, source_files);
    let Some(message) = source_parsed.messages.get(key) else {
        return Ok(Vec::new());
    };
    let source_one = loc_check::ParsedLocale {
        locale: project.source.clone(),
        messages: std::iter::once((key.to_owned(), message.clone())).collect(),
        findings: Vec::new(),
    };
    let report = loc_check::validate(
        &[source_one, parsed],
        &CheckOptions {
            source: project.source.clone(),
            ..CheckOptions::default()
        },
    );
    Ok(report
        .findings
        .iter()
        .filter(|f| f.locale == locale && f.key.as_deref() == Some(key))
        .filter(|f| f.code == "loc_variable_mismatch" || f.code == "loc_plural_missing")
        .map(|f| (f.code.to_owned(), f.message.clone()))
        .collect())
}

/// Plans setting the value of `key` in `locale` to `text` (ADR-3260):
///
/// - when the locale defines the key, the last definition (the one the runtime uses) is rewritten in its
///   file: its value lines are replaced, its attributes and other comments stay, the gather tool's
///   `# TODO translate` marker is dropped and (outside the source locale) the `# regia-source:` hash of the
///   current source text is updated or added above the message, so the stale filter clears;
/// - otherwise the message is appended to the locale's file with the name of the source file that defines
///   the key (created when absent).
///
/// Every other byte of the file is kept.
///
/// # Errors
///
/// `loc_panel_bad_key`, `loc_panel_locale_not_found`, `loc_panel_key_not_found`, `loc_panel_empty_text`,
/// `loc_panel_bad_text`.
pub fn plan_set(project: &LocProject, key: &str, locale: &str, text: &str) -> Result<PlannedSet, PanelError> {
    if !is_message_id(key) {
        return Err(err("bad_key", format!("`{key}` is not a Fluent message id")));
    }
    let Some(target_files) = project.files(locale) else {
        return Err(err(
            "locale_not_found",
            format!("no locale `{locale}`; locales: {}", project.tags().join(", ")),
        ));
    };
    let text = text.trim_end_matches(['\n', '\r']).replace("\r\n", "\n");
    let warnings = check_text(project, key, locale, &text)?;
    let source_files = project.files(&project.source).unwrap_or(&[]);
    let source_block = effective_blocks(source_files).remove(key);
    let hash = (locale != project.source)
        .then(|| {
            source_block
                .as_ref()
                .map(|(_, b)| loc_po::source_hash(&b.pattern))
        })
        .flatten();
    if let Some((path, block)) = effective_blocks(target_files).remove(key) {
        let before = target_files
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        let after = rewrite_block(&before, &block, key, &text, hash.as_deref());
        return Ok(PlannedSet {
            change: FileChange {
                path,
                locale: locale.to_owned(),
                before: Some(before),
                after,
            },
            warnings,
        });
    }
    let Some((source_path, _)) = source_block else {
        return Err(err(
            "key_not_found",
            format!(
                "neither `{locale}` nor the source locale `{}` defines `{key}`",
                project.source
            ),
        ));
    };
    let name = source_path.rsplit('/').next().unwrap_or("main.ftl");
    let path = format!("{LOCALIZATION_DIR}/{locale}/{name}");
    let before = target_files
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, t)| t.clone());
    let existing = before.clone().unwrap_or_default();
    let nl = newline_of(&existing);
    let mut after = existing.clone();
    if !after.is_empty() {
        if !after.ends_with('\n') {
            after.push_str(nl);
        }
        after.push_str(nl);
    }
    if let Some(h) = &hash {
        after.push_str(&format!("{SOURCE_HASH_PREFIX} {h}{nl}"));
    }
    after.push_str(&message_lines(key, &text, nl));
    Ok(PlannedSet {
        change: FileChange {
            path,
            locale: locale.to_owned(),
            before,
            after,
        },
        warnings,
    })
}

/// `text` with `block`'s value replaced by `value` (see [`plan_set`]); every other line keeps its bytes.
fn rewrite_block(text: &str, block: &FtlBlock, key: &str, value: &str, hash: Option<&str>) -> String {
    // `ftl_blocks` counts lines as `str::lines` does; `split_inclusive` keeps the terminators and has
    // the same count.
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let bare = |i: usize| lines[i].trim_end_matches('\n').trim_end_matches('\r');
    let (first, end) = block.span;
    let end = end.min(lines.len());
    let message = (first..end).find(|&i| !bare(i).starts_with('#')).unwrap_or(first);
    let value_end = (message + 1..end).find(|&i| is_attribute(bare(i))).unwrap_or(end);
    // The file's newline (its last line may have none).
    let nl = newline_of(text);
    let mut out = String::with_capacity(text.len() + value.len() + 64);
    for line in &lines[..first] {
        out.push_str(line);
    }
    let mut hash_written = false;
    for raw in &lines[first..message] {
        let line = raw.trim_end_matches('\n').trim_end_matches('\r');
        if line.trim() == PLACEHOLDER_MARKER {
            continue;
        }
        if line.starts_with(SOURCE_HASH_PREFIX) {
            if let Some(h) = hash {
                out.push_str(&format!("{SOURCE_HASH_PREFIX} {h}{nl}"));
                hash_written = true;
            }
            continue;
        }
        out.push_str(raw);
    }
    if let Some(h) = hash
        && !hash_written
    {
        out.push_str(&format!("{SOURCE_HASH_PREFIX} {h}{nl}"));
    }
    let mut replacement = message_lines(key, value, nl);
    // The file's last line had no terminator: keep it so.
    if value_end == lines.len() && !lines[value_end - 1].ends_with('\n') {
        replacement.truncate(replacement.len() - nl.len());
    }
    out.push_str(&replacement);
    for line in &lines[value_end..] {
        out.push_str(line);
    }
    out
}

/// Plans setting the translator context of `key` to `text` (ADR-7855): one `# note: <line>` comment line per
/// non-empty line of `text`, written directly above the message in the file of the **source** locale that
/// defines it, replacing the context comments there; the tool markers above it (`# source:`,
/// `# TODO translate`, `# regia-source:`, `##` group comments) and every other byte of the file stay. An
/// empty `text` removes the context.
///
/// # Errors
///
/// `loc_panel_bad_key`, `loc_panel_key_not_found` when the source locale does not define `key`.
pub fn plan_set_comment(project: &LocProject, key: &str, text: &str) -> Result<FileChange, PanelError> {
    if !is_message_id(key) {
        return Err(err("bad_key", format!("`{key}` is not a Fluent message id")));
    }
    let source_files = project.files(&project.source).unwrap_or(&[]);
    let Some((path, block)) = effective_blocks(source_files).remove(key) else {
        return Err(err(
            "key_not_found",
            format!("the source locale `{}` does not define `{key}`", project.source),
        ));
    };
    let before = source_files
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, t)| t.clone())
        .unwrap_or_default();
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let after = rewrite_comment(&before, &block, &lines);
    Ok(FileChange {
        path,
        locale: project.source.clone(),
        before: Some(before),
        after,
    })
}

/// `text` with the context comments above `block` replaced by `# note:` lines for `context`; tool markers
/// and every other line keep their bytes.
fn rewrite_comment(text: &str, block: &FtlBlock, context: &[&str]) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let bare = |i: usize| lines[i].trim_end_matches('\n').trim_end_matches('\r');
    let (first, end) = block.span;
    let end = end.min(lines.len());
    let message = (first..end).find(|&i| !bare(i).starts_with('#')).unwrap_or(first);
    let nl = newline_of(text);
    let mut out = String::with_capacity(text.len() + 64);
    for line in &lines[..first] {
        out.push_str(line);
    }
    for (i, raw) in lines.iter().enumerate().take(message).skip(first) {
        if loc_po::is_tool_comment(bare(i)) {
            out.push_str(raw);
        }
    }
    for line in context {
        out.push_str(&format!("# note: {line}{nl}"));
    }
    for line in &lines[message..] {
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_keeps_attributes_comments_and_crlf() {
        let text = "# hello\r\na = A\r\n\r\n# note: x\r\n# TODO translate\r\nb = B\r\n    more\r\n    .title = T\r\nc = C";
        let block = loc_po::ftl_blocks(text)
            .into_iter()
            .find(|b| b.key == "b")
            .expect("b");
        let out = rewrite_block(text, &block, "b", "New", Some("00ff"));
        assert_eq!(
            out,
            "# hello\r\na = A\r\n\r\n# note: x\r\n# regia-source: 00ff\r\nb = New\r\n    .title = T\r\nc = C"
        );
        let last = loc_po::ftl_blocks(text)
            .into_iter()
            .find(|b| b.key == "c")
            .expect("c");
        assert_eq!(
            rewrite_block(text, &last, "c", "one\ntwo", None),
            text.replace("c = C", "c =\r\n    one\r\n    two")
        );
    }

    #[test]
    fn filters_parse_by_name() {
        for f in Filter::ALL {
            assert_eq!(Filter::parse(f.as_str()).expect("known"), f);
        }
        assert_eq!(
            Filter::parse("nope").expect_err("unknown").code,
            "loc_panel_bad_filter"
        );
    }

    #[test]
    fn rewrite_comment_replaces_the_context_and_keeps_markers() {
        let text = "a = A\r\n\r\n# source: ui/a.ron:3\r\n# old line\r\n# note: old note\r\n# TODO translate\r\nb = B\r\n    .t = T\r\nc = C\r\n";
        let block = loc_po::ftl_blocks(text)
            .into_iter()
            .find(|b| b.key == "b")
            .expect("b");
        assert_eq!(block.context, ["old line", "old note"]);
        assert_eq!(
            rewrite_comment(text, &block, &["Shown in the pause menu", "Keep it short"]),
            "a = A\r\n\r\n# source: ui/a.ron:3\r\n# TODO translate\r\n# note: Shown in the pause menu\r\n# note: Keep it short\r\nb = B\r\n    .t = T\r\nc = C\r\n"
        );
        assert_eq!(
            rewrite_comment(text, &block, &[]),
            "a = A\r\n\r\n# source: ui/a.ron:3\r\n# TODO translate\r\nb = B\r\n    .t = T\r\nc = C\r\n"
        );
    }
}
