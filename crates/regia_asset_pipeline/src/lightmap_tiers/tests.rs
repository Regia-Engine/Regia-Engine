// SPDX-License-Identifier: MPL-2.0
//! Tests of the tier derive (ADR-3120): synthetic bakes with known charts and values.

use super::*;
use regia_asset::lightmap::{
    LightmapBakeInfo, LightmapEntry, LightmapLayout, LightmapTexelEncoding, MeshUnwrap,
};

/// A quad's two triangles over unwrapped vertices `v..v+4` (corners counter-clockwise).
fn quad(v: u32) -> [u32; 6] {
    [v, v + 1, v + 2, v, v + 2, v + 3]
}

/// An entry of `w × h` texels at `at` holding the given quads (UV rectangles in texels of the entry,
/// `[x0, y0, x1, y1]`), each its own chart; source vertices `xref`.
fn entry_of(
    name: &str,
    at: [u32; 2],
    w: u32,
    h: u32,
    quads: &[[f32; 4]],
    xref: Vec<u32>,
) -> (LightmapEntry, MeshUnwrap) {
    let mut uv = Vec::new();
    let mut indices = Vec::new();
    for (k, q) in quads.iter().enumerate() {
        let [x0, y0, x1, y1] = *q;
        for p in [[x0, y0], [x1, y0], [x1, y1], [x0, y1]] {
            uv.push([p[0] / w as f32, p[1] / h as f32]);
        }
        indices.extend(quad(4 * k as u32));
    }
    let source = xref.iter().copied().max().map_or(0, |m| m + 1);
    (
        LightmapEntry {
            name: name.into(),
            source_vertices: source,
            rect: [at[0], at[1], w, h],
            scale_offset: [0.0; 4],
            vertex_count: 0,
            index_count: 0,
            first_vertex: 0,
            first_index: 0,
            charts: quads.len() as u32,
            area_m2: 1.0,
            texels_per_metre: 16.0,
            key: None,
            seams: None,
            page: 0,
        },
        MeshUnwrap { xref, uv, indices },
    )
}

/// A bake of `entries` on a `w × h` atlas: every texel the bake's rule gives a chart is BAKED with
/// `value(entry, chart, x, y)` (atlas texel), the rest EMPTY.
fn bake(
    w: u32,
    h: u32,
    entries: Vec<(LightmapEntry, MeshUnwrap)>,
    value: impl Fn(usize, u32, u32, u32) -> [f32; 3],
    directional: bool,
) -> Lightmap {
    let n = (w * h) as usize;
    let mut irr = vec![[0.0f32; 3]; n];
    let mut cov = vec![Coverage::EMPTY; n];
    let (mut es, mut us) = (Vec::new(), Vec::new());
    for (k, (mut e, u)) in entries.into_iter().enumerate() {
        let [rx, ry, rw, rh] = e.rect;
        e.scale_offset = [
            rw as f32 / w as f32,
            rh as f32 / h as f32,
            rx as f32 / w as f32,
            ry as f32 / h as f32,
        ];
        let (tri_chart, _) = triangle_charts(&u.indices, u.uv.len());
        let grid = chart_grid(&u.uv, &u.indices, &tri_chart, rw, rh);
        for y in 0..rh {
            for x in 0..rw {
                let c = grid[(y * rw + x) as usize];
                if c != u32::MAX {
                    let t = ((ry + y) * w + rx + x) as usize;
                    irr[t] = value(k, c, rx + x, ry + y);
                    cov[t] = Coverage::BAKED;
                }
            }
        }
        es.push(e);
        us.push(u);
    }
    let mut map = Lightmap::from_irradiance(
        LightmapLayout {
            width: w,
            height: h,
            density: 16.0,
            padding: 2,
            entries: es,
            unwraps: us,
        },
        &irr,
        cov,
        LightmapBakeInfo::default(),
    )
    .expect("valid bake");
    if directional {
        let dirs: Vec<[f32; 4]> = (0..n).map(|_| [0.0, 0.8, 0.0, 0.9]).collect();
        map.set_directions(&dirs).expect("one per texel");
    }
    map
}

/// Two charts of one wall cut along a shared edge (source vertices 1 and 2), two texels apart in a
/// 32 × 16 atlas: chart A on x 2..14, chart B on x 16..30.
fn two_charts(a: f32, b: f32, directional: bool) -> Lightmap {
    let e = entry_of(
        "wall",
        [0, 0],
        32,
        16,
        &[[2.0, 2.0, 14.0, 14.0], [16.0, 2.0, 30.0, 14.0]],
        vec![0, 1, 2, 3, 1, 4, 5, 2],
    );
    bake(
        32,
        16,
        vec![e],
        |_, c, _, _| if c == 0 { [a; 3] } else { [b; 3] },
        directional,
    )
}

/// The chart of every tier texel inside `entry`, by the bake's rule plus the gutter fill: the chart
/// whose value the texel holds. Returns (texel index, chart) of the rasterized texels only.
fn tier_charts(map: &Lightmap, entry: usize) -> Vec<(usize, u32)> {
    let e = &map.entries[entry];
    let u = &map.unwraps[entry];
    let [qx, qy, tw, th] = e.rect;
    let (tri_chart, _) = triangle_charts(&u.indices, u.uv.len());
    let grid = chart_grid(&u.uv, &u.indices, &tri_chart, tw, th);
    let mut out = Vec::new();
    for y in 0..th {
        for x in 0..tw {
            let c = grid[(y * tw + x) as usize];
            if c != u32::MAX {
                out.push((((qy + y) * map.width + qx + x) as usize, c));
            }
        }
    }
    out
}

fn close(a: f32, b: f32, rel: f32) -> bool {
    (a - b).abs() <= rel * a.abs().max(b.abs()).max(1e-6)
}

/// The cut wall's two charts at every factor: each tier texel of a chart holds that chart's light
/// (RGB9E5 precision) and every gutter texel holds one chart's light, never a mix of the two.
#[test]
fn a_tier_never_mixes_two_charts() {
    let src = two_charts(100.0, 1.0, false);
    for factor in [2, 4, 8] {
        let (tier, stats) = derive_lightmap_tier(&src, factor).expect("derives");
        assert_eq!(stats.factor, factor);
        for (t, c) in tier_charts(&tier, 0) {
            let want = if c == 0 { 100.0 } else { 1.0 };
            let got = tier.irradiance_at(t as u32 % tier.width, t as u32 / tier.width)[0];
            assert!(
                close(got, want, 0.01),
                "factor {factor}: texel {t} of chart {c} is {got}, not {want}"
            );
        }
        let [x, y, w, h] = tier.entries[0].rect;
        for ty in y..y + h {
            for tx in x..x + w {
                let v = tier.irradiance_at(tx, ty)[0];
                assert!(
                    close(v, 100.0, 0.01) || close(v, 1.0, 0.01),
                    "factor {factor}: texel ({tx}, {ty}) is {v}: a mix of the two charts"
                );
                assert_ne!(tier.coverage_at(tx, ty), Coverage::EMPTY, "the gutter is filled");
            }
        }
    }
}

