// SPDX-License-Identifier: MPL-2.0
//! `regia loc export-vo`, `regia loc vo-report` and `regia loc validate-conv`: the voice-over
//! script, the missing-VO report and the conversation validator
//! (`docs/specs/56-dialogue-and-subtitles.md` § 6, ADR-1697).
//!
//! The logic is `regia::framework::conversation`; this module reads the project, prints and
//! writes files (atomically: a temp file, then a rename).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use regia::framework::conversation::export::{self, ReportStatus};
use regia::framework::conversation::validate::{ValidateInput, validate};
use regia::framework::conversation::{ConversationLibrary, LibraryValidateEnv};
use regia_ui::localization::Localization;

use super::loc::{LoadedLocale, ProjectArgs, read_config, read_locales};

/// `regia loc export-vo`.
#[derive(Debug, Args)]
pub struct ExportVoArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// The locale to export for.
    #[arg(long)]
    pub locale: String,
    /// Only these conversations (repeat the flag); default all.
    #[arg(long = "conversation")]
    pub conversations: Vec<String>,
    /// The CSV file to write.
    #[arg(long, value_name = "FILE")]
    pub out: PathBuf,
    /// Prefix the file with a UTF-8 byte order mark (for spreadsheet imports).
    #[arg(long, default_value_t = false)]
    pub bom: bool,
    /// Print what would be written; write nothing.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

/// Output format of `vo-report`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ReportFormat {
    /// Human-readable lines.
    Text,
    /// RFC 4180 CSV.
    Csv,
    /// JSON.
    Json,
}

/// `regia loc vo-report`.
#[derive(Debug, Args)]
pub struct VoReportArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// Locales to report (repeat the flag); default every locale.
    #[arg(long = "locale")]
    pub locales: Vec<String>,
    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    pub format: ReportFormat,
    /// Exit non-zero when any voiced line is missing a clip.
    #[arg(long, default_value_t = false)]
    pub require: bool,
}

/// `regia loc validate-conv`.
#[derive(Debug, Args)]
pub struct ValidateConvArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// Warnings are errors.
    #[arg(long, default_value_t = false)]
    pub strict: bool,
    /// Only this conversation.
    #[arg(long)]
    pub conversation: Option<String>,
    /// Only this locale.
    #[arg(long)]
    pub locale: Option<String>,
}

/// A project read for the conversation tools.
pub struct ConvProject {
    /// The source locale.
    pub source: String,
    /// The locales (sorted).
    pub locales: Vec<String>,
    /// The conversations, speakers, settings and clips.
    pub library: ConversationLibrary,
    /// The localization over the project's `.ftl` files.
    pub loc: Localization,
}

