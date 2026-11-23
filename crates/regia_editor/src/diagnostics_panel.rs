// SPDX-License-Identifier: MPL-2.0
//! The Diagnostics window (`docs/specs/54-gpu-breadcrumbs-and-trace-markers.md` § 6, ADR-2771):
//! View → Diagnostics. The GPU tab shows the breadcrumb evidence level, the submitted, completed
//! and in-flight counters, the newest frame's pass list and a "load last crash" button; the
//! Timeline tab is [`trace_timeline`](crate::trace_timeline). The agent twins are
//! `editor.diagnostics` and `editor.trace_timeline` (`book/agents/editor-diagnostics.md`).
//!
//! Nothing here reimplements a reader: the live numbers come from the registered
//! `render.breadcrumbs` handler (the method an agent calls), and the crash view reads crash
//! reports with the same `gpu breadcrumbs:` block the reporter uploads. The state is a pure
//! function ([`gpu_state`], [`load_last_crash`]) so it is tested without a window.

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use serde_json::{Value, json};

/// The block heading a crash report carries (`regia_render::breadcrumbs`' `crash_text`).
pub const BLOCK_HEADING: &str = "gpu breadcrumbs:";

/// Which tab the window shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiagnosticsTab {
    /// Breadcrumb evidence, counters, the last frame's passes, the last crash.
    #[default]
    Gpu,
    /// The `.regiatrace` timeline.
    Timeline,
}

impl DiagnosticsTab {
    /// `gpu` or `timeline`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Gpu => "gpu",
            Self::Timeline => "timeline",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "gpu" => Some(Self::Gpu),
            "timeline" => Some(Self::Timeline),
            _ => None,
        }
    }
}

/// The window's state (a resource; absent = closed).
#[derive(Resource, Clone, Debug, Default)]
pub struct DiagnosticsPanel {
    /// The window is shown.
    pub open: bool,
    /// The tab shown.
    pub tab: DiagnosticsTab,
    /// The crash report last loaded.
    pub crash: Option<CrashView>,
    /// The last status line.
    pub status: String,
}

/// The live GPU numbers.
#[derive(Clone, Debug, PartialEq)]
pub struct GpuState {
    /// `none`, `recorded`, `fenced` or `timestamped`.
    pub level: String,
    /// 0 to 3.
    pub level_number: u64,
    /// The adapter name.
    pub adapter: String,
    /// Submissions made.
    pub submitted: u64,
    /// Highest submission that and every earlier one completed.
    pub completed: u64,
    /// `submitted - completed`.
    pub in_flight: u64,
    /// The frame the CPU last acknowledged.
    pub acknowledged_frame: u64,
    /// The newest frame's number.
    pub last_frame: Option<u64>,
    /// The newest frame's passes in the order the GPU received them.
    pub last_frame_passes: Vec<String>,
    /// Passes the GPU was most likely inside (loss reports).
    pub suspects: Vec<String>,
}

/// Calls the registered method `name` the way an agent does.
fn call(world: &mut World, name: &str, params: &Value) -> Result<Value, String> {
    let handler = world
        .get_resource::<regia_agent::extension::AgentExtension>()
        .and_then(|ext| ext.method(name))
        .map(|(_, handler)| handler)
        .ok_or_else(|| format!("{name} is not registered in this app"))?;
    handler(world, params).map_err(|e| e.message)
}

