// SPDX-License-Identifier: MPL-2.0
//! The Logic Graph panel's Variables list (ADR-8021, spec 102 § 5): declare, rename, type and
//! default the graph's locals (`local/`), declare and rename its world boards (`world/<board>/`),
//! and list and rename the entity keys (bare keys) the graph uses. Every change is one
//! [`super::edit`] call (one undo step), the same ops the agent's `editor.logic_variable_set` /
//! `_remove` make.
//!
//! A rename rewrites every reference in the graph: `BlackboardChanged` keys, condition keys, `BbRef`
//! bindings and `logic::SetBlackboard`'s literal key. Since `.logic.ron` v2 (ADR-8900, P-177) an
//! entity key and a world board key may also be declared with a type and a default: an entity
//! variable is scope `entity` with the key as its name, a world variable is scope `world` with the
//! name `<board>/<key>` (a name without `/` is the board itself). The runtime seeds a declared
//! default when the graph starts and the key is missing.

use regia_ai::logic::{Binding, LocalDef, LogicEvent};
use regia_ai::logic_edit::{LogicGraphDoc, LogicNodeKind, LogicOp};
use regia_ai::state_tree::{Condition, Literal};

/// Which store a variable lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarScope {
    /// A graph local, `local/<name>`.
    Local,
    /// A declared world board, `world/<name>/…`.
    World,
    /// An entity key, `<name>` or `entity/<name>`.
    Entity,
}

impl VarScope {
    /// `local`, `world` or `entity`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::World => "world",
            Self::Entity => "entity",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "local" => Some(Self::Local),
            "world" => Some(Self::World),
            "entity" => Some(Self::Entity),
            _ => None,
        }
    }
}

/// A local's value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarType {
    /// `Bool`.
    Bool,
    /// `Int`.
    Int,
    /// `Float`.
    Float,
    /// `String`.
    String,
}

impl VarType {
    /// Every type, in menu order.
    pub const ALL: [Self; 4] = [Self::Bool, Self::Int, Self::Float, Self::String];

    /// `bool`, `int`, `float` or `string`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Float => "float",
            Self::String => "string",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == s)
    }

    /// The type of `value`.
    #[must_use]
    pub fn of(value: &Literal) -> Self {
        match value {
            Literal::Bool(_) => Self::Bool,
            Literal::Int(_) => Self::Int,
            Literal::Float(_) => Self::Float,
            Literal::String(_) => Self::String,
        }
    }

    /// `value` converted to this type (a number keeps its value where it can; text that does not
    /// parse gives the type's zero).
    #[must_use]
    pub fn convert(self, value: &Literal) -> Literal {
        let text = match value {
            Literal::Bool(b) => b.to_string(),
            Literal::Int(i) => i.to_string(),
            Literal::Float(f) => f.to_string(),
            Literal::String(s) => s.clone(),
        };
        match (self, value) {
            (Self::Bool, Literal::Int(i)) => Literal::Bool(*i != 0),
            (Self::Bool, Literal::Float(f)) => Literal::Bool(*f != 0.0),
            (Self::Bool, _) => Literal::Bool(text.trim() == "true"),
            (Self::Int, Literal::Bool(b)) => Literal::Int(i64::from(*b)),
            (Self::Int, Literal::Float(f)) => Literal::Int(*f as i64),
            (Self::Int, _) => Literal::Int(text.trim().parse().unwrap_or(0)),
            (Self::Float, Literal::Bool(b)) => Literal::Float(if *b { 1.0 } else { 0.0 }),
            (Self::Float, Literal::Int(i)) => Literal::Float(*i as f32),
            (Self::Float, _) => Literal::Float(text.trim().parse().unwrap_or(0.0)),
            (Self::String, _) => Literal::String(text),
        }
    }

    /// The type's zero value (a new local's default).
    #[must_use]
    pub fn zero(self) -> Literal {
        match self {
            Self::Bool => Literal::Bool(false),
            Self::Int => Literal::Int(0),
            Self::Float => Literal::Float(0.0),
            Self::String => Literal::String(String::new()),
        }
    }
}

