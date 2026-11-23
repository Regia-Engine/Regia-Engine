// SPDX-License-Identifier: MPL-2.0
//! STL mesh import (pure Rust; `book/agents/mesh-image-import.md`, ADR-3244).
//!
//! An `.stl` file (CAD and 3D-print parts) is converted into an in-memory glTF 2.0 binary and
//! cooked by the glTF importer, as OBJ is (ADR-1602). The format is the one in 3D Systems'
//! "StereoLithography Interface Specification" (1989): a list of triangles, each with a facet
//! normal and three vertices, stored as text (`solid ... endsolid`) or as binary (an 80-byte
//! header, a `u32` triangle count, then 50 bytes a triangle: normal, three vertices as
//! little-endian `f32`, two attribute bytes that are ignored).
//!
//! * **Binary or text:** a file whose length is exactly `84 + 50 * count` is binary (many binary
//!   files also start with `solid`); otherwise a file starting with `solid` is text.
//! * **Unit:** STL carries none. It is read as **millimeters** (what CAD and slicers write) unless
//!   the `.meta` `unit` says otherwise (`millimeters`, `centimeters`, `meters`, `inches`); `scale`
//!   multiplies on top.
//! * **Axis:** read as +Z up (CAD and 3D printing) and turned to +Y up; `up_axis: y` keeps the
//!   file as written.
//! * **Welding:** every triangle lists its own vertices; vertices within `weld_tolerance` (file
//!   units, default 0.0001; 0 turns welding off) are merged on a grid of that size, so two points
//!   closer than the tolerance can still stay apart when a grid line falls between them.
//!   Triangles that collapse to a line or point are dropped.
//! * **Normals:** a stored facet normal is used when it is finite and not zero; otherwise it is
//!   recomputed from the winding (counter-clockwise front face). A vertex shared by faces whose
//!   normals differ by less than `crease_angle_degrees` (default 40) gets one area-weighted
//!   smooth normal; sharper edges keep separate vertices, so a cube stays hard-edged.

use std::collections::HashMap;
use std::path::Path;

use regia_asset::AssetId;
use serde::{Deserialize, Serialize};

use crate::gltf_import::{GltfImportResult, import_gltf_converted};
use crate::gltf_write::GlbPrimitive;
use crate::lod_classes::LodThresholdInputs;
use crate::mesh_convert::{UpAxis, read_capped, sane_scale, single_primitive_glb};
use crate::texture_compress::TextureCookQuality;
use crate::{Forward, ImportError};

/// Caps on one STL import (untrusted input).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StlLimits {
    /// Largest file, in bytes (default 1 GiB).
    pub max_file_bytes: u64,
    /// Most triangles (default 20 Mi).
    pub max_triangles: usize,
}

impl Default for StlLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 1 << 30,
            max_triangles: 20 << 20,
        }
    }
}

/// The unit one STL length is in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StlUnit {
    /// 1 unit = 1 mm (the default: what CAD and slicers write).
    #[default]
    Millimeters,
    /// 1 unit = 1 cm.
    Centimeters,
    /// 1 unit = 1 m.
    Meters,
    /// 1 unit = 1 inch.
    Inches,
}

impl StlUnit {
    /// Meters in one unit.
    #[must_use]
    pub fn meters(self) -> f32 {
        match self {
            StlUnit::Millimeters => 0.001,
            StlUnit::Centimeters => 0.01,
            StlUnit::Meters => 1.0,
            StlUnit::Inches => 0.0254,
        }
    }
}

/// STL importer settings (the `.meta` `settings` string, next to the glTF keys).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StlImportSettings {
    /// What one file unit is (default millimeters).
    pub unit: StlUnit,
    /// Extra uniform scale on top of the unit (default 1).
    pub scale: f32,
    /// Which axis of the file points up (default `z`, as CAD writes it).
    pub up_axis: UpAxis,
    /// Vertices closer than this, in file units, are merged; 0 turns welding off.
    pub weld_tolerance: f32,
    /// Faces whose normals differ by less than this angle share a smooth vertex normal.
    pub crease_angle_degrees: f32,
    /// Hard caps.
    pub limits: StlLimits,
}

impl Default for StlImportSettings {
    fn default() -> Self {
        Self {
            unit: StlUnit::Millimeters,
            scale: 1.0,
            up_axis: UpAxis::Z,
            weld_tolerance: 0.0001,
            crease_angle_degrees: 40.0,
            limits: StlLimits::default(),
        }
    }
}

