// SPDX-License-Identifier: MPL-2.0
//! Material texture, UV, color-space and extension coverage (ADR-8392).
//!
//! Reuses the importer's extension table and the glTF slot color-space rule.
//! Unknown optional extensions stay in the document. A required extension the
//! importer cannot read fails compatibility and is not stripped. The only
//! metadata write here is `extensionsUsed` declaration completion.
//!
//! Numeric domains follow the public Khronos schemas: `thicknessFactor` is at
//! least 0, `attenuationDistance` is greater than 0 when present, and `ior` is
//! at least 1. Out-of-range values are reported and left unchanged.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::{
    ExtensionEvidence, MAX_EXTENSION_BYTES, MAX_EXTENSION_DEPTH, MaterialFinding, MaterialRepairEffect,
    TextureRoleEvidence,
};

const VOLUME: &str = "KHR_materials_volume";
const TRANSMISSION: &str = "KHR_materials_transmission";
const IOR: &str = "KHR_materials_ior";
const TRANSFORM: &str = "KHR_texture_transform";

/// Core and extension texture slots. The color space comes from the slot key,
/// except `REGIA_materials_detail`, which the importer cooks as linear data.
const SLOTS: &[(&str, &str)] = &[
    ("pbrMetallicRoughness/baseColorTexture", "base_color"),
    (
        "pbrMetallicRoughness/metallicRoughnessTexture",
        "metallic_roughness",
    ),
    ("normalTexture", "normal"),
    ("occlusionTexture", "occlusion"),
    ("emissiveTexture", "emissive"),
    ("extensions/KHR_materials_clearcoat/clearcoatTexture", "clearcoat"),
    (
        "extensions/KHR_materials_clearcoat/clearcoatRoughnessTexture",
        "clearcoat_roughness",
    ),
    (
        "extensions/KHR_materials_clearcoat/clearcoatNormalTexture",
        "clearcoat_normal",
    ),
    (
        "extensions/KHR_materials_transmission/transmissionTexture",
        "transmission",
    ),
    ("extensions/KHR_materials_volume/thicknessTexture", "thickness"),
    ("extensions/KHR_materials_sheen/sheenColorTexture", "sheen_color"),
    (
        "extensions/KHR_materials_sheen/sheenRoughnessTexture",
        "sheen_roughness",
    ),
    ("extensions/KHR_materials_specular/specularTexture", "specular"),
    (
        "extensions/KHR_materials_specular/specularColorTexture",
        "specular_color",
    ),
    (
        "extensions/KHR_materials_iridescence/iridescenceTexture",
        "iridescence",
    ),
    (
        "extensions/KHR_materials_iridescence/iridescenceThicknessTexture",
        "iridescence_thickness",
    ),
    (
        "extensions/KHR_materials_anisotropy/anisotropyTexture",
        "anisotropy",
    ),
    ("extensions/REGIA_materials_detail/albedoTexture", "detail_albedo"),
    ("extensions/REGIA_materials_detail/normalTexture", "detail_normal"),
    (
        "extensions/REGIA_materials_detail/roughnessTexture",
        "detail_roughness",
    ),
    ("extensions/REGIA_materials_detail/maskTexture", "detail_mask"),
    ("extras/regia_detail/albedoTexture", "detail_albedo"),
    ("extras/regia_detail/normalTexture", "detail_normal"),
    ("extras/regia_detail/roughnessTexture", "detail_roughness"),
    ("extras/regia_detail/maskTexture", "detail_mask"),
];

pub(super) struct Coverage {
    pub compatible: bool,
    pub extensions: Vec<ExtensionEvidence>,
    pub texture_roles: Vec<TextureRoleEvidence>,
    pub extension_nodes: u64,
    pub extension_bytes: u64,
}

struct Walk {
    nodes: u64,
    bytes: u64,
    truncated: bool,
    pointer: String,
}

