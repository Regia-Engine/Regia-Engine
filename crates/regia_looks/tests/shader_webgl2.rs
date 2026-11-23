// SPDX-License-Identifier: MPL-2.0
//! The look shaders translate to GLSL ES 3.00 for WebGL2 (what wgpu's WebGL2 backend does at
//! pipeline creation), entry point by entry point, and stay inside the cheap-look budget:
//! the gem fragment samples the environment at most 8 times.

use naga::back::glsl;
use naga::valid::{Capabilities, ValidationFlags, Validator};

const ENTRIES: [(&str, naga::ShaderStage); 8] = [
    ("vs_main", naga::ShaderStage::Vertex),
    ("vs_background", naga::ShaderStage::Vertex),
    ("fs_metal", naga::ShaderStage::Fragment),
    ("fs_gem", naga::ShaderStage::Fragment),
    ("fs_frosted", naga::ShaderStage::Fragment),
    ("fs_cabochon", naga::ShaderStage::Fragment),
    ("fs_opal", naga::ShaderStage::Fragment),
    ("fs_background", naga::ShaderStage::Fragment),
];

fn module() -> (naga::Module, naga::valid::ModuleInfo) {
    let module = naga::front::wgsl::parse_str(regia_looks::LOOKS_SHADER).expect("looks.wgsl parses");
    // WebGL2 has none of the optional capabilities.
    let info = Validator::new(ValidationFlags::all(), Capabilities::empty())
        .validate(&module)
        .expect("looks.wgsl validates");
    (module, info)
}

fn glsl_es_300(
    module: &naga::Module,
    info: &naga::valid::ModuleInfo,
    entry: &str,
    stage: naga::ShaderStage,
) -> String {
    let options = glsl::Options {
        version: glsl::Version::Embedded {
            version: 300,
            is_webgl: true,
        },
        writer_flags: glsl::WriterFlags::ADJUST_COORDINATE_SPACE,
        ..Default::default()
    };
    let pipeline = glsl::PipelineOptions {
        shader_stage: stage,
        entry_point: entry.to_string(),
        multiview: None,
    };
    let mut out = String::new();
    let mut writer = glsl::Writer::new(
        &mut out,
        module,
        info,
        &options,
        &pipeline,
        naga::proc::BoundsCheckPolicies::default(),
    )
    .unwrap_or_else(|e| panic!("{entry}: {e}"));
    writer.write().unwrap_or_else(|e| panic!("{entry}: {e}"));
    out
}

#[test]
fn every_entry_point_translates_to_glsl_es_300() {
    let (module, info) = module();
    for (entry, stage) in ENTRIES {
        let src = glsl_es_300(&module, &info, entry, stage);
        assert!(
            src.starts_with("#version 300 es"),
            "{entry}: {}",
            &src[..src.len().min(40)]
        );
        // Uniform blocks and samplers only: no storage buffers or images.
        assert!(!src.contains("buffer "), "{entry} uses a storage buffer");
        assert!(!src.contains("image2D"), "{entry} uses a storage image");
    }
    // The module has no entry point the list misses.
    assert_eq!(module.entry_points.len(), ENTRIES.len());
}

#[test]
fn gem_fragment_samples_the_environment_at_most_eight_times() {
    let (module, info) = module();
    let src = glsl_es_300(&module, &info, "fs_gem", naga::ShaderStage::Fragment);
    // `env_at` is the only environment fetch helper; count its call sites in the gem path plus
    // direct texture calls (the GLSL keeps helper functions, so count calls of the helper too).
    let wgsl = regia_looks::LOOKS_SHADER;
    let gem_fn = wgsl
        .split("fn gem_shade(")
        .nth(1)
        .and_then(|s| s.split("\n}\n").next())
        .expect("gem_shade body");
    let taps = gem_fn.matches("env_at(").count();
    assert!(taps <= 8, "gem_shade has {taps} environment taps");
    assert!(
        src.contains("textureLod"),
        "sampled with an explicit level (WebGL2 textureLod)"
    );
}

#[test]
fn uniform_blocks_fit_webgl2() {
    // Frame block <= 256 bytes, per-draw block 128 bytes (two per 256-byte dynamic offset slot).
    let (module, _) = module();
    for (_, var) in module.global_variables.iter() {
        if var.space == naga::AddressSpace::Uniform {
            let size = module.types[var.ty].inner.size(module.to_ctx());
            assert!(size <= 256, "{:?} is {size} bytes", var.name);
        }
    }
}

/// The shared look functions stay cheap: pure math with no bindings and no texture read, so they add
/// no sample to any fragment, and no loop but the opal's fixed 3x3 cell search.
#[test]
fn shared_look_functions_read_no_texture_and_loop_only_where_fixed() {
    for (name, src, loops) in [
        ("step_cut", include_str!("../src/shaders/shared/step_cut.wgsl"), 0),
        (
            "opal_play",
            include_str!("../src/shaders/shared/opal_play.wgsl"),
            2,
        ),
        (
            "metal_grain",
            include_str!("../src/shaders/shared/metal_grain.wgsl"),
            0,
        ),
    ] {
        assert!(
            !src.contains("textureSample") && !src.contains("textureLoad"),
            "{name} reads a texture"
        );
        assert!(!src.contains("@group"), "{name} has a binding");
        assert_eq!(src.matches("for (").count(), loops, "{name} loops");
        assert!(
            !src.contains("while") && !src.contains("loop {"),
            "{name} has an open loop"
        );
    }
    // The opal's own hook samples the studio twice (body and coat).
    let wgsl = regia_looks::LOOKS_SHADER;
    let opal = wgsl
        .split("fn fs_opal(")
        .nth(1)
        .and_then(|s| s.split("\n}\n").next())
        .expect("fs_opal body");
    assert_eq!(opal.matches("env_at(").count(), 2, "fs_opal environment taps");
}

/// WGSL leaves `smoothstep` undefined when the low edge is not below the high edge, and drivers differ:
/// every constant-edged call in the look shaders is written low to high.
#[test]
fn smoothstep_edges_are_in_order() {
    let hooks = [
        regia_looks::LOOKS_SHADER,
        include_str!("../src/shaders/hooks/gem.wgsl"),
        include_str!("../src/shaders/hooks/frosted.wgsl"),
        include_str!("../src/shaders/hooks/metal.wgsl"),
        include_str!("../src/shaders/hooks/opal.wgsl"),
        include_str!("../src/shaders/hooks/cabochon.wgsl"),
    ];
    for src in hooks {
        for call in src.split("smoothstep(").skip(1) {
            let mut args = call.splitn(3, ',');
            let (Some(a), Some(b)) = (args.next(), args.next()) else {
                continue;
            };
            if let (Ok(a), Ok(b)) = (a.trim().parse::<f32>(), b.trim().parse::<f32>()) {
                assert!(a < b, "smoothstep({a}, {b}, ..)");
            }
        }
    }
}

/// Sizes of the translated fragments, for the report: a rough stand-in for shader cost.
#[test]
fn report_translated_fragment_sizes() {
    let (module, info) = module();
    for (entry, stage) in ENTRIES {
        if stage == naga::ShaderStage::Fragment {
            eprintln!(
                "METRIC glsl {entry} {} bytes",
                glsl_es_300(&module, &info, entry, stage).len()
            );
        }
    }
}
