// SPDX-License-Identifier: MPL-2.0
//! Layout variants (P-271, addendum § 13.6, ADR-7858): override sets of a `.ui.ron` document chosen by
//! the window's orientation and size.
//!
//! A document lists `variants`, each with a `when` (orientation, width range and aspect range, every
//! part optional) and `overrides` by node: `{ "<node>": { "<field>": <value> } }`. The node key is a
//! node name (the first node with that name, depth first) or a child-index path from the root
//! (`"/0/2"`, `"/"` the root). The field key is a dotted path into the node's written form
//! (`style.inset`, `opacity`, `text`), and the value is that field in the same shape the designer's
//! `editor.ui_designer_set` takes; `null` resets the field to its default. `children` and `name`
//! cannot be overridden: a variant changes fields, never the tree, so focus and scroll state carry
//! over by node key.
//!
//! At run time the *first* variant whose `when` matches the window (logical px: physical size over the
//! OS scale factor) applies on top of the base; no match shows the base. [`spawn_ui`] spawns a
//! template straight into its variant, and [`apply_layout_variants`] respawns a root once when a window
//! change picks another variant (focus and scroll kept by node key, as a live reload does).
//!
//! [`spawn_ui`]: crate::authoring::spawn_ui

use std::collections::BTreeMap;

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::authoring::{UiLibrary, UiNodeDef, UiTemplate};
use crate::{UiRoot, UiScales};

/// Field keys a variant may not override (the tree's shape and the keys state is kept by).
pub const LOCKED_FIELDS: [&str; 2] = ["children", "name"];

/// Window orientation: portrait is taller than wide; a square window is landscape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    /// Height greater than width.
    Portrait,
    /// Width at least the height.
    Landscape,
}

impl Orientation {
    /// The orientation of a window of `size` (width, height).
    #[must_use]
    pub fn of(size: [f32; 2]) -> Self {
        if size[1] > size[0] {
            Self::Portrait
        } else {
            Self::Landscape
        }
    }
}

/// When a variant applies: every given condition must hold (an empty `when` always matches). Widths
/// are logical window px; the aspect is width over height; every bound is inclusive.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VariantWhen {
    /// `portrait` or `landscape`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orientation: Option<Orientation>,
    /// Smallest window width.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_width: Option<f32>,
    /// Largest window width.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_width: Option<f32>,
    /// Smallest width / height.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_aspect: Option<f32>,
    /// Largest width / height.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_aspect: Option<f32>,
}

impl VariantWhen {
    /// Whether a window of `size` (logical px, width and height) meets every condition.
    #[must_use]
    pub fn matches(&self, size: [f32; 2]) -> bool {
        let [w, h] = size;
        let aspect = if h > 0.0 { w / h } else { f32::INFINITY };
        self.orientation.is_none_or(|o| o == Orientation::of(size))
            && self.min_width.is_none_or(|m| w >= m)
            && self.max_width.is_none_or(|m| w <= m)
            && self.min_aspect.is_none_or(|m| aspect >= m)
            && self.max_aspect.is_none_or(|m| aspect <= m)
    }

    /// A short US English description (`portrait, width <= 1200`), `any window` when empty.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(o) = self.orientation {
            parts.push(match o {
                Orientation::Portrait => "portrait".to_owned(),
                Orientation::Landscape => "landscape".to_owned(),
            });
        }
        if let Some(m) = self.min_width {
            parts.push(format!("width >= {m}"));
        }
        if let Some(m) = self.max_width {
            parts.push(format!("width <= {m}"));
        }
        if let Some(m) = self.min_aspect {
            parts.push(format!("aspect >= {m}"));
        }
        if let Some(m) = self.max_aspect {
            parts.push(format!("aspect <= {m}"));
        }
        if parts.is_empty() {
            "any window".to_owned()
        } else {
            parts.join(", ")
        }
    }

    /// Why this `when` can never match (a bound is not a finite non-negative number, or a minimum
    /// exceeds its maximum); `None` when it can.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        for (what, v) in [
            ("min_width", self.min_width),
            ("max_width", self.max_width),
            ("min_aspect", self.min_aspect),
            ("max_aspect", self.max_aspect),
        ] {
            if let Some(v) = v
                && (!v.is_finite() || v < 0.0)
            {
                return Some(format!("{what} {v} is not a finite number >= 0"));
            }
        }
        if let (Some(a), Some(b)) = (self.min_width, self.max_width)
            && a > b
        {
            return Some(format!("min_width {a} is above max_width {b}"));
        }
        if let (Some(a), Some(b)) = (self.min_aspect, self.max_aspect)
            && a > b
        {
            return Some(format!("min_aspect {a} is above max_aspect {b}"));
        }
        None
    }
}

