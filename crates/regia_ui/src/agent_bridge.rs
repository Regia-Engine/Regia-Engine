// SPDX-License-Identifier: MPL-2.0
//! The agent bridge (ADR-273 Lane 1, ADR-291): how an agent sees and drives the runtime UI
//! without `regia_agent` depending on this crate (ADR-202).
//!
//! The agent crate owns the `ui.*` JSON-RPC methods and, for each call, invokes one function
//! pointer the host registered ([`host_call`] is the one to register). Reads (`ui.query`,
//! `ui.state`, the `ui.node_exists` / `ui.node_state` predicates) are answered straight from the
//! world on demand, so an idle UI pays nothing. Writes never touch widget state directly: they
//! expand into per-frame input steps on the [`UiAgentBridge`] queue, and [`drain_bridge`] feeds
//! the steps into the same platform messages the pointer, keyboard and IME paths already read
//! (`CursorMoved`, `MouseButtonInput`, `KeyboardInput`, `ImeEvent`). A click is therefore a real
//! click: hit testing, hover, press, `UiEvent::Click` and widget reactions all run as for a
//! person. Focus and `ui.set_value` have no platform message, so the drain applies them with the
//! same component and event writes the focus and widget systems make.
//!
//! See `book/agents/ui.md`.

use std::collections::VecDeque;

use regia_app::prelude::*;
use serde_json::{Map, Value, json};

use crate::dragdrop::{DragDrop, DragSource, DropTarget, cancel_drag};
use crate::focus::{
    DEFAULT_FOCUS_RING_WIDTH, FocusRingPolicy, FocusState, Focusable, UiInputMode, focus_ring_geometry,
};
use crate::interaction::{HitCandidate, UiEvent, UiEventKind, UiPointer, UiValue, pick_topmost};
use crate::layout::LayoutDirty;
use crate::localization::Localization;
use crate::node::{
    Classes, ComputedNode, Disabled, Focused, Hovered, Interactable, Invalid, NodeName, Pressed, Selected,
    UiImage, UiNode, UiRoot,
};
use crate::scale::UiScales;
use crate::scroll::{ListView, ScrollView};
use crate::text::{Text, TextContent};
use crate::theme::Themed;
use crate::widgets::{Button, Dropdown, Modal, ProgressBar, Slider, TabBar, TextInput, Toggle};

/// Most nodes `ui.query` returns in one reply.
const MAX_QUERY_LIMIT: usize = 1000;
/// Nodes `ui.query` returns when `limit` is omitted.
const DEFAULT_QUERY_LIMIT: usize = 100;
/// Most clicks one `ui.click` may queue.
const MAX_CLICK_COUNT: u64 = 8;

/// The states a node can be in, in the order `ui.query` lists them.
const STATE_NAMES: [&str; 6] = ["hovered", "pressed", "focused", "disabled", "selected", "invalid"];

/// The error type of the bridge: stable snake_case `reason`, an error `class` the agent crate maps
/// onto a JSON-RPC error, a message and a hint.
#[derive(Debug, Clone, PartialEq)]
pub struct UiBridgeError {
    /// `"invalid_params"`, `"not_found"` or `"rejected"` (the request was valid but the UI cannot do it now).
    pub class: &'static str,
    /// Stable lowercase snake_case reason, documented in `book/agents/ui.md`.
    pub reason: &'static str,
    /// What went wrong.
    pub message: String,
    /// How to fix the call.
    pub hint: String,
}

impl UiBridgeError {
    fn new(
        class: &'static str,
        reason: &'static str,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            class,
            reason,
            message: message.into(),
            hint: hint.into(),
        }
    }

    pub(crate) fn invalid(reason: &'static str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new("invalid_params", reason, message, hint)
    }

    pub(crate) fn not_found(
        reason: &'static str,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self::new("not_found", reason, message, hint)
    }

    fn rejected(reason: &'static str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new("rejected", reason, message, hint)
    }

    /// `(class, reason, message, hint)`: the shape the agent crate's host hook returns.
    pub fn into_parts(self) -> (&'static str, &'static str, String, String) {
        (self.class, self.reason, self.message, self.hint)
    }
}

/// What a host hook returns for a failed call: `(class, reason, message, hint)`.
pub type HostError = (&'static str, &'static str, String, String);

/// The function a host registers with the agent crate: `regia::agent::UiAgentHost::new(host_call)`.
pub fn host_call(world: &mut World, method: &str, params: &Value) -> Result<Value, HostError> {
    call(world, method, params).map_err(UiBridgeError::into_parts)
}

/// One input the drain feeds into the UI's input path.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    MoveCursor([f32; 2]),
    Button {
        button: regia_platform::MouseButton,
        pressed: bool,
    },
    Key {
        logical: String,
        physical: String,
    },
    /// One modifier key held down (`pressed`) or let go, around a `ui.click`.
    Modifier {
        logical: &'static str,
        pressed: bool,
    },
    Commit(String),
    Focus(Entity),
    SetValue(Entity, UiValue),
}

/// The queue of agent-driven UI input. Each entry is one frame's worth of steps; the drain runs the
/// oldest entry in `PreUpdate`, before the pointer is read. Empty (and free) unless an agent writes.
#[derive(Resource, Debug, Default)]
pub struct UiAgentBridge {
    frames: VecDeque<Vec<Step>>,
}

impl UiAgentBridge {
    /// Frames of input still waiting to be fed to the UI.
    pub fn pending_frames(&self) -> usize {
        self.frames.len()
    }
}

/// Installs the [`UiAgentBridge`] queue and its drain system. [`crate::UiPlugin`] adds it.
pub struct UiAgentBridgePlugin;

impl Plugin for UiAgentBridgePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<UiAgentBridge>().add_systems(
            PreUpdate,
            drain_bridge
                .run_if(bridge_has_work)
                .before(crate::interaction::update_pointer),
        );
    }
}

fn bridge_has_work(bridge: Res<UiAgentBridge>) -> bool {
    !bridge.frames.is_empty()
}

/// `PreUpdate` (exclusive, only when the queue is not empty): feeds one frame of queued steps.
pub fn drain_bridge(world: &mut World) {
    let _span = tracing::trace_span!("ui_agent_bridge").entered();
    let Some(steps) = world.resource_mut::<UiAgentBridge>().frames.pop_front() else {
        return;
    };
    let window = primary_window(world);
    for step in steps {
        apply_step(world, window, step);
    }
}

fn primary_window(world: &mut World) -> Entity {
    world
        .query_filtered::<Entity, With<regia_platform::PrimaryWindow>>()
        .iter(world)
        .next()
        .unwrap_or(Entity::PLACEHOLDER)
}

fn apply_step(world: &mut World, window: Entity, step: Step) {
    let timestamp = regia_platform::InputTimestamp::ZERO;
    match step {
        Step::MoveCursor(p) => {
            let _ = world.write_message(regia_platform::CursorMoved {
                window,
                position: (f64::from(p[0]), f64::from(p[1])),
                timestamp,
            });
        }
        Step::Button { button, pressed } => {
            let _ = world.write_message(regia_platform::MouseButtonInput {
                window,
                button,
                state: button_state(pressed),
                timestamp,
            });
        }
        Step::Key { logical, physical } => {
            for pressed in [true, false] {
                let _ = world.write_message(regia_platform::KeyboardInput {
                    window,
                    physical_key: physical.clone(),
                    logical_key: logical.clone(),
                    state: button_state(pressed),
                    repeat: false,
                    timestamp,
                });
            }
        }
        Step::Modifier { logical, pressed } => {
            let _ = world.write_message(regia_platform::KeyboardInput {
                window,
                physical_key: format!("{logical}Left"),
                logical_key: logical.to_owned(),
                state: button_state(pressed),
                repeat: false,
                timestamp,
            });
        }
        Step::Commit(text) => {
            let _ = world.write_message(regia_platform::ImeEvent::Commit(window, text));
        }
        Step::Focus(entity) => apply_focus(world, entity),
        Step::SetValue(entity, value) => apply_value(world, entity, value),
    }
}

fn button_state(pressed: bool) -> regia_platform::ButtonState {
    if pressed {
        regia_platform::ButtonState::Pressed
    } else {
        regia_platform::ButtonState::Released
    }
}

/// Moves focus the way `focus::set_focus` does (the `Focused` marker, `FocusChanged` events, the resource).
/// Like a Tab press it switches the input mode to navigation, so the focus ring shows (D9).
pub(crate) fn apply_focus(world: &mut World, next: Entity) {
    if let Some(mut mode) = world.get_resource_mut::<UiInputMode>()
        && *mode != UiInputMode::Navigation
    {
        *mode = UiInputMode::Navigation;
    }
    let prev = world.resource::<FocusState>().focused;
    if prev == Some(next) {
        return;
    }
    if let Some(p) = prev
        && let Ok(mut entity) = world.get_entity_mut(p)
    {
        entity.remove::<Focused>();
    }
    if let Ok(mut entity) = world.get_entity_mut(next) {
        entity.insert(Focused);
    }
    if let Some(p) = prev {
        let _ = world.write_message(UiEvent::new(p, UiEventKind::FocusChanged(false)));
    }
    let _ = world.write_message(UiEvent::new(next, UiEventKind::FocusChanged(true)));
    world.resource_mut::<FocusState>().focused = Some(next);
}

/// Sets a widget's value and emits the `ValueChanged` event its own input path would.
pub(crate) fn apply_value(world: &mut World, entity: Entity, value: UiValue) {
    // The D11 widgets (radio, spin box, foldable, split) and the W11 breadcrumbs emit their
    // own `ValueChanged`.
    if crate::default_widgets::apply_widget_value(world, entity, &value).is_some() {
        world.resource_mut::<LayoutDirty>().mark();
        return;
    }
    let changed = match value {
        UiValue::Bool(checked) => world.get_mut::<Toggle>(entity).map(|mut toggle| {
            toggle.checked = checked;
            UiValue::Bool(checked)
        }),
        UiValue::Number(number) => {
            if let Some(mut slider) = world.get_mut::<Slider>(entity) {
                let stepped = slider.value_near(number);
                slider.value = stepped;
                Some(UiValue::Number(stepped))
            } else if let Some(mut bar) = world.get_mut::<ProgressBar>(entity) {
                bar.value = number.clamp(0.0, 1.0);
                None
            } else {
                None
            }
        }
        UiValue::Index(index) => {
            if let Some(mut dropdown) = world.get_mut::<Dropdown>(entity) {
                dropdown.selected = Some(index);
                Some(UiValue::Index(index))
            } else {
                world.get_mut::<TabBar>(entity).map(|mut bar| {
                    bar.active = index;
                    UiValue::Index(index)
                })
            }
        }
        UiValue::Text(text) => set_text_input(world, entity, &text),
        UiValue::None => None,
    };
    if let Some(value) = changed {
        let _ = world.write_message(UiEvent::new(entity, UiEventKind::ValueChanged(value)));
    }
    world.resource_mut::<LayoutDirty>().mark();
}

fn set_text_input(world: &mut World, entity: Entity, text: &str) -> Option<UiValue> {
    let (shown, placeholder, content) = {
        let mut input = world.get_mut::<TextInput>(entity)?;
        let kept = input.set_external_text(text)?;
        (input.display_text(), input.placeholder.clone(), kept)
    };
    let shown = if shown.is_empty() { placeholder } else { shown };
    let children: Vec<Entity> = world
        .get::<Children>(entity)
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    for child in children {
        if let Some(mut label) = world.get_mut::<Text>(child) {
            label.spans = vec![crate::text::TextSpan::lit(shown.clone())];
        }
    }
    Some(UiValue::Text(content))
}

