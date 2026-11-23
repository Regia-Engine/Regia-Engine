// SPDX-License-Identifier: MPL-2.0
//! Lightmap UV (UV2) generation and atlas packing for `regia bake-lightmaps` (ADR-860,
//! `docs/formats/lightmap.md`).
//!
//! For every placed mesh (world-space positions, so instance scale counts):
//!
//! 1. **Charts.** Triangles are grouped by flood fill over shared edges (positions welded, so a
//!    UV0 or normal seam in the source does not cut a chart). A neighbour joins a chart while its
//!    face normal stays within [`LightmapUvSettings::max_chart_angle_deg`] of the chart's running
//!    area-weighted normal; seeds go largest triangle first. A member that ends up more than 85°
//!    from the final chart normal is ejected and charted again, so the planar projection never
//!    folds a triangle over (Lévy et al. 2002 segment by normal deviation for the same reason;
//!    the charting here is the simpler normal-cone growing of Sander et al. 2001).
//! 2. **Projection.** Each chart is projected onto the plane of its normal, then turned to the
//!    minimum-area bounding rectangle over its convex hull edges (Freeman & Shapira 1975; the
//!    hull by Andrew's monotone chain).
//! 3. **Texel density.** A chart's interior is `ceil(extent · density)` texels per side, at least
//!    [`LightmapUvSettings::min_chart_texels`], plus [`LightmapUvSettings::padding`] gutter texels
//!    on every side. `density = texels_per_metre · input.density_scale · fit_scale`.
//! 4. **Packing.** Two levels, both skyline bottom-left (Jylänki 2010, "A Thousand Ways to Pack
//!    the Bin"): a mesh's charts into its own rectangle (its local UV2 is 0..1 over it), then the
//!    mesh rectangles into a power-of-two-wide atlas. When the atlas would pass
//!    [`LightmapUvSettings::max_atlas`], the rectangles go to pages of `max_atlas` texels a side
//!    instead, up to [`LightmapUvSettings::max_pages`] (ADR-3300, [`LightmapUvReport::pages`]); a
//!    page is filled by the same skyline packer and the next one opens with the rectangles that did
//!    not fit, so a mesh's charts never straddle two pages. Only past that budget does `fit_scale`
//!    drop by 0.8 and everything is laid out again ([`LightmapUvReport::fit_scale`]).
//!
//! The output is a [`LightmapLayout`] ready for the baker: one [`LightmapEntry`] and
//! [`MeshUnwrap`] per input. A chart seam splits a vertex (`xref` names the source vertex).

mod atlas_layout;
mod unwrap_meshlets;

use std::collections::HashMap;

pub use atlas_layout::build_atlas_layout;
use regia_asset::lightmap::{
    LightmapEntry, LightmapLayout, MAX_LIGHTMAP_PAGES, MAX_LIGHTMAP_SIZE, MeshUnwrap,
};
use regia_core::glam::{DVec2, Vec3};
pub use unwrap_meshlets::{
    SourceAttributes, chart_of_each_vertex, cluster_triangles_across_charts, meshlets_for_unwrap,
};

/// Cosine of the largest angle a member's normal may have to its chart's final normal.
const EJECT_COS: f32 = 0.087_155_74; // cos 85°
/// Layout attempts before the atlas is declared full.
const MAX_FIT_ATTEMPTS: u32 = 24;
/// `fit_scale` multiplier per attempt.
const FIT_STEP: f32 = 0.8;
/// Target share of a mesh rectangle covered by chart rectangles when sizing its width.
const FILL_TARGET: f64 = 0.8;

/// Settings of the UV2 generator.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightmapUvSettings {
    /// Texels per metre of surface (before per-mesh scale and atlas fitting).
    pub texels_per_metre: f32,
    /// Gutter texels on every side of a chart (≥ 1 for bilinear filtering).
    pub padding: u32,
    /// Largest atlas side, texels (≤ `MAX_LIGHTMAP_SIZE`).
    pub max_atlas: u32,
    /// Largest angle between a triangle's normal and its chart's, degrees (1..=80).
    pub max_chart_angle_deg: f32,
    /// Smallest chart interior per side, texels.
    pub min_chart_texels: u32,
    /// Most atlas pages the layout may open before it lowers the density (ADR-3300; 1 = one
    /// atlas, as before pages existed; at most `MAX_LIGHTMAP_PAGES`).
    pub max_pages: u32,
}

impl Default for LightmapUvSettings {
    fn default() -> Self {
        Self {
            texels_per_metre: 8.0,
            padding: 2,
            max_atlas: 4096,
            max_chart_angle_deg: 60.0,
            min_chart_texels: 2,
            max_pages: 1,
        }
    }
}

/// One placed mesh to unwrap.
#[derive(Clone, Copy, Debug)]
pub struct UnwrapInput<'a> {
    /// Entry name (unique within the bake).
    pub name: &'a str,
    /// World-space positions, metres.
    pub positions: &'a [[f32; 3]],
    /// Triangle list.
    pub indices: &'a [u32],
    /// Multiplier on the texel density for this mesh (hero pieces > 1, background < 1).
    pub density_scale: f32,
}

/// A UV2 generation error. `code` is the agent error code (`book/agents/lightmaps.md`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct LightmapUvError {
    /// `lightmap_uv_settings`, `lightmap_uv_invalid_mesh`, `lightmap_uv_empty` or
    /// `lightmap_uv_atlas_full`.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl LightmapUvError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// What the layout did.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct LightmapUvReport {
    /// Atlas (page) width, texels.
    pub width: u32,
    /// Atlas (page) height, texels.
    pub height: u32,
    /// Atlas pages the layout opened (ADR-3300; 1 when everything fits one atlas).
    pub pages: u32,
    /// Charts over every mesh.
    pub charts: usize,
    /// Atlas fitting scale on the density (1 = as asked).
    pub fit_scale: f32,
    /// Layout attempts (1 = fitted first time).
    pub attempts: u32,
    /// Share of the atlas (every page) inside chart interiors, 0..1.
    pub chart_fill: f32,
    /// Share of the atlas inside mesh rectangles, 0..1.
    pub rect_fill: f32,
    /// Triangles with (near) zero area.
    pub degenerate_triangles: usize,
    /// Which generator laid the charts out: `charts` ([`build_lightmap_layout`]) or `atlas`
    /// ([`build_atlas_layout`], the default of `regia bake-lightmaps`).
    pub generator: &'static str,
}

/// One chart: a set of triangles of a mesh and their projected 2-D coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct Chart {
    /// Source triangle indices (into `indices / 3`), ascending.
    pub triangles: Vec<u32>,
    /// Area-weighted unit normal.
    pub normal: [f32; 3],
    /// Projected position (metres) per chart corner, `3 · triangles.len()` entries in triangle
    /// order, translated so the minimum is at the origin and turned to the minimum-area box.
    pub corners: Vec<[f64; 2]>,
    /// Extent of the projected chart, metres.
    pub extent: [f64; 2],
    /// World-space area, m².
    pub area: f64,
}

