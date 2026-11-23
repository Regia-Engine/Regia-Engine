// SPDX-License-Identifier: MPL-2.0
//! ADR-2483: the baked HLOD proxy atlas samples the source surface it covers.

use super::*;
use regia_core::math::Quat;

/// A solid sRGB texture.
fn solid(rgba: [u8; 4]) -> Arc<BakeImage> {
    Arc::new(BakeImage::new(4, 4, rgba.repeat(16)).expect("image"))
}

/// A texture whose red channel (sRGB-encoded) is `u` and green `v`, 64 texels square.
fn gradient() -> Arc<BakeImage> {
    let n = 64u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let u = (x as f32 + 0.5) / n as f32;
            let v = (y as f32 + 0.5) / n as f32;
            rgba.extend_from_slice(&[(u * 255.0).round() as u8, (v * 255.0).round() as u8, 64, 255]);
        }
    }
    Arc::new(BakeImage::new(n, n, rgba).expect("image"))
}

/// A unit cube scaled to `size` at `at`.
fn placed(size: f32, at: Vec3) -> Affine3A {
    Affine3A::from_scale_rotation_translation(Vec3::splat(size), Quat::IDENTITY, at)
}

/// The fixture cell: a red-textured 4 m box and a gradient-textured 6 m box whose material halves
/// the red channel.
fn two_textured_boxes() -> Vec<(Affine3A, Arc<MeshSoup>)> {
    let cube = MeshSoup::builtin("builtin:cube").expect("cube");
    let red = BakeMaterial {
        base_color_map: Some(solid([255, 0, 0, 255])),
        roughness: 0.25,
        ..BakeMaterial::default()
    };
    let ramp = BakeMaterial {
        base_color: [0.5, 1.0, 1.0, 1.0],
        base_color_map: Some(gradient()),
        metallic: 1.0,
        roughness: 0.75,
        ..BakeMaterial::default()
    };
    vec![
        (
            placed(4.0, Vec3::new(-5.0, 0.0, 0.0)),
            Arc::new(cube.clone().with_material(Some(Arc::new(red)))),
        ),
        (
            placed(6.0, Vec3::new(6.0, 0.0, 0.0)),
            Arc::new(cube.with_material(Some(Arc::new(ramp)))),
        ),
    ]
}

/// The merged proxy of the meshes (welded, kept whole).
fn proxy_of(meshes: &[(Affine3A, Arc<MeshSoup>)]) -> MeshSoup {
    let mut merged = MeshSoup::default();
    for (world, mesh) in meshes {
        merged.append(mesh, world);
    }
    merged.simplified(1.0, 1)
}

/// The barycentric coordinates of `p` on triangle `t`, when `p` lies on it.
fn on_triangle(p: Vec3, t: [Vec3; 3]) -> Option<[f32; 3]> {
    let n = (t[1] - t[0]).cross(t[2] - t[0]);
    let area = n.length();
    if area < 1e-9 || (p - t[0]).dot(n / area).abs() > 1e-3 {
        return None;
    }
    let w = [
        (t[2] - t[1]).cross(p - t[1]).dot(n) / (area * area),
        (t[0] - t[2]).cross(p - t[2]).dot(n) / (area * area),
        (t[1] - t[0]).cross(p - t[0]).dot(n) / (area * area),
    ];
    w.iter().all(|w| *w >= -1e-4).then_some(w)
}

/// The UV a mesh gives the surface point `p` (`None` off the mesh).
fn uv_at(mesh: &MeshSoup, world: &Affine3A, p: Vec3) -> Option<[f32; 2]> {
    for tri in mesh.indices.as_chunks::<3>().0 {
        let v = tri.map(|i| {
            Vec3::from(world.transform_point3a(Vec3A::from(Vec3::from_array(mesh.positions[i as usize]))))
        });
        if let Some(w) = on_triangle(p, v) {
            let uv = tri.map(|i| Vec2::from_array(mesh.uvs[i as usize]));
            return Some((uv[0] * w[0] + uv[1] * w[1] + uv[2] * w[2]).to_array());
        }
    }
    None
}

/// The atlas texel (as linear RGBA, via `decode`) the baked proxy draws at surface point `p`.
fn atlas_at(baked: &BakedAtlas, image: &[u8], p: Vec3, decode: fn(usize, u8) -> f32) -> [f32; 4] {
    let uv = uv_at(&baked.mesh, &Affine3A::IDENTITY, p).expect("the point is on the proxy");
    let x = ((uv[0] * baked.width as f32) as u32).min(baked.width - 1);
    let y = ((uv[1] * baked.height as f32) as u32).min(baked.height - 1);
    let at = ((y * baked.width + x) * 4) as usize;
    std::array::from_fn(|c| decode(c, image[at + c]))
}

/// Points on the boxes' faces, away from edges: top, front and side faces of both.
fn probe_points() -> Vec<(usize, Vec3)> {
    let mut out = Vec::new();
    for (box_index, (center, half)) in [
        (Vec3::new(-5.0, 0.0, 0.0), 2.0f32),
        (Vec3::new(6.0, 0.0, 0.0), 3.0),
    ]
    .into_iter()
    .enumerate()
    {
        for (a, b) in [
            (-0.5f32, -0.5f32),
            (0.3, -0.4),
            (0.0, 0.0),
            (0.45, 0.4),
            (-0.3, 0.35),
        ] {
            out.push((
                box_index,
                center + Vec3::new(a * 2.0 * half * 0.9, half, b * 2.0 * half * 0.9),
            ));
            out.push((
                box_index,
                center + Vec3::new(a * 2.0 * half * 0.9, b * 2.0 * half * 0.9, half),
            ));
            out.push((
                box_index,
                center + Vec3::new(half, a * 2.0 * half * 0.9, b * 2.0 * half * 0.9),
            ));
        }
    }
    out
}

fn settings(texels_per_m: f32) -> AtlasSettings {
    AtlasSettings {
        texels_per_m,
        max_px: 1024,
    }
}