// ---------------------------------------------------------------------------------------------
// Methods.
// ---------------------------------------------------------------------------------------------

/// Runs one `ui.*` method. Reads answer immediately; writes validate, queue input steps and
/// return what was queued (the effect shows after the next frame).
pub fn call(world: &mut World, method: &str, params: &Value) -> Result<Value, UiBridgeError> {
    let _span = tracing::trace_span!("ui_agent_call").entered();
    match method {
        "ui.query" => query(world, params),
        "ui.state" => Ok(state(world)),
        "ui.click" => click(world, params),
        // Camera-target and world-space UI (ADR-2406, book/agents/ui.md).
        "ui.world_click" => crate::world_target::world_click(world, params),
        "ui.set_target" => crate::world_target::set_target(world, params),
        "ui.targets" => Ok(crate::world_target::targets(world)),
        "ui.focus" => focus(world, params),
        "ui.type" => type_text(world, params),
        "ui.set_value" => set_value(world, params),
        "ui.set_locale" => set_locale(world, params),
        "ui.resolve" => resolve(world, params),
        "ui.set_backdrop_blur" => set_backdrop_blur(world, params),
        "ui.backdrop_stats" => Ok(backdrop_stats(world)),
        "ui.node_exists" => node_exists(world, params),
        "ui.node_state" => node_state(world, params),
        // Popups and menus (book/agents/ui-menus.md).
        "ui.open_popup" => crate::menu::agent_open_popup(world, params, single),
        "ui.close_popup" => crate::menu::agent_close_popup(world, params),
        "ui.menu" => crate::menu::agent_menu(world, params, single),
        "ui.menu_activate" => crate::menu::agent_menu_activate(world, params, single),
        "ui.drag_drop" => drag_drop(world, params),
        // View models and bindings (ADR-1811, book/agents/ui-designer.md).
        "ui.view_models" => Ok(crate::viewmodel::agent_view_models(world)),
        // Collection item templates (ADR-2489, book/agents/ui-designer.md).
        "ui.collection_state" => Ok(crate::repeat::agent_collection_state(world, params)),
        // Group opacity, masks and scale modes (ADR-2866, book/agents/ui-group-layers.md).
        "ui.set_group" => crate::group_layer::agent_set_group(world, params, single),
        "ui.layer_stats" => Ok(crate::group_layer::agent_layer_stats(world)),
        "ui.set_scale_mode" => crate::group_layer::agent_set_scale_mode(world, params),
        // Images and masks named by asset path or icon (ADR-3148, book/agents/ui-image-assets.md).
        "ui.image_assets" => crate::ui_image_assets::agent_image_assets(world, params),
        // Themes (lane D7): `ui.theme_*` and the `ui.theme_generation` predicate.
        theme if crate::theme_agent::handles(theme) => crate::theme_agent::call(world, theme, params),
        other => Err(UiBridgeError::not_found(
            "ui_unknown_method",
            format!("`{other}` is not a ui method"),
            "ui.query, ui.state, ui.click, ui.focus, ui.type, ui.set_value, ui.set_locale, ui.resolve, ui.set_backdrop_blur, ui.backdrop_stats, ui.open_popup, ui.close_popup, ui.menu, ui.menu_activate, ui.drag_drop, ui.set_group, ui.layer_stats, ui.set_scale_mode, ui.image_assets",
        )),
    }
}

/// How to pick nodes: every given field must match.
#[derive(Debug, Default)]
struct Filter {
    node: Option<u64>,
    name: Option<String>,
    text: Option<String>,
    text_contains: Option<String>,
    class: Option<String>,
    kind: Option<String>,
    states: Vec<&'static str>,
}

fn string_param(params: &Value, key: &str) -> Result<Option<String>, UiBridgeError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(UiBridgeError::invalid(
            "ui_bad_param",
            format!("`{key}` must be a string"),
            "see the ui.query parameters in agent.schema",
        )),
    }
}

fn state_name(name: &str) -> Result<&'static str, UiBridgeError> {
    let lower = name.to_ascii_lowercase();
    STATE_NAMES
        .iter()
        .find(|known| **known == lower)
        .copied()
        .ok_or_else(|| {
            UiBridgeError::invalid(
                "ui_unknown_state",
                format!("`{name}` is not a node state"),
                "states: hovered, pressed, focused, disabled, selected, invalid",
            )
        })
}

fn parse_filter(params: &Value) -> Result<Filter, UiBridgeError> {
    let node = match params.get("node").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| {
            UiBridgeError::invalid(
                "ui_bad_param",
                "`node` must be a node id (the `id` field of ui.query)",
                "call ui.query to list node ids",
            )
        })?),
    };
    let mut states = Vec::new();
    match params.get("state").filter(|v| !v.is_null()) {
        None => {}
        Some(Value::String(s)) => states.push(state_name(s)?),
        Some(Value::Array(list)) => {
            for item in list {
                let name = item.as_str().ok_or_else(|| {
                    UiBridgeError::invalid(
                        "ui_bad_param",
                        "`state` entries must be strings",
                        "e.g. [\"focused\"]",
                    )
                })?;
                states.push(state_name(name)?);
            }
        }
        Some(_) => {
            return Err(UiBridgeError::invalid(
                "ui_bad_param",
                "`state` must be a state name or a list of them",
                "e.g. \"focused\" or [\"hovered\", \"pressed\"]",
            ));
        }
    }
    Ok(Filter {
        node,
        name: string_param(params, "name")?,
        text: string_param(params, "text")?,
        text_contains: string_param(params, "text_contains")?,
        class: string_param(params, "class")?,
        kind: string_param(params, "type")?,
        states,
    })
}

/// Every UI entity, in a stable order (entity id).
fn ui_entities(world: &mut World) -> Vec<Entity> {
    let mut list: Vec<Entity> = world
        .query_filtered::<Entity, Or<(With<UiNode>, With<UiRoot>)>>()
        .iter(world)
        .collect();
    list.sort_by_key(|e| e.to_bits());
    list
}

pub(crate) fn node_kind(world: &World, e: Entity) -> &'static str {
    if world.get::<UiRoot>(e).is_some() {
        "Root"
    } else if let Some(kind) = crate::default_widgets::widget_kind(world, e) {
        kind
    } else if world.get::<Button>(e).is_some() {
        "Button"
    } else if world.get::<Toggle>(e).is_some() {
        "Toggle"
    } else if world.get::<Slider>(e).is_some() {
        "Slider"
    } else if world.get::<ProgressBar>(e).is_some() {
        "ProgressBar"
    } else if world.get::<TextInput>(e).is_some() {
        "TextInput"
    } else if world.get::<Dropdown>(e).is_some() {
        "Dropdown"
    } else if world.get::<TabBar>(e).is_some() {
        "TabBar"
    } else if world.get::<Modal>(e).is_some() {
        "Modal"
    } else if world.get::<ListView>(e).is_some() {
        "ListView"
    } else if world.get::<ScrollView>(e).is_some() {
        "ScrollView"
    } else if world.get::<Text>(e).is_some() {
        "Text"
    } else if world.get::<UiImage>(e).is_some() {
        "Image"
    } else {
        "Node"
    }
}

pub(crate) fn states_of(world: &World, e: Entity) -> Vec<&'static str> {
    let flags = [
        world.get::<Hovered>(e).is_some(),
        world.get::<Pressed>(e).is_some(),
        world.get::<Focused>(e).is_some(),
        crate::node::is_disabled(world, e),
        world.get::<Selected>(e).is_some(),
        world.get::<Invalid>(e).is_some(),
    ];
    STATE_NAMES
        .iter()
        .zip(flags)
        .filter_map(|(name, on)| on.then_some(*name))
        .collect()
}

fn resolved_text(text: &Text, loc: &Localization) -> String {
    text.spans
        .iter()
        .map(|span| match &span.content {
            TextContent::Lit(s) => s.clone(),
            content => content.resolve(loc),
        })
        .collect()
}

/// A node's text: its own `Text`, or else the text of its childless `Text` children (a button's label).
pub(crate) fn node_text(world: &World, e: Entity, loc: &Localization) -> Option<String> {
    if let Some(text) = world.get::<Text>(e) {
        return Some(resolved_text(text, loc));
    }
    let children = world.get::<Children>(e)?;
    let labels: Vec<String> = children
        .iter()
        .filter(|c| world.get::<Children>(*c).is_none_or(|k| k.is_empty()))
        .filter_map(|c| world.get::<Text>(c).map(|t| resolved_text(t, loc)))
        .collect();
    if labels.is_empty() {
        None
    } else {
        Some(labels.join(" "))
    }
}

/// Whether `e` is a childless `Text` whose parent shows no text of its own but takes this one as its label.
fn is_label_of_parent(world: &World, e: Entity, text: &str, loc: &Localization) -> bool {
    if world.get::<Text>(e).is_none() || world.get::<Children>(e).is_some_and(|k| !k.is_empty()) {
        return false;
    }
    let Some(parent) = world.get::<ChildOf>(e).map(|p| p.0) else {
        return false;
    };
    world.get::<Text>(parent).is_none() && node_text(world, parent, loc).is_some_and(|label| label == text)
}

fn node_value(world: &World, e: Entity) -> Value {
    if let Some(value) = crate::default_widgets::widget_value(world, e) {
        value
    } else if let Some(toggle) = world.get::<Toggle>(e) {
        json!(toggle.checked)
    } else if let Some(slider) = world.get::<Slider>(e) {
        json!(slider.value)
    } else if let Some(bar) = world.get::<ProgressBar>(e) {
        json!(bar.value)
    } else if let Some(input) = world.get::<TextInput>(e) {
        json!(input.display_text())
    } else if let Some(dropdown) = world.get::<Dropdown>(e) {
        json!(dropdown.selected)
    } else if let Some(bar) = world.get::<TabBar>(e) {
        json!(bar.active)
    } else if let Some(modal) = world.get::<Modal>(e) {
        json!(modal.open)
    } else {
        Value::Null
    }
}

fn matches(world: &World, e: Entity, filter: &Filter, loc: &Localization) -> bool {
    if let Some(id) = filter.node
        && e.to_bits() != id
    {
        return false;
    }
    if let Some(name) = &filter.name
        && world.get::<NodeName>(e).is_none_or(|n| &n.0 != name)
    {
        return false;
    }
    if let Some(class) = &filter.class
        && world.get::<Classes>(e).is_none_or(|c| !c.0.contains(class))
    {
        return false;
    }
    if let Some(kind) = &filter.kind
        && !node_kind(world, e).eq_ignore_ascii_case(kind)
    {
        return false;
    }
    if !filter.states.is_empty() {
        let have = states_of(world, e);
        if !filter.states.iter().all(|s| have.contains(s)) {
            return false;
        }
    }
    if filter.text.is_some() || filter.text_contains.is_some() {
        let Some(text) = node_text(world, e, loc) else {
            return false;
        };
        // A button's label child has the same text as the button: the button is the node an agent wants.
        if filter.kind.is_none() && is_label_of_parent(world, e, &text, loc) {
            return false;
        }
        if let Some(exact) = &filter.text
            && &text != exact
        {
            return false;
        }
        if let Some(part) = &filter.text_contains
            && !text.to_lowercase().contains(&part.to_lowercase())
        {
            return false;
        }
    }
    true
}

