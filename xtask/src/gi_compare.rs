// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask gi-compare`: the standard baked-light review kit (ADR-314,
//! `book/agents/gi-compare.md`).
//!
//! ```text
//! cargo xtask gi-compare --scene assets/fixtures/gi_compare/cornell.gi_compare.ron \
//!     [--views cornell.views.ron] [--out target/gi-compare/cornell] [--spp 1024] \
//!     [--volume map.fgiv] [--gpu-lock /tmp/regia-gpu-2.lock] [--baseline old/summary.json]
//! cargo xtask gi-compare --read target/gi-compare/cornell/summary.json [--baseline old/summary.json]
//! ```
//!
//! For every view of the views file, at one fixed size, it renders in-process:
//!
//! 1. the raster frame lit by the baked volume (the look `render.set_gi_mode {"mode": "baked"}`
//!    selects), with the fixture's capsule occluders;
//! 2. the same frame with live GI (DDGI, the `depth` look), for context;
//! 3. the raster frame's direct light alone (no GI, no flat ambient), to isolate the raster's
//!    indirect light as `baked − direct`;
//! 4. the `regia_pt` reference at `--spp` of the same extracted scene, full and indirect
//!    (the tracer's indirect AOVs, plus the emitters' first bounce when the scene has emitters:
//!    see [`metrics::reference_indirect`]).
//!
//! It writes one contact sheet per view (`<view>.png`: baked, DDGI, reference and the FLIP error
//! of baked against the reference, full on the first row and indirect on the second, each row at
//! one exposure), the tiles as PNG and the linear images as EXR, the baked volume as
//! `<fixture>.fgiv` (reuse it with `--volume`), and `summary.json` ([`summary::Summary`]).
//! `--read` prints the table and the flags of a summary; its last line always sends the reader
//! to the plates. Exit code: 0 with no flag, 1 with a flag, 2 for a refused input.
//!
//! For the look review pack (ADR-1326) it also writes the direct-only frame
//! (`<view>_direct.png`) and the amplified difference `|baked − reference| · 4`
//! (`<view>_diff4x.png`) as a third sheet row; `--size` overrides the views file's size; and a
//! views file with a `flythrough` path gets `--flythrough-frames` baked frames along it
//! (`flythrough_NN.png`) and their strip (`flythrough.png`).

pub mod fixture;
pub mod gpu;
mod input;
pub mod metrics;
pub mod sheet;
pub mod summary;
pub mod views;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use regia_pt::exr::{self, ExrChannel};
use regia_pt::output::{preview_rgba8, write_png};
use regia_render::BakedGiSettings;
use regia_render::compare::{DEFAULT_PPD, compare_linear};

use fixture::{FrameParams, parse_fixture};
use summary::{
    DEFAULT_FLIP_TOLERANCE, Leak, SUMMARY_SCHEMA, Scores, Summary, ViewSummary, parse_summary, render_table,
    verdict,
};
use views::parse_views;

/// Command-line options of `cargo xtask gi-compare`.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// The scene: a `*.gi_compare.ron` fixture (`assets/fixtures/gi_compare/`). A level
    /// (`*.level.ron`) is refused for now (`scene_level_unsupported`, ADR-314).
    #[arg(long)]
    pub scene: Option<PathBuf>,
    /// The views file. Default: `<fixture stem>.views.ron` next to the fixture
    /// (`cornell.gi_compare.ron` → `cornell.views.ron`).
    #[arg(long)]
    pub views: Option<PathBuf>,
    /// Output directory (default `target/gi-compare/<fixture name>`). Files with the names this
    /// command writes are replaced.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Reference samples per pixel.
    #[arg(long, default_value_t = 256)]
    pub spp: u32,
    /// Use this `.fgiv` instead of baking the fixture's layout.
    #[arg(long)]
    pub volume: Option<PathBuf>,
    /// Hold an exclusive lock on this file while the GPU work runs (waits for it first). A card
    /// lock `regia-gpu-<n>.lock` also takes its pre-rename twin `ferrite-gpu-<n>.lock` in the
    /// same directory, second (ADR-8670). Do not also wrap the command in `flock` on the same
    /// files: the locks would wait on each other.
    #[arg(long)]
    pub gpu_lock: Option<PathBuf>,
    /// Reconstruction of the baked lookup: `l1` (the lookup's default), `zh3` (ADR-1377),
    /// `windowed` or `sweep` (all three on one sheet). `windowed` (and so `sweep`) is refused with
    /// `reconstruction_unavailable` until the baked lookup offers it.
    #[arg(long, default_value = "l1")]
    pub reconstruction: String,
    /// Frames the DDGI context frame renders so its probes settle.
    #[arg(long, default_value_t = 60)]
    pub ddgi_frames: u32,
    /// Print the table and flags of an existing `summary.json` and exit (no GPU).
    #[arg(long, value_name = "SUMMARY_JSON")]
    pub read: Option<PathBuf>,
    /// A previous `summary.json`: a mean FLIP that rose by more than `--flip-tolerance` on a view
    /// of the same name is flagged.
    #[arg(long, value_name = "SUMMARY_JSON")]
    pub baseline: Option<PathBuf>,
    /// FLIP increase over the baseline that counts as a regression.
    #[arg(long, default_value_t = DEFAULT_FLIP_TOLERANCE)]
    pub flip_tolerance: f32,
    /// Output size `WIDTHxHEIGHT` (e.g. `1920x1080`) instead of the views file's. The field of
    /// view is vertical, so a wider frame shows more to the sides.
    #[arg(long, value_name = "WxH")]
    pub size: Option<String>,
    /// Baked frames along the views file's `flythrough` path (0 skips it; a file without a path
    /// renders none).
    #[arg(long, default_value_t = 6)]
    pub flythrough_frames: u32,
    /// Scale of the bake's emissive component in the baked frame (`BakedGiSettings::emissive_scale`,
    /// default 1). A diagnostic: 0 shows the frame without the bake's emitter light, e.g. to see
    /// how much of it the emissive area lights already carry.
    #[arg(long, default_value_t = 1.0)]
    pub emissive_scale: f32,
    /// Emissive area light budget of the raster frames instead of Desktop High's 96
    /// (`RenderProfile::emissive_lights`). A diagnostic: 0 renders the emitters the way the tiers
    /// without area lights do, their light carried by the bake alone (ADR-1364).
    #[arg(long, value_name = "N")]
    pub emissive_lights: Option<u32>,
}

