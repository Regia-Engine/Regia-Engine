// SPDX-License-Identifier: MPL-2.0
//! `spawn.spline_mesh`, `road.ribbon`, `road.markings`, `terrain.flatten_road` and `road.exclusion`
//! (spec 64 §§ 6, 7.2, 7.3).
//!
//! The mesh nodes write [`GeneratedMesh`]es into the run output (world space, merged per material
//! up to 65 536 vertices) and also pass them on as `Mesh` items for inspection. In a tiled run they
//! keep what the cell owns (spec 66 § 2): road rows are inserted on every cell edge the center line
//! crosses, and each stretch between rows belongs to the cell holding its midpoint, so neighboring
//! cells share a row; a spline-mesh piece belongs to the cell holding its start point.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use regia_core::math::Vec3;
use regia_scene::spline::{SplineCurve, SplineSample};

use crate::data::{
    AttrColumn, AttrValue, Bundle, DataItem, DataKind, GroundData, KindSet, MeshData, Payload, Point,
    PointSet,
};
use crate::exec::{GeneratedMesh, NodeCtx, TerrainEditGate, TerrainEditRecord};
use crate::nodes::spline::{POLYGON, SPLINE};
use crate::nodes::{
    BuiltinNode, NodeError, POINTS, PinSpec, SettingType as T, Settings, input, num, pin_in, pin_out,
    pin_req, set,
};
use crate::polygon::PolygonData;
use crate::road::RoadProfile;

pub(crate) const GROUND: KindSet = KindSet(1 << DataKind::Ground as u16);
pub(crate) const MESH: KindSet = KindSet(1 << DataKind::Mesh as u16);

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "spawn.spline_mesh",
        "spawn",
        "Bends each point's `mesh` along the spline from `distance_m` over `length_m` into generated meshes",
        &[
            set("forward_axis", T::Enum(&["X", "Z"]), "X", "The mesh's length axis"),
            num("segments_per_m", T::Float, "1.0", 0.0, 100.0, "Extra subdivision along the axis (0 = source vertices)"),
            set("smooth_normals", T::Bool, "true", "Share normals across seams between consecutive pieces"),
            set("uv_along", T::Enum(&["none", "meters", "stretch"]), "none", "V along the path"),
            set("material", T::Asset, "\"\"", "Material of the generated meshes"),
            set("collision", T::Enum(&["none", "mesh"]), "none", "`mesh` builds a static mesh collider per generated mesh"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("in", POINTS), pin_req("spline", SPLINE)],
                vec![pin_out("out", MESH)],
            )
        },
        eval_spline_mesh,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "road.ribbon",
        "road",
        "Carriageway and shoulders as one ribbon mesh per road: rows along the road, crowned, never under ground + lift",
        &[
            set("profile", T::Any, "\"\"", "Fallback profile: a `.road.ron` path or an inline RoadProfile(...)"),
            num("smooth_rows", T::Int, "3", 1.0, 15.0, "Moving average over this many rows (odd)"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("splines", SPLINE), pin_in("ground", GROUND)],
                vec![pin_out("out", MESH), pin_out("out_edges", POINTS)],
            )
        },
        eval_ribbon,
    )
    .uncached()
    .world()
    .checked(check_smooth_rows),
    BuiltinNode::new(
        "road.pieces",
        "road",
        "The profile's edge pieces: each row's mesh repeated along the road at its side and offset, bent to the curve and laid on the road surface",
        &[
            set("profile", T::Any, "\"\"", "Fallback profile: a `.road.ron` path or an inline RoadProfile(...)"),
            num("smooth_rows", T::Int, "3", 1.0, 15.0, "Moving average over this many rows, as `road.ribbon` (odd)"),
            set("forward_axis", T::Enum(&["X", "Z"]), "X", "The mesh's length axis"),
            num("segments_per_m", T::Float, "1.0", 0.0, 100.0, "Extra subdivision along the axis (0 = source vertices)"),
            set("material", T::Asset, "\"\"", "Material of the generated mesh"),
            set("collision", T::Enum(&["none", "mesh"]), "none", "`mesh` builds a static mesh collider per generated mesh"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("splines", SPLINE), pin_in("ground", GROUND)],
                vec![pin_out("out", MESH)],
            )
        },
        eval_pieces,
    )
    .uncached()
    .world()
    .checked(check_smooth_rows),
    BuiltinNode::new(
        "road.markings",
        "road",
        "Marking strips (solid or dash(on_m, off_m)) 5 mm over each road's ribbon surface, one generated mesh per road",
        &[
            set("profile", T::Any, "\"\"", "Fallback profile: a `.road.ron` path or an inline RoadProfile(...)"),
            num("smooth_rows", T::Int, "3", 1.0, 15.0, "Must match the ribbon's smoothing (odd)"),
            set("material", T::Asset, "\"\"", "Marking material"),
        ],
        |_| {
            PinSpec::new(
                vec![pin_req("splines", SPLINE), pin_in("ground", GROUND)],
                vec![pin_out("out", MESH)],
            )
        },
        eval_markings,
    )
    .uncached()
    .world()
    .checked(check_smooth_rows),
    BuiltinNode::new(
        "terrain.flatten_road",
        "road",
        "Bake only: a terrain edit flattening the ground under each road (profile `terrain` limits); passes the splines on",
        &[set("profile", T::Any, "\"\"", "Fallback profile: a `.road.ron` path or an inline RoadProfile(...)")],
        |_| {
            PinSpec::new(
                vec![pin_req("splines", SPLINE), pin_req("ground", GROUND)],
                vec![pin_out("out", SPLINE)],
            )
        },
        eval_flatten,
    )
    .uncached()
    .world(),
    BuiltinNode::new(
        "road.exclusion",
        "road",
        "A polygon per road covering carriageway, shoulders and flatten_extra_m (+ extra_m)",
        &[
            set("profile", T::Any, "\"\"", "Fallback profile: a `.road.ron` path or an inline RoadProfile(...)"),
            num("extra_m", T::Float, "0.0", 0.0, 1.0e3, "Added to the profile's flatten_extra_m"),
        ],
        |_| PinSpec::new(vec![pin_req("splines", SPLINE)], vec![pin_out("out", POLYGON)]),
        eval_exclusion,
    )
    .reads_world_data(),
];

