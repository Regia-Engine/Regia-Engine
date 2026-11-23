// SPDX-License-Identifier: MPL-2.0
//! Reusable `.ui.ron` documents (`docs/specs/111-ui-components.md`, ADR-3147).
//!
//! A **component** is a `.ui.ron` document (optionally with a [`ComponentDecl`]) that other
//! documents instantiate with `use: "ui/health_bar.ui.ron"`. [`expand`] turns a document into the
//! plain tree the spawner builds: every instance becomes a copy of its component's root with the
//! instance's parameters applied, its `$`-relative binding sources resolved against the instance's
//! `bind_root`, and its slot content placed in the component's outlets. Failures never panic: each
//! one is a [`ComponentDiagnostic`] and the failing instance becomes a visible placeholder node.
//!
//! Expansion is a depth-first tree rewrite with an explicit stack of the documents being expanded;
//! a path already on the stack is a cycle (ordinary cycle detection on a depth-first walk, e.g.
//! T. H. Cormen et al., *Introduction to Algorithms*, ch. "Elementary Graph Algorithms").

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::authoring::{
    BackgroundDef, BindDef, BorderDef, TextDef, TextSpanDef, UiFile, UiMigrators, UiNodeDef, WidgetDef,
};
use crate::color::UiColor;

/// Components nested deeper than this give `ui_component_depth`.
pub const MAX_COMPONENT_DEPTH: usize = 8;

/// At most this many instances expand in one document, nesting included (a guard against
/// exponential growth: components that each instantiate several others); the rest give
/// `ui_component_too_large`.
pub const MAX_COMPONENT_INSTANCES: usize = 4096;

/// The name of the placeholder node a failing instance becomes.
pub const PLACEHOLDER_NAME: &str = "ui_component_error";

/// The class of the placeholder node.
pub const PLACEHOLDER_CLASS: &str = "ui-component-error";

/// Every diagnostic code [`expand`] reports (documented in `book/agents/ui-components.md`).
pub const COMPONENT_CODES: [&str; 11] = [
    "ui_component_missing",
    "ui_component_invalid",
    "ui_component_cycle",
    "ui_component_depth",
    "ui_component_unknown_param",
    "ui_component_param_type",
    "ui_component_unknown_slot",
    "ui_component_override",
    "ui_component_bad_path",
    "ui_component_no_bind_root",
    "ui_component_too_large",
];

/// A component document's declaration (`component:` at the top level of a `.ui.ron`, v8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ComponentDecl {
    /// The parameters an instance may set.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ParamDecl>,
    /// The slot names; a node with `slot: "<name>"` is the outlet.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub slots: Vec<String>,
}

impl ComponentDecl {
    /// The parameter called `name`.
    pub fn param(&self, name: &str) -> Option<&ParamDecl> {
        self.params.iter().find(|p| p.name == name)
    }
}

/// One declared parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamDecl {
    /// Parameter name (unique in the component).
    pub name: String,
    /// Value type.
    #[serde(rename = "type")]
    pub ty: ParamType,
    /// Value when an instance does not set it.
    pub default: ParamValue,
    /// The node properties the value is written to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<ParamTarget>,
}

/// Where a parameter's value goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamTarget {
    /// The `name` of a node of the component document (`""` is the root).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node: String,
    /// The property set on it.
    pub property: ParamProperty,
}

/// A parameter's value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamType {
    /// Text.
    String,
    /// A number.
    Number,
    /// `true` or `false`.
    Bool,
    /// A colour: `"#RRGGBB[AA]"` or linear `[r, g, b, a]`.
    Color,
}

impl ParamType {
    /// The `.ui.ron` spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Bool => "bool",
            Self::Color => "color",
        }
    }
}

/// The node properties a parameter can set (the only overridable surface of a component).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamProperty {
    /// The node's text, as one literal span.
    Text,
    /// The node's `command`.
    Command,
    /// A class appended to `classes`.
    Class,
    /// Solid background colour.
    Background,
    /// Border colour (a 1 px border when the node had none).
    BorderColor,
    /// Image tint.
    Tint,
    /// `opacity`.
    Opacity,
    /// Width in UI px.
    Width,
    /// Height in UI px.
    Height,
    /// Progress widget value.
    Progress,
    /// Shown when true, `display: none` when false.
    Visible,
    /// `disabled`.
    Disabled,
}

impl ParamProperty {
    /// Every property, in declaration order (designer pickers).
    pub const ALL: [ParamProperty; 12] = [
        Self::Text,
        Self::Command,
        Self::Class,
        Self::Background,
        Self::BorderColor,
        Self::Tint,
        Self::Opacity,
        Self::Width,
        Self::Height,
        Self::Progress,
        Self::Visible,
        Self::Disabled,
    ];

    /// The type a value for this property must have.
    pub fn value_type(self) -> ParamType {
        match self {
            Self::Text | Self::Command | Self::Class => ParamType::String,
            Self::Background | Self::BorderColor | Self::Tint => ParamType::Color,
            Self::Opacity | Self::Width | Self::Height | Self::Progress => ParamType::Number,
            Self::Visible | Self::Disabled => ParamType::Bool,
        }
    }

