// SPDX-License-Identifier: MPL-2.0
//! `regia loc export` and `regia loc import`: translator exchange through gettext PO files or XLIFF 2.0
//! (`docs/specs/36-localization-exchange-dialogue-subtitles.md` § 3, ADR-1668, ADR-2550).
//!
//! The pure work (PO text, entry layout, stale detection, translation checks) lives in
//! `regia_ui::loc_po`; the project-level export and import plan (reading the project's `.ftl` files,
//! writing `assets/localization/<locale>/imported.ftl`) live in `regia_asset_pipeline::loc_exchange`
//! (ADR-7677), which the editor's Localization panel calls too. This module parses the arguments,
//! writes the PO or XLIFF file and prints the report. Every write is atomic.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use regia_asset_pipeline::loc_exchange::{self as exchange, ExchangeFile, ExchangeFormat};
#[cfg(test)]
use regia_ui::loc_bundle::BundleIndex;
use regia_ui::loc_glossary;
use regia_ui::loc_po::{self, ExportFilter, ImportContext, ImportPlan, PoFile};
use serde_json::json;

use super::loc::ProjectArgs;
#[cfg(test)]
use regia_asset_pipeline::loc_exchange::is_locale_tag;

/// Which entries `export` writes.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OnlyArg {
    /// Every source message.
    All,
    /// Messages the locale has no real translation for.
    Untranslated,
    /// Messages whose translation was made against an older source text.
    Stale,
}

/// The file format of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    /// gettext PO (`<locale>.po`).
    Po,
    /// XLIFF 2.0 (`<locale>.xlf`), placeholders as protected inline codes.
    Xliff,
}

/// `regia loc export`.
#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// The locale to export for.
    #[arg(long)]
    pub locale: String,
    /// The file to write (default `<project>/.regia/loc/<locale>.po`, or `.xlf` for XLIFF).
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// The exchange format.
    #[arg(long, value_enum, default_value = "po")]
    pub format: FormatArg,
    /// Which entries to write.
    #[arg(long, value_enum, default_value = "all")]
    pub only: OnlyArg,
}

/// `regia loc import`.
#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// The PO or XLIFF file a translator returned (the format is detected from the content).
    pub file: PathBuf,
    /// The locale (default: the file's `Language`).
    #[arg(long)]
    pub locale: Option<String>,
    /// Print the report; write nothing.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Stale and fuzzy entries fail the import (exit 1).
    #[arg(long, default_value_t = false)]
    pub strict: bool,
}

/// `regia loc compile`.
#[derive(Debug, Args)]
pub struct CompileArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// Locales to ship (default: `locales` of `localization.ron`, else every locale found).
    #[arg(long, value_delimiter = ',')]
    pub locales: Vec<String>,
    /// Output directory; bundles go to `<out>/loc/` (default `<project>/build`).
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// Warnings fail the validation step too.
    #[arg(long, default_value_t = false)]
    pub strict: bool,
}

/// `regia_asset_pipeline::loc_exchange::write_atomic` (moved to the library with the exchange, ADR-7677).
pub(super) fn write_atomic(path: &Path, text: &str) -> Result<()> {
    exchange::write_atomic(path, text)
}

/// Validate the project's locales and write the bundles to `<out>/loc/` (`regia loc compile` and
/// `regia package`): the `requested` locales (default: `locales` of `localization.ron`, else every locale
/// found) plus their fallback chains and the source. `None` when validation failed (nothing written);
/// else the bundle directory and the shipped locales.
///
/// # Errors
///
/// The localization files cannot be read, the source locale has no files, or a bundle cannot be written.
pub fn write_bundles(
    project: &Path,
    out: &Path,
    requested: &[String],
    strict: bool,
) -> Result<Option<(PathBuf, Vec<String>)>> {
    Ok(regia_asset_pipeline::loc_exchange::compile_bundles(project, out, requested, strict)?.written)
}

