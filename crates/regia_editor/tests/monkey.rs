// SPDX-License-Identifier: MPL-2.0
//! The editor monkey test (ADR-3290): a seeded random-input driver over every built-in panel.
//!
//! Each seed builds the edit world the editor binary builds (scene, editor resources, the
//! `editor.*` surface, play in editor), a temp project and the editor shell, then draws real egui
//! frames and feeds them the events a user's window would: pointer moves, clicks and drags inside
//! the focused panel, key presses and shortcuts, text, scrolls, tab drags (dock and undock), window
//! resizes, undo and redo, Play and Stop. Every step checks:
//!
//! - no panic (a panic fails the seed and prints the seed, the step and the replay command);
//! - every `Transform` and `GlobalTransform` in the edit and play worlds is finite;
//! - the undo and redo stacks stay within `UNDO_CAP`;
//! - egui painted no ID clash warning;
//! - in undo segments (input to the panels whose edits are undoable only), undoing back to the
//!   segment's start returns the scene snapshot to its starting hash.
//!
//! This is a `harness = false` test so it takes its own arguments. Gate run (no arguments): the
//! fixed seeds, 300 steps each, the regression seeds, and two planted-fault checks first (a short
//! run with a panic planted in a fixture op and one with a NaN `Transform` planted in the scene must
//! each fail at the planted step, so a driver that stopped catching faults fails the gate). Replay,
//! soak or prove the catch by hand (`--plant` passes only when every seed fails with the fault):
//!
//! ```text
//! cargo test -p regia_editor --test monkey -- --seed 7 --steps 120
//! REGIA_MONKEY_SEED=7 REGIA_MONKEY_STEPS=120 cargo test -p regia_editor --test monkey
//! cargo test -p regia_editor --test monkey -- --seed 7 --steps 5 --plant panic@2
//! cargo xtask editor-monkey --seeds 50 --steps 5000   # nightly (release build)
//! ```
//!
//! The driver re-runs itself in a child process with a scrubbed environment (empty `PATH`, no
//! display, no session bus, temp `HOME`), so a click can never open a native file dialog, a file
//! manager or a code editor on the host; it also never clicks a label that would (`DENIED`).

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::{Quat, Vec3};
use regia_editor::command::{EditorCommand, Reparent, SpawnEntities, UNDO_CAP, UndoStack};
use regia_editor::editor_layout::{BUILT_IN_PANELS, LayoutRequest, LayoutRequests};
use regia_editor::pie::{PieRequest, PieSession, PlayMode};
use regia_editor::shell::{EditorShell, Panel};
use regia_scene::level::LEVEL_SETTINGS_PATH;
use regia_scene::prelude::{GlobalTransform, Name, Transform, snapshot_scene};
use regia_scene::spawn::SnapshotFilter;

/// The gate's seeds.
const GATE_SEEDS: [u64; 4] = [0x5EED_0001, 0x5EED_0002, 0xC0FF_EE03, 0x0BAD_F00D];
/// Steps per gate seed.
const GATE_STEPS: u32 = 300;
/// Seeds pinned after a fix (seed, steps): each reproduced a crash before its fix.
const REGRESSION_SEEDS: &[(u64, u32)] = &[
    // Two floating windows with overflowing tab bars: egui_dock id clash, a debug-build panic
    // (fixed in `EditorShell::ui`).
    (0x5EED_0002, 240),
    (0x28, 220),
    // `-inf` / `1e40` typed into Inspector number fields made non-finite transforms (fixed in
    // `inspector::impls`).
    (0x3, 40),
    (0x7, 120),
    (0xb, 160),
];
/// Panels whose edits all go through the undo stack: input to them only, then undo back to the
/// start, must give the starting scene. (The undo-coverage ratchet's `undo_policy` will own this
/// list once it lands; until then it is kept here.)
const UNDOABLE: &[Panel] = &[
    Panel::Outliner,
    Panel::Inspector,
    Panel::Viewport,
    Panel::WorldSettings,
];
/// An undo segment starts every this many steps.
const SEGMENT_EVERY: u32 = 60;
/// Steps per undo segment.
const SEGMENT_STEPS: u32 = 14;
/// Labels the driver never clicks: they open a native dialog, a file manager, a code editor or
/// start an external process (the child's scrubbed environment is the second guard).
const DENIED: &[&str] = &[
    "…", "...", "import", "export", "show in", "reveal", "bake", "launch", "compile", "browse", "pick ",
    "quit", "exit",
];
/// The entities the scene starts with (`seed_scene`).
const SCENE_NAMES: &[&str] = &["Rock", "Tree", "Lamp", "Crate"];
/// Window sizes a resize picks from (plus a random one).
const SIZES: &[(f32, f32)] = &[
    (1280.0, 800.0),
    (1920.0, 1080.0),
    (800.0, 600.0),
    (640.0, 480.0),
    (400.0, 300.0),
    (160.0, 120.0),
    (2560.0, 1440.0),
    (1024.0, 1600.0),
];
/// Text the driver types.
const TEXTS: &[&str] = &[
    "a",
    "Cube",
    "nan",
    "NaN",
    "inf",
    "-inf",
    "1e40",
    "-1e40",
    "0",
    "-0",
    "1/0",
    "-1",
    "0.5",
    "9999999999999999999999",
    "\u{1F600}",
    "名前",
    "  ",
    "../../x",
    "\u{0}",
    "\t",
    "%s%n{}",
    "()[",
    "Ω≈ç√",
    "asset:abc",
];

// ------------------------------------------------------------------------------------------------
// Arguments and the isolated child
// ------------------------------------------------------------------------------------------------

/// What to run.
#[derive(Debug, Clone, PartialEq)]
struct Plan {
    seeds: Vec<u64>,
    steps: u32,
    /// Whether the regression seeds run too (only for the default gate run).
    regressions: bool,
    /// A fault planted into every run (`--plant panic@STEP` / `nan@STEP`): the run must fail.
    plant: Option<Plant>,
}

/// A fault the driver plants into a fixture op at one step, to prove the checks catch it: a
/// panic inside the op, or a non-finite `Transform` written into the scene (an invariant break).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plant {
    kind: PlantKind,
    step: u32,
}

/// What [`Plant`] breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlantKind {
    /// The op panics.
    Panic,
    /// The op writes a NaN translation into a scene entity's `Transform`.
    NonFinite,
}

impl Plant {
    /// `panic@3` / `nan@3`.
    fn parse(text: &str) -> Result<Self, String> {
        let (kind, step) = text
            .split_once('@')
            .ok_or_else(|| format!("--plant {text}: expected panic@STEP or nan@STEP"))?;
        let kind = match kind {
            "panic" => PlantKind::Panic,
            "nan" => PlantKind::NonFinite,
            other => return Err(format!("--plant {other}: expected panic or nan")),
        };
        let step = step
            .parse()
            .map_err(|_| format!("--plant {text}: {step} is not a step"))?;
        Ok(Self { kind, step })
    }
}