    /// The `.ui.ron` spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Command => "command",
            Self::Class => "class",
            Self::Background => "background",
            Self::BorderColor => "border_color",
            Self::Tint => "tint",
            Self::Opacity => "opacity",
            Self::Width => "width",
            Self::Height => "height",
            Self::Progress => "progress",
            Self::Visible => "visible",
            Self::Disabled => "disabled",
        }
    }

    /// The property spelled `name`.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }

    /// The node's current value of this property, as a parameter default (make-component).
    pub fn current(self, node: &UiNodeDef) -> ParamValue {
        let num = |v: f32| ParamValue::Number(f64::from(v));
        match self {
            Self::Text => ParamValue::Text(node.text.as_ref().map(text_of).unwrap_or_default()),
            Self::Command => ParamValue::Text(node.command.clone().unwrap_or_default()),
            Self::Class => ParamValue::Text(String::new()),
            Self::Background => match &node.background {
                Some(BackgroundDef::Color(c)) => ParamValue::Channels(c.0),
                Some(BackgroundDef::Gradient { from, .. }) => ParamValue::Channels(from.0),
                None => ParamValue::Channels(UiColor::TRANSPARENT.0),
            },
            Self::BorderColor => {
                ParamValue::Channels(node.border.as_ref().map_or(UiColor::TRANSPARENT.0, |b| b.color.0))
            }
            Self::Tint => ParamValue::Channels(node.image.as_ref().map_or(UiColor::WHITE.0, |i| i.tint.0)),
            Self::Opacity => num(node.opacity.unwrap_or(1.0)),
            Self::Width => num(points(node.style.size[0])),
            Self::Height => num(points(node.style.size[1])),
            Self::Progress => match node.widget {
                Some(WidgetDef::Progress(v)) => num(v),
                _ => num(0.0),
            },
            Self::Visible => ParamValue::Bool(node.style.display != crate::style::Display::None),
            Self::Disabled => ParamValue::Bool(node.disabled),
        }
    }
}

fn points(d: crate::style::Dimension) -> f32 {
    match d {
        crate::style::Dimension::Points(p) => p,
        _ => 0.0,
    }
}

fn text_of(t: &TextDef) -> String {
    t.spans
        .iter()
        .map(|s| match s {
            TextSpanDef::Lit(text) | TextSpanDef::Full { text, .. } => text.as_str(),
            TextSpanDef::Loc(crate::authoring::LocSpan::Loc(key)) => key.as_str(),
        })
        .collect()
}

/// A parameter value as written in `.ui.ron`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParamValue {
    /// `true` / `false`.
    Bool(bool),
    /// A number.
    Number(f64),
    /// Text (also a `"#RRGGBB[AA]"` colour).
    Text(String),
    /// Linear `[r, g, b, a]` colour channels.
    Channels([f32; 4]),
}

impl ParamValue {
    /// Whether the value can be used as `ty`.
    pub fn fits(&self, ty: ParamType) -> bool {
        match ty {
            ParamType::String => matches!(self, Self::Text(_)),
            ParamType::Number => matches!(self, Self::Number(n) if n.is_finite()),
            ParamType::Bool => matches!(self, Self::Bool(_)),
            ParamType::Color => self.color().is_some(),
        }
    }

    /// The colour, when the value is one.
    pub fn color(&self) -> Option<UiColor> {
        match self {
            Self::Text(hex) => UiColor::from_hex(hex),
            Self::Channels(c) => Some(UiColor(*c)),
            _ => None,
        }
    }

    fn number(&self) -> f32 {
        match self {
            Self::Number(n) => *n as f32,
            _ => 0.0,
        }
    }

    fn text(&self) -> String {
        match self {
            Self::Text(t) => t.clone(),
            _ => String::new(),
        }
    }

    fn flag(&self) -> bool {
        matches!(self, Self::Bool(true))
    }

    /// A short type label of the value itself (error messages).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Bool(_) => "bool",
            Self::Number(_) => "number",
            Self::Text(_) => "string",
            Self::Channels(_) => "color channels",
        }
    }
}

/// One expansion problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComponentDiagnostic {
    /// Stable code (one of [`COMPONENT_CODES`]).
    pub code: &'static str,
    /// `error` or `warning`.
    pub severity: &'static str,
    /// The document holding the failing instance.
    pub document: String,
    /// The instance's node key in that document (names from the root, `#<index>` when unnamed).
    pub node: String,
    /// The component path the instance uses.
    pub component: String,
    /// What is wrong.
    pub message: String,
}

/// Why a component could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadFailure {
    /// The file does not exist or cannot be read.
    Missing(String),
    /// The file does not parse, or its declaration is invalid.
    Invalid(String),
}

