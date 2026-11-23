// SPDX-License-Identifier: MPL-2.0
//! Texture-set materials (spec 22 T2.14, ADR-3320): a folder of PBR maps named
//! `<base>_<suffix>.<ext>` becomes one `.material.ron` per base name. Detection, conflicts,
//! the never-overwrite rule, the cook-time channel conventions, and the cooked material's
//! texture references are exercised end to end in temporary projects.

use std::path::{Path, PathBuf};

use image::ImageEncoder as _;
use regia_asset::{AlphaMode, AssetId, CookedStandardMaterial, MaterialAssetRon, MetaSidecar, TextureSlot};
use regia_asset_pipeline::texture_set::{LeftoverReason, TextureSetSettings, format_report, generate, scan};
use regia_asset_pipeline::{CookOptions, cook_project};
use regia_test_utils::unique_temp_dir;

/// Writes a 4×4 RGBA PNG with a fixed, asymmetric pixel pattern (channel values distinct so a
/// flip or swizzle is visible in every channel).
fn write_png(path: &Path) {
    let mut rgba = Vec::with_capacity(4 * 4 * 4);
    for y in 0..4u8 {
        for x in 0..4u8 {
            // R varies with x, G with y, B constant-ish: every channel distinguishable.
            rgba.extend_from_slice(&[x * 40 + 10, y * 50 + 20, 123, 255]);
        }
    }
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&rgba, 4, 4, image::ExtendedColorType::Rgba8)
        .expect("encode png");
    std::fs::write(path, bytes).expect("write png");
}

/// A temp project: `<root>/assets/sets/` with the named maps of one or more sets written.
fn project_with_maps(tag: &str, names: &[&str]) -> (PathBuf, PathBuf) {
    let root = unique_temp_dir(tag);
    let folder = root.join("assets/sets");
    std::fs::create_dir_all(&folder).expect("folder");
    for name in names {
        write_png(&folder.join(name));
    }
    (root, folder)
}

fn meta_id(source: &Path) -> AssetId {
    let bytes = std::fs::read(regia_asset::meta_path_for(source)).expect("meta");
    MetaSidecar::from_ron(&bytes).expect("meta parses").id
}

fn meta_settings(source: &Path) -> String {
    let bytes = std::fs::read(regia_asset::meta_path_for(source)).expect("meta");
    MetaSidecar::from_ron(&bytes)
        .expect("meta parses")
        .settings
        .expect("settings")
}

/// Decodes mip 0 of the cooked texture `id` in `root`'s host cache to RGBA8.
fn cooked_texture(root: &Path, id: AssetId) -> regia_asset::CookedTexture {
    let platform = regia_asset_pipeline::host_platform();
    let cache = regia_asset::cooked_cache_dir(root, platform);
    let bytes = std::fs::read(cache.join(regia_asset::cooked_file_name(id))).expect("cooked file");
    let (_, payload) = regia_asset::decode_cooked_file(&bytes).expect("cooked decodes");
    regia_asset::CookedTexture::from_postcard(payload).expect("cooked texture")
}

fn decode_level0(texture: &regia_asset::CookedTexture, kind: regia_asset::bcn_decode::BcnKind) -> Vec<u8> {
    let reader = ktx2::Reader::new(&texture.ktx2[..]).expect("ktx2");
    let header = reader.header();
    regia_asset::bcn_decode::decode_bcn(
        kind,
        header.pixel_width,
        header.pixel_height,
        reader.levels().next().expect("level 0").data,
    )
    .expect("bcn decodes")
}

#[test]
fn every_naming_variant_maps_to_its_slot() {
    let (_root, folder) = project_with_maps(
        "texset-slots",
        &[
            "hull_BaseColor.png",
            "hull_Normal.png",
            "hull_Roughness.png",
            "hull_Metallic.png",
            "hull_AO.png",
            "hull_Height.png",
            "hull_Emissive.png",
            "hull_Opacity.png",
            "wing_Albedo.png",
            "wing_N.png",
            "wing_ORM.png",
        ],
    );
    let scan = scan(&folder, false, &TextureSetSettings::default()).expect("scan");
    assert_eq!(scan.sets.len(), 2);
    assert!(scan.leftovers.is_empty(), "{:?}", scan.leftovers);
    let hull = scan.sets.iter().find(|s| s.name == "hull").expect("hull set");
    use regia_asset_pipeline::TextureSetSlot::*;
    for slot in [
        BaseColor,
        Normal,
        Roughness,
        Metallic,
        AmbientOcclusion,
        Height,
        Emissive,
        Opacity,
    ] {
        assert!(hull.slots.contains_key(&slot), "{slot:?} missing");
    }
    let wing = scan.sets.iter().find(|s| s.name == "wing").expect("wing set");
    assert!(wing.slots.contains_key(&BaseColor));
    assert!(wing.slots.contains_key(&Normal));
    assert!(wing.slots.contains_key(&Orm));
}

