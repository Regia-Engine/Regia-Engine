// SPDX-License-Identifier: MPL-2.0
//! Runtime inspector of the UI tree (`docs/specs/ui-parity-roadmap.md` row E04, ADR-2703;
//! book page `agents/ui-inspector.md`).
//!
//! [`inspect`] answers "why does this node look like this": its computed layout, every resolved
//! style property with the *source* that won (theme defaults, a type or variation layer of the
//! theme with its state, an inline value, a running animation or a binding), the bindings on the
//! node, its focus and navigation links and its accessible name. [`pick_at`] answers "which node
//! is under this point" by the interaction system's z-order rule, and [`node_path`] names the
//! chain of ancestors.
//!
//! Style inspection replays the theme resolver
//! for one node on request (the same layers in the same order as `theme_resolve`, recording the
//! winner per property) and compares it with the node's live [`Themed`]. The
//! theme code itself is untouched. ADR-7811 adds a bounded event recorder and explicit transient
//! edits; the runtime never saves them to a document.

use regia_app::prelude::*;
use serde_json::{Map, Value, json};

use crate::a11y::{Accessible, build_a11y_tree};
use crate::binding::{Bind, BindTarget};
use crate::focus::{FocusState, Focusable};
use crate::interaction::{HitCandidate, pick_topmost};
use crate::node::{
    Background, BackgroundKind, Classes, ComputedNode, Focused, Hovered, Interactable, Invalid, NodeName,
    NodeStyle, Opacity, Pressed, Selected, UiNode, UiRoot, ZOffset,
};
use crate::style::Style;
use crate::theme::{StyleClass, Themed, UiThemes, VisualOverrides};
use crate::theme_file::ResolvedTheme;
use crate::theme_resolve::{ThemeOverride, ThemeResolver, ThemeScope};
use crate::tween::{TweenProperty, UiTween};
use crate::widget_style::{StyleState, WidgetSize};

/// Where the winning value of one property came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// Nothing set it: the engine default (or the theme's defaults layer for a classed node).
    Default,
    /// A layer of the active theme: a widget type, a variation or a state layer of one.
    Theme,
    /// Set on the node itself: its style, a `ThemeOverride`, a `Background` or a text component.
    Inline,
    /// A running tween or theme transition is writing it now.
    Animation,
    /// A `Bind` component writes it from a model value.
    Binding,
}

impl SourceKind {
    /// The name used in the protocol.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Theme => "theme",
            Self::Inline => "inline",
            Self::Animation => "animation",
            Self::Binding => "binding",
        }
    }
}

/// One inspected property.
#[derive(Debug, Clone, PartialEq)]
pub struct InspectedProperty {
    /// Property name (`radius`, `background`, `size`, ...).
    pub name: String,
    /// `layout`, `visual` or `content`.
    pub group: &'static str,
    /// The resolved value.
    pub value: Value,
    /// Which kind of source won.
    pub source: SourceKind,
    /// A human-readable detail: the theme layer (`Button.primary hover`), the binding path, ...
    pub detail: String,
}

impl InspectedProperty {
    fn json(&self) -> Value {
        json!({
            "name": self.name,
            "group": self.group,
            "value": self.value,
            "source": self.source.name(),
            "detail": self.detail,
        })
    }
}

/// The names of the visual properties, in the order the inspector lists them.
pub const VISUAL_PROPERTIES: [&str; 22] = [
    "background",
    "border_color",
    "border_width",
    "radius",
    "text_color",
    "focus_ring",
    "focus_ring_width",
    "focus_ring_offset",
    "padding_x",
    "height",
    "min_width",
    "gap",
    "text_px",
    "icon_color",
    "elevation",
    "opacity",
    "translate",
    "scale",
    "padding",
    "min_height",
    "max_width",
    "max_height",
];

/// Applies `layer` over `winners`: every field it sets wins, labeled `label`.
fn lay(
    winners: &mut Map<String, Value>,
    labels: &mut Vec<(String, String, Value)>,
    layer: &VisualOverrides,
    label: &str,
) {
    let Ok(Value::Object(set)) = serde_json::to_value(layer) else {
        return;
    };
    for (key, value) in set {
        if value.is_null() {
            continue;
        }
        labels.push((key.clone(), label.to_owned(), value.clone()));
        winners.insert(key, value);
    }
}

/// Replays `compose` (theme resolution) for the given layers, recording the layer that won each field.
fn replay_layers(
    levels: &[(String, &StyleClass)],
    state: &StyleState,
) -> (Map<String, Value>, Vec<(String, String, Value)>) {
    let mut winners = Map::new();
    let mut labels = Vec::new();
    let hovered = state.hovered && !state.disabled && !state.touch;
    let pressed = state.pressed && !state.disabled;
    let focused = state.focused;
    for (name, level) in levels {
        let tag = |state: &str| {
            if state.is_empty() {
                name.clone()
            } else {
                format!("{name} {state}")
            }
        };
        lay(&mut winners, &mut labels, &level.base, &tag(""));
        match state.size {
            WidgetSize::Sm => lay(&mut winners, &mut labels, &level.sm, &tag("sm")),
            WidgetSize::Md => {}
            WidgetSize::Lg => lay(&mut winners, &mut labels, &level.lg, &tag("lg")),
        }
        if state.selected {
            lay(&mut winners, &mut labels, &level.selected, &tag("selected"));
        }
        if hovered {
            lay(&mut winners, &mut labels, &level.hover, &tag("hover"));
        }
        if pressed {
            lay(&mut winners, &mut labels, &level.pressed, &tag("pressed"));
        }
        if state.error {
            lay(&mut winners, &mut labels, &level.error, &tag("error"));
        }
        if focused {
            lay(&mut winners, &mut labels, &level.focused, &tag("focused"));
            if state.focus_visible {
                lay(
                    &mut winners,
                    &mut labels,
                    &level.focus_visible,
                    &tag("focus_visible"),
                );
            }
        }
    }
    if state.disabled {
        for (name, level) in levels {
            lay(
                &mut winners,
                &mut labels,
                &level.disabled,
                &format!("{name} disabled"),
            );
        }
    }
    (winners, labels)
}

