// SPDX-License-Identifier: MIT-0
//! The showcase performance table's shared part (ADR-2444, ADR-3013): the tier ladder, the views file
//! (`perf_views.json`), per-view frame statistics and the Markdown and JSON writers.
//!
//! Two producers write the same table: `cargo xtask showcase-table` (the prebuilt host once per tier on a
//! pinned card, frame times from `render.out_start { timings: true }`) and the interactive demo's benchmark
//! button ([`crate::demo`], agent `demo.benchmark`: the running app plays every view at the tier it runs,
//! frame times are the wall time between presented frames). [`Stamp::source`] says which one wrote a file.
//!
//! The views file is
//! `{ "version": 1, "views": [ { "name": "canopy", "path": <camera.set_path "path">, "fov": [...] } ] }`:
//! each view's object (minus `name` and `description`) is the params of one `camera.set_path` call.

use std::fmt::Write as _;

use serde_json::{Value, json};
/// The fixed step the views play at (frames per second of the render-out job).
pub const VIEW_FPS: u32 = 60;

/// One quality tier of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tier {
    /// Name on the command line and in the table.
    pub name: &'static str,
    /// The `--render-profile` preset the host starts with.
    pub preset: &'static str,
    /// `REGIA_FORWARD_LITE` forced for the host (`low` / `high`), or none.
    pub forward_lite: Option<&'static str>,
    /// What the tier needs from the GPU, with the basis. Design targets are not measurements.
    pub needs: &'static str,
}

/// Every tier, Ultra down to Low, then the Deck and the Mobile rows with the lite forward path.
pub const TIERS: &[Tier] = &[
    Tier {
        name: "ultra",
        preset: "DesktopUltra",
        forward_lite: None,
        needs: "Desktop mid tier or better (design target 1440p90 on an RTX 4070-class card, specs/26 § 4); the Vulkan feature floor is the Pascal-class set of specs/13 § 1",
    },
    Tier {
        name: "high",
        preset: "DesktopHigh",
        forward_lite: None,
        needs: "Desktop floor tier (design target 1080p60 on an RTX 3060 Ti-class card, specs/26 § 4); feature floor Pascal-class (GTX 1080 Ti audited, specs/13 § 1)",
    },
    Tier {
        name: "low",
        preset: "DesktopLow",
        forward_lite: None,
        needs: "Pascal-class desktop GPU and up (specs/13 § 1 feature floor); no frame-rate target is recorded for this tier",
    },
    Tier {
        name: "deck",
        preset: "Deck",
        forward_lite: None,
        needs: "Steam Deck APU (1280x800, 40 fps cap, temporal upscale from 70 %, specs/13 § 3)",
    },
    Tier {
        name: "mobile-high",
        preset: "MobileHigh",
        forward_lite: None,
        needs: "Vulkan phone GPU with 10 or more fragment storage buffers and 5 bind groups (Mali-G710 class audited, specs/13 § 1); the profile asks for the lite forward path, which only tile-GPU vendors take (`adapter_takes_lite`)",
    },
    Tier {
        name: "mobile-low",
        preset: "MobileLow",
        forward_lite: None,
        needs: "As mobile-high; the profile asks for the Low lite tier on tile GPUs",
    },
    Tier {
        name: "mobile-high-lite",
        preset: "MobileHigh",
        forward_lite: Some("high"),
        needs: "As mobile-high, with `REGIA_FORWARD_LITE=high` forcing the lite forward shader on any adapter (a desktop card or lavapipe then measures the lite shader's cost, not a phone)",
    },
    Tier {
        name: "mobile-low-lite",
        preset: "MobileLow",
        forward_lite: Some("low"),
        needs: "As mobile-low, with `REGIA_FORWARD_LITE=low` forcing the lite forward shader on any adapter",
    },
];

