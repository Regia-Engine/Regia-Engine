// SPDX-License-Identifier: MPL-2.0
//! Manual one-variable quality controls. No diagnostic row replaces release acceptance.

use super::*;

/// Reconstruct the pre-closure energy approximation on the current shader interface so these
/// diagnostic controls remain runnable after the production correction.
fn legacy_mobile_shader() -> String {
    let mut source = include_str!("../../../regia_render/shaders/forward_mobile.wgsl").to_owned();
    let begin = source.find("fn lite_brdf(").expect("BRDF start");
    let end = begin + source[begin..].find("\n}\n").expect("BRDF end") + 3;
    let shared_dfg = source[begin..end].contains("dfg: vec4<f32>");
    let legacy = r#"fn lite_brdf(n: vec3<f32>, v: vec3<f32>, l: vec3<f32>, diffuse_color: vec3<f32>, f0: vec3<f32>, perceptual: f32, a2: f32) -> vec3<f32> {
    let h = normalize(l + v);
    let no_h = max(dot(n, h), 0.0);
    let lo_h = max(dot(l, h), 0.0);
    let d = no_h * no_h * (a2 - 1.0) + 1.0;
    let vis = 1.0 / (4.0 * max(lo_h * lo_h, 0.01) * (perceptual + 0.5));
    let x = 1.0 - lo_h;
    let x2 = x * x;
    let f = f0 + (vec3<f32>(1.0) - f0) * (x2 * x2 * x);
    return (diffuse_color + f * min(a2 / max(d * d, 1e-6) * vis, 256.0)) * (1.0 / regia::pbr::lighting::PI);
}
"#;
    let legacy =
        if shared_dfg {
            legacy.replace("a2: f32", "dfg: vec4<f32>").replace("    let h = normalize(l + v);",
            "    let a2 = perceptual * perceptual * perceptual * perceptual;\n    let h = normalize(l + v);")
        } else {
            legacy.to_owned()
        };
    source.replace_range(begin..end, &legacy);
    source
        .replace(
            "let lm_ab = dfg.rg;",
            "let lm_ab = env_brdf_approx(perceptual, no_v);",
        )
        .replace(
            "let lm_ab = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0).rg;",
            "let lm_ab = env_brdf_approx(perceptual, no_v);",
        )
}

