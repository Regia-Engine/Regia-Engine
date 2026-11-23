// SPDX-License-Identifier: MPL-2.0
//! `regia bench`: runs the public benchmark scene set headless and prints frame stats (ADR-2089).
//!
//! The set is the fetched glTF scenes of `bench/scenes.toml` (pinned files with checksums and
//! licences, `--fetch`) plus this crate's synthetic scenes (`bench/scene.rs`). Frames take the
//! runtime path at Desktop High (or `--preset`) with two frames in flight; `--on` turns opt-in
//! renderer features on for their cost (ADR-2309). `--json` prints one JSON object on
//! stdout: the report, or `{"ok": false, "code", "message"}` with an agent error code. The exit is
//! 0 on success, 2 when a request is refused and 1 when a run failed (book `agents/bench.md`).

pub mod generated;
pub mod manifest;
pub mod measure;
pub mod scene;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::Args;
use regia_render::RenderDevice;
use serde_json::{Value, json};

use generated::SceneSize;
use manifest::{FetchedScene, Manifest};
use measure::{BenchFeatures, FEATURE_NAMES, MeasureOptions, SceneStats, measure_scene};
use scene::{BenchScene, MaterialProbes, SYNTHETIC, load_gltf_with, synthetic};

/// The `schema` of the `--json` report.
pub const REPORT_SCHEMA: u32 = 1;

/// Arguments of `regia bench`.
#[derive(Debug, Clone, Args)]
pub struct BenchArgs {
    /// Scene ids (`--list` prints them). Default: every publishable scene that is on disk.
    pub scenes: Vec<String>,
    /// Print the set with licences and what is on disk, then exit.
    #[arg(long)]
    pub list: bool,
    /// Download the named scenes (default the publishable set), check them against the pinned
    /// checksums and record their licences, then exit.
    #[arg(long)]
    pub fetch: bool,
    /// Where fetched scenes live.
    #[arg(long, default_value = ".regia/bench-scenes")]
    pub assets: PathBuf,
    /// Allow local-only scenes such as `sponza_khronos`. Their numbers must not be published.
    #[arg(long)]
    pub restricted: bool,
    /// Also measure this glTF or GLB, which is not in the set: its row is marked local only.
    #[arg(long, value_name = "FILE")]
    pub gltf: Option<PathBuf>,
    /// Write each scene's last frame as `<id>.png` here, to see what was measured.
    #[arg(long, value_name = "DIR")]
    pub png_dir: Option<PathBuf>,
    /// Measured frames per scene.
    #[arg(long, default_value_t = 300)]
    pub frames: u32,
    /// Frames before the measured ones.
    #[arg(long, default_value_t = 60)]
    pub warmup: u32,
    /// Image size, `WIDTHxHEIGHT`.
    #[arg(long, default_value = "1920x1080", value_parser = parse_size)]
    pub size: (u32, u32),
    /// Hold the camera still instead of one orbit over the measured frames.
    #[arg(long)]
    pub held: bool,
    /// Build the generated scenes with counters at their small size (10,000 grass blades, 20
    /// NPCs), so a software rasterizer runs them in a test. Rows say `"size": "small"`.
    #[arg(long)]
    pub small: bool,
    /// Print one JSON object on stdout instead of the table.
    #[arg(long)]
    pub json: bool,
    /// Opt-in renderer features to turn on, comma-separated: `radiance-cache`, `blue-noise`,
    /// `oit`, `sss`, `shadow-occlusion`, `lens-effects` (their cost; the frame is no longer a preset's),
    /// and the material probes `thin-film` (a film on every non-transmissive material),
    /// `transmission` (read the glTF transmission extensions) and `rough-transmission` (that, spread
    /// by the material's roughness).
    #[arg(long, value_delimiter = ',')]
    pub on: Vec<String>,
    /// Render tier: `desktop-high` (default), `desktop-ultra`, `desktop-low`, `deck`.
    #[arg(long, default_value = "desktop-high")]
    pub preset: String,
}

/// The `--preset` names and their presets.
pub const PRESETS: [(&str, regia_render::RenderProfilePreset); 4] = [
    ("desktop-high", regia_render::RenderProfilePreset::DesktopHigh),
    ("desktop-ultra", regia_render::RenderProfilePreset::DesktopUltra),
    ("desktop-low", regia_render::RenderProfilePreset::DesktopLow),
    ("deck", regia_render::RenderProfilePreset::Deck),
];

