// SPDX-License-Identifier: MPL-2.0
//! Edge cases of the texture and heightfield half of `regia_gen` (ADR-7280): extreme sample
//! coordinates, degenerate inputs, open meshes, UVs outside the unit square, and erosion of a
//! field that oscillates.

use regia_gen::{
    CpuMesh, CurvatureOptions, Erosion, Fractal, Heightfield, NoiseKind, NoiseParams, NoiseSampler,
    curvature_masks, noise_image,
};

/// Any integer pixel, however far outside the image, samples to a finite value in [0, 1]
/// without overflow, tiled or not; lacunarity 1 repeats the same cells in every octave.
#[test]
fn extreme_coordinates_and_lacunarity_one_stay_in_range() {
    for tile in [false, true] {
        for kind in [
            NoiseKind::Value,
            NoiseKind::Gradient,
            NoiseKind::Cellular,
            NoiseKind::CellularEdge,
        ] {
            let p = NoiseParams {
                kind,
                seed: u64::MAX,
                cells: 7,
                octaves: 3,
                fractal: Fractal::Ridged,
                lacunarity: 1,
                gain: 1.0,
                tile,
            };
            let s = NoiseSampler::new(&p, 3, 5).expect("sampler");
            for (x, y) in [(i64::MAX, i64::MIN), (i64::MIN, 0), (0, i64::MAX), (-1, -1)] {
                let v = s.sample(x, y);
                assert!(
                    v.is_finite() && (0.0..=1.0).contains(&v),
                    "{kind:?} ({x}, {y}): {v}"
                );
            }
        }
    }
    // One pixel images work and one cell spans the image.
    let one = noise_image(&NoiseParams::new(NoiseKind::Gradient), 1, 1).expect("1x1");
    assert_eq!(one.pixels.len(), 1);
}

/// A flat field exports with a zero height scale (every sample 0) and still meshes through the
/// engine's `mesh_from_heightfield`.
#[test]
fn a_flat_field_exports_to_terrain_with_zero_scale() {
    let mut hf = Heightfield::flat(17, 17, 0.5).expect("flat");
    hf.normalize(3.0, 9.0).expect("normalize a flat field");
    assert!(hf.heights.iter().all(|&h| h == 3.0));
    let t = hf.to_terrain().expect("terrain");
    assert_eq!((t.height_scale, t.height_offset, t.world_width), (0.0, 3.0, 8.0));
    assert!(t.heightfield.samples.iter().all(|&s| s == 0));
    let mesh =
        regia_asset::mesh_from_heightfield(&t.heightfield, t.world_width, t.height_scale, t.height_offset)
            .expect("engine mesh");
    assert!(mesh.positions.iter().all(|p| p[1] == 3.0));
    hf.terrace(4, 1.0).expect("terrace of a flat field is a no-op");
    assert!(hf.heights.iter().all(|&h| h == 3.0));
}

/// Erosion of a checkerboard (every sample steeper than the talus in every direction) still
/// conserves the total and ends with a smaller spread; bad parameters are errors.
#[test]
fn checkerboard_erosion_conserves_and_flattens() {
    let mut hf =
        Heightfield::from_fn(32, 32, 1.0, |x, y| if (x + y) % 2 == 0 { 10.0 } else { 0.0 }).expect("board");
    let before: f64 = hf.heights.iter().map(|&h| f64::from(h)).sum();
    hf.thermal_erode(&Erosion {
        iterations: 40,
        talus: 0.1,
        rate: 0.25,
    })
    .expect("erode");
    let after: f64 = hf.heights.iter().map(|&h| f64::from(h)).sum();
    assert!(((after - before) / before).abs() <= 1e-4, "{before} -> {after}");
    let s = hf.stats();
    assert!(s.max - s.min < 5.0, "{s:?}");
    for bad in [
        Erosion {
            iterations: 1,
            talus: -0.1,
            rate: 0.25,
        },
        Erosion {
            iterations: 1,
            talus: 0.5,
            rate: 0.0,
        },
        Erosion {
            iterations: 1,
            talus: f32::NAN,
            rate: 0.25,
        },
    ] {
        assert_eq!(
            hf.thermal_erode(&bad).err().map(|e| e.code()),
            Some("gen_invalid_input")
        );
    }
}