impl StlImportSettings {
    /// Settings from the `.meta` `settings` string; blank is the defaults, glTF keys are
    /// ignored, a malformed string is an error.
    pub fn try_from_meta_string(settings: Option<&str>) -> Result<Self, StlError> {
        match settings.filter(|s| !s.trim().is_empty()) {
            Some(s) => ron::from_str::<Self>(s).map_err(|e| StlError::Settings(e.to_string())),
            None => Ok(Self::default()),
        }
    }
}

/// Why an STL import failed. [`StlError::code`] is the agent-protocol error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum StlError {
    /// The file could not be read.
    #[error("STL file read error: {0}")]
    Io(String),
    /// The file is larger than [`StlLimits::max_file_bytes`].
    #[error("STL file is {size} bytes, more than the limit of {limit}")]
    TooLarge {
        /// File size in bytes.
        size: u64,
        /// The limit in bytes.
        limit: u64,
    },
    /// A malformed file.
    #[error("STL: {0}")]
    Parse(String),
    /// A count limit was exceeded.
    #[error("STL import limit exceeded: {0}")]
    Limit(String),
    /// The `.meta` settings string is malformed.
    #[error("STL importer settings: {0}")]
    Settings(String),
    /// The file has no triangles (or all collapsed).
    #[error("STL file has no triangles")]
    NoMesh,
}

impl StlError {
    /// The agent-protocol error code.
    pub fn code(&self) -> &'static str {
        match self {
            StlError::Io(_) => "stl_io",
            StlError::TooLarge { .. } => "stl_too_large",
            StlError::Parse(_) => "stl_parse",
            StlError::Limit(_) => "stl_limit",
            StlError::Settings(_) => "stl_bad_settings",
            StlError::NoMesh => "stl_no_mesh",
        }
    }
}

/// What an STL conversion found.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StlImportReport {
    /// `binary` or `ascii`.
    pub encoding: &'static str,
    /// Triangles in the file.
    pub triangles_in: usize,
    /// Triangles written (collapsed ones are dropped).
    pub triangles: usize,
    /// Vertices before welding (three a triangle in the file).
    pub vertices_in: usize,
    /// Vertices written.
    pub vertices: usize,
    /// Positions after welding (before the crease split).
    pub welded_positions: usize,
    /// Triangles dropped because welding collapsed them.
    pub degenerate_removed: usize,
    /// Facets whose stored normal was zero or not finite and was recomputed.
    pub normals_recomputed: usize,
    /// Problems that did not stop the import.
    pub warnings: Vec<String>,
}

/// An STL file converted to an in-memory glTF 2.0 binary.
#[derive(Clone, Debug)]
pub struct StlConversion {
    /// The GLB bytes the glTF importer cooks.
    pub glb: Vec<u8>,
    /// What the conversion found.
    pub report: StlImportReport,
}

/// One triangle as read: stored normal and three corners.
struct Facet {
    normal: [f32; 3],
    v: [[f32; 3]; 3],
}

fn finite3(v: [f32; 3]) -> bool {
    v.iter().all(|c| c.is_finite())
}

fn read_binary(bytes: &[u8], count: usize) -> Result<Vec<Facet>, StlError> {
    let mut facets = Vec::with_capacity(count.min(1 << 20));
    let f = |at: usize| {
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&bytes[at..at + 4]);
        f32::from_le_bytes(raw)
    };
    for i in 0..count {
        let base = 84 + 50 * i;
        let vec3 = |o: usize| [f(base + o), f(base + o + 4), f(base + o + 8)];
        let facet = Facet {
            normal: vec3(0),
            v: [vec3(12), vec3(24), vec3(36)],
        };
        if !facet.v.iter().all(|p| finite3(*p)) {
            return Err(StlError::Parse(format!(
                "triangle {i} has a non-finite coordinate"
            )));
        }
        facets.push(facet);
    }
    Ok(facets)
}