pub(crate) fn check_smooth_rows(s: &Settings) -> Vec<(String, String)> {
    if s.i("smooth_rows") % 2 == 0 {
        vec![("setting_invalid".into(), "`smooth_rows` must be odd".into())]
    } else {
        Vec::new()
    }
}

/// The profile of a spline item: item scope `road_profile`, else the node's `profile` setting.
pub(crate) fn profile_for(
    ctx: &NodeCtx<'_>,
    item: &DataItem,
    s: &Settings,
) -> Result<RoadProfile, NodeError> {
    let from_path = |path: &str| ctx.world().road_profile(path).cloned();
    if let Some(AttrValue::Asset(p) | AttrValue::Name(p)) = item.scope.get("road_profile")
        && let Some(prof) = from_path(p)
    {
        return Ok(prof);
    }
    match s.get("profile") {
        Some(v @ crate::value::SettingValue::Record(..)) => {
            RoadProfile::from_value(v).map_err(|e| NodeError::new("setting_invalid", e))
        }
        Some(v) => match v.as_text().filter(|t| !t.is_empty()) {
            Some(path) => from_path(path).ok_or_else(|| {
                NodeError::new(
                    "road_profile_missing",
                    format!("road profile `{path}` was not captured"),
                )
            }),
            None => Err(NodeError::new(
                "road_profile_missing",
                "the spline has no road_profile and the node has no profile",
            )),
        },
        None => Err(NodeError::new("road_profile_missing", "no profile")),
    }
}

/// A frame's right vector flattened onto XZ (unit), for road cross sections.
pub(crate) fn flat_right(s: &SplineSample) -> Vec3 {
    let r = Vec3::new(s.right.x, 0.0, s.right.z);
    let l = r.length();
    if l > 1e-6 {
        r / l
    } else {
        let t = Vec3::new(s.tangent.x, 0.0, s.tangent.z).normalize_or(Vec3::X);
        Vec3::new(-t.z, 0.0, t.x)
    }
}

/// Splits triangles whose extent along `axis` exceeds `max_len` (longest axis edge first), sharing
/// new mid-edge vertices; deterministic in triangle order.
fn subdivide(
    positions: &mut Vec<[f32; 3]>,
    normals: &mut Vec<[f32; 3]>,
    indices: &[u32],
    axis: usize,
    max_len: f32,
) -> Vec<u32> {
    let mut mids: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    let mut work: Vec<([u32; 3], u32)> = indices.as_chunks::<3>().0.iter().map(|t| (*t, 0)).collect();
    work.reverse();
    let mut out = Vec::with_capacity(indices.len());
    while let Some((t, depth)) = work.pop() {
        let span = |a: u32, b: u32| (positions[a as usize][axis] - positions[b as usize][axis]).abs();
        let edges = [(t[0], t[1], t[2]), (t[1], t[2], t[0]), (t[2], t[0], t[1])];
        let (a, b, c) = edges
            .iter()
            .copied()
            .max_by(|x, y| span(x.0, x.1).total_cmp(&span(y.0, y.1)))
            .unwrap_or((t[0], t[1], t[2]));
        if depth >= 10 || span(a, b) <= max_len {
            out.extend_from_slice(&t);
            continue;
        }
        let key = (a.min(b), a.max(b));
        let m = match mids.get(&key) {
            Some(m) => *m,
            None => {
                let (pa, pb) = (positions[a as usize], positions[b as usize]);
                let (na, nb) = (normals[a as usize], normals[b as usize]);
                positions.push(std::array::from_fn(|k| (pa[k] + pb[k]) * 0.5));
                normals.push(std::array::from_fn(|k| (na[k] + nb[k]) * 0.5));
                let m = (positions.len() - 1) as u32;
                mids.insert(key, m);
                m
            }
        };
        // Children in reverse so the first one is processed next.
        work.push(([m, b, c], depth + 1));
        work.push(([a, m, c], depth + 1));
    }
    out
}

fn eval_spline_mesh(
    ctx: &mut NodeCtx<'_>,
    inputs: &[Bundle],
    s: &Settings,
) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.spline_mesh").entered();
    let splines_b = input(inputs, 1);
    let curves: Vec<SplineCurve> = splines_b
        .0
        .iter()
        .filter_map(|i| match &i.payload {
            Payload::Spline(sd) => Some(sd.curve()),
            _ => None,
        })
        .collect();
    let axis_z = s.text("forward_axis") == "Z";
    let per_m = s.f("segments_per_m") as f32;
    let uv_mode = s.text("uv_along").to_string();
    let material = s.text("material").to_string();
    let opts = BendOpts {
        axis_z,
        per_m,
        uv_mode: &uv_mode,
        material: &material,
        collision: s.text("collision") == "mesh",
        surface: None,
    };
    let tile = ctx.tile();
    let mut built: Vec<GeneratedMesh> = Vec::new();
    for (k, item) in input(inputs, 0).0.iter().enumerate() {
        let Some(points) = item.as_points() else {
            continue;
        };
        let Some(curve) = curves.get(k).or(curves.first()) else {
            return Err(NodeError::new(
                "spline_missing",
                "spawn.spline_mesh needs a spline item",
            ));
        };
        let col_f = |name: &str| match points.attrs.get(name) {
            Some(AttrColumn::Float(v)) => Some(v.clone()),
            _ => None,
        };
        let (Some(starts), Some(lengths)) = (col_f("distance_m"), col_f("length_m")) else {
            return Err(NodeError::new(
                "attr_missing",
                "points need `distance_m` and `length_m` (from spline.modules)",
            ));
        };
        let meshes = match points.attrs.get("mesh") {
            Some(AttrColumn::Asset(v) | AttrColumn::Name(v)) => v.clone(),
            _ => return Err(NodeError::new("attr_missing", "points need a `mesh` attribute")),
        };
        let mut pieces: Vec<(f64, GeneratedMesh)> = Vec::new();
        for ((mesh, start), length) in meshes.iter().zip(&starts).zip(&lengths) {
            ctx.check_cancel()?;
            let Some(src) = ctx.world().mesh(mesh) else {
                ctx.warn("mesh_missing", format!("mesh `{mesh}` was not captured"));
                continue;
            };
            pieces.push((*start, bend(&src, curve, *start, *length, &opts)?));
        }
        let (at, mut pieces): (Vec<f64>, Vec<GeneratedMesh>) = pieces.into_iter().unzip();
        if s.b("smooth_normals") {
            share_seam_normals(&mut pieces);
        }
        // After the seams: both sides of a cell edge get the shared normals.
        for (start, p) in at.into_iter().zip(pieces) {
            let origin = curve.at_distance_f64(start).position;
            if tile.is_none_or(|t| t.owns(f64::from(origin.x), f64::from(origin.z))) {
                GeneratedMesh::merge_into(&mut built, p);
            }
        }
    }
    let mut out = Bundle::empty();
    for m in built {
        out.0.push(DataItem::new(Payload::Mesh(Arc::new(MeshData {
            asset: String::new(),
            positions: m.positions.clone(),
            normals: m.normals.clone(),
            indices: m.indices.clone(),
        }))));
        // The run output takes the built mesh itself (one copy fewer per vertex).
        GeneratedMesh::merge_into(&mut ctx.output().meshes, m);
    }
    Ok(vec![out])
}