/// Runs `f` with the active localization (an empty one when the app has none). Reads only, so the
/// resource's change tick is not touched.
pub(crate) fn with_localization<R>(world: &World, f: impl FnOnce(&Localization) -> R) -> R {
    match world.get_resource::<Localization>() {
        Some(loc) => f(loc),
        None => f(&Localization::default()),
    }
}

fn matching(world: &mut World, filter: &Filter) -> Vec<Entity> {
    let all = ui_entities(world);
    let world: &World = world;
    with_localization(world, |loc| {
        all.into_iter()
            .filter(|e| matches(world, *e, filter, loc))
            .collect()
    })
}

fn rect_json(r: [f32; 4]) -> Value {
    json!([r[0], r[1], r[2], r[3]])
}

fn node_json(world: &World, e: Entity, loc: &Localization) -> Value {
    let computed = world.get::<ComputedNode>(e).copied().unwrap_or_default();
    let visible = computed.rect[2] > 0.0
        && computed.rect[3] > 0.0
        && computed.clip_rect[2] > 0.0
        && computed.clip_rect[3] > 0.0;
    let mut node = Map::new();
    node.insert("id".into(), json!(e.to_bits()));
    node.insert(
        "name".into(),
        world.get::<NodeName>(e).map_or(Value::Null, |n| json!(n.0)),
    );
    node.insert("type".into(), json!(node_kind(world, e)));
    node.insert(
        "classes".into(),
        world.get::<Classes>(e).map_or(json!([]), |c| json!(c.0)),
    );
    node.insert("rect".into(), rect_json(computed.rect));
    node.insert("clip_rect".into(), rect_json(computed.clip_rect));
    node.insert(
        "center".into(),
        json!([
            computed.rect[0] + computed.rect[2] * 0.5,
            computed.rect[1] + computed.rect[3] * 0.5
        ]),
    );
    node.insert("visible".into(), json!(visible));
    node.insert(
        "text".into(),
        node_text(world, e, loc).map_or(Value::Null, |t| json!(t)),
    );
    node.insert("states".into(), json!(states_of(world, e)));
    node.insert("value".into(), node_value(world, e));
    node.insert(
        "interactable".into(),
        json!(world.get::<Interactable>(e).is_some()),
    );
    node.insert("focusable".into(), json!(world.get::<Focusable>(e).is_some()));
    // The resolved layout direction (ADR-518): `text_dir` inherited, else the locale's.
    let rtl = crate::layout::resolved_rtl(world, e, crate::style::locale_is_rtl(loc.locale()));
    node.insert("layout_dir".into(), json!(if rtl { "rtl" } else { "ltr" }));
    node.insert(
        "focus_visible".into(),
        json!(world.get::<Themed>(e).is_some_and(|t| t.focus_visible)),
    );
    node.insert(
        "drag_kind".into(),
        world
            .get::<DragSource>(e)
            .map_or(Value::Null, |s| json!(s.payload.kind)),
    );
    node.insert(
        "drop_accepts".into(),
        world
            .get::<DropTarget>(e)
            .map_or(Value::Null, |t| json!(t.accepts)),
    );
    node.insert(
        "parent".into(),
        world
            .get::<ChildOf>(e)
            .map_or(Value::Null, |p| json!(p.0.to_bits())),
    );
    Value::Object(node)
}

fn query(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let filter = parse_filter(params)?;
    let limit = match params.get("limit").filter(|v| !v.is_null()) {
        None => DEFAULT_QUERY_LIMIT,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_QUERY_LIMIT as u64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| {
                UiBridgeError::invalid(
                    "ui_bad_param",
                    format!("`limit` must be an integer from 1 to {MAX_QUERY_LIMIT}"),
                    "narrow the query with name, text, class, type or state instead",
                )
            })?,
    };
    let found = matching(world, &filter);
    let world: &World = world;
    let nodes: Vec<Value> = with_localization(world, |loc| {
        found
            .iter()
            .take(limit)
            .map(|e| node_json(world, *e, loc))
            .collect()
    });
    Ok(json!({
        "count": found.len(),
        "truncated": found.len() > nodes.len(),
        "nodes": nodes,
    }))
}

fn state(world: &mut World) -> Value {
    let focused = world.resource::<FocusState>().focused;
    let mode = match world.get_resource::<UiInputMode>().copied().unwrap_or_default() {
        UiInputMode::Pointer => "pointer",
        UiInputMode::Navigation => "navigation",
    };
    let scales = world.get_resource::<UiScales>().copied().unwrap_or_default();
    let (cursor, hovered, pressed) = world
        .get_resource::<UiPointer>()
        .map_or((None, None, None), |p| (p.cursor, p.hovered, p.pressed));
    let (selected, available) = world
        .get_resource::<Localization>()
        .map_or((String::new(), Vec::new()), |l| {
            (l.selected.clone(), l.available.clone())
        });
    let root_scales: Vec<Value> = {
        let mut roots = world.query::<(Entity, &UiRoot, Option<&ComputedNode>)>();
        let mut list: Vec<(Entity, f32)> = roots
            .iter(world)
            .map(|(e, root, computed)| {
                (
                    e,
                    computed.map_or_else(|| scales.scale_of(&root.scale_mode), |c| c.scale),
                )
            })
            .collect();
        list.sort_by_key(|(e, _)| e.to_bits());
        list.into_iter()
            .map(|(e, s)| json!({ "root": e.to_bits(), "scale": s }))
            .collect()
    };
    let nodes = ui_entities(world).len();
    let pending = world
        .get_resource::<UiAgentBridge>()
        .map_or(0, UiAgentBridge::pending_frames);
    let tooltip = world.get_resource::<crate::hints::TooltipState>().and_then(|t| {
        t.shown_entity().map(|e| {
            json!({
                "node": e.to_bits(),
                "text": t.text().unwrap_or_default(),
                "trigger": t.trigger().map(crate::hints::TooltipTrigger::name),
            })
        })
    });
    let device = world
        .get_resource::<crate::hints::HintDevice>()
        .copied()
        .unwrap_or_default();
    let hint_bar: Vec<String> = world
        .get_resource::<crate::hints::HintBar>()
        .map_or_else(Vec::new, |bar| bar.lines(device));
    json!({
        "focus": focused.map(Entity::to_bits),
        "input_mode": mode,
        "focus_ring": focus_ring_json(world, focused),
        "tooltip": tooltip,
        "hints": { "device": device.name(), "bar": hint_bar },
        "scale": { "window_size": scales.window_size, "scale_factor": scales.scale_factor, "roots": root_scales },
        "locale": { "selected": selected, "available": available },
        "pointer": { "cursor": cursor, "hovered": hovered.map(Entity::to_bits), "pressed": pressed.map(Entity::to_bits) },
        "nodes": nodes,
        "pending_frames": pending,
    })
}

/// The focus ring as `ui.state` reports it (D9): whether it draws, the policy, and where (the
/// outer rect, band width and colour, from the same geometry extract draws).
fn focus_ring_json(world: &World, focused: Option<Entity>) -> Value {
    let policy = world
        .get_resource::<FocusRingPolicy>()
        .copied()
        .unwrap_or_default();
    let ring = focused.and_then(|e| {
        let themed = world.get::<Themed>(e).filter(|t| t.focus_visible)?;
        let color = themed.focus_ring?;
        let computed = world.get::<ComputedNode>(e)?;
        let scale = computed.scale.max(0.001);
        let geometry = focus_ring_geometry(
            computed.rect,
            themed
                .radius
                .or_else(|| world.get::<crate::node::Border>(e).map(|b| b.radius[0]))
                .unwrap_or(0.0)
                * scale,
            themed.focus_ring_width.unwrap_or(DEFAULT_FOCUS_RING_WIDTH) * scale,
            themed.focus_ring_offset.unwrap_or(0.0) * scale,
        )?;
        Some((e, geometry, color))
    });
    match ring {
        Some((e, g, color)) => json!({
            "visible": true,
            "policy": policy.name(),
            "node": e.to_bits(),
            "rect": rect_json(g.rect),
            "width": g.width,
            "color": crate::theme_color_ops::to_hex(color),
        }),
        None => json!({
            "visible": false,
            "policy": policy.name(),
            "node": Value::Null,
            "rect": Value::Null,
            "width": Value::Null,
            "color": Value::Null,
        }),
    }
}

/// Resolves the one node a write targets: `node` (an id) and/or the query filters; it must match exactly one.
/// [`single`] for the other modules of the crate (`ui.set_target`, ADR-2406).
pub(crate) fn single_node(world: &mut World, params: &Value) -> Result<Entity, UiBridgeError> {
    single(world, params)
}

fn single(world: &mut World, params: &Value) -> Result<Entity, UiBridgeError> {
    let filter = parse_filter(params)?;
    if filter.node.is_none()
        && filter.name.is_none()
        && filter.text.is_none()
        && filter.text_contains.is_none()
        && filter.class.is_none()
        && filter.kind.is_none()
        && filter.states.is_empty()
    {
        return Err(UiBridgeError::invalid(
            "ui_node_required",
            "name the node: `node` (an id) or filters such as `name`, `text`, `class`, `type`, `state`",
            "call ui.query first to find it",
        ));
    }
    let found = matching(world, &filter);
    match found.as_slice() {
        [one] => Ok(*one),
        [] => Err(UiBridgeError::not_found(
            "ui_node_not_found",
            "no UI node matches",
            "call ui.query with fewer filters to see what exists",
        )),
        many => Err(UiBridgeError::invalid(
            "ui_node_ambiguous",
            format!("{} UI nodes match", many.len()),
            "add `name`, `type` or `class`, or pass the `node` id from ui.query",
        )),
    }
}

