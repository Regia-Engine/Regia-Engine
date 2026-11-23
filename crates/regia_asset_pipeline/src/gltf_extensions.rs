// SPDX-License-Identifier: MPL-2.0
//! glTF extension import: the typed material extensions, the side table that keeps everything
//! else, and the handler hook (`docs/formats/gltf-extensions.md`, ADR-252, feature audit G9).
//!
//! The importer reads the material extensions the renderer has parameters for and keeps the
//! rest, so no authored data is lost between a DCC tool and Tincal or DO27:
//!
//! | Extension | Where it lands |
//! |---|---|
//! | `KHR_materials_clearcoat` | `MaterialAssetRon::clearcoat` (factor, roughness, normal map); textures in the side table |
//! | `KHR_materials_anisotropy` | `MaterialAssetRon::anisotropy` (strength, rotation as a tangent-space direction); the renderer's `HardSurfaceFactors::anisotropy` consumes it |
//! | `KHR_materials_emissive_strength` | `StandardMaterialParams::emissive_strength` (as before) |
//! | `REGIA_materials_detail`, or `extras.regia_detail` | `HardSurfaceParams::detail` (ADR-2000, `docs/formats/gltf-extensions.md`); textures in the side table |
//! | `KHR_materials_iridescence` | `MaterialAssetRon::thin_film` (factor, IOR, thickness range, both maps) when the factor is above 0 |
//! | `extras.regia_transmission_roughness` | `MaterialAssetRon::transmission_roughness` on a transmissive material, opt-in (ADR-2732, `docs/formats/gltf-extensions.md`) |
//! | `KHR_materials_sheen` | [`GltfMaterialExtensions::sheen`]; [`GltfSheen::fabric_sheen`] feeds the renderer's `FabricSheen` |
//! | `KHR_materials_specular` | [`GltfMaterialExtensions::specular`]; the renderer has no specular colour or factor yet, so it ignores it |
//! | `KHR_texture_transform` | [`GltfTextureRef::transform`] per texture slot; the renderer has no UV transform yet, so it ignores it |
//! | `KHR_materials_variants` | [`GltfExtensionTable::variant_names`] and [`GltfExtensionTable::variants`]; the cook writes one `MaterialVariantSet` per mapped primitive (ADR-3016) |
//! | `KHR_mesh_quantization`, `EXT_meshopt_compression`, `EXT_mesh_gpu_instancing`, `EXT_texture_webp` | applied when the file opens ([`crate::gltf_normalize`], ADR-3016) |
//! | `KHR_lights_punctual` | [`GltfExtensionTable::lights`] and [`GltfExtensionTable::node_lights`]; the editor's named hierarchy spawns them; the cook does not (ADR-3016) |
//! | any other | [`GltfExtensionTable::unknown`] (raw JSON), one warning per file; a required one stops the import (`gltf_ext_unsupported`) |
//!
//! The cooked material and mesh formats do not change: the side table travels in
//! [`crate::GltfImportResult::extensions`], not in the pak.
//!
//! A host or tool that wants an extension's JSON registers a [`GltfExtensionHandler`] (or a
//! closure through [`GltfExtensionRegistry::on_extension`]) and imports with
//! [`crate::import_gltf_path_with_handlers`]. Everything here reads the document's JSON with
//! `serde_json` and never indexes without a bound check, so hostile input yields a smaller
//! table, not a panic.

use std::collections::{BTreeMap, BTreeSet};

use regia_asset::{
    AnisotropyParams, ClearcoatParams, DetailMapParams, MaterialAssetRon, TextureSlot, ThinFilmParams,
};
use serde::Serialize;
use serde_json::Value;

/// Most unknown-extension entries kept per file; the rest are counted in
/// [`GltfExtensionTable::dropped_unknown`].
pub const MAX_UNKNOWN_EXTENSIONS: usize = 4096;

/// Which kind of glTF object an extension block hangs off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionScope {
    /// The document root (`extensions` beside `asset`).
    Asset,
    /// `scenes[i]`.
    Scene,
    /// `nodes[i]`.
    Node,
    /// `meshes[i]`.
    Mesh,
    /// `meshes[i].primitives[j]` (`index` is the mesh, `sub_index` the primitive).
    Primitive,
    /// `materials[i]`.
    Material,
    /// `textures[i]`.
    Texture,
    /// `images[i]`.
    Image,
    /// `cameras[i]`.
    Camera,
    /// `skins[i]`.
    Skin,
    /// `animations[i]`.
    Animation,
}

/// The glTF object an extension block was found on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtensionTarget<'a> {
    /// Kind of object.
    pub scope: ExtensionScope,
    /// Index in the document's array for that kind (the mesh index for a primitive; 0 for the root).
    pub index: usize,
    /// Primitive index inside the mesh ([`ExtensionScope::Primitive`] only).
    pub sub_index: Option<usize>,
    /// The object's `name`, when it has one.
    pub name: Option<&'a str>,
}

/// Reads extension JSON for nodes, meshes, materials and the other scopes.
///
/// A handler that [`handles`](Self::handles) an extension is called for every block of it, known
/// extension or not; an unknown extension a handler claims is not reported as unknown.
pub trait GltfExtensionHandler: Send {
    /// Whether this handler wants the extension called `extension` (for example
    /// `"VENDOR_node_tags"`).
    fn handles(&self, extension: &str) -> bool;