/// Reads the GPU tab's numbers from a `render.breadcrumbs` answer.
///
/// # Errors
/// The answer does not have the report shape.
pub fn gpu_state_from(answer: &Value) -> Result<GpuState, String> {
    let report = answer
        .get("report")
        .ok_or("render.breadcrumbs answered without a report")?;
    let counters = &report["counters"];
    let num = |v: &Value| v.as_u64().unwrap_or(0);
    let (submitted, completed) = (num(&counters["submitted"]), num(&counters["completed"]));
    let newest = report["frames"].as_array().and_then(|f| f.first());
    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };
    Ok(GpuState {
        level: report["level"].as_str().unwrap_or("none").to_owned(),
        level_number: num(&answer["level_number"]),
        adapter: report["adapter"]["name"].as_str().unwrap_or_default().to_owned(),
        submitted,
        completed,
        in_flight: submitted.saturating_sub(completed),
        acknowledged_frame: num(&counters["acknowledged_frame"]),
        last_frame: newest.map(|f| num(&f["frame"])),
        last_frame_passes: newest.map(|f| strings(&f["passes"])).unwrap_or_default(),
        suspects: report["suspects"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// The live GPU state, through `render.breadcrumbs { frames: 1 }`.
///
/// # Errors
/// Breadcrumbs are off, or the method is not registered (the message says which).
pub fn gpu_state(world: &mut World) -> Result<GpuState, String> {
    let answer = call(
        world,
        "render.breadcrumbs",
        &json!({ "frames": 1, "source": "live" }),
    )?;
    gpu_state_from(&answer)
}

/// A crash report and the `gpu breadcrumbs:` block inside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashView {
    /// Where it came from: a file path, or `bundle`.
    pub source: String,
    /// The first line of the report (its headline), or empty.
    pub headline: String,
    /// The `gpu breadcrumbs:` block (heading line plus its indented lines).
    pub block: String,
}

/// The `gpu breadcrumbs:` block of a crash report: the heading line and the indented lines that
/// follow it (up to the next unindented line). `None` when the report has no block.
#[must_use]
pub fn extract_block(text: &str) -> Option<String> {
    let mut lines = text.lines();
    let heading = lines.by_ref().find(|l| l.starts_with(BLOCK_HEADING))?;
    let mut block = vec![heading];
    block.extend(lines.take_while(|l| l.starts_with(' ') || l.starts_with('\t')));
    Some(block.join("\n"))
}

/// The newest crash report in `dir` (`crash-*.txt`, newest by modification time, then by name)
/// that has a `gpu breadcrumbs:` block.
#[must_use]
pub fn newest_crash_report(dir: &Path) -> Option<CrashView> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(regia_platform::crash::is_crash_file_name)
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort();
    files.into_iter().rev().find_map(|(_, path)| {
        let text = std::fs::read_to_string(&path).ok()?;
        Some(CrashView {
            source: path.display().to_string(),
            headline: text.lines().next().unwrap_or_default().to_owned(),
            block: extract_block(&text)?,
        })
    })
}

/// Loads the last crash: the newest crash report with a block in the crash directory, else the
/// newest crash bundle's `gpu_breadcrumbs.json` (through `render.breadcrumbs { source:
/// "last_bundle" }`, shown in the same text form).
///
/// # Errors
/// Neither exists (the message says where it looked).
pub fn load_last_crash(world: &mut World) -> Result<CrashView, String> {
    if let Some(dir) = regia_platform::crash::crash_dir()
        && let Some(view) = newest_crash_report(&dir)
    {
        return Ok(view);
    }
    let answer = call(world, "render.breadcrumbs", &json!({ "source": "last_bundle" }))
        .map_err(|e| format!("no crash report with a gpu breadcrumbs block, and no crash bundle ({e})"))?;
    let report: regia_render::breadcrumbs::BreadcrumbReport =
        serde_json::from_value(answer["report"].clone())
            .map_err(|e| format!("the crash bundle's report does not parse: {e}"))?;
    let block = report.crash_text();
    Ok(CrashView {
        source: "bundle".to_owned(),
        headline: report
            .lost
            .as_ref()
            .map(|l| format!("device_lost: {}: {}", l.reason, l.message))
            .unwrap_or_default(),
        block: block.trim_end().to_owned(),
    })
}

/// The agent answer for one panel state: the tab, the GPU numbers (or why there are none) and the
/// loaded crash.
#[must_use]
pub fn state_json(world: &mut World) -> Value {
    let panel = world
        .get_resource::<DiagnosticsPanel>()
        .cloned()
        .unwrap_or_default();
    let gpu = match gpu_state(world) {
        Ok(g) => json!({
            "level": g.level,
            "level_number": g.level_number,
            "adapter": g.adapter,
            "submitted": g.submitted,
            "completed": g.completed,
            "in_flight": g.in_flight,
            "acknowledged_frame": g.acknowledged_frame,
            "last_frame": g.last_frame,
            "last_frame_passes": g.last_frame_passes,
            "suspects": g.suspects,
        }),
        Err(e) => json!({ "unavailable": e }),
    };
    json!({
        "open": panel.open,
        "tab": panel.tab.name(),
        "gpu": gpu,
        "crash": panel.crash.as_ref().map(|c| json!({ "source": c.source, "headline": c.headline, "block": c.block })),
        "status": panel.status,
    })
}

/// Draws the window when it is open (called every editor frame).
pub fn window(ctx: &egui::Context, world: &mut World) {
    let Some(mut panel) = world.get_resource::<DiagnosticsPanel>().cloned() else {
        return;
    };
    if !panel.open {
        return;
    }
    let _span = tracing::info_span!("diagnostics_panel").entered();
    let mut open = panel.open;
    egui::Window::new("Diagnostics")
        .default_pos(crate::tool_windows::default_pos(ctx, "Diagnostics"))
        .open(&mut open)
        .default_width(720.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut panel.tab, DiagnosticsTab::Gpu, "GPU");
                ui.selectable_value(&mut panel.tab, DiagnosticsTab::Timeline, "Timeline");
            });
            ui.separator();
            match panel.tab {
                DiagnosticsTab::Gpu => gpu_tab(ui, world, &mut panel),
                DiagnosticsTab::Timeline => crate::trace_timeline::show(ui, world),
            }
        });
    panel.open = open;
    world.insert_resource(panel);
}