/// The result of [`expand`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Expansion {
    /// The plain tree (no `use` nodes left).
    pub root: UiNodeDef,
    /// Every component path the document reached, directly or through other components (also the
    /// ones that failed to load, so creating a missing file re-expands its users).
    pub uses: BTreeSet<String>,
    /// Problems, in tree order.
    pub diagnostics: Vec<ComponentDiagnostic>,
}

impl Expansion {
    /// Whether any diagnostic is an error.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.severity == "error")
    }
}

/// A loader for [`expand`]: component path (under `assets/`) to its parsed, checked document.
pub type ComponentLoader<'a> = dyn FnMut(&str) -> Result<Arc<UiFile>, LoadFailure> + 'a;

/// Whether `node` or anything under it is an instance.
pub fn has_instances(node: &UiNodeDef) -> bool {
    node.use_component.is_some()
        || node.children.iter().any(has_instances)
        || node.slots.values().flatten().any(has_instances)
}

/// Expands every instance of `file` (whose own path is `document`, used for cycles and messages).
pub fn expand(file: &UiFile, document: &str, load: &mut ComponentLoader<'_>) -> Expansion {
    let _span = tracing::debug_span!("ui_component_expand", document).entered();
    let mut root = file.root.clone();
    if !has_instances(&root) {
        return Expansion {
            root,
            ..Expansion::default()
        };
    }
    let mut ctx = Ctx {
        load,
        uses: BTreeSet::new(),
        diagnostics: Vec::new(),
        expanded: 0,
    };
    let mut stack = vec![document.to_owned()];
    ctx.expand_in_place(&mut root, &mut stack, String::new());
    Expansion {
        root,
        uses: ctx.uses,
        diagnostics: ctx.diagnostics,
    }
}

/// Expands one instance node (or a plain subtree) outside a document: [`crate::authoring::spawn_node`]
/// uses it for a `use` node that reached the spawner unexpanded.
pub fn expand_node(node: &UiNodeDef, document: &str, load: &mut ComponentLoader<'_>) -> Expansion {
    let file = UiFile {
        version: crate::authoring::UI_RON_VERSION,
        component: None,
        root: node.clone(),
        variants: Vec::new(),
        back: None,
    };
    expand(&file, document, load)
}

struct Ctx<'l, 'a> {
    load: &'l mut ComponentLoader<'a>,
    uses: BTreeSet<String>,
    diagnostics: Vec<ComponentDiagnostic>,
    /// Instances expanded so far (bounded by [`MAX_COMPONENT_INSTANCES`]).
    expanded: usize,
}

fn child_key(parent: &str, index: usize, node: &UiNodeDef) -> String {
    let own = match &node.name {
        Some(name) => name.clone(),
        None => format!("#{index}"),
    };
    if parent.is_empty() {
        own
    } else {
        format!("{parent}/{own}")
    }
}

impl Ctx<'_, '_> {
    fn diag(&mut self, code: &'static str, stack: &[String], node: &str, component: &str, message: String) {
        let severity = if code == "ui_component_no_bind_root" {
            "warning"
        } else {
            "error"
        };
        tracing::warn!(target: "regia_ui", "{code}: {} {node}: {message}", stack.last().map_or("", String::as_str));
        self.diagnostics.push(ComponentDiagnostic {
            code,
            severity,
            document: stack.last().cloned().unwrap_or_default(),
            node: node.to_owned(),
            component: component.to_owned(),
            message,
        });
    }

    /// Expands `node` (owned by the document on top of `stack`) in place.
    fn expand_in_place(&mut self, node: &mut UiNodeDef, stack: &mut Vec<String>, key: String) {
        if node.use_component.is_some() {
            let inst = std::mem::take(node);
            *node = self.expand_instance(inst, stack, &key);
            return;
        }
        for (i, child) in node.children.iter_mut().enumerate() {
            let k = child_key(&key, i, child);
            self.expand_in_place(child, stack, k);
        }
    }