fn click(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let entity = single(world, params)?;
    let button_name = match params.get("button").filter(|v| !v.is_null()) {
        None => "left",
        Some(v) => v.as_str().ok_or_else(|| {
            UiBridgeError::invalid(
                "ui_bad_param",
                "`button` must be the string left, right or middle",
                "only the left button activates widgets today",
            )
        })?,
    };
    let button = match button_name {
        "left" => regia_platform::MouseButton::Left,
        "right" => regia_platform::MouseButton::Right,
        "middle" => regia_platform::MouseButton::Middle,
        other => {
            return Err(UiBridgeError::invalid(
                "ui_bad_param",
                format!("`button` must be left, right or middle, not `{other}`"),
                "only the left button activates widgets today",
            ));
        }
    };
    let count = match params.get("count").filter(|v| !v.is_null()) {
        None => 1,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_CLICK_COUNT).contains(n))
            .ok_or_else(|| {
                UiBridgeError::invalid(
                    "ui_bad_param",
                    format!("`count` must be an integer from 1 to {MAX_CLICK_COUNT}"),
                    "each click takes two frames",
                )
            })?,
    };
    let modifiers = click_modifiers(params)?;
    if crate::node::is_disabled(world, entity) {
        return Err(UiBridgeError::rejected(
            "ui_node_disabled",
            "the node is disabled and ignores clicks",
            "enable it first, or check `states` in ui.query",
        ));
    }
    check_reachable(world, entity)?;
    let computed = world.get::<ComputedNode>(entity).copied().unwrap_or_default();
    if computed.rect[2] <= 0.0
        || computed.rect[3] <= 0.0
        || computed.clip_rect[2] <= 0.0
        || computed.clip_rect[3] <= 0.0
    {
        return Err(UiBridgeError::rejected(
            "ui_node_not_visible",
            "the node has no visible area (zero size, hidden or clipped away)",
            "let a frame run so layout can place it, or scroll/open what contains it",
        ));
    }
    // Camera-target and world UI (ADR-2406): a window camera's root is offset to its viewport;
    // texture and world roots are not under the window pointer.
    let offset = match world.get::<crate::world_target::UiTargetNode>(entity) {
        None => [0.0; 2],
        Some(tag) => tag.window_origin.ok_or_else(|| {
            UiBridgeError::rejected(
                "ui_node_off_window",
                "the node draws into a camera texture or a world panel, not under the window pointer",
                "click world panels with ui.world_click",
            )
        })?,
    };
    let center = [
        computed.rect[0] + computed.rect[2] * 0.5 + offset[0],
        computed.rect[1] + computed.rect[3] * 0.5 + offset[1],
    ];
    let top_hit = hit_at(world, center);
    let mut frames = Vec::new();
    for index in 0..count {
        let mut press = Vec::new();
        if index == 0 {
            press.push(Step::MoveCursor(center));
        }
        if index == 0 {
            press.extend(modifiers.iter().map(|&logical| Step::Modifier {
                logical,
                pressed: true,
            }));
        }
        press.push(Step::Button {
            button,
            pressed: true,
        });
        frames.push(press);
        frames.push(vec![Step::Button {
            button,
            pressed: false,
        }]);
    }
    // The modifiers go up a frame after the last release: the pointer reads keys before buttons,
    // so letting go in the release frame would strip them from the release's `Click`.
    if !modifiers.is_empty() {
        frames.push(
            modifiers
                .iter()
                .map(|&logical| Step::Modifier {
                    logical,
                    pressed: false,
                })
                .collect(),
        );
    }
    let queued = frames.len();
    world.resource_mut::<UiAgentBridge>().frames.extend(frames);
    Ok(json!({
        "node": entity.to_bits(),
        "at": center,
        "top_hit": top_hit.map(Entity::to_bits),
        "occluded": top_hit.is_some_and(|hit| hit != entity),
        "queued_frames": queued,
    }))
}

/// The `modifiers` option of `ui.click`: a list of `shift`, `ctrl`, `alt` and `meta` (any case;
/// `control`, `option`, `super`, `cmd` and `command` are aliases), as the logical key names the
/// pointer's modifier state reads. Each modifier appears once, in a fixed order.
fn click_modifiers(params: &Value) -> Result<Vec<&'static str>, UiBridgeError> {
    let bad = |message: String| {
        UiBridgeError::invalid(
            "ui_bad_param",
            message,
            "`modifiers` is a list such as [\"shift\"] or [\"ctrl\", \"alt\"]",
        )
    };
    let Some(value) = params.get("modifiers").filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let list = value
        .as_array()
        .ok_or_else(|| bad("`modifiers` must be a list of strings".to_owned()))?;
    let mut held = [false; 4];
    for item in list {
        let name = item
            .as_str()
            .ok_or_else(|| bad("`modifiers` must be a list of strings".to_owned()))?;
        let slot = match name.to_ascii_lowercase().as_str() {
            "shift" => 0,
            "ctrl" | "control" => 1,
            "alt" | "option" => 2,
            "meta" | "super" | "cmd" | "command" => 3,
            other => {
                return Err(bad(format!(
                    "`{other}` is not a modifier (shift, ctrl, alt, meta)"
                )));
            }
        };
        held[slot] = true;
    }
    Ok(["Shift", "Control", "Alt", "Super"]
        .into_iter()
        .zip(held)
        .filter_map(|(name, on)| on.then_some(name))
        .collect())
}

/// What the pointer would hit at `p`, by the interaction system's own rule.
fn hit_at(world: &mut World, p: [f32; 2]) -> Option<Entity> {
    let mut nodes = world.query::<(
        Entity,
        &ComputedNode,
        &Interactable,
        Option<&Disabled>,
        Option<&crate::world_target::UiTargetNode>,
        Option<&crate::group_layer::UiClipMasks>,
    )>();
    let renderer = world.get_resource::<crate::SharedUiRenderer>().cloned();
    let candidates: Vec<HitCandidate> = nodes
        .iter(world)
        .filter_map(|(entity, computed, interactable, disabled, tag, masks)| {
            let (rect, clip) = crate::interaction::window_rects(computed.rect, computed.clip_rect, tag)?;
            // Rounded clips and image masks (ADR-2866) hide what they cut away.
            if let Some(masks) = masks
                && !masks.admits(crate::interaction::node_space(p, tag), |id, uv| {
                    renderer.as_ref().and_then(|r| r.lock().image_alpha(id, uv))
                })
            {
                return None;
            }
            Some(HitCandidate {
                entity,
                rect,
                clip,
                pick_through: interactable.pick_through,
                disabled: disabled.is_some() || crate::node::is_disabled(world, entity),
                z: computed.z,
                paint_order: computed.paint_order,
            })
        })
        .collect();
    pick_topmost(candidates.into_iter(), p)
}

fn value_json(value: &UiValue) -> Value {
    match value {
        UiValue::None => Value::Null,
        UiValue::Bool(b) => json!(b),
        UiValue::Number(n) => json!(n),
        UiValue::Index(i) => json!(i),
        UiValue::Text(t) => json!(t),
    }
}

/// The visible centre of `entity`, or why a pointer cannot reach it.
fn visible_center(world: &World, entity: Entity, role: &str) -> Result<[f32; 2], UiBridgeError> {
    if crate::node::is_disabled(world, entity) {
        return Err(UiBridgeError::rejected(
            "ui_node_disabled",
            format!("the {role} node is disabled and ignores the pointer"),
            "enable it first, or check `states` in ui.query",
        ));
    }
    let computed = world.get::<ComputedNode>(entity).copied().unwrap_or_default();
    if computed.rect[2] <= 0.0
        || computed.rect[3] <= 0.0
        || computed.clip_rect[2] <= 0.0
        || computed.clip_rect[3] <= 0.0
    {
        return Err(UiBridgeError::rejected(
            "ui_node_not_visible",
            format!("the {role} node has no visible area (zero size, hidden or clipped away)"),
            "let a frame run so layout can place it, or scroll/open what contains it",
        ));
    }
    // Camera-target and world UI (ADR-2406): same origin rule as `click` — a drag presses
    // where the window pointer can actually reach.
    let offset = match world.get::<crate::world_target::UiTargetNode>(entity) {
        None => [0.0; 2],
        Some(tag) => tag.window_origin.ok_or_else(|| {
            UiBridgeError::rejected(
                "ui_node_off_window",
                format!("the {role} node draws into a camera texture or a world panel, not under the window pointer"),
                "drag world panels with ui.world_click",
            )
        })?,
    };
    Ok([
        computed.rect[0] + computed.rect[2] * 0.5 + offset[0],
        computed.rect[1] + computed.rect[3] * 0.5 + offset[1],
    ])
}

/// A nested node selector (`from`, `to`): an object of the usual filters.
fn nested_node(world: &mut World, params: &Value, key: &str) -> Result<Option<Entity>, UiBridgeError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) if v.is_object() => single(world, v).map(Some),
        Some(_) => Err(UiBridgeError::invalid(
            "ui_bad_param",
            format!("`{key}` must be an object of node filters, e.g. {{\"name\": \"slot\"}}"),
            "use `node`, `name`, `text`, `class` or `type` inside it",
        )),
    }
}

fn drag_json(world: &World) -> Value {
    let Some(op) = world.get_resource::<DragDrop>().and_then(DragDrop::active) else {
        return json!({ "active": false });
    };
    json!({
        "active": true,
        "source": op.source.to_bits(),
        "kind": op.payload.kind,
        "data": value_json(&op.payload.data),
        "position": op.position,
        "grab_offset": op.grab_offset,
        "hover": op.hover.map(|(target, accepted)| json!({ "node": target.to_bits(), "accepted": accepted })),
    })
}

fn drag_drop(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let action = match params.get("action").filter(|v| !v.is_null()) {
        None => "state",
        Some(v) => v.as_str().ok_or_else(|| {
            UiBridgeError::invalid(
                "ui_bad_param",
                "`action` must be a string",
                "state (default), drag, drop or cancel",
            )
        })?,
    };
    let running = world
        .get_resource::<DragDrop>()
        .is_some_and(|d| d.active().is_some());
    match action {
        "state" => {
            let mut sources = Vec::new();
            let mut targets = Vec::new();
            for e in ui_entities(world) {
                if let Some(s) = world.get::<DragSource>(e) {
                    sources.push(json!({ "node": e.to_bits(), "kind": s.payload.kind }));
                }
                if let Some(t) = world.get::<DropTarget>(e) {
                    targets.push(json!({ "node": e.to_bits(), "accepts": t.accepts }));
                }
            }
            let mut out = drag_json(world);
            out["sources"] = json!(sources);
            out["targets"] = json!(targets);
            Ok(out)
        }
        "drag" => {
            if running {
                return Err(UiBridgeError::rejected(
                    "ui_drag_active",
                    "a drag is already running",
                    "finish it with action drop or cancel first",
                ));
            }
            let from = nested_node(world, params, "from")?.ok_or_else(|| {
                UiBridgeError::invalid(
                    "ui_node_required",
                    "`from` names the node to drag",
                    "{\"action\": \"drag\", \"from\": {\"name\": \"item\"}, \"to\": {\"name\": \"slot\"}}",
                )
            })?;
            if world.get::<DragSource>(from).is_none() {
                return Err(UiBridgeError::rejected(
                    "ui_not_draggable",
                    "the `from` node has no DragSource",
                    "ui.query lists `drag_kind` for draggable nodes",
                ));
            }
            let start = visible_center(world, from, "from")?;
            let end = match nested_node(world, params, "to")? {
                Some(to) => {
                    if world.get::<DropTarget>(to).is_none() {
                        return Err(UiBridgeError::rejected(
                            "ui_not_drop_target",
                            "the `to` node is not a DropTarget",
                            "ui.query lists `drop_accepts` for drop targets; name a node inside one to drop on its nearest target",
                        ));
                    }
                    visible_center(world, to, "to")?
                }
                None => match params.get("at").filter(|v| !v.is_null()) {
                    Some(v) => {
                        let pair: Option<[f32; 2]> = v.as_array().and_then(|a| match a.as_slice() {
                            [x, y] => Some([x.as_f64()? as f32, y.as_f64()? as f32]),
                            _ => None,
                        });
                        pair.ok_or_else(|| {
                            UiBridgeError::invalid(
                                "ui_bad_param",
                                "`at` must be [x, y] in physical px",
                                "or name a `to` node",
                            )
                        })?
                    }
                    None => {
                        return Err(UiBridgeError::invalid(
                            "ui_drag_target_required",
                            "name where to drag: `to` (a node) or `at` ([x, y])",
                            "{\"action\": \"drag\", \"from\": {...}, \"to\": {...}}",
                        ));
                    }
                },
            };
            let release = params.get("release").and_then(Value::as_bool).unwrap_or(true);
            let left = regia_platform::MouseButton::Left;
            let mut frames = vec![
                vec![Step::MoveCursor(start)],
                vec![Step::Button {
                    button: left,
                    pressed: true,
                }],
                // Past the interaction system's drag threshold, so the operation starts.
                vec![Step::MoveCursor([start[0] + 6.0, start[1]])],
                vec![Step::MoveCursor(end)],
            ];
            if release {
                frames.push(vec![Step::Button {
                    button: left,
                    pressed: false,
                }]);
            }
            let queued = frames.len();
            world.resource_mut::<UiAgentBridge>().frames.extend(frames);
            Ok(json!({
                "from": from.to_bits(),
                "start": start,
                "end": end,
                "released": release,
                "queued_frames": queued,
            }))
        }
        "drop" => {
            if !running {
                return Err(UiBridgeError::rejected(
                    "ui_no_drag",
                    "no drag is running",
                    "start one with action drag (release: false keeps it open)",
                ));
            }
            let mut frames = Vec::new();
            if let Some(to) = nested_node(world, params, "to")? {
                frames.push(vec![Step::MoveCursor(visible_center(world, to, "to")?)]);
            }
            frames.push(vec![Step::Button {
                button: regia_platform::MouseButton::Left,
                pressed: false,
            }]);
            let queued = frames.len();
            world.resource_mut::<UiAgentBridge>().frames.extend(frames);
            Ok(json!({ "queued_frames": queued }))
        }
        "cancel" => {
            if !running {
                return Err(UiBridgeError::rejected(
                    "ui_no_drag",
                    "no drag is running",
                    "start one with action drag (release: false keeps it open)",
                ));
            }
            cancel_drag(&mut world.resource_mut::<DragDrop>());
            // Let go of the button too, so the next press starts clean.
            let queued = usize::from(world.resource::<UiPointer>().down());
            if queued == 1 {
                world
                    .resource_mut::<UiAgentBridge>()
                    .frames
                    .push_back(vec![Step::Button {
                        button: regia_platform::MouseButton::Left,
                        pressed: false,
                    }]);
            }
            Ok(json!({ "cancelled": true, "queued_frames": queued }))
        }
        other => Err(UiBridgeError::invalid(
            "ui_bad_param",
            format!("`action` must be state, drag, drop or cancel, not `{other}`"),
            "state (default), drag, drop or cancel",
        )),
    }
}