/// The theme the node resolves against: the nearest `ThemeScope`'s theme, else the active one.
fn theme_for(world: &World, entity: Entity) -> Option<(ResolvedTheme, Option<String>)> {
    let resolver = world.get_resource::<ThemeResolver>()?;
    let active = resolver.resolved_theme()?;
    let mut cur = Some(entity);
    for _ in 0..256 {
        let Some(e) = cur else { break };
        if let Some(scope) = world.get::<ThemeScope>(e) {
            if scope.theme != active.name
                && let Some(themes) = world.get_resource::<UiThemes>()
                && let Ok(theme) = themes.set.resolve(&scope.theme, Some(active.density))
            {
                return Some((theme, Some(scope.theme.clone())));
            }
            return Some((active.clone(), Some(scope.theme.clone())));
        }
        cur = world.get::<ChildOf>(e).map(ChildOf::parent);
    }
    Some((active.clone(), None))
}

fn state_of(world: &World, e: Entity) -> StyleState {
    let touch = world
        .get_resource::<crate::interaction::UiPointer>()
        .is_some_and(|p| p.active_touch.is_some());
    let focused = world.get::<Focused>(e).is_some();
    let ring = world
        .get_resource::<crate::focus::FocusRingPolicy>()
        .copied()
        .unwrap_or_default()
        .shows_ring(
            world
                .get_resource::<crate::focus::UiInputMode>()
                .copied()
                .unwrap_or_default(),
        );
    StyleState {
        hovered: world.get::<Hovered>(e).is_some(),
        pressed: world.get::<Pressed>(e).is_some(),
        focused,
        disabled: crate::node::is_disabled(world, e),
        selected: world.get::<Selected>(e).is_some(),
        error: world.get::<Invalid>(e).is_some(),
        size: world.get::<WidgetSize>(e).copied().unwrap_or_default(),
        touch,
        focus_visible: focused && ring,
    }
}

/// The visual properties with their winning layer, replayed from the node's inputs.
fn visual_properties(
    world: &World,
    e: Entity,
    state: &StyleState,
) -> (Vec<InspectedProperty>, Value, Map<String, Value>) {
    let classes: Vec<String> = world.get::<Classes>(e).map(|c| c.0.clone()).unwrap_or_default();
    let ovr = world.get::<ThemeOverride>(e);
    let mut theme_info = json!({ "classed": !classes.is_empty() });
    let mut winners = Map::new();
    let mut labels: Vec<(String, String, Value)> = Vec::new();
    let mut history = Map::new();
    let resolved = theme_for(world, e);
    let mut inline: Vec<String> = Vec::new();
    let mut size_token: Vec<(&str, f32, String)> = Vec::new();
    if let Some((theme, scope)) = &resolved
        && (!classes.is_empty() || ovr.is_some())
    {
        let disabled_opacity = world
            .get_resource::<UiThemes>()
            .map_or(1.0, |t| t.disabled_opacity);
        let defaults = crate::theme_resolve::defaults_class(theme, disabled_opacity);
        // The ancestors' widget types, for `inside` rules (`.theme.ron` v2).
        let mut ancestor_types: Vec<String> = Vec::new();
        let mut cur = world.get::<ChildOf>(e).map(ChildOf::parent);
        for _ in 0..256 {
            let Some(a) = cur else { break };
            if let Some(c) = world.get::<Classes>(a) {
                ancestor_types.extend(crate::theme_resolve::class_types(theme, &c.0));
            }
            cur = world.get::<ChildOf>(a).map(ChildOf::parent);
        }
        let has_inside = |name: &str| ancestor_types.iter().any(|t| t == name);
        let owned: Vec<(String, StyleClass)> =
            crate::theme_resolve::theme_levels(theme, &classes, &has_inside);
        let rules = crate::theme_resolve::matching_rule_texts(theme, &classes, &has_inside);
        let legacy_classes = world.get_resource::<UiThemes>().map(|t| &t.classes);
        let legacy_custom = legacy_classes
            .map(crate::theme_resolve::custom_legacy_classes)
            .unwrap_or_default();
        let mut levels: Vec<(String, &StyleClass)> = vec![("theme defaults".to_owned(), &defaults)];
        levels.extend(owned.iter().map(|(k, c)| (k.clone(), c)));
        let legacy_levels: Vec<(String, &StyleClass)> = classes
            .iter()
            .filter(|c| legacy_custom.contains(c))
            .filter_map(|c| Some((format!("class {c}"), legacy_classes?.get(c)?)))
            .collect();
        levels.extend(legacy_levels);
        // `compose` masks hover, pressed and focus while disabled; the replay does the same.
        let (w, l) = replay_layers(&levels, state);
        winners = w;
        labels = l;
        if let Some(o) = ovr {
            let (ow, ol) = replay_layers(&[("ThemeOverride".to_owned(), &o.0)], state);
            labels.extend(ol);
            for (key, value) in ow {
                inline.push(key.clone());
                winners.insert(key, value);
            }
        }
        // Size tokens fill `height` and `padding_x` when the layers leave them unset.
        if let Some(size) = world.get::<WidgetSize>(e) {
            let idx = match size {
                WidgetSize::Sm => 0,
                WidgetSize::Md => 1,
                WidgetSize::Lg => 2,
            };
            for (prop, keys) in [
                (
                    "height",
                    ["size.control.sm", "size.control.md", "size.control.lg"],
                ),
                ("padding_x", ["size.pad-x.sm", "size.pad-x.md", "size.pad-x.lg"]),
            ] {
                if !winners.contains_key(prop)
                    && let Some(v) = theme.number(keys[idx])
                {
                    size_token.push((prop, v, keys[idx].to_owned()));
                }
            }
        }
        theme_info = json!({
            "classed": true,
            "theme": theme.name,
            "scope": scope,
            "classes": classes,
            "layers": levels.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            "rules": rules,
        });
    }
    let themed = world.get::<Themed>(e);
    let bg_inline = world.get::<Background>(e).and_then(|b| match &b.kind {
        BackgroundKind::Color(c) => Some(*c),
        _ => None,
    });
    let tweened: Vec<TweenProperty> = world
        .get::<UiTween>(e)
        .map(|t| t.plan.properties())
        .unwrap_or_default();
    let fading = themed.is_some_and(|t| t.anim_remaining > 0.0);
    let mut out = Vec::new();
    for name in VISUAL_PROPERTIES {
        let tween_prop = match name {
            "background" => Some(TweenProperty::Background),
            "border_color" => Some(TweenProperty::BorderColor),
            "text_color" => Some(TweenProperty::TextColor),
            "opacity" => Some(TweenProperty::Opacity),
            "scale" => Some(TweenProperty::Scale),
            _ => None,
        };
        let (mut value, mut source, mut detail) = match winners.get(name) {
            Some(v) => {
                let layer = labels
                    .iter()
                    .rev()
                    .find(|(k, _, _)| k == name)
                    .map(|(_, l, _)| l.clone())
                    .unwrap_or_default();
                if inline.iter().any(|k| k == name) {
                    (v.clone(), SourceKind::Inline, layer)
                } else {
                    (v.clone(), SourceKind::Theme, layer)
                }
            }
            None => (Value::Null, SourceKind::Default, String::new()),
        };
        if value.is_null()
            && let Some((_, v, key)) = size_token.iter().find(|(p, _, _)| *p == name)
        {
            value = json!(v);
            source = SourceKind::Theme;
            detail = format!("token {key}");
        }
        if name == "background"
            && value.is_null()
            && let Some(c) = bg_inline
        {
            value = serde_json::to_value(c).unwrap_or(Value::Null);
            source = SourceKind::Inline;
            detail = "Background component".to_owned();
        }
        if matches!(name, "background" | "border_color" | "text_color") && fading {
            source = SourceKind::Animation;
            detail = "theme colour transition running".to_owned();
        }
        if let Some(tp) = tween_prop
            && tweened.contains(&tp)
        {
            source = SourceKind::Animation;
            detail = "UiTween".to_owned();
        }
        let mut sources: Vec<Value> = labels
            .iter()
            .filter(|(key, _, _)| key == name)
            .map(|(_, label, value)| {
                style_source(resolved.as_ref().map(|(t, _)| t), name, label, value.clone())
            })
            .collect();
        if sources
            .last()
            .is_none_or(|s| s["detail"] != detail || s["source"] != source.name())
        {
            let token = detail.strip_prefix("token ");
            sources.push(json!({"source":source.name(), "kind":if token.is_some() {"token"} else {source.name()}, "detail":detail, "value":value, "tokens":token.into_iter().collect::<Vec<_>>()}));
        }
        let last = sources.len().saturating_sub(1);
        for (i, entry) in sources.iter_mut().enumerate() {
            entry["winning"] = json!(i == last);
        }
        history.insert(name.to_owned(), json!(sources));
        out.push(InspectedProperty {
            name: name.to_owned(),
            group: "visual",
            value,
            source,
            detail,
        });
    }
    (out, theme_info, history)
}