/// World-space surface area of a mesh, m².
fn mesh_area(input: &UnwrapInput<'_>) -> f64 {
    input
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            let p = |k: usize| Vec3::from_array(input.positions[t[k] as usize]).as_dvec3();
            0.5 * (p(1) - p(0)).cross(p(2) - p(0)).length()
        })
        .sum()
}

fn check_settings(s: &LightmapUvSettings) -> Result<(), LightmapUvError> {
    let bad = |m: String| Err(LightmapUvError::new("lightmap_uv_settings", m));
    if !(s.texels_per_metre.is_finite() && s.texels_per_metre > 0.0) {
        return bad(format!("texels_per_metre {} must be > 0", s.texels_per_metre));
    }
    if s.padding == 0 || s.padding > 16 {
        return bad(format!("padding {} must be 1..=16", s.padding));
    }
    if !(16..=MAX_LIGHTMAP_SIZE).contains(&s.max_atlas) {
        return bad(format!(
            "max_atlas {} must be 16..={MAX_LIGHTMAP_SIZE}",
            s.max_atlas
        ));
    }
    if !(1.0..=80.0).contains(&s.max_chart_angle_deg) {
        return bad(format!(
            "max_chart_angle_deg {} must be 1..=80",
            s.max_chart_angle_deg
        ));
    }
    if s.min_chart_texels == 0 || s.min_chart_texels > 64 {
        return bad(format!("min_chart_texels {} must be 1..=64", s.min_chart_texels));
    }
    if !(1..=MAX_LIGHTMAP_PAGES).contains(&s.max_pages) {
        return Err(LightmapUvError::new(
            "lightmap_uv_settings",
            format!("max_pages {} must be 1..={MAX_LIGHTMAP_PAGES}", s.max_pages),
        ));
    }
    Ok(())
}

fn check_mesh(input: &UnwrapInput<'_>) -> Result<(), LightmapUvError> {
    let bad = |m: String| {
        Err(LightmapUvError::new(
            "lightmap_uv_invalid_mesh",
            format!("{}: {m}", input.name),
        ))
    };
    if !input.indices.len().is_multiple_of(3) {
        return bad(format!("{} indices is not a multiple of 3", input.indices.len()));
    }
    if input.indices.is_empty() {
        return Err(LightmapUvError::new(
            "lightmap_uv_empty",
            format!("{}: no triangles", input.name),
        ));
    }
    if let Some(i) = input
        .indices
        .iter()
        .find(|&&i| i as usize >= input.positions.len())
    {
        return bad(format!("index {i} is past {} vertices", input.positions.len()));
    }
    if input.positions.iter().flatten().any(|v| !v.is_finite()) {
        return bad("a position is not finite".into());
    }
    if !(input.density_scale.is_finite() && input.density_scale > 0.0) {
        return bad(format!("density_scale {} must be > 0", input.density_scale));
    }
    Ok(())
}

/// Splits a mesh into charts (steps 1 and 2 of the module docs).
///
/// # Errors
///
/// [`LightmapUvError`] for invalid settings or an invalid mesh.
pub fn generate_charts(
    input: &UnwrapInput<'_>,
    settings: &LightmapUvSettings,
) -> Result<Vec<Chart>, LightmapUvError> {
    check_settings(settings)?;
    check_mesh(input)?;
    let pos = |i: u32| Vec3::from_array(input.positions[i as usize]);
    let tris = input.indices.as_chunks::<3>().0;
    // Welded vertex ids by exact position bits (+0.0 and -0.0 weld).
    let mut weld: HashMap<[u32; 3], u32> = HashMap::new();
    let welded: Vec<u32> = input
        .positions
        .iter()
        .map(|p| {
            let key = p.map(|v| if v == 0.0 { 0 } else { v.to_bits() });
            let next = weld.len() as u32;
            *weld.entry(key).or_insert(next)
        })
        .collect();
    let mut edges: HashMap<(u32, u32), Vec<u32>> = HashMap::new();
    let mut normals = Vec::with_capacity(tris.len());
    let mut areas = Vec::with_capacity(tris.len());
    // The three welded edges of a triangle, each as (low id, high id).
    let tri_edges = |tri: &[u32; 3]| -> [(u32, u32); 3] {
        std::array::from_fn(|k| {
            let (a, b) = (welded[tri[k] as usize], welded[tri[(k + 1) % 3] as usize]);
            (a.min(b), a.max(b))
        })
    };
    for (t, tri) in tris.iter().enumerate() {
        let n = (pos(tri[1]) - pos(tri[0])).cross(pos(tri[2]) - pos(tri[0]));
        areas.push(0.5 * n.length() as f64);
        normals.push(n.normalize_or_zero());
        for e in tri_edges(tri) {
            if e.0 != e.1 {
                edges.entry(e).or_default().push(t as u32);
            }
        }
    }
    let neighbours = |t: usize| -> Vec<u32> {
        let mut out = Vec::new();
        for e in tri_edges(&tris[t]) {
            if let Some(list) = edges.get(&e) {
                out.extend(list.iter().copied().filter(|&o| o as usize != t));
            }
        }
        out
    };
    let cos_max = settings.max_chart_angle_deg.to_radians().cos();
    let mut order: Vec<usize> = (0..tris.len()).collect();
    order.sort_by(|&a, &b| areas[b].total_cmp(&areas[a]).then(a.cmp(&b)));
    let mut chart_of: Vec<Option<usize>> = vec![None; tris.len()];
    let mut members: Vec<Vec<u32>> = Vec::new();
    let mut chart_normals: Vec<Vec3> = Vec::new();
    loop {
        let mut progressed = false;
        for &seed in &order {
            if chart_of[seed].is_some() {
                continue;
            }
            progressed = true;
            let c = members.len();
            chart_of[seed] = Some(c);
            let mut list = vec![seed as u32];
            let mut sum = normals[seed] * areas[seed].max(1e-12) as f32;
            let mut queue = std::collections::VecDeque::from([seed]);
            while let Some(t) = queue.pop_front() {
                for o in neighbours(t) {
                    let o = o as usize;
                    if chart_of[o].is_some() {
                        continue;
                    }
                    let n = normals[o];
                    let avg = sum.normalize_or_zero();
                    // A degenerate triangle has no normal: it joins the first chart that reaches it.
                    if n != Vec3::ZERO && n.dot(avg) < cos_max {
                        continue;
                    }
                    chart_of[o] = Some(c);
                    list.push(o as u32);
                    sum += n * areas[o] as f32;
                    queue.push_back(o);
                }
            }
            let (mut list, final_n) = settle_chart(list, seed, &normals, &areas, &mut chart_of);
            list.sort_unstable();
            members.push(list);
            chart_normals.push(final_n);
        }
        if !progressed {
            break;
        }
    }
    let charts = members
        .into_iter()
        .zip(chart_normals)
        .map(|(triangles, n)| project_chart(&triangles, n, tris, &pos, &areas))
        .collect();
    Ok(charts)
}

