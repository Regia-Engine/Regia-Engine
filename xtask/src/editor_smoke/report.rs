// SPDX-License-Identifier: MPL-2.0
//! The run's outputs: `result.json` (schema [`RESULT_FORMAT_VERSION`]), one contact-sheet PNG per
//! area and `summary.md` with the counts and every FAIL with its reproduction command.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::inventory::{Finding, InventoryCounts};

/// Version of the `result.json` shape. Bump it when a field changes meaning or goes.
pub const RESULT_FORMAT_VERSION: u32 = 1;

/// A step's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Outcome {
    /// Every check held.
    Pass,
    /// A check failed (the reason says which).
    Fail,
    /// Not run, or refused by the editor with a typed error (the reason says why).
    Skip,
}

impl Outcome {
    /// `PASS`, `FAIL` or `SKIP`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        }
    }
}

/// One step of the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepResult {
    /// The contact sheet the screenshot goes to: `panels`, `menus`, `dialogs`, or `other`.
    pub area: String,
    /// `panel`, `menu`, `command`, `action`, `keyboard`, `scale200`, `light` or `setup`.
    pub kind: String,
    /// The panel id, menu path, command id or action id.
    pub name: String,
    /// Verdict.
    pub outcome: Outcome,
    /// Why it failed or was skipped; notes for a pass (empty when there is nothing to say).
    pub reason: String,
    /// Wall time of the step in milliseconds.
    pub time_ms: f64,
    /// The slowest panel draw during the step (`editor.panel_timings`), when any was recorded.
    pub frame_ms: Option<f64>,
    /// The agent call the step made (method and params), for the reproduction.
    pub call: Option<serde_json::Value>,
    /// Error and id-clash log lines (and a panic line) seen during the step.
    pub log_excerpt: Vec<String>,
    /// Screenshot path relative to the run folder.
    pub screenshot: Option<String>,
}

impl StepResult {
    /// `kind:name`, the key `--only` and `--skip` match.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind, self.name)
    }
}

/// Totals of a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    /// Steps that passed.
    pub pass: usize,
    /// Steps that failed.
    pub fail: usize,
    /// Steps skipped or refused.
    pub skip: usize,
}

/// `result.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    /// [`RESULT_FORMAT_VERSION`].
    pub format_version: u32,
    /// UTC start, `YYYY-MM-DDTHH:MM:SSZ`.
    pub started: String,
    /// Wall time of the whole run in seconds.
    pub elapsed_s: f64,
    /// The editor executable.
    pub editor: String,
    /// The sample project that was copied and driven.
    pub sample: String,
    /// `lavapipe`, `card <n>` or `windowed`.
    pub mode: String,
    /// The adapter the editor reported, when known.
    pub adapter: String,
    /// The slow-frame bar of this mode in milliseconds.
    pub frame_budget_ms: f64,
    /// The command line that reproduces this run.
    pub command: String,
    /// What the editor has, per kind.
    pub inventory: InventoryCounts,
    /// Problems the inventory shows by reading alone.
    pub findings: Vec<Finding>,
    /// Every step in run order.
    pub steps: Vec<StepResult>,
    /// Totals over `steps`.
    pub totals: Totals,
}

/// Counts the outcomes.
#[must_use]
pub fn totals(steps: &[StepResult]) -> Totals {
    let mut t = Totals::default();
    for s in steps {
        match s.outcome {
            Outcome::Pass => t.pass += 1,
            Outcome::Fail => t.fail += 1,
            Outcome::Skip => t.skip += 1,
        }
    }
    t
}

/// Whether an RGBA8 image is blank: every pixel within `tolerance` of the first in each channel
/// (a uniform clear color, an all-black or all-white frame). An empty image is blank.
#[must_use]
pub fn is_uniform(rgba: &[u8], tolerance: u8) -> bool {
    let Some(first) = rgba.first_chunk::<4>() else {
        return true;
    };
    rgba.as_chunks::<4>()
        .0
        .iter()
        .all(|px| px.iter().zip(first).all(|(a, b)| a.abs_diff(*b) <= tolerance))
}