/// The derive keeps the unwrap bit for bit, scales the density, validates and round-trips through
/// the current format.
#[test]
fn a_tier_keeps_the_unwrap_and_is_a_current_flmap() {
    let src = two_charts(10.0, 20.0, true);
    let (tier, stats) = derive_lightmap_tier(&src, 4).expect("derives");
    assert_eq!(tier.unwraps, src.unwraps, "the UV2 layout is the bake's");
    assert_eq!((tier.width, tier.height), (8, 4));
    assert_eq!((stats.width, stats.height), (8, 4));
    assert_eq!(tier.density, 4.0);
    assert_eq!(tier.entries[0].rect, [0, 0, 8, 4]);
    assert_eq!(tier.entries[0].texels_per_metre, 4.0);
    assert!(tier.is_directional());
    tier.validate().expect("validates");
    let bytes = tier.to_bytes().expect("writes");
    assert_eq!(
        regia_asset::lightmap::lightmap_version(&bytes).expect("version"),
        regia_asset::lightmap::LIGHTMAP_FORMAT_VERSION
    );
    assert_eq!(Lightmap::from_bytes(&bytes).expect("reads"), tier);
    // Factor 1 is the bake itself.
    assert_eq!(derive_lightmap_tier(&src, 1).expect("same").0, src);
    for bad in [0, 3, 32] {
        let e = derive_lightmap_tier(&src, bad).expect_err("bad factor");
        assert!(e.starts_with("lightmap_tier_derive:"), "{e}");
    }
}

/// A linear gradient over one large chart: a tier texel is the area mean of its footprint, which on
/// a linear field is the value at its centre.
#[test]
fn a_gradient_downsamples_to_its_area_mean() {
    let e = entry_of(
        "floor",
        [0, 0],
        64,
        64,
        &[[0.0, 0.0, 64.0, 64.0]],
        vec![0, 1, 2, 3],
    );
    let src = bake(
        64,
        64,
        vec![e],
        |_, _, x, y| [10.0 + x as f32, 10.0 + y as f32, 5.0],
        false,
    );
    for factor in [2u32, 4, 8, 16] {
        let (tier, stats) = derive_lightmap_tier(&src, factor).expect("derives");
        assert_eq!(
            stats.nearest + stats.looked_up,
            0,
            "every texel has its own baked texels"
        );
        for ty in 0..tier.height {
            for tx in 0..tier.width {
                let c = [
                    (tx as f32 + 0.5) * factor as f32 - 0.5,
                    (ty as f32 + 0.5) * factor as f32 - 0.5,
                ];
                let v = tier.irradiance_at(tx, ty);
                assert!(
                    close(v[0], 10.0 + c[0], 0.01) && close(v[1], 10.0 + c[1], 0.01),
                    "factor {factor}: ({tx}, {ty}) is {v:?}, centre {c:?}"
                );
            }
        }
    }
}

/// A constant bake stays constant at every tier, gutters included: no darkening at chart edges.
#[test]
fn a_constant_bake_keeps_its_level_at_every_tier() {
    let src = two_charts(42.0, 42.0, false);
    for factor in [2, 4, 8, 16] {
        let (tier, _) = derive_lightmap_tier(&src, factor).expect("derives");
        for (k, c) in tier.coverage.iter().enumerate() {
            if *c != Coverage::EMPTY {
                let v = tier.irradiance_at(k as u32 % tier.width, k as u32 / tier.width);
                assert!(
                    v.iter().all(|&x| close(x, 42.0, 0.01)),
                    "factor {factor} texel {k}: {v:?}"
                );
            }
        }
    }
}

/// A chart smaller than a tier texel (a thin jamb) still gets a texel of its own, holding its own
/// light, where the bake's rule alone would leave it none.
#[test]
fn a_sub_texel_chart_gets_a_texel_of_its_own() {
    let e = entry_of(
        "jamb",
        [0, 0],
        32,
        16,
        &[[2.0, 2.0, 20.0, 14.0], [24.0, 4.0, 27.0, 7.0]],
        vec![0, 1, 2, 3, 4, 5, 6, 7],
    );
    let src = bake(
        32,
        16,
        vec![e],
        |_, c, _, _| if c == 0 { [5.0; 3] } else { [80.0; 3] },
        false,
    );
    let (tier, stats) = derive_lightmap_tier(&src, 8).expect("derives");
    assert!(stats.conservative_charts >= 1, "{stats:?}");
    // The small chart's centroid (25.5, 5.5) of the bake is tier texel (3, 0).
    let v = tier.irradiance_at(3, 0)[0];
    assert!(close(v, 80.0, 0.01), "the small chart's texel holds {v}");
    // Its lookup at the chart's centre reads its own light.
    let lit = tier.sample_entry(0, [25.5 / 32.0, 5.5 / 16.0])[0];
    assert!(lit > 40.0, "the tier reads {lit} at the small chart");
}

/// Rectangles at positions that are not a multiple of the factor would overlap when scaled: the
/// derive re-places them, and the result still validates and keeps each entry's light.
#[test]
fn overlapping_scaled_rectangles_are_re_placed() {
    let a = entry_of("a", [0, 0], 7, 8, &[[1.0, 1.0, 6.0, 7.0]], vec![0, 1, 2, 3]);
    let b = entry_of("b", [7, 0], 9, 8, &[[1.0, 1.0, 8.0, 7.0]], vec![0, 1, 2, 3]);
    let src = bake(
        16,
        8,
        vec![a, b],
        |e, _, _, _| if e == 0 { [3.0; 3] } else { [9.0; 3] },
        false,
    );
    let (tier, stats) = derive_lightmap_tier(&src, 2).expect("derives");
    assert!(stats.repacked, "4 + 4 texels from x 3 overlaps 5 from x 3");
    tier.validate().expect("validates");
    for (e, want) in [(0, 3.0), (1, 9.0)] {
        let got = tier.sample_entry(e, [0.5, 0.5])[0];
        assert!(close(got, want, 0.01), "entry {e}: {got}");
    }
}

/// The same bake and rule give the same bytes, run after run (no threads, no hash order).
#[test]
fn the_same_bake_gives_the_same_bytes() {
    let src = two_charts(7.0, 70.0, true);
    let table = LightmapTierTable::default();
    let one = cook_lightmap_tiers(&src, &table, "linux").expect("cooks");
    let two = cook_lightmap_tiers(&src, &table, "linux").expect("cooks");
    assert_eq!(one, two);
    assert_eq!(one.len(), 4);
}

