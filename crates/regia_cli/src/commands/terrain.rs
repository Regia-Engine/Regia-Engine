// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-core/examples/tunnels_export.rs, commit d6ed37677), ADR-8012.
// Lines 1-24, mark: adapt (named issue: the example wrote DO27's export to its own content tree; Regia
// keeps the passage description as `<map>.passages.ron` and writes `do27-tunnels/1` back on request).
// Plan row W-06, ADR-8112.

//! `regia terrain passages import|export`: passage descriptions between DO27's `do27-tunnels/1`
//! JSON and Regia's `<map>.passages.ron` (spec 121 § 9.1, ADR-8112; book
//! `agents/terrain-mesh-layers-formats.md`). `regia terrain cook`: a `<map>.meshlayers.ron` to its
//! cooked `<map>.meshlayers`, canonical and render halves, with the cook report (plan row W-07,
//! ADR-8175). The same cook writes `<map>.viscells.ron` beside the document (plan row W-21,
//! ADR-8371).
//! `regia terrain check-layers` runs the spec 121 § 10 checks on that document, or on a
//! cooked file's sibling source, and prints `terrain.bake_report` (plan row W-09, ADR-8359).
//!
//! `import` reads a `tunnels.json` written by DO27's tunnel export and writes the passage
//! description a `Passages` layer source of `<map>.meshlayers.ron` names, keeping the baked blocks
//! (the sampled centrelines, the mouth holes, the triangle count) so the cook can refuse a stale
//! import (`passages_drift`). `export` writes the JSON back: an imported file round trips byte for
//! byte. Adapted from DO27's `ir-core/examples/tunnels_export.rs` (d6ed37677, regia-absorb-1).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use regia_world_geom::GroundMode;
use regia_world_geom::TerrainBakeReport;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, GroundInput, cook};
use regia_world_geom::passages::PassagesDocument;

/// `regia terrain` subcommands.
#[derive(Debug, Subcommand)]
pub enum TerrainCommand {
    /// Passage descriptions: `import` DO27's `do27-tunnels/1` JSON as `<map>.passages.ron`, `export`
    /// it back.
    #[command(subcommand)]
    Passages(PassagesCommand),
    /// Cook a `<map>.meshlayers.ron` (canonical triangles and the render mesh) to `<map>.meshlayers`
    /// on flat ground, and print the cook report.
    Cook(TerrainCookArgs),
    /// Run the spec 121 § 10 checks on a `<map>.meshlayers.ron`, or on a cooked `<map>.meshlayers`
    /// by cooking its sibling `<map>.meshlayers.ron`. A missing file is `input_missing`.
    CheckLayers(CheckLayersArgs),
    /// Bake the north-up overview and minimap PNGs for one map directory (ADR-8333).
    OverviewBake(OverviewBakeArgs),
    /// Read a count-based terrain source (`.json` or 16-bit `.png`) and print its preview as JSON:
    /// size, datum, units, bounds, effective limits, cook mapping (ADR-8541; the value
    /// `terrain.inspect_source` answers).
    SourceInspect(SourceInspectArgs),
    /// Publish a count-based terrain source as a terrain's `heightfield.png` plus its schema-2
    /// `heightfield.json` mapping, atomically (ADR-8541); `--dry-run` prints the preview only.
    SourceImport(SourceImportArgs),
    /// Bake a count-based source's north-up shaded overview PNG and publish it with its engine
    /// descriptor `<name>.overview.json` (image-to-world mapping, legend, markers), cached by
    /// source and settings (ADR-8596; `terrain.bake_overview { register: true }`).
    SourceOverview(SourceOverviewArgs),
}

/// The source options `source-inspect` and `source-import` share (the `terrain.inspect_source`
/// parameters).
#[derive(Debug, Args)]
pub struct SourceArgs {
    /// The source, relative to the project root.
    pub path: String,
    /// The project root (default: the current directory).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// A project-relative `heightfield.json` whose schema-2 count mapping reinterprets the counts.
    #[arg(long)]
    pub mapping: Option<String>,
    /// PNG settings as a JSON object (`meters_per_count`, `cell_m`, `sample0`, `datum_keyword`,
    /// `zero_counts`, `center_zero`).
    #[arg(long)]
    pub png: Option<String>,
    /// Lower import limits as a JSON object (`max_bytes`, `max_samples`, ...).
    #[arg(long)]
    pub limits: Option<String>,
}

/// `regia terrain source-inspect`.
#[derive(Debug, Args)]
pub struct SourceInspectArgs {
    /// The source.
    #[command(flatten)]
    pub source: SourceArgs,
}

