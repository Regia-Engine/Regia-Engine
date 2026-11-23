// SPDX-License-Identifier: MPL-2.0
//! Graph parameters: float, int, bool, trigger (auto-reset) and vec2/vec3 values.
//!
//! Specification: `docs/specs/17-animation.md` § 3. Set from Rust with
//! `animator.parameters.set_float("Speed", 3.2)`; triggers are consumed by the transitions
//! that fire on them and auto-reset.

use regia_core::math::{Vec2, Vec3};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A parameter value.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum ParamValue {
    /// Floating-point parameter.
    Float(f32),
    /// Integer parameter.
    Int(i32),
    /// Boolean parameter.
    Bool(bool),
    /// Two-component parameter.
    Vec2(Vec2),
    /// Three-component parameter.
    Vec3(Vec3),
    /// Trigger parameter: set, then consumed (auto-reset) by a transition.
    Trigger,
}

impl From<f32> for ParamValue {
    fn from(v: f32) -> Self {
        ParamValue::Float(v)
    }
}

impl From<f64> for ParamValue {
    fn from(v: f64) -> Self {
        ParamValue::Float(v as f32)
    }
}

impl From<i32> for ParamValue {
    fn from(v: i32) -> Self {
        ParamValue::Int(v)
    }
}

impl From<bool> for ParamValue {
    fn from(v: bool) -> Self {
        ParamValue::Bool(v)
    }
}

impl From<Vec2> for ParamValue {
    fn from(v: Vec2) -> Self {
        ParamValue::Vec2(v)
    }
}

impl From<Vec3> for ParamValue {
    fn from(v: Vec3) -> Self {
        ParamValue::Vec3(v)
    }
}

/// A parameter declaration for a graph: `(name, default)`. [`ParameterSet::from_decls`] stores
/// that default in the map for its variant. A later `set_*` writes only its own map and leaves
/// the others, so one name can be present as more than one kind. In RON:
/// `parameters: [("Speed", Float(0.0))]`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ParamDecl(
    /// Parameter name.
    pub String,
    /// Default value. Its variant is the map [`ParameterSet::from_decls`] writes.
    pub ParamValue,
);

/// The parameter set of one [`crate::components::Animator`].
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ParameterSet {
    floats: HashMap<String, f32>,
    ints: HashMap<String, i32>,
    bools: HashMap<String, bool>,
    vec2s: HashMap<String, Vec2>,
    vec3s: HashMap<String, Vec3>,
    triggers: HashMap<String, bool>,
}

impl ParameterSet {
    /// An empty parameter set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Initialise from declarations (applies defaults).
    pub fn from_decls(decls: &[ParamDecl]) -> Self {
        let mut set = Self::default();
        for decl in decls {
            match decl.1 {
                ParamValue::Float(v) => {
                    set.floats.insert(decl.0.clone(), v);
                }
                ParamValue::Int(v) => {
                    set.ints.insert(decl.0.clone(), v);
                }
                ParamValue::Bool(v) => {
                    set.bools.insert(decl.0.clone(), v);
                }
                ParamValue::Vec2(v) => {
                    set.vec2s.insert(decl.0.clone(), v);
                }
                ParamValue::Vec3(v) => {
                    set.vec3s.insert(decl.0.clone(), v);
                }
                ParamValue::Trigger => {
                    set.triggers.insert(decl.0.clone(), false);
                }
            }
        }
        set
    }

    /// Every non-trigger parameter with its current value, sorted by name.
    pub fn values(&self) -> Vec<(String, ParamValue)> {
        let mut out: Vec<(String, ParamValue)> = self
            .floats
            .iter()
            .map(|(k, v)| (k.clone(), ParamValue::Float(*v)))
            .chain(self.ints.iter().map(|(k, v)| (k.clone(), ParamValue::Int(*v))))
            .chain(self.bools.iter().map(|(k, v)| (k.clone(), ParamValue::Bool(*v))))
            .chain(self.vec2s.iter().map(|(k, v)| (k.clone(), ParamValue::Vec2(*v))))
            .chain(self.vec3s.iter().map(|(k, v)| (k.clone(), ParamValue::Vec3(*v))))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Every trigger parameter and whether it is set (waiting to be consumed), sorted by name.
    pub fn triggers(&self) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = self.triggers.iter().map(|(k, v)| (k.clone(), *v)).collect();
        out.sort();
        out
    }