/// Reads `--seed N` / `--seeds N|a,b,c` / `--steps N` and `REGIA_MONKEY_SEED`, `REGIA_MONKEY_SEEDS`,
/// `REGIA_MONKEY_STEPS`. `--seeds 50` means 50 seeds counted from `--first-seed` (default 1). Flags
/// libtest would take (`--nocapture`, `--test-threads=4`, filters) are ignored; a filter that does
/// not name this test skips it (`None`).
fn plan(args: &[String], env: impl Fn(&str) -> Option<String>) -> Result<Option<Plan>, String> {
    let mut seeds: Option<Vec<u64>> = None;
    let mut steps: Option<u32> = None;
    let mut first: u64 = 1;
    let mut count: Option<u64> = None;
    let mut filtered_out = false;
    let mut plant = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = |name: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--seed" => seeds = Some(vec![parse_seed(&value("--seed")?)?]),
            "--seeds" => {
                let text = value("--seeds")?;
                if text.contains(',') {
                    seeds = Some(text.split(',').map(parse_seed).collect::<Result<_, _>>()?);
                } else {
                    count = Some(text.parse().map_err(|_| format!("--seeds {text}: not a count"))?);
                }
            }
            "--first-seed" => first = parse_seed(&value("--first-seed")?)?,
            "--steps" => {
                let text = value("--steps")?;
                steps = Some(text.parse().map_err(|_| format!("--steps {text}: not a count"))?);
            }
            "--plant" => plant = Some(Plant::parse(&value("--plant")?)?),
            "--list" => {
                println!("monkey: test");
                return Ok(None);
            }
            other if other.starts_with('-') => {}
            filter => filtered_out |= !"monkey".contains(filter),
        }
    }
    if filtered_out {
        return Ok(None);
    }
    if seeds.is_none() {
        if let Some(text) = env("REGIA_MONKEY_SEED") {
            seeds = Some(vec![parse_seed(&text)?]);
        } else if let Some(text) = env("REGIA_MONKEY_SEEDS") {
            seeds = Some(text.split(',').map(parse_seed).collect::<Result<_, _>>()?);
        }
    }
    if steps.is_none()
        && let Some(text) = env("REGIA_MONKEY_STEPS")
    {
        steps = Some(
            text.parse()
                .map_err(|_| format!("REGIA_MONKEY_STEPS={text}: not a count"))?,
        );
    }
    if let Some(count) = count {
        seeds = Some((first..first.saturating_add(count)).collect());
    }
    let regressions = seeds.is_none() && plant.is_none();
    Ok(Some(Plan {
        seeds: seeds.unwrap_or_else(|| GATE_SEEDS.to_vec()),
        steps: steps.unwrap_or(GATE_STEPS),
        regressions,
        plant,
    }))
}

/// A seed in decimal or `0x` hex.
fn parse_seed(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(&hex.replace('_', ""), 16),
        None => text.replace('_', "").parse(),
    };
    parsed.map_err(|_| format!("{text}: not a seed"))
}

/// Variables the isolated child keeps (everything else is dropped).
const KEEP_ENV: &[&str] = &[
    "RUST_BACKTRACE",
    "RUST_LOG",
    "RUST_MIN_STACK",
    "LD_LIBRARY_PATH",
    "TMPDIR",
    "TMP",
    "TEMP",
    "SYSTEMROOT",
    "SystemRoot",
    "WINDIR",
    "REGIA_MONKEY_SEED",
    "REGIA_MONKEY_SEEDS",
    "REGIA_MONKEY_STEPS",
    "REGIA_MONKEY_THREADS",
];