/// What the visibility-buffer path needs from a GPU, answered from the code (ADR-2444).
pub const VISBUFFER_REQUIREMENTS: &[&str] = &[
    "No mesh shaders: the cluster cull is a compute pass, the visibility pass is a hardware-raster instanced draw into an R32Uint target, and the material resolve is a full-screen fragment pass (`regia_render::visbuffer`, `vis_*.wgsl`). No wgpu mesh-shader feature is requested anywhere in the renderer.",
    "No 64-bit atomics: the software rasterizer for tiny clusters uses two 32-bit atomics per pixel (a depth pass, then an ID pass; ADR-1887).",
    "At least 10 storage buffers per fragment stage (`RESOLVE_STORAGE_BUFFERS`); below that, `VisBufferPass::supported` is false and the meshes stay on the forward path (`fallback_visbuffer_storage`).",
    "Optional: `MULTI_DRAW_INDIRECT_COUNT` and `INDIRECT_FIRST_INSTANCE` (the GPU-driven cull falls back to fixed-size instanced draws without them), and `TIMESTAMP_QUERY` (pass timings only).",
    "It is opt-in per profile (`visibility_buffer`, off in every preset) and per scene density (`VisibilityPath`, ADR-2071): the tier rows below do not turn it on.",
];

/// Resolves a `--tiers` list (`all` or comma-separated names) to tiers, in the table's order.
///
/// # Errors
///
/// A name is unknown or the list is empty.
pub fn select_tiers(list: &str) -> Result<Vec<Tier>, String> {
    let list = list.trim();
    if list.eq_ignore_ascii_case("all") {
        return Ok(TIERS.to_vec());
    }
    let mut out = Vec::new();
    for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let tier = TIERS
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                let known: Vec<&str> = TIERS.iter().map(|t| t.name).collect();
                format!("unknown tier `{name}` (known: {})", known.join(", "))
            })?;
        if !out.contains(tier) {
            out.push(*tier);
        }
    }
    if out.is_empty() {
        return Err("--tiers is empty".to_owned());
    }
    out.sort_by_key(|t| TIERS.iter().position(|k| k.name == t.name));
    Ok(out)
}

/// One camera view of the table.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    /// Row label.
    pub name: String,
    /// Params of the view's `camera.set_path` call (without `play`).
    pub params: Value,
    /// Seconds the path covers (its last key's time), for the length check.
    pub seconds: f64,
}

/// Reads a views file.
///
/// # Errors
///
/// Invalid JSON, a wrong `version`, no views, a duplicate or empty name, or a view without a
/// `path.keys` list of timed keys.
pub fn parse_views(text: &str) -> Result<Vec<View>, String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| format!("views file is not JSON: {e}"))?;
    if doc.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("views file `version` must be 1".to_owned());
    }
    let list = doc
        .get("views")
        .and_then(Value::as_array)
        .filter(|l| !l.is_empty())
        .ok_or("views file needs a non-empty `views` list")?;
    let mut out: Vec<View> = Vec::new();
    for entry in list {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| {
                !n.is_empty()
                    && n.len() <= 48
                    && n.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            })
            .ok_or("every view needs a `name` of 1..=48 letters, digits, `_` or `-`")?;
        if out.iter().any(|v| v.name == name) {
            return Err(format!("duplicate view name `{name}`"));
        }
        let keys = entry
            .pointer("/path/keys")
            .and_then(Value::as_array)
            .filter(|k| k.len() >= 2)
            .ok_or_else(|| format!("view `{name}` needs `path.keys` with at least two keys"))?;
        let seconds = keys
            .iter()
            .filter_map(|k| k.get("time").and_then(Value::as_f64))
            .fold(f64::NAN, f64::max);
        if !seconds.is_finite() || seconds <= 0.0 {
            return Err(format!(
                "view `{name}`: path keys need numeric `time` values and the last must be above 0"
            ));
        }
        let mut params = entry.clone();
        if let Some(object) = params.as_object_mut() {
            object.remove("name");
            object.remove("description");
        }
        out.push(View {
            name: name.to_owned(),
            params,
            seconds,
        });
    }
    Ok(out)
}