    /// The first stored kind of `name`: float, then int, then bool, then vec2, then vec3.
    /// A trigger is returned as `Bool(set)` only when the name is in none of those maps.
    pub fn get(&self, name: &str) -> Option<ParamValue> {
        if let Some(v) = self.floats.get(name) {
            return Some(ParamValue::Float(*v));
        }
        if let Some(v) = self.ints.get(name) {
            return Some(ParamValue::Int(*v));
        }
        if let Some(v) = self.bools.get(name) {
            return Some(ParamValue::Bool(*v));
        }
        if let Some(v) = self.vec2s.get(name) {
            return Some(ParamValue::Vec2(*v));
        }
        if let Some(v) = self.vec3s.get(name) {
            return Some(ParamValue::Vec3(*v));
        }
        self.triggers.get(name).map(|v| ParamValue::Bool(*v))
    }

    /// Set a float parameter. Unknown names are stored anyway (graphs may declare params they
    /// read before the animator sees a declaration list). A name that is already stored is
    /// overwritten in place. The name is not removed from the other maps.
    pub fn set_float(&mut self, name: &str, value: f32) {
        if let Some(slot) = self.floats.get_mut(name) {
            *slot = value;
            return;
        }
        crate::hot_count::count_set_float_alloc();
        self.floats.insert(name.to_string(), value);
    }

    /// Set an int parameter. A name that is already stored is overwritten in place.
    pub fn set_int(&mut self, name: &str, value: i32) {
        if let Some(slot) = self.ints.get_mut(name) {
            *slot = value;
            return;
        }
        self.ints.insert(name.to_string(), value);
    }

    /// Set a bool parameter. A name that is already stored is overwritten in place.
    pub fn set_bool(&mut self, name: &str, value: bool) {
        if let Some(slot) = self.bools.get_mut(name) {
            *slot = value;
            return;
        }
        self.bools.insert(name.to_string(), value);
    }

    /// Set a vec2 parameter. A name that is already stored is overwritten in place.
    pub fn set_vec2(&mut self, name: &str, value: Vec2) {
        if let Some(slot) = self.vec2s.get_mut(name) {
            *slot = value;
            return;
        }
        self.vec2s.insert(name.to_string(), value);
    }

    /// Set a vec3 parameter. A name that is already stored is overwritten in place.
    pub fn set_vec3(&mut self, name: &str, value: Vec3) {
        if let Some(slot) = self.vec3s.get_mut(name) {
            *slot = value;
            return;
        }
        self.vec3s.insert(name.to_string(), value);
    }

    /// Fire a trigger parameter (sets it; the next transition that consumes it resets it).
    /// A name that is already stored is overwritten in place.
    pub fn set_trigger(&mut self, name: &str) {
        if let Some(slot) = self.triggers.get_mut(name) {
            *slot = true;
        } else {
            self.triggers.insert(name.to_string(), true);
        }
    }

    /// Read a float parameter (0.0 when absent).
    pub fn float(&self, name: &str) -> f32 {
        self.floats.get(name).copied().unwrap_or(0.0)
    }

    /// Read an int parameter (0 when absent).
    pub fn int(&self, name: &str) -> i32 {
        self.ints.get(name).copied().unwrap_or(0)
    }

    /// Read a bool parameter (`false` when absent).
    pub fn bool(&self, name: &str) -> bool {
        self.bools.get(name).copied().unwrap_or(false)
    }

    /// Read a vec2 parameter (zero when absent).
    pub fn vec2(&self, name: &str) -> Vec2 {
        self.vec2s.get(name).copied().unwrap_or(Vec2::ZERO)
    }

    /// Read a vec3 parameter (zero when absent).
    pub fn vec3(&self, name: &str) -> Vec3 {
        self.vec3s.get(name).copied().unwrap_or(Vec3::ZERO)
    }