/// An open mesh (a folded sheet of two quads) has one interior edge: a 90° convex fold marks it,
/// the sheet's boundary edges are not features, and texels whose UVs fall outside [0, 1]² are
/// dropped rather than wrapped.
#[test]
fn an_open_folded_sheet_marks_only_its_fold_and_drops_outside_uvs() {
    // Two unit quads meeting at x = 0 along the Z axis: one in the XZ plane facing +Y (x in
    // [-1, 0]), one in the YZ plane facing +X (y in [-1, 0]) — a convex 90° fold.
    let positions = vec![
        [-1.0, 0.0, 0.0],
        [0.0, 0.0, 0.0],
        [0.0, 0.0, -1.0],
        [-1.0, 0.0, -1.0],
        [0.0, 0.0, 0.0],
        [0.0, -1.0, 0.0],
        [0.0, -1.0, -1.0],
        [0.0, 0.0, -1.0],
    ];
    let uvs = vec![
        [0.0, 0.0],
        [0.5, 0.0],
        [0.5, 1.0],
        [0.0, 1.0],
        [0.5, 0.0],
        [1.0, 0.0],
        [1.0, 1.0],
        [0.5, 1.0],
    ];
    let mesh = CpuMesh {
        normals: vec![[0.0, 1.0, 0.0]; 8],
        positions,
        uvs,
        indices: vec![0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7],
        ..Default::default()
    };
    let opts = CurvatureOptions {
        width: 0.1,
        dilate: 0,
        ..Default::default()
    };
    let m = curvature_masks(&mesh, 64, 16, &opts).expect("masks");
    let at = |x: u32, y: u32| m.edge.pixels[(y * 64 + x) as usize];
    // Next to the fold (u = 0.5) on both sides: marked. Far from it: zero, boundary included.
    assert!(at(31, 8) > 0.8 && at(32, 8) > 0.8, "{} {}", at(31, 8), at(32, 8));
    assert_eq!(at(0, 8), 0.0);
    assert_eq!(at(63, 8), 0.0);
    assert_eq!(at(16, 0), 0.0);
    assert_eq!(m.cavity.stats().max, 0.0);
    // Shift the UVs half an image to the right: the right quad leaves [0, 1]² and is dropped.
    let mut shifted = mesh.clone();
    for uv in &mut shifted.uvs {
        uv[0] += 0.5;
    }
    let s = curvature_masks(&shifted, 64, 16, &opts).expect("shifted");
    let covered = s.coverage.pixels.iter().filter(|&&c| c > 0.0).count();
    assert_eq!(covered, 32 * 16, "only the left quad's texels");
}

/// The agent limits bound a call's cost before anything is built: a recipe of several full-size
/// many-octave cellular layers, or a wide blur chain, is `gen_too_large` by its estimated work
/// even though every single value is within its own limit; a typical recipe fits.
#[test]
fn work_budgets_refuse_expensive_recipes_up_front() {
    use regia_gen::{
        HeightfieldLimits, HeightfieldRecipe, TextureLimits, TextureRecipe, build_heightfield, build_texture,
        heightfield_work, texture_work,
    };
    let cellular = serde_json::json!({ "noise": { "kind": "cellular", "octaves": 16, "tile": true } });
    let layer = serde_json::json!({ "blend": { "mode": "max", "layer": { "source": cellular } } });
    let heavy: TextureRecipe = serde_json::from_value(serde_json::json!({
        "width": 1024, "height": 1024, "source": cellular, "ops": [layer, layer, layer]
    }))
    .expect("parse");
    let limits = TextureLimits::default();
    assert!(texture_work(&heavy) > limits.work);
    assert_eq!(
        build_texture(&heavy, &limits).err().map(|e| e.code()),
        Some("gen_too_large")
    );
    let blur = serde_json::json!({ "blur": { "radius": 32, "passes": 4 } });
    let blurs: TextureRecipe = serde_json::from_value(serde_json::json!({
        "width": 1024, "height": 1024, "source": { "constant": { "value": 0.5 } },
        "ops": [blur, blur, blur, blur]
    }))
    .expect("parse");
    assert_eq!(
        build_texture(&blurs, &limits).err().map(|e| e.code()),
        Some("gen_too_large")
    );
    let typical: TextureRecipe = serde_json::from_value(serde_json::json!({
        "width": 1024, "height": 1024,
        "source": { "noise": { "kind": "gradient", "octaves": 6, "tile": true } },
        "ops": [ { "levels": {} }, blur ]
    }))
    .expect("parse");
    assert!(
        texture_work(&typical) <= limits.work,
        "{}",
        texture_work(&typical)
    );

    let terrain: HeightfieldRecipe = serde_json::from_value(serde_json::json!({
        "width": 1025, "height": 1025, "ops": [
            { "noise": { "noise": { "kind": "cellular", "octaves": 16 }, "amplitude": 50 } },
            { "erode": { "iterations": 10, "talus": 0.5 } }
        ]
    }))
    .expect("parse");
    assert!(heightfield_work(&terrain) > HeightfieldLimits::default().work);
    assert_eq!(
        build_heightfield(&terrain, &HeightfieldLimits::default())
            .err()
            .map(|e| e.code()),
        Some("gen_too_large")
    );
}

