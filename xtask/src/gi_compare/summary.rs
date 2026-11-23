// SPDX-License-Identifier: MPL-2.0
//! `summary.json` of a `gi-compare` run, the verdict on it (leaks, an inactive bake, FLIP
//! regressions against a previous run) and the table `--read` prints.
//!
//! The verdict only flags; it never passes a look. The last line of every table says so.

use serde::{Deserialize, Serialize};

/// Version of the `summary.json` this build writes; `--read` refuses any other.
pub const SUMMARY_SCHEMA: u32 = 1;

/// Default FLIP increase over the baseline that counts as a regression (mean FLIP, absolute).
pub const DEFAULT_FLIP_TOLERANCE: f32 = 0.01;

/// The last line of every printed table.
pub const LOOK_AT_THE_PLATES: &str = "These numbers are only a guide: look at the plates (one contact sheet per view) before judging the bake.";

/// Image scores of one pair (`regia_render::compare`): FLIP and SSIM on the display-encoded
/// pair, RMSE on linear luminance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Scores {
    /// Mean FLIP error (0 = identical, 1 = maximal).
    pub flip_mean: f32,
    /// 95th percentile of the FLIP map.
    pub flip_p95: f32,
    /// Mean SSIM of the display luma (1 = identical).
    pub ssim: f32,
    /// RMSE over the reference's mean luminance; `None` when the reference is black.
    pub rel_rmse: Option<f32>,
    /// Mean linear luminance of the reference, cd/m².
    pub mean_reference: f32,
    /// Mean linear luminance of the test image, cd/m².
    pub mean_test: f32,
}

impl Scores {
    /// From the renderer's compare report (a non-finite relative RMSE becomes `None`).
    #[must_use]
    pub fn from_report(r: &regia_render::compare::CompareReport) -> Self {
        Self {
            flip_mean: r.flip_mean,
            flip_p95: r.flip_p95,
            ssim: r.ssim,
            rel_rmse: r.rel_rmse.is_finite().then_some(r.rel_rmse),
            mean_reference: r.mean_reference,
            mean_test: r.mean_test,
        }
    }
}

/// The leak metric of one view (`metrics::leak`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Leak {
    /// Pixels in the interior mask.
    pub pixels: u64,
    /// Baked indirect luminance in the mask, cd/m².
    pub baked_mean: f32,
    /// Reference indirect luminance in the mask, cd/m².
    pub reference_mean: f32,
    /// Floor under the reference level, cd/m².
    pub floor: f32,
    /// `max(0, baked − reference) / max(reference, floor)`; `None` for a black frame.
    pub leak: Option<f32>,
    /// The view's threshold.
    pub threshold: f32,
}

/// The renderer's baked GI status after the baked frame: the `baked` object of
/// `render.get_gi_mode` (`book/agents/baked-gi.md`), read from the same
/// `FrameRenderer::baked_gi_status`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GiStatus {
    /// A volume is installed.
    pub installed: bool,
    /// It lit the frame.
    pub active: bool,
    /// Probes in the volume.
    pub probes: usize,
    /// GPU bytes of the volume.
    pub gpu_bytes: u64,
    /// Upload or validation error.
    pub error: Option<String>,
    /// Light factors per component (sun bounce, sky, fill, emissive), per channel.
    pub scales: Vec<[f32; 3]>,
    /// Angle between the frame's sun and the baked one, degrees.
    pub sun_mismatch_deg: f32,
    /// Capsule occluders in use.
    pub occluders: usize,
    /// The occluder cap.
    pub occluder_cap: usize,
    /// Occluders dropped (over the cap or unusable).
    pub occluders_dropped: usize,
    /// Interior bricks in the installed volume.
    pub interior_bricks: usize,
    /// Emitters the volume's bake named (`.fgiv` v3, ADR-1386; `None` for a v1/v2 volume, whose
    /// emitters the emissive area lights count again). Absent in summaries before it.
    #[serde(default)]
    pub emitters: Option<usize>,
    /// Emissive area lights the baked frame uploaded, and how many of them the bake holds (left
    /// to it where it covers). Absent in summaries before it.
    #[serde(default)]
    pub emissive_lights: (u32, u32),
}