/// Validate the project's locales and write the shipping bundles (spec 36 § 4): `loc/<locale>.ftl` per
/// shipped locale and `loc/index.ron`. Returns `false` when validation found errors (nothing written).
///
/// # Errors
///
/// The localization files cannot be read, the source locale has no files, or a bundle cannot be written.
pub fn compile(args: &CompileArgs) -> Result<bool> {
    let project = &args.common.project;
    let out = args.out.clone().unwrap_or_else(|| project.join("build"));
    let done =
        regia_asset_pipeline::loc_exchange::compile_bundles(project, &out, &args.locales, args.strict)?;
    let Some((out_dir, ship)) = done.written else {
        if args.common.json {
            let mut value = super::loc::report_json(&done.report, args.strict);
            value["reason"] = json!("validation failed");
            println!("{value}");
        } else {
            println!("validation failed; nothing was written:");
            for f in done
                .report
                .findings
                .iter()
                .filter(|f| f.severity != regia_ui::loc_check::Severity::Info)
            {
                let at = match (&f.file, f.line) {
                    (Some(file), Some(line)) => format!("{file}:{line}"),
                    (Some(file), None) => file.clone(),
                    (None, _) => f.locale.clone(),
                };
                println!("  {} {} {at}: {}", f.severity.as_str(), f.code, f.message);
            }
        }
        return Ok(false);
    };
    if args.common.json {
        println!(
            "{}",
            json!({ "ok": true, "dir": out_dir.display().to_string(), "locales": ship })
        );
    } else {
        println!(
            "wrote {} bundle(s) to {}: {}",
            ship.len(),
            out_dir.display(),
            ship.join(", ")
        );
    }
    Ok(true)
}

/// Write the PO file of `args.locale`; returns the number of entries.
///
/// # Errors
///
/// The project's localization cannot be read, the locale or the source locale is unknown, or the
/// file cannot be written.
pub fn export(args: &ExportArgs) -> Result<usize> {
    let project = &args.common.project;
    let format = match args.format {
        FormatArg::Po => ExchangeFormat::Po,
        FormatArg::Xliff => ExchangeFormat::Xliff,
    };
    let filter = match args.only {
        OnlyArg::All => ExportFilter::All,
        OnlyArg::Untranslated => ExportFilter::Untranslated,
        OnlyArg::Stale => ExportFilter::Stale,
    };
    let output = exchange::export(project, &args.locale, format, filter)?;
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| exchange::default_export_path(project, &args.locale, format));
    write_atomic(&out, &output.text)?;
    let n = output.entries;
    let (tm_pre_filled, tm_near) = (output.tm_pre_filled, output.tm_near);
    if args.common.json {
        println!(
            "{}",
            json!({ "file": out.display().to_string(), "entries": n, "skipped_attributes": output.skipped_attributes, "skipped_terms": output.skipped_terms, "tm_pre_filled": tm_pre_filled, "tm_near": tm_near })
        );
    } else {
        println!("wrote {n} entries to {}", out.display());
        if tm_pre_filled > 0 || tm_near > 0 {
            println!(
                "translation memory: {tm_pre_filled} pre-filled (flagged {review}), {tm_near} near-match note(s)",
                review = loc_glossary::TM_REVIEW_FLAG,
            );
        }
        if output.skipped_attributes > 0 {
            println!(
                "note: {} message(s) have attributes; attributes are not exported",
                output.skipped_attributes
            );
        }
        if output.skipped_terms > 0 {
            println!(
                "note: {} term(s) (-name = ...) are not exported; translate terms in the .ftl files",
                output.skipped_terms
            );
        }
    }
    Ok(n)
}

/// Import a returned PO or XLIFF file. Returns `false` when the import failed (refused file, or `--strict`
/// with stale or fuzzy entries).
///
/// # Errors
///
/// The PO file or the project's localization cannot be read, or a result cannot be written.
pub fn import(args: &ImportArgs) -> Result<bool> {
    let text =
        std::fs::read_to_string(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let file = ExchangeFile::parse(&text, &args.file.display().to_string())?;
    if let ExchangeFile::Po(po) = &file {
        return import_po(&args.common, po, args.locale.clone(), args.dry_run, args.strict);
    }
    import_with(
        &args.common,
        file.language(),
        args.locale.clone(),
        args.dry_run,
        Strict::from_flag(args.strict),
        |ctx| file.plan(ctx),
    )
}

/// The import of a parsed PO file (`regia loc import`, and `regia loc dialogue-sheet import`, which turns
/// a recording sheet into one): the L1 checks, `imported.ftl` and the placeholder removal, and the report.
///
/// # Errors
///
/// The localization files cannot be read or written, or no locale is known.
pub fn import_po(
    common: &ProjectArgs,
    po: &PoFile,
    locale: Option<String>,
    dry_run: bool,
    strict: bool,
) -> Result<bool> {
    let header = po.header_value("Language").map(str::to_owned);
    import_with(
        common,
        header,
        locale,
        dry_run,
        Strict::from_flag(strict),
        |ctx| loc_po::plan_import(po, ctx),
    )
}

/// [`import_po`] for `regia loc dialogue-sheet import`: with `strict`, a plan that fails strict (a stale
/// or fuzzy entry) writes nothing at all (the accepted entries are withheld too), so a sheet import is
/// all or nothing. The decision is the same plan's [`loc_po::ImportReport::failed`] the writer uses.
/// Without `strict` it is [`import_po`].
///
/// # Errors
///
/// The localization files cannot be read or written, or no locale is known.
pub fn import_po_all_or_nothing(
    common: &ProjectArgs,
    po: &PoFile,
    locale: Option<String>,
    dry_run: bool,
    strict: bool,
) -> Result<bool> {
    let header = po.header_value("Language").map(str::to_owned);
    let strict = if strict { Strict::AllOrNothing } else { Strict::Off };
    import_with(common, header, locale, dry_run, strict, |ctx| {
        loc_po::plan_import(po, ctx)
    })
}

/// The last line of a text-mode import report. `withheld` (an all-or-nothing strict import whose plan
/// failed strict) says nothing was written, never "would import".
pub(super) fn summary_line(locale: &str, imported: usize, dry_run: bool, withheld: bool) -> String {
    if withheld {
        format!(
            "not written: --strict and a stale or fuzzy entry; {imported} accepted entries for {locale} withheld"
        )
    } else if dry_run {
        format!("would import {imported} entries for {locale}")
    } else {
        format!("imported {imported} entries for {locale}")
    }
}

/// How `--strict` treats a plan that fails it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Strict {
    /// Not strict: skipped entries do not fail the import.
    Off,
    /// `regia loc import --strict`: accepted entries are written, then the import fails (spec 36 § 3).
    WriteThenFail,
    /// `regia loc dialogue-sheet import --strict`: nothing is written when the plan fails strict.
    AllOrNothing,
}