    fn expand_instance(&mut self, mut inst: UiNodeDef, stack: &mut Vec<String>, key: &str) -> UiNodeDef {
        let path = inst.use_component.clone().unwrap_or_default();
        let fail = |ctx: &mut Self, code: &'static str, message: String, inst: &UiNodeDef| {
            ctx.diag(code, stack, key, &path, message);
            placeholder(code, &path, inst)
        };
        if let Err(why) = check_use_path(&path) {
            return fail(self, "ui_component_bad_path", why, &inst);
        }
        self.uses.insert(path.clone());
        if stack.iter().any(|s| s == &path) {
            let mut chain: Vec<&str> = stack.iter().map(String::as_str).collect();
            chain.push(&path);
            return fail(
                self,
                "ui_component_cycle",
                format!("cycle: {}", chain.join(" -> ")),
                &inst,
            );
        }
        if stack.len() > MAX_COMPONENT_DEPTH {
            return fail(
                self,
                "ui_component_depth",
                format!("more than {MAX_COMPONENT_DEPTH} nested components ({path})"),
                &inst,
            );
        }
        self.expanded += 1;
        if self.expanded > MAX_COMPONENT_INSTANCES {
            return fail(
                self,
                "ui_component_too_large",
                format!("more than {MAX_COMPONENT_INSTANCES} component instances in one document"),
                &inst,
            );
        }
        let comp = match (self.load)(&path) {
            Ok(comp) => comp,
            Err(LoadFailure::Missing(why)) => return fail(self, "ui_component_missing", why, &inst),
            Err(LoadFailure::Invalid(why)) => return fail(self, "ui_component_invalid", why, &inst),
        };
        if let Some(fields) = override_fields(&inst) {
            return fail(
                self,
                "ui_component_override",
                format!(
                    "an instance sets only name, classes, style, params, bind_root, slots and children; this one also sets {fields}"
                ),
                &inst,
            );
        }
        let decl = comp.component.clone().unwrap_or_default();
        for (name, value) in &inst.params {
            let Some(param) = decl.param(name) else {
                let known: Vec<&str> = decl.params.iter().map(|p| p.name.as_str()).collect();
                return fail(
                    self,
                    "ui_component_unknown_param",
                    format!("{path} declares no parameter {name:?} (declared: {known:?})"),
                    &inst,
                );
            };
            if !value.fits(param.ty) {
                return fail(
                    self,
                    "ui_component_param_type",
                    format!(
                        "parameter {name:?} of {path} is a {}, got a {}",
                        param.ty.name(),
                        value.kind()
                    ),
                    &inst,
                );
            }
        }
        let has_slot = |name: &str| decl.slots.iter().any(|s| s == name);
        if let Some(bad) = inst.slots.keys().find(|k| !has_slot(k)) {
            return fail(
                self,
                "ui_component_unknown_slot",
                format!("{path} declares no slot {bad:?} (declared: {:?})", decl.slots),
                &inst,
            );
        }
        if !inst.children.is_empty() && !has_slot("default") {
            return fail(
                self,
                "ui_component_unknown_slot",
                format!("{path} declares no `default` slot for the instance's children"),
                &inst,
            );
        }

        // The body: the component's root with the parameters applied.
        let mut body = comp.root.clone();
        for param in &decl.params {
            let value = inst.params.get(&param.name).unwrap_or(&param.default);
            for target in &param.targets {
                if let Some(node) = find_named_mut(&mut body, &target.node) {
                    apply_param(node, target.property, value);
                }
            }
        }
        // `$` sources resolve against the instance's view-model path (spec 111 § 3).
        let mut unresolved = false;
        resolve_prefixed(&mut body, inst.bind_root.as_deref(), &mut unresolved);
        if unresolved {
            self.diag(
                "ui_component_no_bind_root",
                stack,
                key,
                &path,
                format!("{path} binds `$` sources but the instance gives no bind_root"),
            );
        }
        // Slot content belongs to the outer document: expand it there.
        let mut slots = std::mem::take(&mut inst.slots);
        if !inst.children.is_empty() {
            slots
                .entry("default".to_owned())
                .or_default()
                .extend(std::mem::take(&mut inst.children));
        }
        for (name, content) in &mut slots {
            for (i, child) in content.iter_mut().enumerate() {
                let k = child_key(&format!("{key}/{name}"), i, child);
                self.expand_in_place(child, stack, k);
            }
        }
        // Nested instances of the component (its root included) expand with it on the stack.
        stack.push(path.clone());
        self.expand_in_place(&mut body, stack, key.to_owned());
        stack.pop();
        fill_slots(&mut body, &mut slots);

        if inst.name.is_some() {
            body.name = inst.name;
        }
        body.classes.extend(inst.classes);
        if inst.style != crate::style::Style::default() {
            body.style = inst.style;
        }
        body
    }
}

/// `Err(why)` when `path` is not a relative `.ui.ron` path under `assets/`.
pub fn check_use_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("`use` is empty".to_owned());
    }
    if path.starts_with('/') || path.contains('\\') || path.contains(':') {
        return Err(format!(
            "`use` must be a path under assets/ with `/` separators: {path:?}"
        ));
    }
    if path.split('/').any(|seg| seg == ".." || seg.is_empty()) {
        return Err(format!(
            "`use` must not climb out or hold empty segments: {path:?}"
        ));
    }
    if !path.ends_with(".ui.ron") {
        return Err(format!("`use` must name a .ui.ron document: {path:?}"));
    }
    Ok(())
}

/// The fields an instance node sets beyond the allowed ones, as RON text; `None` when it sets none.
fn override_fields(inst: &UiNodeDef) -> Option<String> {
    let mut probe = inst.clone();
    probe.name = None;
    probe.classes.clear();
    probe.style = crate::style::Style::default();
    probe.use_component = None;
    probe.params.clear();
    probe.bind_root = None;
    probe.slots.clear();
    probe.children.clear();
    if probe == UiNodeDef::default() {
        return None;
    }
    let text = ron::to_string(&probe).unwrap_or_else(|_| "other fields".to_owned());
    Some(text)
}