pub(super) fn augment(document: &mut Value, findings: &mut Vec<MaterialFinding>) -> Coverage {
    let mut roles = Vec::new();
    let mut nested = BTreeSet::new();
    let mut walk = Walk {
        nodes: 0,
        bytes: 0,
        truncated: false,
        pointer: "/materials".into(),
    };
    if let Some(materials) = document.get("materials").and_then(Value::as_array) {
        for (index, material) in materials.iter().enumerate() {
            domains(material, index, findings);
            for (path, role) in SLOTS {
                if let Some(info) = material.pointer(&format!("/{path}")) {
                    inspect_texture(document, index, path, role, info, findings, &mut roles);
                    // Slots under `materials[].extensions` are walked with that object.
                    if !path.starts_with("extensions/")
                        && let Some(extensions) = info.get("extensions")
                    {
                        walk_extension_map(
                            extensions,
                            &format!("/materials/{index}/{path}/extensions"),
                            1,
                            &mut nested,
                            &mut walk,
                        );
                    }
                }
            }
            if let Some(extensions) = material.get("extensions") {
                walk_extension_map(
                    extensions,
                    &format!("/materials/{index}/extensions"),
                    1,
                    &mut nested,
                    &mut walk,
                );
            }
        }
    }
    color_conflicts(&roles, document, findings);
    if walk.truncated {
        findings.push(MaterialFinding {
            code: "extension_nesting".into(),
            material: None,
            pointer: walk.pointer,
            message: format!(
                "extension JSON exceeded depth {MAX_EXTENSION_DEPTH} or {MAX_EXTENSION_BYTES} bytes; the unread tail was left unchanged"
            ),
            effect: MaterialRepairEffect::PreservesAppearance,
        });
    }
    // Material-level keys were already declared in alphabetical order. Nested
    // textureInfo names append after that pass. Required names still missing
    // append in `extensionsRequired` order. Nothing is removed.
    declare(document, nested.into_iter(), findings);
    declare(document, required_names(document).into_iter(), findings);
    texture_sources(document, findings);
    let required = required_names(document);
    let extensions = evidence(document, &required);
    let compatible = required
        .iter()
        .all(|name| crate::gltf_normalize::extension_support(name).is_some());
    Coverage {
        compatible,
        extensions,
        texture_roles: roles,
        extension_nodes: walk.nodes,
        extension_bytes: walk.bytes,
    }
}

fn domains(material: &Value, index: usize, findings: &mut Vec<MaterialFinding>) {
    if let Some(thickness) = number_at(material, &format!("/extensions/{VOLUME}/thicknessFactor"))
        && thickness < 0.0
    {
        super::finding(
            findings,
            index,
            "volume_thickness",
            &format!("extensions/{VOLUME}/thicknessFactor"),
            "thicknessFactor must be nonnegative; the authored value was left unchanged",
            MaterialRepairEffect::PreservesAppearance,
        );
    }
    if let Some(distance) = number_at(material, &format!("/extensions/{VOLUME}/attenuationDistance"))
        && distance <= 0.0
    {
        super::finding(
            findings,
            index,
            "attenuation_distance",
            &format!("extensions/{VOLUME}/attenuationDistance"),
            "attenuationDistance must be greater than 0; the authored value was left unchanged",
            MaterialRepairEffect::PreservesAppearance,
        );
    }
    if let Some(ior) = number_at(material, &format!("/extensions/{IOR}/ior"))
        && ior < 1.0
    {
        super::finding(
            findings,
            index,
            "ior_domain",
            &format!("extensions/{IOR}/ior"),
            "ior must be at least 1; the authored value was left unchanged",
            MaterialRepairEffect::PreservesAppearance,
        );
    }
    let volume = material.pointer(&format!("/extensions/{VOLUME}")).is_some();
    let transmission = material.pointer(&format!("/extensions/{TRANSMISSION}")).is_some();
    if volume && !transmission {
        super::finding(
            findings,
            index,
            "extension_dependency",
            &format!("extensions/{VOLUME}"),
            "KHR_materials_volume requires KHR_materials_transmission; transmission was not added",
            MaterialRepairEffect::PreservesAppearance,
        );
    }
}

fn inspect_texture(
    document: &Value,
    index: usize,
    path: &str,
    role: &str,
    info: &Value,
    findings: &mut Vec<MaterialFinding>,
    roles: &mut Vec<TextureRoleEvidence>,
) {
    let slot_key = path.rsplit('/').next().unwrap_or(path);
    let color_space = if path.contains("REGIA_materials_detail") || path.contains("regia_detail") {
        "linear"
    } else {
        crate::texture_settings::gltf_slot_color_space(slot_key)
    };
    let texture = info.get("index").and_then(Value::as_u64);
    let (tex_coord, tex_coord_path, invalid) = texcoord_of(info, path);
    if let Some(texture_index) = texture
        && let Some(textures) = document.get("textures").and_then(Value::as_array)
        && usize::try_from(texture_index)
            .ok()
            .is_none_or(|slot| slot >= textures.len())
    {
        super::finding(
            findings,
            index,
            "texture_index",
            &format!("{path}/index"),
            &format!(
                "texture index {texture_index} is outside textures[{}]",
                textures.len()
            ),
            MaterialRepairEffect::PreservesAppearance,
        );
    }
    if invalid {
        super::finding(
            findings,
            index,
            "uv_channel",
            &tex_coord_path,
            "texCoord must be a nonnegative integer; the authored value was left unchanged",
            MaterialRepairEffect::PreservesAppearance,
        );
    } else if let Some(channel) = tex_coord {
        localize_uv(document, index, channel, &tex_coord_path, findings);
    }
    roles.push(TextureRoleEvidence {
        material: index,
        pointer: format!("/materials/{index}/{path}"),
        role: role.into(),
        color_space: color_space.into(),
        tex_coord,
        texture,
    });
}

