// SPDX-License-Identifier: MPL-2.0
//! `regia loc` (`docs/specs/content-data.md` § 1, ADR-1466): the localization workflow from the
//! command line.
//!
//! - `regia loc gather` finds the keys content and code use and appends the missing ones to each
//!   locale's `gathered.ftl` (`--dry-run` only prints the plan);
//! - `regia loc validate` (alias `check`) runs the validator (`--strict`: warnings fail too; exit 1 on
//!   failure); `--glyphs` also checks that the shipped fonts can draw every translated character;
//! - `regia loc pseudo` writes a pseudo locale (`en-XA` by default) from a source locale;
//! - `regia loc plural <locale> <number>...` prints the CLDR plural category of each number
//!   (`--type ordinal` for ordinal rules, ADR-3359);
//! - `regia loc status` reports per-locale counts (translated, missing, stale, mismatched) with
//!   `--fail-under` for CI (ADR-3359);
//! - `regia loc cues export|import` converts a `.subs.ron` subtitle cue track to WebVTT or TTML
//!   and back (spec 56 § 11, ADR-3359).
//!
//! Project subcommands take `--project` (default `.`). `plural` works without a project.
//! All subcommands take `--json` (machine output for agents and CI).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use regia_ui::loc_check::{self, Report};
use regia_ui::loc_config::LOCALIZATION_CONFIG_PATH;
use regia_ui::loc_gather::{self, GATHERED_FILE};
use regia_ui::loc_glyphs::{self, FontChain, FontCoverage, GlyphReport};
use regia_ui::{plural, pseudo};
use serde_json::{Value, json};

pub use regia_asset_pipeline::loc_exchange::{LOCALIZATION_DIR, LoadedLocale};

/// `regia loc` subcommands.
#[derive(Debug, Subcommand)]
pub enum LocCommand {
    /// Gather keys from `.ron` content and `loc!` calls in Rust into each locale's `gathered.ftl`.
    Gather(GatherArgs),
    /// Check every locale: missing, unused and extra keys, variables, plural variants (`--glyphs`: fonts too).
    #[command(alias = "check")]
    Validate(ValidateArgs),
    /// Write a pseudo-localized locale (accented, padded, bracketed) for layout testing.
    Pseudo(PseudoArgs),
    /// Print the CLDR plural category of numbers in a locale.
    Plural(PluralArgs),
    /// Report per-locale translation status: translated, missing, stale and placeholder-mismatched
    /// string counts (`--fail-under` fails the run for CI; ADR-3359).
    Status(StatusArgs),
    /// Lay out every `.ui.ron` text node with every locale and report overflow or clip.
    CheckFit(super::loc_check_fit::CheckFitArgs),
    /// Write a gettext PO file of a locale for a translator.
    Export(super::loc_exchange::ExportArgs),
    /// Read a translator's PO file back into `imported.ftl`.
    Import(super::loc_exchange::ImportArgs),
    /// Validate and write the shipping bundles (`loc/<locale>.ftl` and `loc/index.ron`).
    Compile(super::loc_exchange::CompileArgs),
    /// Write the voice-over script (CSV) of a locale for the recording studio.
    ExportVo(super::loc_vo::ExportVoArgs),
    /// Report which voiced lines have a clip per locale (ok, fallback, missing, orphan).
    VoReport(super::loc_vo::VoReportArgs),
    /// Validate the conversations: clips, reading speed, cue counts, labels, loops.
    ValidateConv(super::loc_vo::ValidateConvArgs),
    /// Write or read back the recording sheet (CSV) of the dialogue lines of a locale.
    #[command(subcommand)]
    DialogueSheet(super::loc_sheet::SheetCommand),
    /// WebVTT/TTML exchange for a subtitle cue track (`.subs.ron`).
    #[command(subcommand)]
    Cues(super::loc_cues::CuesCommand),
}

/// Arguments shared by the subcommands.
#[derive(Debug, Args)]
pub struct ProjectArgs {
    /// Project root (the directory holding `assets/`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Print JSON on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `regia loc gather`.
#[derive(Debug, Args)]
pub struct GatherArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// Print what would be written; write nothing.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Also write each locale's character set to `<DIR>/<locale>.txt` (for font subsetting tools).
    #[arg(long, value_name = "DIR")]
    pub charset_out: Option<PathBuf>,
}

