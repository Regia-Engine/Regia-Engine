// SPDX-License-Identifier: MPL-2.0
//! P-17: custom texture acceptance, without image goldens.

use std::path::Path;
use std::sync::Arc;

use regia_asset::AssetId;
use regia_asset::{AssetTypeTag, CookedHeader, CookedImage, encode_cooked_file};
use regia_core::math::Vec3;
use regia_vfx::{DecalFrame, DecalLibrary, DecalPrefab, DecalScene, VfxCamera, VfxProfile, project_pixel};

static GPU_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn prefab(texture: &str) -> DecalPrefab {
    DecalPrefab::from_ron(&format!(
        "(version: 3, name: \"custom\", half_size: (1.0, 1.0, 0.2), texture: Some({texture:?}),
        edge_softness: 0.0, angle_fade: (0.0, 0.0), blend: \"replace\")"
    ))
    .expect("v3 custom texture prefab must load")
}

fn image() -> CookedImage {
    // Left: opaque red. Right: transparent blue. Coverage must not paint blue.
    CookedImage {
        width: 2,
        height: 2,
        rgba8: vec![255, 0, 0, 255, 0, 0, 255, 0, 255, 0, 0, 255, 0, 0, 255, 0],
    }
}

fn write_image(path: &Path, image: &CookedImage) {
    let payload = image.to_postcard().expect("image payload");
    let header = CookedHeader {
        format_version: regia_asset::COOKED_FORMAT_VERSION,
        type_tag: AssetTypeTag::image().0,
        asset_id: AssetId::NIL,
        importer_version: 1,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    let bytes = encode_cooked_file(&header, &payload).expect("cooked image");
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).expect("write image");
    std::fs::rename(tmp, path).expect("publish image");
}

fn pixel(pixels: &[u8], frame: &DecalFrame, position: Vec3) -> [u8; 4] {
    let (x, y) = project_pixel(&frame.camera, frame.width, frame.height, position).expect("in frame");
    pixels[((y * frame.width + x) * 4) as usize..][..4]
        .try_into()
        .expect("pixel")
}

#[test]
fn custom_decal_texture_draws() {
    let _lock = GPU_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // Parse before making a device: the original fail-first check fails here on v2.
    let custom = prefab("paint.fckd");
    let dir = regia_test_utils::TempDir::new("custom-decal");
    write_image(&dir.join("paint.fckd"), &image());
    std::fs::write(dir.join("paint.decal.ron"), custom.to_ron().expect("ron")).expect("prefab");
    let mut library = DecalLibrary::builtin();
    assert_eq!(library.load_dir(&dir).expect("library with image"), 1);
    let device = regia_render::RenderDevice::headless().expect("lavapipe device");
    let mut scene = DecalScene::new(
        &device,
        VfxProfile {
            particle_capacity: 64,
            decal_capacity: 16,
            ..VfxProfile::default()
        },
    )
    .expect("scene");
    let mut frame = DecalFrame {
        camera: VfxCamera::looking_at(Vec3::new(0.0, 5.0, 6.0), Vec3::ZERO),
        ..DecalFrame::default()
    };
    let bare = scene.render(&device, &frame).expect("bare ground");
    let built_in = library
        .get("scorch")
        .expect("built-in")
        .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
        .expect("built-in place");
    frame.decals = vec![built_in];
    let original_builtin = scene.render(&device, &frame).expect("original built-in");
    scene.set_decal_texture_atlas(Arc::clone(library.texture_atlas()));
    let extended_builtin = scene.render(&device, &frame).expect("built-in in extended atlas");
    assert_eq!(
        original_builtin, extended_builtin,
        "custom pages must not alter built-ins"
    );
    frame.decals = vec![
        library
            .get("custom")
            .expect("custom")
            .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
            .expect("resolved placement"),
    ];
    assert_eq!(frame.decals[0].1.atlas_layer, 4);
    let drawn = scene.render(&device, &frame).expect("custom texture draw");
    let red = pixel(&drawn, &frame, Vec3::new(-0.5, 0.0, 0.0));
    assert!(
        red[0] > red[1] + 25 && red[0] > red[2] + 25,
        "opaque half must be red: {red:?}"
    );
    let transparent = Vec3::new(0.5, 0.0, 0.0);
    assert_eq!(
        pixel(&drawn, &frame, transparent),
        pixel(&bare, &frame, transparent),
        "zero alpha preserves ground"
    );
    frame.decals[0].1.receiver_mask = regia_vfx::LAYER_CHARACTER;
    assert_eq!(
        scene.render(&device, &frame).expect("masked"),
        bare,
        "custom texture still respects receiver masks"
    );
}