/// The `spawn.spline_mesh` settings one piece needs.
#[derive(Clone, Copy)]
struct BendOpts<'a> {
    axis_z: bool,
    per_m: f32,
    uv_mode: &'a str,
    material: &'a str,
    collision: bool,
    /// Lay the mesh on a road surface at this sideways offset instead of on the spline's height.
    surface: Option<(&'a RoadSurface, f32)>,
}

/// One module mesh bent along `curve` from `start` over `length` (spec 64 § 6).
///
/// # Errors
///
/// `mesh_bad_index` when a triangle index is past the vertex buffer. `sample.mesh_surface`
/// rejects the same mesh; bending it used to panic inside `subdivide`.
fn bend(
    src: &MeshData,
    curve: &SplineCurve,
    start: f64,
    length: f64,
    opts: &BendOpts<'_>,
) -> Result<GeneratedMesh, NodeError> {
    if let Some(&index) = src.indices.iter().find(|&&i| i as usize >= src.positions.len()) {
        return Err(NodeError::new(
            "mesh_bad_index",
            format!(
                "`{}` index {index} is past its {} vertices",
                src.asset,
                src.positions.len()
            ),
        ));
    }
    let BendOpts {
        axis_z,
        per_m,
        uv_mode,
        material,
        collision,
        surface,
    } = *opts;
    let axis = if axis_z { 2 } else { 0 };
    let up_normals;
    let normals_src: &[[f32; 3]] = if src.normals.len() == src.positions.len() {
        &src.normals
    } else {
        up_normals = vec![[0.0, 1.0, 0.0]; src.positions.len()];
        &up_normals
    };
    let (lo, hi) = src.bounds();
    let (s_min, s_max) = (lo[axis], hi[axis]);
    let extent = (s_max - s_min).max(1e-9);
    // Only subdivision adds vertices; otherwise the source buffers are read in place.
    let (positions, normals, indices) = if per_m > 0.0 && length > 0.0 {
        let mut positions = src.positions.clone();
        let mut normals = normals_src.to_vec();
        // Source units per output meter.
        let max_len = extent / (length as f32 * per_m).max(1.0);
        let indices = subdivide(&mut positions, &mut normals, &src.indices, axis, max_len);
        (Cow::Owned(positions), Cow::Owned(normals), indices)
    } else {
        (
            Cow::Borrowed(src.positions.as_slice()),
            Cow::Borrowed(normals_src),
            src.indices.clone(),
        )
    };
    let mut out = GeneratedMesh {
        material: material.to_string(),
        collision,
        positions: Vec::with_capacity(positions.len()),
        normals: Vec::with_capacity(positions.len()),
        uvs: Vec::with_capacity(positions.len()),
        ..GeneratedMesh::default()
    };
    // Vertices with the same axis coordinate map to the same distance, and `at_distance_f64` is a
    // pure function of it: each distinct coordinate samples the curve once (a ring of a piece, the
    // two sides of a strip), with the exact sample a per-vertex call returns (ADR-7238).
    let mut sample_of: HashMap<u32, usize> = HashMap::new();
    let mut samples: Vec<SplineSample> = Vec::new();
    for (p, n) in positions.iter().zip(normals.iter()) {
        let fr = (p[axis] - s_min) / extent;
        let f = f64::from(fr);
        let d = start + f * length;
        let k = *sample_of.entry(fr.to_bits()).or_insert_with(|| {
            samples.push(curve.at_distance_f64(d));
            samples.len() - 1
        });
        let sm = &samples[k];
        // Side coordinate along the frame's right, height along up.
        let (side, height, ns, na) = if axis_z {
            (-p[0], p[1], -n[0], n[2])
        } else {
            (p[2], p[1], n[2], n[0])
        };
        let mut pos = sm.position + sm.right * (side * sm.scale.x) + sm.up * (height * sm.scale.y);
        let mut nrm = (sm.right * ns + sm.up * n[1] + sm.tangent * na).normalize_or(sm.up);
        if let Some((road, offset)) = surface {
            // Pieces sit on the ribbon: sideways along flat right, then the surface height.
            // The rolled frame stays for spline meshes (`surface: None`) only.
            let right = flat_right(sm);
            pos = sm.position + right * (side * sm.scale.x);
            // The piece sits `offset * scale.x` out (the width scale moves it), so the surface is
            // sampled there, not at the unscaled offset.
            pos.y = road.height_at(d, offset * sm.scale.x) + height * sm.scale.y;
            nrm = (right * ns + Vec3::Y * n[1] + sm.tangent * na).normalize_or(Vec3::Y);
        }
        out.positions.push(pos.to_array());
        out.normals.push(nrm.to_array());
        let v = match uv_mode {
            "meters" => d as f32,
            "stretch" => f as f32,
            _ => 0.0,
        };
        out.uvs.push([if uv_mode == "none" { 0.0 } else { side }, v]);
    }
    out.indices = indices;
    Ok(out)
}

