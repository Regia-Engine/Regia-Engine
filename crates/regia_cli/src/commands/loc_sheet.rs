// SPDX-License-Identifier: MPL-2.0
//! `regia loc dialogue-sheet export|import` (spec 36 § 10 "Recording sheets", D2, ADR-1882): one CSV
//! row per dialogue line for a locale, for the cast and the recording studio, and the way edited texts
//! come back.
//!
//! Columns: `line,speaker,listeners,spoken_text,display_text,recording,note`, then two columns this
//! command adds at the end, `source_text,source_display` (the source locale's patterns when the sheet
//! was written), so import can tell a translation made against an older source (stale) the way the PO
//! exchange does. CSV is RFC 4180 through `regia_asset::csv`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use regia::framework::dialogue::DialogueLibrary;
use regia_ui::loc_po::{self, PoEntry, PoFile};
use serde_json::json;

use super::loc::{ProjectArgs, read_config, read_locales};

/// The sheet's columns, in order.
pub const SHEET_COLUMNS: [&str; 9] = [
    "line",
    "speaker",
    "listeners",
    "spoken_text",
    "display_text",
    "recording",
    "note",
    "source_text",
    "source_display",
];

/// `regia loc dialogue-sheet` subcommands.
#[derive(Debug, Subcommand)]
pub enum SheetCommand {
    /// Write the recording sheet (CSV) of a locale.
    Export(SheetExportArgs),
    /// Read edited spoken and display texts back into `imported.ftl`.
    Import(SheetImportArgs),
}

/// `regia loc dialogue-sheet export`.
#[derive(Debug, Args)]
pub struct SheetExportArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// The locale of the sheet.
    #[arg(long)]
    pub locale: String,
    /// The CSV file to write.
    #[arg(long)]
    pub csv: PathBuf,
}

/// `regia loc dialogue-sheet import`.
#[derive(Debug, Args)]
pub struct SheetImportArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// The locale of the sheet.
    #[arg(long)]
    pub locale: String,
    /// The edited CSV file.
    #[arg(long)]
    pub csv: PathBuf,
    /// Print the report; write nothing.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Stale entries fail the import (exit 1).
    #[arg(long, default_value_t = false)]
    pub strict: bool,
}

/// Runs a sheet subcommand. `false` when the import failed its checks.
///
/// # Errors
///
/// The project or the sheet cannot be read or written.
pub fn run(command: &SheetCommand) -> Result<bool> {
    match command {
        SheetCommand::Export(args) => export(args).map(|_| true),
        SheetCommand::Import(args) => import(args),
    }
}

/// A locale's real translations (no gather placeholders), key to pattern; later files win.
fn patterns(files: &[(String, String)]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (_, text) in files {
        for block in loc_po::ftl_blocks(text) {
            if block.placeholder {
                out.remove(&block.key);
            } else {
                out.insert(block.key, block.pattern);
            }
        }
    }
    out
}

/// The rows of the sheet for `locale` (header first).
///
/// # Errors
///
/// The localization files cannot be read.
pub fn sheet_rows(project: &Path, locale: &str) -> Result<Vec<Vec<String>>> {
    let config = read_config(project)?;
    let source = if config.source.is_empty() {
        "en".to_owned()
    } else {
        config.source.clone()
    };
    let locales = read_locales(project, &config)?;
    let files_of = |tag: &str| {
        locales
            .iter()
            .find(|(p, _)| p.locale == tag)
            .map(|(_, f)| f.clone())
            .unwrap_or_default()
    };
    let source_patterns = patterns(&files_of(&source));
    let target_patterns = patterns(&files_of(locale));
    let assets = project.join("assets");
    let library = DialogueLibrary::load_dir(&assets);
    let mut rows = vec![SHEET_COLUMNS.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>()];
    for (name, dialogue) in &library.dialogues {
        for line in &dialogue.lines {
            let text_key = line.text_key(name);
            let display_key = line.display_key.clone();
            let pick = |key: &str| target_patterns.get(key).cloned().unwrap_or_default();
            let recording = line
                .variants
                .first()
                .map(|v| {
                    let rel = if locale == source {
                        v.sound.clone()
                    } else {
                        format!("{}/{locale}/{}", regia_asset::localized::LOCALIZED_DIR, v.sound)
                    };
                    let path = format!("assets/{rel}");
                    if project.join(&path).is_file() {
                        path
                    } else {
                        String::new()
                    }
                })
                .unwrap_or_default();
            rows.push(vec![
                format!("{name}#{}", line.id),
                line.speaker.clone(),
                line.listeners.join(";"),
                pick(&text_key),
                display_key.as_deref().map(pick).unwrap_or_default(),
                recording,
                line.note.clone(),
                source_patterns.get(&text_key).cloned().unwrap_or_default(),
                display_key
                    .as_deref()
                    .and_then(|k| source_patterns.get(k).cloned())
                    .unwrap_or_default(),
            ]);
        }
    }
    Ok(rows)
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    // One writer for the loc commands: unlinks a planted temp symlink, cleans up on failure.
    super::loc_exchange::write_atomic(path, text)
}

