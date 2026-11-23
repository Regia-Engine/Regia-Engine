// SPDX-License-Identifier: MPL-2.0
//! Foliage sway on ordinary materials (`specs/88` § 2.5, ADR-2304).
//!
//! [`FoliageSway`] is a vertex stage of a material graph. An imported tree or a scattered plant
//! usually has a plain textured [`LabMaterial`] (base colour, normal, ORM and emissive maps) and
//! no graph, so the sway had no way onto it. [`FoliageSway::adopt`] gives such a material the
//! sway without a material graph asset:
//!
//! - **A plain material** becomes a graph material whose surface is [`FOLIAGE_SURFACE`]: the
//!   standard metallic-roughness surface written as a graph (base colour × albedo map × vertex
//!   colour, the tangent-space normal map, ORM or occlusion, emissive). It reads the factors
//!   from the material's standard block (`material.base_color`, `material.factors`,
//!   `material.emissive`), so a host that tints the material afterwards (the scatter field's
//!   per-class, per-variant tint) still tints it. The four maps move into graph texture slots
//!   0..3 (RGBA8, the cooked block-compressed chain or the shared image, whichever the
//!   material had).
//! - **A graph material without a vertex stage** keeps its surface and gains the sway's vertex
//!   stage, as long as its code does not read the sway's slots (14 and 15).
//! - **A material that already sways** gets the new sway values (same code, same pipelines).
//!
//! [`FoliageSway::of_material`] reads a material's sway back, [`FoliageSway::still`] removes it
//! (the surface stays, so a still and a swaying copy of a plant look the same at rest).
//!
//! The image of an adopted plain material at rest is the standard surface's up to the paths it
//! does not take (detail maps, parallax, edge wear, the normal map's length-based roughness
//! widening); a plant uses none of them. Nothing changes for a material nobody adopts.

use std::sync::Arc;

use crate::LabMaterial;
use crate::compressed_texture::CompressedTexture;
use crate::foliage_wind::{FOLIAGE_SWAY_LIMIT_SLOT, FOLIAGE_SWAY_SLOT, FOLIAGE_SWAY_VERTEX, FoliageSway};
use crate::surface_graph::{GraphCode, GraphTexture, SurfaceGraph};
use crate::texture_residency::MaterialImage;

/// Graph parameter slot of [`FOLIAGE_SURFACE`]'s map flags: `[normal map, packed ORM, vertex
/// occlusion, 0]` (1 = on).
pub const FOLIAGE_SURFACE_FLAGS_SLOT: usize = 1;

/// The standard metallic-roughness surface as a graph surface function, for plain materials
/// [`FoliageSway::adopt`] turns into graph materials. Texture slots: 0 base colour (sRGB),
/// 1 normal (linear), 2 occlusion or packed ORM (linear), 3 emissive (sRGB); an empty slot binds
/// white. The normal's Z is rebuilt from X and Y, so a two-channel compressed normal reads the
/// same as an RGB one.
pub const FOLIAGE_SURFACE: &str = r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    let flags = material.graph[1];
    let albedo = textureSample(albedo_tex, tex_samp, in.uv);
    let packed = textureSample(normal_tex, tex_samp, in.uv);
    let orm = textureSample(occlusion_tex, tex_samp, in.uv);
    let glow = textureSample(emissive_tex, tex_samp, in.uv);
    let vertex_alpha = select(in.vertex_color.a, 1.0, flags.z > 0.5);
    let color = albedo.rgb * material.base_color.rgb * in.vertex_color.rgb;
    let alpha = albedo.a * material.base_color.a * vertex_alpha;
    var n = in.world_normal;
    if (flags.x > 0.5 && length(in.world_tangent.xyz) > 1e-5) {
        let raw = packed.xy * 2.0 - 1.0;
        let z = sqrt(max(1.0 - dot(raw, raw), 0.0));
        let t = normalize(in.world_tangent.xyz);
        let b = cross(in.world_normal, t) * in.world_tangent.w;
        let m = mat3x3<f32>(t, b, in.world_normal) * vec3<f32>(raw * material.factors.z, z);
        if (dot(m, m) > 1e-8) {
            n = normalize(m);
        }
    }
    var metallic = material.factors.x;
    var roughness = material.factors.y;
    if (flags.y > 0.5) {
        metallic = clamp(metallic * orm.b, 0.0, 1.0);
        roughness = clamp(roughness * orm.g, 0.0, 1.0);
    }
    let occlusion = 1.0 + material.factors.w * (orm.r - 1.0);
    let emissive = material.emissive.rgb * material.emissive.w * glow.rgb;
    return regia::pbr::surface::Surface(color, alpha, metallic, roughness, n, occlusion, emissive, 0.0);
}
";

