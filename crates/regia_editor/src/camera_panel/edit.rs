// SPDX-License-Identifier: MPL-2.0
//! Typed edits of a camera asset's working copy (ADR-7734): node, rig and transition fields through
//! the serde form tree the ability form uses ([`crate::abilities_panel::form::tree`]), and the
//! structure edits (add, remove, duplicate, reorder, rename) of nodes, rigs and transitions.
//!
//! Every edit is a function of the [`CameraAsset`] alone: it either changes the asset and keeps it
//! loadable (the result parses back to the same value and names no rig that is gone), or refuses with
//! a code and leaves it untouched. The panel wraps each one in an undo step.
//!
//! **Fields.** A node is shown as its serde form: `{ "Boom": { "length": { "Literal": 4.0 }, ... } }`.
//! A field path is relative to the node's variant (`length`, `length.Literal`, `offset[1]`); a value
//! set there is parsed back into a [`Node`], which checks its shape. An enum value (`Value<f32>`'s
//! `Literal`/`Param`/`Var`, a target, a blend) switches variant with its fields default-filled. A
//! `Group` or `When` node's `nodes` are edited by the structure edits only. Transitions use the same
//! paths from the transition record (`blend.Smooth`, `from.Rig`, `conditions`); a rig's form edits
//! `tags` and `blend_out` (its name changes with [`rename_rig`], its nodes and transitions with the
//! structure edits).
//!
//! **References.** Nodes have no names, so nothing refers to a node; transitions, director rules and
//! `RigIs` conditions refer to rigs by name. Removing a rig that something else still names is
//! refused (`camera_panel_rig_in_use`, the places in the message); renaming a rig rewrites every
//! such reference.

use std::collections::BTreeMap;

use regia_framework::camera::asset::{
    Blend, CameraAsset, Condition, DirectorDef, InputSlotDef, Node, RigDef, RigMatch, SlotKind, StartFacing,
    TransitionDef,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::abilities_panel::form::tree;

/// The node kinds [`add_node`] makes, as their `.camera.ron` names, in the Add menu's order.
pub const NODE_KINDS: [&str; 16] = [
    "Pivot",
    "LookInput",
    "Boom",
    "Collision",
    "Fov",
    "Offset",
    "SmoothPosition",
    "SmoothRotation",
    "Lean",
    "Frame",
    "Shake",
    "Fly",
    "Rail",
    "Crane",
    "Group",
    "When",
];

/// The rig fields the rig form edits (the rest have their own edits).
pub const RIG_FIELDS: [&str; 2] = ["tags", "blend_out"];

/// An edit that was refused: a protocol code (lowercase, documented in
/// `book/agents/camera-framework.md`) and what was wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditError {
    /// The code.
    pub code: &'static str,
    /// The message.
    pub message: String,
}

fn refuse(code: &'static str, message: impl Into<String>) -> EditError {
    EditError {
        code,
        message: message.into(),
    }
}

/// Where a transition is: in a rig's own list (`owner` names the rig) or the asset's shared list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionAt {
    /// The rig whose list holds it; `None` for the shared list.
    pub owner: Option<String>,
    /// Its index in that list.
    pub index: usize,
}

// ------------------------------------------------------------------------------------ lookups

fn rig_mut<'a>(asset: &'a mut CameraAsset, rig: &str) -> Result<&'a mut RigDef, EditError> {
    asset
        .rigs
        .iter_mut()
        .find(|def| def.name == rig)
        .ok_or_else(|| refuse("camera_unknown_rig", format!("the asset has no rig {rig:?}")))
}

fn rig_position(asset: &CameraAsset, rig: &str) -> Result<usize, EditError> {
    asset
        .rig_index(rig)
        .ok_or_else(|| refuse("camera_unknown_rig", format!("the asset has no rig {rig:?}")))
}

/// The node list at `parent`: the rig's root for an empty path, else the nested list of the
/// `Group` or `When` there.
fn list_mut<'a>(rig: &'a mut RigDef, parent: &[usize]) -> Result<&'a mut Vec<Node>, EditError> {
    let name = rig.name.clone();
    let mut list = &mut rig.root;
    for (depth, step) in parent.iter().enumerate() {
        let missing = || {
            refuse(
                "camera_panel_unknown_node",
                format!("{name} has no node at {:?}", &parent[..=depth]),
            )
        };
        list = match list.get_mut(*step).ok_or_else(missing)? {
            Node::Group { nodes, .. } | Node::When { nodes, .. } => nodes,
            other => {
                return Err(refuse(
                    "camera_panel_not_a_group",
                    format!(
                        "the node at {:?} is a {}, which has no nested nodes",
                        &parent[..=depth],
                        other.kind()
                    ),
                ));
            }
        };
    }
    Ok(list)
}