/// The preset `--preset` names.
///
/// # Errors
///
/// `bench_bad_args` for an unknown name.
pub fn parse_preset(name: &str) -> Result<regia_render::RenderProfilePreset, BenchError> {
    PRESETS
        .iter()
        .find(|(known, _)| *known == name.trim())
        .map(|(_, preset)| *preset)
        .ok_or_else(|| {
            let known: Vec<&str> = PRESETS.iter().map(|(known, _)| *known).collect();
            BenchError::new(
                "bench_bad_args",
                format!("unknown --preset `{name}` (known: {})", known.join(", ")),
            )
        })
}

/// A bench failure with its agent error code.
#[derive(Debug, Clone)]
pub struct BenchError {
    /// Stable code (lowercase ascii, digits, `_`), documented in `book/agents/bench.md`.
    pub code: &'static str,
    /// What happened.
    pub message: String,
}

impl BenchError {
    /// A new error.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The exit status: 2 for a refused request, 1 for a failed run.
    #[must_use]
    pub fn exit_status(&self) -> u8 {
        match self.code {
            "bench_unknown_scene"
            | "bench_scene_missing"
            | "bench_scene_restricted"
            | "bench_bad_args"
            | "bench_unknown_feature" => 2,
            _ => 1,
        }
    }
}

/// A file name from a scene id: ascii letters, digits, `-` and `_`.
fn safe_stem(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn parse_size(text: &str) -> Result<(u32, u32), String> {
    let (width, height) = text
        .split_once('x')
        .ok_or_else(|| format!("{text:?} is not WIDTHxHEIGHT"))?;
    let width: u32 = width.parse().map_err(|_| format!("bad width in {text:?}"))?;
    let height: u32 = height.parse().map_err(|_| format!("bad height in {text:?}"))?;
    if width == 0 || height == 0 {
        return Err(format!("{text:?}: a size must not be zero"));
    }
    Ok((width, height))
}

/// One row of the set, fetched or synthetic.
struct SetRow<'a> {
    id: &'a str,
    label: &'a str,
    licence: &'a str,
    credit: &'a str,
    publishable: bool,
    fetched: Option<&'a FetchedScene>,
}

fn set_rows(manifest: &Manifest) -> Vec<SetRow<'_>> {
    let synthetic_rows = SYNTHETIC.iter().map(|(id, label)| SetRow {
        id,
        label,
        licence: "Regia Engine's own code",
        credit: "",
        publishable: true,
        fetched: None,
    });
    let fetched_rows = manifest.scene.iter().map(|scene| SetRow {
        id: &scene.id,
        label: &scene.label,
        licence: &scene.licence,
        credit: &scene.credit,
        publishable: scene.publishable,
        fetched: Some(scene),
    });
    synthetic_rows.chain(fetched_rows).collect()
}

/// Which rows a request names, in request order; the default is every publishable row on disk.
fn select<'a>(
    rows: &'a [SetRow<'a>],
    args: &BenchArgs,
    must_be_on_disk: bool,
) -> Result<Vec<&'a SetRow<'a>>, BenchError> {
    let on_disk = |row: &SetRow<'_>| row.fetched.is_none_or(|scene| scene.is_fetched(&args.assets));
    if args.scenes.is_empty() {
        return Ok(rows
            .iter()
            .filter(|row| row.publishable && (!must_be_on_disk || on_disk(row)))
            .collect());
    }
    args.scenes
        .iter()
        .map(|id| {
            let row = rows.iter().find(|row| row.id == id).ok_or_else(|| {
                BenchError::new("bench_unknown_scene", format!("no scene {id:?} (see --list)"))
            })?;
            if !row.publishable && !args.restricted {
                return Err(BenchError::new(
                    "bench_scene_restricted",
                    format!(
                        "{id} is under {} and local only: pass --restricted, and do not publish its numbers",
                        row.licence
                    ),
                ));
            }
            if must_be_on_disk && !on_disk(row) {
                return Err(BenchError::new(
                    "bench_scene_missing",
                    format!(
                        "{id} is not in {}: run `regia bench --fetch {id}`",
                        args.assets.display()
                    ),
                ));
            }
            Ok(row)
        })
        .collect()
}