/// Why [`FoliageSway::adopt`] refused a material.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SwayAdoptError {
    /// The material's graph already has a vertex stage of its own (not the sway).
    #[error("the material graph already has its own vertex stage; add the sway to the graph instead")]
    VertexStageTaken,
    /// The material's graph reads parameter slot 14 or 15, where the sway keeps its values.
    #[error("the material graph uses parameter slots 14-15, which the sway needs; use at most 13 parameters")]
    SlotsTaken,
}

impl SwayAdoptError {
    /// Stable error code (agent protocol, `book/agents/foliage-wind.md`).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::VertexStageTaken => "sway_vertex_stage_taken",
            Self::SlotsTaken => "sway_slots_taken",
        }
    }
}

/// The graph texture for one standard slot: the shared or dynamic image, else the RGBA8 bytes
/// with the cooked chain when there is one; `None` when the material has nothing there.
fn slot_texture(
    image: Option<&MaterialImage>,
    rgba: Option<&crate::LabTexture>,
    compressed: Option<&CompressedTexture>,
    srgb: bool,
) -> Option<GraphTexture> {
    match image {
        Some(MaterialImage::Shared(shared)) => return Some(GraphTexture::from_shared(shared.clone())),
        Some(MaterialImage::Dynamic(dynamic)) => return Some(GraphTexture::from_dynamic(dynamic.clone())),
        // A counted texture keeps its one upload, in the slot's color space (ADR-8610).
        Some(MaterialImage::Asset(asset)) => return Some(GraphTexture::from_asset(asset.clone(), srgb)),
        None => {}
    }
    let compressed = compressed.cloned().map(Arc::new);
    match (rgba, compressed) {
        (Some(rgba), compressed) => Some(GraphTexture {
            compressed,
            ..GraphTexture::rgba(rgba.clone(), srgb)
        }),
        (None, Some(chain)) => Some(GraphTexture {
            compressed: Some(chain),
            ..GraphTexture::default()
        }),
        (None, None) => None,
    }
}

/// The code reads `material.graph[slot]`.
fn reads_slot(wgsl: &str, slot: usize) -> bool {
    wgsl.contains(&format!("graph[{slot}]"))
}

