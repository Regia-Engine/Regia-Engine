// SPDX-License-Identifier: MPL-2.0
//! Lightmaps for every package tier from one bake (ADR-3120), proven on the gallery.
//!
//! - `one_bake_gives_every_tier` (CPU, the real `regia` binary): the gallery baked once at HIGH
//!   (8 texels per metre), cooked, and `regia cook --lightmap-tiers --json` derives every tier of the
//!   default table. The derived LOW (4 texels per metre) is compared with a direct LOW bake of the same
//!   settings, in surface space (every triangle of every part sampled at seven points through each
//!   lightmap's own unwrap): its relative luminance error must stay within the direct bake's own noise
//!   (a second direct LOW bake with another seed) times 1.25 plus 0.01. Seams with the real geometry
//!   (ADR-2024's metric) stay within the shipping budget. The per-tier bytes fall with the tier, the
//!   ULTRA_LOW tier is BC6H with no direction section, and a second run writes the same bytes.
//! - `derived_tier_on_the_frame` (GPU, any adapter, lavapipe in CI): the derived LOW and the direct LOW
//!   drawn on the main view and scored against the path tracer's indirect light with the plate's FLIP
//!   (`score_view`): the derived tier's FLIP within 0.02 of the direct bake's, its energy within 0.05.
//! - `no_surface_point_reads_an_empty_lightmap` (CPU): no surface point reads (almost) no light where
//!   the bake has light within its own two-texel window (ADR-5125: on a derived tier the window is
//!   the bake's, in world units), and `the_dark_check_finds_a_planted_hole_at_every_tier` proves the
//!   check still fails a tier with a real hole.

use super::*;
use regia_asset::lightmap::{DEFAULT_HARD_EDGE_DEG, SeamGeometry, seam_report};

/// Barycentric sample points per triangle: the centroid and six points toward the corners and edges.
const SAMPLES: [[f32; 3]; 7] = [
    [1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0],
    [0.6, 0.2, 0.2],
    [0.2, 0.6, 0.2],
    [0.2, 0.2, 0.6],
    [0.4, 0.4, 0.2],
    [0.2, 0.4, 0.4],
    [0.4, 0.2, 0.4],
];

/// The top bake's density (HIGH) and the derived tier's (LOW).
const TOP_DENSITY: &str = "8";
const LOW_DENSITY: &str = "4";

fn lum(c: [f32; 3]) -> f64 {
    0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2])
}