#[test]
fn the_atlas_returns_the_source_texture_colors_at_known_proxy_points() {
    let meshes = two_textured_boxes();
    let source = BakeSource::new(&meshes).expect("source");
    let proxy = proxy_of(&meshes);
    let baked = bake_proxy_atlas(&proxy, &[&source], settings(8.0)).expect("bake");
    assert!(!baked.capped);
    assert_eq!(baked.width % 4, 0);
    assert_eq!(baked.height % 4, 0);
    assert_eq!(baked.mesh.uvs.len(), baked.mesh.positions.len());
    assert!(baked.mesh.uvs.iter().flatten().all(|v| (0.0..=1.0).contains(v)));
    assert!(
        baked.bake_error_m < 1e-3,
        "the proxy is the source: {}",
        baked.bake_error_m
    );
    assert_eq!(baked.miss_ratio, 0.0);

    let srgb = |c: f32| decode_srgb(0, (c * 255.0).round() as u8);
    for (box_index, p) in probe_points() {
        let got = atlas_at(&baked, &baked.base_color, p, decode_srgb);
        let want = if box_index == 0 {
            [1.0, 0.0, 0.0]
        } else {
            let (world, mesh) = &meshes[1];
            let uv = uv_at(mesh, world, p).expect("on the source");
            // Factor 0.5 on red; the texture's red is u and its green v (sRGB-encoded).
            [0.5 * srgb(uv[0]), srgb(uv[1]), srgb(64.0 / 255.0)]
        };
        for c in 0..3 {
            assert!(
                (got[c] - want[c]).abs() < 0.04,
                "box {box_index} at {p}: atlas {got:?}, source {want:?}"
            );
        }
        // ORM: occlusion 1 (no map), the factors' roughness and metallic.
        let orm = atlas_at(&baked, &baked.orm, p, decode_unorm);
        let (rough, metal) = if box_index == 0 { (0.25, 0.0) } else { (0.75, 1.0) };
        assert!(
            (orm[0] - 1.0).abs() < 0.01 && (orm[1] - rough).abs() < 0.01 && (orm[2] - metal).abs() < 0.01,
            "{orm:?}"
        );
        // No normal map: the baked normal, read back through the proxy's own frame (whose welded
        // normals are smoothed over the box corners), is the source face's flat normal.
        let face = if (p.y - if box_index == 0 { 2.0 } else { 3.0 }).abs() < 1e-4 {
            Vec3::Y
        } else if (p.z - if box_index == 0 { 2.0 } else { 3.0 }).abs() < 1e-4 {
            Vec3::Z
        } else {
            Vec3::X
        };
        let world = world_normal_at(&baked, p);
        assert!(
            world.dot(face) > 0.97,
            "box {box_index} at {p}: {world} for face {face}"
        );
    }
}

/// The world normal the baked atlas gives surface point `p`: the atlas normal turned out of the
/// proxy's tangent frame at `p` (its interpolated normal and its atlas UV derivatives).
fn world_normal_at(baked: &BakedAtlas, p: Vec3) -> Vec3 {
    let local = atlas_at(baked, &baked.normal, p, decode_unorm).map(|v| v * 2.0 - 1.0);
    for tri in baked.mesh.indices.as_chunks::<3>().0 {
        let v = tri.map(|i| Vec3::from_array(baked.mesh.positions[i as usize]));
        let Some(w) = on_triangle(p, v) else { continue };
        let n = tri.map(|i| Vec3::from_array(baked.mesh.normals[i as usize]));
        let n = (n[0] * w[0] + n[1] * w[1] + n[2] * w[2]).normalize();
        let uv = tri.map(|i| Vec2::from_array(baked.mesh.uvs[i as usize]));
        let (t, b) = uv_frame(v, uv);
        let (t, b) = frame(n, t, b).expect("frame");
        return (t * local[0] + b * local[1] + n * local[2]).normalize();
    }
    panic!("{p} is not on the proxy");
}

#[test]
fn a_simplified_proxy_off_the_surface_finds_the_source_along_its_normal() {
    // The proxy is a plane 0.3 m above a textured plane: every texel's ray reaches down to it.
    let plane = MeshSoup::builtin("builtin:plane").expect("plane");
    let source_mesh = vec![(
        placed(8.0, Vec3::ZERO),
        Arc::new(plane.clone().with_material(Some(Arc::new(BakeMaterial {
            base_color_map: Some(gradient()),
            ..BakeMaterial::default()
        })))),
    )];
    let source = BakeSource::new(&source_mesh).expect("source");
    let mut proxy = MeshSoup::default();
    proxy.append(&plane, &placed(8.0, Vec3::new(0.0, 0.3, 0.0)));
    let baked = bake_proxy_atlas(&proxy, &[&source], settings(4.0)).expect("bake");
    assert!((baked.bake_error_m - 0.3).abs() < 1e-3, "{}", baked.bake_error_m);
    assert_eq!(baked.miss_ratio, 0.0);
    let srgb = |c: f32| decode_srgb(0, (c * 255.0).round() as u8);
    for (x, z) in [(-3.0, -3.0), (2.5, -1.0), (0.0, 3.2), (1.0, 1.0)] {
        let got = atlas_at(&baked, &baked.base_color, Vec3::new(x, 0.3, z), decode_srgb);
        let uv = uv_at(&source_mesh[0].1, &source_mesh[0].0, Vec3::new(x, 0.0, z)).expect("on the source");
        assert!(
            (got[0] - srgb(uv[0])).abs() < 0.04 && (got[1] - srgb(uv[1])).abs() < 0.04,
            "({x}, {z}): {got:?} {uv:?}"
        );
    }
}

#[test]
fn texels_with_no_source_are_dilated_and_counted_as_misses() {
    let plane = MeshSoup::builtin("builtin:plane").expect("plane");
    let source = BakeSource::new(&[(
        placed(2.0, Vec3::new(-3.0, 0.0, 0.0)),
        Arc::new(plane.clone().tinted([0.0, 1.0, 0.0, 1.0])),
    )])
    .expect("source");
    // A proxy twice as wide, half of it far from any source.
    let mut proxy = MeshSoup::default();
    proxy.append(
        &plane,
        &Affine3A::from_scale_rotation_translation(Vec3::new(8.0, 1.0, 2.0), Quat::IDENTITY, Vec3::ZERO),
    );
    let baked = bake_proxy_atlas(&proxy, &[&source], settings(4.0)).expect("bake");
    assert!(
        baked.miss_ratio > 0.3 && baked.miss_ratio < 0.95,
        "{}",
        baked.miss_ratio
    );
    // Every texel is filled, with the only baked color there is.
    for texel in baked.base_color.as_chunks::<4>().0 {
        assert_eq!(texel[3], 255);
        assert!(texel[1] > 200 && texel[0] < 20, "{texel:?}");
    }
}

