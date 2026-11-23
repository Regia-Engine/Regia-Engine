// SPDX-License-Identifier: MPL-2.0
//! `regia loc check-fit`: headless text-fit checking (ADR-3359).
//!
//! Lays out every text node of every `.ui.ron` document with each locale through the runtime
//! layout (same shaping, same taffy conversion) without a GPU and reports every overflow or
//! clip with key, document, node path, locale, size and measured excess.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use regia_ui::authoring::UiMigrators;
use regia_ui::loc_fit::{FitReport, FitSize};
use regia_ui::{TextSystem, parse_ui_ron};
use serde_json::{Value, json};

use super::loc::{LOCALIZATION_DIR, ProjectArgs, read_config, read_locales};

/// `regia loc check-fit`.
#[derive(Debug, Args)]
pub struct CheckFitArgs {
    /// Project and output options.
    #[command(flatten)]
    pub common: ProjectArgs,
    /// A viewport size to check at, `WxH` (`--size 1920x1080 --size 1280x720`). Default:
    /// 1920x1080 and 1280x720.
    #[arg(long, value_name = "WxH")]
    pub size: Vec<String>,
    /// Check only these locales (default: every locale, the pseudo-locale included).
    #[arg(long, value_delimiter = ',')]
    pub locale: Vec<String>,
    /// Check only these documents, relative to the project (default: every `.ui.ron` under
    /// `assets/`).
    #[arg(long, value_name = "FILE")]
    pub document: Vec<PathBuf>,
}

fn parse_size(text: &str) -> Option<FitSize> {
    let (w, h) = text.split_once(['x', 'X'])?;
    let (w, h): (f32, f32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0).then_some(FitSize { width: w, height: h })
}

/// Every `.ui.ron` under `assets/`, sorted (deterministic findings). A symlink entry is
/// skipped: following a directory link would read a tree outside the project, and a link
/// back at a parent would recurse without bound. A symlink at `assets/` itself is still
/// the project's asset root and is listed.
fn ui_documents(project: &Path) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(dir) {
            Ok(read) => read.filter_map(Result::ok).collect(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| format!("listing {}", dir.display()));
            }
        };
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            // `file_type` does not follow symlinks.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                walk(&path, out)?;
            } else if path.to_string_lossy().ends_with(".ui.ron") {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    let assets = project.join("assets");
    // `read_dir` follows a symlink at the assets root. Child links are skipped above.
    if assets.symlink_metadata().is_ok_and(|meta| meta.is_dir()) || assets.is_dir() {
        walk(&assets, &mut out)?;
    }
    Ok(out)
}

/// The project's font files (`assets/fonts`), best effort: a file that does not load is
/// skipped, the embedded set stays the floor.
fn load_project_fonts(text_system: &mut TextSystem, project: &Path) {
    fn walk(dir: &Path, fonts: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                walk(&path, fonts);
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| ["ttf", "otf"].contains(&e.to_ascii_lowercase().as_str()))
            {
                fonts.push(path);
            }
        }
    }
    let mut fonts = Vec::new();
    walk(&project.join("assets/fonts"), &mut fonts);
    fonts.sort();
    for path in fonts {
        if let Ok(data) = std::fs::read(&path) {
            text_system.load_font(data.into());
        }
    }
}

/// The findings as JSON (the shape `regia loc check-fit --json` prints).
#[must_use]
pub fn fit_json(reports: &[FitReport]) -> Value {
    let findings: Vec<&regia_ui::loc_fit::FitFinding> =
        reports.iter().flat_map(|r| r.findings.iter()).collect();
    json!({
        "ok": findings.is_empty(),
        "findings": findings.iter().map(|f| json!({
            "document": f.document,
            "node": f.node,
            "key": f.key,
            "locale": f.locale,
            "size": [f.size.width, f.size.height],
            "kind": match f.kind {
                regia_ui::loc_fit::FitKind::WidthOverflow => "width_overflow",
                regia_ui::loc_fit::FitKind::HeightOverflow => "height_overflow",
            },
            "measured": f.measured,
            "box": f.box_size,
            "excess": f.excess,
        })).collect::<Vec<_>>(),
        "nodes_checked": reports.iter().map(|r| r.nodes_checked).sum::<usize>(),
    })
}

/// The project's `Localization` as the runtime builds it: each locale's files merged, the
/// configured `source` as the last link of every chain and the configured extra fallback edges, so
/// a key missing in one locale is measured with the text the game would show.
fn fit_localization(
    config: &regia_ui::loc_config::LocalizationConfig,
    locales: &[super::loc::LoadedLocale],
) -> Result<regia_ui::Localization> {
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
    let mut loc = match regia_ui::Localization::new(merged, Some(source.to_owned())) {
        Ok(l) => l,
        Err(e) => bail!("{e}"),
    };
    loc.set_fallbacks(config.fallbacks.clone());
    Ok(loc)
}