/// Writes the sheet; returns the number of line rows.
///
/// # Errors
///
/// The project cannot be read or the CSV cannot be written.
pub fn export(args: &SheetExportArgs) -> Result<usize> {
    let rows = sheet_rows(&args.common.project, &args.locale)?;
    write_atomic(&args.csv, &regia_asset::csv::to_string(&rows))?;
    let lines = rows.len() - 1;
    if args.common.json {
        println!(
            "{}",
            json!({ "locale": args.locale, "lines": lines, "csv": args.csv.display().to_string() })
        );
    } else {
        println!(
            "wrote {lines} line(s) for {} to {}",
            args.locale,
            args.csv.display()
        );
    }
    Ok(lines)
}

/// The PO file a sheet stands for (`msgctxt` = key, `msgid` = the source column, `msgstr` = the edited
/// text), and the `recording` paths that do not exist. Rows are matched to keys through the dialogues.
///
/// # Errors
///
/// The CSV does not parse or lacks a column.
pub fn sheet_to_po(project: &Path, locale: &str, csv_text: &str) -> Result<(PoFile, Vec<String>)> {
    let records: Vec<Vec<String>> = regia_asset::csv::parse(csv_text)
        .map_err(|e| anyhow::anyhow!("sheet: {e}"))?
        .into_iter()
        .map(|r| r.fields)
        .collect();
    let Some((header, rows)) = records.split_first() else {
        bail!("the sheet is empty");
    };
    let column = |name: &str| {
        header
            .iter()
            .position(|h| h.trim_start_matches('\u{feff}') == name)
            .with_context(|| format!("the sheet has no `{name}` column"))
    };
    let (c_line, c_spoken, c_display, c_recording) = (
        column("line")?,
        column("spoken_text")?,
        column("display_text")?,
        column("recording")?,
    );
    let (c_source, c_source_display) = (column("source_text").ok(), column("source_display").ok());
    let library = DialogueLibrary::load_dir(&project.join("assets"));
    let mut po = PoFile {
        header: vec![("Language".to_owned(), locale.to_owned())],
        entries: Vec::new(),
    };
    let mut missing = Vec::new();
    let cell = |row: &[String], i: usize| row.get(i).cloned().unwrap_or_default();
    for row in rows {
        let id = cell(row, c_line);
        let Some((name, line_id)) = id.split_once('#') else {
            continue;
        };
        let Some(line) = library.dialogues.get(name).and_then(|d| d.line(line_id)) else {
            // An unknown line reaches the import as an unknown key, which reports it.
            po.entries.push(PoEntry {
                ctxt: format!("dlg-{name}-{line_id}"),
                msgstr: cell(row, c_spoken),
                ..PoEntry::default()
            });
            continue;
        };
        let source_of = |c: Option<usize>| c.map(|i| cell(row, i)).unwrap_or_default();
        po.entries.push(PoEntry {
            ctxt: line.text_key(name),
            msgid: source_of(c_source),
            msgstr: cell(row, c_spoken),
            ..PoEntry::default()
        });
        if let Some(key) = &line.display_key {
            po.entries.push(PoEntry {
                ctxt: key.clone(),
                msgid: source_of(c_source_display),
                msgstr: cell(row, c_display),
                ..PoEntry::default()
            });
        }
        let recording = cell(row, c_recording);
        if !recording.is_empty() && !project.join(&recording).is_file() {
            missing.push(recording);
        }
    }
    Ok((po, missing))
}

