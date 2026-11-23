// SPDX-License-Identifier: MPL-2.0
//! Editing and checking `.statetree.ron` state trees without a running world (`docs/specs/38-state-trees.md`,
//! ADR-3342): structural operations on a [`StateTreeDef`] ([`StOp`], [`apply_ops`]) that keep every
//! name reference consistent (a rename updates the root, child lists and every transition target), a
//! validator that reports every problem with a path and a stable code ([`validate`]; the compiler
//! stops at the first), and a canonical text writer ([`to_canonical_ron`]).
//!
//! The state-tree editor panel (`regia_editor::state_tree_editor`) and the `ai.statetree_*` file
//! methods both use this module, so a tree edited in the panel and one edited over the protocol go
//! through the same code. The `.statetree.ron` shape is unchanged (format version 2): this module
//! only reads and writes the existing [`StateTreeDef`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::Deserialize;

use crate::state_tree::{
    Condition, Fallback, IncludeDef, Literal, MAX_CONDITION_DEPTH, MAX_INCLUDE_DEPTH, MAX_STATES,
    STATE_TREE_FORMAT_VERSION, STATE_TREE_MIN_FORMAT_VERSION, Selection, StateDef, StateTree, StateTreeDef,
    Target, TransitionDef, Trigger, UtilityDef,
};
use crate::tree::BtTaskRegistry;

/// How bad a [`StIssue`] is. Errors stop a tree from compiling (and from being saved by the editor
/// and `ai.statetree_edit`); warnings do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StSeverity {
    /// The tree would not compile.
    Error,
    /// The tree compiles but something is probably a mistake.
    Warning,
}

/// One problem found in a tree: a stable lowercase code, a path to the offending field and a
/// sentence.
///
/// Paths name states by name: `root`, `states/Idle`, `states/Idle/children/1`,
/// `states/Idle/transitions/0/target`, `states/Idle/tasks/2`, `ambient/0`, `includes/1`. A state
/// with an empty name is `states/#<index>`.
///
/// Error codes: `unsupported_version`, `empty_tree`, `too_many_states`, `empty_name`,
/// `duplicate_name`, `missing_root`, `missing_child`, `missing_target`, `multiple_parents`,
/// `unreachable_state`, `empty_task`, `unknown_task`, `no_next_sibling`, `parent_of_root`,
/// `hold_on_event`, `bad_value`, `condition_too_deep`, `bad_include`, `include_missing`,
/// `include_invalid`, `include_cycle`, `include_depth`, and, for anything the compiler refuses
/// that the checks above missed, the compiler's own reason. Warning codes: `empty_selection`
/// (a `Utility` or `Weighted` state with no children).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StIssue {
    /// The stable code.
    pub code: &'static str,
    /// Error or warning.
    pub severity: StSeverity,
    /// Where the problem is (see the type docs for the shape).
    pub path: String,
    /// A sentence for people.
    pub message: String,
}

impl StIssue {
    fn error(code: &'static str, path: impl Into<String>, message: String) -> Self {
        Self {
            code,
            severity: StSeverity::Error,
            path: path.into(),
            message,
        }
    }

    fn warning(code: &'static str, path: impl Into<String>, message: String) -> Self {
        Self {
            code,
            severity: StSeverity::Warning,
            path: path.into(),
            message,
        }
    }

    /// The state the issue is about, when its path names one (`states/<name>/...`).
    #[must_use]
    pub fn state(&self) -> Option<&str> {
        let rest = self.path.strip_prefix("states/")?;
        let name = state_of_path(rest);
        (!name.starts_with('#')).then_some(name)
    }
}

/// The state-name part of a path below `states/`: everything up to the first `/children/`,
/// `/transitions/`, `/tasks/` or field suffix (a state name may itself contain `/`, as included
/// states do).
fn state_of_path(rest: &str) -> &str {
    const SUFFIXES: [&str; 8] = [
        "/children/",
        "/transitions/",
        "/tasks/",
        "/enter_set/",
        "/enter",
        "/weight",
        "/utility",
        "/name",
    ];
    let cut = SUFFIXES
        .iter()
        .filter_map(|s| rest.find(s))
        .min()
        .unwrap_or(rest.len());
    &rest[..cut]
}

/// Whether `issues` holds no error (warnings are allowed).
#[must_use]
pub fn is_valid(issues: &[StIssue]) -> bool {
    issues.iter().all(|i| i.severity != StSeverity::Error)
}

/// Reads an included file: project-relative path to RON text (`None` when it cannot be read).
pub type IncludeLoader<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// What [`validate`] checks against. Every field is optional: without a registry task paths are
/// not checked (and the final compile pass is skipped); without a loader includes are not
/// followed.
#[derive(Clone, Copy, Default)]
pub struct ValidateOptions<'a> {
    /// The registered tasks (`unknown_task`).
    pub registry: Option<&'a BtTaskRegistry>,
    /// Project-relative path to RON text, for `includes`.
    pub load: Option<&'a IncludeLoader<'a>>,
    /// The project-relative path of the definition itself, so a tree that includes itself is an
    /// `include_cycle`.
    pub self_path: Option<&'a str>,
}

fn state_path(def: &StateTreeDef, index: usize) -> String {
    let name = &def.states[index].name;
    if name.is_empty() {
        format!("states/#{index}")
    } else {
        format!("states/{name}")
    }
}