fn tri_area(p: &[[f32; 3]], t: &[u32]) -> f64 {
    let v = |i: usize| p[t[i] as usize].map(f64::from);
    let (a, b, c) = (v(0), v(1), v(2));
    let (e1, e2) = (
        [b[0] - a[0], b[1] - a[1], b[2] - a[2]],
        [c[0] - a[0], c[1] - a[1], c[2] - a[2]],
    );
    let n = [
        e1[1] * e2[2] - e1[2] * e2[1],
        e1[2] * e2[0] - e1[0] * e2[2],
        e1[0] * e2[1] - e1[1] * e2[0],
    ];
    0.5 * (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt()
}

/// Area-weighted mean and 95th percentile of the relative luminance step between `a` and `b` over
/// every part's surface, each read through its own unwrap (module docs). The step is
/// `|La - Lb| / max((La + Lb) / 2, floor)`, `floor` 2 % of the mean, as the seam metric.
fn surface_error(a: &Lightmap, b: &Lightmap, meshes: &[CpuMesh]) -> (f64, f64) {
    let mut rows: Vec<(f64, f64, f64)> = Vec::new();
    for (p, mesh) in gallery::parts().iter().zip(meshes) {
        let (ea, eb) = (
            a.entry_for_key(entry_key(p)).expect("an entry in a"),
            b.entry_for_key(entry_key(p)).expect("an entry in b"),
        );
        let (ua, ub) = (&a.unwraps[ea], &b.unwraps[eb]);
        assert_eq!(
            ua.indices.len(),
            mesh.indices.len(),
            "{}: one unwrap triangle per mesh triangle",
            p.name
        );
        assert_eq!(ub.indices.len(), mesh.indices.len(), "{}", p.name);
        for (k, t) in mesh.indices.as_chunks::<3>().0.iter().enumerate() {
            let area = tri_area(&mesh.positions, t) / SAMPLES.len() as f64;
            for bary in SAMPLES {
                let at = |u: &regia_asset::lightmap::MeshUnwrap| {
                    let mut uv = [0.0f32; 2];
                    for (i, w) in bary.iter().enumerate() {
                        let q = u.uv[u.indices[3 * k + i] as usize];
                        uv[0] += w * q[0];
                        uv[1] += w * q[1];
                    }
                    uv
                };
                rows.push((
                    lum(a.sample_entry(ea, at(ua))),
                    lum(b.sample_entry(eb, at(ub))),
                    area,
                ));
            }
        }
    }
    let total: f64 = rows.iter().map(|r| r.2).sum();
    let mean_l = rows.iter().map(|r| 0.5 * (r.0 + r.1) * r.2).sum::<f64>() / total;
    let floor = (0.02 * mean_l).max(1e-9);
    let mut errs: Vec<(f64, f64)> = rows
        .iter()
        .map(|&(x, y, w)| ((x - y).abs() / (0.5 * (x + y)).max(floor), w))
        .collect();
    let mean = errs.iter().map(|e| e.0 * e.1).sum::<f64>() / total;
    errs.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut acc = 0.0;
    let mut p95 = errs.last().map_or(0.0, |e| e.0);
    for (e, w) in &errs {
        acc += w;
        if acc >= 0.95 * total {
            p95 = *e;
            break;
        }
    }
    (mean, p95)
}

/// The worst entry's seam p95 measured with the real geometry.
fn seams_with_geometry(map: &Lightmap, meshes: &[CpuMesh]) -> f32 {
    gallery::parts()
        .iter()
        .zip(meshes)
        .filter_map(|(p, mesh)| {
            let e = map.entry_for_key(entry_key(p))?;
            seam_report(
                map,
                e,
                SeamGeometry {
                    positions: &mesh.positions,
                },
                DEFAULT_HARD_EDGE_DEG,
            )
            .seams
            .map(|s| s.p95)
        })
        .fold(0.0, f32::max)
}

/// The settings every bake of these tests shares (the gate's quick plate settings, more samples with
/// `REGIA_GALLERY_SAMPLES`).
fn bake_args(density: &'static str, seed: &'static str, samples: &str) -> Vec<String> {
    [
        "--density",
        density,
        "--samples",
        samples,
        "--bounces",
        "3",
        "--seed",
        seed,
        "--threads",
        "0",
    ]
    .iter()
    .map(ToString::to_string)
    .collect()
}

fn bake_with(dir: &Path, out: &str, args: &[String]) -> Lightmap {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    bake(dir, out, &refs);
    Lightmap::read(&dir.join(out)).expect("read the bake")
}

fn samples() -> String {
    regia_core::brand::env_var_os(SAMPLES_ENV)
        .map_or_else(|| "32".to_string(), |v| v.to_string_lossy().to_string())
}

/// The tiers `regia cook --lightmap-tiers --json` derives for the project at `dir`, and the derived
/// cooked lightmap of `tier`.
fn cook_tiers(dir: &Path) -> (Value, Vec<(String, Vec<u8>)>) {
    let out = regia(
        dir,
        &[
            "cook",
            "--project",
            ".",
            "--platform",
            "linux",
            "--lightmap-tiers",
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "cook --lightmap-tiers failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json_of(&out);
    let root = regia_asset::cooked_cache_dir(dir, "linux").join("lightmap_tiers");
    (v, tree(&root))
}

fn tier_map(files: &[(String, Vec<u8>)], tier: &str) -> Lightmap {
    let (_, bytes) = files
        .iter()
        .find(|(rel, _)| rel.starts_with(&format!("{tier}/")))
        .unwrap_or_else(|| panic!("no cooked {tier} lightmap"));
    let (_, payload) = regia_asset::decode_cooked_file(bytes).expect("a cooked file");
    Lightmap::from_bytes(payload).expect("a current .flmap")
}

fn tier_row<'a>(v: &'a Value, tier: &str) -> &'a Value {
    v["lightmaps"][0]["tiers"]
        .as_array()
        .expect("tiers")
        .iter()
        .find(|t| t["tier"] == tier)
        .unwrap_or_else(|| panic!("no {tier} row in {v}"))
}

#[test]
fn one_bake_gives_every_tier() {
    let tmp = project("lightmap_gallery_tiers");
    let dir = &tmp.0;
    let meshes: Vec<CpuMesh> = gallery::parts().iter().map(|p| imported(dir, p)).collect();
    let s = samples();
    let top = bake_with(dir, gallery::DEFAULT_OUT, &bake_args(TOP_DENSITY, "1", &s));
    // Direct LOW bakes, outside `assets/` so the cook does not take them: two seeds for the noise floor.
    let direct = bake_with(dir, "direct_low.flmap", &bake_args(LOW_DENSITY, "1", &s));
    let direct2 = bake_with(dir, "direct_low_2.flmap", &bake_args(LOW_DENSITY, "2", &s));

    let (v, files) = cook_tiers(dir);
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&v["tiers"]).unwrap_or_default()
    );
    let low = tier_map(&files, "low");
    assert_eq!(low.density, 4.0, "LOW is the bake's 8 texels per metre over 2");
    assert_eq!(low.unwraps, top.unwraps, "the tier keeps the bake's UV2");
    assert!(low.is_directional());

    let (err_mean, err_p95) = surface_error(&low, &direct, &meshes);
    let (floor_mean, floor_p95) = surface_error(&direct2, &direct, &meshes);
    let (top_mean, _) = surface_error(&top, &direct, &meshes);
    let seams_low = seams_with_geometry(&low, &meshes);
    let seams_direct = seams_with_geometry(&direct, &meshes);
    let seams_top = seams_with_geometry(&top, &meshes);
    eprintln!(
        "derived LOW vs direct LOW: mean {err_mean:.4} p95 {err_p95:.4}; direct seed 2 vs seed 1: mean {floor_mean:.4} p95 {floor_p95:.4}; HIGH vs direct LOW: mean {top_mean:.4}; seams p95: derived {seams_low:.4}, direct {seams_direct:.4}, HIGH {seams_top:.4}"
    );
    write_tier_evidence(
        "derive",
        &[("high", &top), ("low", &low), ("direct_low", &direct)],
        &meshes,
        json!({"surface_mean": err_mean, "surface_p95": err_p95,
            "seed_noise_mean": floor_mean, "seed_noise_p95": floor_p95,
            "cook": v}),
    );
    assert!(
        err_mean <= 1.25 * floor_mean + 0.01,
        "the derived LOW differs from a direct LOW bake by {err_mean:.4} on average, over its own noise {floor_mean:.4} x 1.25 + 0.01"
    );
    assert!(
        err_p95 <= 1.25 * floor_p95 + 0.02,
        "p95 {err_p95:.4} over the direct bake's own {floor_p95:.4} x 1.25 + 0.02"
    );
    // Bytes per tier: the default table from an 8 texels per metre bake (ULTRA and HIGH take the bake).
    let bytes = |t: &str| tier_row(&v, t)["bytes"].as_u64().expect("bytes");
    assert_eq!(
        bytes("ultra"),
        bytes("high"),
        "ULTRA at or above the bake is the bake"
    );
    assert!(
        bytes("low") < bytes("high") && bytes("ultra_low") < bytes("low"),
        "{v}"
    );
    let ul = tier_row(&v, "ultra_low");
    assert_eq!(ul["encoding"], "bc6h");
    assert_eq!(ul["directional"], false);
    assert_eq!(ul["verdict"], "fits");
    let ul_map = tier_map(&files, "ultra_low");
    assert!(!ul_map.is_directional());
    assert_eq!(ul_map.texel_encoding(), LightmapTexelEncoding::Bc6h);
    // ADR-5125: the bake's 2-texel padding over four leaves ULTRA_LOW no whole gutter texel, so its
    // charts are repacked at its own resolution with the block-encoded tier's padding: same
    // vertices and triangles, its own UV2.
    assert!(
        ul["derive"]["repacked_charts"].as_u64().unwrap_or(0) > 0,
        "ULTRA_LOW repacks its charts: {ul}"
    );
    assert_eq!(
        ul["derive"]["chart_padding"],
        regia_asset_pipeline::lightmap_tiers::BLOCK_TIER_CHART_PADDING,
        "{ul}"
    );
    for (a, b) in ul_map.unwraps.iter().zip(&top.unwraps) {
        assert_eq!((&a.xref, &a.indices), (&b.xref, &b.indices));
    }
    let seams_ultra_low = seams_with_geometry(&ul_map, &meshes);
    eprintln!("derived ULTRA_LOW seams p95 {seams_ultra_low:.4}");
    for t in ["ultra", "high", "low", "ultra_low"] {
        let row = tier_row(&v, t);
        let file = files
            .iter()
            .find(|(rel, _)| rel.starts_with(&format!("{t}/")))
            .expect("a file per tier");
        let (_, payload) = regia_asset::decode_cooked_file(&file.1).expect("cooked");
        assert_eq!(
            row["bytes"].as_u64(),
            Some(payload.len() as u64),
            "{t}: the reported bytes are the file's"
        );
    }
    let totals: Vec<&Value> = v["tiers"].as_array().expect("totals").iter().collect();
    assert_eq!(totals.len(), 4);

    // Same input, same bytes.
    let (v2, files2) = cook_tiers(dir);
    assert_eq!(files, files2, "a second run writes the same tier files");
    assert_eq!(v["lightmaps"], v2["lightmaps"]);
    eprintln!("all four tier payload sizes/encodings and second-cook byte determinism passed");

    // Keep the quality assertions after payload checks so a seam failure does not hide them.
    assert!(
        f64::from(seams_low) <= SHIPPING_SEAM_BUDGET,
        "the derived LOW's seams p95 {seams_low} pass the shipping budget"
    );
    assert!(
        seams_low <= seams_direct.max(seams_top) + 0.05,
        "the derive adds seams: {seams_low} against the bakes' {seams_direct} and {seams_top}"
    );
    // The header's carried figures are what the package's seam check reads: within the budget too.
    let carried = low
        .entries
        .iter()
        .filter_map(|e| e.seams.map(|s| s.p95))
        .fold(0.0, f32::max);
    assert!(
        f64::from(carried) <= SHIPPING_SEAM_BUDGET,
        "carried seams {carried}"
    );
    // ULTRA_LOW, with the real geometry and as carried, within the shipping budget (ADR-5125).
    assert!(
        f64::from(seams_ultra_low) <= SHIPPING_SEAM_BUDGET,
        "the derived ULTRA_LOW's seams p95 {seams_ultra_low} pass the shipping budget"
    );
    let carried = ul_map
        .entries
        .iter()
        .filter_map(|e| e.seams.map(|s| s.p95))
        .fold(0.0, f32::max);
    assert!(
        f64::from(carried) <= SHIPPING_SEAM_BUDGET,
        "ULTRA_LOW carried seams {carried}"
    );
}