/// Reads an edited sheet back (same checks as `regia loc import`). With `--strict` the import is all
/// or nothing: when any row is stale or fuzzy no project file is written, where `regia loc import
/// --strict` writes the accepted entries and then fails.
///
/// # Errors
///
/// The project or the sheet cannot be read, or the locale is the source locale.
pub fn import(args: &SheetImportArgs) -> Result<bool> {
    let project = &args.common.project;
    let config = read_config(project)?;
    let source = if config.source.is_empty() {
        "en"
    } else {
        config.source.as_str()
    };
    if args.locale == source {
        bail!("`{source}` is the source locale: edit its .ftl files instead of importing a sheet");
    }
    let text =
        std::fs::read_to_string(&args.csv).with_context(|| format!("reading {}", args.csv.display()))?;
    let (po, missing) = sheet_to_po(project, &args.locale, &text)?;
    for path in &missing {
        if args.common.json {
            println!(
                "{}",
                json!({ "code": "loc_sheet_missing_recording", "recording": path })
            );
        } else {
            println!("warning loc_sheet_missing_recording {path}: the recording does not exist");
        }
    }
    super::loc_exchange::import_po_all_or_nothing(
        &args.common,
        &po,
        Some(args.locale.clone()),
        args.dry_run,
        args.strict,
    )
}

// `add_dialogue_keys` moved to `regia_asset_pipeline::loc_exchange` (ADR-7677), shared with the editor.
pub use regia_asset_pipeline::loc_exchange::add_dialogue_keys;

// `dialogue_findings` moved to `regia_asset_pipeline::loc_exchange` (ADR-7855), shared with the editor's Compile.

// Only the unix symlink-cycle test uses it; an unconditional import warns on Windows.
#[cfg(all(test, unix))]
use regia_asset_pipeline::loc_exchange::find_file;

#[cfg(test)]
mod tests {
    use super::*;