/// Averages the normals of vertices at the same position in consecutive pieces (bit-equal positions).
fn share_seam_normals(pieces: &mut [GeneratedMesh]) {
    for k in 1..pieces.len() {
        let (head, tail) = pieces.split_at_mut(k);
        let (a, b) = (&mut head[k - 1], &mut tail[0]);
        let mut at: BTreeMap<[u32; 3], Vec<usize>> = BTreeMap::new();
        for (i, p) in a.positions.iter().enumerate() {
            at.entry(p.map(f32::to_bits)).or_default().push(i);
        }
        for (j, p) in b.positions.iter().enumerate() {
            if let Some(list) = at.get(&p.map(f32::to_bits)) {
                for &i in list {
                    let s: [f32; 3] = std::array::from_fn(|c| a.normals[i][c] + b.normals[j][c]);
                    let n = Vec3::from(s).normalize_or(Vec3::Y).to_array();
                    a.normals[i] = n;
                    b.normals[j] = n;
                }
            }
        }
    }
}

/// Most evenly spaced ribbon rows one road makes (beyond it the rows get coarser).
const MAX_RIBBON_ROWS: usize = 1_000_000;

/// Row distances of a road ribbon: every `1 / samples_per_m` (at most 4 m apart) plus every control
/// point, both ends included, sorted and deduplicated. With a tier's `cell_m`, also a row on every
/// cell edge the center line crosses, on the whole grid so every cell computes the same rows.
fn ribbon_rows(curve: &SplineCurve, samples_per_m: f32, cell_m: Option<f64>) -> Vec<f64> {
    let len = curve.length_f64();
    let step = 1.0 / f64::from(samples_per_m.max(0.25));
    // A road of astronomical length saturates the count; cap it so the rows stay allocatable.
    let n = ((len / step).ceil().max(1.0) as usize).min(MAX_RIBBON_ROWS);
    let mut rows: Vec<f64> = (0..=n).map(|k| len * k as f64 / n as f64).collect();
    rows.extend(curve.segment_starts().into_iter().map(f64::from));
    rows.sort_by(f64::total_cmp);
    rows.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    if let Some(c) = cell_m {
        let mut split = Vec::with_capacity(rows.len());
        for w in rows.windows(2) {
            split.push(w[0]);
            cell_edge_rows(curve, c, (w[0], w[1]), 0, &mut split);
        }
        split.extend(rows.last());
        split.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
        rows = split;
    }
    rows
}

/// The grid cell of the center line at distance `d`.
fn row_cell(curve: &SplineCurve, c: f64, d: f64) -> crate::tiles::CellCoord {
    let p = curve.at_distance_f64(d).position;
    <crate::tiles::CellCoord as crate::tiles::TileCell>::containing(f64::from(p.x), f64::from(p.z), c)
}

/// Pushes the distances (bisected to 10 µm) where the center line changes cell inside `(a, b)`.
fn cell_edge_rows(curve: &SplineCurve, c: f64, (a, b): (f64, f64), depth: u32, out: &mut Vec<f64>) {
    if row_cell(curve, c, a) == row_cell(curve, c, b) {
        return;
    }
    if b - a <= 1e-5 || depth >= 40 {
        out.push(b);
        return;
    }
    let m = (a + b) * 0.5;
    cell_edge_rows(curve, c, (a, m), depth + 1, out);
    cell_edge_rows(curve, c, (m, b), depth + 1, out);
}

/// Whether the road stretch between distances `a` and `b` belongs to the run's cell.
fn stretch_owned(curve: &SplineCurve, tile: Option<crate::exec::TileFrame>, a: f64, b: f64) -> bool {
    tile.is_none_or(|t| {
        let p = curve.at_distance_f64((a + b) * 0.5).position;
        t.owns(f64::from(p.x), f64::from(p.z))
    })
}

/// The key a road's report line goes under: its entity, else its position in the input.
pub(crate) fn road_key(item: &DataItem, index: usize) -> String {
    match item.scope.get("entity") {
        Some(AttrValue::Name(g)) => g.clone(),
        _ => format!("#{index}"),
    }
}

/// A road's surface: ribbon rows, cross-section offsets and the crowned center height per row.
pub(crate) struct RoadSurface {
    pub(crate) rows: Vec<f64>,
    pub(crate) offsets: Vec<f32>,
    pub(crate) xz: Vec<Vec<Vec3>>,
    pub(crate) center: Vec<f32>,
    slope: f32,
}

impl RoadSurface {
    pub(crate) fn new(
        curve: &SplineCurve,
        prof: &RoadProfile,
        ground: Option<&GroundData>,
        smooth: usize,
        cell_m: Option<f64>,
    ) -> Self {
        let w = prof.width_m * 0.5;
        let (sl, sr) = prof.shoulders_m;
        // Cross-section offsets, left (negative) to right.
        let mut offsets = Vec::new();
        if sl > 0.0 {
            offsets.push(-(w + sl));
        }
        offsets.extend([-w, 0.0, w]);
        if sr > 0.0 {
            offsets.push(w + sr);
        }
        let slope = prof.cross_slope_pct / 100.0;
        let rows = ribbon_rows(curve, prof.samples_per_m, cell_m);
        let xz: Vec<Vec<Vec3>> = rows
            .iter()
            .map(|d| {
                let sm = curve.at_distance_f64(*d);
                let r = flat_right(&sm);
                offsets.iter().map(|o| sm.position + r * *o).collect()
            })
            .collect();
        let ground_at = |p: Vec3| ground.map_or(p.y, |g| g.height(p.x, p.z));
        // The lowest center height per row that keeps every vertex at least `lift` over the ground.
        let need: Vec<f32> = xz
            .iter()
            .map(|row| {
                row.iter()
                    .zip(&offsets)
                    .map(|(p, o)| ground_at(*p) + prof.surface_lift_m + o.abs() * slope)
                    .fold(f32::NEG_INFINITY, f32::max)
            })
            .collect();
        // Smooth along the road, then never below what each row needs.
        let half = smooth / 2;
        let center = (0..need.len())
            .map(|i| {
                let (a, b) = (i.saturating_sub(half), (i + half).min(need.len() - 1));
                let avg = need[a..=b].iter().sum::<f32>() / (b - a + 1) as f32;
                avg.max(need[i])
            })
            .collect();
        Self {
            rows,
            offsets,
            xz,
            center,
            slope,
        }
    }

    /// The crown drop at a sideways offset.
    pub(crate) fn drop(&self, o: f32) -> f32 {
        o.abs() * self.slope
    }