/// Wall times of one view, milliseconds (measurements, never judged).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    /// Baked frame (all frames rendered for it).
    pub raster_baked_ms: f64,
    /// DDGI frames (including the settling frames).
    pub raster_ddgi_ms: f64,
    /// Direct-only frame.
    pub raster_direct_ms: f64,
    /// Reference trace.
    pub reference_ms: f64,
    /// The emitter-free direct trace (0 without emitters).
    pub reference_direct_ms: f64,
    /// Scoring and file output.
    pub compare_ms: f64,
}

/// One view's results.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewSummary {
    /// View name.
    pub name: String,
    /// Contact sheet, relative to the summary's directory.
    pub sheet: String,
    /// Exposure of the full row (EV100).
    pub ev100: f32,
    /// Exposure of the indirect row (EV100).
    pub indirect_ev100: f32,
    /// Baked frame against the reference, full image.
    pub full: Scores,
    /// Baked indirect light against the reference's indirect light.
    pub indirect: Scores,
    /// The DDGI frame against the reference, full image (context only, never judged).
    pub ddgi_full: Option<Scores>,
    /// One extra full-image score per reconstruction of a `--reconstruction sweep`
    /// (`(name, scores)`), empty otherwise.
    #[serde(default)]
    pub reconstructions: Vec<(String, Scores)>,
    /// The leak metric, when the view has an interior box in view.
    pub leak: Option<Leak>,
    /// Baked GI status.
    pub gi: GiStatus,
    /// Wall times.
    pub timings: Timings,
    /// Tile files (PNG and EXR), relative to the summary's directory.
    pub tiles: Vec<String>,
}

/// What the bake was.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BakeSummary {
    /// `baked` (in this run) or the `.fgiv` path given with `--volume`.
    pub source: String,
    /// Probes.
    pub probes: usize,
    /// Valid probes.
    pub valid: usize,
    /// Probes inside geometry after relocation.
    pub inside_geometry: usize,
    /// Probes under the terrain.
    pub under_terrain: usize,
    /// Probes moved by relocation.
    pub relocated: usize,
    /// Rays dropped as non-finite (a tracer bug when non-zero).
    pub rejected_rays: u64,
    /// Bake wall time, seconds (0 for a loaded volume).
    pub seconds: f64,
}

/// A whole run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// [`SUMMARY_SCHEMA`].
    pub schema: u32,
    /// Fixture name.
    pub fixture: String,
    /// Fixture file as given.
    pub fixture_path: String,
    /// Views file as given.
    pub views_path: String,
    /// Output size.
    pub size: (u32, u32),
    /// Reference samples per pixel.
    pub spp: u32,
    /// The views file's exposure of the full tiles (a view may set its own).
    pub ev100: f32,
    /// The views file's exposure of the indirect tiles.
    pub indirect_ev100: f32,
    /// Reconstruction of the baked frame (`l1`, or `sweep`).
    pub reconstruction: String,
    /// Adapter name and backend.
    pub device: String,
    /// The bake.
    pub bake: BakeSummary,
    /// Per view, in the views file's order.
    pub views: Vec<ViewSummary>,
    /// Wall time of the whole run, seconds.
    pub total_seconds: f64,
}

/// What [`verdict`] found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Verdict {
    /// Things to look at first: leaks over threshold, an inactive bake, FLIP regressions.
    pub flags: Vec<String>,
    /// Context (views missing from the baseline, undefined metrics).
    pub info: Vec<String>,
}

/// Why a summary could not be read.
#[derive(Clone, Debug, PartialEq)]
pub enum SummaryError {
    /// Not JSON or not the summary's shape.
    Parse(String),
    /// Another schema version.
    Schema(u32),
}

