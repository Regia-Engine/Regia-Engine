// SPDX-License-Identifier: MPL-2.0
//! The terrain's shared colour pass compiles dormant paths only where they run (ADR-7886).
//!
//! The shader sits at a compiler cliff (ADR-1452, ADR-5943): code behind a uniform test that a
//! frame never takes still costs the pass. Two such paths were compiled into every DO27 frame:
//!
//! - the anisotropic footprint path (ADR-3957). `layer_tap` is inlined at every texture tap site
//!   and each site carried the footprint assembly (a loop of up to 16 trilinear taps) behind a
//!   test of the frame uniform, which is off on every adapter but Mesa's Intel driver;
//! - the engine cloud map's reads (ADR-320, ADR-5944): the sun term, the ground's sunlit share
//!   and the haze's ray march over the map, behind the map's switch, off in a frame without a
//!   cloud layer.
//!
//! On an RTX 5090 (DO27 switchbacks census, same card, interleaved) compiling both out took the
//! terrain forward pass from 2.26 to 2.14 ms with the same image. The shared pipelines of a
//! terrain whose footprint path is off compile `without_aniso_footprint`; those of a frame
//! without a cloud map compile `without_cloud_map`.
//!
//! These tests read the source and its naga IR (no GPU). `terrain_aniso` renders both footprint
//! paths through the real shader (its footprint runs switch the path on a live terrain).

use regia_terrain::gpu::{
    shader_sources, shared_source, without_aniso_footprint, without_cloud_map, without_cost_views,
    without_ground_look, without_surface_weather,
};

/// Every source a shared colour pipeline compiles before the footprint and cloud choices (as in
/// `layer_tap_cost.rs`).
fn shared_sources() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for hw_height in [false, true] {
        for lights in [false, true] {
            let full = shared_source(hw_height, lights);
            let stripped = without_surface_weather(&without_cost_views(&without_ground_look(&full)));
            out.push((format!("hw height {hw_height}, lights {lights}, full"), full));
            out.push((
                format!("hw height {hw_height}, lights {lights}, stripped"),
                stripped,
            ));
        }
    }
    out
}

fn terrain_wgsl() -> &'static str {
    shader_sources()
        .into_iter()
        .find(|(name, _)| *name == "terrain.wgsl")
        .map_or("", |(_, source)| source)
}

fn parse(name: &str, source: &str) -> naga::Module {
    match naga::front::wgsl::parse_str(source) {
        Ok(module) => module,
        Err(err) => panic!("{name}: {}", err.emit_to_string(source)),
    }
}

fn function<'m>(module: &'m naga::Module, function: &str) -> &'m naga::Function {
    module
        .functions
        .iter()
        .find(|(_, f)| f.name.as_deref() == Some(function))
        .map(|(_, f)| f)
        .unwrap_or_else(|| panic!("no function `{function}`"))
}

/// The module-scope variables `f` names, sorted.
fn globals(module: &naga::Module, f: &naga::Function) -> Vec<String> {
    let mut out: Vec<String> = f
        .expressions
        .iter()
        .filter_map(|(_, e)| match e {
            naga::Expression::GlobalVariable(h) => module.global_variables[*h].name.clone(),
            _ => None,
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

fn has_loop(f: &naga::Function) -> bool {
    f.body.iter().any(|s| matches!(s, naga::Statement::Loop { .. }))
}

/// The lines of `source` that are code, trimmed, without the marker comments.
fn code_lines(source: &str, marker: &str) -> Vec<String> {
    source
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(marker))
        .map(str::to_owned)
        .collect()
}

/// The body of `fn name` in `source` (up to the first line that is a lone `}`).
fn body<'s>(source: &'s str, name: &str) -> &'s str {
    source
        .split(&format!("fn {name}("))
        .nth(1)
        .and_then(|rest| rest.split("\n}").next())
        .unwrap_or_default()
}

/// The footprint test in `terrain.wgsl` carries the markers `without_aniso_footprint` reads.
#[test]
fn the_footprint_test_is_marked() {
    let body = body(terrain_wgsl(), "aniso_footprint");
    for marker in [
        "// @aniso-footprint-begin",
        "// @aniso-footprint-end",
        "// @aniso-footprint-off return false;",
    ] {
        assert!(
            body.contains(marker),
            "`aniso_footprint` must carry `{marker}`: the shared colour pass compiles the footprint loop beside every \
             texture tap site unless the test can be made the constant `false` (ADR-7886); body:\n{body}"
        );
    }
}

