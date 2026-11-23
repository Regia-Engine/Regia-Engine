// SPDX-License-Identifier: MPL-2.0
//! The lightmap layout on the UV atlas generator ([`crate::atlas`], ADR-2024): the density rules of
//! the module above (`docs/formats/lightmap.md` § 2) with the atlas's charts, flattening and
//! footprint packing in place of planar charts in rectangles. Curved meshes get a low-stretch
//! unwrap (least squares conformal charts) and no space goes to a chart's empty bounding-box
//! corners.
//!
//! Every mesh is charted once ([`crate::atlas::flatten`]). For a density `texels_per_metre ·
//! density_scale · fit_scale` it is then packed at exactly that density into the smallest square
//! that holds it, doubling from an estimate, and the square is cropped to the charts' padded
//! footprints: that is the entry's rectangle, its local UV2 0..1 over it. The rectangles go into
//! the atlas as in [`super::build_lightmap_layout`], and the atlas fit is the same rule: when the
//! rectangles do not fit one atlas of `max_atlas`, they go to pages up to `max_pages` (ADR-3300),
//! and only past that budget does `fit_scale` drop by 0.8 and everything is laid out again.
//!
//! Deterministic: the same inputs give the same layout for any `threads` (the generator's own
//! guarantee, `book/agents/atlas.md`).

use regia_asset::lightmap::{LightmapEntry, LightmapLayout, MAX_LIGHTMAP_SIZE, MeshUnwrap};

use super::{
    FIT_STEP, LightmapUvError, LightmapUvReport, LightmapUvSettings, MAX_FIT_ATTEMPTS, PackRect, UnwrapInput,
    check_mesh, check_settings, place_rects,
};
use crate::atlas::{self, AtlasError, AtlasMesh, AtlasOptions, AtlasResult, AtlasWarning, FlattenedMesh};

/// Smallest square a mesh is packed into, texels.
const MIN_MESH_SQUARE: u32 = 16;
/// Share of the first square the charts are assumed to fill, before the footprints' padding.
const FIRST_SQUARE_FILL: f64 = 0.5;

fn atlas_error(input: &UnwrapInput<'_>, e: &AtlasError) -> LightmapUvError {
    let code = match e {
        AtlasError::NoTriangles => "lightmap_uv_empty",
        // The mesh is fine: the padding leaves no room in the atlas, as on the planar path.
        AtlasError::InvalidOption {
            option: "resolution" | "padding",
            ..
        } => "lightmap_uv_atlas_full",
        _ => "lightmap_uv_invalid_mesh",
    };
    LightmapUvError {
        code,
        message: format!("{}: {} ({})", input.name, e, e.code()),
    }
}

/// One mesh at one density.
struct MeshPacked {
    result: AtlasResult,
    /// The union of the charts' padded footprints, `[x, y, width, height]` in the square's texels.
    rect: [u32; 4],
}

fn footprint(result: &AtlasResult) -> [u32; 4] {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for c in &result.charts {
        let [x, y, w, h] = c.rect;
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x + w);
        y1 = y1.max(y + h);
    }
    if x0 >= x1 || y0 >= y1 {
        return [0, 0, 1, 1];
    }
    [x0, y0, x1 - x0, y1 - y0]
}

/// Packs `mesh` at exactly `density`: the smallest doubling of the estimate it fits, or `None` past
/// `max_atlas`.
fn pack_mesh(
    input: &UnwrapInput<'_>,
    mesh: &FlattenedMesh<'_>,
    density: f32,
    s: &LightmapUvSettings,
) -> Result<Option<MeshPacked>, LightmapUvError> {
    let d = f64::from(density);
    let estimate = (mesh.surface_area() * d * d / FIRST_SQUARE_FILL).sqrt();
    let first = (estimate.ceil() as u32)
        .saturating_add(4 * s.padding)
        .max(MIN_MESH_SQUARE)
        .max(2 * s.padding + 3);
    let mut square = first.min(s.max_atlas);
    loop {
        if let Some(result) = mesh
            .pack_exact(square, density, s.padding)
            .map_err(|e| atlas_error(input, &e))?
        {
            let rect = footprint(&result);
            return Ok(Some(MeshPacked { result, rect }));
        }
        if square >= s.max_atlas {
            return Ok(None);
        }
        square = square.saturating_mul(2).min(s.max_atlas);
    }
}