/// The default table is ADR-3001's: ULTRA 16, HIGH 8, LOW 4, ULTRA_LOW 2 texels per metre, the last
/// without directions in a block encoding; factors from a 16 texels per metre bake.
#[test]
fn the_default_table_is_adr_3001s() {
    let table = LightmapTierTable::default();
    let want = [
        ("high", 8.0, 2),
        ("low", 4.0, 4),
        ("ultra", 16.0, 1),
        ("ultra_low", 2.0, 8),
    ];
    assert_eq!(table.tiers.len(), 4);
    for (name, density, factor) in want {
        let rule = table.rule(name).expect("tier");
        assert_eq!(rule.density, Some(density));
        assert_eq!(rule.factor_for(16.0), Ok(factor));
        assert_eq!(rule.directional, name != "ultra_low");
        assert!(rule.budget_bytes().is_some());
    }
    assert_eq!(
        table.rule("ultra_low").expect("tier").encoding,
        Some(LightmapCookEncoding::Auto)
    );
    // A bake at HIGH density gives ULTRA the bake itself.
    assert_eq!(table.rule("ultra").expect("tier").factor_for(8.0), Ok(1));
    assert!(
        table
            .rule("medium")
            .expect_err("unknown")
            .starts_with("lightmap_tier_unknown:")
    );
    let bad = LightmapTierRule {
        density: Some(5.0),
        ..LightmapTierRule::default()
    };
    assert!(
        bad.factor_for(16.0)
            .expect_err("not a power of two")
            .starts_with("lightmap_tier_rule:")
    );
    let mut over = BTreeMap::new();
    over.insert("Low".to_string(), LightmapTierRule::default());
    assert!(LightmapTierTable::with_overrides(&over).is_err());
    let mut over = BTreeMap::new();
    over.insert(
        "low".to_string(),
        LightmapTierRule {
            none: true,
            ..LightmapTierRule::default()
        },
    );
    let table = LightmapTierTable::with_overrides(&over).expect("valid");
    assert!(table.rule("low").expect("tier").none);
    assert_eq!(table.tiers.len(), 4);
}

/// The cook of one tier: encoding by platform, the direction section dropped where the rule says,
/// the bytes are the written file's, and the budget verdict.
#[test]
fn a_tier_cook_reports_its_encoding_bytes_and_budget() {
    let src = two_charts(10.0, 30.0, true);
    let table = LightmapTierTable::default();
    let low = table.rule("ultra_low").expect("tier");
    let (bytes, report) = cook_lightmap_tier(&src, "ultra_low", low, "linux").expect("cooks");
    let bytes = bytes.expect("ships");
    assert_eq!(report.encoding, "bc6h");
    assert!(!report.directional);
    assert_eq!(report.bytes, bytes.len() as u64);
    assert_eq!(report.verdict, TierVerdict::Fits);
    assert_eq!(report.density, 2.0);
    let back = Lightmap::from_bytes(&bytes).expect("reads");
    assert!(!back.is_directional());
    assert_eq!(back.texel_encoding(), LightmapTexelEncoding::Bc6h);
    let (_, android) = cook_lightmap_tier(&src, "ultra_low", low, "android").expect("cooks");
    assert_eq!(android.encoding, "astc_hdr");
    // HIGH keeps the bake's encoding (RGB9E5 here) and its directions.
    let (high, report) =
        cook_lightmap_tier(&src, "high", table.rule("high").expect("tier"), "linux").expect("cooks");
    assert_eq!(report.encoding, "rgb9e5");
    assert!(report.directional);
    assert!(
        Lightmap::from_bytes(&high.expect("ships"))
            .expect("reads")
            .is_directional()
    );
    // Over budget: the tier ships without it and says so.
    let tight = LightmapTierRule {
        budget_mib: Some(0.0001),
        ..low.clone()
    };
    let (none, report) = cook_lightmap_tier(&src, "ultra_low", &tight, "linux").expect("cooks");
    assert!(none.is_none());
    assert_eq!(report.verdict, TierVerdict::OverBudget);
    assert!(report.bytes > report.budget_bytes.expect("budget"));
    let off = LightmapTierRule {
        none: true,
        ..LightmapTierRule::default()
    };
    let (none, report) = cook_lightmap_tier(&src, "off", &off, "linux").expect("cooks");
    assert!(none.is_none());
    assert_eq!(report.verdict, TierVerdict::None);
    assert_eq!(report.bytes, 0);
}

/// The stored seam figures carry over with the step the downsample added: a bake whose two charts
/// agree across the cut keeps its figures; one that disagrees cannot report less than it stored.
#[test]
fn stored_seams_carry_over_to_the_tier() {
    let mut src = two_charts(40.0, 40.0, false);
    let stored = LightmapSeams {
        edges: 1,
        mean: 0.01,
        p95: 0.02,
        max: 0.03,
        worst_chart: 0,
    };
    src.entries[0].seams = Some(stored);
    let (tier, _) = derive_lightmap_tier(&src, 4).expect("derives");
    let got = tier.entries[0].seams.expect("carried");
    assert!(close(got.p95, 0.02, 1e-3), "{got:?}");
    let mut uneven = two_charts(40.0, 10.0, false);
    uneven.entries[0].seams = Some(stored);
    let (tier, _) = derive_lightmap_tier(&uneven, 4).expect("derives");
    assert!(tier.entries[0].seams.expect("carried").p95 >= 0.02);
    // An entry the bake did not measure stays unmeasured.
    let (tier, _) = derive_lightmap_tier(&two_charts(1.0, 2.0, false), 2).expect("derives");
    assert_eq!(tier.entries[0].seams, None);
}

/// `src` with every entry on a page of its own at the same rectangle (ADR-3300).
fn one_page_per_entry(src: &Lightmap) -> Lightmap {
    let n = src.page_texels();
    let pages = src.entries.len();
    let mut irr = vec![[0.0f32; 3]; n * pages];
    let mut cov = vec![Coverage::EMPTY; n * pages];
    let mut dirs = vec![[0.0f32; 4]; if src.is_directional() { n * pages } else { 0 }];
    let mut entries = src.entries.clone();
    for (k, e) in entries.iter_mut().enumerate() {
        let [rx, ry, rw, rh] = e.rect;
        for y in ry..ry + rh {
            for x in rx..rx + rw {
                let t = k * n + (y * src.width + x) as usize;
                irr[t] = src.irradiance_at(x, y);
                cov[t] = src.coverage_at(x, y);
                if let Some(d) = src.direction_at(x, y) {
                    dirs[t] = d;
                }
            }
        }
        e.page = k as u32;
    }
    let mut map = Lightmap::from_irradiance(
        LightmapLayout {
            width: src.width,
            height: src.height,
            density: src.density,
            padding: src.padding,
            entries,
            unwraps: src.unwraps.clone(),
        },
        &irr,
        cov,
        src.bake,
    )
    .expect("valid paged bake");
    if src.is_directional() {
        map.set_directions(&dirs).expect("one per texel of every page");
    }
    map
}