/// `regia loc validate`.
#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// Warnings fail the check too.
    #[arg(long, default_value_t = false)]
    pub strict: bool,
    /// Do not gather: skip the unused-key check and the keys content uses.
    #[arg(long, default_value_t = false)]
    pub no_gather: bool,
    /// Also check that the shipped fonts (the project's `assets/fonts`, then the bundled set) cover every
    /// character of every translated message, the pseudo-locale included (`loc_glyph_missing`).
    #[arg(long, default_value_t = false)]
    pub glyphs: bool,
    /// An extra font file for the glyph check, tried before the bundled set (repeatable).
    #[arg(long, value_name = "FILE")]
    pub font: Vec<PathBuf>,
    /// Write each locale's character set to `<DIR>/<locale>.txt` (with `--glyphs`; the pseudo-locale too).
    #[arg(long, value_name = "DIR")]
    pub charset_out: Option<PathBuf>,
}

/// `regia loc pseudo`.
#[derive(Debug, Args)]
pub struct PseudoArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// The locale to transform (default: the configured source locale).
    #[arg(long)]
    pub from: Option<String>,
    /// The locale directory to write.
    #[arg(long, default_value = pseudo::PSEUDO_LOCALE)]
    pub to: String,
    /// Replace an existing locale that is not the default pseudo locale.
    #[arg(long)]
    pub force: bool,
}

/// Which CLDR plural rule family `regia loc plural` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PluralKindArg {
    /// Cardinal rules: "1 item, 2 items".
    Cardinal,
    /// Ordinal rules: "1st, 2nd, 3rd".
    Ordinal,
}

/// `regia loc plural`.
#[derive(Debug, Args)]
pub struct PluralArgs {
    /// The locale (`ru`, `fr-CA`).
    pub locale: String,
    /// Numbers as written (`1`, `21`, `1.50`).
    #[arg(required = true)]
    pub numbers: Vec<String>,
    /// Which CLDR rule family to use (ADR-3359).
    #[arg(long, value_enum, default_value = "cardinal")]
    pub kind: PluralKindArg,
    /// Print JSON on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `regia loc status`.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// Fail (exit 1) when the lowest translated percentage of any locale is under this value.
    #[arg(long, value_name = "PERCENT")]
    pub fail_under: Option<f64>,
}

/// Runs a `regia loc` subcommand. Returns `false` when a check failed (the caller exits 1).
///
/// # Errors
///
/// The project's localization files or config cannot be read or written.
pub fn run(command: &LocCommand) -> Result<bool> {
    match command {
        LocCommand::Gather(args) => gather(args),
        LocCommand::Validate(args) => validate(args),
        LocCommand::Pseudo(args) => pseudo_locale(args),
        LocCommand::Plural(args) => plural_categories(args),
        LocCommand::Status(args) => status(args),
        LocCommand::CheckFit(args) => super::loc_check_fit::check_fit(args),
        LocCommand::Export(args) => super::loc_exchange::export(args).map(|_| true),
        LocCommand::Import(args) => super::loc_exchange::import(args),
        LocCommand::Compile(args) => super::loc_exchange::compile(args),
        LocCommand::ExportVo(args) => super::loc_vo::export_vo(args),
        LocCommand::VoReport(args) => super::loc_vo::vo_report(args),
        LocCommand::ValidateConv(args) => super::loc_vo::validate_conv(args),
        LocCommand::DialogueSheet(command) => super::loc_sheet::run(command),
        LocCommand::Cues(command) => super::loc_cues::run(command),
    }
}

// `read_config` and `read_locales` moved to `regia_asset_pipeline::loc_exchange` (ADR-7677) so the editor's
// Localization panel reads a project exactly as `regia loc` does.
pub use regia_asset_pipeline::loc_exchange::{read_config, read_locales};