/// `regia terrain source-import`.
#[derive(Debug, Args)]
pub struct SourceImportArgs {
    /// The source.
    #[command(flatten)]
    pub source: SourceArgs,
    /// The destination `heightfield.png`, relative to the project root.
    #[arg(long)]
    pub dest: String,
    /// Replace a destination `heightfield.json` that maps counts differently (or a legacy extents
    /// file) with the source's mapping.
    #[arg(long)]
    pub adopt_source: bool,
    /// Print the preview and write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

/// `regia terrain source-overview`.
#[derive(Debug, Args)]
pub struct SourceOverviewArgs {
    /// The source.
    #[command(flatten)]
    pub source: SourceArgs,
    /// A raster descriptor (ADR-8542), project-relative.
    #[arg(long)]
    pub raster: Option<String>,
    /// The overview PNG, relative to the project root; the descriptor is written beside it.
    #[arg(long)]
    pub output: String,
    /// `stretch` (the legacy default: the source fills the image) or `preserve_aspect` (one scale,
    /// centered, letterboxed).
    #[arg(long)]
    pub fit: Option<String>,
    /// Output size, `<width>x<height>` pixels.
    #[arg(long)]
    pub size: Option<String>,
    /// More overview settings as a JSON object (`relief`, `contour_step_m`, `blur_passes`, `light`,
    /// `ambient`, `contrast`, `frame_pixels`), as `terrain.bake_overview`'s `overview`.
    #[arg(long)]
    pub settings: Option<String>,
    /// A marker, `<label>=<x>,<z>` in world meters; repeatable.
    #[arg(long = "marker")]
    pub markers: Vec<String>,
}

/// The `terrain.bake_overview { register: true }` parameters of `args`.
///
/// # Errors
///
/// A malformed `--size`, `--settings` or `--marker`.
pub fn overview_params(args: &SourceOverviewArgs) -> Result<serde_json::Value> {
    let mut params = source_params(&args.source)?;
    params["output"] = serde_json::json!(args.output);
    if let Some(raster) = &args.raster {
        params["raster"] = serde_json::json!(raster);
    }
    let mut overview = match &args.settings {
        Some(text) => serde_json::from_str(text).context("--settings is not JSON")?,
        None => serde_json::json!({}),
    };
    let Some(fields) = overview.as_object_mut() else {
        bail!("--settings must be a JSON object");
    };
    if let Some(fit) = &args.fit {
        fields.insert("fit".into(), serde_json::json!(fit));
    }
    if let Some(size) = &args.size {
        let parsed = size
            .split_once('x')
            .and_then(|(w, h)| Some([w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?]));
        let Some(size) = parsed else {
            bail!("--size must be <width>x<height>, not {size:?}");
        };
        fields.insert("size".into(), serde_json::json!(size));
    }
    params["overview"] = overview;
    let mut markers = Vec::new();
    for marker in &args.markers {
        let parsed = marker.rsplit_once('=').and_then(|(label, xz)| {
            let (x, z) = xz.split_once(',')?;
            Some((
                label,
                [x.trim().parse::<f64>().ok()?, z.trim().parse::<f64>().ok()?],
            ))
        });
        let Some((label, xz)) = parsed else {
            bail!("--marker must be <label>=<x>,<z>, not {marker:?}");
        };
        markers.push(serde_json::json!({ "label": label, "xz": xz }));
    }
    if !markers.is_empty() {
        params["markers"] = serde_json::Value::Array(markers);
    }
    Ok(params)
}

/// `source-overview`: [`regia_agent::publish_project_overview`]. Interrupting the process before
/// publication leaves no PNG or descriptor (the pair is written to temporary names first).
///
/// # Errors
///
/// The service's refusal (the message carries its code).
pub fn source_overview(args: &SourceOverviewArgs) -> Result<serde_json::Value> {
    let params = overview_params(args)?;
    let markers = regia_agent::overview_markers(None, &params).map_err(|e| rpc_error(&e))?;
    regia_agent::publish_project_overview(&args.source.project, Default::default(), &params, markers, |_| {
        std::ops::ControlFlow::Continue(())
    })
    .map_err(|e| rpc_error(&e))
}

/// The `terrain.inspect_source` parameters of `args`.
///
/// # Errors
///
/// `--png` or `--limits` is not a JSON object.
pub fn source_params(args: &SourceArgs) -> Result<serde_json::Value> {
    let mut params = serde_json::json!({ "path": args.path });
    for (key, text) in [("png", &args.png), ("limits", &args.limits)] {
        if let Some(text) = text {
            let value: serde_json::Value =
                serde_json::from_str(text).with_context(|| format!("--{key} is not JSON"))?;
            if !value.is_object() {
                bail!("--{key} must be a JSON object");
            }
            params[key] = value;
        }
    }
    if let Some(mapping) = &args.mapping {
        params["mapping"] = serde_json::json!(mapping);
    }
    Ok(params)
}

fn rpc_error(e: &regia_agent::protocol::RpcError) -> anyhow::Error {
    let code = e
        .data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("terrain_source_bad_params");
    anyhow::anyhow!("{} ({code})", e.message)
}

/// `source-inspect`: the preview `terrain.inspect_source` answers, for the same parameters.
///
/// # Errors
///
/// The source's refusal (the message carries its code).
pub fn source_inspect(args: &SourceInspectArgs) -> Result<serde_json::Value> {
    let params = source_params(&args.source)?;
    // A `*.tiles.json` source tile set (ADR-8595) answers its bounded validation read.
    regia_agent::inspect_project_source(&args.source.project, Default::default(), &params)
        .map_err(|e| rpc_error(&e))
}

/// `source-import`: [`regia_agent::publish_source_asset`].
///
/// # Errors
///
/// The service's refusal (the message carries its code).
pub fn source_import(args: &SourceImportArgs) -> Result<serde_json::Value> {
    let params = source_params(&args.source)?;
    regia_agent::publish_source_asset(
        &args.source.project,
        Default::default(),
        &params,
        &args.dest,
        args.adopt_source,
        args.dry_run,
    )
    .map_err(|e| rpc_error(&e))
}

/// `regia terrain cook`.
#[derive(Debug, Args)]
pub struct TerrainCookArgs {
    /// The `<map>.meshlayers.ron` document; the files it names are read relative to it.
    pub document: PathBuf,
    /// The flat ground's height, metres (-511..1535; the heightfield input arrives with the map
    /// terrain lanes).
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    pub ground_y: f32,
    /// The map's width, metres.
    #[arg(long, default_value_t = 2048.0)]
    pub width_m: f32,
    /// Where to write; default the document path without `.ron`.
    #[arg(short, long)]
    pub out: Option<PathBuf>,
    /// Print the cook report as JSON on stdout.
    #[arg(long)]
    pub json: bool,
}

/// `regia terrain check-layers`.
#[derive(Debug, Args)]
pub struct CheckLayersArgs {
    /// A `<map>.meshlayers.ron`, or a cooked `<map>.meshlayers` whose sibling source is
    /// `<map>.meshlayers.ron`.
    pub map: PathBuf,
    /// Flat ground height, metres, used when the check cooks the source document.
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    pub ground_y: f32,
    /// Map width, metres.
    #[arg(long, default_value_t = 2048.0)]
    pub width_m: f32,
    /// Print `terrain.bake_report` as JSON on stdout.
    #[arg(long)]
    pub json: bool,
}

/// One passage's render triangles in the cook report.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct RenderSectionSummary {
    /// The passage id.
    pub passage: String,
    /// Its render triangles.
    pub triangles: u32,
}

