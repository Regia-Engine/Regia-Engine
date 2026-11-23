// SPDX-License-Identifier: MPL-2.0
//! Alpha-tested voxelization (ADR-3108): the cook measures coverage with the material's alpha
//! test, not the triangle area.

use super::*;

/// A leaf mask `size`² texels: an ellipse with round holes (alpha 255 inside, 0 outside).
fn leaf_alpha(size: usize) -> Vec<u8> {
    let mut alpha = Vec::with_capacity(size * size);
    for y in 0..size {
        for x in 0..size {
            let u = (x as f32 + 0.5) / size as f32 * 2.0 - 1.0;
            let v = (y as f32 + 0.5) / size as f32 * 2.0 - 1.0;
            let inside = u * u / 0.55 + v * v / 0.95 < 1.0;
            let hole =
                ((u * 4.0).fract().abs() - 0.5).powi(2) + ((v * 4.0).fract().abs() - 0.5).powi(2) < 0.05;
            alpha.push(if inside && !hole { 255 } else { 0 });
        }
    }
    alpha
}

fn mask(alpha: &[u8], size: u32) -> VoxelCoverage<'_> {
    VoxelCoverage {
        width: size,
        height: size,
        alpha,
        factor: 1.0,
        cutoff: 0.5,
        vertex_alpha: true,
        uv_transform: TextureTransform::IDENTITY,
    }
}

/// `count` leaf cards (two triangles, UV 0..1 across) in a unit ball, a fixed LCG.
fn cards(count: u32, half: f32) -> regia_asset::CpuMesh {
    let mut state = 0x1357_9bdfu32;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 8) as f32 / (1u32 << 24) as f32
    };
    let mut mesh = regia_asset::CpuMesh::default();
    for _ in 0..count {
        let (mut c, mut l2) = ([0.0f32; 3], 2.0f32);
        while l2 > 1.0 {
            c = [next() * 2.0 - 1.0, next() * 2.0 - 1.0, next() * 2.0 - 1.0];
            l2 = dot(c, c);
        }
        let n = c.map(|v| v / l2.sqrt().max(1e-3));
        let a = if n[0].abs() < 0.9 {
            [1.0, 0.0, 0.0]
        } else {
            [0.0, 1.0, 0.0]
        };
        let t = cross(n, a);
        let t = t.map(|v| v / dot(t, t).sqrt() * half);
        let b = cross(n, t);
        let base = mesh.positions.len() as u32;
        for (u, v) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            mesh.positions
                .push(std::array::from_fn(|k| c[k] * 0.85 + t[k] * u + b[k] * v));
            mesh.normals.push(n);
            mesh.uvs.push([(u + 1.0) * 0.5, (v + 1.0) * 0.5]);
            mesh.colors.push([0.2, 0.5, 0.1, 1.0]);
        }
        mesh.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    mesh
}

fn input(mesh: &regia_asset::CpuMesh) -> VoxelizeInput<'_> {
    VoxelizeInput {
        positions: &mesh.positions,
        normals: &mesh.normals,
        uvs: &mesh.uvs,
        colors: &mesh.colors,
        indices: &mesh.indices,
    }
}

const SETTINGS: VoxelFarCookSettings = VoxelFarCookSettings { resolution: 32 };

#[test]
fn alpha_one_geometry_cooks_exactly_as_the_opaque_cook() {
    let mesh = cards(300, 0.12);
    let opaque = voxelize("cards", &input(&mesh), &SETTINGS).expect("opaque");
    let solid = vec![255u8; 16 * 16];
    let masked =
        voxelize_with_coverage("cards", &input(&mesh), Some(&mask(&solid, 16)), &SETTINGS).expect("alpha 1");
    assert_eq!(
        masked.to_bytes(),
        opaque.to_bytes(),
        "alpha 1 everywhere is the opaque cook"
    );
    // No texture: the factor alone decides, and 1 passes.
    let untextured =
        voxelize_with_coverage("cards", &input(&mesh), Some(&mask(&[], 0)), &SETTINGS).expect("no texture");
    assert_eq!(untextured.to_bytes(), opaque.to_bytes());
}