impl FoliageSway {
    /// `material` with this sway (see the module docs): a plain material becomes a
    /// [`FOLIAGE_SURFACE`] graph material with its maps in graph slots 0..3; a graph material
    /// without a vertex stage gains the sway stage; a swaying material takes the new values.
    /// Every other field (alpha mode, double-sided, tint, weather ...) is kept.
    ///
    /// # Errors
    ///
    /// [`SwayAdoptError::VertexStageTaken`] for a graph with its own vertex stage,
    /// [`SwayAdoptError::SlotsTaken`] for a graph that reads slot 14 or 15.
    pub fn adopt(&self, material: &LabMaterial) -> Result<LabMaterial, SwayAdoptError> {
        let mut out = material.clone();
        let graph = match material.surface_graph.as_deref() {
            Some(graph) if graph.vertex_wgsl.as_deref() == Some(FOLIAGE_SWAY_VERTEX) => {
                let mut graph = graph.clone();
                self.apply(&mut graph);
                graph
            }
            Some(graph) if graph.vertex_wgsl.is_some() => return Err(SwayAdoptError::VertexStageTaken),
            Some(graph) => {
                if reads_slot(&graph.wgsl, FOLIAGE_SWAY_SLOT)
                    || reads_slot(&graph.wgsl, FOLIAGE_SWAY_LIMIT_SLOT)
                {
                    return Err(SwayAdoptError::SlotsTaken);
                }
                let mut code = Self::graph_code(&graph.wgsl);
                code.uv1 = graph.uses_uv1;
                let mut swaying = SurfaceGraph::from_code(&code);
                swaying.params = graph.params;
                swaying.textures = graph.textures.clone();
                swaying.uses_time = graph.uses_time;
                swaying.max_vertex_offset = graph.max_vertex_offset;
                self.apply(&mut swaying);
                swaying
            }
            None => {
                let mut graph = SurfaceGraph::from_code(&Self::graph_code(FOLIAGE_SURFACE));
                let compressed = material.compressed.as_deref();
                let images = &material.images;
                graph.textures[0] = slot_texture(
                    images.albedo.as_ref(),
                    material.albedo.as_ref(),
                    compressed.and_then(|c| c.albedo.as_ref()),
                    true,
                );
                graph.textures[1] = slot_texture(
                    images.normal.as_ref(),
                    material.normal_map.as_ref(),
                    compressed.and_then(|c| c.normal.as_ref()),
                    false,
                );
                graph.textures[2] = slot_texture(
                    images.occlusion.as_ref(),
                    material.occlusion_map.as_ref(),
                    compressed.and_then(|c| c.occlusion.as_ref()),
                    false,
                );
                graph.textures[3] = slot_texture(
                    images.emissive.as_ref(),
                    material.emissive_map.as_ref(),
                    compressed.and_then(|c| c.emissive.as_ref()),
                    true,
                );
                let on = |b: bool| if b { 1.0 } else { 0.0 };
                graph.params[FOLIAGE_SURFACE_FLAGS_SLOT] = [
                    // The standard shader applies the map only for a positive scale.
                    on(graph.textures[1].is_some() && material.normal_scale > 0.0),
                    on(material.orm_packed && graph.textures[2].is_some()),
                    // Vertex alpha is occlusion only where the standard path reads it so
                    // (opaque, lit, not hair, no transmission); elsewhere it stays opacity.
                    on(crate::vertex_ao::is_active(material)),
                    0.0,
                ];
                self.apply(&mut graph);
                // The maps live in the graph now: drop the standard slots' copies.
                out.albedo = None;
                out.normal_map = None;
                out.occlusion_map = None;
                out.emissive_map = None;
                out.compressed = None;
                out.images = crate::MaterialImages::EMPTY;
                graph
            }
        };
        out.surface_graph = Some(Arc::new(graph));
        Ok(out)
    }

    /// The sway `material` carries ([`Self::adopt`] or [`Self::graph`]), or `None` when its graph
    /// has no [`FOLIAGE_SWAY_VERTEX`] stage.
    #[must_use]
    pub fn of_material(material: &LabMaterial) -> Option<Self> {
        let graph = material.surface_graph.as_deref()?;
        if graph.vertex_wgsl.as_deref() != Some(FOLIAGE_SWAY_VERTEX) {
            return None;
        }
        let [stiffness, height_m, flutter_m, gust_period_s] = graph.params[FOLIAGE_SWAY_SLOT];
        Some(Self {
            stiffness,
            height_m,
            flutter_m,
            gust_period_s,
            max_wind_m_per_s: graph.params[FOLIAGE_SWAY_LIMIT_SLOT][0],
            style: if graph.params[FOLIAGE_SWAY_LIMIT_SLOT][1] > 0.5 {
                crate::wind_natural::WindStyle::Natural
            } else {
                crate::wind_natural::WindStyle::Classic
            },
        })
    }