    /// One extension block: `json` is the value under the extension's name.
    fn handle(&mut self, target: &ExtensionTarget<'_>, extension: &str, json: &Value);
}

struct FnHandler<F> {
    extension: String,
    callback: F,
}

impl<F: FnMut(&ExtensionTarget<'_>, &Value) + Send> GltfExtensionHandler for FnHandler<F> {
    fn handles(&self, extension: &str) -> bool {
        self.extension == extension
    }

    fn handle(&mut self, target: &ExtensionTarget<'_>, _extension: &str, json: &Value) {
        (self.callback)(target, json);
    }
}

/// The handlers an import calls. Empty by default.
#[derive(Default)]
pub struct GltfExtensionRegistry {
    handlers: Vec<Box<dyn GltfExtensionHandler>>,
}

impl GltfExtensionRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a handler. Handlers are called in registration order.
    pub fn register(&mut self, handler: impl GltfExtensionHandler + 'static) -> &mut Self {
        self.handlers.push(Box::new(handler));
        self
    }

    /// Call `callback` for every block of the extension called `extension`.
    pub fn on_extension(
        &mut self,
        extension: &str,
        callback: impl FnMut(&ExtensionTarget<'_>, &Value) + Send + 'static,
    ) -> &mut Self {
        self.register(FnHandler {
            extension: extension.to_string(),
            callback,
        })
    }

    fn claims(&self, extension: &str) -> bool {
        self.handlers.iter().any(|h| h.handles(extension))
    }

    fn dispatch(&mut self, target: &ExtensionTarget<'_>, extension: &str, json: &Value) {
        for handler in &mut self.handlers {
            if handler.handles(extension) {
                handler.handle(target, extension, json);
            }
        }
    }
}

/// A `KHR_texture_transform` block (glTF: `uv' = T · R · S · uv`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct TextureTransform {
    /// UV offset.
    pub offset: [f32; 2],
    /// Counter-clockwise rotation about the UV origin, radians.
    pub rotation: f32,
    /// UV scale.
    pub scale: [f32; 2],
    /// `texCoord` override of the transform block, when present.
    pub tex_coord: Option<u32>,
}

impl TextureTransform {
    /// No change to the UVs.
    pub const IDENTITY: Self = Self {
        offset: [0.0, 0.0],
        rotation: 0.0,
        scale: [1.0, 1.0],
        tex_coord: None,
    };

    /// Reads the extension's block: its defaults for a missing or non-finite number.
    pub(crate) fn from_block(block: &Value) -> Self {
        Self {
            offset: vec2(block, "offset", [0.0, 0.0]),
            rotation: num(block, "rotation", 0.0),
            scale: vec2(block, "scale", [1.0, 1.0]),
            tex_coord: block
                .get("texCoord")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok()),
        }
    }

    /// The transformed UV (`KHR_texture_transform`: translation · rotation · scale).
    #[must_use]
    pub fn apply(&self, uv: [f32; 2]) -> [f32; 2] {
        let (s, c) = self.rotation.sin_cos();
        let x = uv[0] * self.scale[0];
        let y = uv[1] * self.scale[1];
        [c * x + s * y + self.offset[0], -s * x + c * y + self.offset[1]]
    }
}

impl Default for TextureTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// A texture an extension (or a transform) refers to.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct GltfTextureRef {
    /// glTF image index (the cook writes it as `texture/<image>`).
    pub image: usize,
    /// UV set (`texCoord`).
    pub tex_coord: u32,
    /// `KHR_texture_transform` on this texture, when present.
    pub transform: Option<TextureTransform>,
}

/// The tiling detail layer of a material: the extension `REGIA_materials_detail` or the
/// `extras.regia_detail` object (`docs/formats/gltf-extensions.md`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GltfDetail {
    /// Detail UV scale over the base UV.
    pub uv_scale: [f32; 2],
    /// Albedo blend strength.
    pub albedo_strength: f32,
    /// Normal blend strength.
    pub normal_strength: f32,
    /// Roughness blend strength.
    pub roughness_strength: f32,
    /// Camera distance where the detail is still at full weight.
    pub fade_start: f32,
    /// Camera distance where it reaches 0; 0 disables the fade.
    pub fade_end: f32,
}

/// `KHR_materials_clearcoat`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GltfClearcoat {
    /// `clearcoatFactor`.
    pub factor: f32,
    /// `clearcoatRoughnessFactor`.
    pub roughness: f32,
    /// `clearcoatNormalTexture.scale`.
    pub normal_scale: f32,
}

/// `KHR_materials_sheen`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GltfSheen {
    /// `sheenColorFactor` (linear).
    pub color: [f32; 3],
    /// `sheenRoughnessFactor`.
    pub roughness: f32,
}

impl GltfSheen {
    /// The renderer's fabric lobe for this sheen, or `None` when the colour is black (no sheen).
    #[must_use]
    pub fn fabric_sheen(&self) -> Option<regia_render::shading_model::FabricSheen> {
        (self.color.iter().any(|c| *c > 0.0)).then_some(regia_render::shading_model::FabricSheen {
            color: self.color,
            roughness: self.roughness,
        })
    }
}

/// `KHR_materials_specular`. The renderer ignores it until it has a specular colour and factor.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GltfSpecular {
    /// `specularFactor`.
    pub factor: f32,
    /// `specularColorFactor` (linear).
    pub color: [f32; 3],
}