/// Fps and frame-time statistics of one view's measured frames.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewStats {
    /// Measured frames.
    pub frames: usize,
    /// Slowest frame as fps.
    pub min_fps: f64,
    /// Frames over total time (the harmonic mean of the per-frame fps).
    pub avg_fps: f64,
    /// Fastest frame as fps.
    pub max_fps: f64,
    /// Median frame time, ms.
    pub p50_ms: f64,
    /// 99th percentile frame time, ms (nearest rank).
    pub p99_ms: f64,
}

/// Statistics of frame times in ms; `None` for invalid samples or unrepresentable output.
#[must_use]
pub fn view_stats(frame_ms: &[f64]) -> Option<ViewStats> {
    if frame_ms.is_empty() || frame_ms.iter().any(|ms| !ms.is_finite() || *ms <= 0.0) {
        return None;
    }
    let mut sorted = frame_ms.to_vec();
    sorted.sort_by(f64::total_cmp);
    // An incremental mean avoids overflowing the sum for large, finite frame times.
    let mut mean = sorted[0];
    for (index, ms) in sorted.iter().enumerate().skip(1) {
        mean += (*ms - mean) / (index + 1) as f64;
    }
    let rank = |p: f64| sorted[(((sorted.len() - 1) as f64 * p).round() as usize).min(sorted.len() - 1)];
    let stats = ViewStats {
        frames: sorted.len(),
        min_fps: 1000.0 / sorted[sorted.len() - 1],
        avg_fps: 1000.0 / mean,
        max_fps: 1000.0 / sorted[0],
        p50_ms: rank(0.5),
        p99_ms: rank(0.99),
    };
    [
        stats.min_fps,
        stats.avg_fps,
        stats.max_fps,
        stats.p50_ms,
        stats.p99_ms,
    ]
    .into_iter()
    .all(|value| value.is_finite() && value > 0.0)
    .then_some(stats)
}

/// What a tier's run reported about the machine.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Machine {
    /// Adapter name.
    pub adapter: String,
    /// Backend (`Vulkan`).
    pub backend: String,
    /// Fragment-stage storage buffers (decides the visibility-buffer path).
    pub storage_buffers: Option<u64>,
    /// `MULTI_DRAW_INDIRECT_COUNT`.
    pub multi_draw_indirect_count: Option<bool>,
    /// `TIMESTAMP_QUERY`.
    pub timestamp_query: Option<bool>,
    /// The profile the host reported after start.
    pub profile: String,
}

/// True for a software adapter (lavapipe / llvmpipe / SwiftShader).
#[must_use]
pub fn is_software_adapter(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["llvmpipe", "lavapipe", "swiftshader", "software"]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// One row of the table: a tier and a view.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Tier name.
    pub tier: String,
    /// View name.
    pub view: String,
    /// The measurement, or `None` for `not run`.
    pub stats: Option<ViewStats>,
    /// Why a row is `not run` or failed.
    pub note: String,
}

/// Which producer wrote a table: the xtask over the render-out path, or the demo's benchmark button.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TableSource {
    /// `cargo xtask showcase-table`: frame time is one render-out frame's render and readback.
    #[default]
    Xtask,
    /// The interactive demo's benchmark (`demo.benchmark`, ADR-3013): frame time is the wall time between
    /// two presented frames of the running app.
    Demo,
}

impl TableSource {
    /// The name in the JSON record's `generator` field.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Xtask => "xtask showcase-table",
            Self::Demo => "demo benchmark",
        }
    }

    /// The paragraph under the Markdown title: who measured and what a frame time is.
    #[must_use]
    pub const fn preamble(self) -> &'static str {
        match self {
            Self::Xtask => {
                "Generated by `cargo xtask showcase-table` (ADR-2444). Every number is a measurement of the run stamped \
                 below; a row that says `not run` was not measured. Frame time is the wall time of one frame's render \
                 and readback as `render.out_start { timings: true }` records it (PNG encoding is not in it)."
            }
            Self::Demo => {
                "Generated by the showcase demo's benchmark button (`demo.benchmark`, ADR-3013). Every number is a \
                 measurement of the run stamped below; a row that says `not run` was not measured. Frame time is the \
                 wall time between two presented frames of the running app at the tier it ran (render, UI and \
                 present); each view's camera moves one 60 Hz step per frame, as in the xtask's table."
            }
        }
    }
}