fn focus(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let entity = single(world, params)?;
    check_focusable(world, entity)?;
    world
        .resource_mut::<UiAgentBridge>()
        .frames
        .push_back(vec![Step::Focus(entity)]);
    Ok(json!({ "node": entity.to_bits(), "queued_frames": 1 }))
}

/// The node must be able to receive input now: not while the top input layer hands input to the game,
/// and inside the top layer's subtree when one is active.
fn check_reachable(world: &World, entity: Entity) -> Result<(), UiBridgeError> {
    let Some(layers) = world.get_resource::<crate::layers::InputLayers>() else {
        return Ok(());
    };
    let blocked = layers.mode() == crate::layers::LayerInputMode::GameOnly
        || layers
            .top_root()
            .is_some_and(|root| !crate::layers::within(world, root, entity));
    if blocked {
        return Err(UiBridgeError::rejected(
            "ui_node_blocked_by_layer",
            "the node is outside the top input layer (or the layer gives input to the game)",
            "act on a node inside the top layer, or close it with ui.layers {action: \"pop\"}",
        ));
    }
    Ok(())
}

fn check_focusable(world: &World, entity: Entity) -> Result<(), UiBridgeError> {
    check_reachable(world, entity)?;
    if world.get::<Focusable>(entity).is_none() {
        return Err(UiBridgeError::rejected(
            "ui_node_not_focusable",
            "the node cannot take focus",
            "only nodes with `focusable: true` in ui.query can; buttons, inputs and sliders are",
        ));
    }
    if crate::node::is_disabled(world, entity) {
        return Err(UiBridgeError::rejected(
            "ui_node_disabled",
            "the node is disabled and cannot take focus",
            "enable it first, or check `states` in ui.query",
        ));
    }
    Ok(())
}

fn type_text(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let text = string_param(params, "text")?;
    let keys: Vec<String> = match params.get("keys").filter(|v| !v.is_null()) {
        None => Vec::new(),
        Some(Value::Array(list)) => list
            .iter()
            .map(|k| k.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                UiBridgeError::invalid(
                    "ui_bad_param",
                    "`keys` must be a list of key names",
                    "e.g. [\"Backspace\", \"Enter\"]",
                )
            })?,
        Some(_) => {
            return Err(UiBridgeError::invalid(
                "ui_bad_param",
                "`keys` must be a list of key names",
                "e.g. [\"Backspace\", \"Enter\"]",
            ));
        }
    };
    if text.as_deref().is_none_or(str::is_empty) && keys.is_empty() {
        return Err(UiBridgeError::invalid(
            "ui_type_empty",
            "give `text` to commit and/or `keys` to press",
            "ui.type {text: \"hello\", keys: [\"Enter\"]}",
        ));
    }
    let mut steps = Vec::new();
    // `text` is the payload here, so the target's own text filter is called `label`.
    let mut target_params = Map::new();
    for key in ["node", "name", "class", "type", "state", "text_contains"] {
        if let Some(v) = params.get(key) {
            target_params.insert(key.to_owned(), v.clone());
        }
    }
    if let Some(label) = params.get("label") {
        target_params.insert("text".to_owned(), label.clone());
    }
    let target_params = Value::Object(target_params);
    let target = if target_params
        .as_object()
        .is_some_and(|m| m.values().any(|v| !v.is_null()))
    {
        let entity = single(world, &target_params)?;
        check_focusable(world, entity)?;
        steps.push(Step::Focus(entity));
        Some(entity)
    } else {
        let focused = world.resource::<FocusState>().focused;
        if focused.is_none() {
            return Err(UiBridgeError::rejected(
                "ui_no_focus",
                "nothing has focus to type into",
                "pass `node`/`name` to focus a node first, or call ui.focus",
            ));
        }
        focused
    };
    if let Some(text) = text.filter(|t| !t.is_empty()) {
        steps.push(Step::Commit(text));
    }
    for key in keys {
        let (logical, physical) = match key.split_once('+') {
            Some((modifier, rest)) if modifier.eq_ignore_ascii_case("shift") && !rest.is_empty() => {
                (rest.to_owned(), "ShiftLeft".to_owned())
            }
            _ => (key.clone(), key.clone()),
        };
        steps.push(Step::Key { logical, physical });
    }
    world.resource_mut::<UiAgentBridge>().frames.push_back(steps);
    Ok(json!({ "node": target.map(Entity::to_bits), "queued_frames": 1 }))
}

/// `ui.set_locale { locale }`: switches the active locale now; every localized text re-resolves and
/// the layout reflows on the next frame (the locale generation moves).
fn set_locale(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let Some(locale) = params.get("locale").and_then(Value::as_str) else {
        return Err(UiBridgeError::invalid(
            "ui_locale_required",
            "`locale` (a locale tag such as \"ja\") is required",
            "ui.state lists the available locales",
        ));
    };
    let mut loc = world.resource_mut::<Localization>();
    if !loc.available.iter().any(|l| l == locale) {
        return Err(UiBridgeError::not_found(
            "ui_locale_unknown",
            format!("no locale `{locale}` (available: {:?})", loc.available),
            "ui.state lists the available locales",
        ));
    }
    let previous = loc.selected.clone();
    let changed = loc.set_locale(locale);
    Ok(json!({
        "locale": loc.selected,
        "previous": previous,
        "changed": changed,
        "generation": loc.generation.0,
    }))
}

/// `ui.resolve { key, args?, locale? }`: formats one message without touching any node.
fn resolve(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let Some(key) = params.get("key").and_then(Value::as_str) else {
        return Err(UiBridgeError::invalid(
            "ui_key_required",
            "`key` (a message id such as \"menu-start\") is required",
            "the ids are in assets://localization/<locale>/*.ftl",
        ));
    };
    let bad_args = || {
        UiBridgeError::invalid(
            "ui_bad_param",
            "`args` must be an object of strings and numbers",
            "e.g. {\"count\": 3}",
        )
    };
    let mut args = Vec::new();
    match params.get("args").filter(|v| !v.is_null()) {
        None => {}
        Some(Value::Object(map)) => {
            for (name, value) in map {
                let text = match value {
                    Value::String(s) => s.clone(),
                    Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
                    Value::Number(n) => n.as_f64().map(|f| f.to_string()).ok_or_else(bad_args)?,
                    _ => return Err(bad_args()),
                };
                args.push((name.clone(), text));
            }
        }
        Some(_) => return Err(bad_args()),
    }
    let loc = world.resource::<Localization>();
    let (locale, text) = match params.get("locale").filter(|v| !v.is_null()) {
        Some(v) => {
            let locale = v.as_str().ok_or_else(bad_args)?;
            if !loc.available.iter().any(|l| l == locale) {
                return Err(UiBridgeError::not_found(
                    "ui_locale_unknown",
                    format!("no locale `{locale}` (available: {:?})", loc.available),
                    "ui.state lists the available locales",
                ));
            }
            (locale.to_owned(), loc.resolve_in(locale, key, &args))
        }
        None => {
            let from = [loc.selected.as_str(), loc.fallback()]
                .into_iter()
                .find(|l| loc.has_in(l, key));
            (
                from.unwrap_or(&loc.selected).to_owned(),
                from.and_then(|l| loc.resolve_in(l, key, &args)),
            )
        }
    };
    Ok(json!({
        "key": key,
        "locale": locale,
        "found": text.is_some(),
        "text": text.unwrap_or_else(|| loc.resolve(key, &args)),
    }))
}

fn set_value(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let entity = single(world, params)?;
    let Some(value) = params.get("value").filter(|v| !v.is_null()) else {
        return Err(UiBridgeError::invalid(
            "ui_value_required",
            "`value` is required",
            "a bool for a Toggle, a number for a Slider or ProgressBar, a string for a TextInput, an index for a Dropdown, TabBar or Breadcrumbs",
        ));
    };
    if crate::node::is_disabled(world, entity) {
        return Err(UiBridgeError::rejected(
            "ui_node_disabled",
            "the node is disabled and cannot be changed",
            "enable it first, or check `states` in ui.query",
        ));
    }
    let kind = node_kind(world, entity);
    let mismatch = |want: &str| {
        UiBridgeError::invalid(
            "ui_value_type_mismatch",
            format!("a {kind} takes {want}"),
            "see the value types in book/agents/ui.md",
        )
    };
    let parsed = match kind {
        "Toggle" | "RadioButton" | "Foldable" => {
            UiValue::Bool(value.as_bool().ok_or_else(|| mismatch("a boolean"))?)
        }
        "Slider" | "ProgressBar" | "SpinBox" | "Split" => {
            let number = value
                .as_f64()
                .filter(|n| n.is_finite())
                .ok_or_else(|| mismatch("a finite number"))?;
            // A finite f64 outside the f32 range becomes ±inf in the widget and lands on the
            // wrong end of its range; refuse it before anything is queued.
            let number = number as f32;
            if !number.is_finite() {
                return Err(UiBridgeError::invalid(
                    "ui_value_out_of_range",
                    format!("{value} is outside the range a widget can hold (f32)"),
                    "stay within ±3.4e38",
                ));
            }
            UiValue::Number(number)
        }
        "TextInput" => UiValue::Text(value.as_str().ok_or_else(|| mismatch("a string"))?.to_owned()),
        "Dropdown" | "TabBar" | "Breadcrumbs" => {
            let index = value.as_u64().ok_or_else(|| mismatch("an index"))? as usize;
            let len = world
                .get::<Dropdown>(entity)
                .map(|d| d.options.len())
                .or_else(|| world.get::<TabBar>(entity).map(|t| t.tabs.len()))
                .or_else(|| {
                    world
                        .get::<crate::default_widgets::Breadcrumbs>(entity)
                        .map(|b| b.segments.len())
                })
                .unwrap_or(0);
            if index >= len {
                return Err(UiBridgeError::invalid(
                    "ui_value_out_of_range",
                    format!("index {index} is out of range (the {kind} has {len} entries)"),
                    "indices start at 0",
                ));
            }
            UiValue::Index(index)
        }
        _ => {
            return Err(UiBridgeError::rejected(
                "ui_node_not_settable",
                format!("a {kind} has no value to set"),
                "settable: Toggle, RadioButton, Foldable, Slider, ProgressBar, SpinBox, Split, TextInput, Dropdown, TabBar, Breadcrumbs",
            ));
        }
    };
    world
        .resource_mut::<UiAgentBridge>()
        .frames
        .push_back(vec![Step::SetValue(entity, parsed)]);
    Ok(json!({ "node": entity.to_bits(), "queued_frames": 1 }))
}

