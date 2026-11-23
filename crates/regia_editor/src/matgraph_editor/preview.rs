// SPDX-License-Identifier: MPL-2.0
//! The live preview: the open graph's material on the built-in sphere, rendered by the
//! editor's preview renderer (`material_editor::render_sphere_preview`). The preview compiles
//! with full validation (the generated code composed into the forward shader) and loads the
//! graph's textures from the project.

use std::collections::BTreeMap;
use std::path::Path;

use regia_matgraph::MaterialGraph;
use regia_render::{LabMaterial, RenderDevice};

/// Builds the material the preview draws: function files resolved, compiled with validation
/// (default switches), textures loaded (relative to `dir`, then the project's `assets/`), the
/// parameters at their defaults and the material clock at 0.
///
/// # Errors
///
/// A function file, the compile or a texture failed; the message says which.
pub fn graph_lab_material(
    graph: &MaterialGraph,
    dir: Option<&Path>,
    root: Option<&Path>,
) -> Result<LabMaterial, String> {
    lab_material_compiled(graph, dir, root, true)
}

/// [`graph_lab_material`] on the fast compile path (no composition into the forward shader for
/// validation): the per-node previews' derived graphs (ADR-9700). The renderer still builds and
/// checks the pipeline when it draws.
///
/// # Errors
///
/// A function file, the compile or a texture failed; the message says which.
pub fn graph_lab_material_fast(
    graph: &MaterialGraph,
    dir: Option<&Path>,
    root: Option<&Path>,
) -> Result<LabMaterial, String> {
    lab_material_compiled(graph, dir, root, false)
}

fn lab_material_compiled(
    graph: &MaterialGraph,
    dir: Option<&Path>,
    root: Option<&Path>,
    validate: bool,
) -> Result<LabMaterial, String> {
    let graph = super::model::resolved(graph, dir, root)?;
    // A Code node that fails its check previews magenta (ADR-7852); its errors are listed
    // by the diagnostics and the code view.
    let compiled = if validate {
        regia_matgraph::compile_lenient_with(&graph, &BTreeMap::new()).map(|(compiled, _)| compiled)
    } else {
        regia_matgraph::compile_unvalidated(&graph).or_else(|e| {
            if graph
                .nodes
                .values()
                .any(|n| matches!(n, regia_matgraph::Node::Code { .. }))
            {
                regia_matgraph::compile_lenient_with(&graph, &BTreeMap::new()).map(|(compiled, _)| compiled)
            } else {
                Err(e)
            }
        })
    }
    .map_err(|e| e.to_string())?;
    let mut images = BTreeMap::new();
    for decl in &graph.textures {
        let path = super::model::graph_file_path(dir, root, &decl.path)
            .map_err(|e| format!("texture `{}`: {e}", decl.name))?;
        let texture = crate::mesh_source::load_texture_file(&path)
            .map_err(|e| format!("texture `{}` ({}): {e}", decl.name, path.display()))?;
        images.insert(decl.name.clone(), texture);
    }
    regia_matgraph::lab_material(
        &graph,
        &compiled,
        regia_matgraph::default_params(&graph, 0.0),
        &images,
    )
    .map_err(|e| e.to_string())
}

/// Renders the preview sphere with the graph's material at `size` pixels.
///
/// # Errors
///
/// [`graph_lab_material`] failed, or the render produced no image.
#[tracing::instrument(target = "regia_editor", skip_all, name = "matgraph_preview")]
pub fn render_graph_preview(
    device: &RenderDevice,
    graph: &MaterialGraph,
    dir: Option<&Path>,
    root: Option<&Path>,
    size: [f32; 2],
) -> Result<egui::ColorImage, String> {
    let material = graph_lab_material(graph, dir, root)?;
    // Key 0: drawn once and not cached, so an edit that keeps the code key but changes a
    // texture or a parameter shows on the next render.
    crate::material_editor::render_sphere_preview(device, &material, size, 0)
        .ok_or_else(|| "the preview render failed (see the log)".to_string())
}

/// [`render_graph_preview_on`] drawn with render profile `profile`'s frame quality (w12 tier
/// preview, ADR-7851; `None`: the editor's own quality, as [`render_graph_preview_on`]).
///
/// # Errors
///
/// [`graph_lab_material`] failed, or the render produced no image.
#[allow(clippy::too_many_arguments)] // ADR-7851: the preview call's six inputs plus the profile.
#[tracing::instrument(target = "regia_editor", skip_all, name = "matgraph_preview_profile")]
pub fn render_graph_preview_profile(
    device: &RenderDevice,
    graph: &MaterialGraph,
    dir: Option<&Path>,
    root: Option<&Path>,
    size: [f32; 2],
    geometry: &crate::material_editor::PreviewGeometry,
    view: &crate::material_editor::PreviewView,
    profile: Option<regia_render::RenderProfilePreset>,
) -> Result<egui::ColorImage, String> {
    let Some(profile) = profile else {
        return render_graph_preview_on(device, graph, dir, root, size, geometry, view);
    };
    let material = graph_lab_material(graph, dir, root)?;
    let quality =
        regia_render::FrameQuality::from_profile(&regia_render::RenderProfile::from_preset(profile));
    crate::material_editor::render_preview_with_quality(device, &material, geometry, view, size, 0, quality)
        .ok_or_else(|| format!("the preview render failed on {} (see the log)", profile.name()))
}

