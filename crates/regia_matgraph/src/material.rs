// SPDX-License-Identifier: MPL-2.0
//! Bridge to the renderer: a compiled graph, a parameter block and resolved images become a
//! [`LabMaterial`] whose surface stage is the graph (`regia_render::surface_graph`).

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_render::surface_graph::{GRAPH_PARAM_SLOTS, GraphTexture, SurfaceGraph};
use regia_render::{
    ALPHA_ADDITIVE, ALPHA_BLEND, ALPHA_MASK, ALPHA_OPAQUE, ALPHA_PREMULTIPLIED, LabMaterial, LabTexture,
};

use crate::compile::CompiledGraph;
use crate::format::{ColorSpace, GraphAlphaMode, MaterialGraph, Shading};
use crate::instance::{InstanceError, MaterialInstance};

/// A graph material could not be built.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum MaterialBuildError {
    /// No image was supplied for a declared texture.
    #[error("no image for texture `{0}`")]
    MissingTexture(String),
    /// The instance does not fit the graph.
    #[error(transparent)]
    Instance(#[from] InstanceError),
    /// The compiled permutation was built with different switches than the instance selects.
    #[error("compiled permutation does not match the instance's switch `{0}`")]
    SwitchMismatch(String),
    /// A compiled texture names a binding slot the surface graph does not have.
    #[error("texture `{name}` uses slot {slot}, but only {max} texture slots exist")]
    SlotOutOfRange {
        /// The texture name.
        name: String,
        /// The slot it asked for.
        slot: usize,
        /// The number of slots available.
        max: usize,
    },
}

/// Lab/runtime material for a compiled graph.
///
/// `params` is the full uniform block (see [`crate::default_params`] and
/// [`MaterialInstance::param_block`]); `images` maps every declared texture name to pixels.
///
/// # Errors
///
/// [`MaterialBuildError::MissingTexture`] when a declared texture has no image;
/// [`MaterialBuildError::SlotOutOfRange`] when a texture slot is past the available slots.
pub fn lab_material(
    graph: &MaterialGraph,
    compiled: &CompiledGraph,
    params: [[f32; 4]; GRAPH_PARAM_SLOTS],
    images: &BTreeMap<String, LabTexture>,
) -> Result<LabMaterial, MaterialBuildError> {
    let mut textures: [Option<GraphTexture>; regia_render::surface_graph::GRAPH_TEXTURE_SLOTS] =
        Default::default();
    for slot in &compiled.textures {
        let image = images
            .get(&slot.name)
            .ok_or_else(|| MaterialBuildError::MissingTexture(slot.name.clone()))?;
        let cell = textures
            .get_mut(slot.slot)
            .ok_or_else(|| MaterialBuildError::SlotOutOfRange {
                name: slot.name.clone(),
                slot: slot.slot,
                max: regia_render::surface_graph::GRAPH_TEXTURE_SLOTS,
            })?;
        *cell = Some(GraphTexture::rgba(
            image.clone(),
            slot.color_space == ColorSpace::Srgb,
        ));
    }
    let mut material = LabMaterial::EMPTY;
    apply_shading(&mut material, &graph.shading);
    material.surface_graph = Some(Arc::new(SurfaceGraph {
        key: compiled.key,
        wgsl: compiled.wgsl.clone(),
        vertex_wgsl: compiled.vertex_wgsl.clone(),
        uses_uv1: compiled.uses_uv1,
        uses_time: compiled.uses_time,
        max_vertex_offset: if compiled.vertex_wgsl.is_some() {
            graph.shading.max_vertex_offset.max(0.0)
        } else {
            0.0
        },
        params,
        textures,
    }));
    Ok(material)
}