/// `KHR_materials_anisotropy`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GltfAnisotropy {
    /// `anisotropyStrength`.
    pub strength: f32,
    /// `anisotropyRotation`, radians.
    pub rotation: f32,
}

/// `KHR_materials_iridescence`: the thin film over the base specular.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GltfIridescence {
    /// `iridescenceFactor` (default 0).
    pub factor: f32,
    /// `iridescenceIor` (default 1.3).
    pub ior: f32,
    /// `iridescenceThicknessMinimum`, nanometres (default 100).
    pub thickness_min: f32,
    /// `iridescenceThicknessMaximum`, nanometres (default 400).
    pub thickness_max: f32,
}

/// The opt-in for rough transmission: the material's `extras.regia_transmission_roughness`
/// (`docs/formats/gltf-extensions.md`, ADR-2732).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub enum GltfTransmissionRoughness {
    /// `true`: the material's core roughness (`roughnessFactor`), which glTF uses for both lobes.
    Core,
    /// A number: this roughness, clamped to `0..=1`.
    Value(f32),
}

/// Every extension a material carries that the importer understands.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct GltfMaterialExtensions {
    /// Material index in the document.
    pub index: usize,
    /// `REGIA_materials_detail` or `extras.regia_detail`.
    pub detail: Option<GltfDetail>,
    /// `KHR_materials_clearcoat`.
    pub clearcoat: Option<GltfClearcoat>,
    /// `KHR_materials_sheen`.
    pub sheen: Option<GltfSheen>,
    /// `KHR_materials_specular`.
    pub specular: Option<GltfSpecular>,
    /// `KHR_materials_anisotropy`.
    pub anisotropy: Option<GltfAnisotropy>,
    /// `KHR_materials_emissive_strength`.
    pub emissive_strength: Option<f32>,
    /// `KHR_materials_iridescence`.
    pub iridescence: Option<GltfIridescence>,
    /// `extras.regia_transmission_roughness`.
    pub transmission_roughness: Option<GltfTransmissionRoughness>,
    /// Textures by slot: `clearcoat`, `clearcoat_roughness`, `clearcoat_normal`, `sheen_color`,
    /// `sheen_roughness`, `specular`, `specular_color`, `anisotropy`, `iridescence`,
    /// `iridescence_thickness` (always listed) and
    /// `base_color`, `metallic_roughness`, `normal`, `occlusion`, `emissive` (listed only when
    /// they carry a `KHR_texture_transform`).
    pub textures: BTreeMap<String, GltfTextureRef>,
}

/// What a `KHR_lights_punctual` light is.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GltfLightKind {
    /// Shines down its node's -Z everywhere; the intensity is in lux.
    Directional,
    /// Shines from its node in every direction; the intensity is in candela.
    Point,
    /// Shines from its node down its -Z in a cone; the intensity is in candela.
    Spot {
        /// Half-angle in radians where the falloff starts, below `outer_cone`.
        inner_cone: f32,
        /// Half-angle in radians where the light ends, up to a quarter turn.
        outer_cone: f32,
    },
}

/// One `KHR_lights_punctual` light, in the extension's units. Numbers a renderer could not use are
/// clamped (a negative intensity to 0, a cone into its legal range).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GltfPunctualLight {
    /// The light's `name`, if it has one.
    pub name: Option<String>,
    /// Its type.
    pub kind: GltfLightKind,
    /// Linear RGB, white by default.
    pub color: [f32; 3],
    /// Candela, or lux for a directional light; 1 by default.
    pub intensity: f32,
    /// Distance where a point or spot light ends. `None` means it has no end.
    pub range: Option<f32>,
}

/// A node that carries a light.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct GltfNodeLight {
    /// The node's index in the file.
    pub node: usize,
    /// Index into [`GltfExtensionTable::lights`]. That list leaves out a light it could not read, so
    /// this can differ from the index in the file.
    pub light: usize,
}

/// One `KHR_materials_variants` mapping of a primitive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GltfVariantMapping {
    /// Material index in the document.
    pub material: usize,
    /// Indices into [`GltfExtensionTable::variant_names`].
    pub variants: Vec<usize>,
}

/// The `KHR_materials_variants` block of one primitive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GltfPrimitiveVariants {
    /// Mesh index in the document.
    pub mesh: usize,
    /// Primitive index in that mesh.
    pub primitive: usize,
    /// Its mappings, in file order. Entries with an index out of range are dropped with a warning.
    pub mappings: Vec<GltfVariantMapping>,
}

/// One extension block the importer does not read.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GltfUnknownExtension {
    /// Object kind.
    pub scope: ExtensionScope,
    /// Object index (the mesh index for a primitive).
    pub index: usize,
    /// Primitive index ([`ExtensionScope::Primitive`] only).
    pub sub_index: Option<usize>,
    /// Extension name.
    pub extension: String,
    /// The block as authored.
    pub json: Value,
}