    /// The surface height at distance `d` and sideways offset `o` (rows interpolated linearly).
    fn height_at(&self, d: f64, o: f32) -> f32 {
        // A road shorter than the row dedup tolerance has one row: its height is that row's,
        // instead of an index past the end (audit wave 5, procgen-g2 Low).
        if self.rows.len() < 2 || self.center.len() < 2 {
            return self.center.first().copied().unwrap_or(0.0) - self.drop(o);
        }
        let j = self
            .rows
            .partition_point(|r| *r <= d)
            .clamp(1, self.rows.len().max(2) - 1);
        let (r0, r1) = (self.rows[j - 1], self.rows[j]);
        let f = if r1 > r0 {
            ((d - r0) / (r1 - r0)).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        self.center[j - 1] + (self.center[j] - self.center[j - 1]) * f - self.drop(o)
    }
}

/// `[start, end)` intervals of a marking pattern (`solid` or `dash(on_m, off_m)`) over `[0, len]`.
fn marking_intervals(pattern: &str, len: f64) -> Result<Vec<(f64, f64)>, NodeError> {
    let p = pattern.trim();
    if p == "solid" {
        return Ok(vec![(0.0, len)]);
    }
    let bad = || {
        NodeError::new(
            "setting_invalid",
            format!("marking pattern `{p}` is not solid or dash(on_m, off_m)"),
        )
    };
    let inner = p
        .strip_prefix("dash(")
        .and_then(|r| r.strip_suffix(')'))
        .ok_or_else(bad)?;
    let mut parts = inner.split(',').map(|x| x.trim().parse::<f64>());
    let (Some(Ok(on)), Some(Ok(off)), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad());
    };
    if !(on > 0.0 && off >= 0.0 && on.is_finite() && off.is_finite()) {
        return Err(bad());
    }
    let period = on + off;
    let n = (len / period).ceil() as usize;
    if n > 1_000_000 {
        return Err(NodeError::new(
            "limit_exceeded",
            "a marking pattern makes more than a million dashes",
        ));
    }
    Ok((0..n)
        .map(|k| (period * k as f64, (period * k as f64 + on).min(len)))
        .filter(|(a, b)| b > a)
        .collect())
}

fn eval_markings(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.road_markings").entered();
    let ground_b = input(inputs, 1);
    let ground: Option<Arc<GroundData>> = ground_b.0.iter().find_map(|i| match &i.payload {
        Payload::Ground(g) => Some(g.clone()),
        _ => None,
    });
    let smooth = (s.i("smooth_rows").clamp(1, 15) as usize) | 1;
    let tile = ctx.tile();
    let cell_m = tile.map(|t| t.cell_m);
    let mut meshes: Vec<GeneratedMesh> = Vec::new();
    for item in &input(inputs, 0).0 {
        ctx.check_cancel()?;
        let Payload::Spline(sd) = &item.payload else {
            continue;
        };
        let prof = profile_for(ctx, item, s)?;
        let curve = sd.curve();
        let len = curve.length_f64();
        if !crate::nodes::common::has_length(len) || prof.markings.is_empty() {
            continue;
        }
        let surf = RoadSurface::new(&curve, &prof, ground.as_deref(), smooth, cell_m);
        let mut mesh = GeneratedMesh {
            material: s.text("material").to_string(),
            ..GeneratedMesh::default()
        };
        // Per quad: whether the run's cell owns it.
        let mut owned: Vec<bool> = Vec::new();
        for m in &prof.markings {
            let hw = m.width_m * 0.5;
            let color = [m.color[0], m.color[1], m.color[2], 1.0];
            for (a, b) in marking_intervals(&m.pattern, len)? {
                let mut ds: Vec<f64> = vec![a];
                ds.extend(surf.rows.iter().copied().filter(|r| *r > a && *r < b));
                ds.push(b);
                let base = mesh.positions.len() as u32;
                for d in &ds {
                    let sm = curve.at_distance_f64(*d);
                    let r = flat_right(&sm);
                    for o in [m.offset_m - hw, m.offset_m + hw] {
                        let p = sm.position + r * o;
                        mesh.positions
                            .push([p.x, surf.height_at(*d, o) + MARKING_LIFT_M, p.z]);
                        mesh.uvs
                            .push([if o > m.offset_m { 1.0 } else { 0.0 }, (*d - a) as f32]);
                        mesh.colors.push(color);
                    }
                }
                for k in 0..ds.len().saturating_sub(1) as u32 {
                    let (l0, r0, l1, r1) =
                        (base + 2 * k, base + 2 * k + 1, base + 2 * k + 2, base + 2 * k + 3);
                    mesh.indices.extend([l0, r0, l1, r0, r1, l1]);
                    owned.push(stretch_owned(&curve, tile, ds[k as usize], ds[k as usize + 1]));
                }
            }
        }
        mesh.normals = vertex_normals(&mesh.positions, &mesh.indices);
        if tile.is_some() {
            mesh.keep_triangles(|t| owned[t / 2]);
        }
        if !mesh.indices.is_empty() {
            GeneratedMesh::merge_into(&mut meshes, mesh);
        }
    }
    let mut out = Bundle::empty();
    for m in &meshes {
        out.0.push(DataItem::new(Payload::Mesh(Arc::new(MeshData {
            asset: String::new(),
            positions: m.positions.clone(),
            normals: m.normals.clone(),
            indices: m.indices.clone(),
        }))));
        GeneratedMesh::merge_into(&mut ctx.output().meshes, m.clone());
    }
    Ok(vec![out])
}

/// Markings sit this far over the ribbon surface, meters.
const MARKING_LIFT_M: f32 = 0.005;