fn gpu_tab(ui: &mut egui::Ui, world: &mut World, panel: &mut DiagnosticsPanel) {
    match gpu_state(world) {
        Ok(g) => {
            ui.horizontal(|ui| {
                ui.strong(format!("Evidence: {} ({})", g.level, g.level_number));
                if !g.adapter.is_empty() {
                    ui.weak(&g.adapter);
                }
            });
            ui.monospace(format!(
                "submitted {}   completed {}   in flight {}   acknowledged frame {}",
                g.submitted, g.completed, g.in_flight, g.acknowledged_frame
            ));
            if !g.suspects.is_empty() {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("suspects: {}", g.suspects.join(", ")),
                );
            }
            ui.separator();
            ui.label(match g.last_frame {
                Some(frame) => format!("Last frame ({frame}): {} passes", g.last_frame_passes.len()),
                None => "No frame recorded yet".to_owned(),
            });
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                for (i, pass) in g.last_frame_passes.iter().enumerate() {
                    ui.monospace(format!("{:>3}  {pass}", i + 1));
                }
            });
        }
        Err(e) => {
            ui.weak(e);
        }
    }
    ui.separator();
    if ui.button("Load last crash bundle").clicked() {
        match load_last_crash(world) {
            Ok(view) => {
                panel.status = format!("loaded {}", view.source);
                panel.crash = Some(view);
            }
            Err(e) => {
                panel.crash = None;
                panel.status = e;
            }
        }
    }
    if !panel.status.is_empty() {
        ui.weak(&panel.status);
    }
    if let Some(crash) = &panel.crash {
        if !crash.headline.is_empty() {
            ui.strong(&crash.headline);
        }
        ui.monospace(&crash.block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = "device_lost: unknown: hung in ssao\nlocation: regia_render::gpu_health\n\
         gpu breadcrumbs: level 3 (timestamped), submitted 9, completed 8, acknowledged frame 4\n  \
         suspects (2): ssao, opaque\n  frame 4: shadow_cascades, ssao, opaque\nbacktrace: none\n";

    #[test]
    fn the_block_is_the_heading_and_its_indented_lines() {
        let block = extract_block(REPORT).expect("block");
        assert_eq!(block.lines().count(), 3, "{block}");
        assert!(block.starts_with("gpu breadcrumbs: level 3"));
        assert!(block.ends_with("opaque"));
        assert!(!block.contains("backtrace"));
        assert_eq!(extract_block("panic: boom\nlocation: x\n"), None);
        assert_eq!(extract_block(""), None);
    }

    #[test]
    fn the_newest_report_with_a_block_wins() {
        let dir = regia_test_utils::unique_temp_dir("diagnostics-crash");
        assert_eq!(newest_crash_report(&dir), None, "empty folder");
        std::fs::write(dir.join("crash-2026-10-01_00-00-00.txt"), REPORT).expect("old");
        std::fs::write(dir.join("notes.txt"), "gpu breadcrumbs: not a crash file name\n").expect("other");
        let view = newest_crash_report(&dir).expect("found");
        assert!(view.source.ends_with("crash-2026-10-01_00-00-00.txt"), "{view:?}");
        assert_eq!(view.headline, "device_lost: unknown: hung in ssao");
        // A newer report without a block is skipped; one with a block replaces the old one.
        let newer = dir.join("crash-2026-10-02_00-00-00.txt");
        std::fs::write(&newer, "panic: boom\n").expect("no block");
        assert!(
            newest_crash_report(&dir)
                .expect("old still")
                .source
                .ends_with("00-00-00.txt")
        );
        let t = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        std::fs::write(&newer, REPORT.replace("hung in ssao", "hung in fog")).expect("block");
        let file = std::fs::File::options().write(true).open(&newer).expect("open");
        file.set_modified(t).expect("mtime");
        assert_eq!(
            newest_crash_report(&dir).expect("new").headline,
            "device_lost: unknown: hung in fog"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gpu_numbers_come_from_the_method_answer() {
        let answer = json!({
            "source": "live",
            "level_number": 2,
            "report": {
                "level": "fenced",
                "adapter": { "name": "llvmpipe" },
                "counters": { "submitted": 12, "completed": 10, "acknowledged_frame": 7 },
                "suspects": [{ "name": "ssao", "frame": 7, "submission": 11 }],
                "frames": [
                    { "frame": 7, "passes": ["shadow_cascades", "opaque", "post"], "submissions": [], "truncated": false },
                    { "frame": 6, "passes": ["opaque"], "submissions": [], "truncated": false }
                ],
            }
        });
        let g = gpu_state_from(&answer).expect("parses");
        assert_eq!(
            (g.level.as_str(), g.level_number, g.adapter.as_str()),
            ("fenced", 2, "llvmpipe")
        );
        assert_eq!(
            (g.submitted, g.completed, g.in_flight, g.acknowledged_frame),
            (12, 10, 2, 7)
        );
        assert_eq!(g.last_frame, Some(7));
        assert_eq!(g.last_frame_passes, ["shadow_cascades", "opaque", "post"]);
        assert_eq!(g.suspects, ["ssao"]);
        assert!(gpu_state_from(&json!({})).is_err());
        // No frames yet: counters still show, the pass list is empty.
        let none = gpu_state_from(
            &json!({ "level_number": 0, "report": { "level": "none", "counters": {}, "frames": [] } }),
        )
        .expect("empty");
        assert_eq!(
            (none.last_frame, none.last_frame_passes.len(), none.in_flight),
            (None, 0, 0)
        );
    }

    #[test]
    fn without_the_extension_the_state_says_why() {
        let mut world = World::new();
        assert!(
            gpu_state(&mut world)
                .expect_err("none")
                .contains("render.breadcrumbs")
        );
        let state = state_json(&mut world);
        assert!(state["gpu"]["unavailable"].is_string());
        assert_eq!(state["tab"], "gpu");
        assert_eq!(state["open"], false);
    }

    #[test]
    fn tabs_round_trip_by_name() {
        for tab in [DiagnosticsTab::Gpu, DiagnosticsTab::Timeline] {
            assert_eq!(DiagnosticsTab::parse(tab.name()), Some(tab));
        }
        assert_eq!(DiagnosticsTab::parse("cpu"), None);
    }
}