/// With the path off, `aniso_footprint` is the constant `false`: it reads nothing, so the driver
/// folds the branch in `layer_tap` and drops the footprint loop at every tap site. With it on,
/// the source keeps the uniform test. Nothing else in the source changes.
#[test]
fn the_off_variant_compiles_the_footprint_path_out() {
    for (name, source) in shared_sources() {
        let off = without_aniso_footprint(&source);
        assert_ne!(
            off, source,
            "{name}: the markers are missing (nothing was stripped)"
        );
        let on = parse(&name, &source);
        assert_eq!(
            globals(&on, function(&on, "aniso_footprint")),
            ["frame"],
            "{name}: with the path on, `aniso_footprint` tests the frame uniform (ADR-3957)"
        );
        let module = parse(&name, &off);
        let test = function(&module, "aniso_footprint");
        assert!(
            globals(&module, test).is_empty() && !has_loop(test),
            "{name}: with the path off, `aniso_footprint` must be the constant `false` (ADR-7886)"
        );
        // Only the test's body moved: the uniform test went, `return false;` came.
        let kept = |s: &str| -> Vec<String> {
            code_lines(s, "// @aniso-footprint")
                .into_iter()
                .filter(|l| l != "return false;" && l != "return frame.texture_lod.w > 0.5;")
                .collect()
        };
        assert_eq!(
            kept(&source),
            kept(&off),
            "{name}: the variant changes `aniso_footprint` only"
        );
        // The assembly stays in the module for the on variant (unreferenced here).
        assert!(
            has_loop(function(&module, "layer_tap_footprint")),
            "{name}: the assembly keeps its loop"
        );
    }
}

/// Every read of the engine cloud map in the shared colour pass sits between `@cloud-map`
/// markers: without them no function reads the map or its switch, and the haze's share is 1.
/// With them the source is the ADR-5944 one.
#[test]
fn the_cloud_map_reads_compile_out_without_a_map() {
    for (name, source) in shared_sources() {
        let on = parse(&name, &source);
        for f in ["shared_cloud_sun", "shared_haze_sun"] {
            assert!(
                globals(&on, function(&on, f)).iter().any(|g| g == "engine_cloud"),
                "{name}: with a cloud map `{f}` reads it (ADR-320, ADR-5944)"
            );
        }
        let off_source = without_cloud_map(&source);
        assert_ne!(
            off_source, source,
            "{name}: the markers are missing (nothing was stripped)"
        );
        let off = parse(&name, &off_source);
        for (fname, f) in off
            .functions
            .iter()
            .map(|(_, f)| (f.name.as_deref().unwrap_or("?"), f))
        {
            if matches!(
                fname,
                "cloud_sun_transmittance" | "cloud_sun_on_ray" | "cloud_shadow_texel"
            ) {
                continue; // the lookup itself (the engine's text, `cloud_shadow_wgsl`), uncalled here
            }
            assert!(
                !globals(&off, f).iter().any(|g| g == "engine_cloud"),
                "{name}: without a cloud map `{fname}` must not read it (ADR-7886)"
            );
        }
        for f in [
            "shared_cloud_sun",
            "shared_ground_sun",
            "shared_sun_shadow",
            "shared_haze_sun",
        ] {
            let src = body(&off_source, f);
            assert!(
                !src.contains("engine_csm.cloud"),
                "{name}: without a cloud map `{f}` must not test the map's switch (ADR-7886):\n{src}"
            );
        }
        assert!(
            body(&off_source, "shared_haze_sun")
                .trim_start()
                .starts_with("world: vec3<f32>) -> f32 {\n    return 1.0;"),
            "{name}: without a cloud map the haze keeps its whole sun (`return 1.0;`):\n{}",
            body(&off_source, "shared_haze_sun")
        );
        // Both strips together, in either order, give the same source.
        assert_eq!(
            without_aniso_footprint(&without_cloud_map(&source)),
            without_cloud_map(&without_aniso_footprint(&source)),
            "{name}: the two strips commute"
        );
    }
}

/// A source without the markers (or with unbalanced ones) comes back unchanged.
#[test]
fn unmarked_sources_are_unchanged() {
    let plain = "fn f() -> bool {\n    return true;\n}\n";
    assert_eq!(without_aniso_footprint(plain), plain);
    assert_eq!(without_cloud_map(plain), plain);
    let open = "fn f() -> bool {\n    // @aniso-footprint-begin\n    return true;\n}\n";
    assert_eq!(without_aniso_footprint(open), open, "unbalanced: unchanged");
    let marked = "fn f() -> bool {\n    // @aniso-footprint-begin\n    return g();\n    // @aniso-footprint-end\n    // @aniso-footprint-off return false;\n}\n";
    assert_eq!(
        without_aniso_footprint(marked),
        "fn f() -> bool {\n    return false;\n}\n"
    );
    let cloud = "fn f() -> f32 {\n    // @cloud-map-begin\n    return g();\n    // @cloud-map-end\n    // @cloud-map-off return 1.0;\n}\n";
    assert_eq!(without_cloud_map(cloud), "fn f() -> f32 {\n    return 1.0;\n}\n");
}