#[test]
fn alpha_zero_geometry_contributes_no_voxels() {
    let mesh = cards(300, 0.12);
    let clear = vec![0u8; 16 * 16];
    assert!(matches!(
        voxelize_with_coverage("cards", &input(&mesh), Some(&mask(&clear, 16)), &SETTINGS),
        Err(VoxelFarCookError::Empty { .. })
    ));
    // A factor under the cutoff cuts everything away too, and so does vertex alpha 0.
    let solid = vec![255u8; 16 * 16];
    let faint = VoxelCoverage {
        factor: 0.4,
        ..mask(&solid, 16)
    };
    assert!(voxelize_with_coverage("cards", &input(&mesh), Some(&faint), &SETTINGS).is_err());
    let mut hidden = mesh.clone();
    hidden.colors.iter_mut().for_each(|c| c[3] = 0.0);
    assert!(voxelize_with_coverage("cards", &input(&hidden), Some(&mask(&solid, 16)), &SETTINGS).is_err());
    // With the vertex alpha carrying occlusion instead, it is not part of the test.
    let ao = VoxelCoverage {
        vertex_alpha: false,
        ..mask(&solid, 16)
    };
    assert!(voxelize_with_coverage("cards", &input(&hidden), Some(&ao), &SETTINGS).is_ok());

    // Two quads far apart, one on the texture's clear half: only the other one makes voxels.
    let mut half = vec![0u8; 16 * 16];
    for row in half.as_chunks_mut::<16>().0 {
        row[8..].fill(255);
    }
    let mut two = regia_asset::CpuMesh::default();
    for (x, u0) in [(-1.0f32, 0.0f32), (1.0, 0.5)] {
        let base = two.positions.len() as u32;
        for (dx, dy, du, dv) in [
            (0.0, 0.0, 0.0, 0.0),
            (0.5, 0.0, 0.5, 0.0),
            (0.5, 0.5, 0.5, 1.0),
            (0.0, 0.5, 0.0, 1.0),
        ] {
            two.positions.push([x + dx, dy, 0.0]);
            two.normals.push([0.0, 0.0, 1.0]);
            // u in [u0, u0 + 0.49]: entirely on one half (bilinear stays clear of the seam).
            two.uvs.push([u0 + 0.01 + du * 0.47, dv]);
        }
        two.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    let proxy =
        voxelize_with_coverage("two", &input(&two), Some(&mask(&half, 16)), &SETTINGS).expect("one quad");
    let size = proxy.levels[0].voxel_size;
    for (xyz, _) in proxy.level_voxels(0) {
        let x = proxy.origin[0] + (xyz[0] as f32 + 0.5) * size;
        assert!(x > 0.9, "a voxel at x {x} lies on the cut-away quad");
    }
    let opaque = voxelize("two", &input(&two), &SETTINGS).expect("opaque");
    assert!(proxy.voxel_count(0) * 2 <= opaque.voxel_count(0) + 2);
}

/// The fill rule: a card cut away to about half keeps its voxels where it is covered, with an
/// opacity near its covered share instead of the opaque rule's 1.
#[test]
fn a_half_covered_card_keeps_half_its_opacity() {
    // A checker of 4 × 4 texel squares over a 64² texture: half covered, finely.
    let size = 64usize;
    let checker: Vec<u8> = (0..size * size)
        .map(|i| {
            if ((i % size) / 4 + (i / size) / 4).is_multiple_of(2) {
                255
            } else {
                0
            }
        })
        .collect();
    let mut quad = regia_asset::CpuMesh::default();
    for (x, y, u, v) in [
        (0.0, 0.0, 0.0, 0.0),
        (1.0, 0.0, 1.0, 0.0),
        (1.0, 1.0, 1.0, 1.0),
        (0.0, 1.0, 0.0, 1.0),
    ] {
        quad.positions.push([x, y, 0.0]);
        quad.normals.push([0.0, 0.0, 1.0]);
        quad.uvs.push([u, v]);
    }
    quad.indices = vec![0, 1, 2, 0, 2, 3];
    let settings = VoxelFarCookSettings { resolution: 4 };
    let opaque = voxelize("quad", &input(&quad), &settings).expect("opaque");
    let masked = voxelize_with_coverage(
        "quad",
        &input(&quad),
        Some(&mask(&checker, size as u32)),
        &settings,
    )
    .expect("masked");
    // The plane lies on a voxel boundary: the opaque cook fills both layers, the samples one.
    let columns = |p: &VoxelFarProxy| {
        p.level_voxels(0)
            .map(|(xyz, _)| [xyz[0], xyz[1]])
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(
        columns(&masked),
        columns(&opaque),
        "every voxel column holds covered texels"
    );
    let mean = |p: &VoxelFarProxy, level: usize| {
        let (sum, n) = p
            .level_voxels(level)
            .fold((0.0f32, 0usize), |(s, n), (_, v)| (s + v.opacity(), n + 1));
        sum / n.max(1) as f32
    };
    for level in 0..masked.levels.len() {
        let (o, m) = (mean(&opaque, level), mean(&masked, level));
        assert!(o > 0.95, "level {level}: an opaque plane stops every ray ({o})");
        assert!((m - 0.5).abs() < 0.1, "level {level}: half covered, opacity {m}");
    }
    // The representative texture coordinate of every voxel is a covered point.
    for level in 0..masked.levels.len() {
        for (_, v) in masked.level_voxels(level) {
            assert!(
                mask(&checker, size as u32).passes(v.uv, 1.0),
                "uv {:?} is cut away",
                v.uv
            );
        }
    }
}

#[test]
fn the_masked_cook_is_deterministic() {
    let mesh = cards(400, 0.12);
    let alpha = leaf_alpha(64);
    let a = voxelize_with_coverage("cards", &input(&mesh), Some(&mask(&alpha, 64)), &SETTINGS).expect("a");
    let b = voxelize_with_coverage("cards", &input(&mesh), Some(&mask(&alpha, 64)), &SETTINGS).expect("b");
    assert_eq!(a.to_bytes(), b.to_bytes());
    let opaque = voxelize("cards", &input(&mesh), &SETTINGS).expect("opaque");
    assert_ne!(a.to_bytes(), opaque.to_bytes(), "the leaf mask changes the proxy");
    a.validate().expect("a valid proxy");
}

/// The cook finds the mesh's masked material through the model, material and texture sub-assets,
/// decodes the block-compressed texture, and voxelizes with its alpha; the mask's bytes are part of
/// the cache key.
#[test]
fn the_cook_reads_the_masked_material_beside_the_mesh() {
    let dir = regia_test_utils::unique_temp_dir("regia-voxel-far-masked");
    let parent = AssetId::new_random();
    let mesh = cards(200, 0.12);
    let cooked =
        crate::mesh_cook::pack_cooked_mesh(&crate::mesh_cook::cook_mesh(mesh.clone()), None).expect("pack");
    let payload = cooked.to_postcard().expect("postcard");
    let mesh_id = AssetId::sub_asset(parent, "mesh/Crown/primitive_0");
    let texture_id = AssetId::sub_asset(parent, "texture/0");
    let material_id = AssetId::sub_asset(parent, "material/Leaf");
    let model_id = AssetId::sub_asset(parent, "model");
    let texture_bytes = |alpha: &[u8]| {
        let rgba: Vec<u8> = alpha.iter().flat_map(|&a| [60, 140, 40, a]).collect();
        crate::texture_compress::cook_texture_rgba8(
            64,
            64,
            &rgba,
            regia_asset::TextureCookUsage::ColorSrgb,
            "linux",
            crate::texture_compress::TextureCookQuality::High,
        )
        .expect("texture cook")
        .to_postcard()
        .expect("postcard")
    };
    let alpha = leaf_alpha(64);
    let texture = texture_bytes(&alpha);
    let mut authored = regia_asset::MaterialAssetRon::default_standard();
    authored.render_state.alpha_mode = regia_asset::AlphaMode::Mask;
    authored.render_state.alpha_cutoff = 0.5;
    authored.base.base_color_map = Some(regia_asset::TextureSlot::Asset(texture_id));
    let material = regia_asset::CookedStandardMaterial::new(authored)
        .to_postcard()
        .expect("postcard");
    let model = regia_asset::CookedModel {
        version: regia_asset::COOKED_MODEL_VERSION,
        parts: vec![regia_asset::CookedModelPart {
            mesh: mesh_id,
            material: Some(material_id),
            node: "Crown".into(),
        }],
    }
    .to_postcard()
    .expect("postcard");
    let subs = |texture: &[u8]| -> Vec<(&'static str, AssetId, &'static str, Vec<u8>)> {
        vec![
            ("texture/0", texture_id, TEXTURE_TYPE_PATH, texture.to_vec()),
            ("material/Leaf", material_id, MATERIAL_TYPE_PATH, material.clone()),
            ("mesh/Crown/primitive_0", mesh_id, MESH_TYPE_PATH, payload.clone()),
            ("model", model_id, MODEL_TYPE_PATH, model.clone()),
        ]
    };
    let cook = |subs: &[(&'static str, AssetId, &'static str, Vec<u8>)]| {
        cook_voxel_far_sub_assets(
            &dir,
            "assets/tree.gltf",
            1,
            parent,
            &SETTINGS,
            subs.iter().map(|(p, id, t, b)| (*p, *id, *t, b.as_slice())),
        )
        .expect("cook")
    };
    let first = cook(&subs(&texture));
    assert_eq!(first.built, 1);
    let file = dir.join(cooked_file_name(first.manifest[0].id));
    let read = |file: &std::path::Path| {
        VoxelFarProxy::from_cooked_file(&std::fs::read(file).expect("read")).expect("runtime load")
    };
    let proxy = read(&file);
    let opaque = voxel_far_for_cooked_mesh("voxels/Crown/primitive_0", &payload, &SETTINGS).expect("opaque");
    assert_ne!(
        proxy.to_bytes(),
        opaque.to_bytes(),
        "the masked material was found"
    );
    // The same as voxelizing with the decoded texture's alpha.
    let decoded = regia_render::CompressedTexture::from_cooked(
        &regia_asset::CookedTexture::from_postcard(&texture).expect("texture"),
    )
    .expect("ktx2")
    .decode_rgba8()
    .expect("decode");
    let decoded_alpha: Vec<u8> = decoded.rgba.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
    let direct = voxel_far_for_cooked_mesh_with_coverage(
        "voxels/Crown/primitive_0",
        &payload,
        Some(&mask(&decoded_alpha, 64)),
        &SETTINGS,
    )
    .expect("direct");
    assert_eq!(proxy.to_bytes(), direct.to_bytes());
    assert_eq!(cook(&subs(&texture)).built, 0, "cached");
    // An edited mask recooks.
    let mut wider = alpha.clone();
    wider.iter_mut().step_by(7).for_each(|a| *a = 255);
    assert_eq!(
        cook(&subs(&texture_bytes(&wider))).built,
        1,
        "a new mask rebuilds"
    );
    // An opaque material cooks the opaque proxy.
    let mut opaque_subs = subs(&texture);
    let mut plain = regia_asset::MaterialAssetRon::default_standard();
    plain.base.base_color_map = Some(regia_asset::TextureSlot::Asset(texture_id));
    opaque_subs[1].3 = regia_asset::CookedStandardMaterial::new(plain)
        .to_postcard()
        .expect("postcard");
    assert_eq!(cook(&opaque_subs).built, 1);
    assert_eq!(read(&file).to_bytes(), opaque.to_bytes());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The sampled box error (ADR-3108, `.fvox` v3): a flat surface's boxes are flat, so the measured
/// distance from their faces to the surface is far below the analytic bound (1.12 voxel edges for a
/// full-width flat box), and it is honest: a dense grid on every box face of every level stays within
/// it of the surface.
#[test]
fn sampled_box_errors_are_tighter_for_flat_boxes_and_honest() {
    // A tilted plane through the grid (flat boxes on one axis, thin on none).
    let mut plane = regia_asset::CpuMesh::default();
    for (x, z) in [(0.0f32, 0.0f32), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
        plane.positions.push([x, 0.3 * x + 0.1 * z, z]);
        plane.normals.push([0.0, 1.0, 0.0]);
        plane.uvs.push([x, z]);
    }
    plane.indices = vec![0, 1, 2, 0, 2, 3];
    let settings = VoxelFarCookSettings { resolution: 16 };
    let proxy = voxelize("plane", &input(&plane), &settings).expect("plane");
    assert_eq!(
        proxy.sampled_errors.len(),
        proxy.levels.len(),
        "a tighter error per level"
    );
    let first = &proxy.levels[0];
    let sampled = proxy.level_box_error(0);
    println!(
        "plane: level 0 analytic {:.3} edges, sampled {:.3} edges",
        first.error / first.voxel_size,
        sampled / first.voxel_size
    );
    assert!(
        sampled < first.error * 0.5,
        "flat boxes: {sampled} vs {}",
        first.error
    );
    proxy.validate().expect("valid v3 proxy");

    let canopy = cards(300, 0.12);
    let leaves = voxelize("cards", &input(&canopy), &SETTINGS).expect("cards");
    for (name, p, mesh) in [("plane", &proxy, &plane), ("cards", &leaves, &canopy)] {
        let tris: Vec<[V3; 3]> = mesh
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| t.map(|i| mesh.positions[i as usize]))
            .collect();
        let aabbs: Vec<(V3, V3)> = tris
            .iter()
            .map(|v| {
                (
                    std::array::from_fn(|k| v[0][k].min(v[1][k]).min(v[2][k])),
                    std::array::from_fn(|k| v[0][k].max(v[1][k]).max(v[2][k])),
                )
            })
            .collect();
        for level in 0..p.levels.len() {
            let info = p.levels[level];
            let bound = p.level_box_error(level);
            assert!(
                bound <= info.error,
                "{name} level {level}: sampled above analytic"
            );
            let step = info.voxel_size / VOXEL_EXTENT_STEPS as f32;
            let mut worst = 0.0f32;
            for (xyz, _, packed) in p.level_voxel_extents(level) {
                let (lo, hi) = regia_render::visbuffer::unpack_voxel_extent(packed);
                let corner: V3 = std::array::from_fn(|k| p.origin[k] + xyz[k] as f32 * info.voxel_size);
                let blo: V3 = std::array::from_fn(|k| corner[k] + lo[k] as f32 * step);
                let bhi: V3 = std::array::from_fn(|k| corner[k] + hi[k] as f32 * step);
                // Only triangles that can be within the bound of the box can be the nearest one
                // below it (an empty list reads as infinitely far and fails, as it should).
                let near: Vec<[V3; 3]> = tris
                    .iter()
                    .zip(&aabbs)
                    .filter(|(_, (tlo, thi))| {
                        (0..3).all(|k| tlo[k] <= bhi[k] + bound && thi[k] >= blo[k] - bound)
                    })
                    .map(|(v, _)| *v)
                    .collect();
                // A 15 x 15 grid per face (not the cook's 1/8-voxel grid).
                for k in 0..3 {
                    let (a, b) = ((k + 1) % 3, (k + 2) % 3);
                    for side in [blo[k], bhi[k]] {
                        for i in 0..=14 {
                            for j in 0..=14 {
                                let mut q = [0.0f32; 3];
                                q[k] = side;
                                q[a] = blo[a] + (bhi[a] - blo[a]) * i as f32 / 14.0;
                                q[b] = blo[b] + (bhi[b] - blo[b]) * j as f32 / 14.0;
                                let d = near
                                    .iter()
                                    .map(|v| {
                                        let w = closest_barycentric(q, *v);
                                        let c: V3 = std::array::from_fn(|n| {
                                            v[0][n] * w[0] + v[1][n] * w[1] + v[2][n] * w[2]
                                        });
                                        dot(sub(q, c), sub(q, c)).sqrt()
                                    })
                                    .fold(f32::MAX, f32::min);
                                worst = worst.max(d);
                            }
                        }
                    }
                }
            }
            assert!(
                worst <= bound * (1.0 + 1e-4),
                "{name} level {level}: a box face point lies {worst} from the surface, bound {bound}"
            );
            if level == 0 {
                println!(
                    "{name}: level 0 analytic {:.3}, sampled {:.3}, measured worst {:.3} edges",
                    info.error / info.voxel_size,
                    bound / info.voxel_size,
                    worst / info.voxel_size
                );
            }
        }
    }
}

/// Builds the cooked sub-assets of one glTF crown (mesh, material, texture, model) for the
/// end-to-end cook tests of ADR-3227, with `alpha` as the base color texture (64², BC-cooked).
struct CookedCrown {
    parent: AssetId,
    material_id: AssetId,
    payload: Vec<u8>,
    subs: Vec<(&'static str, AssetId, &'static str, Vec<u8>)>,
}

fn cooked_crown(mesh: &regia_asset::CpuMesh, alpha: &[u8]) -> CookedCrown {
    let parent = AssetId::new_random();
    let cooked =
        crate::mesh_cook::pack_cooked_mesh(&crate::mesh_cook::cook_mesh(mesh.clone()), None).expect("pack");
    let payload = cooked.to_postcard().expect("postcard");
    let mesh_id = AssetId::sub_asset(parent, "mesh/Crown/primitive_0");
    let texture_id = AssetId::sub_asset(parent, "texture/0");
    let material_id = AssetId::sub_asset(parent, "material/Leaf");
    let model_id = AssetId::sub_asset(parent, "model");
    let rgba: Vec<u8> = alpha.iter().flat_map(|&a| [60, 140, 40, a]).collect();
    let texture = crate::texture_compress::cook_texture_rgba8(
        64,
        64,
        &rgba,
        regia_asset::TextureCookUsage::ColorSrgb,
        "linux",
        crate::texture_compress::TextureCookQuality::High,
    )
    .expect("texture cook")
    .to_postcard()
    .expect("postcard");
    let mut authored = regia_asset::MaterialAssetRon::default_standard();
    authored.render_state.alpha_mode = regia_asset::AlphaMode::Mask;
    authored.render_state.alpha_cutoff = 0.5;
    authored.base.base_color_map = Some(regia_asset::TextureSlot::Asset(texture_id));
    let material = regia_asset::CookedStandardMaterial::new(authored)
        .to_postcard()
        .expect("postcard");
    let model = regia_asset::CookedModel {
        version: regia_asset::COOKED_MODEL_VERSION,
        parts: vec![regia_asset::CookedModelPart {
            mesh: mesh_id,
            material: Some(material_id),
            node: "Crown".into(),
        }],
    }
    .to_postcard()
    .expect("postcard");
    CookedCrown {
        parent,
        material_id,
        subs: vec![
            ("texture/0", texture_id, TEXTURE_TYPE_PATH, texture),
            ("material/Leaf", material_id, MATERIAL_TYPE_PATH, material),
            ("mesh/Crown/primitive_0", mesh_id, MESH_TYPE_PATH, payload.clone()),
            ("model", model_id, MODEL_TYPE_PATH, model),
        ],
        payload,
    }
}

impl CookedCrown {
    /// Cooks the crown into `dir` with `source`; the proxy as the runtime loads it, and how many
    /// proxies were built (0: the cache key matched).
    fn cook(
        &self,
        dir: &std::path::Path,
        source: &VoxelFarSource,
    ) -> Result<(VoxelFarProxy, usize), VoxelFarCookError> {
        let out = cook_voxel_far_sub_assets_from(
            dir,
            "assets/tree.gltf",
            1,
            self.parent,
            &SETTINGS,
            source,
            self.subs.iter().map(|(p, id, t, b)| (*p, *id, *t, b.as_slice())),
        )?;
        let file = dir.join(cooked_file_name(out.manifest[0].id));
        let proxy =
            VoxelFarProxy::from_cooked_file(&std::fs::read(file).expect("read")).expect("runtime load");
        Ok((proxy, out.built))
    }

    /// The proxy's cooked file and cache key in `dir`.
    fn proxy_files(&self, dir: &std::path::Path) -> [std::path::PathBuf; 2] {
        let id = AssetId::sub_asset(self.parent, "voxels/Crown/primitive_0");
        [
            dir.join(cooked_file_name(id)),
            dir.join(format!("{}.key", cooked_file_name(id))),
        ]
    }

    /// Cooks the crown into `dir` with `source`; true when the cook succeeded, built nothing,
    /// listed no proxy and left no proxy file or key on disk (the alpha test cut it away).
    fn cook_cut_away(&self, dir: &std::path::Path, source: &VoxelFarSource) -> bool {
        let out = cook_voxel_far_sub_assets_from(
            dir,
            "assets/tree.gltf",
            1,
            self.parent,
            &SETTINGS,
            source,
            self.subs.iter().map(|(p, id, t, b)| (*p, *id, *t, b.as_slice())),
        )
        .expect("a cut-away primitive does not fail the cook");
        out.built == 0
            && out.manifest.is_empty()
            && out.meta.is_empty()
            && self.proxy_files(dir).iter().all(|f| !f.exists())
    }
}

/// A masked primitive its alpha test cuts away everywhere (alpha 0 across the texture: an
/// exporter's invisible helper) gets no voxel proxy, and the asset still cooks (ADR-5571); before,
/// the whole glTF cook failed with `Empty`. A proxy cooked before the mask cut it away is removed
/// with its key, so the runtime, which finds the proxy by id, does not load the stale one.
#[test]
fn a_cut_away_masked_primitive_does_not_fail_the_cook() {
    let mesh = cards(50, 0.12);
    let clear = vec![0u8; 64 * 64];
    let crown = cooked_crown(&mesh, &clear);
    let dir = regia_test_utils::unique_temp_dir("regia-voxel-far-cut-away");
    // A stale proxy and key from an earlier cook of this primitive.
    for file in crown.proxy_files(&dir) {
        std::fs::write(file, b"stale").expect("stale file");
    }
    assert!(crown.cook_cut_away(&dir, &VoxelFarSource::default()));
    // Cooking again changes nothing.
    assert!(crown.cook_cut_away(&dir, &VoxelFarSource::default()));
    // The same crown with a solid texture cooks its proxy as before.
    let solid = cooked_crown(&mesh, &vec![255u8; 64 * 64]);
    assert_eq!(solid.cook(&dir, &VoxelFarSource::default()).expect("solid").1, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `count` leaf cards whose texture coordinates span only `0..0.5` across (the left half of the
/// texture).
fn left_half_cards(count: u32) -> regia_asset::CpuMesh {
    let mut mesh = cards(count, 0.12);
    for uv in &mut mesh.uvs {
        uv[0] *= 0.5;
    }
    mesh
}

/// A 64² texture whose left half (u < 0.5) is clear and right half opaque.
fn right_half_alpha() -> Vec<u8> {
    let mut alpha = vec![0u8; 64 * 64];
    for row in alpha.as_chunks_mut::<64>().0 {
        row[32..].fill(255);
    }
    alpha
}

/// `KHR_texture_transform` (ADR-3227): cards whose UVs read the clear half of the texture are cut
/// away entirely without the base color's transform, and the glTF's offset of half the texture
/// moves them onto the opaque half, where they cook exactly as opaque cards. The transform comes
/// from the import's extension table by material index and is in the cache key.
#[test]
fn the_coverage_test_applies_the_base_color_texture_transform() {
    let mesh = left_half_cards(200);
    let alpha = right_half_alpha();
    let shift = TextureTransform {
        offset: [0.5, 0.0],
        ..TextureTransform::IDENTITY
    };
    // The voxelizer: without the transform nothing is covered, with it everything is.
    assert!(matches!(
        voxelize_with_coverage("cards", &input(&mesh), Some(&mask(&alpha, 64)), &SETTINGS),
        Err(VoxelFarCookError::Empty { .. })
    ));
    let moved = VoxelCoverage {
        uv_transform: shift,
        ..mask(&alpha, 64)
    };
    assert_eq!(moved.texture_uv([0.25, 0.5]), [0.75, 0.5]);
    let opaque = voxelize("cards", &input(&mesh), &SETTINGS).expect("opaque");
    let transformed = voxelize_with_coverage("cards", &input(&mesh), Some(&moved), &SETTINGS).expect("moved");
    assert_eq!(
        transformed.to_bytes(),
        opaque.to_bytes(),
        "every point now covered"
    );
    // The representative coordinates stay the mesh's (the renderer samples near and far alike).
    assert!(transformed.voxels.iter().all(|v| v.uv[0] <= 0.5 + 1e-6));

    // End to end: the source's extension table names the transform by material index.
    let crown = cooked_crown(&mesh, &alpha);
    let mut extensions = GltfExtensionTable::default();
    let mut material = crate::gltf_extensions::GltfMaterialExtensions {
        index: 0,
        ..Default::default()
    };
    material.textures.insert(
        "base_color".into(),
        crate::gltf_extensions::GltfTextureRef {
            image: 0,
            tex_coord: 0,
            transform: Some(shift),
        },
    );
    extensions.materials.push(material);
    let source = VoxelFarSource::from_gltf(false, &[crown.material_id], &extensions);
    assert_eq!(source.base_color_transforms.get(&crown.material_id), Some(&shift));
    let dir = regia_test_utils::unique_temp_dir("regia-voxel-far-uv-transform");
    // Without the transform the clear half cuts every card away: the primitive gets no proxy
    // and the cook still succeeds (ADR-5571).
    assert!(
        crown.cook_cut_away(&dir, &VoxelFarSource::default()),
        "without the transform the clear half cuts every card away"
    );
    let (proxy, built) = crown.cook(&dir, &source).expect("with the transform");
    assert_eq!(built, 1);
    let opaque_cooked =
        voxel_far_for_cooked_mesh("voxels/Crown/primitive_0", &crown.payload, &SETTINGS).expect("opaque");
    assert_eq!(proxy.to_bytes(), opaque_cooked.to_bytes());
    assert_eq!(crown.cook(&dir, &source).expect("cached").1, 0, "cached");
    let other = VoxelFarSource {
        base_color_transforms: [(
            crown.material_id,
            TextureTransform {
                offset: [0.625, 0.0],
                ..TextureTransform::IDENTITY
            },
        )]
        .into(),
        ..VoxelFarSource::default()
    };
    assert_eq!(
        crown.cook(&dir, &other).expect("other offset").1,
        1,
        "the transform is in the key"
    );
    // An identity transform, or one on a material that is not in the file, is no transform.
    let identity = VoxelFarSource::from_gltf(false, &[], &extensions);
    assert!(identity.base_color_transforms.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// `vertex_ao` (ADR-3227): a crown whose `COLOR_0` alpha carries occlusion (0.3 everywhere) on a
/// solid leaf texture keeps all its coverage when the importer says the alpha is occlusion, and
/// cooks exactly as the opaque crown; read as opacity (the setting off), 0.3 under the 0.5 cutoff
/// cuts every card away. The setting is in the cache key.
#[test]
fn a_vertex_ao_alpha_does_not_scale_the_coverage() {
    let mut mesh = cards(200, 0.12);
    for color in &mut mesh.colors {
        color[3] = 0.3;
    }
    let solid = vec![255u8; 64 * 64];
    let crown = cooked_crown(&mesh, &solid);
    let dir = regia_test_utils::unique_temp_dir("regia-voxel-far-vertex-ao");
    // Read as opacity, 0.3 cuts every card away: no proxy (ADR-5571).
    assert!(crown.cook_cut_away(&dir, &VoxelFarSource::default()));
    let ao = VoxelFarSource {
        vertex_ao: true,
        ..VoxelFarSource::default()
    };
    let (proxy, built) = crown.cook(&dir, &ao).expect("occlusion is not opacity");
    assert_eq!(built, 1);
    let opaque =
        voxel_far_for_cooked_mesh("voxels/Crown/primitive_0", &crown.payload, &SETTINGS).expect("opaque");
    assert_eq!(proxy.to_bytes(), opaque.to_bytes());
    // The coverage estimate agrees: the level draws what the opaque crown draws.
    assert_eq!(proxy.coverage, opaque.coverage);
    assert_eq!(crown.cook(&dir, &ao).expect("cached").1, 0);
    // A crown with the alpha above the cutoff cooks the same either way (no AO to leave out).
    let mut lit = mesh.clone();
    lit.colors.iter_mut().for_each(|c| c[3] = 1.0);
    let lit_crown = cooked_crown(&lit, &solid);
    let plain = lit_crown.cook(&dir, &VoxelFarSource::default()).expect("plain").0;
    let with_ao = lit_crown.cook(&dir, &ao).expect("ao").0;
    assert_eq!(plain.to_bytes(), with_ao.to_bytes());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The coverage estimate (ADR-3227, `.fvox` v4) on masked leaf-card canopies (a leaf mask with
/// holes): every voxel level a cut uses draws within `COVERAGE_TOLERANCE` of the alpha-tested
/// source's coverage at its switch distance, so the far canopy neither thins nor swells. Before
/// the box-fitted opacity (ADR-3108's fill rule) the see-through canopy measured 1.161, 1.275 and
/// 1.691 on these levels, outside the bound. The cook by area and an opaque crown are printed
/// (`--nocapture`) for comparison: with the masked raster testing each face at its texture
/// coordinate, the area cook lands near 1 on the fine levels too; an opaque crown keeps the fill
/// rule and swells on coarse levels.
#[test]
fn a_masked_canopy_keeps_its_coverage_at_the_switch_distance() {
    let alpha = leaf_alpha(64);
    let leaf = mask(&alpha, 64);
    let within = |c: f32| (1.0 - COVERAGE_TOLERANCE..=1.0 + COVERAGE_TOLERANCE).contains(&c);
    let mut misses = Vec::new();
    for (name, mesh) in [("see-through", cards(120, 0.08)), ("dense", cards(300, 0.12))] {
        let masked = voxelize_with_coverage("cards", &input(&mesh), Some(&leaf), &SETTINGS).expect("masked");
        assert_eq!(
            masked.coverage.len(),
            masked.levels.len(),
            "one estimate per level"
        );
        let area = voxelize("cards", &input(&mesh), &SETTINGS).expect("by area");
        let area_vs_masked = level_coverage(&input(&mesh), Some(&leaf), &area);
        for (i, level) in masked.levels.iter().enumerate() {
            println!(
                "{name} canopy, level {i} ({}³, box error {:.4}): alpha-tested cook {:.3}, by area {:.3}; \
                 opaque crown {:.3}",
                level.resolution,
                masked.level_box_error(i),
                masked.coverage[i],
                area_vs_masked[i],
                area.coverage[i]
            );
        }
        for (i, &c) in masked.coverage.iter().enumerate().take(COVERAGE_LEVELS) {
            if !within(c) {
                misses.push(format!("{name} level {i}: {c:.3}"));
            }
        }
        // The estimate survives the format round trip.
        assert_eq!(
            VoxelFarProxy::from_bytes(&masked.to_bytes())
                .expect("v4")
                .coverage,
            masked.coverage
        );
    }
    assert!(
        misses.is_empty(),
        "the alpha-tested cook leaves the bound of {COVERAGE_TOLERANCE}: {misses:?}"
    );
}

/// Levels the coverage bound holds on: the levels a canopy's cut uses past its clusters (32³,
/// 16³, 8³ at `SETTINGS`); the 4³ and coarser levels hold a crown in a handful of voxels.
const COVERAGE_LEVELS: usize = 3;
/// Allowed deviation of a level's coverage from the source's (ADR-3227).
const COVERAGE_TOLERANCE: f32 = 0.2;