/// Settles a grown chart (`list`, seed first, in growth order): members more than 85° from the
/// chart's area-weighted normal are ejected (they chart again later), the normal is recomputed from
/// what stays, and the test repeats until every member passes. When even the seed fails (a curved
/// walk dragged the normal past it), the seed charts alone on its own normal. Projection then never
/// sees a member edge-on or from behind (ADR-1101, review of ADR-860).
fn settle_chart(
    mut list: Vec<u32>,
    seed: usize,
    normals: &[Vec3],
    areas: &[f64],
    chart_of: &mut [Option<usize>],
) -> (Vec<u32>, Vec3) {
    let own = if normals[seed] != Vec3::ZERO {
        normals[seed]
    } else {
        Vec3::Y
    };
    // The same sum, in the same order, as the growth's running normal.
    let normal_of = |list: &[u32]| {
        let sum: Vec3 = list
            .iter()
            .map(|&t| normals[t as usize] * areas[t as usize].max(1e-12) as f32)
            .sum();
        if sum.length_squared() > 0.0 {
            sum.normalize()
        } else {
            own
        }
    };
    let passes = |t: usize, n: Vec3| normals[t] == Vec3::ZERO || normals[t].dot(n) >= EJECT_COS;
    let mut n = normal_of(&list);
    for _ in 0..8 {
        let before = list.len();
        list.retain(|&t| {
            let t = t as usize;
            let keep = t == seed || passes(t, n);
            if !keep {
                chart_of[t] = None;
            }
            keep
        });
        if list.len() == before {
            break;
        }
        n = normal_of(&list);
    }
    if list.iter().all(|&t| passes(t as usize, n)) {
        return (list, n);
    }
    for &t in &list {
        if t as usize != seed {
            chart_of[t as usize] = None;
        }
    }
    (vec![seed as u32], own)
}

fn project_chart(
    triangles: &[u32],
    n: Vec3,
    tris: &[[u32; 3]],
    pos: &impl Fn(u32) -> Vec3,
    areas: &[f64],
) -> Chart {
    let helper = if n.x.abs() < 0.9 { Vec3::X } else { Vec3::Y };
    let u = n.cross(helper).normalize();
    let v = n.cross(u);
    let mut corners: Vec<DVec2> = triangles
        .iter()
        .flat_map(|&t| tris[t as usize])
        .map(|i| {
            let p = pos(i);
            DVec2::new(p.dot(u) as f64, p.dot(v) as f64)
        })
        .collect();
    // Minimum-area box over the hull edges.
    let hull = convex_hull(&corners);
    let mut best = (f64::INFINITY, DVec2::X);
    for k in 0..hull.len() {
        let e = hull[(k + 1) % hull.len()] - hull[k];
        if e.length_squared() <= 1e-24 {
            continue;
        }
        let dir = e.normalize();
        let (lo, hi) = rotated_bounds(&hull, dir);
        let area = (hi.x - lo.x) * (hi.y - lo.y);
        if area < best.0 - 1e-12 {
            best = (area, dir);
        }
    }
    let dir = best.1;
    let (lo, hi) = rotated_bounds(&corners, dir);
    for c in &mut corners {
        *c = rotate(*c, dir) - lo;
    }
    let mut extent = hi - lo;
    // Lay charts out wider than tall (the packer's rows are short).
    if extent.y > extent.x {
        for c in &mut corners {
            *c = DVec2::new(c.y, extent.x - c.x);
        }
        extent = DVec2::new(extent.y, extent.x);
    }
    Chart {
        triangles: triangles.to_vec(),
        normal: n.to_array(),
        corners: corners.iter().map(|c| [c.x, c.y]).collect(),
        extent: [extent.x, extent.y],
        area: triangles.iter().map(|&t| areas[t as usize]).sum(),
    }
}

/// `p` in the frame whose x axis is `dir` (unit).
fn rotate(p: DVec2, dir: DVec2) -> DVec2 {
    DVec2::new(p.dot(dir), dir.perp_dot(p))
}

fn rotated_bounds(points: &[DVec2], dir: DVec2) -> (DVec2, DVec2) {
    let mut lo = DVec2::splat(f64::INFINITY);
    let mut hi = DVec2::splat(f64::NEG_INFINITY);
    for &p in points {
        let r = rotate(p, dir);
        lo = lo.min(r);
        hi = hi.max(r);
    }
    if points.is_empty() {
        return (DVec2::ZERO, DVec2::ZERO);
    }
    (lo, hi)
}

/// Andrew's monotone chain; counter-clockwise, no repeated end point.
fn convex_hull(points: &[DVec2]) -> Vec<DVec2> {
    let mut p: Vec<DVec2> = points.to_vec();
    p.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    p.dedup();
    if p.len() < 3 {
        return p;
    }
    let cross = |o: DVec2, a: DVec2, b: DVec2| (a - o).perp_dot(b - o);
    // One chain per pass (lower over the sorted points, upper over them reversed), each popping
    // right turns; each chain's last point is the other's first.
    let chain = |points: &mut dyn Iterator<Item = DVec2>| {
        let mut out: Vec<DVec2> = Vec::new();
        for q in points {
            while out.len() >= 2 && cross(out[out.len() - 2], out[out.len() - 1], q) <= 0.0 {
                out.pop();
            }
            out.push(q);
        }
        out.pop();
        out
    };
    let mut hull = chain(&mut p.iter().copied());
    hull.extend(chain(&mut p.iter().rev().copied()));
    hull
}

/// A rectangle to place: `w × h` texels, `id` back to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PackRect {
    w: u32,
    h: u32,
    id: usize,
}

/// Skyline bottom-left packing into a strip of width `width` (unbounded height). Returns each
/// rect's corner (indexed like `rects`) and the height used, or `None` when a rect is wider than
/// the strip.
fn skyline_pack(rects: &[PackRect], width: u32) -> Option<(Vec<[u32; 2]>, u32)> {
    let (at, used) = skyline_pack_limited(rects, width, None)?;
    Some((at.into_iter().map(|a| a.unwrap_or([0, 0])).collect(), used))
}