#[test]
fn custom_images_are_transactional_and_keep_stable_layers() {
    let mut library = DecalLibrary::builtin();
    let before = Arc::clone(library.texture_atlas());
    let invalid = CookedImage {
        width: 0,
        height: 1,
        rgba8: Vec::new(),
    };
    assert!(library.add_textured(prefab("bad.fckd"), &invalid).is_err());
    assert!(Arc::ptr_eq(&before, library.texture_atlas()));
    assert!(library.get("custom").is_none());
    let img = image();
    library.add_textured(prefab("a.fckd"), &img).expect("first");
    let (_, surface) = library
        .get("custom")
        .expect("custom")
        .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
        .expect("place");
    assert_eq!(surface.atlas_layer, 4);
    let old = Arc::clone(library.texture_atlas());
    let changed = CookedImage {
        width: 1,
        height: 1,
        rgba8: vec![0, 255, 0, 255],
    };
    library
        .add_textured(prefab("a.fckd"), &changed)
        .expect("replace same layer");
    assert_eq!(library.texture_atlas().layers(), 5);
    assert_ne!(old.rgba(), library.texture_atlas().rgba());
    assert_eq!(&library.texture_atlas().rgba()[..4 * 64 * 64 * 4], before.rgba());
    for path in ["b.fckd", "c.fckd", "d.fckd"] {
        library.add_textured(prefab(path), &img).expect("remaining layer");
    }
    let full = Arc::clone(library.texture_atlas());
    assert!(library.add_textured(prefab("overflow.fckd"), &img).is_err());
    assert!(Arc::ptr_eq(&full, library.texture_atlas()));
    assert_eq!(
        library.get("custom").expect("old preserved").texture.as_deref(),
        Some("d.fckd")
    );
    let forward = regia_vfx::decal_forward::forward_copy(
        &library
            .get("custom")
            .expect("custom")
            .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
            .expect("place")
            .0,
        &surface,
    );
    assert_eq!(forward.atlas_layer, Some(4));
    assert!(
        !forward.normal_page && !forward.material_page,
        "custom color cannot invent normal/material maps"
    );
    assert!(library.texture_atlas().forward_atlas().validate().is_ok());
}

#[test]
fn failed_directory_load_keeps_prefabs_and_atlas() {
    let dir = regia_test_utils::TempDir::new("decal-transaction");
    write_image(&dir.join("good.fckd"), &image());
    std::fs::write(
        dir.join("a.decal.ron"),
        prefab("good.fckd").to_ron().expect("ron"),
    )
    .expect("first");
    std::fs::write(
        dir.join("z.decal.ron"),
        prefab("missing.fckd").to_ron().expect("ron"),
    )
    .expect("second");
    let mut library = DecalLibrary::builtin();
    let before = Arc::clone(library.texture_atlas());
    assert!(library.load_dir(&dir).is_err());
    assert!(library.get("custom").is_none());
    assert!(Arc::ptr_eq(&before, library.texture_atlas()));
}