/// Checks `def` and lists every problem (the compiler would stop at the first). Never panics on a
/// malformed definition.
#[must_use]
pub fn validate(def: &StateTreeDef, options: ValidateOptions<'_>) -> Vec<StIssue> {
    let _span = tracing::info_span!(target: "regia_ai", "statetree_validate").entered();
    let mut issues = Vec::new();
    if !(STATE_TREE_MIN_FORMAT_VERSION..=STATE_TREE_FORMAT_VERSION).contains(&def.format_version) {
        issues.push(StIssue::error(
            "unsupported_version",
            "format_version",
            format!(
                "format_version {} is not one this build reads ({STATE_TREE_MIN_FORMAT_VERSION} to {STATE_TREE_FORMAT_VERSION})",
                def.format_version
            ),
        ));
    }
    // Includes first: when every one resolves, the merged tree is what the remaining checks see
    // (as the compiler does); otherwise references into an unresolved include are not reported.
    let include_ok = include_issues(def, options, &mut issues);
    let merged;
    let (checked, unresolved_prefixes): (&StateTreeDef, Vec<String>) = if def.includes.is_empty() {
        (def, Vec::new())
    } else if include_ok && let Some(load) = options.load {
        match def.expand_includes(load) {
            Ok(m) => {
                merged = m;
                (&merged, Vec::new())
            }
            Err(e) => {
                issues.push(StIssue::error(
                    "include_invalid",
                    "includes",
                    format!("the includes could not be merged: {e}"),
                ));
                (
                    def,
                    def.includes.iter().map(|i| format!("{}/", i.prefix)).collect(),
                )
            }
        }
    } else {
        (
            def,
            def.includes.iter().map(|i| format!("{}/", i.prefix)).collect(),
        )
    };
    structure_issues(checked, options.registry, &unresolved_prefixes, &mut issues);
    ambient_issues(def, options.registry, &mut issues);
    // A last pass through the compiler catches anything the checks above do not know about; it
    // needs the registry (otherwise every task would be unknown).
    if is_valid(&issues)
        && let Some(registry) = options.registry
    {
        let none = |_: &str| None::<String>;
        let load: &dyn Fn(&str) -> Option<String> = match options.load {
            Some(load) => load,
            None => &none,
        };
        if let Err(err) = StateTree::compile_with(def, registry, load) {
            let path = err
                .state()
                .map_or_else(|| "states".to_string(), |s| format!("states/{s}"));
            issues.push(StIssue::error(err.reason(), path, err.to_string()));
        }
    }
    issues
}

/// Include checks, following each include through the loader. Returns whether every include
/// resolved (so the merged tree can be checked).
fn include_issues(def: &StateTreeDef, options: ValidateOptions<'_>, issues: &mut Vec<StIssue>) -> bool {
    let mut ok = true;
    let mut prefixes = BTreeSet::new();
    for (i, include) in def.includes.iter().enumerate() {
        let path = format!("includes/{i}");
        if include.prefix.is_empty() || include.path.is_empty() || include.prefix.contains('/') {
            issues.push(StIssue::error(
                "bad_include",
                &path,
                format!(
                    "include {i} needs a non-empty prefix without `/` and a path (prefix {:?}, path {:?})",
                    include.prefix, include.path
                ),
            ));
            ok = false;
            continue;
        }
        if !prefixes.insert(include.prefix.as_str()) {
            issues.push(StIssue::error(
                "bad_include",
                &path,
                format!("the prefix {:?} is used by two includes", include.prefix),
            ));
            ok = false;
        }
    }
    let Some(load) = options.load else {
        return ok && def.includes.is_empty();
    };
    let mut stack: Vec<String> = options.self_path.map(str::to_string).into_iter().collect();
    for (i, include) in def.includes.iter().enumerate() {
        if include.path.is_empty() {
            continue;
        }
        if let Err((code, message)) = follow_include(include, load, &mut stack, 1) {
            issues.push(StIssue::error(code, format!("includes/{i}"), message));
            ok = false;
        }
    }
    ok
}

/// Follows one include and everything it includes. `depth` counts the include being followed.
fn follow_include(
    include: &IncludeDef,
    load: &dyn Fn(&str) -> Option<String>,
    stack: &mut Vec<String>,
    depth: usize,
) -> Result<(), (&'static str, String)> {
    if stack.contains(&include.path) {
        let mut chain = stack.clone();
        chain.push(include.path.clone());
        return Err((
            "include_cycle",
            format!(
                "{:?} is part of an include cycle ({})",
                include.path,
                chain.join(" -> ")
            ),
        ));
    }
    if depth > MAX_INCLUDE_DEPTH {
        return Err((
            "include_depth",
            format!(
                "includes nest deeper than {MAX_INCLUDE_DEPTH} at {:?}",
                include.path
            ),
        ));
    }
    let text = load(&include.path).ok_or_else(|| {
        (
            "include_missing",
            format!("the included file {:?} could not be read", include.path),
        )
    })?;
    let inner = StateTreeDef::from_ron(&text).map_err(|e| {
        (
            "include_invalid",
            format!("the included file {:?}: {e}", include.path),
        )
    })?;
    stack.push(include.path.clone());
    let result = inner
        .includes
        .iter()
        .try_for_each(|next| follow_include(next, load, stack, depth + 1));
    stack.pop();
    result
}

fn condition_issues(c: &Condition, path: &str, issues: &mut Vec<StIssue>) {
    if c.depth() > MAX_CONDITION_DEPTH {
        issues.push(StIssue::error(
            "condition_too_deep",
            path,
            format!("the condition nests deeper than {MAX_CONDITION_DEPTH}"),
        ));
    }
    if !condition_finite(c) {
        issues.push(StIssue::error(
            "bad_value",
            path,
            "the condition compares against a non-finite number".into(),
        ));
    }
}

fn condition_finite(c: &Condition) -> bool {
    match c {
        Condition::Compare { value, .. } => value.is_finite(),
        Condition::Not(inner) => condition_finite(inner),
        Condition::All(items) | Condition::Any(items) => items.iter().all(condition_finite),
        _ => true,
    }
}

fn task_issue(task: &str, path: String, registry: Option<&BtTaskRegistry>, issues: &mut Vec<StIssue>) {
    if task.is_empty() {
        issues.push(StIssue::error("empty_task", path, "a task path is empty".into()));
    } else if let Some(registry) = registry
        && registry.construct(task).is_none()
    {
        issues.push(StIssue::error(
            "unknown_task",
            path,
            format!("the task {task:?} is not registered"),
        ));
    }
}