#[test]
fn derived_tier_on_the_frame() {
    let tmp = project("lightmap_gallery_tiers_frame");
    let dir = &tmp.0;
    let parts = gallery::parts();
    let meshes: Vec<CpuMesh> = parts.iter().map(|p| imported(dir, p)).collect();
    let s = samples();
    bake_with(dir, gallery::DEFAULT_OUT, &bake_args(TOP_DENSITY, "1", &s));
    let direct = bake_with(dir, "direct_low.flmap", &bake_args(LOW_DENSITY, "1", &s));
    let (_, files) = cook_tiers(dir);
    let low = tier_map(&files, "low");

    let q = quality(false);
    let light = level_light();
    let view = VIEWS[0];
    let reference = reference(&lab_scene(&meshes, view, q, &light), view, &light, q);
    let score = |map: Lightmap| {
        let entries: Vec<usize> = parts
            .iter()
            .map(|p| map.entry_for_key(entry_key(p)).expect("an entry per part"))
            .collect();
        let map = Arc::new(map);
        score_view(view, &reference, &meshes, &light, (&map, &entries), q, None)
    };
    let (derived, ambient, _) = score(low);
    let (baked, _, _) = score(direct);
    eprintln!(
        "{}: indirect FLIP derived LOW {:.4} (energy {:?}), direct LOW {:.4} (energy {:?}), flat ambient {:.4}",
        view.name,
        derived.flip_mean,
        derived.energy_ratio,
        baked.flip_mean,
        baked.energy_ratio,
        ambient.flip_mean
    );
    assert!(derived.pixels > 0);
    assert!(
        derived.flip_mean <= baked.flip_mean + 0.02,
        "derived LOW FLIP {} against a direct LOW bake's {}",
        derived.flip_mean,
        baked.flip_mean
    );
    let (de, be) = (
        derived.energy_ratio.expect("energy"),
        baked.energy_ratio.expect("energy"),
    );
    assert!(
        (de - be).abs() <= 0.05,
        "energy {de} against the direct bake's {be}"
    );
    assert!(derived.flip_mean < ambient.flip_mean);
}

/// The frame's lookup (bicubic irradiance, the bilinear direction, the half-cosine rebalance for
/// `n`) at an entry's local UV2: `lightmap_irradiance_for_normal` in `shaders/lightmap.wgsl`.
fn frame_lookup(map: &Lightmap, entry: usize, local: [f32; 2], n: [f32; 3]) -> [f32; 3] {
    let s = map.entries[entry].scale_offset;
    let uv = [local[0] * s[0] + s[2], local[1] * s[1] + s[3]];
    let page = map.entries[entry].page;
    let e = map.sample_bicubic_page(page, uv);
    match map.sample_direction_page(page, uv) {
        Some(d) => regia_asset::lightmap::shade_directional(e, d, n),
        None => e,
    }
}

/// Surface points whose frame lookup reads (almost) no light where the baked texels around them have
/// some: `(part, world position, frame luminance, the brightest baked texel within two texels)`.
fn dark_points(map: &Lightmap, meshes: &[CpuMesh]) -> Vec<(String, [f32; 3], f64, f64)> {
    dark_points_using(map, meshes, frame_lookup)
}

fn dark_points_using(
    map: &Lightmap,
    meshes: &[CpuMesh],
    lookup: impl Fn(&Lightmap, usize, [f32; 2], [f32; 3]) -> [f32; 3],
) -> Vec<(String, [f32; 3], f64, f64)> {
    let mut out = Vec::new();
    // A grid of barycentrics, the edges included (a chart's border is where a lookup meets its gutter).
    let steps = 8;
    for (p, mesh) in gallery::parts().iter().zip(meshes) {
        let e = map.entry_for_key(entry_key(p)).expect("an entry");
        let u = &map.unwraps[e];
        let [rx, ry, rw, rh] = map.entries[e].rect;
        let ry = ry + map.entries[e].page * map.height;
        for (k, t) in mesh.indices.as_chunks::<3>().0.iter().enumerate() {
            let pos = t.map(|i| mesh.positions[i as usize]);
            let nrm = t.map(|i| mesh.normals[i as usize]);
            for i in 0..=steps {
                for j in 0..=steps - i {
                    let b = [
                        i as f32 / steps as f32,
                        j as f32 / steps as f32,
                        (steps - i - j) as f32 / steps as f32,
                    ];
                    let mix = |v: [[f32; 3]; 3]| -> [f32; 3] {
                        std::array::from_fn(|c| b[0] * v[0][c] + b[1] * v[1][c] + b[2] * v[2][c])
                    };
                    let mut local = [0.0f32; 2];
                    for (w, &vi) in b.iter().zip(&u.indices[3 * k..3 * k + 3]) {
                        local[0] += w * u.uv[vi as usize][0];
                        local[1] += w * u.uv[vi as usize][1];
                    }
                    let n = mix(nrm);
                    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-12);
                    let n = n.map(|c| c / len);
                    let got = lum(lookup(map, e, local, n));
                    let (tx, ty) = (
                        (local[0] * rw as f32) as i64 + i64::from(rx),
                        (local[1] * rh as f32) as i64 + i64::from(ry),
                    );
                    let mut near = 0.0f64;
                    for dy in -2..=2 {
                        for dx in -2..=2 {
                            let (x, y) = (tx + dx, ty + dy);
                            if x < 0 || y < 0 {
                                continue;
                            }
                            let (x, y) = (x as u32, y as u32);
                            if map.coverage_at(x, y) == regia_asset::lightmap::Coverage::BAKED {
                                near = near.max(lum(map.irradiance_at(x, y)));
                            }
                        }
                    }
                    if near > 0.0 && got < 0.05 * near {
                        out.push((p.name.to_string(), mix(pos), got, near));
                    }
                }
            }
        }
    }
    out
}

/// A surface point a tier reads (almost) no light at where its bake has light
/// ([`dark_points_against`]).
#[derive(Clone, Debug)]
struct DarkPoint {
    part: String,
    world: [f32; 3],
    /// The tier's frame lookup, lux.
    tier: f64,
    /// The bake's frame lookup at the same point, lux.
    bake: f64,
    /// The brightest BAKED bake texel within the bake's two-texel window, lux.
    near: f64,
}

/// An entry's local UV2 at barycentric `b` of triangle `k`.
fn local_at(u: &regia_asset::lightmap::MeshUnwrap, k: usize, b: [f32; 3]) -> [f32; 2] {
    let mut local = [0.0f32; 2];
    for (w, &vi) in b.iter().zip(&u.indices[3 * k..3 * k + 3]) {
        local[0] += w * u.uv[vi as usize][0];
        local[1] += w * u.uv[vi as usize][1];
    }
    local
}