#[test]
fn the_cap_truncates_the_density_and_says_so() {
    let meshes = two_textured_boxes();
    let source = BakeSource::new(&meshes).expect("source");
    let proxy = proxy_of(&meshes);
    let baked = bake_proxy_atlas(
        &proxy,
        &[&source],
        AtlasSettings {
            texels_per_m: 64.0,
            max_px: 32,
        },
    )
    .expect("bake");
    assert!(baked.capped);
    assert!(baked.texels_per_m < baked.asked_texels_per_m);
    assert!(
        baked.width <= 32 && baked.height <= 32,
        "{}x{}",
        baked.width,
        baked.height
    );
    assert_eq!(
        AtlasSettings::for_level(16, 9000, 2),
        AtlasSettings {
            texels_per_m: 4.0,
            max_px: 4096
        }
    );
    assert_eq!(AtlasSettings::for_level(16, 1, 0).max_px, ATLAS_MIN_PX);
}

#[test]
fn two_bakes_and_two_writes_give_identical_bytes() {
    let meshes = two_textured_boxes();
    let source = BakeSource::new(&meshes).expect("source");
    let proxy = proxy_of(&meshes);
    let a = bake_proxy_atlas(&proxy, &[&source], settings(6.0)).expect("bake");
    let b = bake_proxy_atlas(
        &proxy,
        &[&BakeSource::new(&meshes).expect("source")],
        settings(6.0),
    )
    .expect("bake");
    assert_eq!(a, b);
    let id =
        regia_asset::AssetId::sub_asset(regia_asset::AssetId(regia_core::id::Guid::from_u128(9)), "hlod/0");
    let wa = write_atlas(id, &a, "linux", None, "a.level.ron", 0).expect("write");
    let wb = write_atlas(id, &b, "linux", None, "a.level.ron", 0).expect("write");
    assert_eq!(wa, wb);
    assert_eq!(wa.manifest.len(), 4);
    assert_eq!(wa.record.material, atlas_ids(id)[3]);
    assert_eq!(wa.record.size, [a.width, a.height]);
}