/// The layout (style) properties: a value that differs from `Style::default()` is inline.
fn layout_properties(world: &World, e: Entity) -> Vec<InspectedProperty> {
    let Some(style) = world.get::<NodeStyle>(e) else {
        return Vec::new();
    };
    let (Ok(Value::Object(now)), Ok(Value::Object(base))) = (
        serde_json::to_value(&style.0),
        serde_json::to_value(Style::default()),
    ) else {
        return Vec::new();
    };
    let binds: Vec<&Bind> = world.get::<Bind>(e).into_iter().collect();
    let tweened: Vec<TweenProperty> = world
        .get::<UiTween>(e)
        .map(|t| t.plan.properties())
        .unwrap_or_default();
    let tween_hits = |name: &str| {
        tweened.iter().any(|p| match name {
            "inset" => matches!(
                p,
                TweenProperty::InsetLeft
                    | TweenProperty::InsetTop
                    | TweenProperty::InsetRight
                    | TweenProperty::InsetBottom
            ),
            "size" => matches!(p, TweenProperty::Width | TweenProperty::Height),
            _ => false,
        })
    };
    now.into_iter()
        .map(|(name, value)| {
            let (source, detail) =
                if name == "display" && binds.iter().any(|b| b.target == BindTarget::Visible) {
                    (
                        SourceKind::Binding,
                        binds
                            .iter()
                            .find(|b| b.target == BindTarget::Visible)
                            .map(|b| b.path().to_owned())
                            .unwrap_or_default(),
                    )
                } else if tween_hits(&name) {
                    (SourceKind::Animation, "UiTween".to_owned())
                } else if base.get(&name) != Some(&value) {
                    (SourceKind::Inline, "NodeStyle".to_owned())
                } else {
                    (SourceKind::Default, String::new())
                };
            InspectedProperty {
                name,
                group: "layout",
                value,
                source,
                detail,
            }
        })
        .collect()
}

/// Properties that are not style: the text and the node's own opacity.
fn content_properties(world: &World, e: Entity) -> Vec<InspectedProperty> {
    let mut out = Vec::new();
    let bind_of = |t: BindTarget| world.get::<Bind>(e).filter(|b| b.target == t);
    let loc_text = world
        .get::<crate::TextInput>(e)
        .map(crate::TextInput::display_text)
        .or_else(|| {
            crate::agent_bridge::with_localization(world, |loc| crate::agent_bridge::node_text(world, e, loc))
        });
    if let Some(text) = loc_text {
        let target = if world.get::<crate::TextInput>(e).is_some() {
            BindTarget::Input
        } else {
            BindTarget::Text
        };
        let (source, detail) = match bind_of(target) {
            Some(b) => (SourceKind::Binding, b.path().to_owned()),
            None => (SourceKind::Inline, "Text".to_owned()),
        };
        out.push(InspectedProperty {
            name: "text".to_owned(),
            group: "content",
            value: json!(text),
            source,
            detail,
        });
    }
    if let Some(op) = world.get::<Opacity>(e) {
        let tweened = world
            .get::<UiTween>(e)
            .is_some_and(|t| t.plan.properties().contains(&TweenProperty::Opacity));
        let (source, detail) = if tweened {
            (SourceKind::Animation, "UiTween".to_owned())
        } else if let Some(b) = bind_of(BindTarget::Opacity) {
            (SourceKind::Binding, b.path().to_owned())
        } else {
            (SourceKind::Inline, "Opacity".to_owned())
        };
        out.push(InspectedProperty {
            name: "node_opacity".to_owned(),
            group: "content",
            value: json!(op.0),
            source,
            detail,
        });
    }
    out
}

fn bindings_json(world: &World, e: Entity) -> Vec<Value> {
    world
        .get::<Bind>(e)
        .map(|b| {
            json!({
                "source": b.path(),
                "source_kind": match &b.source {
                    crate::binding::BindSource::None => "none",
                    crate::binding::BindSource::Resource(_) => "resource",
                    crate::binding::BindSource::Component(_) => "component",
                    crate::binding::BindSource::Item(_) => "item",
                    crate::binding::BindSource::ScriptField(_) => "script_field",
                    crate::binding::BindSource::Theme(_) => "theme",
                    crate::binding::BindSource::Animation(_) => "animation",
                },
                "target": b.target.name(),
                "mode": b.mode.name(),
                "format": b.format,
                "invert": b.invert,
                "value": b.last_value(),
                "error": b.last_error(),
            })
        })
        .into_iter()
        .collect()
}