/// A paged bake derives a paged tier (ADR-3300): every entry stays on its page, the tier has the
/// bake's pages, and each entry's tier light is the light the same entry gets when the bake is one
/// page: the derive never reads or fills across pages.
#[test]
fn a_paged_bake_derives_a_tier_per_page() {
    let wall = entry_of(
        "wall",
        [0, 0],
        32,
        16,
        &[[2.0, 2.0, 14.0, 14.0], [16.0, 2.0, 30.0, 14.0]],
        vec![0, 1, 2, 3, 1, 4, 5, 2],
    );
    let floor = entry_of(
        "floor",
        [0, 16],
        32,
        16,
        &[[2.0, 2.0, 30.0, 14.0]],
        vec![0, 1, 2, 3],
    );
    let flat = bake(
        32,
        32,
        vec![wall, floor],
        |e, c, x, y| {
            [
                1.0 + e as f32 + 0.1 * c as f32 + 0.01 * x as f32,
                2.0 + 0.02 * y as f32,
                0.5,
            ]
        },
        true,
    );
    let paged = one_page_per_entry(&flat);
    assert_eq!(paged.pages(), 2);
    for factor in [2u32, 4] {
        let (a, _) = derive_lightmap_tier(&flat, factor).expect("one page");
        let (b, stats) = derive_lightmap_tier(&paged, factor).expect("two pages");
        assert_eq!(b.pages(), 2, "factor {factor}");
        assert_eq!(b.entries[1].page, 1);
        assert!(!stats.repacked);
        let back = Lightmap::from_bytes(&b.to_bytes().expect("writes")).expect("reads");
        assert_eq!(back, b);
        for k in 0..2 {
            for local in [[0.2, 0.3], [0.5, 0.5], [0.8, 0.7], [0.45, 0.15]] {
                let (pa, pb) = (a.sample_entry(k, local), b.sample_entry(k, local));
                assert!(
                    (0..3).all(|c| (pa[c] - pb[c]).abs() <= 1e-4 * pa[c].abs().max(1.0)),
                    "factor {factor} entry {k} {local:?}: {pa:?} vs {pb:?}"
                );
                let n = [0.0, 0.6, 0.8];
                let (sa, sb) = (
                    a.shade_entry(k, local, n, false),
                    b.shade_entry(k, local, n, false),
                );
                assert!(
                    (0..3).all(|c| (sa[c] - sb[c]).abs() <= 0.02 * sa[c].abs().max(1.0)),
                    "factor {factor} entry {k} {local:?}, directional: {sa:?} vs {sb:?}"
                );
            }
        }
    }
}

/// A source whose gutter continues an affine irradiance field must retain that field at chart
/// borders after reduction. Copying the edge mean into the new gutter flattens its derivative.
#[test]
fn a_tier_preserves_the_source_gradient_at_chart_borders() {
    let e = entry_of(
        "gradient",
        [0, 0],
        32,
        16,
        &[[4.0, 4.0, 28.0, 12.0]],
        vec![0, 1, 2, 3],
    );
    let mut src = bake(32, 16, vec![e], |_, _, x, _| [10.0 + x as f32; 3], false);
    for y in 0..src.height {
        for x in 0..src.width {
            let i = (y * src.width + x) as usize;
            if src.coverage[i] == Coverage::EMPTY {
                src.coverage[i] = Coverage::DILATED;
                src.texels[i] = regia_asset::baked_gi::rgb9e5([src.exposure * (10.0 + x as f32); 3]);
            }
        }
    }
    for factor in [2, 4] {
        let (tier, _) = derive_lightmap_tier(&src, factor).expect("derive");
        for uv in [[4.0 / 32.0, 0.5], [28.0 / 32.0, 0.5]] {
            let want = src.sample_entry(0, uv)[0];
            let got = tier.sample_entry(0, uv)[0];
            assert!(
                close(got, want, 0.01),
                "factor {factor}, uv {uv:?}: {got} != {want}"
            );
        }
    }
}

#[test]
fn gutter_gradient_is_bounded_and_does_not_extrapolate_directions() {
    let mut light = vec![[0.0; 3], [1.0; 3], [100.0; 3], [0.0; 3]];
    let mut directions = vec![[0.0; 4], [0.25; 4], [0.75; 4], [0.0; 4]];
    let mut coverage = vec![Coverage::EMPTY, Coverage::BAKED, Coverage::BAKED, Coverage::EMPTY];
    let mut charts = vec![u32::MAX, 0, 0, u32::MAX];
    assert_eq!(
        fill_gutters(
            &mut light,
            &mut directions,
            &mut coverage,
            &mut charts,
            [0, 0, 4, 1],
            4
        ),
        2
    );
    assert_eq!(light, [[0.5; 3], [1.0; 3], [100.0; 3], [150.0; 3]]);
    assert_eq!(directions, [[0.25; 4], [0.25; 4], [0.75; 4], [0.75; 4]]);
}

#[test]
fn gutter_gradient_never_uses_another_charts_inner_texel() {
    let mut light = vec![[0.0; 3], [1.0; 3], [100.0; 3], [0.0; 3]];
    let mut coverage = vec![Coverage::EMPTY, Coverage::BAKED, Coverage::BAKED, Coverage::EMPTY];
    let mut charts = vec![u32::MAX, 0, 1, u32::MAX];
    fill_gutters(&mut light, &mut [], &mut coverage, &mut charts, [0, 0, 4, 1], 4);
    assert_eq!(light, [[1.0; 3], [1.0; 3], [100.0; 3], [100.0; 3]]);
    assert_eq!(charts, [0, 0, 1, 1]);
}

/// The cut wall of [`two_charts`] lit by a field that rises along the wall, baked and stitched as
/// the baker leaves it: each gutter column continues its own chart's field, so both sides' lookups
/// read the same light at the cut (34 at world x = 12 m). Chart A's u 2..14 is world 0..12,
/// chart B's u 16..30 is world 12..26.
fn stitched_wall() -> Lightmap {
    let field = |x: u32| {
        let u = x as f32 + 0.5;
        let world = if x <= 14 { u - 2.0 } else { 12.0 + u - 16.0 };
        10.0 + 2.0 * world
    };
    let e = entry_of(
        "wall",
        [0, 0],
        32,
        16,
        &[[2.0, 2.0, 14.0, 14.0], [16.0, 2.0, 30.0, 14.0]],
        vec![0, 1, 2, 3, 1, 4, 5, 2],
    );
    let mut src = bake(32, 16, vec![e], |_, _, x, _| [field(x); 3], false);
    for y in 0..src.height {
        for x in 0..src.width {
            let i = (y * src.width + x) as usize;
            if src.coverage[i] == Coverage::EMPTY {
                src.coverage[i] = Coverage::DILATED;
                src.texels[i] = regia_asset::baked_gi::rgb9e5([src.exposure * field(x); 3]);
            }
        }
    }
    src
}

/// A smooth seam the bake stitched stays stitched at the tier: both sides of the cut read the
/// bake's light there (ADR-4995). Area means alone read 34 on one side and 36 on the other at
/// factor two, because the one gutter texel left between the charts can hold only one side's
/// continuation.
#[test]
fn a_stitched_seam_stays_stitched_at_every_tier() {
    let src = stitched_wall();
    for y in [0.4f32, 0.5, 0.6] {
        let (a, b) = (
            src.sample_entry(0, [14.0 / 32.0, y])[0],
            src.sample_entry(0, [16.0 / 32.0, y])[0],
        );
        assert!(
            close(a, 34.0, 0.01) && close(b, 34.0, 0.01),
            "the source is stitched: {a} {b}"
        );
    }
    for factor in [2, 4] {
        let (tier, stats) = derive_lightmap_tier(&src, factor).expect("derive");
        assert!(stats.border_fit_rows > 0, "factor {factor}: {stats:?}");
        for y in [0.4f32, 0.5, 0.6] {
            let a = tier.sample_entry(0, [14.0 / 32.0, y])[0];
            let b = tier.sample_entry(0, [16.0 / 32.0, y])[0];
            assert!(
                close(a, b, 0.02) && close(a, 34.0, 0.02) && close(b, 34.0, 0.02),
                "factor {factor}, y {y}: side A reads {a}, side B {b}, the bake 34"
            );
        }
    }
}