fn read_ascii(bytes: &[u8], limits: &StlLimits) -> Result<Vec<Facet>, StlError> {
    let text = String::from_utf8_lossy(bytes);
    let mut facets: Vec<Facet> = Vec::new();
    let mut normal: Option<[f32; 3]> = None;
    let mut corners: Vec<[f32; 3]> = Vec::new();
    let mut in_facet = false;
    let mut open_solid = false;
    for (n, raw) in text.lines().enumerate() {
        let line = n + 1;
        let mut words = raw.split_whitespace();
        let Some(keyword) = words.next() else { continue };
        let mut numbers = |what: &str| -> Result<[f32; 3], StlError> {
            let mut out = [0.0f32; 3];
            for slot in &mut out {
                let token = words
                    .next()
                    .ok_or_else(|| StlError::Parse(format!("line {line}: {what} needs three numbers")))?;
                *slot = token
                    .parse()
                    .map_err(|_| StlError::Parse(format!("line {line}: {token:?} is not a number")))?;
            }
            Ok(out)
        };
        match keyword {
            "solid" => open_solid = true,
            "endsolid" => open_solid = false,
            "facet" => {
                if in_facet {
                    return Err(StlError::Parse(format!("line {line}: facet inside a facet")));
                }
                in_facet = true;
                corners.clear();
                // `facet normal nx ny nz`: skip the word `normal`.
                let mut words_rest = raw.split_whitespace().skip(1);
                if words_rest.next() != Some("normal") {
                    return Err(StlError::Parse(format!("line {line}: expected `facet normal`")));
                }
                let mut out = [0.0f32; 3];
                for slot in &mut out {
                    let token = words_rest.next().ok_or_else(|| {
                        StlError::Parse(format!("line {line}: a normal needs three numbers"))
                    })?;
                    *slot = token
                        .parse()
                        .map_err(|_| StlError::Parse(format!("line {line}: {token:?} is not a number")))?;
                }
                normal = Some(out);
            }
            "vertex" => {
                if !in_facet {
                    return Err(StlError::Parse(format!("line {line}: vertex outside a facet")));
                }
                let p = numbers("a vertex")?;
                if !finite3(p) {
                    return Err(StlError::Parse(format!("line {line}: non-finite coordinate")));
                }
                if corners.len() >= 3 {
                    return Err(StlError::Parse(format!(
                        "line {line}: more than three vertices in a facet"
                    )));
                }
                corners.push(p);
            }
            "endfacet" => {
                if !in_facet || corners.len() != 3 {
                    return Err(StlError::Parse(format!(
                        "line {line}: a facet needs exactly three vertices (has {})",
                        corners.len()
                    )));
                }
                if facets.len() >= limits.max_triangles {
                    return Err(StlError::Limit(format!(
                        "more than {} triangles",
                        limits.max_triangles
                    )));
                }
                facets.push(Facet {
                    normal: normal.take().unwrap_or([0.0; 3]),
                    v: [corners[0], corners[1], corners[2]],
                });
                in_facet = false;
            }
            _ => {}
        }
    }
    if in_facet {
        return Err(StlError::Parse("the file ends inside a facet".into()));
    }
    if open_solid {
        return Err(StlError::Parse(
            "the file ends before `endsolid` (truncated?)".into(),
        ));
    }
    Ok(facets)
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn length(a: [f32; 3]) -> f32 {
    dot(a, a).sqrt()
}

/// One smoothing cluster at a welded position.
struct Cluster {
    /// Unit normal of the cluster's first face.
    rep: [f32; 3],
    /// Area-weighted sum of its faces' normals.
    sum: [f32; 3],
    /// The output vertex.
    out: u32,
}

/// The id of `p` among the welded positions: the one already in its grid cell, else a new one
/// (every call is a new one when `tolerance` is `None`).
fn weld_point(
    welded: &mut Vec<[f32; 3]>,
    cells: &mut HashMap<[i64; 3], u32>,
    tolerance: Option<f64>,
    p: [f32; 3],
) -> u32 {
    if let Some(tol) = tolerance {
        let key = [
            (f64::from(p[0]) / tol).round() as i64,
            (f64::from(p[1]) / tol).round() as i64,
            (f64::from(p[2]) / tol).round() as i64,
        ];
        if let Some(&id) = cells.get(&key) {
            return id;
        }
        cells.insert(key, welded.len() as u32);
    }
    welded.push(p);
    (welded.len() - 1) as u32
}

/// Converts STL bytes to an in-memory glTF 2.0 binary.
pub fn convert_stl_to_glb(bytes: &[u8], settings: &StlImportSettings) -> Result<StlConversion, StlError> {
    let _span = tracing::info_span!("stl_convert", bytes = bytes.len()).entered();
    let limits = &settings.limits;
    if bytes.len() as u64 > limits.max_file_bytes {
        return Err(StlError::TooLarge {
            size: bytes.len() as u64,
            limit: limits.max_file_bytes,
        });
    }
    let binary_count = (bytes.len() >= 84)
        .then(|| {
            let mut raw = [0u8; 4];
            raw.copy_from_slice(&bytes[80..84]);
            u32::from_le_bytes(raw) as usize
        })
        .filter(|&n| n.checked_mul(50).and_then(|b| b.checked_add(84)) == Some(bytes.len()));
    let (encoding, facets) = if let Some(count) = binary_count {
        if count > limits.max_triangles {
            return Err(StlError::Limit(format!(
                "{count} triangles (limit {})",
                limits.max_triangles
            )));
        }
        ("binary", read_binary(bytes, count)?)
    } else if bytes.trim_ascii_start().starts_with(b"solid") {
        ("ascii", read_ascii(bytes, limits)?)
    } else if bytes.len() >= 84 {
        return Err(StlError::Parse(
            "the length does not match the triangle count in the header (truncated or padded binary file)"
                .into(),
        ));
    } else {
        return Err(StlError::Parse(
            "not an STL file (too short for binary, no `solid`)".into(),
        ));
    };
    if facets.is_empty() {
        return Err(StlError::NoMesh);
    }

    let meters = settings.unit.meters() * sane_scale(settings.scale);
    let up = settings.up_axis;
    let tolerance = if settings.weld_tolerance.is_finite() && settings.weld_tolerance > 0.0 {
        Some(f64::from(settings.weld_tolerance))
    } else {
        None
    };
    let crease_cos = settings.crease_angle_degrees.clamp(0.0, 180.0).to_radians().cos();
    let mut report = StlImportReport {
        encoding,
        triangles_in: facets.len(),
        triangles: 0,
        vertices_in: facets.len() * 3,
        vertices: 0,
        welded_positions: 0,
        degenerate_removed: 0,
        normals_recomputed: 0,
        warnings: Vec::new(),
    };

    // Weld: one id per distinct grid cell (or per corner when welding is off).
    let mut welded: Vec<[f32; 3]> = Vec::new();
    let mut cells: HashMap<[i64; 3], u32> = HashMap::new();
    let mut clusters_at: Vec<Vec<u32>> = Vec::new();
    let mut clusters: Vec<Cluster> = Vec::new();
    let mut primitive = GlbPrimitive::default();
    for facet in &facets {
        let ids = facet.v.map(|p| weld_point(&mut welded, &mut cells, tolerance, p));
        if clusters_at.len() < welded.len() {
            clusters_at.resize_with(welded.len(), Vec::new);
        }
        if ids[0] == ids[1] || ids[1] == ids[2] || ids[0] == ids[2] {
            report.degenerate_removed += 1;
            continue;
        }
        let area_vec = cross(sub(facet.v[1], facet.v[0]), sub(facet.v[2], facet.v[0]));
        let area2 = length(area_vec);
        if !area2.is_finite() || area2 == 0.0 {
            report.degenerate_removed += 1;
            continue;
        }
        let computed = [area_vec[0] / area2, area_vec[1] / area2, area_vec[2] / area2];
        let stored_len = length(facet.normal);
        let normal = if finite3(facet.normal) && stored_len > 1e-6 {
            [
                facet.normal[0] / stored_len,
                facet.normal[1] / stored_len,
                facet.normal[2] / stored_len,
            ]
        } else {
            report.normals_recomputed += 1;
            computed
        };
        for id in ids {
            let found = clusters_at[id as usize]
                .iter()
                .copied()
                .find(|&c| dot(clusters[c as usize].rep, normal) >= crease_cos);
            let cluster = match found {
                Some(c) => c as usize,
                None => {
                    let out = primitive.positions.len();
                    if out >= u32::MAX as usize - 3 {
                        return Err(StlError::Limit("too many output vertices".into()));
                    }
                    let p = welded[id as usize];
                    primitive
                        .positions
                        .push(up.apply([p[0] * meters, p[1] * meters, p[2] * meters]));
                    clusters.push(Cluster {
                        rep: normal,
                        sum: [0.0; 3],
                        out: out as u32,
                    });
                    clusters_at[id as usize].push((clusters.len() - 1) as u32);
                    clusters.len() - 1
                }
            };
            let c = &mut clusters[cluster];
            for (sum, n) in c.sum.iter_mut().zip(normal) {
                *sum += n * area2;
            }
            primitive.indices.push(c.out);
        }
        report.triangles += 1;
    }
    if report.triangles == 0 {
        return Err(StlError::NoMesh);
    }
    report.welded_positions = welded.len();
    primitive.normals = vec![[0.0, 1.0, 0.0]; primitive.positions.len()];
    for c in &clusters {
        let l = length(c.sum);
        let n = if l > 0.0 && l.is_finite() {
            [c.sum[0] / l, c.sum[1] / l, c.sum[2] / l]
        } else {
            c.rep
        };
        primitive.normals[c.out as usize] = up.apply(n);
    }
    report.vertices = primitive.positions.len();
    if report.degenerate_removed > 0 {
        report.warnings.push(format!(
            "{} degenerate triangle(s) dropped",
            report.degenerate_removed
        ));
    }
    for warning in &report.warnings {
        tracing::warn!(target: "regia_asset_pipeline", "STL import: {warning}");
    }
    Ok(StlConversion {
        glb: single_primitive_glb("Regia Engine STL import", "stl", primitive)
            .map_err(|err| StlError::Parse(err.to_string()))?,
        report,
    })
}

/// Reads an `.stl` file within `limits.max_file_bytes`.
pub fn read_stl_file(path: &Path, limits: &StlLimits) -> Result<Vec<u8>, StlError> {
    match read_capped(path, limits.max_file_bytes) {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(message)) => Err(StlError::Io(message)),
        Err(size) => Err(StlError::TooLarge {
            size,
            limit: limits.max_file_bytes,
        }),
    }
}

