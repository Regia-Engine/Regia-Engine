// SPDX-License-Identifier: MPL-2.0
//! Alpha coverage of a masked leaf-card mesh against its cluster LOD level (ADR-3915, item 3).
//!
//! A canopy of bent, subdivided leaf cards with a leaf-shaped alpha mask is cooked as the cook
//! does it (meshopt cluster DAG with UV-aware simplification, `voxel_far_cook::voxelize`, and the
//! voxel levels on top), then every LOD cut of the DAG (`lod_error <= t < parent_error` at each
//! level's error) is rasterized on the CPU, orthographically along the three axes, with the masked
//! raster's alpha test (the base color's alpha at the hit's UV against the cutoff). The covered
//! share of the view must not collapse on coarse levels: the alpha-tested raster draws exactly
//! these cuts, so a level whose coverage fell would thin a forest with distance. Numbers are
//! printed (`--nocapture`).

use regia_asset_pipeline::gltf_extensions::TextureTransform;
use regia_asset_pipeline::voxel_far_cook::{
    VoxelCoverage, VoxelFarCookSettings, VoxelizeInput, voxelize, voxelize_with_coverage,
};
use regia_asset_pipeline::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::LabVertex;
use regia_render::visbuffer::{MeshletMesh, VoxelFarSettings};

/// The leaf mask (alpha), `TEX` × `TEX`: an ellipse with round holes, as `visbuffer_masked.rs`.
const TEX: usize = 64;
const CUTOFF: u8 = 128;
const GRID: usize = 96;

/// One triangle's corners: position and UV.
type Triangle = [([f32; 3], [f32; 2]); 3];

fn leaf_alpha() -> Vec<u8> {
    let mut alpha = Vec::with_capacity(TEX * TEX);
    for y in 0..TEX {
        for x in 0..TEX {
            let u = (x as f32 + 0.5) / TEX as f32 * 2.0 - 1.0;
            let v = (y as f32 + 0.5) / TEX as f32 * 2.0 - 1.0;
            let inside = u * u / 0.55 + v * v / 0.95 < 1.0;
            let hole =
                ((u * 4.0).fract().abs() - 0.5).powi(2) + ((v * 4.0).fract().abs() - 0.5).powi(2) < 0.05;
            alpha.push(if inside && !hole { 255 } else { 0 });
        }
    }
    alpha
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// `count` leaf cards of 3 × 3 quads in a unit ball (fixed LCG), each bent along its length,
/// facing out from the center, UV 0..1 across the card.
fn canopy(count: u32) -> (Vec<LabVertex>, Vec<u32>) {
    let mut state = 0x2468_ace1u32;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 8) as f32 / (1u32 << 24) as f32
    };
    let n = 3u32;
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for _ in 0..count {
        let (mut c, mut l2) = ([0.0f32; 3], 2.0f32);
        while l2 > 1.0 {
            c = [next() * 2.0 - 1.0, next() * 2.0 - 1.0, next() * 2.0 - 1.0];
            l2 = c[0] * c[0] + c[1] * c[1] + c[2] * c[2];
        }
        let len = l2.sqrt().max(1e-3);
        let normal = c.map(|v| v / len);
        let a = if normal[0].abs() < 0.9 {
            [1.0, 0.0, 0.0]
        } else {
            [0.0, 1.0, 0.0]
        };
        let t = cross(normal, a);
        let tl = (t[0] * t[0] + t[1] * t[1] + t[2] * t[2]).sqrt();
        let t = t.map(|v| v / tl * 0.12);
        let b = cross(normal, t);
        let base = vertices.len() as u32;
        for j in 0..=n {
            for i in 0..=n {
                let (u, v) = (i as f32 / n as f32, j as f32 / n as f32);
                let (su, sv) = (u * 2.0 - 1.0, v * 2.0 - 1.0);
                let bend = 0.04 * (1.0 - sv * sv);
                let p = std::array::from_fn(|k| c[k] * 0.85 + t[k] * su + b[k] * sv * 1.4 + normal[k] * bend);
                vertices.push(LabVertex::from_pos_normal_uv(p, normal, [u, 1.0 - v]));
            }
        }
        for j in 0..n {
            for i in 0..n {
                let a = base + j * (n + 1) + i;
                let b = a + n + 1;
                indices.extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
            }
        }
    }
    (vertices, indices)
}

/// Triangles (positions, uvs) of the cut `lod_error <= t < parent_error`, and the cut's coarsest
/// cluster level.
fn cut(mesh: &MeshletMesh, vertices: &[LabVertex], t: f32) -> (Vec<Triangle>, u32) {
    let mut out = Vec::new();
    let mut level = 0;
    for m in &mesh.meshlets {
        if m.lod_error > t || m.parent_error <= t {
            continue;
        }
        level = level.max(m.level);
        for k in 0..m.triangle_count {
            let packed = mesh.triangles[(m.triangle_offset + k) as usize];
            out.push(std::array::from_fn(|corner| {
                let local = (packed >> (corner * 8)) & 0xff;
                let v = &vertices[mesh.vertices[(m.vertex_offset + local) as usize] as usize];
                (v.position, v.uv)
            }));
        }
    }
    (out, level)
}

