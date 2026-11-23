// SPDX-License-Identifier: MPL-2.0
//! Vulkan validation gate for every engine shader.
//!
//! naga 30 decorates struct members with `Offset` and every array with
//! `ArrayStride`. On a Function or Private variable that is
//! `VUID-StandaloneSpirv-None-10684`: Mesa rejects the module and NVIDIA still
//! creates it (ADR-026). The same decoration on a Workgroup variable is a known
//! naga output. Drivers and SPIRV-Tools 2025.1 accept it; SPIRV-Tools 2026.4
//! rejects it under that same VUID (ADR-135). wgpu does not enable
//! `WorkgroupMemoryExplicitLayoutKHR`. wgpu-hal 30.0.1 drops message id
//! `0xb210f7c2` inside its debug callback, so a log capture cannot see it.
//! [`validate_engine_shaders`] runs `spirv-val` on the SPIR-V naga emits for
//! Vulkan 1.3 (SPIR-V 1.6), then creates every composed module and the
//! pipelines a Desktop High and a Mobile High frame use, with
//! [`wgpu::InstanceFlags::VALIDATION`] forced on, and fails on any validation
//! error other than that one allowlisted VUID.
//!
//! Every module is also scanned with [`spirv_explicit_layout_violations`].
//! Function and Private hits fail the gate, because an older `spirv-val` misses
//! some of them. A Workgroup hit is a warning: the scan still names
//! `VUID-StandaloneSpirv-None-10684`, and `spirv-val` output that cites only
//! that exact VUID id is ignored. No other VUID is ignored.
//! A missing `spirv-val` is an error, except on a gate host that sets
//! [`SPIRV_VAL_FALLBACK_ENV`]`=1`; there naga's validator and the scan run alone.
//! CI never sets it.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use naga_oil::compose::ShaderDefValue;

use crate::shader_lib::compose_source;

mod cost;
mod matrix;
use crate::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, FrameTarget, LabMaterial,
    LabVertex, PostSettings, RenderDevice, RenderError, RenderProfile, RenderProfilePreset,
};
pub use cost::{
    SHADER_COST_REPORT_VERSION, ShaderBytesBudget, ShaderCostReport, ShaderCostRow, shader_cost_report,
    shader_cost_row,
};
#[cfg(test)]
pub(crate) use matrix::FXC_REJECTS;
pub(crate) use matrix::vertex_format;
pub use matrix::{
    BackendResult, BackendStatus, ForwardPermutation, MATRIX_TEXTURE_TABLE_SIZE, MatrixBackend, MatrixInput,
    MatrixModule, MatrixOrigin, MatrixPermutation, MatrixScope, PresetFrameResult, SHADER_MATRIX_JOBS_ENV,
    SHADER_MATRIX_REPORT_VERSION, ShaderMatrixOptions, ShaderMatrixReport, default_matrix_jobs, define_key,
    forward_permutations_for, run_matrix_inputs, run_shader_matrix, shader_matrix_inputs,
    validate_preset_frames,
};

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Error
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if let Ok(mut lines) = LINES.lock() {
            lines.push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

fn install_logger() -> Result<(), RenderError> {
    static INSTALL: OnceLock<bool> = OnceLock::new();
    let ok = *INSTALL.get_or_init(|| {
        let installed = log::set_logger(&Capture).is_ok();
        if installed {
            log::set_max_level(log::LevelFilter::Error);
        }
        installed
    });
    if ok {
        Ok(())
    } else {
        Err(RenderError::Shader(
            "validation log capture could not be installed (a logger is already set)".into(),
        ))
    }
}

fn defs(pairs: &[(&str, bool)]) -> HashMap<String, ShaderDefValue> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), ShaderDefValue::Bool(*value)))
        .collect()
}