/// The visible node a failing instance becomes.
pub fn placeholder(code: &str, path: &str, inst: &UiNodeDef) -> UiNodeDef {
    UiNodeDef {
        name: Some(PLACEHOLDER_NAME.to_owned()),
        classes: vec![PLACEHOLDER_CLASS.to_owned()],
        style: inst.style.clone(),
        background: Some(BackgroundDef::Color(UiColor::linear(0.55, 0.03, 0.06, 0.85))),
        border: Some(BorderDef {
            color: UiColor::linear(1.0, 0.2, 0.2, 1.0),
            width: 2.0,
            radius: 0.0,
        }),
        text: Some(TextDef::literal(format!("{code}: {path}"))),
        ..UiNodeDef::default()
    }
}

/// The node called `name` in `root` (`""` is the root), not looking inside other instances.
pub fn find_named_mut<'a>(root: &'a mut UiNodeDef, name: &str) -> Option<&'a mut UiNodeDef> {
    if name.is_empty() || root.name.as_deref() == Some(name) {
        return Some(root);
    }
    root.children.iter_mut().find_map(|c| find_named_mut(c, name))
}

/// The node called `name` in `root` (`""` is the root).
pub fn find_named<'a>(root: &'a UiNodeDef, name: &str) -> Option<&'a UiNodeDef> {
    if name.is_empty() || root.name.as_deref() == Some(name) {
        return Some(root);
    }
    root.children.iter().find_map(|c| find_named(c, name))
}

/// Writes `value` to `property` of `node` (types were checked before).
pub fn apply_param(node: &mut UiNodeDef, property: ParamProperty, value: &ParamValue) {
    match property {
        ParamProperty::Text => {
            let text = value.text();
            match &mut node.text {
                Some(t) => t.spans = vec![TextSpanDef::Lit(text)],
                None => node.text = Some(TextDef::literal(text)),
            }
        }
        ParamProperty::Command => node.command = Some(value.text()),
        ParamProperty::Class => {
            let class = value.text();
            if !class.is_empty() {
                node.classes.push(class);
            }
        }
        ParamProperty::Background => {
            if let Some(c) = value.color() {
                node.background = Some(BackgroundDef::Color(c));
            }
        }
        ParamProperty::BorderColor => {
            if let Some(c) = value.color() {
                match &mut node.border {
                    Some(b) => b.color = c,
                    None => {
                        node.border = Some(BorderDef {
                            color: c,
                            width: 1.0,
                            radius: 0.0,
                        });
                    }
                }
            }
        }
        ParamProperty::Tint => {
            if let (Some(img), Some(c)) = (&mut node.image, value.color()) {
                img.tint = c;
            }
        }
        ParamProperty::Opacity => node.opacity = Some(value.number()),
        ParamProperty::Width => node.style.size[0] = crate::style::Dimension::Points(value.number()),
        ParamProperty::Height => node.style.size[1] = crate::style::Dimension::Points(value.number()),
        ParamProperty::Progress => {
            if let Some(WidgetDef::Progress(v)) = &mut node.widget {
                *v = value.number().clamp(0.0, 1.0);
            }
        }
        ParamProperty::Visible => {
            if !value.flag() {
                node.style.display = crate::style::Display::None;
            }
        }
        ParamProperty::Disabled => node.disabled = value.flag(),
    }
}

/// Rewrites every `$` source under `node` (binds, repeat sources, nested `bind_root`s and the slot
/// content of nested instances, which belong to this document) against `prefix`. `unresolved` is
/// set when a `$` source is found and there is no prefix.
fn resolve_prefixed(node: &mut UiNodeDef, prefix: Option<&str>, unresolved: &mut bool) {
    let mut fix = |source: &mut String| {
        if !crate::binding::is_prefixed(source) {
            return;
        }
        match prefix.and_then(|p| crate::binding::prefixed_source(p, source)) {
            Some(resolved) => *source = resolved,
            None => *unresolved = true,
        }
    };
    let fix_bind = |b: &mut BindDef, fix: &mut dyn FnMut(&mut String)| fix(&mut b.source);
    if let Some(b) = &mut node.bind {
        fix_bind(b, &mut fix);
    }
    for b in &mut node.bindings {
        fix_bind(b, &mut fix);
    }
    if let Some(r) = &mut node.repeat {
        fix(&mut r.source);
    }
    if let Some(root) = &mut node.bind_root {
        fix(root);
    }
    for child in &mut node.children {
        resolve_prefixed(child, prefix, unresolved);
    }
    for content in node.slots.values_mut() {
        for child in content {
            resolve_prefixed(child, prefix, unresolved);
        }
    }
}

/// Moves each slot's content into its outlet (replacing the outlet's default children) and clears
/// the outlet marks.
fn fill_slots(node: &mut UiNodeDef, slots: &mut BTreeMap<String, Vec<UiNodeDef>>) {
    if let Some(name) = node.slot.take()
        && let Some(content) = slots.remove(&name)
    {
        node.children = content;
        return;
    }
    for child in &mut node.children {
        fill_slots(child, slots);
    }
}

