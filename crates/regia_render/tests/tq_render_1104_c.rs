// SPDX-License-Identifier: MPL-2.0
//! Test-gap sweep of 2026-11-04, batch 3 (lane sprint-testgaps-render-pt): the emissive fit's
//! cache key, the bindless level parser and the compressed-texture CPU decode. Pure CPU, fixed
//! inputs, no device, no goldens. Each test's doc says what bug it would catch.

use regia_render::emissive_lights::{material_emits, material_signature};
use regia_render::texture_table::{DEFAULT_BINDLESS_LEVEL, MAX_BINDLESS_LEVEL, requested_level};
use regia_render::{CompressedTexture, LabMaterial, LabTexture, NormalLayout};

fn glowing() -> LabMaterial {
    LabMaterial {
        emissive: [1.0, 0.5, 0.25],
        emissive_intensity: 3.0,
        ..LabMaterial::default()
    }
}

/// Catches an emissive cache key that ignores one of the fields the fit reads: emissive color,
/// intensity, sidedness, and the map's presence and size; and one that changes with fields the
/// fit does not read (albedo), which would refit every frame.
#[test]
fn the_emissive_signature_follows_what_the_fit_reads() {
    let base = glowing();
    let s = material_signature(&base);
    assert_eq!(
        s,
        material_signature(&base.clone()),
        "a clone without a map keys the same"
    );
    let changed = [
        LabMaterial {
            emissive: [1.0, 0.5, 0.3],
            ..glowing()
        },
        LabMaterial {
            emissive_intensity: 3.5,
            ..glowing()
        },
        LabMaterial {
            double_sided: !base.double_sided,
            ..glowing()
        },
        LabMaterial {
            emissive_map: Some(LabTexture {
                width: 1,
                height: 1,
                rgba: vec![255; 4],
            }),
            ..glowing()
        },
    ];
    for (i, m) in changed.iter().enumerate() {
        assert_ne!(material_signature(m), s, "change {i} must re-key the fit");
    }
    let other_albedo = LabMaterial {
        base_color: [0.1, 0.9, 0.1, 1.0],
        ..glowing()
    };
    assert_eq!(material_signature(&other_albedo), s);
    // Zero emission of either sign does not emit, so it never reaches the fit.
    let zero = LabMaterial {
        emissive: [0.0; 3],
        ..LabMaterial::default()
    };
    let negative_zero = LabMaterial {
        emissive: [-0.0; 3],
        ..LabMaterial::default()
    };
    assert!(!material_emits(&zero) && !material_emits(&negative_zero));
}

/// Catches an emissive cache key built from the map's address and length alone: a host that
/// repaints its emissive map in place keeps both, so the renderer kept the old fit (stale
/// emissive lights). Defect fixed in this lane (32 sampled texels join the key).
#[test]
fn repainting_the_emissive_map_in_place_re_keys_the_fit() {
    for (w, h) in [(1u32, 1u32), (3, 2), (64, 64)] {
        let mut m = LabMaterial {
            emissive_map: Some(LabTexture {
                width: w,
                height: h,
                rgba: vec![40; (w * h * 4) as usize],
            }),
            ..glowing()
        };
        let before = material_signature(&m);
        let ptr = m.emissive_map.as_ref().map(|t| t.rgba.as_ptr());
        if let Some(map) = m.emissive_map.as_mut() {
            map.rgba.iter_mut().for_each(|b| *b = 200);
        }
        assert_eq!(
            ptr,
            m.emissive_map.as_ref().map(|t| t.rgba.as_ptr()),
            "edited in place"
        );
        assert_ne!(
            material_signature(&m),
            before,
            "{w}x{h}: a repainted map must re-key"
        );
        assert_eq!(
            material_signature(&m),
            material_signature(&m),
            "stable for the same state"
        );
    }
    // A map whose buffer is shorter than its size, or empty, keys without panicking.
    let odd = LabMaterial {
        emissive_map: Some(LabTexture {
            width: 4,
            height: 4,
            rgba: vec![1, 2, 3],
        }),
        ..glowing()
    };
    let _ = material_signature(&odd);
}