/// [`lab_material`] for an instance: the instance's overrides on the parent's defaults.
/// `compiled` must be the permutation of the instance's switches
/// ([`crate::compile_with`]`(graph, &instance.switches)`).
///
/// # Errors
///
/// [`MaterialBuildError`]; [`MaterialBuildError::SwitchMismatch`] when `compiled` was built
/// with other switch values than the instance selects.
pub fn instance_material(
    graph: &MaterialGraph,
    compiled: &CompiledGraph,
    instance: &MaterialInstance,
    images: &BTreeMap<String, LabTexture>,
    time: f32,
) -> Result<LabMaterial, MaterialBuildError> {
    let params = instance.param_block(graph, time)?;
    for decl in &graph.switches {
        let wanted = instance.switches.get(&decl.name).copied().unwrap_or(decl.default);
        if compiled.switches.get(&decl.name) != Some(&wanted) {
            return Err(MaterialBuildError::SwitchMismatch(decl.name.clone()));
        }
    }
    lab_material(graph, compiled, params, images)
}

/// Copies the lighting response and render state onto the material factors the shared
/// lighting code reads.
pub(crate) fn apply_shading(material: &mut LabMaterial, shading: &Shading) {
    let (mode, cutoff) = match shading.alpha_mode {
        GraphAlphaMode::Opaque => (ALPHA_OPAQUE, 0.5),
        GraphAlphaMode::Mask(cutoff) => (ALPHA_MASK, cutoff.clamp(0.0, 1.0)),
        GraphAlphaMode::Blend => (ALPHA_BLEND, 0.5),
        GraphAlphaMode::Premultiplied => (ALPHA_PREMULTIPLIED, 0.5),
        GraphAlphaMode::Additive => (ALPHA_ADDITIVE, 0.5),
    };
    material.alpha_mode = mode;
    material.alpha_cutoff = cutoff;
    material.unlit = shading.unlit;
    let hs = &mut material.hard_surface;
    hs.clearcoat = shading.clearcoat.clamp(0.0, 1.0);
    hs.clearcoat_roughness = shading.clearcoat_roughness;
    hs.anisotropy = shading.anisotropy.clamp(-1.0, 1.0);
    hs.anisotropy_rotation = shading.anisotropy_rotation;
    hs.specular_occlusion = if shading.specular_occlusion { 1.0 } else { 0.0 };
    hs.multi_scatter = if shading.multi_scatter { 1.0 } else { 0.0 };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile_unvalidated;
    use crate::format::{Node, ParamDecl, ParamValue, TextureDecl};

    fn graph() -> MaterialGraph {
        let mut graph = MaterialGraph::new("t");
        graph
            .params
            .push(ParamDecl::new("tint", ParamValue::Color([0.25, 0.5, 0.75])));
        graph.textures.push(TextureDecl {
            name: "albedo".into(),
            path: String::new(),
            color_space: ColorSpace::Srgb,
        });
        graph.nodes.insert("uv".into(), Node::TexCoord);
        graph.nodes.insert(
            "s".into(),
            Node::Sample {
                texture: "albedo".into(),
                uv: "uv".into(),
                lod: None,
            },
        );
        graph.nodes.insert(
            "rgb".into(),
            Node::Swizzle {
                input: "s".into(),
                channels: "rgb".into(),
            },
        );
        graph.nodes.insert("tint".into(), Node::Param("tint".into()));
        graph
            .nodes
            .insert("base".into(), Node::Mul("rgb".into(), "tint".into()));
        graph.output.base_color = Some("base".into());
        graph.shading.alpha_mode = GraphAlphaMode::Mask(0.3);
        graph.shading.clearcoat = 1.0;
        graph
    }

    #[test]
    fn the_material_carries_code_params_textures_and_shading() {
        let graph = graph();
        let compiled = compile_unvalidated(&graph).expect("compile");
        let mut images = BTreeMap::new();
        assert_eq!(
            lab_material(&graph, &compiled, crate::default_params(&graph, 0.0), &images)
                .expect_err("missing"),
            MaterialBuildError::MissingTexture("albedo".into())
        );
        images.insert(
            "albedo".into(),
            LabTexture {
                width: 1,
                height: 1,
                rgba: vec![255; 4],
            },
        );
        let mut instance = MaterialInstance::new("t");
        instance.set("tint", ParamValue::Color([1.0, 0.0, 0.0]));
        let material = instance_material(&graph, &compiled, &instance, &images, 3.0).expect("material");
        let surface = material.surface_graph.as_ref().expect("graph");
        assert_eq!(surface.key, compiled.key);
        assert_eq!(surface.params[0][0], 3.0);
        assert_eq!(surface.params[1], [1.0, 0.0, 0.0, 0.0]);
        assert!(surface.textures[0].as_ref().is_some_and(|t| t.srgb));
        assert!(surface.textures[1].is_none());
        assert_eq!(material.alpha_mode, ALPHA_MASK);
        assert!((material.alpha_cutoff - 0.3).abs() < 1e-6);
        assert_eq!(material.hard_surface.clearcoat, 1.0);
        assert_eq!(material.hard_surface.multi_scatter, 1.0);
    }

    #[test]
    fn an_instance_needs_the_permutation_of_its_switches() {
        let mut graph = graph();
        graph.switches.push(crate::format::SwitchDecl {
            name: "wear".into(),
            default: true,
        });
        let mut images = BTreeMap::new();
        images.insert(
            "albedo".into(),
            LabTexture {
                width: 1,
                height: 1,
                rgba: vec![255; 4],
            },
        );
        let mut instance = MaterialInstance::new("t");
        instance.switches.insert("wear".into(), false);
        let default_branch = compile_unvalidated(&graph).expect("compile");
        assert_eq!(
            instance_material(&graph, &default_branch, &instance, &images, 0.0)
                .expect_err("wrong permutation"),
            MaterialBuildError::SwitchMismatch("wear".into())
        );
        let matching = crate::compile::compile_unvalidated_with(&graph, &instance.switches).expect("compile");
        assert!(instance_material(&graph, &matching, &instance, &images, 0.0).is_ok());
    }

    #[test]
    fn out_of_range_shading_factors_are_clamped() {
        let mut graph = graph();
        graph.shading.alpha_mode = GraphAlphaMode::Mask(2.0);
        graph.shading.clearcoat = 2.0;
        graph.shading.anisotropy = -3.0;
        let compiled = compile_unvalidated(&graph).expect("compile");
        let mut images = BTreeMap::new();
        images.insert(
            "albedo".into(),
            LabTexture {
                width: 1,
                height: 1,
                rgba: vec![255; 4],
            },
        );
        let material = instance_material(&graph, &compiled, &MaterialInstance::new("t"), &images, 0.0)
            .expect("material");
        assert_eq!(material.alpha_cutoff, 1.0);
        assert_eq!(material.hard_surface.clearcoat, 1.0);
        assert_eq!(material.hard_surface.anisotropy, -1.0);
    }

    #[test]
    fn an_out_of_range_texture_slot_is_an_error_not_a_panic() {
        let graph = graph();
        let mut compiled = compile_unvalidated(&graph).expect("compile");
        let bad = regia_render::surface_graph::GRAPH_TEXTURE_SLOTS;
        compiled.textures[0].slot = bad;
        let mut images = BTreeMap::new();
        images.insert(
            "albedo".into(),
            LabTexture {
                width: 1,
                height: 1,
                rgba: vec![255; 4],
            },
        );
        assert_eq!(
            lab_material(&graph, &compiled, crate::default_params(&graph, 0.0), &images).expect_err("slot"),
            MaterialBuildError::SlotOutOfRange {
                name: "albedo".into(),
                slot: bad,
                max: bad
            }
        );
    }

    #[test]
    fn shading_factors_are_clamped_on_the_other_side_too() {
        let mut material = LabMaterial::default();
        let mut shading = graph().shading;
        shading.alpha_mode = GraphAlphaMode::Mask(-1.0);
        shading.clearcoat = -1.0;
        shading.anisotropy = 3.0;
        apply_shading(&mut material, &shading);
        assert_eq!(material.alpha_cutoff, 0.0);
        assert_eq!(material.hard_surface.clearcoat, 0.0);
        assert_eq!(material.hard_surface.anisotropy, 1.0);
    }
}