fn eval_ribbon(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.road_ribbon").entered();
    let ground_b = input(inputs, 1);
    let ground: Option<Arc<GroundData>> = ground_b.0.iter().find_map(|i| match &i.payload {
        Payload::Ground(g) => Some(g.clone()),
        _ => None,
    });
    let smooth = (s.i("smooth_rows").clamp(1, 15) as usize) | 1;
    let tile = ctx.tile();
    let cell_m = tile.map(|t| t.cell_m);
    let mut meshes: Vec<GeneratedMesh> = Vec::new();
    let mut edges = PointSet::new();
    let (mut edge_side, mut edge_dist) = (Vec::new(), Vec::new());
    for (index, item) in input(inputs, 0).0.iter().enumerate() {
        ctx.check_cancel()?;
        let Payload::Spline(sd) = &item.payload else {
            continue;
        };
        let prof = profile_for(ctx, item, s)?;
        let curve = sd.curve();
        if !crate::nodes::common::has_length(f64::from(curve.length())) {
            ctx.warn(
                "spline_degenerate",
                "a road spline with no length makes no ribbon",
            );
            continue;
        }
        ctx.road_report(&road_key(item, index)).length_m = curve.length();
        let surf = RoadSurface::new(&curve, &prof, ground.as_deref(), smooth, cell_m);
        let w = prof.width_m * 0.5;
        let (rows, offsets, xz, center) = (&surf.rows, &surf.offsets, &surf.xz, &surf.center);
        let drop = |o: f32| surf.drop(o);
        let cols = offsets.len();
        let mut mesh = GeneratedMesh {
            material: prof.material.clone(),
            ..GeneratedMesh::default()
        };
        for (r, row) in xz.iter().enumerate() {
            for (c, p) in row.iter().enumerate() {
                let o = offsets[c];
                mesh.positions.push([p.x, center[r] - drop(o), p.z]);
                mesh.uvs.push([
                    (o + w) / (2.0 * w),
                    (rows[r] / f64::from(prof.uv_m_per_tile)) as f32,
                ]);
            }
            for (side, c) in [(-1i64, 0usize), (1, cols - 1)] {
                let p = mesh.positions[r * cols + c];
                edges.push(Point {
                    position: p,
                    seed: ctx.point_seed(p),
                    ..Point::default()
                });
                edge_side.push(side);
                edge_dist.push(rows[r]);
            }
        }
        for r in 0..rows.len().saturating_sub(1) {
            for c in 0..cols - 1 {
                let i = |rr: usize, cc: usize| (rr * cols + cc) as u32;
                mesh.indices.extend([
                    i(r, c),
                    i(r, c + 1),
                    i(r + 1, c),
                    i(r, c + 1),
                    i(r + 1, c + 1),
                    i(r + 1, c),
                ]);
            }
        }
        mesh.normals = vertex_normals(&mesh.positions, &mesh.indices);
        ctx.check_points(edges.len())?;
        let quads = cols - 1;
        if tile.is_some() {
            let owned: Vec<bool> = rows
                .windows(2)
                .map(|d| stretch_owned(&curve, tile, d[0], d[1]))
                .collect();
            mesh.keep_triangles(|t| owned[t / (2 * quads)]);
        }
        if let Some(shoulder) = prof.shoulder_material.as_ref().filter(|_| cols > 3) {
            // The carriageway's quads are those between the cross section's -w and +w columns.
            let first = offsets.iter().position(|o| *o >= -w).unwrap_or(0);
            let last = offsets.iter().position(|o| *o >= w).unwrap_or(cols - 1);
            let is_road = |t: usize| (first..last).contains(&((t / 2) % quads));
            let mut edge = mesh.clone();
            edge.material = shoulder.clone();
            edge.keep_triangles(|t| !is_road(t));
            mesh.keep_triangles(is_road);
            if !edge.indices.is_empty() {
                GeneratedMesh::merge_into(&mut meshes, edge);
            }
        }
        if !mesh.indices.is_empty() {
            GeneratedMesh::merge_into(&mut meshes, mesh);
        }
    }
    edges
        .attrs
        .insert("side".into(), AttrColumn::Int(edge_side.into()));
    edges
        .attrs
        .insert("distance_m".into(), AttrColumn::Float(edge_dist.into()));
    let mut out = Bundle::empty();
    for m in &meshes {
        out.0.push(DataItem::new(Payload::Mesh(Arc::new(MeshData {
            asset: String::new(),
            positions: m.positions.clone(),
            normals: m.normals.clone(),
            indices: m.indices.clone(),
        }))));
        GeneratedMesh::merge_into(&mut ctx.output().meshes, m.clone());
    }
    Ok(vec![out, Bundle::one(DataItem::points(edges))])
}

/// `road.pieces`: places a profile's `pieces` rows (spec 64 § 7.1, ADR-2731). Each row repeats its
/// mesh along the whole road: `round(length / mesh length)` copies, at least one, stretched to fit
/// exactly, in the frame of `spawn.spline_mesh`, moved sideways by `offset_m` (Left is negative) and
/// laid on the surface `road.ribbon` builds at that offset.
fn eval_pieces(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.road_pieces").entered();
    let ground_b = input(inputs, 1);
    let ground: Option<Arc<GroundData>> = ground_b.0.iter().find_map(|i| match &i.payload {
        Payload::Ground(g) => Some(g.clone()),
        _ => None,
    });
    let smooth = (s.i("smooth_rows").clamp(1, 15) as usize) | 1;
    let axis_z = s.text("forward_axis") == "Z";
    let axis = if axis_z { 2 } else { 0 };
    let material = s.text("material").to_string();
    let tile = ctx.tile();
    let cell_m = tile.map(|t| t.cell_m);
    let mut built: Vec<GeneratedMesh> = Vec::new();
    for item in crate::nodes::input_items(inputs, 0) {
        let Payload::Spline(sd) = &item.payload else {
            continue;
        };
        let prof = profile_for(ctx, item, s)?;
        if prof.pieces.is_empty() {
            continue;
        }
        let curve = sd.curve();
        let len = f64::from(curve.length());
        if !crate::nodes::common::has_length(len) {
            ctx.warn("spline_degenerate", "a road spline with no length has no pieces");
            continue;
        }
        let surf = RoadSurface::new(&curve, &prof, ground.as_deref(), smooth, cell_m);
        for piece in &prof.pieces {
            ctx.check_cancel()?;
            let Some(src) = ctx.world().mesh(&piece.mesh) else {
                ctx.warn("mesh_missing", format!("mesh `{}` was not captured", piece.mesh));
                continue;
            };
            let (lo, hi) = src.bounds();
            let extent = f64::from(hi[axis] - lo[axis]);
            if !crate::nodes::common::has_length(extent) {
                ctx.warn("mesh_missing", format!("mesh `{}` has no length", piece.mesh));
                continue;
            }
            let count = (crate::detmath::floor(len / extent + 0.5) as usize).max(1);
            ctx.check_points(count)?;
            let offset = match piece.side {
                crate::road::RoadSide::Left => -piece.offset_m,
                crate::road::RoadSide::Right => piece.offset_m,
            };
            let mut moved = (*src).clone();
            for p in &mut moved.positions {
                // The side coordinate of `bend`: +Z of an X-long mesh, -X of a Z-long one.
                if axis_z {
                    p[0] -= offset;
                } else {
                    p[2] += offset;
                }
            }
            let opts = BendOpts {
                axis_z,
                per_m: s.f("segments_per_m") as f32,
                uv_mode: "meters",
                material: &material,
                collision: s.text("collision") == "mesh",
                surface: Some((&surf, offset)),
            };
            let module = len / count as f64;
            for k in 0..count {
                let start = k as f64 * module;
                let origin = curve.at_distance_f64(start).position;
                if tile.is_none_or(|t| t.owns(f64::from(origin.x), f64::from(origin.z))) {
                    GeneratedMesh::merge_into(&mut built, bend(&moved, &curve, start, module, &opts)?);
                }
            }
        }
    }
    let mut out = Bundle::empty();
    for m in &built {
        out.0.push(DataItem::new(Payload::Mesh(Arc::new(MeshData {
            asset: String::new(),
            positions: m.positions.clone(),
            normals: m.normals.clone(),
            indices: m.indices.clone(),
        }))));
        GeneratedMesh::merge_into(&mut ctx.output().meshes, m.clone());
    }
    Ok(vec![out])
}