/// [`skyline_pack`] with a height limit: a rectangle whose lowest place would end past `limit`
/// is left out (`None` in its slot) and the skyline does not change, so the next page can take it
/// (ADR-3300). Without a limit every rectangle is placed. `None` when a rect is wider than the strip.
fn skyline_pack_limited(
    rects: &[PackRect],
    width: u32,
    limit: Option<u32>,
) -> Option<(Vec<Option<[u32; 2]>>, u32)> {
    let mut order: Vec<usize> = (0..rects.len()).collect();
    order.sort_by(|&a, &b| {
        rects[b]
            .h
            .cmp(&rects[a].h)
            .then(rects[b].w.cmp(&rects[a].w))
            .then(rects[a].id.cmp(&rects[b].id))
    });
    // Skyline segments (x, y, width), left to right, covering 0..width.
    let mut sky: Vec<(u32, u32, u32)> = vec![(0, 0, width)];
    let mut at = vec![None; rects.len()];
    let mut used = 0u32;
    for k in order {
        let r = rects[k];
        if r.w > width {
            return None;
        }
        let mut best: Option<(u32, u32, usize)> = None; // (top, x, segment)
        for (i, &(x, _, _)) in sky.iter().enumerate() {
            if x + r.w > width {
                break;
            }
            let mut y = 0;
            let mut span = 0;
            for &(_, sy, sw) in &sky[i..] {
                y = y.max(sy);
                span += sw;
                if span >= r.w {
                    break;
                }
            }
            let top = y + r.h;
            if best.is_none_or(|(bt, bx, _)| (top, x) < (bt, bx)) {
                best = Some((top, x, i));
            }
        }
        let (top, x, i) = best?;
        if limit.is_some_and(|l| top > l) {
            continue;
        }
        let y = top - r.h;
        at[k] = Some([x, y]);
        used = used.max(top);
        // Replace the covered span with one segment at `top`.
        let end = x + r.w;
        let mut rebuilt: Vec<(u32, u32, u32)> = sky[..i].to_vec();
        rebuilt.push((x, top, r.w));
        for &(sx, sy, sw) in &sky[i..] {
            let s_end = sx + sw;
            if s_end <= end {
                continue;
            }
            let nx = sx.max(end);
            rebuilt.push((nx, sy, s_end - nx));
        }
        // Merge neighbours at the same height.
        sky.clear();
        for seg in rebuilt {
            match sky.last_mut() {
                Some(last) if last.1 == seg.1 && last.0 + last.2 == seg.0 => last.2 += seg.2,
                _ => sky.push(seg),
            }
        }
    }
    Some((at, used))
}

/// The rectangles in one page as before pages existed: the narrowest power-of-two width (at least
/// the widest rectangle and the area at [`FILL_TARGET`]) whose skyline packing is no taller than
/// wide, the height rounded up to whole 4×4 blocks. `None` past `max_atlas`.
fn one_page(rects: &[PackRect], s: &LightmapUvSettings) -> Option<(Vec<[u32; 2]>, u32, u32)> {
    let total: f64 = rects.iter().map(|r| f64::from(r.w) * f64::from(r.h)).sum();
    let widest = rects.iter().map(|r| r.w).max().unwrap_or(1);
    let mut width = ((total / FILL_TARGET).sqrt().ceil() as u32)
        .max(widest)
        .max(16)
        .next_power_of_two();
    let (at, used) = loop {
        if width > s.max_atlas {
            return None;
        }
        let (at, used) = skyline_pack(rects, width)?;
        if used <= width {
            break (at, used);
        }
        width *= 2;
    };
    let height = used.max(4).div_ceil(4) * 4;
    if height > s.max_atlas {
        return None;
    }
    Some((at, width, height))
}

/// A rectangle's page and corner on it (ADR-3300).
type PagePlace = (u32, [u32; 2]);

/// What [`place_rects`] returns: each rectangle's page and corner, the page width and height, and
/// the page count.
type Placement = (Vec<PagePlace>, u32, u32, u32);

/// The rectangles on pages of `side × side` (ADR-3300): page 0 takes what the skyline packer fits
/// under `side`, the next page the rest in the same order, and so on. Returns each rectangle's page
/// and corner, the tallest page's used height and the page count; `None` when a rectangle is larger
/// than a page or more than `max_pages` pages are needed.
fn pack_pages(rects: &[PackRect], side: u32, max_pages: u32) -> Option<(Vec<PagePlace>, u32, u32)> {
    if rects.iter().any(|r| r.w > side || r.h > side) {
        return None;
    }
    let mut placed: Vec<Option<PagePlace>> = vec![None; rects.len()];
    let mut left: Vec<usize> = (0..rects.len()).collect();
    let (mut used, mut pages) = (0u32, 0u32);
    while !left.is_empty() {
        if pages >= max_pages {
            return None;
        }
        let subset: Vec<PackRect> = left.iter().map(|&k| rects[k]).collect();
        let (at, page_used) = skyline_pack_limited(&subset, side, Some(side))?;
        let mut rest = Vec::new();
        for (&k, a) in left.iter().zip(at) {
            match a {
                Some(a) => placed[k] = Some((pages, a)),
                None => rest.push(k),
            }
        }
        if rest.len() == left.len() {
            return None;
        }
        used = used.max(page_used);
        pages += 1;
        left = rest;
    }
    Some((
        placed.into_iter().map(|p| p.unwrap_or((0, [0, 0]))).collect(),
        used,
        pages,
    ))
}

/// Where the mesh rectangles go (module docs, step 4): one page as before pages existed when it
/// fits `max_atlas`, else pages of `max_atlas` (rounded down to whole blocks) up to `max_pages`.
/// Returns each rectangle's page and corner, the page width and height and the page count.
fn place_rects(rects: &[PackRect], s: &LightmapUvSettings) -> Option<Placement> {
    if let Some((at, width, height)) = one_page(rects, s) {
        return Some((at.into_iter().map(|a| (0, a)).collect(), width, height, 1));
    }
    if s.max_pages <= 1 {
        return None;
    }
    let side = s.max_atlas / 4 * 4;
    let (at, used, pages) = pack_pages(rects, side, s.max_pages)?;
    Some((at, side, used.max(4).div_ceil(4) * 4, pages))
}

/// A mesh laid out in its own rectangle at one density.
struct MeshLayout {
    /// Rectangle size, texels.
    size: [u32; 2],
    /// Each chart's interior origin in the rectangle (texels) and its texel scale.
    charts: Vec<([f64; 2], f64)>,
    /// Texels inside chart interiors.
    interior_texels: u64,
}

/// Each chart's rectangle at `density`: its interior (at least `min_chart_texels` a side) plus
/// `padding` gutter texels on every side.
fn chart_rects(charts: &[Chart], density: f64, s: &LightmapUvSettings) -> Vec<PackRect> {
    let side = |e: f64| ((e * density).ceil() as u32).max(s.min_chart_texels);
    charts
        .iter()
        .enumerate()
        .map(|(id, c)| PackRect {
            w: side(c.extent[0]) + 2 * s.padding,
            h: side(c.extent[1]) + 2 * s.padding,
            id,
        })
        .collect()
}