/// An STL file imported through the glTF importer.
#[derive(Clone, Debug)]
pub struct StlImportResult {
    /// The glTF importer's result for the converted scene.
    pub gltf: GltfImportResult,
    /// The converted GLB.
    pub glb: Vec<u8>,
    /// What the conversion found.
    pub report: StlImportReport,
}

/// Imports an `.stl` from disk through the glTF importer, with the `.meta` settings string.
pub fn import_stl_path(
    source: &Path,
    parent_id: AssetId,
    forward: Forward,
    platform: &str,
    meta_settings: Option<&str>,
    texture_quality: TextureCookQuality,
) -> Result<StlImportResult, ImportError> {
    let settings = StlImportSettings::try_from_meta_string(meta_settings)?;
    let bytes = read_stl_file(source, &settings.limits)?;
    let conversion = convert_stl_to_glb(&bytes, &settings)?;
    let lod = LodThresholdInputs::resolve(&[], "", None).0;
    let gltf = import_gltf_converted(
        &conversion.glb,
        source.parent(),
        parent_id,
        forward,
        platform,
        meta_settings,
        texture_quality,
        &lod,
    )?;
    Ok(StlImportResult {
        gltf,
        glb: conversion.glb,
        report: conversion.report,
    })
}

/// The 12 triangles of a cube, `edge` file units long, with corner at the origin. Every other
/// facet carries a zero normal (to be recomputed from the winding).
fn cube_facets(edge: f32) -> Vec<Facet> {
    let c = |x: u8, y: u8, z: u8| [f32::from(x) * edge, f32::from(y) * edge, f32::from(z) * edge];
    // (normal, quad corners counter-clockwise seen from outside)
    let quads: [([f32; 3], [[u8; 3]; 4]); 6] = [
        ([0.0, 0.0, -1.0], [[0, 0, 0], [0, 1, 0], [1, 1, 0], [1, 0, 0]]),
        ([0.0, 0.0, 1.0], [[0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]]),
        ([0.0, -1.0, 0.0], [[0, 0, 0], [1, 0, 0], [1, 0, 1], [0, 0, 1]]),
        ([0.0, 1.0, 0.0], [[0, 1, 0], [0, 1, 1], [1, 1, 1], [1, 1, 0]]),
        ([-1.0, 0.0, 0.0], [[0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0]]),
        ([1.0, 0.0, 0.0], [[1, 0, 0], [1, 1, 0], [1, 1, 1], [1, 0, 1]]),
    ];
    let mut out = Vec::new();
    for (i, (normal, q)) in quads.iter().enumerate() {
        let p = |k: usize| c(q[k][0], q[k][1], q[k][2]);
        for (j, tri) in [[0, 1, 2], [0, 2, 3]].iter().enumerate() {
            out.push(Facet {
                normal: if (2 * i + j) % 2 == 0 { *normal } else { [0.0; 3] },
                v: [p(tri[0]), p(tri[1]), p(tri[2])],
            });
        }
    }
    out
}