/// Area-weighted vertex normals.
pub(crate) fn vertex_normals(positions: &[[f32; 3]], indices: &[u32]) -> Vec<[f32; 3]> {
    let mut acc = vec![Vec3::ZERO; positions.len()];
    for t in indices.as_chunks::<3>().0 {
        let [a, b, c] = t.map(|i| Vec3::from(positions[i as usize]));
        let n = (b - a).cross(c - a);
        for i in t {
            acc[*i as usize] += n;
        }
    }
    acc.into_iter()
        .map(|n| n.normalize_or(Vec3::Y).to_array())
        .collect()
}

/// Ribbon smoothing the flatten node assumes (the `road.ribbon` default), so ground and road agree.
pub(crate) const FLATTEN_SMOOTH_ROWS: usize = 3;

fn eval_flatten(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.flatten_road").entered();
    match ctx.terrain_edits() {
        TerrainEditGate::Allowed => {}
        TerrainEditGate::NotBaked => {
            return Err(NodeError::new(
                "flatten_runtime_forbidden",
                "terrain.flatten_road runs only in Baked generators",
            ));
        }
        TerrainEditGate::NotAllowed => {
            return Err(NodeError::new(
                "terrain_edits_not_allowed",
                "the generator (allow_terrain_edits) or the project ([procgen] terrain_edits) does not allow terrain edits",
            ));
        }
    }
    let ground_b = input(inputs, 1);
    let Some(g) = ground_b.0.iter().find_map(|i| match &i.payload {
        Payload::Ground(g) => Some(g.clone()),
        _ => None,
    }) else {
        return Err(NodeError::new(
            "ground_missing",
            "terrain.flatten_road needs a Ground item",
        ));
    };
    let (cols, rows) = (g.size[0] as usize, g.size[1] as usize);
    let span = |n: u32| (n.max(2) - 1) as f32;
    let half_world = g.world_width * 0.5;
    let world_of = |c: usize, r: usize| {
        (
            (g.origin[0] as usize + c) as f32 / span(g.map_size[0]) * g.world_width - half_world,
            (g.origin[1] as usize + r) as f32 / span(g.map_size[1]) * g.world_width - half_world,
        )
    };
    let grid_of = |x: f32, z: f32| {
        (
            (x + half_world) / g.world_width * span(g.map_size[0]) - g.origin[0] as f32,
            (z + half_world) / g.world_width * span(g.map_size[1]) - g.origin[1] as f32,
        )
    };
    let mut heights = g.heights.clone();
    let mut changed: Option<[usize; 4]> = None;
    let splines = input(inputs, 0);
    // A tiled bake: each cell records the samples inside it (spec 66 § 2).
    let tile = ctx.tile();
    for (index, item) in splines.0.iter().enumerate() {
        ctx.check_cancel()?;
        let Payload::Spline(sd) = &item.payload else {
            continue;
        };
        let prof = profile_for(ctx, item, s)?;
        if !prof.terrain.flatten {
            continue;
        }
        let curve = sd.curve();
        if !crate::nodes::common::has_length(f64::from(curve.length())) {
            continue;
        }
        let mut flat_spline = sd.spline.clone();
        for p in &mut flat_spline.points {
            p.position.y = 0.0;
            p.tangent_in.y = 0.0;
            p.tangent_out.y = 0.0;
        }
        let flat = flat_spline.curve();
        let surf = RoadSurface::new(
            &curve,
            &prof,
            Some(g.as_ref()),
            FLATTEN_SMOOTH_ROWS,
            tile.map(|t| t.cell_m),
        );
        let t = &prof.terrain;
        let edge = prof.width_m * 0.5 + prof.shoulders_m.0.max(prof.shoulders_m.1);
        let half = edge + t.flatten_extra_m;
        let reach = half + t.falloff_m;
        let (lo, hi) = curve.bounds();
        let (mut x0, mut z0, mut x1, mut z1) = (lo.x - reach, lo.z - reach, hi.x + reach, hi.z + reach);
        if let Some(t) = tile {
            x0 = x0.max(t.rect[0] as f32);
            z0 = z0.max(t.rect[1] as f32);
            x1 = x1.min(t.rect[2] as f32);
            z1 = z1.min(t.rect[3] as f32);
        }
        if x0 > x1 || z0 > z1 {
            continue;
        }
        let (c0, r0) = grid_of(x0, z0);
        let (c1, r1) = grid_of(x1, z1);
        let clampi = |v: f32, n: usize| (v.max(0.0) as usize).min(n.saturating_sub(1));
        let (c0, c1) = (clampi(c0.floor(), cols), clampi(c1.ceil(), cols));
        let (r0, r1) = (clampi(r0.floor(), rows), clampi(r1.ceil(), rows));
        let mut worst_limit: Option<f32> = None;
        let mut road_changed: Option<[usize; 4]> = None;
        for r in r0..=r1 {
            for c in c0..=c1 {
                let (x, z) = world_of(c, r);
                if tile.is_some_and(|t| !t.owns(f64::from(x), f64::from(z))) {
                    continue;
                }
                let hit = flat.closest(Vec3::new(x, 0.0, z));
                let dist = hit.distance_m;
                if dist >= reach {
                    continue;
                }
                let weight = if dist <= half {
                    1.0
                } else {
                    let u = ((dist - half) / t.falloff_m.max(1e-6)).clamp(0.0, 1.0);
                    1.0 - u * u * (3.0 - 2.0 * u)
                };
                let fs = flat.at_key(hit.key);
                let r_flat = flat_right(&fs);
                let o = (Vec3::new(x, 0.0, z) - fs.position)
                    .dot(r_flat)
                    .clamp(-edge, edge);
                let along = f64::from(curve.at_key(hit.key).along_m);
                let target = surf.height_at(along, o) - prof.surface_lift_m;
                let idx = r * cols + c;
                let cur = heights[idx];
                let delta = target - cur;
                let limited = delta.clamp(-t.carve_max_m, t.fill_max_m);
                if limited != delta && dist <= half {
                    worst_limit = Some(worst_limit.map_or(hit.along_m, |w: f32| w.min(hit.along_m)));
                }
                let new = cur + weight * limited;
                if new != cur {
                    heights[idx] = new;
                    let grown = |b: Option<[usize; 4]>| {
                        Some(match b {
                            None => [c, r, c, r],
                            Some(b) => [b[0].min(c), b[1].min(r), b[2].max(c), b[3].max(r)],
                        })
                    };
                    changed = grown(changed);
                    road_changed = grown(road_changed);
                }
            }
        }
        let origin = g.origin;
        let line = ctx.road_report(&road_key(item, index));
        line.length_m = curve.length();
        line.flatten_rect = road_changed.map(|b| {
            [
                origin[0] + b[0] as u32,
                origin[1] + b[1] as u32,
                origin[0] + b[2] as u32 + 1,
                origin[1] + b[3] as u32 + 1,
            ]
        });
        line.flatten_limit_at_m = worst_limit;
        if let Some(at) = worst_limit {
            ctx.warn(
                "flatten_limit",
                format!("the road needs more than the carve/fill limits from {at:.1} m along; the ribbon spans the gap"),
            );
        }
    }
    if let Some(b) = changed {
        let mut rec = TerrainEditRecord {
            rect: [
                g.origin[0] + b[0] as u32,
                g.origin[1] + b[1] as u32,
                g.origin[0] + b[2] as u32 + 1,
                g.origin[1] + b[3] as u32 + 1,
            ],
            ..TerrainEditRecord::default()
        };
        for r in b[1]..=b[3] {
            rec.samples
                .extend_from_slice(&heights[r * cols + b[0]..=r * cols + b[2]]);
            rec.original
                .extend_from_slice(&g.heights[r * cols + b[0]..=r * cols + b[2]]);
        }
        ctx.output().terrain_edits.push(rec);
    }
    Ok(vec![splines])
}