/// The editor panel's preview (w13, P-083): the graph's material on `geometry` from `view`, at
/// render profile `profile`'s frame quality (`None`: the editor's own), in the shared preview
/// scene `scene` (lighting preset, environment intensity, floor, turntable; ADR-7771). With the
/// default scene and no profile the picture is [`render_graph_preview_on`]'s.
///
/// # Errors
///
/// [`graph_lab_material`] failed, or the render produced no image.
#[allow(clippy::too_many_arguments)] // ADR-7851's preview inputs plus the scene (ADR-7771).
#[tracing::instrument(target = "regia_editor", skip_all, name = "matgraph_preview_scene")]
pub fn render_graph_preview_scene(
    device: &RenderDevice,
    graph: &MaterialGraph,
    dir: Option<&Path>,
    root: Option<&Path>,
    size: [f32; 2],
    geometry: &crate::material_editor::PreviewGeometry,
    view: &crate::material_editor::PreviewView,
    profile: Option<regia_render::RenderProfilePreset>,
    scene: &crate::preview_scene::PreviewContext,
) -> Result<egui::ColorImage, String> {
    let material = graph_lab_material(graph, dir, root)?;
    let quality = profile.map_or(regia_render::FrameQuality::EDITOR, |p| {
        regia_render::FrameQuality::from_profile(&regia_render::RenderProfile::from_preset(p))
    });
    crate::material_editor::render_preview_scene_with_quality(
        device, &material, geometry, view, size, 0, scene, quality,
    )
    .ok_or_else(|| match profile {
        Some(p) => format!("the preview render failed on {} (see the log)", p.name()),
        None => "the preview render failed (see the log)".to_string(),
    })
}

/// Renders the graph's material on any preview shape ([`crate::material_editor::PreviewShape`]
/// through its geometry) from `view` (orbit and light), at `size` pixels (ADR-9700), in the
/// default preview scene.
///
/// # Errors
///
/// [`graph_lab_material`] failed, or the render produced no image.
#[tracing::instrument(target = "regia_editor", skip_all, name = "matgraph_preview_on")]
pub fn render_graph_preview_on(
    device: &RenderDevice,
    graph: &MaterialGraph,
    dir: Option<&Path>,
    root: Option<&Path>,
    size: [f32; 2],
    geometry: &crate::material_editor::PreviewGeometry,
    view: &crate::material_editor::PreviewView,
) -> Result<egui::ColorImage, String> {
    let material = graph_lab_material(graph, dir, root)?;
    crate::material_editor::render_preview(device, &material, geometry, view, size, 0)
        .ok_or_else(|| "the preview render failed (see the log)".to_string())
}

/// A hash of what the preview scene draws ([`crate::preview_scene::PreviewContext::key`]): the
/// panel re-renders its preview when it changes (a turning turntable changes it every step).
#[must_use]
pub fn scene_hash(scene: &crate::preview_scene::PreviewContext) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{:?}", scene.key()).hash(&mut h);
    h.finish()
}

/// Simple numbers an agent can check without looking at the image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PreviewStats {
    /// Mean linear-ish (display) RGB of the sphere's pixels, 0..1.
    pub mean_rgb: [f32; 3],
    /// The center pixel's RGB, 0..255.
    pub center_rgb: [u8; 3],
    /// Share of the image the sphere covers (pixels that differ from the corner pixel).
    pub coverage: f32,
}