#[test]
fn a_conflict_is_reported_and_nothing_is_guessed() {
    let (_root, folder) = project_with_maps(
        "texset-conflict",
        &["rock_BaseColor.png", "rock_Albedo.png", "rock_Normal.png"],
    );
    let scan = scan(&folder, false, &TextureSetSettings::default()).expect("scan");
    assert_eq!(scan.sets.len(), 1);
    let set = &scan.sets[0];
    use regia_asset_pipeline::TextureSetSlot::*;
    assert!(!set.slots.contains_key(&BaseColor), "no winner is picked");
    assert_eq!(set.conflicts.len(), 1);
    assert_eq!(set.conflicts[0].slot, BaseColor);
    assert_eq!(set.conflicts[0].candidates.len(), 2);
    // Generation writes nothing for the contested slot but still writes the material.
    let outcomes = generate(&scan, &TextureSetSettings::default(), true).expect("generate");
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].written);
    assert!(!outcomes[0].textures.contains_key(&BaseColor));
    let material = MaterialAssetRon::from_ron(&std::fs::read(&outcomes[0].material_path).expect("material"))
        .expect("material parses");
    assert!(material.base.base_color_map.is_none());
    assert!(matches!(material.base.normal_map, Some(TextureSlot::Asset(_))));
}

#[test]
fn an_existing_material_is_untouched() {
    let (_root, folder) = project_with_maps("texset-exists", &["ship_BaseColor.png", "ship_Normal.png"]);
    let existing = folder.join("ship.material.ron");
    std::fs::write(&existing, "// artist's hand-written material\n").expect("material");

    let scan = scan(&folder, false, &TextureSetSettings::default()).expect("scan");
    let outcomes = generate(&scan, &TextureSetSettings::default(), true).expect("generate");
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].material_exists);
    assert!(!outcomes[0].written);
    assert_eq!(
        std::fs::read_to_string(&existing).expect("material"),
        "// artist's hand-written material\n"
    );
}

#[test]
fn leftovers_report_why_a_file_joined_no_set() {
    let (_root, folder) = project_with_maps(
        "texset-leftovers",
        &["solo_BaseColor.png", "rock_RM.png", "normal.png", "grass.png"],
    );
    let scan = scan(&folder, false, &TextureSetSettings::default()).expect("scan");
    assert_eq!(scan.sets.len(), 1, "only solo's base colour groups");
    let reasons: Vec<LeftoverReason> = scan.leftovers.iter().map(|l| l.reason.clone()).collect();
    assert!(reasons.contains(&LeftoverReason::CookOnlySuffix), "{reasons:?}");
    assert!(reasons.contains(&LeftoverReason::NoBaseName), "{reasons:?}");
    assert!(reasons.contains(&LeftoverReason::UnknownSuffix), "{reasons:?}");
    let text = format_report(&[], &scan.leftovers);
    assert!(text.contains("cook_only_suffix"), "{text}");
}

