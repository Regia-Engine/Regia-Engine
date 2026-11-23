// SPDX-License-Identifier: MPL-2.0
//! Deleting a layer from a definition asset (spec 37 § 4.4, ADR-7203): refused with
//! `layer_in_use` while a level that binds the definition has members in it.
//!
//! The caller passes every level it knows (the project's level sources and the open level's
//! snapshot, which may hold unsaved members); only levels whose [`LevelLayers`] binding names the
//! definition count. A layer that is another layer's parent is refused too
//! (`layer_parent_invalid`): deleting it would leave the child naming a missing parent.

use regia_core::id::Guid;

use crate::format::SceneFile;
use crate::layers::{LayerDef, LayerDefs, LayerMembership, LevelLayers};

/// One entity that still belongs to the layer being deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerMember {
    /// The level it lives in, as the caller named it (a path relative to `assets/`).
    pub level: String,
    /// The entity's guid.
    pub entity: Guid,
}

/// Why [`delete_layer`] refused. [`LayerDeleteError::reason`] is the agent protocol's
/// `data.reason`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LayerDeleteError {
    /// The definition has no layer of that name.
    #[error("layer_unknown: {0}")]
    Unknown(String),
    /// Levels that bind the definition still have members in the layer.
    #[error("layer_in_use: `{layer}` still has {count} member(s): {listed}", count = .members.len(), listed = list_members(.members))]
    InUse {
        /// The layer.
        layer: String,
        /// Its members, in level order then guid order.
        members: Vec<LayerMember>,
    },
    /// Other layers name it as their parent.
    #[error("layer_parent_invalid: `{layer}` is the parent of {children:?}; reparent or delete them first")]
    ParentOf {
        /// The layer.
        layer: String,
        /// The layers whose parent it is.
        children: Vec<String>,
    },
}

impl LayerDeleteError {
    /// The agent `data.reason` string.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Unknown(_) => "layer_unknown",
            Self::InUse { .. } => "layer_in_use",
            Self::ParentOf { .. } => "layer_parent_invalid",
        }
    }
}

/// At most this many members are named in the message; the error carries all of them.
const LISTED_MEMBERS: usize = 8;

fn list_members(members: &[LayerMember]) -> String {
    let mut text: Vec<String> = members
        .iter()
        .take(LISTED_MEMBERS)
        .map(|m| format!("{} in {}", m.entity, m.level))
        .collect();
    if members.len() > LISTED_MEMBERS {
        text.push(format!("and {} more", members.len() - LISTED_MEMBERS));
    }
    text.join(", ")
}

/// The members of `layer` in the levels among `levels` that bind the definition `defs_id`.
#[must_use]
pub fn layer_members(defs_id: Guid, layer: &str, levels: &[(&str, &SceneFile)]) -> Vec<LayerMember> {
    let mut members = Vec::new();
    for (name, scene) in levels {
        let binds = LevelLayers::from_scene(scene)
            .and_then(Result::ok)
            .is_some_and(|binding| binding.def_ids().contains(&defs_id));
        if !binds {
            continue;
        }
        let mut found: Vec<Guid> = scene
            .entities
            .iter()
            .filter(|record| {
                LayerMembership::from_record(record).is_some_and(|m| m.layers.iter().any(|l| l == layer))
            })
            .map(|record| record.guid)
            .collect();
        found.sort();
        members.extend(found.into_iter().map(|entity| LayerMember {
            level: (*name).to_string(),
            entity,
        }));
    }
    members
}