/// The list holding the node at `path` and the node's index in it.
fn parent_of<'a>(rig: &'a mut RigDef, path: &[usize]) -> Result<(&'a mut Vec<Node>, usize), EditError> {
    let Some((last, parent)) = path.split_last() else {
        return Err(refuse("camera_panel_unknown_node", "the node path is empty"));
    };
    let name = rig.name.clone();
    let list = list_mut(rig, parent)?;
    if *last >= list.len() {
        return Err(refuse(
            "camera_panel_unknown_node",
            format!("{name} has no node at {path:?}"),
        ));
    }
    Ok((list, *last))
}

fn node_mut<'a>(asset: &'a mut CameraAsset, rig: &str, path: &[usize]) -> Result<&'a mut Node, EditError> {
    let (list, index) = parent_of(rig_mut(asset, rig)?, path)?;
    Ok(&mut list[index])
}

fn transitions_mut<'a>(
    asset: &'a mut CameraAsset,
    owner: Option<&str>,
) -> Result<&'a mut Vec<TransitionDef>, EditError> {
    match owner {
        None => Ok(&mut asset.transitions),
        Some(rig) => Ok(&mut rig_mut(asset, rig)?.transitions),
    }
}

fn transition_mut<'a>(
    asset: &'a mut CameraAsset,
    at: &TransitionAt,
) -> Result<&'a mut TransitionDef, EditError> {
    let list = transitions_mut(asset, at.owner.as_deref())?;
    list.get_mut(at.index).ok_or_else(|| unknown_transition(at))
}

fn unknown_transition(at: &TransitionAt) -> EditError {
    let list = at
        .owner
        .as_deref()
        .map_or_else(|| "the shared list".to_owned(), |rig| format!("rig {rig:?}"));
    refuse(
        "camera_panel_unknown_transition",
        format!("{list} has no transition {}", at.index),
    )
}

fn bad_index(len: usize, to: usize) -> EditError {
    refuse(
        "camera_panel_bad_index",
        format!("index {to} is past the end of a list of {len}"),
    )
}

// ------------------------------------------------------------------------------- form trees

/// One step of a field path.
#[derive(Debug, PartialEq)]
enum Step<'a> {
    Key(&'a str),
    Index(usize),
}

/// Splits `offset[1]` or `length.Literal` into steps; `None` for a malformed path.
fn steps(path: &str) -> Option<Vec<Step<'_>>> {
    let mut out = Vec::new();
    for part in path.split('.') {
        let (key, mut rest) = match part.find('[') {
            Some(at) => (&part[..at], &part[at..]),
            None => (part, ""),
        };
        if key.is_empty() {
            return None;
        }
        out.push(Step::Key(key));
        while let Some(inner) = rest.strip_prefix('[') {
            let close = inner.find(']')?;
            out.push(Step::Index(inner[..close].parse().ok()?));
            rest = &inner[close + 1..];
        }
        if !rest.is_empty() {
            return None;
        }
    }
    (!out.is_empty()).then_some(out)
}

fn at_path<'a>(tree: &'a mut Value, path: &str, shown: &str) -> Result<&'a mut Value, EditError> {
    let missing = || refuse("camera_panel_no_field", format!("there is no field `{shown}`"));
    let mut at = tree;
    for step in steps(path).ok_or_else(missing)? {
        at = match step {
            Step::Key(key) => at.as_object_mut().and_then(|o| o.get_mut(key)),
            Step::Index(i) => at.as_array_mut().and_then(|a| a.get_mut(i)),
        }
        .ok_or_else(missing)?;
    }
    Ok(at)
}

fn first_key(field: &str) -> &str {
    field.split(['.', '[']).next().unwrap_or(field)
}

fn to_tree<T: Serialize>(value: &T) -> Result<Value, EditError> {
    tree::to_tree(value).map_err(|e| refuse("camera_bad_value", e.to_string()))
}

/// `value` with `path` set to `new`, read back as a `T` (which checks the value's shape).
fn with_field<T: Serialize + DeserializeOwned>(
    value: &T,
    path: &str,
    shown: &str,
    new: &Value,
) -> Result<T, EditError> {
    let mut tree = to_tree(value)?;
    *at_path(&mut tree, path, shown)? = new.clone();
    tree::from_tree(tree)
        .map_err(|e| refuse("camera_bad_value", format!("`{shown}` does not take {new}: {e}")))
}

/// `value` with the enum at `path` switched to `variant`, its fields default-filled.
fn with_variant<T: Serialize + DeserializeOwned>(
    value: &T,
    path: &str,
    shown: &str,
    variant: &str,
) -> Result<T, EditError> {
    let mut tree = to_tree(value)?;
    let enums = tree::enum_variants::<T>(&tree).map_err(|e| refuse("camera_bad_value", e.to_string()))?;
    match enums.get(path) {
        Some(variants) if variants.contains(&variant) => {}
        Some(variants) => {
            return Err(refuse(
                "camera_bad_value",
                format!("`{shown}` has no variant `{variant}` (one of {variants:?})"),
            ));
        }
        None => {
            at_path(&mut tree, path, shown)?;
            return Err(refuse("camera_bad_value", format!("`{shown}` is not an enum")));
        }
    }
    *at_path(&mut tree, path, shown)? = tree::fill_record(Some(variant));
    let filled = tree::filled::<T>(&tree).map_err(|e| {
        refuse(
            "camera_bad_value",
            format!("`{shown}` as `{variant}` cannot be filled in: {e}"),
        )
    })?;
    tree::from_tree(filled).map_err(|e| refuse("camera_bad_value", e.to_string()))
}