/// One layer of the cook report.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct LayerSummary {
    /// The layer id.
    pub id: String,
    /// Canonical triangles.
    pub triangles: u32,
    /// Vertices the weld step moved onto the ground.
    pub welded_vertices: u32,
    /// Floor triangles the trim step took out of support.
    pub trimmed_triangles: u32,
    /// Zero-area canonical triangles.
    pub degenerate_triangles: u32,
    /// Render triangles (the draw list).
    pub render_triangles: u32,
    /// Render triangles per passage (the cut sections are in [`CookSummary::sections`]).
    pub render_sections: Vec<RenderSectionSummary>,
    /// Segments of skylight ramps opened to the sky (K8).
    pub skylight_segments_opened: u32,
    /// Sections cut from this layer.
    pub sections: u32,
    /// Blocker plug triangles appended to this layer.
    pub plug_triangles: u32,
}

/// What `cook` wrote.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct CookSummary {
    /// The file written.
    pub out: PathBuf,
    /// The canonical section's BLAKE3 (`content_hash`), hex.
    pub content_hash: String,
    /// The file's size, bytes.
    pub bytes: u64,
    /// One entry per layer, in document order.
    pub layers: Vec<LayerSummary>,
    /// Every cut section, in id order (lane W-08; `terrain.bake_report` reads it).
    pub sections: Vec<regia_world_geom::cook::SectionReport>,
    /// Every portal, in id order.
    pub portals: Vec<regia_world_geom::cook::PortalReport>,
    /// `<map>.viscells.ron` written beside the document (lane W-21).
    pub viscells: PathBuf,
    /// Visibility cells in the file (outdoor cell 0, then one per section).
    pub vis_cells: Vec<regia_world_geom::cook::VisCellReport>,
    /// Visibility portal quads in the file.
    pub vis_portals: Vec<regia_world_geom::cook::VisPortalReport>,
    /// Every spec 121 § 10 check (`terrain.bake_report`).
    pub bake_report: TerrainBakeReport,
}

/// `regia terrain overview-bake`.
#[derive(Debug, Args)]
pub struct OverviewBakeArgs {
    /// Map directory (`heightfield.json`, `layout.json`, optional `tunnels.json`).
    pub map: PathBuf,
    /// Where to write the PNGs. Default: the map directory.
    #[arg(short, long)]
    pub out: Option<PathBuf>,
    /// Print paths, sides and BLAKE3 digests as one JSON object.
    #[arg(long)]
    pub json: bool,
    /// Overview width and height. Must divide evenly by 4.
    #[arg(long, default_value_t = 1024)]
    pub size: u32,
    /// Contour spacing in metres. Zero disables contours.
    #[arg(long, default_value_t = 5.0)]
    pub contour_step: f64,
    /// Rewrite the PNGs when they already exist.
    #[arg(long)]
    pub force: bool,
}

/// `regia terrain passages` subcommands.
#[derive(Debug, Subcommand)]
pub enum PassagesCommand {
    /// Convert a `do27-tunnels/1` file (`tunnels.json`), or a `do27-tunnels-v2/1` description, to
    /// `<map>.passages.ron`.
    Import(ImportArgs),
    /// Write a `<map>.passages.ron` as `do27-tunnels/1` JSON.
    Export(ExportArgs),
}

/// `regia terrain passages import`.
#[derive(Debug, Args)]
pub struct ImportArgs {
    /// The `do27-tunnels/1` JSON file, or a `do27-tunnels-v2/1` description.
    pub file: PathBuf,
    /// Where to write; default `<map_id>.passages.ron` next to the input.
    #[arg(short, long)]
    pub out: Option<PathBuf>,
    /// The map id of a `do27-tunnels-v2/1` description (it carries none); default the file stem.
    #[arg(long)]
    pub map_id: Option<String>,
    /// Print a summary as JSON on stdout.
    #[arg(long)]
    pub json: bool,
    /// A `<map>.meshlayers.ron` naming the imported file: with `--heightfield`, the summary carries
    /// the cooked ground (mode, per-tile levels, mouths; spec 121 § 3, ADR-8214).
    #[arg(long, requires = "heightfield")]
    pub meshlayers: Option<PathBuf>,
    /// The map's 16-bit grayscale heightfield PNG (the sim ground lattice's samples).
    #[arg(long, requires = "meshlayers", requires = "world_width_m")]
    pub heightfield: Option<PathBuf>,
    /// The heightfield's world width, metres.
    #[arg(long)]
    pub world_width_m: Option<f32>,
    /// Metres from count 0 to count 65535 (default 2048, 1/32 m a count).
    #[arg(long, default_value_t = 2048.0)]
    pub height_scale_m: f32,
    /// The height of count 0, metres.
    #[arg(long, default_value_t = 0.0)]
    pub height_offset_m: f32,
}

/// `regia terrain passages export`.
#[derive(Debug, Args)]
pub struct ExportArgs {
    /// The `<map>.passages.ron` file.
    pub file: PathBuf,
    /// Write here instead of standard output.
    #[arg(short, long)]
    pub out: Option<PathBuf>,
}

/// What `import` wrote.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct ImportSummary {
    /// The file written.
    pub out: PathBuf,
    /// The layout's map id.
    pub map_id: String,
    /// Passage count.
    pub passages: usize,
    /// Opening count.
    pub openings: usize,
    /// Junction count.
    pub junctions: usize,
    /// The triangle count the export recorded.
    pub triangle_count: Option<u64>,
    /// The cooked ground, with `--meshlayers` and `--heightfield`.
    pub ground: Option<GroundSummary>,
}

/// The ground a cook of the imported layout gives (spec 121 § 3, ADR-8214).
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct GroundSummary {
    /// The authored ground mode: `Full` or `Nested(max_level: k)`.
    pub mode: String,
    /// Lattice samples per side.
    pub lattice_n: u32,
    /// Lattice cells per ground tile side.
    pub ground_tile_cells: u16,
    /// Ground tiles per side.
    pub tiles_per_side: u32,
    /// Tiles per level (`"0"` is full pitch).
    pub levels: std::collections::BTreeMap<u8, u32>,
    /// The level of every ground tile, ascending tile index (`b * tiles_per_side + a`).
    pub tile_levels: Vec<u8>,
    /// Hanging-vertex overrides (the nested-pitch seam stitch).
    pub hanging_vertices: u32,
    /// Ring pins.
    pub ring_pins: u32,
    /// The mouths cut.
    pub mouths: Vec<MouthSummary>,
    /// The sim-geometry bytes and the budget (spec 121 § 11).
    pub sim_bytes: u64,
    /// The budget.
    pub budget_bytes: u64,
    /// The largest ground step across a pitch border, metres (`Nested` only).
    pub max_pitch_step_m: Option<f32>,
    /// `content_hash` of the canonical section, hex.
    pub content_hash: String,
}