/// Runs `regia loc check-fit`. Returns `false` when any text overflowed (the caller exits 1).
///
/// # Errors
///
/// The localization, the documents or a font cannot be read, or a size is malformed.
pub fn check_fit(args: &CheckFitArgs) -> Result<bool> {
    let project = &args.common.project;
    let config = read_config(project)?;
    let locales = read_locales(project, &config)?;
    if locales.is_empty() {
        bail!("no locales: create {LOCALIZATION_DIR}/<locale>/ or list `locales` in the project config");
    }
    let sizes: Vec<FitSize> = match &args.size {
        s if s.is_empty() => vec![
            FitSize {
                width: 1920.0,
                height: 1080.0,
            },
            FitSize {
                width: 1280.0,
                height: 720.0,
            },
        ],
        s => {
            let mut out = Vec::new();
            for text in s {
                let Some(size) = parse_size(text) else {
                    bail!("--size `{text}` is not `WxH` with positive numbers");
                };
                out.push(size);
            }
            out
        }
    };
    let documents = if args.document.is_empty() {
        ui_documents(project)?
    } else {
        args.document
            .iter()
            .map(|d| {
                if d.is_absolute() {
                    Ok(d.clone())
                } else {
                    Ok(project.join(d))
                }
            })
            .collect::<Result<Vec<_>>>()?
    };
    if documents.is_empty() {
        bail!("no .ui.ron documents under {}", project.join("assets").display());
    }
    let mut sources = Vec::new();
    for path in &documents {
        let rel = path.strip_prefix(project).unwrap_or(path).display().to_string();
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        sources.push((rel, text));
    }
    let picked: Vec<String> = if args.locale.is_empty() {
        locales.iter().map(|(p, _)| p.locale.clone()).collect()
    } else {
        args.locale.clone()
    };
    let mut loc = fit_localization(&config, &locales)?;
    let mut reports = Vec::new();
    let mut errors = Vec::new();
    let mut skipped: Vec<&str> = Vec::new();
    for tag in &picked {
        let Some((_, files)) = locales.iter().find(|(p, _)| &p.locale == tag) else {
            bail!("no locale `{tag}` under {LOCALIZATION_DIR}");
        };
        if files.is_empty() {
            skipped.push(tag.as_str());
            continue;
        }
        loc.set_locale(tag);
        let mut text_system = TextSystem::embedded_only();
        text_system.set_locale(tag);
        load_project_fonts(&mut text_system, project);
        for (path, text) in &sources {
            let file = match parse_ui_ron(text, path, &UiMigrators::default()) {
                Ok(f) => f,
                Err(e) => {
                    errors.push(format!("{path}: {e}"));
                    continue;
                }
            };
            for size in &sizes {
                let report =
                    regia_ui::loc_fit::check_document(path, &file, &loc, &mut text_system, *size, 1.0)
                        .map_err(|e| anyhow::anyhow!("{path}: layout failed: {e}"))?;
                reports.push(report);
            }
        }
    }
    if !errors.is_empty() {
        bail!("documents did not parse: {}", errors.join("; "));
    }
    if skipped.len() == picked.len() {
        bail!(
            "no .ftl files in the picked locale(s) {}: nothing was checked",
            skipped.join(", ")
        );
    }
    if !skipped.is_empty() {
        eprintln!(
            "warning: locale(s) with no .ftl files were not checked: {}",
            skipped.join(", ")
        );
    }
    let total: usize = reports.iter().map(|r| r.nodes_checked).sum();
    let findings: usize = reports.iter().map(|r| r.findings.len()).sum();
    let ok = findings == 0;
    if args.common.json {
        println!("{}", fit_json(&reports));
    } else {
        let mut all: Vec<&regia_ui::loc_fit::FitFinding> =
            reports.iter().flat_map(|r| r.findings.iter()).collect();
        all.sort_by(|a, b| {
            (a.document.as_str(), a.node.as_str(), a.locale.as_str()).cmp(&(
                b.document.as_str(),
                b.node.as_str(),
                b.locale.as_str(),
            ))
        });
        for f in all {
            let key = f.key.as_deref().unwrap_or("-");
            println!(
                "{} {} [{}] {} at {:.0}x{:.0}: {} overflow by {:.1}px (measured {:.1} vs box {:.1})",
                f.document,
                f.node,
                f.locale,
                key,
                f.size.width,
                f.size.height,
                match f.kind {
                    regia_ui::loc_fit::FitKind::WidthOverflow => "width",
                    regia_ui::loc_fit::FitKind::HeightOverflow => "height",
                },
                f.excess,
                f.measured,
                f.box_size,
            );
        }
        println!(
            "{findings} overflow(s) in {total} text node(s){}",
            if ok { "" } else { ": FAILED" }
        );
    }
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    const LONG_DE: &str = "EinstellungenundnochvielmehrTextdernichtinpasst";

    fn project_with(label_style: &str, en: &str, de: &str) -> std::path::PathBuf {
        let root = regia_test_utils::unique_temp_dir("cli-loc-check-fit");
        write(
            &root,
            "assets/localization/localization.ron",
            "(format_version: 1, source: \"en\", locales: [\"en\", \"de\"])",
        );
        write(&root, "assets/localization/en/main.ftl", en);
        write(&root, "assets/localization/de/main.ftl", de);
        write(
            &root,
            "assets/ui/menu.ui.ron",
            &format!(
                "(version: 6, root: (children: [(name: \"label\", style: ({label_style}), text: (spans: [Loc(\"label-key\")], wrap: none))]))"
            ),
        );
        root
    }

    fn check_args(root: &Path, json: bool) -> CheckFitArgs {
        CheckFitArgs {
            common: ProjectArgs {
                project: root.to_path_buf(),
                json,
            },
            size: vec!["800x600".to_owned()],
            locale: Vec::new(),
            document: Vec::new(),
        }
    }

    #[test]
    fn a_german_string_too_long_for_its_box_is_reported_and_a_wider_box_fixes_it() {
        let en = "label-key = Settings\n";
        let de = &format!("label-key = {LONG_DE}\n");
        // 200 px box: the German word overflows, English fits.
        let root = project_with("size: (200, 40)", en, de);
        let ok = check_fit(&check_args(&root, true)).expect("check");
        assert!(!ok, "the overflow fails the check");
        let reports = run_collect(&root);
        let de_findings: Vec<_> = reports
            .iter()
            .flat_map(|r| r.findings.iter())
            .filter(|f| f.locale == "de")
            .collect();
        assert_eq!(de_findings.len(), 1, "{reports:?}");
        assert_eq!(de_findings[0].key.as_deref(), Some("label-key"));
        assert_eq!(de_findings[0].document, "assets/ui/menu.ui.ron");
        assert_eq!(de_findings[0].node, "root/label");
        assert!(de_findings[0].excess > 0.0);
        assert!(
            reports
                .iter()
                .flat_map(|r| r.findings.iter())
                .all(|f| f.locale != "en"),
            "English fits"
        );
        // A wider box fixes it.
        let root = project_with("size: (900, 40)", en, de);
        assert!(
            check_fit(&check_args(&root, false)).expect("check"),
            "wider box fits"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Corr-low (cli LOW): a picked locale with no `.ftl` files was skipped silently, so
    /// `--locale fr` on an empty `fr/` passed having checked nothing. That is an error now.
    #[test]
    fn corr_low_picked_locales_with_no_files_do_not_pass_unchecked() {
        let root = project_with(
            "size: (200, 40)",
            "label-key = Settings\n",
            "label-key = Einstellungen\n",
        );
        write(
            &root,
            "assets/localization/localization.ron",
            "(format_version: 1, source: \"en\", locales: [\"en\", \"de\", \"fr\"])",
        );
        std::fs::create_dir_all(root.join("assets/localization/fr")).expect("empty fr");
        let mut args = check_args(&root, true);
        args.locale = vec!["fr".to_owned()];
        match check_fit(&args) {
            Err(e) => assert!(e.to_string().contains("fr"), "{e:#}"),
            Ok(ok) => panic!("a check of nothing must not pass (returned {ok})"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cjk_and_rtl_strings_lay_out_without_false_reports() {
        for (tag, text) in [("ja", "設定とオプション"), ("ar", "الإعدادات")] {
            let root = project_with(
                "size: (400, 200)",
                "label-key = Settings\n",
                &format!("label-key = {text}\n"),
            );
            // The fixture ships only en+de; add the locale.
            write(
                &root,
                "assets/localization/localization.ron",
                "(format_version: 1, source: \"en\", locales: [\"en\", \"de\", \"PLACEHOLDER\"])",
            );
            let config =
                std::fs::read_to_string(root.join("assets/localization/localization.ron")).expect("config");
            std::fs::write(
                root.join("assets/localization/localization.ron"),
                config.replace("PLACEHOLDER", tag),
            )
            .expect("config");
            write(
                &root,
                &format!("assets/localization/{tag}/main.ftl"),
                &format!("label-key = {text}\n"),
            );
            let reports = run_collect(&root);
            assert!(
                reports
                    .iter()
                    .flat_map(|r| r.findings.iter())
                    .all(|f| f.locale != tag),
                "{tag}: {reports:?}"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// A key missing in `fr` is measured with the source locale's text (what the game shows), not
    /// with the alphabetically first locale's (`de`).
    #[test]
    fn audit_a_missing_key_is_measured_through_the_source_fallback() {
        let root = project_with(
            "size: (200, 40)",
            "label-key = Settings\n",
            &format!("label-key = {LONG_DE}\n"),
        );
        write(
            &root,
            "assets/localization/localization.ron",
            "(format_version: 1, source: \"en\", locales: [\"en\", \"de\", \"fr\"])",
        );
        write(&root, "assets/localization/fr/main.ftl", "other-key = Autre\n");
        let mut fr = check_args(&root, true);
        fr.locale = vec!["fr".to_owned()];
        assert!(
            check_fit(&fr).expect("check"),
            "fr falls back to the English text, which fits"
        );
        // A configured edge fr -> de is followed: the German text overflows.
        write(
            &root,
            "assets/localization/localization.ron",
            "(format_version: 1, source: \"en\", locales: [\"en\", \"de\", \"fr\"], fallbacks: {\"fr\": [\"de\"]})",
        );
        assert!(
            !check_fit(&fr).expect("check"),
            "the configured fr -> de edge is used"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bad_size_and_missing_locale_are_refused() {
        let root = project_with(
            "size: (200, 40)",
            "label-key = Settings\n",
            "label-key = Einstellungen\n",
        );
        let mut bad = check_args(&root, false);
        bad.size = vec!["nope".to_owned()];
        assert!(check_fit(&bad).is_err(), "bad size");
        let mut bad = check_args(&root, false);
        bad.locale = vec!["tlh".to_owned()];
        assert!(check_fit(&bad).is_err(), "unknown locale");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Runs the check the way the command does and returns the per-document reports.
    fn run_collect(root: &Path) -> Vec<FitReport> {
        let config = read_config(root).expect("config");
        let locales = read_locales(root, &config).expect("locales");
        let sources = ui_documents(root).expect("docs");
        let mut loc = fit_localization(&config, &locales).expect("loc");
        let mut ts = TextSystem::embedded_only();
        let mut out = Vec::new();
        for (parsed, files) in &locales {
            if files.is_empty() {
                continue;
            }
            let tag = parsed.locale.as_str();
            loc.set_locale(tag);
            ts.set_locale(tag);
            load_project_fonts(&mut ts, root);
            for path in &sources {
                let rel = path.strip_prefix(root).unwrap_or(path).display().to_string();
                let text = std::fs::read_to_string(path).expect("doc");
                let file = parse_ui_ron(&text, &rel, &UiMigrators::default()).expect("parses");
                out.push(
                    regia_ui::loc_fit::check_document(
                        &rel,
                        &file,
                        &loc,
                        &mut ts,
                        FitSize {
                            width: 800.0,
                            height: 600.0,
                        },
                        1.0,
                    )
                    .expect("check"),
                );
            }
        }
        out
    }

    /// A directory symlink under `assets/` is not walked. Following it would read a tree
    /// outside the project, and a link back at a parent would recurse without bound.
    #[cfg(unix)]
    #[test]
    fn ui_walk_does_not_follow_a_directory_symlink() {
        let root = regia_test_utils::unique_temp_dir("cli-loc-fit-link");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        std::fs::write(assets.join("menu.ui.ron"), "menu").expect("doc");
        let outside = regia_test_utils::unique_temp_dir("cli-loc-fit-outside");
        std::fs::write(outside.join("evil.ui.ron"), "evil").expect("outside");
        std::os::unix::fs::symlink(&outside, assets.join("linked")).expect("link");
        let docs = ui_documents(&root).expect("walk");
        assert!(docs.iter().any(|p| p.ends_with("menu.ui.ron")), "{docs:?}");
        assert!(
            docs.iter().all(|p| !p.ends_with("evil.ui.ron")),
            "followed a directory symlink: {docs:?}"
        );
        // Only reached once the walk returns. A link at `..` must not recurse.
        std::os::unix::fs::symlink("..", assets.join("up")).expect("cycle");
        let docs = ui_documents(&root).expect("cycle returns");
        assert_eq!(docs.len(), 1, "{docs:?}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