fn eval_exclusion(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        let Payload::Spline(sd) = &item.payload else {
            continue;
        };
        let prof = profile_for(ctx, item, s)?;
        let curve = sd.curve();
        let line: Vec<[f64; 2]> = curve
            .to_polyline(0.05)
            .iter()
            .map(|p| [f64::from(p.x), f64::from(p.z)])
            .collect();
        let half = f64::from(
            prof.width_m * 0.5 + prof.shoulders_m.0.max(prof.shoulders_m.1) + prof.terrain.flatten_extra_m,
        ) + s.f("extra_m");
        let y = sd
            .spline
            .points
            .iter()
            .map(|p| f64::from(p.position.y))
            .sum::<f64>()
            / sd.spline.points.len().max(1) as f64;
        let o = line.first().copied().unwrap_or([0.0; 2]);
        let snap = |v: f64| (v / crate::polygon::GRID_M).round() * crate::polygon::GRID_M;
        let poly = PolygonData::band(&[line], half, [snap(o[0]), snap(o[1])], y)
            .map_err(|e| NodeError::new(e.code(), e.to_string()))?;
        out.0.push(item.with_payload(Payload::Polygon(Arc::new(poly))));
    }
    Ok(vec![out])
}

#[cfg(test)]
mod one_row_tests {
    use super::*;

    /// Audit procgen1-correctness (medium): `spawn.spline_mesh` must reject an index past the
    /// vertex buffer with `mesh_bad_index` instead of panicking inside `subdivide`.
    #[test]
    fn spline_mesh_rejects_an_index_past_the_vertex_buffer() {
        use regia_scene::spline::Spline;
        let src = MeshData {
            asset: "bad.glb".into(),
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 1.0, 0.0]; 3],
            indices: vec![0, 1, 5],
        };
        let curve = Spline::through([Vec3::new(0.0, 0.0, 0.0), Vec3::new(4.0, 0.0, 0.0)]).curve();
        let material = String::new();
        let uv_mode = String::from("stretch");
        let opts = BendOpts {
            axis_z: false,
            per_m: 1.0,
            uv_mode: &uv_mode,
            material: &material,
            collision: false,
            surface: None,
        };
        // `length_m > 0` and `segments_per_m` 1 take the subdivide path.
        let err = bend(&src, &curve, 0.0, 2.0, &opts).expect_err("index past the buffer");
        assert_eq!(err.code, "mesh_bad_index");
        let mut ok = src.clone();
        ok.indices = vec![0, 1, 2];
        let bent = bend(&ok, &curve, 0.0, 2.0, &opts).expect("in-range indices bend");
        assert!(bent.indices.iter().all(|&i| (i as usize) < bent.positions.len()));
        assert!(!bent.positions.is_empty());
    }

    /// Audit wave 5 (procgen-g2 Low): a surface whose rows deduplicated to one (a spline shorter
    /// than 1 µm) answers that row's height, crown drop included, instead of panicking.
    #[test]
    fn a_one_row_surface_answers_its_row_height() {
        let surface = RoadSurface {
            rows: vec![0.0],
            offsets: vec![0.0],
            xz: vec![vec![Vec3::ZERO]],
            center: vec![2.5],
            slope: 0.5,
        };
        assert_eq!(surface.height_at(0.0, 0.0), 2.5);
        assert_eq!(surface.height_at(7.0, -2.0), 1.5);
    }
}
