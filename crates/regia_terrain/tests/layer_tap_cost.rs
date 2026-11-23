// SPDX-License-Identifier: MPL-2.0
//! Where the terrain's anisotropic taps get the sampler path's LOD offset from (ADR-5943).
//!
//! `layer_tap` in `terrain.wgsl` runs once per texture tap: up to three stochastic taps of each
//! of a layer's albedo, normal, ORM and height arrays, the triplanar sides and the macro pair,
//! tens of times per pixel. `aniso_grads` runs once per projection of a layer. ADR-5459 scaled
//! both gradients in `layer_tap` by a factor of the frame uniform alone (`aniso_sampler_scale`:
//! a negation, `exp2`, a compare and a `select`, then two vector multiplies). With the factor
//! exactly 1 that cost an RTX 5090 1.5 % of `terrain_forward` on DO27's switchbacks: the
//! driver's compiler inlines every tap site and derives the factor from the uniform again at each
//! one. The same offset derived from the uniform in `aniso_grads` still cost 0.4 %; stored once
//! per pixel by `pixel_footprint` in a private and added in `aniso_grads` it costs nothing that
//! can be measured (lane fable-perf-adr5459, same-card interleaved runs; numbers in ADR-5943).
//!
//! The tests read the naga IR of every source the shared colour pipelines compile (no GPU):
//!
//! - `the_tap_computes_nothing_from_the_frame`: `layer_tap` is the tap it was before ADR-5459.
//!   It keeps its two `/ dims` divides and nothing else (no other arithmetic, no math call, no
//!   `select`, no uniform read) and calls only the tap rotation, the footprint test and the
//!   footprint assembly;
//! - `the_lod_offset_is_read_once_per_pixel`: `pixel_footprint` stores the offset from the
//!   frame uniform, `aniso_grads` (the one place every projection's gradients come from) adds
//!   that stored value and reads no uniform for it, and nothing else touches it. So the feature
//!   ADR-5459 added still reaches every anisotropic sampler tap.
//!
//! `terrain_aniso`'s `the_sampler_lod_offset_softens_the_sampler_path` renders the offset through
//! the real shader.

use regia_terrain::gpu::{shared_source, without_cost_views, without_ground_look, without_surface_weather};

/// Every source a shared colour pipeline compiles: both height reads, with and without the
/// engine's local lights, each as the full shader (a ground look, a cost view or wet weather on)
/// and as the stripped one every other frame runs.
fn shared_sources() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for hw_height in [false, true] {
        for lights in [false, true] {
            let full = shared_source(hw_height, lights);
            let stripped = without_surface_weather(&without_cost_views(&without_ground_look(&full)));
            out.push((
                format!("shared (hw height {hw_height}, lights {lights}, full)"),
                full,
            ));
            out.push((
                format!("shared (hw height {hw_height}, lights {lights}, stripped)"),
                stripped,
            ));
        }
    }
    out
}

fn parse(name: &str, source: &str) -> naga::Module {
    match naga::front::wgsl::parse_str(source) {
        Ok(module) => module,
        Err(err) => panic!("{name}: {}", err.emit_to_string(source)),
    }
}

/// Every function of the module with its name, entry points included.
fn functions(module: &naga::Module) -> Vec<(&str, &naga::Function)> {
    module
        .functions
        .iter()
        .map(|(_, f)| (f.name.as_deref().unwrap_or("?"), f))
        .chain(
            module
                .entry_points
                .iter()
                .map(|ep| (ep.name.as_str(), &ep.function)),
        )
        .collect()
}

fn function<'m>(name: &str, module: &'m naga::Module, function: &str) -> &'m naga::Function {
    functions(module)
        .into_iter()
        .find(|(fname, _)| *fname == function)
        .map(|(_, f)| f)
        .unwrap_or_else(|| panic!("{name}: no function `{function}`"))
}

/// What one function computes itself (its callees are listed, not followed).
#[derive(Debug, Default, PartialEq, Eq)]
struct Work {
    /// Binary operators, as naga names them, sorted.
    binary: Vec<String>,
    /// Unary operators.
    unary: usize,
    /// Math built-ins (`exp2`, `max`, `length`, ...).
    math: usize,
    /// `select` calls.
    select: usize,
    /// Texture samples.
    samples: usize,
    /// Module-scope variables the function names (uniforms, textures, samplers, privates), sorted.
    globals: Vec<String>,
    /// Functions it calls, in the order of the source.
    calls: Vec<String>,
}

fn calls(module: &naga::Module, block: &naga::Block, out: &mut Vec<String>) {
    for statement in block.iter() {
        match statement {
            naga::Statement::Call { function, .. } => out.push(
                module.functions[*function]
                    .name
                    .clone()
                    .unwrap_or_else(|| "?".to_owned()),
            ),
            naga::Statement::Block(inner) => calls(module, inner, out),
            naga::Statement::If { accept, reject, .. } => {
                calls(module, accept, out);
                calls(module, reject, out);
            }
            naga::Statement::Switch { cases, .. } => {
                for case in cases {
                    calls(module, &case.body, out);
                }
            }
            naga::Statement::Loop { body, continuing, .. } => {
                calls(module, body, out);
                calls(module, continuing, out);
            }
            _ => {}
        }
    }
}