/// Field overrides of one node: dotted field key to value.
pub type NodeOverrides = BTreeMap<String, Value>;

/// One layout variant of a `.ui.ron` document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutVariant {
    /// Optional display name (the designer's switcher, `ui.inspect`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// When it applies.
    pub when: VariantWhen,
    /// Field overrides by node key (a node name, or a path such as `"/0/2"`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, NodeOverrides>,
}

impl LayoutVariant {
    /// The name, or `variant <index + 1>` when it has none.
    #[must_use]
    pub fn label(&self, index: usize) -> String {
        self.name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| format!("variant {}", index + 1))
    }
}

/// The index of the first variant whose `when` matches a window of `size` (logical px).
#[must_use]
pub fn select_variant(variants: &[LayoutVariant], size: [f32; 2]) -> Option<usize> {
    variants.iter().position(|v| v.when.matches(size))
}

/// The logical window size the variants read (physical px over the OS scale factor).
#[must_use]
pub fn logical_window_size(scales: &UiScales) -> [f32; 2] {
    let f = if scales.scale_factor.is_finite() && scales.scale_factor > 0.0 {
        scales.scale_factor as f32
    } else {
        1.0
    };
    [scales.window_size[0] / f, scales.window_size[1] / f]
}

/// The path (child indices from the root) of the node a variant key names: `"/"` the root, `"/0/2"` a
/// path, otherwise a node name (a leading `#` is accepted), the first match depth first.
#[must_use]
pub fn find_node(root: &UiNodeDef, key: &str) -> Option<Vec<usize>> {
    let key = key.trim();
    if let Some(rest) = key.strip_prefix('/') {
        let mut path = Vec::new();
        let mut node = root;
        for part in rest.split('/').filter(|p| !p.is_empty()) {
            let i: usize = part.parse().ok()?;
            node = node.children.get(i)?;
            path.push(i);
        }
        return Some(path);
    }
    let name = key.strip_prefix('#').unwrap_or(key);
    if name.is_empty() {
        return None;
    }
    fn walk(node: &UiNodeDef, name: &str, path: &mut Vec<usize>) -> bool {
        if node.name.as_deref() == Some(name) {
            return true;
        }
        for (i, child) in node.children.iter().enumerate() {
            path.push(i);
            if walk(child, name, path) {
                return true;
            }
            path.pop();
        }
        false
    }
    let mut path = Vec::new();
    walk(root, name, &mut path).then_some(path)
}

/// The key a variant uses for the node at `path`: its name when the name finds this node, else the
/// path (`"/0/2"`).
#[must_use]
pub fn node_key(root: &UiNodeDef, path: &[usize]) -> String {
    let mut node = root;
    for &i in path {
        match node.children.get(i) {
            Some(child) => node = child,
            None => break,
        }
    }
    if let Some(name) = node
        .name
        .as_deref()
        .filter(|n| !n.trim().is_empty() && !n.starts_with('/'))
        && find_node(root, name).as_deref() == Some(path)
    {
        return name.to_owned();
    }
    let mut out = String::from("/");
    out.push_str(&path.iter().map(usize::to_string).collect::<Vec<_>>().join("/"));
    out
}

/// `node` with the field at the dotted `key` set to `value` in its written form (`null` resets the
/// field to its default). Errors name the key; the node is unchanged then.
pub fn apply_override(node: &UiNodeDef, key: &str, value: &Value) -> Result<UiNodeDef, String> {
    let key = key.trim();
    let parts: Vec<&str> = key.split('.').collect();
    if key.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return Err(format!("{key:?} is not a dotted field path"));
    }
    if LOCKED_FIELDS.contains(&parts[0]) {
        return Err(format!("{key}: a variant cannot override `{}`", parts[0]));
    }
    let mut json = serde_json::to_value(node).map_err(|e| format!("{key}: {e}"))?;
    let mut slot = &mut json;
    for (i, part) in parts.iter().enumerate() {
        let Value::Object(map) = slot else {
            return Err(format!("{key}: {part} is not inside a struct"));
        };
        if i + 1 == parts.len() {
            if value.is_null() {
                map.remove(*part);
            } else {
                map.insert((*part).to_owned(), value.clone());
            }
            break;
        }
        slot = map
            .entry((*part).to_owned())
            .or_insert_with(|| Value::Object(Default::default()));
    }
    serde_json::from_value::<UiNodeDef>(json).map_err(|e| format!("{key}: {e}"))
}