/// Where the bake's two sides disagree (a crease, or a cut it never stitched), the texel both
/// sides read is not shared: it keeps its owner's light and the other side is left as it was.
#[test]
fn a_border_fit_never_blends_a_crease() {
    let src = two_charts(100.0, 1.0, false);
    for factor in [2, 4, 8] {
        let (tier, stats) = derive_lightmap_tier(&src, factor).expect("derive");
        // Frozen where both sides' rows read the texel, held where the bake's light is not linear
        // inward of the border: either way the fit saw the step and kept it.
        assert!(
            stats.border_frozen + stats.border_steep > 0,
            "factor {factor}: {stats:?}"
        );
        let a = tier.sample_entry(0, [8.0 / 32.0, 0.5])[0];
        let b = tier.sample_entry(0, [23.0 / 32.0, 0.5])[0];
        assert!(
            close(a, 100.0, 0.01),
            "factor {factor}: chart A's interior reads {a}"
        );
        assert!(
            close(b, 1.0, 0.01),
            "factor {factor}: chart B's interior reads {b}"
        );
    }
}

/// The rasterized texels of the two charts of [`two_charts`] at the tier: the smallest Chebyshev
/// distance between a texel of chart 0 and one of chart 1.
fn chart_gap(tier: &Lightmap) -> u32 {
    let texels = tier_charts(tier, 0);
    let xy = |t: usize| ((t as u32 % tier.width) as i64, (t as u32 / tier.width) as i64);
    let mut gap = u32::MAX;
    for &(a, ca) in &texels {
        for &(b, cb) in &texels {
            if ca == 0 && cb == 1 {
                let ((ax, ay), (bx, by)) = (xy(a), xy(b));
                gap = gap.min((ax - bx).abs().max((ay - by).abs()) as u32);
            }
        }
    }
    gap
}

/// ADR-5125: where the bake's gutter over the factor leaves less than the tier's padding (two
/// texels per side over four or eight: none), the charts are repacked at the tier's resolution
/// with that padding. Only UV2 moves, by whole tier texels per chart; vertices, `xref` and indices
/// are the bake's; the charts are `2 × padding` gutter texels apart; texel density is the tier's;
/// no texel mixes the two charts; the result is a current `.flmap`.
#[test]
fn a_tier_without_a_whole_gutter_repacks_its_charts() {
    let src = two_charts(100.0, 1.0, true);
    assert_eq!(src.padding, 2);
    for (factor, padding) in [(4, 1), (4, 2), (8, 1)] {
        let (tier, stats) = derive_lightmap_tier_padded(&src, factor, Some(padding)).expect("derives");
        let label = format!("factor {factor}, padding {padding}");
        assert_eq!(stats.repacked_charts, 2, "{label}: {stats:?}");
        assert_eq!(stats.chart_padding, padding, "{label}");
        assert_eq!(tier.padding, padding, "{label}");
        let (s, t) = (&src.unwraps[0], &tier.unwraps[0]);
        assert_eq!(t.xref, s.xref, "{label}: the bake's vertices");
        assert_eq!(t.indices, s.indices, "{label}: the bake's triangles");
        assert_ne!(t.uv, s.uv, "{label}: the charts moved");
        let [_, _, tw, th] = tier.entries[0].rect;
        let native = [
            src.entries[0].rect[2].div_ceil(factor) as f32,
            src.entries[0].rect[3].div_ceil(factor) as f32,
        ];
        for chart in [0..4usize, 4..8] {
            let moves: Vec<[f32; 2]> = chart
                .map(|v| {
                    [
                        t.uv[v][0] * tw as f32 - s.uv[v][0] * native[0],
                        t.uv[v][1] * th as f32 - s.uv[v][1] * native[1],
                    ]
                })
                .collect();
            for m in &moves {
                for (v, v0) in m.iter().zip(&moves[0]) {
                    assert!(
                        (v - v.round()).abs() < 1e-3 && (v - v0).abs() < 1e-3,
                        "{label}: a chart moves by one whole-texel translation: {moves:?}"
                    );
                }
            }
        }
        assert!(
            chart_gap(&tier) > 2 * padding,
            "{label}: the charts are {} texels apart",
            chart_gap(&tier)
        );
        assert_eq!(tier.entries[0].texels_per_metre, 16.0 / factor as f32, "{label}");
        assert_eq!(tier.density, 16.0 / factor as f32, "{label}");
        for (k, c) in tier_charts(&tier, 0) {
            let want = if c == 0 { 100.0 } else { 1.0 };
            let got = tier.irradiance_at(k as u32 % tier.width, k as u32 / tier.width)[0];
            assert!(close(got, want, 0.01), "{label}: chart {c} texel reads {got}");
        }
        let [x, y, w, h] = tier.entries[0].rect;
        for ty in y..y + h {
            for tx in x..x + w {
                let v = tier.irradiance_at(tx, ty)[0];
                assert!(
                    close(v, 100.0, 0.01) || close(v, 1.0, 0.01),
                    "{label}: ({tx}, {ty}) reads {v}, a mix"
                );
                assert_ne!(
                    tier.coverage_at(tx, ty),
                    Coverage::EMPTY,
                    "{label}: gutter filled"
                );
            }
        }
        // Each chart's interior reads its own light through its own (moved) UV2.
        let mid = |a: usize, b: usize| [0.5 * (t.uv[a][0] + t.uv[b][0]), 0.5 * (t.uv[a][1] + t.uv[b][1])];
        assert!(close(tier.sample_entry(0, mid(0, 2))[0], 100.0, 0.01), "{label}");
        assert!(close(tier.sample_entry(0, mid(4, 6))[0], 1.0, 0.01), "{label}");
        assert!(tier.is_directional());
        tier.validate().expect("validates");
        let back = Lightmap::from_bytes(&tier.to_bytes().expect("writes")).expect("reads");
        assert_eq!(back.unwraps, tier.unwraps, "{label}: the tier ships its own UV2");
    }
}

/// A tier whose scaled bake gutter already holds the padding keeps the bake's layout: the padded
/// derive is the plain one bit for bit. Out-of-range paddings are refused.
#[test]
fn a_tier_with_a_whole_gutter_keeps_the_bake_layout() {
    let src = two_charts(10.0, 20.0, true);
    let plain = derive_lightmap_tier(&src, 2).expect("derives");
    let padded = derive_lightmap_tier_padded(&src, 2, Some(1)).expect("derives");
    assert_eq!(padded, plain);
    assert_eq!(padded.0.unwraps, src.unwraps);
    assert_eq!(padded.1.repacked_charts, 0);
    assert_eq!(padded.1.chart_padding, 1);
    for bad in [0, 17] {
        let e = derive_lightmap_tier_padded(&src, 4, Some(bad)).expect_err("bad padding");
        assert!(e.starts_with("lightmap_tier_rule:"), "{e}");
    }
}

