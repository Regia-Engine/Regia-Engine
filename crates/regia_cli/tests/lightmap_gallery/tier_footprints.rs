// SPDX-License-Identifier: MPL-2.0
//! Diagnose the original dark-point check without changing its lookup or neighborhood.
use super::*;
use std::cell::RefCell;

/// Geometry under an atlas texel, when its center lies inside an unwrapped triangle.
fn geometry_at(map: &Lightmap, meshes: &[CpuMesh], x: u32, y: u32) -> Option<Value> {
    for (part, mesh) in gallery::parts().iter().zip(meshes) {
        let e = map.entry_for_key(entry_key(part))?;
        let entry = &map.entries[e];
        let [rx, ry, w, h] = entry.rect;
        let ry = ry + entry.page * map.height;
        if x < rx || y < ry || x >= rx + w || y >= ry + h {
            continue;
        }
        let q = [
            (x as f32 + 0.5 - rx as f32) / w as f32,
            (y as f32 + 0.5 - ry as f32) / h as f32,
        ];
        let unwrap = &map.unwraps[e];
        for (k, tri) in unwrap.indices.as_chunks::<3>().0.iter().enumerate() {
            let [a, b, c] = tri.map(|v| unwrap.uv[v as usize]);
            let cross = |a: [f32; 2], b: [f32; 2]| a[0] * b[1] - a[1] * b[0];
            let sub = |a: [f32; 2], b: [f32; 2]| [a[0] - b[0], a[1] - b[1]];
            let det = cross(sub(b, a), sub(c, a));
            if det.abs() < 1e-12 {
                continue;
            }
            let v = cross(sub(q, a), sub(c, a)) / det;
            let w = cross(sub(b, a), sub(q, a)) / det;
            let bary = [1.0 - v - w, v, w];
            if bary.iter().any(|v| *v < -1e-5) {
                continue;
            }
            let t = &mesh.indices[3 * k..3 * k + 3];
            let mix = |data: &[[f32; 3]]| -> [f32; 3] {
                std::array::from_fn(|c| bary.iter().zip(t).map(|(b, i)| b * data[*i as usize][c]).sum())
            };
            return Some(json!({"part":part.name, "triangle":k, "local_uv":q,
                "world":mix(&mesh.positions), "normal":mix(&mesh.normals)}));
        }
        return Some(json!({"part":part.name, "local_uv":q, "triangle":null}));
    }
    None
}

/// First 16 failures in the original traversal, paired with the same source lookup and the
/// brightest atlas neighbor's geometry. Atlas proximity is deliberately not treated as world proximity.
pub(super) fn report(map: &Lightmap, source: &Lightmap, meshes: &[CpuMesh]) -> Vec<Value> {
    let rows = RefCell::new(Vec::new());
    let same_unwrap = map.unwraps == source.unwraps;
    let points = dark_points_using(map, meshes, |map, e, uv, n| {
        let color = frame_lookup(map, e, uv, n);
        if rows.borrow().len() >= 16 {
            return color;
        }
        let [rx, ry, w, h] = map.entries[e].rect;
        let ry = ry + map.entries[e].page * map.height;
        let (tx, ty) = (
            (uv[0] * w as f32) as i64 + i64::from(rx),
            (uv[1] * h as f32) as i64 + i64::from(ry),
        );
        let mut brightest = (0.0f64, 0u32, 0u32);
        for dy in -2..=2 {
            for dx in -2..=2 {
                let (x, y) = (tx + dx, ty + dy);
                if x < 0 || y < 0 {
                    continue;
                }
                let (x, y) = (x as u32, y as u32);
                if map.coverage_at(x, y) == regia_asset::lightmap::Coverage::BAKED {
                    let value = lum(map.irradiance_at(x, y));
                    if value > brightest.0 {
                        brightest = (value, x, y);
                    }
                }
            }
        }
        if brightest.0 > 0.0 && lum(color) < 0.05 * brightest.0 {
            // The runtime B-spline's 4x4 support, clamped to this page as in sampling.rs.
            // When all taps are BAKED, changing gutters alone cannot change this failure.
            let entry = &map.entries[e];
            let s = entry.scale_offset;
            let x0 = ((uv[0] * s[0] + s[2]) * map.width as f32 - 0.5).floor();
            let y0 = ((uv[1] * s[1] + s[3]) * map.height as f32 - 0.5).floor();
            let all_baked = (0..4).all(|dy| {
                (0..4).all(|dx| {
                    let x = (x0 - 1.0 + dx as f32).clamp(0.0, (map.width - 1) as f32) as u32;
                    let y = (y0 - 1.0 + dy as f32).clamp(0.0, (map.height - 1) as f32) as u32;
                    map.coverage_at(x, entry.page * map.height + y) == regia_asset::lightmap::Coverage::BAKED
                })
            });
            let (source_bilinear, source_frame) = if same_unwrap {
                (
                    lum(source.sample_entry(e, uv)),
                    lum(frame_lookup(source, e, uv, n)),
                )
            } else {
                target_layout::source_at(map, source, e, uv, n)
            };
            rows.borrow_mut().push(json!({"entry":e, "uv":uv, "normal":n,
                "source_frame_lux":source_frame,
                "source_bilinear_irradiance_lux":source_bilinear,
                "tier_frame_lux":lum(color), "neighbor_lux":brightest.0,
                "all_filter_texels_baked":all_baked,
                "neighbor_texel":[brightest.1,brightest.2],
                "neighbor_geometry":geometry_at(map,meshes,brightest.1,brightest.2)}));
        }
        color
    });
    let mut rows = rows.into_inner();
    for (row, (part, world, _, _)) in rows.iter_mut().zip(points) {
        row["part"] = json!(part);
        row["world"] = json!(world);
    }
    rows
}