fn focus_json(world: &mut World, e: Entity) -> Value {
    let focusable = world.get::<Focusable>(e).copied();
    let focused = world.get_resource::<FocusState>().and_then(|f| f.focused);
    let links = focusable.map(|f| {
        json!({
            "up": f.nav.up.map(Entity::to_bits),
            "down": f.nav.down.map(Entity::to_bits),
            "left": f.nav.left.map(Entity::to_bits),
            "right": f.nav.right.map(Entity::to_bits),
        })
    });
    // Where each arrow press would go from this node, by the focus graph's geometric rule.
    let edges: Vec<Value> = match (focusable, world.get::<ComputedNode>(e).map(|c| c.rect)) {
        (Some(_), Some(rect)) => {
            let graph = crate::focus_graph::world_graph(world);
            crate::debug_overlay::focus_edges(&graph, e, rect)
                .into_iter()
                .map(|(d, t)| json!({ "direction": crate::focus_graph::direction_name(d), "entity": t.to_bits() }))
                .collect()
        }
        _ => Vec::new(),
    };
    json!({
        "focusable": focusable.is_some(),
        "focused": focused == Some(e),
        "explicit_links": links,
        "edges": edges,
        "interactable": world.get::<Interactable>(e).is_some(),
    })
}

fn accessibility_json(world: &mut World, e: Entity) -> Value {
    let tree = build_a11y_tree(world);
    if let Some(node) = tree.get(e.to_bits()) {
        return json!({
            "in_tree": true,
            "name": node.name,
            "role": node.role.map(crate::a11y::A11yRole::name),
            "description": node.description,
            "hint": node.hint,
            "value": node.value,
            "states": node.states.names(),
        });
    }
    let hidden = world.get::<Accessible>(e).is_some_and(|a| a.hidden);
    json!({ "in_tree": false, "hidden": hidden, "name": "", "role": null })
}

/// The chain of ancestors of `entity`, root first, ending with the node itself.
#[must_use]
pub fn node_path(world: &World, entity: Entity) -> Vec<Value> {
    let mut chain = Vec::new();
    let mut cur = Some(entity);
    while let Some(e) = cur {
        chain.push(json!({
            "id": e.to_bits(),
            "name": world.get::<NodeName>(e).map(|n| n.0.clone()),
            "type": crate::agent_bridge::node_kind(world, e),
        }));
        if chain.len() > 512 {
            break;
        }
        cur = world.get::<ChildOf>(e).map(ChildOf::parent);
    }
    chain.reverse();
    chain
}

/// The full inspection of one node (see the module docs). `entity` must be a UI node.
#[must_use]
pub fn inspect(world: &mut World, entity: Entity) -> Value {
    let _span = tracing::trace_span!("ui_inspect").entered();
    let state = state_of(world, entity);
    let (visual, theme, history) = visual_properties(world, entity, &state);
    let mut properties: Vec<InspectedProperty> = layout_properties(world, entity);
    properties.extend(visual);
    properties.extend(content_properties(world, entity));
    let computed = world.get::<ComputedNode>(entity).copied().unwrap_or_default();
    let rect = |r: [f32; 4]| json!({ "x": r[0], "y": r[1], "w": r[2], "h": r[3] });
    let live = world.get::<Themed>(entity).map(|t| {
        json!({
            "generation": t.generation,
            "radius": t.radius,
            "background": t.background,
            "opacity": t.opacity,
            "animating": t.anim_remaining > 0.0,
        })
    });
    let classes = world.get::<Classes>(entity).map_or(json!([]), |c| json!(c.0));
    let kind = crate::agent_bridge::node_kind(world, entity);
    let states = crate::agent_bridge::states_of(world, entity);
    let layout_defaults = serde_json::to_value(Style::default()).unwrap_or(Value::Null);
    let focus = focus_json(world, entity);
    let accessibility = accessibility_json(world, entity);
    json!({
        "node": entity.to_bits(),
        "name": world.get::<NodeName>(entity).map(|n| n.0.clone()),
        "type": kind,
        "path": node_path(world, entity),
        "classes": classes,
        "states": states,
        "layout": {
            "rect": rect(computed.rect),
            "clip_rect": rect(computed.clip_rect),
            "z": computed.z,
            "z_offset": world.get::<ZOffset>(entity).map(|z| z.0),
            "paint_order": computed.paint_order,
            "scale": computed.scale,
            "content_size": computed.content_size,
        },
        "theme": theme,
        "live": live,
        "properties": properties.iter().map(|p| {
            let mut out = p.json();
            out["sources"] = if p.group == "visual" { history.get(&p.name).cloned() } else { None }
                .unwrap_or_else(|| json!([{"source":p.source.name(), "kind":p.source.name(), "detail":p.detail, "value":p.value, "winning":true}]));
            if p.group == "layout" && p.source != SourceKind::Default
                && let Some(base) = layout_defaults.get(&p.name)
                && let Some(sources) = out["sources"].as_array_mut() {
                sources.insert(0,json!({"source":"default", "kind":"default", "detail":"Style default", "value":base, "winning":false}));
            }
            out
        }).collect::<Vec<_>>(),
        "events": inspect_events(world, entity)["events"],
        "document": document_origin(world, entity),
        "live_edits": world.get::<LiveEdits>(entity).map(|e| &e.0),
        "bindings": bindings_json(world, entity),
        "focus": focus,
        "accessibility": accessibility,
        // The layout variant the node's root shows (P-271, ADR-7858).
        "layout_variant": crate::layout_variants::inspect_json(world, entity),
    })
}