fn ambient_issues(def: &StateTreeDef, registry: Option<&BtTaskRegistry>, issues: &mut Vec<StIssue>) {
    for (i, task) in def.ambient.iter().enumerate() {
        task_issue(task, format!("ambient/{i}"), registry, issues);
    }
}

/// Names, the hierarchy, reachability, transitions, tasks and values.
fn structure_issues(
    def: &StateTreeDef,
    registry: Option<&BtTaskRegistry>,
    unresolved: &[String],
    issues: &mut Vec<StIssue>,
) {
    if def.states.is_empty() {
        issues.push(StIssue::error(
            "empty_tree",
            "states",
            "the tree has no states; add a root state".into(),
        ));
        return;
    }
    if def.states.len() > MAX_STATES {
        issues.push(StIssue::error(
            "too_many_states",
            "states",
            format!("{} states (the limit is {MAX_STATES})", def.states.len()),
        ));
    }
    let skip = |name: &str| unresolved.iter().any(|p| name.starts_with(p.as_str()));
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, s) in def.states.iter().enumerate() {
        if s.name.is_empty() {
            issues.push(StIssue::error(
                "empty_name",
                format!("states/#{i}/name"),
                format!("state {i} has an empty name"),
            ));
        } else if index.insert(s.name.as_str(), i).is_some() {
            issues.push(StIssue::error(
                "duplicate_name",
                format!("states/{}", s.name),
                format!("the state {:?} is declared twice", s.name),
            ));
        }
    }
    let root = index.get(def.root.as_str()).copied();
    if root.is_none() && !skip(&def.root) {
        issues.push(StIssue::error(
            "missing_root",
            "root",
            format!("the root {:?} names no state", def.root),
        ));
    }
    let count = def.states.len();
    let mut parents: Vec<Option<usize>> = vec![None; count];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (i, s) in def.states.iter().enumerate() {
        for (k, child) in s.children.iter().enumerate() {
            let Some(&c) = index.get(child.as_str()) else {
                if !skip(child) {
                    issues.push(StIssue::error(
                        "missing_child",
                        format!("{}/children/{k}", state_path(def, i)),
                        format!("the child {child:?} names no state"),
                    ));
                }
                continue;
            };
            if Some(c) == root || parents[c].is_some() {
                issues.push(StIssue::error(
                    "multiple_parents",
                    format!("states/{child}"),
                    if Some(c) == root {
                        format!("the root {child:?} is listed as a child of {:?}", s.name)
                    } else {
                        format!("the state {child:?} is listed under more than one parent")
                    },
                ));
                continue;
            }
            parents[c] = Some(i);
            children[i].push(c);
        }
    }
    if let Some(root) = root {
        let mut reached = vec![false; count];
        let mut stack = vec![root];
        while let Some(s) = stack.pop() {
            if std::mem::replace(&mut reached[s], true) {
                continue;
            }
            stack.extend(children[s].iter().copied());
        }
        for (i, seen) in reached.iter().enumerate() {
            // Duplicates and empty names are reported already; the second copy is never reached.
            let named_once =
                !def.states[i].name.is_empty() && index.get(def.states[i].name.as_str()) == Some(&i);
            if !seen && named_once {
                issues.push(StIssue::error(
                    "unreachable_state",
                    state_path(def, i),
                    format!(
                        "the state {:?} cannot be reached from the root",
                        def.states[i].name
                    ),
                ));
            }
        }
    }
    for (i, s) in def.states.iter().enumerate() {
        let sp = state_path(def, i);
        condition_issues(&s.enter, &format!("{sp}/enter"), issues);
        if !s.weight.is_finite() {
            issues.push(StIssue::error(
                "bad_value",
                format!("{sp}/weight"),
                "the weight is not finite".into(),
            ));
        }
        if let Some(u) = &s.utility
            && !(u.scale.is_finite() && u.bias.is_finite())
        {
            issues.push(StIssue::error(
                "bad_value",
                format!("{sp}/utility"),
                "the utility scale or bias is not finite".into(),
            ));
        }
        for (key, literal) in &s.enter_set {
            if key.is_empty() || matches!(literal, Literal::Float(v) if !v.is_finite()) {
                issues.push(StIssue::error(
                    "bad_value",
                    format!("{sp}/enter_set/{key}"),
                    "an enter_set key is empty or its float is not finite".into(),
                ));
            }
        }
        for (k, task) in s.tasks.iter().enumerate() {
            task_issue(task, format!("{sp}/tasks/{k}"), registry, issues);
        }
        if matches!(s.selection, Selection::Utility | Selection::Weighted) && s.children.is_empty() {
            issues.push(StIssue::warning(
                "empty_selection",
                sp.clone(),
                format!(
                    "the state {:?} selects among children by {:?} but has none",
                    s.name, s.selection
                ),
            ));
        }
        let has_next = parents[i].is_some_and(|p| children[p].last() != Some(&i));
        for (k, t) in s.transitions.iter().enumerate() {
            let tp = format!("{sp}/transitions/{k}");
            condition_issues(&t.condition, &format!("{tp}/condition"), issues);
            if matches!(&t.trigger, Trigger::Event(e) if e.is_empty()) {
                issues.push(StIssue::error(
                    "bad_value",
                    format!("{tp}/trigger"),
                    "the event name is empty".into(),
                ));
            }
            if !t.hold.is_finite() || t.hold < 0.0 {
                issues.push(StIssue::error(
                    "bad_value",
                    format!("{tp}/hold"),
                    "hold must be a finite number of seconds, 0 or more".into(),
                ));
            } else if t.hold > 0.0 && matches!(t.trigger, Trigger::Event(_)) {
                issues.push(StIssue::error(
                    "hold_on_event",
                    format!("{tp}/hold"),
                    "an Event transition cannot have a hold".into(),
                ));
            }
            match &t.target {
                Target::State(name) if !index.contains_key(name.as_str()) && !skip(name) => {
                    issues.push(StIssue::error(
                        "missing_target",
                        format!("{tp}/target"),
                        format!("the target {name:?} names no state"),
                    ));
                }
                Target::Next | Target::NextEnterable if !has_next && !skip(&s.name) => {
                    issues.push(StIssue::error(
                        "no_next_sibling",
                        format!("{tp}/target"),
                        format!("{:?} has no next sibling to go to", s.name),
                    ));
                }
                Target::Parent if Some(i) == root => {
                    issues.push(StIssue::error(
                        "parent_of_root",
                        format!("{tp}/target"),
                        "the root has no parent".into(),
                    ));
                }
                _ => {}
            }
        }
    }
}