/// Catches a level parser that reads a number too large for `u32` as the default level instead of
/// capping it as documented, accepts signs or hex, or does not trim.
#[test]
fn bindless_level_text_parses_caps_and_falls_back() {
    assert_eq!(requested_level(None), DEFAULT_BINDLESS_LEVEL);
    assert_eq!(requested_level(Some("")), DEFAULT_BINDLESS_LEVEL);
    assert_eq!(requested_level(Some("   ")), DEFAULT_BINDLESS_LEVEL);
    assert_eq!(requested_level(Some(" 2\n")), 2);
    assert_eq!(requested_level(Some("0")), 0);
    assert_eq!(requested_level(Some("5")), MAX_BINDLESS_LEVEL);
    assert_eq!(requested_level(Some("4294967295")), MAX_BINDLESS_LEVEL);
    assert_eq!(
        requested_level(Some("99999999999")),
        MAX_BINDLESS_LEVEL,
        "larger numbers are capped"
    );
    for junk in ["-1", "0x2", "two", "2.0", "+"] {
        assert_eq!(requested_level(Some(junk)), DEFAULT_BINDLESS_LEVEL, "{junk:?}");
    }
}

/// Catches a CPU decode that shifts the size by the level before checking it exists: a level of
/// 32 or more panicked (`>> 32`) instead of answering `None`, even on a chain that lists that
/// many levels. Defect fixed in this lane.
#[test]
fn decoding_a_level_past_32_answers_instead_of_panicking() {
    let texture = |levels: usize| CompressedTexture {
        format: wgpu::TextureFormat::Bc7RgbaUnormSrgb,
        width: 8,
        height: 8,
        levels: (0..levels).map(|_| vec![0u8; 16].into()).collect(),
        normal_layout: NormalLayout::default(),
        mip_stream: None,
    };
    assert!(texture(0).decode_level_rgba8(0).is_none());
    assert!(texture(1).decode_level_rgba8(1).is_none(), "a missing level");
    assert!(texture(1).decode_level_rgba8(40).is_none());
    assert!(texture(1).decode_level_rgba8(u32::MAX).is_none());
    // Level 0 of an 8 x 8 chain needs four blocks: one is a truncated level.
    assert!(texture(1).decode_level_rgba8(0).is_none(), "a truncated level");
    // A malformed chain with 40 one-block levels: the deep levels are 1 x 1.
    let deep = texture(40);
    for level in [3u32, 31, 32, 39] {
        if let Some(img) = deep.decode_level_rgba8(level) {
            assert_eq!((img.width, img.height), (1, 1), "level {level}");
            assert_eq!(img.rgba.len(), 4);
        }
    }
}

/// Catches a lite-tier resolver whose process default (no device created, so no adapter noted)
/// takes the lite path, and tier names or defines that do not round-trip or collide.
#[test]
fn forward_lite_defaults_to_the_full_shader_without_an_adapter() {
    use regia_render::forward_lite::{ForwardLite, frame_tier, parse_env};
    // This test binary never creates a device: the adapter bit is 0 and no env override is read.
    assert_eq!(frame_tier(ForwardLite::Low, None), ForwardLite::Off);
    assert_eq!(frame_tier(ForwardLite::High, None), ForwardLite::Off);
    assert_eq!(
        frame_tier(ForwardLite::Off, Some(ForwardLite::High)),
        ForwardLite::High
    );
    let mut defs = std::collections::HashSet::new();
    for tier in [ForwardLite::Off, ForwardLite::Low, ForwardLite::High] {
        assert_eq!(ForwardLite::from_name(tier.name()), Some(tier));
        assert_eq!(
            ForwardLite::from_name(&format!("  {} ", tier.name().to_uppercase())),
            Some(tier)
        );
        assert_eq!(tier.shader_def().is_none(), tier == ForwardLite::Off);
        assert!(defs.insert(tier.shader_def()));
    }
    for junk in [None, Some(""), Some("auto"), Some("lowest"), Some("1")] {
        assert_eq!(parse_env(junk), None, "{junk:?}");
    }
}