/// The enum paths of `tree` (a `T`'s form tree) under `prefix.`, with the prefix taken off.
fn enums_under<T: DeserializeOwned>(
    tree: &Value,
    prefix: Option<&str>,
) -> BTreeMap<String, Vec<&'static str>> {
    let Ok(all) = tree::enum_variants::<T>(tree) else {
        return BTreeMap::new();
    };
    all.into_iter()
        .filter_map(|(path, variants)| {
            let path = match prefix {
                Some(prefix) => path.strip_prefix(prefix)?.strip_prefix('.')?.to_owned(),
                None if path.is_empty() => return None,
                None => path,
            };
            Some((path, variants.to_vec()))
        })
        .collect()
}

/// What a form shows of one node, transition or rig: its fields as a form tree and the paths that
/// hold enum values with their variants.
#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    /// The node kind (`Boom`); empty for a transition or a rig.
    pub kind: String,
    /// The fields: a record of field name to form value.
    pub fields: Value,
    /// Every field path holding an enum, with its variants.
    pub enums: BTreeMap<String, Vec<&'static str>>,
}

/// The form of a node. `Group` and `When` show their `nodes` only as a count.
///
/// # Errors
///
/// `camera_bad_value` when the node cannot be written as a tree (a bug).
pub fn node_form(node: &Node) -> Result<Form, EditError> {
    let tree = to_tree(node)?;
    let (kind, mut fields) = tree
        .as_object()
        .and_then(|o| o.iter().next())
        .map(|(k, v)| (k.clone(), v.clone()))
        .ok_or_else(|| refuse("camera_bad_value", "a node is a one-key record"))?;
    let enums = enums_under::<Node>(&tree, Some(&kind))
        .into_iter()
        .filter(|(path, _)| first_key(path) != "nodes")
        .collect();
    if let Some(nodes) = fields.get_mut("nodes") {
        *nodes = Value::from(node.children().len());
    }
    Ok(Form { kind, fields, enums })
}

/// The form of a transition.
///
/// # Errors
///
/// `camera_bad_value` when it cannot be written as a tree (a bug).
pub fn transition_form(transition: &TransitionDef) -> Result<Form, EditError> {
    let fields = to_tree(transition)?;
    let enums = enums_under::<TransitionDef>(&fields, None);
    Ok(Form {
        kind: String::new(),
        fields,
        enums,
    })
}

/// The form of a rig's own fields ([`RIG_FIELDS`]).
///
/// # Errors
///
/// `camera_bad_value` when it cannot be written as a tree (a bug).
pub fn rig_form(rig: &RigDef) -> Result<Form, EditError> {
    let tree = to_tree(rig)?;
    let mut fields = serde_json::Map::new();
    for key in RIG_FIELDS {
        if let Some(value) = tree.get(key) {
            fields.insert(key.to_owned(), value.clone());
        }
    }
    Ok(Form {
        kind: String::new(),
        fields: Value::Object(fields),
        enums: BTreeMap::new(),
    })
}

fn node_path(node: &Node, field: &str) -> Result<String, EditError> {
    if first_key(field) == "nodes" && matches!(node, Node::Group { .. } | Node::When { .. }) {
        return Err(refuse(
            "camera_panel_structure_field",
            "nested nodes change with add_node, remove_node, duplicate_node and move_node",
        ));
    }
    let kind = node_form(node)?.kind;
    Ok(format!("{kind}.{field}"))
}

/// Sets `field` of the node at `path` of `rig` to `value` (a form value).
///
/// # Errors
///
/// `camera_unknown_rig`, `camera_panel_unknown_node`, `camera_panel_not_a_group` (the path goes
/// through a node with no nested nodes), `camera_panel_no_field`, `camera_panel_structure_field`
/// (`nodes`), `camera_bad_value` (the value does not fit the field).
pub fn set_node_field(
    asset: &mut CameraAsset,
    rig: &str,
    path: &[usize],
    field: &str,
    value: &Value,
) -> Result<(), EditError> {
    let node = node_mut(asset, rig, path)?;
    let full = node_path(node, field)?;
    *node = with_field(node, &full, field, value)?;
    Ok(())
}

/// Switches the enum at `field` of the node at `path` to `variant` (its fields default-filled).
///
/// # Errors
///
/// As [`set_node_field`]; `camera_bad_value` also when the field is not an enum or has no such
/// variant.
pub fn set_node_variant(
    asset: &mut CameraAsset,
    rig: &str,
    path: &[usize],
    field: &str,
    variant: &str,
) -> Result<(), EditError> {
    let node = node_mut(asset, rig, path)?;
    let full = node_path(node, field)?;
    *node = with_variant(node, &full, field, variant)?;
    Ok(())
}