/// Runs this binary again with a scrubbed environment and returns its exit code.
fn run_isolated(args: &[String]) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("monkey: cannot find the test binary: {e}");
            return 1;
        }
    };
    let sandbox = regia_test_utils::unique_temp_dir("monkey-env");
    let empty_bin = sandbox.join("bin");
    let home = sandbox.join("home");
    for dir in [&empty_bin, &home] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("monkey: cannot create {}: {e}", dir.display());
            return 1;
        }
    }
    let mut command = std::process::Command::new(exe);
    command.args(args).env_clear();
    for key in KEEP_ENV {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("REGIA_MONKEY_CHILD", "1")
        .env("PATH", &empty_bin)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_RUNTIME_DIR", &home)
        .env("APPDATA", &home)
        .env("USERPROFILE", &home)
        .env("REGIA_EDITOR_CONFIG_DIR", home.join("regia"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/regia-monkey-bus",
        );
    let code = match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("monkey: cannot start the isolated run: {e}");
            1
        }
    };
    let _ = std::fs::remove_dir_all(&sandbox);
    code
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if std::env::var_os("REGIA_MONKEY_CHILD").is_none() && !args.iter().any(|a| a == "--no-isolate") {
        // Parse first so a bad argument fails before the child starts.
        match plan(&args, |key| std::env::var(key).ok()) {
            Ok(Some(_)) => std::process::exit(run_isolated(&args)),
            Ok(None) => return,
            Err(e) => {
                eprintln!("monkey: {e}");
                std::process::exit(2);
            }
        }
    }
    let plan = match plan(&args, |key| std::env::var(key).ok()) {
        Ok(Some(plan)) => plan,
        Ok(None) => return,
        Err(e) => {
            eprintln!("monkey: {e}");
            std::process::exit(2);
        }
    };
    self_checks();
    if plan.regressions
        && let Err(report) = plant_checks()
    {
        eprintln!("{report}");
        eprintln!("monkey: test result: FAILED (planted-fault check)");
        std::process::exit(1);
    }
    let mut runs: Vec<(u64, u32)> = plan.seeds.iter().map(|&seed| (seed, plan.steps)).collect();
    if plan.regressions {
        runs.extend_from_slice(REGRESSION_SEEDS);
    }
    let threads = std::env::var("REGIA_MONKEY_THREADS")
        .ok()
        .and_then(|t| t.parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(2, |n| n.get().min(4)))
        .max(1);
    println!(
        "monkey: {} run(s), {} thread(s): {}",
        runs.len(),
        threads,
        runs.iter()
            .map(|(seed, steps)| format!("{seed:#x}x{steps}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let queue = std::sync::Mutex::new(runs.into_iter());
    let failures = std::sync::Mutex::new(Vec::<String>::new());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().ok().and_then(|mut q| q.next());
                    let Some((seed, steps)) = next else { break };
                    let result = run_seed_deep(seed, steps, plan.plant);
                    if let Some(plant) = plan.plant {
                        // A planted run passes when the planted fault is what failed it.
                        if planted_caught(plant, &result) {
                            println!(
                                "monkey: seed {seed:#x} x {steps}: planted {:?} caught",
                                plant.kind
                            );
                        } else {
                            eprintln!(
                                "monkey: seed {seed:#x}: the planted {:?} fault was not caught: {result:?}",
                                plant.kind
                            );
                            if let Ok(mut f) = failures.lock() {
                                f.push(format!("seed {seed:#x} x {steps} (planted fault not caught)"));
                            }
                        }
                        continue;
                    }
                    match result {
                        Ok(stats) => println!("monkey: seed {seed:#x} x {steps}: ok ({stats})"),
                        Err(report) => {
                            eprintln!("{report}");
                            if let Ok(mut f) = failures.lock() {
                                f.push(format!("seed {seed:#x} x {steps}"));
                            }
                        }
                    }
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap_or_default();
    if failures.is_empty() {
        println!("monkey: test result: ok");
    } else {
        eprintln!("monkey: test result: FAILED ({})", failures.join(", "));
        std::process::exit(1);
    }
}

/// Unit checks of the argument parser and the label rules (no libtest here, so they run first).
fn self_checks() {
    let none = |_: &str| None;
    let args = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let gate = plan(&[], none).ok().flatten();
    assert_eq!(
        gate,
        Some(Plan {
            seeds: GATE_SEEDS.to_vec(),
            steps: GATE_STEPS,
            regressions: true,
            plant: None,
        })
    );
    let replay = plan(&args(&["--seed", "0x10", "--steps", "7", "--nocapture"]), none)
        .ok()
        .flatten();
    assert_eq!(
        replay,
        Some(Plan {
            seeds: vec![16],
            steps: 7,
            regressions: false,
            plant: None,
        })
    );
    let nightly = plan(&args(&["--seeds", "3", "--first-seed", "100"]), none)
        .ok()
        .flatten();
    assert_eq!(nightly.map(|p| p.seeds), Some(vec![100, 101, 102]));
    let env = |key: &str| match key {
        "REGIA_MONKEY_SEED" => Some("42".to_owned()),
        "REGIA_MONKEY_STEPS" => Some("9".to_owned()),
        _ => None,
    };
    let from_env = plan(&[], env).ok().flatten();
    assert_eq!(from_env.map(|p| (p.seeds, p.steps)), Some((vec![42], 9)));
    assert!(
        plan(&args(&["other_test"]), none).ok().flatten().is_none(),
        "a filter skips"
    );
    assert!(plan(&args(&["--steps", "x"]), none).is_err());
    let planted = plan(&args(&["--seed", "5", "--plant", "nan@2"]), none)
        .ok()
        .flatten()
        .and_then(|p| p.plant);
    assert_eq!(
        planted,
        Some(Plant {
            kind: PlantKind::NonFinite,
            step: 2
        })
    );
    assert!(plan(&args(&["--plant", "boom@1"]), none).is_err());
    assert!(plan(&args(&["--plant", "panic"]), none).is_err());
    assert!(denied("Import…") && denied("Bake all") && denied("Show in file manager"));
    assert!(!denied("Undo") && !denied("Add component"));
    assert!(is_undo_label("⟲ Undo") && is_undo_label("Redo") && !is_undo_label("Add"));
    let mut a = Rng::new(9);
    let mut b = Rng::new(9);
    assert_eq!(
        (0..8).map(|_| a.next()).collect::<Vec<_>>(),
        (0..8).map(|_| b.next()).collect::<Vec<_>>()
    );
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

// ------------------------------------------------------------------------------------------------
// Random numbers
// ------------------------------------------------------------------------------------------------

/// SplitMix64 (Steele, Lea and Flood, "Fast splittable pseudorandom number generators", 2014):
/// one seed gives one input sequence on every platform.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    /// Uniform in `0.0..1.0`.
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn chance(&mut self, p: f32) -> bool {
        self.unit() < p
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// A point inside `rect`.
    fn point_in(&mut self, rect: egui::Rect) -> egui::Pos2 {
        egui::pos2(
            rect.min.x + self.unit() * rect.width().max(0.0),
            rect.min.y + self.unit() * rect.height().max(0.0),
        )
    }
}

// ------------------------------------------------------------------------------------------------
// The driver
// ------------------------------------------------------------------------------------------------

/// A text label the last frame painted, with its screen rectangle.
#[derive(Debug, Clone)]
struct Label {
    text: String,
    rect: egui::Rect,
}

/// Whether the driver must not click `label`.
fn denied(label: &str) -> bool {
    let lower = label.to_lowercase();
    DENIED.iter().any(|d| lower.contains(d))
}

/// Whether `label` steps the undo stack (an Undo or Redo button or menu entry).
fn is_undo_label(label: &str) -> bool {
    let lower = label.to_lowercase();
    lower.contains("undo") || lower.contains("redo")
}

/// What the run did, for the summary line.
#[derive(Debug, Default)]
struct Stats {
    frames: u64,
    clicks: u32,
    denied: u32,
    plays: u32,
    segments: u32,
    segments_skipped: u32,
    undone: u32,
    panels_drawn: std::collections::BTreeSet<&'static str>,
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} frames, {} clicks ({} denied labels avoided), {} plays, {} undo segments ({} skipped, {} undos), {}/{} panels drawn",
            self.frames,
            self.clicks,
            self.denied,
            self.plays,
            self.segments,
            self.segments_skipped,
            self.undone,
            self.panels_drawn.len(),
            BUILT_IN_PANELS.len()
        )
    }
}

struct Monkey {
    app: App,
    shell: EditorShell,
    ctx: egui::Context,
    session: Option<PieSession>,
    rng: Rng,
    screen: egui::Vec2,
    time: f64,
    pointer: egui::Pos2,
    labels: Vec<Label>,
    id_clashes: Vec<String>,
    focus: Panel,
    /// Panels still to visit before any repeats (shuffled from the seed).
    deck: Vec<Panel>,
    /// Steps since the focus last moved.
    since_focus: u32,
    history: VecDeque<String>,
    stats: Stats,
    root: PathBuf,
    config: PathBuf,
}

/// The edit world the editor binary builds (`main.rs` `edit_app`), without the parts that start
/// threads or sockets (agent server, asset server, extension discovery, SpaceMouse).
fn edit_app(root: &Path, config: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut()
        .insert_resource(regia_editor::editor_prefs::EditorConfigPaths::at(config));
    regia_editor::editor_prefs::load_prefs(app.world_mut());
    regia_editor::editor_keymap::load_keymap(app.world_mut());
    regia_editor::quit::install(&mut app);
    app.init_resource::<regia_render::RuntimeRenderer>()
        .init_resource::<regia_render::ExtractedFrame>();
    app.world_mut().spawn(regia_platform::PrimaryWindow);
    app.init_resource::<regia_render::ScreenshotRequests>()
        .init_resource::<UndoStack>()
        .init_resource::<regia_editor::selection::Selection>()
        .init_resource::<regia_editor::viewport::EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>()
        .init_resource::<regia_editor::pie::PieRequests>()
        .init_resource::<regia_editor::pie::PieStatus>()
        .init_resource::<regia_editor::pie_net::PieNet>()
        .init_resource::<regia_editor::prefab_edit::PrefabEditMode>()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    app.register_type::<regia_framework::camera::CameraRig>()
        .register_type::<regia_framework::camera::CameraMode>()
        .register_type::<regia_framework::camera::ThirdPerson>()
        .register_type::<regia_framework::camera::Overhead>()
        .register_type::<regia_framework::camera::FirstPerson>()
        .register_type::<regia_framework::camera::Free>()
        .register_type::<regia_framework::camera::ChaseCamera>()
        .register_type::<regia_render::Primitive>()
        .register_type::<regia_ai::smart_object::SmartObject>()
        .register_type::<regia_ai::smart_object::SmartObjectSlot>()
        .init_resource::<regia_editor::state_tree_panel::StateTreePanel>()
        .init_resource::<regia_editor::smart_object_panel::SmartObjectPanel>()
        .init_resource::<regia_editor::camera_panel::CameraPanel>();
    app.add_plugins(regia_framework::camera::load::CameraAssetsPlugin);
    regia_editor::lightmap_panel::install_level_lightmaps(&mut app);
    app.world_mut()
        .resource_mut::<regia_framework::camera::director::CameraHooks>()
        .accept_any_name();
    if let Err(err) = regia_editor::project::open_project(app.world_mut(), root) {
        panic!("the monkey project did not open: {err}");
    }
    app.insert_resource(regia_procgen::world::ProcGenSettings {
        editor: true,
        source: regia_procgen::world::ProcGraphSource::Dir(root.to_path_buf()),
        ..regia_procgen::world::ProcGenSettings::default()
    });
    app.add_plugins(regia_procgen::ProcGenPlugin);
    regia_editor::agent_ext::install(app.world_mut());
    app.finish_plugins();
    app.update();
    regia_editor::visibility::mark_internal_entities(app.world_mut());
    app
}