/// What the importer learned about a file's extensions. Returned in
/// [`crate::GltfImportResult::extensions`]; not part of any cooked format.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct GltfExtensionTable {
    /// `extensionsUsed`.
    pub used: Vec<String>,
    /// `extensionsRequired`.
    pub required: Vec<String>,
    /// Understood extensions per material that has any (document order).
    pub materials: Vec<GltfMaterialExtensions>,
    /// Blocks of extensions that are neither understood nor claimed by a handler.
    pub unknown: Vec<GltfUnknownExtension>,
    /// Unknown blocks beyond [`MAX_UNKNOWN_EXTENSIONS`], counted but not kept.
    pub dropped_unknown: usize,
    /// Distinct unknown extension names (sorted), the list the single warning names.
    pub unknown_names: Vec<String>,
    /// `KHR_materials_variants`: the file's variant names, in file order.
    pub variant_names: Vec<String>,
    /// `KHR_materials_variants`: the mapped primitives, in document order.
    pub variants: Vec<GltfPrimitiveVariants>,
    /// `KHR_lights_punctual`: the file's readable lights, in file order.
    pub lights: Vec<GltfPunctualLight>,
    /// `KHR_lights_punctual`: the nodes with a light, in node order.
    pub node_lights: Vec<GltfNodeLight>,
}

impl GltfExtensionTable {
    /// The understood extensions of material `index`, if it has any.
    #[must_use]
    pub fn material(&self, index: usize) -> Option<&GltfMaterialExtensions> {
        self.materials.iter().find(|m| m.index == index)
    }

    /// The `KHR_materials_variants` mappings of one primitive, if it has any.
    #[must_use]
    pub fn primitive_variants(&self, mesh: usize, primitive: usize) -> Option<&[GltfVariantMapping]> {
        self.variants
            .iter()
            .find(|v| v.mesh == mesh && v.primitive == primitive)
            .map(|v| v.mappings.as_slice())
    }