/// Every node whose rect and clip contain `p`, topmost first, by the interaction system's rule
/// (higher `z` wins, then later paint order). The inspector also sees non-interactable and
/// disabled nodes. With `top_layer_only`, nodes outside the top input layer are skipped.
#[must_use]
pub fn pick_stack(world: &mut World, p: [f32; 2], top_layer_only: bool) -> Vec<Entity> {
    let _span = tracing::trace_span!("ui_pick_at").entered();
    let top = if top_layer_only {
        world
            .get_resource::<crate::layers::InputLayers>()
            .and_then(crate::layers::InputLayers::top_root)
    } else {
        None
    };
    let mut nodes = world
        .query_filtered::<(Entity, &ComputedNode, Option<&crate::world_target::UiTargetNode>), With<UiNode>>(
        );
    let mut candidates: Vec<HitCandidate> = nodes
        .iter(world)
        .filter_map(|(entity, computed, tag)| {
            let (rect, clip) = crate::interaction::window_rects(computed.rect, computed.clip_rect, tag)?;
            Some(HitCandidate {
                entity,
                rect,
                clip,
                pick_through: false,
                disabled: false,
                z: computed.z,
                paint_order: computed.paint_order,
            })
        })
        .collect();
    if let Some(root) = top {
        candidates.retain(|c| crate::layers::within(world, root, c.entity));
    }
    let mut stack = Vec::new();
    // Peel the topmost repeatedly: the same rule as a pointer hit, one level at a time.
    while let Some(hit) = pick_topmost(candidates.iter().copied(), p) {
        stack.push(hit);
        candidates.retain(|c| c.entity != hit);
    }
    stack
}

/// The topmost node under `p` (see [`pick_stack`]).
#[must_use]
pub fn pick_at(world: &mut World, p: [f32; 2], top_layer_only: bool) -> Option<Entity> {
    pick_stack(world, p, top_layer_only).first().copied()
}

/// Whether `e` is a UI node (has the node marker).
#[must_use]
pub fn is_ui_node(world: &World, e: Entity) -> bool {
    world.get::<UiNode>(e).is_some() || world.get::<UiRoot>(e).is_some()
}

/// The theme state a replay would use, exposed for tests.
#[doc(hidden)]
#[must_use]
pub fn replay_state(world: &World, e: Entity) -> StyleState {
    state_of(world, e)
}

/// Source metadata for the retained theme cascade. Token names are read from authored expressions;
/// numeric equality never guesses a token (several tokens may have the same value).
fn style_source(theme: Option<&ResolvedTheme>, property: &str, label: &str, value: Value) -> Value {
    let mut source = "theme";
    let mut kind = "class";
    let mut tokens = Vec::new();
    if label == "ThemeOverride" || label.starts_with("ThemeOverride ") {
        source = "inline";
        kind = "inline";
    } else if label.starts_with("rule ") {
        // A `.theme.ron` v2 style rule; the label carries its selector text.
        kind = "rule";
    } else if label.starts_with("theme defaults") {
        kind = "default";
        let token = match (property, label.strip_prefix("theme defaults ")) {
            ("background", Some("selected")) => Some("color.selection"),
            ("border_color", Some("error")) => Some("color.danger"),
            ("text_color", Some("disabled")) => Some("color.text-disabled"),
            ("border_color", _) => Some("color.border"),
            ("radius", _) => Some("radius.md"),
            ("text_color", _) => Some("color.text"),
            ("focus_ring", _) => Some(if theme.is_some_and(|t| t.color("color.focus-ring").is_some()) {
                "color.focus-ring"
            } else {
                "color.accent"
            }),
            ("focus_ring_width", _) => Some("border.width.focus"),
            ("focus_ring_offset", _) => Some("border.width.focus-offset"),
            _ => None,
        };
        tokens.extend(token.map(str::to_owned));
    } else if let Some(theme) = theme {
        let (name, state) = label.split_once(' ').unwrap_or((label, ""));
        if theme.variations.contains_key(name) {
            kind = "variation";
        }
        if let Some(def) = theme.variations.get(name).or_else(|| theme.types.get(name)) {
            let props = match state {
                "hover" => Some(&def.hover),
                "pressed" => Some(&def.pressed),
                "focused" => Some(&def.focused),
                "focus_visible" => Some(&def.focus_visible),
                "disabled" => Some(&def.disabled),
                "selected" => Some(&def.selected),
                "error" => Some(&def.error),
                "sm" | "lg" => def.sizes.get(state),
                _ => Some(&def.props),
            };
            let alias = match property {
                "border_color" | "border_width" => "border",
                "focus_ring_width" | "focus_ring_offset" => "focus_ring",
                _ => property,
            };
            if let Some(prop) = props.and_then(|p| p.get(property).or_else(|| p.get(alias))) {
                token_references(prop, &mut tokens);
            }
        }
    }
    json!({"source":source, "kind":kind, "detail":label, "value":value, "tokens":tokens})
}

fn token_references(value: &crate::theme_file::PropValue, out: &mut Vec<String>) {
    use crate::theme_file::PropValue;
    match value {
        PropValue::Text(text) => {
            if let Ok(expression) = crate::theme_tokens::Expr::parse(text) {
                let mut references = Vec::new();
                expression.references(&mut references);
                for token in references {
                    if !out.contains(&token) {
                        out.push(token);
                    }
                }
            }
        }
        PropValue::List(list) => list.iter().for_each(|v| token_references(v, out)),
        PropValue::Number(_) => {}
    }
}

/// Maximum records retained across the runtime UI; older records are evicted first.
pub const INSPECT_EVENT_CAPACITY: usize = 1024;

/// Bounded event history for live inspection. Installed by `UiPlugin`; reads never drain it.
#[derive(Resource, Default)]
pub struct UiEventLog {
    records: std::collections::VecDeque<Value>,
    sequence: u64,
}

impl UiEventLog {
    /// Recent command records across the UI, oldest first. Reading does not drain the log.
    #[must_use]
    pub fn commands(&self) -> Vec<Value> {
        self.records
            .iter()
            .filter(|event| event["kind"] == "command")
            .cloned()
            .collect()
    }

    fn push(&mut self, node: Entity, frame: u64, kind: &str, value: Value) {
        if self.records.len() == INSPECT_EVENT_CAPACITY {
            self.records.pop_front();
        }
        self.sequence = self.sequence.saturating_add(1);
        self.records.push_back(json!({"node":node.to_bits(), "frame":frame, "sequence":self.sequence, "kind":kind, "value":value}));
    }
}

