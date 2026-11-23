// SPDX-License-Identifier: MPL-2.0
//! Diagnostic target-layout transfer; no production tier contract or acceptance changes.
use super::*;

/// Nearest point on a UV triangle, expressed in its barycentric coordinates.
fn closest_barycentric(p: [f32; 2], tri: [[f32; 2]; 3]) -> ([f32; 3], f32) {
    let sub = |a: [f32; 2], b: [f32; 2]| [a[0] - b[0], a[1] - b[1]];
    let dot = |a: [f32; 2], b: [f32; 2]| a[0] * b[0] + a[1] * b[1];
    let cross = |a: [f32; 2], b: [f32; 2]| a[0] * b[1] - a[1] * b[0];
    let det = cross(sub(tri[1], tri[0]), sub(tri[2], tri[0]));
    if det.abs() > 1e-12 {
        let v = cross(sub(p, tri[0]), sub(tri[2], tri[0])) / det;
        let w = cross(sub(tri[1], tri[0]), sub(p, tri[0])) / det;
        let bary = [1.0 - v - w, v, w];
        if bary.iter().all(|b| *b >= 0.0) {
            return (bary, 0.0);
        }
    }
    let mut best = ([1.0, 0.0, 0.0], f32::INFINITY);
    for a in 0..3 {
        let b = (a + 1) % 3;
        let edge = sub(tri[b], tri[a]);
        let length = dot(edge, edge);
        let t = if length > 0.0 {
            (dot(sub(p, tri[a]), edge) / length).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let q = [tri[a][0] + t * edge[0], tri[a][1] + t * edge[1]];
        let distance = dot(sub(p, q), sub(p, q));
        if distance < best.1 {
            let mut bary = [0.0; 3];
            bary[a] = 1.0 - t;
            bary[b] = t;
            best = (bary, distance);
        }
    }
    best
}

fn source_lookup(
    source: &Lightmap,
    source_entry: usize,
    target: &regia_asset::lightmap::MeshUnwrap,
    p: [f32; 2],
    [w, h]: [u32; 2],
    bicubic: bool,
) -> ([f32; 3], [f32; 4]) {
    let src = &source.entries[source_entry];
    let su = &source.unwraps[source_entry];
    let mut nearest = (usize::MAX, [0.0; 3], f32::INFINITY);
    for (k, tri) in target.indices.as_chunks::<3>().0.iter().enumerate() {
        let triangle = tri.map(|v| {
            let uv = target.uv[v as usize];
            [uv[0] * w as f32, uv[1] * h as f32]
        });
        let (bary, distance) = closest_barycentric(p, triangle);
        if distance < nearest.2 {
            nearest = (k, bary, distance);
            if distance == 0.0 {
                break;
            }
        }
    }
    assert_ne!(nearest.0, usize::MAX, "nonempty target mesh");
    let tri = &su.indices[3 * nearest.0..3 * nearest.0 + 3];
    let local: [f32; 2] = std::array::from_fn(|c| {
        tri.iter()
            .zip(nearest.1)
            .map(|(v, b)| su.uv[*v as usize][c] * b)
            .sum()
    });
    let uv = [
        local[0] * src.scale_offset[0] + src.scale_offset[2],
        local[1] * src.scale_offset[1] + src.scale_offset[3],
    ];
    let value = if bicubic {
        source.sample_bicubic_page(src.page, uv)
    } else {
        source.sample_entry(source_entry, local)
    };
    (
        value,
        source.sample_direction_page(src.page, uv).unwrap_or([0.0; 4]),
    )
}

/// Original source light at a target-layout surface point, using matching source vertices.
pub(super) fn source_at(
    map: &Lightmap,
    source: &Lightmap,
    entry: usize,
    uv: [f32; 2],
    normal: [f32; 3],
) -> (f64, f64) {
    let source_entry = source
        .entry_for_key(map.entries[entry].key.expect("target key"))
        .expect("source key");
    let [_, _, w, h] = map.entries[entry].rect;
    let p = [uv[0] * w as f32, uv[1] * h as f32];
    let (bilinear, _) = source_lookup(source, source_entry, &map.unwraps[entry], p, [w, h], false);
    let (bicubic, direction) = source_lookup(source, source_entry, &map.unwraps[entry], p, [w, h], true);
    let frame = if source.is_directional() {
        regia_asset::lightmap::shade_directional(bicubic, direction, normal)
    } else {
        bicubic
    };
    (lum(bilinear), lum(frame))
}

/// Reuse only the target's layout and coverage; overwrite every irradiance/direction texel.
/// Each target center is mapped through the matching triangle to the HIGH bake. Gutter centers
/// use the closest triangle boundary. Bilinear/bicubic controls differ only in source filtering.
fn transfer(source: &Lightmap, layout: &Lightmap, bicubic: bool, samples_axis: u32) -> Lightmap {
    let mut map = layout.clone();
    map.texels.fill(0);
    map.blocks = None;
    map.direction_blocks = None;
    let mut directions = vec![[0.0; 4]; map.texels.len()];
    for (e, target) in map.entries.iter().enumerate() {
        let source_entry = source
            .entry_for_key(target.key.expect("target entry key"))
            .expect("source entry");
        let (tu, su) = (&map.unwraps[e], &source.unwraps[source_entry]);
        assert_eq!(tu.indices.len(), su.indices.len(), "triangle correspondence");
        for (target_tri, source_tri) in tu
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .zip(su.indices.as_chunks::<3>().0)
        {
            assert_eq!(
                target_tri.map(|v| tu.xref[v as usize]),
                source_tri.map(|v| su.xref[v as usize]),
                "source vertex correspondence"
            );
        }
        let [rx, ry, w, h] = target.rect;
        for y in 0..h {
            for x in 0..w {
                let at = ((target.page * map.height + ry + y) * map.width + rx + x) as usize;
                if map.coverage[at] == regia_asset::lightmap::Coverage::EMPTY {
                    continue;
                }
                let mut irradiance = [0.0; 3];
                let mut direction = [0.0; 4];
                let mut direction_weight = 0.0;
                for sy in 0..samples_axis {
                    for sx in 0..samples_axis {
                        let p = [
                            x as f32 + (sx as f32 + 0.5) / samples_axis as f32,
                            y as f32 + (sy as f32 + 0.5) / samples_axis as f32,
                        ];
                        let (value, d) = source_lookup(source, source_entry, tu, p, [w, h], bicubic);
                        let weight = lum(value) as f32;
                        for (sum, v) in irradiance.iter_mut().zip(value) {
                            *sum += v;
                        }
                        for (sum, v) in direction.iter_mut().zip(d) {
                            *sum += weight * v;
                        }
                        direction_weight += weight;
                    }
                }
                let scale = map.exposure / (samples_axis * samples_axis) as f32;
                map.texels[at] = regia_asset::baked_gi::rgb9e5(irradiance.map(|v| v * scale));
                directions[at] = if direction_weight > 0.0 {
                    direction.map(|v| v / direction_weight)
                } else {
                    [0.0; 4]
                };
            }
        }
    }
    if !map.directions.is_empty() {
        map.set_directions(&directions)
            .expect("transfer source directions");
    }
    map
}

#[test]
fn triangle_projection_preserves_interior_and_clamps_edges() {
    let tri = [[0.0, 0.0], [2.0, 0.0], [0.0, 2.0]];
    assert_eq!(closest_barycentric([0.5, 0.5], tri), ([0.5, 0.25, 0.25], 0.0));
    assert_eq!(closest_barycentric([1.0, -1.0], tri), ([0.5, 0.5, 0.0], 1.0));
    assert_eq!(closest_barycentric([3.0, 0.0], tri), ([0.0, 1.0, 0.0], 1.0));
}

#[test]
#[ignore = "manual target-layout diagnosis; no production UV policy or acceptance replacement"]
fn target_layout_transfer_study() {
    let tmp = project("lightmap_gallery_target_layout");
    let dir = &tmp.0;
    let meshes: Vec<CpuMesh> = gallery::parts().iter().map(|p| imported(dir, p)).collect();
    let samples = samples();
    let top = bake_with(dir, gallery::DEFAULT_OUT, &bake_args(TOP_DENSITY, "1", &samples));
    let mut maps = vec![("source".to_string(), top.clone())];
    for density in ["4", "2"] {
        let mut direct = bake_with(dir, "direct_layout.flmap", &bake_args(density, "1", &samples));
        if density == "2" {
            direct.directions.clear();
        }
        for (bicubic, samples_axis) in [(false, 1), (true, 1), (false, 4), (true, 4)] {
            let transferred = transfer(&top, &direct, bicubic, samples_axis);
            let stitched = restitch_control(&transferred, &meshes);
            for (name, map) in [("transferred", transferred), ("stitched", stitched)] {
                eprintln!(
                    "LAYOUT_CONTROL density={density} bicubic={bicubic} samples_axis={samples_axis} mode={name} seam={} dark={} error_direct={:?}",
                    seams_with_geometry(&map, &meshes),
                    dark_points(&map, &meshes).len(),
                    surface_error(&map, &direct, &meshes)
                );
                maps.push((
                    format!("density_{density}_bicubic_{bicubic}_samples_{samples_axis}_{name}"),
                    map,
                ));
            }
        }
        maps.push((format!("direct_{density}"), direct));
    }
    write_tier_evidence(
        "target_layout",
        &maps.iter().map(|(n, m)| (n.as_str(), m)).collect::<Vec<_>>(),
        &meshes,
        json!({"diagnostic_only":true, "target_lighting_overwritten":true, "changes_uv_contract":true}),
    );
}
