// SPDX-License-Identifier: MPL-2.0
//! Static checks of the own-canopy shader (`lab_forward.wgsl` `shade_canopy`, v0.1.19e hardening,
//! ADR-262), on the composed WGSL rather than on images.
//!
//! 1. Every implicit-derivative operation runs before the first `discard`. naga 30 writes WGSL
//!    `discard` as SPIR-V `OpKill` (not `OpDemoteToHelperInvocation`): the lane ends, so once one
//!    lane of a 2×2 quad has discarded, the code after that point runs in non-uniform control flow
//!    and a derivative there is undefined (implicit-LOD sampling can pick a wrong mip, `dpdx` a
//!    wrong slope). The canopy's front-layer entry used to discard the layers behind the nearest
//!    one before shading, and the shading samples with `textureSampleBias`, takes `dpdx` for
//!    specular AA and builds the shadow normal from `dpdx(world)`. Lavapipe drew the same canopy
//!    images before and after the reorder, so no image test can see it. The check walks the
//!    module's IR in statement order and fails when a derivative (`dpdx`/`dpdy`/`fwidth`, an
//!    implicit-LOD or biased sample) can run after a `discard` on the same path, through calls.
//! 2. The canopy's cascade compare carries `sample_cascade`'s receiver bias statement for
//!    statement (the coarse-cascade ramp included), so the two cannot drift apart again.

use std::collections::HashMap;

use naga::{Block, Expression, Function, Handle, Module, SampleLevel, Statement};
use naga_oil::compose::ShaderDefValue;