/// Sets `field` of transition `at` to `value`.
///
/// # Errors
///
/// `camera_unknown_rig`, `camera_panel_unknown_transition`, `camera_panel_no_field`,
/// `camera_bad_value`.
pub fn set_transition_field(
    asset: &mut CameraAsset,
    at: &TransitionAt,
    field: &str,
    value: &Value,
) -> Result<(), EditError> {
    let transition = transition_mut(asset, at)?;
    *transition = with_field(transition, field, field, value)?;
    Ok(())
}

/// Switches the enum at `field` of transition `at` to `variant`.
///
/// # Errors
///
/// As [`set_transition_field`].
pub fn set_transition_variant(
    asset: &mut CameraAsset,
    at: &TransitionAt,
    field: &str,
    variant: &str,
) -> Result<(), EditError> {
    let transition = transition_mut(asset, at)?;
    *transition = with_variant(transition, field, field, variant)?;
    Ok(())
}

/// Sets `field` (under one of [`RIG_FIELDS`]) of `rig`.
///
/// # Errors
///
/// `camera_unknown_rig`, `camera_panel_no_field` (also for `name`, `root` and `transitions`, which
/// have their own edits), `camera_bad_value`.
pub fn set_rig_field(
    asset: &mut CameraAsset,
    rig: &str,
    field: &str,
    value: &Value,
) -> Result<(), EditError> {
    if !RIG_FIELDS.contains(&first_key(field)) {
        return Err(refuse(
            "camera_panel_no_field",
            format!(
                "a rig's form edits {RIG_FIELDS:?}; `{field}` changes with rename_rig or the node and transition edits"
            ),
        ));
    }
    let def = rig_mut(asset, rig)?;
    *def = with_field(def, field, field, value)?;
    Ok(())
}

/// The list at `path` of `value`'s tree with one more default-filled entry (serde defaults, else zero
/// values, an enum's first variant; when the type has none, the last entry repeated).
fn list_with_entry<T: Serialize + DeserializeOwned>(
    value: &T,
    path: &str,
    shown: &str,
) -> Result<Value, EditError> {
    let mut tree = to_tree(value)?;
    let items = at_path(&mut tree, path, shown)?
        .as_array_mut()
        .ok_or_else(|| refuse("camera_bad_value", format!("`{shown}` is not a list")))?;
    let last = items.last().cloned();
    items.push(tree::fill_record(None));
    match tree::filled::<T>(&tree) {
        Ok(mut filled) => Ok(at_path(&mut filled, path, shown)?.clone()),
        Err(e) => {
            let last = last.ok_or_else(|| {
                refuse(
                    "camera_bad_value",
                    format!("`{shown}` cannot get a default entry ({e}); set the whole list"),
                )
            })?;
            let mut tree = to_tree(value)?;
            let list = at_path(&mut tree, path, shown)?;
            if let Some(items) = list.as_array_mut() {
                items.push(last);
            }
            Ok(list.clone())
        }
    }
}

/// The list `field` of the node at `path` with one more entry: the value a `node_field` edit of
/// `field` sets for the form's **Add entry**.
///
/// # Errors
///
/// As [`set_node_field`]; `camera_bad_value` when `field` is not a list.
pub fn node_list_entry(
    asset: &CameraAsset,
    rig: &str,
    path: &[usize],
    field: &str,
) -> Result<Value, EditError> {
    let mut copy = asset.clone();
    let node = node_mut(&mut copy, rig, path)?;
    let full = node_path(node, field)?;
    list_with_entry(node, &full, field)
}

/// The list `field` of transition `at` with one more entry (see [`node_list_entry`]).
///
/// # Errors
///
/// As [`set_transition_field`].
pub fn transition_list_entry(
    asset: &CameraAsset,
    at: &TransitionAt,
    field: &str,
) -> Result<Value, EditError> {
    let mut copy = asset.clone();
    list_with_entry(transition_mut(&mut copy, at)?, field, field)
}

/// The list `field` of `rig` with one more entry (see [`node_list_entry`]).
///
/// # Errors
///
/// As [`set_rig_field`].
pub fn rig_list_entry(asset: &CameraAsset, rig: &str, field: &str) -> Result<Value, EditError> {
    let mut copy = asset.clone();
    list_with_entry(rig_mut(&mut copy, rig)?, field, field)
}

// --------------------------------------------------------------------------------- nodes