/// The share of a `GRID` × `GRID` orthographic view along `axis` over [-1.2, 1.2]² that the
/// triangles cover where the alpha test passes (pixel centers, nearest texel).
fn coverage(triangles: &[Triangle], alpha: &[u8], axis: usize) -> f64 {
    let (ax, ay) = ((axis + 1) % 3, (axis + 2) % 3);
    let mut covered = vec![false; GRID * GRID];
    let to_px = |v: f32| (v + 1.2) / 2.4 * GRID as f32;
    for tri in triangles {
        let p = tri.map(|(pos, _)| [to_px(pos[ax]), to_px(pos[ay])]);
        let area = (p[1][0] - p[0][0]) * (p[2][1] - p[0][1]) - (p[2][0] - p[0][0]) * (p[1][1] - p[0][1]);
        if area.abs() < 1e-9 {
            continue;
        }
        let lo = |k: usize| p.iter().map(|q| q[k]).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
        let hi = |k: usize| (p.iter().map(|q| q[k]).fold(f32::MIN, f32::max).ceil() as usize).min(GRID);
        for y in lo(1)..hi(1) {
            for x in lo(0)..hi(0) {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let w = |a: [f32; 2], b: [f32; 2]| {
                    ((b[0] - a[0]) * (py - a[1]) - (px - a[0]) * (b[1] - a[1])) / area
                };
                let (w0, w1, w2) = (w(p[1], p[2]), w(p[2], p[0]), w(p[0], p[1]));
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let uv: [f32; 2] =
                    std::array::from_fn(|k| tri[0].1[k] * w0 + tri[1].1[k] * w1 + tri[2].1[k] * w2);
                let texel = |c: f32| ((c.rem_euclid(1.0) * TEX as f32) as usize).min(TEX - 1);
                if alpha[texel(uv[1]) * TEX + texel(uv[0])] >= CUTOFF {
                    covered[y * GRID + x] = true;
                }
            }
        }
    }
    covered.iter().filter(|&&c| c).count() as f64 / (GRID * GRID) as f64
}

fn mean_coverage(triangles: &[Triangle], alpha: &[u8]) -> f64 {
    (0..3).map(|axis| coverage(triangles, alpha, axis)).sum::<f64>() / 3.0
}

/// The cuts' thresholds: 0 (level 0), then each level's largest own error, ascending.
fn thresholds(mesh: &MeshletMesh) -> Vec<f32> {
    let levels = mesh.meshlets.iter().map(|m| m.level).max().unwrap_or(0);
    let mut out = vec![0.0];
    for level in 1..=levels {
        let t = mesh
            .meshlets
            .iter()
            .filter(|m| m.level == level)
            .map(|m| m.lod_error)
            .fold(0.0f32, f32::max);
        if t > *out.last().unwrap_or(&0.0) {
            out.push(t);
        }
    }
    out
}