/// Captures events after the widget, focus and command systems, using simulation frame numbers.
pub(crate) fn record_events(
    mut events: MessageReader<crate::UiEvent>,
    mut commands: MessageReader<crate::UiCommand>,
    time: Res<regia_app::Time>,
    inputs: Query<&crate::TextInput>,
    mut log: ResMut<UiEventLog>,
) {
    let _span = tracing::trace_span!("ui_inspect_events").entered();
    use crate::UiEventKind as K;
    for event in events.read() {
        let masked = inputs.get(event.target).is_ok_and(|input| input.secret.is_some());
        let (kind, value) = match &event.kind {
            K::Click => ("click", Value::Null),
            K::PointerEnter => ("pointer_enter", Value::Null),
            K::PointerLeave => ("pointer_leave", Value::Null),
            K::DragStart => ("drag_start", Value::Null),
            K::Drag(delta) => ("drag", json!(delta)),
            K::DragEnd => ("drag_end", Value::Null),
            K::ValueChanged(crate::UiValue::Text(_)) if masked => ("value_changed", Value::Null),
            K::ValueChanged(value) => ("value_changed", event_value(value)),
            K::Submitted(_) if masked => ("submitted", Value::Null),
            K::Submitted(text) => ("submitted", json!(bounded_text(text))),
            K::FocusChanged(focused) => ("focus_changed", json!(focused)),
            K::DoubleClick => ("double_click", Value::Null),
            K::ContextClick => ("context_click", Value::Null),
            K::MiddleClick => ("middle_click", Value::Null),
            K::Scroll(delta) => ("scroll", json!(delta)),
            K::LongPress => ("long_press", Value::Null),
            K::Repeat(n) => ("repeat", json!(n)),
        };
        log.push(event.target, time.frame_count(), kind, value);
    }
    for command in commands.read() {
        log.push(
            command.node,
            time.frame_count(),
            "command",
            json!(bounded_text(&command.name)),
        );
    }
}

fn bounded_text(text: &str) -> String {
    text.chars().take(1024).collect()
}

fn event_value(value: &crate::UiValue) -> Value {
    match value {
        crate::UiValue::None => Value::Null,
        crate::UiValue::Bool(v) => json!(v),
        crate::UiValue::Number(v) => json!(v),
        crate::UiValue::Index(v) => json!(v),
        crate::UiValue::Text(v) => json!(bounded_text(v)),
    }
}

/// Recent events for `entity`, oldest first, filtered from the global bounded history.
#[must_use]
pub fn inspect_events(world: &World, entity: Entity) -> Value {
    let events: Vec<&Value> = world
        .get_resource::<UiEventLog>()
        .into_iter()
        .flat_map(|log| &log.records)
        .filter(|e| e["node"].as_u64() == Some(entity.to_bits()))
        .collect();
    json!({"node":entity.to_bits(), "capacity":INSPECT_EVENT_CAPACITY, "events":events})
}

/// A node's transient live patch. It is never written by the runtime or serialized into content.
#[derive(Component, Default)]
struct LiveEdits(std::collections::BTreeMap<String, Value>);

/// Applies one validated inline layout property (`style.<field>`) or plain `text` to a live node.
/// Does not change the library template or any file. Returns the refreshed inspection.
///
/// # Errors
/// `ui_node_not_found`, `ui_live_field` or `ui_live_value`; rejection leaves the node unchanged.
pub fn set_live_style(
    world: &mut World,
    entity: Entity,
    field: &str,
    value: Value,
) -> Result<Value, crate::agent_bridge::UiBridgeError> {
    use crate::agent_bridge::UiBridgeError;
    let bad = |reason, message| {
        UiBridgeError::invalid(
            reason,
            message,
            "use text or style.<field> with the field's JSON value",
        )
    };
    if !is_ui_node(world, entity) {
        return Err(UiBridgeError::not_found(
            "ui_node_not_found",
            "no UI node has that id",
            "query the live UI again",
        ));
    }
    if let Some(key) = field.strip_prefix("style.") {
        let mut style = serde_json::to_value(
            world
                .get::<NodeStyle>(entity)
                .map(|s| s.0.clone())
                .unwrap_or_default(),
        )
        .map_err(|e| bad("ui_live_value", e.to_string()))?;
        if style.get(key).is_none() {
            return Err(bad("ui_live_field", format!("unknown layout field {key}")));
        }
        style[key] = value.clone();
        let decoded: Style =
            serde_json::from_value(style).map_err(|e| bad("ui_live_value", e.to_string()))?;
        world.entity_mut(entity).insert(NodeStyle(decoded));
    } else if field == "text" {
        let text = value.as_str().filter(|s| s.len() <= 65536).ok_or_else(|| {
            bad(
                "ui_live_value",
                "text must be a string of at most 65536 bytes".to_owned(),
            )
        })?;
        if let Some(mut input) = world.get_mut::<crate::TextInput>(entity) {
            input.set_text(text);
        } else {
            let mut content = world.get::<crate::Text>(entity).cloned().unwrap_or_default();
            let mut span = content
                .spans
                .first()
                .cloned()
                .unwrap_or_else(|| crate::TextSpan::lit(text));
            span.content = crate::text::TextContent::Lit(text.to_owned());
            content.spans = vec![span];
            world.entity_mut(entity).insert(content);
        }
    } else {
        return Err(bad("ui_live_field", format!("unsupported live field {field}")));
    }
    let mut edits = world.get_mut::<LiveEdits>(entity);
    if let Some(edits) = edits.as_mut() {
        edits.0.insert(field.to_owned(), value);
    } else {
        let mut edits = LiveEdits::default();
        edits.0.insert(field.to_owned(), value);
        world.entity_mut(entity).insert(edits);
    }
    world
        .get_resource_or_insert_with(crate::layout::LayoutDirty::default)
        .mark();
    world
        .get_resource_or_insert_with(crate::extract::UiDamageSet::default)
        .full = true;
    world
        .get_resource_or_insert_with(crate::extract::UiDamage::default)
        .0 = true;
    Ok(inspect(world, entity))
}

/// Exact authored positions, captured before runtime widgets add their generated children.
#[derive(Component, Clone)]
struct DocumentNode {
    document: std::sync::Arc<str>,
    file: std::sync::Arc<crate::UiFile>,
    path: Vec<usize>,
}