#[test]
fn old_versions_migrate_without_textures_and_unresolved_images_refuse_placement() {
    for version in [1, 2, 3] {
        let old = format!(
            "// preserve\n(version: {version}, name: \"s\", half_size: (1.0, 1.0, 0.2), atlas: \"pad\")"
        );
        let migrated = regia_vfx::decal_format::migrate_decal_ron(&old).expect("migrate");
        assert!(migrated.starts_with("// preserve"));
        let loaded = DecalPrefab::from_ron(&migrated).expect("old prefab");
        assert_eq!(loaded.version, 3);
        assert!(loaded.texture.is_none());
        assert_eq!(
            loaded
                .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
                .expect("place")
                .1
                .atlas_layer,
            1
        );
        assert_eq!(
            regia_vfx::decal_format::migrate_decal_ron(&migrated).expect("idempotent"),
            migrated
        );
    }
    assert!(
        prefab("unresolved.fckd")
            .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
            .is_err()
    );
    for path in [
        "",
        "../image.fckd",
        "/image.fckd",
        "C:/image.fckd",
        "a\\image.fckd",
        "a//b",
        "./b",
    ] {
        let text = format!("(version: 3, name: \"x\", half_size: (1.0, 1.0, 1.0), texture: Some({path:?}))");
        assert!(DecalPrefab::from_ron(&text).is_err(), "reject {path:?}");
    }
}

#[cfg(unix)]
#[test]
fn image_symlinks_cannot_escape_the_prefab_directory() {
    let dir = regia_test_utils::TempDir::new("decal-root");
    let outside = regia_test_utils::TempDir::new("decal-outside");
    write_image(&outside.join("image.fckd"), &image());
    std::os::unix::fs::symlink(outside.join("image.fckd"), dir.join("escape.fckd")).expect("symlink");
    std::fs::write(
        dir.join("paint.decal.ron"),
        prefab("escape.fckd").to_ron().expect("ron"),
    )
    .expect("prefab");
    assert!(DecalLibrary::builtin().load_dir(&dir).is_err());
}

#[test]
fn custom_texture_forward_draws() {
    use regia_render::{
        DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
        PostSettings,
    };
    let _lock = GPU_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = regia_render::set_process_forward_decals(true);
    assert!(regia_render::process_forward_decals());
    let device = regia_render::RenderDevice::headless().expect("lavapipe device");
    let mut library = DecalLibrary::builtin();
    library
        .add_textured(prefab("paint.fckd"), &image())
        .expect("texture");
    let (decal, mut surface) = library
        .get("custom")
        .expect("prefab")
        .instantiate(Vec3::ZERO, Vec3::Z, 0.0, None)
        .expect("place on wall");
    surface.receiver_mask = regia_vfx::LAYER_STATIC_MESH;
    let forward = regia_vfx::decal_forward::forward_copy(&decal, &surface);
    assert_eq!(forward.atlas_layer, Some(4));
    let lab = LabScene {
        width: 128,
        height: 96,
        eye: [0.0, 0.0, 3.0],
        target: [0.0; 3],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: 1.0,
            metallic: 0.0,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: [
                ([-1.6, -1.6, 0.0], [0.0, 1.0]),
                ([1.6, -1.6, 0.0], [1.0, 1.0]),
                ([1.6, 1.6, 0.0], [1.0, 0.0]),
                ([-1.6, 1.6, 0.0], [0.0, 0.0]),
            ]
            .into_iter()
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], uv))
            .collect(),
            indices: vec![0, 1, 2, 0, 2, 3],
            ..LabMesh::unskinned()
        }],
    };
    let frame = lab.as_frame_scene_with_quality(FrameQuality {
        specular_aa: false,
        ..FrameQuality::LAB
    });
    let render = |draw: bool| {
        let mut renderer = FrameRenderer::new();
        renderer
            .set_forward_decal_atlas(&library.texture_atlas().forward_atlas())
            .expect("atlas upload");
        assert!(renderer.set_forward_decals(if draw { std::slice::from_ref(&forward) } else { &[] }));
        let mut pixels = Vec::new();
        for _ in 0..4 {
            pixels = renderer
                .render_offscreen(&device, &frame)
                .expect("forward draw")
                .pixels;
        }
        pixels
    };
    let bare = render(false);
    let drawn = render(true);
    let at =
        |pixels: &[u8], x: usize| -> [u8; 4] { pixels[(48 * 128 + x) * 4..][..4].try_into().expect("pixel") };
    let red = at(&drawn, 48);
    assert!(
        red[0] > red[1] + 20 && red[0] > red[2] + 20,
        "custom forward color: {red:?}"
    );
    assert_eq!(at(&drawn, 80), at(&bare, 80), "forward alpha preserves receiver");
}