/// Crop a visible panel's body, using the logical rect and scale from `editor.layout_get`.
/// Round inward so adjacent dock chrome cannot make an otherwise empty body pass. Missing,
/// empty, non-finite or out-of-image regions are failures, never whole-window fallbacks.
pub(super) fn panel_image(
    image: &image::RgbaImage,
    layout: &serde_json::Value,
    id: &str,
) -> Result<image::RgbaImage, String> {
    let rect = layout
        .get("panel_regions")
        .and_then(|regions| regions.get(id))
        .and_then(serde_json::Value::as_array)
        .filter(|r| r.len() == 4)
        .ok_or_else(|| format!("no visible body rectangle for panel {id}"))?;
    let scale = layout
        .get("pixels_per_point")
        .and_then(serde_json::Value::as_f64)
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or("editor.layout_get has no valid pixels_per_point")?;
    let mut values = [0.0; 4];
    for (out, value) in values.iter_mut().zip(rect) {
        *out = value
            .as_f64()
            .filter(|v| v.is_finite())
            .ok_or("panel rectangle contains a non-finite or non-numeric coordinate")?;
    }
    let [x, y, w, h] = values;
    let [left, top, right, bottom] = [
        (x * scale).ceil(),
        (y * scale).ceil(),
        ((x + w) * scale).floor(),
        ((y + h) * scale).floor(),
    ];
    if ![left, top, right, bottom].iter().all(|v| v.is_finite())
        || left < 0.0
        || top < 0.0
        || right <= left
        || bottom <= top
        || right > f64::from(image.width())
        || bottom > f64::from(image.height())
    {
        return Err(format!(
            "panel {id} body rectangle is empty or outside the screenshot"
        ));
    }
    Ok(image::imageops::crop_imm(
        image,
        left as u32,
        top as u32,
        (right - left) as u32,
        (bottom - top) as u32,
    )
    .to_image())
}

/// The thumbnail size of the contact sheets.
pub const THUMB: (u32, u32) = (320, 180);
/// Thumbnails per sheet row.
pub const SHEET_COLUMNS: u32 = 6;

/// Writes `<area>.png` contact sheets under `dir`: the screenshots of each area's steps in run
/// order, [`SHEET_COLUMNS`] per row, a red frame round a failed step. Returns the sheet paths
/// (relative to `dir`) with their cell lists. An area without screenshots gets no sheet.
///
/// # Errors
///
/// A sheet could not be encoded or written. An unreadable screenshot leaves its cell gray.
pub fn write_contact_sheets(dir: &Path, steps: &[StepResult]) -> Result<Vec<(String, Vec<String>)>> {
    let mut out = Vec::new();
    // The named areas first, then any other area with screenshots (`scale200`, `light`) in run
    // order; `other` (commands, actions, routes, keyboard) gets no sheet.
    let mut areas: Vec<&str> = vec!["panels", "menus", "dialogs"];
    for s in steps {
        if s.screenshot.is_some() && s.area != "other" && !areas.contains(&s.area.as_str()) {
            areas.push(s.area.as_str());
        }
    }
    for area in areas {
        let cells: Vec<&StepResult> = steps
            .iter()
            .filter(|s| s.area == area && s.screenshot.is_some())
            .collect();
        if cells.is_empty() {
            continue;
        }
        let n = u32::try_from(cells.len()).unwrap_or(u32::MAX);
        let rows = n.div_ceil(SHEET_COLUMNS);
        let gap = 4;
        let width = SHEET_COLUMNS * (THUMB.0 + gap) + gap;
        let height = rows * (THUMB.1 + gap) + gap;
        let mut sheet = image::RgbaImage::from_pixel(width, height, image::Rgba([24, 24, 24, 255]));
        let mut names = Vec::new();
        for (i, step) in (0u32..).zip(cells.iter()) {
            let x = gap + (i % SHEET_COLUMNS) * (THUMB.0 + gap);
            let y = gap + (i / SHEET_COLUMNS) * (THUMB.1 + gap);
            let thumb = step
                .screenshot
                .as_ref()
                .and_then(|p| image::open(dir.join(p)).ok())
                .map(|img| fit_thumb(&img.to_rgba8()))
                .unwrap_or_else(|| {
                    image::RgbaImage::from_pixel(THUMB.0, THUMB.1, image::Rgba([90, 90, 90, 255]))
                });
            image::imageops::overlay(&mut sheet, &thumb, i64::from(x), i64::from(y));
            if step.outcome == Outcome::Fail {
                frame(&mut sheet, x, y, THUMB, image::Rgba([230, 40, 40, 255]));
            }
            names.push(format!("{} {}", step.outcome.label(), step.key()));
        }
        let name = format!("{area}.png");
        sheet
            .save(dir.join(&name))
            .with_context(|| format!("writing {}", dir.join(&name).display()))?;
        out.push((name, names));
    }
    Ok(out)
}