/// Gain of the amplified difference tile, `|baked − reference| · DIFF_GAIN`.
pub const DIFF_GAIN: f32 = 4.0;

/// Parses `--size WIDTHxHEIGHT`.
///
/// # Errors
///
/// `views_size` when the text is not two whole numbers joined by `x`. The range check is the
/// views file's (`views::validate`).
pub fn parse_size(text: &str) -> Result<(u32, u32)> {
    let bad = || anyhow!("[views_size] --size {text:?}: use WIDTHxHEIGHT, e.g. 1920x1080");
    let (w, h) = text.split_once(['x', 'X']).ok_or_else(bad)?;
    let w: u32 = w.trim().parse().map_err(|_| bad())?;
    let h: u32 = h.trim().parse().map_err(|_| bad())?;
    Ok((w, h))
}

/// Reconstructions of the baked lookup the sweep tiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reconstruction {
    /// L1 spherical harmonics, as baked (ADR-206).
    L1,
    /// L1 with a ringing window.
    Windowed,
    /// Zonal-harmonic hallucination to a quadratic lobe.
    Zh3,
}

impl Reconstruction {
    /// Name on the sheet and in the summary.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::L1 => "l1",
            Self::Windowed => "windowed",
            Self::Zh3 => "zh3",
        }
    }

    /// The renderer settings that select this reconstruction: L1 is the default settings
    /// (de-ringed, as the default `deringing`), ZH3 the same with `BakedGiSettings::zh3` on
    /// (ADR-1377). The baked
    /// lookup has no windowed L1, and the flag is refused before any GPU work.
    ///
    /// # Errors
    ///
    /// `reconstruction_unavailable` for `windowed`.
    pub fn settings(self) -> Result<BakedGiSettings> {
        match self {
            Self::L1 => Ok(BakedGiSettings::default()),
            Self::Zh3 => Ok(BakedGiSettings {
                zh3: true,
                ..BakedGiSettings::default()
            }),
            Self::Windowed => Err(anyhow!(
                "[reconstruction_unavailable] the baked lookup in this build has no windowed L1; use \
                 l1 or zh3"
            )),
        }
    }
}

/// Parses `--reconstruction`: one name, or `sweep` for all three.
///
/// # Errors
///
/// `reconstruction_unknown` for another name.
pub fn parse_reconstruction(text: &str) -> Result<Vec<Reconstruction>> {
    match text {
        "l1" => Ok(vec![Reconstruction::L1]),
        "windowed" => Ok(vec![Reconstruction::Windowed]),
        "zh3" => Ok(vec![Reconstruction::Zh3]),
        "sweep" => Ok(vec![
            Reconstruction::L1,
            Reconstruction::Windowed,
            Reconstruction::Zh3,
        ]),
        other => Err(anyhow!(
            "[reconstruction_unknown] unknown reconstruction {other:?}: use l1, windowed, zh3 or sweep"
        )),
    }
}

/// `views.ron` next to a fixture: `x.gi_compare.ron` → `x.views.ron`.
#[must_use]
pub fn default_views_path(fixture: &Path) -> PathBuf {
    let name = fixture.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let stem = name
        .strip_suffix(".gi_compare.ron")
        .or_else(|| name.strip_suffix(".ron"))
        .unwrap_or(name);
    fixture.with_file_name(format!("{stem}.views.ron"))
}