/// A project with a level, a code-editor setting that names no real program, and no layout.
fn monkey_project(seed: u64) -> (PathBuf, PathBuf) {
    let root = regia_test_utils::unique_temp_dir(&format!("monkey-{seed:x}"));
    let config = regia_test_utils::unique_temp_dir(&format!("monkey-config-{seed:x}"));
    let write = |path: PathBuf, text: &str| {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("monkey project folder");
        }
        std::fs::write(&path, text).expect("monkey project file");
    };
    write(
        root.join("Regia.toml"),
        "[project]\nname = \"Monkey\"\nid = \"com.example.monkey\"\n",
    );
    write(
        root.join("assets/levels/main.level.ron"),
        "( format_version: 1, entities: [] )",
    );
    write(
        root.join(".regia/agent.toml"),
        "[editor]\ncode_editor = [\"regia-monkey-no-such-editor\", \"{file}\"]\n",
    );
    write(
        config.join(regia_editor::editor_prefs::PREFS_FILE),
        "(\n    format_version: 1,\n    values: {\n        \"code.editor_command\": Text(\"regia-monkey-no-such-editor {file}\"),\n        \"layout.save_on_exit\": Bool(false),\n    },\n)",
    );
    (root, config)
}

/// Spawns a few entities (one parented) through the editor's own undoable spawn path.
fn seed_scene(world: &mut World) {
    let mut spawned = Vec::new();
    for (i, name) in SCENE_NAMES.iter().copied().enumerate() {
        let transform = Transform {
            translation: Vec3::new(i as f32 * 2.0 - 3.0, 0.5, -(i as f32)),
            rotation: Quat::from_rotation_y(i as f32 * 0.4),
            scale: Vec3::ONE,
        };
        let mut command =
            SpawnEntities::new_entity(world, Guid::new_random(), name, transform).expect("monkey spawn");
        command.apply(world);
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(Box::new(command));
        let mut query = world.query::<(Entity, &Name)>();
        if let Some(entity) = query
            .iter(world)
            .find(|(_, n)| n.as_str() == name)
            .map(|(e, _)| e)
        {
            spawned.push(entity);
        }
    }
    if let &[parent, child, ..] = spawned.as_slice() {
        let mut command = Reparent::capture(world, child, Some(parent));
        command.apply(world);
        world.resource_mut::<UndoStack>().push(Box::new(command));
    }
    world.resource_mut::<UndoStack>().seal();
}

/// The level snapshot `editor.save_level` and PIE use, as text.
fn scene_text(world: &mut World) -> String {
    let filter = SnapshotFilter {
        level: None,
        resources: vec![LEVEL_SETTINGS_PATH.to_string()],
    };
    match snapshot_scene(world, &filter) {
        Ok(file) => file
            .to_ron_string()
            .unwrap_or_else(|e| format!("snapshot does not serialize: {e}")),
        Err(e) => format!("snapshot failed: {e}"),
    }
}

fn hash_text(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The first line where `a` and `b` differ, for the failure report.
fn first_difference(a: &str, b: &str) -> String {
    for (i, (la, lb)) in a.lines().zip(b.lines()).enumerate() {
        if la != lb {
            return format!("line {}: before `{}`, after `{}`", i + 1, la.trim(), lb.trim());
        }
    }
    format!("lengths {} and {} lines", a.lines().count(), b.lines().count())
}

/// The non-finite transforms of `world`.
fn bad_transforms(world: &mut World, which: &str) -> Vec<String> {
    let mut bad = Vec::new();
    let mut local = world.query::<(Entity, &Transform)>();
    for (entity, t) in local.iter(world) {
        if !(t.translation.is_finite() && t.rotation.is_finite() && t.scale.is_finite()) {
            bad.push(format!("{which} {entity:?} Transform {t:?}"));
        }
    }
    let mut global = world.query::<(Entity, &GlobalTransform)>();
    for (entity, g) in global.iter(world) {
        if !g.0.is_finite() {
            bad.push(format!("{which} {entity:?} GlobalTransform {:?}", g.0));
        }
    }
    bad
}

/// Collects the painted text labels and the ID clash warnings of a frame's shapes.
fn collect_shapes(shapes: &[egui::epaint::ClippedShape], labels: &mut Vec<Label>, clashes: &mut Vec<String>) {
    fn walk(
        shape: &egui::epaint::Shape,
        clip: egui::Rect,
        labels: &mut Vec<Label>,
        clashes: &mut Vec<String>,
    ) {
        match shape {
            egui::epaint::Shape::Vec(shapes) => {
                for shape in shapes {
                    walk(shape, clip, labels, clashes);
                }
            }
            egui::epaint::Shape::Text(text) => {
                let string = text.galley.text().to_owned();
                if string.contains(" use of ") && string.contains(" ID ") {
                    clashes.push(string);
                    return;
                }
                let rect = text.visual_bounding_rect().intersect(clip);
                if rect.is_positive() && !string.trim().is_empty() {
                    labels.push(Label { text: string, rect });
                }
            }
            _ => {}
        }
    }
    for clipped in shapes {
        walk(&clipped.shape, clipped.clip_rect, labels, clashes);
    }
}

fn key_event(key: egui::Key, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers,
    }
}

fn button_event(
    pos: egui::Pos2,
    button: egui::PointerButton,
    pressed: bool,
    modifiers: egui::Modifiers,
) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers,
    }
}