    /// `material` without its sway: the same surface (and textures) with no vertex stage, so it
    /// draws at rest and casts like a static caster. A material without a sway is returned
    /// unchanged.
    #[must_use]
    pub fn still(material: &LabMaterial) -> LabMaterial {
        let mut out = material.clone();
        let Some(graph) = material
            .surface_graph
            .as_deref()
            .filter(|g| g.vertex_wgsl.as_deref() == Some(FOLIAGE_SWAY_VERTEX))
        else {
            return out;
        };
        let mut still = SurfaceGraph::from_code(&GraphCode {
            surface: graph.wgsl.clone(),
            vertex: None,
            uv1: graph.uses_uv1,
        });
        still.params = graph.params;
        still.params[FOLIAGE_SWAY_SLOT] = [0.0; 4];
        still.params[FOLIAGE_SWAY_LIMIT_SLOT] = [0.0; 4];
        still.textures = graph.textures.clone();
        // The adopted standard surface never reads the clock; another surface may.
        still.uses_time = graph.uses_time && &*graph.wgsl != FOLIAGE_SURFACE;
        out.surface_graph = Some(Arc::new(still));
        out
    }

    /// The plain material a [`FOLIAGE_SURFACE`] material was adopted from: no graph, the maps
    /// back in the standard slots (shared, dynamic and counted images in `images`, RGBA8 bytes and
    /// cooked chains in their slots). For CPU readers of the standard slots (the scatter impostor
    /// bake).
    /// `None` for any other material (plain, or another graph).
    #[must_use]
    pub fn plain(material: &LabMaterial) -> Option<LabMaterial> {
        let graph = material
            .surface_graph
            .as_deref()
            .filter(|g| &*g.wgsl == FOLIAGE_SURFACE)?;
        let mut out = material.clone();
        out.surface_graph = None;
        let mut compressed = crate::compressed_texture::CompressedMaps::default();
        for (slot, texture) in graph.textures.iter().take(4).enumerate() {
            let Some(t) = texture else { continue };
            let image = if let Some(asset) = &t.asset {
                Some(MaterialImage::Asset(asset.clone()))
            } else if let Some(shared) = &t.shared {
                Some(MaterialImage::Shared(shared.clone()))
            } else {
                t.dynamic.clone().map(MaterialImage::Dynamic)
            };
            let rgba = (image.is_none() && !t.image.rgba.is_empty()).then(|| t.image.clone());
            let chain = t.compressed.as_deref().filter(|_| image.is_none()).cloned();
            match slot {
                0 => (out.images.albedo, out.albedo, compressed.albedo) = (image, rgba, chain),
                1 => (out.images.normal, out.normal_map, compressed.normal) = (image, rgba, chain),
                2 => (out.images.occlusion, out.occlusion_map, compressed.occlusion) = (image, rgba, chain),
                _ => (out.images.emissive, out.emissive_map, compressed.emissive) = (image, rgba, chain),
            }
        }
        if compressed != crate::compressed_texture::CompressedMaps::default() {
            out.compressed = Some(Arc::new(compressed));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LabTexture;

    fn texture(rgba: [u8; 4]) -> LabTexture {
        LabTexture {
            width: 1,
            height: 1,
            rgba: rgba.to_vec(),
        }
    }

    fn leaf_material() -> LabMaterial {
        LabMaterial {
            base_color: [0.3, 0.5, 0.2, 1.0],
            albedo: Some(texture([200, 220, 180, 255])),
            normal_map: Some(texture([128, 128, 255, 255])),
            occlusion_map: Some(texture([255, 128, 0, 255])),
            orm_packed: true,
            normal_scale: 1.0,
            alpha_mode: crate::ALPHA_MASK,
            alpha_cutoff: 0.4,
            double_sided: true,
            ..LabMaterial::default()
        }
    }

    fn surface_flags(material: &LabMaterial) -> [f32; 4] {
        let adopted = FoliageSway::default().adopt(material).expect("plain material");
        adopted.surface_graph.expect("graph").params[FOLIAGE_SURFACE_FLAGS_SLOT]
    }

    /// Audit G render5 #14: flag Z (vertex alpha is occlusion, not opacity) follows
    /// [`crate::vertex_ao::is_active`] like the standard shader, so a masked, blended, unlit,
    /// hair or transmissive material keeps its vertex alpha as opacity after adoption.
    #[test]
    fn the_vertex_occlusion_flag_follows_the_standard_predicate() {
        let opaque = LabMaterial {
            alpha_mode: crate::ALPHA_OPAQUE,
            vertex_ao: true,
            ..leaf_material()
        };
        assert!(crate::vertex_ao::is_active(&opaque));
        assert_eq!(surface_flags(&opaque)[2], 1.0, "opaque lit vertex AO");
        let masked = LabMaterial {
            vertex_ao: true,
            alpha_cutoff: 0.5,
            ..leaf_material()
        };
        assert_eq!(
            surface_flags(&masked)[2],
            0.0,
            "a masked material's vertex alpha is opacity"
        );
        let blended = LabMaterial {
            alpha_mode: crate::ALPHA_BLEND,
            ..masked.clone()
        };
        let unlit = LabMaterial {
            unlit: true,
            ..opaque.clone()
        };
        let transmissive = LabMaterial {
            transmission: 0.5,
            ..opaque.clone()
        };
        let mut hair = opaque.clone();
        hair.hair.enabled = true;
        for (name, material) in [
            ("blend", blended),
            ("unlit", unlit),
            ("transmission", transmissive),
            ("hair", hair),
        ] {
            assert_eq!(surface_flags(&material)[2], 0.0, "{name}");
        }
        assert_eq!(surface_flags(&leaf_material())[2], 0.0, "vertex AO off");
    }

    /// Audit G render5 #15: the standard shader applies the normal map only for
    /// `normal_scale > 0`; a negative scale must not apply it with X and Y flipped.
    #[test]
    fn the_normal_map_flag_needs_a_positive_scale() {
        for (scale, want) in [(-1.0, 0.0), (0.0, 0.0), (0.5, 1.0), (1.0, 1.0)] {
            let material = LabMaterial {
                normal_scale: scale,
                ..leaf_material()
            };
            assert_eq!(surface_flags(&material)[0], want, "normal_scale {scale}");
        }
        let no_map = LabMaterial {
            normal_map: None,
            ..leaf_material()
        };
        assert_eq!(surface_flags(&no_map)[0], 0.0, "no normal map");
    }

    /// Audit G render5 #13: a double-sided back face shades with the front's mapped normal
    /// reversed on the standard path (`back_face_tangent`: the whole frame negated). The graph
    /// surface input flipped only the normal, so [`FOLIAGE_SURFACE`]'s frame mirrored the map's X
    /// slope there. The GPU frame cannot be read back here: this pins the shader text and checks
    /// a CPU mirror of both frames.
    #[test]
    fn a_back_face_graph_frame_is_the_front_frame_negated() {
        let forward = include_str!("../shaders/lab_forward.wgsl");
        let start = forward
            .find("fn graph_surface_input(")
            .expect("graph_surface_input in lab_forward.wgsl");
        let end = start
            + forward[start..]
                .find("\n}\n")
                .expect("end of graph_surface_input");
        let input = &forward[start..end];
        // The back face's tangent and its bitangent sign are both negated: the graph rebuilds
        // the bitangent as `cross(n, t) * w`, so negating `t` alone would keep `b` the front's.
        let flips_tangent = input.contains("back_face_tangent(")
            && input.contains("select(-in.world_tangent.w, in.world_tangent.w, front_facing)");
        // The frame FOLIAGE_SURFACE builds from that input (mirrored below).
        assert!(FOLIAGE_SURFACE.contains("let t = normalize(in.world_tangent.xyz);"));
        assert!(FOLIAGE_SURFACE.contains("let b = cross(in.world_normal, t) * in.world_tangent.w;"));
        assert!(FOLIAGE_SURFACE.contains("mat3x3<f32>(t, b, in.world_normal)"));

        let cross = |a: [f32; 3], b: [f32; 3]| {
            [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ]
        };
        let neg = |v: [f32; 3]| v.map(|c| -c);
        let apply = |t: [f32; 3], b: [f32; 3], n: [f32; 3], m: [f32; 3]| {
            [0, 1, 2].map(|i| t[i] * m[0] + b[i] * m[1] + n[i] * m[2])
        };
        let (n0, t0) = ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0]);
        let mapped = [0.6, 0.3, 0.74];
        for w in [1.0f32, -1.0] {
            for front in [true, false] {
                let n = if front { n0 } else { neg(n0) };
                // Standard path (`lab_forward.wgsl`): b from the flipped normal and the front
                // tangent, then `back_face_tangent`.
                let b_std = cross(n, t0).map(|c| c * w);
                let t_std = if front { t0 } else { neg(t0) };
                let standard = apply(t_std, b_std, n, mapped);
                // Graph path: the input's tangent, then FOLIAGE_SURFACE's frame.
                let (t_in, w_in) = if front || !flips_tangent {
                    (t0, w)
                } else {
                    (neg(t0), -w)
                };
                let b_graph = cross(n, t_in).map(|c| c * w_in);
                let graph = apply(t_in, b_graph, n, mapped);
                assert_eq!(graph, standard, "front {front}, handedness {w}");
                if !front {
                    // The back face's mapped normal is the front's reversed.
                    let front_n = apply(t0, cross(n0, t0).map(|c| c * w), n0, mapped);
                    assert_eq!(graph, neg(front_n));
                }
            }
        }
    }