/// The default rules pad every tier by one texel and the block-encoded ULTRA_LOW by two: from a
/// 16 texels per metre bake with the baker's 2-texel padding, HIGH (factor two) keeps the bake's
/// UV2 and LOW and ULTRA_LOW (four and eight) repack, and the cook reports it.
#[test]
fn the_default_table_pads_every_tier() {
    let table = LightmapTierTable::default();
    for (name, rule) in &table.tiers {
        let want = if name == "ultra_low" {
            BLOCK_TIER_CHART_PADDING
        } else {
            DEFAULT_TIER_CHART_PADDING
        };
        assert_eq!(rule.padding, Some(want), "{name}");
    }
    let src = two_charts(10.0, 30.0, true);
    for (tier, repacked) in [("ultra", 0), ("high", 0), ("low", 2), ("ultra_low", 2)] {
        let (bytes, report) =
            cook_lightmap_tier(&src, tier, table.rule(tier).expect("tier"), "linux").expect("cooks");
        assert_eq!(
            report.derive.repacked_charts, repacked,
            "{tier}: {:?}",
            report.derive
        );
        let map = Lightmap::from_bytes(&bytes.expect("ships")).expect("reads");
        assert_eq!(map.unwraps == src.unwraps, repacked == 0, "{tier}");
        if repacked > 0 {
            let padding = table.rule(tier).ok().and_then(|r| r.padding).expect("padded");
            assert!(chart_gap(&map) > 2 * padding, "{tier}: gap {}", chart_gap(&map));
        }
    }
}

/// A stitched seam stays stitched when the tier's charts are repacked (ADR-5125 with ADR-4995):
/// read through the tier's own UV2 at the same point of the cut, both sides read the bake's 34.
#[test]
fn a_repacked_tier_keeps_a_stitched_seam_stitched() {
    let src = stitched_wall();
    for (factor, padding) in [(4, 1), (4, 2)] {
        let (tier, stats) = derive_lightmap_tier_padded(&src, factor, Some(padding)).expect("derive");
        assert_eq!(stats.repacked_charts, 2);
        assert!(stats.border_fit_rows > 0, "{stats:?}");
        let u = &tier.unwraps[0];
        // The cut: chart A's edge from vertex 1 to 2, chart B's from 4 to 7 (both world x = 12 m).
        let along = |a: usize, b: usize, f: f32| {
            [
                u.uv[a][0] + (u.uv[b][0] - u.uv[a][0]) * f,
                u.uv[a][1] + (u.uv[b][1] - u.uv[a][1]) * f,
            ]
        };
        for f in [0.375f32, 0.5, 0.625] {
            let a = tier.sample_entry(0, along(1, 2, f))[0];
            let b = tier.sample_entry(0, along(4, 7, f))[0];
            assert!(
                close(a, b, 0.02) && close(a, 34.0, 0.02) && close(b, 34.0, 0.02),
                "factor {factor}, padding {padding}, f {f}: side A reads {a}, side B {b}, the bake 34"
            );
        }
    }
}

/// A chart the bake gave no texel of its own reads the bake's light on its own surface, nearest the
/// tier texel's centre (ADR-5125): never the black of an empty bake texel off the chart, even once
/// its own gutter isolates it from its neighbours.
#[test]
fn a_texelless_chart_reads_the_bake_on_its_own_surface() {
    // A sliver one sixth of a texel wide at x 20.0..20.17 of a 32 × 16 entry: no bake texel centre
    // inside it. The bake holds light only on a band at x 19..21 (DILATED, as the baker's gutter
    // fill leaves it); everything else is EMPTY, so at factor four the tier texel's centre (bake
    // x 22.0) reads nothing.
    let e = entry_of(
        "sliver",
        [0, 0],
        32,
        16,
        &[[20.0, 2.0, 20.17, 14.0]],
        vec![0, 1, 2, 3],
    );
    let mut src = bake(32, 16, vec![e], |_, _, _, _| [0.0; 3], false);
    for y in 0..16u32 {
        for x in 19..21u32 {
            let i = (y * 32 + x) as usize;
            src.coverage[i] = Coverage::DILATED;
            src.texels[i] = regia_asset::baked_gi::rgb9e5([src.exposure * 50.0; 3]);
        }
    }
    for i in 0..src.coverage.len() {
        assert_ne!(
            src.coverage[i],
            Coverage::BAKED,
            "the sliver has no texel of its own"
        );
    }
    for (factor, padding) in [(4, None), (4, Some(1)), (8, Some(1))] {
        let (tier, stats) = derive_lightmap_tier_padded(&src, factor, padding).expect("derive");
        assert!(stats.looked_up > 0, "{stats:?}");
        let u = &tier.unwraps[0];
        let mid = [0.5 * (u.uv[0][0] + u.uv[2][0]), 0.5 * (u.uv[0][1] + u.uv[2][1])];
        let got = tier.sample_entry(0, mid)[0];
        assert!(
            close(got, 50.0, 0.02),
            "factor {factor}, padding {padding:?}: the sliver reads {got}, the bake 50 ({stats:?})"
        );
    }
}

/// A baked chart whose vertices are split along an edge is two pieces of the unwrap, packed
/// touching. The repack moves it as one chart (ADR-5125): the shared edge stays where both pieces
/// meet, so no new seam opens inside the chart; a chart the bake packed apart moves on its own.
#[test]
fn a_repack_moves_a_split_chart_as_one() {
    // Pieces 0 (x 2..8) and 1 (x 8..14) touch at x = 8 with their own vertices (1, 2 and 4, 7);
    // piece 2 (x 18..30) is a chart of its own, four texels away (twice the padding).
    let e = entry_of(
        "split",
        [0, 0],
        32,
        16,
        &[
            [2.0, 2.0, 8.0, 14.0],
            [8.0, 2.0, 14.0, 14.0],
            [18.0, 2.0, 30.0, 14.0],
        ],
        vec![0, 1, 2, 3, 1, 4, 5, 2, 6, 7, 8, 9],
    );
    let src = bake(32, 16, vec![e], |_, c, _, _| [10.0 + c as f32; 3], false);
    let (tier, stats) = derive_lightmap_tier_padded(&src, 4, Some(1)).expect("derives");
    assert_eq!(stats.repacked_charts, 2, "two baked charts: {stats:?}");
    let u = &tier.unwraps[0];
    for (a, b) in [(1, 4), (2, 7)] {
        assert!(
            (u.uv[a][0] - u.uv[b][0]).abs() < 1e-6 && (u.uv[a][1] - u.uv[b][1]).abs() < 1e-6,
            "the split edge stays shared: {:?} vs {:?}",
            u.uv[a],
            u.uv[b]
        );
    }
    let [_, _, tw, _] = tier.entries[0].rect;
    let gap = (u.uv[8][0] - u.uv[5][0])
        .abs()
        .min((u.uv[0][0] - u.uv[9][0]).abs())
        * tw as f32;
    let dy = (u.uv[8][1] - u.uv[5][1]).abs();
    assert!(gap >= 2.0 || dy > 0.0, "the separate chart has its own gutter");
}