/// One mouth of [`GroundSummary`].
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct MouthSummary {
    /// The named hole id.
    pub id: String,
    /// Ring loops.
    pub rings: usize,
    /// Ring vertices over all its loops.
    pub ring_vertices: usize,
    /// Lattice triangles cut.
    pub cut_triangles: u32,
    /// Collar triangles (canonical floor triangles on the ring).
    pub collar_triangles: u32,
}

/// Runs a `regia terrain` subcommand.
///
/// # Errors
///
/// A file cannot be read or written, or does not parse (the message carries the error code).
pub fn run(command: &TerrainCommand) -> Result<()> {
    match command {
        TerrainCommand::Passages(PassagesCommand::Import(args)) => {
            let summary = import(args)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                println!(
                    "imported {} ({} passages, {} openings, {} junctions) -> {}",
                    summary.map_id,
                    summary.passages,
                    summary.openings,
                    summary.junctions,
                    summary.out.display()
                );
                if let Some(g) = &summary.ground {
                    println!(
                        "ground {}: {} tiles per side, levels {:?}, {} hanging vertices, {} mouths, {} of {} bytes",
                        g.mode,
                        g.tiles_per_side,
                        g.levels,
                        g.hanging_vertices,
                        g.mouths.len(),
                        g.sim_bytes,
                        g.budget_bytes
                    );
                }
            }
            Ok(())
        }
        TerrainCommand::Cook(args) => {
            let summary = cook_layers(args)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                for l in &summary.layers {
                    println!(
                        "layer {}: {} canonical triangles, {} render triangles, {} sections",
                        l.id, l.triangles, l.render_triangles, l.sections
                    );
                }
                println!(
                    "{} sections, {} portals",
                    summary.sections.len(),
                    summary.portals.len()
                );
                println!(
                    "cooked -> {} (content hash {})",
                    summary.out.display(),
                    summary.content_hash
                );
            }
            Ok(())
        }
        TerrainCommand::CheckLayers(args) => check_layers(args),
        TerrainCommand::Passages(PassagesCommand::Export(args)) => {
            let text = export(&args.file)?;
            match &args.out {
                Some(out) => write_atomic(out, &text),
                None => {
                    print!("{text}");
                    Ok(())
                }
            }
        }
        TerrainCommand::OverviewBake(args) => bake_overview(args).map_err(|err| anyhow::anyhow!("{err}")),
        TerrainCommand::SourceInspect(args) => {
            println!("{}", serde_json::to_string_pretty(&source_inspect(args)?)?);
            Ok(())
        }
        TerrainCommand::SourceImport(args) => {
            println!("{}", serde_json::to_string_pretty(&source_import(args)?)?);
            Ok(())
        }
        TerrainCommand::SourceOverview(args) => {
            println!("{}", serde_json::to_string_pretty(&source_overview(args)?)?);
            Ok(())
        }
    }
}

/// Runs `regia terrain`, including overview-bake's exit code 2 on a bad map.
pub fn run_exit(command: &TerrainCommand) -> ExitCode {
    match command {
        TerrainCommand::OverviewBake(args) => match bake_overview(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::from(err.exit_code())
            }
        },
        _ => match run(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        },
    }
}

fn bake_overview(args: &OverviewBakeArgs) -> Result<(), regia_world_geom::overview::OverviewError> {
    let out = args.out.clone().unwrap_or_else(|| args.map.clone());
    let report = regia_world_geom::overview::bake_directory(
        &args.map,
        &out,
        &regia_world_geom::overview::BakeOptions {
            side: args.size,
            contour_step_m: args.contour_step,
            force: args.force,
        },
        None,
    )?;
    if args.json {
        let overview_blake3 = digest_file(&report.overview)?;
        let minimap_blake3 = digest_file(&report.minimap)?;
        println!(
            "{}",
            serde_json::json!({
                "overview": report.overview,
                "minimap": report.minimap,
                "side": report.side,
                "mini_side": report.mini_side,
                "skipped": report.skipped,
                "overview_blake3": overview_blake3,
                "minimap_blake3": minimap_blake3,
            })
        );
    } else if report.skipped {
        println!("have {}", report.overview.display());
        println!("have {}", report.minimap.display());
    } else {
        println!("wrote {}", report.overview.display());
        println!("wrote {}", report.minimap.display());
    }
    Ok(())
}

