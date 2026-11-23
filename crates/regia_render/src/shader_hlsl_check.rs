// SPDX-License-Identifier: MPL-2.0
//! DX12 shader translation gate that runs on Linux (ADR-295, ADR-326).
//!
//! The DX12 backend of wgpu turns each naga module into HLSL and compiles it with FXC (Shader
//! Model 5.1) or, with the `static-dxc` feature, DXC (Shader Model 6). None of that can run on a
//! Linux host, so these tests stop at the first half: every module the SPIR-V gate in
//! `shader_check` covers (the engine entry shaders in every shader-def set, and the material
//! graph host modules) goes through naga's HLSL writer, the way wgpu-hal calls it (overrides
//! substituted first, one write per entry point, wgpu-hal's writer options), once for Shader
//! Model 6.0 (DXC) and once for 5.1 (FXC: what a build without `static-dxc` runs when no
//! `dxcompiler.dll` is found, `backend_policy::fxc_may_compile`). Any naga error fails the test,
//! and so does a Shader Model 6 construct (wave intrinsics, 16- or 64-bit integer types) in the
//! 5.1 text, which naga writes without complaint but FXC rejects. Both runs walk the SPIR-V
//! gate's own module list, and a compose failure counts as a failure, so no module can drop out;
//! there is no allow-list.
//! This proves naga can express the shader in HLSL; it does not prove FXC or DXC accepts the
//! text, and it cannot see a driver bug.
//!
//! A second test checks the loops ADR-201 made FXC-safe, in the WGSL text: FXC must unroll a
//! loop that contains a texture operation, so each stays a fixed-trip `for` with no `break` or
//! `continue`.

use std::collections::HashMap;

use naga::back::hlsl;
use naga_oil::compose::ShaderDefValue;

use crate::shader_lib::compose_source;
use crate::surface_graph::{
    GraphCode, SAMPLE_SURFACE, SAMPLE_VERTEX, code_defs, forward_source_with_code, vertex_host_defs,
    vertex_host_source,
};

/// Writes `module` as HLSL for `shader_model`, one write per entry point. Returns the HLSL text
/// of every entry point, or the first naga error with the module's name.
fn translate(name: &str, module: &naga::Module, shader_model: hlsl::ShaderModel) -> Result<String, String> {
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(module)
    .map_err(|err| format!("{name}: naga validation: {err}"))?;
    let constants = naga::back::PipelineConstants::new();
    let (module, info) = naga::back::pipeline_constants::process_overrides(module, &info, None, &constants)
        .map_err(|err| format!("{name}: pipeline overrides: {err}"))?;
    // naga's defaults are wgpu-hal's writer options (workgroup zeroing, restricted indexing,
    // forced loop bounds) apart from the binding map, which needs the pipeline layout.
    let options = hlsl::Options {
        shader_model,
        ..hlsl::Options::default()
    };
    let mut all = String::new();
    for entry in &module.entry_points {
        let pipeline = hlsl::PipelineOptions {
            entry_point: Some((entry.stage, entry.name.clone())),
        };
        let mut text = String::new();
        let mut writer = hlsl::Writer::new(&mut text, &options, &pipeline);
        let reflection = writer
            .write(module.as_ref(), info.as_ref(), None)
            .map_err(|err| format!("{name} ({}): hlsl: {err}", entry.name))?;
        for written in reflection.entry_point_names {
            written.map_err(|err| format!("{name} ({}): hlsl entry point: {err}", entry.name))?;
        }
        all.push_str(&text);
    }
    Ok(all)
}

// HLSL that needs Shader Model 6: the matrix's list (`shader_check::FXC_REJECTS`).
use crate::shader_check::FXC_REJECTS;

/// `module` (named `label`) translated for `shader_model`: `Ok(())`, or the naga error, or the
/// Shader Model 6 constructs found in Shader Model 5.1 text.
fn check(label: &str, module: &naga::Module, shader_model: hlsl::ShaderModel) -> Result<(), String> {
    let text = translate(label, module, shader_model)?;
    if shader_model == hlsl::ShaderModel::V5_1 {
        let found: Vec<&str> = FXC_REJECTS.into_iter().filter(|t| text.contains(t)).collect();
        if !found.is_empty() {
            return Err(format!(
                "{label}: Shader Model 6 HLSL in the FXC (5.1) text: {}",
                found.join(", ")
            ));
        }
    }
    Ok(())
}