/// A variant override that did not apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantDiagnostic {
    /// The node key.
    pub node: String,
    /// The field key (empty when the node is unknown).
    pub field: String,
    /// What is wrong.
    pub message: String,
}

fn node_at_mut<'a>(root: &'a mut UiNodeDef, path: &[usize]) -> Option<&'a mut UiNodeDef> {
    let mut node = root;
    for &i in path {
        node = node.children.get_mut(i)?;
    }
    Some(node)
}

/// `root` with `variant`'s overrides applied (node keys in order, fields in key order, so a whole field
/// comes before its sub-fields). An override that does not apply is skipped and reported.
#[must_use]
pub fn apply_variant(root: &UiNodeDef, variant: &LayoutVariant) -> (UiNodeDef, Vec<VariantDiagnostic>) {
    let mut out = root.clone();
    let mut diagnostics = Vec::new();
    for (key, fields) in &variant.overrides {
        let Some(path) = find_node(&out, key) else {
            diagnostics.push(VariantDiagnostic {
                node: key.clone(),
                field: String::new(),
                message: format!("no node {key:?}"),
            });
            continue;
        };
        let Some(node) = node_at_mut(&mut out, &path) else {
            continue;
        };
        for (field, value) in fields {
            match apply_override(node, field, value) {
                Ok(next) => *node = next,
                Err(message) => diagnostics.push(VariantDiagnostic {
                    node: key.clone(),
                    field: field.clone(),
                    message,
                }),
            }
        }
    }
    (out, diagnostics)
}

/// The variant a root shows: set by [`crate::authoring::spawn_ui`] on every root spawned from a
/// template (`index` `None`: the base).
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct ActiveLayoutVariant {
    /// Index into the document's `variants`.
    pub index: Option<usize>,
    /// The variant's name.
    pub name: Option<String>,
    /// The node keys the variant overrides.
    pub nodes: Vec<String>,
    /// The overrides that did not apply, as `node.field: message`.
    pub problems: Vec<String>,
}

/// Counters of [`apply_layout_variants`] (tests and `ui.inspect` read them).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayoutVariantStats {
    /// Roots respawned because the window picked another variant.
    pub respawns: u64,
    /// Window sizes looked at (changes of [`UiScales`]).
    pub checks: u64,
}

/// The window size [`apply_layout_variants`] last looked at.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
struct LastVariantSize(Option<[f32; 2]>);

/// The root definition to spawn for `file` at a window of `size` (logical px), with what shows.
#[must_use]
pub fn resolve(file: &crate::authoring::UiFile, size: [f32; 2]) -> (UiNodeDef, ActiveLayoutVariant) {
    let Some(index) = select_variant(&file.variants, size) else {
        return (file.root.clone(), ActiveLayoutVariant::default());
    };
    let variant = &file.variants[index];
    let (root, diagnostics) = apply_variant(&file.root, variant);
    let active = ActiveLayoutVariant {
        index: Some(index),
        name: Some(variant.label(index)),
        nodes: variant.overrides.keys().cloned().collect(),
        problems: diagnostics
            .iter()
            .map(|d| {
                if d.field.is_empty() {
                    format!("{}: {}", d.node, d.message)
                } else {
                    format!("{}.{}: {}", d.node, d.field, d.message)
                }
            })
            .collect(),
    };
    (root, active)
}