fn list(rows: &[SetRow<'_>], args: &BenchArgs) -> Value {
    let scenes: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                "label": row.label,
                "licence": row.licence,
                "credit": row.credit,
                "publishable": row.publishable,
                "synthetic": row.fetched.is_none(),
                "on_disk": row.fetched.is_none_or(|scene| scene.is_fetched(&args.assets)),
                "bytes": row.fetched.map_or(0, |scene| scene.file.iter().map(|file| file.bytes).sum::<u64>()),
            })
        })
        .collect();
    json!({ "schema": REPORT_SCHEMA, "scenes": scenes })
}

/// Writes `LICENSES.txt` beside the scenes: what was fetched, its licence and its credit.
fn write_licences(manifest: &Manifest, assets: &Path) -> Result<(), BenchError> {
    let mut text =
        String::from("Fetched by `regia bench --fetch`; never redistribute a scene marked local only.\n\n");
    for scene in manifest.scene.iter().filter(|scene| scene.is_fetched(assets)) {
        let publish = if scene.publishable { "" } else { " [local only]" };
        text.push_str(&format!(
            "{}: {}{publish}\n  credit: {}\n  source: {}{}\n",
            scene.id, scene.licence, scene.credit, manifest.base, scene.entry
        ));
    }
    let path = assets.join("LICENSES.txt");
    std::fs::write(&path, text)
        .map_err(|err| BenchError::new("bench_fetch_failed", format!("{}: {err}", path.display())))
}

fn fetch(manifest: &Manifest, rows: &[SetRow<'_>], args: &BenchArgs) -> Result<Value, BenchError> {
    let mut report = Vec::new();
    for row in select(rows, args, false)? {
        let Some(scene) = row.fetched else {
            continue;
        };
        eprintln!("fetch {} ({}) ...", scene.id, scene.licence);
        let downloaded = scene.fetch(&manifest.base, &args.assets)?;
        report.push(json!({ "id": scene.id, "files": scene.file.len(), "downloaded": downloaded, "licence": scene.licence }));
    }
    std::fs::create_dir_all(&args.assets)
        .map_err(|err| BenchError::new("bench_fetch_failed", format!("{}: {err}", args.assets.display())))?;
    write_licences(manifest, &args.assets)?;
    Ok(json!({ "schema": REPORT_SCHEMA, "ok": true, "assets": args.assets, "fetched": report }))
}

fn load(
    row: &SetRow<'_>,
    assets: &Path,
    probes: MaterialProbes,
    size: SceneSize,
) -> Result<BenchScene, BenchError> {
    match row.fetched {
        Some(scene) => load_gltf_with(&scene.entry_path(assets), probes),
        None => synthetic(row.id, size)
            .unwrap_or_else(|| {
                Err(BenchError::new(
                    "bench_unknown_scene",
                    format!("no synthetic scene {}", row.id),
                ))
            })
            .map(|mut scene| {
                scene.apply_probes(probes);
                scene
            }),
    }
}

/// The report's fixed fields for one scene.
struct RowLabel<'a> {
    id: &'a str,
    licence: &'a str,
    publishable: bool,
}

fn scene_report(
    row: &RowLabel<'_>,
    scene: &BenchScene,
    load_s: f64,
    stats: &SceneStats,
    probes: MaterialProbes,
    small: bool,
) -> Value {
    let mut value = stats.to_json();
    if let Value::Object(map) = &mut value {
        map.insert("id".into(), json!(row.id));
        map.insert("licence".into(), json!(row.licence));
        map.insert("publishable".into(), json!(row.publishable));
        map.insert("meshes".into(), json!(scene.meshes.len()));
        map.insert("materials".into(), json!(scene.materials));
        map.insert("triangles".into(), json!(scene.triangles));
        map.insert("load_s".into(), json!((load_s * 100.0).round() / 100.0));
        if scene.extra.is_some() {
            map.insert("size".into(), json!(if small { "small" } else { "full" }));
        }
        if probes.any() {
            map.insert(
                "probed_materials".into(),
                json!({
                    "thin_film": scene.probed.thin_film,
                    "transmission": scene.probed.transmission,
                    "rough": probes.rough_transmission,
                }),
            );
        }
    }
    value
}

