// SPDX-License-Identifier: MPL-2.0
//! `regia assets font-subset`: report what subsetting a font would do, and write the subset (ADR-2697, book
//! `agents/font-subset.md`). `regia cook` and `regia package` run the same code on every font under `assets/`; this
//! command answers for one file, with other locales or characters, without cooking.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;
use regia_asset_pipeline::font_subset::{
    FontSubsetMode, UNREADABLE_KEPT, plan_font, read_font_settings, read_project_charsets, run_plan,
    summary_line,
};
use serde_json::json;

/// `regia assets font-subset` arguments.
#[derive(Debug, Args)]
pub struct AssetsFontSubsetArgs {
    /// The font file (`.ttf`, `.otf`, `.ttc`, `.otc`). Its `.meta` settings apply when it has one.
    pub font: PathBuf,
    /// Project directory: its `assets/localization` gives the character sets.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Subset to these locales (comma separated) instead of what the font's `.meta` says.
    #[arg(long, value_delimiter = ',', value_name = "TAGS")]
    pub locales: Vec<String>,
    /// More characters to keep.
    #[arg(long, default_value = "", value_name = "TEXT")]
    pub chars: String,
    /// Write the subset here. Without it nothing is written.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// Print JSON on stdout (agents and CI).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Runs the command and returns what to print.
///
/// # Errors
///
/// The font or a locale cannot be read, the file is not a font (with its `font_subset_*` code), the settings are bad, a
/// locale is unknown, or the output cannot be written.
pub fn run(args: &AssetsFontSubsetArgs) -> Result<String> {
    let bytes = std::fs::read(&args.font).with_context(|| format!("reading {}", args.font.display()))?;
    let mut settings = read_font_settings(&args.font).map_err(|e| anyhow::anyhow!("{e}"))?;
    if !args.locales.is_empty() {
        settings.subset = FontSubsetMode::Locales(args.locales.clone());
    }
    settings.extra_chars.push_str(&args.chars);
    let plan = plan_font(&settings, &read_project_charsets(&args.project), &bytes)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let name = args.font.display().to_string();
    let made = run_plan(&name, &bytes, &plan);
    if made.summary.code.as_deref() == Some(UNREADABLE_KEPT) {
        bail!("{}", made.warnings.join("; "));
    }
    if let Some(out) = &args.out {
        write_atomic(out, &made.bytes)?;
    }
    if args.json {
        let value = json!({
            "font": made.summary,
            "written": args.out.as_ref().map(|p| p.display().to_string()),
            "warnings": made.warnings,
        });
        return Ok(serde_json::to_string_pretty(&value)?);
    }
    let mut text = summary_line(&made.summary);
    for warning in &made.warnings {
        text.push_str(&format!("\nwarning: {warning}"));
    }
    for (locale, missing) in &made.summary.missing {
        text.push_str(&format!(
            "\n{locale}: the font does not map {} character(s): {missing}",
            missing.chars().count()
        ));
    }
    if let Some(out) = &args.out {
        text.push_str(&format!(
            "\nwrote {} bytes to {}",
            made.bytes.len(),
            out.display()
        ));
    }
    Ok(text)
}

/// Writes through a temporary file next to `path`, so a failed write leaves no torn font.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use regia_asset_pipeline::font_subset_fixture::{gid, tiny_font};
    use skrifa::GlyphId;

    use super::*;

    fn args(font: PathBuf, project: PathBuf) -> AssetsFontSubsetArgs {
        AssetsFontSubsetArgs {
            font,
            project,
            locales: Vec::new(),
            chars: String::new(),
            out: None,
            json: false,
        }
    }

    fn fixture_project() -> (PathBuf, PathBuf) {
        let root = regia_test_utils::unique_temp_dir("assets_font_subset");
        for (tag, text) in [("en", "a = fi\n"), ("fr", "a = é\n")] {
            let dir = root.join("assets/localization").join(tag);
            std::fs::create_dir_all(&dir).expect("dir");
            std::fs::write(dir.join("main.ftl"), text).expect("ftl");
        }
        let font = root.join("ui.ttf");
        std::fs::write(&font, tiny_font()).expect("font");
        (root, font)
    }