fn defs_label(defs: &HashMap<String, ShaderDefValue>) -> String {
    let mut keys: Vec<String> = defs
        .iter()
        .map(|(key, value)| format!("{key}={value:?}"))
        .collect();
    keys.sort();
    keys.join(",")
}

#[test]
fn hlsl_writer_emits_a_compute_entry() {
    let module = match naga::front::wgsl::parse_str(
        "@compute @workgroup_size(8, 8, 1)\nfn main(@builtin(global_invocation_id) id: vec3<u32>) { _ = id; }\n",
    ) {
        Ok(module) => module,
        Err(err) => panic!("fixture does not parse: {err}"),
    };
    for model in [hlsl::ShaderModel::V5_1, hlsl::ShaderModel::V6_0] {
        let text = translate("fixture", &module, model);
        assert!(
            matches!(&text, Ok(t) if !t.is_empty()),
            "fixture did not translate for {model:?}: {text:?}"
        );
        assert_eq!(check("fixture", &module, model), Ok(()));
    }
}

#[test]
fn the_fxc_scan_catches_shader_model_6_text() {
    // A subgroup ballot writes `WaveActiveBallot`: fine for DXC, an FXC compile error. (naga 30
    // takes subgroup builtins without `enable subgroups;`, which it does not implement yet.)
    let module = match naga::front::wgsl::parse_str(
        "@compute @workgroup_size(64)\nfn main() { _ = subgroupBallot(true); }\n",
    ) {
        Ok(module) => module,
        Err(err) => panic!("fixture does not parse: {err}"),
    };
    assert_eq!(check("ballot", &module, hlsl::ShaderModel::V6_0), Ok(()));
    let fxc = check("ballot", &module, hlsl::ShaderModel::V5_1);
    assert!(
        matches!(&fxc, Err(err) if err.contains("WaveActive")),
        "the FXC scan missed a wave intrinsic: {fxc:?}"
    );
}

/// Every module the SPIR-V gate covers, translated for `shader_model`: (translated, failures).
fn translate_engine_shaders(shader_model: hlsl::ShaderModel) -> (usize, Vec<String>) {
    let mut failures = Vec::new();
    let mut translated = 0usize;
    for (name, source, shader_defs) in crate::shader_check::shader_sources() {
        let label = format!("{name} [{}]", defs_label(&shader_defs));
        match compose_source(source, name, shader_defs) {
            Ok(module) => match check(&label, &module, shader_model) {
                Ok(()) => translated += 1,
                Err(err) => failures.push(err),
            },
            Err(err) => failures.push(format!("{label}: compose: {err}")),
        }
    }

    // The material graph host modules `validate_graph_code_spirv` covers.
    let staged = GraphCode {
        surface: SAMPLE_SURFACE.into(),
        vertex: Some(SAMPLE_VERTEX.into()),
        uv1: true,
    };
    for code in [GraphCode::surface(SAMPLE_SURFACE), staged] {
        match forward_source_with_code(&code) {
            Ok(source) => {
                for extra in [
                    &["ALPHA_TEST"][..],
                    &["ALPHA_TEST", "skip_srgb_encode"][..],
                    &["GPU_DRIVEN", "GPU_INSTANCED"][..],
                    &["GPU_DRIVEN", "GPU_INSTANCED", "ALPHA_TEST", "skip_srgb_encode"][..],
                    &["GPU_DRIVEN"][..],
                    &["GPU_DRIVEN", "ALPHA_TEST"][..],
                ] {
                    let label = format!("lab_forward_graph.wgsl [{}]", extra.join(","));
                    match compose_source(&source, "lab_forward_graph.wgsl", code_defs(&code, extra)) {
                        Ok(module) => match check(&label, &module, shader_model) {
                            Ok(()) => translated += 1,
                            Err(err) => failures.push(err),
                        },
                        Err(err) => failures.push(format!("{label}: compose: {err}")),
                    }
                }
            }
            Err(err) => failures.push(format!("graph forward source: {err}")),
        }
        for velocity in [false, true] {
            let host = match vertex_host_source(&code, velocity) {
                Ok(Some(host)) => host,
                Ok(None) => continue,
                Err(err) => {
                    failures.push(format!("graph vertex host (velocity={velocity}): {err}"));
                    continue;
                }
            };
            let name = if velocity {
                "velocity_graph.wgsl"
            } else {
                "lab_shadow_graph.wgsl"
            };
            let sets: &[&[&str]] = if velocity { &[&[]] } else { &[&[], &["GPU_DRIVEN"]] };
            for extra in sets {
                let label = format!("{name} [{}]", extra.join(","));
                match compose_source(&host, name, vertex_host_defs(&code, extra)) {
                    Ok(module) => match check(&label, &module, shader_model) {
                        Ok(()) => translated += 1,
                        Err(err) => failures.push(err),
                    },
                    Err(err) => failures.push(format!("{label}: compose: {err}")),
                }
            }
        }
    }

    (translated, failures)
}

