// SPDX-License-Identifier: MPL-2.0
//! `regia loc cues`: WebVTT and TTML exchange for subtitle cue tracks (spec 56 § 11, ADR-3359).
//!
//! `regia loc cues export` resolves a `.subs.ron` track (`text_key` cues through the project's
//! `Localization`, raw text as is) and writes a WebVTT or TTML file for a subtitle vendor.
//! `regia loc cues import` reads the returned file back into a `.subs.ron` track of raw-text
//! cues. Times round trip to the millisecond; see [`regia_ui::cue_exchange`] for the one
//! documented open-cue mapping.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use regia_ui::cue_exchange::{CueLimits, TimedCue};
use serde_json::json;

use super::loc::{ProjectArgs, read_config, read_locales};

/// The cue file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CuesFormatArg {
    /// WebVTT (`.vtt`, W3C WebVTT).
    Webvtt,
    /// TTML 2 (`.ttml`, the `p begin/end` body subset).
    Ttml,
}

/// `regia loc cues export`.
#[derive(Debug, Args)]
pub struct CuesExportArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// The `.subs.ron` track, relative to the project (or absolute).
    #[arg(long, value_name = "FILE")]
    pub cues: PathBuf,
    /// The output format.
    #[arg(long, value_enum, default_value = "webvtt")]
    pub format: CuesFormatArg,
    /// The file to write (default `<project>/.regia/loc/<track>.<ext>`).
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// The locale `text_key` cues resolve in (default: the configured source locale).
    #[arg(long)]
    pub locale: Option<String>,
}

/// `regia loc cues import`.
#[derive(Debug, Args)]
pub struct CuesImportArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// The WebVTT or TTML file (the format is detected from the content when `--format` is absent).
    pub file: PathBuf,
    /// The format of `file` (default: detected).
    #[arg(long, value_enum)]
    pub format: Option<CuesFormatArg>,
    /// The `.subs.ron` track to write, relative to the project (default: next to `file`,
    /// `.vtt`/`.ttml` renamed to `.subs.ron`).
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}

/// `regia loc cues` subcommands.
#[derive(Debug, Subcommand)]
pub enum CuesCommand {
    /// Write a `.subs.ron` track as WebVTT or TTML, text resolved in the chosen locale.
    Export(CuesExportArgs),
    /// Read a WebVTT or TTML file back into a `.subs.ron` track of raw-text cues.
    Import(CuesImportArgs),
}

/// Runs a `regia loc cues` subcommand. Returns `false` when a check failed.
///
/// # Errors
///
/// A file cannot be read or written, or its content is refused.
pub fn run(command: &CuesCommand) -> Result<bool> {
    match command {
        CuesCommand::Export(args) => export(args),
        CuesCommand::Import(args) => import(args),
    }
}