/// Parses `.statetree.ron` text (a version 1 file reads as version 2).
///
/// # Errors
///
/// The text is not a valid `.statetree.ron` (RON syntax, shape or version).
pub fn parse_statetree_ron(text: &str) -> Result<StateTreeDef, String> {
    StateTreeDef::from_ron(text).map_err(|e| e.to_string())
}

fn ron_of<T: serde::Serialize>(value: &T) -> Result<String, StEditError> {
    ron::ser::to_string(value).map_err(|e| StEditError::new("bad_value", e.to_string()))
}

fn string_list(items: &[String]) -> Result<String, StEditError> {
    let parts = items.iter().map(ron_of).collect::<Result<Vec<_>, _>>()?;
    Ok(format!("[{}]", parts.join(", ")))
}

fn finite(value: f32, what: &str) -> Result<f32, StEditError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(StEditError::new("bad_value", format!("{what} is not finite")))
    }
}

/// Writes `def` in the canonical `.statetree.ron` layout: the tree fields, then one block per state
/// in declaration order with one field per line and one line per transition. Fields that hold
/// their default are left out. The same definition always gives the same text, and
/// [`parse_statetree_ron`] reads it back to an equal definition.
///
/// # Errors
///
/// A number is not finite (RON cannot hold it): code `bad_value`.
pub fn to_canonical_ron(def: &StateTreeDef) -> Result<String, StEditError> {
    let mut out = String::new();
    let _ = writeln!(out, "(\n    format_version: {STATE_TREE_FORMAT_VERSION},");
    if !def.description.is_empty() {
        let _ = writeln!(out, "    description: {},", ron_of(&def.description)?);
    }
    let _ = writeln!(out, "    root: {},", ron_of(&def.root)?);
    if def.seed != 0 {
        let _ = writeln!(out, "    seed: {},", def.seed);
    }
    if !def.state_key.is_empty() {
        let _ = writeln!(out, "    state_key: {},", ron_of(&def.state_key)?);
    }
    if !def.ambient.is_empty() {
        let _ = writeln!(out, "    ambient: {},", string_list(&def.ambient)?);
    }
    if !def.includes.is_empty() {
        out.push_str("    includes: [\n");
        for include in &def.includes {
            let _ = writeln!(
                out,
                "        (prefix: {}, path: {}),",
                ron_of(&include.prefix)?,
                ron_of(&include.path)?
            );
        }
        out.push_str("    ],\n");
    }
    out.push_str("    states: [\n");
    for state in &def.states {
        write_state(&mut out, state)?;
    }
    out.push_str("    ],\n)\n");
    Ok(out)
}

fn write_state(out: &mut String, s: &StateDef) -> Result<(), StEditError> {
    out.push_str("        (\n");
    let _ = writeln!(out, "            name: {},", ron_of(&s.name)?);
    if !s.children.is_empty() {
        let _ = writeln!(out, "            children: {},", string_list(&s.children)?);
    }
    if s.selection != Selection::First {
        let _ = writeln!(out, "            selection: {},", ron_of(&s.selection)?);
    }
    if s.enter != Condition::Always {
        if !condition_finite(&s.enter) {
            return Err(StEditError::new(
                "bad_value",
                format!("{:?}: the enter condition is not finite", s.name),
            ));
        }
        let _ = writeln!(out, "            enter: {},", ron_of(&s.enter)?);
    }
    if !s.tasks.is_empty() {
        let _ = writeln!(out, "            tasks: {},", string_list(&s.tasks)?);
    }
    if !s.enter_set.is_empty() {
        let mut parts = Vec::with_capacity(s.enter_set.len());
        for (key, literal) in &s.enter_set {
            if let Literal::Float(v) = literal {
                finite(*v, "an enter_set float")?;
            }
            parts.push(format!("{}: {}", ron_of(key)?, ron_of(literal)?));
        }
        let _ = writeln!(out, "            enter_set: {{{}}},", parts.join(", "));
    }
    if s.weight != 1.0 {
        let _ = writeln!(
            out,
            "            weight: {},",
            ron_of(&finite(s.weight, "the weight")?)?
        );
    }
    if let Some(u) = &s.utility {
        let _ = writeln!(
            out,
            "            utility: Some((key: {}, scale: {}, bias: {})),",
            ron_of(&u.key)?,
            ron_of(&finite(u.scale, "the utility scale")?)?,
            ron_of(&finite(u.bias, "the utility bias")?)?
        );
    }
    if s.fallback != Fallback::Stay {
        let _ = writeln!(out, "            fallback: {},", ron_of(&s.fallback)?);
    }
    if !s.transitions.is_empty() {
        out.push_str("            transitions: [\n");
        for t in &s.transitions {
            let mut line = format!("(trigger: {}", ron_of(&t.trigger)?);
            if t.condition != Condition::Always {
                if !condition_finite(&t.condition) {
                    return Err(StEditError::new(
                        "bad_value",
                        format!("{:?}: a transition condition is not finite", s.name),
                    ));
                }
                let _ = write!(line, ", condition: {}", ron_of(&t.condition)?);
            }
            if t.priority != 0 {
                let _ = write!(line, ", priority: {}", t.priority);
            }
            if t.hold != 0.0 {
                let _ = write!(line, ", hold: {}", ron_of(&finite(t.hold, "a hold")?)?);
            }
            let _ = write!(line, ", target: {})", ron_of(&t.target)?);
            let _ = writeln!(out, "                {line},");
        }
        out.push_str("            ],\n");
    }
    out.push_str("        ),\n");
    Ok(())
}

