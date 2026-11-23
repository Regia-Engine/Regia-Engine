// SPDX-License-Identifier: MPL-2.0
//! Per-profile shader statistics of the open document (w12 P-172, ADR-7851): for each render
//! profile the permutation policy enumerates, the graph's colour pass is composed with that
//! profile's defines and counted on the naga IR (`regia_render::surface_graph::graph_shader_stats`):
//! instructions (IR expressions and statements of the fragment entry and its callees, and of
//! the graph code alone), texture samples, bindings and samplers, plus the passes the set
//! builds. The Stats strip under the preview shows the preview's profile; the Permutation
//! policy section and `editor.matgraph_cost` list every profile.

use std::collections::BTreeMap;

use regia_matgraph::MaterialGraph;
use regia_render::permutation_policy::{ProfileTarget, graph_set_passes};
use regia_render::surface_graph::GraphShaderStats;
use regia_render::{RenderProfile, RenderProfilePreset};
use serde_json::{Value, json};

/// The statistics of one profile.
#[derive(Clone, Debug, PartialEq)]
pub struct ProfileStats {
    /// The profile's name (`DesktopHigh`).
    pub profile: &'static str,
    /// The profile draws graphs with the GPU-driven instanced variants (its colour pass is the
    /// instanced module).
    pub instanced: bool,
    /// Pipelines (passes) one graph set builds on this profile.
    pub passes: usize,
    /// The counts, or why the module does not compose on this profile.
    pub stats: Result<GraphShaderStats, String>,
}

/// The profiles the stats cover: the permutation policy's ([`super::model::policy_presets`]).
#[must_use]
pub fn profiles() -> Vec<RenderProfilePreset> {
    super::model::policy_presets()
}

/// Parses a profile name the policy enumerates (`DesktopHigh`, `desktop_high`).
///
/// # Errors
///
/// Unknown, or not one of [`profiles`] (the offline preset is not a game frame).
pub fn parse_profile(name: &str) -> Result<RenderProfilePreset, String> {
    let names = || profiles().iter().map(|p| p.name()).collect::<Vec<_>>().join(", ");
    RenderProfilePreset::from_name(name)
        .filter(|p| profiles().contains(p))
        .ok_or_else(|| format!("unknown render profile `{name}` (one of: {})", names()))
}

/// Compiles `graph` (functions resolved; its own switch defaults pick the permutation, as an
/// instance's view does) and counts its colour pass on `profile`.
///
/// # Errors
///
/// The graph does not compile (the same message for every profile).
pub fn profile_stats(graph: &MaterialGraph, profile: RenderProfilePreset) -> Result<ProfileStats, String> {
    let _span =
        tracing::info_span!(target: "regia_editor", "matgraph_cost", profile = profile.name()).entered();
    let compiled = regia_matgraph::compile_with(graph, &BTreeMap::new()).map_err(|e| e.to_string())?;
    Ok(stats_of_code(&compiled.code(), profile))
}

fn stats_of_code(code: &regia_render::GraphCode, profile: RenderProfilePreset) -> ProfileStats {
    let target = ProfileTarget::of(&RenderProfile::from_preset(profile));
    ProfileStats {
        profile: profile.name(),
        instanced: target.instanced,
        passes: graph_set_passes(target.instanced, code.vertex.is_some()).len(),
        stats: regia_render::surface_graph::graph_shader_stats(code, target.instanced)
            .map_err(|e| e.to_string()),
    }
}

/// [`profile_stats`] for every profile of [`profiles`] (one compile).
///
/// # Errors
///
/// The graph does not compile.
pub fn cost_table(graph: &MaterialGraph) -> Result<Vec<ProfileStats>, String> {
    let compiled = regia_matgraph::compile_with(graph, &BTreeMap::new()).map_err(|e| e.to_string())?;
    let code = compiled.code();
    Ok(profiles().into_iter().map(|p| stats_of_code(&code, p)).collect())
}

/// One line for the Stats strip and the policy section.
#[must_use]
pub fn stats_line(stats: &ProfileStats) -> String {
    match &stats.stats {
        Ok(s) => format!(
            "{}: {} instructions ({} in the graph), {} texture samples ({} in the graph), {} bindings ({} samplers), {} passes{}",
            stats.profile,
            s.instructions,
            s.graph_instructions,
            s.texture_samples,
            s.graph_texture_samples,
            s.bindings,
            s.samplers,
            stats.passes,
            if stats.instanced { ", instanced" } else { "" }
        ),
        Err(e) => format!("{}: does not compose: {e}", stats.profile),
    }
}

/// One profile as protocol JSON.
#[must_use]
pub fn stats_json(stats: &ProfileStats) -> Value {
    match &stats.stats {
        Ok(s) => json!({
            "profile": stats.profile,
            "instanced": stats.instanced,
            "passes": stats.passes,
            "instructions": s.instructions,
            "graph_instructions": s.graph_instructions,
            "texture_samples": s.texture_samples,
            "graph_texture_samples": s.graph_texture_samples,
            "bindings": s.bindings,
            "samplers": s.samplers,
            "error": Value::Null,
        }),
        Err(e) => json!({
            "profile": stats.profile,
            "instanced": stats.instanced,
            "passes": stats.passes,
            "error": e,
        }),
    }
}

