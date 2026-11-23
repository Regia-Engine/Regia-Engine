// SPDX-License-Identifier: MPL-2.0
//! Cooked material graphs: what the cook writes into the cache and the pak, and what a shipped
//! game loads.
//!
//! The cook compiles every `.matgraph.ron` (with its declared switch defaults) and every
//! `.matinst.ron` (its parent's permutation for the instance's switches, with the instance's
//! parameter and texture overrides) and validates the generated code in every host shader
//! ([`regia_render::surface_graph::validate_graph_code`]). The cooked form carries the
//! generated WGSL, the parameter block, the texture bindings (cooked RGBA8 image ids) and the
//! shading block, so loading one is a postcard decode: no graph compile and no WGSL
//! validation at run time.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_asset::AssetId;
use regia_render::surface_graph::{
    GRAPH_PARAM_SLOTS, GRAPH_TEXTURE_SLOTS, GraphCode, GraphTexture, SurfaceGraph,
};
use regia_render::{LabMaterial, LabTexture};
use serde::{Deserialize, Serialize};

use crate::compile::{CompileErrors, compile_with, default_params};
use crate::format::{ColorSpace, MaterialGraph, Shading};
use crate::instance::{InstanceError, MaterialInstance};

/// Version of the cooked payload ([`CookedMaterialGraph::version`]).
pub const COOKED_MATGRAPH_VERSION: u32 = 1;

/// Type path recorded in the cooked header and the manifest.
pub const COOKED_MATGRAPH_TYPE_PATH: &str = "regia_matgraph::CookedMaterialGraph";