fn gather(args: &GatherArgs) -> Result<bool> {
    let project = &args.common.project;
    let config = read_config(project)?;
    let mut gathered = loc_gather::gather_project(project, &config.gather)?;
    add_conversation_keys(project, &config.source, &mut gathered)?;
    super::loc_sheet::add_dialogue_keys(project, &mut gathered)?;
    let locales = read_locales(project, &config)?;
    if locales.is_empty() {
        bail!(
            "no locales: create {LOCALIZATION_DIR}/<locale>/ or list `locales` in {LOCALIZATION_CONFIG_PATH}"
        );
    }
    let mut existing = BTreeMap::new();
    for (parsed, files) in &locales {
        let gathered_rel = format!("{LOCALIZATION_DIR}/{}/{GATHERED_FILE}", parsed.locale);
        if let Some((_, text)) = files.iter().find(|(rel, _)| *rel == gathered_rel) {
            existing.insert(parsed.locale.clone(), text.clone());
        }
    }
    let plan = loc_gather::plan(&gathered, &locales, &config.source, LOCALIZATION_DIR, &existing);
    if !args.dry_run {
        for file in &plan {
            loc_gather::write_planned(project, file).with_context(|| format!("writing {}", file.path))?;
        }
    }
    let charsets = match &args.charset_out {
        Some(dir) => {
            let files: Vec<(String, Vec<(String, String)>)> = locales
                .iter()
                .map(|(p, f)| (p.locale.clone(), f.clone()))
                .collect();
            let report = loc_glyphs::check_glyphs(&files, &config.source, &FontChain::default(), false);
            write_charsets(dir, &report, args.dry_run)?
        }
        None => Vec::new(),
    };
    if args.common.json {
        let out = json!({
            "files_scanned": gathered.files_scanned,
            "keys": gathered.keys.keys().collect::<Vec<_>>(),
            "bad_keys": gathered.bad_keys.iter().map(|(at, key)| json!({ "file": at.file, "line": at.line, "text": key })).collect::<Vec<_>>(),
            "written": !args.dry_run,
            "files": plan.iter().map(|f| json!({ "locale": f.locale, "path": f.path, "added": f.added })).collect::<Vec<_>>(),
            "charsets": charsets,
        });
        println!("{out}");
    } else {
        println!(
            "gathered {} key(s) from {} file(s)",
            gathered.keys.len(),
            gathered.files_scanned
        );
        for (at, key) in &gathered.bad_keys {
            println!("  not a key: {key:?} at {}:{}", at.file, at.line);
        }
        let verb = if args.dry_run { "would add" } else { "added" };
        for file in &plan {
            println!(
                "{}: {verb} {} key(s) to {}",
                file.locale,
                file.added.len(),
                file.path
            );
        }
        if plan.is_empty() {
            println!("every locale has every key");
        }
    }
    Ok(gathered.bad_keys.is_empty())
}

/// Adds the text and sub-cue keys of `assets/conversations/*.conv.ron` to a gather, with the cast
/// note, speaker, emotion and character budget as comments (spec 56 § 6).
///
/// # Errors
///
/// A conversation file does not parse.
pub fn add_conversation_keys(
    project: &Path,
    source: &str,
    gathered: &mut loc_gather::Gathered,
) -> Result<()> {
    let assets = project.join("assets");
    if !assets.join("conversations").is_dir() {
        return Ok(());
    }
    let library = regia::framework::conversation::ConversationLibrary::load_assets(&assets)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    for asset in library.conversations.values() {
        let rel = format!("assets/conversations/{}.conv.ron", asset.id);
        let text = std::fs::read_to_string(project.join(&rel)).unwrap_or_default();
        gathered.files_scanned += 1;
        for key in regia::framework::conversation::gather::gather_keys(asset, &text, &library, source) {
            gathered.add_key(&key.key, &rel, key.line_no, key.notes);
        }
    }
    Ok(())
}

// `read_glossary` moved to `regia_asset_pipeline::loc_exchange` (ADR-7855), shared with the editor's Compile.

/// Validates the project's locales (`used`: run the gather tool first).
///
/// # Errors
///
/// The files cannot be read.
pub fn validate_project(project: &Path, used: bool) -> Result<Report> {
    let config = read_config(project)?;
    let locales = read_locales(project, &config)?;
    let used_keys = if used {
        let mut gathered = loc_gather::gather_project(project, &config.gather)?;
        add_conversation_keys(project, &config.source, &mut gathered)?;
        super::loc_sheet::add_dialogue_keys(project, &mut gathered)?;
        Some(gathered.key_set())
    } else {
        None
    };
    regia_asset_pipeline::loc_exchange::validate_locales(project, &config, locales, used_keys)
}

/// A report as JSON (the shape `regia loc validate --json` prints).
#[must_use]
pub fn report_json(report: &Report, strict: bool) -> Value {
    json!({
        "ok": !report.fails(strict),
        "errors": report.count(loc_check::Severity::Error),
        "warnings": report.count(loc_check::Severity::Warning),
        "locales": report.locales.iter().map(|l| json!({
            "locale": l.locale, "keys": l.keys, "missing": l.missing, "untranslated": l.untranslated,
            "extra": l.extra, "chain": l.chain,
        })).collect::<Vec<_>>(),
        "findings": report.findings.iter().map(|f| json!({
            "code": f.code, "severity": f.severity.as_str(), "locale": f.locale, "key": f.key,
            "file": f.file, "line": f.line, "message": f.message,
        })).collect::<Vec<_>>(),
    })
}