/// Every DAG cut of the leaf canopy keeps at least 90 % of level 0's alpha coverage, and no voxel
/// level on top (alpha-tested at each voxel's representative UV) falls under half of it.
#[test]
fn masked_leaf_coverage_holds_on_every_cluster_level() {
    let (vertices, indices) = canopy(160);
    let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
    let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
    let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
    let colors: Vec<[f32; 4]> = vertices.iter().map(|v| v.color).collect();
    let dag = build_meshlet_mesh_with_attributes(
        &positions,
        Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        }),
        &indices,
        &MeshletSettings::default(),
    )
    .expect("cluster DAG");
    let alpha = leaf_alpha();
    let levels = dag.meshlets.iter().map(|m| m.level).max().unwrap_or(0);
    let (base, _) = cut(&dag, &vertices, 0.0);
    let reference = mean_coverage(&base, &alpha);
    println!(
        "leaf canopy: {} triangles, {} clusters, {} DAG levels above 0, level-0 coverage {:.4}",
        indices.len() / 3,
        dag.meshlets.len(),
        levels,
        reference
    );
    assert!(reference > 0.05, "the canopy covers the view");
    let mut worst = 1.0f64;
    for t in thresholds(&dag) {
        let (triangles, level) = cut(&dag, &vertices, t);
        let share = mean_coverage(&triangles, &alpha) / reference;
        worst = worst.min(share);
        println!(
            "  DAG cut t {t:.5}: up to level {level}, {} triangles, coverage {:.1} % of level 0",
            triangles.len(),
            share * 100.0
        );
    }
    assert!(
        worst >= 0.9,
        "a coarse DAG level loses alpha coverage: {:.1} % of level 0",
        worst * 100.0
    );

    // The voxel levels of the same canopy, alpha-tested where each voxel samples its texture.
    let proxy = voxelize(
        "canopy",
        &VoxelizeInput {
            positions: &positions,
            normals: &normals,
            uvs: &uvs,
            colors: &colors,
            indices: &indices,
        },
        &VoxelFarCookSettings { resolution: 32 },
    )
    .expect("voxelize");
    let far = proxy
        .extend_meshlets(&dag, &vertices, &VoxelFarSettings::default())
        .expect("extend");
    for level in &far.levels {
        let (triangles, _) = cut(&far.meshlets, &far.vertices, level.error);
        let tested = mean_coverage(&triangles, &alpha) / reference;
        let opaque = mean_coverage(&triangles, &[255u8; TEX * TEX]) / reference;
        println!(
            "  voxel level {} (error {:.4}): {} triangles, alpha-tested coverage {:.1} % of level 0, \
             untested {:.1} %",
            level.dag_level,
            level.error,
            triangles.len(),
            tested * 100.0,
            opaque * 100.0
        );
        // Measured 11-05: 69 %, 96 % and 168 % on the three levels (untested 187 % to 201 %). Each
        // voxel tests one texel, so a level's coverage scatters around level 0's, but no level
        // collapses.
        assert!(
            tested >= 0.5,
            "voxel level {} collapses under the alpha test: {:.1} % of level 0",
            level.dag_level,
            tested * 100.0
        );
    }
    assert!(!far.levels.is_empty(), "the canopy has voxel levels");
}

/// The far canopy keeps its coverage (ADR-3227): the same 160-card canopy cooked with the
/// alpha-tested voxelization (ADR-3108), its voxel levels stacked on the DAG and cut where each
/// level takes over, rasterized with the masked raster's test at each voxel's representative UV.
/// Every level the cut draws stays within `TOLERANCE` of the cluster path's level-0 coverage
/// (the same raster), where the cook by triangle area measured 69 % to 168 % above. The cook's
/// own estimate ([`regia_render::visbuffer::VoxelFarProxy::coverage`]) is printed beside it.
#[test]
fn alpha_tested_voxel_levels_keep_the_cluster_coverage() {
    const TOLERANCE: f64 = 0.25;
    let (vertices, indices) = canopy(160);
    let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
    let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
    let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
    let colors: Vec<[f32; 4]> = vertices.iter().map(|v| v.color).collect();
    let dag = build_meshlet_mesh_with_attributes(
        &positions,
        Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        }),
        &indices,
        &MeshletSettings::default(),
    )
    .expect("cluster DAG");
    let alpha = leaf_alpha();
    let (base, _) = cut(&dag, &vertices, 0.0);
    let reference = mean_coverage(&base, &alpha);
    let mask = VoxelCoverage {
        width: TEX as u32,
        height: TEX as u32,
        alpha: &alpha,
        factor: 1.0,
        cutoff: f32::from(CUTOFF) / 255.0,
        vertex_alpha: true,
        uv_transform: TextureTransform::IDENTITY,
    };
    let input = VoxelizeInput {
        positions: &positions,
        normals: &normals,
        uvs: &uvs,
        colors: &colors,
        indices: &indices,
    };
    let settings = VoxelFarCookSettings { resolution: 32 };
    let mut shares = Vec::new();
    for (name, proxy) in [
        (
            "alpha-tested",
            voxelize_with_coverage("canopy", &input, Some(&mask), &settings).expect("alpha-tested"),
        ),
        ("by area", voxelize("canopy", &input, &settings).expect("by area")),
    ] {
        let far = proxy
            .extend_meshlets(&dag, &vertices, &VoxelFarSettings::default())
            .expect("extend");
        assert!(!far.levels.is_empty(), "the canopy has voxel levels");
        let mut worst = 0.0f64;
        for level in &far.levels {
            let (triangles, _) = cut(&far.meshlets, &far.vertices, level.error);
            let share = mean_coverage(&triangles, &alpha) / reference;
            worst = worst.max((share - 1.0).abs());
            println!(
                "  {name}: voxel level {} (error {:.4}): {} triangles, coverage {:.1} % of the clusters' \
                 (cook estimate {:?})",
                level.dag_level,
                level.error,
                triangles.len(),
                share * 100.0,
                proxy.coverage.get(level.level)
            );
        }
        shares.push(worst);
    }
    assert!(
        shares[0] <= TOLERANCE,
        "an alpha-tested voxel level is {:.1} % off the clusters' coverage",
        shares[0] * 100.0
    );
    assert!(
        shares[1] > TOLERANCE,
        "the cook by area must miss the bound (it measured {:.1} % off)",
        shares[1] * 100.0
    );
}