fn print_table(report: &Value) {
    println!(
        "adapter: {} ({}), {}, {}x{}, {} warm-up + {} frames",
        report["adapter"]["name"].as_str().unwrap_or("?"),
        report["adapter"]["driver"].as_str().unwrap_or("?"),
        report["adapter"]["backend"].as_str().unwrap_or("?"),
        report["size"][0],
        report["size"][1],
        report["warmup"],
        report["frames"],
    );
    println!(
        "{:<20} {:>10} {:>7} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "scene", "triangles", "drawn", "wall p50", "wall p99", "gpu p50", "gpu p99", "cpu p50", "fps p50"
    );
    let cell = |value: &Value| {
        value
            .as_f64()
            .map_or_else(|| "-".to_string(), |v| format!("{v:.2}"))
    };
    for scene in report["scenes"].as_array().map_or(&[][..], Vec::as_slice) {
        println!(
            "{:<20} {:>10} {:>7} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
            scene["id"].as_str().unwrap_or("?"),
            scene["triangles"],
            scene["draws"]["drawn"],
            cell(&scene["wall_ms"]["p50"]),
            cell(&scene["wall_ms"]["p99"]),
            cell(&scene["gpu_frame_ms"]["p50"]),
            cell(&scene["gpu_frame_ms"]["p99"]),
            cell(&scene["cpu_encode_ms"]["p50"]),
            cell(&scene["fps_p50"]),
        );
        if scene["publishable"] == false {
            println!("  {} is local only: do not publish this row", scene["id"]);
        }
    }
    println!(
        "ms per frame; every published number also names the card, its load and the build (docs/benchmark-scenes.md)"
    );
}

/// Runs a request and returns its report.
///
/// # Errors
/// A [`BenchError`] with its code.
pub fn execute(args: &BenchArgs) -> Result<Value, BenchError> {
    let manifest = Manifest::embedded()?;
    let rows = set_rows(&manifest);
    if args.list {
        return Ok(list(&rows, args));
    }
    if args.fetch {
        return fetch(&manifest, &rows, args);
    }
    if args.frames == 0 {
        return Err(BenchError::new("bench_bad_args", "--frames must be at least 1"));
    }
    let preset = parse_preset(&args.preset)?;
    let features = BenchFeatures::parse(&args.on).map_err(|name| {
        BenchError::new(
            "bench_unknown_feature",
            format!(
                "unknown --on feature `{name}` (known: {})",
                FEATURE_NAMES.join(", ")
            ),
        )
    })?;
    // A file alone measures just that file; the set's default does not join it.
    let chosen = if args.gltf.is_some() && args.scenes.is_empty() {
        Vec::new()
    } else {
        select(&rows, args, true)?
    };
    let device = RenderDevice::headless().map_err(|err| BenchError::new("bench_no_gpu", err.to_string()))?;
    let info = device.adapter.get_info();
    let (width, height) = args.size;
    let options = MeasureOptions {
        width,
        height,
        warmup: args.warmup,
        frames: args.frames,
        held: args.held,
        preset,
        features,
    };
    let png_for = |id: &str| {
        args.png_dir
            .as_ref()
            .map(|dir| dir.join(format!("{}.png", safe_stem(id))))
    };
    let mut scenes = Vec::with_capacity(chosen.len() + 1);
    for row in chosen {
        eprintln!("bench {} ...", row.id);
        let start = Instant::now();
        let size = if args.small {
            SceneSize::Small
        } else {
            SceneSize::Full
        };
        let scene = load(row, &args.assets, features.materials, size)?;
        let load_s = start.elapsed().as_secs_f64();
        let stats = measure_scene(&device, &scene, options, png_for(row.id).as_deref())?;
        let label = RowLabel {
            id: row.id,
            licence: row.licence,
            publishable: row.publishable,
        };
        scenes.push(scene_report(
            &label,
            &scene,
            load_s,
            &stats,
            features.materials,
            args.small,
        ));
    }
    if let Some(path) = &args.gltf {
        let stem = path
            .file_stem()
            .map_or_else(|| "scene".into(), |s| s.to_string_lossy());
        let id = format!("file:{stem}");
        eprintln!("bench {id} ...");
        let start = Instant::now();
        let scene = load_gltf_with(path, features.materials)?;
        let load_s = start.elapsed().as_secs_f64();
        let stats = measure_scene(&device, &scene, options, png_for(&id).as_deref())?;
        let label = RowLabel {
            id: &id,
            licence: "not in the public set",
            publishable: false,
        };
        scenes.push(scene_report(
            &label,
            &scene,
            load_s,
            &stats,
            features.materials,
            args.small,
        ));
    }
    Ok(json!({
        "schema": REPORT_SCHEMA,
        "ok": true,
        "engine_version": env!("CARGO_PKG_VERSION"),
        "adapter": {
            "name": info.name,
            "driver": info.driver_info,
            "backend": format!("{:?}", info.backend),
            "device_type": format!("{:?}", info.device_type),
        },
        "size": [width, height],
        "warmup": args.warmup,
        "frames": args.frames,
        "held": args.held,
        "preset": args.preset.trim(),
        "on": features.names(),
        "scenes": scenes,
    }))
}