#[test]
#[ignore = "manual Mobile Low diagnosis; original room acceptance remains mandatory"]
fn mobile_low_quality_study() {
    let source = legacy_mobile_shader();
    regia_render::shader_reload::stage_shader_edit("forward_mobile.wgsl", &source)
        .expect("stage pre-closure control");
    let scene = lightmap_room();
    let (pt, image) = reference(&scene);
    let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
    let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
    let reference_indirect = add_planes(&image.indirect_diffuse, &image.indirect_specular);
    let laid = lay_out(&pt, None);
    let map = Arc::new(bake_cpu(&laid, &bake_settings()).0);
    let full_direct = realtime_direct(&scene, None);
    let low = Some(ForwardLite::Low);
    let low_direct = realtime_direct(&scene, low);
    let settings = LightmapSettings {
        enabled: true,
        ..LightmapSettings::default()
    };
    let mut rows = Vec::new();
    let mut record = |name: &str, frame: Vec<[f32; 3]>, direct: &[[f32; 3]]| {
        let score = |base: &[[f32; 3]]| {
            let test = sub_planes(&frame, base);
            let gi = measure(&GiErrorInput {
                reference: &reference_indirect,
                test: &test,
                surface: &surface,
                shadow: &shadow,
                width: GI_WIDTH,
                height: GI_HEIGHT,
                ev100: scene.lab.exposure_ev,
                scale: None,
            });
            (
                gi,
                compare_aov(&reference_indirect, &test, &surface, scene.lab.exposure_ev),
            )
        };
        let legacy = score(&full_direct);
        let own = score(direct);
        eprintln!(
            "MOBILE_CONTROL {name}: legacy={:.8}/{:.8} own={:.8}/{:.8} energy={:?}",
            legacy.0.flip_mean,
            legacy.1["flip_p95"].as_f64().expect("p95"),
            own.0.flip_mean,
            own.1["flip_p95"].as_f64().expect("p95"),
            own.0.energy_ratio
        );
        rows.push(
            json!({"name":name, "legacy":gi_json(&legacy.0), "legacy_compare": legacy.1,
            "own_direct":gi_json(&own.0), "own_direct_compare": own.1}),
        );
    };
    for (name, lite, opts) in [
        ("full", None, settings),
        (
            "full_bilinear",
            None,
            LightmapSettings {
                bicubic: false,
                ..settings
            },
        ),
        (
            "full_bilinear_nondirectional",
            None,
            LightmapSettings {
                bicubic: false,
                directional: false,
                ..settings
            },
        ),
        ("low", low, settings),
    ] {
        record(
            name,
            realtime_lightmapped_config(&scene, &map, &laid.receivers, lite, base_quality(), opts),
            if lite.is_some() { &low_direct } else { &full_direct },
        );
    }
    for (name, encoding) in [
        ("low_bc6h", regia_asset::lightmap::LightmapTexelEncoding::Bc6h),
        (
            "low_astc_hdr",
            regia_asset::lightmap::LightmapTexelEncoding::AstcHdr,
        ),
    ] {
        let mut encoded = (*map).clone();
        encoded.encode_texels(encoding).expect("encode diagnostic");
        record(
            name,
            realtime_lightmapped(&scene, &Arc::new(encoded), &laid.receivers, low),
            &low_direct,
        );
    }
    for (name, density, samples) in [
        ("low_density_32", 2.0, SAMPLES),
        ("low_1024spp", 1.0, SAMPLES_HIGH),
    ] {
        let laid = lay_out_with(&pt, |_, _| density);
        let map = Arc::new(
            bake_cpu(
                &laid,
                &LightmapBakeSettings {
                    samples,
                    ..bake_settings()
                },
            )
            .0,
        );
        record(
            name,
            realtime_lightmapped(&scene, &map, &laid.receivers, low),
            &low_direct,
        );
    }
    let lut_source = if source.contains("@group(0) @binding(13) var brdf_lut:") {
        source.clone()
    } else {
        source.replace("@group(0) @binding(14) var ibl_samp: sampler;",
            "@group(0) @binding(13) var brdf_lut: texture_2d<f32>;\n@group(0) @binding(14) var ibl_samp: sampler;")
    };
    let indirect_lut = lut_source.replace(
        "let lm_ab = env_brdf_approx(perceptual, no_v);",
        "let lm_ab = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(no_v, perceptual), 0.0).rg;",
    );
    let diffuse_albedo = source.replace("return (diffuse_color + f * min(",
        "let ab = env_brdf_approx(perceptual, max(dot(n, v), 1e-5));\n    let es = clamp(f0 * ab.x + vec3<f32>(ab.y), vec3<f32>(0.0), vec3<f32>(1.0));\n    return (diffuse_color * (vec3<f32>(1.0) - es) + f * min(");
    let exact_direct = |text: &str| {
        let begin = text.find("fn lite_brdf(").expect("BRDF start");
        let end = begin + text[begin..].find("\n}\n").expect("BRDF end") + 3;
        let mut result = text.to_owned();
        result.replace_range(begin..end, r#"fn lite_brdf(n: vec3<f32>, v: vec3<f32>, l: vec3<f32>, diffuse_color: vec3<f32>, f0: vec3<f32>, perceptual: f32, a2: f32) -> vec3<f32> {
    let h = normalize(l + v);
    let nv = max(dot(n, v), 1e-5);
    let nl = max(dot(n, l), 1e-5);
    let f = regia::pbr::lighting::f_schlick(max(dot(l, h), 0.0), f0);
    let rough = perceptual * perceptual;
    let spec = f * regia::pbr::lighting::d_ggx(max(dot(n, h), 0.0), rough)
        * regia::pbr::lighting::v_smith_ggx_correlated(nv, nl, rough);
    let dfg = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(nv, perceptual), 0.0);
    let es = clamp(f0 * dfg.x + vec3<f32>(dfg.y), vec3<f32>(0.0), vec3<f32>(1.0));
    let multi = material.response.y > 0.5;
    let diffuse_weight = select(vec3<f32>(1.0) - f, vec3<f32>(1.0) - es, multi);
    let compensation = select(vec3<f32>(1.0), regia::pbr::lighting::energy_compensation(f0, dfg.z, dfg.w), multi);
    return diffuse_color * diffuse_weight / regia::pbr::lighting::PI + spec * compensation;
}"#);
        if text[begin..end].contains("dfg: vec4<f32>") {
            result = result.replace("a2: f32", "dfg: vec4<f32>").replace(
                "    let dfg = textureSampleLevel(brdf_lut, ibl_samp, vec2<f32>(nv, perceptual), 0.0);\n",
                "",
            );
        }
        result
    };
    let variants = [
        ("low_diffuse_directional_albedo_only", diffuse_albedo),
        ("low_indirect_lut_only", indirect_lut.clone()),
        ("low_exact_direct_only", exact_direct(&lut_source)),
        ("low_exact_direct_and_indirect_lut", exact_direct(&indirect_lut)),
        ("low_bicubic_only", source.replace(
            "lightmap_atlas_sample(lightmap_atlas_uv(in.lightmap_uv, in.lightmap_scale_offset), lightmap_page(in.lightmap_scale_offset)).rgb",
            "lightmap_sample_bicubic(lightmap_atlas_uv(in.lightmap_uv, in.lightmap_scale_offset), lightmap_page(in.lightmap_scale_offset))")),
        ("low_diffuse_energy_only", source.replace(
            "return (diffuse_color + f * min(", "return (diffuse_color * (vec3<f32>(1.0) - f) + f * min(")),
        ("low_normal_bias_only", source.replace("const SUN_NORMAL_OFFSET_TEXELS: f32 = 1.5;", "const SUN_NORMAL_OFFSET_TEXELS: f32 = 1.0;")),
        ("low_depth_bias_only", source.replace("const SUN_DEPTH_BIAS_TEXELS: f32 = 1.5;", "const SUN_DEPTH_BIAS_TEXELS: f32 = 1.0;")),
        ("low_smith_visibility_only", source.replace(
            "let vis = 1.0 / (4.0 * max(lo_h * lo_h, 0.01) * (perceptual + 0.5));",
            "let vis = regia::pbr::lighting::v_smith_ggx_correlated(max(dot(n, v), 1e-5), max(dot(n, l), 1e-5), perceptual * perceptual);")),
        ("low_geometric_shadow_normal_only", source.replace(
            "    let normal_scale = material.factors.z;",
            "    let face_cross = cross(dpdx(in.world_position), dpdy(in.world_position));\n    let face = face_cross / max(length(face_cross), 1e-12);\n    let shadow_n = select(-face, face, dot(face, n) >= 0.0);\n    let normal_scale = material.factors.z;")
            .replace("sun_visibility(in.world_position, n, no_l, view_z)",
                "sun_visibility(in.world_position, shadow_n, max(dot(shadow_n, l), 0.0), view_z)")),
    ];
    for (name, shader) in variants {
        assert_ne!(shader, source, "the control must change its one variable");
        regia_render::shader_reload::stage_shader_edit("forward_mobile.wgsl", &shader)
            .expect("stage diagnostic shader");
        let direct = realtime_direct(&scene, low);
        record(
            name,
            realtime_lightmapped(&scene, &map, &laid.receivers, low),
            &direct,
        );
        regia_render::shader_reload::clear_shader_overrides();
    }
    let current_direct = realtime_direct(&scene, low);
    record(
        "low_current",
        realtime_lightmapped(&scene, &map, &laid.receivers, low),
        &current_direct,
    );
    if let Some(dir) = evidence_dir() {
        regia_test_utils::write_atomic(
            &dir.join("mobile_low_controls.json"),
            &serde_json::to_vec_pretty(&rows).expect("JSON"),
        )
        .expect("write controls");
    }
}