/// A failed edit: a stable lowercase code plus a sentence.
///
/// Codes: `unknown_state`, `duplicate_name`, `empty_name`, `would_cycle`, `is_root`,
/// `cannot_remove_root`, `has_parent`, `bad_index`, `bad_value`, `unknown_include`,
/// `duplicate_prefix`, `too_many_states`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StEditError {
    /// The stable code.
    pub code: &'static str,
    /// A sentence for people.
    pub message: String,
}

impl StEditError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for StEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for StEditError {}

/// One structural edit of a state tree. Reads from JSON as `{ "op": "add_state", "name": "Idle",
/// "parent": "Root" }` (`snake_case` op names). Enum fields use the asset's spelling: `"trigger":
/// "Tick"` or `{ "Event": "heard" }`, `"target": { "State": "Alert" }`, `"selection": "Utility"`,
/// `"value": { "Float": 1.5 }`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum StOp {
    /// Adds a state. With a `parent` it is listed there (at `index`, default last); with no parent
    /// and an empty tree it becomes the root; otherwise it is left without a parent (and the
    /// validator reports it unreachable until it gets one).
    AddState {
        /// The new state's name (unique, non-empty).
        name: String,
        /// List it under this state.
        #[serde(default)]
        parent: Option<String>,
        /// Position among the parent's children.
        #[serde(default)]
        index: Option<usize>,
    },
    /// Removes a state. With `subtree` its descendants go too; otherwise its children take its
    /// place in its parent's list. Transitions anywhere that target a removed state are removed.
    /// The root can only be removed when it has exactly one child and `subtree` is false (the
    /// child becomes the root).
    RemoveState {
        /// The state.
        name: String,
        /// Remove the descendants as well.
        #[serde(default)]
        subtree: bool,
    },
    /// Renames a state and every reference to it: the root, its parent's child list and every
    /// `State` transition target.
    RenameState {
        /// The current name.
        name: String,
        /// The new name (unique, non-empty).
        to: String,
    },
    /// Moves a state under another parent (or within the same one), at `index` of the parent's
    /// list after the state was taken out of its old place (default last). `name` may also be a
    /// state of an include (`<prefix>/<name>`, such as the included root): that lists it under
    /// `parent`.
    ReparentState {
        /// The state.
        name: String,
        /// The new parent.
        parent: String,
        /// Position among the new parent's children.
        #[serde(default)]
        index: Option<usize>,
    },
    /// Makes a state with no parent the root.
    SetRoot {
        /// The state.
        name: String,
    },
    /// Changes a state's selection, enter condition, weight, utility or fallback; absent fields
    /// stay.
    SetState {
        /// The state.
        name: String,
        /// How it picks a child.
        #[serde(default)]
        selection: Option<Selection>,
        /// The enter condition.
        #[serde(default)]
        enter: Option<Condition>,
        /// The weight under a `Weighted` parent.
        #[serde(default)]
        weight: Option<f32>,
        /// The score under a `Utility` parent.
        #[serde(default)]
        utility: Option<UtilityDef>,
        /// Remove the utility score.
        #[serde(default)]
        clear_utility: bool,
        /// What a transition into it does when nothing below can be entered.
        #[serde(default)]
        fallback: Option<Fallback>,
    },
    /// Replaces the tasks a state runs while active (registered task paths; each task's own
    /// enter, tick and exit run with the state).
    SetTasks {
        /// The state.
        name: String,
        /// The task paths, in tick order.
        tasks: Vec<String>,
    },
    /// Replaces the tree's ambient tasks (ticked first on every tick).
    SetAmbient {
        /// The task paths, in tick order.
        tasks: Vec<String>,
    },
    /// Sets (or, with no `value`, removes) one blackboard literal the state writes on entry (its
    /// parameters, spec 38 § 3).
    SetParam {
        /// The state.
        state: String,
        /// The blackboard key (non-empty).
        key: String,
        /// The value; absent removes the key.
        #[serde(default)]
        value: Option<Literal>,
    },
    /// Adds a transition to a state (at `index` of its list, default last).
    AddTransition {
        /// The owning state.
        state: String,
        /// What triggers it.
        trigger: Trigger,
        /// Its guard (default always).
        #[serde(default)]
        condition: Option<Condition>,
        /// Where it goes.
        target: Target,
        /// Its priority (default 0).
        #[serde(default)]
        priority: Option<i32>,
        /// Seconds the trigger and condition must hold (default 0).
        #[serde(default)]
        hold: Option<f32>,
        /// Position in the state's list.
        #[serde(default)]
        index: Option<usize>,
    },
    /// Removes a transition.
    RemoveTransition {
        /// The owning state.
        state: String,
        /// Its position in the state's list.
        index: usize,
    },
    /// Changes a transition; absent fields stay.
    SetTransition {
        /// The owning state.
        state: String,
        /// Its position in the state's list.
        index: usize,
        /// New trigger.
        #[serde(default)]
        trigger: Option<Trigger>,
        /// New guard.
        #[serde(default)]
        condition: Option<Condition>,
        /// New target.
        #[serde(default)]
        target: Option<Target>,
        /// New priority.
        #[serde(default)]
        priority: Option<i32>,
        /// New hold.
        #[serde(default)]
        hold: Option<f32>,
    },
    /// Moves a transition within its state's list (declaration order breaks priority ties).
    MoveTransition {
        /// The owning state.
        state: String,
        /// Its current position.
        from: usize,
        /// Its new position.
        to: usize,
    },
    /// Adds an include (at `index` of the list, default last).
    AddInclude {
        /// The prefix its state names get (non-empty, no `/`, unique).
        prefix: String,
        /// The project-relative path of the included `.statetree.ron`.
        path: String,
        /// Position in the list.
        #[serde(default)]
        index: Option<usize>,
    },
    /// Removes the include with this prefix, and every child-list entry and `State` target that
    /// names one of its states.
    RemoveInclude {
        /// Its prefix.
        prefix: String,
    },
    /// Changes the include with this prefix; absent fields stay. A new prefix is carried into
    /// every reference to the include's states.
    SetInclude {
        /// Its current prefix.
        prefix: String,
        /// New prefix.
        #[serde(default)]
        to_prefix: Option<String>,
        /// New path.
        #[serde(default)]
        path: Option<String>,
    },
    /// Changes the tree's description, seed or state key; absent fields stay.
    SetTree {
        /// What the tree is for.
        #[serde(default)]
        description: Option<String>,
        /// The `Weighted` selection seed.
        #[serde(default)]
        seed: Option<u64>,
        /// The blackboard key the active leaf's name is written to (empty: none).
        #[serde(default)]
        state_key: Option<String>,
    },
}