    /// Extensions the file uses that the importer cannot read, sorted: what an import falls back from.
    /// A required one never gets here (`gltf_ext_unsupported`).
    #[must_use]
    pub fn fallbacks(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .used
            .iter()
            .filter(|name| crate::gltf_normalize::extension_support(name).is_none())
            .cloned()
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Unknown blocks of one extension name.
    pub fn unknown_of<'a>(&'a self, extension: &'a str) -> impl Iterator<Item = &'a GltfUnknownExtension> {
        self.unknown.iter().filter(move |u| u.extension == extension)
    }

    /// Read the table from the JSON of a `.gltf` or the JSON chunk of a `.glb`, calling
    /// `registry`'s handlers. Unreadable JSON gives an empty table. Warns once (`tracing`) when
    /// the file has unknown extensions.
    #[must_use]
    pub fn from_bytes(bytes: &[u8], registry: &mut GltfExtensionRegistry) -> Self {
        let Some(json) = json_chunk(bytes) else {
            return Self::default();
        };
        let Ok(root) = serde_json::from_slice::<Value>(json) else {
            return Self::default();
        };
        Self::from_root(&root, registry)
    }

    fn from_root(root: &Value, registry: &mut GltfExtensionRegistry) -> Self {
        let mut table = Self {
            used: string_list(root.get("extensionsUsed")),
            required: string_list(root.get("extensionsRequired")),
            ..Self::default()
        };
        // `EXT_texture_webp` wins over the fallback `source`, as in the cook (`gltf_normalize`).
        let tex_to_image: Vec<Option<usize>> = array(root, "textures")
            .iter()
            .map(|t| {
                t.pointer("/extensions/EXT_texture_webp/source")
                    .or_else(|| t.get("source"))
                    .and_then(as_index)
            })
            .collect();
        table.variant_names = variant_names(root);
        (table.lights, table.node_lights) = read_punctual_lights(root);
        let mut names = BTreeSet::new();
        let mut walker = Walker {
            table: &mut table,
            registry,
            names: &mut names,
        };
        walker.block(ExtensionScope::Asset, 0, None, None, root.get("extensions"));
        for (scope, key) in [
            (ExtensionScope::Scene, "scenes"),
            (ExtensionScope::Node, "nodes"),
            (ExtensionScope::Texture, "textures"),
            (ExtensionScope::Image, "images"),
            (ExtensionScope::Camera, "cameras"),
            (ExtensionScope::Skin, "skins"),
            (ExtensionScope::Animation, "animations"),
        ] {
            for (i, item) in array(root, key).iter().enumerate() {
                walker.block(scope, i, None, item_name(item), item.get("extensions"));
            }
        }
        for (i, mesh) in array(root, "meshes").iter().enumerate() {
            walker.block(
                ExtensionScope::Mesh,
                i,
                None,
                item_name(mesh),
                mesh.get("extensions"),
            );
            for (j, prim) in array(mesh, "primitives").iter().enumerate() {
                walker.block(
                    ExtensionScope::Primitive,
                    i,
                    Some(j),
                    item_name(mesh),
                    prim.get("extensions"),
                );
                if let Some(block) = prim.pointer("/extensions/KHR_materials_variants")
                    && let Some(mappings) = variant_mappings(
                        block,
                        i,
                        j,
                        array(root, "materials").len(),
                        walker.table.variant_names.len(),
                    )
                {
                    walker.table.variants.push(GltfPrimitiveVariants {
                        mesh: i,
                        primitive: j,
                        mappings,
                    });
                }
            }
        }
        for (i, material) in array(root, "materials").iter().enumerate() {
            walker.block(
                ExtensionScope::Material,
                i,
                None,
                item_name(material),
                material.get("extensions"),
            );
            if let Some(ext) = parse_material(i, material, &tex_to_image) {
                walker.table.materials.push(ext);
            }
        }
        table.unknown_names = names.into_iter().collect();
        if !table.unknown_names.is_empty() || table.dropped_unknown > 0 {
            let unsupported_required: Vec<&String> = table
                .required
                .iter()
                .filter(|r| table.unknown_names.contains(r))
                .collect();
            tracing::warn!(
                extensions = %table.unknown_names.join(", "),
                blocks = table.unknown.len() + table.dropped_unknown,
                required_unsupported = ?unsupported_required,
                "glTF file uses extensions the importer does not read; kept as raw JSON in the extension table"
            );
        }
        table
    }

    /// Apply the understood extensions of material `index` to its cooked parameters:
    /// clearcoat and anisotropy become the material's own fields. `slot` turns a glTF image
    /// index into the texture slot the cook uses for it.
    pub(crate) fn apply_to_material(
        &self,
        index: usize,
        authored: &mut MaterialAssetRon,
        slot: impl Fn(usize) -> Option<TextureSlot>,
    ) {
        let Some(ext) = self.material(index) else {
            return;
        };
        if let Some(cc) = &ext.clearcoat
            && cc.factor > 0.0
        {
            authored.clearcoat = Some(ClearcoatParams {
                factor: cc.factor,
                roughness: cc.roughness,
                normal_map: ext.textures.get("clearcoat_normal").and_then(|t| slot(t.image)),
            });
        }
        if let Some(an) = &ext.anisotropy
            && an.strength != 0.0
        {
            let (s, c) = an.rotation.sin_cos();
            authored.anisotropy = Some(AnisotropyParams {
                strength: an.strength,
                direction: [c, s],
            });
        }
        if let Some(detail) = &ext.detail {
            let texture = |key: &str| ext.textures.get(key).and_then(|t| slot(t.image));
            authored.hard_surface.get_or_insert_with(Default::default).detail = Some(DetailMapParams {
                albedo: texture("detail_albedo"),
                normal: texture("detail_normal"),
                roughness: texture("detail_roughness"),
                mask: texture("detail_mask"),
                uv_scale: detail.uv_scale,
                albedo_strength: detail.albedo_strength,
                normal_strength: detail.normal_strength,
                roughness_strength: detail.roughness_strength,
                fade_start: detail.fade_start,
                fade_end: detail.fade_end,
            });
        }
        apply_iridescence(ext, authored, &slot);
        apply_transmission_roughness(ext, authored);
    }

    /// Source images of understood extension textures and the cook usage each needs, for tools
    /// and tests.
    #[must_use]
    pub fn extension_texture_usages(&self) -> Vec<(usize, regia_asset::TextureCookUsage)> {
        self.texture_usages()
    }

    /// Source images of understood extension textures and the cook usage each needs, for the
    /// texture cook (a clearcoat normal map is cooked as a normal map, not as colour).
    pub(crate) fn texture_usages(&self) -> Vec<(usize, regia_asset::TextureCookUsage)> {
        use regia_asset::TextureCookUsage::{ColorLinear, ColorSrgb, Mask, NormalMap};
        let mut out = Vec::new();
        for ext in &self.materials {
            for (key, tex) in &ext.textures {
                let usage = match key.as_str() {
                    // Linear with every channel: the thickness is in G, which a mask would drop.
                    "iridescence" | "iridescence_thickness" => ColorLinear,
                    "clearcoat_normal" => NormalMap,
                    // The detail layer reads all four channels of its packed maps, so no block
                    // format that drops one (`CompressedMaps::detail`, `detail_normal`).
                    "detail_albedo" | "detail_normal" | "detail_roughness" | "detail_mask" => ColorLinear,
                    "sheen_color" | "specular_color" | "base_color" | "emissive" => ColorSrgb,
                    "clearcoat" | "specular" => Mask,
                    "normal" | "occlusion" | "metallic_roughness" => continue,
                    _ => ColorLinear,
                };
                out.push((tex.image, usage));
            }
        }
        out
    }
}

/// `KHR_materials_iridescence` as the material's thin film. A factor of 0 is no film, so such a
/// material cooks exactly as one without the extension.
/// The opt-in reaches the cooked material on a transmissive material only, and 0 is clear glass.
/// `authored.base.roughness` is `roughnessFactor`: the metallic-roughness texture's G channel varies
/// the reflection, not this one number (ADR-2732).
fn apply_transmission_roughness(ext: &GltfMaterialExtensions, authored: &mut MaterialAssetRon) {
    let Some(asked) = ext.transmission_roughness else {
        return;
    };
    if !authored.transmission.as_ref().is_some_and(|t| t.factor > 0.0) {
        return;
    }
    let roughness = match asked {
        GltfTransmissionRoughness::Core => authored.base.roughness,
        GltfTransmissionRoughness::Value(value) => value,
    };
    if roughness.is_finite() && roughness > 0.0 {
        authored.transmission_roughness = Some(roughness.min(1.0));
    }
}

fn apply_iridescence(
    ext: &GltfMaterialExtensions,
    authored: &mut MaterialAssetRon,
    slot: &impl Fn(usize) -> Option<TextureSlot>,
) {
    let Some(film) = ext.iridescence.as_ref().filter(|film| film.factor > 0.0) else {
        return;
    };
    let map = |key: &str| ext.textures.get(key).and_then(|t| slot(t.image));
    authored.thin_film = Some(ThinFilmParams {
        factor: film.factor,
        factor_map: map("iridescence"),
        ior: film.ior,
        thickness_min: film.thickness_min,
        thickness_max: film.thickness_max,
        thickness_map: map("iridescence_thickness"),
    });
}

struct Walker<'a> {
    table: &'a mut GltfExtensionTable,
    registry: &'a mut GltfExtensionRegistry,
    names: &'a mut BTreeSet<String>,
}