impl SummaryError {
    /// Stable error code, documented in `book/agents/gi-compare.md`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "summary_parse",
            Self::Schema(_) => "summary_schema",
        }
    }
}

impl std::fmt::Display for SummaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(m) => write!(f, "not a gi-compare summary.json: {m}"),
            Self::Schema(v) => write!(
                f,
                "summary.json schema {v} is not supported (this build reads {SUMMARY_SCHEMA})"
            ),
        }
    }
}

impl std::error::Error for SummaryError {}

/// Parses a `summary.json`.
///
/// # Errors
///
/// Not JSON, not the shape, or another schema.
pub fn parse_summary(text: &str) -> Result<Summary, SummaryError> {
    #[derive(Deserialize)]
    struct Head {
        schema: u32,
    }
    let head: Head = serde_json::from_str(text).map_err(|e| SummaryError::Parse(e.to_string()))?;
    if head.schema != SUMMARY_SCHEMA {
        return Err(SummaryError::Schema(head.schema));
    }
    serde_json::from_str(text).map_err(|e| SummaryError::Parse(e.to_string()))
}

/// Flags leaks over their threshold, views the bake did not light, and (with a `baseline`) a mean
/// FLIP that rose by more than `flip_tolerance`, full or indirect, on a view of the same name.
#[must_use]
pub fn verdict(summary: &Summary, baseline: Option<&Summary>, flip_tolerance: f32) -> Verdict {
    let mut out = Verdict::default();
    for v in &summary.views {
        if !v.gi.active {
            out.flags.push(format!(
                "{}: the baked volume did not light the frame{}",
                v.name,
                v.gi.error.as_ref().map_or(String::new(), |e| format!(" ({e})"))
            ));
        }
        if let Some(l) = &v.leak {
            match l.leak {
                Some(x) if x > l.threshold => out.flags.push(format!(
                    "{}: leak {:.2} > {:.2}: the bake puts {:.3} cd/m2 of indirect light inside the interior box, the reference {:.3}",
                    v.name, x, l.threshold, l.baked_mean, l.reference_mean
                )),
                Some(_) => {}
                None => out.info.push(format!("{}: leak undefined (black frame)", v.name)),
            }
        }
    }
    if let Some(base) = baseline {
        for v in &summary.views {
            let Some(b) = base.views.iter().find(|b| b.name == v.name) else {
                out.info.push(format!("{}: not in the baseline", v.name));
                continue;
            };
            for (what, now, before) in [
                ("full", v.full.flip_mean, b.full.flip_mean),
                ("indirect", v.indirect.flip_mean, b.indirect.flip_mean),
            ] {
                if now > before + flip_tolerance {
                    out.flags.push(format!(
                        "{}: {what} FLIP regressed {before:.4} -> {now:.4} (tolerance {flip_tolerance:.4})",
                        v.name
                    ));
                }
            }
        }
        if (base.spp, base.size) != (summary.spp, summary.size) {
            out.info.push(format!(
                "the baseline ran at {} spp, {}x{}; this run at {} spp, {}x{}: FLIP moves with noise and size",
                base.spp, base.size.0, base.size.1, summary.spp, summary.size.0, summary.size.1
            ));
        }
    }
    out
}

fn opt(v: Option<f32>, digits: usize) -> String {
    v.map_or_else(|| "-".to_string(), |x| format!("{x:.digits$}"))
}

