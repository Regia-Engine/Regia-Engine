// SPDX-License-Identifier: MPL-2.0
//! DX12 checks for the shaders of the engine crates outside `regia_render` (ADR-367).
//!
//! `regia_render` checks its own modules (`shader_hlsl`); it cannot see terrain, VFX, UI, 2D,
//! the path tracer or lookdev, which depend on it. This module lists their WGSL the way each
//! crate hands it to wgpu and runs `regia_render::shader_hlsl` over it: naga's HLSL writer for
//! Shader Model 5.1 and 6.0, the Direct3D 12 compute limits and FXC's loop rule.
//! `cargo xtask check-shaders` runs it after the Vulkan check, and so does this module's test.
//! [`msl_problems`] runs naga's MSL writer with wgpu's Metal options over the same list (ADR-2644).

/// Every plain-WGSL module (`ShaderSource::Wgsl`) of the engine crates outside `regia_render`,
/// as `(name, source)`: the `check-shaders` Vulkan list (terrain, UI, VFX, lookdev portal) plus
/// the terrain impostor, grass, water and shared-pass variants, the UI world panels, the VFX
/// present and soft-particle composite, the 2D sprite shader, the XR passes, the path tracer's
/// denoiser and the particle liquid.
#[must_use]
pub fn plain_sources() -> Vec<(String, String)> {
    use regia_terrain::gpu::{hw_height_source, impostor, shared_source};

    let mut out: Vec<(String, String)> = regia_terrain::gpu::shader_sources()
        .into_iter()
        .map(|(name, source)| (name.to_string(), source.to_string()))
        .collect();
    let terrain = regia_terrain::gpu::shader_sources()
        .into_iter()
        .find(|(name, _)| *name == "terrain.wgsl")
        .map_or("", |(_, source)| source);
    out.push(("terrain.wgsl (hw height)".into(), hw_height_source()));
    for hw_height in [false, true] {
        for local_lights in [false, true] {
            out.push((
                format!("terrain.wgsl (shared, hw height {hw_height}, local lights {local_lights})"),
                shared_source(hw_height, local_lights),
            ));
        }
    }
    out.push(("impostor_cull.wgsl".into(), impostor::cull_source()));
    out.push(("impostor.wgsl".into(), impostor::draw_source(terrain)));
    out.push((
        "impostor.wgsl (hw height)".into(),
        impostor::draw_source(&hw_height_source()),
    ));
    out.push((
        "ui-overlay.wgsl".into(),
        regia_ui::render::overlay_shader_source().to_string(),
    ));
    for multisampled in [false, true] {
        let name = if multisampled {
            "ui-world.wgsl (msaa depth)"
        } else {
            "ui-world.wgsl"
        };
        out.push((
            name.into(),
            regia_ui::world_target::world_shader_variant(multisampled),
        ));
    }
    out.push(("grass_cull.wgsl".into(), regia_terrain::gpu::grass::cull_source()));
    out.push((
        "grass.wgsl".into(),
        regia_terrain::gpu::grass::draw_source(terrain),
    ));
    out.extend(regia_vfx::gpu::shader_sources());
    for (name, source) in [
        (
            "vfx present.wgsl",
            include_str!("../../crates/regia_vfx/src/gpu/present.wgsl"),
        ),
        (
            "vfx soft_half_res.wgsl",
            include_str!("../../crates/regia_vfx/src/gpu/soft_half_res.wgsl"),
        ),
        (
            "sprite2d.wgsl",
            include_str!("../../crates/regia_2d/shaders/sprite2d.wgsl"),
        ),
        (
            "draw2d.wgsl",
            include_str!("../../crates/regia_quick/shaders/draw2d.wgsl"),
        ),
        (
            "portal.wgsl",
            include_str!("../../samples/lookdev/shaders/portal.wgsl"),
        ),
        (
            "xr_eye_composite.wgsl",
            include_str!("../../crates/regia_xr/shaders/xr_eye_composite.wgsl"),
        ),
        (
            "xr_quad.wgsl",
            include_str!("../../crates/regia_xr/shaders/xr_quad.wgsl"),
        ),
        (
            "xr_mirror.wgsl",
            include_str!("../../crates/regia_xr/shaders/xr_mirror.wgsl"),
        ),
        (
            "xr_stereo_forward.wgsl",
            include_str!("../../crates/regia_xr/shaders/xr_stereo_forward.wgsl"),
        ),
        (
            "water_body.wgsl",
            include_str!("../../crates/regia_terrain/src/water_body/water_body.wgsl"),
        ),
        (
            "water_ocean.wgsl",
            include_str!("../../crates/regia_terrain/src/water_body/water_ocean.wgsl"),
        ),
        // The path traced preview's GPU denoiser (plain WGSL, `regia_pt::denoise_atrous`).
        (
            "atrous.wgsl",
            include_str!("../../crates/regia_pt/shaders/atrous.wgsl"),
        ),
        // The particle liquid's GPU path (regia_fluid `gpu` feature, ADR-2441).
        (
            "fluid_flip.wgsl",
            include_str!("../../crates/regia_fluid/src/fluid_flip.wgsl"),
        ),
    ] {
        out.push((name.to_string(), source.to_string()));
    }
    // Built at runtime (`water_body/gpu.rs`, `probe_ring_vertices`): the water shader with the
    // compute probe appended (ADR-2664).
    out.push((
        "water_body.wgsl + PROBE_ENTRY".to_string(),
        format!(
            "{}\n{}",
            regia_terrain::water_body::gpu::SHADER,
            regia_terrain::water_body::gpu::PROBE_ENTRY
        ),
    ));
    // ADR-5802: entry shaders added after this list was written. The product viewer's family
    // shaders (prelude, hook, entry points) and its contact shadow, built as `regia_viewer`'s
    // renderer builds them; the jewelry looks (`regia_looks::LOOKS_SHADER`, the shared helpers
    // then `looks.wgsl`); the XeSS velocity conversion (`regia_xess::motion`).
    let hooks = regia_viewer::ShaderHooks::default();
    for family in regia_viewer::ShaderFamily::ALL {
        out.push((
            format!("viewer entry.wgsl ({})", family.name()),
            hooks.family_source(family),
        ));
    }
    out.push((
        "viewer contact_shadow.wgsl".to_string(),
        regia_viewer::hooks::contact_shadow_source(),
    ));
    out.push((
        "looks.wgsl".to_string(),
        concat!(
            include_str!("../../crates/regia_looks/src/shaders/shared/step_cut.wgsl"),
            "\n",
            include_str!("../../crates/regia_looks/src/shaders/shared/opal_play.wgsl"),
            "\n",
            include_str!("../../crates/regia_looks/src/shaders/shared/metal_grain.wgsl"),
            "\n",
            include_str!("../../crates/regia_looks/src/shaders/looks.wgsl"),
        )
        .to_string(),
    ));
    out.push((
        "xess motion.wgsl".to_string(),
        include_str!("../../crates/regia_xess/src/motion.wgsl").to_string(),
    ));
    out
}