/// The work estimate saturates instead of overflowing on adversarial blur parameters: a recipe
/// with `u32::MAX` blur radius and passes is refused as `gen_too_large` before anything is
/// built, not a debug-mode panic (review finding 1 of rev-claude-45-k; radius/passes are raw
/// serde u32s checked only after `texture_work` runs).
#[test]
fn blur_work_saturates_on_u32_max_radius_and_passes() {
    use regia_gen::{TextureLimits, TextureRecipe, build_texture, texture_work};
    let recipe: TextureRecipe = serde_json::from_value(serde_json::json!({
        "width": 64, "height": 64, "source": { "constant": { "value": 0.5 } },
        "ops": [ { "blur": { "radius": u32::MAX, "passes": u32::MAX } } ]
    }))
    .expect("parse");
    let limits = TextureLimits::default();
    assert!(
        texture_work(&recipe) >= u64::MAX / 32,
        "the estimate must saturate, not wrap: {}",
        texture_work(&recipe)
    );
    assert_eq!(
        build_texture(&recipe, &limits).err().map(|e| e.code()),
        Some("gen_too_large")
    );
}

/// CW-wound UV triangles rasterize like CCW ones: the barycentric test divides the signed
/// edge functions by the signed area, so both windings accept exactly the interior texels and
/// a mirrored UV island (CW winding, as in symmetric unwraps) still bakes full coverage
/// (review finding 2 of rev-claude-45-k: the claimed zero-coverage failure does not
/// reproduce; the test pins the behaviour and the module docs state it).
#[test]
fn cw_wound_uv_triangles_bake_full_coverage() {
    // A flat quad (no feature edges) covering [0, 1]², as two triangles with per-triangle
    // vertices so the mirrored variant only permutes UVs within one triangle.
    let quad = |mirror: bool| {
        let mut m = CpuMesh {
            normals: vec![[0.0; 3]; 6],
            positions: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 1.0],
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 1.0],
                [0.0, 0.0, 1.0],
            ],
            uvs: vec![
                [0.0, 0.0],
                [1.0, 0.0],
                [1.0, 1.0],
                [0.0, 0.0],
                [1.0, 1.0],
                [0.0, 1.0],
            ],
            indices: (0..6).collect(),
            ..Default::default()
        };
        if mirror {
            // Swap the UVs of corners 1 and 2 of every triangle: flips the UV winding only.
            for t in m.indices.as_chunks::<3>().0 {
                m.uvs.swap(t[1] as usize, t[2] as usize);
            }
        }
        m
    };
    let opts = CurvatureOptions {
        dilate: 0,
        ..Default::default()
    };
    for (name, mesh) in [("ccw", quad(false)), ("cw", quad(true))] {
        let masks = curvature_masks(&mesh, 32, 32, &opts).expect(name);
        let covered = masks.coverage.pixels.iter().filter(|&&c| c > 0.0).count();
        assert_eq!(covered, 32 * 32, "{name}: winding must not lose coverage");
    }
}

/// A mesh whose index count is not a multiple of 3 is refused as `gen_invalid_input` instead
/// of silently dropping the trailing partial triangle (review finding 3 of rev-claude-45-k).
#[test]
fn a_trailing_partial_triangle_is_an_error() {
    let mesh = CpuMesh {
        normals: vec![[0.0, 1.0, 0.0]; 4],
        positions: vec![
            [-1.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            [0.0, 0.0, -1.0],
            [-1.0, 0.0, -1.0],
        ],
        uvs: vec![[0.0, 0.0], [0.5, 0.0], [0.5, 1.0], [0.0, 1.0]],
        indices: vec![0, 1, 2, 0, 2, 3, 1],
        ..Default::default()
    };
    assert_eq!(
        curvature_masks(&mesh, 8, 8, &CurvatureOptions::default())
            .err()
            .map(|e| e.code()),
        Some("gen_invalid_input")
    );
}