/// `Update`, after the live-reload pass: when the window size changed, respawns once each window root
/// whose template now picks another variant (same [`UiRoot`] settings; focus and scroll kept by node
/// key). Camera-target and world roots keep the variant they spawned with, also when they share the
/// template with a window root.
pub fn apply_layout_variants(world: &mut World) {
    let Some(scales) = world.get_resource::<UiScales>().copied() else {
        return;
    };
    let size = logical_window_size(&scales);
    world.init_resource::<LastVariantSize>();
    if world.resource::<LastVariantSize>().0 == Some(size) {
        return;
    }
    world.resource_mut::<LastVariantSize>().0 = Some(size);
    let _span = tracing::debug_span!("apply_layout_variants").entered();
    world.init_resource::<LayoutVariantStats>();
    world.resource_mut::<LayoutVariantStats>().checks += 1;
    let mut roots =
        world.query_filtered::<(Entity, &UiTemplate, Option<&ActiveLayoutVariant>), With<UiRoot>>();
    let live: Vec<(Entity, String, Option<usize>)> = roots
        .iter(world)
        .map(|(e, t, a)| (e, t.0.clone(), a.and_then(|a| a.index)))
        .collect();
    // Only the stale window roots: a world or camera root of the same template keeps its tree and
    // the variant it spawned with (ADR-7858).
    let mut stale: Vec<(Entity, String)> = Vec::new();
    {
        let Some(library) = world.get_resource::<UiLibrary>() else {
            return;
        };
        for (entity, template, shown) in &live {
            if crate::world_target::root_target_size(world, *entity).is_some() {
                continue;
            }
            let Some(file) = library.templates.get(template) else {
                continue;
            };
            if file.variants.is_empty() {
                continue;
            }
            if select_variant(&file.variants, size) != *shown {
                stale.push((*entity, template.clone()));
            }
        }
    }
    for (entity, template) in stale {
        if crate::authoring::respawn_ui_root(world, entity) {
            world.resource_mut::<LayoutVariantStats>().respawns += 1;
            tracing::debug!(target: "regia_ui", "layout variant change: {template} (window root re-laid out)");
        }
    }
}

/// The `layout_variant` entry of `ui.inspect` for a node: the variant its root shows, or `null` when
/// the root came from no template.
#[must_use]
pub fn inspect_json(world: &World, entity: Entity) -> Value {
    let mut at = entity;
    loop {
        if let Some(active) = world.get::<ActiveLayoutVariant>(at) {
            let template = world.get::<UiTemplate>(at).map(|t| t.0.clone());
            let count = template
                .as_ref()
                .and_then(|t| world.get_resource::<UiLibrary>()?.templates.get(t))
                .map_or(0, |f| f.variants.len());
            let node_name = world.get::<crate::NodeName>(entity).map(|n| n.0.clone());
            let overridden = node_name.is_some_and(|n| active.nodes.contains(&n));
            return serde_json::json!({
                "template": template,
                "index": active.index,
                "name": active.name,
                "variants": count,
                "nodes": active.nodes,
                "node_overridden": overridden,
                "problems": active.problems,
            });
        }
        match world.get::<ChildOf>(at) {
            Some(parent) => at = parent.parent(),
            None => return Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> UiNodeDef {
        UiNodeDef {
            children: vec![
                UiNodeDef {
                    name: Some("a".into()),
                    children: vec![UiNodeDef {
                        name: Some("b".into()),
                        ..UiNodeDef::default()
                    }],
                    ..UiNodeDef::default()
                },
                UiNodeDef {
                    name: Some("b".into()),
                    ..UiNodeDef::default()
                },
                UiNodeDef::default(),
            ],
            ..UiNodeDef::default()
        }
    }

    #[test]
    fn node_keys_use_unique_reachable_names_else_paths() {
        let root = tree();
        assert_eq!(node_key(&root, &[0]), "a");
        assert_eq!(node_key(&root, &[0, 0]), "b", "the first b depth first");
        assert_eq!(node_key(&root, &[1]), "/1", "a later b is found by path");
        assert_eq!(node_key(&root, &[2]), "/2");
        assert_eq!(node_key(&root, &[]), "/");
        for path in [vec![0], vec![0, 0], vec![1], vec![2], vec![]] {
            assert_eq!(find_node(&root, &node_key(&root, &path)), Some(path));
        }
        assert_eq!(find_node(&root, "#a"), Some(vec![0]));
        assert_eq!(find_node(&root, "/7"), None);
        assert_eq!(find_node(&root, "zz"), None);
    }

    #[test]
    fn bad_bounds_are_reported() {
        let w = VariantWhen {
            min_width: Some(900.0),
            max_width: Some(600.0),
            ..VariantWhen::default()
        };
        assert!(w.problem().is_some());
        let w = VariantWhen {
            min_aspect: Some(f32::NAN),
            ..VariantWhen::default()
        };
        assert!(w.problem().is_some());
        assert_eq!(VariantWhen::default().problem(), None);
        assert_eq!(VariantWhen::default().describe(), "any window");
    }

    #[test]
    fn logical_size_divides_by_the_os_factor() {
        let s = UiScales {
            window_size: [2000.0, 1000.0],
            scale_factor: 2.0,
        };
        assert_eq!(logical_window_size(&s), [1000.0, 500.0]);
        let zero = UiScales {
            window_size: [10.0, 20.0],
            scale_factor: 0.0,
        };
        assert_eq!(logical_window_size(&zero), [10.0, 20.0]);
    }
}