/// The dark-point check of a lightmap derived from `bake` (ADR-5125, ADR-4995's proposal 1): the
/// same surface grid, lookup and 5 % threshold as [`dark_points`], with the neighbourhood taken from
/// the bake's own window: the brightest BAKED bake texel within two bake texels of the same surface
/// point (the triangle and barycentrics through each map's own unwrap). The window is therefore the
/// same in world units, two bake texels (2 / bake density metres), at every tier, instead of two
/// tier texels, which reach 2× (LOW) or 4× (ULTRA_LOW) as far. A point fails when the bake has
/// light there and the tier reads under 5 % of it; for `map == bake` this is [`dark_points`].
fn dark_points_against(map: &Lightmap, bake: &Lightmap, meshes: &[CpuMesh]) -> Vec<DarkPoint> {
    let mut out = Vec::new();
    let steps = 8;
    for (p, mesh) in gallery::parts().iter().zip(meshes) {
        let e = map.entry_for_key(entry_key(p)).expect("an entry");
        let be = bake.entry_for_key(entry_key(p)).expect("a bake entry");
        let (u, bu) = (&map.unwraps[e], &bake.unwraps[be]);
        assert_eq!(
            u.indices.len(),
            bu.indices.len(),
            "{}: the tier's triangles are the bake's",
            p.name
        );
        let [rx, ry, rw, rh] = bake.entries[be].rect;
        let ry = ry + bake.entries[be].page * bake.height;
        for (k, t) in mesh.indices.as_chunks::<3>().0.iter().enumerate() {
            let pos = t.map(|i| mesh.positions[i as usize]);
            let nrm = t.map(|i| mesh.normals[i as usize]);
            for i in 0..=steps {
                for j in 0..=steps - i {
                    let b = [
                        i as f32 / steps as f32,
                        j as f32 / steps as f32,
                        (steps - i - j) as f32 / steps as f32,
                    ];
                    let mix = |v: [[f32; 3]; 3]| -> [f32; 3] {
                        std::array::from_fn(|c| b[0] * v[0][c] + b[1] * v[1][c] + b[2] * v[2][c])
                    };
                    let n = mix(nrm);
                    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-12);
                    let n = n.map(|c| c / len);
                    let (local, blocal) = (local_at(u, k, b), local_at(bu, k, b));
                    let got = lum(frame_lookup(map, e, local, n));
                    let (tx, ty) = (
                        (blocal[0] * rw as f32) as i64 + i64::from(rx),
                        (blocal[1] * rh as f32) as i64 + i64::from(ry),
                    );
                    let mut near = 0.0f64;
                    for dy in -2..=2 {
                        for dx in -2..=2 {
                            let (x, y) = (tx + dx, ty + dy);
                            if x >= 0
                                && y >= 0
                                && bake.coverage_at(x as u32, y as u32)
                                    == regia_asset::lightmap::Coverage::BAKED
                            {
                                near = near.max(lum(bake.irradiance_at(x as u32, y as u32)));
                            }
                        }
                    }
                    if near > 0.0 && got < 0.05 * near {
                        out.push(DarkPoint {
                            part: p.name.to_string(),
                            world: mix(pos),
                            tier: got,
                            bake: lum(frame_lookup(bake, be, blocal, n)),
                            near,
                        });
                    }
                }
            }
        }
    }
    out
}

/// No point of the gallery reads (almost) no light from its lightmap where the bake has light around
/// it: the one-pixel zero-lightmap artifact of the interior plate (`SUMMARY.md` § 8, at about
/// (9.5, 3.9, 8.0) m where the adit's jamb meets its side wall), on the bake and on every derived tier.
/// A derived tier is checked against the bake's own window in world units ([`dark_points_against`],
/// ADR-5125): the bake itself is under 5 % of a texel two tier texels away at most of the points
/// the tier window used to flag, so that window measured the window, not the tier.
#[test]
fn no_surface_point_reads_an_empty_lightmap() {
    let tmp = project("lightmap_gallery_dark");
    let dir = &tmp.0;
    let meshes: Vec<CpuMesh> = gallery::parts().iter().map(|p| imported(dir, p)).collect();
    let s = samples();
    let top = bake_with(dir, gallery::DEFAULT_OUT, &bake_args(TOP_DENSITY, "1", &s));
    let mut maps = vec![("bake".to_string(), top)];
    let (_, files) = cook_tiers(dir);
    for t in ["low", "ultra_low"] {
        maps.push((t.to_string(), tier_map(&files, t)));
    }
    write_tier_evidence(
        "dark",
        &maps
            .iter()
            .map(|(name, map)| (name.as_str(), map))
            .collect::<Vec<_>>(),
        &meshes,
        json!({}),
    );
    let mut failures = Vec::new();
    for (name, map) in &maps {
        let dark: Vec<DarkPoint> = if name == "bake" {
            dark_points(map, &meshes)
                .into_iter()
                .map(|(part, world, got, near)| DarkPoint {
                    part,
                    world,
                    tier: got,
                    bake: got,
                    near,
                })
                .collect()
        } else {
            dark_points_against(map, &maps[0].1, &meshes)
        };
        for d in dark.iter().take(12) {
            eprintln!(
                "{name}: {} at ({:.2}, {:.2}, {:.2}) reads {:.4} lux (the bake {:.4}) beside {:.4}",
                d.part, d.world[0], d.world[1], d.world[2], d.tier, d.bake, d.near
            );
        }
        eprintln!(
            "DARK_CHECK {name}: {} dark points (old two-tier-texel window: {})",
            dark.len(),
            dark_points(map, &meshes).len()
        );
        if !dark.is_empty() {
            failures.push(format!("{name}: {} dark points", dark.len()));
        }
    }
    assert!(failures.is_empty(), "{failures:?}");
}

/// Plants a hole in `map`: every texel within four texels of a point of the largest lit triangle
/// set EMPTY and black, inside its entry's rectangle. A lookup within two texels of that point then
/// reads no light at all. The triangle is the largest lit one whose tier-texel edges are at most 16,
/// and the point its centroid: the surface grid of [`dark_points_against`] (eight steps a triangle)
/// has a point within two texels of it. When no such triangle is lit (the gallery's LOW tier once
/// the denoiser stopped lighting buried faces, ADR-7590), the triangle is the largest lit one of any
/// size and the point a grid point nearest its centroid: barycentric (3/8, 3/8, 2/8), one of three
/// equally near points on the eight-step grid, so the grid samples the hole's centre (ADR-7618). Returns the part and the point's world position.
fn plant_hole(map: &mut Lightmap, meshes: &[CpuMesh]) -> (String, [f32; 3]) {
    // (area, part, triangle): the largest lit triangle of at most 16 texels, and of any size.
    let mut small: Option<(f32, usize, usize)> = None;
    let mut any: Option<(f32, usize, usize)> = None;
    for (pi, (p, mesh)) in gallery::parts().iter().zip(meshes).enumerate() {
        let e = map.entry_for_key(entry_key(p)).expect("an entry");
        let [_, _, w, h] = map.entries[e].rect;
        let u = &map.unwraps[e];
        for k in 0..mesh.indices.len() / 3 {
            let q: [[f32; 2]; 3] = std::array::from_fn(|c| {
                let uv = u.uv[u.indices[3 * k + c] as usize];
                [uv[0] * w as f32, uv[1] * h as f32]
            });
            let area = 0.5
                * ((q[1][0] - q[0][0]) * (q[2][1] - q[0][1]) - (q[1][1] - q[0][1]) * (q[2][0] - q[0][0]))
                    .abs();
            let lit = lum(map.sample_entry(e, local_at(u, k, [1.0 / 3.0; 3]))) > 1.0;
            if !lit {
                continue;
            }
            if any.is_none_or(|(a, _, _)| area > a) {
                any = Some((area, pi, k));
            }
            let edge = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).hypot(a[1] - b[1]);
            let longest = edge(q[0], q[1]).max(edge(q[1], q[2])).max(edge(q[2], q[0]));
            if longest <= 16.0 && small.is_none_or(|(a, _, _)| area > a) {
                small = Some((area, pi, k));
            }
        }
    }
    let ((_, pi, k), at) = match (small, any) {
        (Some(best), _) => (best, [1.0 / 3.0; 3]),
        (None, Some(best)) => (best, [3.0 / 8.0, 3.0 / 8.0, 2.0 / 8.0]),
        (None, None) => panic!("a lit triangle to plant a hole in"),
    };
    let part = &gallery::parts()[pi];
    let mesh = &meshes[pi];
    let e = map.entry_for_key(entry_key(part)).expect("an entry");
    let entry = map.entries[e].clone();
    let [rx, ry, rw, rh] = entry.rect;
    let ry = ry + entry.page * map.height;
    let local = local_at(&map.unwraps[e], k, at);
    let (cx, cy) = (
        (local[0] * rw as f32) as i64 + i64::from(rx),
        (local[1] * rh as f32) as i64 + i64::from(ry),
    );
    for y in (cy - 4).max(i64::from(ry))..=(cy + 4).min(i64::from(ry + rh) - 1) {
        for x in (cx - 4).max(i64::from(rx))..=(cx + 4).min(i64::from(rx + rw) - 1) {
            let i = y as usize * map.width as usize + x as usize;
            map.texels[i] = 0;
            map.coverage[i] = regia_asset::lightmap::Coverage::EMPTY;
        }
    }
    map.blocks = None;
    let t = &mesh.indices[3 * k..3 * k + 3];
    let world = std::array::from_fn(|c| {
        t.iter()
            .zip(at)
            .map(|(&i, w)| w * mesh.positions[i as usize][c])
            .sum::<f32>()
    });
    (part.name.to_string(), world)
}

