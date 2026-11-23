// SPDX-License-Identifier: MPL-2.0
//! Material instances (`.matinst.ron`, v2): a parent graph plus parameter, texture and static
//! switch overrides (`specs/13` § 7 "a material may derive from a parent material and override
//! parameters only"). Parameter and texture overrides never change the generated code, so every
//! instance of a graph with the same switch values shares its pipelines; a switch override
//! selects another permutation (compiled once, shared by every instance that picks it).
//!
//! v2 added `switches` (optional); the `1 → 2` step only moves the version.

use std::collections::BTreeMap;

use regia_core::version::{self, VersionStep};
use regia_render::surface_graph::GRAPH_PARAM_SLOTS;
use serde::{Deserialize, Serialize};

use crate::format::{FormatError, MaterialGraph, ParamValue, ValueType};

/// Current `.matinst.ron` version.
pub const MATINST_FORMAT_VERSION: u32 = 2;

/// An instance does not fit its parent graph.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum InstanceError {
    /// The parent declares no such parameter.
    #[error("parameter `{0}` is not declared by the parent graph")]
    UnknownParam(String),
    /// The override's type differs from the declaration.
    #[error("parameter `{name}` is {expected:?} in the parent graph, the override is {found:?}")]
    TypeMismatch {
        /// Parameter name.
        name: String,
        /// Declared type.
        expected: ValueType,
        /// Override type.
        found: ValueType,
    },
    /// A component is NaN or infinite.
    #[error("parameter `{0}` override is not finite")]
    NotFinite(String),
    /// The parent declares no such texture.
    #[error("texture `{0}` is not declared by the parent graph")]
    UnknownTexture(String),
    /// The parent declares no such static switch.
    #[error("switch `{0}` is not declared by the parent graph")]
    UnknownSwitch(String),
}

/// A material instance: overrides on top of a parent graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialInstance {
    /// Format version ([`MATINST_FORMAT_VERSION`]).
    pub format_version: u32,
    /// Parent graph (a `.matgraph.ron` path, or a library name).
    pub parent: String,
    /// Parameter overrides by name; the type must match the parent's declaration.
    #[serde(default)]
    pub params: BTreeMap<String, ParamValue>,
    /// Texture overrides: texture name → image path.
    #[serde(default)]
    pub textures: BTreeMap<String, String>,
    /// Static switch overrides (v2): switch name → value. Selects the compiled permutation.
    #[serde(default)]
    pub switches: BTreeMap<String, bool>,
}

impl MaterialInstance {
    /// An instance of `parent` with no overrides.
    #[must_use]
    pub fn new(parent: impl Into<String>) -> Self {
        Self {
            format_version: MATINST_FORMAT_VERSION,
            parent: parent.into(),
            params: BTreeMap::new(),
            textures: BTreeMap::new(),
            switches: BTreeMap::new(),
        }
    }

    /// Sets one parameter override (checked against the parent in [`Self::validate`]).
    pub fn set(&mut self, name: impl Into<String>, value: ParamValue) -> &mut Self {
        self.params.insert(name.into(), value);
        self
    }

    /// Checks every override against `graph`.
    ///
    /// # Errors
    ///
    /// The first [`InstanceError`] found.
    pub fn validate(&self, graph: &MaterialGraph) -> Result<(), InstanceError> {
        for (name, value) in &self.params {
            let declared = crate::compile::declared(graph, name)
                .ok_or_else(|| InstanceError::UnknownParam(name.clone()))?;
            if declared.value_type() != value.value_type() {
                return Err(InstanceError::TypeMismatch {
                    name: name.clone(),
                    expected: declared.value_type(),
                    found: value.value_type(),
                });
            }
            if !value.is_finite() {
                return Err(InstanceError::NotFinite(name.clone()));
            }
        }
        for name in self.textures.keys() {
            if !graph.textures.iter().any(|t| &t.name == name) {
                return Err(InstanceError::UnknownTexture(name.clone()));
            }
        }
        for name in self.switches.keys() {
            if !graph.switches.iter().any(|s| &s.name == name) {
                return Err(InstanceError::UnknownSwitch(name.clone()));
            }
        }
        Ok(())
    }

    /// The uniform parameter block: the parent's defaults, then the overrides; slot 0 holds
    /// `time` (seconds).
    ///
    /// # Errors
    ///
    /// As [`Self::validate`].
    pub fn param_block(
        &self,
        graph: &MaterialGraph,
        time: f32,
    ) -> Result<[[f32; 4]; GRAPH_PARAM_SLOTS], InstanceError> {
        self.validate(graph)?;
        let mut block = crate::compile::default_params(graph, time);
        for (i, p) in graph.params.iter().take(crate::compile::MAX_PARAMS).enumerate() {
            if let Some(value) = self.params.get(&p.name) {
                block[i + 1] = value.to_vec4();
            }
        }
        Ok(block)
    }