/// A default-filled node of `kind` (one of [`NODE_KINDS`]), valid in `asset`: name fields take the
/// asset's first fitting input slot, a rail names a spline `rail`.
///
/// # Errors
///
/// `camera_panel_bad_kind` for a kind not in [`NODE_KINDS`].
pub fn new_node(asset: &CameraAsset, kind: &str) -> Result<Node, EditError> {
    if !NODE_KINDS.contains(&kind) {
        return Err(refuse(
            "camera_panel_bad_kind",
            format!("unknown node kind {kind:?} (one of {NODE_KINDS:?})"),
        ));
    }
    let filled = tree::filled::<Node>(&tree::fill_record(Some(kind))).map_err(|e| {
        refuse(
            "camera_panel_bad_kind",
            format!("{kind} cannot be filled in: {e}"),
        )
    })?;
    let mut node: Node =
        tree::from_tree(filled).map_err(|e| refuse("camera_panel_bad_kind", e.to_string()))?;
    let slot = |two: bool| {
        asset
            .inputs
            .iter()
            .find(|slot| !two || slot.kind == SlotKind::Axis2)
            .map_or_else(|| "look".to_owned(), |slot| slot.name.clone())
    };
    match &mut node {
        Node::LookInput { slot: name, .. } => *name = slot(false),
        Node::Fly {
            slot: name, speed, ..
        } => {
            *name = slot(true);
            *speed = 5.0;
        }
        Node::Rail { spline, .. } => "rail".clone_into(spline),
        Node::Fov { degrees } => *degrees = regia_framework::camera::asset::Value::Literal(70.0),
        Node::Collision { radius, .. } => *radius = 0.25,
        Node::Lean { gain, max, speed } => (*gain, *max, *speed) = (0.1, 0.2, 4.0),
        _ => {}
    }
    Ok(node)
}

/// Adds a new node of `kind` to the list at `parent` (empty: the rig's root; else a `Group` or
/// `When`), at `index` (default: the end). Returns the new node's path.
///
/// # Errors
///
/// `camera_unknown_rig`, `camera_panel_unknown_node`, `camera_panel_not_a_group`,
/// `camera_panel_bad_kind`, `camera_panel_bad_index`.
pub fn add_node(
    asset: &mut CameraAsset,
    rig: &str,
    parent: &[usize],
    index: Option<usize>,
    kind: &str,
) -> Result<Vec<usize>, EditError> {
    let node = new_node(asset, kind)?;
    let list = list_mut(rig_mut(asset, rig)?, parent)?;
    let at = index.unwrap_or(list.len());
    if at > list.len() {
        return Err(bad_index(list.len(), at));
    }
    list.insert(at, node);
    let mut path = parent.to_vec();
    path.push(at);
    Ok(path)
}

/// Removes the node at `path` (with its nested nodes).
///
/// # Errors
///
/// `camera_unknown_rig`, `camera_panel_unknown_node`, `camera_panel_not_a_group`.
pub fn remove_node(asset: &mut CameraAsset, rig: &str, path: &[usize]) -> Result<Node, EditError> {
    let (list, index) = parent_of(rig_mut(asset, rig)?, path)?;
    Ok(list.remove(index))
}

/// Puts a copy of the node at `path` right after it. Returns the copy's path.
///
/// # Errors
///
/// As [`remove_node`].
pub fn duplicate_node(asset: &mut CameraAsset, rig: &str, path: &[usize]) -> Result<Vec<usize>, EditError> {
    let (list, index) = parent_of(rig_mut(asset, rig)?, path)?;
    let copy = list[index].clone();
    list.insert(index + 1, copy);
    let mut out = path.to_vec();
    if let Some(last) = out.last_mut() {
        *last = index + 1;
    }
    Ok(out)
}