    #[test]
    fn the_standard_surface_composes_with_the_sway_in_every_host() {
        crate::surface_graph::validate_graph_code(&FoliageSway::graph_code(FOLIAGE_SURFACE))
            .expect("foliage surface + sway in forward, shadow and velocity");
        crate::surface_graph::validate_graph_code(&GraphCode::surface(FOLIAGE_SURFACE))
            .expect("the still surface alone");
    }

    #[test]
    fn a_plain_material_moves_its_maps_into_the_graph_and_keeps_its_state() {
        let leaf = leaf_material();
        let sway = FoliageSway::default();
        let adopted = sway.adopt(&leaf).expect("plain material");
        let graph = adopted.surface_graph.as_deref().expect("graph");
        assert_eq!(&*graph.wgsl, FOLIAGE_SURFACE);
        assert_eq!(graph.vertex_wgsl.as_deref(), Some(FOLIAGE_SWAY_VERTEX));
        assert!(graph.uses_time);
        assert!(graph.max_vertex_offset >= sway.max_offset());
        assert_eq!(graph.params[FOLIAGE_SURFACE_FLAGS_SLOT], [1.0, 1.0, 0.0, 0.0]);
        let albedo = graph.textures[0].as_ref().expect("albedo slot");
        assert!(albedo.srgb);
        assert_eq!(albedo.image.rgba, vec![200, 220, 180, 255]);
        assert!(!graph.textures[1].as_ref().expect("normal").srgb);
        assert!(graph.textures[3].is_none(), "no emissive map, white slot");
        assert!(adopted.albedo.is_none() && adopted.normal_map.is_none() && adopted.occlusion_map.is_none());
        // Everything that is not a map is the material's.
        assert_eq!(adopted.base_color, leaf.base_color);
        assert_eq!(adopted.alpha_mode, leaf.alpha_mode);
        assert_eq!(adopted.alpha_cutoff, leaf.alpha_cutoff);
        assert!(adopted.double_sided);
        assert_eq!(FoliageSway::of_material(&adopted), Some(sway.sanitized()));
        assert_eq!(FoliageSway::of_material(&leaf), None);
    }