/// The bake-window check still fails a tier with a real hole (ADR-5125): a block of EMPTY, black
/// texels planted in a lit chart of each derived tier is found at the planted place, where the
/// untouched tier had no dark point.
#[test]
fn the_dark_check_finds_a_planted_hole_at_every_tier() {
    let tmp = project("lightmap_gallery_dark_hole");
    let dir = &tmp.0;
    let meshes: Vec<CpuMesh> = gallery::parts().iter().map(|p| imported(dir, p)).collect();
    let top = bake_with(
        dir,
        gallery::DEFAULT_OUT,
        &bake_args(TOP_DENSITY, "1", &samples()),
    );
    assert_eq!(
        dark_points_against(&top, &top, &meshes).len(),
        dark_points(&top, &meshes).len(),
        "on the bake the bake-window check is the original one"
    );
    let (_, files) = cook_tiers(dir);
    for t in ["low", "ultra_low"] {
        let tier = tier_map(&files, t);
        let clean = dark_points_against(&tier, &top, &meshes);
        let mut holed = tier.clone();
        let (part, at) = plant_hole(&mut holed, &meshes);
        let found = dark_points_against(&holed, &top, &meshes);
        // Within five tier texels of the centroid, in metres.
        let radius = 5.0 / tier.density;
        let here = |d: &DarkPoint| {
            d.part == part && (0..3).map(|c| (d.world[c] - at[c]).powi(2)).sum::<f32>().sqrt() <= radius
        };
        eprintln!(
            "PLANTED {t}: {part} at {at:?}: {} dark points before, {} after ({} at the hole)",
            clean.len(),
            found.len(),
            found.iter().filter(|d| here(d)).count()
        );
        assert!(
            !clean.iter().any(here),
            "{t}: the untouched tier already reads dark at the planted place"
        );
        assert!(
            found.iter().any(here),
            "{t}: the check misses a hole planted in {part} at {at:?}"
        );
        assert!(
            found.iter().any(|d| here(d) && d.tier == 0.0 && d.bake > 0.0),
            "{t}: a point inside the hole reads no light where the bake reads some"
        );
    }
}

/// The CPU oracle must follow the receiver's page, even when page zero contains different light.
#[test]
fn frame_lookup_uses_the_entry_page() {
    let fixture = include_bytes!("../../../regia_asset/tests/fixtures/lightmap-v1.flmap");
    let mut one = Lightmap::from_bytes(fixture).expect("fixture");
    let bright = regia_asset::baked_gi::rgb9e5([100.0 * one.exposure; 3]);
    one.texels.fill(bright);
    one.coverage.fill(regia_asset::lightmap::Coverage::BAKED);
    let mut paged = one.clone();
    for entry in &mut paged.entries {
        entry.page = 1;
    }
    paged.texels = std::iter::repeat_n(0, one.page_texels())
        .chain(one.texels.iter().copied())
        .collect();
    paged.coverage = std::iter::repeat_n(regia_asset::lightmap::Coverage::BAKED, one.page_texels())
        .chain(one.coverage.iter().copied())
        .collect();
    paged.validate().expect("two pages");
    let expected = frame_lookup(&one, 0, [0.5, 0.5], [0.0, 1.0, 0.0]);
    assert!(expected[0] > 90.0);
    assert_eq!(frame_lookup(&paged, 0, [0.5, 0.5], [0.0, 1.0, 0.0]), expected);
}

/// Diagnostic only: distinguish insufficient source gutters from other derivation errors.
/// Every variant keeps the original density, samples, seed and bounce count. These alternate
/// fixtures do not replace the default acceptance fixture or its unchanged quality assertions.
#[test]
#[ignore = "hand-run source-gutter diagnosis; not a replacement acceptance fixture"]
fn tier_gutter_study() {
    let tmp = project("lightmap_gallery_tier_gutters");
    let dir = &tmp.0;
    let meshes: Vec<CpuMesh> = gallery::parts().iter().map(|p| imported(dir, p)).collect();
    let s = samples();
    for padding in [2, 4, 8, 16] {
        let mut args = bake_args(TOP_DENSITY, "1", &s);
        args.extend(["--padding".to_owned(), padding.to_string()]);
        let top = bake_with(dir, gallery::DEFAULT_OUT, &args);
        let (_, files) = cook_tiers(dir);
        let low = tier_map(&files, "low");
        let ultra_low = tier_map(&files, "ultra_low");
        assert_eq!(low.unwraps, top.unwraps);
        // ULTRA_LOW repacks its charts where the bake's padding over four leaves no whole gutter
        // texel (ADR-5125): the bake's vertices and triangles, its own UV2.
        for (a, b) in ultra_low.unwraps.iter().zip(&top.unwraps) {
            assert_eq!((&a.xref, &a.indices), (&b.xref, &b.indices));
        }
        assert_eq!(low.density, 4.0);
        assert_eq!(ultra_low.density, 2.0);
        for (name, map) in [("high", &top), ("low", &low), ("ultra_low", &ultra_low)] {
            eprintln!(
                "GUTTER padding={padding} tier={name} seam_p95={} dark_points={} bake_window_dark={}",
                seams_with_geometry(map, &meshes),
                dark_points(map, &meshes).len(),
                dark_points_against(map, &top, &meshes).len()
            );
        }
        write_tier_evidence(
            &format!("padding_{padding}"),
            &[("high", &top), ("low", &low), ("ultra_low", &ultra_low)],
            &meshes,
            json!({"source_padding": padding, "diagnostic_only": true}),
        );
    }
}