/// A steep border (the bake's light not linear over the two texels inward: a dark band one texel
/// in) keeps its on-chart texels, but the gutter texel its lookup mostly reads is still fitted to
/// the bake's border light (ADR-5125), even where its fill is a third of that light (a fill
/// extrapolated from a dark inside): its bound takes in the bake's border light. Holding that
/// gutter texel, or bounding it by its fill alone (at most 1.5 × 30), left a narrow chart (a
/// pillar's strip at ULTRA_LOW) reading its fill at the border: a seam the bake stitched.
#[test]
fn a_steep_border_still_fits_its_gutter_texel() {
    // One chart on u 2..10.4: texel columns 2..9 are on it (centres inside), column 10's centre
    // (10.5) is off it, so a lookup at the right edge reads column 10 at 0.9 and column 9 at 0.1.
    let e = entry_of(
        "strip",
        [0, 0],
        16,
        16,
        &[[2.0, 2.0, 10.4, 14.0]],
        vec![0, 1, 2, 3],
    );
    let field = |x: u32| if x == 8 { 30.0 } else { 100.0 };
    let mut src = bake(16, 16, vec![e], |_, _, x, _| [field(x); 3], false);
    for y in 0..src.height {
        for x in 0..src.width {
            let i = (y * src.width + x) as usize;
            if src.coverage[i] == Coverage::EMPTY {
                src.coverage[i] = Coverage::DILATED;
                src.texels[i] = regia_asset::baked_gi::rgb9e5([src.exposure * field(x); 3]);
            }
        }
    }
    src.blocks = None;
    assert_eq!(src.coverage_at(9, 8), Coverage::BAKED);
    assert_eq!(src.coverage_at(10, 8), Coverage::DILATED);
    let border = src.sample_entry(0, [10.4 / 16.0, 0.5])[0];
    assert!(close(border, 100.0, 0.01), "the bake's border light {border}");
    // The tier is the bake at factor one with the gutter column's fill at 30 % of the bake's.
    let n = (src.width * src.height) as usize;
    let mut irr: Vec<[f32; 3]> = (0..n as u32)
        .map(|i| src.irradiance_at(i % src.width, i / src.width))
        .collect();
    for y in 0..src.height {
        irr[(y * src.width + 10) as usize] = [30.0; 3];
    }
    let before: Vec<[f32; 3]> = irr.clone();
    let cov = src.coverage.clone();
    let owner = vec![0u32; n];
    let (tri_chart, _) = triangle_charts(&src.unwraps[0].indices, src.unwraps[0].uv.len());
    let stats = super::border_fit::fit_borders(
        &src,
        &src.entries,
        &src.unwraps,
        &[[16, 16]],
        &[tri_chart],
        &mut super::border_fit::TierTexels {
            irr: &mut irr,
            cov: &cov,
            owner: &owner,
            width: 16,
            height: 16,
        },
    );
    assert!(stats.steep > 0, "the right border is steep: {stats:?}");
    for y in 4..12u32 {
        let i = (y * 16) as usize;
        // The on-chart texel the steep rows read is held exactly; the rest of the chart keeps its
        // values (no row asks for a change there): the band is not widened inward.
        assert_eq!(irr[i + 9], before[i + 9], "the border texel of row {y} moved");
        for x in 2..9usize {
            assert!(
                close(irr[i + x][0], before[i + x][0], 1e-3),
                "texel ({x}, {y}) moved: {} from {}",
                irr[i + x][0],
                before[i + x][0]
            );
        }
        // The gutter texel is fitted back toward the bake's border light.
        let read = 0.1 * irr[i + 9][0] + 0.9 * irr[i + 10][0];
        assert!(
            close(read, border, 0.02),
            "row {y}: the border lookup reads {read}, the bake {border} (gutter {})",
            irr[i + 10][0]
        );
    }
}

/// The tier is the same bake at a coarser texel rate. Dropping `source_hashes` made
/// `receiver_freshness` report `Unverified` for a mesh the bake already named.
#[test]
fn audfix_derived_tier_keeps_the_source_mesh_hash() {
    let mut src = two_charts(0.2, 0.8, false);
    src.source_hashes = vec![0x000A_11CE];
    let (tier, _) = derive_lightmap_tier(&src, 2).expect("derive");
    assert_eq!(tier.source_hash(0), Some(0x000A_11CE));
}

/// A project tier that ships a 4 × 4 block encoding and leaves `padding` unset pads by
/// [`BLOCK_TIER_CHART_PADDING`], as the default table's ULTRA_LOW does: the padding comes from the
/// encoding the tier resolves to on the platform, not from the rule's one-texel default, which
/// leaves a block holding two charts' borders (ADR-5621). An uncompressed tier without `padding`
/// keeps one texel, and a `padding` the project writes wins either way.
#[test]
fn a_project_block_tier_without_padding_pads_for_its_blocks() {
    let over: BTreeMap<String, LightmapTierRule> = toml::from_str(
        "[ultra_low]\ndensity = 2.0\nencoding = \"auto\"\ndirectional = false\n\
         [low]\ndensity = 4.0\n\
         [narrow]\ndensity = 2.0\nencoding = \"bc6h\"\npadding = 1\n",
    )
    .expect("a Regia.toml tier table");
    let table = LightmapTierTable::with_overrides(&over).expect("valid");
    let src = two_charts(10.0, 30.0, false);
    for (tier, platform, want) in [
        ("ultra_low", "linux", BLOCK_TIER_CHART_PADDING),
        ("ultra_low", "android", BLOCK_TIER_CHART_PADDING),
        ("low", "linux", DEFAULT_TIER_CHART_PADDING),
        ("narrow", "linux", 1),
    ] {
        let rule = table.rule(tier).expect("tier");
        let (bytes, report) = cook_lightmap_tier(&src, tier, rule, platform).expect("cooks");
        assert_eq!(
            report.derive.chart_padding, want,
            "{tier} on {platform} ({}): {:?}",
            report.encoding, report.derive
        );
        let map = Lightmap::from_bytes(&bytes.expect("ships")).expect("reads");
        assert_eq!(map.padding, want, "{tier} on {platform}: the file's padding");
        assert!(
            chart_gap(&map) > 2 * want,
            "{tier} on {platform}: charts {} texels apart",
            chart_gap(&map)
        );
    }
}