/// Whether two PNG files decode to the same size and the same RGBA8 pixels. An unreadable file
/// is never the same.
pub fn same_pixels(a: &Path, b: &Path) -> bool {
    match (image::open(a), image::open(b)) {
        (Ok(a), Ok(b)) => a.to_rgba8() == b.to_rgba8(),
        _ => false,
    }
}

/// Scales `img` into a [`THUMB`]-sized cell keeping its aspect ratio (never enlarged, so text stays
/// crisp), centered on the sheet's background. Stretching a tall panel crop (Outliner, Inspector)
/// to 16:9 made its text unreadable on the sheet.
pub fn fit_thumb(img: &image::RgbaImage) -> image::RgbaImage {
    let mut cell = image::RgbaImage::from_pixel(THUMB.0, THUMB.1, image::Rgba([24, 24, 24, 255]));
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return cell;
    }
    let scale = (f64::from(THUMB.0) / f64::from(w))
        .min(f64::from(THUMB.1) / f64::from(h))
        .min(1.0);
    // Rounded and clamped to the cell: `scale` keeps both sides within it up to rounding.
    let tw = ((f64::from(w) * scale).round() as u32).clamp(1, THUMB.0);
    let th = ((f64::from(h) * scale).round() as u32).clamp(1, THUMB.1);
    let scaled = if (tw, th) == (w, h) {
        img.clone()
    } else {
        image::imageops::resize(img, tw, th, image::imageops::FilterType::Triangle)
    };
    image::imageops::overlay(
        &mut cell,
        &scaled,
        i64::from((THUMB.0 - tw) / 2),
        i64::from((THUMB.1 - th) / 2),
    );
    cell
}

fn frame(img: &mut image::RgbaImage, x: u32, y: u32, size: (u32, u32), color: image::Rgba<u8>) {
    for t in 0..3 {
        for dx in 0..size.0 {
            for py in [y + t, y + size.1 - 1 - t] {
                if x + dx < img.width() && py < img.height() {
                    img.put_pixel(x + dx, py, color);
                }
            }
        }
        for dy in 0..size.1 {
            for px in [x + t, x + size.0 - 1 - t] {
                if px < img.width() && y + dy < img.height() {
                    img.put_pixel(px, y + dy, color);
                }
            }
        }
    }
}

/// The command that reruns one step.
#[must_use]
pub fn repro_command(base: &str, step: &StepResult) -> String {
    format!("{base} --only '{}'", step.key().replace('\'', "'\\''"))
}

