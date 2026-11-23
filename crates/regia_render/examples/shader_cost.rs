// SPDX-License-Identifier: MPL-2.0
//! CPU side of a pipeline's cost, without a GPU: how long composing, validating and
//! translating the engine's big shaders takes, and how much of that wgpu's Vulkan backend
//! repeats for every pipeline stage.
//!
//! wgpu-hal 30's Vulkan backend always sets `SEPARATE_ENTRY_POINTS`, so a shader module keeps
//! its naga IR and every pipeline stage runs `process_overrides` (for a module with more than
//! one entry point: clone, compact to the entry point, validate again) and then the SPIR-V
//! writer. This prints both paths per stage: the whole module (what a multi-entry module costs
//! per stage) and a module split to that entry point once (what [`regia_render`]'s split
//! modules cost per stage), and checks the SPIR-V is the same words.
//!
//! ```text
//! cargo run --release -p regia_render --example shader_cost [-- ROUNDS]
//! ```
//!
//! `SHADER_COST_DUMP=DIR` also writes each stage's SPIR-V to `DIR` (for `spirv-dis`).

use std::collections::HashMap;
use std::time::Instant;

use naga::back::spv;
use naga::valid::{Capabilities, ValidationFlags, Validator};
use naga_oil::compose::ShaderDefValue;

fn defs(names: &[&str]) -> HashMap<String, ShaderDefValue> {
    names
        .iter()
        .map(|n| ((*n).to_string(), ShaderDefValue::Bool(true)))
        .collect()
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

fn validate(module: &naga::Module) -> Result<naga::valid::ModuleInfo, String> {
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(module)
        .map_err(|e| format!("{e:?}"))
}

/// One stage the way wgpu-hal's Vulkan `compile_stage` does it.
fn stage_spirv(
    module: &naga::Module,
    info: &naga::valid::ModuleInfo,
    stage: naga::ShaderStage,
    entry: &str,
) -> Result<(Vec<u32>, f64, f64), String> {
    let start = Instant::now();
    let (module, info) = naga::back::pipeline_constants::process_overrides(
        module,
        info,
        Some((stage, entry)),
        &naga::back::PipelineConstants::default(),
    )
    .map_err(|e| e.to_string())?;
    let overrides_ms = ms(start);
    let start = Instant::now();
    let pipeline = spv::PipelineOptions {
        shader_stage: stage,
        entry_point: entry.to_string(),
    };
    let words = spv::write_vec(&module, &info, &spv::Options::default(), Some(&pipeline))
        .map_err(|e| e.to_string())?;
    Ok((words, overrides_ms, ms(start)))
}

/// A shader to measure: name, entry source, file, defines and the stages to translate.
type Case<'a> = (
    &'a str,
    &'a str,
    &'a str,
    HashMap<String, ShaderDefValue>,
    Vec<(naga::ShaderStage, &'a str)>,
);

fn main() -> Result<(), String> {
    let rounds: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(3);
    let forward = include_str!("../shaders/lab_forward.wgsl");
    let shadow = include_str!("../shaders/lab_shadow.wgsl");
    let cases: Vec<Case> = vec![
        (
            "lab_forward ALPHA_TEST",
            forward,
            "lab_forward.wgsl",
            defs(&["ALPHA_TEST"]),
            vec![
                (naga::ShaderStage::Vertex, "vs_main"),
                (naga::ShaderStage::Fragment, "fs_main"),
                (naga::ShaderStage::Fragment, "fs_depth"),
            ],
        ),
        (
            "lab_forward GLASS GPU_INSTANCED",
            forward,
            "lab_forward.wgsl",
            defs(&["GLASS", "GPU_INSTANCED"]),
            vec![
                (naga::ShaderStage::Vertex, "vs_main"),
                (naga::ShaderStage::Fragment, "fs_main"),
            ],
        ),
        (
            "lab_shadow",
            shadow,
            "lab_shadow.wgsl",
            HashMap::new(),
            vec![(naga::ShaderStage::Vertex, "vs_main")],
        ),
    ];
    println!(
        "stage | whole-module overrides ms | whole-module spv ms | split once ms | split overrides ms | split spv ms | same SPIR-V"
    );
    for (name, source, file, shader_defs, stages) in cases {
        let start = Instant::now();
        let module =
            regia_render::compose_source(source, file, shader_defs).map_err(|e| format!("{name}: {e}"))?;
        let compose_ms = ms(start);
        let start = Instant::now();
        let info = validate(&module)?;
        let validate_ms = ms(start);
        println!(
            "{name}: compose {compose_ms:.1} ms, validate {validate_ms:.1} ms, {} entry points, {} functions",
            module.entry_points.len(),
            module.functions.len()
        );
        for (stage, entry) in stages {
            let mut whole = (0.0, 0.0);
            let mut split = (0.0, 0.0, 0.0);
            let mut same = true;
            let mut words = 0;
            for _ in 0..rounds {
                let (a, o, w) = stage_spirv(&module, &info, stage, entry)?;
                whole.0 += o;
                whole.1 += w;
                let start = Instant::now();
                let mut single = module.clone();
                single
                    .entry_points
                    .retain(|ep| ep.stage == stage && ep.name == entry);
                naga::compact::compact(&mut single, naga::compact::KeepUnused::No);
                let single_info = validate(&single)?;
                split.0 += ms(start);
                let (b, o, w) = stage_spirv(&single, &single_info, stage, entry)?;
                split.1 += o;
                split.2 += w;
                same &= a == b;
                words = a.len();
                if let Ok(dir) = regia_core::brand::env_var("SHADER_COST_DUMP") {
                    let path = format!("{dir}/{}-{entry}.spv", name.replace(' ', "_"));
                    let bytes: Vec<u8> = a.iter().flat_map(|w| w.to_le_bytes()).collect();
                    std::fs::write(&path, bytes).map_err(|e| format!("{path}: {e}"))?;
                }
            }
            let n = rounds as f64;
            println!(
                "  {entry:<9} | {:>6.2} | {:>6.2} | {:>6.2} | {:>6.2} | {:>6.2} | {same} ({words} words)",
                whole.0 / n,
                whole.1 / n,
                split.0 / n,
                split.1 / n,
                split.2 / n
            );
        }
    }
    Ok(())
}