/// Keys the driver presses (with random modifiers on top of the listed ones).
const KEYS: &[egui::Key] = &[
    egui::Key::W,
    egui::Key::E,
    egui::Key::R,
    egui::Key::F,
    egui::Key::G,
    egui::Key::V,
    egui::Key::C,
    egui::Key::Q,
    egui::Key::S,
    egui::Key::A,
    egui::Key::D,
    egui::Key::X,
    egui::Key::B,
    egui::Key::I,
    egui::Key::P,
    egui::Key::K,
    egui::Key::Delete,
    egui::Key::Backspace,
    egui::Key::Escape,
    egui::Key::Enter,
    egui::Key::Space,
    egui::Key::Tab,
    egui::Key::ArrowUp,
    egui::Key::ArrowDown,
    egui::Key::ArrowLeft,
    egui::Key::ArrowRight,
    egui::Key::Home,
    egui::Key::End,
    egui::Key::PageUp,
    egui::Key::PageDown,
    egui::Key::F2,
    egui::Key::Num1,
    egui::Key::Num0,
];

impl Monkey {
    fn new(seed: u64) -> Self {
        let (root, config) = monkey_project(seed);
        let mut app = edit_app(&root, &config);
        seed_scene(app.world_mut());
        let shell = EditorShell::new(None, Some(root.clone()));
        let ctx = egui::Context::default();
        ctx.options_mut(|o| o.warn_on_id_clash = true);
        let mut monkey = Self {
            app,
            shell,
            ctx,
            session: None,
            rng: Rng::new(seed),
            screen: egui::vec2(1280.0, 800.0),
            time: 0.0,
            pointer: egui::Pos2::ZERO,
            labels: Vec::new(),
            id_clashes: Vec::new(),
            focus: Panel::Viewport,
            deck: Vec::new(),
            since_focus: 0,
            history: VecDeque::new(),
            stats: Stats::default(),
            root,
            config,
        };
        // Every built-in panel gets a tab (Window ▸ <panel>), then the viewport comes forward.
        for panel in BUILT_IN_PANELS {
            LayoutRequests::push(monkey.app.world_mut(), LayoutRequest::Open(*panel));
        }
        LayoutRequests::push(monkey.app.world_mut(), LayoutRequest::Open(Panel::Viewport));
        monkey.frames(2, Vec::new());
        // Each panel comes forward and draws once before the random input starts.
        for panel in BUILT_IN_PANELS {
            LayoutRequests::push(monkey.app.world_mut(), LayoutRequest::Open(*panel));
            monkey.frames(2, Vec::new());
        }
        monkey
    }

    fn note(&mut self, line: String) {
        if self.history.len() == 40 {
            self.history.pop_front();
        }
        self.history.push_back(line);
    }