/// Every engine entry shader with the shader-def sets the frame compiles it with (also the
/// revalidation set of [`crate::shader_reload`]).
pub(crate) fn shader_sources() -> Vec<(&'static str, &'static str, HashMap<String, ShaderDefValue>)> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<_>, name: &'static str, source: &'static str, shader_defs: HashMap<_, _>| {
        out.push((name, source, shader_defs));
    };
    // M15c outdoor passes (sky + aerial, froxel fog, clouds).
    push(
        &mut out,
        "outdoor_sky.wgsl",
        include_str!("../shaders/outdoor_sky.wgsl"),
        HashMap::new(),
    );
    push(
        &mut out,
        "volumetric_fog.wgsl",
        include_str!("../shaders/volumetric_fog.wgsl"),
        HashMap::new(),
    );
    push(
        &mut out,
        "clouds.wgsl",
        include_str!("../shaders/clouds.wgsl"),
        HashMap::new(),
    );
    push(
        &mut out,
        "cluster_build.wgsl",
        include_str!("../shaders/cluster_build.wgsl"),
        HashMap::new(),
    );
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        HashMap::new(),
    );
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[("skip_srgb_encode", true)]),
    );
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[("GPU_DRIVEN", true), ("skip_srgb_encode", true)]),
    );
    // The lite forward shader, every composition the renderer can ask for (ADR-1874).
    for tier in [
        crate::forward_lite::ForwardLite::Low,
        crate::forward_lite::ForwardLite::High,
    ] {
        for alpha_test in [false, true] {
            for encode in [false, true] {
                for weather in [false, true] {
                    push(
                        &mut out,
                        crate::forward_lite::FORWARD_LITE_SHADER_FILE,
                        include_str!("../shaders/forward_mobile.wgsl"),
                        crate::forward_lite::lite_defs(tier, alpha_test, encode, weather),
                    );
                }
                // Lightmap receivers on the lite tiers (`frame/lightmap_paths.rs`, ADR-2107).
                let mut lit = crate::forward_lite::lite_defs(tier, alpha_test, encode, false);
                lit.insert("LIGHTMAP".to_string(), ShaderDefValue::Bool(true));
                push(
                    &mut out,
                    crate::forward_lite::FORWARD_LITE_SHADER_FILE,
                    include_str!("../shaders/forward_mobile.wgsl"),
                    lit,
                );
            }
        }
    }
    // The Adreno sun shadow workaround (ADR-1749), CPU and GPU-driven paths.
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[
            ("SUN_SHADOW_PCF_ONLY", true),
            ("STUDIO_IBL_OFF", true),
            ("FORWARD_TRIM", true),
            ("ALPHA_TEST", true),
        ]),
    );
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[
            ("SUN_SHADOW_PCF_ONLY", true),
            ("STUDIO_IBL_OFF", true),
            ("FORWARD_TRIM", true),
            ("GPU_DRIVEN", true),
            ("skip_srgb_encode", true),
        ]),
    );
    // Per-instance glass (own-canopy mode, crack pages; `frame/glass_gpu.rs`), both encodings.
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[("GLASS", true), ("ALPHA_TEST", true)]),
    );
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[("GLASS", true), ("ALPHA_TEST", true), ("skip_srgb_encode", true)]),
    );
    // Material mask recolor and decal slots (`frame/mask_decals_gpu.rs`), both encodings.
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[("MASK_DECALS", true), ("ALPHA_TEST", true)]),
    );
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[
            ("MASK_DECALS", true),
            ("ALPHA_TEST", true),
            ("skip_srgb_encode", true),
        ]),
    );
    // Thin film over the base specular (`frame/thin_film_gpu.rs`, ADR-1977), both encodings.
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[("THIN_FILM", true), ("ALPHA_TEST", true)]),
    );
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[
            ("THIN_FILM", true),
            ("ALPHA_TEST", true),
            ("skip_srgb_encode", true),
        ]),
    );
    // The film on masked instances and lightmap receivers (`frame/thin_film_gpu/variants.rs`,
    // ADR-2732), both encodings, with and without the traced sun's bindings.
    for feature in ["MASK_DECALS", "LIGHTMAP"] {
        for encode in [&[][..], &[("skip_srgb_encode", true)][..]] {
            let mut set = defs(&[("THIN_FILM", true), (feature, true), ("ALPHA_TEST", true)]);
            set.extend(defs(encode));
            push(
                &mut out,
                "lab_forward.wgsl",
                include_str!("../shaders/lab_forward.wgsl"),
                set.clone(),
            );
            crate::hwrt::SunShadowVariant::RayQuery.add_forward_defs(&mut set);
            push(
                &mut out,
                "lab_forward.wgsl",
                include_str!("../shaders/lab_forward.wgsl"),
                set,
            );
        }
    }
    // Subsurface scattering's draws and blur (`frame/subsurface_gpu.rs`, ADR-2049): always linear
    // HDR, with and without the traced sun's bindings.
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[
            ("SUBSURFACE", true),
            ("ALPHA_TEST", true),
            ("skip_srgb_encode", true),
        ]),
    );
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[
            ("SUBSURFACE", true),
            ("ALPHA_TEST", true),
            ("skip_srgb_encode", true),
            ("SUN_SHADOW_RT", true),
        ]),
    );
    push(
        &mut out,
        "subsurface_scatter.wgsl",
        include_str!("../shaders/subsurface_scatter.wgsl"),
        defs(&[]),
    );
    // Order-independent transparency's accumulation and composite (`frame/oit_gpu.rs`, ADR-2065):
    // always linear HDR, with and without the traced sun's bindings.
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[("OIT", true), ("ALPHA_TEST", true), ("skip_srgb_encode", true)]),
    );
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[
            ("OIT", true),
            ("ALPHA_TEST", true),
            ("skip_srgb_encode", true),
            ("SUN_SHADOW_RT", true),
        ]),
    );
    push(
        &mut out,
        "oit_composite.wgsl",
        include_str!("../shaders/oit_composite.wgsl"),
        defs(&[]),
    );
    // Baked lightmap receivers (`frame/lightmap_forward.rs`, ADR-1623), both encodings.
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[("LIGHTMAP", true), ("ALPHA_TEST", true)]),
    );
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        defs(&[
            ("LIGHTMAP", true),
            ("ALPHA_TEST", true),
            ("skip_srgb_encode", true),
        ]),
    );
    // Instanced lightmap receivers (`frame/lightmap_paths.rs`, ADR-2107).
    for extra in [
        &[][..],
        &[("ALPHA_TEST", true)][..],
        &[("skip_srgb_encode", true)][..],
        &[("ALPHA_TEST", true), ("skip_srgb_encode", true)][..],
    ] {
        let mut set = defs(&[("GPU_DRIVEN", true), ("GPU_INSTANCED", true), ("LIGHTMAP", true)]);
        set.extend(defs(extra));
        push(
            &mut out,
            "lab_forward.wgsl",
            include_str!("../shaders/lab_forward.wgsl"),
            set.clone(),
        );
        // The same under a profile that traces the sun (ADR-2035, ADR-2106).
        crate::hwrt::SunShadowVariant::RayQuery.add_forward_defs(&mut set);
        push(
            &mut out,
            "lab_forward.wgsl",
            include_str!("../shaders/lab_forward.wgsl"),
            set,
        );
    }
    push(
        &mut out,
        "lab_shadow.wgsl",
        include_str!("../shaders/lab_shadow.wgsl"),
        HashMap::new(),
    );
    push(
        &mut out,
        "lab_shadow.wgsl",
        include_str!("../shaders/lab_shadow.wgsl"),
        defs(&[("GPU_DRIVEN", true)]),
    );
    // Alpha-tested instanced casters (`FrameRenderer::set_alpha_tested_shadows`, ADR-1415).
    push(
        &mut out,
        "lab_shadow.wgsl",
        include_str!("../shaders/lab_shadow.wgsl"),
        defs(&[("GPU_DRIVEN", true), ("ALPHA_TEST", true)]),
    );
    // Translucent shadow casters (colored shadow map, `frame/translucent_shadow.rs`).
    push(
        &mut out,
        "translucent_shadow.wgsl",
        include_str!("../shaders/translucent_shadow.wgsl"),
        HashMap::new(),
    );
    push(
        &mut out,
        "translucent_shadow.wgsl",
        include_str!("../shaders/translucent_shadow.wgsl"),
        defs(&[("GLASS", true)]),
    );
    // Stochastic SSR and its single-pass depth pyramid (both reductions, both sources).
    for (min, from_depth) in [(false, false), (false, true), (true, false), (true, true)] {
        push(
            &mut out,
            "sssr_spd.wgsl",
            include_str!("../shaders/sssr_spd.wgsl"),
            defs(&[("SPD_MIN", min), ("SPD_FROM_DEPTH", from_depth)])
                .into_iter()
                .filter(|(_, value)| *value == ShaderDefValue::Bool(true))
                .collect(),
        );
    }
    for (name, source) in [
        (
            "sssr_classify.wgsl",
            include_str!("../shaders/sssr_classify.wgsl"),
        ),
        (
            "sssr_intersect.wgsl",
            include_str!("../shaders/sssr_intersect.wgsl"),
        ),
        (
            "sssr_reproject.wgsl",
            include_str!("../shaders/sssr_reproject.wgsl"),
        ),
        (
            "sssr_prefilter.wgsl",
            include_str!("../shaders/sssr_prefilter.wgsl"),
        ),
        ("sssr_resolve.wgsl", include_str!("../shaders/sssr_resolve.wgsl")),
        (
            "sssr_composite.wgsl",
            include_str!("../shaders/sssr_composite.wgsl"),
        ),
        ("sssr_probe.wgsl", include_str!("../shaders/sssr_probe.wgsl")),
    ] {
        push(&mut out, name, source, HashMap::new());
    }
    for (name, source) in [
        ("gpu_cull.wgsl", include_str!("../shaders/gpu_cull.wgsl")),
        (
            "prepass_occlusion.wgsl",
            include_str!("../shaders/prepass_occlusion.wgsl"),
        ),
        // Two-phase occlusion of the shadow casters (`shadow_occlusion.rs`, ADR-2072): plain WGSL
        // at runtime, no defines.
        (
            "shadow_occlusion.wgsl",
            include_str!("../shaders/shadow_occlusion.wgsl"),
        ),
        (
            "shadow_pyramid.wgsl",
            include_str!("../shaders/shadow_pyramid.wgsl"),
        ),
        ("hiz_copy.wgsl", include_str!("../shaders/hiz_copy.wgsl")),
        ("hiz_down.wgsl", include_str!("../shaders/hiz_down.wgsl")),
        ("ssao.wgsl", include_str!("../shaders/ssao.wgsl")),
        (
            "contact_shadows.wgsl",
            include_str!("../shaders/contact_shadows.wgsl"),
        ),
        ("taau.wgsl", include_str!("../shaders/taau.wgsl")),
        (
            "scripted_vendor.wgsl",
            include_str!("upscaler/vendor/scripted_vendor.wgsl"),
        ),
        ("taau_sharpen.wgsl", include_str!("../shaders/taau_sharpen.wgsl")),
        ("velocity.wgsl", include_str!("../shaders/velocity.wgsl")),
        ("tsr_prepare.wgsl", crate::tsr::PREPARE_WGSL),
        ("tsr_resolve.wgsl", crate::tsr::RESOLVE_WGSL),
        ("tsr_sharpen.wgsl", crate::tsr::SHARPEN_WGSL),
        ("ssgi.wgsl", include_str!("../shaders/ssgi.wgsl")),
        ("ssr.wgsl", include_str!("../shaders/ssr.wgsl")),
        ("page_clear.wgsl", include_str!("../shaders/page_clear.wgsl")),
        ("xr_eye_mask.wgsl", include_str!("../shaders/xr_eye_mask.wgsl")),
        ("depth_match.wgsl", include_str!("../shaders/depth_match.wgsl")),
        (
            "depth_resolve.wgsl",
            include_str!("../shaders/depth_resolve.wgsl"),
        ),
        ("post.wgsl", include_str!("../shaders/post.wgsl")),
        ("sgsr.wgsl", include_str!("../shaders/sgsr.wgsl")),
        ("smaa.wgsl", include_str!("../shaders/smaa.wgsl")),
        ("motion_blur.wgsl", include_str!("../shaders/motion_blur.wgsl")),
        ("dof.wgsl", include_str!("../shaders/dof.wgsl")),
        (
            "exposure_hist.wgsl",
            include_str!("../shaders/exposure_hist.wgsl"),
        ),
        // Morph targets pre-pass (`frame/morph_gpu.rs`, Tincal B1).
        ("morph.wgsl", include_str!("../shaders/morph.wgsl")),
        // GI denoiser (`gi_denoise/gpu.rs`, ADR-864).
        ("gi_denoise.wgsl", include_str!("../shaders/gi_denoise.wgsl")),
        // Runtime lens effects (`lens_effects/gpu.rs`, ADR-1863).
        ("lens_effects.wgsl", include_str!("../shaders/lens_effects.wgsl")),
        // Video convert pass (`video_convert.rs`, ADR-2044).
        (
            "video_convert.wgsl",
            include_str!("../shaders/video_convert.wgsl"),
        ),
        (
            "video_fullscreen.wgsl",
            include_str!("../shaders/video_fullscreen.wgsl"),
        ),
    ] {
        push(&mut out, name, source, HashMap::new());
    }
    // Depth of field: the tile pass above, the blur passes here.
    push(
        &mut out,
        "dof.wgsl",
        include_str!("../shaders/dof.wgsl"),
        defs(&[("DOF_BLUR", true)]),
    );
    // FSR 3.1 upscaler (ADR-113): the shared module prepended to each compute pass.
    for (name, source) in crate::fsr3::checked_sources() {
        push(&mut out, name, source, HashMap::new());
    }
    // Frame generation (ADR-2067): the shared modules prepended to each pass.
    for (name, source) in crate::frame_gen::wgsl::checked_sources() {
        push(&mut out, name, source, HashMap::new());
    }
    // Visibility buffer (ADR-078): cull, Hi-Z, raster, motion and the resolve variants.
    for (name, source, names) in crate::visbuffer::gpu::checked_sources() {
        let pairs: Vec<(&str, bool)> = names.into_iter().map(|n| (n, true)).collect();
        push(&mut out, name, source, defs(&pairs));
    }
    // The alpha-tested visibility raster (ADR-3915) needs a device with the table's binding array
    // features, as the single-pass resolve does: `visbuffer::tests::masked` runs `spirv_validate`
    // and the uniformity walk on it, and frames build it.
    // Boot branding screen (`boot.rs`), drawn before the first frame.
    push(
        &mut out,
        "boot-splash.wgsl",
        include_str!("../shaders/boot-splash.wgsl"),
        HashMap::new(),
    );
    // Fullscreen fade overlay (`fade_overlay.rs`).
    push(
        &mut out,
        "fade_overlay.wgsl",
        include_str!("../shaders/fade_overlay.wgsl"),
        HashMap::new(),
    );
    // Fog of war world overlay (`fog_overlay.rs`, audit gap FW-1).
    push(
        &mut out,
        "fog_of_war_overlay.wgsl",
        include_str!("../shaders/fog_of_war_overlay.wgsl"),
        HashMap::new(),
    );
    // Immediate triangles (the quick draw layer).
    push(
        &mut out,
        "immediate.wgsl",
        crate::immediate::immediate_shader_source(),
        HashMap::new(),
    );
    // Gizmo lines and joints: every PERSPECTIVE × DEPTH_MULTISAMPLED variant.
    for perspective in [false, true] {
        for multisampled in [false, true] {
            push(
                &mut out,
                "gizmos.wgsl",
                crate::gizmos::render::shader_source(),
                crate::gizmos::render::shader_defs(perspective, multisampled),
            );
        }
    }
    // Scalar overlay: both depth variants.
    for multisampled in [false, true] {
        push(
            &mut out,
            "scalar_overlay.wgsl",
            crate::scalar_overlay_pass::shader_source(),
            crate::scalar_overlay_pass::shader_defs(multisampled),
        );
    }
    // Compute fallback for shadow rays (ADR-515). The ray-query variant needs a device with
    // `EXPERIMENTAL_RAY_QUERY`; `hwrt::shadow` validates it with naga alone.
    push(
        &mut out,
        "hwrt_shadow_fallback.wgsl",
        crate::hwrt::FALLBACK_SHADOW_WGSL,
        HashMap::new(),
    );
    // The single-pass visibility resolve (ADR-2071) needs a device with the table's binding array
    // features: `visbuffer::tests::single_pass` runs `spirv_validate` on it, and frames build it.
    // The frame's sun trace (ADR-2035): the plain tracers. The ray-query one needs a device with
    // `EXPERIMENTAL_RAY_QUERY`; `hwrt::sun_trace` validates it with naga alone.
    push(
        &mut out,
        "sun_trace_main.wgsl",
        crate::hwrt::SUN_TRACE_BVH_WGSL,
        HashMap::new(),
    );
    push(
        &mut out,
        "sun_trace_screen.wgsl",
        crate::hwrt::SUN_TRACE_SCREEN_WGSL,
        HashMap::new(),
    );
    // The forward shader's ray-traced sun-shadow variant (ADR-1149), as a frame compiles it.
    let mut rt_forward = crate::hwrt::SunShadowVariant::RayQuery.forward_shader_defs();
    rt_forward.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        rt_forward,
    );
    // The forward shader's many-light opaque variant and the frame's blue-noise binding
    // (ADR-2652), as a desktop frame composes them.
    push(
        &mut out,
        "lab_forward.wgsl",
        include_str!("../shaders/lab_forward.wgsl"),
        HashMap::from([
            ("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true)),
            ("RESTIR_DI".to_string(), ShaderDefValue::Bool(true)),
            ("FRAME_BLUE_NOISE".to_string(), ShaderDefValue::Bool(true)),
        ]),
    );
    // The energy-compensated BRDF (ADR-2632): a profile or the process opts in and every forward
    // module the frame composes takes the define (`bindless::add_material_defs`). The full shader
    // on the CPU path (sRGB encode) and the GPU-driven path (linear HDR), and both lite tiers with
    // and without a lightmap.
    for mut compensated in [
        defs(&[("ALPHA_TEST", true)]),
        defs(&[("GPU_DRIVEN", true), ("skip_srgb_encode", true)]),
    ] {
        crate::BrdfEnergy::Compensated.add_forward_def(&mut compensated);
        push(
            &mut out,
            "lab_forward.wgsl",
            include_str!("../shaders/lab_forward.wgsl"),
            compensated,
        );
    }
    for tier in [
        crate::forward_lite::ForwardLite::Low,
        crate::forward_lite::ForwardLite::High,
    ] {
        for lightmap in [false, true] {
            let mut compensated = crate::forward_lite::lite_defs(tier, true, true, false);
            if lightmap {
                compensated.insert("LIGHTMAP".to_string(), ShaderDefValue::Bool(true));
            }
            crate::BrdfEnergy::Compensated.add_forward_def(&mut compensated);
            push(
                &mut out,
                crate::forward_lite::FORWARD_LITE_SHADER_FILE,
                include_str!("../shaders/forward_mobile.wgsl"),
                compensated,
            );
        }
    }
    // Forward decals (ADR-3255): the gather before lighting and the receiver-layer varying, as a
    // desktop frame with the switch on composes them (opaque and alpha-tested).
    for alpha_test in [false, true] {
        let mut defs = HashMap::from([
            ("FRAME_FORWARD_DECALS".to_string(), ShaderDefValue::Bool(true)),
            ("FRAME_BLUE_NOISE".to_string(), ShaderDefValue::Bool(true)),
        ]);
        if alpha_test {
            defs.insert("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true));
        }
        push(
            &mut out,
            "lab_forward.wgsl",
            include_str!("../shaders/lab_forward.wgsl"),
            defs,
        );
    }
    // Reservoir direct lighting (ADR-2470): the sampler's three compute passes, the depth-only
    // G-buffer and the debug composite (`restir_di::{gpu, depth_gbuffer, debug_pass}`).
    for (name, source) in [
        ("restir_di.wgsl", include_str!("../shaders/restir_di.wgsl")),
        (
            "restir_di_debug.wgsl",
            include_str!("../shaders/restir_di_debug.wgsl"),
        ),
        (
            "restir_di_composite.wgsl",
            include_str!("../shaders/restir_di_composite.wgsl"),
        ),
    ] {
        push(&mut out, name, source, HashMap::new());
    }
    out
}

fn quad_scene<'a>(
    quality: FrameQuality,
    material: &'a LabMaterial,
    vertices: &'a [LabVertex],
    settings: &'a PostSettings,
) -> FrameScene<'a> {
    FrameScene {
        width: 64,
        height: 64,
        eye: [0.0, 2.0, 4.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.0,
        near: 0.1,
        far: 30.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(settings),
        fog: crate::FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, -0.4], [1.0; 3], 100_000.0),
        local_lights: &[],
        quality,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1,
            object_key: 1,
            vertices,
            indices: &[0, 2, 1, 0, 3, 2],
            material,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: crate::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    }
}