#[test]
fn the_generated_material_cooks_and_references_the_sets_texture_ids() {
    let (root, folder) = project_with_maps(
        "texset-cook",
        &[
            "canopy_BaseColor.png",
            "canopy_Normal.png",
            "canopy_ORM.png",
            "canopy_Emissive.png",
            "canopy_Opacity.png",
        ],
    );
    let scan = scan(&folder, false, &TextureSetSettings::default()).expect("scan");
    let outcomes = generate(&scan, &TextureSetSettings::default(), true).expect("generate");
    assert_eq!(outcomes.len(), 1);
    let outcome = &outcomes[0];
    assert!(outcome.written);
    assert_eq!(outcome.alpha_mode, AlphaMode::Mask);
    // The opacity map has no slot in the standard material; it set the alpha mode.
    assert_eq!(outcome.unreferenced.len(), 1);
    assert!(outcome.unreferenced[0].path.ends_with("canopy_Opacity.png"));

    // Cook the project; the material's slots must name the members' ids.
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let material_id = meta_id(&folder.join("canopy.material.ron"));
    let platform = regia_asset_pipeline::host_platform();
    let cache = regia_asset::cooked_cache_dir(&root, platform);
    let bytes =
        std::fs::read(cache.join(regia_asset::cooked_file_name(material_id))).expect("cooked material");
    let (_, payload) = regia_asset::decode_cooked_file(&bytes).expect("cooked decodes");
    let cooked = CookedStandardMaterial::from_postcard(payload).expect("cooked material decodes");
    let authored = cooked.authored;
    assert_eq!(
        authored.base.base_color_map,
        Some(TextureSlot::Asset(meta_id(&folder.join("canopy_BaseColor.png"))))
    );
    assert_eq!(
        authored.base.normal_map,
        Some(TextureSlot::Asset(meta_id(&folder.join("canopy_Normal.png"))))
    );
    assert_eq!(
        authored.base.orm_map,
        Some(TextureSlot::Asset(meta_id(&folder.join("canopy_ORM.png"))))
    );
    assert!(authored.base.orm_packed);
    assert_eq!(
        authored.base.emissive_map,
        Some(TextureSlot::Asset(meta_id(&folder.join("canopy_Emissive.png"))))
    );
    assert_eq!(authored.base.emissive, [1.0, 1.0, 1.0]);
    assert_eq!(authored.render_state.alpha_mode, AlphaMode::Mask);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_directx_flag_flips_green_at_cook() {
    // A DirectX-convention normal and an OpenGL reference under different base names:
    // same-slot members of one set would conflict, and the comparison needs both cooked.
    let (root, folder) = project_with_maps("texset-green", &["hull_NormalDX.png", "wing_Normal.png"]);
    let scan = scan(&folder, false, &TextureSetSettings::default()).expect("scan");
    generate(&scan, &TextureSetSettings::default(), true).expect("generate");
    // The generator recorded the DirectX convention in the member's meta.
    let settings = meta_settings(&folder.join("hull_NormalDX.png"));
    assert!(settings.contains("normal_green: Some(directx)"), "{settings}");
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    let dx = decode_level0(
        &cooked_texture(&root, meta_id(&folder.join("hull_NormalDX.png"))),
        regia_asset::bcn_decode::BcnKind::Bc5,
    );
    let gl = decode_level0(
        &cooked_texture(&root, meta_id(&folder.join("wing_Normal.png"))),
        regia_asset::bcn_decode::BcnKind::Bc5,
    );
    assert_eq!(dx.len(), gl.len());
    // The green channel is inverted (BC5 interpolates, so allow a quantization step of 1);
    // red is the same channel on both sides and must decode identically.
    for i in 0..(dx.len() / 4) {
        let (dxr, dxg) = (dx[i * 4], dx[i * 4 + 1]);
        let (glr, glg) = (gl[i * 4], gl[i * 4 + 1]);
        assert_eq!(dxr, glr, "red channel untouched at texel {i}");
        assert!(
            (i32::from(dxg) - i32::from(255 - glg)).abs() <= 1,
            "green channel flipped at texel {i}: {dxg} vs 255-{glg}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_rma_source_is_swizzled_to_orm_at_cook() {
    let (root, folder) =
        project_with_maps("texset-rma", &["mat_RMA.png", "ref_ORM.png", "mat_BaseColor.png"]);
    let scan = scan(&folder, false, &TextureSetSettings::default()).expect("scan");
    generate(&scan, &TextureSetSettings::default(), true).expect("generate");
    let settings = meta_settings(&folder.join("mat_RMA.png"));
    assert!(settings.contains("orm_channel_order: Some(rma)"), "{settings}");
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    // BC7's decode is lossy; compare with margins: the swizzle moved whole channels, so each
    // channel of RMA must track a different source channel of ORM. The source pixels are
    // (x*40+10, y*50+20, 123): swizzled RMA is (123, r, g).
    let rma = decode_level0(
        &cooked_texture(&root, meta_id(&folder.join("mat_RMA.png"))),
        regia_asset::bcn_decode::BcnKind::Bc7,
    );
    let orm = decode_level0(
        &cooked_texture(&root, meta_id(&folder.join("ref_ORM.png"))),
        regia_asset::bcn_decode::BcnKind::Bc7,
    );
    for i in 0..(rma.len() / 4) {
        let rma_px = &rma[i * 4..i * 4 + 3];
        let orm_px = &orm[i * 4..i * 4 + 3];
        // RMA's decoded R must track ORM's B (both are the source's B channel, 123-ish).
        assert!(
            (i32::from(rma_px[0]) - i32::from(orm_px[2])).abs() <= 8,
            "texel {i}: RMA.r {} vs ORM.b {}",
            rma_px[0],
            orm_px[2]
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn manifest_settings_extend_the_suffix_table() {
    let toml = "[project]\nname = \"x\"\n[import.texture_sets]\nnormal_green = \"directx\"\n[import.texture_sets.suffixes]\nbase_color = [\"diff\"]\n";
    let settings = TextureSetSettings::from_manifest_toml(toml).expect("parses");
    assert_eq!(
        settings.normal_green,
        regia_asset_pipeline::NormalGreenSetting::Directx
    );
    let (_root, folder) = project_with_maps("texset-config", &["pod_diff.png", "pod_N.png"]);
    let scan = scan(&folder, false, &settings).expect("scan");
    assert_eq!(scan.sets.len(), 1);
    use regia_asset_pipeline::TextureSetSlot::*;
    assert!(scan.sets[0].slots.contains_key(&BaseColor));
    // The project default reaches the generated meta: the plain `_N` normal records directx.
    generate(&scan, &settings, true).expect("generate");
    let normal = meta_settings(&folder.join("pod_N.png"));
    assert!(normal.contains("normal_green: Some(directx)"), "{normal}");
    for (bad, needle) in [
        (
            "[import.texture_sets]\nnormal_green = \"vulkan\"\n",
            "normal_green",
        ),
        ("[import.texture_sets]\nfoo = 1\n", "unknown key `foo`"),
        (
            "[import.texture_sets.suffixes]\nnope = [\"x\"]\n",
            "unknown slot `nope`",
        ),
        (
            "[import.texture_sets.suffixes]\nbase_color = [\"Bad Word\"]\n",
            "must be lowercase ascii",
        ),
    ] {
        let err = TextureSetSettings::from_manifest_toml(bad).expect_err(bad);
        assert!(err.contains(needle), "{bad}: {err}");
    }
}
