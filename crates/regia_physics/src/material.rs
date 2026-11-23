// SPDX-License-Identifier: MPL-2.0
//! `PhysicsMaterial` and `.physmat.ron` (v1, ADR-2424): friction, restitution, combine rules and
//! density as a shared content asset that colliders reference by name.
//!
//! A material is a named bundle of the surface fields a [`Collider`] otherwise carries inline.
//! [`PhysicsMaterials`] is the registry (filled from `.physmat.ron` text); a [`PhysicsMaterialRef`]
//! next to a [`Collider`] names one, and [`apply_physics_materials`] copies its values into the
//! collider (in `PreUpdate`, before the fixed step, so the sync sees them). A collider without a
//! reference is untouched, and [`PhysicsMaterial::default`] holds exactly the numbers
//! `Collider::default()` has, so scenes that do not use materials are bit-identical.
//!
//! [`combine`] is the pure form of rapier's coefficient combine rule, so authors (and tests) can
//! predict the friction or bounce of a contact: when the two colliders name different rules the
//! higher-priority one applies (Average < Min < Multiply < Max).
//!
//! Provenance: the combine-rule priority is rapier3d's documented behavior (Apache-2.0);
//! everything else is original. See `docs/formats/physmat.md`.

use std::collections::BTreeMap;

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::components::{Collider, CombineRule};

/// Current `.physmat.ron` format version.
pub const PHYSMAT_FORMAT_VERSION: u32 = 1;

/// Why a `.physmat.ron` did not load.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PhysicsMaterialError {
    /// The text is not valid RON for the format.
    #[error("physics material does not parse: {0}")]
    Parse(String),
    /// The file's `format_version` is not the current one.
    #[error("physics material format version {found}; this build reads {expected}")]
    Version {
        /// The version in the file.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },
    /// The name is empty.
    #[error("physics material has an empty name")]
    EmptyName,
    /// A coefficient is out of range or not finite.
    #[error("physics material field `{0}` is out of range or not finite")]
    Value(&'static str),
}

impl PhysicsMaterialError {
    /// A stable identifier: lowercase ASCII, digits and `_` (agent protocol, logs).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "physmat_parse",
            Self::Version { .. } => "physmat_version",
            Self::EmptyName => "physmat_name",
            Self::Value(_) => "physmat_value",
        }
    }
}

/// A `.physmat.ron`: a named bundle of surface properties for colliders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PhysicsMaterial {
    /// Must be [`PHYSMAT_FORMAT_VERSION`].
    pub format_version: u32,
    /// Registry name colliders reference (not empty).
    pub name: String,
    /// Friction coefficient, `0..=2` (0 = ice).
    pub friction: f32,
    /// Restitution (bounciness), `0..=1`.
    pub restitution: f32,
    /// Density in kg/m³ for automatic mass, `0..=100` (0 = massless collider).
    pub density: f32,
    /// How friction combines with the other collider.
    pub friction_combine: CombineRule,
    /// How restitution combines with the other collider.
    pub restitution_combine: CombineRule,
}

impl Default for PhysicsMaterial {
    /// The numbers `Collider::default()` carries (friction 0.5, no bounce, density 1).
    fn default() -> Self {
        let c = Collider::default();
        Self {
            format_version: PHYSMAT_FORMAT_VERSION,
            name: "default".to_owned(),
            friction: c.friction,
            restitution: c.restitution,
            density: c.density,
            friction_combine: c.friction_combine,
            restitution_combine: c.restitution_combine,
        }
    }
}

impl PhysicsMaterial {
    /// A material of the current version with the default coefficients.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Parses and validates a `.physmat.ron` text.
    ///
    /// # Errors
    /// [`PhysicsMaterialError`]: a parse failure, another version, an empty name or a bad value.
    pub fn from_ron(text: &str) -> Result<Self, PhysicsMaterialError> {
        let found = regia_core::version::peek_ron_version(text).map_err(PhysicsMaterialError::Parse)?;
        if found != PHYSMAT_FORMAT_VERSION {
            return Err(PhysicsMaterialError::Version {
                found,
                expected: PHYSMAT_FORMAT_VERSION,
            });
        }
        let material: Self = ron::from_str(text).map_err(|e| PhysicsMaterialError::Parse(e.to_string()))?;
        material.validate()?;
        Ok(material)
    }