fn print_human(args: &BenchArgs, report: &Value) {
    if args.list {
        println!(
            "{:<20} {:<9} {:<8} {:>9}  licence",
            "scene", "publish", "on disk", "MB"
        );
        for scene in report["scenes"].as_array().map_or(&[][..], Vec::as_slice) {
            let bytes = scene["bytes"].as_u64().unwrap_or(0) as f64 / 1_000_000.0;
            println!(
                "{:<20} {:<9} {:<8} {:>9.1}  {}",
                scene["id"].as_str().unwrap_or("?"),
                if scene["publishable"] == true {
                    "yes"
                } else {
                    "local"
                },
                if scene["on_disk"] == true { "yes" } else { "no" },
                bytes,
                scene["licence"].as_str().unwrap_or("?"),
            );
        }
    } else if args.fetch {
        println!(
            "fetched into {} (licences in LICENSES.txt there): {}",
            args.assets.display(),
            report["fetched"]
        );
    } else {
        print_table(report);
    }
}

/// `regia bench`: prints the result and returns the exit status.
pub fn run(args: &BenchArgs) -> ExitCode {
    match execute(args) {
        Ok(report) => {
            if args.json {
                println!("{report}");
            } else {
                print_human(args, &report);
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            if args.json {
                println!(
                    "{}",
                    json!({ "ok": false, "code": err.code, "message": err.message })
                );
            } else {
                eprintln!("error [{}]: {}", err.code, err.message);
            }
            ExitCode::from(err.exit_status())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(scenes: &[&str]) -> BenchArgs {
        BenchArgs {
            scenes: scenes.iter().map(|s| (*s).to_string()).collect(),
            list: false,
            fetch: false,
            assets: std::env::temp_dir().join("regia-bench-no-such-dir"),
            restricted: false,
            gltf: None,
            png_dir: None,
            frames: 2,
            warmup: 1,
            size: (64, 36),
            held: false,
            small: true,
            json: true,
            on: Vec::new(),
            preset: "desktop-high".to_owned(),
        }
    }

    #[test]
    fn png_names_stay_in_their_directory() {
        assert_eq!(safe_stem("file:../../etc/x"), "file_______etc_x");
        assert_eq!(safe_stem("synthetic_cubes"), "synthetic_cubes");
    }

    #[test]
    fn sizes_parse_and_refuse() {
        assert_eq!(parse_size("1920x1080"), Ok((1920, 1080)));
        for bad in ["1920", "x", "0x10", "10x0", "axb"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn refused_requests_have_stable_codes_and_exit_2() {
        let code_of = |args: BenchArgs| {
            let err = execute(&args).expect_err("must be refused");
            assert_eq!(err.exit_status(), 2, "{}", err.code);
            err.code
        };
        assert_eq!(code_of(args(&["nope"])), "bench_unknown_scene");
        assert_eq!(code_of(args(&["fox"])), "bench_scene_missing");
        assert_eq!(code_of(args(&["sponza_khronos"])), "bench_scene_restricted");
        let mut zero = args(&["synthetic_cubes"]);
        zero.frames = 0;
        assert_eq!(code_of(zero), "bench_bad_args");
        // Refused before any device opens.
        let mut feature = args(&["synthetic_cubes"]);
        feature.on = vec!["oit".to_owned(), "warp-drive".to_owned()];
        assert_eq!(code_of(feature), "bench_unknown_feature");
        let mut preset = args(&["synthetic_cubes"]);
        preset.preset = "mobile-low".to_owned();
        assert_eq!(code_of(preset), "bench_bad_args");
    }

    #[test]
    fn presets_parse() {
        assert_eq!(
            parse_preset("desktop-ultra").ok(),
            Some(regia_render::RenderProfilePreset::DesktopUltra)
        );
        assert_eq!(
            parse_preset(" desktop-high ").ok(),
            Some(regia_render::RenderProfilePreset::DesktopHigh)
        );
        assert!(parse_preset("ultra").is_err());
    }

    #[test]
    fn error_codes_are_lowercase_ascii() {
        for code in [
            "bench_unknown_scene",
            "bench_scene_missing",
            "bench_scene_restricted",
            "bench_bad_args",
            "bench_unknown_feature",
            "bench_manifest_invalid",
            "bench_fetch_failed",
            "bench_checksum_mismatch",
            "bench_load_failed",
            "bench_render_failed",
            "bench_no_gpu",
        ] {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
            assert_eq!(
                BenchError::new(code, "").exit_status() == 2,
                matches!(
                    code,
                    "bench_unknown_scene"
                        | "bench_scene_missing"
                        | "bench_scene_restricted"
                        | "bench_bad_args"
                        | "bench_unknown_feature"
                )
            );
        }
    }

    /// The default selection publishes: synthetic scenes always, fetched ones only when on disk,
    /// and the local-only scene never without being named.
    #[test]
    fn the_default_selection_is_the_publishable_scenes_on_disk() {
        let manifest = Manifest::embedded().expect("manifest");
        let rows = set_rows(&manifest);
        let request = args(&[]);
        let chosen: Vec<&str> = select(&rows, &request, true)
            .expect("selection")
            .iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(
            chosen,
            [
                "synthetic_cubes",
                "synthetic_atrium",
                "synthetic_grass_1m",
                "template_npc_200"
            ]
        );
        let all: Vec<&str> = select(&rows, &request, false)
            .expect("selection")
            .iter()
            .map(|row| row.id)
            .collect();
        assert!(all.contains(&"flight_helmet"));
        assert!(!all.contains(&"sponza_khronos"));
    }

    #[test]
    fn the_list_names_every_scene_with_its_licence() {
        let mut request = args(&[]);
        request.list = true;
        let report = execute(&request).expect("list");
        let scenes = report["scenes"].as_array().expect("scenes");
        assert!(
            scenes
                .iter()
                .any(|s| s["id"] == "sponza_khronos" && s["publishable"] == false)
        );
        assert!(
            scenes
                .iter()
                .any(|s| s["id"] == "synthetic_atrium" && s["on_disk"] == true)
        );
        assert!(
            scenes
                .iter()
                .all(|s| !s["licence"].as_str().unwrap_or("").is_empty())
        );
    }

    #[test]
    fn licences_land_beside_the_scenes() {
        let dir = regia_test_utils::unique_temp_dir("bench_licences");
        let source = dir.join("source");
        std::fs::create_dir_all(&source).expect("mkdir");
        std::fs::write(source.join("m.glb"), b"x").expect("write");
        let sha = regia_pt::sha256_file_hex(&source.join("m.glb")).expect("hash");
        // The base goes in as a TOML string the toml crate quotes: a Windows path's `\` is an
        // escape in a basic string (ADR-4924).
        let base = toml::Value::String(format!("file://{}/", source.display()));
        let text = format!(
            "version = 1\nbase = {base}\n[[scene]]\nid = \"m\"\nlabel = \"M\"\nlicence = \"CC0-1.0\"\ncredit = \"someone\"\npublishable = true\nentry = \"m.glb\"\n[[scene.file]]\npath = \"m.glb\"\nbytes = 1\nsha256 = \"{sha}\"\n"
        );
        let manifest = Manifest::parse(&text).expect("manifest");
        let assets = dir.join("assets");
        let rows = set_rows(&manifest);
        let mut request = args(&["m"]);
        request.assets = assets.clone();
        request.fetch = true;
        let report = fetch(&manifest, &rows, &request).expect("fetch");
        assert_eq!(report["fetched"][0]["downloaded"], 1);
        let licences = std::fs::read_to_string(assets.join("LICENSES.txt")).expect("read");
        assert!(licences.contains("m: CC0-1.0"), "{licences}");
        assert!(licences.contains("credit: someone"), "{licences}");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