/// Preserve failing tier inputs and raw per-entry measurements before the acceptance assertions.
/// Output goes only to the explicit evidence directory, never to a reference golden or fixture.
fn write_tier_evidence(tag: &str, maps: &[(&str, &Lightmap)], meshes: &[CpuMesh], summary: Value) {
    let Some(dir) = evidence_dir() else {
        return;
    };
    // Separate compression loss from resampling loss without changing the cooked acceptance input.
    let uncompressed = (tag == "dark").then(|| {
        let (mut map, _) = regia_asset_pipeline::lightmap_tiers::derive_lightmap_tier_padded(
            maps[0].1,
            4,
            Some(regia_asset_pipeline::lightmap_tiers::BLOCK_TIER_CHART_PADDING),
        )
        .expect("derive uncompressed diagnostic control");
        map.directions.clear();
        map.direction_blocks = None;
        map
    });
    let nondirectional = (tag == "dark").then(|| {
        let mut map = maps[1].1.clone();
        map.directions.clear();
        map.direction_blocks = None;
        map
    });
    let mut inputs = maps.to_vec();
    if let Some(map) = &uncompressed {
        inputs.push(("ultra_low_uncompressed_control", map));
    }
    if let Some(map) = &nondirectional {
        inputs.push(("low_nondirectional_control", map));
    }
    let mut rows = Vec::new();
    for (name, map) in inputs {
        let entries: Vec<Value> = gallery::parts()
            .iter()
            .zip(meshes)
            .map(|(part, mesh)| {
                let index = map.entry_for_key(entry_key(part)).expect("an entry per part");
                let seam = seam_report(
                    map,
                    index,
                    SeamGeometry {
                        positions: &mesh.positions,
                    },
                    DEFAULT_HARD_EDGE_DEG,
                );
                json!({"part": part.name, "page": map.entries[index].page,
                    "rect": map.entries[index].rect, "charts": map.entries[index].charts,
                    "texels_per_metre": map.entries[index].texels_per_metre,
                    "seams": seam.seams})
            })
            .collect();
        rows.push(json!({"name": name, "width": map.width, "height": map.height,
            "density": map.density, "padding": map.padding,
            "encoding": map.texel_encoding().name(), "directional": map.is_directional(),
            "entries": entries, "dark_points": dark_points(map, meshes),
            "dark_points_bake_window": dark_points_against(map, maps[0].1, meshes).iter()
                .map(|d| json!({"part": d.part, "world": d.world, "tier": d.tier,
                    "bake": d.bake, "near": d.near}))
                .collect::<Vec<_>>(),
            "dark_footprints": footprints::report(map, maps[0].1, meshes),
            "dark_classes": footprints::classify(map, maps[0].1, meshes)}));
        regia_test_utils::write_atomic(
            &dir.join(format!("tier_{tag}_{name}.flmap")),
            &map.to_bytes().expect("encode evidence map"),
        )
        .expect("write evidence map");
    }
    regia_test_utils::write_atomic(
        &dir.join(format!("tier_{tag}.json")),
        &serde_json::to_vec_pretty(&json!({"summary": summary, "maps": rows}))
            .expect("encode tier measurements"),
    )
    .expect("write tier measurements");
}

/// One-variable controls of the cook's reconstruction, never replacement acceptance inputs.
#[test]
#[ignore = "manual resampling diagnosis; original acceptance tests remain mandatory"]
fn tier_resampling_study() {
    let tmp = project("lightmap_gallery_resampling");
    let dir = &tmp.0;
    let meshes: Vec<CpuMesh> = gallery::parts().iter().map(|p| imported(dir, p)).collect();
    let s = samples();
    let top = bake_with(dir, gallery::DEFAULT_OUT, &bake_args(TOP_DENSITY, "1", &s));
    let direct = bake_with(dir, "direct_low.flmap", &bake_args(LOW_DENSITY, "1", &s));
    let mut maps = vec![
        ("source".to_string(), top.clone()),
        ("direct_low".to_string(), direct),
    ];
    let mut direct_ultra_low = bake_with(dir, "direct_ultra_low.flmap", &bake_args("2", "1", &s));
    direct_ultra_low.directions.clear();
    direct_ultra_low.direction_blocks = None;
    maps.push(("direct_ultra_low".to_string(), direct_ultra_low));
    for factor in [2, 4] {
        let (base, stats) =
            regia_asset_pipeline::lightmap_tiers::derive_lightmap_tier(&top, factor).expect("derive control");
        eprintln!("factor {factor}: {stats:?}");
        for mode in [
            "original",
            "bilinear_centers",
            "bicubic_centers",
            "gutter_only",
            "all_coverage_mean",
        ] {
            let mut map = base.clone();
            for (e, entry) in base.entries.iter().enumerate() {
                let [rx, ry, rw, rh] = entry.rect;
                let src = &top.entries[e];
                let [sx, sy, sw, sh] = src.rect;
                for y in 0..rh {
                    for x in 0..rw {
                        let at = ((entry.page * map.height + ry + y) * map.width + rx + x) as usize;
                        if mode == "original"
                            || (mode == "gutter_only"
                                && map.coverage[at] == regia_asset::lightmap::Coverage::BAKED)
                        {
                            continue;
                        }
                        let local = [(x as f32 + 0.5) / rw as f32, (y as f32 + 0.5) / rh as f32];
                        let uv = [
                            local[0] * src.scale_offset[0] + src.scale_offset[2],
                            local[1] * src.scale_offset[1] + src.scale_offset[3],
                        ];
                        let value = if mode == "all_coverage_mean" {
                            let mut sum = [0.0; 3];
                            let mut weight = 0.0;
                            for yy in y * sh / rh..((y + 1) * sh).div_ceil(rh) {
                                for xx in x * sw / rw..((x + 1) * sw).div_ceil(rw) {
                                    let gy = src.page * top.height + sy + yy;
                                    if top.coverage_at(sx + xx, gy) != regia_asset::lightmap::Coverage::EMPTY
                                    {
                                        let c = top.irradiance_at(sx + xx, gy);
                                        for (v, c) in sum.iter_mut().zip(c) {
                                            *v += c;
                                        }
                                        weight += 1.0;
                                    }
                                }
                            }
                            if weight > 0.0 {
                                sum.map(|v| v / weight)
                            } else {
                                top.sample_entry(e, local)
                            }
                        } else if mode == "bicubic_centers" {
                            top.sample_bicubic_page(src.page, uv)
                        } else {
                            top.sample_entry(e, local)
                        };
                        map.texels[at] = regia_asset::baked_gi::rgb9e5(value.map(|v| v * map.exposure));
                    }
                }
            }
            eprintln!(
                "CONTROL factor={factor} mode={mode} seam={} dark={} error={:?}",
                seams_with_geometry(&map, &meshes),
                dark_points(&map, &meshes).len(),
                surface_error(&map, &maps[1].1, &meshes)
            );
            eprintln!(
                "CONTROL factor={factor} mode={mode} bilinear_dark={}",
                dark_points_using(&map, &meshes, |map, e, uv, n| map.shade_entry(e, uv, n, false)).len()
            );
            maps.push((format!("factor_{factor}_{mode}"), map));
        }
    }
    write_tier_evidence(
        "resampling",
        &maps.iter().map(|(n, m)| (n.as_str(), m)).collect::<Vec<_>>(),
        &meshes,
        json!({"diagnostic_only": true}),
    );
}

#[path = "tier_footprints.rs"]
mod footprints;

// Test-only reuse of the baker's original solver, without adding a public runtime API. Unlike
// derive_lightmap_tier, this gallery harness has the source geometry needed to distinguish creases.
#[path = "../../../regia_pt/src/lightmap/stitch.rs"]
mod stitch_control;

fn restitch_control(base: &Lightmap, meshes: &[CpuMesh]) -> Lightmap {
    let mut map = base.clone();
    let mut positions: Vec<&[[f32; 3]]> = vec![&[]; map.entries.len()];
    for (part, mesh) in gallery::parts().iter().zip(meshes) {
        positions[map.entry_for_key(entry_key(part)).expect("entry")] = &mesh.positions;
    }
    let layout = regia_asset::lightmap::LightmapLayout {
        width: map.width,
        height: map.height,
        density: map.density,
        padding: map.padding,
        entries: map.entries.clone(),
        unwraps: map.unwraps.clone(),
    };
    let mut values: Vec<[f32; 7]> = (0..map.texels.len())
        .map(|k| {
            let (x, y) = (k as u32 % map.width, k as u32 / map.width);
            let e = map.irradiance_at(x, y);
            let d = map.direction_at(x, y).unwrap_or([0.0; 4]);
            [e[0], e[1], e[2], d[0], d[1], d[2], d[3]]
        })
        .collect();
    let stats = stitch_control::stitch(
        &mut values,
        &map.coverage,
        &layout.stacked(),
        &positions,
        map.width as usize,
        (map.height * map.pages()) as usize,
        // One page of the stacked atlas (audfix-pt-g: seam samples do not read the next page).
        map.height as usize,
    );
    eprintln!("RESTITCH_CONTROL {stats:?}");
    for (word, p) in map.texels.iter_mut().zip(&values) {
        *word = regia_asset::baked_gi::rgb9e5([p[0], p[1], p[2]].map(|v| v * map.exposure));
    }
    if !map.directions.is_empty() {
        let directions: Vec<_> = values.iter().map(|p| [p[3], p[4], p[5], p[6]]).collect();
        map.set_directions(&directions)
            .expect("encode stitched directions");
    }
    map
}