fn find(def: &StateTreeDef, name: &str) -> Result<usize, StEditError> {
    def.states
        .iter()
        .position(|s| s.name == name)
        .ok_or_else(|| StEditError::new("unknown_state", format!("no state is named {name:?}")))
}

fn parent_of(def: &StateTreeDef, name: &str) -> Option<usize> {
    def.states
        .iter()
        .position(|s| s.children.iter().any(|c| c == name))
}

fn check_new_name(def: &StateTreeDef, name: &str) -> Result<(), StEditError> {
    if name.is_empty() {
        return Err(StEditError::new("empty_name", "a state name cannot be empty"));
    }
    if def.states.iter().any(|s| s.name == name) {
        return Err(StEditError::new(
            "duplicate_name",
            format!("a state is already named {name:?}"),
        ));
    }
    Ok(())
}

/// Whether `node` is `ancestor` or below it (following child names; tolerant of bad documents).
fn is_within(def: &StateTreeDef, ancestor: &str, node: &str) -> bool {
    let mut stack = vec![ancestor.to_string()];
    let mut seen = BTreeSet::new();
    while let Some(name) = stack.pop() {
        if name == node {
            return true;
        }
        if seen.insert(name.clone())
            && let Some(state) = def.states.iter().find(|s| s.name == name)
        {
            stack.extend(state.children.iter().cloned());
        }
    }
    false
}

fn insert_at<T>(list: &mut Vec<T>, index: Option<usize>, item: T) -> Result<(), StEditError> {
    let len = list.len();
    let at = index.unwrap_or(len);
    if at > len {
        return Err(StEditError::new(
            "bad_index",
            format!("index {at} is past the {len} entries"),
        ));
    }
    list.insert(at, item);
    Ok(())
}

fn transition_mut<'a>(
    def: &'a mut StateTreeDef,
    state: &str,
    index: usize,
) -> Result<&'a mut TransitionDef, StEditError> {
    let s = find(def, state)?;
    let len = def.states[s].transitions.len();
    def.states[s].transitions.get_mut(index).ok_or_else(|| {
        StEditError::new(
            "bad_index",
            format!("{state:?} has {len} transitions; there is no transition {index}"),
        )
    })
}

fn check_hold(hold: Option<f32>) -> Result<(), StEditError> {
    match hold {
        Some(h) if !h.is_finite() || h < 0.0 => Err(StEditError::new(
            "bad_value",
            "hold must be a finite number of seconds, 0 or more",
        )),
        _ => Ok(()),
    }
}

fn check_condition(c: Option<&Condition>) -> Result<(), StEditError> {
    match c {
        Some(c) if !condition_finite(c) => Err(StEditError::new(
            "bad_value",
            "the condition compares against a non-finite number",
        )),
        _ => Ok(()),
    }
}

fn include_index(def: &StateTreeDef, prefix: &str) -> Result<usize, StEditError> {
    def.includes
        .iter()
        .position(|i| i.prefix == prefix)
        .ok_or_else(|| StEditError::new("unknown_include", format!("no include has the prefix {prefix:?}")))
}

/// Whether `name` is a state of one of the tree's includes (`<prefix>/...`).
fn is_included_name(def: &StateTreeDef, name: &str) -> bool {
    def.includes.iter().any(|i| {
        name.strip_prefix(i.prefix.as_str())
            .is_some_and(|rest| rest.len() > 1 && rest.starts_with('/'))
    })
}

/// Rewrites every reference to an included state (`old/...` to `new/...`): child lists, `State`
/// targets and the root.
fn rename_prefix(def: &mut StateTreeDef, old: &str, new: &str) {
    let lead = format!("{old}/");
    let swap = |name: &mut String| {
        if let Some(rest) = name.strip_prefix(&lead) {
            *name = format!("{new}/{rest}");
        }
    };
    swap(&mut def.root);
    for s in &mut def.states {
        s.children.iter_mut().for_each(swap);
        for t in &mut s.transitions {
            if let Target::State(target) = &mut t.target {
                swap(target);
            }
        }
    }
}

fn check_prefix(def: &StateTreeDef, prefix: &str) -> Result<(), StEditError> {
    if prefix.is_empty() || prefix.contains('/') {
        return Err(StEditError::new(
            "bad_value",
            "an include prefix must be non-empty and contain no `/`",
        ));
    }
    if def.includes.iter().any(|i| i.prefix == prefix) {
        return Err(StEditError::new(
            "duplicate_prefix",
            format!("an include already has the prefix {prefix:?}"),
        ));
    }
    Ok(())
}