    /// The material as pretty RON text.
    ///
    /// # Errors
    /// [`PhysicsMaterialError::Parse`] when RON cannot write the value (never for a valid one).
    pub fn to_ron(&self) -> Result<String, PhysicsMaterialError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| PhysicsMaterialError::Parse(e.to_string()))
    }

    /// Checks the name and the ranges.
    ///
    /// # Errors
    /// [`PhysicsMaterialError::EmptyName`] or [`PhysicsMaterialError::Value`].
    pub fn validate(&self) -> Result<(), PhysicsMaterialError> {
        if self.name.trim().is_empty() {
            return Err(PhysicsMaterialError::EmptyName);
        }
        let in_range = |v: f32, max: f32| v.is_finite() && (0.0..=max).contains(&v);
        for (ok, field) in [
            (in_range(self.friction, 2.0), "friction"),
            (in_range(self.restitution, 1.0), "restitution"),
            (in_range(self.density, 100.0), "density"),
        ] {
            if !ok {
                return Err(PhysicsMaterialError::Value(field));
            }
        }
        Ok(())
    }

    /// Copies the surface fields into `collider`; shape, layer, events and the rest stay.
    pub fn apply_to(&self, collider: &mut Collider) {
        collider.friction = self.friction;
        collider.restitution = self.restitution;
        collider.density = self.density;
        collider.friction_combine = self.friction_combine;
        collider.restitution_combine = self.restitution_combine;
    }
}

/// Versions a migrator accepts below the current one (none at v1).
#[must_use]
pub fn physmat_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Serde field-list fingerprint of `.physmat.ron` (for `cargo xtask check-formats`).
#[must_use]
pub fn physmat_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<PhysicsMaterial>()])
}

/// The coefficient of a contact between two colliders: rapier's combine rule. Equal rules apply
/// as named; different rules resolve to the higher priority one (Average < Min < Multiply < Max).
#[must_use]
pub fn combine(a_rule: CombineRule, b_rule: CombineRule, a: f32, b: f32) -> f32 {
    let priority = |r: CombineRule| match r {
        CombineRule::Average => 0,
        CombineRule::Min => 1,
        CombineRule::Multiply => 2,
        CombineRule::Max => 3,
    };
    let rule = if priority(a_rule) >= priority(b_rule) {
        a_rule
    } else {
        b_rule
    };
    match rule {
        CombineRule::Average => (a + b) * 0.5,
        CombineRule::Min => a.min(b),
        CombineRule::Multiply => a * b,
        CombineRule::Max => a.max(b),
    }
}

/// The materials colliders can name, by [`PhysicsMaterial::name`].
#[derive(Resource, Debug, Default, Clone, PartialEq)]
pub struct PhysicsMaterials {
    by_name: BTreeMap<String, PhysicsMaterial>,
    /// References that named no material on the last application (a counter for tests and the
    /// agent; the warning is logged once per change).
    pub unresolved: usize,
}

impl PhysicsMaterials {
    /// Adds (or replaces) a material.
    pub fn insert(&mut self, material: PhysicsMaterial) {
        self.by_name.insert(material.name.clone(), material);
    }

    /// Parses `.physmat.ron` text and adds the material; returns its name.
    ///
    /// # Errors
    /// [`PhysicsMaterialError`] from [`PhysicsMaterial::from_ron`].
    pub fn insert_ron(&mut self, text: &str) -> Result<String, PhysicsMaterialError> {
        let material = PhysicsMaterial::from_ron(text)?;
        let name = material.name.clone();
        self.insert(material);
        Ok(name)
    }

    /// The material named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&PhysicsMaterial> {
        self.by_name.get(name)
    }

    /// Material names in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// Number of materials.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// True when no material is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Next to a [`Collider`]: the [`PhysicsMaterials`] entry whose surface values the collider takes.