/// The `GLASS` module the glass pipelines build (`frame/glass_gpu.rs` `GlassPipes::module`).
fn glass_module(encode: bool) -> Module {
    let mut defs = HashMap::new();
    defs.insert("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true));
    defs.insert("GLASS".to_string(), ShaderDefValue::Bool(true));
    if !encode {
        defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
    }
    match regia_render::compose_source(
        include_str!("../shaders/lab_forward.wgsl"),
        "lab_forward.wgsl",
        defs,
    ) {
        Ok(module) => module,
        Err(err) => panic!("lab_forward.wgsl (GLASS) does not compose: {err}"),
    }
}

fn is_derivative(e: &Expression) -> bool {
    match e {
        Expression::Derivative { .. } => true,
        Expression::ImageSample { gather, level, .. } => {
            gather.is_none() && matches!(level, SampleLevel::Auto | SampleLevel::Bias(_))
        }
        _ => false,
    }
}

/// Per function: does it (or anything it calls) take a derivative, and can it discard?
struct Summary {
    derivative: HashMap<Handle<Function>, bool>,
    kills: HashMap<Handle<Function>, bool>,
}

fn called(block: &Block, out: &mut Vec<Handle<Function>>) {
    for s in block.iter() {
        match s {
            Statement::Call { function, .. } => out.push(*function),
            Statement::Block(b) => called(b, out),
            Statement::If { accept, reject, .. } => {
                called(accept, out);
                called(reject, out);
            }
            Statement::Switch { cases, .. } => cases.iter().for_each(|c| called(&c.body, out)),
            Statement::Loop { body, continuing, .. } => {
                called(body, out);
                called(continuing, out);
            }
            _ => {}
        }
    }
}

fn has_kill(block: &Block) -> bool {
    block.iter().any(|s| match s {
        Statement::Kill => true,
        Statement::Block(b) => has_kill(b),
        Statement::If { accept, reject, .. } => has_kill(accept) || has_kill(reject),
        Statement::Switch { cases, .. } => cases.iter().any(|c| has_kill(&c.body)),
        Statement::Loop { body, continuing, .. } => has_kill(body) || has_kill(continuing),
        _ => false,
    })
}

fn summarize(module: &Module) -> Summary {
    let mut s = Summary {
        derivative: HashMap::new(),
        kills: HashMap::new(),
    };
    // To a fixed point (whatever order the composer left the functions in).
    loop {
        let mut changed = false;
        for (handle, f) in module.functions.iter() {
            let mut calls = Vec::new();
            called(&f.body, &mut calls);
            let derivative = f.expressions.iter().any(|(_, e)| is_derivative(e))
                || calls
                    .iter()
                    .any(|c| s.derivative.get(c).copied().unwrap_or(false));
            let kills = has_kill(&f.body) || calls.iter().any(|c| s.kills.get(c).copied().unwrap_or(false));
            changed |= s.derivative.insert(handle, derivative) != Some(derivative);
            changed |= s.kills.insert(handle, kills) != Some(kills);
        }
        if !changed {
            return s;
        }
    }
}

/// Walks `block` in order with `killed` = some earlier statement on a path to here could have
/// discarded; records what takes a derivative after that. Returns `killed` at the block's end.
fn walk(
    module: &Module,
    f: &Function,
    s: &Summary,
    block: &Block,
    mut killed: bool,
    hits: &mut Vec<String>,
) -> bool {
    for st in block.iter() {
        match st {
            Statement::Emit(range) => {
                if killed {
                    for h in range.clone() {
                        if is_derivative(&f.expressions[h]) {
                            hits.push(format!("{:?}", f.expressions[h]));
                        }
                    }
                }
            }
            Statement::Call { function, .. } => {
                let name = module.functions[*function].name.clone().unwrap_or_default();
                if killed && s.derivative.get(function).copied().unwrap_or(false) {
                    hits.push(format!("call {name} (takes a derivative) after a discard"));
                }
                killed |= s.kills.get(function).copied().unwrap_or(false);
            }
            Statement::Kill => killed = true,
            Statement::Block(b) => killed = walk(module, f, s, b, killed, hits),
            Statement::If { accept, reject, .. } => {
                let a = walk(module, f, s, accept, killed, hits);
                let r = walk(module, f, s, reject, killed, hits);
                killed = a || r;
            }
            Statement::Switch { cases, .. } => {
                let mut any = killed;
                for c in cases {
                    any |= walk(module, f, s, &c.body, killed, hits);
                }
                killed = any;
            }
            Statement::Loop { body, continuing, .. } => {
                // Twice: a discard in one iteration precedes the next iteration's derivatives.
                for _ in 0..2 {
                    killed = walk(module, f, s, body, killed, hits);
                    killed = walk(module, f, s, continuing, killed, hits);
                }
            }
            _ => {}
        }
    }
    killed
}

/// Derivatives after a discard in `entry` and in every function it reaches.
fn derivatives_after_discard(module: &Module, entry: &str) -> Vec<String> {
    let s = summarize(module);
    let Some(ep) = module.entry_points.iter().find(|ep| ep.name == entry) else {
        panic!("no entry point {entry}");
    };
    let mut hits = Vec::new();
    walk(module, &ep.function, &s, &ep.function.body, false, &mut hits);
    // Inside the callees (each from a clean start; a caller's earlier discard is covered above).
    let mut todo = Vec::new();
    called(&ep.function.body, &mut todo);
    let mut seen = std::collections::HashSet::new();
    while let Some(h) = todo.pop() {
        if !seen.insert(h) {
            continue;
        }
        let f = &module.functions[h];
        let mut inner = Vec::new();
        walk(module, f, &s, &f.body, false, &mut inner);
        let name = f.name.clone().unwrap_or_default();
        hits.extend(inner.into_iter().map(|hit| format!("in {name}: {hit}")));
        called(&f.body, &mut todo);
    }
    hits
}

#[test]
fn the_canopy_entry_points_take_no_derivative_after_a_discard() {
    for encode in [true, false] {
        let module = glass_module(encode);
        for entry in ["fs_canopy_add", "fs_canopy_add_front"] {
            let hits = derivatives_after_discard(&module, entry);
            assert!(
                hits.is_empty(),
                "{entry} (encode {encode}): derivatives after a discard (undefined under OpKill):\n{}",
                hits.join("\n")
            );
        }
        // The full forward entry points are checked below (`shade_forward_discards_after_its_derivatives`);
        // the glass module's count is printed as a trend line.
        for entry in ["fs_main", "fs_glass_add"] {
            let n = derivatives_after_discard(&module, entry).len();
            eprintln!("{entry} (encode {encode}): {n} derivative uses after a discard");
        }
    }
}

/// `shade_forward` (every material's forward pipeline) decides the alpha test early but discards
/// only after its implicit derivatives (material samples, specular AA, shadow normal, skin
/// curvature), as `shade_canopy` does (ADR-265). The alpha-tested module without glass has no other
/// discard, so nothing may take a derivative after one.
#[test]
fn shade_forward_discards_after_its_derivatives() {
    let mut defs = HashMap::new();
    defs.insert("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true));
    let module = match regia_render::compose_source(
        include_str!("../shaders/lab_forward.wgsl"),
        "lab_forward.wgsl",
        defs,
    ) {
        Ok(module) => module,
        Err(err) => panic!("lab_forward.wgsl (ALPHA_TEST) does not compose: {err}"),
    };
    let hits = derivatives_after_discard(&module, "fs_main");
    assert!(
        hits.is_empty(),
        "fs_main: derivatives after the alpha-test discard (undefined under OpKill):\n{}",
        hits.join("\n")
    );
}

#[test]
fn the_walk_finds_a_derivative_after_a_discard() {
    // The checker itself: a discard before `dpdx` is caught, through a call too; after it is not.
    let bad = naga::front::wgsl::parse_str(
        "fn shade(x: f32) -> f32 { return dpdx(x); }
         @fragment fn bad(@location(0) x: f32) -> @location(0) vec4<f32> {
             if (x > 0.5) { discard; }
             return vec4<f32>(shade(x));
         }
         @fragment fn good(@location(0) x: f32) -> @location(0) vec4<f32> {
             let d = shade(x);
             if (x > 0.5) { discard; }
             return vec4<f32>(d);
         }",
    )
    .unwrap_or_else(|err| panic!("test WGSL: {err}"));
    assert!(!derivatives_after_discard(&bad, "bad").is_empty());
    assert!(derivatives_after_discard(&bad, "good").is_empty());
}

/// The body of `fn name(` in `src`, braces matched.
fn wgsl_fn<'a>(src: &'a str, name: &str) -> Option<&'a str> {
    let start = src.find(&format!("fn {name}("))?;
    let open = start + src[start..].find('{')?;
    let mut depth = 0usize;
    for (i, c) in src[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&src[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `body`'s statements (split at `;` and braces) with comments and whitespace removed.
fn statements(body: &str) -> Vec<String> {
    let code: String = body
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    code.split([';', '{', '}'])
        .map(|s| s.chars().filter(|c| !c.is_whitespace()).collect::<String>())
        .filter(|s| !s.is_empty())
        .collect()
}

#[test]
fn the_canopy_cascade_compare_biases_as_sample_cascade() {
    let src = include_str!("../shaders/lab_forward.wgsl");
    let full = statements(wgsl_fn(src, "sample_cascade").expect("fn sample_cascade"));
    // The canopy's compare (`canopy_cascade_visibility` since the hardening; before it, inline in
    // `canopy_sun_visibility`).
    let canopy_fn = wgsl_fn(src, "canopy_cascade_visibility")
        .or_else(|| wgsl_fn(src, "canopy_sun_visibility"))
        .expect("the canopy's cascade compare");
    let canopy = statements(canopy_fn);
    // The bias: texel and depth scale, normal offset (fine cap → coarse ramp), receiver-plane
    // slope and its cap, constant bias (same ramp), compare depth and the curvature allowance.
    let bias_vars = [
        "lettexel_world=",
        "letdepth_scale=",
        "letno_l=",
        "letsin_theta=",
        "letcoarse=",
        "letoffset_m=",
        "letmax_slope=",
        "letconstant_m=",
        "letz=",
        "letbend=",
    ];
    let wanted: Vec<&String> = full
        .iter()
        .filter(|s| bias_vars.iter().any(|v| s.starts_with(v)))
        .collect();
    assert_eq!(
        wanted.len(),
        bias_vars.len(),
        "sample_cascade's bias statements: {wanted:?}"
    );
    let missing: Vec<&&String> = wanted.iter().filter(|s| !canopy.contains(s)).collect();
    assert!(
        missing.is_empty(),
        "the canopy's cascade compare lacks sample_cascade's bias statements: {missing:?}"
    );
}