pub(crate) fn tag_document_nodes(world: &mut World, root: Entity, template: &str) {
    let _span = tracing::trace_span!("ui_inspect_document_origins").entered();
    let Some((document, file)) = world
        .get_resource::<crate::authoring::UiLibrary>()
        .and_then(|lib| lib.source(template))
        .map(|(path, file)| {
            (
                std::sync::Arc::<str>::from(path),
                std::sync::Arc::new(file.clone()),
            )
        })
    else {
        return;
    };
    fn tag(
        world: &mut World,
        entity: Entity,
        def: &crate::authoring::UiNodeDef,
        document: &std::sync::Arc<str>,
        file: &std::sync::Arc<crate::UiFile>,
        path: &mut Vec<usize>,
    ) {
        // Expanded components and generated repeat children have no exact inline destination.
        if def.use_component.is_some() {
            return;
        }
        world.entity_mut(entity).insert(DocumentNode {
            document: document.clone(),
            file: file.clone(),
            path: path.clone(),
        });
        if def.repeat.is_some() {
            return;
        }
        let children: Vec<Entity> = world
            .get::<Children>(entity)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        for (i, (child, def)) in children.into_iter().zip(&def.children).enumerate() {
            path.push(i);
            tag(world, child, def, document, file, path);
            path.pop();
        }
    }
    tag(world, root, &file.root, &document, &file, &mut Vec::new());
}

fn document_origin(world: &World, entity: Entity) -> Value {
    let Some(origin) = world.get::<DocumentNode>(entity) else {
        return Value::Null;
    };
    let mut def = &origin.file.root;
    for index in &origin.path {
        let Some(child) = def.children.get(*index) else {
            return Value::Null;
        };
        def = child;
    }
    let path = format!(
        "/{}",
        origin
            .path
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join("/")
    );
    json!({"path":origin.document.as_ref(),"node":path,
        "expected":{"style":def.style,"text":def.text,"name":def.name,"widget":def.widget}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(crate::UiPlugin::code_only());
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        app
    }

    fn prop<'a>(out: &'a Value, name: &str) -> &'a Value {
        out["properties"]
            .as_array()
            .and_then(|a| a.iter().find(|p| p["name"] == name))
            .unwrap_or(&Value::Null)
    }

    #[test]
    fn inspect_retains_losing_style_sources() {
        let mut app = app();
        let node = app
            .world_mut()
            .spawn((
                UiNode,
                NodeStyle::default(),
                Classes(vec!["button".into()]),
                ThemeOverride::new(VisualOverrides {
                    radius: Some(3.0),
                    ..Default::default()
                }),
            ))
            .id();
        app.update();
        app.update();
        let out = inspect(app.world_mut(), node);
        let sources = prop(&out, "radius")["sources"]
            .as_array()
            .expect("ordered source history");
        assert!(
            sources.len() >= 2,
            "theme radius must remain behind the inline winner: {sources:?}"
        );
        assert_eq!(sources.last().expect("winner")["source"], "inline");
        assert!(sources[..sources.len() - 1].iter().all(|s| s["winning"] == false));
    }

    #[test]
    fn inspect_records_events_and_commands_with_frame_timestamps() {
        let mut app = app();
        let node = app
            .world_mut()
            .spawn((UiNode, crate::UiCommandName("accept".into())))
            .id();
        app.world_mut()
            .write_message(crate::UiEvent::new(node, crate::UiEventKind::Click));
        app.update();
        let out = inspect(app.world_mut(), node);
        let events = out["events"].as_array().expect("recent events");
        assert!(
            events.iter().any(|e| e["kind"] == "click" && e["frame"].is_u64()),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e["kind"] == "command" && e["value"] == "accept"),
            "{events:?}"
        );
    }

    #[test]
    fn sources_follow_theme_inline_default_and_binding() {
        let mut app = app();
        let button = app
            .world_mut()
            .spawn((
                UiNode,
                NodeStyle(Style {
                    flex_grow: 2.0,
                    ..Default::default()
                }),
                Classes(vec!["button".into(), "primary".into()]),
                Bind {
                    target: BindTarget::Opacity,
                    ..Bind::text("Model.alpha", None)
                },
                Opacity(0.5),
                ThemeOverride::new(VisualOverrides {
                    radius: Some(3.0),
                    ..Default::default()
                }),
                NodeName("save".into()),
            ))
            .id();
        app.update();
        app.update();
        let out = inspect(app.world_mut(), button);
        let radius = prop(&out, "radius");
        assert_eq!(radius["source"], "inline", "{radius}");
        assert_eq!(radius["detail"], "ThemeOverride");
        let grow = prop(&out, "flex_grow");
        assert_eq!(grow["source"], "inline");
        assert_eq!(prop(&out, "flex_shrink")["source"], "default");
        let bg = prop(&out, "background");
        assert_eq!(bg["source"], "theme", "{bg}");
        assert!(
            bg["detail"].as_str().is_some_and(|d| d.contains("Button")),
            "{bg}"
        );
        let text_color = prop(&out, "text_color");
        assert_eq!(text_color["source"], "theme");
        assert_eq!(prop(&out, "node_opacity")["source"], "binding");
        assert_eq!(out["bindings"][0]["target"], "opacity");
        assert_eq!(out["name"], "save");
    }

    #[test]
    fn replayed_theme_values_equal_the_live_look() {
        let mut app = app();
        let button = app
            .world_mut()
            .spawn((UiNode, NodeStyle::default(), Classes(vec!["button".into()])))
            .id();
        app.update();
        app.update();
        let out = inspect(app.world_mut(), button);
        let themed = *app.world().get::<Themed>(button).expect("themed");
        let radius = prop(&out, "radius")["value"].as_f64().map(|v| v as f32);
        assert_eq!(radius, themed.radius);
        assert!(radius.is_some(), "the button type sets a radius");
        let hover_free = prop(&out, "background")["detail"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(!hover_free.contains("hover"));
    }

    #[test]
    fn a_state_layer_names_itself_as_the_source() {
        let mut app = app();
        let button = app
            .world_mut()
            .spawn((
                UiNode,
                NodeStyle::default(),
                Classes(vec!["button".into()]),
                crate::node::Disabled,
            ))
            .id();
        app.update();
        app.update();
        let out = inspect(app.world_mut(), button);
        let opacity = prop(&out, "opacity");
        assert_eq!(opacity["source"], "theme", "{opacity}");
        assert!(
            opacity["detail"]
                .as_str()
                .is_some_and(|d| d.ends_with("disabled")),
            "{opacity}"
        );
    }

    #[test]
    fn pick_at_returns_the_topmost_of_overlapping_nodes() {
        let mut app = app();
        let node = |z: f32, order: u32, rect: [f32; 4]| {
            (
                UiNode,
                ComputedNode {
                    rect,
                    clip_rect: [0.0, 0.0, 1000.0, 1000.0],
                    z,
                    paint_order: order,
                    scale: 1.0,
                    ..Default::default()
                },
            )
        };
        let low = app.world_mut().spawn(node(1.0, 0, [0.0, 0.0, 100.0, 100.0])).id();
        let high = app
            .world_mut()
            .spawn(node(2.0, 1, [50.0, 50.0, 100.0, 100.0]))
            .id();
        let late = app.world_mut().spawn(node(2.0, 2, [60.0, 60.0, 10.0, 10.0])).id();
        assert_eq!(pick_at(app.world_mut(), [10.0, 10.0], false), Some(low));
        assert_eq!(pick_at(app.world_mut(), [55.0, 55.0], false), Some(high));
        assert_eq!(
            pick_stack(app.world_mut(), [65.0, 65.0], false),
            vec![late, high, low]
        );
        assert_eq!(pick_at(app.world_mut(), [500.0, 500.0], false), None);
    }

    #[test]
    fn path_lists_ancestors_root_first() {
        let mut app = app();
        let root = app.world_mut().spawn((UiNode, NodeName("root".into()))).id();
        let child = app.world_mut().spawn((UiNode, NodeName("kid".into()))).id();
        app.world_mut().entity_mut(root).add_child(child);
        let path = node_path(app.world(), child);
        assert_eq!(path.len(), 2);
        assert_eq!(path[0]["name"], "root");
        assert_eq!(path[1]["id"], child.to_bits());
    }
}