fn validation_hits(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| {
            line.contains("VUID-")
                || line.contains("Validation Error")
                || line.contains("Invalid explicit layout")
                || line.contains("UNASSIGNED-")
        })
        .cloned()
        .collect()
}

/// Identity `@group` / `@binding` map. wgpu-hal fills the same numbers from the
/// pipeline layout; Regia Engine layouts do not remap.
fn identity_binding_map(module: &naga::Module) -> naga::back::spv::BindingMap {
    let mut binding_map = naga::back::spv::BindingMap::new();
    for (_, var) in module.global_variables.iter() {
        let Some(binding) = var.binding else {
            continue;
        };
        binding_map.insert(
            binding,
            naga::back::spv::BindingInfo {
                descriptor_set: binding.group,
                binding: binding.binding,
                binding_array_size: None,
            },
        );
    }
    binding_map
}

/// SPIR-V options that match wgpu-hal on a Vulkan 1.3 desktop device.
///
/// Index bounds use `Restrict` (wgpu always does). Buffer bounds stay
/// `Unchecked`, which is what wgpu emits when `robustBufferAccess2` is
/// present. `fake_missing_bindings` is off: a resource with no `@binding`
/// fails here instead of producing a module the driver would reject later.
fn spirv_options(module: &naga::Module) -> naga::back::spv::Options<'static> {
    naga::back::spv::Options {
        lang_version: (1, 6),
        flags: naga::back::spv::WriterFlags::LABEL_VARYINGS | naga::back::spv::WriterFlags::FORCE_POINT_SIZE,
        fake_missing_bindings: false,
        binding_map: identity_binding_map(module),
        capabilities: None,
        bounds_check_policies: naga::proc::BoundsCheckPolicies {
            index: naga::proc::BoundsCheckPolicy::Restrict,
            buffer: naga::proc::BoundsCheckPolicy::Unchecked,
            image_load: naga::proc::BoundsCheckPolicy::Restrict,
            binding_array: naga::proc::BoundsCheckPolicy::Unchecked,
        },
        zero_initialize_workgroup_memory: naga::back::spv::ZeroInitializeWorkgroupMemoryMode::Native,
        force_loop_bounding: true,
        ray_query_initialization_tracking: true,
        trace_ray_argument_validation: true,
        use_storage_input_output_16: false,
        debug_info: None,
        task_dispatch_limits: None,
        mesh_shader_primitive_indices_clamp: true,
        emit_int_div_checks: true,
    }
}

/// Exact VUID id SPIRV-Tools 2026.4 raises for naga's `ArrayStride` on a
/// Workgroup variable. Allowlisted by this id alone (ADR-135).
///
/// Function and Private uses of the same id stay errors via the layout scan
/// (ADR-026). No other VUID is ignored, and a diagnostic that cites this id
/// together with any other `VUID-` is not ignored.
const NAGA_WORKGROUP_ARRAYSTRIDE_VUID: &str = "VUID-StandaloneSpirv-None-10684";

/// Emits SPIR-V 1.6 the way wgpu-hal does, scans it for the ADR-026 layout
/// rule, and runs `spirv-val --target-env vulkan1.3`.
///
/// wgpu-hal's debug callback returns without logging when the message id is
/// `VUID-StandaloneSpirv-None-10684` (`0xb210f7c2`), so these process-side
/// checks are the ones that fail the gate on a Function or Private hit. The
/// scan runs on every host because SPIRV-Tools 2025.1 accepts `ArrayStride` on
/// a Workgroup variable, which SPIRV-Tools 2026.4 rejects under the same VUID.
/// That Workgroup hit is a warning (ADR-135): drivers accept the module, and
/// rewriting every workgroup array as one variable per index blows naga's
/// compile cost. `spirv-val` diagnostics that cite only
/// [`NAGA_WORKGROUP_ARRAYSTRIDE_VUID`] are ignored. Pipeline-overridable
/// constants are replaced with their WGSL defaults first: naga 30 refuses to
/// emit SPIR-V while the override arena is still populated, and wgpu does the
/// same substitution before it emits.
pub(crate) fn spirv_validate(name: &str, module: &naga::Module) -> Result<(), String> {
    validate_spirv_module(name, module).map(|_| ())
}

/// Validates `module` exactly as the shader gate does and returns the emitted
/// SPIR-V 1.6 words.
///
/// The ray-query shaders cannot sit in [`validate_engine_shaders`]'s device
/// list (they need `EXPERIMENTAL_RAY_QUERY`), so their test
/// (`tests/hwrt_ray_query_spirv.rs`) calls [`validate_ray_query_spirv_module`]
/// instead of copying the gate: one place owns naga validation,
/// pipeline-override processing, the ADR-026 layout scan with the ADR-135
/// allowlist, and `spirv-val --target-env vulkan1.3`. The test scans the
/// returned words for the `RayQueryKHR` capability and the
/// `SPV_KHR_ray_query` extension itself.
///
/// # Errors
///
/// The reasons of `spirv_validate`, with the words not returned.
pub fn validate_spirv_module(name: &str, module: &naga::Module) -> Result<Vec<u32>, String> {
    validate_spirv_module_inner(name, module, false)
}