/// Checks a component document's declaration (spec 111 § 2.1); `Err` lists the first problem.
pub fn check_component(file: &UiFile) -> Result<(), String> {
    let mut outlets: Vec<String> = Vec::new();
    collect_outlets(&file.root, &mut outlets);
    let Some(decl) = &file.component else {
        if let Some(outlet) = outlets.first() {
            return Err(format!(
                "outlet slot {outlet:?} without a `component:` declaration"
            ));
        }
        return Ok(());
    };
    let mut names = BTreeSet::new();
    for param in &decl.params {
        if param.name.is_empty() || !names.insert(param.name.as_str()) {
            return Err(format!("parameter name {:?} is empty or used twice", param.name));
        }
        if !param.default.fits(param.ty) {
            return Err(format!(
                "parameter {:?}: default is a {}, declared {}",
                param.name,
                param.default.kind(),
                param.ty.name()
            ));
        }
        for target in &param.targets {
            if target.property.value_type() != param.ty {
                return Err(format!(
                    "parameter {:?} ({}) cannot set {} (a {})",
                    param.name,
                    param.ty.name(),
                    target.property.name(),
                    target.property.value_type().name()
                ));
            }
            let Some(node) = find_named(&file.root, &target.node) else {
                return Err(format!(
                    "parameter {:?}: no node named {:?}",
                    param.name, target.node
                ));
            };
            if target.property == ParamProperty::Progress
                && !matches!(node.widget, Some(WidgetDef::Progress(_)))
            {
                return Err(format!(
                    "parameter {:?}: node {:?} is not a progress bar",
                    param.name, target.node
                ));
            }
            if target.property == ParamProperty::Tint && node.image.is_none() {
                return Err(format!(
                    "parameter {:?}: node {:?} has no image",
                    param.name, target.node
                ));
            }
        }
    }
    let mut slots = BTreeSet::new();
    for slot in &decl.slots {
        if slot.is_empty() || !slots.insert(slot.as_str()) {
            return Err(format!("slot name {slot:?} is empty or used twice"));
        }
        let count = outlets.iter().filter(|o| *o == slot).count();
        if count != 1 {
            return Err(format!(
                "slot {slot:?} has {count} outlet nodes (needs exactly one)"
            ));
        }
    }
    if let Some(stray) = outlets.iter().find(|o| !slots.contains(o.as_str())) {
        return Err(format!("outlet slot {stray:?} is not declared in `slots`"));
    }
    Ok(())
}

fn collect_outlets(node: &UiNodeDef, out: &mut Vec<String>) {
    out.extend(node.slot.clone());
    for child in &node.children {
        collect_outlets(child, out);
    }
}

/// Parses a component document (`parse_ui_ron` plus [`check_component`]).
pub fn parse_component(text: &str, path: &str, migrators: &UiMigrators) -> Result<UiFile, LoadFailure> {
    let file = crate::authoring::parse_ui_ron(text, path, migrators)
        .map_err(|e| LoadFailure::Invalid(e.to_string()))?;
    check_component(&file).map_err(|why| LoadFailure::Invalid(format!("{path}: {why}")))?;
    Ok(file)
}

/// A caching loader over a text reader (`path` under `assets/` to the file's text): each component
/// is read and parsed once per loader.
pub fn text_loader<'a>(
    mut read: impl FnMut(&str) -> Result<String, String> + 'a,
    migrators: &'a UiMigrators,
) -> impl FnMut(&str) -> Result<Arc<UiFile>, LoadFailure> + 'a {
    let mut cache: HashMap<String, Result<Arc<UiFile>, LoadFailure>> = HashMap::new();
    move |path: &str| {
        if let Some(hit) = cache.get(path) {
            return hit.clone();
        }
        let loaded = read(path)
            .map_err(LoadFailure::Missing)
            .and_then(|text| parse_component(&text, path, migrators).map(Arc::new));
        cache.insert(path.to_owned(), loaded.clone());
        loaded
    }
}

/// A component's summary (`ui.components`, the designer palette).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ComponentInfo {
    /// Path under `assets/`.
    pub path: String,
    /// Declared parameters.
    pub params: Vec<ParamDecl>,
    /// Declared slots.
    pub slots: Vec<String>,
}

impl ComponentInfo {
    /// The summary of a component document.
    pub fn of(path: &str, file: &UiFile) -> Self {
        let decl = file.component.clone().unwrap_or_default();
        Self {
            path: path.to_owned(),
            params: decl.params,
            slots: decl.slots,
        }
    }
}