impl Strict {
    fn from_flag(strict: bool) -> Self {
        if strict { Self::WriteThenFail } else { Self::Off }
    }
}

/// The import shared by the PO and XLIFF paths: `plan` decides from the project's context what to write.
fn import_with(
    common: &ProjectArgs,
    header_locale: Option<String>,
    locale: Option<String>,
    dry_run: bool,
    strict_mode: Strict,
    plan: impl FnOnce(&ImportContext) -> ImportPlan,
) -> Result<bool> {
    let strict = strict_mode != Strict::Off;
    let project = &common.project;
    let planned = exchange::plan_import(project, header_locale, locale, plan)?;
    let locale = planned.locale.clone();
    let report = &planned.plan.report;
    let ok = !report.failed(strict);
    // All-or-nothing: the same plan's strict verdict withholds every write.
    let withheld = strict_mode == Strict::AllOrNothing && !ok;
    let written = !dry_run && !withheld && planned.writes();
    if written {
        exchange::write_import(project, &planned)?;
    }
    if common.json {
        let notes = |ns: &[loc_po::ImportNote]| {
            ns.iter()
                .map(|n| json!({ "code": n.code, "key": n.key, "message": n.message }))
                .collect::<Vec<_>>()
        };
        println!(
            "{}",
            json!({
                "locale": locale,
                "imported": report.imported,
                "skipped": notes(&report.skipped),
                "warnings": notes(&report.warnings),
                "refused": report.refused.as_ref().map(|n| json!({ "code": n.code, "message": n.message })),
                "written": written,
                "ok": ok,
            })
        );
    } else {
        if let Some(r) = &report.refused {
            println!("{}: {}", r.code, r.message);
        }
        for n in &report.skipped {
            println!("skipped {} {}: {}", n.code, n.key, n.message);
        }
        for n in &report.warnings {
            println!("warning {} {}: {}", n.code, n.key, n.message);
        }
        println!("{}", summary_line(&locale, report.imported, dry_run, withheld));
    }
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Corr-low (cli LOW): a failed rename left `<name>.tmp<pid>` next to the target (the
    /// target itself was unharmed). `loc sheet` shares this writer.
    #[test]
    fn corr_low_a_failed_rename_leaves_no_temporary_file() {
        let dir = regia_test_utils::unique_temp_dir("loc_write_atomic_leak");
        let target = dir.join("fr.ftl");
        std::fs::create_dir_all(target.join("inside")).expect("blocking directory");
        assert!(write_atomic(&target, "x = y\n").is_err());
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["fr.ftl"], "a temporary file was left: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    const EN: &str = "menu-quit = Quit\ngreet = Hello, { $name }!\n";

    fn project() -> PathBuf {
        project_in(regia_test_utils::unique_temp_dir("cli-loc-exchange"))
    }

    /// Lays a small project out at `dir` (created if missing).
    fn project_in(dir: PathBuf) -> PathBuf {
        std::fs::create_dir_all(dir.join("assets/localization/en")).expect("en dir");
        std::fs::create_dir_all(dir.join("assets/localization/fr")).expect("fr dir");
        std::fs::write(dir.join("assets/localization/en/main.ftl"), EN).expect("en");
        std::fs::write(
            dir.join("assets/localization/fr/main.ftl"),
            "menu-quit = Quitter\n",
        )
        .expect("fr");
        dir
    }

    fn common(dir: &Path) -> ProjectArgs {
        ProjectArgs {
            project: dir.to_path_buf(),
            json: false,
        }
    }

    /// A planted symlink at the predictable temp name is not followed.
    #[cfg(unix)]
    #[test]
    fn exchange_write_does_not_follow_a_planted_symlink() {
        let dir = regia_test_utils::unique_temp_dir("loc-exchange-atomic");
        let secret = dir.join("secret");
        std::fs::write(&secret, b"keep").expect("secret");
        let dest = dir.join("out.po");
        let tmp = dest.with_extension(format!("tmp{}", std::process::id()));
        std::os::unix::fs::symlink(&secret, &tmp).expect("plant");
        write_atomic(&dest, "new").expect("write");
        assert_eq!(std::fs::read(&secret).expect("secret"), b"keep");
        assert_eq!(std::fs::read(&dest).expect("dest"), b"new");
        assert!(dest.symlink_metadata().expect("meta").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_fill_import_round_trip_on_disk() {
        let dir = project();
        let out = dir.join("fr.po");
        let n = export(&ExportArgs {
            common: common(&dir),
            locale: "fr".into(),
            out: Some(out.clone()),
            only: OnlyArg::Untranslated,
            format: FormatArg::Po,
        })
        .expect("exports");
        assert_eq!(n, 1, "menu-quit is already translated");
        let po = std::fs::read_to_string(&out).expect("po");
        assert!(po.contains("msgctxt \"greet\""), "{po}");
        let filled = po.replace(
            "msgid \"Hello, { $name }!\"\nmsgstr \"\"",
            "msgid \"Hello, { $name }!\"\nmsgstr \"Bonjour, { $name } !\"",
        );
        assert_ne!(filled, po);
        std::fs::write(&out, filled).expect("fill");
        let args = |dry_run| ImportArgs {
            common: common(&dir),
            file: out.clone(),
            locale: None,
            dry_run,
            strict: false,
        };
        assert!(import(&args(true)).expect("dry run"));
        assert!(
            !dir.join("assets/localization/fr/imported.ftl").exists(),
            "dry run writes nothing"
        );
        assert!(import(&args(false)).expect("imports"));
        let imported =
            std::fs::read_to_string(dir.join("assets/localization/fr/imported.ftl")).expect("imported.ftl");
        assert!(imported.contains("greet = Bonjour, { $name } !"), "{imported}");
        // Idempotent: a second import leaves the bytes alone.
        assert!(import(&args(false)).expect("again"));
        assert_eq!(
            std::fs::read_to_string(dir.join("assets/localization/fr/imported.ftl")).expect("again"),
            imported
        );
        // Nothing left to translate.
        let n = export(&ExportArgs {
            common: common(&dir),
            locale: "fr".into(),
            out: Some(out.clone()),
            only: OnlyArg::Untranslated,
            format: FormatArg::Po,
        })
        .expect("exports");
        assert_eq!(n, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec 36 § 3 / § 7.2: a dialogue line's text key gets the `#:` reference of its line in the
    /// `.dialogue.ron` file on export (lane spec-15-35, ADR-5778).
    #[test]
    fn dialogue_keys_get_references_on_export() {
        let dir = project();
        std::fs::create_dir_all(dir.join("assets/dialogue")).expect("dialogue dir");
        std::fs::write(
            dir.join("assets/dialogue/intro.dialogue.ron"),
            "(format_version: 1, name: \"intro\",\n lines: [(id: \"hello\", speaker: \"ranger\", variants: [])])\n",
        )
        .expect("dialogue");
        std::fs::write(
            dir.join("assets/localization/en/main.ftl"),
            format!("{EN}dlg-intro-hello = Hello there\n"),
        )
        .expect("en");
        let out = dir.join("fr.po");
        export(&ExportArgs {
            common: common(&dir),
            locale: "fr".into(),
            out: Some(out.clone()),
            only: OnlyArg::All,
            format: FormatArg::Po,
        })
        .expect("exports");
        let po = std::fs::read_to_string(&out).expect("po");
        let ctxt = po
            .find("msgctxt \"dlg-intro-hello\"")
            .expect("the dialogue key is exported");
        let reference = po
            .find("#: assets/dialogue/intro.dialogue.ron:2")
            .expect("the dialogue line is referenced");
        assert!(reference < ctxt, "{po}");
        assert!(
            !po[reference..ctxt].contains("msgctxt"),
            "the reference belongs to the dialogue entry: {po}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compile_writes_deterministic_bundles_and_skips_unlisted_locales() {
        let dir = project();
        std::fs::create_dir_all(dir.join("assets/localization/de")).expect("de dir");
        std::fs::write(
            dir.join("assets/localization/de/main.ftl"),
            "menu-quit = Beenden\ngreet = Hallo, { $name }!\n",
        )
        .expect("de");
        std::fs::write(
            dir.join("assets/localization/fr/main.ftl"),
            "menu-quit = Quitter\ngreet = Bonjour, { $name } !\n",
        )
        .expect("fr");
        let args = || CompileArgs {
            common: common(&dir),
            locales: vec!["fr".into()],
            out: None,
            strict: false,
        };
        assert!(compile(&args()).expect("compiles"));
        let loc = dir.join("build/loc");
        let first = std::fs::read(loc.join("fr.ftl")).expect("fr bundle");
        let index = std::fs::read_to_string(loc.join("index.ron")).expect("index");
        assert!(loc.join("en.ftl").is_file(), "the source locale always ships");
        assert!(!loc.join("de.ftl").exists(), "an unlisted locale is absent");
        let parsed = BundleIndex::from_ron(&index).expect("index parses");
        assert_eq!(
            parsed.locales.iter().map(|l| l.tag.as_str()).collect::<Vec<_>>(),
            ["en", "fr"]
        );
        assert!(compile(&args()).expect("again"));
        assert_eq!(std::fs::read(loc.join("fr.ftl")).expect("fr again"), first);
        assert_eq!(
            std::fs::read_to_string(loc.join("index.ron")).expect("index again"),
            index
        );
        // A translation that drops a variable is a warning: `--strict` refuses to write bundles.
        std::fs::write(
            dir.join("assets/localization/fr/main.ftl"),
            "menu-quit = Quitter\ngreet = Bonjour !\n",
        )
        .expect("fr");
        std::fs::remove_dir_all(&loc).expect("clean");
        let strict = CompileArgs {
            strict: true,
            ..args()
        };
        assert!(!compile(&strict).expect("runs"));
        assert!(!loc.exists(), "nothing is written when validation fails");
        // Spec 36 § 4 acceptance (lane spec-15-35): without `--strict` the warning passes...
        assert!(
            compile(&args()).expect("runs"),
            "a warning passes without --strict"
        );
        assert!(loc.join("fr.ftl").is_file());
        std::fs::remove_dir_all(&loc).expect("clean");
        // ...and an error finding (a variable the source does not pass) fails it either way.
        std::fs::write(
            dir.join("assets/localization/fr/main.ftl"),
            "menu-quit = Quitter\ngreet = Bonjour, { $name } { $title } !\n",
        )
        .expect("fr");
        assert!(
            !compile(&args()).expect("runs"),
            "an error fails without --strict"
        );
        assert!(!loc.exists(), "nothing is written on an error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_fixture(path: &Path, text: &str) {
        let parent = path.parent().expect("fixture has parent");
        std::fs::create_dir_all(parent).expect("fixture parent");
        let tmp = path.with_extension(format!("fixture-tmp-{}", std::process::id()));
        std::fs::write(&tmp, text).expect("write fixture temp");
        std::fs::rename(tmp, path).expect("publish fixture");
    }

    #[test]
    fn compile_republishes_changed_locale_set_with_indexed_runtime_values() {
        let dir = regia_test_utils::unique_temp_dir("cli-loc-compile-history");
        write_fixture(
            &dir.join("assets/localization.ron"),
            "(format_version: 1, source: \"en\", locales: [\"en\", \"fr\", \"fr-CA\", \"de\"], fallbacks: {\"fr-CA\": [\"fr\"]})",
        );
        write_fixture(
            &dir.join("assets/localization/en/main.ftl"),
            "hello = Hello\nbye = Goodbye\n",
        );
        write_fixture(&dir.join("assets/localization/fr/main.ftl"), "hello = Bonjour\n");
        write_fixture(
            &dir.join("assets/localization/fr-CA/main.ftl"),
            "bye = Au revoir\n",
        );
        write_fixture(
            &dir.join("assets/localization/de/main.ftl"),
            "hello = Guten Tag\nbye = Tschüss\n",
        );
        let out = dir.join("build");
        let compile = |locales: &[&str]| {
            write_bundles(
                &dir,
                &out,
                &locales.iter().map(|tag| (*tag).to_owned()).collect::<Vec<_>>(),
                false,
            )
            .expect("compile command runs")
            .expect("validation passes")
        };

        let (_, first_ship) = compile(&["fr-CA"]);
        assert_eq!(first_ship, ["en", "fr", "fr-CA"]);
        let (_, second_ship) = compile(&["de", "de"]);
        assert_eq!(
            second_ship,
            ["de", "en"],
            "duplicate requests do not duplicate shipped rows"
        );

        write_fixture(
            &dir.join("assets/localization/de/main.ftl"),
            "hello = Guten Morgen\nbye = Tschüss!\n",
        );
        let (_, changed_ship) = compile(&["de"]);
        assert_eq!(changed_ship, ["de", "en"]);
        let loc = out.join("loc");
        let index_text = std::fs::read_to_string(loc.join("index.ron")).expect("published index");
        let index = BundleIndex::from_ron(&index_text).expect("index parses");
        assert_eq!(index.source, "en");
        let tags = index
            .locales
            .iter()
            .map(|item| item.tag.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            tags,
            ["de", "en"],
            "index is exactly the available fallback closure and source"
        );
        assert_eq!(
            tags.iter().collect::<std::collections::BTreeSet<_>>().len(),
            tags.len()
        );
        for item in &index.locales {
            let bytes = std::fs::read(out.join(&item.file)).expect("listed bundle bytes");
            let text = String::from_utf8(bytes.clone()).expect("bundle is utf8");
            assert_eq!(
                item.hash,
                blake3::hash(&bytes).to_hex().to_string(),
                "hash for {}",
                item.tag
            );
            let expected_count = match item.tag.as_str() {
                "de" | "en" => 2,
                other => panic!("unexpected indexed locale {other}"),
            };
            let expected_entries = if item.tag == "de" {
                "bye = Tschüss!\n\nhello = Guten Morgen\n"
            } else {
                "bye = Goodbye\n\nhello = Hello\n"
            };
            assert_eq!(item.messages, expected_count, "message count for {}", item.tag);
            assert_eq!(text, expected_entries, "normalized content for {}", item.tag);
        }
        let mut runtime = regia_ui::Localization::load_bundles(&index, |file| {
            std::fs::read(out.join(file)).map_err(|error| error.to_string())
        })
        .expect("UI loads published bundles");
        assert!(runtime.set_locale("de"));
        assert_eq!(runtime.resolve("hello", &[]), "Guten Morgen");
        assert_eq!(runtime.resolve("bye", &[]), "Tschüss!");
        assert!(
            !index
                .locales
                .iter()
                .any(|item| item.tag == "fr" || item.tag == "fr-CA")
        );

        let clean = regia_test_utils::unique_temp_dir("cli-loc-compile-clean");
        for (relative, text) in [
            (
                "assets/localization.ron",
                "(format_version: 1, source: \"en\", locales: [\"en\", \"fr\", \"fr-CA\", \"de\"], fallbacks: {\"fr-CA\": [\"fr\"]})",
            ),
            (
                "assets/localization/en/main.ftl",
                "hello = Hello\nbye = Goodbye\n",
            ),
            ("assets/localization/fr/main.ftl", "hello = Bonjour\n"),
            ("assets/localization/fr-CA/main.ftl", "bye = Au revoir\n"),
            (
                "assets/localization/de/main.ftl",
                "hello = Guten Morgen\nbye = Tschüss!\n",
            ),
        ] {
            write_fixture(&clean.join(relative), text);
        }
        let (_, clean_ship) = write_bundles(&clean, &clean.join("build"), &["de".to_owned()], false)
            .expect("clean compile")
            .expect("clean validation");
        assert_eq!(clean_ship, changed_ship);
        for item in &index.locales {
            assert_eq!(
                std::fs::read(loc.join(format!("{}.ftl", item.tag))).expect("current bundle"),
                std::fs::read(clean.join("build/loc").join(format!("{}.ftl", item.tag)))
                    .expect("clean bundle"),
                "current indexed output matches clean compile for {}",
                item.tag
            );
        }
        assert_eq!(
            std::fs::read(loc.join("index.ron")).expect("current index"),
            std::fs::read(clean.join("build/loc/index.ron")).expect("clean index")
        );
    }

    #[test]
    fn strict_compile_validation_failure_preserves_published_index_and_bundles() {
        let dir = project();
        let out = dir.join("build");
        let locales = vec!["fr".to_owned()];
        let (loc, _) = write_bundles(&dir, &out, &locales, false)
            .expect("initial compile")
            .expect("initial validation");
        let before_index = std::fs::read(loc.join("index.ron")).expect("initial index");
        let before_bundles =
            ["en.ftl", "fr.ftl"].map(|name| std::fs::read(loc.join(name)).expect("initial bundle"));

        write_fixture(
            &dir.join("assets/localization/fr/main.ftl"),
            "menu-quit = Quitter\ngreet = Bonjour !\n",
        );
        assert!(
            write_bundles(&dir, &out, &locales, true)
                .expect("strict compile command runs")
                .is_none(),
            "strict validation rejects dropped source variable"
        );
        assert_eq!(
            std::fs::read(loc.join("index.ron")).expect("index remains"),
            before_index
        );
        for (name, bytes) in ["en.ftl", "fr.ftl"].into_iter().zip(before_bundles) {
            assert_eq!(
                std::fs::read(loc.join(name)).expect("bundle remains"),
                bytes,
                "{name}"
            );
        }
    }

    /// Fill `needle` -> `replacement` in the exported file at `out` and return the new text.
    fn translate(out: &Path, edits: &[(&str, &str)]) -> String {
        let mut text = std::fs::read_to_string(out).expect("export");
        for (needle, replacement) in edits {
            assert!(text.contains(needle), "{needle} not in {text}");
            text = text.replacen(needle, replacement, 1);
        }
        text
    }

    fn import_file(dir: &Path, file: &Path, strict: bool) -> bool {
        import(&ImportArgs {
            common: common(dir),
            file: file.to_path_buf(),
            locale: None,
            dry_run: false,
            strict,
        })
        .expect("import runs")
    }

    /// `regia loc export` writes `regia_asset_pipeline::loc_exchange::export`'s text, and `regia loc import`
    /// writes what that module's `plan_import` + `write_import` write: the two calls the editor's
    /// Localization panel makes (ADR-7677), so the panel and the CLI write the same bytes. PO and XLIFF.
    #[test]
    fn the_cli_writes_the_bytes_of_the_library_the_editor_calls() {
        for (format, lib, ext) in [
            (FormatArg::Po, ExchangeFormat::Po, "po"),
            (FormatArg::Xliff, ExchangeFormat::Xliff, "xlf"),
        ] {
            let dir = project();
            let twin = project();
            let out = dir.join(format!("fr.{ext}"));
            let n = export(&ExportArgs {
                common: common(&dir),
                locale: "fr".into(),
                out: Some(out.clone()),
                only: OnlyArg::All,
                format,
            })
            .expect("exports");
            let library = exchange::export(&dir, "fr", lib, ExportFilter::All).expect("library export");
            assert_eq!(
                std::fs::read_to_string(&out).expect("written"),
                library.text,
                "{ext}"
            );
            assert_eq!(n, library.entries);

            let filled = match ExchangeFile::parse(&library.text, "export").expect("parses") {
                ExchangeFile::Po(mut po) => {
                    for e in po.entries.iter_mut().filter(|e| e.ctxt == "greet") {
                        "Salut, { $name } !".clone_into(&mut e.msgstr);
                    }
                    loc_po::write_po(&po)
                }
                ExchangeFile::Xliff(mut x) => {
                    for e in x.po.entries.iter_mut().filter(|e| e.ctxt == "greet") {
                        "Salut, { $name } !".clone_into(&mut e.msgstr);
                    }
                    regia_ui::loc_xliff::write_xliff(&x.po).expect("xliff")
                }
            };
            std::fs::write(&out, &filled).expect("fill");
            assert!(import_file(&dir, &out, false));
            let parsed = ExchangeFile::parse(&filled, "file").expect("parses");
            let planned =
                exchange::plan_import(&twin, parsed.language(), None, |ctx| parsed.plan(ctx)).expect("plans");
            exchange::write_import(&twin, &planned).expect("writes");
            let read =
                |d: &Path| std::fs::read(d.join("assets/localization/fr/imported.ftl")).expect("imported");
            assert_eq!(read(&dir), read(&twin), "{ext}");
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_dir_all(&twin);
        }
    }

    #[test]
    fn xliff_round_trip_imports_the_same_bytes_as_po() {
        let po_dir = project();
        let xl_dir = project();
        let export_to = |dir: &Path, name: &str, format| {
            let out = dir.join(name);
            export(&ExportArgs {
                common: common(dir),
                locale: "fr".into(),
                out: Some(out.clone()),
                only: OnlyArg::All,
                format,
            })
            .expect("exports");
            out
        };
        let po = export_to(&po_dir, "fr.po", FormatArg::Po);
        let xl = export_to(&xl_dir, "fr.xlf", FormatArg::Xliff);
        let xml = std::fs::read_to_string(&xl).expect("xliff");
        assert!(
            xml.contains("<ph id=\"1\" canDelete=\"no\" equiv=\"{ $name }\""),
            "{xml}"
        );
        let filled_po = translate(
            &po,
            &[(
                "msgid \"Hello, { $name }!\"\nmsgstr \"\"",
                "msgid \"Hello, { $name }!\"\nmsgstr \"Bonjour, { $name } !\"",
            )],
        );
        std::fs::write(&po, filled_po).expect("fill po");
        let filled_xl = translate(
            &xl,
            &[(
                "<target xml:space=\"preserve\"></target>\n      </segment>\n    </unit>\n    <unit id=\"menu-quit\"",
                "<target xml:space=\"preserve\">Bonjour, <ph id=\"1\" canDelete=\"no\" equiv=\"{ $name }\"/> !</target>\n      </segment>\n    </unit>\n    <unit id=\"menu-quit\"",
            )],
        );
        std::fs::write(&xl, filled_xl).expect("fill xliff");
        assert!(import_file(&po_dir, &po, false));
        assert!(import_file(&xl_dir, &xl, false));
        let read = |dir: &Path| {
            std::fs::read(dir.join("assets/localization/fr/imported.ftl")).expect("imported.ftl")
        };
        assert_eq!(read(&po_dir), read(&xl_dir), "same imported.ftl bytes");
        assert!(
            String::from_utf8(read(&xl_dir))
                .expect("utf8")
                .contains("greet = Bonjour, { $name } !")
        );
        let _ = std::fs::remove_dir_all(&po_dir);
        let _ = std::fs::remove_dir_all(&xl_dir);
    }

    #[test]
    fn xliff_import_refuses_a_deleted_placeholder_and_writes_nothing() {
        let dir = project();
        let xl = dir.join("fr.xlf");
        export(&ExportArgs {
            common: common(&dir),
            locale: "fr".into(),
            out: Some(xl.clone()),
            only: OnlyArg::Untranslated,
            format: FormatArg::Xliff,
        })
        .expect("exports");
        let broken = translate(
            &xl,
            &[(
                "<target xml:space=\"preserve\"></target>",
                "<target xml:space=\"preserve\">Bonjour !</target>",
            )],
        );
        std::fs::write(&xl, broken).expect("fill");
        let mut json = common(&dir);
        json.json = true;
        let ok = import(&ImportArgs {
            common: json,
            file: xl.clone(),
            locale: None,
            dry_run: true,
            strict: false,
        })
        .expect("runs");
        assert!(ok, "a skipped entry is not a failed import, as with PO");
        assert!(import_file(&dir, &xl, false));
        assert!(
            !dir.join("assets/localization/fr/imported.ftl").exists(),
            "nothing was accepted, so nothing is written"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `Language` header of a translator's file cannot steer the write out of
    /// `assets/localization`: `../../evil` is refused and nothing is written. The project sits one
    /// level inside its own unique directory, so the escape target (`<root>/evil`) is the test's
    /// own and a stray `evil` in the shared temp root cannot fail it (w7-runtime-smalls-z item 4).
    #[test]
    fn audit_a_language_header_cannot_leave_the_localization_dir() {
        let root = regia_test_utils::unique_temp_dir("cli-loc-exchange-escape");
        let dir = project_in(root.join("project"));
        let out = dir.join("fr.po");
        export(&ExportArgs {
            common: common(&dir),
            locale: "fr".into(),
            out: Some(out.clone()),
            only: OnlyArg::Untranslated,
            format: FormatArg::Po,
        })
        .expect("exports");
        let po = std::fs::read_to_string(&out).expect("po");
        assert!(po.contains("Language: fr"), "{po}");
        let evil = po.replace("Language: fr", "Language: ../../../evil").replace(
            "msgid \"Hello, { $name }!\"\nmsgstr \"\"",
            "msgid \"Hello, { $name }!\"\nmsgstr \"Bonjour, { $name } !\"",
        );
        std::fs::write(&out, evil).expect("evil po");
        let err = import(&ImportArgs {
            common: common(&dir),
            file: out,
            locale: None,
            dry_run: false,
            strict: false,
        })
        .expect_err("refused");
        assert!(err.to_string().contains("not a locale tag"), "{err}");
        assert!(!dir.join("evil").exists());
        assert!(!root.join("evil").exists());
        for good in ["fr", "pt-BR", "zh_Hant_TW", "x1"] {
            assert!(is_locale_tag(good), "{good}");
        }
        for bad in ["", "..", "../x", "fr/..", "/tmp", "C:x", "1x", "fr\\x"] {
            assert!(!is_locale_tag(bad), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_refuses_other_languages_and_exporting_the_source_fails() {
        let dir = project();
        let out = dir.join("fr.po");
        export(&ExportArgs {
            common: common(&dir),
            locale: "fr".into(),
            out: Some(out.clone()),
            only: OnlyArg::All,
            format: FormatArg::Po,
        })
        .expect("exports");
        let refused = import(&ImportArgs {
            common: common(&dir),
            file: out,
            locale: Some("de".into()),
            dry_run: false,
            strict: false,
        })
        .expect("runs");
        assert!(!refused, "Language fr does not match --locale de");
        assert!(
            export(&ExportArgs {
                common: common(&dir),
                locale: "en".into(),
                out: None,
                only: OnlyArg::All,
                format: FormatArg::Po,
            })
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