#[cfg(test)]
mod live_inspection_tests {
    use super::*;

    #[test]
    fn inspect_event_log_omits_masked_input_text() {
        let mut app = App::new();
        app.add_message::<crate::UiEvent>()
            .add_message::<crate::UiCommand>()
            .init_resource::<UiEventLog>()
            .add_systems(Last, record_events);
        let node = app
            .world_mut()
            .spawn((
                UiNode,
                crate::TextInput {
                    secret: Some('*'),
                    ..Default::default()
                },
            ))
            .id();
        for kind in [
            crate::UiEventKind::Submitted("private value".into()),
            crate::UiEventKind::ValueChanged(crate::UiValue::Text("private value".into())),
        ] {
            app.world_mut().write_message(crate::UiEvent::new(node, kind));
        }
        app.update();
        let out = inspect_events(app.world(), node);
        let events = out["events"].as_array().expect("events");
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| event["value"].is_null()));
        assert!(!out.to_string().contains("private value"));
    }

    #[test]
    fn inspect_event_ring_is_bounded_ordered_and_non_draining() {
        let mut world = World::new();
        let node = world.spawn(UiNode).id();
        let other = world.spawn(UiNode).id();
        let mut log = UiEventLog::default();
        for frame in 0..(INSPECT_EVENT_CAPACITY as u64 + 9) {
            log.push(node, frame, "click", Value::Null);
        }
        log.push(other, 2000, "focus_changed", json!(true));
        world.insert_resource(log);
        let first = inspect_events(&world, node);
        assert_eq!(first, inspect_events(&world, node));
        let events = first["events"].as_array().expect("events");
        assert_eq!(events.len(), INSPECT_EVENT_CAPACITY - 1);
        assert_eq!(events[0]["frame"], 10);
        assert!(
            events
                .windows(2)
                .all(|pair| pair[0]["sequence"].as_u64() < pair[1]["sequence"].as_u64())
        );
        assert_eq!(
            inspect_events(&world, other)["events"]
                .as_array()
                .expect("other")
                .len(),
            1
        );
    }

    #[test]
    fn inspect_live_rejection_is_atomic_and_text_edits_reach_input_widgets() {
        let mut world = World::new();
        let node = world
            .spawn((UiNode, NodeStyle::default(), crate::TextInput::default()))
            .id();
        let before = world.get::<NodeStyle>(node).expect("style").0.clone();
        let err =
            set_live_style(&mut world, node, "style.flex_grow", json!("wrong type")).expect_err("invalid");
        assert_eq!(err.reason, "ui_live_value");
        assert_eq!(world.get::<NodeStyle>(node).expect("style").0, before);
        assert!(world.get::<LiveEdits>(node).is_none());
        assert_eq!(
            set_live_style(&mut world, node, "unknown", json!(2))
                .expect_err("unknown")
                .reason,
            "ui_live_field"
        );
        let out = set_live_style(&mut world, node, "text", json!("Live text")).expect("edit");
        assert_eq!(
            world.get::<crate::TextInput>(node).expect("input").display_text(),
            "Live text"
        );
        assert_eq!(out["live_edits"]["text"], "Live text");
        assert!(
            world.get::<crate::Text>(node).is_none(),
            "the input owns its label; no duplicate root text"
        );
        assert!(
            out["properties"]
                .as_array()
                .expect("properties")
                .iter()
                .any(|p| p["name"] == "text" && p["value"] == "Live text")
        );
    }
}

#[cfg(test)]
mod document_inspection_tests {
    use super::*;

    #[test]
    fn inspect_document_origin_and_live_patch_leave_the_library_unchanged() {
        let mut app = App::new();
        app.add_plugins(crate::UiPlugin::code_only());
        let file = crate::UiFile {
            version: crate::UI_RON_VERSION,
            component: None,
            variants: Vec::new(),
            root: crate::authoring::UiNodeDef {
                children: vec![crate::authoring::UiNodeDef {
                    name: Some("label".into()),
                    text: Some(crate::authoring::TextDef::literal("Original")),
                    ..Default::default()
                }],
                ..Default::default()
            },
            back: None,
        };
        crate::authoring::insert_ui_template(app.world_mut(), "live", "ui/live.ui.ron", file.clone());
        let root = crate::authoring::spawn_ui(app.world_mut(), "live").expect("spawn");
        let child = app.world().get::<Children>(root).expect("children")[0];
        let out = set_live_style(app.world_mut(), child, "text", json!("Transient")).expect("edit");
        assert_eq!(out["document"]["path"], "ui/live.ui.ron");
        assert_eq!(out["document"]["node"], "/0");
        assert_eq!(out["document"]["expected"]["name"], "label");
        assert_eq!(
            app.world()
                .resource::<crate::UiLibrary>()
                .source("live")
                .expect("source")
                .1,
            &file
        );
        assert_eq!(app.world().resource::<crate::UiLibrary>().templates["live"], file);
        let mut newer = file.clone();
        newer.root.children[0].name = Some("Replaced".into());
        crate::authoring::insert_ui_template(app.world_mut(), "live", "ui/live.ui.ron", newer);
        assert_eq!(
            inspect(app.world_mut(), child)["document"]["expected"]["name"],
            "label",
            "origin stays at spawn time after a library replacement"
        );
    }
}