/// `summary.md`.
#[must_use]
pub fn summary_markdown(result: &RunResult, sheets: &[(String, Vec<String>)]) -> String {
    let mut s = String::new();
    let t = result.totals;
    let _ = writeln!(s, "# Editor smoke ({})\n", result.started);
    let _ = writeln!(
        s,
        "Mode `{}`, adapter `{}`, sample `{}`, slow-frame bar {} ms, {:.0} s.\n",
        result.mode, result.adapter, result.sample, result.frame_budget_ms, result.elapsed_s
    );
    let _ = writeln!(s, "Command: `{}`\n", result.command);
    let _ = writeln!(s, "## Counts\n");
    let _ = writeln!(s, "| PASS | FAIL | SKIP | steps |\n|---|---|---|---|");
    let _ = writeln!(
        s,
        "| {} | {} | {} | {} |\n",
        t.pass,
        t.fail,
        t.skip,
        result.steps.len()
    );
    let mut kinds: Vec<&str> = result.steps.iter().map(|x| x.kind.as_str()).collect();
    kinds.dedup();
    let mut seen = std::collections::BTreeSet::new();
    kinds.retain(|k| seen.insert(*k));
    let _ = writeln!(s, "| Kind | PASS | FAIL | SKIP |\n|---|---|---|---|");
    for kind in kinds {
        let of: Vec<&StepResult> = result.steps.iter().filter(|x| x.kind == kind).collect();
        let c = |o: Outcome| of.iter().filter(|x| x.outcome == o).count();
        let _ = writeln!(
            s,
            "| {kind} | {} | {} | {} |",
            c(Outcome::Pass),
            c(Outcome::Fail),
            c(Outcome::Skip)
        );
    }
    let inv = &result.inventory;
    let _ = writeln!(s, "\n## Inventory\n");
    let _ = writeln!(
        s,
        "{} panels, {} menu items, {} palette commands, {} registered actions, {} control sites ({}), {} agent methods answered, {} editor methods in the snapshot.\n",
        inv.panels,
        inv.menu_items,
        inv.commands,
        inv.actions,
        inv.sites,
        inv.sites_by_class
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", "),
        inv.methods,
        inv.snapshot_methods
    );
    let _ = writeln!(s, "## Failures\n");
    let fails: Vec<&StepResult> = result
        .steps
        .iter()
        .filter(|x| x.outcome == Outcome::Fail)
        .collect();
    if fails.is_empty() {
        let _ = writeln!(s, "None.\n");
    } else {
        let _ = writeln!(
            s,
            "| Step | Reason | Log | Screenshot | Reproduce |\n|---|---|---|---|---|"
        );
        for f in fails {
            let _ = writeln!(
                s,
                "| `{}` | {} | {} | {} | `{}` |",
                f.key(),
                cell(&f.reason),
                cell(&f.log_excerpt.first().cloned().unwrap_or_default()),
                f.screenshot.clone().unwrap_or_default(),
                repro_command(&result.command, f)
            );
        }
        let _ = writeln!(s);
    }
    let _ = writeln!(s, "## Inventory findings\n");
    if result.findings.is_empty() {
        let _ = writeln!(s, "None.\n");
    } else {
        let _ = writeln!(s, "| Kind | Subject | Detail |\n|---|---|---|");
        for f in &result.findings {
            let _ = writeln!(s, "| {} | `{}` | {} |", f.kind, f.subject, cell(&f.detail));
        }
        let _ = writeln!(s);
    }
    let notes: Vec<&StepResult> = result
        .steps
        .iter()
        .filter(|x| x.outcome == Outcome::Pass && !x.reason.is_empty())
        .collect();
    if !notes.is_empty() {
        let _ = writeln!(s, "## Notes on passing steps\n\n| Step | Note |\n|---|---|");
        for n in notes {
            let _ = writeln!(s, "| `{}` | {} |", n.key(), cell(&n.reason));
        }
        let _ = writeln!(s);
    }
    let _ = writeln!(s, "## Skipped\n\n| Step | Reason |\n|---|---|");
    for k in result.steps.iter().filter(|x| x.outcome == Outcome::Skip) {
        let _ = writeln!(s, "| `{}` | {} |", k.key(), cell(&k.reason));
    }
    let _ = writeln!(s, "\n## Contact sheets\n");
    for (name, cells) in sheets {
        let _ = writeln!(s, "`{name}`, cells left to right, top to bottom:\n");
        for (i, c) in cells.iter().enumerate() {
            let _ = writeln!(s, "{}. {}", i + 1, c);
        }
        let _ = writeln!(s);
    }
    s
}

fn cell(text: &str) -> String {
    let one_line: String = text.replace(['\n', '\r'], " ").replace('|', "\\|");
    if one_line.chars().count() > 240 {
        let cut: String = one_line.chars().take(240).collect();
        format!("{cut}…")
    } else {
        one_line
    }
}