/// One row of the Variables list.
#[derive(Debug, Clone, PartialEq)]
pub struct VarRow {
    /// Where it lives.
    pub scope: VarScope,
    /// Its name (a local's name, a board's name or an entity key).
    pub name: String,
    /// A local's default (`None` for a board or an entity key).
    pub default: Option<Literal>,
    /// How many places in the graph read or write it.
    pub uses: usize,
}

/// A refused variable change: a stable code and a sentence.
pub type VarError = (&'static str, String);

/// A Variables list change (the panel's controls and the agent methods share it).
#[derive(Debug, Clone, PartialEq)]
pub enum VariableEdit {
    /// Declare a local (or change its type or default) / declare a world board.
    Set {
        /// `Local` or `World` (an entity key has no declaration).
        scope: VarScope,
        /// The name.
        name: String,
        /// A local's new type (its default converts).
        ty: Option<VarType>,
        /// A local's new default.
        default: Option<Literal>,
    },
    /// Rename a variable and every reference to it.
    Rename {
        /// Its scope.
        scope: VarScope,
        /// The current name.
        from: String,
        /// The new name.
        to: String,
    },
    /// Remove a local's or a board's declaration (references stay and show as issues).
    Remove {
        /// `Local` or `World`.
        scope: VarScope,
        /// The name.
        name: String,
    },
}

impl VariableEdit {
    /// The undo entry's label.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Set { scope, name, .. } => format!("set {} variable {name}", scope.name()),
            Self::Rename { scope, from, to } => format!("rename {} variable {from} to {to}", scope.name()),
            Self::Remove { scope, name } => format!("remove {} variable {name}", scope.name()),
        }
    }
}

fn check_name(name: &str) -> Result<(), VarError> {
    if name.is_empty() || name.contains('/') {
        Err((
            "bad_name",
            format!("{name:?}: a variable name is non-empty and has no `/`"),
        ))
    } else {
        Ok(())
    }
}

/// Every key the graph reads or writes, with the node it is in.
fn each_key(doc: &LogicGraphDoc, mut f: impl FnMut(&str)) {
    fn cond_keys(c: &Condition, f: &mut dyn FnMut(&str)) {
        match c {
            Condition::Always => {}
            Condition::Key(k) | Condition::Has(k) => f(k),
            Condition::Compare { key, .. } | Condition::Text { key, .. } => f(key),
            Condition::Not(inner) => cond_keys(inner, f),
            Condition::All(items) | Condition::Any(items) => items.iter().for_each(|c| cond_keys(c, f)),
        }
    }
    for node in doc.nodes.values() {
        match &node.kind {
            LogicNodeKind::Event {
                event: LogicEvent::BlackboardChanged { key },
                ..
            } => f(key),
            LogicNodeKind::Event { .. } => {}
            LogicNodeKind::Condition { condition } => cond_keys(condition, &mut f),
            LogicNodeKind::Action { task, set } => {
                for (param, binding) in set {
                    match binding {
                        Binding::BbRef(k) => f(k),
                        Binding::Literal(Literal::String(k))
                            if task == "logic::SetBlackboard" && param == "key" =>
                        {
                            f(k);
                        }
                        Binding::Literal(_) => {}
                    }
                }
            }
        }
    }
}

/// The scope and name a key refers to (`local/x` → local `x`, `world/b/x` → board `b`, `x` or
/// `entity/x` → entity `x`).
fn key_target(key: &str) -> Option<(VarScope, &str)> {
    if let Some(rest) = key.strip_prefix("local/") {
        Some((VarScope::Local, rest))
    } else if let Some(rest) = key.strip_prefix("world/") {
        rest.split_once('/').map(|(board, _)| (VarScope::World, board))
    } else if let Some(rest) = key.strip_prefix("entity/") {
        Some((VarScope::Entity, rest))
    } else if key.contains('/') || key.is_empty() {
        None
    } else {
        Some((VarScope::Entity, key))
    }
}