/// Measures a preview: the sphere is every pixel that differs from the top-left corner (the
/// sky behind it is smooth there) by more than 24 levels in some channel.
#[must_use]
pub fn measure(image: &egui::ColorImage) -> PreviewStats {
    let [w, h] = image.size;
    let at = |x: usize, y: usize| image.pixels.get(y * w + x).copied().unwrap_or_default();
    let corner = at(0, 0);
    let center = at(w / 2, h / 2);
    let mut sum = [0.0f64; 3];
    let mut n = 0usize;
    for p in &image.pixels {
        let differs = (i16::from(p.r()) - i16::from(corner.r())).abs() > 24
            || (i16::from(p.g()) - i16::from(corner.g())).abs() > 24
            || (i16::from(p.b()) - i16::from(corner.b())).abs() > 24;
        if differs {
            sum[0] += f64::from(p.r());
            sum[1] += f64::from(p.g());
            sum[2] += f64::from(p.b());
            n += 1;
        }
    }
    let mean = |c: f64| {
        if n == 0 {
            0.0
        } else {
            (c / n as f64 / 255.0) as f32
        }
    };
    PreviewStats {
        mean_rgb: [mean(sum[0]), mean(sum[1]), mean(sum[2])],
        center_rgb: [center.r(), center.g(), center.b()],
        coverage: if image.pixels.is_empty() {
            0.0
        } else {
            n as f32 / image.pixels.len() as f32
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red_graph() -> MaterialGraph {
        MaterialGraph::from_ron(
            r#"(
                format_version: 2,
                params: [(name: "tint", value: Color((0.9, 0.05, 0.03)))],
                nodes: { "tint": Param("tint"), "r": Float(0.5) },
                output: (base_color: Some("tint"), roughness: Some("r")),
            )"#,
        )
        .expect("parses")
    }

    /// A `Noise` base color varies over the sphere (lavapipe or hardware; skips with no
    /// adapter): many distinct gray levels, where a constant color gives a few from shading.
    #[test]
    fn a_noise_graph_previews_as_a_varied_sphere() {
        let device = match crate::render::headless_device() {
            Ok(d) => d,
            Err(err) => {
                eprintln!("skip a_noise_graph_previews_as_a_varied_sphere: {err}");
                return;
            }
        };
        let graph = |noise: bool| {
            let base = if noise {
                r#"Noise(input: "p", octaves: 4)"#
            } else {
                "Float(0.5)"
            };
            MaterialGraph::from_ron(&format!(
                r#"(
                    format_version: 4,
                    nodes: {{
                        "wp": WorldPosition, "k": Float(4.0), "p": Mul("wp", "k"),
                        "n": {base}, "rgb": Swizzle(input: "n", channels: "xxx"),
                    }},
                    output: (base_color: Some("rgb")),
                    shading: (unlit: true),
                )"#
            ))
            .expect("parses")
        };
        let levels = |noise: bool| {
            let image =
                render_graph_preview(&device, &graph(noise), None, None, [128.0, 128.0]).expect("renders");
            let [w, h] = image.size;
            let mut seen = std::collections::BTreeSet::new();
            // The central quarter is all sphere.
            for y in h * 3 / 8..h * 5 / 8 {
                for x in w * 3 / 8..w * 5 / 8 {
                    seen.insert(image.pixels[y * w + x].r());
                }
            }
            seen.len()
        };
        let (flat, noisy) = (levels(false), levels(true));
        assert!(flat <= 4, "an unlit constant is flat: {flat} levels");
        assert!(noisy >= 16, "noise varies: {noisy} levels (flat {flat})");
    }

    #[test]
    fn measure_finds_a_disc_and_its_color() {
        let size = [32, 32];
        let mut pixels = vec![egui::Color32::from_rgb(10, 10, 10); 32 * 32];
        for y in 8..24 {
            for x in 8..24 {
                pixels[y * 32 + x] = egui::Color32::from_rgb(200, 20, 20);
            }
        }
        let image = egui::ColorImage::new(size, pixels);
        let stats = measure(&image);
        assert_eq!(stats.center_rgb, [200, 20, 20]);
        assert!((stats.coverage - 0.25).abs() < 1e-6);
        assert!(stats.mean_rgb[0] > 0.7 && stats.mean_rgb[1] < 0.1);
    }

    #[test]
    fn a_broken_graph_names_its_node_in_the_preview_error() {
        let mut graph = red_graph();
        graph
            .nodes
            .insert("bad".into(), regia_matgraph::Node::Saturate("missing".into()));
        graph.output.metallic = Some("bad".into());
        let err = graph_lab_material(&graph, None, None).err().unwrap_or_default();
        assert!(err.contains("bad"), "{err}");
    }

    /// The preview renders a red graph as a red sphere (lavapipe or hardware; skips with no
    /// adapter). No golden image: coverage and hue only.
    #[test]
    fn a_red_graph_previews_as_a_red_sphere() {
        let device = match crate::render::headless_device() {
            Ok(d) => d,
            Err(err) => {
                eprintln!("skip a_red_graph_previews_as_a_red_sphere: {err}");
                return;
            }
        };
        let image = render_graph_preview(&device, &red_graph(), None, None, [96.0, 96.0]).expect("renders");
        assert_eq!(image.size, [96, 96]);
        let stats = measure(&image);
        assert!(stats.coverage > 0.2, "{stats:?}");
        let [r, g, b] = stats.center_rgb;
        assert!(
            r > 60 && r > 2 * g && r > 2 * b,
            "the sphere reads red: {stats:?}"
        );
    }
}