fn work(module: &naga::Module, function: &naga::Function) -> Work {
    let mut work = Work::default();
    for (_, expression) in function.expressions.iter() {
        match expression {
            naga::Expression::Binary { op, .. } => work.binary.push(format!("{op:?}")),
            naga::Expression::Unary { .. } => work.unary += 1,
            naga::Expression::Math { .. } => work.math += 1,
            naga::Expression::Select { .. } => work.select += 1,
            naga::Expression::ImageSample { .. } => work.samples += 1,
            naga::Expression::GlobalVariable(handle) => work.globals.push(
                module.global_variables[*handle]
                    .name
                    .clone()
                    .unwrap_or_else(|| "?".to_owned()),
            ),
            _ => {}
        }
    }
    work.binary.sort();
    work.globals.sort();
    work.globals.dedup();
    calls(module, &function.body, &mut work.calls);
    work
}

/// `layer_tap` is the tap it was before ADR-5459: the gradients divided by the texture size, one
/// anisotropic and one isotropic sample, and nothing that the frame alone decides.
#[test]
fn the_tap_computes_nothing_from_the_frame() {
    for (name, source) in shared_sources() {
        let module = parse(&name, &source);
        let tap = work(&module, function(&name, &module, "layer_tap"));
        eprintln!("{name}: layer_tap {tap:?}");
        assert_eq!(
            tap.binary,
            ["Divide", "Divide"],
            "{name}: `layer_tap` runs once per texture tap (tens per pixel): besides the two `/ dims` divides \
             it must do no arithmetic. Work per projection belongs in `aniso_grads`, and a value that comes \
             from a uniform in a private `pixel_footprint` stores (ADR-5943: a scale computed here cost an \
             RTX 5090 1.5 % of terrain_forward at scale 1)"
        );
        assert_eq!(
            (tap.unary, tap.math, tap.select),
            (0, 0, 0),
            "{name}: `layer_tap` must not compute a per-tap factor (unary, math, select; ADR-5943)"
        );
        assert_eq!(
            tap.calls,
            ["rotate2", "rotate2", "aniso_footprint", "layer_tap_footprint"],
            "{name}: `layer_tap` calls the tap rotation, the footprint test and the footprint assembly only \
             (ADR-5943: `aniso_sampler_scale` per tap was the regression)"
        );
        assert_eq!(
            tap.globals,
            ["material_aniso_samp", "material_samp"],
            "{name}: `layer_tap` reads no uniform itself (ADR-5943)"
        );
        assert_eq!(
            tap.samples, 2,
            "{name}: `layer_tap` holds one anisotropic and one isotropic sample"
        );
    }
}

/// The sampler path's LOD offset (ADR-5459) reaches every anisotropic sampler tap, and the
/// frame uniform is read for it once per pixel: `pixel_footprint` stores it in the private
/// `aniso_lod_offset`, `aniso_grads` adds that to its exponent, and all gradients a tap can take
/// come from `aniso_grads`.
#[test]
fn the_lod_offset_is_read_once_per_pixel() {
    for (name, source) in shared_sources() {
        let module = parse(&name, &source);
        let footprint = work(&module, function(&name, &module, "pixel_footprint"));
        assert_eq!(
            footprint.globals,
            ["aniso_dx", "aniso_dy", "aniso_lod_offset", "frame"],
            "{name}: `pixel_footprint` stores the footprint axes and the sampler path's LOD offset from the \
             frame uniform (ADR-5459, ADR-5943)"
        );
        let grads = work(&module, function(&name, &module, "aniso_grads"));
        eprintln!("{name}: aniso_grads {grads:?}");
        assert_eq!(
            grads.globals,
            ["aniso_lod_offset"],
            "{name}: `aniso_grads` adds the stored LOD offset (ADR-5459) and reads no uniform for it: \
             derived from the uniform here it cost an RTX 5090 0.4 % of terrain_forward (ADR-5943)"
        );
        assert_eq!(
            grads.calls,
            ["aniso_level"],
            "{name}: `aniso_grads` calls the anisotropy level only (ADR-5943)"
        );
        let mut users: Vec<&str> = functions(&module)
            .into_iter()
            .filter(|(_, f)| {
                work(&module, f)
                    .globals
                    .iter()
                    .any(|global| global == "aniso_lod_offset")
            })
            .map(|(fname, _)| fname)
            .collect();
        users.sort_unstable();
        assert_eq!(
            users,
            ["aniso_grads", "pixel_footprint"],
            "{name}: the LOD offset is stored by `pixel_footprint` and read by `aniso_grads` alone"
        );
        // Every gradient pair a tap can take is `aniso_grads`' (the macro tile's is its copy with
        // the edge test): a function that taps a layer texture makes its gradients there.
        for (fname, f) in functions(&module) {
            let called = work(&module, f).calls;
            let taps = called
                .iter()
                .any(|call| matches!(call.as_str(), "layer_tap" | "tri_sample" | "tri_normal"));
            if !taps || matches!(fname, "tri_sample" | "tri_normal") {
                continue;
            }
            assert!(
                called
                    .iter()
                    .any(|call| matches!(call.as_str(), "aniso_grads" | "aniso_grads_top")),
                "{name}: `{fname}` taps a layer texture without gradients from `aniso_grads`: calls {called:?}"
            );
        }
    }
}