fn layout_mesh(charts: &[Chart], density: f64, s: &LightmapUvSettings) -> Option<MeshLayout> {
    let pad = s.padding;
    let rects = chart_rects(charts, density, s);
    let total: f64 = rects.iter().map(|r| f64::from(r.w) * f64::from(r.h)).sum();
    let widest = rects.iter().map(|r| r.w).max().unwrap_or(1);
    let width = ((total / FILL_TARGET).sqrt().ceil() as u32).max(widest);
    if width > s.max_atlas {
        return None;
    }
    let (at, used) = skyline_pack(&rects, width)?;
    let interior_texels = rects
        .iter()
        .map(|r| u64::from(r.w - 2 * pad) * u64::from(r.h - 2 * pad))
        .sum();
    Some(MeshLayout {
        size: [width, used],
        charts: at
            .iter()
            .map(|&[x, y]| ([f64::from(x + pad), f64::from(y + pad)], density))
            .collect(),
        interior_texels,
    })
}

/// Packs chart rectangles of `sizes` texels (their padding included) as `layout_mesh` packs one
/// mesh's charts: a skyline strip as wide as the widest rectangle or the area at `FILL_TARGET`.
/// Returns the used `[width, height]` and each rectangle's corner. The tier derive repacks a
/// tier's charts at its own resolution with it (ADR-5125).
pub(crate) fn pack_chart_rects(sizes: &[[u32; 2]]) -> ([u32; 2], Vec<[u32; 2]>) {
    let rects: Vec<PackRect> = sizes
        .iter()
        .enumerate()
        .map(|(id, s)| PackRect {
            w: s[0].max(1),
            h: s[1].max(1),
            id,
        })
        .collect();
    let total: f64 = rects.iter().map(|r| f64::from(r.w) * f64::from(r.h)).sum();
    let widest = rects.iter().map(|r| r.w).max().unwrap_or(1);
    let width = ((total / FILL_TARGET).sqrt().ceil() as u32).max(widest);
    // No rectangle is wider than the strip, so the packer always places every one.
    let (at, used) = skyline_pack(&rects, width).unwrap_or_else(|| (vec![[0, 0]; rects.len()], 0));
    ([width, used.max(1)], at)
}