/// Moves the node at `path` to index `to` of the same list. Returns its new path.
///
/// # Errors
///
/// As [`remove_node`]; `camera_panel_bad_index` when `to` is past the end.
pub fn move_node(
    asset: &mut CameraAsset,
    rig: &str,
    path: &[usize],
    to: usize,
) -> Result<Vec<usize>, EditError> {
    let (list, index) = parent_of(rig_mut(asset, rig)?, path)?;
    if to >= list.len() {
        return Err(bad_index(list.len(), to));
    }
    let node = list.remove(index);
    list.insert(to, node);
    let mut out = path.to_vec();
    if let Some(last) = out.last_mut() {
        *last = to;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------- rigs

/// `base`, or `base_2`, `base_3`, ... : the first name no rig of `asset` has.
#[must_use]
pub fn unique_rig_name(asset: &CameraAsset, base: &str) -> String {
    let base = if base.trim().is_empty() {
        "rig"
    } else {
        base.trim()
    };
    if asset.rig(base).is_none() {
        return base.to_owned();
    }
    (2u32..)
        .map(|n| format!("{base}_{n}"))
        .find(|name| asset.rig(name).is_none())
        .unwrap_or_else(|| base.to_owned())
}

fn check_new_name(asset: &CameraAsset, name: &str) -> Result<(), EditError> {
    if name.trim().is_empty() || name.trim() != name {
        return Err(refuse(
            "camera_bad_value",
            format!("a rig name is not empty and has no outer spaces: {name:?}"),
        ));
    }
    if asset.rig(name).is_some() {
        return Err(refuse(
            "camera_panel_name_taken",
            format!("the asset already has a rig {name:?}"),
        ));
    }
    Ok(())
}

/// Adds a rig named `name` (default: a free `rig`, `rig_2`, ...) at the end, with one `Fov` node.
/// Returns its name.
///
/// # Errors
///
/// `camera_panel_name_taken`, `camera_bad_value` (an empty name).
pub fn add_rig(asset: &mut CameraAsset, name: Option<&str>) -> Result<String, EditError> {
    let name = match name {
        Some(name) => {
            check_new_name(asset, name)?;
            name.to_owned()
        }
        None => unique_rig_name(asset, "rig"),
    };
    let fov = new_node(asset, "Fov")?;
    asset.rigs.push(RigDef {
        name: name.clone(),
        tags: Vec::new(),
        root: vec![fov],
        transitions: Vec::new(),
        blend_out: None,
    });
    Ok(name)
}

fn condition_names(cond: &Condition, rig: &str) -> bool {
    match cond {
        Condition::RigIs(name) => name == rig,
        Condition::All(list) | Condition::Any(list) => list.iter().any(|c| condition_names(c, rig)),
        Condition::Not(inner) => condition_names(inner, rig),
        _ => false,
    }
}

fn rename_in_condition(cond: &mut Condition, old: &str, new: &str) {
    match cond {
        Condition::RigIs(name) if name == old => new.clone_into(name),
        Condition::All(list) | Condition::Any(list) => {
            for c in list {
                rename_in_condition(c, old, new);
            }
        }
        Condition::Not(inner) => rename_in_condition(inner, old, new),
        _ => {}
    }
}

fn transition_names(transition: &TransitionDef, rig: &str) -> bool {
    let end = |m: &RigMatch| matches!(m, RigMatch::Rig(name) if name == rig);
    end(&transition.from)
        || end(&transition.to)
        || transition.conditions.iter().any(|c| condition_names(c, rig))
}

fn rename_in_transition(transition: &mut TransitionDef, old: &str, new: &str) {
    for end in [&mut transition.from, &mut transition.to] {
        if let RigMatch::Rig(name) = end
            && name == old
        {
            new.clone_into(name);
        }
    }
    for cond in &mut transition.conditions {
        rename_in_condition(cond, old, new);
    }
}

fn director_refs(director: &DirectorDef, rig: &str, at: &str, out: &mut Vec<String>) {
    match director {
        DirectorDef::Fixed(name) if name == rig => out.push(format!("{at} (fixed)")),
        DirectorDef::Rules { default, rules } => {
            if default == rig {
                out.push(format!("{at} default"));
            }
            for (i, rule) in rules.iter().enumerate() {
                if rule.rig == rig || condition_names(&rule.when, rig) {
                    out.push(format!("{at} rule {i}"));
                }
            }
        }
        DirectorDef::Ranked(list) => {
            for (i, ranked) in list.iter().enumerate() {
                director_refs(&ranked.director, rig, &format!("{at} ranked {i}"), out);
            }
        }
        _ => {}
    }
}

fn rename_in_director(director: &mut DirectorDef, old: &str, new: &str) {
    match director {
        DirectorDef::Fixed(name) if name == old => new.clone_into(name),
        DirectorDef::Rules { default, rules } => {
            if default == old {
                new.clone_into(default);
            }
            for rule in rules {
                if rule.rig == old {
                    new.clone_into(&mut rule.rig);
                }
                rename_in_condition(&mut rule.when, old, new);
            }
        }
        DirectorDef::Ranked(list) => {
            for ranked in list {
                rename_in_director(&mut ranked.director, old, new);
            }
        }
        _ => {}
    }
}

fn nodes_name(nodes: &[Node], rig: &str) -> bool {
    nodes.iter().any(|node| match node {
        Node::When { cond, nodes } => condition_names(cond, rig) || nodes_name(nodes, rig),
        Node::Group { nodes, .. } => nodes_name(nodes, rig),
        _ => false,
    })
}

fn rename_in_nodes(nodes: &mut [Node], old: &str, new: &str) {
    for node in nodes {
        match node {
            Node::When { cond, nodes } => {
                rename_in_condition(cond, old, new);
                rename_in_nodes(nodes, old, new);
            }
            Node::Group { nodes, .. } => rename_in_nodes(nodes, old, new),
            _ => {}
        }
    }
}

/// Every place outside `rig` itself that names it: the director, transitions of the shared list and
/// of other rigs, and `RigIs` conditions of other rigs' `When` nodes.
#[must_use]
pub fn rig_references(asset: &CameraAsset, rig: &str) -> Vec<String> {
    let mut out = Vec::new();
    director_refs(&asset.director, rig, "the director", &mut out);
    for (i, transition) in asset.transitions.iter().enumerate() {
        if transition_names(transition, rig) {
            out.push(format!("shared transition {i}"));
        }
    }
    for other in asset.rigs.iter().filter(|other| other.name != rig) {
        for (i, transition) in other.transitions.iter().enumerate() {
            if transition_names(transition, rig) {
                out.push(format!("transition {i} of rig {}", other.name));
            }
        }
        if nodes_name(&other.root, rig) {
            out.push(format!("a When node of rig {}", other.name));
        }
    }
    out
}

/// Removes `rig` (its nodes and own transitions go with it).
///
/// # Errors
///
/// `camera_unknown_rig`; `camera_panel_rig_in_use` while anything outside the rig names it (the
/// message lists the places: change or remove those first, or rename the rig instead).
pub fn remove_rig(asset: &mut CameraAsset, rig: &str) -> Result<RigDef, EditError> {
    let index = rig_position(asset, rig)?;
    let refs = rig_references(asset, rig);
    if !refs.is_empty() {
        return Err(refuse(
            "camera_panel_rig_in_use",
            format!("rig {rig:?} is still named by {}", refs.join(", ")),
        ));
    }
    Ok(asset.rigs.remove(index))
}

/// Puts a copy of `rig` right after it under `name` (default: a free `<rig>_copy`). References to
/// the rig inside the copy (its own transitions, its `RigIs` conditions) are left as they were: they
/// still name the original. Returns the copy's name.
///
/// # Errors
///
/// `camera_unknown_rig`, `camera_panel_name_taken`, `camera_bad_value`.
pub fn duplicate_rig(asset: &mut CameraAsset, rig: &str, name: Option<&str>) -> Result<String, EditError> {
    let index = rig_position(asset, rig)?;
    let name = match name {
        Some(name) => {
            check_new_name(asset, name)?;
            name.to_owned()
        }
        None => unique_rig_name(asset, &format!("{rig}_copy")),
    };
    let mut copy = asset.rigs[index].clone();
    copy.name.clone_from(&name);
    asset.rigs.insert(index + 1, copy);
    Ok(name)
}

/// Moves `rig` to index `to` of the rig list.
///
/// # Errors
///
/// `camera_unknown_rig`, `camera_panel_bad_index`.
pub fn move_rig(asset: &mut CameraAsset, rig: &str, to: usize) -> Result<(), EditError> {
    let index = rig_position(asset, rig)?;
    if to >= asset.rigs.len() {
        return Err(bad_index(asset.rigs.len(), to));
    }
    let def = asset.rigs.remove(index);
    asset.rigs.insert(to, def);
    Ok(())
}

/// Renames `rig` to `name` and rewrites every reference to it (director, transitions, `RigIs`
/// conditions, the rig's own included).
///
/// # Errors
///
/// `camera_unknown_rig`, `camera_panel_name_taken`, `camera_bad_value` (an empty name).
pub fn rename_rig(asset: &mut CameraAsset, rig: &str, name: &str) -> Result<(), EditError> {
    let index = rig_position(asset, rig)?;
    if name == rig {
        return Ok(());
    }
    check_new_name(asset, name)?;
    name.clone_into(&mut asset.rigs[index].name);
    rename_in_director(&mut asset.director, rig, name);
    for transition in &mut asset.transitions {
        rename_in_transition(transition, rig, name);
    }
    for def in &mut asset.rigs {
        for transition in &mut def.transitions {
            rename_in_transition(transition, rig, name);
        }
        rename_in_nodes(&mut def.root, rig, name);
    }
    Ok(())
}

// --------------------------------------------------------------------------- transitions

/// Adds a transition from `from` to `to` (default `Any` to `Any`), a 0.25 s smooth blend, at the end
/// of the shared list (`owner` `None`) or of a rig's own list. Returns where it is.
///
/// # Errors
///
/// `camera_unknown_rig` (an unknown owner, or an end naming a rig the asset lacks).
pub fn add_transition(
    asset: &mut CameraAsset,
    owner: Option<&str>,
    from: Option<RigMatch>,
    to: Option<RigMatch>,
) -> Result<TransitionAt, EditError> {
    for end in [&from, &to].into_iter().flatten() {
        if let RigMatch::Rig(name) = end {
            rig_position(asset, name)?;
        }
    }
    let list = transitions_mut(asset, owner)?;
    list.push(TransitionDef {
        from: from.unwrap_or(RigMatch::Any),
        to: to.unwrap_or(RigMatch::Any),
        blend: Blend::Smooth(0.25),
        start_facing: StartFacing::OwnSeed,
        hold_outgoing: false,
        share_state: false,
        interruptible: true,
        conditions: Vec::new(),
    });
    Ok(TransitionAt {
        owner: owner.map(str::to_owned),
        index: list.len() - 1,
    })
}

/// Removes transition `at`.
///
/// # Errors
///
/// `camera_unknown_rig`, `camera_panel_unknown_transition`.
pub fn remove_transition(asset: &mut CameraAsset, at: &TransitionAt) -> Result<TransitionDef, EditError> {
    let list = transitions_mut(asset, at.owner.as_deref())?;
    if at.index >= list.len() {
        return Err(unknown_transition(at));
    }
    Ok(list.remove(at.index))
}

/// Puts a copy of transition `at` right after it. Returns where the copy is.
///
/// # Errors
///
/// As [`remove_transition`].
pub fn duplicate_transition(asset: &mut CameraAsset, at: &TransitionAt) -> Result<TransitionAt, EditError> {
    let copy = transition_mut(asset, at)?.clone();
    let list = transitions_mut(asset, at.owner.as_deref())?;
    list.insert(at.index + 1, copy);
    Ok(TransitionAt {
        owner: at.owner.clone(),
        index: at.index + 1,
    })
}

/// Moves transition `at` to index `to` of the same list (the order is the choice order, § 6.2).
///
/// # Errors
///
/// As [`remove_transition`]; `camera_panel_bad_index`.
pub fn move_transition(
    asset: &mut CameraAsset,
    at: &TransitionAt,
    to: usize,
) -> Result<TransitionAt, EditError> {
    let list = transitions_mut(asset, at.owner.as_deref())?;
    if at.index >= list.len() {
        return Err(unknown_transition(at));
    }
    if to >= list.len() {
        return Err(bad_index(list.len(), to));
    }
    let transition = list.remove(at.index);
    list.insert(to, transition);
    Ok(TransitionAt {
        owner: at.owner.clone(),
        index: to,
    })
}

// ------------------------------------------------------------------------------ new asset

/// A new, valid camera asset named `name`: a `look` input slot and one rig `follow` (pivot on the
/// driver's target, look input, a 4 m boom, 70° field of view) that a fixed director runs.
#[must_use]
pub fn new_asset(name: &str) -> CameraAsset {
    use regia_framework::camera::asset::{PivotSpace, TargetRef, Value as NodeValue};
    CameraAsset {
        format_version: regia_framework::camera::asset::CAMERA_FORMAT_VERSION,
        name: name.to_owned(),
        params: BTreeMap::new(),
        variables: Vec::new(),
        inputs: vec![InputSlotDef {
            name: "look".to_owned(),
            kind: SlotKind::Axis2,
            speed: (120.0, 90.0),
            pitch_clamp: (-80.0, 60.0),
            invert_pitch: false,
            normalize: false,
            pull_toward_var: None,
            pull_weight: 0.1,
        }],
        rigs: vec![RigDef {
            name: "follow".to_owned(),
            tags: Vec::new(),
            root: vec![
                Node::Pivot {
                    target: TargetRef::DriveTarget,
                    offset: [0.0, 1.5, 0.0],
                    space: PivotSpace::Target,
                    aim_offset: [0.0; 3],
                    inherit_rotation: false,
                },
                Node::LookInput {
                    slot: "look".to_owned(),
                    additive: false,
                },
                Node::Boom {
                    length: NodeValue::Literal(4.0),
                    side_offset: NodeValue::Literal(0.0),
                    up_offset: NodeValue::Literal(0.0),
                    length_speed: NodeValue::Literal(0.0),
                },
                Node::Fov {
                    degrees: NodeValue::Literal(70.0),
                },
            ],
            transitions: Vec::new(),
            blend_out: None,
        }],
        transitions: Vec::new(),
        director: DirectorDef::Fixed("follow".to_owned()),
    }
}

/// The text of [`new_asset`] for a file whose stem is `stem` (the asset's name), in canonical form.
///
/// # Errors
///
/// The canonical writer failed (a bug).
pub fn new_asset_text(stem: &str) -> Result<String, String> {
    new_asset(stem)
        .to_ron()
        .map_err(|e| format!("camera serialize: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_split_into_keys_and_indices() {
        assert_eq!(
            steps("offset[1]"),
            Some(vec![Step::Key("offset"), Step::Index(1)])
        );
        assert_eq!(
            steps("length.Literal"),
            Some(vec![Step::Key("length"), Step::Key("Literal")])
        );
        for bad in ["", ".x", "a[", "a[x]", "a[0]b"] {
            assert_eq!(steps(bad), None, "{bad}");
        }
    }

    #[test]
    fn every_node_kind_fills_in() {
        let asset = new_asset("n");
        let mut kinds = std::collections::BTreeSet::new();
        for kind in NODE_KINDS {
            let node = new_node(&asset, kind).unwrap_or_else(|e| panic!("{kind}: {e:?}"));
            assert_eq!(node_form(&node).expect("form").kind, kind);
            kinds.insert(node.kind());
        }
        assert_eq!(kinds.len(), NODE_KINDS.len(), "one node per kind");
        assert_eq!(
            new_node(&asset, "Nope").expect_err("bad").code,
            "camera_panel_bad_kind"
        );
    }

    #[test]
    fn a_new_asset_is_valid_and_canonical() {
        let asset = new_asset("cam");
        assert!(
            asset.validate(&|_| false).is_empty(),
            "{:?}",
            asset.validate(&|_| false)
        );
        let text = new_asset_text("cam").expect("text");
        assert_eq!(CameraAsset::from_ron(&text).expect("parses"), asset);
        assert_eq!(asset.to_ron().expect("again"), text);
    }
}