/// Catches a sheen distribution that turns NaN or infinite at the roughness floor the material
/// pack clamps to (`1/α` is about 204 there) or at grazing and head-on half vectors.
#[test]
fn charlie_sheen_is_finite_down_to_the_roughness_floor() {
    use regia_render::shading_model::{d_charlie, sheen_albedo, v_sheen};
    for roughness in [0.07f32, 0.1, 0.3, 0.7, 1.0] {
        let alpha = roughness * roughness;
        for i in 0..=40 {
            let no_h = -1.0 + i as f32 / 20.0;
            let d = d_charlie(no_h, alpha);
            assert!(
                d.is_finite() && d >= 0.0,
                "roughness {roughness}, n.h {no_h}: {d}"
            );
        }
        for mu in [0.0f32, 0.01, 0.5, 1.0, -1.0, f32::NAN] {
            let a = sheen_albedo(mu, roughness);
            assert!((0.0..=1.0).contains(&a), "{mu} {roughness}: {a}");
        }
    }
    assert!(v_sheen(0.0, 0.0).is_finite());
    assert!(v_sheen(-1.0, 1.0).is_finite());
}

/// Catches a cooked meshlet reader that accepts another asset type's payload, panics on a bad
/// wrapper or a truncated blob, or loses data in the cook wrapper round trip; and agent codes that
/// do not map each failure as documented.
#[test]
fn cooked_meshlet_files_round_trip_and_refuse_the_rest() {
    use regia_asset::{AssetId, COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file};
    use regia_core::id::{Guid, TypePathHash};
    use regia_render::visbuffer::{MeshletFormatError, MeshletLoadError, MeshletMesh};
    let positions: Vec<[f32; 3]> = (0..16).map(|i| [(i % 4) as f32, (i / 4) as f32, 0.0]).collect();
    let indices: Vec<u32> = (0..3)
        .flat_map(|y| (0..3).map(move |x| y * 4 + x))
        .flat_map(|i| [i, i + 1, i + 4, i + 1, i + 5, i + 4])
        .collect();
    let mesh = MeshletMesh::single_level(&positions, &indices);
    mesh.validate().expect("a valid single-level mesh");
    let payload = mesh.to_bytes();
    let wrap = |type_path: &str, payload: &[u8]| {
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: TypePathHash::of(type_path),
            asset_id: AssetId(Guid::from_u128(0x1104)),
            importer_version: 1,
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        encode_cooked_file(&header, payload).expect("wraps")
    };
    let read = MeshletMesh::from_cooked_file(&wrap(regia_render::visbuffer::MESHLET_TYPE_PATH, &payload));
    assert_eq!(read.as_ref().ok(), Some(&mesh));
    let other = MeshletMesh::from_cooked_file(&wrap("regia_render::NotMeshlets", &payload));
    assert!(matches!(other, Err(MeshletFormatError::WrongType)), "{other:?}");
    let garbage = MeshletMesh::from_cooked_file(b"definitely not cooked");
    assert!(
        matches!(garbage, Err(MeshletFormatError::Cooked(_))),
        "{garbage:?}"
    );
    assert!(MeshletMesh::from_cooked_file(&[]).is_err());
    for cut in [0, 3, 4, payload.len() / 2, payload.len() - 1] {
        let truncated =
            MeshletMesh::from_cooked_file(&wrap(regia_render::visbuffer::MESHLET_TYPE_PATH, &payload[..cut]));
        assert!(truncated.is_err(), "a blob cut at {cut} must not read");
    }
    let code = |e: MeshletFormatError| MeshletLoadError::Format(e).code();
    assert_eq!(code(MeshletFormatError::WrongType), "meshlets_unreadable");
    assert_eq!(code(MeshletFormatError::BadMagic), "meshlets_unreadable");
    assert_eq!(code(MeshletFormatError::Truncated), "meshlets_invalid");
    assert_eq!(
        MeshletLoadError::NotCooked("x".into()).code(),
        "meshlets_not_cooked"
    );
}