/// A valid binary STL: a cube of `edge` file units (12 triangles, 36 corners, every other facet
/// normal zero). The header starts with `solid` on purpose (many writers do).
#[must_use]
pub fn cube_fixture_binary(edge: f32) -> Vec<u8> {
    let facets = cube_facets(edge);
    let mut out = vec![0u8; 80];
    out[..5].copy_from_slice(b"solid");
    out.extend_from_slice(&(facets.len() as u32).to_le_bytes());
    for f in &facets {
        for v in std::iter::once(&f.normal).chain(&f.v) {
            for c in v {
                out.extend_from_slice(&c.to_le_bytes());
            }
        }
        out.extend_from_slice(&[0, 0]);
    }
    out
}

/// The same cube as text.
#[must_use]
pub fn cube_fixture_ascii(edge: f32) -> String {
    let mut out = String::from("solid cube\n");
    for f in cube_facets(edge) {
        out.push_str(&format!(
            "  facet normal {} {} {}\n    outer loop\n",
            f.normal[0], f.normal[1], f.normal[2]
        ));
        for v in f.v {
            out.push_str(&format!("      vertex {} {} {}\n", v[0], v[1], v[2]));
        }
        out.push_str("    endloop\n  endfacet\n");
    }
    out.push_str("endsolid cube\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_protocol_shaped() {
        let errors = [
            StlError::Io(String::new()),
            StlError::TooLarge { size: 0, limit: 0 },
            StlError::Parse(String::new()),
            StlError::Limit(String::new()),
            StlError::Settings(String::new()),
            StlError::NoMesh,
        ];
        for error in errors {
            let code = error.code();
            assert!(code.starts_with("stl_"), "{code}");
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code}"
            );
        }
    }

    #[test]
    fn binary_and_ascii_give_the_same_mesh() {
        let s = StlImportSettings::default();
        let a = convert_stl_to_glb(&cube_fixture_binary(10.0), &s).expect("binary");
        let b = convert_stl_to_glb(cube_fixture_ascii(10.0).as_bytes(), &s).expect("ascii");
        assert_eq!(a.report.encoding, "binary");
        assert_eq!(b.report.encoding, "ascii");
        assert_eq!(a.glb, b.glb);
        assert_eq!(a.report.triangles, 12);
        assert_eq!(a.report.vertices_in, 36);
        assert_eq!(a.report.welded_positions, 8);
        assert_eq!(a.report.vertices, 24, "hard edges keep three normals a corner");
        assert_eq!(a.report.normals_recomputed, 6);
    }

    #[test]
    fn a_wide_crease_angle_welds_the_cube_to_eight() {
        let s = StlImportSettings {
            crease_angle_degrees: 180.0,
            ..StlImportSettings::default()
        };
        let c = convert_stl_to_glb(&cube_fixture_binary(10.0), &s).expect("convert");
        assert_eq!(c.report.vertices, 8);
        let off = StlImportSettings {
            weld_tolerance: 0.0,
            ..StlImportSettings::default()
        };
        let c = convert_stl_to_glb(&cube_fixture_binary(10.0), &off).expect("convert");
        assert_eq!(c.report.vertices, 36, "welding off keeps every corner");
    }

    #[test]
    fn hostile_files_are_refused_without_a_panic() {
        let s = StlImportSettings::default();
        let cube = cube_fixture_binary(1.0);
        for len in 0..cube.len() {
            let _ = convert_stl_to_glb(&cube[..len], &s);
        }
        // A header claiming a billion triangles with no body.
        let mut huge = vec![0u8; 84];
        huge[80..84].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(convert_stl_to_glb(&huge, &s).is_err());
        for text in [
            "",
            "solid x\nfacet normal 0 0 1\nvertex 0 0 0\nendfacet\nendsolid\n",
            "solid x\nfacet normal 0 0\nendsolid\n",
            "solid x\nvertex 1 2 3\nendsolid\n",
            "solid x\nfacet normal 0 0 1\nouter loop\nvertex a b c\n",
            "solid x\nendsolid x\n",
        ] {
            let e = convert_stl_to_glb(text.as_bytes(), &s).expect_err(text);
            assert!(e.code().starts_with("stl_"), "{e}");
        }
    }

    #[test]
    fn collapsed_triangles_are_dropped() {
        let text = "solid x\n\
            facet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\n\
            facet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 0 0 0.00001\nvertex 1 0 0\nendloop\nendfacet\n\
            endsolid\n";
        let c = convert_stl_to_glb(text.as_bytes(), &StlImportSettings::default()).expect("convert");
        assert_eq!(c.report.triangles, 1);
        assert_eq!(c.report.degenerate_removed, 1);
    }
}