    #[test]
    fn adopting_again_keeps_the_code_and_takes_the_new_values() {
        let first = FoliageSway::default().adopt(&leaf_material()).expect("first");
        let stiff = FoliageSway {
            stiffness: 0.01,
            height_m: 12.0,
            ..FoliageSway::default()
        };
        let second = stiff.adopt(&first).expect("re-adopt");
        let (a, b) = (
            first.surface_graph.as_deref().expect("a"),
            second.surface_graph.as_deref().expect("b"),
        );
        assert_eq!(a.key, b.key, "same code, same pipelines");
        assert_eq!(FoliageSway::of_material(&second), Some(stiff));
        assert_eq!(
            b.textures[0].as_ref().map(|t| t.image.rgba.clone()),
            Some(vec![200, 220, 180, 255])
        );
    }

    #[test]
    fn a_graph_material_keeps_its_surface_and_gains_the_stage() {
        let surface = crate::surface_graph::SAMPLE_SURFACE;
        let mut graph = SurfaceGraph::new(surface);
        graph.params[1] = [0.2, 0.4, 0.6, 0.0];
        graph.uses_uv1 = true;
        let material = LabMaterial {
            surface_graph: Some(Arc::new(graph)),
            ..LabMaterial::default()
        };
        let adopted = FoliageSway::default()
            .adopt(&material)
            .expect("surface-only graph");
        let g = adopted.surface_graph.as_deref().expect("graph");
        assert_eq!(&*g.wgsl, surface);
        assert!(g.uses_uv1);
        assert_eq!(g.params[1], [0.2, 0.4, 0.6, 0.0]);
        assert_eq!(g.vertex_wgsl.as_deref(), Some(FOLIAGE_SWAY_VERTEX));
    }