/// The Stats strip under the preview: the preview profile's counts (the editor default counts
/// as `DesktopHigh`, the profile the editor's own frame is closest to). Counted when the code
/// key or the profile changes (a parameter drag changes neither), never every frame.
pub fn stats_strip_ui(ui: &mut egui::Ui, editor: &mut super::MatGraphEditor, code_key: Option<u64>) {
    let profile = editor
        .ui
        .parity
        .profile
        .unwrap_or(RenderProfilePreset::DesktopHigh);
    let want = (code_key, profile.name());
    let stale = editor
        .ui
        .parity
        .stats
        .as_ref()
        .is_none_or(|(have, _)| *have != want);
    if stale && code_key.is_some() {
        let graph = super::model::resolved_live(
            &editor.doc.view_graph(),
            editor.graph_dir().as_deref(),
            editor.root.as_deref(),
            &editor.live_functions(),
        );
        let counted = graph.and_then(|g| profile_stats(&g, profile));
        editor.ui.parity.stats = Some((want, counted));
    }
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    match &editor.ui.parity.stats {
        Some((have, Ok(stats))) if *have == want => match &stats.stats {
            Ok(s) => {
                ui.horizontal_wrapped(|ui| {
                    ui.weak(format!("Stats ({}):", stats.profile));
                    ui.label(format!("{} instr", s.instructions))
                        .on_hover_text("IR expressions and statements of the fragment entry and its calls");
                    ui.label(format!("{} in graph", s.graph_instructions));
                    ui.label(format!("{} samples", s.texture_samples));
                    ui.label(format!("{} bindings", s.bindings));
                    ui.label(format!("{} samplers", s.samplers));
                });
            }
            Err(e) => {
                ui.colored_label(accents.error_text, format!("Stats ({}): {e}", stats.profile));
            }
        },
        Some((have, Err(e))) if *have == want => {
            ui.colored_label(accents.error_text, format!("Stats: {e}"));
        }
        _ => {
            ui.weak("Stats: fix the errors to count the shader.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(extra_sample: bool) -> MaterialGraph {
        let base = if extra_sample {
            r#""s2": Sample(texture: "albedo", uv: "uv2"), "k": Float(2.0), "uv2": Mul("uv", "k"),
               "rgb2": Swizzle(input: "s2", channels: "rgb"), "base": Mul("rgb", "rgb2"),"#
        } else {
            r#""base": Mul("rgb", "tint"),"#
        };
        MaterialGraph::from_ron(&format!(
            r#"(
                format_version: 4,
                params: [(name: "tint", value: Color((0.8, 0.2, 0.1)))],
                textures: [(name: "albedo", path: "a.png", color_space: Srgb)],
                nodes: {{
                    "tint": Param("tint"),
                    "uv": TexCoord,
                    "s": Sample(texture: "albedo", uv: "uv"),
                    "rgb": Swizzle(input: "s", channels: "rgb"),
                    {base}
                }},
                output: (base_color: Some("base")),
            )"#
        ))
        .expect("fixture parses")
    }

    #[test]
    fn every_policy_profile_is_counted_and_a_second_sample_shows() {
        let one = cost_table(&graph(false)).expect("compiles");
        assert_eq!(one.len(), profiles().len());
        for p in &one {
            let s = p.stats.as_ref().expect("composes on every profile");
            assert_eq!(s.graph_texture_samples, 1, "{}", stats_line(p));
            assert!(s.instructions > s.graph_instructions && s.graph_instructions > 0);
            assert!(s.bindings > 0 && s.samplers > 0);
            assert!(p.passes >= 6);
        }
        let two = cost_table(&graph(true)).expect("compiles");
        for (a, b) in one.iter().zip(&two) {
            let (a, b) = (a.stats.as_ref().expect("a"), b.stats.as_ref().expect("b"));
            assert_eq!(b.graph_texture_samples, 2);
            assert_eq!(b.texture_samples, a.texture_samples + 1);
        }
        // The same graph counts the same.
        assert_eq!(cost_table(&graph(false)).expect("again"), one);
    }

    #[test]
    fn profiles_parse_by_any_spelling_and_refuse_the_offline_preset() {
        assert_eq!(
            parse_profile("desktop_high"),
            Ok(RenderProfilePreset::DesktopHigh)
        );
        assert_eq!(parse_profile("MobileLow"), Ok(RenderProfilePreset::MobileLow));
        assert!(parse_profile("Cinematic").is_err());
        assert!(
            parse_profile("nope")
                .expect_err("unknown")
                .contains("DesktopHigh")
        );
    }

    #[test]
    fn a_graph_that_does_not_compile_reports_the_error() {
        let mut g = graph(false);
        g.nodes.remove("tint");
        let err = profile_stats(&g, RenderProfilePreset::DesktopHigh).expect_err("does not compile");
        assert!(err.contains("tint"), "{err}");
    }
}