/// `path` as given, or under the workspace `root` when it is relative and exists only there (the
/// command may run from another directory than the repository's).
#[must_use]
pub fn resolve(root: &Path, path: &Path) -> PathBuf {
    if path.is_relative() && !path.exists() && root.join(path).exists() {
        root.join(path)
    } else {
        path.to_path_buf()
    }
}

fn read_text(path: &Path, code: &str) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("[{code}] reading {}", path.display()))
}

fn load_summary(path: &Path) -> Result<Summary> {
    let text = read_text(path, "summary_unreadable")?;
    parse_summary(&text).map_err(|e| anyhow!("[{}] {}: {e}", e.code(), path.display()))
}

/// The files `--gpu-lock <path>` locks, in order: `path`, and for a card lock in the current
/// spelling (`regia-gpu-<n>.lock`) its pre-rename twin (`crate::gpu_bench::legacy_lock_path`'s
/// name) in the same directory, second (ADR-8670).
fn gpu_lock_files(path: &Path) -> Vec<PathBuf> {
    let mut files = vec![path.to_path_buf()];
    let card = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("regia-gpu-"))
        .and_then(|n| n.strip_suffix(".lock"));
    if let Some(card) = card.filter(|c| !c.is_empty()) {
        let legacy = format!("{}-gpu-{card}.lock", regia_core::brand::LEGACY_KEY_PREFIX);
        files.push(path.with_file_name(legacy));
    }
    files
}

/// Holds the `--gpu-lock` file locked until dropped.
fn acquire_gpu_lock(path: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("[gpu_lock] opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            println!("gi-compare: waiting for {}", path.display());
            file.lock()
                .with_context(|| format!("[gpu_lock] locking {}", path.display()))?;
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(anyhow!("[gpu_lock] locking {}: {e}", path.display()));
        }
    }
    Ok(file)
}

/// Runs the review, or with `--read` prints a summary's table.
///
/// # Errors
///
/// Only for failures outside the review itself (none today); refused inputs and GPU failures
/// print `gi-compare: [code] message` and exit 2.
pub fn run(args: &Args, root: &Path) -> Result<ExitCode> {
    match run_inner(args, root) {
        Ok(code) => Ok(code),
        Err(e) => {
            eprintln!("gi-compare: {e:#}");
            Ok(ExitCode::from(2))
        }
    }
}