/// Writes `result.json`, the contact sheets and `summary.md` into `dir`.
///
/// # Errors
///
/// A file could not be written.
pub fn write_all(dir: &Path, result: &RunResult) -> Result<()> {
    let json = serde_json::to_string_pretty(result).context("encoding result.json")?;
    write_atomic(&dir.join("result.json"), json.as_bytes())?;
    let sheets = write_contact_sheets(dir, &result.steps)?;
    write_atomic(
        &dir.join("summary.md"),
        summary_markdown(result, &sheets).as_bytes(),
    )?;
    Ok(())
}

/// Writes through a temp file and a rename, so a reader never sees half a file.
///
/// # Errors
///
/// The temp file could not be written or renamed.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(kind: &str, name: &str, outcome: Outcome, area: &str) -> StepResult {
        StepResult {
            area: area.into(),
            kind: kind.into(),
            name: name.into(),
            outcome,
            reason: if outcome == Outcome::Fail {
                "boom | bar".into()
            } else {
                String::new()
            },
            time_ms: 12.5,
            frame_ms: Some(3.0),
            call: Some(serde_json::json!({ "method": "editor.layout_apply", "params": { "open": name } })),
            log_excerpt: vec!["ERROR x".into()],
            screenshot: None,
        }
    }

    fn result(steps: Vec<StepResult>) -> RunResult {
        RunResult {
            format_version: RESULT_FORMAT_VERSION,
            started: "2026-11-15T22:00:00Z".into(),
            elapsed_s: 1.0,
            editor: "regia-editor".into(),
            sample: "samples/third_person".into(),
            mode: "lavapipe".into(),
            adapter: "llvmpipe".into(),
            frame_budget_ms: 250.0,
            command: "cargo xtask editor-smoke".into(),
            inventory: InventoryCounts::default(),
            findings: vec![Finding {
                kind: "no_agent_route".into(),
                subject: "a.rs:1".into(),
                detail: "d".into(),
            }],
            totals: totals(&steps),
            steps,
        }
    }

    #[test]
    fn result_json_round_trips_with_the_documented_fields() {
        let r = result(vec![
            step("panel", "viewport", Outcome::Pass, "panels"),
            step("menu", "File/Exit", Outcome::Skip, "menus"),
            step("command", "view.toggle_grid", Outcome::Fail, "other"),
        ]);
        let v = serde_json::to_value(&r).expect("encode");
        for key in [
            "format_version",
            "started",
            "mode",
            "inventory",
            "findings",
            "steps",
            "totals",
        ] {
            assert!(v.get(key).is_some(), "result.json has `{key}`");
        }
        let s0 = &v["steps"][0];
        for key in [
            "area",
            "kind",
            "name",
            "outcome",
            "reason",
            "time_ms",
            "frame_ms",
            "log_excerpt",
            "screenshot",
        ] {
            assert!(s0.get(key).is_some(), "a step has `{key}`");
        }
        assert_eq!(s0["outcome"], "PASS");
        assert_eq!(v["steps"][2]["outcome"], "FAIL");
        assert_eq!(
            v["totals"],
            serde_json::json!({ "pass": 1, "fail": 1, "skip": 1 })
        );
        let back: RunResult = serde_json::from_value(v).expect("decode");
        assert_eq!(back.steps, r.steps);
    }

    #[test]
    fn summary_lists_each_failure_with_its_reproduction() {
        let r = result(vec![
            step("panel", "viewport", Outcome::Pass, "panels"),
            step("command", "view.toggle_grid", Outcome::Fail, "other"),
        ]);
        let md = summary_markdown(&r, &[]);
        assert!(md.contains("| 1 | 1 | 0 | 2 |"), "{md}");
        assert!(
            md.contains("cargo xtask editor-smoke --only 'command:view.toggle_grid'"),
            "{md}"
        );
        assert!(
            md.contains("boom \\| bar"),
            "a pipe in a reason does not break the table"
        );
        assert!(md.contains("no_agent_route"));
    }

    #[test]
    fn uniformity_tolerance_is_two_levels_in_each_channel() {
        for channel in 0..4 {
            for delta in [2, 3] {
                let mut pixels = [100; 8];
                pixels[4 + channel] += delta;
                assert_eq!(is_uniform(&pixels, 2), delta == 2);
                pixels[4 + channel] = 100 - delta;
                assert_eq!(is_uniform(&pixels, 2), delta == 2);
            }
        }
    }

    #[test]
    fn uniform_images_are_blank_and_varied_ones_are_not() {
        assert!(is_uniform(&[], 0));
        assert!(is_uniform(&[10, 10, 10, 255, 11, 10, 9, 255], 2));
        assert!(!is_uniform(&[10, 10, 10, 255, 200, 10, 9, 255], 2));
    }

    #[test]
    fn same_pixels_compares_decoded_pixels() {
        let dir = regia_test_utils::unique_temp_dir("editor-smoke-same");
        std::fs::create_dir_all(&dir).expect("dir");
        let a = image::RgbaImage::from_pixel(8, 4, image::Rgba([10, 20, 30, 255]));
        let mut b = a.clone();
        a.save(dir.join("a.png")).expect("a");
        a.save(dir.join("a2.png")).expect("a2");
        b.put_pixel(7, 3, image::Rgba([10, 20, 31, 255]));
        b.save(dir.join("b.png")).expect("b");
        assert!(same_pixels(&dir.join("a.png"), &dir.join("a2.png")));
        assert!(!same_pixels(&dir.join("a.png"), &dir.join("b.png")));
        assert!(!same_pixels(&dir.join("a.png"), &dir.join("missing.png")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thumbnails_keep_the_aspect_ratio_of_tall_and_small_crops() {
        // A tall Inspector-like crop: 100 x 400 white on the sheet's dark background.
        let tall = image::RgbaImage::from_pixel(100, 400, image::Rgba([255, 255, 255, 255]));
        let t = fit_thumb(&tall);
        assert_eq!(t.dimensions(), THUMB);
        // Scale 180 / 400 = 0.45: 45 x 180, centered (x 137..182).
        let white = |x: u32, y: u32| t.get_pixel(x, y)[0] >= 250;
        assert!(white(THUMB.0 / 2, 0) && white(THUMB.0 / 2, THUMB.1 - 1));
        assert!(white(138, 90) && white(181, 90));
        assert!(
            !white(130, 90) && !white(190, 90),
            "not stretched to the cell width"
        );
        // A small crop is not enlarged.
        let small = image::RgbaImage::from_pixel(40, 20, image::Rgba([255, 255, 255, 255]));
        let s = fit_thumb(&small);
        let count = s.pixels().filter(|p| p[0] == 255).count();
        assert_eq!(count, 40 * 20);
        // An empty image gives an empty cell.
        assert_eq!(fit_thumb(&image::RgbaImage::new(0, 0)).dimensions(), THUMB);
    }

    #[test]
    fn contact_sheets_hold_one_cell_per_screenshot() {
        let dir = regia_test_utils::unique_temp_dir("editor-smoke-sheet");
        std::fs::create_dir_all(&dir).expect("dir");
        let shot = image::RgbaImage::from_fn(64, 36, |x, y| {
            image::Rgba([(x * 4) as u8, (y * 7) as u8, 128, 255])
        });
        shot.save(dir.join("a.png")).expect("png");
        let mut a = step("panel", "viewport", Outcome::Pass, "panels");
        a.screenshot = Some("a.png".into());
        let mut b = step("panel", "outliner", Outcome::Fail, "panels");
        b.screenshot = Some("missing.png".into());
        let mut c = step("command", "x", Outcome::Pass, "other");
        c.screenshot = Some("a.png".into());
        let mut d = step("light", "viewport", Outcome::Pass, "light");
        d.screenshot = Some("a.png".into());
        let sheets = write_contact_sheets(&dir, &[a, b, c, d]).expect("sheets");
        let names: Vec<&str> = sheets.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["panels.png", "light.png"],
            "`other` gets no sheet; extra areas do"
        );
        assert_eq!(sheets[0].1, ["PASS panel:viewport", "FAIL panel:outliner"]);
        let img = image::open(dir.join("panels.png")).expect("sheet").to_rgba8();
        assert_eq!(img.width(), SHEET_COLUMNS * (THUMB.0 + 4) + 4);
        assert_eq!(img.height(), THUMB.1 + 8);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