/// The brightest BAKED texel within two texels of `uv` in entry `e`: the original check's neighbour.
fn brightest_baked(map: &Lightmap, e: usize, uv: [f32; 2]) -> f64 {
    let [rx, ry, w, h] = map.entries[e].rect;
    let ry = ry + map.entries[e].page * map.height;
    let (tx, ty) = (
        (uv[0] * w as f32) as i64 + i64::from(rx),
        (uv[1] * h as f32) as i64 + i64::from(ry),
    );
    let mut near = 0.0f64;
    for dy in -2..=2 {
        for dx in -2..=2 {
            let (x, y) = (tx + dx, ty + dy);
            if x >= 0
                && y >= 0
                && map.coverage_at(x as u32, y as u32) == regia_asset::lightmap::Coverage::BAKED
            {
                near = near.max(lum(map.irradiance_at(x as u32, y as u32)));
            }
        }
    }
    near
}

/// Every failure of the original dark-point check, classified against the bake it was derived from
/// at the same surface point (same unwrap only; another layout reports nothing). Diagnostic only:
/// the check, its window and its threshold are unchanged.
///
/// - `bake_reads_zero`: the bake's frame lookup reads no light there.
/// - `bake_also_below`: the bake's own lookup is under 5 % of the same tier neighbour too.
/// - `tier_darker_than_bake`: the tier reads under 95 % of the bake: light the derive lost.
pub(super) fn classify(map: &Lightmap, source: &Lightmap, meshes: &[CpuMesh]) -> Value {
    if map.unwraps != source.unwraps {
        return Value::Null;
    }
    let failing = RefCell::new(Vec::<(f64, f64, f64)>::new());
    let points = dark_points_using(map, meshes, |map, e, uv, n| {
        let color = frame_lookup(map, e, uv, n);
        let near = brightest_baked(map, e, uv);
        if near > 0.0 && lum(color) < 0.05 * near {
            failing
                .borrow_mut()
                .push((lum(color), lum(frame_lookup(source, e, uv, n)), near));
        }
        color
    });
    let failing = failing.into_inner();
    assert_eq!(
        failing.len(),
        points.len(),
        "the classifier repeats the original check"
    );
    let (mut zero, mut below, mut darker) = (0usize, 0usize, 0usize);
    let mut darker_examples = Vec::new();
    for ((part, world, _, _), &(got, bake, near)) in points.iter().zip(&failing) {
        if bake == 0.0 {
            zero += 1;
        }
        if bake < 0.05 * near {
            below += 1;
        }
        if got < 0.95 * bake {
            darker += 1;
            if darker_examples.len() < 16 {
                darker_examples
                    .push(json!({"part": part, "world": world, "tier": got, "bake": bake, "near": near}));
            }
        }
    }
    eprintln!(
        "DARK_CLASSES dark={} bake_reads_zero={zero} bake_also_below={below} tier_darker_than_bake={darker}",
        points.len()
    );
    json!({"dark_points": points.len(), "bake_reads_zero": zero, "bake_also_below": below,
        "tier_darker_than_bake": darker, "darker_examples": darker_examples})
}