/// The Variables list of `doc`: locals in declaration order, boards in declaration order, then the
/// entity keys the graph uses (sorted).
#[must_use]
pub fn rows(doc: &LogicGraphDoc) -> Vec<VarRow> {
    let mut uses: std::collections::BTreeMap<(&'static str, String), usize> =
        std::collections::BTreeMap::new();
    each_key(doc, |key| {
        if let Some((scope, name)) = key_target(key) {
            *uses.entry((scope.name(), name.to_string())).or_default() += 1;
        }
    });
    let count =
        |scope: VarScope, name: &str| uses.get(&(scope.name(), name.to_string())).copied().unwrap_or(0);
    let mut out: Vec<VarRow> = doc
        .locals
        .iter()
        .map(|l| VarRow {
            scope: VarScope::Local,
            name: l.name.clone(),
            default: Some(l.default.clone()),
            uses: count(VarScope::Local, &l.name),
        })
        .collect();
    out.extend(doc.world.iter().map(|b| VarRow {
        scope: VarScope::World,
        name: b.clone(),
        default: None,
        uses: count(VarScope::World, b),
    }));
    // Declared world variables (v2) as `<board>/<key>`, with the uses of that exact key.
    let mut key_uses: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    each_key(doc, |key| {
        if let Some(rest) = key.strip_prefix("world/") {
            *key_uses.entry(rest.to_string()).or_default() += 1;
        }
    });
    out.extend(doc.world_vars.iter().map(|v| {
        let name = format!("{}/{}", v.board, v.name);
        VarRow {
            scope: VarScope::World,
            uses: key_uses.get(&name).copied().unwrap_or(0),
            name,
            default: Some(v.default.clone()),
        }
    }));
    // Entity keys: every declared one (v2, with its default) and every used one, sorted.
    let mut entity: std::collections::BTreeMap<String, VarRow> = uses
        .iter()
        .filter(|((scope, _), _)| *scope == "entity")
        .map(|((_, name), n)| {
            (
                name.clone(),
                VarRow {
                    scope: VarScope::Entity,
                    name: name.clone(),
                    default: None,
                    uses: *n,
                },
            )
        })
        .collect();
    for var in &doc.entity_vars {
        entity
            .entry(var.name.clone())
            .or_insert_with(|| VarRow {
                scope: VarScope::Entity,
                name: var.name.clone(),
                default: None,
                uses: 0,
            })
            .default = Some(var.default.clone());
    }
    out.extend(entity.into_values());
    out
}

/// A `<board>/<key>` world variable name split in two (`None`: a plain board name).
fn world_var(name: &str) -> Option<(&str, &str)> {
    name.split_once('/')
}

/// A world variable's board and key, both non-empty and the key without `/`.
fn check_world_var(name: &str) -> Result<(&str, &str), VarError> {
    match world_var(name) {
        Some((board, key)) if !board.is_empty() && !key.is_empty() && !key.contains('/') => Ok((board, key)),
        _ => Err((
            "bad_name",
            format!("{name:?}: a world variable is named <board>/<key>, both non-empty"),
        )),
    }
}

/// The type and default a declaration gets from `ty`, `default` and the `current` default (a
/// local, entity or world variable all follow the same rule).
fn typed_default(
    ty: Option<VarType>,
    default: Option<&Literal>,
    current: Option<&Literal>,
) -> Result<Literal, VarError> {
    let ty = ty
        .or_else(|| default.map(VarType::of))
        .or(current.map(VarType::of));
    let value = match (default, current) {
        (Some(d), _) => d.clone(),
        (None, Some(c)) => c.clone(),
        (None, None) => ty.unwrap_or(VarType::Int).zero(),
    };
    let value = match ty {
        Some(ty) if VarType::of(&value) != ty => {
            if default.is_some() {
                return Err((
                    "bad_value",
                    format!("the default {value:?} is not a {}", ty.name()),
                ));
            }
            ty.convert(&value)
        }
        _ => value,
    };
    if let Literal::Float(f) = value
        && !f.is_finite()
    {
        return Err(("bad_value", "a default float must be finite".into()));
    }
    Ok(value)
}

/// `key` with the variable `scope`/`from` renamed to `to` (`None`: the key does not refer to it).
fn renamed(key: &str, scope: VarScope, from: &str, to: &str) -> Option<String> {
    match scope {
        VarScope::Local => (key.strip_prefix("local/")? == from).then(|| format!("local/{to}")),
        VarScope::World => {
            let (board, rest) = key.strip_prefix("world/")?.split_once('/')?;
            (board == from).then(|| format!("world/{to}/{rest}"))
        }
        VarScope::Entity => {
            if key == from {
                Some(to.to_string())
            } else {
                (key.strip_prefix("entity/")? == from).then(|| format!("entity/{to}"))
            }
        }
    }
}

fn rename_condition(c: &Condition, r: &dyn Fn(&str) -> Option<String>) -> Condition {
    let k = |key: &str| r(key).unwrap_or_else(|| key.to_string());
    match c {
        Condition::Always => Condition::Always,
        Condition::Key(key) => Condition::Key(k(key.as_str())),
        Condition::Has(key) => Condition::Has(k(key.as_str())),
        Condition::Compare { key, op, value } => Condition::Compare {
            key: k(key.as_str()),
            op: *op,
            value: *value,
        },
        Condition::Text { key, value } => Condition::Text {
            key: k(key.as_str()),
            value: value.clone(),
        },
        Condition::Not(inner) => Condition::Not(Box::new(rename_condition(inner, r))),
        Condition::All(items) => Condition::All(items.iter().map(|c| rename_condition(c, r)).collect()),
        Condition::Any(items) => Condition::Any(items.iter().map(|c| rename_condition(c, r)).collect()),
    }
}

/// The ops that rewrite every reference to `scope`/`from` as `to`.
fn reference_ops(doc: &LogicGraphDoc, scope: VarScope, from: &str, to: &str) -> Vec<LogicOp> {
    let r = |key: &str| renamed(key, scope, from, to);
    let mut ops = Vec::new();
    for (id, node) in &doc.nodes {
        match &node.kind {
            LogicNodeKind::Event {
                event: LogicEvent::BlackboardChanged { key },
                ..
            } => {
                if let Some(key) = r(key) {
                    ops.push(LogicOp::SetEvent {
                        node: *id,
                        rule: None,
                        event: Some(LogicEvent::BlackboardChanged { key }),
                        priority: None,
                        cooldown_ticks: None,
                    });
                }
            }
            LogicNodeKind::Event { .. } => {}
            LogicNodeKind::Condition { condition } => {
                let next = rename_condition(condition, &r);
                if &next != condition {
                    ops.push(LogicOp::SetCondition {
                        node: *id,
                        condition: next,
                    });
                }
            }
            LogicNodeKind::Action { task, set } => {
                for (param, binding) in set {
                    let next = match binding {
                        Binding::BbRef(k) => r(k).map(Binding::BbRef),
                        Binding::Literal(Literal::String(k))
                            if task == "logic::SetBlackboard" && param == "key" =>
                        {
                            r(k).map(|k| Binding::Literal(Literal::String(k)))
                        }
                        Binding::Literal(_) => None,
                    };
                    if let Some(value) = next {
                        ops.push(LogicOp::SetBinding {
                            node: *id,
                            key: param.clone(),
                            value: Some(value),
                        });
                    }
                }
            }
        }
    }
    ops
}

/// The ops that rewrite every reference to the exact key `from` (a `world/<board>/<key>`) as `to`.
fn exact_key_ops(doc: &LogicGraphDoc, from: &str, to: &str) -> Vec<LogicOp> {
    let mut ops = Vec::new();
    let r = |key: &str| (key == from).then(|| to.to_string());
    for (id, node) in &doc.nodes {
        match &node.kind {
            LogicNodeKind::Event {
                event: LogicEvent::BlackboardChanged { key },
                ..
            } => {
                if let Some(key) = r(key) {
                    ops.push(LogicOp::SetEvent {
                        node: *id,
                        rule: None,
                        event: Some(LogicEvent::BlackboardChanged { key }),
                        priority: None,
                        cooldown_ticks: None,
                    });
                }
            }
            LogicNodeKind::Event { .. } => {}
            LogicNodeKind::Condition { condition } => {
                let next = rename_condition(condition, &r);
                if &next != condition {
                    ops.push(LogicOp::SetCondition {
                        node: *id,
                        condition: next,
                    });
                }
            }
            LogicNodeKind::Action { task, set } => {
                for (param, binding) in set {
                    let next = match binding {
                        Binding::BbRef(k) => r(k).map(Binding::BbRef),
                        Binding::Literal(Literal::String(k))
                            if task == "logic::SetBlackboard" && param == "key" =>
                        {
                            r(k).map(|k| Binding::Literal(Literal::String(k)))
                        }
                        Binding::Literal(_) => None,
                    };
                    if let Some(value) = next {
                        ops.push(LogicOp::SetBinding {
                            node: *id,
                            key: param.clone(),
                            value: Some(value),
                        });
                    }
                }
            }
        }
    }
    ops
}

/// Re-declares every local in order with `f` applied (keeps the declaration order a rename or a
/// type change would otherwise move to the end).
fn redeclare(locals: &[LocalDef], f: impl Fn(&LocalDef) -> LocalDef) -> Vec<LogicOp> {
    let mut ops: Vec<LogicOp> = locals
        .iter()
        .map(|l| LogicOp::SetLocal {
            name: l.name.clone(),
            default: None,
        })
        .collect();
    ops.extend(locals.iter().map(|l| {
        let l = f(l);
        LogicOp::SetLocal {
            name: l.name,
            default: Some(l.default),
        }
    }));
    ops
}

/// The ops one [`VariableEdit`] makes on `doc`.
///
/// # Errors
///
/// `bad_name`, `duplicate_name`, `unknown_variable`, `bad_value` (a default and a type that
/// disagree).
pub fn ops_for(doc: &LogicGraphDoc, edit: &VariableEdit) -> Result<Vec<LogicOp>, VarError> {
    let local = |name: &str| doc.locals.iter().find(|l| l.name == name);
    match edit {
        VariableEdit::Set {
            scope: VarScope::Local,
            name,
            ty,
            default,
        } => {
            check_name(name)?;
            let current = local(name.as_str()).map(|l| l.default.clone());
            let value = typed_default(*ty, default.as_ref(), current.as_ref())?;
            Ok(vec![LogicOp::SetLocal {
                name: name.clone(),
                default: Some(value),
            }])
        }
        // A typed world variable `<board>/<key>` (v2): the board is declared too when missing.
        VariableEdit::Set {
            scope: VarScope::World,
            name,
            ty,
            default,
        } if name.contains('/') => {
            let (board, key) = check_world_var(name)?;
            let current = doc
                .world_vars
                .iter()
                .find(|v| v.board == board && v.name == key)
                .map(|v| v.default.clone());
            let value = typed_default(*ty, default.as_ref(), current.as_ref())?;
            let mut ops = Vec::new();
            if !doc.world.iter().any(|b| b == board) {
                let mut world = doc.world.clone();
                world.push(board.to_string());
                ops.push(LogicOp::SetGraph {
                    description: None,
                    seed: None,
                    world: Some(world),
                });
            }
            ops.push(LogicOp::SetWorldVar {
                board: board.to_string(),
                name: key.to_string(),
                default: Some(value),
            });
            Ok(ops)
        }
        VariableEdit::Set {
            scope: VarScope::World,
            name,
            ..
        } => {
            check_name(name)?;
            if doc.world.contains(name) {
                return Ok(Vec::new());
            }
            let mut world = doc.world.clone();
            world.push(name.clone());
            Ok(vec![LogicOp::SetGraph {
                description: None,
                seed: None,
                world: Some(world),
            }])
        }
        // A typed entity variable (v2): the key's type and the default the runtime seeds.
        VariableEdit::Set {
            scope: VarScope::Entity,
            name,
            ty,
            default,
        } => {
            check_name(name)?;
            let current = doc
                .entity_vars
                .iter()
                .find(|v| v.name == *name)
                .map(|v| v.default.clone());
            let value = typed_default(*ty, default.as_ref(), current.as_ref())?;
            Ok(vec![LogicOp::SetEntityVar {
                name: name.clone(),
                default: Some(value),
            }])
        }
        VariableEdit::Remove {
            scope: VarScope::Entity,
            name,
        } => {
            if doc.entity_vars.iter().any(|v| v.name == *name) {
                Ok(vec![LogicOp::SetEntityVar {
                    name: name.clone(),
                    default: None,
                }])
            } else {
                Err((
                    "unknown_variable",
                    format!("no declared entity variable {name:?} (a used key has no declaration to remove)"),
                ))
            }
        }
        VariableEdit::Remove {
            scope: VarScope::World,
            name,
        } if name.contains('/') => {
            let (board, key) = check_world_var(name)?;
            if doc.world_vars.iter().any(|v| v.board == board && v.name == key) {
                Ok(vec![LogicOp::SetWorldVar {
                    board: board.to_string(),
                    name: key.to_string(),
                    default: None,
                }])
            } else {
                Err(("unknown_variable", format!("no world variable {name:?}")))
            }
        }
        VariableEdit::Remove { scope, name } => match scope {
            VarScope::Local if local(name.as_str()).is_some() => Ok(vec![LogicOp::SetLocal {
                name: name.clone(),
                default: None,
            }]),
            VarScope::World if doc.world.contains(name) => Ok(vec![LogicOp::SetGraph {
                description: None,
                seed: None,
                world: Some(doc.world.iter().filter(|b| *b != name).cloned().collect()),
            }]),
            _ => Err((
                "unknown_variable",
                format!("no {} variable {name:?}", scope.name()),
            )),
        },
        // A world variable `<board>/<key>` renames its key (same board) and every reference.
        VariableEdit::Rename {
            scope: VarScope::World,
            from,
            to,
        } if from.contains('/') => {
            let (board, old_key) = check_world_var(from)?;
            let (to_board, new_key) = check_world_var(to)?;
            if to_board != board {
                return Err((
                    "bad_name",
                    format!("{to:?}: a world variable keeps its board ({board})"),
                ));
            }
            if from == to {
                return Ok(Vec::new());
            }
            let Some(var) = doc
                .world_vars
                .iter()
                .find(|v| v.board == board && v.name == old_key)
            else {
                return Err(("unknown_variable", format!("no world variable {from:?}")));
            };
            if doc
                .world_vars
                .iter()
                .any(|v| v.board == board && v.name == new_key)
            {
                return Err(("duplicate_name", format!("a world variable {to:?} exists")));
            }
            let mut ops = vec![
                LogicOp::SetWorldVar {
                    board: board.to_string(),
                    name: old_key.to_string(),
                    default: None,
                },
                LogicOp::SetWorldVar {
                    board: board.to_string(),
                    name: new_key.to_string(),
                    default: Some(var.default.clone()),
                },
            ];
            let (old_full, new_full) = (format!("world/{from}"), format!("world/{to}"));
            ops.extend(exact_key_ops(doc, &old_full, &new_full));
            Ok(ops)
        }
        VariableEdit::Rename { scope, from, to } => {
            check_name(to)?;
            if from == to {
                return Ok(Vec::new());
            }
            let mut ops = match scope {
                VarScope::Local => {
                    if local(from.as_str()).is_none() {
                        return Err(("unknown_variable", format!("no local variable {from:?}")));
                    }
                    if local(to.as_str()).is_some() {
                        return Err(("duplicate_name", format!("a local named {to:?} exists")));
                    }
                    redeclare(&doc.locals, |l| LocalDef {
                        name: if l.name == *from {
                            to.clone()
                        } else {
                            l.name.clone()
                        },
                        default: l.default.clone(),
                    })
                }
                VarScope::World => {
                    if !doc.world.contains(from) {
                        return Err(("unknown_variable", format!("no world board {from:?}")));
                    }
                    if doc.world.contains(to) {
                        return Err(("duplicate_name", format!("a world board named {to:?} exists")));
                    }
                    let mut ops = vec![LogicOp::SetGraph {
                        description: None,
                        seed: None,
                        world: Some(
                            doc.world
                                .iter()
                                .map(|b| if b == from { to.clone() } else { b.clone() })
                                .collect(),
                        ),
                    }];
                    // The board's typed variables (v2) move with it.
                    for v in doc.world_vars.iter().filter(|v| v.board == *from) {
                        ops.push(LogicOp::SetWorldVar {
                            board: from.clone(),
                            name: v.name.clone(),
                            default: None,
                        });
                        ops.push(LogicOp::SetWorldVar {
                            board: to.clone(),
                            name: v.name.clone(),
                            default: Some(v.default.clone()),
                        });
                    }
                    ops
                }
                VarScope::Entity => {
                    let used = rows(doc)
                        .iter()
                        .any(|r| r.scope == VarScope::Entity && r.name == *from);
                    if !used {
                        return Err((
                            "unknown_variable",
                            format!("the graph uses no entity key {from:?}"),
                        ));
                    }
                    // A declared entity variable (v2) is renamed in place (its order kept).
                    if doc.entity_vars.iter().any(|v| v.name == *to) {
                        return Err(("duplicate_name", format!("an entity variable {to:?} exists")));
                    }
                    if doc.entity_vars.iter().any(|v| v.name == *from) {
                        let mut ops: Vec<LogicOp> = doc
                            .entity_vars
                            .iter()
                            .map(|v| LogicOp::SetEntityVar {
                                name: v.name.clone(),
                                default: None,
                            })
                            .collect();
                        ops.extend(doc.entity_vars.iter().map(|v| LogicOp::SetEntityVar {
                            name: if v.name == *from {
                                to.clone()
                            } else {
                                v.name.clone()
                            },
                            default: Some(v.default.clone()),
                        }));
                        ops
                    } else {
                        Vec::new()
                    }
                }
            };
            ops.extend(reference_ops(doc, *scope, from, to));
            Ok(ops)
        }
    }
}

/// Applies one [`VariableEdit`] to the open graph as one undo step.
///
/// # Errors
///
/// The codes of [`ops_for`], `no_graph_open`, or the code of the op that did not fit.
pub fn apply(world: &mut regia_ecs::prelude::World, edit: &VariableEdit) -> Result<(), VarError> {
    let doc = match world.get_resource::<super::LogicEditor>() {
        Some(e) if e.is_open() => e.doc.clone(),
        _ => return Err(("no_graph_open", "no logic graph is open".into())),
    };
    let ops = ops_for(&doc, edit)?;
    if ops.is_empty() {
        return Ok(());
    }
    super::edit(world, &edit.label(), None, &ops)
        .map(|_| ())
        .map_err(|e| (e.error.code, format!("op {}: {}", e.op_index, e.error.message)))
}