/// The composed modules (`regia_render::compose_source`, no shader defs) of the engine crates
/// outside `regia_render`, as `(file, source)`: the path tracer and the GPU lightmap baker.
#[must_use]
pub fn composed_sources() -> Vec<(&'static str, &'static str)> {
    vec![
        ("pt.wgsl", include_str!("../../crates/regia_pt/shaders/pt.wgsl")),
        (
            "lightmap_bake.wgsl",
            include_str!("../../crates/regia_pt/shaders/lightmap_bake.wgsl"),
        ),
    ]
}

/// The composed modules that fail the ADR-026 layout scan today. Empty since ADR-7879: `pt.wgsl`
/// (15 variables) and `lightmap_bake.wgsl` (5), listed by ADR-2664 for their function-local
/// struct and array variables, keep one variable per member and their BVH traversal stacks in
/// workgroup memory, so both take the Vulkan step of `check-shaders` ([`composed_vulkan_sources`])
/// besides their HLSL and MSL steps. A test pins this list, so a module that fails the scan again
/// must be fixed, not listed.
pub const COMPOSED_LAYOUT_FAILURES: [&str; 0] = [];

/// [`composed_sources`] without [`COMPOSED_LAYOUT_FAILURES`]: the modules that take the Vulkan step.
#[must_use]
pub fn composed_vulkan_sources() -> Vec<(&'static str, &'static str)> {
    composed_sources()
        .into_iter()
        .filter(|(name, _)| !COMPOSED_LAYOUT_FAILURES.contains(name))
        .collect()
}

/// Plain modules (by [`regia_render::contract_file_name`]) that fail the ADR-026 layout scan today.
/// Empty since ADR-7587: the product viewer's family shaders and contact shadow and the jewelry
/// looks (listed by ADR-5802 for their function-local struct variables) build their structs with
/// constructors and take the Vulkan step of `check-shaders` like every other plain module. A test
/// pins this list, so a module that fails the scan again must be fixed, not listed.
pub const PLAIN_LAYOUT_FAILURES: [&str; 0] = [];