#[test]
fn resolved_texture_state_is_not_part_of_the_wire_format() {
    let mut library = DecalLibrary::builtin();
    library
        .add_textured(prefab("paint.fckd"), &image())
        .expect("image");
    let text = library
        .get("custom")
        .expect("prefab")
        .to_ron()
        .expect("serialize");
    assert!(text.contains("texture: Some"));
    assert!(!text.contains("resolved_texture"));
    let restored = DecalPrefab::from_ron(&text).expect("parse without IO");
    assert!(
        restored.instantiate(Vec3::ZERO, Vec3::Y, 0.0, None).is_err(),
        "serialized data cannot forge a resolved layer"
    );
    let fingerprint = regia_vfx::decal_schema_fingerprint();
    assert!(fingerprint.split(',').any(|field| field == "texture"));
    assert!(!fingerprint.contains("resolved_texture"));
    println!(
        "decal v3 schema hash: {}",
        regia_asset::content_hash_hex(fingerprint.as_bytes())
    );
}

#[test]
fn custom_image_library_drives_static_and_event_decals() {
    use regia_app::App;
    use regia_vfx::{DecalPool, PlaceDecal, StaticDecal, VfxPlugin, VfxSettings, place_decal};
    let dir = regia_test_utils::TempDir::new("decal-app");
    write_image(&dir.join("paint.fckd"), &image());
    std::fs::write(
        dir.join("paint.decal.ron"),
        prefab("paint.fckd").to_ron().expect("ron"),
    )
    .expect("prefab");
    let mut app = App::new();
    app.insert_resource(VfxSettings {
        library_dir: Some(dir.to_path_buf()),
        ..VfxSettings::default()
    });
    app.add_plugins(VfxPlugin);
    app.update();
    app.world_mut().spawn((
        StaticDecal {
            prefab: "custom".into(),
            ..StaticDecal::default()
        },
        regia_scene::Transform::default(),
    ));
    assert_eq!(app.world().resource::<DecalPool>().len(), 1);
    let request = PlaceDecal {
        position: Vec3::ZERO,
        normal: Vec3::Y,
        yaw: 0.0,
        prefab: "custom".into(),
        lifetime: None,
    };
    assert!(place_decal(app.world_mut(), &request).is_some());
    let pool = app.world().resource::<DecalPool>();
    assert_eq!(pool.len(), 2);
    assert!(pool.surfaces().iter().all(|surface| surface.atlas_layer == 4));
    let before = Arc::clone(app.world().resource::<DecalLibrary>().texture_atlas());
    write_image(
        &dir.join("paint.fckd"),
        &CookedImage {
            width: 1,
            height: 1,
            rgba8: vec![0, 255, 0, 255],
        },
    );
    app.world_mut()
        .write_message(regia_asset::SourceAssetChanged::new(dir.join("paint.decal.ron")));
    app.update();
    let after = app.world().resource::<DecalLibrary>().texture_atlas();
    assert!(
        !Arc::ptr_eq(&before, after),
        "prefab reload republishes its image"
    );
    assert_eq!(after.layers(), 5, "reload reuses the layer");
    assert_eq!(app.world().resource::<DecalPool>().len(), 2);
    assert!(
        app.world()
            .resource::<DecalPool>()
            .surfaces()
            .iter()
            .all(|surface| surface.atlas_layer == 4)
    );
}