/// A tier's `padding` outside `1..=16` is refused where the other rule checks are: when the table
/// is built from the project's overrides, and by the cook before it looks at the rule's `none`, so
/// a tier that ships no lightmap cannot carry a bad padding either (ADR-5621).
#[test]
fn a_tier_padding_outside_its_range_is_refused_with_the_rule() {
    let src = two_charts(10.0, 30.0, false);
    for none in [false, true] {
        for bad in [0u32, 17] {
            let rule = LightmapTierRule {
                none,
                density: Some(4.0),
                padding: Some(bad),
                ..LightmapTierRule::default()
            };
            let mut over = BTreeMap::new();
            over.insert("low".to_string(), rule.clone());
            let e = LightmapTierTable::with_overrides(&over).expect_err("a bad padding");
            assert!(
                e.starts_with("lightmap_tier_rule:") && e.contains("padding"),
                "none {none}, padding {bad}: {e}"
            );
            let e = cook_lightmap_tier(&src, "low", &rule, "linux").expect_err("a bad padding");
            assert!(
                e.starts_with("lightmap_tier_rule:") && e.contains("padding"),
                "none {none}, padding {bad}: {e}"
            );
        }
        for good in [1u32, 16] {
            let rule = LightmapTierRule {
                none,
                density: Some(4.0),
                padding: Some(good),
                ..LightmapTierRule::default()
            };
            let mut over = BTreeMap::new();
            over.insert("low".to_string(), rule.clone());
            LightmapTierTable::with_overrides(&over).expect("in range");
            cook_lightmap_tier(&src, "low", &rule, "linux").expect("in range");
        }
    }
}

/// An imported UV2 whose distinct charts lie closer than the bake's padding (a hand-made unwrap
/// packed tighter than the baker packs) keeps them as separate charts in the repack: only pieces
/// whose boxes overlap or touch are one baked chart, so every imported chart moves on its own and
/// gets its own gutter at the tier, instead of keeping its sub-texel gap to its neighbour (the seam
/// the repack exists to fix) inside one merged group.
#[test]
fn a_repack_keeps_close_imported_charts_apart() {
    // Three charts with their own vertices, one bake texel apart (the bake's padding is two):
    // A and B side by side on the top band, C under both.
    let e = entry_of(
        "imported",
        [0, 0],
        32,
        16,
        &[
            [2.0, 2.0, 14.0, 7.0],
            [15.0, 2.0, 30.0, 7.0],
            [2.0, 8.0, 30.0, 14.0],
        ],
        (0..12).collect(),
    );
    let src = bake(32, 16, vec![e], |_, c, _, _| [10.0 + 10.0 * c as f32; 3], false);
    assert_eq!(src.padding, 2);
    for (factor, padding) in [(4, 1), (4, 2), (8, 1)] {
        let label = format!("factor {factor}, padding {padding}");
        let (tier, stats) = derive_lightmap_tier_padded(&src, factor, Some(padding)).expect("derives");
        assert_eq!(
            stats.repacked_charts, 3,
            "{label}: three imported charts: {stats:?}"
        );
        let u = &tier.unwraps[0];
        let [_, _, tw, th] = tier.entries[0].rect;
        let boxes: Vec<[f32; 4]> = (0..3)
            .map(|c| {
                let vs = &u.uv[4 * c..4 * c + 4];
                let (xs, ys) = (
                    vs.iter().map(|v| v[0] * tw as f32),
                    vs.iter().map(|v| v[1] * th as f32),
                );
                [
                    xs.clone().fold(f32::INFINITY, f32::min),
                    ys.clone().fold(f32::INFINITY, f32::min),
                    xs.fold(f32::NEG_INFINITY, f32::max),
                    ys.fold(f32::NEG_INFINITY, f32::max),
                ]
            })
            .collect();
        for a in 0..3 {
            for b in a + 1..3 {
                let (p, q) = (boxes[a], boxes[b]);
                let gap = (q[0] - p[2]).max(p[0] - q[2]).max((q[1] - p[3]).max(p[1] - q[3]));
                assert!(
                    gap >= 2.0 * padding as f32 - 1e-3,
                    "{label}: charts {a} and {b} are {gap} tier texels apart: {boxes:?}"
                );
            }
        }
        for (k, c) in tier_charts(&tier, 0) {
            let want = 10.0 + 10.0 * c as f32;
            let got = tier.irradiance_at(k as u32 % tier.width, k as u32 / tier.width)[0];
            assert!(close(got, want, 0.01), "{label}: chart {c} texel reads {got}");
        }
    }
}

/// The border fit counts the texels it holds on a steep border in `border_frozen`, with the texels
/// it freezes where two charts' bake light disagrees: a held texel is left as it was, and every
/// other chart's row that reads it is dropped, exactly as a frozen one (ADR-5621). One chart here,
/// so no texel is frozen for a disagreement, and every frozen texel is a held one.
#[test]
fn a_steep_border_counts_its_held_texels_as_frozen() {
    // The strip of `a_steep_border_still_fits_its_gutter_texel`: a dark column one texel inside
    // the right border makes every sample on that border steep.
    let e = entry_of(
        "strip",
        [0, 0],
        16,
        16,
        &[[2.0, 2.0, 10.4, 14.0]],
        vec![0, 1, 2, 3],
    );
    let field = |x: u32| if x == 8 { 30.0 } else { 100.0 };
    let mut src = bake(16, 16, vec![e], |_, _, x, _| [field(x); 3], false);
    for y in 0..src.height {
        for x in 0..src.width {
            let i = (y * src.width + x) as usize;
            if src.coverage[i] == Coverage::EMPTY {
                src.coverage[i] = Coverage::DILATED;
                src.texels[i] = regia_asset::baked_gi::rgb9e5([src.exposure * field(x); 3]);
            }
        }
    }
    src.blocks = None;
    let n = (src.width * src.height) as usize;
    let mut irr: Vec<[f32; 3]> = (0..n as u32)
        .map(|i| src.irradiance_at(i % src.width, i / src.width))
        .collect();
    let before = irr.clone();
    let cov = src.coverage.clone();
    let owner = vec![0u32; n];
    let (tri_chart, _) = triangle_charts(&src.unwraps[0].indices, src.unwraps[0].uv.len());
    let stats = super::border_fit::fit_borders(
        &src,
        &src.entries,
        &src.unwraps,
        &[[16, 16]],
        &[tri_chart],
        &mut super::border_fit::TierTexels {
            irr: &mut irr,
            cov: &cov,
            owner: &owner,
            width: 16,
            height: 16,
        },
    );
    assert!(stats.steep > 0, "the right border is steep: {stats:?}");
    // Column 9 is the last on-chart column (centre 9.5 < 10.4): the steep rows read it on every
    // texel row the chart covers (2..14), and hold it.
    let held_column: Vec<usize> = (0..16usize)
        .map(|y| y * 16 + 9)
        .filter(|&i| cov[i] == Coverage::BAKED)
        .collect();
    assert_eq!(held_column.len(), 12);
    for &i in &held_column {
        assert_eq!(irr[i], before[i], "held texel {i} moved");
    }
    let baked = cov.iter().filter(|&&c| c == Coverage::BAKED).count() as u64;
    assert!(
        stats.frozen >= held_column.len() as u64 && stats.frozen <= baked,
        "the held texels are counted: {stats:?}"
    );
}