fn texcoord_of(info: &Value, path: &str) -> (Option<u64>, String, bool) {
    if let Some(value) = info.pointer(&format!("/extensions/{TRANSFORM}/texCoord")) {
        return classify_texcoord(value, format!("{path}/extensions/{TRANSFORM}/texCoord"));
    }
    if let Some(value) = info.get("texCoord") {
        return classify_texcoord(value, format!("{path}/texCoord"));
    }
    (Some(0), format!("{path}/texCoord"), false)
}

fn classify_texcoord(value: &Value, path: String) -> (Option<u64>, String, bool) {
    match value.as_u64() {
        Some(channel) => (Some(channel), path, false),
        None => (None, path, true),
    }
}

fn localize_uv(
    document: &Value,
    material: usize,
    channel: u64,
    path: &str,
    findings: &mut Vec<MaterialFinding>,
) {
    let Some(meshes) = document.get("meshes").and_then(Value::as_array) else {
        return;
    };
    let mut problems: Vec<String> = Vec::new();
    for (mesh_i, mesh) in meshes.iter().enumerate() {
        let Some(primitives) = mesh.get("primitives").and_then(Value::as_array) else {
            continue;
        };
        for (prim_i, primitive) in primitives.iter().enumerate() {
            if primitive.get("material").and_then(Value::as_u64)
                != Some(u64::try_from(material).unwrap_or(u64::MAX))
            {
                continue;
            }
            if let Some(problem) = uv_problem(document, mesh_i, prim_i, primitive, channel) {
                problems.push(problem);
            }
        }
    }
    let Some(first) = problems.first() else {
        return;
    };
    super::finding(
        findings,
        material,
        "uv_channel",
        path,
        &format!(
            "{first} ({} primitive(s) missing TEXCOORD_{channel})",
            problems.len()
        ),
        MaterialRepairEffect::PreservesAppearance,
    );
}

fn uv_problem(document: &Value, mesh: usize, primitive: usize, prim: &Value, channel: u64) -> Option<String> {
    let key = format!("TEXCOORD_{channel}");
    let Some(raw) = prim.pointer(&format!("/attributes/{key}")) else {
        return Some(format!("mesh {mesh} primitive {primitive} has no {key}"));
    };
    let Some(accessor_index) = raw.as_u64() else {
        return Some(format!(
            "mesh {mesh} primitive {primitive} {key} is not an accessor index"
        ));
    };
    let Some(accessors) = document.get("accessors").and_then(Value::as_array) else {
        return Some(format!(
            "mesh {mesh} primitive {primitive} {key} has no accessors array"
        ));
    };
    let Some(accessor) = usize::try_from(accessor_index)
        .ok()
        .and_then(|index| accessors.get(index))
    else {
        return Some(format!(
            "mesh {mesh} primitive {primitive} {key} accessor {accessor_index} is outside accessors"
        ));
    };
    if accessor.get("type").and_then(Value::as_str) != Some("VEC2") {
        let kind = accessor.get("type").and_then(Value::as_str).unwrap_or("missing");
        return Some(format!(
            "mesh {mesh} primitive {primitive} {key} accessor {accessor_index} type is {kind}, expected VEC2"
        ));
    }
    None
}

fn texture_sources(document: &Value, findings: &mut Vec<MaterialFinding>) {
    let Some(textures) = document.get("textures").and_then(Value::as_array) else {
        return;
    };
    let Some(images) = document.get("images").and_then(Value::as_array) else {
        return;
    };
    for (index, texture) in textures.iter().enumerate() {
        let Some(source) = texture.get("source").and_then(Value::as_u64) else {
            continue;
        };
        if usize::try_from(source).ok().is_none_or(|i| i >= images.len()) {
            findings.push(MaterialFinding {
                code: "texture_index".into(),
                material: None,
                pointer: format!("/textures/{index}/source"),
                message: format!("image index {source} is outside images[{}]", images.len()),
                effect: MaterialRepairEffect::PreservesAppearance,
            });
        }
    }
}