    /// How many contours `glyph` draws in the font `data`.
    fn contours(data: &[u8], glyph: u16) -> u32 {
        use skrifa::instance::{LocationRef, Size};
        use skrifa::outline::{DrawSettings, OutlinePen};
        use skrifa::{MetadataProvider, raw::FontRef};
        #[derive(Default)]
        struct Count(u32);
        impl OutlinePen for Count {
            fn move_to(&mut self, _: f32, _: f32) {
                self.0 += 1;
            }
            fn line_to(&mut self, _: f32, _: f32) {}
            fn quad_to(&mut self, _: f32, _: f32, _: f32, _: f32) {}
            fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {}
            fn close(&mut self) {}
        }
        let font = FontRef::new(data).expect("parses");
        let outlines = font.outline_glyphs();
        let outline = outlines.get(GlyphId::new(u32::from(glyph))).expect("glyph");
        let mut pen = Count::default();
        outline
            .draw(
                DrawSettings::unhinted(Size::unscaled(), LocationRef::default()),
                &mut pen,
            )
            .expect("draws");
        pen.0
    }

    #[test]
    fn it_reports_and_writes_a_subset_for_named_locales_and_extra_characters() {
        let (root, font) = fixture_project();
        let plain = root.join("out/ui-plain.ttf");
        let mut a = args(font.clone(), root.clone());
        a.locales = vec!["fr".to_owned()];
        a.out = Some(plain.clone());
        run(&a).expect("runs");
        let plain = std::fs::read(&plain).expect("the subset");
        assert_eq!(contours(&plain, gid::E_MACRON), 0, "no locale of `fr` uses it");

        // `--chars` keeps it; `--json` reports the file it wrote.
        let out = root.join("out/ui-fr.ttf");
        a.chars = "\u{113}".to_owned();
        a.out = Some(out.clone());
        a.json = true;
        let text = run(&a).expect("runs");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        let row = &value["font"];
        assert_eq!(row["mode"], "locales");
        assert_eq!(row["subset"], true);
        let before = tiny_font().len() as u64;
        assert_eq!(row["bytes_before"], before);
        let written = std::fs::read(&out).expect("the subset");
        assert_eq!(row["bytes_after"], written.len() as u64);
        assert!((written.len() as u64) < before);
        assert!(contours(&written, gid::E_MACRON) > 0, "--chars keeps it");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn text_output_names_the_locales_missing_characters_and_writes_nothing_without_out() {
        let (root, font) = fixture_project();
        let dir = root.join("assets/localization/en");
        std::fs::write(dir.join("main.ftl"), "a = fih\n").expect("ftl");
        let text = run(&args(font.clone(), root.clone())).expect("runs");
        assert!(text.contains("-> "), "{text}");
        assert!(
            text.contains("en: the font does not map 1 character(s): h"),
            "{text}"
        );
        assert_eq!(
            std::fs::read(&font).expect("font"),
            tiny_font(),
            "the source is untouched"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_file_that_is_not_a_font_and_an_unknown_locale_fail_with_their_codes() {
        let (root, font) = fixture_project();
        let bad = root.join("bad.ttf");
        std::fs::write(&bad, b"nope").expect("bad");
        let err = run(&args(bad, root.clone())).expect_err("not a font").to_string();
        assert!(err.contains("font_subset_not_a_font"), "{err}");
        let mut a = args(font, root.clone());
        a.locales = vec!["xx".to_owned()];
        let err = run(&a).expect_err("unknown").to_string();
        assert!(err.contains("font_subset_unknown_locale"), "{err}");
        let _ = std::fs::remove_dir_all(root);
    }
}