#[derive(Component, Reflect, Debug, Clone, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct PhysicsMaterialRef(pub String);

/// Copies referenced materials into their colliders (`PreUpdate`). Runs for references that
/// changed, and for every reference when the registry changed. Colliders without a
/// [`PhysicsMaterialRef`] are never touched.
pub fn apply_physics_materials(
    mut materials: ResMut<PhysicsMaterials>,
    mut colliders: Query<(&mut Collider, Ref<PhysicsMaterialRef>)>,
) {
    let all = materials.is_changed();
    let mut unresolved = 0;
    for (mut collider, reference) in &mut colliders {
        let Some(material) = materials.get(&reference.0) else {
            unresolved += 1;
            if reference.is_changed() {
                tracing::warn!(target: "regia_physics", "collider names unknown physics material `{}`", reference.0);
            }
            continue;
        };
        if all || reference.is_changed() {
            material.apply_to(&mut collider);
        }
    }
    if materials.unresolved != unresolved {
        // A counter, not an edit: it must not make the next run re-apply every reference.
        materials.bypass_change_detection().unresolved = unresolved;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_material_equals_the_default_collider() {
        let m = PhysicsMaterial::default();
        let mut c = Collider::default();
        let before = c.clone();
        m.apply_to(&mut c);
        assert_eq!(c, before);
    }

    #[test]
    fn combine_rules_follow_rapier() {
        use CombineRule::*;
        assert_eq!(combine(Average, Average, 0.2, 0.6), 0.4);
        assert_eq!(combine(Min, Min, 0.2, 0.6), 0.2);
        assert!((combine(Multiply, Multiply, 0.5, 0.5) - 0.25).abs() < 1e-7);
        assert_eq!(combine(Max, Max, 0.2, 0.6), 0.6);
        // Different rules: the higher priority wins, from either side.
        assert_eq!(combine(Average, Min, 0.2, 0.6), 0.2);
        assert_eq!(combine(Min, Average, 0.2, 0.6), 0.2);
        assert!((combine(Min, Multiply, 0.5, 0.5) - 0.25).abs() < 1e-7);
        assert_eq!(combine(Multiply, Max, 0.2, 0.6), 0.6);
        assert_eq!(combine(Max, Average, 0.2, 0.6), 0.6);
    }

    #[test]
    fn round_trips_and_defaults_missing_fields() {
        let mut m = PhysicsMaterial::new("ice");
        m.friction = 0.02;
        m.friction_combine = CombineRule::Min;
        let text = m.to_ron().expect("ron");
        assert_eq!(PhysicsMaterial::from_ron(&text), Ok(m));
        let sparse = PhysicsMaterial::from_ron(r#"(format_version: 1, name: "rubber", restitution: 0.8)"#)
            .expect("parse");
        assert_eq!(sparse.restitution, 0.8);
        assert_eq!(sparse.friction, 0.5);
    }

    #[test]
    fn problems_have_stable_codes() {
        let code = |t: &str| PhysicsMaterial::from_ron(t).expect_err("rejected").code();
        assert_eq!(code("(("), "physmat_parse");
        assert_eq!(code(r#"(format_version: 2, name: "x")"#), "physmat_version");
        assert_eq!(code(r#"(format_version: 1, name: " ")"#), "physmat_name");
        assert_eq!(
            code(r#"(format_version: 1, name: "x", friction: -1.0)"#),
            "physmat_value"
        );
        assert_eq!(
            code(r#"(format_version: 1, name: "x", restitution: 1.5)"#),
            "physmat_value"
        );
        assert_eq!(
            code(r#"(format_version: 1, name: "x", density: inf)"#),
            "physmat_value"
        );
    }

    #[test]
    fn the_fingerprint_names_the_fields() {
        let f = physmat_schema_fingerprint();
        for field in [
            "format_version",
            "friction",
            "restitution",
            "density",
            "friction_combine",
        ] {
            assert!(f.contains(field), "{field} missing from {f}");
        }
    }
}