fn assert_all_translate(shader_model: hlsl::ShaderModel) {
    let (translated, failures) = translate_engine_shaders(shader_model);
    assert!(translated > 0, "no shader was translated");
    assert!(
        failures.is_empty(),
        "{} of {} modules fail naga's HLSL ({shader_model:?}) writer:\n{}",
        failures.len(),
        failures.len() + translated,
        failures.join("\n")
    );
}

#[test]
fn engine_shaders_translate_to_hlsl_sm6() {
    assert_all_translate(hlsl::ShaderModel::V6_0);
}

#[test]
fn engine_shaders_translate_to_hlsl_sm5_1_for_fxc() {
    assert!(crate::backend_policy::fxc_may_compile(wgpu::Backend::Dx12));
    assert_all_translate(hlsl::ShaderModel::V5_1);
}

/// `src` without `//` line comments (the shaders carry no block comments in these functions).
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of `fn name(` in `src`, from its opening brace to the matching close.
fn wgsl_fn_body<'a>(src: &'a str, name: &str) -> Option<&'a str> {
    let start = src.find(&format!("fn {name}("))?;
    let open = start + src[start..].find('{')?;
    let mut depth = 0usize;
    for (offset, ch) in src[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&src[open..open + offset + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whole-word occurrences of `word` in `text`.
fn has_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|w| w == word)
}

/// The `for` headers (text between `for (` and the opening brace) of `body`.
fn for_headers(body: &str) -> Vec<&str> {
    body.match_indices("for (")
        .filter_map(|(at, _)| {
            let rest = &body[at..];
            rest.find('{').map(|end| &rest[..end])
        })
        .collect()
}

#[test]
fn fxc_safe_loops_stay_unrollable() {
    let source = strip_line_comments(include_str!("../shaders/lab_forward.wgsl"));
    // (function, expected `for` headers). Fixed literal trip counts, tap/step limit as a predicate.
    let expected: [(&str, &[&str]); 2] = [
        ("csm_pcss", &["i: u32 = 0u; i < 12u;", "i: u32 = 0u; i < 16u;"]),
        ("parallax_uv", &["i: u32 = 0u; i < 16u;"]),
    ];
    let mut problems = Vec::new();
    for (name, headers) in expected {
        let Some(body) = wgsl_fn_body(&source, name) else {
            problems.push(format!("fn {name} not found in lab_forward.wgsl"));
            continue;
        };
        for word in ["break", "continue", "loop", "while"] {
            if has_word(body, word) {
                problems.push(format!(
                    "{name}: `{word}` in a function ADR-201 made FXC-unrollable"
                ));
            }
        }
        let found = for_headers(body);
        if found.len() != headers.len() {
            problems.push(format!(
                "{name}: expected {} `for` loops, found {}",
                headers.len(),
                found.len()
            ));
        }
        for want in headers {
            if !found.iter().any(|header| header.contains(want)) {
                problems.push(format!("{name}: no loop with the literal bound `{want}`"));
            }
        }
    }
    // The predicate that replaced `break`: tap or step limits guard each body.
    let pcss = wgsl_fn_body(&source, "csm_pcss").unwrap_or("");
    for guard in ["i < search_taps", "i < filter_taps"] {
        if !pcss.contains(guard) {
            problems.push(format!("csm_pcss: missing the `{guard}` predicate"));
        }
    }
    let parallax = wgsl_fn_body(&source, "parallax_uv").unwrap_or("");
    if !parallax.contains("i < steps") {
        problems.push("parallax_uv: missing the `i < steps` predicate".into());
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
