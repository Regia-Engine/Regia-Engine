// SPDX-License-Identifier: MPL-2.0
//! `regia atlas`: unwrap and pack a Wavefront `.obj` mesh into a lightmap UV atlas with
//! [`regia_asset_pipeline::atlas`] (ADR-1483, `book/agents/atlas.md`).
//!
//! Prints a report (charts by kind, density, utilization, warnings; `--json` for tools) and, with
//! `--out`, writes the unwrapped mesh as `.obj` with the atlas coordinates as `vt`, so the unwrap
//! can be inspected in any DCC tool.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use regia_asset_pipeline::atlas::{self, AtlasMesh, AtlasOptions, AtlasResult, ChartKind};
use serde_json::json;

/// `regia atlas` arguments.
#[derive(Debug, Args)]
pub struct AtlasArgs {
    /// The `.obj` mesh to unwrap (`v` and `f` lines; polygons are fanned into triangles).
    pub mesh: PathBuf,
    /// Atlas side in texels.
    #[arg(long, default_value_t = 1024)]
    pub resolution: u32,
    /// Gutter texels around each chart.
    #[arg(long, default_value_t = 2)]
    pub padding: u32,
    /// Fixed texel density per mesh unit; 0 searches the largest that fits.
    #[arg(long, default_value_t = 0.0)]
    pub texels_per_unit: f32,
    /// Try every position when packing (slower, tighter).
    #[arg(long, default_value_t = false)]
    pub brute_force: bool,
    /// Worker threads (0 = all cores); the result is the same for any value.
    #[arg(long, default_value_t = 0)]
    pub threads: usize,
    /// Write the unwrapped mesh here (`.obj`, atlas coordinates as `vt`, vertices split at seams).
    #[arg(long, value_name = "OBJ")]
    pub out: Option<PathBuf>,
    /// Print the report as JSON on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Positions and a triangle list read from an `.obj` file.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjMesh {
    /// `v` positions.
    pub positions: Vec<[f32; 3]>,
    /// Triangles (polygons fanned from their first corner).
    pub indices: Vec<u32>,
}

/// Parses the `v` and `f` lines of an `.obj` file (other lines are ignored). Face corners may be
/// `i`, `i/t`, `i//n` or `i/t/n`, and negative indices count back from the last vertex.
///
/// # Errors
/// A malformed `v` or `f` line, a face with fewer than three corners, or an index outside the
/// vertices read so far, with its line number.
pub fn parse_obj(text: &str) -> Result<ObjMesh> {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut indices = Vec::new();
    let mut corners = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line_no = n + 1;
        let line = raw.split('#').next().unwrap_or("").trim();
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("v") => {
                let mut p = [0.0f32; 3];
                for slot in &mut p {
                    let token = parts
                        .next()
                        .ok_or_else(|| anyhow!("line {line_no}: `v` needs three coordinates"))?;
                    *slot = token
                        .parse()
                        .map_err(|_| anyhow!("line {line_no}: {token:?} is not a number"))?;
                }
                positions.push(p);
            }
            Some("f") => {
                corners.clear();
                for token in parts {
                    let index_text = token.split('/').next().unwrap_or("");
                    let index: i64 = index_text
                        .parse()
                        .map_err(|_| anyhow!("line {line_no}: {token:?} is not a face corner"))?;
                    let count = positions.len() as i64;
                    let resolved = if index < 0 { count + index } else { index - 1 };
                    if index == 0 || resolved < 0 || resolved >= count {
                        bail!("line {line_no}: vertex {index} does not exist ({count} so far)");
                    }
                    corners.push(resolved as u32);
                }
                if corners.len() < 3 {
                    bail!("line {line_no}: a face needs at least three corners");
                }
                for k in 1..corners.len() - 1 {
                    indices.extend_from_slice(&[corners[0], corners[k], corners[k + 1]]);
                }
            }
            _ => {}
        }
    }
    Ok(ObjMesh { positions, indices })
}

/// The unwrapped mesh as `.obj` text: one `v` and one `vt` per output vertex (`vt` y up, as `.obj`
/// expects), one `f` per source face.
#[must_use]
pub fn write_obj(result: &AtlasResult, positions: &[[f32; 3]]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# regia atlas: {} charts, {}x{} texels, {} texels per unit",
        result.charts.len(),
        result.width,
        result.height,
        result.texels_per_unit
    );
    for v in &result.vertices {
        let p = positions[v.xref as usize];
        let _ = writeln!(out, "v {} {} {}", p[0], p[1], p[2]);
    }
    for v in &result.vertices {
        let _ = writeln!(out, "vt {} {}", v.uv[0], 1.0 - v.uv[1]);
    }
    let (triangles, _) = result.indices.as_chunks::<3>();
    for tri in triangles {
        let [a, b, c] = tri.map(|i| i + 1);
        let _ = writeln!(out, "f {a}/{a} {b}/{b} {c}/{c}");
    }
    out
}

/// The report as JSON.
fn report(result: &AtlasResult, faces: usize, out: Option<&Path>) -> serde_json::Value {
    let mut kinds = serde_json::Map::new();
    for kind in [
        ChartKind::Planar,
        ChartKind::Ortho,
        ChartKind::Lscm,
        ChartKind::Piecewise,
    ] {
        kinds.insert(kind.name().to_string(), json!(result.chart_count(kind)));
    }
    json!({
        "width": result.width,
        "height": result.height,
        "charts": result.charts.len(),
        "chart_kinds": kinds,
        "faces": faces,
        "vertices": result.vertices.len(),
        "texels_per_unit": result.texels_per_unit,
        "utilization": result.utilization,
        "pack_passes": result.pack_passes,
        "warnings": result
            .warnings
            .iter()
            .map(|w| json!({ "code": w.code(), "message": w.to_string() }))
            .collect::<Vec<_>>(),
        "out": out.map(|p| p.display().to_string()),
    })
}