fn digest_file(path: &Path) -> Result<String, regia_world_geom::overview::OverviewError> {
    let bytes = std::fs::read(path).map_err(|err| {
        regia_world_geom::overview::OverviewError::Input(format!("reading {}: {err}", path.display()))
    })?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// `import`: reads, converts and writes; returns what it wrote.
///
/// # Errors
///
/// As [`run`].
pub fn import(args: &ImportArgs) -> Result<ImportSummary> {
    let text =
        std::fs::read_to_string(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let imported = if PassagesDocument::is_description(&text) {
        let stem = args.file.file_stem().map(|s| s.to_string_lossy().into_owned());
        let map_id = args.map_id.clone().or(stem).unwrap_or_else(|| "map".into());
        PassagesDocument::import_description(&text, &map_id)
    } else {
        PassagesDocument::import_do27_json(&text)
    };
    let doc = match imported {
        Ok(doc) => doc,
        Err(e) => bail!("{}: {e} ({})", args.file.display(), e.code()),
    };
    let out = match &args.out {
        Some(out) => out.clone(),
        None => {
            let dir = args.file.parent().unwrap_or_else(|| Path::new("."));
            dir.join(format!("{}.passages.ron", doc.layout.map_id))
        }
    };
    let ron = match doc.to_ron() {
        Ok(ron) => ron,
        Err(e) => bail!("{e} ({})", e.code()),
    };
    write_atomic(&out, &ron)?;
    let ground = match (&args.meshlayers, &args.heightfield) {
        (Some(meshlayers), Some(heightfield)) => {
            let bytes =
                std::fs::read(heightfield).with_context(|| format!("reading {}", heightfield.display()))?;
            let hf = match regia_asset_pipeline::import_png_height(&bytes) {
                Ok(hf) => hf,
                Err(e) => bail!("{}: {e} (ground_invalid)", heightfield.display()),
            };
            if hf.width != hf.height {
                bail!(
                    "{}: the ground lattice is square, not {}x{} (ground_invalid)",
                    heightfield.display(),
                    hf.width,
                    hf.height
                );
            }
            let Some(world_width_m) = args.world_width_m else {
                bail!("--heightfield needs --world-width-m");
            };
            let text = std::fs::read_to_string(meshlayers)
                .with_context(|| format!("reading {}", meshlayers.display()))?;
            let dir = meshlayers.parent().unwrap_or_else(|| Path::new("."));
            Some(ground_summary(
                &text,
                dir,
                &out,
                &doc,
                &GroundInput {
                    lattice_n: hf.width,
                    width_m: world_width_m,
                    offset_m: args.height_offset_m,
                    step_m: args.height_scale_m / 65535.0,
                    counts: hf.samples,
                },
            )?)
        }
        _ => None,
    };
    Ok(ImportSummary {
        out,
        map_id: doc.layout.map_id.clone(),
        passages: doc.layout.passages.len(),
        openings: doc.layout.openings.len(),
        junctions: doc.layout.junctions.len(),
        triangle_count: doc.triangle_count,
        ground,
    })
}

/// Cooks `document` (its `Passages` layer naming `imported` reads the just-imported layout; other
/// files are read next to the document) on `ground` and summarizes the ground. The layout is sampled
/// on a Stage A sampler over the same heightfield, so the imported sampled block (DO27's own grid)
/// is not compared here; the map's real cook does that (`passages_drift`).
///
/// # Errors
///
/// The cook's error, with its code.
pub fn ground_summary(
    document: &str,
    dir: &Path,
    imported: &Path,
    doc: &PassagesDocument,
    ground: &GroundInput,
) -> Result<GroundSummary> {
    let report_doc = PassagesDocument {
        sampled: None,
        holes: None,
        ..doc.clone()
    };
    let imported_ron = match report_doc.to_ron() {
        Ok(ron) => ron,
        Err(e) => bail!("{e} ({})", e.code()),
    };
    let imported_name = imported.file_name().map(std::ffi::OsStr::to_os_string);
    let read = |p: &str| -> std::result::Result<Vec<u8>, String> {
        if Path::new(p).file_name().map(std::ffi::OsStr::to_os_string) == imported_name {
            return Ok(imported_ron.clone().into_bytes());
        }
        std::fs::read(dir.join(p)).map_err(|e| e.to_string())
    };
    // The Stage A sampler over the same heights at 1/32 m a count, zero at count 32768: the rule
    // `regia cook` uses for a level's mesh layers (ADR-8451).
    let Some(terrain) = regia_asset_pipeline::terrain_mesh_layers::stage_a_sampler(ground) else {
        bail!("the heightfield is not a square lattice (ground_invalid)");
    };
    let out = match cook(&CookInputs {
        document: document.as_bytes(),
        ground,
        terrain: Some(&terrain),
        read_file: &read,
    }) {
        Ok(out) => out,
        Err(e) => bail!("{e} ({})", e.code()),
    };
    let g = &out.report.ground;
    let mut levels = std::collections::BTreeMap::new();
    for &l in &g.tile_levels {
        *levels.entry(l).or_insert(0u32) += 1;
    }
    let hash = match regia_world_geom::cooked::content_hash_of(&out.file.canonical) {
        Ok(h) => h,
        Err(e) => bail!("{e} ({})", e.code()),
    };
    Ok(GroundSummary {
        mode: match g.mode {
            GroundMode::Full => "Full".into(),
            GroundMode::Nested { max_level } => format!("Nested(max_level: {max_level})"),
        },
        lattice_n: out.file.canonical.lattice.lattice_n,
        ground_tile_cells: out.file.canonical.lattice.ground_tile_cells,
        tiles_per_side: g.tiles_per_side,
        levels,
        tile_levels: g.tile_levels.clone(),
        hanging_vertices: g.hanging_vertices,
        ring_pins: g.ring_pins,
        mouths: g
            .mouths
            .iter()
            .map(|m| MouthSummary {
                id: m.id.clone(),
                rings: m.rings.len(),
                ring_vertices: m.rings.iter().map(Vec::len).sum(),
                cut_triangles: m.cut_triangles,
                collar_triangles: m.collar_triangles,
            })
            .collect(),
        sim_bytes: g.sim_bytes,
        budget_bytes: g.budget_bytes,
        max_pitch_step_m: g.pitch_seam.map(|s| s.max_step_m),
        content_hash: hash.iter().map(|b| format!("{b:02x}")).collect(),
    })
}

/// `export`: the `do27-tunnels/1` text of a passages file.
///
/// # Errors
///
/// As [`run`]; `passages_unsampled` for a Regia-authored file without a sampled block.
pub fn export(file: &Path) -> Result<String> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let doc = match PassagesDocument::from_ron(&text) {
        Ok(doc) => doc,
        Err(e) => bail!("{}: {e} ({})", file.display(), e.code()),
    };
    match doc.to_do27_json() {
        Ok(json) => Ok(json),
        Err(e) => bail!("{}: {e} ({})", file.display(), e.code()),
    }
}

/// Flat ground at `y` metres: the sim lattice (`offset -512`, 1/32 m counts) and the Stage A sampler
/// over the same heights.
fn flat_ground(y: f32, width_m: f32) -> Result<(GroundInput, TerrainSampler)> {
    if !y.is_finite() || !(-511.0..=1535.0).contains(&y) {
        bail!("--ground-y {y} must be within -511..1535 m (ground_invalid)");
    }
    if !width_m.is_finite() || width_m <= 0.0 {
        bail!("--width-m {width_m} must be above 0 (ground_invalid)");
    }
    let count = ((y + 512.0) * 32.0).round() as u16;
    let ground = GroundInput {
        lattice_n: 2,
        width_m,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![count; 4],
    };
    let Some(grid) = HeightGrid::square(2, vec![count; 4]) else {
        bail!("flat ground: no grid (ground_invalid)");
    };
    Ok((
        ground,
        TerrainSampler::new(grid, width_m).with_zero_counts(16_384),
    ))
}

/// `cook`: reads the document and its files, cooks, writes the cooked file; returns the report.
///
/// # Errors
///
/// A file cannot be read or written; the cook fails (the message carries the error code).
pub fn cook_layers(args: &TerrainCookArgs) -> Result<CookSummary> {
    let document = std::fs::read(&args.document)
        .with_context(|| format!("reading {} (input_missing)", args.document.display()))?;
    let dir = args.document.parent().map(Path::to_path_buf).unwrap_or_default();
    let read = |p: &str| std::fs::read(dir.join(p)).map_err(|e| e.to_string());
    let (ground, terrain) = flat_ground(args.ground_y, args.width_m)?;
    let mut out = match cook(&CookInputs {
        document: &document,
        ground: &ground,
        terrain: Some(&terrain),
        read_file: &read,
    }) {
        Ok(out) => out,
        Err(e) => bail!("{}: {e} ({})", args.document.display(), e.code()),
    };
    let bytes = match out.file.encode() {
        Ok(bytes) => bytes,
        Err(e) => bail!("{e} ({})", e.code()),
    };
    let hash = match regia_world_geom::cooked::content_hash_of(&out.file.canonical) {
        Ok(hash) => hash,
        Err(e) => bail!("{e} ({})", e.code()),
    };
    let path = match &args.out {
        Some(p) => p.clone(),
        None => {
            let name = args.document.to_string_lossy();
            PathBuf::from(
                name.strip_suffix(".ron")
                    .map_or_else(|| format!("{name}.cooked"), str::to_string),
            )
        }
    };
    write_atomic_bytes(&path, &bytes)?;
    let viscells = regia_render::viscells::viscells_ron_path(&args.document);
    write_atomic(&viscells, &out.viscells_ron)?;
    let sections = std::mem::take(&mut out.report.sections);
    let portals = std::mem::take(&mut out.report.portals);
    let vis_cells = std::mem::take(&mut out.report.vis_cells);
    let vis_portals = std::mem::take(&mut out.report.vis_portals);
    let bake_report = out.bake_report;
    Ok(CookSummary {
        out: path,
        sections,
        portals,
        bake_report,
        viscells,
        vis_cells,
        vis_portals,
        content_hash: hash.iter().map(|b| format!("{b:02x}")).collect(),
        bytes: bytes.len() as u64,
        layers: out
            .report
            .layers
            .into_iter()
            .map(|l| LayerSummary {
                id: l.id,
                triangles: l.triangles,
                welded_vertices: l.welded_vertices,
                trimmed_triangles: l.trimmed_triangles,
                degenerate_triangles: l.degenerate_triangles,
                render_triangles: l.render_triangles,
                render_sections: l
                    .render_sections
                    .into_iter()
                    .map(|s| RenderSectionSummary {
                        passage: s.passage,
                        triangles: s.triangles,
                    })
                    .collect(),
                skylight_segments_opened: l.skylight_segments_opened,
                sections: l.sections,
                plug_triangles: l.plug_triangles,
            })
            .collect(),
    })
}

/// Runs the spec 121 § 10 checks. A `.ron` source is cooked in memory. Any other path is a cooked
/// file, and the check cooks the sibling `<path>.ron`. Nothing is written.
///
/// # Errors
///
/// `input_missing` when the path or its sibling source cannot be read. A failed check prints
/// `terrain.bake_report` and bails with that check's code.
pub fn check_layers(args: &CheckLayersArgs) -> Result<()> {
    let ron = source_ron(&args.map)?;
    let document =
        std::fs::read(&ron).with_context(|| format!("input_missing: {} cannot be read", ron.display()))?;
    let dir = ron.parent().map(Path::to_path_buf).unwrap_or_default();
    let read = |path: &str| std::fs::read(dir.join(path)).map_err(|e| e.to_string());
    let (ground, terrain) = flat_ground(args.ground_y, args.width_m)?;
    match cook(&CookInputs {
        document: &document,
        ground: &ground,
        terrain: Some(&terrain),
        read_file: &read,
    }) {
        Ok(out) => print_bake_report(args.json, &out.bake_report),
        Err(CookError::Checks(report)) => {
            print_bake_report(args.json, &report)?;
            bail!("{}", report.failure_code());
        }
        Err(e) => bail!("{e} ({})", e.code()),
    }
}

/// The source document for a check. A missing path is `input_missing`, never an empty pass.
fn source_ron(map: &Path) -> Result<PathBuf> {
    if !map.exists() {
        bail!("input_missing: {} cannot be read", map.display());
    }
    if map.extension().and_then(|ext| ext.to_str()) == Some("ron") {
        return Ok(map.to_path_buf());
    }
    let sibling = PathBuf::from(format!("{}.ron", map.display()));
    if !sibling.exists() {
        bail!(
            "input_missing: {} cannot be read (never an empty pass)",
            sibling.display()
        );
    }
    Ok(sibling)
}

/// Prints `terrain.bake_report` as JSON or as one line per failed check.
fn print_bake_report(json: bool, report: &TerrainBakeReport) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        println!("{report}");
    }
    Ok(())
}