    /// True when the trigger is set. Does not consume it (see [`ParameterSet::consume_trigger`]).
    pub fn trigger(&self, name: &str) -> bool {
        self.triggers.get(name).copied().unwrap_or(false)
    }

    /// Consume a trigger: returns whether it was set, and resets it.
    pub fn consume_trigger(&mut self, name: &str) -> bool {
        match self.triggers.get_mut(name) {
            Some(flag) if *flag => {
                *flag = false;
                true
            }
            _ => false,
        }
    }

    /// Every stored value, sorted by name (triggers report `Bool(pending)`), for debugging
    /// panels and the agent protocol.
    pub fn entries(&self) -> Vec<(String, ParamValue)> {
        let mut out: Vec<(String, ParamValue)> = Vec::new();
        out.extend(
            self.floats
                .iter()
                .map(|(k, v)| (k.clone(), ParamValue::Float(*v))),
        );
        out.extend(self.ints.iter().map(|(k, v)| (k.clone(), ParamValue::Int(*v))));
        out.extend(self.bools.iter().map(|(k, v)| (k.clone(), ParamValue::Bool(*v))));
        out.extend(self.vec2s.iter().map(|(k, v)| (k.clone(), ParamValue::Vec2(*v))));
        out.extend(self.vec3s.iter().map(|(k, v)| (k.clone(), ParamValue::Vec3(*v))));
        out.extend(
            self.triggers
                .iter()
                .map(|(k, v)| (k.clone(), ParamValue::Bool(*v))),
        );
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_read_all_kinds() {
        let mut p = ParameterSet::new();
        p.set_float("Speed", 3.2);
        p.set_int("Combo", 2);
        p.set_bool("Crouch", true);
        p.set_vec2("Move", Vec2::new(1.0, -1.0));
        p.set_vec3("AimTarget", Vec3::new(0.0, 1.5, -3.0));
        assert_eq!(p.float("Speed"), 3.2);
        assert_eq!(p.int("Combo"), 2);
        assert!(p.bool("Crouch"));
        assert_eq!(p.vec2("Move"), Vec2::new(1.0, -1.0));
        assert_eq!(p.vec3("AimTarget"), Vec3::new(0.0, 1.5, -3.0));
        // Absent values have defaults.
        assert_eq!(p.float("Nope"), 0.0);
        assert!(!p.bool("Nope"));
    }

    #[test]
    fn triggers_auto_reset_on_consume() {
        let mut p = ParameterSet::new();
        p.set_trigger("Jump");
        assert!(p.trigger("Jump"));
        assert!(p.consume_trigger("Jump"), "first consume sees the trigger");
        assert!(!p.consume_trigger("Jump"), "second consume is reset");
        assert!(!p.trigger("Jump"));
        assert!(!p.consume_trigger("NeverSet"));
    }

    #[test]
    fn from_decls_applies_defaults() {
        let decls = vec![
            ParamDecl("Speed".into(), ParamValue::Float(1.5)),
            ParamDecl("Jump".into(), ParamValue::Trigger),
        ];
        let p = ParameterSet::from_decls(&decls);
        assert_eq!(p.float("Speed"), 1.5);
        assert!(!p.trigger("Jump"));
    }

    /// One name is a separate entry in each map. `get` returns the earliest kind.
    #[test]
    fn the_same_name_lives_in_every_map_it_was_set_on() {
        let mut p = ParameterSet::from_decls(&[ParamDecl("Speed".into(), ParamValue::Float(0.0))]);
        p.set_int("Speed", 4);
        assert_eq!(p.float("Speed"), 0.0);
        assert_eq!(p.int("Speed"), 4);
        assert_eq!(p.get("Speed"), Some(ParamValue::Float(0.0)));
        let values = p.values();
        assert!(values.contains(&("Speed".into(), ParamValue::Float(0.0))));
        assert!(values.contains(&("Speed".into(), ParamValue::Int(4))));

        p.set_bool("Jump", false);
        p.set_trigger("Jump");
        assert!(p.trigger("Jump"));
        assert!(!p.bool("Jump"));
        assert_eq!(p.get("Jump"), Some(ParamValue::Bool(false)));
    }
}