/// Removes `layer` from `defs` (the definition asset `defs_id`) and returns it, unless a level
/// among `levels` that binds the definition still has members in it, or another layer names it
/// as parent. A refused delete leaves `defs` unchanged.
///
/// # Errors
/// [`LayerDeleteError`]: `layer_unknown`, `layer_in_use`, `layer_parent_invalid`.
pub fn delete_layer(
    defs: &mut LayerDefs,
    defs_id: Guid,
    layer: &str,
    levels: &[(&str, &SceneFile)],
) -> Result<LayerDef, LayerDeleteError> {
    let Some(index) = defs.layers.iter().position(|def| def.name == layer) else {
        return Err(LayerDeleteError::Unknown(layer.to_string()));
    };
    let children: Vec<String> = defs
        .layers
        .iter()
        .filter(|def| def.parent.as_deref() == Some(layer))
        .map(|def| def.name.clone())
        .collect();
    if !children.is_empty() {
        return Err(LayerDeleteError::ParentOf {
            layer: layer.to_string(),
            children,
        });
    }
    let members = layer_members(defs_id, layer, levels);
    if !members.is_empty() {
        return Err(LayerDeleteError::InUse {
            layer: layer.to_string(),
            members,
        });
    }
    Ok(defs.layers.remove(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::SceneFile;
    use crate::layers::{LayerDef, LayerDefs};
    use regia_core::id::Guid;

    const DEFS: u128 = 0xdef5_0000_0000_4000_8000_0000_0000_0001;

    fn defs() -> LayerDefs {
        let def = |name: &str, parent: Option<&str>| LayerDef {
            name: name.into(),
            parent: parent.map(Into::into),
            ..LayerDef::default()
        };
        LayerDefs {
            format_version: crate::layers::LAYERS_FORMAT_VERSION,
            layers: vec![
                def("night", None),
                def("day", None),
                def("streets", None),
                def("lamps", Some("streets")),
            ],
        }
    }

    fn level(binds: Option<Guid>, members: &[(u128, &str)]) -> SceneFile {
        let resources = binds.map_or(String::new(), |id| {
            format!(r#""regia_scene::layers::LevelLayers": (defs: ["asset:{id}"], entries: [])"#)
        });
        let entities: Vec<String> = members
            .iter()
            .map(|(guid, layer)| {
                format!(
                    r#"(guid: "{}", components: {{ "regia_scene::layers::LayerMembership": (layers: ["{layer}"]) }})"#,
                    Guid::from_u128(*guid)
                )
            })
            .collect();
        SceneFile::from_ron_str(&format!(
            "SceneFile(format_version: {}, entities: [{}], resources: {{ {resources} }})",
            crate::format::SCENE_FORMAT_VERSION,
            entities.join(", ")
        ))
        .expect("scene")
    }

    #[test]
    fn deleting_a_layer_with_members_in_a_binding_level_is_refused() {
        let id = Guid::from_u128(DEFS);
        let town = level(Some(id), &[(0xa1, "night"), (0xa2, "night")]);
        let other = level(None, &[(0xb1, "day")]);
        let levels = [("levels/town.level.d", &town), ("levels/other.level.ron", &other)];

        let mut edited = defs();
        let err = delete_layer(&mut edited, id, "night", &levels).expect_err("in use");
        assert_eq!(err.reason(), "layer_in_use");
        let text = err.to_string();
        assert!(text.starts_with("layer_in_use: "), "{text}");
        assert!(text.contains("levels/town.level.d"), "{text}");
        assert!(text.contains(&Guid::from_u128(0xa1).to_string()), "{text}");
        match &err {
            LayerDeleteError::InUse { members, .. } => assert_eq!(members.len(), 2),
            other => panic!("{other:?}"),
        }
        assert_eq!(edited, defs(), "a refused delete changes nothing");

        // `day` has members only in a level that does not bind this definition.
        let removed = delete_layer(&mut edited, id, "day", &levels).expect("free to delete");
        assert_eq!(removed.name, "day");
        assert!(edited.get("day").is_none());

        // A parent of another layer and an unknown name are refused too.
        let err = delete_layer(&mut edited, id, "streets", &levels).expect_err("parent");
        assert_eq!(err.reason(), "layer_parent_invalid");
        let err = delete_layer(&mut edited, id, "nope", &levels).expect_err("unknown");
        assert_eq!(err.reason(), "layer_unknown");
    }
}