/// Reads a project's conversations and locales.
///
/// # Errors
///
/// A file cannot be read or does not parse.
pub fn load_project(project: &Path) -> Result<ConvProject> {
    let config = read_config(project)?;
    let loaded: Vec<LoadedLocale> = read_locales(project, &config)?;
    let mut sources = Vec::new();
    let mut locales = Vec::new();
    for (parsed, files) in &loaded {
        let text: String = files.iter().map(|(_, t)| format!("{t}\n")).collect();
        sources.push((parsed.locale.clone(), text));
        locales.push(parsed.locale.clone());
    }
    let mut loc = Localization::new(sources, Some(config.source.clone()))
        .map_err(|e| anyhow::anyhow!("localization: {e}"))?;
    loc.set_fallbacks(config.fallbacks.clone());
    let library =
        ConversationLibrary::load_assets(&project.join("assets")).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(ConvProject {
        source: config.source,
        locales,
        library,
        loc,
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}-{seq}", std::process::id()));
    std::fs::write(&tmp, bytes)
        .and_then(|()| std::fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
        .with_context(|| format!("writing {}", path.display()))
}

/// `regia loc export-vo`.
///
/// # Errors
///
/// The project cannot be read, the locale is unknown or the file cannot be written.
pub fn export_vo(args: &ExportVoArgs) -> Result<bool> {
    let project = load_project(&args.common.project)?;
    if !project.locales.contains(&args.locale) {
        bail!("no locale `{}` under assets/localization", args.locale);
    }
    for id in &args.conversations {
        if !project.library.conversations.contains_key(id) {
            bail!("no conversation `{id}`");
        }
    }
    let env = LibraryValidateEnv {
        library: &project.library,
        loc: &project.loc,
    };
    let rows = export::vo_rows(
        &project.library,
        &env,
        &project.source,
        &args.locale,
        &args.conversations,
    );
    let csv = export::vo_csv(&rows, args.bom);
    if !args.dry_run {
        write_atomic(&args.out, csv.as_bytes())?;
    }
    if args.common.json {
        println!(
            "{}",
            serde_json::json!({
                "rows": rows.len(),
                "out": args.out.display().to_string(),
                "written": !args.dry_run,
            })
        );
    } else {
        let verb = if args.dry_run { "would write" } else { "wrote" };
        println!("{verb} {} row(s) to {}", rows.len(), args.out.display());
    }
    Ok(true)
}

/// `regia loc vo-report`. Returns `false` with `--require` when a clip is missing.
///
/// # Errors
///
/// The project cannot be read.
pub fn vo_report(args: &VoReportArgs) -> Result<bool> {
    let project = load_project(&args.common.project)?;
    let locales = if args.locales.is_empty() {
        project.locales.clone()
    } else {
        args.locales.clone()
    };
    let env = LibraryValidateEnv {
        library: &project.library,
        loc: &project.loc,
    };
    let reports = export::vo_report(&project.library, &env, &project.source, &locales);
    let missing: usize = reports.iter().map(|r| r.count(ReportStatus::Missing)).sum();
    if args.common.json {
        println!("{}", export::report_json(&reports));
    } else {
        match args.format {
            ReportFormat::Text => print!("{}", export::report_text(&reports)),
            ReportFormat::Csv => print!("{}", export::report_csv(&reports)),
            ReportFormat::Json => println!("{}", export::report_json(&reports)),
        }
    }
    Ok(!(args.require && missing > 0))
}

/// `regia loc validate-conv`. Returns `false` when an error exists.
///
/// # Errors
///
/// The project cannot be read.
pub fn validate_conv(args: &ValidateConvArgs) -> Result<bool> {
    let project = load_project(&args.common.project)?;
    let conversations: Vec<_> = project
        .library
        .conversations
        .values()
        .filter(|c| args.conversation.as_ref().is_none_or(|id| *id == c.id))
        .cloned()
        .collect();
    if let Some(id) = &args.conversation
        && conversations.is_empty()
    {
        bail!("no conversation `{id}`");
    }
    let locales: Vec<String> = match &args.locale {
        Some(l) if project.locales.contains(l) => vec![l.clone()],
        Some(l) => bail!("no locale `{l}` under assets/localization"),
        None => project.locales.clone(),
    };
    let env = LibraryValidateEnv {
        library: &project.library,
        loc: &project.loc,
    };
    let report = validate(
        &ValidateInput {
            conversations: &conversations,
            speakers: &project.library.speakers,
            settings: &project.library.settings,
            locales: &locales,
            source_locale: &project.source,
            strict: args.strict,
        },
        &env,
    );
    if args.common.json {
        println!("{}", report.to_json());
    } else {
        for issue in &report.issues {
            println!(
                "{} {} {}#{} [{}]: {}",
                issue.severity.name(),
                issue.code,
                issue.conversation,
                issue.line,
                issue.locale,
                issue.message
            );
        }
        let (errors, warnings, infos) = report.counts();
        println!("{errors} error(s), {warnings} warning(s), {infos} info");
    }
    Ok(!report.has_errors())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write");
    }

    /// A one-second 8 kHz mono 16-bit PCM wav.
    fn wav() -> Vec<u8> {
        let samples = 8000u32;
        let data_len = samples * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&8000u32.to_le_bytes());
        out.extend_from_slice(&16000u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(out.len() + data_len as usize, 0);
        out
    }

    fn project() -> PathBuf {
        let root = regia_test_utils::unique_temp_dir("cli-conv");
        write(
            &root,
            "assets/localization/localization.ron",
            b"(format_version: 1, source: \"en\", locales: [\"en\", \"de\"])",
        );
        write(
            &root,
            "assets/localization/en/main.ftl",
            b"speaker-guard = Guard\nconv-greet = Hello, \"friend\"\nconv-warn =\n    First line\n    second line\n",
        );
        write(
            &root,
            "assets/localization/de/main.ftl",
            b"speaker-guard = Wache\nconv-greet = Hallo\n",
        );
        write(
            &root,
            "assets/conversations/speakers.ron",
            br#"(speakers: {"guard": (name_key: Some("speaker-guard"))})"#,
        );
        write(
            &root,
            "assets/conversations/conversation.ron",
            br#"(voice_path: "audio/vo/{locale}/{conversation}_{line}.wav")"#,
        );
        write(
            &root,
            "assets/conversations/intro.conv.ron",
            br#"(
    id: "intro",
    lines: [
        (id: "greet", speaker: "guard", text: "conv-greet", note: "Waving them in", emotion: Some("weary"), duration: Clip),
        (id: "warn", speaker: "guard", text: "conv-warn", duration: Fixed(2000)),
    ],
)"#,
        );
        write(&root, "assets/audio/vo/en/intro_greet.wav", &wav());
        write(&root, "assets/audio/vo/de/intro_greet.wav", &wav());
        write(&root, "assets/audio/vo/en/intro_extra.wav", &wav());
        root
    }

    fn common(root: &Path) -> ProjectArgs {
        ProjectArgs {
            project: root.to_path_buf(),
            json: false,
        }
    }

    #[test]
    fn export_vo_writes_rfc4180_csv() {
        let root = project();
        let out = root.join("vo-en.csv");
        let args = ExportVoArgs {
            common: common(&root),
            locale: "en".into(),
            conversations: Vec::new(),
            out: out.clone(),
            bom: false,
            dry_run: false,
        };
        assert!(export_vo(&args).expect("export"));
        let text = std::fs::read_to_string(&out).expect("read");
        let lines: Vec<&str> = text.split("\r\n").collect();
        assert_eq!(
            lines[0],
            "conversation,line,speaker,speaker_name,locale,text,fallback_text,note,emotion,tags,chars,max_ms,filename,status"
        );
        // Embedded comma and quotes are quoted and doubled; the note has no special characters.
        assert_eq!(
            lines[1],
            "intro,greet,guard,Guard,en,\"Hello, \"\"friend\"\"\",false,Waving them in,weary,,15,1000,intro_greet.wav,recorded"
        );
        // The multi-line text is quoted with its newline kept inside the field.
        assert!(
            lines[2].starts_with("intro,warn,guard,Guard,en,\"First line"),
            "{}",
            lines[2]
        );
        assert!(text.contains("second line\",false,"), "{text}");
        assert!(text.ends_with("\r\n"));
        assert!(lines.last().is_some_and(|l| l.is_empty()));
        assert!(text.ends_with("missing\r\n"), "{text}");

        // German: speaker name and text come from de, the second line falls back to the source text.
        let de = root.join("vo-de.csv");
        let mut german = args;
        german.locale = "de".into();
        german.out = de.clone();
        german.bom = true;
        assert!(export_vo(&german).expect("export de"));
        let bytes = std::fs::read(&de).expect("read de");
        assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF]);
        let de_text = String::from_utf8_lossy(&bytes[3..]).into_owned();
        assert!(
            de_text.contains("intro,greet,guard,Wache,de,Hallo,false,"),
            "{de_text}"
        );
        assert!(
            de_text.contains(",true,"),
            "the fallback text row says fallback_text=true: {de_text}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn export_vo_is_stable_and_dry_run_writes_nothing() {
        let root = project();
        let out = root.join("vo.csv");
        let mut args = ExportVoArgs {
            common: common(&root),
            locale: "en".into(),
            conversations: Vec::new(),
            out: out.clone(),
            bom: false,
            dry_run: true,
        };
        export_vo(&args).expect("dry run");
        assert!(!out.exists());
        args.dry_run = false;
        export_vo(&args).expect("first");
        let first = std::fs::read(&out).expect("read");
        export_vo(&args).expect("second");
        assert_eq!(first, std::fs::read(&out).expect("read again"));
        args.locale = "xx".into();
        assert!(export_vo(&args).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn vo_report_counts_and_exit_code() {
        let root = project();
        let project = load_project(&root).expect("load");
        let env = LibraryValidateEnv {
            library: &project.library,
            loc: &project.loc,
        };
        let reports = export::vo_report(
            &project.library,
            &env,
            "en",
            &["en".to_string(), "de".to_string()],
        );
        let en = &reports[0];
        assert_eq!(en.count(ReportStatus::Ok), 1);
        assert_eq!(en.count(ReportStatus::Missing), 1);
        assert_eq!(en.count(ReportStatus::Orphan), 1);
        // Line entries carry the full expected path, the same form an orphan carries.
        assert!(
            en.entries.iter().any(|e| e.clip == "audio/vo/en/intro_greet.wav"),
            "{:?}",
            en.entries
        );
        assert!((en.coverage_percent() - 50.0).abs() < 1e-9);
        let de = &reports[1];
        assert_eq!(de.count(ReportStatus::Ok), 1);
        assert_eq!(de.count(ReportStatus::Missing), 1);
        assert_eq!(de.count(ReportStatus::Orphan), 0);
        let args = |require| VoReportArgs {
            common: common(&root),
            locales: Vec::new(),
            format: ReportFormat::Json,
            require,
        };
        assert!(vo_report(&args(false)).expect("report"));
        assert!(!vo_report(&args(true)).expect("report required"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn gather_adds_conversation_keys_with_comments_and_is_idempotent() {
        let root = project();
        let args = super::super::loc::GatherArgs {
            common: common(&root),
            dry_run: false,
            charset_out: None,
        };
        // `de` lacks conv-warn; the gather appends it with the cast comments.
        assert!(super::super::loc::run(&super::super::loc::LocCommand::Gather(args)).expect("gather"));
        let gathered =
            std::fs::read_to_string(root.join("assets/localization/de/gathered.ftl")).expect("gathered");
        assert!(gathered.contains("# speaker: guard"), "{gathered}");
        assert!(gathered.contains("conv-warn"), "{gathered}");
        assert!(gathered.contains("# budget: 34 chars"), "{gathered}");
        let again = super::super::loc::GatherArgs {
            common: common(&root),
            dry_run: false,
            charset_out: None,
        };
        super::super::loc::run(&super::super::loc::LocCommand::Gather(again)).expect("gather again");
        let second =
            std::fs::read_to_string(root.join("assets/localization/de/gathered.ftl")).expect("gathered");
        assert_eq!(gathered, second);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn validate_conv_reports_and_sets_the_exit_code() {
        let root = project();
        let args = |strict| ValidateConvArgs {
            common: common(&root),
            strict,
            conversation: None,
            locale: None,
        };
        // Missing clips are warnings (policy warn) and untranslated text is a warning: no errors.
        assert!(validate_conv(&args(false)).expect("validate"));
        // --strict promotes the warnings to errors.
        assert!(!validate_conv(&args(true)).expect("strict"));
        let bad = ValidateConvArgs {
            conversation: Some("nope".into()),
            ..args(false)
        };
        assert!(validate_conv(&bad).is_err());
        std::fs::remove_dir_all(&root).ok();
    }
}