#[test]
fn a_written_atlas_loads_as_an_ordinary_material_and_textures() {
    let meshes = two_textured_boxes();
    let source = BakeSource::new(&meshes).expect("source");
    let baked = bake_proxy_atlas(&proxy_of(&meshes), &[&source], settings(4.0)).expect("bake");
    let dir = regia_test_utils::unique_temp_dir("hlod_atlas_write");
    let id =
        regia_asset::AssetId::sub_asset(regia_asset::AssetId(regia_core::id::Guid::from_u128(5)), "hlod/3");
    for platform in ["linux", "android"] {
        let written = write_atlas(id, &baked, platform, Some(&dir), "a.level.ron", 3).expect("write");
        let [base, normal, orm, material] = atlas_ids(id);
        let read = |id| std::fs::read(dir.join(regia_asset::cooked_file_name(id))).expect("file");
        let (_, cooked) = regia_asset::load_material_bytes(&read(material)).expect("material");
        assert_eq!(
            cooked.authored.base.base_color_map,
            Some(regia_asset::TextureSlot::Asset(base))
        );
        assert_eq!(
            cooked.authored.base.normal_map,
            Some(regia_asset::TextureSlot::Asset(normal))
        );
        assert_eq!(
            cooked.authored.base.orm_map,
            Some(regia_asset::TextureSlot::Asset(orm))
        );
        assert!(cooked.authored.base.orm_packed);
        for texture in [base, normal, orm] {
            let (_, t) = regia_asset::load_texture_bytes(&read(texture)).expect("texture");
            assert_eq!([t.width, t.height], [baked.width, baked.height]);
            // The renderer's decoder reads it back (BC7 on desktop, ASTC on Android).
            let decoded = regia_render::CompressedTexture::from_cooked(&t)
                .expect("ktx2")
                .decode_rgba8()
                .expect("decodes");
            assert_eq!(decoded.rgba.len(), baked.base_color.len());
        }
        assert_eq!(written.manifest[3].path, "a.level.ron#hlod/3/atlas/material");
    }
    // The compressed base color still holds the source colors (BC7 / ASTC error is small).
    let (_, t) = regia_asset::load_texture_bytes(
        &std::fs::read(dir.join(regia_asset::cooked_file_name(atlas_ids(id)[0]))).expect("file"),
    )
    .expect("texture");
    let decoded = regia_render::CompressedTexture::from_cooked(&t)
        .expect("ktx2")
        .decode_rgba8()
        .expect("decodes");
    let p = Vec3::new(-5.0, 2.0, 0.0);
    let got = atlas_at(&baked, &decoded.rgba, p, decode_srgb);
    assert!(
        (got[0] - 1.0).abs() < 0.06 && got[1] < 0.06 && got[2] < 0.06,
        "{got:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn vertex_colors_bake_without_a_material_and_bad_settings_are_refused() {
    let cube = MeshSoup::builtin("builtin:cube")
        .expect("cube")
        .tinted([0.2, 0.4, 0.6, 1.0]);
    let meshes = vec![(placed(3.0, Vec3::ZERO), Arc::new(cube))];
    let source = BakeSource::new(&meshes).expect("source");
    let proxy = proxy_of(&meshes);
    let baked = bake_proxy_atlas(&proxy, &[&source], settings(4.0)).expect("bake");
    let got = atlas_at(&baked, &baked.base_color, Vec3::new(0.2, 1.5, -0.3), decode_srgb);
    for (g, w) in got.iter().zip([0.2, 0.4, 0.6]) {
        assert!((g - w).abs() < 0.01, "{got:?}");
    }
    let err = bake_proxy_atlas(&proxy, &[&source], settings(0.0)).expect_err("zero density");
    assert_eq!(err.code, "hlod_atlas_settings");
    let err = bake_proxy_atlas(&MeshSoup::default(), &[&source], settings(4.0)).expect_err("empty");
    assert_eq!(err.code, "hlod_atlas_empty");
    assert!(BakeSource::new(&[]).is_none());
}

#[test]
fn a_tilted_source_normal_map_tilts_the_baked_normal_toward_the_same_world_side() {
    // A plane whose normal map leans toward +U. The baked atlas normal, turned back to world space
    // through the proxy's own UV frame, leans toward the source's +U world direction.
    let plane = MeshSoup::builtin("builtin:plane").expect("plane");
    let lean = [218u8, 128, 218, 255]; // about (0.71, 0, 0.71)
    let source_mesh = vec![(
        placed(4.0, Vec3::ZERO),
        Arc::new(plane.clone().with_material(Some(Arc::new(BakeMaterial {
            normal_map: Some(solid(lean)),
            ..BakeMaterial::default()
        })))),
    )];
    let source = BakeSource::new(&source_mesh).expect("source");
    let mut proxy = MeshSoup::default();
    proxy.append(&plane, &placed(4.0, Vec3::ZERO));
    let baked = bake_proxy_atlas(&proxy, &[&source], settings(4.0)).expect("bake");
    // The source's +U direction in world space: along +X for the built-in plane.
    let world = world_normal_at(&baked, Vec3::new(0.4, 0.0, 0.3));
    assert!(world.x > 0.6 && world.y > 0.6, "{world}");
    assert!(world.z.abs() < 0.1, "{world}");
}

/// A world normal as the runtime reads it: the vertex tangents `regia_render` generates for a cooked
/// mesh, the shaders' `b = cross(n, t) * w`, and an RGB-encoded tangent-space sample.
fn runtime_world_normal(mesh: &MeshSoup, p: Vec3, encoded: [f32; 3]) -> Vec3 {
    let mut tangents = vec![[1.0, 0.0, 0.0, 1.0]; mesh.positions.len()];
    assert!(regia_render::generate_vertex_tangents(
        &mesh.positions,
        &mesh.normals,
        &mesh.uvs,
        &mesh.indices,
        &mut tangents,
    ));
    let local = encoded.map(|v| v * 2.0 - 1.0);
    for tri in mesh.indices.as_chunks::<3>().0 {
        let v = tri.map(|i| Vec3::from_array(mesh.positions[i as usize]));
        let Some(w) = on_triangle(p, v) else { continue };
        let mix = |a: [Vec3; 3]| a[0] * w[0] + a[1] * w[1] + a[2] * w[2];
        let n = mix(tri.map(|i| Vec3::from_array(mesh.normals[i as usize]))).normalize();
        let t = mix(tri.map(|i| Vec3::from_slice(&tangents[i as usize][..3]))).normalize();
        let b = n.cross(t) * tangents[tri[0] as usize][3];
        return (t * local[0] + b * local[1] + n * local[2]).normalize();
    }
    panic!("{p} is not on the mesh");
}

/// The proxy's atlas normal at `p`, read back as the runtime reads it.
fn baked_runtime_normal(baked: &BakedAtlas, p: Vec3) -> Vec3 {
    let sample = atlas_at(baked, &baked.normal, p, decode_unorm);
    runtime_world_normal(&baked.mesh, p, [sample[0], sample[1], sample[2]])
}

/// A source normal map reads the same on the proxy as on the source, through the frames the runtime
/// draws with, on both tangent-space axes (the bake's own frame cannot catch a flipped axis).
#[test]
fn a_mapped_normal_reads_back_through_the_runtime_tangent_frame() {
    let plane = MeshSoup::builtin("builtin:plane").expect("plane");
    let pose = placed(4.0, Vec3::ZERO);
    let mut world_plane = MeshSoup::default();
    world_plane.append(&plane, &pose);
    world_plane.uvs = plane.uvs.clone();
    let point = Vec3::new(0.4, 0.0, 0.3);
    for lean in [
        [218u8, 128, 218, 255],
        [128, 218, 218, 255],
        [38, 128, 218, 255],
        [128, 38, 218, 255],
    ] {
        let source_mesh = vec![(
            pose,
            Arc::new(plane.clone().with_material(Some(Arc::new(BakeMaterial {
                normal_map: Some(solid(lean)),
                ..BakeMaterial::default()
            })))),
        )];
        let source = BakeSource::new(&source_mesh).expect("source");
        let mut proxy = MeshSoup::default();
        proxy.append(&plane, &pose);
        let baked = bake_proxy_atlas(&proxy, &[&source], settings(4.0)).expect("bake");
        let encoded = [lean[0], lean[1], lean[2]].map(|v| f32::from(v) / 255.0);
        let on_source = runtime_world_normal(&world_plane, point, encoded);
        let on_proxy = baked_runtime_normal(&baked, point);
        assert!(
            on_proxy.dot(on_source) > 0.99,
            "map {lean:?}: {on_proxy} on the proxy, {on_source} on the source"
        );
    }
    // The built-in plane's V runs along +Z: a map's +Y leans up the image, toward -Z.
    let up = runtime_world_normal(&world_plane, point, [0.5, 218.0 / 255.0, 218.0 / 255.0]);
    assert!(up.z < -0.6 && up.y > 0.6, "{up}");
}

/// A source surface that slopes away from a flat proxy lights the proxy as it lit the source: the
/// baked normal, read back through the runtime's frame, is the source's, whichever way it leans.
#[test]
fn a_sloped_source_bakes_its_own_normal_whichever_way_it_leans() {
    let plane = MeshSoup::builtin("builtin:plane").expect("plane");
    let pose = placed(4.0, Vec3::ZERO);
    let point = Vec3::new(0.4, 0.0, 0.3);
    for (sx, sz) in [
        (0.08f32, 0.0f32),
        (-0.08, 0.0),
        (0.0, 0.08),
        (0.0, -0.08),
        (0.08, -0.06),
    ] {
        let tilted = MeshSoup {
            positions: plane
                .positions
                .iter()
                .map(|p| [p[0] * 4.0, p[0] * 4.0 * sx + p[2] * 4.0 * sz, p[2] * 4.0])
                .collect(),
            normals: vec![Vec3::new(-sx, 1.0, -sz).normalize().to_array(); plane.positions.len()],
            ..plane.clone()
        };
        let source_mesh = vec![(Affine3A::IDENTITY, Arc::new(tilted))];
        let source = BakeSource::new(&source_mesh).expect("source");
        let mut proxy = MeshSoup::default();
        proxy.append(&plane, &pose);
        let baked = bake_proxy_atlas(&proxy, &[&source], settings(8.0)).expect("bake");
        let expected = Vec3::new(-sx, 1.0, -sz).normalize();
        let read = baked_runtime_normal(&baked, point);
        assert!(
            read.dot(expected) > 0.999,
            "slope ({sx}, {sz}): read {read}, the source's {expected}"
        );
    }
}

/// A cutout material's holes are not surface: a texel over a hole takes its neighbors' color and is
/// counted as a miss; the same texture drawn whole bakes the hole's color.
#[test]
fn a_cutout_hole_is_filled_from_the_surface_around_it() {
    // Green leaf, with a transparent black square in the middle of the texture.
    let n = 32u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let hole = (12..20).contains(&x) && (12..20).contains(&y);
            rgba.extend_from_slice(if hole { &[0, 0, 0, 0] } else { &[40, 200, 40, 255] });
        }
    }
    let leaf = Arc::new(BakeImage::new(n, n, rgba).expect("image"));
    let plane = MeshSoup::builtin("builtin:plane").expect("plane");
    let pose = placed(4.0, Vec3::ZERO);
    let bake = |alpha_cutoff| {
        let source_mesh = vec![(
            pose,
            Arc::new(plane.clone().with_material(Some(Arc::new(BakeMaterial {
                base_color_map: Some(Arc::clone(&leaf)),
                alpha_cutoff,
                ..BakeMaterial::default()
            })))),
        )];
        let source = BakeSource::new(&source_mesh).expect("source");
        let mut proxy = MeshSoup::default();
        proxy.append(&plane, &pose);
        bake_proxy_atlas(&proxy, &[&source], settings(8.0)).expect("bake")
    };
    // The middle of the plane is the middle of the texture.
    let middle = Vec3::new(0.0, 0.0, 0.0);
    let whole = bake(None);
    let cut = bake(Some(0.5));
    let green = |baked: &BakedAtlas| atlas_at(baked, &baked.base_color, middle, decode_srgb)[1];
    assert!(
        green(&whole) < 0.05,
        "drawn whole, the hole bakes black: {}",
        green(&whole)
    );
    assert_eq!(whole.miss_ratio, 0.0);
    assert!(cut.miss_ratio > 0.01, "{}", cut.miss_ratio);
    assert!(
        green(&cut) > 0.3,
        "the hole takes the leaf's color: {}",
        green(&cut)
    );
}