/// A cooked graph could not be built or read.
#[derive(Debug, thiserror::Error)]
pub enum CookedGraphError {
    /// The graph does not compile.
    #[error(transparent)]
    Compile(#[from] CompileErrors),
    /// The instance does not fit its parent.
    #[error(transparent)]
    Instance(#[from] InstanceError),
    /// Postcard encode/decode.
    #[error("cooked material graph: {0}")]
    Postcard(#[from] postcard::Error),
    /// The payload is newer than this engine.
    #[error("cooked material graph v{found} is newer than this engine (v{COOKED_MATGRAPH_VERSION}); recook")]
    NewerVersion {
        /// Version in the payload.
        found: u32,
    },
    /// The parameter block has the wrong size.
    #[error("cooked material graph has {0} parameter slots, expected {GRAPH_PARAM_SLOTS}")]
    ParamSlots(usize),
    /// A texture slot is out of range.
    #[error("cooked material graph binds texture slot {0}")]
    TextureSlot(u8),
    /// A shading value or a parameter is NaN or infinite, or the vertex offset is negative.
    #[error("cooked material graph has an invalid value for `{0}`")]
    BadValue(&'static str),
}

/// One texture binding of a cooked graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedGraphTexture {
    /// Texture slot (`regia_render::surface_graph::GRAPH_TEXTURE_BINDINGS`).
    pub slot: u8,
    /// Sampled as sRGB colour.
    pub srgb: bool,
    /// Cooked RGBA8 image (`regia_asset::CookedImage`); `None` binds white.
    pub image: Option<AssetId>,
    /// The project-relative source path (diagnostics).
    pub path: String,
}

/// A compiled, validated graph permutation with its parameter values and texture bindings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedMaterialGraph {
    /// [`COOKED_MATGRAPH_VERSION`].
    pub version: u32,
    /// Display name.
    pub name: String,
    /// Generated `material_surface`.
    pub surface_wgsl: String,
    /// Generated `material_vertex_offset`, if the graph offsets positions.
    pub vertex_wgsl: Option<String>,
    /// Reads the second UV set.
    pub uses_uv1: bool,
    /// Reads time.
    pub uses_time: bool,
    /// Parameter block (16 `vec4`s, slot 0 = built-ins, written by the renderer's clock).
    pub params: Vec<[f32; 4]>,
    /// Texture bindings.
    pub textures: Vec<CookedGraphTexture>,
    /// Lighting response and render state.
    pub shading: Shading,
    /// Switch values of this permutation.
    pub switches: BTreeMap<String, bool>,
}

impl CookedMaterialGraph {
    /// Compiles and validates `graph` (function files already resolved) for `instance` (or the
    /// graph's own defaults). `image` maps a texture's project-relative path to its cooked
    /// image id (`None`: bind white).
    ///
    /// # Errors
    ///
    /// [`CookedGraphError::Compile`] or [`CookedGraphError::Instance`].
    pub fn cook(
        graph: &MaterialGraph,
        instance: Option<&MaterialInstance>,
        image: &mut dyn FnMut(&str) -> Option<AssetId>,
    ) -> Result<Self, CookedGraphError> {
        let empty = BTreeMap::new();
        let switches = instance.map_or(&empty, |i| &i.switches);
        let compiled = compile_with(graph, switches)?;
        let params = match instance {
            Some(instance) => instance.param_block(graph, 0.0)?,
            None => default_params(graph, 0.0),
        };
        let mut textures = Vec::new();
        for slot in &compiled.textures {
            let path = instance
                .and_then(|i| i.textures.get(&slot.name).cloned())
                .or_else(|| {
                    graph
                        .textures
                        .iter()
                        .find(|t| t.name == slot.name)
                        .map(|t| t.path.clone())
                })
                .unwrap_or_default();
            textures.push(CookedGraphTexture {
                slot: u8::try_from(slot.slot).unwrap_or(u8::MAX),
                srgb: slot.color_space == ColorSpace::Srgb,
                image: if path.is_empty() { None } else { image(&path) },
                path,
            });
        }
        Ok(Self {
            version: COOKED_MATGRAPH_VERSION,
            name: instance.map_or_else(|| graph.name.clone(), |i| i.parent.clone()),
            surface_wgsl: compiled.wgsl.to_string(),
            vertex_wgsl: compiled.vertex_wgsl.as_deref().map(str::to_string),
            uses_uv1: compiled.uses_uv1,
            uses_time: compiled.uses_time,
            params: params.to_vec(),
            textures,
            shading: graph.shading,
            switches: compiled.switches.clone(),
        })
    }

    /// Postcard bytes (the cooked payload).
    ///
    /// # Errors
    ///
    /// [`CookedGraphError::Postcard`].
    pub fn to_postcard(&self) -> Result<Vec<u8>, CookedGraphError> {
        Ok(postcard::to_allocvec(self)?)
    }

    /// Decodes a cooked payload, rejecting a newer version and a malformed block.
    ///
    /// # Errors
    ///
    /// [`CookedGraphError`].
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, CookedGraphError> {
        let mut cooked: Self = postcard::from_bytes(bytes)?;
        // A graph cooked before the rename to Regia Engine imports the engine's shader modules
        // under the legacy name (`<old name>::pbr::surface`); spell them as the renamed library
        // defines them (ADR-1429).
        cooked.surface_wgsl = regia_core::brand::current_type_path(&cooked.surface_wgsl).into_owned();
        cooked.vertex_wgsl = cooked
            .vertex_wgsl
            .as_deref()
            .map(|wgsl| regia_core::brand::current_type_path(wgsl).into_owned());
        if cooked.version > COOKED_MATGRAPH_VERSION {
            return Err(CookedGraphError::NewerVersion {
                found: cooked.version,
            });
        }
        if cooked.params.len() != GRAPH_PARAM_SLOTS {
            return Err(CookedGraphError::ParamSlots(cooked.params.len()));
        }
        if let Some(bad) = cooked
            .textures
            .iter()
            .find(|t| usize::from(t.slot) >= GRAPH_TEXTURE_SLOTS)
        {
            return Err(CookedGraphError::TextureSlot(bad.slot));
        }
        cooked.check_values()?;
        Ok(cooked)
    }

    /// Rejects non-finite shading values and parameters and a negative vertex offset (a
    /// shipped game never compiles, so a damaged payload is caught here).
    fn check_values(&self) -> Result<(), CookedGraphError> {
        let sh = &self.shading;
        let mut values = vec![
            ("clearcoat", sh.clearcoat),
            ("clearcoat_roughness", sh.clearcoat_roughness),
            ("anisotropy", sh.anisotropy),
            ("anisotropy_rotation", sh.anisotropy_rotation),
        ];
        if let crate::format::GraphAlphaMode::Mask(cutoff) = sh.alpha_mode {
            values.push(("alpha cutoff", cutoff));
        }
        for (what, value) in values {
            if !value.is_finite() {
                return Err(CookedGraphError::BadValue(what));
            }
        }
        if !sh.max_vertex_offset.is_finite() || sh.max_vertex_offset < 0.0 {
            return Err(CookedGraphError::BadValue("max_vertex_offset"));
        }
        if self.params.iter().flatten().any(|v| !v.is_finite()) {
            return Err(CookedGraphError::BadValue("params"));
        }
        Ok(())
    }

    /// The code fields as the renderer keys them.
    #[must_use]
    pub fn code(&self) -> GraphCode {
        GraphCode {
            surface: self.surface_wgsl.as_str().into(),
            vertex: self.vertex_wgsl.as_deref().map(Arc::from),
            uv1: self.uses_uv1,
        }
    }

    /// The runtime material: `images` maps a texture slot to its pixels (a missing slot binds
    /// white).
    #[must_use]
    pub fn to_material(&self, images: &BTreeMap<u8, LabTexture>) -> LabMaterial {
        let mut params = [[0.0; 4]; GRAPH_PARAM_SLOTS];
        for (slot, value) in params.iter_mut().zip(&self.params) {
            *slot = *value;
        }
        let mut textures: [Option<GraphTexture>; GRAPH_TEXTURE_SLOTS] = Default::default();
        for texture in &self.textures {
            if let (Some(slot), Some(image)) = (
                textures.get_mut(usize::from(texture.slot)),
                images.get(&texture.slot),
            ) {
                *slot = Some(GraphTexture::rgba(image.clone(), texture.srgb));
            }
        }
        let mut surface = SurfaceGraph::from_code(&self.code());
        surface.uses_time = self.uses_time;
        surface.max_vertex_offset = if self.vertex_wgsl.is_some() {
            self.shading.max_vertex_offset.max(0.0)
        } else {
            0.0
        };
        surface.params = params;
        surface.textures = textures;
        let mut material = LabMaterial::EMPTY;
        crate::material::apply_shading(&mut material, &self.shading);
        material.surface_graph = Some(Arc::new(surface));
        material
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{ColorSpace, Node, ParamDecl, ParamValue, SwitchDecl, TextureDecl};

    fn assert_same_image(left: &LabTexture, right: &LabTexture) {
        assert_eq!(left.width, right.width);
        assert_eq!(left.height, right.height);
        assert_eq!(left.rgba, right.rgba);
    }

    const GRAPH: &str = r#"(
        format_version: 2,
        params: [(name: "tint", value: Color((0.5, 0.25, 0.125)))],
        textures: [(name: "albedo", path: "assets/a.png", color_space: Srgb)],
        switches: [(name: "textured", default: true)],
        nodes: {
            "uv": TexCoord,
            "s": Sample(texture: "albedo", uv: "uv"),
            "rgb": Swizzle(input: "s", channels: "rgb"),
            "tint": Param("tint"),
            "mul": Mul("rgb", "tint"),
            "base": Switch(switch: "textured", on: "mul", off: "tint"),
        },
        output: (base_color: Some("base")),
        shading: (clearcoat: 0.5),
    )"#;

    #[test]
    fn a_cooked_instance_round_trips_into_the_same_material_as_a_compiled_one() {
        let graph = MaterialGraph::from_ron(GRAPH).expect("graph");
        let mut instance = MaterialInstance::new("assets/g.matgraph.ron");
        instance.set("tint", crate::ParamValue::Color([1.0, 0.0, 0.0]));
        instance.textures.insert("albedo".into(), "assets/b.png".into());
        let id = AssetId::new_random();
        let mut asked = Vec::new();
        let cooked = CookedMaterialGraph::cook(&graph, Some(&instance), &mut |path| {
            asked.push(path.to_string());
            Some(id)
        })
        .expect("cook");
        assert_eq!(asked, vec!["assets/b.png".to_string()]);
        let bytes = cooked.to_postcard().expect("encode");
        let back = CookedMaterialGraph::from_postcard(&bytes).expect("decode");
        assert_eq!(back, cooked);

        let image = LabTexture {
            width: 1,
            height: 1,
            rgba: vec![200, 100, 50, 255],
        };
        let mut images = BTreeMap::new();
        images.insert(0u8, image.clone());
        let from_cook = back.to_material(&images);
        let compiled = crate::compile(&graph).expect("compile");
        let mut by_name = BTreeMap::new();
        by_name.insert("albedo".to_string(), image);
        let direct = crate::instance_material(&graph, &compiled, &instance, &by_name, 0.0).expect("direct");
        let (a, b) = (
            from_cook.surface_graph.as_ref().expect("graph"),
            direct.surface_graph.as_ref().expect("graph"),
        );
        assert_eq!(a.key, b.key);
        assert_eq!(a.params, b.params);
        assert_eq!(from_cook.hard_surface.clearcoat, 0.5);
        assert!(a.textures[0].as_ref().is_some_and(|t| t.srgb));

        // A switch override cooks another permutation.
        instance.switches.insert("textured".into(), false);
        let plain = CookedMaterialGraph::cook(&graph, Some(&instance), &mut |_| Some(id)).expect("cook");
        assert_ne!(plain.code().key(), cooked.code().key());
        assert!(!plain.surface_wgsl.contains("textureSample"));
    }

    fn assert_same_runtime_material(left: &LabMaterial, right: &LabMaterial) {
        assert_eq!(left.alpha_mode, right.alpha_mode);
        assert_eq!(left.alpha_cutoff, right.alpha_cutoff);
        assert_eq!(left.unlit, right.unlit);
        assert_eq!(left.hard_surface, right.hard_surface);

        let left = left.surface_graph.as_ref().expect("left graph");
        let right = right.surface_graph.as_ref().expect("right graph");
        assert_eq!(left.key, right.key);
        assert_eq!(left.wgsl, right.wgsl);
        assert_eq!(left.vertex_wgsl, right.vertex_wgsl);
        assert_eq!(left.uses_uv1, right.uses_uv1);
        assert_eq!(left.uses_time, right.uses_time);
        assert_eq!(left.max_vertex_offset, right.max_vertex_offset);
        assert_eq!(left.params, right.params);
        let fallback = LabTexture {
            width: 1,
            height: 1,
            rgba: vec![255; 4],
        };
        for (left, right) in left.textures.iter().zip(&right.textures) {
            match (left, right) {
                (Some(left), Some(right)) => {
                    assert_same_image(&left.image, &right.image);
                    assert_eq!(left.srgb, right.srgb);
                }
                (None, None) => {}
                (None, Some(right)) => {
                    assert_same_image(&right.image, &fallback);
                    assert!(right.srgb);
                }
                _ => panic!("graph texture slots differ"),
            }
        }
    }

    fn parity_graph() -> MaterialGraph {
        let mut graph = MaterialGraph::new("parity");
        graph
            .params
            .push(ParamDecl::new("tint", ParamValue::Color([0.25, 0.5, 0.75])));
        graph.textures = vec![
            TextureDecl {
                name: "unused_hole".into(),
                path: "missing.png".into(),
                color_space: ColorSpace::Srgb,
            },
            TextureDecl {
                name: "mask".into(),
                path: "mask.png".into(),
                color_space: ColorSpace::Linear,
            },
            TextureDecl {
                name: "albedo".into(),
                path: "albedo.png".into(),
                color_space: ColorSpace::Srgb,
            },
        ];
        graph.switches.push(SwitchDecl {
            name: "detail".into(),
            default: true,
        });
        graph.nodes.insert("uv1".into(), Node::TexCoord1);
        graph.nodes.insert("time".into(), Node::Time);
        graph.nodes.insert("tint".into(), Node::Param("tint".into()));
        graph.nodes.insert("mask_uv".into(), Node::TexCoord);
        graph.nodes.insert(
            "mask_sample".into(),
            Node::Sample {
                texture: "mask".into(),
                uv: "mask_uv".into(),
                lod: None,
            },
        );
        graph.nodes.insert("albedo_uv".into(), Node::TexCoord1);
        graph.nodes.insert(
            "albedo_sample".into(),
            Node::Sample {
                texture: "albedo".into(),
                uv: "albedo_uv".into(),
                lod: None,
            },
        );
        graph.nodes.insert(
            "albedo_rgb".into(),
            Node::Swizzle {
                input: "albedo_sample".into(),
                channels: "rgb".into(),
            },
        );
        graph.nodes.insert(
            "detail".into(),
            Node::Switch {
                switch: "detail".into(),
                on: "albedo_rgb".into(),
                off: "tint".into(),
            },
        );
        graph.nodes.insert("offset".into(), Node::Vec3([0.0, 0.2, 0.0]));
        graph.output.base_color = Some("detail".into());
        graph.output.roughness = Some("time".into());
        graph.output.world_position_offset = Some("offset".into());
        graph.shading.max_vertex_offset = 0.375;
        graph.shading.clearcoat = 0.4;
        graph
    }

    #[test]
    fn cooked_postcard_material_matches_compiled_graph_feature_matrix() {
        let graph = parity_graph();
        let mut instance = MaterialInstance::new("parity");
        instance.set("tint", ParamValue::Color([0.9, 0.4, 0.1]));
        let ids = [
            AssetId::sub_asset(AssetId::NIL, "missing"),
            AssetId::sub_asset(AssetId::NIL, "mask"),
            AssetId::sub_asset(AssetId::NIL, "albedo"),
        ];
        let mut resolved = Vec::new();
        let cooked = CookedMaterialGraph::cook(&graph, Some(&instance), &mut |path| {
            resolved.push(path.to_owned());
            (path != "missing.png").then_some(if path == "mask.png" { ids[1] } else { ids[2] })
        })
        .expect("cook");
        assert_eq!(
            resolved,
            vec![
                "missing.png".to_owned(),
                "mask.png".to_owned(),
                "albedo.png".to_owned(),
            ]
        );
        let back =
            CookedMaterialGraph::from_postcard(&cooked.to_postcard().expect("encode")).expect("decode");
        assert!(back.uses_uv1);
        assert!(back.uses_time);
        assert!(back.vertex_wgsl.is_some());
        assert_eq!(back.shading.max_vertex_offset, 0.375);
        assert_eq!(
            back.textures.iter().map(|t| t.slot).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(
            back.textures.iter().map(|t| t.image).collect::<Vec<_>>(),
            [None, Some(ids[1]), Some(ids[2])]
        );
        assert_eq!(
            back.textures.iter().map(|t| t.srgb).collect::<Vec<_>>(),
            [true, false, true]
        );

        let mask = LabTexture {
            width: 1,
            height: 1,
            rgba: vec![32, 64, 128, 255],
        };
        let albedo = LabTexture {
            width: 1,
            height: 1,
            rgba: vec![200, 100, 50, 255],
        };
        // Missing slot 0 intentionally remains unbound (the documented white fallback).
        let cooked_images = BTreeMap::from([(1, mask.clone()), (2, albedo.clone())]);
        let from_cooked = back.to_material(&cooked_images);
        let direct_compiled = crate::compile_with(&graph, &instance.switches).expect("compile switches");
        assert_eq!(back.code(), direct_compiled.code());
        assert_eq!(back.code().uv1, direct_compiled.uses_uv1);
        let named_images = BTreeMap::from([
            (
                "unused_hole".into(),
                LabTexture {
                    width: 1,
                    height: 1,
                    rgba: vec![255; 4],
                },
            ),
            ("mask".into(), mask),
            ("albedo".into(), albedo),
        ]);
        let direct = crate::instance_material(&graph, &direct_compiled, &instance, &named_images, 0.0)
            .expect("direct material");
        assert_same_runtime_material(&from_cooked, &direct);
        let surface = from_cooked.surface_graph.as_ref().expect("surface");
        assert_eq!(surface.max_vertex_offset, 0.375);
        assert_eq!(surface.params[0][0], 0.0);
        assert!(surface.textures[0].is_none());
        assert_eq!(surface.textures[1].as_ref().map(|t| t.srgb), Some(false));
        assert_eq!(surface.textures[2].as_ref().map(|t| t.srgb), Some(true));

        // With no vertex output the declared bound is not used for culling.
        let mut surface_only = graph.clone();
        surface_only.output.world_position_offset = None;
        let no_vertex = CookedMaterialGraph::cook(&surface_only, None, &mut |_| None).expect("surface only");
        let no_vertex_back =
            CookedMaterialGraph::from_postcard(&no_vertex.to_postcard().expect("encode")).expect("decode");
        assert_eq!(
            no_vertex_back
                .to_material(&BTreeMap::new())
                .surface_graph
                .as_ref()
                .expect("surface")
                .max_vertex_offset,
            0.0
        );

        // Switches select code, while parameter overrides stay on each material instance.
        let mut plain_instance = instance.clone();
        plain_instance.switches.insert("detail".into(), false);
        plain_instance.set("tint", ParamValue::Color([0.1, 0.2, 0.3]));
        let plain =
            CookedMaterialGraph::cook(&graph, Some(&plain_instance), &mut |_| None).expect("plain cook");
        let plain_back =
            CookedMaterialGraph::from_postcard(&plain.to_postcard().expect("encode")).expect("decode");
        assert_ne!(plain_back.code().key(), back.code().key());
        let mut same_code_instance = instance.clone();
        same_code_instance.set("tint", ParamValue::Color([0.3, 0.6, 0.9]));
        let same_code = CookedMaterialGraph::cook(&graph, Some(&same_code_instance), &mut |_| None)
            .expect("second instance cook");
        let same_code_back =
            CookedMaterialGraph::from_postcard(&same_code.to_postcard().expect("second instance encode"))
                .expect("second instance decode");
        assert_eq!(back.code().key(), same_code_back.code().key());
        let first_material = back.to_material(&BTreeMap::new());
        let second_material = same_code_back.to_material(&BTreeMap::new());
        let first_surface = first_material
            .surface_graph
            .as_ref()
            .expect("first instance graph");
        let second_surface = second_material
            .surface_graph
            .as_ref()
            .expect("second instance graph");
        assert_eq!(first_surface.key, second_surface.key);
        assert_ne!(first_surface.params[1], second_surface.params[1]);
        assert_eq!(back.params[1], [0.9, 0.4, 0.1, 0.0]);
        assert_eq!(same_code_back.params[1], [0.3, 0.6, 0.9, 0.0]);
        assert_eq!(plain_back.params[1], [0.1, 0.2, 0.3, 0.0]);
        assert!(!plain_back.surface_wgsl.contains("textureSample"));
    }

    #[test]
    fn a_graph_cooked_before_the_rename_imports_the_renamed_shader_library() {
        let graph = MaterialGraph::from_ron(GRAPH).expect("graph");
        let cooked = CookedMaterialGraph::cook(&graph, None, &mut |_| None).expect("cook");
        let current = format!("{}::pbr::surface::", regia_core::brand::MAIN_CRATE);
        let legacy = format!("{}::pbr::surface::", regia_core::brand::LEGACY_MAIN_CRATE);
        assert!(cooked.surface_wgsl.contains(&current), "{}", cooked.surface_wgsl);
        let mut old = cooked.clone();
        old.surface_wgsl = old.surface_wgsl.replace(&current, &legacy);
        old.vertex_wgsl = Some(format!("fn v(in: {legacy}VertexOffsetInput) {{}}"));
        let back = CookedMaterialGraph::from_postcard(&old.to_postcard().expect("encode")).expect("decode");
        assert_eq!(back.surface_wgsl, cooked.surface_wgsl);
        assert_eq!(
            back.vertex_wgsl.as_deref(),
            Some(format!("fn v(in: {current}VertexOffsetInput) {{}}").as_str())
        );
    }

    #[test]
    fn malformed_payloads_are_rejected() {
        let graph = MaterialGraph::from_ron(GRAPH).expect("graph");
        let mut cooked = CookedMaterialGraph::cook(&graph, None, &mut |_| None).expect("cook");
        assert!(cooked.textures[0].image.is_none());
        cooked.version = COOKED_MATGRAPH_VERSION + 1;
        let bytes = cooked.to_postcard().expect("encode");
        assert!(matches!(
            CookedMaterialGraph::from_postcard(&bytes),
            Err(CookedGraphError::NewerVersion { .. })
        ));
        cooked.version = COOKED_MATGRAPH_VERSION;
        cooked.params.pop();
        let bytes = cooked.to_postcard().expect("encode");
        assert!(matches!(
            CookedMaterialGraph::from_postcard(&bytes),
            Err(CookedGraphError::ParamSlots(15))
        ));
        assert!(CookedMaterialGraph::from_postcard(&bytes[..4]).is_err());
    }

    #[test]
    fn non_finite_or_negative_values_are_rejected_on_load() {
        let graph = MaterialGraph::from_ron(GRAPH).expect("graph");
        let good = CookedMaterialGraph::cook(&graph, None, &mut |_| None).expect("cook");
        let decode =
            |c: &CookedMaterialGraph| CookedMaterialGraph::from_postcard(&c.to_postcard().expect("encode"));
        assert!(decode(&good).is_ok());
        let mut c = good.clone();
        c.shading.clearcoat = f32::NAN;
        assert!(matches!(decode(&c), Err(CookedGraphError::BadValue("clearcoat"))));
        let mut c = good.clone();
        c.shading.alpha_mode = crate::format::GraphAlphaMode::Mask(f32::NAN);
        assert!(matches!(
            decode(&c),
            Err(CookedGraphError::BadValue("alpha cutoff"))
        ));
        let mut c = good.clone();
        c.shading.max_vertex_offset = f32::NAN;
        assert!(matches!(
            decode(&c),
            Err(CookedGraphError::BadValue("max_vertex_offset"))
        ));
        let mut c = good.clone();
        c.shading.max_vertex_offset = -0.2;
        assert!(matches!(
            decode(&c),
            Err(CookedGraphError::BadValue("max_vertex_offset"))
        ));
        let mut c = good;
        c.params[3][1] = f32::INFINITY;
        assert!(matches!(decode(&c), Err(CookedGraphError::BadValue("params"))));
    }
}