/// Every `.ttf`, `.otf`, `.ttc` and `.otc` file under `dir`, sorted by path, to `depth` levels.
fn font_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<()> {
    if depth == 0 || !dir.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("listing {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            font_files(&path, depth - 1, out)?;
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| ["ttf", "otf", "ttc", "otc"].contains(&e.to_ascii_lowercase().as_str()))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// The font chain the game ships: the project's `assets/fonts` files in path order, then `extra`, then
/// the bundled default set (ADR-2550).
///
/// # Errors
///
/// A font file cannot be read or is not a font.
pub fn font_chain(project: &Path, extra: &[PathBuf]) -> Result<FontChain> {
    let mut paths = Vec::new();
    font_files(&project.join("assets/fonts"), 4, &mut paths)?;
    paths.extend(extra.iter().cloned());
    let mut chain = FontChain::default();
    for path in paths {
        let data = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let Some(font) = FontCoverage::from_font_data(name, &data) else {
            bail!("{} is not a font file", path.display());
        };
        chain.push(font);
    }
    chain.fonts.extend(FontChain::embedded().fonts);
    Ok(chain)
}

/// The glyph coverage of the project's locales against its font chain, the pseudo-locale included.
///
/// # Errors
///
/// The localization files or a font cannot be read.
pub fn glyph_report(project: &Path, extra_fonts: &[PathBuf]) -> Result<(GlyphReport, FontChain)> {
    let config = read_config(project)?;
    let locales = read_locales(project, &config)?;
    let chain = font_chain(project, extra_fonts)?;
    let files: Vec<(String, Vec<(String, String)>)> =
        locales.into_iter().map(|(p, f)| (p.locale, f)).collect();
    let source = if config.source.is_empty() {
        "en"
    } else {
        config.source.as_str()
    };
    Ok((loc_glyphs::check_glyphs(&files, source, &chain, true), chain))
}

/// Write `<dir>/<locale>.txt` for every locale of `report` (unless `dry_run`); returns the file names.
fn write_charsets(dir: &Path, report: &GlyphReport, dry_run: bool) -> Result<Vec<String>> {
    let mut written = Vec::new();
    for (locale, chars) in &report.charsets {
        let path = dir.join(format!("{locale}.txt"));
        if !dry_run {
            super::loc_exchange::write_atomic(&path, &loc_glyphs::charset_text(chars))?;
        }
        written.push(path.display().to_string());
    }
    Ok(written)
}

fn validate(args: &ValidateArgs) -> Result<bool> {
    let mut report = validate_project(&args.common.project, !args.no_gather)?;
    let mut glyphs = None;
    if args.glyphs {
        let (found, chain) = glyph_report(&args.common.project, &args.font)?;
        report.findings.extend(found.findings.iter().cloned());
        let written = match &args.charset_out {
            Some(dir) => write_charsets(dir, &found, false)?,
            None => Vec::new(),
        };
        glyphs = Some((found, chain, written));
    } else if args.charset_out.is_some() || !args.font.is_empty() {
        bail!("--font and --charset-out belong to --glyphs");
    }
    let ok = !report.fails(args.strict);
    if args.common.json {
        let mut out = report_json(&report, args.strict);
        if let (Some((found, chain, written)), Some(map)) = (&glyphs, out.as_object_mut()) {
            map.insert(
                "glyphs".to_owned(),
                json!({
                    "fonts": chain.names(),
                    "missing": found.findings.len(),
                    "characters": found.charsets.iter().map(|(l, c)| (l.clone(), c.len())).collect::<BTreeMap<_, _>>(),
                    "charset_files": written,
                }),
            );
        }
        println!("{out}");
    } else {
        for f in &report.findings {
            let place = match (&f.file, f.line) {
                (Some(file), Some(line)) => format!(" {file}:{line}"),
                (Some(file), None) => format!(" {file}"),
                _ => String::new(),
            };
            println!(
                "{} {} [{}]{place}: {}",
                f.severity.as_str(),
                f.code,
                f.locale,
                f.message
            );
        }
        println!(
            "{} error(s), {} warning(s) in {} locale(s){}",
            report.count(loc_check::Severity::Error),
            report.count(loc_check::Severity::Warning),
            report.locales.len(),
            if ok { "" } else { ": FAILED" }
        );
        if let Some((found, chain, written)) = &glyphs {
            println!(
                "glyphs: {} text(s) with characters no font covers; chain: {}",
                found.findings.len(),
                chain.names().join(", ")
            );
            for path in written {
                println!("wrote {path}");
            }
        }
    }
    Ok(ok)
}

fn pseudo_locale(args: &PseudoArgs) -> Result<bool> {
    let project = &args.common.project;
    let config = read_config(project)?;
    let from = args.from.clone().unwrap_or_else(|| config.source.clone());
    if !is_locale_tag(&args.to) {
        bail!("`{}` is not a locale tag", args.to);
    }
    let locales = read_locales(project, &config)?;
    let Some((_, files)) = locales.iter().find(|(p, _)| p.locale == from) else {
        bail!("no locale `{from}` under {LOCALIZATION_DIR}");
    };
    // The write replaces each file in place with no backup, so real translations need a yes.
    if args.to == from {
        bail!("`--to {}` is the locale being transformed", args.to);
    }
    let replaces_a_locale =
        locales.iter().any(|(p, f)| p.locale == args.to && !f.is_empty()) && args.to != pseudo::PSEUDO_LOCALE;
    if replaces_a_locale && !args.force {
        bail!(
            "{LOCALIZATION_DIR}/{} already holds translations; `--force` replaces them",
            args.to
        );
    }
    let mut written = Vec::new();
    for (rel, text) in files {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let out = pseudo::pseudo_ftl(text).map_err(|e| anyhow::anyhow!("{rel}: {e}"))?;
        let planned = loc_gather::PlannedFile {
            locale: args.to.clone(),
            path: format!("{LOCALIZATION_DIR}/{}/{name}", args.to),
            added: Vec::new(),
            text: out,
        };
        loc_gather::write_planned(project, &planned).with_context(|| format!("writing {}", planned.path))?;
        written.push(planned.path);
    }
    if args.common.json {
        println!("{}", json!({ "from": from, "to": args.to, "files": written }));
    } else {
        for path in &written {
            println!("wrote {path}");
        }
    }
    Ok(true)
}

/// A minimal locale-tag check (letters, digits and `-`, starting with a letter) for the pseudo
/// target; the runtime parses the full tag when it loads the directory.
fn is_locale_tag(tag: &str) -> bool {
    tag.starts_with(|c: char| c.is_ascii_alphabetic())
        && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !tag.ends_with('-')
}

fn plural_categories(args: &PluralArgs) -> Result<bool> {
    let kind = match args.kind {
        PluralKindArg::Cardinal => plural::PluralKind::Cardinal,
        PluralKindArg::Ordinal => plural::PluralKind::Ordinal,
    };
    let Some(categories) = plural::categories_of_kind(&args.locale, kind) else {
        bail!("the plural table does not know `{}`", args.locale);
    };
    let mut rows = Vec::new();
    for number in &args.numbers {
        let Some(operands) = plural::PluralOperands::from_decimal(number) else {
            bail!("`{number}` is not a plain decimal number");
        };
        rows.push((
            number.clone(),
            plural::category_of_kind(&args.locale, kind, &operands).as_str(),
        ));
    }
    if args.json {
        let out = json!({
            "locale": args.locale,
            "kind": match kind { plural::PluralKind::Cardinal => "cardinal", plural::PluralKind::Ordinal => "ordinal" },
            "categories": categories.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
            "numbers": rows.iter().map(|(n, c)| json!({ "number": n, "category": c })).collect::<Vec<_>>(),
        });
        println!("{out}");
    } else {
        let names: Vec<&str> = categories.iter().map(|c| c.as_str()).collect();
        println!(
            "{} ({}): {}",
            args.locale,
            match kind {
                plural::PluralKind::Cardinal => "cardinal",
                plural::PluralKind::Ordinal => "ordinal",
            },
            names.join(", ")
        );
        for (n, c) in rows {
            println!("  {n} -> {c}");
        }
    }
    Ok(true)
}

/// The status rows of a project as JSON (the shape `regia loc status --json` prints).
#[must_use]
pub fn status_json(rows: &[regia_ui::loc_status::LocaleStatus]) -> Value {
    let min_percent = rows
        .iter()
        .map(|r| r.percent)
        .min_by(|a, b| a.total_cmp(b))
        .unwrap_or(100.0);
    json!({
        "locales": rows.iter().map(|r| json!({
            "locale": r.locale,
            "chain": r.chain,
            "messages": r.messages,
            "translated": r.translated,
            "untranslated": r.untranslated,
            "placeholders": r.placeholders,
            "missing": r.missing,
            "stale": r.stale,
            "mismatched": r.mismatched,
            "percent": r.percent,
        })).collect::<Vec<_>>(),
        "min_percent": min_percent,
    })
}

/// The per-locale status rows of a project: reads the config and locales, runs the gather scan
/// so `missing` covers keys content and code use, and counts each locale (ADR-3359).
///
/// # Errors
///
/// The localization files or config cannot be read.
pub fn project_status(project: &Path) -> Result<Vec<regia_ui::loc_status::LocaleStatus>> {
    let config = read_config(project)?;
    let locales = read_locales(project, &config)?;
    if locales.is_empty() {
        bail!(
            "no locales: create {LOCALIZATION_DIR}/<locale>/ or list `locales` in {LOCALIZATION_CONFIG_PATH}"
        );
    }
    let source = if config.source.is_empty() {
        "en"
    } else {
        config.source.as_str()
    };
    let source_files = locales
        .iter()
        .find(|(p, _)| p.locale == source)
        .map_or_else(Vec::new, |(_, f)| f.clone());
    // The same gather the validator runs, so `missing` means "a player sees the missing-key
    // marker", including keys content uses that no locale defines.
    let mut gathered = loc_gather::gather_project(project, &config.gather)?;
    add_conversation_keys(project, source, &mut gathered)?;
    super::loc_sheet::add_dialogue_keys(project, &mut gathered)?;
    Ok(regia_ui::loc_status::status(
        source,
        &source_files,
        &locales,
        &config.fallbacks,
        Some(&gathered.key_set()),
    ))
}

fn status(args: &StatusArgs) -> Result<bool> {
    if let Some(p) = args.fail_under
        && !(0.0..=100.0).contains(&p)
    {
        bail!("--fail-under must be between 0 and 100, got {p}");
    }
    let rows = project_status(&args.common.project)?;
    let min_percent = rows
        .iter()
        .map(|r| r.percent)
        .min_by(|a, b| a.total_cmp(b))
        .unwrap_or(100.0);
    let under = args.fail_under.is_some_and(|p| min_percent < p);
    if args.common.json {
        let mut out = status_json(&rows);
        if let Some(p) = args.fail_under
            && let Some(map) = out.as_object_mut()
        {
            map.insert("fail_under".to_owned(), json!(p));
            map.insert("ok".to_owned(), json!(!under));
        }
        println!("{out}");
    } else {
        for r in &rows {
            println!(
                "{:8} {:6.2}%  {}/{} translated  {} untranslated ({} placeholders)  {} missing  {} stale  {} mismatched",
                r.locale,
                r.percent,
                r.translated,
                r.messages,
                r.untranslated,
                r.placeholders,
                r.missing,
                r.stale,
                r.mismatched,
            );
        }
        match args.fail_under {
            Some(p) if under => println!("min translated percent {min_percent:.2} is under {p}: FAILED"),
            Some(p) => println!("min translated percent {min_percent:.2} >= {p}: ok"),
            None => {}
        }
    }
    Ok(!under)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn validate_project_reads_config_locales_and_gathers() {
        let root = regia_test_utils::unique_temp_dir("cli-loc");
        write(
            &root,
            LOCALIZATION_CONFIG_PATH,
            "(format_version: 1, source: \"en\", locales: [\"en\", \"de\"])",
        );
        write(
            &root,
            "assets/localization/en/main.ftl",
            "menu-start = Start\nold = Old\n",
        );
        write(
            &root,
            "assets/ui/menu.ui.ron",
            "(text: (spans: [Loc(\"menu-start\"), Loc(\"menu-quit\")]))\n",
        );
        let report = validate_project(&root, true).expect("validate");
        let codes: std::collections::BTreeSet<&str> = report.findings.iter().map(|f| f.code).collect();
        assert!(codes.contains("loc_missing_key"), "{codes:?}");
        assert!(codes.contains("loc_unused_key"), "{codes:?}");
        // `de` is configured but has no directory: every key is untranslated there.
        assert_eq!(
            report
                .locales
                .iter()
                .map(|l| l.locale.as_str())
                .collect::<Vec<_>>(),
            vec!["de", "en"]
        );
        let out = report_json(&report, false);
        assert_eq!(out["ok"], json!(false));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn gather_then_validate_is_clean_apart_from_untranslated() {
        let root = regia_test_utils::unique_temp_dir("cli-loc-gather");
        write(&root, "assets/localization/en/main.ftl", "menu-start = Start\n");
        write(
            &root,
            "assets/localization/fr/main.ftl",
            "menu-start = Commencer\n",
        );
        write(
            &root,
            "src/main.rs",
            "fn main() { let _ = regia_ui::loc!(\"menu-quit\"); }\n",
        );
        write(
            &root,
            "assets/ui/menu.ui.ron",
            "(text: (spans: [Loc(\"menu-start\")]))\n",
        );
        let ok = run(&LocCommand::Gather(GatherArgs {
            common: ProjectArgs {
                project: root.clone(),
                json: true,
            },
            dry_run: false,
            charset_out: None,
        }))
        .expect("gather");
        assert!(ok);
        let report = validate_project(&root, true).expect("validate");
        assert!(!report.fails(false), "{:#?}", report.findings);
        let pseudo = run(&LocCommand::Pseudo(PseudoArgs {
            common: ProjectArgs {
                project: root.clone(),
                json: true,
            },
            from: None,
            to: pseudo::PSEUDO_LOCALE.to_owned(),
            force: false,
        }))
        .expect("pseudo");
        assert!(pseudo);
        // Levels audit M4: the write has no backup, so it never lands on real translations.
        let pseudo_to = |to: &str, force: bool| {
            run(&LocCommand::Pseudo(PseudoArgs {
                common: ProjectArgs {
                    project: root.clone(),
                    json: true,
                },
                from: None,
                to: to.to_owned(),
                force,
            }))
        };
        let french = std::fs::read_to_string(root.join("assets/localization/fr/main.ftl")).expect("fr");
        assert!(
            pseudo_to("en", false)
                .expect_err("the source")
                .to_string()
                .contains("transformed")
        );
        assert!(
            pseudo_to("en", true).is_err(),
            "`--force` never replaces the source"
        );
        assert!(
            pseudo_to("fr", false)
                .expect_err("fr")
                .to_string()
                .contains("--force")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("assets/localization/fr/main.ftl")).expect("fr"),
            french,
            "refused before writing"
        );
        assert!(
            pseudo_to(pseudo::PSEUDO_LOCALE, false).is_ok(),
            "the pseudo locale can be redone"
        );
        assert!(pseudo_to("fr", true).is_ok());
        let text = std::fs::read_to_string(root.join("assets/localization/en-XA/gathered.ftl"))
            .expect("pseudo file");
        assert!(text.contains("menu-quit"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plural_command_rejects_unknown_locales_and_bad_numbers() {
        let args = |locale: &str, n: &str| PluralArgs {
            locale: locale.to_owned(),
            numbers: vec![n.to_owned()],
            kind: PluralKindArg::Cardinal,
            json: true,
        };
        assert!(plural_categories(&args("ru", "21")).expect("ru"));
        assert!(plural_categories(&args("tlh", "1")).is_err());
        assert!(plural_categories(&args("ru", "1e3")).is_err());
    }

    /// ADR-3359: `--type ordinal` answers from the CLDR ordinal table.
    #[test]
    fn plural_command_prints_ordinal_categories() {
        let args = PluralArgs {
            locale: "en".to_owned(),
            numbers: vec!["1".to_owned(), "2".to_owned(), "3".to_owned(), "11".to_owned()],
            kind: PluralKindArg::Ordinal,
            json: true,
        };
        assert!(plural_categories(&args).expect("ordinal"));
        let args = PluralArgs {
            locale: "tlh".to_owned(),
            numbers: vec!["1".to_owned()],
            kind: PluralKindArg::Ordinal,
            json: true,
        };
        assert!(plural_categories(&args).is_err(), "unknown locale");
    }

    /// ADR-3359: `regia loc status` counts translated/missing/stale/mismatched per locale and
    /// `--fail-under` gives exit 1 through the documented `false` return.
    #[test]
    fn status_counts_and_fail_under() {
        let root = regia_test_utils::unique_temp_dir("cli-loc-status");
        write(
            &root,
            LOCALIZATION_CONFIG_PATH,
            "(format_version: 1, source: \"en\", locales: [\"en\", \"de\", \"fr\"])",
        );
        write(
            &root,
            "assets/localization/en/main.ftl",
            "menu-start = Start\nmenu-quit = Quit\nhud-score = Score: { $score }\n",
        );
        // de: translated 2 of 3, one placeholder, one variable mismatch (score dropped).
        write(
            &root,
            "assets/localization/de/main.ftl",
            "# TODO translate\nmenu-start = Start\nmenu-quit = Beenden\nhud-score = Punkte\n",
        );
        // fr: all three translated, hud-score recorded against an older source (stale).
        let old = loc_po_hash("Score: { $points }");
        write(
            &root,
            "assets/localization/fr/main.ftl",
            &format!(
                "menu-start = Démarrer\nmenu-quit = Quitter\n# regia-source: {old}\nhud-score = Points : {{ $points }}\n"
            ),
        );
        // Content uses a key no locale defines: it counts as missing everywhere.
        write(
            &root,
            "src/main.rs",
            "fn main() { let _ = regia_ui::loc!(\"hud-missing\"); }\n",
        );
        let args = |fail_under, json| StatusArgs {
            common: ProjectArgs {
                project: root.clone(),
                json,
            },
            fail_under,
        };
        let ok = run(&LocCommand::Status(args(None, true))).expect("status");
        assert!(ok);
        let rows = project_status(&root).expect("rows");
        let de = rows.iter().find(|r| r.locale == "de").expect("de");
        assert_eq!(
            (de.messages, de.translated, de.missing, de.stale, de.mismatched),
            (3, 2, 1, 0, 1)
        );
        let fr = rows.iter().find(|r| r.locale == "fr").expect("fr");
        assert_eq!((fr.translated, fr.stale, fr.mismatched, fr.missing), (3, 1, 1, 1));
        // --fail-under above de's 66.67% fails; at or below it passes.
        let ok = run(&LocCommand::Status(args(Some(80.0), true))).expect("run");
        assert!(!ok, "under 80 fails");
        assert!(run(&LocCommand::Status(args(Some(60.0), true))).expect("60 passes"));
        assert!(
            run(&LocCommand::Status(args(Some(101.0), true))).is_err(),
            "out of range is refused"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn loc_po_hash(text: &str) -> String {
        // The FNV-1a hash imported.ftl records; recomputed here to keep the fixture readable.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in text.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{h:016x}")
    }
    #[test]
    fn glyph_check_reports_the_message_and_line_writes_charsets_and_fails_validation() {
        let root = regia_test_utils::unique_temp_dir("cli-loc-glyphs");
        write(&root, "assets/localization/en/main.ftl", "greet = Hello\n");
        // U+A66E is in no bundled font; it sits in the `other` variant on line 5.
        write(
            &root,
            "assets/localization/fr/main.ftl",
            "greet = Salut\n\ncount = { $n ->\n    [one] un\n   *[other] \u{A66E}\n}\n",
        );
        let (report, chain) = glyph_report(&root, &[]).expect("report");
        let missing: Vec<_> = report.findings.iter().filter(|f| f.locale == "fr").collect();
        assert_eq!(missing.len(), 1, "{:?}", report.findings);
        assert_eq!(missing[0].code, "loc_glyph_missing");
        assert_eq!(missing[0].key.as_deref(), Some("count"));
        assert_eq!(missing[0].line, Some(5));
        assert!(
            missing[0]
                .file
                .as_deref()
                .is_some_and(|f| f.ends_with("fr/main.ftl"))
        );
        assert!(chain.names().iter().any(|n| n == "Inter-Regular.ttf"));
        assert!(
            report.charsets.contains_key("en-XA"),
            "the pseudo-locale is scanned"
        );
        // The validator fails on it, and writes the character sets on request.
        let sets = root.join("sets");
        let args = |glyphs| ValidateArgs {
            common: ProjectArgs {
                project: root.clone(),
                json: false,
            },
            strict: false,
            no_gather: true,
            glyphs,
            font: Vec::new(),
            charset_out: glyphs.then(|| sets.clone()),
        };
        assert!(validate(&args(false)).expect("plain validate"), "off by default");
        assert!(!validate(&args(true)).expect("glyph validate"));
        let fr = std::fs::read_to_string(sets.join("fr.txt")).expect("fr charset");
        assert!(
            fr.contains('\u{A66E}') && fr.contains('S') && fr.ends_with('\n'),
            "{fr:?}"
        );
        assert!(sets.join("en-XA.txt").is_file());
        // `--font` puts a file ahead of the bundled set; a non-font is refused.
        let inter = regia_ui::text::embedded_font_data()[0].1;
        std::fs::write(root.join("extra.ttf"), inter).expect("font");
        let (_, chain) = glyph_report(&root, &[root.join("extra.ttf")]).expect("with font");
        assert_eq!(chain.names().first().map(String::as_str), Some("extra.ttf"));
        std::fs::write(root.join("bad.ttf"), b"nope").expect("junk");
        assert!(glyph_report(&root, &[root.join("bad.ttf")]).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