impl Walker<'_> {
    fn block(
        &mut self,
        scope: ExtensionScope,
        index: usize,
        sub_index: Option<usize>,
        name: Option<&str>,
        extensions: Option<&Value>,
    ) {
        let Some(Value::Object(map)) = extensions else {
            return;
        };
        let target = ExtensionTarget {
            scope,
            index,
            sub_index,
            name,
        };
        for (extension, json) in map {
            let claimed = self.registry.claims(extension);
            if claimed {
                self.registry.dispatch(&target, extension, json);
            }
            if claimed || is_understood(extension, scope) {
                continue;
            }
            self.names.insert(extension.clone());
            if self.table.unknown.len() >= MAX_UNKNOWN_EXTENSIONS {
                self.table.dropped_unknown += 1;
            } else {
                self.table.unknown.push(GltfUnknownExtension {
                    scope,
                    index,
                    sub_index,
                    extension: extension.clone(),
                    json: json.clone(),
                });
            }
        }
    }
}

/// Extensions the importer reads (or that the `gltf` crate applies itself) where they appear.
fn is_understood(extension: &str, scope: ExtensionScope) -> bool {
    match scope {
        ExtensionScope::Asset => matches!(extension, "KHR_materials_variants" | "KHR_lights_punctual"),
        ExtensionScope::Primitive => extension == "KHR_materials_variants",
        ExtensionScope::Node => matches!(extension, "EXT_mesh_gpu_instancing" | "KHR_lights_punctual"),
        ExtensionScope::Texture => extension == "EXT_texture_webp",
        ExtensionScope::Material => matches!(
            extension,
            "KHR_materials_clearcoat"
                | "KHR_materials_iridescence"
                | "KHR_materials_sheen"
                | "KHR_materials_specular"
                | "KHR_materials_anisotropy"
                | "KHR_materials_emissive_strength"
                | "KHR_materials_ior"
                | "KHR_materials_transmission"
                | "KHR_materials_volume"
                | "REGIA_materials_detail"
        ),
        _ => false,
    }
}

/// The `KHR_lights_punctual` lights and light nodes of a `.gltf` or `.glb`, as
/// [`GltfExtensionTable::lights`] and [`GltfExtensionTable::node_lights`] hold them, for a reader that
/// wants nothing else of the table. Unreadable JSON gives none.
#[must_use]
pub fn punctual_lights(bytes: &[u8]) -> (Vec<GltfPunctualLight>, Vec<GltfNodeLight>) {
    let Some(root) = json_chunk(bytes).and_then(|json| serde_json::from_slice::<Value>(json).ok()) else {
        return (Vec::new(), Vec::new());
    };
    read_punctual_lights(&root)
}

/// One light of the root's list, or `None` for a type this extension does not define.
fn read_punctual_light(light: &Value) -> Option<GltfPunctualLight> {
    let kind = match light.get("type").and_then(Value::as_str)? {
        "directional" => GltfLightKind::Directional,
        "point" => GltfLightKind::Point,
        "spot" => {
            let spot = light.get("spot").unwrap_or(&Value::Null);
            let outer = num(spot, "outerConeAngle", std::f32::consts::FRAC_PI_4)
                .clamp(1e-4, std::f32::consts::FRAC_PI_2);
            GltfLightKind::Spot {
                inner_cone: num(spot, "innerConeAngle", 0.0).clamp(0.0, outer * 0.9999),
                outer_cone: outer,
            }
        }
        _ => return None,
    };
    let range = (kind != GltfLightKind::Directional)
        .then(|| light.get("range").and_then(Value::as_f64))
        .flatten()
        .map(|range| range as f32)
        .filter(|range| range.is_finite() && *range > 0.0);
    Some(GltfPunctualLight {
        name: item_name(light).map(str::to_string),
        kind,
        color: vec3(light, "color", [1.0; 3]).map(|c| c.max(0.0)),
        intensity: num(light, "intensity", 1.0).max(0.0),
        range,
    })
}

/// The root's `KHR_lights_punctual` list and the nodes that use it. A light of an unknown type or a node
/// naming no readable light is dropped with a warning. The `gltf` crate refuses both on the import
/// path, so this reaches a tool reading the JSON alone.
fn read_punctual_lights(root: &Value) -> (Vec<GltfPunctualLight>, Vec<GltfNodeLight>) {
    let mut lights = Vec::new();
    // By index in the file: where that light is in `lights`.
    let mut kept = Vec::new();
    if let Some(list) = root
        .pointer("/extensions/KHR_lights_punctual/lights")
        .and_then(Value::as_array)
    {
        for (i, light) in list.iter().enumerate() {
            kept.push(read_punctual_light(light).map(|light| {
                lights.push(light);
                lights.len() - 1
            }));
            if kept[i].is_none() {
                tracing::warn!(target: "regia_asset_pipeline", light = i, "KHR_lights_punctual: not a known light type; dropped");
            }
        }
    }
    let mut node_lights = Vec::new();
    for (node, item) in array(root, "nodes").iter().enumerate() {
        let Some(block) = item.pointer("/extensions/KHR_lights_punctual") else {
            continue;
        };
        let target = block
            .get("light")
            .and_then(as_index)
            .and_then(|index| kept.get(index).copied().flatten());
        match target {
            Some(light) => node_lights.push(GltfNodeLight { node, light }),
            None => {
                tracing::warn!(target: "regia_asset_pipeline", node, "KHR_lights_punctual: the node names no readable light; dropped");
            }
        }
    }
    (lights, node_lights)
}