/// Writes bytes through a temporary file and a rename.
fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
}

/// Writes through a temporary file and a rename, so a reader never sees half a file.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_world_geom::compat::layout::{
        LAYOUT_VERSION, Passage, PassageKind, SAMPLE_STEP_M, TunnelLayout,
    };
    use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};

    fn layout() -> TunnelLayout {
        TunnelLayout {
            map_id: "demo".into(),
            version: LAYOUT_VERSION,
            sample_step_m: SAMPLE_STEP_M,
            passages: vec![Passage {
                id: "core".into(),
                kind: PassageKind::Tube,
                width_m: 9.0,
                ceiling_m: 10.0,
                points: vec![[-40.0, -20.0, 0.0], [0.0, -20.0, 10.0], [40.0, -20.0, 0.0]],
            }],
            openings: Vec::new(),
            junctions: Vec::new(),
            crawlways: Vec::new(),
        }
    }

    #[test]
    fn import_then_export_gives_the_json_back() {
        let dir = regia_test_utils::temp::unique_temp_dir("terrain-passages");
        let json = layout().export_json(None);
        let input = dir.join("tunnels.json");
        std::fs::write(&input, &json).expect("write fixture");
        let summary = import(&ImportArgs {
            file: input,
            out: None,
            map_id: None,
            json: false,
            meshlayers: None,
            heightfield: None,
            world_width_m: None,
            height_scale_m: 2048.0,
            height_offset_m: 0.0,
        })
        .expect("import");
        assert_eq!(summary.out, dir.join("demo.passages.ron"));
        assert_eq!((summary.map_id.as_str(), summary.passages), ("demo", 1));
        assert_eq!(export(&summary.out).expect("export"), json);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `document` with ADR-8359's K1 waiver rows when its cook (the one `ground_summary` runs:
    /// the imported layout without `sampled`/`holes`, the Stage A sampler of `ground`) is refused
    /// only by `ramp_facet_slope_over_limit`; otherwise `document` unchanged.
    fn waive_known_ramp_facets(document: &str, doc: &PassagesDocument, ground: &GroundInput) -> String {
        let imported = PassagesDocument {
            sampled: None,
            holes: None,
            ..doc.clone()
        }
        .to_ron()
        .expect("to ron");
        let terrain =
            regia_asset_pipeline::terrain_mesh_layers::stage_a_sampler(ground).expect("square lattice");
        let read = |_: &str| -> std::result::Result<Vec<u8>, String> { Ok(imported.clone().into_bytes()) };
        match cook(&CookInputs {
            document: document.as_bytes(),
            ground,
            terrain: Some(&terrain),
            read_file: &read,
        }) {
            Err(CookError::Checks(report)) if regia_world_geom::checks::only_ramp_facet_failures(&report) => {
                regia_world_geom::checks::waive_ramp_facets(document, &report).expect("waive")
            }
            _ => document.to_string(),
        }
    }

    /// ADR-8214: the import summary reports the cooked ground mode, the per-tile levels and the
    /// mouths a layer fills.
    #[test]
    fn the_summary_reports_the_ground_mode_and_levels() {
        let layout = regia_world_geom::compat::fixtures::for_map("switchbacks").expect("fixture");
        let doc = PassagesDocument::from_layout(layout.clone());
        let n = 1025u32;
        let ground = GroundInput {
            lattice_n: n,
            width_m: 1280.0,
            offset_m: -512.0,
            step_m: 1.0 / 32.0,
            counts: vec![8192; (n * n) as usize],
        };
        let terrain = TerrainSampler::new(
            HeightGrid::square(n as usize, vec![8192; (n * n) as usize]).expect("grid"),
            1280.0,
        )
        .with_zero_counts(16384);
        let mouths = regia_world_geom::cut::passage_mouths(&layout, &layout.sample(Some(&terrain)));
        assert!(!mouths.is_empty());
        let fills: Vec<String> = mouths.iter().map(|m| format!("{:?}", m.id)).collect();
        let document = format!(
            "(version: 1, ground_tile_cells: 64, ground: Nested(max_level: 4), layers: [(id: \"tubes\", source: Passages(path: \"sb.passages.ron\"), fills: [{}])])",
            fills.join(", ")
        );
        // ADR-8359 / ADR-8447: the Switchbacks fixture's two known ramp facets (slope 1.58 over
        // the 1.50 limit, K1) refuse every unwaived cook until the Stage B mesher bump; a test cook
        // that fails only that check authors the hashed waiver and recooks. Any other refusal,
        // or a waiver whose triangle hash lapses, still fails `ground_summary` below.
        let document = waive_known_ramp_facets(&document, &doc, &ground);
        let summary = ground_summary(
            &document,
            Path::new("."),
            Path::new("sb.passages.ron"),
            &doc,
            &ground,
        )
        .expect("cooks");
        assert_eq!(summary.mode, "Nested(max_level: 4)");
        assert_eq!(
            summary.tile_levels.len() as u32,
            summary.tiles_per_side * summary.tiles_per_side
        );
        assert_eq!(summary.mouths.len(), mouths.len());
        assert!(summary.ring_pins > 0 && summary.max_pitch_step_m.is_some());
        assert!(
            summary.levels.keys().any(|&l| l > 0),
            "flat ground away from the tubes goes coarse"
        );
        let json = serde_json::to_value(&summary).expect("json");
        assert!(json["tile_levels"].is_array() && json["mode"].is_string());
    }

    #[test]
    fn cook_reports_the_render_triangles_per_section() {
        let dir = regia_test_utils::temp::unique_temp_dir("terrain-cook");
        std::fs::write(
            dir.join("demo.passages.ron"),
            PassagesDocument::from_layout(layout()).to_ron().expect("to ron"),
        )
        .expect("write fixture");
        let doc = dir.join("demo.meshlayers.ron");
        std::fs::write(
            &doc,
            "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"demo.passages.ron\"))])",
        )
        .expect("write fixture");
        let args = TerrainCookArgs {
            document: doc.clone(),
            ground_y: 40.0,
            width_m: 512.0,
            out: None,
            json: true,
        };
        let summary = cook_layers(&args).expect("cook");
        assert_eq!(summary.out, dir.join("demo.meshlayers"));
        let layer = &summary.layers[0];
        assert!(layer.triangles > 0 && layer.render_triangles >= 14 * layer.triangles);
        assert_eq!(
            layer.render_sections,
            vec![RenderSectionSummary {
                passage: "core".into(),
                triangles: layer.render_triangles,
            }]
        );
        let bytes = std::fs::read(&summary.out).expect("cooked file");
        assert_eq!(bytes.len() as u64, summary.bytes);
        // Lane W-08: the report lists the cut sections and their portals (one tube, no openings: one covered
        // section, no portal).
        assert_eq!(layer.sections as usize, summary.sections.len());
        assert!(!summary.sections.is_empty());
        assert!(summary.sections.iter().all(|s| s.source == "tubes/core"));
        let json = serde_json::to_value(&summary).expect("json");
        assert!(
            json["sections"].is_array() && json["portals"].is_array(),
            "{json}"
        );
        // Lane W-21: cell 0 plus one cell per section. This capped tube has no mouth. The chord
        // cut inside it is one cooked portal, and the file emits that cut as one quad between
        // the two indoor cells. The file beside the document is what the VisCell loader reads.
        assert_eq!(summary.viscells, dir.join("demo.viscells.ron"));
        assert_eq!(summary.vis_cells.len(), summary.sections.len() + 1);
        assert!(summary.vis_cells[0].outdoor && summary.vis_cells[0].id == 0);
        assert_eq!(summary.portals.len(), 1, "the chord cut is one cooked portal");
        assert_eq!(summary.vis_portals.len(), 1, "{:?}", summary.vis_portals);
        let cooked = &summary.portals[0];
        assert!(cooked.mouth.is_none(), "{cooked:?}");
        let [Some(sa), Some(sb)] = cooked.sides else {
            panic!("the chord cut joins two sections, got {cooked:?}");
        };
        let vis = &summary.vis_portals[0];
        assert_eq!(vis.cooked, cooked.id);
        assert!(
            (vis.a == sa + 1 && vis.b == sb + 1) || (vis.a == sb + 1 && vis.b == sa + 1),
            "{vis:?} sections {sa} {sb}"
        );
        let vis_bytes = std::fs::read(&summary.viscells).expect("viscells");
        assert!(!vis_bytes.is_empty());
        let loaded =
            regia_render::viscells::VisCells::from_ron(std::str::from_utf8(&vis_bytes).expect("utf-8"))
                .expect("viscells load");
        assert_eq!(loaded.cells.len(), summary.vis_cells.len());
        assert!(json["vis_cells"].is_array() && json["vis_portals"].is_array());
        assert_eq!(json["viscells"].as_str(), summary.viscells.to_str());
        let geom = regia_world_geom::WorldGeom::from_cooked(&bytes).expect("loads");
        assert_eq!(geom.triangles().len() as u32, layer.triangles);
        // The same input cooks to the same bytes; a bad ground and a bad document name their codes.
        let again = cook_layers(&args).expect("again");
        assert_eq!(again.content_hash, summary.content_hash);
        assert_eq!(std::fs::read(&summary.out).expect("cooked file"), bytes);
        assert_eq!(
            std::fs::read(&summary.viscells).expect("viscells again"),
            vis_bytes
        );
        let err = cook_layers(&TerrainCookArgs {
            ground_y: f32::NAN,
            ..args
        })
        .expect_err("refused");
        assert!(err.to_string().contains("ground_invalid"), "{err}");
        std::fs::write(
            &doc,
            "(version: 1, layers: [(id: \"a\", source: Gltf(path: \"a.glb\"))])",
        )
        .expect("write");
        let err = cook_layers(&TerrainCookArgs {
            document: doc,
            ground_y: 0.0,
            width_m: 512.0,
            out: None,
            json: false,
        })
        .expect_err("refused");
        assert!(err.to_string().contains("cook_not_built"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_v2_description_imports_as_a_regia_authored_file() {
        let dir = regia_test_utils::temp::unique_temp_dir("terrain-passages-v2");
        let input = dir.join("bore.json");
        let desc = r#"{"passages": [{"id": "main", "kind": "shaft", "width_m": 22.0, "ceiling_m": 12.0,
            "points": [[-60.0, -40.0, 0.0], [60.0, -38.0, 0.0]]}], "looks": {"floor_roughness": 0.7}}"#;
        std::fs::write(&input, desc).expect("write fixture");
        let summary = import(&ImportArgs {
            file: input.clone(),
            out: None,
            map_id: None,
            json: false,
            meshlayers: None,
            heightfield: None,
            world_width_m: None,
            height_scale_m: 2048.0,
            height_offset_m: 0.0,
        })
        .expect("import");
        assert_eq!(summary.out, dir.join("bore.passages.ron"));
        assert_eq!(
            (summary.map_id.as_str(), summary.passages, summary.triangle_count),
            ("bore", 1, None)
        );
        let doc = PassagesDocument::from_ron(&std::fs::read_to_string(&summary.out).expect("read"))
            .expect("parses");
        assert!(doc.sampled.is_none(), "the cook samples a description itself");
        std::fs::write(&input, r#"{"passages": []}"#).expect("write fixture");
        let err = import(&ImportArgs {
            file: input,
            out: None,
            map_id: Some("x".into()),
            json: false,
            meshlayers: None,
            heightfield: None,
            world_width_m: None,
            height_scale_m: 2048.0,
            height_offset_m: 0.0,
        })
        .expect_err("refused");
        assert!(err.to_string().contains("tunnel_description_invalid"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_input_names_its_code() {
        let dir = regia_test_utils::temp::unique_temp_dir("terrain-passages-bad");
        let input = dir.join("tunnels.json");
        std::fs::write(&input, "{\"format\": \"other/1\"}").expect("write fixture");
        let err = import(&ImportArgs {
            file: input,
            out: None,
            map_id: None,
            json: false,
            meshlayers: None,
            heightfield: None,
            world_width_m: None,
            height_scale_m: 2048.0,
            height_offset_m: 0.0,
        })
        .expect_err("refused");
        assert!(err.to_string().contains("passages_import"), "{err}");
        let authored = dir.join("a.passages.ron");
        std::fs::write(
            &authored,
            PassagesDocument::from_layout(layout()).to_ron().expect("to ron"),
        )
        .expect("write fixture");
        let err = export(&authored).expect_err("refused");
        assert!(err.to_string().contains("passages_unsampled"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_layers_missing_file_is_input_missing() {
        let dir = regia_test_utils::temp::unique_temp_dir("check-layers");
        let missing = dir.join("absent.meshlayers.ron");
        let err = run(&TerrainCommand::CheckLayers(CheckLayersArgs {
            map: missing,
            ground_y: 0.0,
            width_m: 512.0,
            json: false,
        }))
        .expect_err("missing");
        assert!(err.to_string().contains("input_missing"), "{err}");
        let cooked = dir.join("demo.meshlayers");
        std::fs::write(&cooked, []).expect("write cooked stub");
        let err = run(&TerrainCommand::CheckLayers(CheckLayersArgs {
            map: cooked,
            ground_y: 0.0,
            width_m: 512.0,
            json: true,
        }))
        .expect_err("no sibling source");
        assert!(err.to_string().contains("input_missing"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