    #[test]
    fn graphs_that_need_the_stage_or_the_slots_are_refused() {
        let own_stage = SurfaceGraph::from_code(&GraphCode {
            surface: crate::surface_graph::SAMPLE_SURFACE.into(),
            vertex: Some(crate::surface_graph::SAMPLE_VERTEX.into()),
            uv1: false,
        });
        let material = |g: SurfaceGraph| LabMaterial {
            surface_graph: Some(Arc::new(g)),
            ..LabMaterial::default()
        };
        let err = FoliageSway::default()
            .adopt(&material(own_stage))
            .expect_err("own vertex stage");
        assert_eq!(err, SwayAdoptError::VertexStageTaken);
        assert_eq!(err.code(), "sway_vertex_stage_taken");
        let greedy = SurfaceGraph::new(crate::surface_graph::SAMPLE_SURFACE.replace("graph[1]", "graph[15]"));
        let err = FoliageSway::default()
            .adopt(&material(greedy))
            .expect_err("slot 15");
        assert_eq!(err.code(), "sway_slots_taken");
    }

    #[test]
    fn plain_gives_the_maps_back_to_the_standard_slots() {
        let leaf = leaf_material();
        let adopted = FoliageSway::default().adopt(&leaf).expect("adopt");
        for m in [&adopted, &FoliageSway::still(&adopted)] {
            let plain = FoliageSway::plain(m).expect("an adopted material");
            assert!(plain.surface_graph.is_none());
            assert_eq!(
                plain.albedo.as_ref().map(|t| &t.rgba),
                leaf.albedo.as_ref().map(|t| &t.rgba)
            );
            assert_eq!(
                plain.normal_map.as_ref().map(|t| &t.rgba),
                leaf.normal_map.as_ref().map(|t| &t.rgba)
            );
            assert_eq!(
                plain.occlusion_map.as_ref().map(|t| &t.rgba),
                leaf.occlusion_map.as_ref().map(|t| &t.rgba)
            );
            assert!(plain.emissive_map.is_none() && plain.compressed.is_none());
            assert_eq!(plain.base_color, leaf.base_color);
        }
        assert!(
            FoliageSway::plain(&leaf).is_none(),
            "a plain material has nothing to give back"
        );
    }

    #[test]
    fn still_drops_the_stage_and_keeps_the_look() {
        let adopted = FoliageSway::default().adopt(&leaf_material()).expect("adopt");
        let still = FoliageSway::still(&adopted);
        let (a, s) = (
            adopted.surface_graph.as_deref().expect("a"),
            still.surface_graph.as_deref().expect("s"),
        );
        assert_eq!(s.wgsl, a.wgsl);
        assert!(s.vertex_wgsl.is_none());
        assert!(!s.uses_time, "the standard surface does not read the clock");
        assert_ne!(s.key, a.key);
        assert_eq!(
            s.params[FOLIAGE_SURFACE_FLAGS_SLOT],
            a.params[FOLIAGE_SURFACE_FLAGS_SLOT]
        );
        assert_eq!(FoliageSway::of_material(&still), None);
        // A material without a sway comes back as it was.
        let plain = leaf_material();
        assert!(FoliageSway::still(&plain).surface_graph.is_none());
    }
}