/// The table `--read` prints: one row per view, the flags, then [`LOOK_AT_THE_PLATES`] as the
/// last line, always.
#[must_use]
pub fn render_table(summary: &Summary, verdict: &Verdict) -> String {
    let mut out = format!(
        "gi-compare: {} | {} view(s) | {}x{} | {} spp | reconstruction {} | {}\n",
        summary.fixture,
        summary.views.len(),
        summary.size.0,
        summary.size.1,
        summary.spp,
        summary.reconstruction,
        summary.device
    );
    out.push_str(&format!(
        "bake: {} | {} probes, {} valid, {} inside geometry | {:.1} s\n\n",
        summary.bake.source,
        summary.bake.probes,
        summary.bake.valid,
        summary.bake.inside_geometry,
        summary.bake.seconds
    ));
    out.push_str(&format!(
        "{:<18} {:>9} {:>6} {:>8} | {:>9} {:>6} {:>8} | {:>9} | {:>6} {:>9}\n",
        "view",
        "full FLIP",
        "SSIM",
        "relRMSE",
        "ind. FLIP",
        "SSIM",
        "relRMSE",
        "DDGI FLIP",
        "leak",
        "threshold"
    ));
    for v in &summary.views {
        let (leak, threshold) = match &v.leak {
            Some(l) => (opt(l.leak, 2), format!("{:.2}", l.threshold)),
            None => ("-".to_string(), "-".to_string()),
        };
        out.push_str(&format!(
            "{:<18} {:>9.4} {:>6.3} {:>8} | {:>9.4} {:>6.3} {:>8} | {:>9} | {:>6} {:>9}\n",
            v.name,
            v.full.flip_mean,
            v.full.ssim,
            opt(v.full.rel_rmse, 3),
            v.indirect.flip_mean,
            v.indirect.ssim,
            opt(v.indirect.rel_rmse, 3),
            opt(v.ddgi_full.map(|s| s.flip_mean), 4),
            leak,
            threshold,
        ));
        for (name, s) in &v.reconstructions {
            out.push_str(&format!(
                "  {:<16} {:>9.4} {:>6.3} {:>8}\n",
                name,
                s.flip_mean,
                s.ssim,
                opt(s.rel_rmse, 3)
            ));
        }
    }
    out.push('\n');
    if verdict.flags.is_empty() {
        out.push_str("flags: none\n");
    } else {
        for f in &verdict.flags {
            out.push_str(&format!("flag: {f}\n"));
        }
    }
    for i in &verdict.info {
        out.push_str(&format!("note: {i}\n"));
    }
    out.push_str("sheets:");
    for v in &summary.views {
        out.push_str(&format!(" {}", v.sheet));
    }
    out.push('\n');
    out.push_str(LOOK_AT_THE_PLATES);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scores(flip: f32) -> Scores {
        Scores {
            flip_mean: flip,
            flip_p95: flip * 3.0,
            ssim: 0.9,
            rel_rmse: Some(0.2),
            mean_reference: 10.0,
            mean_test: 11.0,
        }
    }

    fn view(name: &str, flip: f32, leak: Option<f32>) -> ViewSummary {
        ViewSummary {
            name: name.into(),
            sheet: format!("{name}.png"),
            ev100: 12.0,
            indirect_ev100: 10.0,
            full: scores(flip),
            indirect: scores(flip * 2.0),
            ddgi_full: Some(scores(0.2)),
            reconstructions: Vec::new(),
            leak: leak.map(|x| Leak {
                pixels: 100,
                baked_mean: 3.0,
                reference_mean: 2.0,
                floor: 0.1,
                leak: Some(x),
                threshold: 0.25,
            }),
            gi: GiStatus {
                installed: true,
                active: true,
                probes: 1331,
                scales: vec![[1.0; 3]; 4],
                occluder_cap: 64,
                ..GiStatus::default()
            },
            timings: Timings::default(),
            tiles: vec![format!("{name}_baked.png")],
        }
    }

    fn summary(views: Vec<ViewSummary>) -> Summary {
        Summary {
            schema: SUMMARY_SCHEMA,
            fixture: "leak_room".into(),
            size: (320, 240),
            spp: 1024,
            ev100: 12.0,
            indirect_ev100: 10.0,
            reconstruction: "l1".into(),
            views,
            ..Summary::default()
        }
    }

    #[test]
    fn the_summary_round_trips_through_json() {
        let mut s = summary(vec![view("inside", 0.05, Some(0.1)), view("outside", 0.04, None)]);
        s.views[1].full.rel_rmse = None;
        s.views[0].reconstructions = vec![("zh3".into(), scores(0.03))];
        let text = serde_json::to_string_pretty(&s).expect("serialize");
        assert_eq!(parse_summary(&text).expect("parse"), s);
        assert!(text.contains("\"schema\": 1"), "{text}");
    }

    #[test]
    fn another_schema_or_shape_is_refused_with_a_code() {
        assert_eq!(
            parse_summary("{\"schema\": 2}").expect_err("schema").code(),
            "summary_schema"
        );
        assert_eq!(
            parse_summary("{\"schema\": 1}").expect_err("shape").code(),
            "summary_parse"
        );
        assert_eq!(
            parse_summary("not json").expect_err("json").code(),
            "summary_parse"
        );
    }

    #[test]
    fn a_leak_above_threshold_is_flagged_and_one_below_is_not() {
        let s = summary(vec![
            view("inside", 0.05, Some(0.6)),
            view("door", 0.05, Some(0.2)),
        ]);
        let v = verdict(&s, None, DEFAULT_FLIP_TOLERANCE);
        assert_eq!(v.flags.len(), 1, "{v:?}");
        assert!(v.flags[0].starts_with("inside: leak 0.60 > 0.25"), "{v:?}");
    }

    #[test]
    fn a_flip_regression_against_the_baseline_is_flagged() {
        let base = summary(vec![view("inside", 0.05, None), view("gone", 0.05, None)]);
        let mut now = summary(vec![view("inside", 0.05, None), view("new", 0.05, None)]);
        assert!(
            verdict(&now, Some(&base), DEFAULT_FLIP_TOLERANCE)
                .flags
                .is_empty()
        );
        // Indirect only: 0.10 -> 0.13.
        now.views[0].indirect.flip_mean = 0.13;
        let v = verdict(&now, Some(&base), DEFAULT_FLIP_TOLERANCE);
        assert_eq!(v.flags.len(), 1, "{v:?}");
        assert!(
            v.flags[0].contains("indirect FLIP regressed 0.1000 -> 0.1300"),
            "{v:?}"
        );
        assert!(
            v.info.iter().any(|i| i.starts_with("new: not in the baseline")),
            "{v:?}"
        );
        // Within tolerance is not a regression; an improvement never is.
        now.views[0].indirect.flip_mean = 0.105;
        now.views[0].full.flip_mean = 0.01;
        assert!(
            verdict(&now, Some(&base), DEFAULT_FLIP_TOLERANCE)
                .flags
                .is_empty()
        );
    }

    #[test]
    fn an_inactive_bake_is_flagged() {
        let mut s = summary(vec![view("inside", 0.05, None)]);
        s.views[0].gi.active = false;
        s.views[0].gi.error = Some("volume too large".into());
        let v = verdict(&s, None, DEFAULT_FLIP_TOLERANCE);
        assert_eq!(
            v.flags,
            vec!["inside: the baked volume did not light the frame (volume too large)".to_string()]
        );
    }

    #[test]
    fn the_table_always_ends_by_sending_the_reader_to_the_plates() {
        for views in [vec![], vec![view("inside", 0.05, Some(0.9))]] {
            let s = summary(views);
            let table = render_table(&s, &verdict(&s, None, DEFAULT_FLIP_TOLERANCE));
            assert_eq!(table.lines().last(), Some(LOOK_AT_THE_PLATES), "{table}");
        }
    }

    /// A summary as `gi-compare` writes it, as literal JSON: the field names are the schema.
    fn fixture_json(inside_flip: f64, inside_leak: f64) -> String {
        let scores = |flip: f64| {
            serde_json::json!({
                "flip_mean": flip, "flip_p95": flip * 2.0, "ssim": 0.9, "rel_rmse": 0.25,
                "mean_reference": 40.0, "mean_test": 38.0
            })
        };
        let view = |name: &str, flip: f64, leak: Option<f64>| {
            serde_json::json!({
                "name": name,
                "sheet": format!("{name}.png"),
                "ev100": 11.0,
                "indirect_ev100": 10.0,
                "full": scores(flip),
                "indirect": scores(flip * 1.5),
                "ddgi_full": null,
                "reconstructions": [],
                "leak": leak.map(|l| serde_json::json!({
                    "pixels": 5000, "baked_mean": 12.0, "reference_mean": 8.0, "floor": 0.4,
                    "leak": l, "threshold": 0.25
                })),
                "gi": {
                    "installed": true, "active": true, "probes": 3487, "gpu_bytes": 1048576,
                    "error": null, "scales": [[1.0, 1.0, 1.0], [1.0, 1.0, 1.0], [0.0, 0.0, 0.0], [1.0, 1.0, 1.0]],
                    "sun_mismatch_deg": 0.0, "occluders": 0, "occluder_cap": 64,
                    "occluders_dropped": 0, "interior_bricks": 1
                },
                "timings": {
                    "raster_baked_ms": 40.0, "raster_ddgi_ms": 900.0, "raster_direct_ms": 35.0,
                    "reference_ms": 5200.0, "reference_direct_ms": 0.0, "compare_ms": 300.0
                },
                "tiles": [format!("{name}_baked.png")]
            })
        };
        serde_json::json!({
            "schema": 1,
            "fixture": "leak_room",
            "fixture_path": "assets/fixtures/gi_compare/leak_room.gi_compare.ron",
            "views_path": "assets/fixtures/gi_compare/leak_room.views.ron",
            "size": [480, 270],
            "spp": 1024,
            "ev100": 15.0,
            "indirect_ev100": 12.5,
            "reconstruction": "l1",
            "device": "test adapter",
            "bake": {
                "source": "baked", "probes": 3487, "valid": 3300, "inside_geometry": 120,
                "under_terrain": 0, "relocated": 410, "rejected_rays": 0, "seconds": 4.0
            },
            "views": [view("inside_door", inside_flip, Some(inside_leak)), view("outside", 0.05, None)],
            "total_seconds": 30.0
        })
        .to_string()
    }

    #[test]
    fn fixture_json_flags_a_leak_and_a_regression_against_a_stored_baseline() {
        let baseline = parse_summary(&fixture_json(0.08, 0.1)).expect("baseline parses");
        let healthy = parse_summary(&fixture_json(0.085, 0.2)).expect("parses");
        assert!(
            verdict(&healthy, Some(&baseline), DEFAULT_FLIP_TOLERANCE)
                .flags
                .is_empty(),
            "FLIP within tolerance, leak under threshold"
        );
        let leaking = parse_summary(&fixture_json(0.08, 0.5)).expect("parses");
        let v = verdict(&leaking, Some(&baseline), DEFAULT_FLIP_TOLERANCE);
        assert_eq!(v.flags.len(), 1, "{v:?}");
        assert!(v.flags[0].starts_with("inside_door: leak 0.50 > 0.25"), "{v:?}");
        // 0.08 -> 0.10 full and 0.12 -> 0.15 indirect: both regress.
        let worse = parse_summary(&fixture_json(0.10, 0.1)).expect("parses");
        let v = verdict(&worse, Some(&baseline), DEFAULT_FLIP_TOLERANCE);
        assert_eq!(v.flags.len(), 2, "{v:?}");
        assert!(
            v.flags
                .iter()
                .all(|f| f.starts_with("inside_door: ") && f.contains("FLIP regressed")),
            "{v:?}"
        );
        let table = render_table(&worse, &v);
        assert!(
            table.contains("flag: inside_door: full FLIP regressed"),
            "{table}"
        );
        assert_eq!(table.lines().last(), Some(LOOK_AT_THE_PLATES));
    }
}