// -- ADR-2713: cutout continuation, coverage, the source index, material graphs -------------------

/// A green leaf texture with a transparent square in its middle (texture 12..20 of 32).
fn leaf_with_hole() -> Arc<BakeImage> {
    let n = 32u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let hole = (12..20).contains(&x) && (12..20).contains(&y);
            rgba.extend_from_slice(if hole { &[0, 0, 0, 0] } else { &[40, 200, 40, 255] });
        }
    }
    Arc::new(BakeImage::new(n, n, rgba).expect("image"))
}

/// The raw alpha byte of the base color the proxy draws at `p`.
fn alpha_at(baked: &BakedAtlas, p: Vec3) -> u8 {
    let uv = uv_at(&baked.mesh, &Affine3A::IDENTITY, p).expect("on the proxy");
    let x = ((uv[0] * baked.width as f32) as u32).min(baked.width - 1);
    let y = ((uv[1] * baked.height as f32) as u32).min(baked.height - 1);
    baked.base_color[((y * baked.width + x) * 4 + 3) as usize]
}

/// A 4 m cutout card 0.3 m in front of a 4 m blue wall: the proxy is the card. Through the card's
/// hole the bake continues to the wall, so the texels there take the wall's color; the hole's
/// coverage is zero; the leaf keeps its own color and full coverage. The same holds with the card
/// and the wall in two cells (two sources), and the written material is alpha-masked.
#[test]
fn a_cutout_card_in_front_of_a_wall_bakes_the_wall_through_its_holes_with_zero_coverage() {
    let plane = MeshSoup::builtin("builtin:plane").expect("plane");
    let wall = (
        placed(4.0, Vec3::ZERO),
        Arc::new(plane.clone().with_material(Some(Arc::new(BakeMaterial {
            base_color_map: Some(solid([20, 40, 230, 255])),
            ..BakeMaterial::default()
        })))),
    );
    let card_pose = placed(4.0, Vec3::new(0.0, 0.3, 0.0));
    let card = (
        card_pose,
        Arc::new(plane.clone().with_material(Some(Arc::new(BakeMaterial {
            base_color_map: Some(leaf_with_hole()),
            alpha_cutoff: Some(0.5),
            ..BakeMaterial::default()
        })))),
    );
    let mut proxy = MeshSoup::default();
    proxy.append(&plane, &card_pose);
    let hole = Vec3::new(0.0, 0.3, 0.0);
    let leaf = Vec3::new(1.4, 0.3, -1.3);
    for split in [false, true] {
        let sources: Vec<BakeSource> = if split {
            vec![
                BakeSource::new(std::slice::from_ref(&card)).expect("card"),
                BakeSource::new(std::slice::from_ref(&wall)).expect("wall"),
            ]
        } else {
            vec![BakeSource::new(&[card.clone(), wall.clone()]).expect("cell")]
        };
        let refs: Vec<&BakeSource> = sources.iter().collect();
        let baked = bake_proxy_atlas(&proxy, &refs, settings(8.0)).expect("bake");
        let through = atlas_at(&baked, &baked.base_color, hole, decode_srgb);
        assert!(
            through[2] > 0.6 && through[1] < 0.1,
            "split {split}: the hole bakes the wall's blue, got {through:?}"
        );
        assert_eq!(
            alpha_at(&baked, hole),
            0,
            "split {split}: no coverage at the hole"
        );
        let own = atlas_at(&baked, &baked.base_color, leaf, decode_srgb);
        assert!(
            own[1] > 0.3 && own[2] < 0.1,
            "split {split}: the leaf is green, got {own:?}"
        );
        assert_eq!(alpha_at(&baked, leaf), 255, "split {split}: the leaf is covered");
        assert_eq!(
            baked.miss_ratio, 0.0,
            "split {split}: the wall is behind every hole"
        );
        assert!(baked.hole_ratio > 0.01, "split {split}: {}", baked.hole_ratio);
        assert!(baked.counts.cutout_steps > 0, "split {split}: {:?}", baked.counts);
    }

    // The written material cuts the holes out with the standard material's alpha mask.
    let sources = [BakeSource::new(&[card, wall]).expect("cell")];
    let baked = bake_proxy_atlas(&proxy, &[&sources[0]], settings(8.0)).expect("bake");
    let dir = regia_test_utils::unique_temp_dir("hlod_atlas_cutout");
    let id =
        regia_asset::AssetId::sub_asset(regia_asset::AssetId(regia_core::id::Guid::from_u128(9)), "hlod/0");
    let written = write_atlas(id, &baked, "linux", Some(&dir), "a.level.ron", 0).expect("write");
    let read = |id| std::fs::read(dir.join(regia_asset::cooked_file_name(id))).expect("file");
    let (_, cooked) = regia_asset::load_material_bytes(&read(atlas_ids(id)[3])).expect("material");
    assert_eq!(
        cooked.authored.render_state.alpha_mode,
        regia_asset::AlphaMode::Mask
    );
    assert_eq!(cooked.authored.render_state.alpha_cutoff, PROXY_ALPHA_CUTOFF);
    // The texture bytes are the three texture files, exactly.
    let on_disk: u64 = atlas_ids(id)[..3].iter().map(|t| read(*t).len() as u64).sum();
    assert_eq!(written.texture_bytes, on_disk);
    assert!(
        written.bytes > written.texture_bytes,
        "the material file is the rest"
    );
    // The compressed base color keeps the hole (BC7 holds alpha).
    let (_, t) = regia_asset::load_texture_bytes(&read(atlas_ids(id)[0])).expect("texture");
    let decoded = regia_render::CompressedTexture::from_cooked(&t)
        .expect("ktx2")
        .decode_rgba8()
        .expect("decodes");
    let uv = uv_at(&baked.mesh, &Affine3A::IDENTITY, hole).expect("on the proxy");
    let (x, y) = (
        ((uv[0] * baked.width as f32) as u32).min(baked.width - 1),
        ((uv[1] * baked.height as f32) as u32).min(baked.height - 1),
    );
    assert!(decoded.rgba[((y * baked.width + x) * 4 + 3) as usize] < 32);
    std::fs::remove_dir_all(&dir).ok();
}