fn exit_for(flags: usize) -> ExitCode {
    if flags == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn run_inner(args: &Args, root: &Path) -> Result<ExitCode> {
    let baseline = args
        .baseline
        .as_deref()
        .map(|p| load_summary(&resolve(root, p)))
        .transpose()?;
    if let Some(path) = &args.read {
        let s = load_summary(&resolve(root, path))?;
        let v = verdict(&s, baseline.as_ref(), args.flip_tolerance);
        print!("{}", render_table(&s, &v));
        return Ok(exit_for(v.flags.len()));
    }
    let scene_path = resolve(
        root,
        args.scene.as_deref().ok_or_else(|| {
            anyhow!("[scene_missing] --scene <fixture.gi_compare.ron> is required (or --read)")
        })?,
    );
    let scene_name = scene_path.to_string_lossy();
    if scene_name.ends_with(".level.ron") || scene_name.ends_with(".scene.ron") {
        return Err(anyhow!(
            "[scene_level_unsupported] {scene_name}: levels need the agent's linear HDR capture to \
             isolate indirect light; this build compares fixtures only (ADR-314)"
        ));
    }
    if args.spp == 0 || args.spp > 65_536 {
        return Err(anyhow!("[spp_range] --spp must be 1..=65536"));
    }
    let reconstructions = parse_reconstruction(&args.reconstruction)?;
    if !(args.emissive_scale.is_finite() && args.emissive_scale >= 0.0) {
        return Err(anyhow!(
            "[emissive_scale_range] --emissive-scale must be a finite number >= 0"
        ));
    }
    gpu::set_emissive_lights(args.emissive_lights);
    let recon_settings = reconstructions
        .iter()
        .map(|r| {
            r.settings().map(|mut s| {
                s.emissive_scale = args.emissive_scale;
                s
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let fx = parse_fixture(&read_text(&scene_path, "fixture_unreadable")?)
        .map_err(|e| anyhow!("[{}] {}: {e}", e.code(), scene_path.display()))?;
    let views_path = args
        .views
        .as_deref()
        .map_or_else(|| default_views_path(&scene_path), |p| resolve(root, p));
    let mut set = parse_views(&read_text(&views_path, "views_unreadable")?)
        .map_err(|e| anyhow!("[{}] {}: {e}", e.code(), views_path.display()))?;
    if let Some(size) = &args.size {
        set.size = parse_size(size)?;
        views::validate(&set).map_err(|e| anyhow!("[{}] --size: {e}", e.code()))?;
    }
    let out = match &args.out {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => root.join(p),
        None => root.join("target/gi-compare").join(&fx.name),
    };
    std::fs::create_dir_all(&out).with_context(|| format!("[out_unwritable] creating {}", out.display()))?;

    let _locks = match args.gpu_lock.as_deref() {
        Some(path) => gpu_lock_files(path)
            .iter()
            .map(|p| acquire_gpu_lock(p))
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };
    let started = Instant::now();
    let gpu = gpu::Gpu::new()?;
    println!(
        "gi-compare: {} on {}, {} view(s) -> {}",
        fx.name,
        gpu.adapter(),
        set.views.len(),
        out.display()
    );

    let first = FrameParams {
        view: &set.views[0],
        size: set.size,
        ev100: set.ev100,
    };
    let (volume, bake) = match &args.volume {
        Some(path) => gpu::load_volume(&resolve(root, path))?,
        None => {
            let scene = gpu::traced_scene(&fx.lab_scene(&first, false, true));
            let baked = gpu::bake(&gpu, &fx, &scene)?;
            let bytes = baked
                .0
                .to_bytes()
                .map_err(|e| anyhow!("[out_unwritable] encoding the volume: {e}"))?;
            write_atomic(&out.join(format!("{}.fgiv", fx.name)), &bytes)?;
            baked
        }
    };
    let mut summary = Summary {
        schema: SUMMARY_SCHEMA,
        fixture: fx.name.clone(),
        fixture_path: scene_path.display().to_string(),
        views_path: views_path.display().to_string(),
        size: set.size,
        spp: args.spp,
        ev100: set.ev100,
        indirect_ev100: set.indirect_ev100(),
        reconstruction: args.reconstruction.clone(),
        device: gpu.adapter(),
        bake,
        views: Vec::new(),
        total_seconds: 0.0,
    };
    let occluders = fx.occluders();
    for view in &set.views {
        println!("gi-compare: view {}", view.name);
        let ctx = ViewRun {
            gpu: &gpu,
            fx: &fx,
            set: &set,
            view,
            volume: &volume,
            occluders: &occluders,
            reconstructions: &reconstructions,
            recon_settings: &recon_settings,
            spp: args.spp,
            ddgi_frames: args.ddgi_frames,
            out: &out,
        };
        summary.views.push(run_view(&ctx)?);
        summary.total_seconds = started.elapsed().as_secs_f64();
        write_summary(&out, &summary)?;
    }
    let fly = set.flythrough_views(args.flythrough_frames);
    if !fly.is_empty() {
        println!("gi-compare: fly-through, {} baked frame(s)", fly.len());
        let settings = recon_settings.first().copied().unwrap_or_default();
        let (w, h) = set.size;
        let mut frames = Vec::with_capacity(fly.len());
        for (i, view) in fly.iter().enumerate() {
            let ev = view.ev100.unwrap_or(set.ev100);
            let frame = FrameParams {
                view,
                size: set.size,
                ev100: ev,
            };
            let lab = fx.lab_scene(&frame, true, true);
            let (cap, _) = gpu::baked_frame(&gpu, &lab, &volume, settings, &occluders)?;
            let rgba = preview_rgba8(&cap.rgb, ev);
            let file = format!("flythrough_{i:02}.png");
            write_png(&out.join(&file), w, h, &rgba).map_err(|e| anyhow!("[out_unwritable] {e}"))?;
            frames.push((format!("{} ev {ev:.1}", view.name), rgba));
        }
        let tiles: Vec<sheet::Tile<'_>> = frames
            .iter()
            .map(|(label, rgba)| sheet::Tile {
                label: label.as_str(),
                rgba: rgba.as_slice(),
                size: (w, h),
            })
            .collect();
        let header = vec![format!(
            "{} / fly-through | baked | {} frames | {}x{}",
            fx.name,
            fly.len(),
            w,
            h
        )];
        let strip = sheet::compose(&header, &tiles, 6, 480)
            .ok_or_else(|| anyhow!("[out_unwritable] empty fly-through strip"))?;
        strip
            .save(out.join("flythrough.png"))
            .context("[out_unwritable] writing flythrough.png")?;
    }
    let v = verdict(&summary, baseline.as_ref(), args.flip_tolerance);
    print!("{}", render_table(&summary, &v));
    Ok(exit_for(v.flags.len()))
}

struct ViewRun<'a> {
    gpu: &'a gpu::Gpu,
    fx: &'a fixture::Fixture,
    set: &'a views::ViewSet,
    view: &'a views::View,
    volume: &'a std::sync::Arc<regia_asset::baked_gi::BakedGiVolume>,
    occluders: &'a [regia_render::baked_gi::GiCapsule],
    reconstructions: &'a [Reconstruction],
    recon_settings: &'a [BakedGiSettings],
    spp: u32,
    ddgi_frames: u32,
    out: &'a Path,
}

fn run_view(c: &ViewRun<'_>) -> Result<ViewSummary> {
    let (w, h) = c.set.size;
    let (ev, iev) = c.set.exposures(c.view);
    let frame = FrameParams {
        view: c.view,
        size: c.set.size,
        ev100: ev,
    };
    let full_lab = c.fx.lab_scene(&frame, true, true);
    let direct_lab = c.fx.lab_scene(&frame, true, false);

    let mut baked = Vec::with_capacity(c.recon_settings.len());
    let mut gi_status = None;
    for settings in c.recon_settings {
        let (cap, status) = gpu::baked_frame(c.gpu, &full_lab, c.volume, *settings, c.occluders)?;
        gi_status.get_or_insert(status);
        baked.push(cap);
    }
    let gi = gi_status.unwrap_or_default();
    let ddgi = gpu::ddgi_frame(c.gpu, &full_lab, c.ddgi_frames)?;
    let direct = gpu::direct_frame(c.gpu, &direct_lab)?;
    let traced = gpu::traced_scene(&full_lab);
    let (reference, reference_ms) = gpu::trace(c.gpu, &traced, c.spp, None, "reference")?;
    // With emissive area lights on (Desktop High: `emissive_lights` > 0) the raster lights the
    // emitters' surroundings directly, so the direct-only frame already holds the emitters' first
    // bounce and the reference's indirect light is its indirect AOVs alone (ADR-1326). With them
    // off, only the bake carries that light and it counts as indirect (ADR-314).
    let emitter_direct = if c.fx.has_emitters() && gpu::quality().emissive_lights == 0 {
        let spp = (c.spp / 4).max(16);
        Some(gpu::trace(
            c.gpu,
            &gpu::without_emitters(&traced),
            spp,
            Some(1),
            "direct without emitters",
        )?)
    } else {
        None
    };

    let compare_started = Instant::now();
    let depth = &reference.depth;
    let ref_full = &reference.beauty;
    let prepared = input::prepare(
        c.set.size,
        &baked,
        &ddgi,
        &direct,
        &reference,
        emitter_direct.as_ref().map(|(img, _)| img),
    )?;
    let input::Prepared {
        ref_ind,
        baked_ind,
        ddgi_ind,
    } = prepared;
    let main = baked
        .first()
        .ok_or_else(|| anyhow!("[render_failed] no baked frame"))?;
    let (full_report, full_flip) = score(ref_full, &main.rgb, depth, c.set.size, ev)?;
    let (ind_report, ind_flip) = score(&ref_ind, &baked_ind, depth, c.set.size, iev)?;
    let (ddgi_report, _) = score(ref_full, &ddgi.rgb, depth, c.set.size, ev)?;
    let reconstructions: Vec<(String, Scores)> = if baked.len() > 1 {
        c.reconstructions
            .iter()
            .zip(&baked)
            .map(|(r, cap)| {
                let report = score(ref_full, &cap.rgb, depth, c.set.size, ev)?.0;
                Ok((r.name().to_string(), Scores::from_report(&report)))
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let leak = if let Some(interior) = c.view.interior.as_ref() {
        let camera = metrics::PixelCamera {
            eye: c.view.eye,
            target: c.view.target,
            fov_y: c.view.fov_y_deg.to_radians(),
            width: w,
            height: h,
        };
        let mask = metrics::interior_mask(&camera, depth, interior);
        input::checked_leak(c.set.size, &baked_ind, &ref_ind, ref_full, depth, &mask)?.map(|l| Leak {
            pixels: l.pixels,
            baked_mean: l.baked_mean,
            reference_mean: l.reference_mean,
            floor: l.floor,
            leak: l.leak,
            threshold: c.view.leak_threshold(),
        })
    } else {
        None
    };

    // Tiles: PNG at the row's exposure, EXR of every linear plane.
    let name = &c.view.name;
    let mut files = Vec::new();
    let mut tile = |suffix: &str, label: String, rgba: Vec<u8>| -> Result<(String, Vec<u8>)> {
        let file = format!("{name}_{suffix}.png");
        write_png(&c.out.join(&file), w, h, &rgba).map_err(|e| anyhow!("[out_unwritable] {e}"))?;
        files.push(file);
        Ok((label, rgba))
    };
    let mut tiles = vec![
        tile("baked", format!("baked ev {ev:.1}"), preview_rgba8(&main.rgb, ev))?,
        tile("ddgi", "ddgi".to_string(), preview_rgba8(&ddgi.rgb, ev))?,
        tile(
            "reference",
            format!("reference {} spp", c.spp),
            preview_rgba8(ref_full, ev),
        )?,
        tile(
            "error",
            "flip baked vs ref".to_string(),
            metrics::error_tile(&full_flip.map),
        )?,
        tile(
            "baked_indirect",
            format!("baked indirect ev {iev:.1}"),
            preview_rgba8(&baked_ind, iev),
        )?,
        tile(
            "ddgi_indirect",
            "ddgi indirect".to_string(),
            preview_rgba8(&ddgi_ind, iev),
        )?,
        tile(
            "reference_indirect",
            "reference indirect".to_string(),
            preview_rgba8(&ref_ind, iev),
        )?,
        tile(
            "error_indirect",
            "flip indirect".to_string(),
            metrics::error_tile(&ind_flip.map),
        )?,
    ];
    if baked.len() > 1 {
        for (r, cap) in c.reconstructions.iter().zip(&baked) {
            tiles.push(tile(
                &format!("baked_{}", r.name()),
                format!("{} ev {ev:.1}", r.name()),
                preview_rgba8(&cap.rgb, ev),
            )?);
        }
    }
    // The review row (ADR-1326): what the frame is without any indirect light, and where the
    // baked frame differs from the reference, amplified, at the full row's exposure. Sky pixels
    // come from the reference, as for the scores, so the backgrounds do not show as error.
    let diff = metrics::abs_diff_gain(
        &metrics::fill_sky(&main.rgb, ref_full, depth),
        ref_full,
        DIFF_GAIN,
    );
    tiles.push(tile(
        "direct",
        format!("direct only ev {ev:.1}"),
        preview_rgba8(&direct.rgb, ev),
    )?);
    tiles.push(tile(
        "diff4x",
        format!("|baked - ref| x{DIFF_GAIN:.0} ev {ev:.1}"),
        preview_rgba8(&diff, ev),
    )?);
    let ref_exr = format!("{name}_reference.exr");
    reference
        .write_exr(&c.out.join(&ref_exr), &[])
        .map_err(|e| anyhow!("[out_unwritable] {e}"))?;
    files.push(ref_exr);
    let raster_exr = format!("{name}_raster.exr");
    let planes: [(&str, &[[f32; 3]]); 6] = [
        ("baked", &main.rgb),
        ("ddgi", &ddgi.rgb),
        ("direct", &direct.rgb),
        ("baked_indirect", &baked_ind),
        ("ddgi_indirect", &ddgi_ind),
        ("reference_indirect", &ref_ind),
    ];
    let mut channels = Vec::with_capacity(planes.len() * 3);
    for (layer, plane) in planes {
        for (k, ch) in ["R", "G", "B"].into_iter().enumerate() {
            channels.push(ExrChannel {
                name: format!("{layer}.{ch}"),
                data: plane.iter().map(|p| p[k]).collect(),
            });
        }
    }
    exr::write(&c.out.join(&raster_exr), w, h, &channels, &[])
        .map_err(|e| anyhow!("[out_unwritable] {raster_exr}: {e}"))?;
    files.push(raster_exr);

    // The sheet: full row, indirect row, then the reconstructions of a sweep.
    let header = vec![
        format!(
            "{} / {} | {}x{} | {} spp | flip {:.3} ssim {:.3} | indirect flip {:.3} ssim {:.3}",
            c.fx.name,
            name,
            w,
            h,
            c.spp,
            full_report.flip_mean,
            full_report.ssim,
            ind_report.flip_mean,
            ind_report.ssim
        ),
        match &leak {
            Some(l) => format!(
                "leak {} (threshold {:.2}) | probes {} | {}",
                l.leak.map_or_else(|| "-".to_string(), |x| format!("{x:.2}")),
                l.threshold,
                gi.probes,
                c.fx.purpose
            ),
            None => format!("probes {} | {}", gi.probes, c.fx.purpose),
        },
        "the numbers are a guide: judge the plates".to_string(),
    ];
    let sheet_tiles: Vec<sheet::Tile<'_>> = tiles
        .iter()
        .map(|(label, rgba)| sheet::Tile {
            label: label.as_str(),
            rgba: rgba.as_slice(),
            size: (w, h),
        })
        .collect();
    let sheet_file = format!("{name}.png");
    let image = sheet::compose(&header, &sheet_tiles, 4, 480)
        .ok_or_else(|| anyhow!("[out_unwritable] empty sheet"))?;
    image
        .save(c.out.join(&sheet_file))
        .with_context(|| format!("[out_unwritable] writing {sheet_file}"))?;

    Ok(ViewSummary {
        name: name.clone(),
        sheet: sheet_file,
        ev100: ev,
        indirect_ev100: iev,
        full: Scores::from_report(&full_report),
        indirect: Scores::from_report(&ind_report),
        ddgi_full: Some(Scores::from_report(&ddgi_report)),
        reconstructions,
        leak,
        gi,
        timings: summary::Timings {
            raster_baked_ms: baked.iter().map(|b| b.ms).sum(),
            raster_ddgi_ms: ddgi.ms,
            raster_direct_ms: direct.ms,
            reference_ms,
            reference_direct_ms: emitter_direct.as_ref().map_or(0.0, |(_, ms)| *ms),
            compare_ms: compare_started.elapsed().as_secs_f64() * 1e3,
        },
        tiles: files,
    })
}

/// Scores `test` against `reference` (linear, display-encoded at `exposure` for FLIP and SSIM),
/// with the sky pixels taken from the reference (`metrics::fill_sky`).
fn score(
    reference: &[[f32; 3]],
    test: &[[f32; 3]],
    depth: &[f32],
    size: (u32, u32),
    exposure: f32,
) -> Result<(
    regia_render::compare::CompareReport,
    regia_render::compare::FlipResult,
)> {
    input::check_score(size, reference, test, depth)?;
    let test = metrics::fill_sky(test, reference, depth);
    Ok(compare_linear(
        reference,
        &test,
        size.0,
        size.1,
        exposure,
        DEFAULT_PPD,
    ))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("[out_unwritable] writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("[out_unwritable] replacing {}", path.display()))
}

/// Writes `summary.json` atomically, after every view, so an interrupted run keeps what it did.
fn write_summary(out: &Path, summary: &Summary) -> Result<()> {
    let text = serde_json::to_string_pretty(summary).context("[out_unwritable] serializing summary.json")?;
    write_atomic(&out.join("summary.json"), text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-8670: a current-spelling card lock also takes the pre-rename lock, second; any other
    /// file is locked alone.
    #[test]
    fn a_card_lock_also_takes_its_pre_rename_twin_second() {
        assert_eq!(
            gpu_lock_files(Path::new("/tmp/regia-gpu-2.lock")),
            vec![
                PathBuf::from("/tmp/regia-gpu-2.lock"),
                PathBuf::from("/tmp/ferrite-gpu-2.lock")
            ]
        );
        assert_eq!(
            gpu_lock_files(Path::new("/tmp/regia-gpu-arc.lock"))[1],
            PathBuf::from("/tmp/ferrite-gpu-arc.lock")
        );
        assert_eq!(
            gpu_lock_files(&crate::gpu_bench::lock_path(4)),
            vec![
                crate::gpu_bench::lock_path(4),
                crate::gpu_bench::legacy_lock_path(4)
            ]
        );
        for alone in ["/tmp/ferrite-gpu-2.lock", "/tmp/my.lock", "/tmp/regia-gpu-.lock"] {
            assert_eq!(
                gpu_lock_files(Path::new(alone)),
                vec![PathBuf::from(alone)],
                "{alone}"
            );
        }
    }

    #[test]
    fn the_views_file_sits_next_to_the_fixture() {
        assert_eq!(
            default_views_path(Path::new("assets/fixtures/gi_compare/cornell.gi_compare.ron")),
            PathBuf::from("assets/fixtures/gi_compare/cornell.views.ron")
        );
        assert_eq!(
            default_views_path(Path::new("x/room.ron")),
            PathBuf::from("x/room.views.ron")
        );
    }

    #[test]
    fn relative_inputs_also_resolve_against_the_workspace_root() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let rel = Path::new("assets/fixtures/gi_compare/cornell.gi_compare.ron");
        assert!(
            resolve(&root, rel).exists(),
            "found under the root from any directory"
        );
        let missing = Path::new("no/such/fixture.gi_compare.ron");
        assert_eq!(
            resolve(&root, missing),
            missing.to_path_buf(),
            "a missing path stays as given"
        );
        let abs = root.join(rel);
        assert_eq!(resolve(&root, &abs), abs);
    }

    #[test]
    fn l1_and_zh3_reconstruct_windowed_is_refused_and_unknown_names_are_refused() {
        let l1 = parse_reconstruction("l1").expect("l1");
        assert_eq!(l1, vec![Reconstruction::L1]);
        let l1_settings = l1[0].settings().expect("l1 settings");
        assert!(!l1_settings.zh3, "l1 turns ZH3 off");
        let sweep = parse_reconstruction("sweep").expect("sweep");
        assert_eq!(sweep.len(), 3);
        let zh3 = sweep[2].settings().expect("zh3 is in this build");
        assert!(zh3.zh3);
        assert_eq!(
            zh3,
            BakedGiSettings {
                zh3: true,
                ..l1_settings
            },
            "only the evaluation differs"
        );
        let err = sweep[1].settings().expect_err("windowed is not in this build");
        assert!(
            err.to_string().starts_with("[reconstruction_unavailable]"),
            "{err}"
        );
        let err = parse_reconstruction("l2").expect_err("unknown");
        assert!(err.to_string().starts_with("[reconstruction_unknown]"), "{err}");
    }

    /// Every checked-in fixture parses, builds its layout and frames, and has a views file
    /// whose interior boxes (if any) lie inside one of the fixture's interiors or bricks.
    #[test]
    fn the_checked_in_fixtures_and_views_are_valid() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/fixtures/gi_compare");
        let mut found = 0;
        for name in [
            "cornell",
            "leak_room",
            "rocks_capsule",
            "corridor",
            "courtyard",
            "window_room",
            "outpost_standin",
        ] {
            let fx_path = dir.join(format!("{name}.gi_compare.ron"));
            let text = std::fs::read_to_string(&fx_path).expect("fixture file");
            let fx =
                parse_fixture(&text).unwrap_or_else(|e| panic!("{}: [{}] {e}", fx_path.display(), e.code()));
            assert_eq!(fx.name, name);
            let views_text = std::fs::read_to_string(default_views_path(&fx_path)).expect("views file");
            let set = parse_views(&views_text).unwrap_or_else(|e| panic!("{name}: [{}] {e}", e.code()));
            let layout = fx.volume_layout().expect("layout");
            assert!(
                layout.probe_count() <= 200_000,
                "{name}: {} probes",
                layout.probe_count()
            );
            for view in &set.views {
                let lab = fx.lab_scene(
                    &FrameParams {
                        view,
                        size: set.size,
                        ev100: set.ev100,
                    },
                    true,
                    true,
                );
                assert!(!lab.meshes.is_empty());
            }
            found += 1;
        }
        assert_eq!(found, 7);
    }

    /// Look review (ADR-1364): every interior brick of a checked-in fixture has its probe lattice
    /// on one of the fixture's interior boxes (the walls' inner faces), as `regia bake-gi
    /// --from-scene` places it. A lattice inside the air (the first review fixtures) drew bright
    /// seams along every room edge and lit the outpost's hab with outdoor light.
    #[test]
    fn interior_bricks_lie_on_their_interior_box() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/fixtures/gi_compare");
        let mut interiors = 0;
        for name in [
            "cornell",
            "leak_room",
            "rocks_capsule",
            "corridor",
            "courtyard",
            "window_room",
            "outpost_standin",
        ] {
            let text =
                std::fs::read_to_string(dir.join(format!("{name}.gi_compare.ron"))).expect("fixture file");
            let fx = parse_fixture(&text).unwrap_or_else(|e| panic!("{name}: [{}] {e}", e.code()));
            let layout = fx.volume_layout().expect("layout");
            for brick in layout.bricks.iter().filter(|b| b.is_interior()) {
                let lo = brick.origin;
                let hi: [f32; 3] =
                    std::array::from_fn(|a| lo[a] + brick.spacing[a] * (brick.counts[a] - 1) as f32);
                let on_a_box = fx.layout.interiors.iter().any(|b| {
                    (0..3).all(|a| (b.min[a] - lo[a]).abs() < 1e-3 && (b.max[a] - hi[a]).abs() < 1e-3)
                });
                assert!(
                    on_a_box,
                    "{name}: interior brick {lo:?}..{hi:?} is not on an interior box"
                );
                interiors += 1;
            }
        }
        // Cornell, leak room, corridor, window room, the outpost's hab and hangar.
        assert_eq!(interiors, 6);
    }

    #[test]
    fn size_overrides_parse_and_bad_ones_have_a_code() {
        assert_eq!(parse_size("1920x1080").expect("size"), (1920, 1080));
        assert_eq!(parse_size("640X360").expect("size"), (640, 360));
        for bad in ["1920", "x1080", "1920x", "-1x5", "axb", ""] {
            let err = parse_size(bad).expect_err(bad);
            assert!(err.to_string().starts_with("[views_size]"), "{bad}: {err}");
        }
    }

    /// The review pack's views files carry a fly-through path whose frames are valid cameras.
    #[test]
    fn the_review_pack_views_have_a_flythrough() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/fixtures/gi_compare");
        for name in ["corridor", "courtyard", "window_room", "outpost_standin"] {
            let text = std::fs::read_to_string(dir.join(format!("{name}.views.ron"))).expect("views file");
            let set = parse_views(&text).unwrap_or_else(|e| panic!("{name}: [{}] {e}", e.code()));
            assert_eq!(set.flythrough_views(6).len(), 6, "{name}");
        }
    }

    #[test]
    fn the_leak_fixture_flags_its_interior_and_the_capsule_fixture_has_an_occluder() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/fixtures/gi_compare");
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).expect("fixture file");
        let room = parse_fixture(&read("leak_room.gi_compare.ron")).expect("leak room");
        let layout = room.volume_layout().expect("layout");
        assert!(
            layout.bricks.iter().any(|b| b.flags != 0),
            "the room's brick is an interior brick"
        );
        let views = parse_views(&read("leak_room.views.ron")).expect("views");
        assert!(
            views.views.iter().any(|v| v.interior.is_some()),
            "a view has the leak mask"
        );
        let rocks = parse_fixture(&read("rocks_capsule.gi_compare.ron")).expect("rocks");
        assert_eq!(rocks.occluders().len(), 1);
        let cornell = parse_fixture(&read("cornell.gi_compare.ron")).expect("cornell");
        assert!(cornell.has_emitters() && cornell.sun.is_none());
    }
}

#[cfg(test)]
mod command_tests;
