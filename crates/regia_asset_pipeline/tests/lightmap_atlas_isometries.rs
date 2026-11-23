// SPDX-License-Identifier: MPL-2.0
//! Metamorphic density checks for both lightmap UV builders.
//!
//! These fixtures deliberately use integer coordinates so quarter-turns, reflections and the
//! tested uniform scales are exactly representable in `f32`.

use regia_asset_pipeline::lightmap_uv::{
    LightmapUvSettings, UnwrapInput, build_atlas_layout, build_lightmap_layout,
};

type Geometry = (Vec<[f32; 3]>, Vec<u32>);
type Builder = for<'a> fn(
    &[UnwrapInput<'_>],
    &LightmapUvSettings,
) -> Result<
    (
        regia_asset::lightmap::LightmapLayout,
        regia_asset_pipeline::lightmap_uv::LightmapUvReport,
    ),
    regia_asset_pipeline::lightmap_uv::LightmapUvError,
>;

fn asymmetric_box() -> Geometry {
    let mut positions = Vec::new();
    let mut indices = Vec::new();
    let faces = [
        ([1, 0, 0], [0, 1, 0], [0, 0, 1]),
        ([-1, 0, 0], [0, 0, 1], [0, 1, 0]),
        ([0, 1, 0], [0, 0, 1], [1, 0, 0]),
        ([0, -1, 0], [1, 0, 0], [0, 0, 1]),
        ([0, 0, 1], [1, 0, 0], [0, 1, 0]),
        ([0, 0, -1], [0, 1, 0], [1, 0, 0]),
    ];
    for (normal, a, b) in faces {
        let base = positions.len() as u32;
        for (sa, sb) in [(-1, -1), (1, -1), (1, 1), (-1, 1)] {
            positions.push([
                (3 * (normal[0] + sa * a[0] + sb * b[0])) as f32,
                (2 * (normal[1] + sa * a[1] + sb * b[1])) as f32,
                (normal[2] + sa * a[2] + sb * b[2]) as f32,
            ]);
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (positions, indices)
}

fn welded_plane() -> Geometry {
    (
        vec![
            [-2.0, 0.0, -1.0],
            [2.0, 0.0, -1.0],
            [2.0, 0.0, 1.0],
            [-2.0, 0.0, 1.0],
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

fn transformed(g: &Geometry, map: impl Fn([f32; 3]) -> [f32; 3], reverse: bool) -> Geometry {
    let positions = g.0.iter().copied().map(map).collect();
    let mut indices = g.1.clone();
    if reverse {
        for tri in indices.as_chunks_mut::<3>().0 {
            tri.swap(1, 2);
        }
    }
    (positions, indices)
}

fn area(g: &Geometry) -> f64 {
    g.1.as_chunks::<3>()
        .0
        .iter()
        .map(|tri| {
            let p = |i: usize| regia_core::glam::DVec3::from_array(g.0[tri[i] as usize].map(f64::from));
            0.5 * (p(1) - p(0)).cross(p(2) - p(0)).length()
        })
        .sum()
}

fn unwrap_area_texels(layout: &regia_asset::lightmap::LightmapLayout, entry: usize) -> f64 {
    let mesh = &layout.unwraps[entry];
    let [_, _, w, h] = layout.entries[entry].rect;
    mesh.indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|tri| {
            let uv = |i: usize| {
                let p = mesh.uv[tri[i] as usize];
                [f64::from(p[0]) * f64::from(w), f64::from(p[1]) * f64::from(h)]
            };
            let (a, b, c) = (uv(0), uv(1), uv(2));
            ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])).abs() * 0.5
        })
        .sum()
}

fn assert_layout(builder: Builder, g: &Geometry, density_scale: f32, expected_density: f64) -> (f64, f64) {
    let input = UnwrapInput {
        name: "fixture",
        positions: &g.0,
        indices: &g.1,
        density_scale,
    };
    let settings = LightmapUvSettings {
        texels_per_metre: 8.0,
        max_atlas: 1024,
        padding: 2,
        ..LightmapUvSettings::default()
    };
    let (layout, report) = builder(&[input], &settings).expect("layout");
    assert_eq!(report.fit_scale, 1.0, "fixture must not atlas-fit");
    assert_eq!(report.degenerate_triangles, 0);
    let entry = &layout.entries[0];
    assert_eq!(entry.texels_per_metre, 8.0 * density_scale);
    let unwrap = &layout.unwraps[0];
    assert_eq!(unwrap.indices.len(), g.1.len());
    assert!(unwrap.xref.iter().all(|&i| (i as usize) < g.0.len()));
    for (out, source) in unwrap
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .zip(g.1.as_chunks::<3>().0)
    {
        let mapped = [out[0], out[1], out[2]].map(|i| unwrap.xref[i as usize]);
        assert_eq!(
            mapped.as_slice(),
            source.as_slice(),
            "xref preserves triangle mapping"
        );
    }
    assert!(
        unwrap
            .uv
            .iter()
            .flatten()
            .all(|uv| uv.is_finite() && (0.0..=1.0).contains(uv))
    );
    // Both APIs place chart outlines at texel centres. Allow half a texel for f32 UV conversion.
    let pad = layout.padding as f32;
    let [_, _, w, h] = entry.rect;
    for uv in &unwrap.uv {
        assert!(uv[0] * w as f32 >= pad - 0.51 && uv[0] * w as f32 <= w as f32 - pad + 0.51);
        assert!(uv[1] * h as f32 >= pad - 0.51 && uv[1] * h as f32 <= h as f32 - pad + 0.51);
    }
    let mut parent: Vec<usize> = (0..unwrap.xref.len()).collect();
    fn find(parent: &mut [usize], index: usize) -> usize {
        let previous = parent[index];
        if previous != index {
            let root = find(parent, previous);
            parent[index] = root;
        }
        parent[index]
    }
    for tri in unwrap.indices.as_chunks::<3>().0 {
        let first = find(&mut parent, tri[0] as usize);
        for &vertex in &tri[1..] {
            let other = find(&mut parent, vertex as usize);
            parent[other] = first;
        }
    }
    let mut chart_orientation = std::collections::HashMap::new();
    for tri in unwrap.indices.as_chunks::<3>().0 {
        let p = |i: u32| unwrap.uv[i as usize];
        let (a, b, c) = (p(tri[0]), p(tri[1]), p(tri[2]));
        let signed = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
        assert!(
            signed.is_finite() && signed.abs() > 1e-12,
            "folded/collapsed chart triangle"
        );
        let root = find(&mut parent, tri[0] as usize);
        let orientation = signed.is_sign_positive();
        if let Some(previous) = chart_orientation.insert(root, orientation) {
            assert_eq!(previous, orientation, "chart has a foldover");
        }
    }
    let observed = unwrap_area_texels(&layout, 0);
    let expected = area(g) * expected_density * expected_density;
    // A half-pixel coordinate displacement at each of a triangle's vertices changes its area by
    // at most sqrt(2)/2 times the texel-space perimeter, plus one pixel² for the displacement
    // cross term. Sum that per-triangle bound for chart-border pixel rounding.
    let rounding: f64 =
        g.1.as_chunks::<3>()
            .0
            .iter()
            .map(|tri| {
                let p = |k: usize| regia_core::glam::DVec3::from_array(g.0[tri[k] as usize].map(f64::from));
                let perimeter = (p(1) - p(0)).length() + (p(2) - p(0)).length();
                std::f64::consts::FRAC_1_SQRT_2 * perimeter * expected_density + 1.0
            })
            .sum();
    assert!(
        (observed - expected).abs() <= rounding + expected * 1e-6,
        "interior area {observed:.3} texels² != expected {expected:.3} (rounding allowance {rounding:.3})"
    );
    (observed, rounding)
}

#[test]
fn both_builders_preserve_density_under_rigid_and_uniform_transforms() {
    let builders: [Builder; 2] = [build_lightmap_layout, |i, s| build_atlas_layout(i, s, 1)];
    let box_mesh = asymmetric_box();
    let plane = welded_plane();
    let cases = [
        transformed(&box_mesh, |[x, y, z]| [x + 16.0, y - 32.0, z + 64.0], false),
        transformed(&box_mesh, |[x, y, z]| [z, x, y], false),
        transformed(&box_mesh, |[x, y, z]| [-x, y, z], true),
        transformed(&box_mesh, |[x, y, z]| [2.0 * x, 2.0 * y, 2.0 * z], false),
        transformed(&box_mesh, |[x, y, z]| [0.5 * x, 0.5 * y, 0.5 * z], false),
        transformed(&plane, |[x, y, z]| [x + 32.0, y + 8.0, z - 16.0], false),
        transformed(&plane, |[x, y, z]| [z, x, y], false),
        transformed(&plane, |[x, y, z]| [-x, y, z], true),
        transformed(&plane, |[x, y, z]| [2.0 * x, 2.0 * y, 2.0 * z], false),
        transformed(&plane, |[x, y, z]| [0.5 * x, 0.5 * y, 0.5 * z], false),
    ];
    for builder in builders {
        for (g, label) in cases.iter().zip([
            "box translation",
            "box rotation",
            "box mirror",
            "box doubled",
            "box halved",
            "plane translation",
            "plane rotation",
            "plane mirror",
            "plane doubled",
            "plane halved",
        ]) {
            let density = 8.0;
            let (got, _) = assert_layout(builder, g, 1.0, density);
            assert!(got > 0.0, "{label}");
        }
        // Doubling the world geometry while halving its requested density preserves footprint.
        let doubled = transformed(&box_mesh, |[x, y, z]| [2.0 * x, 2.0 * y, 2.0 * z], false);
        let (base_area, base_rounding) = assert_layout(builder, &box_mesh, 1.0, 8.0);
        let (reduced_density_area, scaled_rounding) = assert_layout(builder, &doubled, 0.5, 4.0);
        assert!((base_area - reduced_density_area).abs() <= base_rounding + scaled_rounding);
    }
}

#[test]
fn anisotropic_scale_uses_transformed_triangle_areas() {
    let builders: [Builder; 2] = [build_lightmap_layout, |i, s| build_atlas_layout(i, s, 1)];
    let box_mesh = asymmetric_box();
    let stretched = transformed(&box_mesh, |[x, y, z]| [2.0 * x, y, z], false);
    assert!(area(&stretched) > area(&box_mesh) * 1.5);
    for builder in builders {
        let (observed, rounding) = assert_layout(builder, &stretched, 1.0, 8.0);
        assert!((observed - area(&stretched) * 64.0).abs() <= rounding);
    }
}