/// The unwrap of one mesh in its rectangle: local UV2 0..1 over `rect`.
fn unwrap_of(p: &MeshPacked) -> MeshUnwrap {
    let [x, y, w, h] = p.rect.map(f64::from);
    let (rw, rh) = (f64::from(p.result.width), f64::from(p.result.height));
    MeshUnwrap {
        xref: p.result.vertices.iter().map(|v| v.xref).collect(),
        uv: p
            .result
            .vertices
            .iter()
            .map(|v| {
                let u = (f64::from(v.uv[0]) * rw - x) / w;
                let t = (f64::from(v.uv[1]) * rh - y) / h;
                [u.clamp(0.0, 1.0) as f32, t.clamp(0.0, 1.0) as f32]
            })
            .collect(),
        indices: p.result.indices.clone(),
    }
}

/// Lays every input out at `fit`, or `None` when something would pass `max_atlas`.
fn try_layout(
    inputs: &[UnwrapInput<'_>],
    meshes: &[FlattenedMesh<'_>],
    s: &LightmapUvSettings,
    fit: f32,
    workers: usize,
) -> Result<Option<(LightmapLayout, LightmapUvReport)>, LightmapUvError> {
    // One mesh per task. A task skips only when a mesh before it already failed: every mesh before
    // the first failure in index order still runs, so the first failure found below is the one a
    // serial loop stops at, for any worker count and any timing.
    let first_failed = std::sync::atomic::AtomicUsize::new(usize::MAX);
    let results = atlas::parallel_map(inputs.len(), workers, |k| {
        if first_failed.load(std::sync::atomic::Ordering::Relaxed) < k {
            return None;
        }
        let input = &inputs[k];
        let density = s.texels_per_metre * input.density_scale * fit;
        let r = pack_mesh(input, &meshes[k], density, s);
        if !matches!(r, Ok(Some(_))) {
            first_failed.fetch_min(k, std::sync::atomic::Ordering::Relaxed);
        }
        Some(r)
    });
    let mut packed = Vec::with_capacity(inputs.len());
    for r in results {
        match r {
            Some(Ok(Some(p))) => packed.push(p),
            Some(Err(e)) => return Err(e),
            Some(Ok(None)) | None => return Ok(None),
        }
    }
    let rects: Vec<PackRect> = packed
        .iter()
        .enumerate()
        .map(|(id, p)| PackRect {
            w: p.rect[2],
            h: p.rect[3],
            id,
        })
        .collect();
    let Some((at, width, height, pages)) = place_rects(&rects, s) else {
        return Ok(None);
    };
    let (aw, ah) = (f64::from(width), f64::from(height));
    let mut entries = Vec::with_capacity(inputs.len());
    let mut unwraps = Vec::with_capacity(inputs.len());
    let (mut chart_texels, mut rect_texels) = (0.0f64, 0u64);
    for ((input, p), &(page, [x, y])) in inputs.iter().zip(&packed).zip(&at) {
        let [_, _, mw, mh] = p.rect;
        rect_texels += u64::from(mw) * u64::from(mh);
        chart_texels +=
            f64::from(p.result.utilization) * f64::from(p.result.width) * f64::from(p.result.height);
        unwraps.push(unwrap_of(p));
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
            charts: p.result.charts.len() as u32,
            area_m2: super::mesh_area(input) as f32,
            texels_per_metre: s.texels_per_metre * input.density_scale * fit,
            key: None,
            seams: None,
            page,
        });
    }
    let atlas_area = (aw * ah * f64::from(pages)).max(1.0);
    let report = LightmapUvReport {
        width,
        height,
        pages,
        charts: packed.iter().map(|p| p.result.charts.len()).sum(),
        fit_scale: fit,
        attempts: 0,
        chart_fill: (chart_texels / atlas_area) as f32,
        rect_fill: (rect_texels as f64 / atlas_area) as f32,
        degenerate_triangles: packed
            .iter()
            .map(|p| {
                p.result
                    .warnings
                    .iter()
                    .filter(|w| matches!(w, AtlasWarning::DegenerateTriangle { .. }))
                    .count()
            })
            .sum(),
        generator: "atlas",
    };
    let layout = LightmapLayout {
        width,
        height,
        density: s.texels_per_metre,
        padding: s.padding,
        entries,
        unwraps,
    };
    Ok(Some((layout, report)))
}