/// An intentionally expensive diagnostic: preserve every entry texel and local UV but put an
/// empty layer between entries. This isolates cross-entry filtering/neighborhood interference
/// from shadow transitions and chart packing inside an entry. It is not a shipping tier policy.
#[test]
#[ignore = "manual entry-isolation control; original tier acceptance remains mandatory"]
fn tier_entry_isolation_study() {
    let tmp = project("lightmap_gallery_entry_isolation");
    let dir = &tmp.0;
    let meshes: Vec<CpuMesh> = gallery::parts().iter().map(|p| imported(dir, p)).collect();
    let top = bake_with(
        dir,
        gallery::DEFAULT_OUT,
        &bake_args(TOP_DENSITY, "1", &samples()),
    );
    let mut maps = vec![("source".to_string(), top.clone())];
    for factor in [2, 4] {
        let (mut base, _) =
            regia_asset_pipeline::lightmap_tiers::derive_lightmap_tier(&top, factor).expect("derive control");
        if factor == 4 {
            base.directions.clear();
        }
        let mut isolated = base.clone();
        let page_len = (base.width * base.height) as usize;
        let len = page_len * base.entries.len() * 2;
        isolated.texels = vec![0; len];
        isolated.coverage = vec![regia_asset::lightmap::Coverage::EMPTY; len];
        if !base.directions.is_empty() {
            isolated.directions = vec![0; len];
        }
        for (e, entry) in base.entries.iter().enumerate() {
            let page = 2 * e + 1;
            isolated.entries[e].page = page as u32;
            let [x, y, w, h] = entry.rect;
            for row in y..y + h {
                let old = entry.page as usize * page_len + (row * base.width + x) as usize;
                let new = page * page_len + (row * base.width + x) as usize;
                let w = w as usize;
                isolated.texels[new..new + w].copy_from_slice(&base.texels[old..old + w]);
                isolated.coverage[new..new + w].copy_from_slice(&base.coverage[old..old + w]);
                if !base.directions.is_empty() {
                    isolated.directions[new..new + w].copy_from_slice(&base.directions[old..old + w]);
                }
            }
            assert_eq!(isolated.entries[e].rect, entry.rect);
            assert_eq!(isolated.entries[e].scale_offset, entry.scale_offset);
        }
        assert_eq!(isolated.unwraps, base.unwraps);
        let stitched = restitch_control(&base, &meshes);
        let isolated_stitched = restitch_control(&isolated, &meshes);
        for (name, map) in [
            ("base", base),
            ("isolated", isolated),
            ("stitched", stitched),
            ("isolated_stitched", isolated_stitched),
        ] {
            eprintln!(
                "ENTRY_CONTROL factor={factor} mode={name} pages={} seam={} dark={}",
                map.pages(),
                seams_with_geometry(&map, &meshes),
                dark_points(&map, &meshes).len(),
            );
            maps.push((format!("factor_{factor}_{name}"), map));
        }
    }
    write_tier_evidence(
        "entry_isolation",
        &maps.iter().map(|(n, m)| (n.as_str(), m)).collect::<Vec<_>>(),
        &meshes,
        json!({"diagnostic_only": true, "isolation_preserves_entry_texels_and_local_uvs": true, "stitch_changes_texels": true}),
    );
}

#[path = "tier_layout.rs"]
mod target_layout;

/// Diagnostic only (ADR-5125): ULTRA_LOW (factor four) derived with no repack and with its charts
/// repacked at paddings one to three, BC6H without directions as the cook ships it: seams with the
/// real geometry, dark points (bake window and the old tier window), bytes and atlas size. LOW
/// (factor two) at padding two for comparison. The default table's padding stays the acceptance input.
#[test]
#[ignore = "hand-run chart padding study; the default tier table is what acceptance cooks"]
fn tier_repack_padding_study() {
    let tmp = project("lightmap_gallery_repack_padding");
    let dir = &tmp.0;
    let meshes: Vec<CpuMesh> = gallery::parts().iter().map(|p| imported(dir, p)).collect();
    let top = bake_with(
        dir,
        gallery::DEFAULT_OUT,
        &bake_args(TOP_DENSITY, "1", &samples()),
    );
    let mut maps = vec![("source".to_string(), top.clone())];
    for (factor, padding) in [(4, None), (4, Some(1)), (4, Some(2)), (4, Some(3)), (2, Some(2))] {
        let (mut map, stats) =
            regia_asset_pipeline::lightmap_tiers::derive_lightmap_tier_padded(&top, factor, padding)
                .expect("derive");
        if factor == 4 {
            map.directions.clear();
            map.direction_blocks = None;
            map.encode_texels(LightmapTexelEncoding::Bc6h)
                .expect("BC6H as the cook ships ULTRA_LOW");
        }
        let bytes = map.to_bytes().expect("encode").len();
        eprintln!(
            "REPACK factor={factor} padding={padding:?} seam_p95={} bake_window_dark={} tier_window_dark={} bytes={bytes} atlas={}x{} repacked_charts={} border_fit_rows={} frozen={} steep={}",
            seams_with_geometry(&map, &meshes),
            dark_points_against(&map, &top, &meshes).len(),
            dark_points(&map, &meshes).len(),
            map.width,
            map.height,
            stats.repacked_charts,
            stats.border_fit_rows,
            stats.border_frozen,
            stats.border_steep
        );
        maps.push((format!("factor_{factor}_padding_{padding:?}"), map));
    }
    write_tier_evidence(
        "repack_padding",
        &maps.iter().map(|(n, m)| (n.as_str(), m)).collect::<Vec<_>>(),
        &meshes,
        json!({"diagnostic_only": true}),
    );
}

/// An edge by its two welded end positions (bit patterns), lower first.
type WeldedEdge = ([u32; 3], [u32; 3]);