/// `ui.set_backdrop_blur { ...node filters, radius }`: frosted glass on one node (ADR-1836).
/// `radius` is the Gaussian σ in UI px, `0..=64`; 0 removes the effect. Takes effect at once (the
/// next extract sees the component) and answers with what is set now.
fn set_backdrop_blur(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let entity = single(world, params)?;
    let Some(radius) = params
        .get("radius")
        .and_then(Value::as_f64)
        .filter(|r| r.is_finite())
    else {
        return Err(UiBridgeError::invalid(
            "ui_radius_required",
            "`radius` (a finite number of UI px) is required",
            "0 removes the blur; the maximum is 64",
        ));
    };
    if !(0.0..=64.0).contains(&radius) {
        return Err(UiBridgeError::invalid(
            "ui_radius_out_of_range",
            format!("radius {radius} is outside 0..=64"),
            "use 0 to remove the blur",
        ));
    }
    let previous = world.get::<crate::node::BackdropBlur>(entity).map(|b| b.radius);
    if radius > 0.0 {
        world.entity_mut(entity).insert(crate::node::BackdropBlur {
            radius: radius as f32,
        });
    } else {
        world.entity_mut(entity).remove::<crate::node::BackdropBlur>();
    }
    Ok(json!({
        "node": entity.to_bits(),
        "radius": radius,
        "previous": previous,
        "changed": previous != (radius > 0.0).then_some(radius as f32),
    }))
}

/// `ui.backdrop_stats {}`: what the blur did in the last window draw, plus how many
/// nodes have one now and the render targets its pool holds now (ADR-5420). Zeros when no
/// renderer is installed or nothing was drawn yet.
fn backdrop_stats(world: &mut World) -> Value {
    let nodes = world.query::<&crate::node::BackdropBlur>().iter(world).count();
    let stats = world
        .get_resource::<crate::SharedUiRenderer>()
        .map(|renderer| renderer.lock().backdrop_stats())
        .unwrap_or_default();
    json!({
        "nodes": nodes,
        "quads": stats.quads,
        "blurred": stats.blurred,
        "skipped": stats.skipped,
        "passes": stats.passes,
        "shaded_pixels": stats.shaded_pixels,
        "samples": stats.samples,
        "copied_pixels": stats.copied_pixels,
        "live_targets": stats.live_targets,
        "live_target_bytes": stats.live_target_bytes,
        "targets_created": stats.targets_created,
        "targets_freed": stats.targets_freed,
    })
}

/// `wait_until` predicate `ui.node_exists`: `{holds, detail}`; `exists: false` waits for a node to go away.
fn node_exists(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let filter = parse_filter(params)?;
    let want = params.get("exists").and_then(Value::as_bool).unwrap_or(true);
    let count = matching(world, &filter).len();
    Ok(json!({
        "holds": (count > 0) == want,
        "detail": format!("{count} UI nodes match, expected {}", if want { "at least one" } else { "none" }),
    }))
}