/// Writes `bytes` to `path` through a temporary file in the same directory and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
}

/// Runs `regia atlas`.
///
/// # Errors
/// The mesh cannot be read or parsed, the generator fails (its `atlas_*` code is in the message),
/// or `--out` cannot be written.
pub fn run(args: &AtlasArgs) -> Result<()> {
    let text =
        std::fs::read_to_string(&args.mesh).with_context(|| format!("reading {}", args.mesh.display()))?;
    let mesh = parse_obj(&text).with_context(|| format!("parsing {}", args.mesh.display()))?;
    let options = AtlasOptions {
        resolution: args.resolution,
        padding: args.padding,
        texels_per_unit: args.texels_per_unit,
        brute_force: args.brute_force,
        threads: args.threads,
        ..AtlasOptions::default()
    };
    let result = atlas::generate(&AtlasMesh::new(&mesh.positions, &mesh.indices), &options)
        .map_err(|e| anyhow!("{e} ({})", e.code()))?;
    if let Some(out) = &args.out {
        write_atomic(out, write_obj(&result, &mesh.positions).as_bytes())?;
    }
    let r = report(&result, mesh.indices.len() / 3, args.out.as_deref());
    if args.json {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        println!(
            "{} charts ({}), {}x{} texels, {:.1} % used, {:.3} texels per unit, {} packing passes",
            result.charts.len(),
            [
                ChartKind::Planar,
                ChartKind::Ortho,
                ChartKind::Lscm,
                ChartKind::Piecewise
            ]
            .iter()
            .map(|k| format!("{} {}", result.chart_count(*k), k.name()))
            .collect::<Vec<_>>()
            .join(", "),
            result.width,
            result.height,
            result.utilization * 100.0,
            result.texels_per_unit,
            result.pack_passes
        );
        for w in &result.warnings {
            println!("warning [{}]: {w}", w.code());
        }
        if let Some(out) = &args.out {
            println!("wrote {}", out.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUBE_OBJ: &str = "# a cube with quads\n\
        v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\nv 1 1 1\nv 0 1 1\n\
        f 1 4 3 2\nf 5 6 7 8\nf 1 2 6 5\nf 2 3 7 6\nf 3 4 8 7\nf 4 1 5 8\n";

    #[test]
    fn obj_faces_are_fanned_and_indices_resolved() {
        let mesh = parse_obj(CUBE_OBJ).expect("cube");
        assert_eq!(mesh.positions.len(), 8);
        assert_eq!(mesh.indices.len(), 36);
        assert_eq!(&mesh.indices[..6], &[0, 3, 2, 0, 2, 1]);
        let slashes =
            parse_obj("v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nf 1/1 2//3 -1/2/3 # tail\n").expect("slashes");
        assert_eq!(slashes.indices, vec![0, 1, 2]);
    }

    #[test]
    fn obj_errors_name_the_line() {
        for (text, needle) in [
            ("v 0 0\n", "line 1"),
            ("v 0 0 x\n", "not a number"),
            ("v 0 0 0\nf 1 2 3\n", "line 2: vertex 2 does not exist"),
            ("v 0 0 0\nv 1 0 0\nf 1 2\n", "at least three"),
            ("v 0 0 0\nf 0 1 1\n", "vertex 0 does not exist"),
            ("v 0 0 0\nf a 1 1\n", "not a face corner"),
        ] {
            let error = parse_obj(text).expect_err(text).to_string();
            assert!(error.contains(needle), "{text:?}: {error}");
        }
    }

    #[test]
    fn the_command_unwraps_a_file_and_writes_an_inspectable_obj() {
        let dir = regia_test_utils::unique_temp_dir("cli_atlas");
        let input = dir.join("cube.obj");
        let output = dir.join("cube_uv2.obj");
        regia_test_utils::write_atomic(&input, CUBE_OBJ.as_bytes()).expect("write input");
        let args = AtlasArgs {
            mesh: input.clone(),
            resolution: 128,
            padding: 2,
            texels_per_unit: 0.0,
            brute_force: false,
            threads: 1,
            out: Some(output.clone()),
            json: true,
        };
        run(&args).expect("atlas");
        let written = std::fs::read_to_string(&output).expect("output");
        let back = parse_obj(&written).expect("parse output");
        assert_eq!(back.indices.len(), 36, "one triangle per source face");
        assert_eq!(back.positions.len(), 24, "each corner split per side");
        let vt: Vec<&str> = written.lines().filter(|l| l.starts_with("vt ")).collect();
        assert_eq!(vt.len(), 24);
        for line in vt {
            let uv: Vec<f32> = line[3..]
                .split_whitespace()
                .map(|t| t.parse().expect("number"))
                .collect();
            assert!(uv.iter().all(|c| (0.0..=1.0).contains(c)), "{line}");
        }
        // A missing file and a mesh with no usable triangle are errors with their cause.
        let missing = run(&AtlasArgs {
            mesh: dir.join("missing.obj"),
            out: None,
            ..args
        })
        .expect_err("missing");
        assert!(missing.to_string().contains("reading"), "{missing}");
        let flat = dir.join("flat.obj");
        regia_test_utils::write_atomic(&flat, b"v 0 0 0\nv 1 0 0\nv 2 0 0\nf 1 2 3\n").expect("write");
        let error = run(&AtlasArgs {
            mesh: flat,
            resolution: 128,
            padding: 2,
            texels_per_unit: 0.0,
            brute_force: false,
            threads: 1,
            out: None,
            json: false,
        })
        .expect_err("degenerate");
        assert!(format!("{error:#}").contains("atlas_no_triangles"), "{error:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