/// Run context stamped into the files.
#[derive(Clone, Debug, Default)]
pub struct Stamp {
    /// Which producer wrote the table.
    pub source: TableSource,
    /// Date.
    pub date: String,
    /// Commit.
    pub commit: String,
    /// Driver text, or empty.
    pub driver: String,
    /// `REGIA_GPU_INDEX`, when a card was selected.
    pub gpu_index: Option<u32>,
    /// `REGIA_GPU_UUID` pin, when set.
    pub gpu_uuid: Option<String>,
    /// Output size.
    pub size: (u32, u32),
    /// Measured frames per view.
    pub frames: u32,
    /// Warm-up frames per view.
    pub warmup: u32,
    /// Software adapter run.
    pub cpu: bool,
    /// Scene directory as given.
    pub scene: String,
    /// What the machine reported, per tier (first non-empty wins for the header).
    pub machine: Machine,
}

fn cell(row: Option<&ViewStats>, pick: impl Fn(&ViewStats) -> String) -> String {
    row.map_or_else(|| "not run".to_owned(), pick)
}

/// The Markdown table.
#[must_use]
pub fn markdown(stamp: &Stamp, tiers: &[Tier], views: &[View], rows: &[Row]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Showcase performance table\n");
    let _ = writeln!(out, "{}\n", stamp.source.preamble());
    let _ = writeln!(out, "## Run\n");
    let _ = writeln!(out, "- Date: {}", stamp.date);
    let _ = writeln!(out, "- Commit: {}", stamp.commit);
    let _ = writeln!(out, "- Scene: `{}`", stamp.scene);
    let _ = writeln!(out, "- Resolution: {}x{}", stamp.size.0, stamp.size.1);
    let _ = writeln!(
        out,
        "- Frames per view: {} measured after {} warm-up frames at {VIEW_FPS} Hz",
        stamp.frames, stamp.warmup
    );
    let adapter = if stamp.machine.adapter.is_empty() {
        "not run".to_owned()
    } else {
        format!("{} ({})", stamp.machine.adapter, stamp.machine.backend)
    };
    let _ = writeln!(out, "- GPU: {adapter}");
    let _ = writeln!(
        out,
        "- Driver: {}",
        if stamp.driver.is_empty() {
            "not recorded"
        } else {
            &stamp.driver
        }
    );
    if let Some(index) = stamp.gpu_index {
        let _ = writeln!(out, "- REGIA_GPU_INDEX: {index}");
    }
    if let Some(uuid) = &stamp.gpu_uuid {
        let _ = writeln!(out, "- REGIA_GPU_UUID: {uuid}");
    }
    if stamp.cpu {
        let _ = writeln!(
            out,
            "- **CPU fallback numbers** (software adapter): they show the table runs and how tiers compare in CPU work, \
             and say nothing about any GPU's frame rate."
        );
    }
    let _ = writeln!(out, "\n## Per-view results\n");
    for tier in tiers {
        let _ = writeln!(
            out,
            "### {} (`{}`{})\n",
            tier.name,
            tier.preset,
            tier.forward_lite
                .map_or(String::new(), |l| format!(", forward lite {l}"))
        );
        let _ = writeln!(
            out,
            "| View | Frames | Min fps | Avg fps | Max fps | p50 ms | p99 ms | Note |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|---|---|");
        for view in views {
            let row = rows.iter().find(|r| r.tier == tier.name && r.view == view.name);
            let stats = row.and_then(|r| r.stats.as_ref());
            let note = row.map_or("", |r| r.note.as_str());
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} | {} | {} |",
                view.name,
                cell(stats, |s| s.frames.to_string()),
                cell(stats, |s| format!("{:.1}", s.min_fps)),
                cell(stats, |s| format!("{:.1}", s.avg_fps)),
                cell(stats, |s| format!("{:.1}", s.max_fps)),
                cell(stats, |s| format!("{:.2}", s.p50_ms)),
                cell(stats, |s| format!("{:.2}", s.p99_ms)),
                note
            );
        }
        out.push('\n');
    }
    let _ = writeln!(out, "## Lowest GPU class per tier\n");
    let _ = writeln!(
        out,
        "These are the specs' design targets and the feature floor, not results of this table; the table's rows are the measurements.\n"
    );
    let _ = writeln!(out, "| Tier | Preset | Needs |");
    let _ = writeln!(out, "|---|---|---|");
    for tier in TIERS {
        let _ = writeln!(out, "| {} | `{}` | {} |", tier.name, tier.preset, tier.needs);
    }
    let _ = writeln!(out, "\n## Does the visibility-buffer path need mesh shaders?\n");
    let _ = writeln!(out, "No. From the code:\n");
    for line in VISBUFFER_REQUIREMENTS {
        let _ = writeln!(out, "- {line}");
    }
    if let Some(n) = stamp.machine.storage_buffers {
        let _ = writeln!(
            out,
            "\nOn this run's adapter: {n} fragment-stage storage buffers (the path needs 10), `MULTI_DRAW_INDIRECT_COUNT` {}, `TIMESTAMP_QUERY` {}.",
            stamp
                .machine
                .multi_draw_indirect_count
                .map_or("unknown", |b| if b { "present" } else { "absent" }),
            stamp
                .machine
                .timestamp_query
                .map_or("unknown", |b| if b { "present" } else { "absent" }),
        );
    }
    let _ = writeln!(out, "\n## Other platforms\n");
    let _ = writeln!(
        out,
        "Mac and Android rows are not part of this table until a device pass measures them \
         (`cargo xtask android-pass`, the Mac host); they are `not run` here."
    );
    out
}