/// Like [`validate_spirv_module`], for shaders that use the `ray_query`
/// builtin (the two passes of ADR-3384, which cannot sit in
/// [`validate_engine_shaders`]'s device list because they need
/// `EXPERIMENTAL_RAY_QUERY`).
///
/// The ADR-026 scan is unchanged except that it does not flag the `ray_query`
/// lowering artifacts: the ray query object itself (the `SPV_KHR_ray_query`
/// extension fixes its struct layout) and the intersection struct that
/// `rayQueryGet*Intersection` materializes as a Function variable written only
/// by ray-query instruction results. WGSL has no spelling for either struct,
/// so the exemption cannot mask user code — a user struct or array with an
/// explicit layout in Function or Private storage still fails (see
/// `layout_findings`). Every SPIRV-Tools release and the shipped drivers
/// accept both lowerings.
///
/// # Errors
///
/// As `validate_spirv_module`.
pub fn validate_ray_query_spirv_module(name: &str, module: &naga::Module) -> Result<Vec<u32>, String> {
    validate_spirv_module_inner(name, module, true)
}

fn validate_spirv_module_inner(
    name: &str,
    module: &naga::Module,
    ray_query_exempt: bool,
) -> Result<Vec<u32>, String> {
    let hazards = crate::image_parameter_hazards(module);
    if !hazards.is_empty() {
        return Err(format!(
            "{name}: image op through a texture parameter (crashes Mali's compiler; create the \
             module through compose_source or wgsl_source): {}",
            hazards.join("; ")
        ));
    }
    let words = emit_spirv(name, module)?;
    let warnings = layout_scan_words_with(name, &words, ray_query_exempt)?;
    if !warnings.is_empty() {
        // Known naga output (ADR-135). Printed even when no tracing subscriber
        // is installed, same as the spirv-val fallback note.
        eprintln!(
            "warning: {name}: {NAGA_WORKGROUP_ARRAYSTRIDE_VUID} on Workgroup storage is a known naga 30 output (ADR-135; drivers and SPIRV-Tools 2025.1 accept it): {}",
            warnings.join("; ")
        );
    }
    spirv_val_words(name, &words)?;
    Ok(words)
}

/// The ADR-026 / ADR-135 explicit-layout scan of `words`: a Function or Private hit is the
/// error; the Workgroup hits (warnings) are returned.
pub(crate) fn layout_scan_words(name: &str, words: &[u32]) -> Result<Vec<String>, String> {
    layout_scan_words_with(name, words, false)
}

/// `layout_scan_words`, optionally without the `ray_query` lowering artifacts
/// (`validate_ray_query_spirv_module`).
fn layout_scan_words_with(name: &str, words: &[u32], ray_query_exempt: bool) -> Result<Vec<String>, String> {
    let (findings, names) = layout_findings(words).map_err(|err| format!("{name}: {err}"))?;
    let violations: Vec<String> = findings
        .iter()
        .filter(|finding| !(ray_query_exempt && finding.is_ray_query_object))
        .map(|finding| finding.message(&names))
        .collect();
    let (errors, warnings): (Vec<&str>, Vec<&str>) = violations
        .iter()
        .map(String::as_str)
        .partition(layout_finding_is_error);
    if !errors.is_empty() {
        return Err(format!(
            "{name}: layout scan: {NAGA_WORKGROUP_ARRAYSTRIDE_VUID}: {}",
            errors.join("; ")
        ));
    }
    Ok(warnings.into_iter().map(str::to_string).collect())
}