/// Diagnostic only (ADR-5125): every UV2 cut of the pillars (the ULTRA_LOW seam's worst entry) on
/// the bake and on ULTRA_LOW at the default table's chart padding, uncompressed: per cut, both sides'
/// luminance along it on each map, the tier step, and where each side's edge falls in tier texels.
#[test]
#[ignore = "hand-run pillar seam diagnosis; the default tier table is what acceptance cooks"]
fn tier_pillar_seam_study() {
    let tmp = project("lightmap_gallery_pillar_seams");
    let dir = &tmp.0;
    let meshes: Vec<CpuMesh> = gallery::parts().iter().map(|p| imported(dir, p)).collect();
    let top = bake_with(
        dir,
        gallery::DEFAULT_OUT,
        &bake_args(TOP_DENSITY, "1", &samples()),
    );
    let (tier, stats) = regia_asset_pipeline::lightmap_tiers::derive_lightmap_tier_padded(
        &top,
        4,
        Some(regia_asset_pipeline::lightmap_tiers::BLOCK_TIER_CHART_PADDING),
    )
    .expect("derive");
    eprintln!("PILLAR stats {stats:?}");
    let (pi, part) = gallery::parts()
        .into_iter()
        .enumerate()
        .find(|(_, p)| p.name == "Pillars")
        .expect("pillars");
    let mesh = &meshes[pi];
    for (name, map) in [("bake", &top), ("ultra_low", &tier)] {
        let e = map.entry_for_key(entry_key(&part)).expect("an entry");
        let u = &map.unwraps[e];
        let [_, _, rw, rh] = map.entries[e].rect;
        let pos = |v: u32| mesh.positions[u.xref[v as usize] as usize];
        let key = |p: [f32; 3]| p.map(|c| (c + 0.0).to_bits());
        let mut sides: std::collections::BTreeMap<WeldedEdge, Vec<(u32, u32)>> =
            std::collections::BTreeMap::new();
        for t in u.indices.as_chunks::<3>().0 {
            for c in 0..3 {
                let (a, b) = (t[c], t[(c + 1) % 3]);
                let (ka, kb) = (key(pos(a)), key(pos(b)));
                let (k, s) = if ka <= kb {
                    ((ka, kb), (a, b))
                } else {
                    ((kb, ka), (b, a))
                };
                sides.entry(k).or_default().push(s);
            }
        }
        for (k, s) in &sides {
            if s.len() != 2 {
                continue;
            }
            let (s0, s1) = (s[0], s[1]);
            if u.uv[s0.0 as usize] == u.uv[s1.0 as usize] && u.uv[s0.1 as usize] == u.uv[s1.1 as usize] {
                continue;
            }
            let at = |s: (u32, u32), t: f32| {
                let (pa, pb) = (u.uv[s.0 as usize], u.uv[s.1 as usize]);
                [pa[0] + (pb[0] - pa[0]) * t, pa[1] + (pb[1] - pa[1]) * t]
            };
            let tex = |s: (u32, u32)| {
                let p = u.uv[s.0 as usize];
                [p[0] * rw as f32, p[1] * rh as f32]
            };
            let mut line = String::new();
            let mut err = 0.0;
            for i in 0..16 {
                let t = (i as f32 + 0.5) / 16.0;
                let (la, lb) = (
                    lum(map.sample_entry(e, at(s0, t))),
                    lum(map.sample_entry(e, at(s1, t))),
                );
                err += (la - lb).abs() / (0.5 * (la + lb)).max(1e-9) / 16.0;
                line.push_str(&format!(" {la:.0}/{lb:.0}"));
            }
            let w = |b: [u32; 3]| b.map(f32::from_bits);
            eprintln!(
                "PILLAR_CUT {name} {:?}-{:?} err={err:.3} side0_tex={:?} side1_tex={:?}:{line}",
                w(k.0),
                w(k.1),
                tex(s0),
                tex(s1)
            );
            // Across each side at mid-edge: the texel column under the point, three texels either
            // way across the cut, luminance and coverage.
            let [rx, ry, _, _] = map.entries[e].rect;
            let ry = ry + map.entries[e].page * map.height;
            for (label, s) in [("side0", s0), ("side1", s1)] {
                let p = at(s, 0.5);
                let (px, py) = (p[0] * rw as f32, p[1] * rh as f32);
                let mut col = String::new();
                for dy in -3i64..=3 {
                    let y = py.floor() as i64 + dy + i64::from(ry);
                    let x = px.floor() as i64 + i64::from(rx);
                    if x >= 0 && y >= 0 {
                        let (x, y) = (x as u32, y as u32);
                        col.push_str(&format!(
                            " y{}:{:.0}c{}",
                            i64::from(y) - i64::from(ry),
                            lum(map.irradiance_at(x, y)),
                            map.coverage_at(x, y)
                        ));
                    }
                }
                eprintln!("PILLAR_ACROSS {name} {label} at ({px:.2}, {py:.2}):{col}");
            }
        }
    }
}

/// The default game setup's bake density: ULTRA, 16 texels per metre (module docs).
const ULTRA_DENSITY: &str = "16";

/// Diagnostic only: the gallery baked at ULTRA (16 texels per metre, the density a game bakes at by
/// default) and cooked with the default table, which the acceptance tests above (baked at HIGH,
/// `TOP_DENSITY`) never measure. There LOW is factor four and repacks its charts (the bake's 2-texel
/// padding over four is no whole texel), and ULTRA_LOW is factor eight (a quarter of a texel). For
/// LOW and ULTRA_LOW: seam p95 with the real geometry, the carried seam p95, dark points in the
/// bake's window (ADR-5125) and in the old tier window, and the derive's chart figures. One line per
/// tier, prefixed `ULTRA_BAKE`; the maps go to the evidence directory when it is set.
#[test]
#[ignore = "hand-run study of the default ULTRA bake; acceptance bakes at TOP_DENSITY"]
fn tier_ultra_bake_study() {
    let tmp = project("lightmap_gallery_ultra_bake");
    let dir = &tmp.0;
    let meshes: Vec<CpuMesh> = gallery::parts().iter().map(|p| imported(dir, p)).collect();
    let s = samples();
    let top = bake_with(dir, gallery::DEFAULT_OUT, &bake_args(ULTRA_DENSITY, "1", &s));
    assert_eq!(top.density, 16.0);
    let (v, files) = cook_tiers(dir);
    eprintln!(
        "ULTRA_BAKE samples={s} bake atlas={}x{} pages={} padding={} seam_p95={:.4} dark={}",
        top.width,
        top.height,
        top.pages(),
        top.padding,
        seams_with_geometry(&top, &meshes),
        dark_points(&top, &meshes).len()
    );
    let mut maps = vec![("bake", top.clone())];
    let mut summary = serde_json::Map::new();
    for (tier, factor) in [("low", 4u32), ("ultra_low", 8)] {
        let map = tier_map(&files, tier);
        let row = tier_row(&v, tier);
        assert_eq!(row["factor"], factor, "{tier}: {row}");
        let seam = seams_with_geometry(&map, &meshes);
        let carried = map
            .entries
            .iter()
            .filter_map(|e| e.seams.map(|s| s.p95))
            .fold(0.0, f32::max);
        let dark = dark_points_against(&map, &top, &meshes);
        let old = dark_points(&map, &meshes).len();
        for d in dark.iter().take(12) {
            eprintln!(
                "ULTRA_BAKE {tier}: {} at ({:.2}, {:.2}, {:.2}) reads {:.4} lux (the bake {:.4}) beside {:.4}",
                d.part, d.world[0], d.world[1], d.world[2], d.tier, d.bake, d.near
            );
        }
        eprintln!(
            "ULTRA_BAKE tier={tier} factor={factor} density={} encoding={} atlas={}x{} bytes={} seam_p95={seam:.4} carried_p95={carried:.4} bake_window_dark={} tier_window_dark={old} repacked_charts={} chart_padding={} border_frozen={} border_steep={}",
            map.density,
            map.texel_encoding().name(),
            map.width,
            map.height,
            row["bytes"],
            dark.len(),
            row["derive"]["repacked_charts"],
            row["derive"]["chart_padding"],
            row["derive"]["border_frozen"],
            row["derive"]["border_steep"]
        );
        summary.insert(
            tier.to_string(),
            json!({"factor": factor, "seam_p95": seam, "carried_p95": carried,
                "bake_window_dark": dark.len(), "tier_window_dark": old, "cook": row}),
        );
        maps.push((tier, map));
    }
    write_tier_evidence(
        "ultra_bake",
        &maps.iter().map(|(n, m)| (*n, m)).collect::<Vec<_>>(),
        &meshes,
        json!({"diagnostic_only": true, "bake_density": 16, "samples": s, "tiers": summary}),
    );
}