/// The JSON record.
#[must_use]
pub fn json(stamp: &Stamp, tiers: &[Tier], views: &[View], rows: &[Row]) -> String {
    let rows_json: Vec<Value> = tiers
        .iter()
        .flat_map(|tier| {
            views.iter().map(move |view| {
                let row = rows.iter().find(|r| r.tier == tier.name && r.view == view.name);
                let stats = row.and_then(|r| r.stats.as_ref());
                json!({
                    "tier": tier.name,
                    "preset": tier.preset,
                    "view": view.name,
                    "status": if stats.is_some() { "measured" } else { "not run" },
                    "frames": stats.map(|s| s.frames),
                    "min_fps": stats.map(|s| s.min_fps),
                    "avg_fps": stats.map(|s| s.avg_fps),
                    "max_fps": stats.map(|s| s.max_fps),
                    "p50_ms": stats.map(|s| s.p50_ms),
                    "p99_ms": stats.map(|s| s.p99_ms),
                    "note": row.map(|r| r.note.clone()).unwrap_or_default(),
                })
            })
        })
        .collect();
    let doc = json!({
        "schema": 1,
        "generator": stamp.source.name(),
        "date": stamp.date,
        "commit": stamp.commit,
        "scene": stamp.scene,
        "width": stamp.size.0,
        "height": stamp.size.1,
        "frames": stamp.frames,
        "warmup": stamp.warmup,
        "cpu_fallback": stamp.cpu,
        "adapter": stamp.machine.adapter,
        "backend": stamp.machine.backend,
        "driver": stamp.driver,
        "gpu_index": stamp.gpu_index,
        "gpu_uuid": stamp.gpu_uuid,
        "rows": rows_json,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"
}

/// The length check of a view: it must cover warm-up plus measured frames at [`VIEW_FPS`].
///
/// # Errors
///
/// The path ends before the last frame.
pub fn check_view_length(view: &View, warmup: u32, frames: u32) -> Result<(), String> {
    let needed = f64::from(warmup + frames) / f64::from(VIEW_FPS);
    if view.seconds + 1e-9 < needed {
        return Err(format!(
            "view_too_short: view `{}` ends at {:.2} s but {} warm-up + {} frames need {:.2} s",
            view.name, view.seconds, warmup, frames, needed
        ));
    }
    Ok(())
}

/// A `not run` row of `tier` and `view` with `note` (a `|` or line break in it would break the Markdown
/// table and is replaced).
#[must_use]
pub fn not_run(tier: &Tier, view: &View, note: &str) -> Row {
    Row {
        tier: tier.name.to_owned(),
        view: view.name.clone(),
        stats: None,
        note: note.replace('|', "/").replace('\n', " "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWS: &str = r#"{"version":1,"views":[
        {"name":"canopy","path":{"interp":"Linear","keys":[{"time":0,"position":[0,10,0]},{"time":6,"position":[5,10,0]}]},"look_at":{"Point":[0,5,10]}},
        {"name":"ground-1","path":{"interp":"Linear","keys":[{"time":0,"position":[0,2,0]},{"time":1,"position":[1,2,0]}]}}
    ]}"#;

    fn views() -> Vec<View> {
        match parse_views(VIEWS) {
            Ok(views) => views,
            Err(e) => panic!("views: {e}"),
        }
    }

    #[test]
    fn tiers_select_in_table_order_and_reject_unknown() {
        assert_eq!(select_tiers("all").map(|t| t.len()), Ok(TIERS.len()));
        let two = select_tiers("low, ultra").unwrap_or_default();
        assert_eq!(two.iter().map(|t| t.name).collect::<Vec<_>>(), ["ultra", "low"]);
        assert!(select_tiers("nope").is_err());
        assert!(select_tiers(" , ").is_err());
    }

    #[test]
    fn every_tier_names_a_preset_the_renderer_knows() {
        for tier in TIERS {
            assert!(
                regia_render::RenderProfilePreset::from_name(tier.preset).is_some(),
                "{}",
                tier.preset
            );
            if let Some(lite) = tier.forward_lite {
                assert!(
                    regia_render::forward_lite::ForwardLite::from_name(lite).is_some(),
                    "{lite}"
                );
            }
        }
    }

    #[test]
    fn views_parse_with_lengths_and_reject_bad_files() {
        let views = views();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].name, "canopy");
        assert!((views[0].seconds - 6.0).abs() < 1e-9);
        assert!(views[0].params.get("name").is_none());
        assert!(views[0].params.get("look_at").is_some());
        assert!(parse_views(r#"{"version":2,"views":[]}"#).is_err());
        assert!(parse_views(r#"{"version":1,"views":[]}"#).is_err());
        assert!(parse_views(r#"{"version":1,"views":[{"name":"a b","path":{"keys":[]}}]}"#).is_err());
        assert!(parse_views(&VIEWS.replace("ground-1", "canopy")).is_err());
        assert!(parse_views("not json").is_err());
    }

    #[test]
    fn short_views_are_refused_with_a_code() {
        let views = views();
        assert!(
            check_view_length(&views[0], 30, 240).is_ok(),
            "6 s covers 270 frames at 60 Hz"
        );
        let error = check_view_length(&views[1], 30, 240).err().unwrap_or_default();
        assert!(error.starts_with("view_too_short"), "{error}");
    }

    #[test]
    fn stats_use_harmonic_average_and_nearest_rank() {
        let mut ms = vec![10.0; 98];
        ms.extend([40.0, 40.0]);
        let Some(s) = view_stats(&ms) else {
            panic!("stats");
        };
        assert_eq!(s.frames, 100);
        assert!((s.min_fps - 25.0).abs() < 1e-9);
        assert!((s.max_fps - 100.0).abs() < 1e-9);
        assert!((s.avg_fps - 1000.0 * 100.0 / 1060.0).abs() < 1e-9);
        assert!((s.p50_ms - 10.0).abs() < 1e-9);
        assert!((s.p99_ms - 40.0).abs() < 1e-9, "rank 98.01 rounds to index 98");
        assert!(view_stats(&[]).is_none());
        assert!(view_stats(&[10.0, 0.0]).is_none());
        assert!(view_stats(&[10.0, f64::NAN]).is_none());
    }

    #[test]
    fn software_adapters_are_recognised() {
        assert!(is_software_adapter("llvmpipe (LLVM 18.1.3, 256 bits)"));
        assert!(is_software_adapter("Lavapipe"));
        assert!(!is_software_adapter("NVIDIA GeForce RTX 5080"));
    }

    fn stamp() -> Stamp {
        Stamp {
            date: "2026-11-04".into(),
            commit: "abc".into(),
            size: (2560, 1440),
            frames: 240,
            warmup: 30,
            scene: "scene".into(),
            ..Stamp::default()
        }
    }

    #[test]
    fn unmeasured_rows_say_not_run_and_no_number_appears() {
        let views = views();
        let tiers = select_tiers("ultra,low").unwrap_or_default();
        let rows: Vec<Row> = tiers
            .iter()
            .flat_map(|t| views.iter().map(move |v| not_run(t, v, "dry | run\nnow")))
            .collect();
        let md = markdown(&stamp(), &tiers, &views, &rows);
        assert!(
            md.contains(
                "| canopy | not run | not run | not run | not run | not run | not run | dry / run now |"
            ),
            "{md}"
        );
        assert!(md.contains("- GPU: not run"));
        assert!(md.contains("Driver: not recorded"));
        assert!(md.contains("No. From the code"));
        assert!(md.contains("Mac and Android rows"));
        assert!(md.contains("`cargo xtask showcase-table`"));
        let parsed: Value = serde_json::from_str(&json(&stamp(), &tiers, &views, &rows)).unwrap_or_default();
        assert_eq!(parsed["generator"], "xtask showcase-table");
        let list = parsed["rows"].as_array().cloned().unwrap_or_default();
        assert_eq!(list.len(), 4);
        assert!(
            list.iter()
                .all(|r| r["status"] == "not run" && r["avg_fps"].is_null())
        );
    }

    #[test]
    fn measured_rows_print_their_numbers_and_cpu_runs_are_marked() {
        let views = views();
        let tiers = select_tiers("high").unwrap_or_default();
        let rows = vec![Row {
            tier: "high".into(),
            view: "canopy".into(),
            stats: view_stats(&[8.0, 10.0, 12.5]),
            note: String::new(),
        }];
        let mut s = stamp();
        s.cpu = true;
        s.machine.adapter = "llvmpipe".into();
        s.machine.backend = "Vulkan".into();
        let md = markdown(&s, &tiers, &views, &rows);
        assert!(md.contains("CPU fallback numbers"));
        assert!(md.contains("| canopy | 3 | 80.0 |"), "{md}");
        assert!(md.contains("| ground-1 | not run |"));
    }

    #[test]
    fn a_demo_table_says_who_measured_and_keeps_the_layout() {
        let views = views();
        let tiers = select_tiers("high").unwrap_or_default();
        let rows: Vec<Row> = views.iter().map(|v| not_run(&tiers[0], v, "cancelled")).collect();
        let demo = Stamp {
            source: TableSource::Demo,
            ..stamp()
        };
        let md = markdown(&demo, &tiers, &views, &rows);
        assert!(md.contains("`demo.benchmark`"));
        assert!(!md.contains("`cargo xtask showcase-table`"));
        // Everything after the preamble is the xtask's layout.
        let tail = |text: &str| {
            text.split_once("## Run")
                .map(|(_, t)| t.to_owned())
                .unwrap_or_default()
        };
        assert_eq!(tail(&md), tail(&markdown(&stamp(), &tiers, &views, &rows)));
        let parsed: Value = serde_json::from_str(&json(&demo, &tiers, &views, &rows)).unwrap_or_default();
        assert_eq!(parsed["generator"], "demo benchmark");
        assert_eq!(parsed["schema"], 1);
    }

    #[test]
    fn the_shipped_example_views_parse_and_cover_the_default_run() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene/perf_views.json");
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let views = parse_views(&text).unwrap_or_default();
        assert!(views.len() >= 3);
        for view in &views {
            assert_eq!(check_view_length(view, 30, 240), Ok(()), "{}", view.name);
        }
    }
}