    /// One frame in the editor binary's order: PIE requests, play and panel pumps, the edit world's
    /// update, then the UI with `events` and the shell's frame effects.
    fn frame(&mut self, events: Vec<egui::Event>) {
        let Self {
            app, shell, session, ..
        } = self;
        let requests = std::mem::take(
            &mut app
                .world_mut()
                .resource_mut::<regia_editor::pie::PieRequests>()
                .queue,
        );
        for request in requests {
            let toast = regia_editor::pie::apply_request(app.world_mut(), session, request, None);
            shell.set_toast(toast);
        }
        regia_editor::pie_worlds::pump(app.world_mut(), session.as_mut());
        if let Some(running) = session.as_mut()
            && let Some(panic) = running.frame()
        {
            shell.set_toast(format!("play stopped by a panic: {panic}"));
            if let Some(stopped) = session.take() {
                regia_editor::pie::stop_play(stopped);
            }
        }
        regia_editor::pie::sync_status(app.world_mut(), session.as_ref());
        let paused = session.as_ref().is_some_and(|s| s.mode == PlayMode::Paused);
        regia_editor::pie_net::sync_with_play(app.world_mut(), session.is_some());
        regia_editor::pie_net::step_frame(app.world_mut(), paused);
        regia_editor::abilities_panel::pump(app.world_mut(), session.as_mut());
        regia_editor::audio_mixer_panel::pump(app.world_mut(), session.as_mut());
        regia_editor::xr_simulator::pump(app.world_mut(), session.as_mut());
        regia_editor::tests_panel::pump(app.world_mut(), session.as_mut());
        regia_editor::state_tree_panel::pump(app.world_mut(), session.as_mut());
        regia_editor::bt_editor::pump(app.world_mut(), session.as_mut());
        regia_editor::smart_object_panel::pump(app.world_mut(), session.as_mut());
        regia_editor::camera_panel::pump(app.world_mut(), session.as_mut());
        app.update();
        // `quit.request` has no window to close here; the flag is taken like the binary does.
        if let Some(mut quit) = app
            .world_mut()
            .get_resource_mut::<regia_editor::quit::EditorQuit>()
        {
            let _ = quit.take();
        }

        self.time += 1.0 / 60.0;
        // The held modifiers follow the frame's key and button events, the way a window reports
        // them (egui keeps them until the next change).
        let mut events = events;
        let held = events.iter().rev().find_map(|e| match e {
            egui::Event::Key { modifiers, .. } | egui::Event::PointerButton { modifiers, .. } => {
                Some(*modifiers)
            }
            _ => None,
        });
        if let Some(held) = held {
            events.insert(0, egui::Event::ModifiersChanged(held));
        }
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, self.screen)),
            time: Some(self.time),
            predicted_dt: 1.0 / 60.0,
            events,
            focused: true,
            ..Default::default()
        };
        let Self {
            app,
            shell,
            ctx,
            session,
            ..
        } = self;
        let world = app.world_mut();
        let mut play = session.as_mut().map(|s| s.app.world_mut());
        regia_editor::asset_watch::pump_asset_watch(world, play.as_deref_mut());
        let mut output = ctx.run_ui(raw, |ui| shell.ui(ui, world, play.as_deref_mut()));
        shell.frame_effects(ctx, world);
        let _ = shell.take_close_decision();
        // No renderer takes the font atlas here.
        output.textures_delta.clear();
        self.labels.clear();
        let mut clashes = Vec::new();
        collect_shapes(&output.shapes, &mut self.labels, &mut clashes);
        self.id_clashes.extend(clashes);
        self.stats.frames += 1;
        for panel in BUILT_IN_PANELS {
            if self.shell.panel_area(*panel).is_some_and(|a| a.active) {
                self.stats.panels_drawn.insert(panel.title());
            }
        }
    }

    fn frames(&mut self, count: u32, events: Vec<egui::Event>) {
        let mut pending = events;
        for _ in 0..count {
            self.frame(std::mem::take(&mut pending));
        }
    }

    /// The focused panel's body, bringing it forward first when it is hidden.
    fn body(&mut self) -> Option<egui::Rect> {
        let area = self.shell.panel_area(self.focus);
        if area.is_none_or(|a| !a.active) {
            LayoutRequests::push(self.app.world_mut(), LayoutRequest::Open(self.focus));
            self.frames(1, Vec::new());
        }
        self.shell
            .panel_area(self.focus)
            .filter(|a| a.active && a.body.is_positive())
            .map(|a| a.body)
    }

    fn label_at(&self, pos: egui::Pos2) -> Option<&Label> {
        self.labels
            .iter()
            .rev()
            .find(|l| l.rect.expand(4.0).contains(pos))
    }

    /// A click target in `body`: half the time a painted label's center, else any point. `None`
    /// when the point is on a denied label.
    fn target(&mut self, body: egui::Rect) -> Option<egui::Pos2> {
        let pos = if self.rng.chance(0.5) {
            let inside: Vec<egui::Pos2> = self
                .labels
                .iter()
                .filter(|l| body.contains(l.rect.center()) && !denied(&l.text))
                .map(|l| l.rect.center())
                .collect();
            if inside.is_empty() {
                self.rng.point_in(body)
            } else {
                *self.rng.pick(&inside)
            }
        } else {
            self.rng.point_in(body)
        };
        match self.label_at(pos) {
            Some(label) if denied(&label.text) => {
                self.stats.denied += 1;
                None
            }
            _ => Some(pos),
        }
    }

    fn modifiers(&mut self) -> egui::Modifiers {
        match self.rng.below(10) {
            0 => egui::Modifiers::COMMAND,
            1 => egui::Modifiers::SHIFT,
            2 => egui::Modifiers::ALT,
            3 => egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            _ => egui::Modifiers::NONE,
        }
    }

    fn click(
        &mut self,
        pos: egui::Pos2,
        button: egui::PointerButton,
        double: bool,
        modifiers: egui::Modifiers,
    ) {
        self.stats.clicks += 1;
        self.pointer = pos;
        let presses = if double { 2 } else { 1 };
        self.frames(1, vec![egui::Event::PointerMoved(pos)]);
        for _ in 0..presses {
            self.frames(1, vec![button_event(pos, button, true, modifiers)]);
            self.frames(
                1,
                vec![
                    button_event(pos, button, false, modifiers),
                    egui::Event::ModifiersChanged(egui::Modifiers::NONE),
                ],
            );
        }
    }

    fn drag(
        &mut self,
        from: egui::Pos2,
        to: egui::Pos2,
        button: egui::PointerButton,
        modifiers: egui::Modifiers,
    ) {
        self.frames(1, vec![egui::Event::PointerMoved(from)]);
        self.frames(1, vec![button_event(from, button, true, modifiers)]);
        for i in 1..=4 {
            let t = i as f32 / 4.0;
            self.frames(1, vec![egui::Event::PointerMoved(from.lerp(to, t))]);
        }
        self.frames(
            1,
            vec![
                button_event(to, button, false, modifiers),
                egui::Event::ModifiersChanged(egui::Modifiers::NONE),
            ],
        );
        self.pointer = to;
    }

    fn key(&mut self, key: egui::Key, modifiers: egui::Modifiers) {
        // Enter or Space on a focused button presses it; the label rule cannot see which button
        // has focus, so those keys only go to text fields.
        if matches!(key, egui::Key::Enter | egui::Key::Space)
            && self.ctx.memory(|m| m.focused()).is_some()
            && !self.ctx.text_edit_focused()
        {
            self.note(format!("key {key:?} skipped: a non-text widget has focus"));
            return;
        }
        self.frames(1, vec![key_event(key, true, modifiers)]);
        self.frames(
            1,
            vec![
                key_event(key, false, modifiers),
                egui::Event::ModifiersChanged(egui::Modifiers::NONE),
            ],
        );
    }

    /// One random step. `undoable_only` keeps to input that may edit the scene only through the
    /// undo stack (no Play, no layout moves, no undo or redo keys).
    fn step(&mut self, undoable_only: bool) {
        let roll = self.rng.below(100);
        self.since_focus += 1;
        if !undoable_only && (roll < 4 || self.since_focus >= 6) {
            // A shuffled deck: every panel gets the focus before any panel gets it twice.
            if self.deck.is_empty() {
                self.deck = BUILT_IN_PANELS.to_vec();
                for i in (1..self.deck.len()).rev() {
                    let j = self.rng.below(i + 1);
                    self.deck.swap(i, j);
                }
            }
            let panel = self.deck.pop().unwrap_or(Panel::Viewport);
            self.since_focus = 0;
            self.focus = panel;
            self.note(format!("focus {}", panel.title()));
            LayoutRequests::push(self.app.world_mut(), LayoutRequest::Open(panel));
            self.frames(1, Vec::new());
            return;
        }
        if !undoable_only && roll < 10 {
            let size = if self.rng.chance(0.7) {
                *self.rng.pick(SIZES)
            } else {
                (100.0 + self.rng.unit() * 2400.0, 80.0 + self.rng.unit() * 1500.0)
            };
            self.screen = egui::vec2(size.0, size.1);
            self.note(format!("resize {}x{}", size.0, size.1));
            self.frames(2, Vec::new());
            return;
        }
        if !undoable_only && roll < 14 {
            self.dock_drag();
            return;
        }
        if !undoable_only && roll < 20 {
            if self.rng.chance(0.6) {
                self.note("undo (Ctrl+Z)".into());
                self.key(egui::Key::Z, egui::Modifiers::COMMAND);
            } else if self.rng.chance(0.5) {
                self.note("redo (Ctrl+Y)".into());
                self.key(egui::Key::Y, egui::Modifiers::COMMAND);
            } else {
                self.note("redo (Ctrl+Shift+Z)".into());
                self.key(egui::Key::Z, egui::Modifiers::COMMAND | egui::Modifiers::SHIFT);
            }
            return;
        }
        if !undoable_only && roll < 23 {
            self.play_or_stop();
            return;
        }
        let Some(body) = self.body() else {
            self.note(format!("{} has no body this frame", self.focus.title()));
            self.frames(1, Vec::new());
            return;
        };
        let kind = self.rng.below(100);
        if kind < 12 {
            let pos = self.rng.point_in(body);
            self.pointer = pos;
            self.note(format!("move {:.0},{:.0}", pos.x, pos.y));
            self.frames(1, vec![egui::Event::PointerMoved(pos)]);
        } else if kind < 50 {
            let Some(pos) = self.target(body) else {
                self.note("click skipped: denied label".into());
                return;
            };
            // Undo segments leave the stack to the check (no Undo/Redo buttons either).
            if undoable_only && self.label_at(pos).is_some_and(|l| is_undo_label(&l.text)) {
                self.note("click skipped: undo/redo button in an undo segment".into());
                return;
            }
            let button = if self.rng.chance(0.8) {
                egui::PointerButton::Primary
            } else {
                egui::PointerButton::Secondary
            };
            let double = self.rng.chance(0.15);
            let modifiers = if self.rng.chance(0.8) {
                egui::Modifiers::NONE
            } else {
                self.modifiers()
            };
            let label = self.label_at(pos).map(|l| l.text.clone()).unwrap_or_default();
            self.note(format!(
                "{}click {button:?} {:.0},{:.0} {modifiers:?} on {label:?} in {}",
                if double { "double " } else { "" },
                pos.x,
                pos.y,
                self.focus.title()
            ));
            self.click(pos, button, double, modifiers);
        } else if kind < 62 {
            let Some(from) = self.target(body) else {
                self.note("drag skipped: denied label".into());
                return;
            };
            if undoable_only && self.label_at(from).is_some_and(|l| is_undo_label(&l.text)) {
                return;
            }
            let to = if self.rng.chance(0.8) {
                self.rng.point_in(body)
            } else {
                self.rng
                    .point_in(egui::Rect::from_min_size(egui::Pos2::ZERO, self.screen))
            };
            let button = match self.rng.below(6) {
                0 => egui::PointerButton::Secondary,
                1 => egui::PointerButton::Middle,
                _ => egui::PointerButton::Primary,
            };
            let modifiers = self.modifiers();
            self.note(format!(
                "drag {button:?} {:.0},{:.0} -> {:.0},{:.0} {modifiers:?} in {}",
                from.x,
                from.y,
                to.x,
                to.y,
                self.focus.title()
            ));
            self.drag(from, to, button, modifiers);
        } else if kind < 80 {
            let key = *self.rng.pick(KEYS);
            let modifiers = if self.rng.chance(0.7) {
                egui::Modifiers::NONE
            } else {
                self.modifiers()
            };
            // Undo segments leave the stack to the check.
            let undo_key = modifiers.command && matches!(key, egui::Key::Z | egui::Key::Y);
            if undoable_only && undo_key {
                return;
            }
            // The hover decides which panel a shortcut reaches.
            if !body.contains(self.pointer) {
                self.pointer = self.rng.point_in(body);
                self.frames(1, vec![egui::Event::PointerMoved(self.pointer)]);
            }
            self.note(format!("key {key:?} {modifiers:?} in {}", self.focus.title()));
            self.key(key, modifiers);
        } else if kind < 90 {
            let text = (*self.rng.pick(TEXTS)).to_owned();
            self.note(format!("text {text:?} in {}", self.focus.title()));
            self.frames(1, vec![egui::Event::Text(text)]);
        } else {
            let pos = self.rng.point_in(body);
            let delta = egui::vec2((self.rng.unit() - 0.5) * 400.0, (self.rng.unit() - 0.5) * 1200.0);
            let modifiers = if self.rng.chance(0.25) {
                egui::Modifiers::COMMAND
            } else {
                egui::Modifiers::NONE
            };
            self.pointer = pos;
            self.note(format!(
                "scroll {:.0},{:.0} at {:.0},{:.0} {modifiers:?} in {}",
                delta.x,
                delta.y,
                pos.x,
                pos.y,
                self.focus.title()
            ));
            self.frames(
                1,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta,
                        phase: egui::TouchPhase::Move,
                        modifiers,
                    },
                ],
            );
        }
    }

    /// Drags the focused panel's tab bar somewhere: onto another group (dock) or into empty space
    /// (undock into a window), the way a user rearranges the layout.
    fn dock_drag(&mut self) {
        let Some(area) = self.shell.panel_area(self.focus) else {
            self.note(format!("dock: {} not in the layout", self.focus.title()));
            self.frames(1, Vec::new());
            return;
        };
        let bar = egui::Rect::from_min_max(area.group.min, egui::pos2(area.group.max.x, area.body.min.y));
        if bar.height() < 2.0 || bar.width() < 2.0 {
            self.note(format!("dock: {} has no tab bar", self.focus.title()));
            return;
        }
        let from = self.rng.point_in(bar);
        let to = self
            .rng
            .point_in(egui::Rect::from_min_size(egui::Pos2::ZERO, self.screen));
        if self.label_at(from).is_some_and(|l| denied(&l.text)) {
            self.stats.denied += 1;
            return;
        }
        self.note(format!(
            "dock drag {} tab bar {:.0},{:.0} -> {:.0},{:.0}",
            self.focus.title(),
            from.x,
            from.y,
            to.x,
            to.y
        ));
        self.drag(from, to, egui::PointerButton::Primary, egui::Modifiers::NONE);
    }

    /// Clicks the toolbar's Play or Stop button (queues the request when the button is not on
    /// screen, e.g. in a tiny window).
    fn play_or_stop(&mut self) {
        let playing = self.session.is_some();
        let wanted = if playing { "⏹ Stop" } else { "▶ Play" };
        let button = self
            .labels
            .iter()
            .find(|l| l.text.trim() == wanted)
            .map(|l| l.rect.center());
        match button {
            Some(pos) => {
                self.note(format!("click {wanted:?} at {:.0},{:.0}", pos.x, pos.y));
                self.click(pos, egui::PointerButton::Primary, false, egui::Modifiers::NONE);
            }
            None => {
                self.note(format!("{wanted:?} not on screen: queued"));
                self.app
                    .world_mut()
                    .resource_mut::<regia_editor::pie::PieRequests>()
                    .push(if playing {
                        PieRequest::Stop
                    } else {
                        PieRequest::Play
                    });
            }
        }
        self.frames(2, Vec::new());
        if !playing && self.session.is_some() {
            self.stats.plays += 1;
        }
    }

    /// The per-step checks.
    fn check(&mut self) -> Result<(), String> {
        let mut bad = bad_transforms(self.app.world_mut(), "edit");
        if let Some(session) = self.session.as_mut() {
            bad.extend(bad_transforms(session.app.world_mut(), "play"));
        }
        if !bad.is_empty() {
            return Err(format!("non-finite transform: {}", bad.join("; ")));
        }
        let stack = self.app.world().resource::<UndoStack>();
        if stack.undo_depth() > UNDO_CAP || stack.redo_depth() > UNDO_CAP {
            return Err(format!(
                "undo stack over its cap {UNDO_CAP}: undo {} redo {}",
                stack.undo_depth(),
                stack.redo_depth()
            ));
        }
        if !self.id_clashes.is_empty() {
            let mut clashes = std::mem::take(&mut self.id_clashes);
            clashes.sort();
            clashes.dedup();
            return Err(format!("egui ID clash: {}", clashes.join("; ")));
        }
        Ok(())
    }

    /// Stops play (so the edit world is the one on screen) and lets go of pointer and focus.
    fn settle(&mut self) {
        if self.session.is_some() {
            self.app
                .world_mut()
                .resource_mut::<regia_editor::pie::PieRequests>()
                .push(PieRequest::Stop);
        }
        self.frames(1, vec![key_event(egui::Key::Escape, true, egui::Modifiers::NONE)]);
        self.frames(
            2,
            vec![
                key_event(egui::Key::Escape, false, egui::Modifiers::NONE),
                egui::Event::PointerGone,
            ],
        );
    }

    /// Lets go of the pointer (no drag carries into what follows).
    fn settle_pointer(&mut self) {
        self.frames(1, vec![egui::Event::PointerGone]);
    }

    /// An undo segment: input to the undoable panels only, then undo back to the start; the scene
    /// must hash as it did at the start.
    fn undo_segment(&mut self, step: u32) -> Result<(), String> {
        self.settle();
        let depth = self.app.world().resource::<UndoStack>().undo_depth();
        if depth + 200 > UNDO_CAP {
            self.stats.segments_skipped += 1;
            return Ok(());
        }
        // A window big enough to edit in (the user resizes before working).
        if self.screen.x < 800.0 || self.screen.y < 600.0 {
            self.screen = egui::vec2(1280.0, 800.0);
            self.note("resize 1280x800 for the undo segment".into());
            self.frames(2, Vec::new());
        }
        // Select something first, the way a user starts editing: click an entity's row in the
        // Outliner (the Inspector and the gizmo then have a target).
        self.focus = Panel::Outliner;
        if let Some(body) = self.body() {
            let rows: Vec<egui::Pos2> = self
                .labels
                .iter()
                .filter(|l| body.contains(l.rect.center()) && SCENE_NAMES.iter().any(|n| l.text.contains(n)))
                .map(|l| l.rect.center())
                .collect();
            if !rows.is_empty() {
                let pos = *self.rng.pick(&rows);
                self.note(format!("select row at {:.0},{:.0} in Outliner", pos.x, pos.y));
                self.click(pos, egui::PointerButton::Primary, false, egui::Modifiers::NONE);
            }
        }
        self.settle_pointer();
        let depth = self.app.world().resource::<UndoStack>().undo_depth();
        self.app.world_mut().resource_mut::<UndoStack>().seal();
        let before = scene_text(self.app.world_mut());
        self.note(format!("undo segment at step {step}: depth {depth}"));
        for _ in 0..SEGMENT_STEPS {
            self.focus = *self.rng.pick(UNDOABLE);
            self.step(true);
            self.check()?;
        }
        self.settle();
        let mut undone = 0;
        while self.app.world().resource::<UndoStack>().undo_depth() > depth {
            if !regia_editor::command::undo_world(self.app.world_mut()) {
                break;
            }
            undone += 1;
            self.frames(1, Vec::new());
        }
        self.stats.segments += 1;
        self.stats.undone += undone;
        let after = scene_text(self.app.world_mut());
        if hash_text(&before) != hash_text(&after) {
            return Err(format!(
                "undo-all after an undo segment did not restore the scene ({undone} undos, depth {depth}): {}",
                first_difference(&before, &after)
            ));
        }
        Ok(())
    }

    /// A fixture op with a planted fault ([`Plant`]): panics, or writes a NaN translation into
    /// the first scene entity's `Transform`.
    fn planted_fault(&mut self, kind: PlantKind, step: u32) {
        self.note(format!("planted fault {kind:?}"));
        match kind {
            PlantKind::Panic => panic!("planted panic in a fixture op at step {step}"),
            PlantKind::NonFinite => {
                let world = self.app.world_mut();
                let mut query = world.query::<(Entity, &Name)>();
                let target = query
                    .iter(world)
                    .find(|(_, name)| SCENE_NAMES.contains(&name.as_str()))
                    .map(|(entity, _)| entity);
                if let Some(mut transform) = target.and_then(|entity| world.get_mut::<Transform>(entity)) {
                    transform.translation.x = f32::NAN;
                }
            }
        }
    }

    fn cleanup(self) {
        if let Some(session) = self.session {
            regia_editor::pie::stop_play(session);
        }
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.config);
    }
}