    /// Parses a `.matinst.ron` document (newer versions rejected, older ones migrated).
    ///
    /// # Errors
    ///
    /// [`FormatError`].
    pub fn from_ron(text: &str) -> Result<Self, FormatError> {
        let found = version::peek_ron_version(text).map_err(FormatError::Ron)?;
        if found > MATINST_FORMAT_VERSION {
            return Err(FormatError::NewerVersion {
                found,
                expected: MATINST_FORMAT_VERSION,
            });
        }
        let mut instance: Self = ron::from_str(text).map_err(|e| FormatError::Ron(e.to_string()))?;
        if instance.format_version < MATINST_FORMAT_VERSION {
            instance.format_version = version::migrate(
                &mut instance,
                found,
                builtin_instance_migrations(),
                MATINST_FORMAT_VERSION,
            )
            .map_err(|e| FormatError::Migration(e.to_string()))?;
        }
        Ok(instance)
    }

    /// Serialises to pretty RON.
    ///
    /// # Errors
    ///
    /// [`FormatError::Ron`].
    pub fn to_ron(&self) -> Result<String, FormatError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| FormatError::Ron(e.to_string()))
    }
}

/// v1 → v2: `switches` is optional (no overrides), so the step only moves the version.
fn v1_to_v2(_instance: &mut MaterialInstance) -> Result<(), String> {
    Ok(())
}

/// The built-in migrator chain for `.matinst.ron`.
#[must_use]
pub fn builtin_instance_migrations() -> &'static [VersionStep<MaterialInstance>] {
    &[VersionStep {
        from: 1,
        to: 2,
        migrate: v1_to_v2,
    }]
}

/// The `from` versions of [`builtin_instance_migrations`].
#[must_use]
pub fn matinst_migration_sources() -> Vec<u32> {
    builtin_instance_migrations().iter().map(|s| s.from).collect()
}

/// The serde fingerprint of `.matinst.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn matinst_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<MaterialInstance>(),
        serde_variant_names::<ParamValue>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{ColorSpace, ParamDecl, TextureDecl};

    fn parent() -> MaterialGraph {
        let mut graph = MaterialGraph::new("parent");
        graph
            .params
            .push(ParamDecl::new("tint", ParamValue::Color([0.5, 0.5, 0.5])));
        graph
            .params
            .push(ParamDecl::new("roughness", ParamValue::Float(0.7)));
        graph.textures.push(TextureDecl {
            name: "albedo".into(),
            path: "a.png".into(),
            color_space: ColorSpace::Srgb,
        });
        graph
    }

    #[test]
    fn overrides_replace_defaults_in_slot_order() {
        let mut instance = MaterialInstance::new("parent.matgraph.ron");
        instance.set("roughness", ParamValue::Float(0.2));
        let block = instance.param_block(&parent(), 1.5).expect("block");
        assert_eq!(block[0][0], 1.5);
        assert_eq!(block[1], [0.5, 0.5, 0.5, 0.0]);
        assert_eq!(block[2], [0.2, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_vec3_may_override_a_colour_but_types_must_match() {
        let graph = parent();
        let mut ok = MaterialInstance::new("p");
        ok.set("tint", ParamValue::Vec3([1.0, 0.0, 0.0]));
        assert!(ok.validate(&graph).is_ok());
        let mut bad = MaterialInstance::new("p");
        bad.set("roughness", ParamValue::Vec2([1.0, 0.0]));
        assert!(matches!(
            bad.validate(&graph),
            Err(InstanceError::TypeMismatch { .. })
        ));
        let mut unknown = MaterialInstance::new("p");
        unknown.set("gloss", ParamValue::Float(1.0));
        assert_eq!(
            unknown.validate(&graph),
            Err(InstanceError::UnknownParam("gloss".into()))
        );
        let mut tex = MaterialInstance::new("p");
        tex.textures.insert("normal".into(), "n.png".into());
        assert_eq!(
            tex.validate(&graph),
            Err(InstanceError::UnknownTexture("normal".into()))
        );
        let mut nan = MaterialInstance::new("p");
        nan.set("roughness", ParamValue::Float(f32::NAN));
        assert_eq!(
            nan.validate(&graph),
            Err(InstanceError::NotFinite("roughness".into()))
        );
    }

    #[test]
    fn instances_round_trip_and_reject_newer_versions() {
        let mut instance = MaterialInstance::new("rock.matgraph.ron");
        instance.set("tint", ParamValue::Color([0.08, 0.052, 0.034]));
        instance.textures.insert("albedo".into(), "rock_b.png".into());
        let text = instance.to_ron().expect("write");
        assert_eq!(MaterialInstance::from_ron(&text).expect("read"), instance);
        assert!(matches!(
            MaterialInstance::from_ron("(format_version: 3, parent: \"x\")"),
            Err(FormatError::NewerVersion { found: 3, .. })
        ));
        assert!(matinst_schema_fingerprint().contains("switches"));
        assert_eq!(matinst_migration_sources(), vec![1]);
        let v1 = MaterialInstance::from_ron("(format_version: 1, parent: \"x\")").expect("v1 reads");
        assert_eq!(v1.format_version, 2);
        assert!(v1.switches.is_empty());
    }

    #[test]
    fn switch_overrides_must_name_a_declared_switch() {
        let mut graph = parent();
        graph.switches.push(crate::format::SwitchDecl {
            name: "wear".into(),
            default: true,
        });
        let mut instance = MaterialInstance::new("p");
        instance.switches.insert("wear".into(), false);
        assert!(instance.validate(&graph).is_ok());
        instance.switches.insert("rust".into(), true);
        assert_eq!(
            instance.validate(&graph),
            Err(InstanceError::UnknownSwitch("rust".into()))
        );
    }
}