fn color_conflicts(roles: &[TextureRoleEvidence], document: &Value, findings: &mut Vec<MaterialFinding>) {
    let Some(textures) = document.get("textures").and_then(Value::as_array) else {
        return;
    };
    let mut seen: BTreeSet<(usize, u64)> = BTreeSet::new();
    for role in roles {
        let Some(texture) = role.texture else {
            continue;
        };
        let Some(texture_index) = usize::try_from(texture).ok() else {
            continue;
        };
        if texture_index >= textures.len() || !seen.insert((role.material, texture)) {
            continue;
        }
        let users: Vec<&TextureRoleEvidence> = roles
            .iter()
            .filter(|other| other.material == role.material && other.texture == Some(texture))
            .collect();
        let mut spaces = BTreeSet::new();
        for user in &users {
            spaces.insert(user.color_space.as_str());
        }
        if spaces.len() < 2 {
            continue;
        }
        let first = users[0];
        let second = users[1];
        let prefix = format!("/materials/{}/", role.material);
        let relative = second
            .pointer
            .strip_prefix(&prefix)
            .unwrap_or(second.pointer.as_str());
        super::finding(
            findings,
            role.material,
            "texture_color_space",
            relative,
            &format!(
                "texture {texture} is {} ({}) and {} ({}); the image file name is not the color space",
                first.color_space, first.role, second.color_space, second.role
            ),
            MaterialRepairEffect::PreservesAppearance,
        );
    }
}

fn number_at(material: &Value, pointer: &str) -> Option<f64> {
    material.pointer(pointer).and_then(Value::as_f64)
}

fn required_names(document: &Value) -> Vec<String> {
    document
        .get("extensionsRequired")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn used_names(document: &Value) -> Vec<String> {
    document
        .get("extensionsUsed")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn declare(document: &mut Value, names: impl Iterator<Item = String>, findings: &mut Vec<MaterialFinding>) {
    let mut used = used_names(document);
    let mut added = false;
    for name in names {
        if used.iter().any(|have| have == &name) {
            continue;
        }
        findings.push(MaterialFinding {
            code: "extensions_used".into(),
            material: None,
            pointer: "/extensionsUsed".into(),
            message: format!("declare {name}"),
            effect: MaterialRepairEffect::PreservesAppearance,
        });
        used.push(name);
        added = true;
    }
    if added {
        document["extensionsUsed"] = json!(used);
    }
}

fn evidence(document: &Value, required: &[String]) -> Vec<ExtensionEvidence> {
    let mut names: BTreeSet<String> = used_names(document).into_iter().collect();
    names.extend(required.iter().cloned());
    names
        .into_iter()
        .map(|name| ExtensionEvidence {
            required: required.iter().any(|item| item == &name),
            status: support_name(&name).into(),
            name,
        })
        .collect()
}

fn support_name(name: &str) -> &'static str {
    match crate::gltf_normalize::extension_support(name) {
        Some(crate::gltf_normalize::ExtensionSupport::Cooked) => "cooked",
        Some(crate::gltf_normalize::ExtensionSupport::SideTable) => "side_table",
        None => "unsupported",
    }
}

fn walk_extension_map(
    value: &Value,
    pointer: &str,
    depth: usize,
    names: &mut BTreeSet<String>,
    walk: &mut Walk,
) {
    let Some(map) = value.as_object() else {
        return;
    };
    for (name, body) in map {
        if walk.truncated {
            return;
        }
        names.insert(name.clone());
        walk_value(body, &format!("{pointer}/{name}"), depth, names, walk);
    }
}

fn walk_value(value: &Value, pointer: &str, depth: usize, names: &mut BTreeSet<String>, walk: &mut Walk) {
    if walk.truncated {
        return;
    }
    if depth > MAX_EXTENSION_DEPTH {
        walk.truncated = true;
        walk.pointer = pointer.into();
        return;
    }
    walk.nodes += 1;
    match value {
        Value::Object(map) => {
            if let Some(extensions) = map.get("extensions") {
                walk_extension_map(
                    extensions,
                    &format!("{pointer}/extensions"),
                    depth + 1,
                    names,
                    walk,
                );
            }
            for (key, child) in map {
                if walk.truncated {
                    return;
                }
                if key == "extensions" {
                    continue;
                }
                if !walk.charge(key.len() as u64, pointer) {
                    return;
                }
                walk_value(child, &format!("{pointer}/{key}"), depth + 1, names, walk);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                if walk.truncated {
                    return;
                }
                walk_value(child, &format!("{pointer}/{index}"), depth + 1, names, walk);
            }
        }
        Value::String(text) => {
            walk.charge(text.len() as u64, pointer);
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

impl Walk {
    fn charge(&mut self, bytes: u64, pointer: &str) -> bool {
        if self.truncated {
            return false;
        }
        if self.bytes.saturating_add(bytes) > MAX_EXTENSION_BYTES {
            self.bytes = MAX_EXTENSION_BYTES;
            self.truncated = true;
            self.pointer = pointer.into();
            return false;
        }
        self.bytes += bytes;
        true
    }
}