/// What a planted fault's report must say for the run to count as caught.
fn planted_message(kind: PlantKind) -> &'static str {
    match kind {
        PlantKind::Panic => "panic: planted panic",
        PlantKind::NonFinite => "non-finite transform",
    }
}

/// Runs one seed on its own thread with a deep stack (debug egui frames are deep).
fn run_seed_deep(seed: u64, steps: u32, plant: Option<Plant>) -> Result<String, String> {
    std::thread::Builder::new()
        .name(format!("monkey-{seed:#x}"))
        .stack_size(64 << 20)
        .spawn(move || run_seed(seed, steps, plant))
        .map_err(|e| format!("cannot spawn: {e}"))
        .and_then(|handle| {
            handle
                .join()
                .unwrap_or_else(|p| Err(format!("driver panicked: {}", panic_text(&*p))))
        })
}

/// Whether a planted run's outcome is the planted fault, caught: the run failed at the planted
/// step with the planted message.
fn planted_caught(plant: Plant, outcome: &Result<String, String>) -> bool {
    outcome.as_ref().err().is_some_and(|report| {
        report.contains(&format!("FAILED at step {} ", plant.step))
            && report.contains(planted_message(plant.kind))
    })
}

/// The gate's proof that the checks bite: a short run with a panic planted in a fixture op and
/// one with a non-finite transform planted in the scene must each fail at the planted step.
fn plant_checks() -> Result<(), String> {
    for kind in [PlantKind::Panic, PlantKind::NonFinite] {
        let plant = Plant { kind, step: 1 };
        println!("monkey: planted-fault check {kind:?} at step 1 (expected to be caught)");
        let outcome = run_seed_deep(GATE_SEEDS[0], 3, Some(plant));
        if !planted_caught(plant, &outcome) {
            return Err(format!(
                "monkey: the planted {kind:?} fault was not caught: {}",
                match &outcome {
                    Ok(stats) => format!("the run passed ({stats})"),
                    Err(report) => report.clone(),
                }
            ));
        }
    }
    Ok(())
}