/// Applies one operation.
///
/// # Errors
///
/// The operation does not fit the tree; the tree is left as it was.
pub fn apply_op(def: &mut StateTreeDef, op: &StOp) -> Result<(), StEditError> {
    let backup = def.clone();
    let result = apply_op_inner(def, op);
    if result.is_err() {
        *def = backup;
    }
    result
}

fn apply_op_inner(def: &mut StateTreeDef, op: &StOp) -> Result<(), StEditError> {
    match op {
        StOp::AddState { name, parent, index } => {
            if def.states.len() >= MAX_STATES {
                return Err(StEditError::new(
                    "too_many_states",
                    format!("a tree holds at most {MAX_STATES} states"),
                ));
            }
            check_new_name(def, name)?;
            match parent {
                Some(p) => {
                    let p = find(def, p)?;
                    insert_at(&mut def.states[p].children, *index, name.clone())?;
                }
                None if def.states.is_empty() => def.root.clone_from(name),
                None => {}
            }
            def.states.push(StateDef::new(name.clone()));
        }
        StOp::RemoveState { name, subtree } => {
            let at = find(def, name)?;
            let parent = parent_of(def, name);
            let mut doomed = BTreeSet::from([name.clone()]);
            if *subtree {
                let mut stack = def.states[at].children.clone();
                while let Some(child) = stack.pop() {
                    if doomed.insert(child.clone())
                        && let Some(s) = def.states.iter().find(|s| s.name == child)
                    {
                        stack.extend(s.children.iter().cloned());
                    }
                }
            }
            if *name == def.root {
                let children = &def.states[at].children;
                if *subtree || children.len() != 1 {
                    return Err(StEditError::new(
                        "cannot_remove_root",
                        "the root can only be removed when it has exactly one child (which becomes the root); \
                         set another root first",
                    ));
                }
                def.root.clone_from(&children[0]);
            } else if !*subtree && let Some(p) = parent {
                let moved = def.states[at].children.clone();
                let list = &mut def.states[p].children;
                if let Some(pos) = list.iter().position(|c| c == name) {
                    list.remove(pos);
                    for (k, child) in moved.into_iter().enumerate() {
                        list.insert(pos + k, child);
                    }
                }
            }
            def.states.retain(|s| !doomed.contains(&s.name));
            for s in &mut def.states {
                s.children.retain(|c| !doomed.contains(c));
                s.transitions
                    .retain(|t| !matches!(&t.target, Target::State(n) if doomed.contains(n)));
            }
        }
        StOp::RenameState { name, to } => {
            let at = find(def, name)?;
            if name == to {
                return Ok(());
            }
            check_new_name(def, to)?;
            def.states[at].name.clone_from(to);
            if def.root == *name {
                def.root.clone_from(to);
            }
            for s in &mut def.states {
                for child in &mut s.children {
                    if child == name {
                        child.clone_from(to);
                    }
                }
                for t in &mut s.transitions {
                    if let Target::State(target) = &mut t.target
                        && target == name
                    {
                        target.clone_from(to);
                    }
                }
            }
        }
        StOp::ReparentState { name, parent, index } => {
            if find(def, name).is_err() && !is_included_name(def, name) {
                return Err(StEditError::new(
                    "unknown_state",
                    format!("no state (or included state) is named {name:?}"),
                ));
            }
            let p = find(def, parent)?;
            if *name == def.root {
                return Err(StEditError::new(
                    "is_root",
                    "the root cannot be moved under another state",
                ));
            }
            if is_within(def, name, parent) {
                return Err(StEditError::new(
                    "would_cycle",
                    format!("{parent:?} is {name:?} or below it"),
                ));
            }
            for s in &mut def.states {
                s.children.retain(|c| c != name);
            }
            insert_at(&mut def.states[p].children, *index, name.clone())?;
        }
        StOp::SetRoot { name } => {
            find(def, name)?;
            if parent_of(def, name).is_some() {
                return Err(StEditError::new(
                    "has_parent",
                    format!("{name:?} is listed as a child; take it out of its parent first"),
                ));
            }
            def.root.clone_from(name);
        }
        StOp::SetState {
            name,
            selection,
            enter,
            weight,
            utility,
            clear_utility,
            fallback,
        } => {
            let at = find(def, name)?;
            if let Some(w) = weight {
                finite(*w, "the weight")?;
            }
            if let Some(u) = utility {
                finite(u.scale, "the utility scale")?;
                finite(u.bias, "the utility bias")?;
            }
            check_condition(enter.as_ref())?;
            let s = &mut def.states[at];
            if let Some(v) = selection {
                s.selection = *v;
            }
            if let Some(v) = enter {
                s.enter = v.clone();
            }
            if let Some(v) = weight {
                s.weight = *v;
            }
            if *clear_utility {
                s.utility = None;
            }
            if let Some(v) = utility {
                s.utility = Some(v.clone());
            }
            if let Some(v) = fallback {
                s.fallback = *v;
            }
        }
        StOp::SetTasks { name, tasks } => {
            let at = find(def, name)?;
            def.states[at].tasks.clone_from(tasks);
        }
        StOp::SetAmbient { tasks } => def.ambient.clone_from(tasks),
        StOp::SetParam { state, key, value } => {
            let at = find(def, state)?;
            if key.is_empty() {
                return Err(StEditError::new("bad_value", "a parameter key cannot be empty"));
            }
            match value {
                Some(Literal::Float(v)) if !v.is_finite() => {
                    return Err(StEditError::new("bad_value", "a float parameter must be finite"));
                }
                Some(v) => {
                    def.states[at].enter_set.insert(key.clone(), v.clone());
                }
                None => {
                    def.states[at].enter_set.remove(key);
                }
            }
        }
        StOp::AddTransition {
            state,
            trigger,
            condition,
            target,
            priority,
            hold,
            index,
        } => {
            let at = find(def, state)?;
            check_hold(*hold)?;
            check_condition(condition.as_ref())?;
            let transition = TransitionDef {
                trigger: trigger.clone(),
                condition: condition.clone().unwrap_or_default(),
                priority: priority.unwrap_or(0),
                target: target.clone(),
                hold: hold.unwrap_or(0.0),
            };
            insert_at(&mut def.states[at].transitions, *index, transition)?;
        }
        StOp::RemoveTransition { state, index } => {
            transition_mut(def, state, *index)?;
            let at = find(def, state)?;
            def.states[at].transitions.remove(*index);
        }
        StOp::SetTransition {
            state,
            index,
            trigger,
            condition,
            target,
            priority,
            hold,
        } => {
            check_hold(*hold)?;
            check_condition(condition.as_ref())?;
            let t = transition_mut(def, state, *index)?;
            if let Some(v) = trigger {
                t.trigger = v.clone();
            }
            if let Some(v) = condition {
                t.condition = v.clone();
            }
            if let Some(v) = target {
                t.target = v.clone();
            }
            if let Some(v) = priority {
                t.priority = *v;
            }
            if let Some(v) = hold {
                t.hold = *v;
            }
        }
        StOp::MoveTransition { state, from, to } => {
            let at = find(def, state)?;
            let list = &mut def.states[at].transitions;
            let len = list.len();
            if *from >= len || *to >= len {
                return Err(StEditError::new(
                    "bad_index",
                    format!("positions {from} and {to} must be below the {len} transitions"),
                ));
            }
            let t = list.remove(*from);
            list.insert(*to, t);
        }
        StOp::AddInclude { prefix, path, index } => {
            check_prefix(def, prefix)?;
            if path.is_empty() {
                return Err(StEditError::new("bad_value", "an include path cannot be empty"));
            }
            let include = IncludeDef {
                prefix: prefix.clone(),
                path: path.clone(),
            };
            insert_at(&mut def.includes, *index, include)?;
        }
        StOp::RemoveInclude { prefix } => {
            let at = include_index(def, prefix)?;
            def.includes.remove(at);
            let lead = format!("{prefix}/");
            for s in &mut def.states {
                s.children.retain(|c| !c.starts_with(&lead));
                s.transitions
                    .retain(|t| !matches!(&t.target, Target::State(n) if n.starts_with(&lead)));
            }
        }
        StOp::SetInclude {
            prefix,
            to_prefix,
            path,
        } => {
            let at = include_index(def, prefix)?;
            if let Some(p) = to_prefix
                && p != prefix
            {
                check_prefix(def, p)?;
            }
            if path.as_ref().is_some_and(String::is_empty) {
                return Err(StEditError::new("bad_value", "an include path cannot be empty"));
            }
            if let Some(p) = to_prefix
                && p != prefix
            {
                rename_prefix(def, prefix, p);
            }
            let include = &mut def.includes[at];
            if let Some(p) = to_prefix {
                include.prefix.clone_from(p);
            }
            if let Some(p) = path {
                include.path.clone_from(p);
            }
        }
        StOp::SetTree {
            description,
            seed,
            state_key,
        } => {
            if let Some(v) = description {
                def.description.clone_from(v);
            }
            if let Some(v) = seed {
                def.seed = *v;
            }
            if let Some(v) = state_key {
                def.state_key.clone_from(v);
            }
        }
    }
    Ok(())
}