/// `extensions.KHR_materials_variants.variants[*].name` of the root. A variant without a name keeps
/// its index in the list, under a name made from the index.
fn variant_names(root: &Value) -> Vec<String> {
    root.pointer("/extensions/KHR_materials_variants/variants")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .enumerate()
                .map(|(i, variant)| item_name(variant).map_or_else(|| format!("variant_{i}"), str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The usable mappings of one primitive's block. An index out of range is dropped with a warning.
fn variant_mappings(
    block: &Value,
    mesh: usize,
    primitive: usize,
    materials: usize,
    names: usize,
) -> Option<Vec<GltfVariantMapping>> {
    let warn = |what: &str| {
        tracing::warn!(target: "regia_asset_pipeline", mesh, primitive, "KHR_materials_variants: {what}");
    };
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for mapping in block.get("mappings").and_then(Value::as_array)? {
        let Some(material) = mapping
            .get("material")
            .and_then(as_index)
            .filter(|m| *m < materials)
        else {
            warn("a mapping has no valid material; dropped");
            continue;
        };
        let mut variants = Vec::new();
        for value in array(mapping, "variants") {
            match as_index(value).filter(|v| *v < names) {
                // The extension allows a variant in one mapping of a primitive only; the first wins.
                Some(variant) if seen.insert(variant) => variants.push(variant),
                Some(_) => warn("a variant is mapped twice; the first mapping wins"),
                None => warn("a mapping names a variant that does not exist; dropped"),
            }
        }
        if !variants.is_empty() {
            out.push(GltfVariantMapping { material, variants });
        }
    }
    Some(out)
}

pub(crate) fn json_chunk(bytes: &[u8]) -> Option<&[u8]> {
    if !bytes.starts_with(b"glTF") {
        return Some(bytes);
    }
    let len = u32::from_le_bytes(bytes.get(12..16)?.try_into().ok()?) as usize;
    if bytes.get(16..20)? != b"JSON" {
        return None;
    }
    bytes.get(20..20usize.saturating_add(len))
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn item_name(value: &Value) -> Option<&str> {
    value.get("name").and_then(Value::as_str)
}

fn as_index(value: &Value) -> Option<usize> {
    value.as_u64().and_then(|v| usize::try_from(v).ok())
}

/// A finite `f32` under `key`, else `default`.
fn num(map: &Value, key: &str, default: f32) -> f32 {
    map.get(key)
        .and_then(Value::as_f64)
        .map(|v| v as f32)
        .filter(|v| v.is_finite())
        .unwrap_or(default)
}

fn vec3(map: &Value, key: &str, default: [f32; 3]) -> [f32; 3] {
    let Some(a) = map.get(key).and_then(Value::as_array) else {
        return default;
    };
    let mut out = default;
    for (slot, v) in out.iter_mut().zip(a) {
        if let Some(f) = v.as_f64().map(|f| f as f32).filter(|f| f.is_finite()) {
            *slot = f;
        }
    }
    out
}

fn vec2(map: &Value, key: &str, default: [f32; 2]) -> [f32; 2] {
    let v = vec3(map, key, [default[0], default[1], 0.0]);
    [v[0], v[1]]
}

/// A `textureInfo` (or `normalTexture`) value: image, UV set, optional transform.
fn texture_ref(info: Option<&Value>, tex_to_image: &[Option<usize>]) -> Option<GltfTextureRef> {
    let info = info?;
    let texture = info.get("index").and_then(as_index)?;
    let image = (*tex_to_image.get(texture)?)?;
    let transform = info
        .pointer("/extensions/KHR_texture_transform")
        .map(TextureTransform::from_block);
    let tex_coord = transform
        .and_then(|t| t.tex_coord)
        .or_else(|| {
            info.get("texCoord")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
        })
        .unwrap_or(0);
    Some(GltfTextureRef {
        image,
        tex_coord,
        transform,
    })
}

/// The extension's numbers: its defaults for a missing or non-finite value, clamped to its ranges.
fn parse_iridescence(block: &Value) -> GltfIridescence {
    GltfIridescence {
        factor: num(block, "iridescenceFactor", 0.0).clamp(0.0, 1.0),
        ior: num(block, "iridescenceIor", 1.3).max(1.0),
        thickness_min: num(block, "iridescenceThicknessMinimum", 100.0).max(0.0),
        thickness_max: num(block, "iridescenceThicknessMaximum", 400.0).max(0.0),
    }
}

fn parse_material(
    index: usize,
    material: &Value,
    tex_to_image: &[Option<usize>],
) -> Option<GltfMaterialExtensions> {
    let mut out = GltfMaterialExtensions {
        index,
        ..GltfMaterialExtensions::default()
    };
    let mut add_tex = |key: &str, info: Option<&Value>, always: bool| {
        if let Some(t) = texture_ref(info, tex_to_image)
            && (always || t.transform.is_some())
        {
            out.textures.insert(key.to_string(), t);
        }
    };
    let pbr = material.get("pbrMetallicRoughness");
    add_tex("base_color", pbr.and_then(|p| p.get("baseColorTexture")), false);
    add_tex(
        "metallic_roughness",
        pbr.and_then(|p| p.get("metallicRoughnessTexture")),
        false,
    );
    add_tex("normal", material.get("normalTexture"), false);
    add_tex("occlusion", material.get("occlusionTexture"), false);
    add_tex("emissive", material.get("emissiveTexture"), false);
    let ext = material.get("extensions");
    let get = |name: &str| ext.and_then(|e| e.get(name)).filter(|v| v.is_object());

    let clearcoat = get("KHR_materials_clearcoat").map(|c| {
        add_tex("clearcoat", c.get("clearcoatTexture"), true);
        add_tex("clearcoat_roughness", c.get("clearcoatRoughnessTexture"), true);
        add_tex("clearcoat_normal", c.get("clearcoatNormalTexture"), true);
        GltfClearcoat {
            factor: num(c, "clearcoatFactor", 0.0).clamp(0.0, 1.0),
            roughness: num(c, "clearcoatRoughnessFactor", 0.0).clamp(0.0, 1.0),
            normal_scale: num(
                c.get("clearcoatNormalTexture").unwrap_or(&Value::Null),
                "scale",
                1.0,
            ),
        }
    });
    let sheen = get("KHR_materials_sheen").map(|s| {
        add_tex("sheen_color", s.get("sheenColorTexture"), true);
        add_tex("sheen_roughness", s.get("sheenRoughnessTexture"), true);
        GltfSheen {
            color: vec3(s, "sheenColorFactor", [0.0; 3]).map(|c| c.clamp(0.0, 1.0)),
            roughness: num(s, "sheenRoughnessFactor", 0.0).clamp(0.0, 1.0),
        }
    });
    let specular = get("KHR_materials_specular").map(|s| {
        add_tex("specular", s.get("specularTexture"), true);
        add_tex("specular_color", s.get("specularColorTexture"), true);
        GltfSpecular {
            factor: num(s, "specularFactor", 1.0).clamp(0.0, 1.0),
            color: vec3(s, "specularColorFactor", [1.0; 3]).map(|c| c.max(0.0)),
        }
    });
    let anisotropy = get("KHR_materials_anisotropy").map(|a| {
        add_tex("anisotropy", a.get("anisotropyTexture"), true);
        GltfAnisotropy {
            strength: num(a, "anisotropyStrength", 0.0).clamp(-1.0, 1.0),
            rotation: num(a, "anisotropyRotation", 0.0),
        }
    });
    let emissive_strength =
        get("KHR_materials_emissive_strength").map(|e| num(e, "emissiveStrength", 1.0).max(0.0));
    let detail = get("REGIA_materials_detail")
        .or_else(|| {
            material
                .get("extras")
                .and_then(|e| e.get("regia_detail"))
                .filter(|v| v.is_object())
        })
        .map(|d| {
            add_tex("detail_albedo", d.get("albedoTexture"), true);
            add_tex("detail_normal", d.get("normalTexture"), true);
            add_tex("detail_roughness", d.get("roughnessTexture"), true);
            add_tex("detail_mask", d.get("maskTexture"), true);
            let uv = |i: usize| {
                d.get("uvScale")
                    .and_then(|s| s.get(i))
                    .and_then(Value::as_f64)
                    .map(|v| v as f32)
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .unwrap_or_else(|| DetailMapParams::default().uv_scale[i])
            };
            GltfDetail {
                uv_scale: [uv(0), uv(1)],
                albedo_strength: num(d, "albedoStrength", 1.0).clamp(0.0, 1.0),
                normal_strength: num(d, "normalStrength", 1.0).clamp(0.0, 2.0),
                roughness_strength: num(d, "roughnessStrength", 1.0).clamp(0.0, 1.0),
                fade_start: num(d, "fadeStart", 0.0).max(0.0),
                fade_end: num(d, "fadeEnd", 0.0).max(0.0),
            }
        });
    out.detail = detail;
    out.transmission_roughness = material
        .get("extras")
        .and_then(|e| e.get("regia_transmission_roughness"))
        .and_then(|v| match v {
            Value::Bool(true) => Some(GltfTransmissionRoughness::Core),
            Value::Number(n) => n
                .as_f64()
                .map(|n| GltfTransmissionRoughness::Value((n as f32).clamp(0.0, 1.0))),
            _ => None,
        });
    let iridescence = get("KHR_materials_iridescence").map(|i| {
        add_tex("iridescence", i.get("iridescenceTexture"), true);
        add_tex(
            "iridescence_thickness",
            i.get("iridescenceThicknessTexture"),
            true,
        );
        parse_iridescence(i)
    });
    out.clearcoat = clearcoat;
    out.sheen = sheen;
    out.specular = specular;
    out.anisotropy = anisotropy;
    out.emissive_strength = emissive_strength;
    out.iridescence = iridescence;
    let any = out.detail.is_some()
        || out.transmission_roughness.is_some()
        || out.iridescence.is_some()
        || out.clearcoat.is_some()
        || out.sheen.is_some()
        || out.specular.is_some()
        || out.anisotropy.is_some()
        || out.emissive_strength.is_some()
        || !out.textures.is_empty();
    any.then_some(out)
}