/// Lays out every input in one atlas with the UV atlas generator (module docs). `threads` is the
/// generator's worker count (0 = all cores); the result is the same for any value. With more
/// meshes than workers each worker charts and packs whole meshes; with fewer, the workers share
/// each mesh's charts (ADR-2955).
///
/// # Errors
///
/// [`LightmapUvError`]: `lightmap_uv_settings`, `lightmap_uv_invalid_mesh` (a bad mesh or a duplicate
/// name), `lightmap_uv_empty` (nothing to unwrap, or a mesh with no usable triangle) or
/// `lightmap_uv_atlas_full`.
pub fn build_atlas_layout(
    inputs: &[UnwrapInput<'_>],
    settings: &LightmapUvSettings,
    threads: usize,
) -> Result<(LightmapLayout, LightmapUvReport), LightmapUvError> {
    let _span = tracing::info_span!("lightmap_atlas_layout", meshes = inputs.len()).entered();
    check_settings(settings)?;
    let mut seen = std::collections::HashSet::new();
    for i in inputs {
        check_mesh(i)?;
        if !seen.insert(i.name) {
            return Err(LightmapUvError {
                code: "lightmap_uv_invalid_mesh",
                message: format!("{}: duplicate entry name", i.name),
            });
        }
    }
    if inputs.is_empty() {
        return Err(LightmapUvError {
            code: "lightmap_uv_empty",
            message: "no meshes to unwrap".into(),
        });
    }
    // Many small meshes: one worker per mesh, each mesh's stages on its thread (spawning workers
    // for a mesh of a few charts costs more than the charts). Few large meshes: the workers go to
    // each mesh's charts. A mesh's result is the same for any thread count, so the layout is too.
    let workers = atlas::worker_count(threads);
    let outer = workers.min(inputs.len()).max(1);
    let options = AtlasOptions {
        threads: (workers / outer).max(1),
        resolution: settings.max_atlas.clamp(16, MAX_LIGHTMAP_SIZE),
        padding: settings.padding,
        min_chart_texels: settings.min_chart_texels,
        ..AtlasOptions::default()
    };
    let meshes = atlas::parallel_map(inputs.len(), outer, |k| {
        let i = &inputs[k];
        atlas::flatten(&AtlasMesh::new(i.positions, i.indices), &options).map_err(|e| atlas_error(i, &e))
    })
    .into_iter()
    .collect::<Result<Vec<_>, _>>()?;
    // Fit attempts that cannot hold the charts' area are not worth packing, but they count: the
    // density the rule reaches is the same as packing them one by one. Every page holds that area.
    let atlas_area =
        f64::from(settings.max_atlas) * f64::from(settings.max_atlas) * f64::from(settings.max_pages.max(1));
    let area_at = |fit: f32| -> f64 {
        inputs
            .iter()
            .zip(&meshes)
            .map(|(i, m)| {
                let d = f64::from(settings.texels_per_metre * i.density_scale * fit);
                m.surface_area() * d * d
            })
            .sum()
    };
    let mut fit = 1.0f32;
    for attempt in 1..=MAX_FIT_ATTEMPTS {
        if area_at(fit) <= atlas_area
            && let Some((layout, mut report)) = try_layout(inputs, &meshes, settings, fit, outer)?
        {
            report.attempts = attempt;
            return Ok((layout, report));
        }
        fit *= FIT_STEP;
    }
    Err(LightmapUvError {
        code: "lightmap_uv_atlas_full",
        message: format!(
            "the charts do not fit {1} page(s) of {0}×{0} even at {fit:.4} of the density: raise max_atlas or max_pages, or lower texels_per_metre",
            settings.max_atlas, settings.max_pages
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::super::build_lightmap_layout;
    use super::*;

    /// Test geometry: positions and triangle indices.
    type Geometry = (Vec<[f32; 3]>, Vec<u32>);

    /// A unit-radius latitude/longitude sphere of `size` metres radius.
    fn sphere(segments: u32, size: f32) -> Geometry {
        let rings = (segments / 2).max(2);
        let mut p = Vec::new();
        for r in 0..=rings {
            let theta = std::f32::consts::PI * r as f32 / rings as f32;
            for k in 0..=segments {
                let phi = std::f32::consts::TAU * k as f32 / segments as f32;
                let (st, ct) = theta.sin_cos();
                let (sp, cp) = phi.sin_cos();
                p.push([size * st * cp, size * ct, size * st * sp]);
            }
        }
        let row = segments + 1;
        let mut idx = Vec::new();
        for r in 0..rings {
            for k in 0..segments {
                let (a, b) = (r * row + k, (r + 1) * row + k);
                if r != 0 {
                    idx.extend([a, a + 1, b]);
                }
                if r != rings - 1 {
                    idx.extend([a + 1, b + 1, b]);
                }
            }
        }
        (p, idx)
    }

    /// A flat `n`×`n` metre floor of one quad.
    fn floor(n: f32) -> Geometry {
        (
            vec![[0.0, 0.0, 0.0], [0.0, 0.0, n], [n, 0.0, n], [n, 0.0, 0.0]],
            vec![0, 1, 2, 0, 2, 3],
        )
    }

    fn settings() -> LightmapUvSettings {
        LightmapUvSettings {
            texels_per_metre: 16.0,
            max_atlas: 2048,
            ..LightmapUvSettings::default()
        }
    }

    fn input<'a>(name: &'a str, g: &'a Geometry, scale: f32) -> UnwrapInput<'a> {
        UnwrapInput {
            name,
            positions: &g.0,
            indices: &g.1,
            density_scale: scale,
        }
    }

    /// Twice the signed area of a UV triangle, in texels².
    fn uv_area(u: &MeshUnwrap, t: &[u32; 3], w: f32, h: f32) -> f32 {
        let q = |k: usize| {
            let p = u.uv[t[k] as usize];
            [p[0] * w, p[1] * h]
        };
        let (a, b, c) = (q(0), q(1), q(2));
        (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])
    }

    /// The layout is one the baker and the file format accept: every source triangle is kept in
    /// order, rectangles stay apart and every entry records its density.
    #[test]
    fn a_scene_lays_out_into_entries_the_baker_accepts() {
        let (s, f) = (sphere(32, 1.0), floor(8.0));
        let (layout, report) = build_atlas_layout(
            &[input("sphere", &s, 1.0), input("floor", &f, 1.0)],
            &settings(),
            2,
        )
        .expect("layout");
        assert_eq!(report.generator, "atlas");
        assert_eq!(report.fit_scale, 1.0);
        assert_eq!(report.attempts, 1);
        assert!(layout.width.is_power_of_two());
        assert!(layout.height.is_multiple_of(4));
        assert_eq!(layout.entries.len(), 2);
        for (((e, u), g), name) in layout
            .entries
            .iter()
            .zip(&layout.unwraps)
            .zip([&s, &f])
            .zip(["sphere", "floor"])
        {
            assert_eq!(e.name, name);
            assert_eq!(e.source_vertices as usize, g.0.len());
            assert_eq!(u.indices.len(), g.1.len(), "every source triangle stays");
            assert!((e.texels_per_metre - 16.0).abs() < 1e-5);
            assert!(e.charts >= 1);
            // The unwrap names the source triangle in the source's order.
            for (t, src) in u.indices.as_chunks::<3>().0.iter().zip(g.1.as_chunks::<3>().0) {
                let got = t.map(|i| u.xref[i as usize]);
                assert_eq!(&got, src);
            }
            assert!(u.uv.iter().flatten().all(|v| (0.0..=1.0).contains(v)));
        }
        let [a, b] = [layout.entries[0].rect, layout.entries[1].rect];
        assert!(
            a[0] + a[2] <= b[0] || b[0] + b[2] <= a[0] || a[1] + a[3] <= b[1] || b[1] + b[3] <= a[1],
            "{a:?} {b:?}"
        );
        // The floor takes 16 texels per metre: its 8 m side is 128 texels, plus the gutters.
        let [_, _, fw, fh] = layout.entries[1].rect;
        assert!(fw.max(fh) >= 128, "{fw}×{fh}");
        assert!(fw.max(fh) <= 128 + 12, "{fw}×{fh}");
    }

    /// The density rule: `density_scale` multiplies one mesh's texels per metre, and the rectangle
    /// follows it.
    /// The density rule's floor (`lightmap_uv` step 3) holds here too: a prop far smaller than a
    /// texel still gets a chart of `min_chart_texels` a side.
    #[test]
    fn a_tiny_chart_keeps_the_smallest_side() {
        let positions = [[0.0, 0.0, 0.0], [0.01, 0.0, 0.0], [0.0, 0.01, 0.0]];
        let indices = [0u32, 1, 2];
        let input = UnwrapInput {
            name: "tiny",
            positions: &positions,
            indices: &indices,
            density_scale: 1.0,
        };
        for least in [1u32, 2, 4] {
            let settings = LightmapUvSettings {
                texels_per_metre: 1.0,
                min_chart_texels: least,
                ..LightmapUvSettings::default()
            };
            let (layout, _) =
                build_atlas_layout(std::slice::from_ref(&input), &settings, 1).expect("lays out");
            let (entry, unwrap) = (&layout.entries[0], &layout.unwraps[0]);
            let side = |axis: usize, size: u32| {
                let values = || unwrap.uv.iter().map(|uv| uv[axis]);
                let (low, high) = (
                    values().fold(f32::INFINITY, f32::min),
                    values().fold(f32::NEG_INFINITY, f32::max),
                );
                (high - low) * size as f32
            };
            let (w, h) = (side(0, entry.rect[2]), side(1, entry.rect[3]));
            assert!(
                w >= least as f32 - 1e-3 && h >= least as f32 - 1e-3,
                "{least}: {w} x {h}"
            );
        }
    }

    /// A padding that leaves no room in the atlas says the atlas is full, as the planar charts do:
    /// nothing is wrong with the mesh.
    #[test]
    fn a_padding_with_no_room_is_a_full_atlas() {
        let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let indices = [0u32, 1, 2];
        let input = UnwrapInput {
            name: "tri",
            positions: &positions,
            indices: &indices,
            density_scale: 1.0,
        };
        let settings = LightmapUvSettings {
            padding: 8,
            max_atlas: 16,
            ..LightmapUvSettings::default()
        };
        let e = build_atlas_layout(std::slice::from_ref(&input), &settings, 1).expect_err("no room");
        assert_eq!(e.code, "lightmap_uv_atlas_full", "{}", e.message);
    }

    #[test]
    fn density_scale_follows_the_rule() {
        let f = floor(8.0);
        let (one, _) = build_atlas_layout(&[input("floor", &f, 1.0)], &settings(), 1).expect("layout");
        let (two, _) = build_atlas_layout(&[input("floor", &f, 2.0)], &settings(), 1).expect("layout");
        assert!((two.entries[0].texels_per_metre - 32.0).abs() < 1e-5);
        let side = |l: &LightmapLayout| l.entries[0].rect[2].max(l.entries[0].rect[3]);
        assert!(side(&two) > side(&one) * 3 / 2, "{} {}", side(&two), side(&one));
    }

    /// The atlas fit: past `max_atlas` the density drops by 0.8 steps, reports it, and the charts
    /// then fit; a hopeless one is `lightmap_uv_atlas_full`.
    #[test]
    fn the_atlas_fit_lowers_the_density_by_the_rule() {
        let f = floor(64.0);
        let small = LightmapUvSettings {
            texels_per_metre: 16.0,
            max_atlas: 256,
            ..LightmapUvSettings::default()
        };
        let (layout, report) = build_atlas_layout(&[input("floor", &f, 1.0)], &small, 1).expect("layout");
        assert!(report.fit_scale < 1.0, "{report:?}");
        assert!(report.attempts > 1, "{report:?}");
        assert!(layout.width <= 256 && layout.height <= 256);
        assert!((layout.entries[0].texels_per_metre - 16.0 * report.fit_scale).abs() < 1e-3);
        // The steps are the rule's: fit_scale = 0.8^(attempts - 1).
        let want = 0.8f32.powi(report.attempts as i32 - 1);
        assert!((report.fit_scale - want).abs() < 1e-5, "{report:?}");

        let huge = floor(100_000.0);
        let tiny = LightmapUvSettings {
            texels_per_metre: 4096.0,
            max_atlas: 64,
            ..LightmapUvSettings::default()
        };
        let err = build_atlas_layout(&[input("floor", &huge, 1.0)], &tiny, 1).expect_err("does not fit");
        assert_eq!(err.code, "lightmap_uv_atlas_full");
    }

    /// Chart triangles keep at least `padding` texels from the rectangle's edge, so the bake's
    /// gutter dilation and the runtime's lookups stay inside the entry.
    #[test]
    fn charts_keep_the_gutter_inside_the_rectangle() {
        let s = sphere(24, 2.0);
        let (layout, _) = build_atlas_layout(&[input("sphere", &s, 1.0)], &settings(), 1).expect("layout");
        let [_, _, w, h] = layout.entries[0].rect;
        let pad = settings().padding as f32;
        for uv in &layout.unwraps[0].uv {
            let (x, y) = (uv[0] * w as f32, uv[1] * h as f32);
            assert!(x >= pad - 1e-3 && x <= w as f32 - pad + 1e-3, "{x} of {w}");
            assert!(y >= pad - 1e-3 && y <= h as f32 - pad + 1e-3, "{y} of {h}");
        }
    }

    /// Every chart triangle has positive UV area after the crop, and the charts do not overlap: the
    /// total UV area is the chart area at the entry's density (a stretch-free map's area).
    #[test]
    fn a_curved_mesh_unwraps_with_little_stretch_where_planar_charts_stretch() {
        let s = sphere(48, 1.0);
        let i = input("sphere", &s, 1.0);
        let (atlas, report) = build_atlas_layout(&[i], &settings(), 1).expect("layout");
        let [_, _, w, h] = atlas.entries[0].rect;
        assert_eq!(report.degenerate_triangles, 0);
        // The share of the surface's texels the unwrap's triangles cover. Flattening a curved chart
        // onto its plane shrinks it, so a planar chart resolves the surface below the density asked
        // for and its rectangle comes out smaller; conformal charts keep each chart's area.
        let share = |l: &LightmapLayout| {
            let e = &l.entries[0];
            let [_, _, w, h] = e.rect.map(|v| v as f32);
            let u = &l.unwraps[0];
            let uv_total: f32 = u
                .indices
                .as_chunks::<3>()
                .0
                .iter()
                .map(|t| {
                    let a = uv_area(u, t, w, h);
                    assert!(a.abs() > 0.0, "a degenerate chart triangle");
                    a.abs() * 0.5
                })
                .sum();
            uv_total / (e.area_m2 * e.texels_per_metre * e.texels_per_metre)
        };
        let atlas_share = share(&atlas);
        assert!(
            (atlas_share - 1.0).abs() < 0.1,
            "UV area {atlas_share} of the surface's, rect {w}×{h}"
        );
        let (charts, charts_report) = build_lightmap_layout(&[i], &settings()).expect("layout");
        assert_eq!(charts_report.generator, "charts");
        let planar_share = share(&charts);
        assert!(
            (atlas_share - 1.0).abs() < (planar_share - 1.0).abs(),
            "the atlas keeps the density closer: atlas {atlas_share}, planar charts {planar_share}"
        );
    }

    /// The same inputs give the same layout for any worker count.
    #[test]
    fn the_layout_does_not_depend_on_the_thread_count() {
        let (s, f) = (sphere(32, 1.5), floor(6.0));
        let inputs = [input("sphere", &s, 1.0), input("floor", &f, 2.0)];
        let (one, r1) = build_atlas_layout(&inputs, &settings(), 1).expect("layout");
        let (three, r3) = build_atlas_layout(&inputs, &settings(), 3).expect("layout");
        assert_eq!(one, three);
        assert_eq!(r1, r3);
        let (again, _) = build_atlas_layout(&inputs, &settings(), 1).expect("layout");
        assert_eq!(one, again);
    }

    /// Many meshes go one per worker (ADR-2955): the layout, the report and the atlas fit are the
    /// same for one worker and for more workers than meshes.
    #[test]
    fn many_meshes_lay_out_the_same_on_any_worker_count() {
        let shapes: Vec<(String, Geometry)> = (0..24u32)
            .map(|k| {
                let g = if k.is_multiple_of(3) {
                    sphere(8 + 2 * (k % 5), 0.5 + 0.1 * k as f32)
                } else {
                    floor(1.0 + k as f32 * 0.7)
                };
                (format!("mesh_{k}"), g)
            })
            .collect();
        let inputs: Vec<UnwrapInput<'_>> = shapes
            .iter()
            .enumerate()
            .map(|(k, (name, g))| input(name, g, if k.is_multiple_of(4) { 2.0 } else { 1.0 }))
            .collect();
        let (one, r1) = build_atlas_layout(&inputs, &settings(), 1).expect("layout");
        assert_eq!(one.entries.len(), 24);
        for threads in [4, 64] {
            let (more, r) = build_atlas_layout(&inputs, &settings(), threads).expect("layout");
            assert_eq!(one, more, "threads {threads}");
            assert_eq!(r1, r, "threads {threads}");
        }
        // A small atlas: the fit steps the density down the same way on any worker count.
        let small = LightmapUvSettings {
            max_atlas: 256,
            ..settings()
        };
        let (one, r1) = build_atlas_layout(&inputs, &small, 1).expect("layout");
        assert!(r1.fit_scale < 1.0, "{r1:?}");
        let (more, r) = build_atlas_layout(&inputs, &small, 64).expect("layout");
        assert_eq!(one, more);
        assert_eq!(r1, r);
    }

    /// With meshes on several workers the error is still the first bad mesh's, in input order.
    #[test]
    fn the_first_bad_mesh_in_input_order_is_the_error_on_any_worker_count() {
        let f = floor(4.0);
        let line = (vec![[0.0; 3], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]], vec![0u32, 1, 2]);
        let names: Vec<String> = (0..16).map(|k| format!("m{k}")).collect();
        let inputs: Vec<UnwrapInput<'_>> = names
            .iter()
            .enumerate()
            .map(|(k, n)| input(n, if k == 5 || k == 11 { &line } else { &f }, 1.0))
            .collect();
        for threads in [1, 3, 16] {
            let err = build_atlas_layout(&inputs, &settings(), threads).expect_err("bad mesh");
            assert_eq!(err.code, "lightmap_uv_empty");
            assert!(
                err.message.starts_with("m5:"),
                "threads {threads}: {}",
                err.message
            );
        }
    }

    #[test]
    fn errors_carry_the_generator_s_codes() {
        let f = floor(4.0);
        let bad = LightmapUvSettings {
            padding: 0,
            ..settings()
        };
        assert_eq!(
            build_atlas_layout(&[input("floor", &f, 1.0)], &bad, 1)
                .expect_err("padding")
                .code,
            "lightmap_uv_settings"
        );
        assert_eq!(
            build_atlas_layout(&[], &settings(), 1).expect_err("empty").code,
            "lightmap_uv_empty"
        );
        assert_eq!(
            build_atlas_layout(&[input("a", &f, 1.0), input("a", &f, 1.0)], &settings(), 1)
                .expect_err("duplicate")
                .code,
            "lightmap_uv_invalid_mesh"
        );
        // Every face degenerate: three points on a line.
        let line = (vec![[0.0; 3], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]], vec![0u32, 1, 2]);
        assert_eq!(
            build_atlas_layout(&[input("line", &line, 1.0)], &settings(), 1)
                .expect_err("no usable triangle")
                .code,
            "lightmap_uv_empty"
        );
    }

    /// Ten 7 m floors at 16 texels per meter on 256-texel pages: four rectangles fill a page, so
    /// the level needs two and a half pages.
    fn ten_floors() -> Vec<(String, Geometry)> {
        (0..10).map(|k| (format!("floor_{k}"), floor(7.0))).collect()
    }

    fn paged(max_pages: u32) -> LightmapUvSettings {
        LightmapUvSettings {
            texels_per_metre: 16.0,
            max_atlas: 256,
            max_pages,
            ..LightmapUvSettings::default()
        }
    }

    /// Past one atlas the layout opens pages at the asked density (ADR-3300): `fit_scale` stays 1,
    /// every entry keeps its texels per meter, rectangles stay inside their page and apart from the
    /// other rectangles of that page, and a mesh's charts are one rectangle on one page.
    #[test]
    fn a_level_past_one_atlas_opens_pages_at_the_asked_density() {
        let shapes = ten_floors();
        let inputs: Vec<UnwrapInput<'_>> = shapes.iter().map(|(n, g)| input(n, g, 1.0)).collect();
        let (layout, report) = build_atlas_layout(&inputs, &paged(4), 1).expect("layout");
        assert_eq!(report.fit_scale, 1.0, "{report:?}");
        assert_eq!(report.attempts, 1);
        assert_eq!(report.pages, 3, "{report:?}");
        assert_eq!(layout.pages(), 3);
        assert_eq!((layout.width, report.width), (256, 256));
        assert!(layout.height <= 256 && layout.height.is_multiple_of(4));
        for (k, e) in layout.entries.iter().enumerate() {
            assert!((e.texels_per_metre - 16.0).abs() < 1e-5, "{e:?}");
            let [x, y, w, h] = e.rect;
            assert!(x + w <= layout.width && y + h <= layout.height, "{e:?}");
            for o in &layout.entries[..k] {
                let apart = o.page != e.page
                    || x + w <= o.rect[0]
                    || o.rect[0] + o.rect[2] <= x
                    || y + h <= o.rect[1]
                    || o.rect[1] + o.rect[3] <= y;
                assert!(apart, "{e:?} overlaps {o:?}");
            }
            // The entry's UV maps into its rectangle on its page.
            let so = e.scale_offset;
            assert!(
                so[2] + so[0] <= 1.0 + 1e-6 && so[3] + so[1] <= 1.0 + 1e-6,
                "{so:?}"
            );
        }
        // Pages fill in order: the first two are full (four each), the last has the rest.
        let on = |p: u32| layout.entries.iter().filter(|e| e.page == p).count();
        assert_eq!((on(0), on(1), on(2)), (4, 4, 2));
    }

    /// The page budget: with one page the old rule scales the density down (`fit_scale` below 1,
    /// one page, the same layout as the library default of one page); with two pages the density
    /// drops only as far as two pages need; a budget that holds every page leaves it at 1.
    #[test]
    fn only_past_the_page_budget_does_the_density_drop() {
        let shapes = ten_floors();
        let inputs: Vec<UnwrapInput<'_>> = shapes.iter().map(|(n, g)| input(n, g, 1.0)).collect();
        let (one, r1) = build_atlas_layout(&inputs, &paged(1), 1).expect("layout");
        assert!(r1.fit_scale < 1.0 && r1.pages == 1, "{r1:?}");
        assert!(one.entries.iter().all(|e| e.page == 0));
        let default = LightmapUvSettings {
            texels_per_metre: 16.0,
            max_atlas: 256,
            ..LightmapUvSettings::default()
        };
        assert_eq!(default.max_pages, 1, "the library keeps one page unless asked");
        let (same, rs) = build_atlas_layout(&inputs, &default, 1).expect("layout");
        assert_eq!((one, r1.clone()), (same, rs));
        let (two, r2) = build_atlas_layout(&inputs, &paged(2), 1).expect("layout");
        assert_eq!(r2.pages, 2, "{r2:?}");
        assert!(r2.fit_scale < 1.0 && r2.fit_scale > r1.fit_scale, "{r1:?} {r2:?}");
        assert!(
            two.entries
                .iter()
                .all(|e| (e.texels_per_metre - 16.0 * r2.fit_scale).abs() < 1e-3)
        );
        let (_, r8) = build_atlas_layout(&inputs, &paged(8), 1).expect("layout");
        assert_eq!((r8.fit_scale, r8.pages), (1.0, 3), "unused budget opens no page");
        // A level that fits one atlas lays out exactly as before, whatever the budget.
        let few: Vec<UnwrapInput<'_>> = inputs[..2].to_vec();
        let (a, ra) = build_atlas_layout(&few, &paged(1), 1).expect("layout");
        let (b, rb) = build_atlas_layout(&few, &paged(8), 1).expect("layout");
        assert_eq!((a, ra.clone()), (b, rb));
        assert_eq!(ra.pages, 1);
        // A mesh larger than a page cannot straddle two: the density drops for it.
        let big = floor(40.0);
        let (lone, rl) = build_atlas_layout(&[input("big", &big, 1.0)], &paged(8), 1).expect("layout");
        assert!(rl.fit_scale < 1.0 && rl.pages == 1, "{rl:?}");
        assert_eq!(lone.entries[0].page, 0);
        let bad = LightmapUvSettings {
            max_pages: 0,
            ..paged(1)
        };
        assert_eq!(
            build_atlas_layout(&inputs, &bad, 1).expect_err("no pages").code,
            "lightmap_uv_settings"
        );
    }

    /// Paged layouts are the same on any worker count.
    #[test]
    fn paged_layouts_do_not_depend_on_the_thread_count() {
        let shapes = ten_floors();
        let inputs: Vec<UnwrapInput<'_>> = shapes.iter().map(|(n, g)| input(n, g, 1.0)).collect();
        let (one, r1) = build_atlas_layout(&inputs, &paged(4), 1).expect("layout");
        for threads in [2, 8, 64] {
            let (more, r) = build_atlas_layout(&inputs, &paged(4), threads).expect("layout");
            assert_eq!(one, more, "threads {threads}");
            assert_eq!(r1, r, "threads {threads}");
        }
    }
}