/// A failed batch: which operation failed and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StBatchError {
    /// The position of the failing operation in the list.
    pub op_index: usize,
    /// Why it failed.
    pub error: StEditError,
}

/// Applies `ops` in order, all or nothing: the first failure leaves `def` untouched.
///
/// # Errors
///
/// The first operation that does not fit the tree.
pub fn apply_ops(def: &mut StateTreeDef, ops: &[StOp]) -> Result<(), StBatchError> {
    let mut work = def.clone();
    for (op_index, op) in ops.iter().enumerate() {
        apply_op(&mut work, op).map_err(|error| StBatchError { op_index, error })?;
    }
    *def = work;
    Ok(())
}

/// The parent of each state, by name (`None` for the root and for states no one lists).
#[must_use]
pub fn parents(def: &StateTreeDef) -> BTreeMap<String, Option<String>> {
    let mut out: BTreeMap<String, Option<String>> =
        def.states.iter().map(|s| (s.name.clone(), None)).collect();
    for s in &def.states {
        for c in &s.children {
            if let Some(slot) = out.get_mut(c)
                && slot.is_none()
            {
                *slot = Some(s.name.clone());
            }
        }
    }
    out
}

/// The states in display order: depth first from the root, children in their list order, then
/// every state the root does not reach (depth 0). Each entry is `(index into def.states, depth)`.
#[must_use]
pub fn hierarchy(def: &StateTreeDef) -> Vec<(usize, usize)> {
    let index: BTreeMap<&str, usize> = def
        .states
        .iter()
        .enumerate()
        .rev()
        .map(|(i, s)| (s.name.as_str(), i))
        .collect();
    let mut out = Vec::with_capacity(def.states.len());
    let mut seen = vec![false; def.states.len()];
    let mut stack: Vec<(usize, usize)> = index
        .get(def.root.as_str())
        .map(|&r| (r, 0))
        .into_iter()
        .collect();
    while let Some((i, depth)) = stack.pop() {
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        out.push((i, depth));
        for child in def.states[i].children.iter().rev() {
            if let Some(&c) = index.get(child.as_str()) {
                stack.push((c, depth + 1));
            }
        }
    }
    for (i, s) in seen.iter().enumerate() {
        if !s {
            out.push((i, 0));
        }
    }
    out
}

#[cfg(test)]
mod tests;