/// `wait_until` predicate `ui.node_state`: whether the one matching node has `state` (`is: false` for not).
fn node_state(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let Some(name) = params.get("state").and_then(Value::as_str) else {
        return Err(UiBridgeError::invalid(
            "ui_bad_param",
            "`state` (one state name) is required",
            "hovered, pressed, focused, disabled, selected or invalid",
        ));
    };
    let wanted = state_name(name)?;
    let want = params.get("is").and_then(Value::as_bool).unwrap_or(true);
    // The node is chosen by the other filters; the state is the thing under test.
    let mut node_params = params.clone();
    if let Some(map) = node_params.as_object_mut() {
        map.remove("state");
    }
    let found = {
        let filter = parse_filter(&node_params)?;
        matching(world, &filter)
    };
    match found.as_slice() {
        [one] => {
            let has = states_of(world, *one).contains(&wanted);
            Ok(json!({
                "holds": has == want,
                "detail": format!("node {} {} {wanted}", one.to_bits(), if has { "is" } else { "is not" }),
            }))
        }
        [] => Ok(json!({ "holds": false, "detail": "no UI node matches" })),
        many => Ok(
            json!({ "holds": false, "detail": format!("{} UI nodes match; narrow the filter", many.len()) }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeStyle;

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

    fn spawn_labelled(app: &mut App, label: &str) -> Entity {
        let world = app.world_mut();
        let button = world
            .spawn((
                UiNode,
                NodeStyle::default(),
                Button,
                Interactable::default(),
                Focusable::default(),
            ))
            .id();
        let text = world.spawn((UiNode, Text::new(label))).id();
        world.entity_mut(button).add_child(text);
        button
    }

    #[test]
    fn query_finds_a_button_by_its_label() {
        let mut app = app();
        let settings = spawn_labelled(&mut app, "Settings");
        spawn_labelled(&mut app, "Play");
        let out = call(app.world_mut(), "ui.query", &json!({ "text": "Settings" })).expect("query");
        assert_eq!(out["count"], 1);
        assert_eq!(out["nodes"][0]["id"], settings.to_bits());
        assert_eq!(out["nodes"][0]["type"], "Button");
        let partial = call(
            app.world_mut(),
            "ui.query",
            &json!({ "text_contains": "pla", "type": "button" }),
        )
        .expect("query");
        assert_eq!(partial["count"], 1);
    }

    #[test]
    fn writes_name_one_node_and_validate() {
        let mut app = app();
        spawn_labelled(&mut app, "Same");
        spawn_labelled(&mut app, "Same");
        let ambiguous = call(app.world_mut(), "ui.focus", &json!({ "text": "Same" })).expect_err("two match");
        assert_eq!(ambiguous.reason, "ui_node_ambiguous");
        let missing = call(app.world_mut(), "ui.focus", &json!({ "text": "Nope" })).expect_err("none match");
        assert_eq!(missing.reason, "ui_node_not_found");
        let nameless = call(app.world_mut(), "ui.click", &json!({})).expect_err("no target");
        assert_eq!(nameless.reason, "ui_node_required");
        let bad_state = call(app.world_mut(), "ui.query", &json!({ "state": "glowing" })).expect_err("state");
        assert_eq!(bad_state.reason, "ui_unknown_state");
    }

    #[test]
    fn focus_is_queued_then_applied_by_the_drain() {
        let mut app = app();
        let button = spawn_labelled(&mut app, "Go");
        call(app.world_mut(), "ui.focus", &json!({ "node": button.to_bits() })).expect("queued");
        assert_eq!(app.world().resource::<UiAgentBridge>().pending_frames(), 1);
        assert!(
            app.world().get::<Focused>(button).is_none(),
            "nothing happens until a frame runs"
        );
        app.update();
        assert!(app.world().get::<Focused>(button).is_some());
        assert_eq!(app.world().resource::<FocusState>().focused, Some(button));
        assert_eq!(app.world().resource::<UiAgentBridge>().pending_frames(), 0);
        let holds = call(
            app.world_mut(),
            "ui.node_state",
            &json!({ "node": button.to_bits(), "state": "Focused" }),
        )
        .expect("predicate");
        assert_eq!(holds["holds"], true);
    }

    #[test]
    fn set_value_checks_the_widget_type() {
        let mut app = app();
        let toggle = app.world_mut().spawn((UiNode, Toggle::default())).id();
        let wrong = call(
            app.world_mut(),
            "ui.set_value",
            &json!({ "node": toggle.to_bits(), "value": 3 }),
        )
        .expect_err("type");
        assert_eq!(wrong.reason, "ui_value_type_mismatch");
        call(
            app.world_mut(),
            "ui.set_value",
            &json!({ "node": toggle.to_bits(), "value": true }),
        )
        .expect("queued");
        app.update();
        assert!(app.world().get::<Toggle>(toggle).is_some_and(|t| t.checked));
        let plain = app.world_mut().spawn(UiNode).id();
        let none = call(
            app.world_mut(),
            "ui.set_value",
            &json!({ "node": plain.to_bits(), "value": 1 }),
        )
        .expect_err("no value");
        assert_eq!(none.reason, "ui_node_not_settable");
    }

    #[test]
    fn set_value_text_goes_in_the_way_typing_would() {
        // Audit ui1 correctness 1: the writer must sanitize and cut on grapheme clusters
        // like typed text, not store raw bytes cut at `max_chars` scalars.
        let mut app = app();
        let input = app.world_mut().spawn((UiNode, TextInput::default())).id();
        // A raw newline becomes a space in a single-line field.
        apply_value(app.world_mut(), input, UiValue::Text("a\nb".to_owned()));
        assert_eq!(
            app.world().get::<TextInput>(input).map(|i| i.text.as_str()),
            Some("a b")
        );
        // "é" spelled as e + U+0301 is one cluster of two scalars: it does not fit max_chars 1
        // and must not be stored cut in half.
        app.world_mut().get_mut::<TextInput>(input).unwrap().max_chars = 1;
        apply_value(app.world_mut(), input, UiValue::Text("e\u{0301}".to_owned()));
        assert_eq!(
            app.world().get::<TextInput>(input).map(|i| i.text.as_str()),
            Some("")
        );
    }

    #[test]
    fn set_value_text_refuses_a_read_only_field_and_a_filter_rejection() {
        // Audit ui1 correctness 1: ui.set_value must not write a read-only field nor a value
        // the field's filter would refuse while typing.
        let mut app = app();
        let input = app.world_mut().spawn((UiNode, TextInput::default())).id();
        app.world_mut().get_mut::<TextInput>(input).unwrap().read_only = true;
        apply_value(app.world_mut(), input, UiValue::Text("hi".to_owned()));
        assert_eq!(
            app.world().get::<TextInput>(input).map(|i| i.text.as_str()),
            Some(""),
            "a read-only field keeps its content"
        );
        {
            let mut field = app.world_mut().get_mut::<TextInput>(input).unwrap();
            field.read_only = false;
            field.filter = crate::InputFilter::Numeric;
        }
        apply_value(app.world_mut(), input, UiValue::Text("abc".to_owned()));
        assert_eq!(
            app.world().get::<TextInput>(input).map(|i| i.text.as_str()),
            Some(""),
            "a filter rejection leaves the field unchanged"
        );
        apply_value(app.world_mut(), input, UiValue::Text("3.5".to_owned()));
        assert_eq!(
            app.world().get::<TextInput>(input).map(|i| i.text.as_str()),
            Some("3.5")
        );
    }

    #[test]
    fn set_value_text_clears_the_undo_history() {
        // Audit ui1 correctness 1: a programmatic set must clear the undo history the way
        // TextInput::set_text does, or the next undo would restore text the user never saw.
        let mut app = app();
        let input = app.world_mut().spawn((UiNode, TextInput::default())).id();
        app.world_mut()
            .get_mut::<TextInput>(input)
            .unwrap()
            .type_text("ab");
        assert!(
            app.world().get::<TextInput>(input).is_some_and(|i| i.can_undo()),
            "typing pushed an undo step"
        );
        apply_value(app.world_mut(), input, UiValue::Text("zzz".to_owned()));
        let mut field = app.world_mut().get_mut::<TextInput>(input).unwrap();
        assert!(
            !field.undo(),
            "the user's typing must not come back after a programmatic set"
        );
        assert_eq!(field.text, "zzz");
    }

    #[test]
    fn set_value_rejects_a_number_outside_the_f32_range() {
        // Audit ui1 correctness 6: a finite f64 past the f32 range becomes ±inf in the widget
        // and lands on the wrong end of the range; refuse it before anything is queued.
        let mut app = app();
        let slider = app.world_mut().spawn((UiNode, Slider::default())).id();
        let err = call(
            app.world_mut(),
            "ui.set_value",
            &json!({ "node": slider.to_bits(), "value": 1e40 }),
        )
        .expect_err("out of range");
        assert_eq!(err.reason, "ui_value_out_of_range");
        app.update();
        assert_eq!(
            app.world().get::<Slider>(slider).map(|s| s.value),
            Some(0.5),
            "the refused value never reaches the widget"
        );
    }

    #[test]
    fn backdrop_stats_counts_the_nodes_and_reads_zero_before_a_draw() {
        let mut app = app();
        let node = app.world_mut().spawn(UiNode).id();
        call(
            app.world_mut(),
            "ui.set_backdrop_blur",
            &json!({ "node": node.to_bits(), "radius": 8 }),
        )
        .expect("set");
        let out = call(app.world_mut(), "ui.backdrop_stats", &json!({})).expect("stats");
        assert_eq!(out["nodes"], json!(1));
        assert_eq!(out["blurred"], json!(0));
        assert_eq!(out["shaded_pixels"], json!(0));
        // No blur drawn: the pool holds nothing yet (ADR-5420).
        assert_eq!(out["live_targets"], json!(0));
        assert_eq!(out["live_target_bytes"], json!(0));
        assert_eq!(out["targets_created"], json!(0));
        assert_eq!(out["targets_freed"], json!(0));
    }

    #[test]
    fn set_backdrop_blur_sets_changes_and_removes_the_component() {
        let mut app = app();
        let node = app.world_mut().spawn(UiNode).id();
        let radius = |app: &App| {
            app.world()
                .get::<crate::node::BackdropBlur>(node)
                .map(|b| b.radius)
        };
        let set = call(
            app.world_mut(),
            "ui.set_backdrop_blur",
            &json!({ "node": node.to_bits(), "radius": 12 }),
        )
        .expect("set");
        assert_eq!(set["changed"], true);
        assert_eq!(set["previous"], Value::Null);
        assert_eq!(radius(&app), Some(12.0));
        let same = call(
            app.world_mut(),
            "ui.set_backdrop_blur",
            &json!({ "node": node.to_bits(), "radius": 12 }),
        )
        .expect("again");
        assert_eq!(same["changed"], false);
        assert_eq!(same["previous"], 12.0);
        let off = call(
            app.world_mut(),
            "ui.set_backdrop_blur",
            &json!({ "node": node.to_bits(), "radius": 0 }),
        )
        .expect("off");
        assert_eq!(off["changed"], true);
        assert_eq!(radius(&app), None);
        let off_again = call(
            app.world_mut(),
            "ui.set_backdrop_blur",
            &json!({ "node": node.to_bits(), "radius": 0 }),
        )
        .expect("off again");
        assert_eq!(off_again["changed"], false);
    }

    #[test]
    fn set_backdrop_blur_validates_its_input() {
        let mut app = app();
        let node = app.world_mut().spawn(UiNode).id();
        let missing = call(
            app.world_mut(),
            "ui.set_backdrop_blur",
            &json!({ "node": node.to_bits() }),
        )
        .expect_err("radius");
        assert_eq!(missing.reason, "ui_radius_required");
        for bad in [json!(-1), json!(65), json!("big")] {
            let err = call(
                app.world_mut(),
                "ui.set_backdrop_blur",
                &json!({ "node": node.to_bits(), "radius": bad }),
            )
            .expect_err("bad radius");
            assert!(
                matches!(err.reason, "ui_radius_out_of_range" | "ui_radius_required"),
                "{}",
                err.reason
            );
        }
        let no_node =
            call(app.world_mut(), "ui.set_backdrop_blur", &json!({ "radius": 4 })).expect_err("node");
        assert_eq!(no_node.reason, "ui_node_required");
        assert!(app.world().get::<crate::node::BackdropBlur>(node).is_none());
    }

    #[test]
    fn set_value_refuses_a_disabled_widget() {
        let mut app = app();
        let toggle = app.world_mut().spawn((UiNode, Toggle::default(), Disabled)).id();
        let err = call(
            app.world_mut(),
            "ui.set_value",
            &json!({ "node": toggle.to_bits(), "value": true }),
        )
        .expect_err("disabled");
        assert_eq!(err.reason, "ui_node_disabled");
        assert_eq!(app.world().resource::<UiAgentBridge>().pending_frames(), 0);
    }

    #[test]
    fn click_and_focus_refuse_a_node_outside_the_top_layer() {
        let mut app = app();
        let outside = spawn_labelled(&mut app, "Outside");
        app.world_mut().entity_mut(outside).insert(ComputedNode {
            rect: [0.0, 0.0, 50.0, 20.0],
            clip_rect: [0.0, 0.0, 50.0, 20.0],
            scale: 1.0,
            ..Default::default()
        });
        let root = app
            .world_mut()
            .spawn((
                UiNode,
                crate::InputLayer {
                    mode: crate::LayerInputMode::UiOnly,
                },
            ))
            .id();
        let inside = spawn_labelled(&mut app, "Inside");
        app.world_mut().entity_mut(root).add_child(inside);
        app.update();
        assert_eq!(
            app.world().resource::<crate::layers::InputLayers>().top_root(),
            Some(root)
        );
        for method in ["ui.click", "ui.focus"] {
            let err = call(app.world_mut(), method, &json!({ "node": outside.to_bits() }))
                .expect_err("outside the layer");
            assert_eq!(err.reason, "ui_node_blocked_by_layer", "{method}");
        }
        call(app.world_mut(), "ui.focus", &json!({ "node": inside.to_bits() })).expect("inside is fine");
        assert_eq!(app.world().resource::<UiAgentBridge>().pending_frames(), 1);
    }

    /// Spec `ui-parity-roadmap.md` lane 4 / ADR-293 follow-up: `ui.click` carries `modifiers` through
    /// the real keyboard path, so the `Click` the UI sends reports them, and they are let go after.
    #[test]
    fn click_with_modifiers_reports_them_on_the_click_and_releases_them_after() {
        let mut app = app();
        let target = spawn_rect(&mut app, [10.0, 10.0, 60.0, 30.0]);
        let out = call(
            app.world_mut(),
            "ui.click",
            &json!({ "node": target.to_bits(), "modifiers": ["ctrl", "Shift", "shift"] }),
        )
        .expect("click");
        assert_eq!(out["queued_frames"], 3, "press, release, modifiers up: {out}");
        let mut clicks = Vec::new();
        for _ in 0..4 {
            app.update();
            clicks.extend(
                app.world_mut()
                    .resource_mut::<Messages<UiEvent>>()
                    .iter_current_update_messages()
                    .filter(|e| e.target == target && e.kind == UiEventKind::Click)
                    .cloned()
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(clicks.len(), 1, "{clicks:?}");
        let held = clicks[0].modifiers;
        assert!(held.shift && held.ctrl && !held.alt && !held.meta, "{held:?}");
        assert!(
            app.world().resource::<UiPointer>().modifiers.is_empty(),
            "the modifiers are let go after the click"
        );

        // A plain click still reports none and queues no extra frame.
        let out = call(app.world_mut(), "ui.click", &json!({ "node": target.to_bits() })).expect("click");
        assert_eq!(out["queued_frames"], 2);
        let bad = call(
            app.world_mut(),
            "ui.click",
            &json!({ "node": target.to_bits(), "modifiers": ["hyper"] }),
        )
        .expect_err("unknown modifier");
        assert_eq!(bad.reason, "ui_bad_param");
        let bad = call(
            app.world_mut(),
            "ui.click",
            &json!({ "node": target.to_bits(), "modifiers": "shift" }),
        )
        .expect_err("not a list");
        assert_eq!(bad.reason, "ui_bad_param");
    }

    #[test]
    fn click_rejects_a_non_string_button() {
        let mut app = app();
        let button = spawn_labelled(&mut app, "Go");
        let err = call(
            app.world_mut(),
            "ui.click",
            &json!({ "node": button.to_bits(), "button": 1 }),
        )
        .expect_err("not a string");
        assert_eq!(err.reason, "ui_bad_param");
    }

    #[test]
    fn click_refuses_a_node_without_an_area() {
        let mut app = app();
        let button = spawn_labelled(&mut app, "Go");
        let err = call(app.world_mut(), "ui.click", &json!({ "node": button.to_bits() }))
            .expect_err("not laid out");
        assert_eq!(err.reason, "ui_node_not_visible");
    }

    #[test]
    fn query_reports_the_resolved_layout_direction() {
        use crate::style::{Style, TextDir};
        let mut app = app();
        let world = app.world_mut();
        let rtl_panel = world
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    text_dir: TextDir::Rtl,
                    ..Style::default()
                }),
            ))
            .id();
        let inherits = world.spawn((UiNode, NodeStyle::default())).id();
        world.entity_mut(rtl_panel).add_child(inherits);
        let plain = world.spawn(UiNode).id();
        let dir = |app: &mut App, e: Entity| {
            let out = call(app.world_mut(), "ui.query", &json!({ "node": e.to_bits() })).expect("query");
            out["nodes"][0]["layout_dir"].clone()
        };
        assert_eq!(dir(&mut app, rtl_panel), "rtl");
        assert_eq!(
            dir(&mut app, inherits),
            "rtl",
            "Auto inherits the parent's direction"
        );
        assert_eq!(dir(&mut app, plain), "ltr");
        // With no explicit direction above it, a node follows the locale.
        app.world_mut().resource_mut::<Localization>().selected = "ar".to_owned();
        assert_eq!(dir(&mut app, plain), "rtl");
    }

    fn spawn_rect(app: &mut App, rect: [f32; 4]) -> Entity {
        app.world_mut()
            .spawn((
                UiNode,
                Interactable::default(),
                ComputedNode {
                    rect,
                    clip_rect: rect,
                    scale: 1.0,
                    ..Default::default()
                },
            ))
            .id()
    }

    fn drag_scene(app: &mut App) -> (Entity, Entity) {
        let item = spawn_rect(app, [0.0, 0.0, 40.0, 40.0]);
        app.world_mut()
            .entity_mut(item)
            .insert(DragSource::new(crate::dragdrop::DragPayload::new(
                "item",
                UiValue::Index(4),
            )));
        let slot = spawn_rect(app, [200.0, 0.0, 80.0, 80.0]);
        app.world_mut()
            .entity_mut(slot)
            .insert(DropTarget::accepting(["item"]));
        (item, slot)
    }

    fn dropped(app: &mut App) -> Vec<crate::dragdrop::DragDropEvent> {
        app.world_mut()
            .resource_mut::<Messages<crate::dragdrop::DragDropEvent>>()
            .iter_current_update_messages()
            .filter(|e| matches!(e, crate::dragdrop::DragDropEvent::Dropped { .. }))
            .cloned()
            .collect()
    }

    #[test]
    fn drag_drop_state_lists_sources_and_targets_and_query_marks_them() {
        let mut app = app();
        let (item, slot) = drag_scene(&mut app);
        let out = call(app.world_mut(), "ui.drag_drop", &json!({})).expect("state");
        assert_eq!(out["active"], false);
        assert_eq!(out["sources"][0]["node"], item.to_bits());
        assert_eq!(out["sources"][0]["kind"], "item");
        assert_eq!(out["targets"][0]["node"], slot.to_bits());
        assert_eq!(out["targets"][0]["accepts"], json!(["item"]));
        let q = call(app.world_mut(), "ui.query", &json!({ "node": item.to_bits() })).expect("query");
        assert_eq!(q["nodes"][0]["drag_kind"], "item");
        assert_eq!(q["nodes"][0]["drop_accepts"], Value::Null);
    }

    #[test]
    fn drag_action_drives_a_real_drop_through_the_input_path() {
        let mut app = app();
        let (item, slot) = drag_scene(&mut app);
        let out = call(
            app.world_mut(),
            "ui.drag_drop",
            &json!({ "action": "drag", "from": { "node": item.to_bits() }, "to": { "node": slot.to_bits() } }),
        )
        .expect("drag");
        assert_eq!(out["queued_frames"], 5);
        let mut drops = Vec::new();
        for _ in 0..6 {
            app.update();
            drops.extend(dropped(&mut app));
        }
        assert_eq!(drops.len(), 1, "{drops:?}");
        assert!(matches!(
            &drops[0],
            crate::dragdrop::DragDropEvent::Dropped { source, target, .. } if *source == item && *target == slot
        ));
    }

    #[test]
    fn held_drag_shows_hover_then_cancel_ends_it_without_a_drop() {
        let mut app = app();
        let (item, slot) = drag_scene(&mut app);
        call(
            app.world_mut(),
            "ui.drag_drop",
            &json!({ "action": "drag", "from": { "node": item.to_bits() }, "to": { "node": slot.to_bits() }, "release": false }),
        )
        .expect("drag");
        let mut drops = 0;
        for _ in 0..5 {
            app.update();
            drops += dropped(&mut app).len();
        }
        let state = call(app.world_mut(), "ui.drag_drop", &json!({})).expect("state");
        assert_eq!(state["active"], true);
        assert_eq!(state["kind"], "item");
        assert_eq!(state["hover"]["node"], slot.to_bits());
        assert_eq!(state["hover"]["accepted"], true);
        let out = call(app.world_mut(), "ui.drag_drop", &json!({ "action": "cancel" })).expect("cancel");
        assert_eq!(out["queued_frames"], 1, "the held button is released too");
        for _ in 0..3 {
            app.update();
            drops += dropped(&mut app).len();
        }
        assert_eq!(drops, 0);
        let state = call(app.world_mut(), "ui.drag_drop", &json!({})).expect("state");
        assert_eq!(state["active"], false);
    }

    #[test]
    fn drag_drop_validates_before_queueing() {
        let mut app = app();
        let (item, slot) = drag_scene(&mut app);
        let plain = spawn_rect(&mut app, [300.0, 300.0, 10.0, 10.0]);
        let reason = |app: &mut App, params: Value| {
            call(app.world_mut(), "ui.drag_drop", &params)
                .expect_err("refused")
                .reason
        };
        assert_eq!(reason(&mut app, json!({ "action": "spin" })), "ui_bad_param");
        assert_eq!(reason(&mut app, json!({ "action": "drop" })), "ui_no_drag");
        assert_eq!(reason(&mut app, json!({ "action": "cancel" })), "ui_no_drag");
        assert_eq!(reason(&mut app, json!({ "action": "drag" })), "ui_node_required");
        assert_eq!(
            reason(
                &mut app,
                json!({ "action": "drag", "from": { "node": plain.to_bits() }, "at": [1, 1] })
            ),
            "ui_not_draggable"
        );
        assert_eq!(
            reason(
                &mut app,
                json!({ "action": "drag", "from": { "node": item.to_bits() } })
            ),
            "ui_drag_target_required"
        );
        assert_eq!(
            reason(
                &mut app,
                json!({ "action": "drag", "from": { "node": item.to_bits() }, "to": { "node": plain.to_bits() } })
            ),
            "ui_not_drop_target"
        );
        assert_eq!(
            reason(
                &mut app,
                json!({ "action": "drag", "from": 3, "to": { "node": slot.to_bits() } })
            ),
            "ui_bad_param"
        );
        assert_eq!(
            app.world().resource::<UiAgentBridge>().pending_frames(),
            0,
            "a refused call queues nothing"
        );
    }

    #[test]
    fn drag_presses_where_the_window_pointer_can_reach_the_node() {
        // Audit ui1 correctness 2: ui.drag_drop must add window_origin like ui.click, or the
        // press lands on whatever sits at the un-offset rect.
        let mut app = app();
        let item = spawn_rect(&mut app, [10.0, 10.0, 40.0, 40.0]);
        app.world_mut().entity_mut(item).insert((
            DragSource::new(crate::dragdrop::DragPayload::new("item", UiValue::Index(0))),
            crate::world_target::UiTargetNode {
                root: item,
                window_origin: Some([960.0, 0.0]),
            },
        ));
        let out = call(
            app.world_mut(),
            "ui.drag_drop",
            &json!({ "action": "drag", "from": { "node": item.to_bits() }, "at": [100, 100] }),
        )
        .expect("drag");
        assert_eq!(out["queued_frames"], 5);
        let frames = &app.world().resource::<UiAgentBridge>().frames;
        assert_eq!(
            frames[0],
            vec![Step::MoveCursor([990.0, 30.0])],
            "the press is at the local centre (30, 30) plus the window origin (960, 0)"
        );
    }

    #[test]
    fn drag_refuses_a_node_the_window_pointer_cannot_reach() {
        // Audit ui1 correctness 2: a drag source on a world panel or camera texture is pressed
        // nowhere useful; refuse it like ui.click does.
        let mut app = app();
        let item = spawn_rect(&mut app, [10.0, 10.0, 40.0, 40.0]);
        app.world_mut().entity_mut(item).insert((
            DragSource::new(crate::dragdrop::DragPayload::new("item", UiValue::Index(0))),
            crate::world_target::UiTargetNode {
                root: item,
                window_origin: None,
            },
        ));
        let err = call(
            app.world_mut(),
            "ui.drag_drop",
            &json!({ "action": "drag", "from": { "node": item.to_bits() }, "at": [100, 100] }),
        )
        .expect_err("off window");
        assert_eq!(err.reason, "ui_node_off_window");
        assert_eq!(app.world().resource::<UiAgentBridge>().pending_frames(), 0);
    }

    fn localized_app() -> App {
        let mut app = app();
        let loc = Localization::new(
            vec![
                (
                    "en".to_owned(),
                    "go = Go\nitems = { $count ->\n    [one] One item\n   *[other] { $count } items\n}\n"
                        .to_owned(),
                ),
                ("ja".to_owned(), "go = 行く\nitems = { $count }個\n".to_owned()),
            ],
            Some("en".to_owned()),
        )
        .expect("ftl");
        app.world_mut().insert_resource(loc);
        app
    }

    #[test]
    fn set_locale_re_resolves_node_text() {
        let mut app = localized_app();
        let label = app
            .world_mut()
            .spawn((
                UiNode,
                Text {
                    spans: vec![crate::text::TextSpan::loc("go")],
                    ..Default::default()
                },
            ))
            .id();
        let before = call(app.world_mut(), "ui.query", &json!({ "text": "Go" })).expect("query");
        assert_eq!(before["count"], 1);
        let reply = call(app.world_mut(), "ui.set_locale", &json!({ "locale": "ja" })).expect("set");
        assert_eq!(reply["changed"], true);
        assert_eq!(reply["previous"], "en");
        assert_eq!(reply["generation"], 1);
        app.update();
        let after = call(app.world_mut(), "ui.query", &json!({ "text": "行く" })).expect("query");
        assert_eq!(after["nodes"][0]["id"], label.to_bits());
        let state = call(app.world_mut(), "ui.state", &json!({})).expect("state");
        assert_eq!(state["locale"]["selected"], "ja");
        // Setting the same locale again is not a change and does not bump the generation.
        let again = call(app.world_mut(), "ui.set_locale", &json!({ "locale": "ja" })).expect("set");
        assert_eq!(again["changed"], false);
        assert_eq!(again["generation"], 1);
    }

    #[test]
    fn set_locale_errors() {
        let mut app = localized_app();
        let none = call(app.world_mut(), "ui.set_locale", &json!({})).expect_err("required");
        assert_eq!(none.reason, "ui_locale_required");
        let unknown =
            call(app.world_mut(), "ui.set_locale", &json!({ "locale": "zz" })).expect_err("unknown");
        assert_eq!(unknown.reason, "ui_locale_unknown");
        let state = call(app.world_mut(), "ui.state", &json!({})).expect("state");
        assert_eq!(state["locale"]["selected"], "en");
    }

    #[test]
    fn resolve_formats_plurals_per_locale() {
        let mut app = localized_app();
        let one = call(
            app.world_mut(),
            "ui.resolve",
            &json!({ "key": "items", "args": { "count": 1 } }),
        )
        .expect("resolve");
        assert_eq!(one["text"], "One item");
        assert_eq!(one["found"], true);
        let many = call(
            app.world_mut(),
            "ui.resolve",
            &json!({ "key": "items", "args": { "count": 5 } }),
        )
        .expect("resolve");
        assert_eq!(many["text"], "5 items");
        let ja = call(
            app.world_mut(),
            "ui.resolve",
            &json!({ "key": "items", "args": { "count": 5 }, "locale": "ja" }),
        )
        .expect("resolve");
        assert_eq!(ja["text"], "5個");
        assert_eq!(ja["locale"], "ja");
        let missing = call(
            app.world_mut(),
            "ui.resolve",
            &json!({ "key": "nope", "locale": "ja" }),
        )
        .expect("resolve");
        assert_eq!(missing["found"], false);
        let no_key = call(app.world_mut(), "ui.resolve", &json!({})).expect_err("key");
        assert_eq!(no_key.reason, "ui_key_required");
        let bad = call(
            app.world_mut(),
            "ui.resolve",
            &json!({ "key": "go", "args": [1] }),
        )
        .expect_err("args");
        assert_eq!(bad.reason, "ui_bad_param");
        let unknown = call(
            app.world_mut(),
            "ui.resolve",
            &json!({ "key": "go", "locale": "zz" }),
        )
        .expect_err("locale");
        assert_eq!(unknown.reason, "ui_locale_unknown");
    }
}