/// The `Localization` of a project, built the way the runtime combines files: each locale's
/// `.ftl` files merged in name order.
fn project_localization(project: &Path, locale: &str) -> Result<Option<regia_ui::Localization>> {
    let config = read_config(project)?;
    let locales = read_locales(project, &config)?;
    if !locales.iter().any(|(p, _)| p.locale == locale) {
        return Ok(None);
    }
    let merged: Vec<(String, String)> = locales
        .iter()
        .map(|(p, f)| {
            (
                p.locale.clone(),
                f.iter()
                    .map(|(_, text)| text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        })
        .collect();
    let source = if config.source.is_empty() {
        "en"
    } else {
        config.source.as_str()
    };
    let mut loc =
        regia_ui::Localization::new(merged, Some(source.to_owned())).map_err(|e| anyhow::anyhow!("{e}"))?;
    loc.set_fallbacks(config.fallbacks);
    Ok(Some(loc))
}

fn export(args: &CuesExportArgs) -> Result<bool> {
    let project = &args.common.project;
    let track_path = project.join(&args.cues);
    let text =
        std::fs::read_to_string(&track_path).with_context(|| format!("reading {}", track_path.display()))?;
    let track = regia_asset::dialogue::parse_subtitles(&track_path.display().to_string(), &text)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut cues = regia_ui::cue_exchange::track_to_timed(&track);
    // Resolve text_key cues through the project's localization in the chosen locale.
    let locale = args
        .locale
        .clone()
        .unwrap_or_else(|| read_config(project).map_or_else(|_| "en".to_owned(), |c| c.source));
    let Some(loc) = project_localization(project, &locale)? else {
        bail!("loc_unknown_locale: {locale} is not a locale of this project");
    };
    // The chain the runtime walks for this locale (`fr-CA` -> `fr` -> configured edges -> source).
    let chain = loc.chain(&locale);
    let mut resolved = 0usize;
    let mut unresolved: Vec<String> = Vec::new();
    for (cue, raw) in cues.iter_mut().zip(&track.cues) {
        if let Some(key) = &raw.text_key {
            match chain.iter().find_map(|l| loc.resolve_in(l, key, &[])) {
                Some(value) => {
                    cue.text = value;
                    resolved += 1;
                }
                None => {
                    cue.text = loc.resolve(key, &[]);
                    unresolved.push(key.clone());
                }
            }
        }
    }
    if !unresolved.is_empty() && !args.common.json {
        eprintln!(
            "warning: {} key(s) not found in {locale} or its fallbacks: {}",
            unresolved.len(),
            unresolved.join(", ")
        );
    }
    let stem = track_path
        .file_name()
        .map_or("cues".to_owned(), |n| n.to_string_lossy().into_owned());
    let stem = stem.strip_suffix(".subs.ron").unwrap_or(&stem).to_owned();
    let extension = match args.format {
        CuesFormatArg::Webvtt => "vtt",
        CuesFormatArg::Ttml => "ttml",
    };
    let out = args.out.clone().unwrap_or_else(|| {
        project
            .join(".regia")
            .join("loc")
            .join(format!("{stem}.{extension}"))
    });
    let written = match args.format {
        CuesFormatArg::Webvtt => regia_ui::cue_exchange::to_webvtt(&cues),
        CuesFormatArg::Ttml => regia_ui::cue_exchange::to_ttml(&cues, &locale),
    };
    super::loc_exchange::write_atomic(&out, &written)?;
    if args.common.json {
        println!(
            "{}",
            json!({
                "file": out.display().to_string(),
                "cues": cues.len(),
                "resolved_keys": resolved,
                "unresolved_keys": unresolved,
                "format": format!("{:?}", args.format).to_lowercase(),
            })
        );
    } else {
        println!(
            "wrote {} cue(s) to {} ({} key(s) resolved in {locale})",
            cues.len(),
            out.display(),
            resolved
        );
    }
    Ok(true)
}

fn import(args: &CuesImportArgs) -> Result<bool> {
    let text =
        std::fs::read_to_string(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let limits = CueLimits::default();
    let format = match args.format {
        Some(f) => f,
        None if looks_like_ttml(&text) => CuesFormatArg::Ttml,
        None => CuesFormatArg::Webvtt,
    };
    let cues: Vec<TimedCue> = match format {
        CuesFormatArg::Webvtt => {
            regia_ui::cue_exchange::read_webvtt(&text, &limits).map_err(|e| anyhow::anyhow!("{e}"))?
        }
        CuesFormatArg::Ttml => {
            regia_ui::cue_exchange::read_ttml(&text, &limits).map_err(|e| anyhow::anyhow!("{e}"))?
        }
    };
    if cues.is_empty() {
        bail!("{} holds no cues", args.file.display());
    }
    let track = regia_ui::cue_exchange::timed_to_track(&cues);
    let serialized = ron::ser::to_string_pretty(&track, default_ron())
        .map_err(|e| anyhow::anyhow!("serializing the track: {e}"))?;
    let out = args
        .out
        .clone()
        .map(|o| args.common.project.join(o))
        .unwrap_or_else(|| {
            let mut o = args.file.clone();
            o.set_extension("subs.ron");
            o
        });
    // An existing track is never replaced by a different one, in `--json` mode too: the default
    // output is the `.subs.ron` next to the cue file, whose `text_key` cues would turn into raw text.
    // A track that cannot be read back as text counts as different.
    if out.exists() && !std::fs::read_to_string(&out).is_ok_and(|old| old == serialized) {
        bail!(
            "{} exists with different content; pass --out to write elsewhere",
            out.display()
        );
    }
    super::loc_exchange::write_atomic(&out, &serialized)?;
    if args.common.json {
        println!(
            "{}",
            json!({ "file": out.display().to_string(), "cues": cues.len() })
        );
    } else {
        println!("wrote {} cue(s) to {}", cues.len(), out.display());
    }
    Ok(true)
}

fn default_ron() -> ron::ser::PrettyConfig {
    regia_core::ron_pretty().struct_names(false).compact_arrays(true)
}

/// True when the first element, after a BOM, XML declarations and comments, is a TTML `tt`
/// root. `to_ttml` writes `<?xml ...?>` before `<tt`, so a prefix check on the raw text
/// would send this crate's own export through the WebVTT parser. `<ttml` is not a `tt` root.
fn looks_like_ttml(text: &str) -> bool {
    let mut rest = text.trim_start_matches('\u{feff}');
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("<?") {
            let Some(end) = after.find("?>") else {
                return false;
            };
            rest = &after[end + 2..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("<!--") {
            let Some(end) = after.find("-->") else {
                return false;
            };
            rest = &after[end + 3..];
            continue;
        }
        if rest.len() >= 9 && rest.is_char_boundary(9) && rest[..9].eq_ignore_ascii_case("<!DOCTYPE") {
            // A DOCTYPE, with or without an internal subset (`[ … ]>`).
            let after = &rest[9..];
            let end = match (after.find('['), after.find('>')) {
                (Some(open), Some(close)) if open < close => after.find("]>").map(|at| at + 2),
                (_, Some(close)) => Some(close + 1),
                _ => None,
            };
            let Some(end) = end else {
                return false;
            };
            rest = &after[end..];
            continue;
        }
        break;
    }
    let Some(after) = rest.strip_prefix("<tt") else {
        return false;
    };
    matches!(
        after.as_bytes().first().copied(),
        Some(b' ' | b'>' | b'\t' | b'\n' | b'\r' | b'/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Corr-low (cli LOW): a TTML file with a `<!DOCTYPE …>` line was not recognized and was
    /// parsed as WebVTT when `--format` was omitted (`read_ttml` itself reads it fine).
    #[test]
    fn corr_low_a_doctype_before_the_root_is_still_ttml() {
        let text = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE tt>\n<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div><p begin=\"00:00:01.000\" end=\"00:00:02.000\">Hi</p></div></body></tt>\n";
        assert!(looks_like_ttml(text));
        assert!(looks_like_ttml("<!doctype tt [ <!ENTITY a \"b\"> ]>\n<tt>"));
        assert!(!looks_like_ttml("<!DOCTYPE html>\n<html>"));
        assert!(!looks_like_ttml("WEBVTT\n\n00:01.000 --> 00:02.000\nHi\n"));
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    fn fixture(root: &Path) {
        write(
            root,
            "assets/localization/en/main.ftl",
            "dlg-a-hi = Hello there\n",
        );
        write(
            root,
            "assets/audio/a.subs.ron",
            "(format_version: 1, priority: 1, keep_breaks: false, one_at_a_time: false, cues: [\n    (at: 0.5, until: Some(2.2), text_key: Some(\"dlg-a-hi\"), text: None),\n    (at: 2.4, until: None, text_key: None, text: Some(\"raw\")),\n])",
        );
    }

    #[test]
    fn export_resolves_keys_and_import_round_trips() {
        let root = regia_test_utils::unique_temp_dir("cli-loc-cues");
        fixture(&root);
        for format in [CuesFormatArg::Webvtt, CuesFormatArg::Ttml] {
            let ok = run(&CuesCommand::Export(CuesExportArgs {
                common: ProjectArgs {
                    project: root.clone(),
                    json: true,
                },
                cues: PathBuf::from("assets/audio/a.subs.ron"),
                format,
                out: None,
                locale: None,
            }))
            .expect("export");
            assert!(ok);
            let ext = match format {
                CuesFormatArg::Webvtt => "vtt",
                CuesFormatArg::Ttml => "ttml",
            };
            let file = root.join(".regia/loc").join(format!("a.{ext}"));
            let text = std::fs::read_to_string(&file).expect("file");
            if format == CuesFormatArg::Webvtt {
                assert!(text.contains("Hello there"), "{text}");
            } else {
                assert!(text.contains("Hello there"), "{text}");
                assert!(text.contains("xml:lang=\"en\""));
            }
            // Import back: times to the millisecond, resolved text now raw.
            let out = root.join(format!("back-{ext}.subs.ron"));
            let ok = run(&CuesCommand::Import(CuesImportArgs {
                common: ProjectArgs {
                    project: root.clone(),
                    json: true,
                },
                file: file.clone(),
                format: Some(format),
                out: Some(out.clone()),
            }))
            .expect("import");
            assert!(ok);
            let track = regia_asset::dialogue::parse_subtitles(
                &out.display().to_string(),
                &std::fs::read_to_string(&out).expect("track"),
            )
            .expect("track parses");
            assert_eq!(track.cues.len(), 2);
            assert!((f64::from(track.cues[0].at) - 0.5).abs() < 1e-6);
            assert!((f64::from(track.cues[0].until.unwrap_or(0.0)) - 2.2).abs() < 1e-3);
            assert_eq!(track.cues[0].text.as_deref(), Some("Hello there"));
            // The open last cue (until: None) round-trips open.
            assert_eq!(track.cues[1].until, None);
            assert_eq!(track.cues[1].text.as_deref(), Some("raw"));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_refuses_malformed_files_with_documented_codes() {
        let root = regia_test_utils::unique_temp_dir("cli-loc-cues-bad");
        let vtt = root.join("bad.vtt");
        std::fs::write(&vtt, "nope\n").expect("vtt");
        let err = run(&CuesCommand::Import(CuesImportArgs {
            common: ProjectArgs {
                project: root.clone(),
                json: false,
            },
            file: vtt,
            format: None,
            out: None,
        }))
        .expect_err("refused");
        assert!(err.to_string().contains("webvtt_header"), "{err}");
        let ttml = root.join("bad.ttml");
        std::fs::write(
            &ttml,
            "<tt><body><div><p begin=\"x\" end=\"1:00:00.000\">t</p></div></body></tt>",
        )
        .expect("ttml");
        let err = run(&CuesCommand::Import(CuesImportArgs {
            common: ProjectArgs {
                project: root.clone(),
                json: false,
            },
            file: ttml,
            format: Some(CuesFormatArg::Ttml),
            out: None,
        }))
        .expect_err("refused");
        assert!(err.to_string().contains("ttml_time"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Export resolves a key through the locale's fallback chain: `fr-CA` without the key takes
    /// `fr`'s text, not the source locale's; a key missing everywhere is not counted as resolved.
    #[test]
    fn audit_export_resolves_through_the_locale_chain() {
        let root = regia_test_utils::unique_temp_dir("cli-loc-cues-chain");
        fixture(&root);
        write(&root, "assets/localization/fr/main.ftl", "dlg-a-hi = Bonjour\n");
        write(&root, "assets/localization/fr-CA/main.ftl", "other = Autre\n");
        write(
            &root,
            "assets/audio/b.subs.ron",
            "(format_version: 1, priority: 1, keep_breaks: false, one_at_a_time: false, cues: [\n    (at: 0.5, until: Some(2.2), text_key: Some(\"dlg-a-hi\"), text: None),\n    (at: 2.4, until: Some(3.0), text_key: Some(\"dlg-missing\"), text: None),\n])",
        );
        let out = root.join("b.vtt");
        run(&CuesCommand::Export(CuesExportArgs {
            common: ProjectArgs {
                project: root.clone(),
                json: true,
            },
            cues: PathBuf::from("assets/audio/b.subs.ron"),
            format: CuesFormatArg::Webvtt,
            out: Some(out.clone()),
            locale: Some("fr-CA".to_owned()),
        }))
        .expect("export");
        let text = std::fs::read_to_string(&out).expect("vtt");
        assert!(text.contains("Bonjour"), "{text}");
        assert!(!text.contains("Hello there"), "{text}");
        let loc = project_localization(&root, "fr-CA")
            .expect("loc")
            .expect("fr-CA loaded");
        assert_eq!(loc.chain("fr-CA").first().map(String::as_str), Some("fr-CA"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `--json` (the agent path) must not replace an existing, different track either: importing
    /// `a.vtt` next to `a.subs.ron` would turn its `text_key` cues into raw text.
    #[test]
    fn audit_json_import_never_replaces_an_existing_track() {
        let root = regia_test_utils::unique_temp_dir("cli-loc-cues-json-keep");
        fixture(&root);
        let track = root.join("assets/audio/a.subs.ron");
        let before = std::fs::read_to_string(&track).expect("track");
        write(
            &root,
            "assets/audio/a.vtt",
            "WEBVTT\n\n00:00:00.500 --> 00:00:02.200\nHello there\n",
        );
        for json in [true, false] {
            let err = run(&CuesCommand::Import(CuesImportArgs {
                common: ProjectArgs {
                    project: root.clone(),
                    json,
                },
                file: root.join("assets/audio/a.vtt"),
                format: None,
                out: None,
            }))
            .expect_err("an existing, different track is kept");
            assert!(err.to_string().contains("exists with different content"), "{err}");
            assert_eq!(
                std::fs::read_to_string(&track).expect("track"),
                before,
                "json {json}"
            );
        }
        // A track that is not UTF-8 is not replaced either.
        std::fs::write(&track, [0xff_u8, 0xfe, 0x00]).expect("binary track");
        assert!(
            run(&CuesCommand::Import(CuesImportArgs {
                common: ProjectArgs {
                    project: root.clone(),
                    json: false,
                },
                file: root.join("assets/audio/a.vtt"),
                format: None,
                out: None,
            }))
            .is_err()
        );
        assert_eq!(std::fs::read(&track).expect("track"), [0xff_u8, 0xfe, 0x00]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `to_ttml` writes an XML declaration. Auto-detect must still treat that file as TTML.
    #[test]
    fn import_detects_ttml_that_starts_with_an_xml_declaration() {
        let root = regia_test_utils::unique_temp_dir("cli-loc-cues-ttml-decl");
        fixture(&root);
        run(&CuesCommand::Export(CuesExportArgs {
            common: ProjectArgs {
                project: root.clone(),
                json: true,
            },
            cues: PathBuf::from("assets/audio/a.subs.ron"),
            format: CuesFormatArg::Ttml,
            out: None,
            locale: None,
        }))
        .expect("export");
        let file = root.join(".regia/loc/a.ttml");
        let text = std::fs::read_to_string(&file).expect("ttml");
        assert!(text.starts_with("<?xml"), "{text}");
        let out = root.join("from-decl.subs.ron");
        run(&CuesCommand::Import(CuesImportArgs {
            common: ProjectArgs {
                project: root.clone(),
                json: false,
            },
            file,
            format: None,
            out: Some(out.clone()),
        }))
        .expect("declaration is still TTML");
        let track = regia_asset::dialogue::parse_subtitles(
            &out.display().to_string(),
            &std::fs::read_to_string(&out).expect("track"),
        )
        .expect("track parses");
        assert_eq!(track.cues[0].text.as_deref(), Some("Hello there"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A locale the project does not have is an error. It must not write a file of placeholders
    /// and report those keys as resolved.
    #[test]
    fn export_refuses_a_locale_the_project_does_not_have() {
        let root = regia_test_utils::unique_temp_dir("cli-loc-cues-unknown");
        fixture(&root);
        let err = run(&CuesCommand::Export(CuesExportArgs {
            common: ProjectArgs {
                project: root.clone(),
                json: true,
            },
            cues: PathBuf::from("assets/audio/a.subs.ron"),
            format: CuesFormatArg::Webvtt,
            out: None,
            locale: Some("zz".to_owned()),
        }))
        .expect_err("unknown locale");
        assert!(err.to_string().contains("unknown_locale"), "{err}");
        assert!(
            !root.join(".regia/loc/a.vtt").exists(),
            "nothing is written for a locale the project does not have"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