/// [`plain_sources`] without [`PLAIN_LAYOUT_FAILURES`]: the modules that take the Vulkan step.
#[must_use]
pub fn plain_vulkan_sources() -> Vec<(String, String)> {
    plain_sources()
        .into_iter()
        .filter(|(name, _)| !PLAIN_LAYOUT_FAILURES.contains(&regia_render::contract_file_name(name)))
        .collect()
}

/// Every DX12 problem in [`plain_sources`] and [`composed_sources`], one line each; empty when
/// they all pass.
#[must_use]
pub fn problems() -> Vec<String> {
    let plain = plain_sources();
    let borrowed: Vec<(&str, &str)> = plain
        .iter()
        .map(|(name, source)| (name.as_str(), source.as_str()))
        .collect();
    let mut out = regia_render::shader_hlsl::wgsl_dx12_problems(&borrowed);
    for (file, source) in composed_sources() {
        match regia_render::compose_source(source, file, std::collections::HashMap::new()) {
            Ok(module) => out.extend(regia_render::shader_hlsl::dx12_module_problems(file, &module)),
            Err(err) => out.push(format!("{file}: compose: {err}")),
        }
    }
    out
}

/// Every Metal problem in [`plain_sources`] and [`composed_sources`] (naga's MSL writer with
/// wgpu's Metal options, `regia_render::shader_msl_check`, ADR-2644), one line each; empty when
/// they all pass. The `xcrun metal` compile of the text is a Mac step.
#[must_use]
pub fn msl_problems() -> Vec<String> {
    let plain = plain_sources();
    let borrowed: Vec<(&str, &str)> = plain
        .iter()
        .map(|(name, source)| (name.as_str(), source.as_str()))
        .collect();
    let mut out = regia_render::shader_msl_check::wgsl_msl_problems(&borrowed);
    for (file, source) in composed_sources() {
        match regia_render::compose_source(source, file, std::collections::HashMap::new()) {
            Ok(module) => out.extend(regia_render::shader_msl_check::msl_module_problems(file, &module)),
            Err(err) => out.push(format!("{file}: compose: {err}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The other crates' shaders translate to MSL too (nothing checked them for Metal before).
    #[test]
    fn engine_crate_shaders_have_no_msl_problem() {
        let problems = msl_problems();
        assert!(
            problems.is_empty(),
            "{} MSL problem(s):\n{}",
            problems.len(),
            problems.join("\n")
        );
    }

    /// The composed modules split exactly as `COMPOSED_LAYOUT_FAILURES` says: a listed module
    /// declares struct or array function-locals (ADR-026), an unlisted one none. Since ADR-7879
    /// `pt.wgsl` and `lightmap_bake.wgsl` are unlisted and take the Vulkan step.
    /// The plain modules split exactly as `PLAIN_LAYOUT_FAILURES` says (ADR-5802): a listed module
    /// still has a Function or Private explicit-layout variable, an unlisted one has none.
    #[test]
    fn the_plain_modules_split_by_the_layout_scan_as_listed() {
        let mut wrong = Vec::new();
        for (name, source) in plain_sources() {
            let errors = match regia_render::wgsl_function_layout_errors(&name, &source) {
                Ok(errors) => errors,
                Err(err) => panic!("{err}"),
            };
            let listed = PLAIN_LAYOUT_FAILURES.contains(&regia_render::contract_file_name(&name));
            if errors.is_empty() == listed {
                wrong.push(format!(
                    "{name}: listed {listed}, {} layout hit(s): {errors:?}",
                    errors.len()
                ));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
        // Every plain module takes the Vulkan step (ADR-7587: the five viewer family variants, the
        // contact shadow and the looks joined it).
        let skipped = plain_sources().len() - plain_vulkan_sources().len();
        assert_eq!(skipped, 0, "plain modules out of the Vulkan step");
    }

    #[test]
    fn the_composed_modules_split_by_the_layout_scan_as_listed() {
        for (file, source) in composed_sources() {
            let errors = match regia_render::composed_function_layout_errors(file, source) {
                Ok(errors) => errors,
                Err(err) => panic!("{err}"),
            };
            let listed = COMPOSED_LAYOUT_FAILURES.contains(&file);
            assert_eq!(
                !errors.is_empty(),
                listed,
                "{file}: {} layout hit(s): {errors:?}",
                errors.len()
            );
            if listed {
                eprintln!(
                    "{file}: {} Function/Private explicit-layout variable(s)",
                    errors.len()
                );
            }
        }
        // Every composed module takes the Vulkan step (ADR-7879).
        assert_eq!(composed_vulkan_sources().len(), composed_sources().len());
    }

    #[test]
    fn the_lists_hold_every_crate() {
        let names: Vec<String> = plain_sources().into_iter().map(|(name, _)| name).collect();
        for want in [
            "terrain.wgsl",
            "impostor_cull.wgsl",
            "ui-overlay.wgsl",
            "vfx sim",
            "vfx present.wgsl",
            "sprite2d.wgsl",
            "portal.wgsl",
            "ui-world.wgsl",
            "ui-world.wgsl (msaa depth)",
            "grass_cull.wgsl",
            "grass.wgsl",
            "water_body.wgsl",
            "water_ocean.wgsl",
            "water_body.wgsl + PROBE_ENTRY",
            "atrous.wgsl",
        ] {
            assert!(
                names.iter().any(|name| name == want),
                "{want} missing from {names:?}"
            );
        }
        let composed: Vec<&str> = composed_sources().into_iter().map(|(name, _)| name).collect();
        assert_eq!(composed, ["pt.wgsl", "lightmap_bake.wgsl"]);
    }

    /// Every `.wgsl` file of the engine crates with an entry point reaches `check-shaders`: by
    /// file name in the permutation matrix's inputs or these lists, or with its text inside a
    /// listed source (a module built by joining files). A new shader cannot skip the checks.
    #[test]
    fn every_entry_shader_of_the_engine_crates_is_checked() {
        use std::collections::HashSet;
        use std::path::Path;

        let matrix = regia_render::shader_matrix_inputs(regia_render::MatrixScope::Full);
        let plain = plain_sources();
        let composed = composed_sources();
        let mut names: HashSet<&str> = matrix.iter().map(|input| input.file.as_str()).collect();
        names.extend(plain.iter().map(|(name, _)| name.as_str()));
        names.extend(composed.iter().map(|(name, _)| *name));
        let mut sources: HashSet<&str> = matrix.iter().map(|input| input.source.as_ref()).collect();
        sources.extend(plain.iter().map(|(_, source)| source.as_str()));
        sources.extend(composed.iter().map(|(_, source)| *source));

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates");
        let mut missing = Vec::new();
        let mut seen = 0usize;
        let mut dirs = vec![root.clone()];
        while let Some(dir) = dirs.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(err) => panic!("reading {}: {err}", dir.display()),
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let file = entry.file_name().to_string_lossy().into_owned();
                if path.is_dir() {
                    // The editor's shaders and test fixtures stay out, as in `shader_sources`.
                    if !matches!(
                        file.as_str(),
                        "target" | "tests" | "examples" | "benches" | "regia_editor"
                    ) {
                        dirs.push(path);
                    }
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("wgsl") {
                    continue;
                }
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(err) => panic!("reading {}: {err}", path.display()),
                };
                let has_entry = text.lines().any(|line| {
                    let code = line.split("//").next().unwrap_or("");
                    ["@compute", "@vertex", "@fragment"]
                        .iter()
                        .any(|attr| code.contains(attr))
                });
                if !has_entry {
                    continue;
                }
                seen += 1;
                let body = text.trim();
                // A Vulkan-only module (ray-query hit-vertex return) is checked by regia_render's
                // own Vulkan-only test, never by DX12 (ADR-3349, ADR-5802).
                let vulkan_only = regia_render::shader_hlsl::VULKAN_ONLY_MODULES
                    .iter()
                    .any(|(name, _)| *name == file);
                if !vulkan_only
                    && !names.contains(file.as_str())
                    && !sources.iter().any(|source| source.contains(body))
                {
                    missing.push(path.strip_prefix(&root).unwrap_or(&path).display().to_string());
                }
            }
        }
        assert!(
            seen > 50,
            "only {seen} entry shaders found under {}",
            root.display()
        );
        missing.sort();
        assert!(
            missing.is_empty(),
            "entry shaders no check-shaders list reaches (add each to `plain_sources`, or to \
             regia_render's `shader_check::shader_sources` or `extra_engine_modules`): {}",
            missing.join(", ")
        );
    }

    #[test]
    fn engine_crate_shaders_have_no_dx12_problem() {
        let problems = problems();
        assert!(
            problems.is_empty(),
            "{} DX12 problem(s):\n{}",
            problems.len(),
            problems.join("\n")
        );
    }
}