/// Runs `spirv-val --target-env vulkan1.3` on `words` (ignoring only the ADR-135 VUID).
pub(crate) fn spirv_val_words(name: &str, words: &[u32]) -> Result<(), String> {
    let spawned = Command::new("spirv-val")
        .args(["--target-env", "vulkan1.3", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && spirv_val_fallback_enabled() => {
            warn_spirv_val_fallback();
            return Ok(());
        }
        Err(err) => {
            return Err(format!(
                "{name}: spirv-val: {err} (SPIRV-Tools must be installed; only a gate host without it may set \
                 {SPIRV_VAL_FALLBACK_ENV}=1)"
            ));
        }
    };
    let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| format!("{name}: spirv-val stdin missing"))?;
        stdin
            .write_all(&bytes)
            .map_err(|err| format!("{name}: spirv-val write: {err}"))?;
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .map_err(|err| format!("{name}: spirv-val wait: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let raw = format!("{stderr}{stdout}");
    let kept = spirv_val_text_without_allowlisted_vuid(&raw);
    // Empty kept text is a pass only when the failure cited the allowlisted
    // VUID. A validator that exits non-zero and prints nothing still fails.
    if kept.trim().is_empty() && raw.contains(NAGA_WORKGROUP_ARRAYSTRIDE_VUID) {
        Ok(())
    } else {
        Err(format!("{name}: spirv-val: {kept}"))
    }
}

/// Validates `module` with naga, replaces pipeline-overridable constants with
/// their WGSL defaults, and emits SPIR-V 1.6 with wgpu-hal's desktop options.
fn emit_spirv(name: &str, module: &naga::Module) -> Result<Vec<u32>, String> {
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(module)
    .map_err(|err| format!("{name}: naga validation: {err}"))?;
    let constants = naga::back::PipelineConstants::new();
    let (module, info) = naga::back::pipeline_constants::process_overrides(module, &info, None, &constants)
        .map_err(|err| format!("{name}: pipeline overrides: {err}"))?;
    write_spirv(name, module.as_ref(), info.as_ref())
}

/// SPIR-V 1.6 of a validated `module` whose overrides are already substituted, with wgpu-hal's
/// desktop options.
pub(crate) fn write_spirv(
    name: &str,
    module: &naga::Module,
    info: &naga::valid::ModuleInfo,
) -> Result<Vec<u32>, String> {
    naga::back::spv::write_vec(module, info, &spirv_options(module), None)
        .map_err(|err| format!("{name}: spirv emit: {err}"))
}

/// The ADR-026 errors in one standalone WGSL module: every Function or Private
/// variable whose type carries an explicit layout (`ArrayStride`, `Offset`,
/// `MatrixStride`), which `VUID-StandaloneSpirv-None-10684` forbids.
///
/// It needs no device and no `spirv-val`: the module is parsed and
/// specialised the way [`crate::wgsl_source`] hands it to wgpu, emitted as
/// SPIR-V 1.6 and scanned with [`spirv_explicit_layout_violations`]. Workgroup
/// hits (ADR-135 warnings) are not returned. Crates whose shaders are not in
/// [`validate_engine_shaders`] (2D sprites, VFX, terrain) call it from unit
/// tests so a dynamically indexed local array fails `cargo test` on any host.
///
/// # Errors
///
/// [`RenderError::Shader`] when the source does not parse, does not validate,
/// or cannot be emitted.
pub fn wgsl_function_layout_errors(name: &str, source: &str) -> Result<Vec<String>, RenderError> {
    let mut module = naga::front::wgsl::parse_str(source)
        .map_err(|err| RenderError::Shader(format!("{name}: wgsl parse: {err}")))?;
    crate::specialize_handle_arguments(&mut module);
    let words = emit_spirv(name, &module).map_err(RenderError::Shader)?;
    let found = spirv_explicit_layout_violations(&words)
        .map_err(|err| RenderError::Shader(format!("{name}: {err}")))?;
    Ok(found
        .into_iter()
        .filter(|finding| layout_finding_is_error(&finding.as_str()))
        .collect())
}

/// [`wgsl_function_layout_errors`] for a module composed against the engine library
/// (`compose_source`, no defines): the ADR-026 Function and Private hits of the SPIR-V naga emits.
///
/// # Errors
///
/// [`RenderError::Shader`] when the source does not compose, does not validate, or cannot be
/// emitted.
pub fn composed_function_layout_errors(name: &str, source: &str) -> Result<Vec<String>, RenderError> {
    let module = compose_source(source, name, std::collections::HashMap::new())?;
    let words = emit_spirv(name, &module).map_err(RenderError::Shader)?;
    let found = spirv_explicit_layout_violations(&words)
        .map_err(|err| RenderError::Shader(format!("{name}: {err}")))?;
    Ok(found
        .into_iter()
        .filter(|finding| layout_finding_is_error(&finding.as_str()))
        .collect())
}

/// `true` for a Function or Private explicit-layout hit. A Workgroup hit is the
/// known naga `ArrayStride` warning (ADR-135), not a gate failure.
fn layout_finding_is_error(finding: &&str) -> bool {
    !finding.starts_with("Workgroup variable")
}

fn is_spirv_val_diagnostic_start(line: &str) -> bool {
    let start = line.trim_start();
    start.starts_with("error:") || start.starts_with("warning:") || start.starts_with("fatal")
}

/// `true` when every `VUID-` token in `block` is exactly
/// [`NAGA_WORKGROUP_ARRAYSTRIDE_VUID`] and there is at least one.
fn diagnostic_is_only_allowlisted_vuid(block: &str) -> bool {
    let mut found = false;
    let mut rest = block;
    while let Some(at) = rest.find("VUID-") {
        found = true;
        let tail = &rest[at..];
        let end = tail
            .find(|c: char| c.is_whitespace() || c == ']' || c == ')' || c == ',')
            .unwrap_or(tail.len());
        if &tail[..end] != NAGA_WORKGROUP_ARRAYSTRIDE_VUID {
            return false;
        }
        rest = &tail[end..];
    }
    found
}

/// Drops spirv-val diagnostics that cite only [`NAGA_WORKGROUP_ARRAYSTRIDE_VUID`].
///
/// A diagnostic is a line starting with `error:`, `warning:` or `fatal`, plus
/// the following lines until the next diagnostic. Any other VUID, a diagnostic
/// with no VUID, and text that is not a diagnostic are kept.
fn spirv_val_text_without_allowlisted_vuid(text: &str) -> String {
    let mut kept = String::new();
    let mut block = String::new();
    let mut in_diag = false;
    for line in text.split_inclusive('\n') {
        let is_new = is_spirv_val_diagnostic_start(line);
        if is_new && !block.is_empty() {
            if !(in_diag && diagnostic_is_only_allowlisted_vuid(&block)) {
                kept.push_str(&block);
            }
            block.clear();
        }
        if is_new {
            in_diag = true;
        }
        block.push_str(line);
    }
    if !block.is_empty() && !(in_diag && diagnostic_is_only_allowlisted_vuid(&block)) {
        kept.push_str(&block);
    }
    kept
}

/// Runs `spirv-val` on the forward module a material graph builds (`surface_graph`), for both
/// output encodings (display-encoded and linear HDR).
///
/// # Errors
///
/// [`RenderError::Shader`] on a composition, naga or `spirv-val` failure.
pub fn validate_surface_graph(surface_wgsl: &str) -> Result<(), RenderError> {
    validate_graph_code_spirv(&crate::surface_graph::GraphCode::surface(surface_wgsl))
}

/// Runs `spirv-val` on every module a material graph with `code` builds: the forward module in
/// both output encodings, the instanced GPU-driven colour and depth variants, and with a vertex
/// stage the shadow (CPU and instanced) and velocity modules.
///
/// # Errors
///
/// [`RenderError::Shader`] on a composition, naga or `spirv-val` failure.
pub fn validate_graph_code_spirv(code: &crate::surface_graph::GraphCode) -> Result<(), RenderError> {
    use crate::surface_graph::{code_defs, forward_source_with_code, vertex_host_defs, vertex_host_source};
    let source = forward_source_with_code(code)?;
    for extra in [
        &["ALPHA_TEST"][..],
        &["ALPHA_TEST", "skip_srgb_encode"][..],
        &["GPU_DRIVEN", "GPU_INSTANCED"][..],
        &["GPU_DRIVEN", "GPU_INSTANCED", "ALPHA_TEST", "skip_srgb_encode"][..],
        &["GPU_DRIVEN"][..],
        &["GPU_DRIVEN", "ALPHA_TEST"][..],
    ] {
        let module = compose_source(&source, "lab_forward_graph.wgsl", code_defs(code, extra))?;
        spirv_validate("lab_forward_graph.wgsl", &module).map_err(RenderError::Shader)?;
    }
    if let Some(shadow) = vertex_host_source(code, false)? {
        for extra in [&[][..], &["GPU_DRIVEN"][..]] {
            let module = compose_source(&shadow, "lab_shadow_graph.wgsl", vertex_host_defs(code, extra))?;
            spirv_validate("lab_shadow_graph.wgsl", &module).map_err(RenderError::Shader)?;
        }
    }
    if let Some(velocity) = vertex_host_source(code, true)? {
        let module = compose_source(&velocity, "velocity_graph.wgsl", vertex_host_defs(code, &[]))?;
        spirv_validate("velocity_graph.wgsl", &module).map_err(RenderError::Shader)?;
    }
    Ok(())
}

/// Environment variable that lets a gate host without SPIRV-Tools replace
/// `spirv-val` with naga's validator plus [`spirv_explicit_layout_violations`].
///
/// Only `1` enables it, and only when the `spirv-val` executable is not found.
pub const SPIRV_VAL_FALLBACK_ENV: &str = "REGIA_SPIRV_VAL_FALLBACK";

fn spirv_val_fallback_enabled() -> bool {
    regia_core::brand::env_var(SPIRV_VAL_FALLBACK_ENV).ok().as_deref() == Some("1")
}

/// Says once, loudly, that `spirv-val` did not run: only naga validation and
/// the explicit-layout scan checked the modules.
fn warn_spirv_val_fallback() {
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| {
        eprintln!(
            "WARNING: spirv-val not installed; {SPIRV_VAL_FALLBACK_ENV}=1 -> naga validation + SPIR-V explicit-layout \
             scan only (install SPIRV-Tools for the full validator)"
        );
    });
}

/// Variables in the Function, Private or Workgroup storage class whose type
/// is, or contains, a type with explicit layout decorations (`Offset`,
/// `ArrayStride`, `MatrixStride`).
///
/// SPIR-V 1.5+ for Vulkan forbids that (`VUID-StandaloneSpirv-None-10684`,
/// ADR-026) unless a Workgroup variable is decorated under
/// `WorkgroupMemoryExplicitLayoutKHR`, which naga does not enable. It is the
/// bug naga 30 produces for a struct or array `var` in a function, and for any
/// workgroup `array<T, N>`. The shader checks run it on every module next to
/// `spirv-val` (older `spirv-val` releases do not know the rule, and SPIRV-Tools
/// 2025.1 still accepts the workgroup form) and alone when `spirv-val` is
/// absent and [`SPIRV_VAL_FALLBACK_ENV`] allows it. It reads the
/// module's logical layout as the SPIR-V specification (§ 2.3, § 3) defines
/// it: a 5-word header, then instructions whose first word packs the word
/// count (high 16 bits) and the opcode (low 16 bits).
///
/// Each entry names the variable and the laid-out type. An entry that starts
/// with `Workgroup variable` is a warning under ADR-135 (naga's `ArrayStride`
/// on Workgroup storage; drivers accept it). `Function` and `Private` entries
/// are errors. An `Err` means the words are not a well-formed SPIR-V module.
pub fn spirv_explicit_layout_violations(words: &[u32]) -> Result<Vec<String>, String> {
    let (findings, names) = layout_findings(words)?;
    Ok(findings.iter().map(|finding| finding.message(&names)).collect())
}

/// One ADR-026 finding: a Workgroup, Private or Function variable whose type
/// tree carries an explicit layout decoration.
#[derive(Debug)]
struct LayoutFinding {
    /// SPIR-V storage class: Workgroup 4, Private 6, Function 7.
    storage: u32,
    /// The variable's id.
    variable: u32,
    /// The laid-out type's id.
    laid_out: u32,
    /// Whether the variable is a `ray_query` builtin object (see
    /// [`layout_findings`]).
    is_ray_query_object: bool,
}

impl LayoutFinding {
    /// The gate's message format, unchanged since ADR-026.
    fn message(&self, names: &HashMap<u32, String>) -> String {
        const WORKGROUP: u32 = 4;
        const PRIVATE: u32 = 6;
        const FUNCTION: u32 = 7;
        let class = match self.storage {
            FUNCTION => "Function",
            PRIVATE => "Private",
            WORKGROUP => "Workgroup",
            _ => "Other",
        };
        let var = names
            .get(&self.variable)
            .map_or(String::new(), |name| format!(" ({name})"));
        let laid = names
            .get(&self.laid_out)
            .map_or(String::new(), |name| format!(" ({name})"));
        format!(
            "{class} variable %{}{var} uses explicitly laid out type %{}{laid}",
            self.variable, self.laid_out
        )
    }
}

/// Structured form of [`spirv_explicit_layout_violations`].
///
/// The scan additionally records which findings are `ray_query` builtin
/// lowering artifacts, which the gate never sees (ray-query shaders are not
/// in [`validate_engine_shaders`]) and every SPIRV-Tools release and the
/// shipped drivers accept:
///
/// - the ray query object itself: naga lowers `var rq: ray_query` to a
///   Function variable of the extension's fixed-layout struct type, routed
///   through wrapper functions whose first parameter is the object; and
/// - the intersection struct that `rayQueryGet*Intersection` materializes as
///   a second Function variable, written only by ray-query instruction
///   results and read back whole.
///
/// Neither struct is spellable in WGSL, so no user code can share the
/// exemption. `is_ray_query_object` is set for a variable when its pointer
/// type is the type of an `OpRayQueryInitializeKHR` wrapper's object
/// parameter, or when every store into it (through access chains) writes a
/// ray-derived value: an expression whose leaves are all ray-query results or
/// constants with at least one ray-query result among them. A user struct or
/// array with an explicit layout in Function storage is still flagged: its
/// stores mention user variables or non-constant expressions.
fn layout_findings(words: &[u32]) -> Result<(Vec<LayoutFinding>, HashMap<u32, String>), String> {
    const MAGIC: u32 = 0x0723_0203;
    const OP_NAME: u32 = 5;
    const OP_TYPE_ARRAY: u32 = 28;
    const OP_TYPE_RUNTIME_ARRAY: u32 = 29;
    const OP_TYPE_STRUCT: u32 = 30;
    const OP_TYPE_POINTER: u32 = 32;
    const OP_FUNCTION_PARAMETER: u32 = 55;
    const OP_VARIABLE: u32 = 59;
    const OP_STORE: u32 = 62;
    const OP_ACCESS_CHAIN: u32 = 65;
    const OP_IN_BOUNDS_ACCESS_CHAIN: u32 = 66;
    const OP_DECORATE: u32 = 71;
    const OP_MEMBER_DECORATE: u32 = 72;
    const OP_FUNCTION: u32 = 54;
    const OP_FUNCTION_END: u32 = 56;
    const OP_FUNCTION_CALL: u32 = 57;
    const OP_CONSTANT_TRUE: u32 = 41;
    const OP_CONSTANT_FALSE: u32 = 42;
    const OP_CONSTANT: u32 = 43;
    const OP_CONSTANT_COMPOSITE: u32 = 44;
    const OP_CONSTANT_SAMPLER: u32 = 45;
    const OP_CONSTANT_NULL: u32 = 46;
    const OP_SPEC_CONSTANT_TRUE: u32 = 48;
    const OP_SPEC_CONSTANT_FALSE: u32 = 49;
    const OP_SPEC_CONSTANT: u32 = 50;
    const OP_SPEC_CONSTANT_COMPOSITE: u32 = 51;
    const OP_SPEC_CONSTANT_OP: u32 = 52;
    const OP_COPY_OBJECT: u32 = 83;
    const OP_BITCAST: u32 = 124;
    const OP_SELECT: u32 = 169;
    // OpIEqual (170) through OpFUnordGreaterThanEqual (191).
    const OP_COMPARE_FIRST: u32 = 170;
    const OP_COMPARE_LAST: u32 = 191;
    const OP_PHI: u32 = 245;
    const OP_RAY_QUERY_INITIALIZE: u32 = 4473;
    // OpRayQuery* "get" results, public SPIR-V registry: OpRayQueryProceedKHR
    // (4477) and OpRayQueryGetIntersectionTypeKHR (4479), the OpRayQueryGet*
    // block (6016-6032), and OpRayQueryGetIntersectionTriangleVertexPositionsKHR
    // (5340, the hit-vertex-return extension). 4473-4476 return nothing. All
    // of these have their result id as the second operand.
    const RAY_QUERY_RESULT_OPS: [u32; 20] = [
        4477, 4479, 5340, 6016, 6017, 6018, 6019, 6020, 6021, 6022, 6023, 6024, 6025, 6026, 6027, 6028, 6029,
        6030, 6031, 6032,
    ];
    const ARRAY_STRIDE: u32 = 6;
    const MATRIX_STRIDE: u32 = 7;
    const OFFSET: u32 = 35;
    // SPIR-V storage classes (§ 3.7): Workgroup 4, Private 6, Function 7.
    const WORKGROUP: u32 = 4;
    const PRIVATE: u32 = 6;
    const FUNCTION: u32 = 7;

    if words.len() < 5 || words[0] != MAGIC {
        return Err("not a SPIR-V module (bad header)".to_string());
    }
    // Pass 1: naga wraps `rayQueryInitialize` in a helper function whose first
    // parameter is the ray query object, so the object's pointer type is the
    // marker that travels to the caller's OpVariable. Collect every parameter
    // id's type and every initialize target, then intersect.
    let mut parameter_pointer: HashMap<u32, u32> = HashMap::new();
    let mut init_operands = Vec::new();
    let mut at = 5;
    while at < words.len() {
        let count = (words[at] >> 16) as usize;
        let opcode = words[at] & 0xffff;
        if count == 0 || at + count > words.len() {
            return Err(format!("truncated instruction at word {at}"));
        }
        let ops = &words[at + 1..at + count];
        match (opcode, ops) {
            (OP_FUNCTION_PARAMETER, [ty, id, ..]) => {
                parameter_pointer.insert(*id, *ty);
            }
            (OP_RAY_QUERY_INITIALIZE, [ray, ..]) => {
                init_operands.push(*ray);
            }
            _ => {}
        }
        at += count;
    }
    let ray_query_pointer_types: HashSet<u32> = init_operands
        .iter()
        .filter_map(|operand| parameter_pointer.get(operand))
        .copied()
        .collect();

    let mut laid_out = HashSet::new();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut pointee: HashMap<u32, u32> = HashMap::new();
    let mut names: HashMap<u32, String> = HashMap::new();
    let mut variables = Vec::new();
    // Every OpStore (pointer, value): the pointer may be an access chain, so
    // resolve it to its base variable through `access_base` before use. Ray
    // values are the results of ray-query instructions and of calls to
    // functions whose body contains one, since naga wraps every ray op in a
    // helper function.
    let mut stores: Vec<(u32, u32)> = Vec::new();
    let mut access_base: HashMap<u32, u32> = HashMap::new();
    let mut ray_query_results = HashSet::new();
    let mut calls: Vec<(u32, u32)> = Vec::new();
    let mut wrapper_functions = HashSet::new();
    let mut current_function: Option<u32> = None;
    // Expression nodes naga emits when it materializes the intersection
    // struct: result id -> value operands (result types and branch labels
    // excluded; a select's condition is a value operand like any other).
    let mut expression_operands: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut constants = HashSet::new();
    let mut at = 5;
    while at < words.len() {
        let count = (words[at] >> 16) as usize;
        let opcode = words[at] & 0xffff;
        if count == 0 || at + count > words.len() {
            return Err(format!("truncated instruction at word {at}"));
        }
        let ops = &words[at + 1..at + count];
        match (opcode, ops) {
            (OP_NAME, [target, text @ ..]) => {
                let bytes: Vec<u8> = text.iter().flat_map(|word| word.to_le_bytes()).collect();
                let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                names.insert(*target, String::from_utf8_lossy(&bytes[..end]).into_owned());
            }
            (OP_TYPE_STRUCT, [id, members @ ..]) => {
                children.insert(*id, members.to_vec());
            }
            (OP_TYPE_ARRAY | OP_TYPE_RUNTIME_ARRAY, [id, element, ..]) => {
                children.insert(*id, vec![*element]);
            }
            (OP_TYPE_POINTER, [id, _storage, ty]) => {
                pointee.insert(*id, *ty);
            }
            (OP_VARIABLE, [ty, id, storage, ..])
                if *storage == FUNCTION || *storage == PRIVATE || *storage == WORKGROUP =>
            {
                variables.push((*id, *ty, *storage));
            }
            (OP_DECORATE, [target, ARRAY_STRIDE, ..]) => {
                laid_out.insert(*target);
            }
            (OP_MEMBER_DECORATE, [target, _member, OFFSET | MATRIX_STRIDE, ..]) => {
                laid_out.insert(*target);
            }
            (OP_STORE, [ptr, value, ..]) => {
                stores.push((*ptr, *value));
            }
            (OP_ACCESS_CHAIN | OP_IN_BOUNDS_ACCESS_CHAIN, [_, result, base, ..]) => {
                access_base.insert(*result, *base);
            }
            (OP_FUNCTION, [_, id, ..]) => {
                current_function = Some(*id);
            }
            (OP_FUNCTION_END, []) => {
                current_function = None;
            }
            (OP_FUNCTION_CALL, [_, result, callee, ..]) => {
                calls.push((*result, *callee));
            }
            (op, [_, result, ..]) if RAY_QUERY_RESULT_OPS.contains(&op) => {
                ray_query_results.insert(*result);
                if let Some(function) = current_function {
                    wrapper_functions.insert(function);
                }
            }
            (
                OP_CONSTANT_TRUE
                | OP_CONSTANT_FALSE
                | OP_CONSTANT
                | OP_CONSTANT_COMPOSITE
                | OP_CONSTANT_SAMPLER
                | OP_CONSTANT_NULL
                | OP_SPEC_CONSTANT_TRUE
                | OP_SPEC_CONSTANT_FALSE
                | OP_SPEC_CONSTANT
                | OP_SPEC_CONSTANT_COMPOSITE
                | OP_SPEC_CONSTANT_OP,
                [_, result, ..],
            ) => {
                constants.insert(*result);
            }
            (OP_COPY_OBJECT | OP_BITCAST, [_, result, value, ..]) => {
                expression_operands.insert(*result, vec![*value]);
            }
            (OP_COMPARE_FIRST..=OP_COMPARE_LAST, [_, result, rest @ ..]) => {
                expression_operands.insert(*result, rest.to_vec());
            }
            (OP_SELECT, [_, result, rest @ ..]) => {
                expression_operands.insert(*result, rest.to_vec());
            }
            (OP_PHI, [_, result, rest @ ..]) => {
                expression_operands.insert(*result, rest.iter().step_by(2).copied().collect());
            }
            _ => {}
        }
        at += count;
    }

    let ray_call_results: HashSet<u32> = calls
        .iter()
        .filter(|(_, callee)| wrapper_functions.contains(callee))
        .map(|(result, _)| *result)
        .collect();
    // Whether every store into `base` (through access chains) writes a
    // ray-derived value, with at least one store present. A value is
    // ray-derived when every leaf of its expression is a ray-query result (a
    // ray instruction's own result, or a call to a wrapper function whose
    // body contains one) or a constant, with at least one ray-query result
    // among the leaves, so an all-constant user store is never exempted.
    // Memoized; cycles resolve to false on revisit.
    let is_intersection_slot = |base: u32| {
        fn ray_derived(
            id: u32,
            ray_query_results: &HashSet<u32>,
            ray_call_results: &HashSet<u32>,
            constants: &HashSet<u32>,
            expression_operands: &HashMap<u32, Vec<u32>>,
            memo: &mut HashMap<u32, bool>,
            deriving: &mut HashSet<u32>,
        ) -> bool {
            if let Some(&out) = memo.get(&id) {
                return out;
            }
            if ray_query_results.contains(&id) || ray_call_results.contains(&id) {
                return true;
            }
            if constants.contains(&id) {
                return false;
            }
            if !deriving.insert(id) {
                return false;
            }
            let out = expression_operands.get(&id).is_some_and(|operands| {
                let mut saw_ray = false;
                let all_leaves_ok = operands.iter().all(|operand| {
                    let derived = ray_derived(
                        *operand,
                        ray_query_results,
                        ray_call_results,
                        constants,
                        expression_operands,
                        memo,
                        deriving,
                    );
                    saw_ray |= derived;
                    derived || constants.contains(operand)
                });
                all_leaves_ok && saw_ray
            });
            deriving.remove(&id);
            memo.insert(id, out);
            out
        }
        let mut memo = HashMap::new();
        let mut deriving = HashSet::new();
        let mut stored = false;
        let mut only_ray_values = true;
        for (ptr, value) in &stores {
            let mut root = *ptr;
            while let Some(&next) = access_base.get(&root) {
                root = next;
            }
            if root == base {
                stored = true;
                only_ray_values &= ray_derived(
                    *value,
                    &ray_query_results,
                    &ray_call_results,
                    &constants,
                    &expression_operands,
                    &mut memo,
                    &mut deriving,
                );
            }
        }
        stored && only_ray_values
    };

    let mut found = Vec::new();
    for (id, pointer, storage) in variables {
        let Some(&ty) = pointee.get(&pointer) else {
            continue;
        };
        let is_ray_query_object = ray_query_pointer_types.contains(&pointer) || is_intersection_slot(id);
        let mut stack = vec![ty];
        let mut seen = HashSet::new();
        while let Some(current) = stack.pop() {
            if !seen.insert(current) {
                continue;
            }
            if laid_out.contains(&current) {
                found.push(LayoutFinding {
                    storage,
                    variable: id,
                    laid_out: current,
                    is_ray_query_object,
                });
                break;
            }
            if let Some(next) = children.get(&current) {
                stack.extend(next.iter().copied());
            }
        }
    }
    Ok((found, names))
}

/// Creates every `regia_render` shader module and the pipelines used by a
/// lab frame and a Desktop High frame. Fails on any Vulkan validation error.
///
/// `spirv-val` runs first. wgpu-hal swallows `VUID-StandaloneSpirv-None-10684`,
/// so a clean device log is not enough. The instance is created with
/// validation forced on, including release builds.
pub fn validate_engine_shaders() -> Result<(), RenderError> {
    install_logger()?;
    if let Ok(mut lines) = LINES.lock() {
        lines.clear();
    }

    let mut spirv_problems = Vec::new();
    let mut composed = Vec::new();
    for (name, source, shader_defs) in shader_sources() {
        let module = compose_source(source, name, shader_defs)?;
        if let Err(err) = spirv_validate(name, &module) {
            spirv_problems.push(err);
        }
        composed.push((name, module));
    }
    if let Err(err) = validate_surface_graph(crate::surface_graph::SAMPLE_SURFACE) {
        spirv_problems.push(err.to_string());
    }
    // A graph with a vertex stage and the second UV set: every host module it splices into.
    let staged = crate::surface_graph::GraphCode {
        surface: crate::surface_graph::SAMPLE_SURFACE.into(),
        vertex: Some(crate::surface_graph::SAMPLE_VERTEX.into()),
        uv1: true,
    };
    if let Err(err) = validate_graph_code_spirv(&staged) {
        spirv_problems.push(err.to_string());
    }
    if !spirv_problems.is_empty() {
        return Err(RenderError::Shader(spirv_problems.join("\n")));
    }

    let device = validation_device()?;
    let scope = device.device.push_error_scope(wgpu::ErrorFilter::Validation);

    let mut modules = Vec::new();
    for (name, module) in composed {
        modules.push(device.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(name),
            source: wgpu::ShaderSource::Naga(Cow::Owned(module)),
        }));
    }

    let material = LabMaterial {
        base_color: [0.7, 0.7, 0.7, 1.0],
        metallic: 0.0,
        roughness: 0.6,
        ..LabMaterial::EMPTY
    };
    let vertices = [
        LabVertex::from_pos_normal_uv([-1.0, 0.0, -1.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([1.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([1.0, 0.0, 1.0], [0.0, 1.0, 0.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([-1.0, 0.0, 1.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
    ];
    let settings = PostSettings {
        auto_exposure: true,
        bloom: true,
        bloom_mips: 3,
        taa: true,
        ..PostSettings::default()
    };
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("shader check"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(&device, true);
    // The lens passes (motion blur, both depth of field kernels) on the Desktop High frame.
    let lens = |mode| PostSettings {
        motion_blur: Some(crate::MotionBlur::default()),
        depth_of_field: Some(crate::DepthOfField {
            mode,
            focal_distance: 2.0,
            ..crate::DepthOfField::default()
        }),
        ..settings.clone()
    };
    let bokeh = lens(crate::DepthOfFieldMode::Bokeh);
    let gaussian = lens(crate::DepthOfFieldMode::Gaussian);
    let desktop = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    for (quality, settings) in [
        (FrameQuality::LAB, &settings),
        (desktop, &settings),
        (desktop, &bokeh),
        (desktop, &gaussian),
        // SMAA after the tonemap and the SGSR upscale onto the target.
        (
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::MobileHigh)),
            &settings,
        ),
    ] {
        let scene = quad_scene(quality, &material, &vertices, settings);
        for _ in 0..2 {
            renderer.render_into(&device, &scene, FrameTarget::full(&view, format, 64, 64))?;
        }
    }
    drop(modules);

    if device.device.poll(wgpu::PollType::Poll).is_err() {
        tracing::warn!(target: "regia_render", "shader-check poll failed");
    }
    let mut problems = Vec::new();
    if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
        problems.push(err.to_string());
    }
    if let Ok(lines) = LINES.lock() {
        problems.extend(validation_hits(&lines));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(RenderError::Shader(problems.join("\n")))
    }
}

/// Parses each `(name, wgsl)` source, runs `spirv-val`, and creates the module the way
/// [`crate::wgsl_source`] does (texture parameters specialised away).
///
/// Crates outside `regia_render` (terrain, UI, VFX, lookdev) have no pipeline
/// set here. [`validate_engine_shaders`] still builds the lab, Desktop High and
/// Mobile High pipelines. One device is created for the whole list.
pub fn validate_wgsl_modules(sources: &[(&str, &str)]) -> Result<(), RenderError> {
    install_logger()?;
    if let Ok(mut lines) = LINES.lock() {
        lines.clear();
    }

    let mut spirv_problems = Vec::new();
    for (name, source) in sources {
        let mut module = naga::front::wgsl::parse_str(source)
            .map_err(|err| RenderError::Shader(format!("{name}: wgsl parse: {err}")))?;
        // What `wgsl_source` hands wgpu.
        crate::specialize_handle_arguments(&mut module);
        if let Err(err) = spirv_validate(name, &module) {
            spirv_problems.push(err);
        }
    }
    if !spirv_problems.is_empty() {
        return Err(RenderError::Shader(spirv_problems.join("\n")));
    }

    create_modules_checked(
        sources
            .iter()
            .map(|(name, source)| (*name, crate::wgsl_source(source)))
            .collect(),
    )
}

/// A headless device with Vulkan validation on, for the checks that create modules and frames.
///
/// A check run (`regia shader-check`, a CLI or agent call) opens one. This crate's unit tests use
/// the device they already share instead: on an NVIDIA adapter every device a unit test opens is
/// kept until the process exits (`keep_for_test_run`, ADR-1584), and late in the lib run a fresh
/// `vkCreateDevice` failed with `VK_ERROR_INITIALIZATION_FAILED`, which wgpu reports as "Parent
/// device is lost" (the v045b and v047 gates' GPU pass, ADR-5554).
#[cfg(not(test))]
fn validation_device() -> Result<RenderDevice, RenderError> {
    open_validation_device()
}

/// The unit tests' `validation_device` (see there). Test builds carry debug assertions, so the
/// instance flags already hold `VALIDATION` and the shared `crate::test_device()`, which the first
/// renderer test opens early in the run, is a validation device. Without that flag (a release
/// test build, `WGPU_VALIDATION=0`) one validation device is opened and shared.
#[cfg(test)]
fn validation_device() -> Result<RenderDevice, RenderError> {
    if crate::device::instance_flags().contains(wgpu::InstanceFlags::VALIDATION) {
        return Ok(crate::test_device());
    }
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    if let Some(device) = DEVICE.get() {
        return Ok(device.clone());
    }
    let device = open_validation_device()?;
    Ok(DEVICE.get_or_init(|| device).clone())
}

fn open_validation_device() -> Result<RenderDevice, RenderError> {
    let mut flags = crate::device::instance_flags();
    flags.insert(wgpu::InstanceFlags::VALIDATION);
    RenderDevice::headless_with_flags(flags)
}

/// Creates each module on one validation-enabled device and returns every validation message.
fn create_modules_checked(modules: Vec<(&str, wgpu::ShaderSource<'_>)>) -> Result<(), RenderError> {
    let device = validation_device()?;
    let scope = device.device.push_error_scope(wgpu::ErrorFilter::Validation);
    for (name, source) in modules {
        let _module = device.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(name),
            source,
        });
    }
    if device.device.poll(wgpu::PollType::Poll).is_err() {
        tracing::warn!(target: "regia_render", "shader-check poll failed");
    }
    let mut problems = Vec::new();
    if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
        problems.push(err.to_string());
    }
    if let Ok(lines) = LINES.lock() {
        problems.extend(validation_hits(&lines));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(RenderError::Shader(problems.join("\n")))
    }
}

/// [`validate_wgsl_modules`] for modules the renderer composes against the engine library
/// (`compose_source`, no defines), such as the path tracer's `pt.wgsl` and `lightmap_bake.wgsl`
/// (ADR-2664): each `(file, source)` is composed, runs the SPIR-V layout scan and `spirv-val`,
/// and is created on a validation-enabled device.
///
/// # Errors
///
/// [`RenderError::Shader`] with every composition, SPIR-V or Vulkan validation message.
pub fn validate_composed_modules(sources: &[(&str, &str)]) -> Result<(), RenderError> {
    install_logger()?;
    if let Ok(mut lines) = LINES.lock() {
        lines.clear();
    }
    let mut problems = Vec::new();
    let mut modules = Vec::new();
    for (name, source) in sources {
        match compose_source(source, name, std::collections::HashMap::new()) {
            Ok(module) => match spirv_validate(name, &module) {
                Ok(()) => modules.push((*name, wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)))),
                Err(err) => problems.push(err),
            },
            Err(err) => problems.push(format!("{name}: compose: {err}")),
        }
    }
    if !problems.is_empty() {
        return Err(RenderError::Shader(problems.join("\n")));
    }
    create_modules_checked(modules)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_shaders_pass_vulkan_validation() {
        match validate_engine_shaders() {
            Ok(()) => {}
            Err(err) => panic!("{err}"),
        }
    }

    #[test]
    fn function_local_struct_var_fails_spirv_val() {
        let source = r#"
struct SurfaceLocal {
    albedo: vec3<f32>,
    roughness: f32,
}

@compute @workgroup_size(1)
fn main() {
    var local: SurfaceLocal;
    local.albedo = vec3<f32>(1.0, 1.0, 1.0);
    local.roughness = 0.5;
}
"#;
        let module = compose_source(source, "vuid_probe.wgsl", HashMap::new()).expect("compose");
        let err = spirv_validate("vuid_probe.wgsl", &module).expect_err("function-local struct var");
        assert!(
            err.contains("VUID-StandaloneSpirv-None-10684"),
            "gate did not catch the explicit-layout bug: {err}"
        );
    }

    /// A workgroup array is naga's known `ArrayStride` output (ADR-135).
    ///
    /// The scan still reports `VUID-StandaloneSpirv-None-10684`, as a warning.
    /// `spirv_validate` does not fail: SPIRV-Tools 2026.4's diagnostic for that
    /// exact VUID is allowlisted, and 2025.1 does not emit it. Scalar and vector
    /// workgroup variables carry no explicit layout and stay silent.
    #[test]
    fn workgroup_array_is_a_layout_warning_and_vectors_pass() {
        let bad = r#"
var<workgroup> rows: array<f32, 4>;

@compute @workgroup_size(1)
fn main() {
    rows[0] = 1.0;
}
"#;
        let module = compose_source(bad, "wg_array.wgsl", HashMap::new()).expect("compose");
        let found = spirv_explicit_layout_violations(&emit_words(&module)).expect("well-formed");
        assert!(
            found.len() == 1 && found[0].starts_with("Workgroup variable"),
            "scan did not report the workgroup array: {found:?}"
        );
        assert!(!layout_finding_is_error(&found[0].as_str()));
        spirv_validate("wg_array.wgsl", &module).expect("workgroup array is a warning, not an error");

        let good = r#"
var<workgroup> lane: vec4<f32>;
var<workgroup> tile: mat4x4<f32>;
var<workgroup> count: atomic<u32>;

@compute @workgroup_size(1)
fn main() {
    let t = lane;
    let c0 = tile[0];
    lane = vec4<f32>(c0.x, t.y, t.z, t.w);
    tile = mat4x4<f32>(vec4<f32>(1.0), tile[1], tile[2], tile[3]);
    atomicStore(&count, 1u);
}
"#;
        let module = compose_source(good, "wg_vec.wgsl", HashMap::new()).expect("compose");
        let found = spirv_explicit_layout_violations(&emit_words(&module)).expect("well-formed");
        assert!(found.is_empty(), "vectors are not explicitly laid out: {found:?}");
        spirv_validate("wg_vec.wgsl", &module).expect("workgroup vectors are not explicitly laid out");
    }

    /// The allowlist is the one VUID id. Another VUID in the same stream, or in
    /// the same diagnostic, still fails.
    #[test]
    fn allowlist_drops_only_vuid_10684() {
        let only = "error: [VUID-StandaloneSpirv-None-10684] Invalid explicit layout\n  \
                    %6 = OpVariable %_ptr_Workgroup__arr_uint Workgroup\n";
        assert!(
            spirv_val_text_without_allowlisted_vuid(only).trim().is_empty(),
            "the known VUID must be dropped"
        );
        let other = "error: [VUID-StandaloneSpirv-None-10684] layout\n  OpVariable Workgroup\n\
                     error: [VUID-StandaloneSpirv-Uniform-06676] missing Block\n  %s = OpTypeStruct\n";
        let kept = spirv_val_text_without_allowlisted_vuid(other);
        assert!(kept.contains("VUID-StandaloneSpirv-Uniform-06676"), "{kept}");
        assert!(!kept.contains("10684"), "{kept}");
        let mixed = "error: [VUID-StandaloneSpirv-None-10684] and [VUID-StandaloneSpirv-Uniform-06676]\n";
        let kept = spirv_val_text_without_allowlisted_vuid(mixed);
        assert!(kept.contains("06676") && kept.contains("10684"), "{kept}");
        assert!(!diagnostic_is_only_allowlisted_vuid("error: no vuid here\n"));
    }

    /// The SPIR-V words `spirv_validate` hands to `spirv-val`, without running it.
    fn emit_words(module: &naga::Module) -> Vec<u32> {
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(module)
        .expect("naga validation");
        let constants = naga::back::PipelineConstants::new();
        let (module, info) =
            naga::back::pipeline_constants::process_overrides(module, &info, None, &constants)
                .expect("overrides");
        naga::back::spv::write_vec(
            module.as_ref(),
            info.as_ref(),
            &spirv_options(module.as_ref()),
            None,
        )
        .expect("spirv emit")
    }

    #[test]
    fn layout_scan_flags_function_local_struct_and_spilled_array() {
        let source = r#"
struct SurfaceLocal {
    albedo: vec3<f32>,
    roughness: f32,
}

@group(0) @binding(0) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    var local: SurfaceLocal;
    local.roughness = 0.5;
    let rows = array<f32, 4>(1.0, 2.0, 3.0, 4.0);
    out[0] = local.roughness + rows[id.x & 3u];
}
"#;
        let module = compose_source(source, "layout_probe.wgsl", HashMap::new()).expect("compose");
        let found = spirv_explicit_layout_violations(&emit_words(&module)).expect("well-formed");
        // One for `var local`, one for the Function local naga spills `rows[id.x & 3u]` into.
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found.iter().all(|v| v.starts_with("Function variable")),
            "{found:?}"
        );
    }

    /// The ray-query gate variant exempts only the `ray_query` builtin's own
    /// Function object; a user function-local struct with an explicit layout
    /// is still an ADR-026 error there (audit rescue-hwrt-lavapipe-coverage
    /// finding 4: the exemption cannot mask user code).
    #[test]
    fn ray_query_exemption_keeps_user_layout_errors() {
        let source = r#"
struct SurfaceLocal {
    albedo: vec3<f32>,
    roughness: f32,
}

@group(0) @binding(0) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    var local: SurfaceLocal;
    local.roughness = 0.5;
    out[0] = local.roughness;
}
"#;
        let module = compose_source(source, "layout_probe.wgsl", HashMap::new()).expect("compose");
        let err = validate_ray_query_spirv_module("layout_probe.wgsl", &module)
            .expect_err("a user function-local struct stays an error");
        assert!(err.contains("layout scan"), "{err}");
    }

    /// `var rq: ray_query` lowers to a Function variable of the extension's
    /// laid-out struct type; the scan classifies exactly it as the builtin
    /// object, so the gate variant can exempt it without touching user code.
    #[test]
    fn ray_query_object_finding_is_classified() {
        // Both ray-query shaders: the shadow pass materializes one laid-out
        // struct, the sun trace two (a select of constants on a ray-derived
        // condition reaches one of them).
        for source in [
            crate::hwrt::RAY_QUERY_SHADOW_WGSL,
            crate::hwrt::SUN_TRACE_RAY_QUERY_WGSL,
        ] {
            let mut module = naga::front::wgsl::parse_str(source).expect("parse");
            crate::specialize_handle_arguments(&mut module);
            let (findings, _names) = layout_findings(&emit_words(&module)).expect("well-formed");
            assert!(
                !findings.is_empty(),
                "the ray query lowering is explicitly laid out"
            );
            assert!(
                findings
                    .iter()
                    .all(|finding| finding.is_ray_query_object && finding.storage == 7),
                "{findings:?}"
            );
        }
    }

    /// A dynamically indexed local corner table (the old boot splash, sprite and
    /// soft-particle vertex shaders) is an ADR-026 error; a `switch` table is not.
    #[test]
    fn standalone_layout_check_flags_an_indexed_local_array() {
        let bad = r#"
@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    var corners = array<vec2<f32>, 3>(vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0));
    return vec4<f32>(corners[index], 0.0, 1.0);
}
"#;
        let found = wgsl_function_layout_errors("indexed_corners.wgsl", bad).expect("emits");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("Function variable"), "{found:?}");

        let good = r#"