/// A cutout card with nothing behind it (an open canopy): the hole has zero coverage and its color
/// comes from the leaf around it, counted as a miss. An opaque proxy keeps alpha 255 everywhere and
/// an opaque material, as before ADR-2713.
#[test]
fn an_open_canopy_hole_has_zero_coverage_and_an_opaque_proxy_stays_opaque() {
    let plane = MeshSoup::builtin("builtin:plane").expect("plane");
    let pose = placed(4.0, Vec3::ZERO);
    let card = vec![(
        pose,
        Arc::new(plane.clone().with_material(Some(Arc::new(BakeMaterial {
            base_color_map: Some(leaf_with_hole()),
            alpha_cutoff: Some(0.5),
            ..BakeMaterial::default()
        })))),
    )];
    let source = BakeSource::new(&card).expect("source");
    let mut proxy = MeshSoup::default();
    proxy.append(&plane, &pose);
    let baked = bake_proxy_atlas(&proxy, &[&source], settings(8.0)).expect("bake");
    assert_eq!(alpha_at(&baked, Vec3::ZERO), 0);
    assert!(atlas_at(&baked, &baked.base_color, Vec3::ZERO, decode_srgb)[1] > 0.3);
    assert!(baked.miss_ratio > 0.01 && baked.hole_ratio >= baked.miss_ratio);

    let meshes = two_textured_boxes();
    let source = BakeSource::new(&meshes).expect("source");
    let opaque = bake_proxy_atlas(&proxy_of(&meshes), &[&source], settings(4.0)).expect("bake");
    assert_eq!(opaque.hole_ratio, 0.0);
    assert!(opaque.base_color.as_chunks::<4>().0.iter().all(|t| t[3] == 255));
    let dir = regia_test_utils::unique_temp_dir("hlod_atlas_opaque");
    let id =
        regia_asset::AssetId::sub_asset(regia_asset::AssetId(regia_core::id::Guid::from_u128(8)), "hlod/0");
    write_atlas(id, &opaque, "linux", Some(&dir), "a.level.ron", 0).expect("write");
    let bytes = std::fs::read(dir.join(regia_asset::cooked_file_name(atlas_ids(id)[3]))).expect("file");
    let (_, cooked) = regia_asset::load_material_bytes(&bytes).expect("material");
    assert_eq!(
        cooked.authored.render_state.alpha_mode,
        regia_asset::AlphaMode::default()
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `n` x `n` cells of `size` m: each a ground tile, a box and a sphere, vertex-colored by cell.
fn cell_grid(n: u32, size: f32) -> Vec<Vec<(Affine3A, Arc<MeshSoup>)>> {
    let plane = MeshSoup::builtin("builtin:plane").expect("plane");
    let cube = MeshSoup::builtin("builtin:cube").expect("cube");
    let sphere = MeshSoup::builtin("builtin:sphere").expect("sphere");
    let mut cells = Vec::new();
    for j in 0..n {
        for i in 0..n {
            let center = Vec3::new((i as f32 + 0.5) * size, 0.0, (j as f32 + 0.5) * size);
            let tint = [i as f32 / n as f32, j as f32 / n as f32, 0.5, 1.0];
            let colored = |mesh: &MeshSoup| {
                let mut mesh = mesh.clone();
                mesh.colors = vec![tint; mesh.positions.len()];
                Arc::new(mesh)
            };
            cells.push(vec![
                (placed(size, center), colored(&plane)),
                (
                    placed(size * 0.2, center + Vec3::new(size * 0.2, size * 0.1, 0.0)),
                    colored(&cube),
                ),
                (
                    placed(
                        size * 0.15,
                        center + Vec3::new(-size * 0.2, size * 0.1, size * 0.2),
                    ),
                    colored(&sphere),
                ),
            ]);
        }
    }
    cells
}

/// The proxy a high node bakes: every cell merged and simplified to about 256 triangles, as a
/// node keeps about a leaf's triangle count whatever its level (each merge of four children keeps
/// a quarter of their triangles by default).
fn parent_proxy(cells: &[Vec<(Affine3A, Arc<MeshSoup>)>]) -> MeshSoup {
    let mut merged = MeshSoup::default();
    for cell in cells {
        for (world, mesh) in cell {
            merged.append(mesh, world);
        }
    }
    let ratio = (256.0 / merged.triangles().max(1) as f32).min(1.0);
    merged.simplified(ratio, 64)
}

/// A parent over 64 cells bakes the same atlas through the source index as through the search
/// before it (every near cell's tree in turn, each with the full reach), with far less work per
/// query: the cell trees a query enters drop at least 5x and its box and triangle tests at least
/// 2x; the triangle tests alone do not grow. Each cell's own tree already pruned the triangles, and
/// its traversal (about 20 tests a query here) is a floor neither search goes under, so the total
/// gain grows with the cells (ADR-2713; 256 cells in the next test).
#[test]
fn a_parent_over_64_cells_bakes_the_same_atlas_with_far_fewer_tests_per_query() {
    let cells = cell_grid(8, 16.0);
    let sources: Vec<BakeSource> = cells.iter().map(|c| BakeSource::new(c).expect("cell")).collect();
    let refs: Vec<&BakeSource> = sources.iter().collect();
    let proxy = parent_proxy(&cells);
    let indexed = bake_proxy_atlas(&proxy, &refs, settings(0.5)).expect("indexed");
    let linear = bake_with(&proxy, &refs, settings(0.5), Some(Lookup::Linear)).expect("linear");
    assert_eq!(indexed.base_color, linear.base_color, "same colors");
    assert_eq!(indexed.normal, linear.normal, "same normals");
    assert_eq!(indexed.orm, linear.orm, "same ORM");
    assert_eq!(indexed.miss_ratio, linear.miss_ratio);
    assert_eq!(indexed.bake_error_m, linear.bake_error_m);
    let (a, b) = (linear.counts, indexed.counts);
    assert_eq!((a.rays, a.closest), (b.rays, b.closest), "the same queries");
    assert!(a.rays > 1000, "{a:?}");
    let per = |c: &BakeCounts, x: u64| x as f64 / (c.rays + c.closest) as f64;
    let cells_before = per(&a, a.cells_entered);
    let cells_after = per(&b, b.cells_entered);
    println!(
        "64 cells: cells entered per query {cells_before:.2} -> {cells_after:.2}; box+triangle tests per query \
         {:.2} -> {:.2}; triangles per query {:.2} -> {:.2}",
        a.tests_per_query(),
        b.tests_per_query(),
        a.triangles_per_query(),
        b.triangles_per_query()
    );
    assert!(
        cells_before >= 5.0 * cells_after,
        "{cells_before} vs {cells_after}"
    );
    assert!(
        a.tests_per_query() >= 2.0 * b.tests_per_query(),
        "{} vs {}",
        a.tests_per_query(),
        b.tests_per_query()
    );
    assert!(b.tree.triangles <= a.tree.triangles, "{a:?} vs {b:?}");
}

/// The index's work per query stays about flat as a parent covers more cells, where the search
/// before it grew with every cell: 16 cells against 256, where the index does at least 5x less.
#[test]
fn the_source_index_keeps_the_work_per_query_flat_as_cells_grow() {
    let run = |n: u32| {
        let cells = cell_grid(n, 16.0);
        let sources: Vec<BakeSource> = cells.iter().map(|c| BakeSource::new(c).expect("cell")).collect();
        let refs: Vec<&BakeSource> = sources.iter().collect();
        let proxy = parent_proxy(&cells);
        // The same texel count whatever the size: density falls as the side grows.
        let density = 32.0 / (n as f32 * 16.0);
        let indexed = bake_proxy_atlas(&proxy, &refs, settings(density)).expect("indexed");
        let index = SourceIndex::build(&refs).expect("index");
        assert!(
            index.node_count() < refs.len() * 2,
            "a binary tree over the cells"
        );
        let linear = bake_with(&proxy, &refs, settings(density), Some(Lookup::Linear)).expect("linear");
        (linear.counts.tests_per_query(), indexed.counts.tests_per_query())
    };
    let (small_before, small_after) = run(4);
    let (big_before, big_after) = run(16);
    println!("16 cells: {small_before:.1} -> {small_after:.1}; 256 cells: {big_before:.1} -> {big_after:.1}");
    assert!(
        big_before > 4.0 * small_before,
        "the old search grows with the cells"
    );
    assert!(big_after < 2.0 * small_after, "the index does not");
    assert!(
        big_before >= 5.0 * big_after,
        "at 256 cells: {big_before} vs {big_after}"
    );
}

/// Same bake, same bytes, through the index (determinism holds with it).
#[test]
fn the_indexed_bake_is_deterministic() {
    let cells = cell_grid(4, 16.0);
    let sources: Vec<BakeSource> = cells.iter().map(|c| BakeSource::new(c).expect("cell")).collect();
    let refs: Vec<&BakeSource> = sources.iter().collect();
    let proxy = parent_proxy(&cells);
    let a = bake_proxy_atlas(&proxy, &refs, settings(1.0)).expect("a");
    let b = bake_proxy_atlas(&proxy, &refs, settings(1.0)).expect("b");
    assert_eq!(a, b);
}

/// A plane with every vertex colored `rgba`.
fn colored_plane(rgba: [f32; 4]) -> MeshSoup {
    let mut plane = MeshSoup::builtin("builtin:plane").expect("plane");
    plane.colors = vec![rgba; plane.positions.len()];
    plane
}

const TINT_GRAPH: &str = r#"(
    format_version: 2,
    params: [(name: "tint", value: Color((0.8, 0.2, 0.1))), (name: "rough", value: Float(0.9))],
    nodes: {
        "tint": Param("tint"),
        "rough": Param("rough"),
        "half": Float(0.5),
        "metal": Mul("half", "half"),
    },
    output: (base_color: Some("tint"), roughness: Some("rough"), metallic: Some("metal")),
)"#;

/// A material graph bakes its folded color, not the mesh's vertex color: a graph whose base color
/// is a parameter ignores a green vertex color; one that multiplies the vertex color in takes it;
/// an instance's value wins over the default; and a texture sample is named as approximated.
#[test]
fn a_material_graph_bakes_its_folded_color_not_the_vertex_color() {
    let graph = regia_matgraph::MaterialGraph::from_ron(TINT_GRAPH).expect("graph");
    let folded = bake_material_from_graph(&graph, None);
    assert!(folded.approximated.is_empty(), "{:?}", folded.approximated);
    assert!(!folded.material.vertex_color);
    assert_eq!(folded.material.base_color, [0.8, 0.2, 0.1, 1.0]);
    assert_eq!(folded.material.roughness, 0.9);
    assert_eq!(folded.material.metallic, 0.25);

    let green = [0.0, 1.0, 0.0, 1.0];
    let pose = placed(4.0, Vec3::ZERO);
    let bake = |material: BakeMaterial| {
        let plane = colored_plane(green);
        let meshes = vec![(
            pose,
            Arc::new(plane.clone().with_material(Some(Arc::new(material)))),
        )];
        let source = BakeSource::new(&meshes).expect("source");
        let mut proxy = MeshSoup::default();
        proxy.append(&plane, &pose);
        let baked = bake_proxy_atlas(&proxy, &[&source], settings(4.0)).expect("bake");
        let rgba = atlas_at(&baked, &baked.base_color, Vec3::new(0.3, 0.0, 0.2), decode_srgb);
        let orm = atlas_at(&baked, &baked.orm, Vec3::new(0.3, 0.0, 0.2), decode_unorm);
        (rgba, orm)
    };
    let (color, orm) = bake(folded.material.clone());
    for (c, want) in color.iter().zip([0.8, 0.2, 0.1]) {
        assert!(
            (c - want).abs() < 0.02,
            "the graph's tint, not the vertex green: {color:?}"
        );
    }
    assert!(
        (orm[1] - 0.9).abs() < 0.01 && (orm[2] - 0.25).abs() < 0.01,
        "{orm:?}"
    );

    // A graph that multiplies the vertex color in takes it.
    let mut times_vertex = graph.clone();
    times_vertex
        .nodes
        .insert("vcol".into(), regia_matgraph::Node::VertexColor);
    times_vertex.nodes.insert(
        "rgb".into(),
        regia_matgraph::Node::Swizzle {
            input: "vcol".into(),
            channels: "rgb".into(),
        },
    );
    times_vertex.nodes.insert(
        "base".into(),
        regia_matgraph::Node::Mul("tint".into(), "rgb".into()),
    );
    times_vertex.output.base_color = Some("base".into());
    let folded = bake_material_from_graph(&times_vertex, None);
    assert!(folded.material.vertex_color);
    let (color, _) = bake(folded.material);
    assert!(
        color[0] < 0.02 && (color[1] - 0.2).abs() < 0.02,
        "tint x green: {color:?}"
    );

    // An instance's value wins.
    let mut instance = regia_matgraph::MaterialInstance::new("assets/tint.matgraph.ron");
    instance.set("tint", regia_matgraph::ParamValue::Color([0.1, 0.1, 0.9]));
    let folded = bake_material_from_graph(&graph, Some(&instance));
    assert_eq!(folded.material.base_color, [0.1, 0.1, 0.9, 1.0]);

    // A texture sample reads white and is named.
    let mut sampled = graph.clone();
    sampled.nodes.insert("uv".into(), regia_matgraph::Node::TexCoord);
    sampled.nodes.insert(
        "tex".into(),
        regia_matgraph::Node::Sample {
            texture: "albedo".into(),
            uv: "uv".into(),
            lod: None,
        },
    );
    sampled.nodes.insert(
        "lit".into(),
        regia_matgraph::Node::Mul("tint".into(), "tex".into()),
    );
    sampled.output.base_color = Some("lit".into());
    let folded = bake_material_from_graph(&sampled, None);
    // The sample is not evaluated, so neither is its UV input.
    assert_eq!(folded.approximated, vec!["tex".to_owned()]);
    assert_eq!(&folded.material.base_color[..3], &[0.8, 0.2, 0.1]);
}

/// Graph and instance sources read from a project; anything else, or a broken file, is `None`.
#[test]
fn graph_and_instance_sources_read_from_the_project() {
    let root = regia_test_utils::unique_temp_dir("hlod_atlas_graph");
    std::fs::create_dir_all(root.join("assets")).expect("dir");
    std::fs::write(root.join("assets/tint.matgraph.ron"), TINT_GRAPH).expect("graph");
    let mut instance = regia_matgraph::MaterialInstance::new("assets/tint.matgraph.ron");
    instance.set("tint", regia_matgraph::ParamValue::Color([0.0, 0.5, 1.0]));
    std::fs::write(
        root.join("assets/blue.matinst.ron"),
        instance.to_ron().expect("ron"),
    )
    .expect("instance");
    std::fs::write(root.join("assets/broken.matgraph.ron"), "(nope").expect("broken");
    let graph = read_graph_bake_material(&root, "assets/tint.matgraph.ron").expect("graph");
    assert_eq!(graph.base_color, [0.8, 0.2, 0.1, 1.0]);
    let inst = read_graph_bake_material(&root, "assets/blue.matinst.ron").expect("instance");
    assert_eq!(inst.base_color, [0.0, 0.5, 1.0, 1.0]);
    assert!(read_graph_bake_material(&root, "assets/broken.matgraph.ron").is_none());
    assert!(read_graph_bake_material(&root, "assets/missing.matgraph.ron").is_none());
    assert!(read_graph_bake_material(&root, "assets/x.material.ron").is_none());
    assert!(read_graph_bake_material(&root, "../outside.matgraph.ron").is_none());
    std::fs::remove_dir_all(&root).ok();
}