/// Lays out every input in one atlas, or in up to [`LightmapUvSettings::max_pages`] pages of
/// `max_atlas` when one does not hold them (module docs, steps 1–4).
///
/// # Errors
///
/// [`LightmapUvError`]: invalid settings, an invalid or empty mesh, duplicate names
/// (`lightmap_uv_invalid_mesh`), or charts that do not fit `max_pages` pages of `max_atlas`
/// even at `0.8^23` of the density (`lightmap_uv_atlas_full`).
pub fn build_lightmap_layout(
    inputs: &[UnwrapInput<'_>],
    settings: &LightmapUvSettings,
) -> Result<(LightmapLayout, LightmapUvReport), LightmapUvError> {
    let _span = tracing::info_span!("lightmap_uv_layout", meshes = inputs.len()).entered();
    check_settings(settings)?;
    let mut seen = std::collections::HashSet::new();
    for i in inputs {
        if !seen.insert(i.name) {
            return Err(LightmapUvError::new(
                "lightmap_uv_invalid_mesh",
                format!("{}: duplicate entry name", i.name),
            ));
        }
    }
    if inputs.is_empty() {
        return Err(LightmapUvError::new("lightmap_uv_empty", "no meshes to unwrap"));
    }
    let charts: Vec<Vec<Chart>> = inputs
        .iter()
        .map(|i| generate_charts(i, settings))
        .collect::<Result<_, _>>()?;
    let degenerate_triangles = inputs
        .iter()
        .map(|i| {
            i.indices
                .as_chunks::<3>()
                .0
                .iter()
                .filter(|t| {
                    let p = |k: usize| Vec3::from_array(i.positions[t[k] as usize]);
                    (p(1) - p(0)).cross(p(2) - p(0)).length() <= 1e-12
                })
                .count()
        })
        .sum();
    let mut fit = 1.0f32;
    for attempt in 1..=MAX_FIT_ATTEMPTS {
        if let Some(done) = try_layout(inputs, &charts, settings, fit) {
            let (layout, mut report) = done;
            report.fit_scale = fit;
            report.attempts = attempt;
            report.degenerate_triangles = degenerate_triangles;
            return Ok((layout, report));
        }
        fit *= FIT_STEP;
    }
    Err(LightmapUvError::new(
        "lightmap_uv_atlas_full",
        format!(
            "the charts do not fit {1} page(s) of {0}×{0} even at {fit:.4} of the density: raise max_atlas or max_pages, or lower texels_per_metre",
            settings.max_atlas, settings.max_pages
        ),
    ))
}

fn try_layout(
    inputs: &[UnwrapInput<'_>],
    charts: &[Vec<Chart>],
    s: &LightmapUvSettings,
    fit: f32,
) -> Option<(LightmapLayout, LightmapUvReport)> {
    let meshes: Vec<MeshLayout> = inputs
        .iter()
        .zip(charts)
        .map(|(i, c)| layout_mesh(c, f64::from(s.texels_per_metre * i.density_scale * fit), s))
        .collect::<Option<_>>()?;
    let rects: Vec<PackRect> = meshes
        .iter()
        .enumerate()
        .map(|(id, m)| PackRect {
            w: m.size[0],
            h: m.size[1].max(1),
            id,
        })
        .collect();
    let (at, width, height, pages) = place_rects(&rects, s)?;
    let (aw, ah) = (f64::from(width), f64::from(height));
    let mut entries = Vec::with_capacity(inputs.len());
    let mut unwraps = Vec::with_capacity(inputs.len());
    let mut chart_texels = 0u64;
    let mut rect_texels = 0u64;
    for (((input, mesh_charts), m), &(page, [x, y])) in inputs.iter().zip(charts).zip(&meshes).zip(&at) {
        let [mw, mh] = [m.size[0], m.size[1].max(1)];
        chart_texels += m.interior_texels;
        rect_texels += u64::from(mw) * u64::from(mh);
        unwraps.push(unwrap_mesh(input, mesh_charts, m));
        entries.push(LightmapEntry {
            name: input.name.to_string(),
            source_vertices: input.positions.len() as u32,
            rect: [x, y, mw, mh],
            scale_offset: [
                (f64::from(mw) / aw) as f32,
                (f64::from(mh) / ah) as f32,
                (f64::from(x) / aw) as f32,
                (f64::from(y) / ah) as f32,
            ],
            vertex_count: 0,
            index_count: 0,
            first_vertex: 0,
            first_index: 0,
            charts: mesh_charts.len() as u32,
            area_m2: mesh_charts.iter().map(|c| c.area).sum::<f64>() as f32,
            texels_per_metre: s.texels_per_metre * input.density_scale * fit,
            key: None,
            seams: None,
            page,
        });
    }
    let atlas = (aw * ah * f64::from(pages)).max(1.0);
    let report = LightmapUvReport {
        width,
        height,
        pages,
        charts: charts.iter().map(Vec::len).sum(),
        fit_scale: fit,
        attempts: 0,
        chart_fill: (chart_texels as f64 / atlas) as f32,
        rect_fill: (rect_texels as f64 / atlas) as f32,
        degenerate_triangles: 0,
        generator: "charts",
    };
    let layout = LightmapLayout {
        width,
        height,
        density: s.texels_per_metre,
        padding: s.padding,
        entries,
        unwraps,
    };
    Some((layout, report))
}

/// The unwrapped vertices and indices of one mesh in its rectangle.
fn unwrap_mesh(input: &UnwrapInput<'_>, charts: &[Chart], m: &MeshLayout) -> MeshUnwrap {
    let tris = input.indices.as_chunks::<3>().0;
    // Corner UVs per triangle, from its chart.
    let mut corner_uv = vec![[[0.0f32; 2]; 3]; tris.len()];
    let mut chart_of = vec![0u32; tris.len()];
    let (w, h) = (f64::from(m.size[0]), f64::from(m.size[1].max(1)));
    for (c, chart) in charts.iter().enumerate() {
        let (origin, scale) = m.charts[c];
        for (&t, corners) in chart.triangles.iter().zip(chart.corners.as_chunks::<3>().0) {
            chart_of[t as usize] = c as u32;
            for (slot, p) in corner_uv[t as usize].iter_mut().zip(corners) {
                let u = (origin[0] + p[0] * scale) / w;
                let v = (origin[1] + p[1] * scale) / h;
                *slot = [u.clamp(0.0, 1.0) as f32, v.clamp(0.0, 1.0) as f32];
            }
        }
    }
    let mut ids: HashMap<(u32, u32), u32> = HashMap::new();
    let mut out = MeshUnwrap::default();
    for (t, tri) in tris.iter().enumerate() {
        for (j, &src) in tri.iter().enumerate() {
            let key = (src, chart_of[t]);
            let id = *ids.entry(key).or_insert_with(|| {
                out.xref.push(src);
                out.uv.push(corner_uv[t][j]);
                (out.xref.len() - 1) as u32
            });
            out.indices.push(id);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An axis-aligned box `size` metres, 24 vertices (split per face like a glTF export).
    fn cube(size: f32) -> (Vec<[f32; 3]>, Vec<u32>) {
        let h = size * 0.5;
        let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
            ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
            ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        ];
        let mut p = Vec::new();
        let mut idx = Vec::new();
        for (n, a, b) in faces {
            let base = p.len() as u32;
            for (sa, sb) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                p.push(std::array::from_fn(|k| h * (n[k] + sa * a[k] + sb * b[k])));
            }
            idx.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        (p, idx)
    }

    /// A UV sphere of `radius` with `seg × rings` quads (welded poles skipped: rings start
    /// off the pole).
    fn sphere(radius: f32, seg: u32, rings: u32) -> (Vec<[f32; 3]>, Vec<u32>) {
        let mut p = Vec::new();
        for r in 0..=rings {
            let th = std::f32::consts::PI * (0.05 + 0.9 * r as f32 / rings as f32);
            for s in 0..=seg {
                let ph = std::f32::consts::TAU * s as f32 / seg as f32;
                p.push([
                    radius * th.sin() * ph.cos(),
                    radius * th.cos(),
                    radius * th.sin() * ph.sin(),
                ]);
            }
        }
        let mut idx = Vec::new();
        for r in 0..rings {
            for s in 0..seg {
                let a = r * (seg + 1) + s;
                let b = a + seg + 1;
                idx.extend([a, a + 1, b, a + 1, b + 1, b]);
            }
        }
        (p, idx)
    }

    fn input<'a>(name: &'a str, p: &'a [[f32; 3]], i: &'a [u32]) -> UnwrapInput<'a> {
        UnwrapInput {
            name,
            positions: p,
            indices: i,
            density_scale: 1.0,
        }
    }

    fn signed_area(a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> f32 {
        (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])
    }

    /// Texel centres of an entry's rectangle covered by each chart; panics when two charts
    /// cover one centre (charts overlap).
    fn assert_no_overlap(layout: &LightmapLayout, e: usize) {
        let entry = &layout.entries[e];
        let u = &layout.unwraps[e];
        let (w, h) = (entry.rect[2] as usize, entry.rect[3] as usize);
        let mut owner: Vec<Option<usize>> = vec![None; w * h];
        // Chart id per triangle: triangles sharing an unwrapped vertex are in one chart.
        let tris = u.indices.as_chunks::<3>().0;
        let mut parent: Vec<usize> = (0..u.xref.len()).collect();
        fn find(p: &mut [usize], x: usize) -> usize {
            let mut r = x;
            while p[r] != r {
                r = p[r];
            }
            p[x] = r;
            r
        }
        for t in tris {
            let a = find(&mut parent, t[0] as usize);
            for &o in &t[1..] {
                let b = find(&mut parent, o as usize);
                parent[b] = a;
            }
        }
        for t in tris {
            let chart = find(&mut parent, t[0] as usize);
            let uv = t.map(|i| {
                let q = u.uv[i as usize];
                [q[0] * w as f32, q[1] * h as f32]
            });
            let area = signed_area(uv[0], uv[1], uv[2]);
            if area.abs() < 1e-9 {
                continue;
            }
            let lo = |k: usize| {
                uv.iter()
                    .map(|q| q[k])
                    .fold(f32::INFINITY, f32::min)
                    .floor()
                    .max(0.0) as usize
            };
            let hi = |k: usize, n: usize| {
                (uv.iter().map(|q| q[k]).fold(f32::NEG_INFINITY, f32::max).ceil() as usize).min(n)
            };
            for y in lo(1)..hi(1, h) {
                for x in lo(0)..hi(0, w) {
                    let c = [x as f32 + 0.5, y as f32 + 0.5];
                    let w0 = signed_area(uv[1], uv[2], c) / area;
                    let w1 = signed_area(uv[2], uv[0], c) / area;
                    let w2 = 1.0 - w0 - w1;
                    // Strictly inside (shared edges are not overlaps).
                    if w0 > 1e-4 && w1 > 1e-4 && w2 > 1e-4 {
                        let slot = &mut owner[y * w + x];
                        if let Some(o) = *slot {
                            assert_eq!(o, chart, "charts overlap at ({x}, {y})");
                        } else {
                            *slot = Some(chart);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_cube_makes_six_charts_without_flips() {
        let (p, i) = cube(2.0);
        let charts = generate_charts(&input("cube", &p, &i), &LightmapUvSettings::default()).expect("charts");
        assert_eq!(charts.len(), 6);
        for c in &charts {
            assert_eq!(c.triangles.len(), 2);
            assert!((c.area - 4.0).abs() < 1e-5, "{}", c.area);
            assert!(
                (c.extent[0] - 2.0).abs() < 1e-5 && (c.extent[1] - 2.0).abs() < 1e-5,
                "{:?}",
                c.extent
            );
        }
        let (layout, report) =
            build_lightmap_layout(&[input("cube", &p, &i)], &LightmapUvSettings::default()).expect("layout");
        assert_eq!(report.charts, 6);
        assert_eq!(report.fit_scale, 1.0);
        let u = &layout.unwraps[0];
        assert_eq!(u.indices.len(), i.len());
        assert_eq!(u.xref.len(), 24, "each face is its own chart: no extra splits");
        for t in u.indices.as_chunks::<3>().0 {
            let a = signed_area(u.uv[t[0] as usize], u.uv[t[1] as usize], u.uv[t[2] as usize]);
            assert!(a.abs() > 0.0, "no collapsed triangle");
        }
        assert_no_overlap(&layout, 0);
    }

    #[test]
    fn welded_seams_keep_a_flat_plane_in_one_chart() {
        // A 4×4 grid of quads whose vertices are split per quad (like a UV0 seam everywhere).
        let mut p = Vec::new();
        let mut idx = Vec::new();
        for z in 0..4 {
            for x in 0..4 {
                let base = p.len() as u32;
                for (dx, dz) in [(0, 0), (1, 0), (1, 1), (0, 1)] {
                    p.push([(x + dx) as f32, 0.0, (z + dz) as f32]);
                }
                idx.extend([base, base + 2, base + 1, base, base + 3, base + 2]);
            }
        }
        let charts =
            generate_charts(&input("plane", &p, &idx), &LightmapUvSettings::default()).expect("charts");
        assert_eq!(charts.len(), 1);
        assert!((charts[0].extent[0] - 4.0).abs() < 1e-5);
        assert!((charts[0].area - 16.0).abs() < 1e-4);
    }

    #[test]
    fn a_sphere_charts_by_normal_cone_and_never_folds() {
        let (p, i) = sphere(3.0, 24, 12);
        let s = LightmapUvSettings::default();
        let charts = generate_charts(&input("sphere", &p, &i), &s).expect("charts");
        assert!(
            charts.len() >= 4 && charts.len() <= 200,
            "{} charts",
            charts.len()
        );
        let tris = i.as_chunks::<3>().0;
        for c in &charts {
            let n = Vec3::from_array(c.normal);
            for &t in &c.triangles {
                let q = |k: usize| Vec3::from_array(p[tris[t as usize][k] as usize]);
                let fn_ = (q(1) - q(0)).cross(q(2) - q(0)).normalize_or_zero();
                assert!(
                    fn_.dot(n) >= EJECT_COS - 1e-4,
                    "a member faces away from its chart"
                );
            }
        }
        let (layout, _) = build_lightmap_layout(&[input("sphere", &p, &i)], &s).expect("layout");
        let u = &layout.unwraps[0];
        // One winding for every triangle of the unwrap (no fold-over).
        let signs: Vec<bool> = u
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| signed_area(u.uv[t[0] as usize], u.uv[t[1] as usize], u.uv[t[2] as usize]))
            .filter(|a| a.abs() > 1e-12)
            .map(|a| a > 0.0)
            .collect();
        let pos = signs.iter().filter(|&&b| b).count();
        assert!(
            pos == 0 || pos == signs.len(),
            "{pos} of {} triangles flipped",
            signs.len()
        );
        assert_no_overlap(&layout, 0);
    }

    #[test]
    fn charts_keep_their_gutters_and_density() {
        let (p, i) = cube(4.0);
        let s = LightmapUvSettings {
            texels_per_metre: 4.0,
            padding: 2,
            ..LightmapUvSettings::default()
        };
        let charts = generate_charts(&input("cube", &p, &i), &s).expect("charts");
        let m = layout_mesh(&charts, 4.0, &s).expect("mesh layout");
        // Each face: 4 m × 4 texels/m = 16 interior texels + 2 × 2 gutter.
        let rects = chart_rects(&charts, 4.0, &s);
        for r in &rects {
            assert_eq!([r.w, r.h], [20, 20]);
        }
        // Gutters stay apart once packed in the mesh rectangle.
        let (at, used) = skyline_pack(&rects, m.size[0]).expect("packs");
        assert_eq!(used, m.size[1]);
        for (a, (ra, pa)) in rects.iter().zip(&at).enumerate() {
            for (rb, pb) in rects.iter().zip(&at).skip(a + 1) {
                let apart = pa[0] + ra.w <= pb[0]
                    || pb[0] + rb.w <= pa[0]
                    || pa[1] + ra.h <= pb[1]
                    || pb[1] + rb.h <= pa[1];
                assert!(apart, "{pa:?} and {pb:?} overlap");
            }
        }
        // Interior origins sit one gutter inside their rectangles.
        for (&(origin, _), p) in m.charts.iter().zip(&at) {
            assert_eq!(origin, [f64::from(p[0] + 2), f64::from(p[1] + 2)]);
        }
        assert_eq!(m.interior_texels, 6 * 16 * 16);
    }

    #[test]
    fn many_meshes_share_one_atlas_and_scale_offsets_map_into_it() {
        let (cp, ci) = cube(1.0);
        let (sp, si) = sphere(1.5, 16, 8);
        let names: Vec<String> = (0..10).map(|k| format!("piece_{k}")).collect();
        let mut inputs: Vec<UnwrapInput<'_>> = names
            .iter()
            .enumerate()
            .map(|(k, n)| {
                if k.is_multiple_of(2) {
                    input(n, &cp, &ci)
                } else {
                    input(n, &sp, &si)
                }
            })
            .collect();
        inputs[0].density_scale = 2.0;
        let (layout, report) =
            build_lightmap_layout(&inputs, &LightmapUvSettings::default()).expect("layout");
        assert!(layout.width.is_power_of_two());
        assert!(layout.height.is_multiple_of(4) && layout.height <= layout.width);
        assert!(report.rect_fill > 0.2, "{report:?}");
        assert!(report.chart_fill <= report.rect_fill);
        assert_eq!(layout.entries[0].texels_per_metre, 16.0);
        let map = regia_asset::lightmap::Lightmap::from_irradiance(
            layout.clone(),
            &vec![[1.0; 3]; (layout.width * layout.height) as usize],
            vec![1; (layout.width * layout.height) as usize],
            regia_asset::lightmap::LightmapBakeInfo::default(),
        )
        .expect("the layout is a valid lightmap (rects inside, no overlaps, UVs in 0..1)");
        for (e, entry) in map.entries.iter().enumerate() {
            let s = entry.scale_offset;
            assert!((s[0] * layout.width as f32 - entry.rect[2] as f32).abs() < 1e-3);
            assert!((s[3] * layout.height as f32 - entry.rect[1] as f32).abs() < 1e-3);
            assert_no_overlap(&layout, e);
        }
    }

    #[test]
    fn an_atlas_too_small_scales_the_density_down_then_gives_up() {
        let (p, i) = cube(10.0);
        let s = LightmapUvSettings {
            texels_per_metre: 32.0,
            max_atlas: 256,
            ..LightmapUvSettings::default()
        };
        let (layout, report) =
            build_lightmap_layout(&[input("big", &p, &i)], &s).expect("fits after scaling");
        assert!(report.fit_scale < 1.0 && report.attempts > 1, "{report:?}");
        assert!(layout.width <= 256 && layout.height <= 256);
        assert!((layout.entries[0].texels_per_metre - 32.0 * report.fit_scale).abs() < 1e-3);

        // 2000 separate triangles each need at least a 6×6 rect: they never fit 16×16.
        let mut p = Vec::new();
        let mut idx = Vec::new();
        for k in 0..2000u32 {
            let x = k as f32 * 3.0;
            p.extend([[x, 0.0, 0.0], [x + 1.0, 0.0, 0.0], [x, 1.0, (k % 7) as f32]]);
            idx.extend([3 * k, 3 * k + 1, 3 * k + 2]);
        }
        let tiny = LightmapUvSettings {
            max_atlas: 16,
            ..LightmapUvSettings::default()
        };
        let e = build_lightmap_layout(&[input("dust", &p, &idx)], &tiny).expect_err("full");
        assert_eq!(e.code, "lightmap_uv_atlas_full");
    }

    #[test]
    fn bad_inputs_have_codes() {
        let (p, i) = cube(1.0);
        let s = LightmapUvSettings::default();
        let code =
            |r: Result<(LightmapLayout, LightmapUvReport), LightmapUvError>| r.expect_err("error").code;
        assert_eq!(code(build_lightmap_layout(&[], &s)), "lightmap_uv_empty");
        assert_eq!(
            code(build_lightmap_layout(
                &[input("a", &p, &i), input("a", &p, &i)],
                &s
            )),
            "lightmap_uv_invalid_mesh"
        );
        assert_eq!(
            code(build_lightmap_layout(&[input("a", &p, &i[..4])], &s)),
            "lightmap_uv_invalid_mesh"
        );
        assert_eq!(
            code(build_lightmap_layout(&[input("a", &p[..3], &i)], &s)),
            "lightmap_uv_invalid_mesh"
        );
        assert_eq!(
            code(build_lightmap_layout(&[input("a", &p, &[])], &s)),
            "lightmap_uv_empty"
        );
        let zero = LightmapUvSettings {
            texels_per_metre: 0.0,
            ..s
        };
        assert_eq!(
            code(build_lightmap_layout(&[input("a", &p, &i)], &zero)),
            "lightmap_uv_settings"
        );
        let nopad = LightmapUvSettings { padding: 0, ..s };
        assert_eq!(
            code(build_lightmap_layout(&[input("a", &p, &i)], &nopad)),
            "lightmap_uv_settings"
        );
    }

    #[test]
    fn degenerate_triangles_are_counted_and_kept() {
        let (mut p, mut i) = cube(1.0);
        let base = p.len() as u32;
        p.extend([[0.5, 0.5, 0.5], [0.5, 0.5, 0.5], [0.5, 0.5, 0.5]]);
        i.extend([base, base + 1, base + 2]);
        let (layout, report) =
            build_lightmap_layout(&[input("cube", &p, &i)], &LightmapUvSettings::default()).expect("layout");
        assert_eq!(report.degenerate_triangles, 1);
        assert_eq!(layout.unwraps[0].indices.len(), i.len());
    }

    #[test]
    fn the_layout_is_deterministic() {
        let (p, i) = sphere(2.0, 20, 10);
        let s = LightmapUvSettings::default();
        let a = build_lightmap_layout(&[input("s", &p, &i)], &s).expect("a");
        let b = build_lightmap_layout(&[input("s", &p, &i)], &s).expect("b");
        assert_eq!(a, b);
    }

    #[test]
    fn skyline_packs_without_overlap_and_refuses_too_wide() {
        let rects: Vec<PackRect> = (0..40)
            .map(|id| PackRect {
                w: 3 + (id as u32 * 7) % 11,
                h: 2 + (id as u32 * 5) % 9,
                id,
            })
            .collect();
        let (at, used) = skyline_pack(&rects, 32).expect("packs");
        for (a, (ra, pa)) in rects.iter().zip(&at).enumerate() {
            assert!(pa[0] + ra.w <= 32 && pa[1] + ra.h <= used);
            for (rb, pb) in rects.iter().zip(&at).skip(a + 1) {
                let apart = pa[0] + ra.w <= pb[0]
                    || pb[0] + rb.w <= pa[0]
                    || pa[1] + ra.h <= pb[1]
                    || pb[1] + rb.h <= pa[1];
                assert!(apart);
            }
        }
        assert!(skyline_pack(&[PackRect { w: 33, h: 1, id: 0 }], 32).is_none());
    }

    /// A curved walk can drag a chart's area-weighted normal past its seed (the seed is never
    /// ejected by the growth). Here twenty members face 95° from the seed: their normal ends 92°
    /// from it, and projecting the seed on it would fold it. The seed then charts alone on its own
    /// normal and the others chart again.
    #[test]
    fn a_chart_whose_normal_drifts_past_its_seed_leaves_the_seed_alone() {
        let tilt = 95f32.to_radians();
        let mut normals = vec![Vec3::Y];
        normals.extend(std::iter::repeat_n(Vec3::new(tilt.sin(), tilt.cos(), 0.0), 20));
        let areas = vec![1.0f64; normals.len()];
        let mut chart_of = vec![Some(0usize); normals.len()];
        let list: Vec<u32> = (0..normals.len() as u32).collect();
        let (kept, n) = settle_chart(list, 0, &normals, &areas, &mut chart_of);
        assert_eq!(kept, [0]);
        assert_eq!(n, Vec3::Y);
        assert_eq!(chart_of[0], Some(0));
        assert!(
            chart_of[1..].iter().all(Option::is_none),
            "the others chart again"
        );

        // A chart inside the cone keeps every member and its area-weighted normal.
        let normals = [Vec3::Y, Vec3::new(0.0, 0.8, 0.6)];
        let mut chart_of = [Some(0usize); 2];
        let (kept, n) = settle_chart(vec![0, 1], 0, &normals, &[1.0, 1.0], &mut chart_of);
        assert_eq!(kept, [0, 1]);
        assert!((n - Vec3::new(0.0, 1.8, 0.6).normalize()).length() < 1e-6, "{n}");

        // A member far off the settled normal is ejected and the normal recomputed without it.
        let normals = [Vec3::Y, Vec3::Y, Vec3::new(0.0, -0.2, 0.98).normalize()];
        let mut chart_of = [Some(0usize); 3];
        let (kept, n) = settle_chart(vec![0, 1, 2], 0, &normals, &[1.0, 1.0, 0.1], &mut chart_of);
        assert_eq!(kept, [0, 1]);
        assert_eq!(n, Vec3::Y);
        assert_eq!(chart_of[2], None);
    }
}