@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    var c: vec2<f32>;
    switch index {
        case 1u: { c = vec2<f32>(1.0, 0.0); }
        case 2u: { c = vec2<f32>(0.0, 1.0); }
        default: { c = vec2<f32>(0.0, 0.0); }
    }
    return vec4<f32>(c, 0.0, 1.0);
}
"#;
        let found = wgsl_function_layout_errors("switch_corners.wgsl", good).expect("emits");
        assert!(found.is_empty(), "{found:?}");
        assert!(wgsl_function_layout_errors("broken.wgsl", "fn (").is_err());
    }

    /// The boot splash is created before any frame, through plain WGSL.
    #[test]
    fn boot_splash_has_no_function_layout_errors() {
        let found =
            wgsl_function_layout_errors("boot-splash.wgsl", include_str!("../shaders/boot-splash.wgsl"))
                .expect("boot splash emits");
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn layout_scan_passes_every_engine_shader_and_rejects_garbage() {
        let mut workgroup_warnings = 0usize;
        for (name, source, shader_defs) in shader_sources() {
            let module = compose_source(source, name, shader_defs).expect("compose");
            let found = spirv_explicit_layout_violations(&emit_words(&module)).expect("well-formed");
            let errors: Vec<_> = found
                .iter()
                .filter(|f| layout_finding_is_error(&f.as_str()))
                .collect();
            assert!(errors.is_empty(), "{name}: {errors:?}");
            workgroup_warnings += found.len() - errors.len();
        }
        assert!(
            workgroup_warnings > 0,
            "the scan no longer reports naga's Workgroup ArrayStride"
        );
        assert!(spirv_explicit_layout_violations(&[1, 2, 3]).is_err());
        assert!(spirv_explicit_layout_violations(&[0x0723_0203, 0, 0, 0, 0, 0x0005_0005]).is_err());
    }
}