/// Every `.ui.ron` under `assets_dir` (recursively) that declares `component:`, sorted by path.
/// Unreadable or unparsable files are skipped.
pub fn scan_components(assets_dir: &std::path::Path) -> Vec<ComponentInfo> {
    let mut out = Vec::new();
    let mut stack = vec![assets_dir.to_path_buf()];
    let migrators = UiMigrators::default();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".ui.ron") {
                continue;
            }
            let Ok(rel) = path.strip_prefix(assets_dir) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(file) = crate::authoring::parse_ui_ron(&text, &rel, &migrators)
                && file.component.is_some()
            {
                out.push(ComponentInfo::of(&rel, &file));
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authoring::parse_ui_ron;

    fn files(list: &[(&str, &str)]) -> impl FnMut(&str) -> Result<String, String> + use<> {
        let map: HashMap<String, String> = list
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |p: &str| map.get(p).cloned().ok_or_else(|| format!("{p}: not found"))
    }

    const BAR: &str = r##"(version: 9,
        component: (
            params: [
                (name: "label", type: string, default: "HP", targets: [(node: "Label", property: text)]),
                (name: "fill", type: color, default: "#30C060", targets: [(node: "Fill", property: background)]),
            ],
            slots: ["badge"],
        ),
        root: (name: "Bar", children: [
            (name: "Label", text: (spans: ["?"]), bind: (source: "$.name")),
            (name: "Fill", bindings: [(source: "$.hp", target: opacity)]),
            (name: "Badge", slot: "badge", children: [(name: "DefaultBadge")]),
        ]),
    )"##;

    fn doc(text: &str) -> UiFile {
        parse_ui_ron(text, "doc", &UiMigrators::default()).unwrap()
    }

    #[test]
    fn the_prefix_hook_resolves_absolute_and_item_relative_roots() {
        use crate::binding::prefixed_source;
        assert_eq!(
            prefixed_source("Party.p1", "$.hp").as_deref(),
            Some("Party.p1.hp")
        );
        assert_eq!(prefixed_source("Party.p1", "$").as_deref(), Some("Party.p1"));
        assert_eq!(prefixed_source(".", "$.hp").as_deref(), Some(".hp"));
        assert_eq!(prefixed_source(".", "$").as_deref(), Some("."));
        assert_eq!(prefixed_source(".stats", "$.hp").as_deref(), Some(".stats.hp"));
        assert_eq!(prefixed_source("A", "$x"), None);
        assert_eq!(prefixed_source("A", "Plain.path"), None);
    }

    #[test]
    fn parameters_bind_roots_and_slots_expand() {
        let m = UiMigrators::default();
        let mut load = text_loader(files(&[("ui/bar.ui.ron", BAR)]), &m);
        let file = doc(r##"(version: 9, root: (children: [
            (use: "ui/bar.ui.ron", name: "P1", bind_root: "Party.p1", params: {"label": "P1"}),
            (use: "ui/bar.ui.ron", name: "P2", bind_root: "Party.p2", params: {"fill": "#FF0000"},
                slots: {"badge": [(name: "Star")]}),
        ]))"##);
        let out = expand(&file, "ui/hud.ui.ron", &mut load);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.uses.iter().collect::<Vec<_>>(), ["ui/bar.ui.ron"]);
        let p1 = &out.root.children[0];
        assert_eq!(p1.name.as_deref(), Some("P1"));
        assert_eq!(p1.children[0].text.as_ref().map(text_of).as_deref(), Some("P1"));
        assert_eq!(p1.children[0].bind.as_ref().unwrap().source, "Party.p1.name");
        assert_eq!(p1.children[1].bindings[0].source, "Party.p1.hp");
        assert_eq!(p1.children[2].children[0].name.as_deref(), Some("DefaultBadge"));
        assert!(p1.children[2].slot.is_none());
        let p2 = &out.root.children[1];
        assert_eq!(p2.children[0].text.as_ref().map(text_of).as_deref(), Some("HP"));
        assert_eq!(
            p2.children[1].background,
            Some(BackgroundDef::Color(UiColor::from_hex("#FF0000").unwrap()))
        );
        assert_eq!(p2.children[2].children.len(), 1);
        assert_eq!(p2.children[2].children[0].name.as_deref(), Some("Star"));
        assert!(!has_instances(&out.root));
    }

    #[test]
    fn errors_become_placeholders_with_codes() {
        let m = UiMigrators::default();
        let cases = [
            (r#"(use: "ui/none.ui.ron")"#, "ui_component_missing"),
            (
                r#"(use: "ui/bar.ui.ron", params: {"nope": 1})"#,
                "ui_component_unknown_param",
            ),
            (
                r#"(use: "ui/bar.ui.ron", params: {"label": 3})"#,
                "ui_component_param_type",
            ),
            (
                r#"(use: "ui/bar.ui.ron", slots: {"x": []})"#,
                "ui_component_unknown_slot",
            ),
            (
                r#"(use: "ui/bar.ui.ron", children: [()])"#,
                "ui_component_unknown_slot",
            ),
            (r#"(use: "ui/bar.ui.ron", opacity: 0.5)"#, "ui_component_override"),
            (r#"(use: "../bar.ui.ron")"#, "ui_component_bad_path"),
            (r#"(use: "ui/broken.ui.ron")"#, "ui_component_invalid"),
            (r#"(use: "ui/bar.ui.ron")"#, "ui_component_no_bind_root"),
        ];
        for (node, code) in cases {
            let mut load = text_loader(
                files(&[
                    ("ui/bar.ui.ron", BAR),
                    ("ui/broken.ui.ron", "(version: 9, root: (slot: \"x\"))"),
                ]),
                &m,
            );
            let file = doc(&format!("(version: 9, root: (children: [{node}]))"));
            let out = expand(&file, "ui/hud.ui.ron", &mut load);
            assert_eq!(out.diagnostics.len(), 1, "{node}: {:?}", out.diagnostics);
            assert_eq!(out.diagnostics[0].code, code, "{node}");
            assert!(COMPONENT_CODES.contains(&code));
            let placed = &out.root.children[0];
            if code == "ui_component_no_bind_root" {
                assert_eq!(
                    placed.name.as_deref(),
                    Some("Bar"),
                    "a warning keeps the instance"
                );
                assert_eq!(out.diagnostics[0].severity, "warning");
            } else {
                assert_eq!(placed.name.as_deref(), Some(PLACEHOLDER_NAME), "{node}");
                assert!(
                    placed
                        .text
                        .as_ref()
                        .map(text_of)
                        .unwrap_or_default()
                        .contains(code)
                );
            }
        }
    }

    #[test]
    fn cycles_and_depth_are_reported() {
        let m = UiMigrators::default();
        let a = r#"(version: 9, root: (name: "A", children: [(use: "ui/b.ui.ron")]))"#;
        let b = r#"(version: 9, root: (name: "B", children: [(use: "ui/a.ui.ron")]))"#;
        let mut load = text_loader(files(&[("ui/a.ui.ron", a), ("ui/b.ui.ron", b)]), &m);
        let out = expand(&doc(a), "ui/a.ui.ron", &mut load);
        assert_eq!(out.diagnostics.len(), 1, "{:?}", out.diagnostics);
        assert_eq!(out.diagnostics[0].code, "ui_component_cycle");
        assert!(
            out.diagnostics[0]
                .message
                .contains("ui/a.ui.ron -> ui/b.ui.ron -> ui/a.ui.ron")
        );

        // c0 -> c1 -> … -> c9: nine nested components, one more than allowed.
        let chain: Vec<(String, String)> = (0..10)
            .map(|i| {
                let body = if i < 9 {
                    format!(
                        r#"(version: 9, root: (name: "C{i}", children: [(use: "ui/c{}.ui.ron")]))"#,
                        i + 1
                    )
                } else {
                    r#"(version: 9, root: (name: "C9"))"#.to_owned()
                };
                (format!("ui/c{i}.ui.ron"), body)
            })
            .collect();
        let list: Vec<(&str, &str)> = chain.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let mut load = text_loader(files(&list), &m);
        let top = doc(r#"(version: 9, root: (children: [(use: "ui/c0.ui.ron")]))"#);
        let out = expand(&top, "ui/top.ui.ron", &mut load);
        assert_eq!(out.diagnostics.len(), 1, "{:?}", out.diagnostics);
        assert_eq!(out.diagnostics[0].code, "ui_component_depth");
        // Eight levels are fine.
        let top = doc(r#"(version: 9, root: (children: [(use: "ui/c2.ui.ron")]))"#);
        let mut load = text_loader(files(&list), &m);
        assert!(expand(&top, "ui/top.ui.ron", &mut load).diagnostics.is_empty());
    }

    #[test]
    fn declarations_are_checked() {
        let bad = [
            r#"(version: 9, component: (params: [(name: "a", type: number, default: "x")]), root: ())"#,
            r#"(version: 9, component: (params: [(name: "a", type: number, default: 1, targets: [(node: "N", property: text)])]), root: (name: "N"))"#,
            r#"(version: 9, component: (params: [(name: "a", type: string, default: "", targets: [(node: "Gone", property: text)])]), root: ())"#,
            r#"(version: 9, component: (slots: ["s"]), root: ())"#,
            r#"(version: 9, component: (slots: ["s"]), root: (children: [(slot: "s"), (slot: "s")]))"#,
            r#"(version: 9, root: (slot: "s"))"#,
            r#"(version: 9, component: (params: [(name: "p", type: number, default: 1, targets: [(property: progress)])]), root: ())"#,
        ];
        for text in bad {
            assert!(check_component(&doc(text)).is_err(), "{text}");
        }
        assert!(check_component(&doc(BAR)).is_ok());
    }

    #[test]
    fn a_component_inside_a_repeat_binds_its_row() {
        let m = UiMigrators::default();
        let row = r#"(version: 9, root: (name: "Row", children: [(name: "N", text: (spans: [""]), bind: (source: "$.name"))]))"#;
        let mut load = text_loader(files(&[("ui/row.ui.ron", row)]), &m);
        let file = doc(
            r#"(version: 9, root: (repeat: (source: "Inv.items", key: "id"), children: [(use: "ui/row.ui.ron", bind_root: ".")]))"#,
        );
        let out = expand(&file, "ui/list.ui.ron", &mut load);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(
            out.root.children[0].children[0].bind.as_ref().unwrap().source,
            ".name"
        );
    }
}