/// Runs one seed; the error is the full failure report. `plant` breaks one step on purpose.
fn run_seed(seed: u64, steps: u32, plant: Option<Plant>) -> Result<String, String> {
    let replay = format!(
        "replay: cargo test -p regia_editor --test monkey -- --seed {seed:#x} --steps {steps}   (or REGIA_MONKEY_SEED={seed:#x} REGIA_MONKEY_STEPS={steps})"
    );
    let mut monkey = match std::panic::catch_unwind(AssertUnwindSafe(|| Monkey::new(seed))) {
        Ok(monkey) => monkey,
        Err(p) => {
            return Err(format!(
                "monkey: seed {seed:#x}: setup panicked: {}\n{replay}",
                panic_text(&*p)
            ));
        }
    };
    // Every built-in panel drew during the opening sweep.
    let missing: Vec<&str> = BUILT_IN_PANELS
        .iter()
        .map(|p| p.title())
        .filter(|t| !monkey.stats.panels_drawn.contains(t))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "monkey: seed {seed:#x}: panels that never drew: {}\n{replay}",
            missing.join(", ")
        ));
    }
    for step in 0..steps {
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            if let Some(plant) = plant.filter(|plant| plant.step == step) {
                monkey.planted_fault(plant.kind, step);
                return monkey.check();
            }
            if step % SEGMENT_EVERY == SEGMENT_EVERY / 2 {
                monkey.undo_segment(step)?;
            }
            monkey.step(false);
            monkey.check()
        }));
        let failure = match outcome {
            Ok(Ok(())) => continue,
            Ok(Err(message)) => message,
            Err(p) => format!("panic: {}", panic_text(&*p)),
        };
        let history: Vec<String> = monkey.history.iter().cloned().collect();
        return Err(format!(
            "monkey: seed {seed:#x} FAILED at step {step} of {steps}: {failure}\n  last actions:\n    {}\n  project: {}\n{replay}",
            history.join("\n    "),
            monkey.root.display()
        ));
    }
    let stats = monkey.stats.to_string();
    monkey.cleanup();
    Ok(stats)
}