    fn write_atomic_test(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture directory");
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, bytes).expect("fixture temporary write");
        std::fs::rename(tmp, path).expect("fixture atomic rename");
    }

    fn invoke_import(root: &Path, csv: &str, strict: bool) -> Result<bool> {
        let csv_path = root.join("edited.csv");
        write_atomic_test(&csv_path, csv.as_bytes());
        let result = run(&SheetCommand::Import(SheetImportArgs {
            common: ProjectArgs {
                project: root.to_path_buf(),
                json: true,
            },
            locale: "fr".into(),
            csv: csv_path.clone(),
            dry_run: false,
            strict,
        }));
        let _ = std::fs::remove_file(csv_path);
        result
    }

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(dir: &Path, root: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).expect("fixture read dir") {
                let path = entry.expect("fixture entry").path();
                if path.is_dir() {
                    visit(&path, root, files);
                } else {
                    if path.file_name().is_some_and(|name| name == "edited.csv") {
                        continue;
                    }
                    files.insert(
                        path.strip_prefix(root)
                            .expect("fixture relative path")
                            .to_path_buf(),
                        std::fs::read(path).expect("fixture file"),
                    );
                }
            }
        }
        let mut files = BTreeMap::new();
        visit(root, root, &mut files);
        files
    }

    fn seed_import_transaction_files(root: &Path) {
        write_atomic_test(
            &root.join("assets/localization/fr/imported.ftl"),
            b"dlg-fr-existing = Deja traduit.\n",
        );
        write_atomic_test(
            &root.join("assets/localization/fr/other.ftl"),
            b"unrelated = doit rester intact\n",
        );
        write_atomic_test(
            &root.join("assets/localization/de/main.ftl"),
            b"gruss = Guten Tag\n",
        );
    }

    fn edited_rows(root: &Path) -> Vec<Vec<String>> {
        sheet_rows(root, "fr").expect("sheet rows")
    }

    fn csv_for(rows: &[Vec<String>]) -> String {
        regia_asset::csv::to_string(rows)
    }

    fn project() -> PathBuf {
        project_with_source(
            b"dlg-intro-hello = Hello, traveler.\ndlg-intro-hello-short = Hello!\ndlg-intro-bye = Bye.\n",
        )
    }

    fn project_with_source(source_text: &[u8]) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir("loc-sheet");
        let loc = root.join("assets/localization");
        write_atomic_test(&loc.join("en/main.ftl"), source_text);
        write_atomic_test(&loc.join("fr/main.ftl"), b"dlg-intro-bye = Au revoir.\n");
        write_atomic_test(
            &root.join("assets/dialogue/intro.dialogue.ron"),
            br#"(format_version: 1, name: "intro", lines: [
                (id: "hello", speaker: "ranger", listeners: ["player", "child"], note: "Warm, \"tired\"",
                 display_key: Some("dlg-intro-hello-short"), variants: [(sound: "audio/hello.ogg")]),
                (id: "bye", speaker: "ranger", variants: [(sound: "audio/bye.ogg")]),
            ])"#,
        );
        write_atomic_test(&root.join("assets/localized/fr/audio/bye.ogg"), b"x");
        root
    }

    #[test]
    fn export_writes_one_row_per_line_with_quoting_and_recordings() {
        let root = project();
        let rows = sheet_rows(&root, "fr").expect("rows");
        assert_eq!(rows[0], SHEET_COLUMNS);
        assert_eq!(rows.len(), 3);
        let bye = rows.iter().find(|r| r[0] == "intro#bye").expect("bye");
        assert_eq!(bye[3], "Au revoir.");
        assert_eq!(bye[5], "assets/localized/fr/audio/bye.ogg", "recorded");
        assert_eq!(bye[7], "Bye.");
        let hello = rows.iter().find(|r| r[0] == "intro#hello").expect("hello");
        assert_eq!(hello[2], "player;child");
        assert_eq!(
            (hello[3].as_str(), hello[5].as_str()),
            ("", ""),
            "untranslated, unrecorded"
        );
        assert_eq!(hello[8], "Hello!");
        let csv = regia_asset::csv::to_string(&rows);
        assert!(csv.contains("\"Warm, \"\"tired\"\"\""), "{csv}");
        // The source locale's sheet: native lines.
        let en = sheet_rows(&root, "en").expect("en");
        assert_eq!(
            en.iter()
                .find(|r| r[0] == "intro#hello")
                .map(|r| r[3].clone())
                .as_deref(),
            Some("Hello, traveler.")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_edited_sheet_becomes_po_entries_with_stale_sources_and_missing_recordings() {
        let root = project();
        let mut rows = sheet_rows(&root, "fr").expect("rows");
        let hello = rows.iter_mut().find(|r| r[0] == "intro#hello").expect("hello");
        hello[3] = "Bonjour, voyageur.".into();
        hello[4] = "Bonjour !".into();
        hello[5] = "assets/localized/fr/audio/hello.ogg".into();
        let bye = rows.iter_mut().find(|r| r[0] == "intro#bye").expect("bye");
        bye[7] = "Goodbye.".into(); // made against an older source text
        let csv = regia_asset::csv::to_string(&rows);
        let (po, missing) = sheet_to_po(&root, "fr", &csv).expect("po");
        assert_eq!(missing, ["assets/localized/fr/audio/hello.ogg"]);
        let entry = |key: &str| po.entries.iter().find(|e| e.ctxt == key).expect("entry").clone();
        assert_eq!(entry("dlg-intro-hello").msgstr, "Bonjour, voyageur.");
        assert_eq!(entry("dlg-intro-hello").msgid, "Hello, traveler.");
        assert_eq!(entry("dlg-intro-hello-short").msgstr, "Bonjour !");
        assert_eq!(entry("dlg-intro-bye").msgid, "Goodbye.");
        // Through the L1 import: hello imports, bye is stale and skipped.
        let common = ProjectArgs {
            project: root.clone(),
            json: false,
        };
        assert!(
            super::super::loc_exchange::import_po(&common, &po, Some("fr".into()), false, false)
                .expect("import")
        );
        let imported =
            std::fs::read_to_string(root.join("assets/localization/fr/imported.ftl")).expect("imported.ftl");
        assert!(
            imported.contains("dlg-intro-hello = Bonjour, voyageur."),
            "{imported}"
        );
        assert!(!imported.contains("Au revoir"), "{imported}");
        assert!(
            !super::super::loc_exchange::import_po(&common, &po, Some("fr".into()), true, true)
                .expect("strict"),
            "strict: the stale entry fails"
        );
        let args = SheetImportArgs {
            common: ProjectArgs {
                project: root.clone(),
                json: true,
            },
            locale: "en".into(),
            csv: root.join("none.csv"),
            dry_run: true,
            strict: false,
        };
        assert!(import(&args).is_err(), "the source locale is refused");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_row_after_valid_translation_is_skipped_without_losing_valid_row() {
        let root = project();
        seed_import_transaction_files(&root);
        let mut rows = edited_rows(&root);
        let valid = rows.iter_mut().find(|r| r[0] == "intro#hello").expect("hello");
        valid[3] = "Bonjour, voyageur.".into();
        let invalid = rows.iter_mut().find(|r| r[0] == "intro#bye").expect("bye");
        invalid[3] = "Bonjour { $unexpected }".into();
        let (po, _) = sheet_to_po(&root, "fr", &csv_for(&rows)).expect("parsed sheet");
        let invalid_entry = po
            .entries
            .iter()
            .find(|e| e.ctxt == "dlg-intro-bye")
            .expect("bye entry");
        assert_eq!(invalid_entry.msgstr, "Bonjour { $unexpected }");
        assert!(invoke_import(&root, &csv_for(&rows), false).expect("command"));
        let imported =
            std::fs::read_to_string(root.join("assets/localization/fr/imported.ftl")).expect("imported file");
        assert!(
            imported.contains("dlg-intro-hello = Bonjour, voyageur."),
            "{imported}"
        );
        assert!(
            !imported.contains("dlg-intro-bye"),
            "mismatched placeholder is skipped: {imported}"
        );
        assert!(imported.contains("dlg-fr-existing = Deja traduit."), "{imported}");
        assert_eq!(
            std::fs::read(root.join("assets/localization/fr/other.ftl")).expect("other"),
            b"unrelated = doit rester intact\n"
        );
        assert_eq!(
            std::fs::read(root.join("assets/localization/de/main.ftl")).expect("de"),
            b"gruss = Guten Tag\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_csv_after_valid_prefix_is_fatal_and_leaves_project_unchanged() {
        let root = project();
        seed_import_transaction_files(&root);
        let before = snapshot(&root);
        let csv = "line,speaker,listeners,spoken_text,display_text,recording,note,source_text,source_display\nintro#hello,ranger,,Bonjour,,, ,Hello,Hello!\n\"unterminated";
        assert!(invoke_import(&root, csv, false).is_err());
        assert_eq!(
            snapshot(&root),
            before,
            "CSV parse failure must write no project files"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The stale row's source block is a `# TODO translate` placeholder: the sheet carries no source
    /// text for it (the export skips placeholders) while the importer still compares it with the
    /// placeholder's pattern, so only the plan the writer uses sees it as stale. A valid sibling row
    /// must not be written either.
    #[test]
    fn strict_stale_row_on_a_placeholder_source_leaves_all_files_unchanged() {
        let root = project_with_source(
            b"dlg-intro-hello = Hello, traveler.\ndlg-intro-hello-short = Hello!\n# TODO translate\ndlg-intro-bye = Bye.\n",
        );
        seed_import_transaction_files(&root);
        let mut rows = edited_rows(&root);
        let hello = rows.iter_mut().find(|r| r[0] == "intro#hello").expect("hello");
        hello[3] = "Bonjour".into();
        let bye = rows.iter_mut().find(|r| r[0] == "intro#bye").expect("bye");
        bye[3] = "Adieu".into();
        let csv = csv_for(&rows);
        let before = snapshot(&root);
        assert!(!invoke_import(&root, &csv, true).expect("command"));
        assert_eq!(snapshot(&root), before, "a strict sheet import is all or nothing");
        let line = super::super::loc_exchange::summary_line("fr", 1, false, true);
        assert!(!line.contains("would import"), "{line}");
        assert!(line.starts_with("not written"), "{line}");
        // A dry run of a withheld plan writes nothing, so it must not say it would import.
        let dry = super::super::loc_exchange::summary_line("fr", 1, true, true);
        assert!(!dry.contains("would import"), "{dry}");
        assert!(dry.starts_with("not written"), "{dry}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A planted symlink at the predictable temp name is not followed.
    #[cfg(unix)]
    #[test]
    fn sheet_write_does_not_follow_a_planted_symlink() {
        let dir = regia_test_utils::unique_temp_dir("loc-sheet-atomic");
        let secret = dir.join("secret");
        std::fs::write(&secret, b"keep").expect("secret");
        let dest = dir.join("out.csv");
        let tmp = dest.with_extension(format!("tmp{}", std::process::id()));
        std::os::unix::fs::symlink(&secret, &tmp).expect("plant");
        write_atomic(&dest, "new").expect("write");
        assert_eq!(std::fs::read(&secret).expect("secret"), b"keep");
        assert_eq!(std::fs::read(&dest).expect("dest"), b"new");
        assert!(dest.symlink_metadata().expect("meta").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strict_stale_row_after_valid_row_is_fatal_and_leaves_all_files_unchanged() {
        let root = project();
        seed_import_transaction_files(&root);
        let mut rows = edited_rows(&root);
        let hello = rows.iter_mut().find(|r| r[0] == "intro#hello").expect("hello");
        hello[3] = "Bonjour".into();
        let bye = rows.iter_mut().find(|r| r[0] == "intro#bye").expect("bye");
        bye[3] = "Adieu".into();
        bye[7] = "Old source".into();
        let csv = csv_for(&rows);
        let before = snapshot(&root);
        assert!(!invoke_import(&root, &csv, true).expect("command"));
        assert_eq!(snapshot(&root), before, "strict stale rejection is transactional");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn duplicate_line_rows_use_last_accepted_translation() {
        let root = project();
        seed_import_transaction_files(&root);
        let rows = edited_rows(&root);
        let header = rows[0].clone();
        let hello = rows.iter().find(|r| r[0] == "intro#hello").expect("hello");
        let mut first = hello.clone();
        first[3] = "Premier".into();
        let mut last = hello.clone();
        last[3] = "Dernier".into();
        let mut duplicate_rows = vec![header, first, last];
        duplicate_rows[0] = SHEET_COLUMNS.iter().map(|s| (*s).to_owned()).collect();
        assert!(invoke_import(&root, &csv_for(&duplicate_rows), false).expect("command"));
        let imported =
            std::fs::read_to_string(root.join("assets/localization/fr/imported.ftl")).expect("imported");
        assert!(imported.contains("dlg-intro-hello = Dernier"), "{imported}");
        assert!(
            !imported.contains("Premier"),
            "last accepted row is current policy: {imported}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_source_and_missing_recording_are_skipped_and_warned_while_valid_text_imports() {
        let root = project();
        seed_import_transaction_files(&root);
        let mut rows = edited_rows(&root);
        let hello = rows.iter_mut().find(|r| r[0] == "intro#hello").expect("hello");
        hello[3] = "Bonjour".into();
        hello[5] = "assets/localized/fr/audio/not-recorded.ogg".into();
        let bye = rows.iter_mut().find(|r| r[0] == "intro#bye").expect("bye");
        bye[3] = "Adieu".into();
        bye[7] = "Old source".into();
        let csv = csv_for(&rows);
        let (po, missing) = sheet_to_po(&root, "fr", &csv).expect("sheet");
        assert_eq!(missing, ["assets/localized/fr/audio/not-recorded.ogg"]);
        let plan = regia_ui::loc_po::plan_import(&po, &regia_ui::loc_po::ImportContext {
            locale: "fr".into(),
            source: "en".into(),
            source_files: vec![("main.ftl".into(), "dlg-intro-hello = Hello, traveler.\ndlg-intro-hello-short = Hello!\ndlg-intro-bye = Bye.\n".into())],
            gathered: None,
            existing_imported: Some("dlg-fr-existing = Deja traduit.\n".into()),
        });
        assert_eq!(plan.report.skipped_with("loc_po_stale"), 1);
        assert!(invoke_import(&root, &csv, false).expect("command"));
        let imported =
            std::fs::read_to_string(root.join("assets/localization/fr/imported.ftl")).expect("imported");
        assert!(imported.contains("dlg-intro-hello = Bonjour"), "{imported}");
        assert!(
            !imported.contains("dlg-intro-bye"),
            "stale text is skipped: {imported}"
        );
        assert!(
            !root.join("assets/localized/fr/audio/not-recorded.ogg").exists(),
            "a missing recording is reported but never synthesized"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rich_csv_text_round_trips_fluent_selects_and_import_is_idempotent() {
        let root = project_with_source(
            b"dlg-intro-hello = Hello, { $name }! { $count ->\n    [one] item\n   *[other] items\n}\ndlg-intro-hello-short = Hello!\ndlg-intro-bye = Bye.\n",
        );
        seed_import_transaction_files(&root);
        let mut rows = edited_rows(&root);
        let hello = rows.iter_mut().find(|r| r[0] == "intro#hello").expect("hello");
        hello[3] = "Bonjour, \"ami\" { $name }\n{ $count ->\n    [one] ami\n   *[other] amis\n}".into();
        hello[4] = "Ligne, \"courte\"\nseconde ligne".into();
        let csv = csv_for(&rows);
        let (po, _) = sheet_to_po(&root, "fr", &csv).expect("RFC 4180 multiline cells");
        assert!(
            po.entries
                .iter()
                .any(|e| e.ctxt == "dlg-intro-hello" && e.msgstr.contains("\n"))
        );
        assert!(invoke_import(&root, &csv, false).expect("command"));
        let path = root.join("assets/localization/fr/imported.ftl");
        let imported = std::fs::read(&path).expect("imported");
        assert!(
            std::str::from_utf8(&imported)
                .expect("utf8")
                .contains("{ $count ->")
        );
        let imported_text = std::str::from_utf8(&imported).expect("utf8");
        assert!(
            imported_text.contains("Bonjour, \"ami\" { $name }"),
            "{imported_text}"
        );
        assert!(
            imported_text.contains("dlg-intro-hello-short ="),
            "display translation is present: {imported_text}"
        );
        assert!(
            imported_text.contains("Ligne, \"courte\""),
            "display punctuation survives: {imported_text}"
        );
        assert!(
            imported_text.contains("seconde ligne"),
            "display newline survives: {imported_text}"
        );
        assert!(invoke_import(&root, &csv, false).expect("repeat command"));
        assert_eq!(std::fs::read(path).expect("repeat bytes"), imported);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dialogue_keys_reach_the_gather_with_notes() {
        let root = project();
        std::fs::write(
            root.join("assets/dialogue/ranger.voice.ron"),
            r#"(format_version: 1, id: "ranger", name_key: "voice-ranger-name")"#,
        )
        .expect("write");
        let mut gathered = regia_ui::loc_gather::Gathered::default();
        add_dialogue_keys(&root, &mut gathered).expect("gather");
        for key in [
            "dlg-intro-hello",
            "dlg-intro-hello-short",
            "dlg-intro-bye",
            "voice-ranger-name",
        ] {
            assert!(
                gathered.keys.contains_key(key),
                "{key}: {:?}",
                gathered.keys.keys().collect::<Vec<_>>()
            );
        }
        let uses = &gathered.keys["dlg-intro-hello"];
        assert_eq!(uses[0].file, "assets/dialogue/intro.dialogue.ron");
        assert_eq!(uses[0].line, 2);
        let notes = gathered.notes.get("dlg-intro-hello").expect("notes");
        assert!(notes.iter().any(|n| n.starts_with("note: Warm")), "{notes:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_reports_the_dialogue_checks() {
        let root = project();
        let report = super::super::loc::validate_project(&root, false).expect("validate");
        let codes: Vec<&str> = report.findings.iter().map(|f| f.code).collect();
        assert!(
            codes.contains(&"dialogue_unknown_voice"),
            "no .voice.ron for ranger: {codes:?}"
        );
        let missing: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.code == "dialogue_missing_sound")
            .map(|f| (f.locale.as_str(), f.file.as_deref().unwrap_or_default()))
            .collect();
        assert!(missing.contains(&("en", "intro#hello")), "{missing:?}");
        assert!(
            !missing.contains(&("fr", "intro#bye")),
            "fr has the bye recording: {missing:?}"
        );
        assert!(report.fails(false), "dialogue errors fail the check");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A directory symlink under `assets/` is not walked, so a link at `..` cannot hang the walk.
    /// The recording behind a real directory is still found.
    #[cfg(unix)]
    #[test]
    fn find_file_does_not_follow_a_directory_symlink_cycle() {
        let root = regia_test_utils::unique_temp_dir("loc-sheet-cycle");
        let assets = root.join("assets");
        std::fs::create_dir_all(assets.join("audio")).expect("audio");
        std::fs::write(assets.join("audio/line.wav"), b"RIFF").expect("wav");
        std::os::unix::fs::symlink("..", assets.join("up")).expect("cycle");
        assert_eq!(find_file(&assets, "line.wav"), "audio/line.wav");
        let _ = std::fs::remove_dir_all(&root);
    }
}
