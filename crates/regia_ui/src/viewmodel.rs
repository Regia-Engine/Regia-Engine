// SPDX-License-Identifier: MPL-2.0
//! View models, two-way bindings and UI commands (ADR-1811, `docs/specs/81-ui-designer.md` § 2).
//!
//! A *view model* is a reflected resource the game registers with
//! [`ViewModelAppExt::register_view_model`]; `.ui.ron` nodes bind widget properties to its
//! fields (`bindings: [(source: "HudVm.health", target: progress)]`). One-way refresh is the
//! Tier 1 [`crate::binding::update_bindings`]; this module adds the widget → model direction
//! ([`collect_two_way_edits`] + [`apply_two_way_edits`]) and click / submit commands
//! ([`emit_ui_commands`]). The pattern is the public presentation-model / model-view-viewmodel
//! split (M. Fowler, "Presentation Model", 2004; J. Gossman, 2005).

use regia_app::prelude::*;
use regia_ecs::reflect::{GetTypeRegistration, PartialReflect, ReflectMut, TypeInfo, TypeRegistry, Typed};

use crate::binding::{Bind, BindConverter, BindMode, BindSource, BindTarget, reflect_to_string};
use crate::interaction::{UiEvent, UiEventKind, UiValue};
use crate::node::NodeName;

/// Several bindings on one node (`.ui.ron` `bindings`, v3). Refreshed by
/// [`crate::binding::update_bindings`] like a single [`Bind`].
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct ViewBindings(pub Vec<Bind>);

/// The command a node sends as a [`UiCommand`] when clicked or when its text is submitted
/// (`.ui.ron` `command`, v3).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct UiCommandName(pub String);

/// A command from the UI: a click on, or a text submit from, a node with a [`UiCommandName`].
#[derive(Message, Debug, Clone, PartialEq)]
pub struct UiCommand {
    /// The command name from the layout (`"settings.apply"`).
    pub name: String,
    /// The node that sent it.
    pub node: Entity,
    /// The submitted text for a submit; `UiValue::None` for a click.
    pub value: UiValue,
    /// The stable key of the collection item the node was generated for (ADR-2489); `None`
    /// outside a repeat template.
    pub item: Option<String>,
}

/// One field of a registered view model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewModelField {
    /// Field name.
    pub name: String,
    /// `number`, `integer`, `bool`, `text`, `list` or `other`.
    pub kind: &'static str,
    /// The field's full type path.
    pub type_path: String,
    /// A `list` field's item fields (a struct item's named fields; empty otherwise). The designer's
    /// picker and lint read them for `.field` bindings in repeat templates (ADR-2489).
    pub item_fields: Vec<ViewModelField>,
}

/// A registered view model: its short name (the first segment of a binding path) and fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewModelInfo {
    /// Short type name (`"HudVm"`).
    pub name: String,
    /// Full type path.
    pub type_path: String,
    /// Named fields (empty for a model that is not a struct).
    pub fields: Vec<ViewModelField>,
}

impl ViewModelInfo {
    /// The field named `name`.
    pub fn field(&self, name: &str) -> Option<&ViewModelField> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// The registered view models (the designer's binding picker and lint, agents, and the
/// two-way write rule: only fields of a registered model are written).
#[derive(Resource, Debug, Clone, Default)]
pub struct ViewModels {
    models: Vec<ViewModelInfo>,
}

impl ViewModels {
    /// Every registered model, in registration order.
    pub fn models(&self) -> &[ViewModelInfo] {
        &self.models
    }

    /// The model named `name` (short type name).
    pub fn get(&self, name: &str) -> Option<&ViewModelInfo> {
        self.models.iter().find(|m| m.name == name)
    }

    /// Records a model from its reflected type info (replaces one of the same name).
    pub fn record(&mut self, info: &TypeInfo) {
        let name = info.type_path_table().short_path().to_owned();
        let model = ViewModelInfo {
            name,
            type_path: info.type_path().to_owned(),
            fields: struct_fields(info, MAX_ITEM_FIELD_DEPTH),
        };
        match self.models.iter_mut().find(|m| m.name == model.name) {
            Some(slot) => *slot = model,
            None => self.models.push(model),
        }
    }

    /// Every bound node with each binding's path, target, mode, last value and error, in a
    /// stable order (entity bits, then binding index).
    pub fn status(world: &mut World) -> Vec<BindingStatus> {
        let mut out = Vec::new();
        let mut q = world.query::<(Entity, Option<&NodeName>, Option<&Bind>, Option<&ViewBindings>)>();
        for (entity, name, single, list) in q.iter(world) {
            let binds = single
                .into_iter()
                .chain(list.into_iter().flat_map(|l| l.0.iter()));
            for (index, b) in binds.enumerate() {
                out.push(BindingStatus {
                    node: entity,
                    name: name.map(|n| n.0.clone()),
                    index,
                    path: b.path().to_owned(),
                    target: b.target,
                    mode: b.mode,
                    invert: b.invert,
                    last: b.last_value().map(str::to_owned),
                    error: b.last_error().map(str::to_owned),
                });
            }
        }
        out.sort_by_key(|s| (s.node.to_bits(), s.index));
        out
    }
}

/// How deep [`struct_fields`] follows lists of structs; a recursive item type would never end.
const MAX_ITEM_FIELD_DEPTH: u32 = 4;

/// A struct's named fields; a list field also lists its item's fields, `depth` levels down.
fn struct_fields(info: &TypeInfo, depth: u32) -> Vec<ViewModelField> {
    let TypeInfo::Struct(s) = info else {
        return Vec::new();
    };
    s.iter()
        .map(|f| {
            let item = match f.type_info() {
                Some(TypeInfo::List(l)) => Some(l.item_info()),
                Some(TypeInfo::Array(a)) => Some(a.item_info()),
                _ => None,
            };
            let item_fields = match item.flatten() {
                Some(info) if depth > 0 => struct_fields(info, depth - 1),
                _ => Vec::new(),
            };
            ViewModelField {
                name: f.name().to_owned(),
                kind: if item.is_some() {
                    "list"
                } else {
                    field_kind(f.type_path())
                },
                type_path: f.type_path().to_owned(),
                item_fields,
            }
        })
        .collect()
}

/// The type kind a binding sees: `number`, `integer`, `bool`, `text`, `list` or `other`.
fn field_kind(type_path: &str) -> &'static str {
    match type_path {
        "f32" | "f64" => "number",
        "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize" => "integer",
        "bool" => "bool",
        "alloc::string::String" | "String" => "text",
        _ => "other",
    }
}

/// One binding's state, for agents and the designer (`ViewModels::status`).
#[derive(Debug, Clone, PartialEq)]
pub struct BindingStatus {
    /// The bound node.
    pub node: Entity,
    /// Its `NodeName`, when it has one.
    pub name: Option<String>,
    /// Index among the node's bindings (the single `Bind` first, then `ViewBindings`).
    pub index: usize,
    /// Source path.
    pub path: String,
    /// Target property.
    pub target: BindTarget,
    /// Direction.
    pub mode: BindMode,
    /// Boolean inversion.
    pub invert: bool,
    /// The last value pushed.
    pub last: Option<String>,
    /// The last error, prefixed with its code (`binding_unresolved`, `binding_not_view_model`,
    /// `binding_type_mismatch`).
    pub error: Option<String>,
}

/// Registering view models on the app.
pub trait ViewModelAppExt {
    /// Registers `T` as a view model: reflection registration, the default resource when absent,
    /// and an entry in [`ViewModels`].
    fn register_view_model<T>(&mut self) -> &mut Self
    where
        T: Resource + Reflect + Typed + GetTypeRegistration + Default;
}

impl ViewModelAppExt for App {
    fn register_view_model<T>(&mut self) -> &mut Self
    where
        T: Resource + Reflect + Typed + GetTypeRegistration + Default,
    {
        self.register_type::<T>();
        if !self.world().contains_resource::<T>() {
            self.world_mut().insert_resource(T::default());
        }
        self.world_mut()
            .get_resource_or_insert_with(ViewModels::default)
            .record(T::type_info());
        self
    }
}

/// Where a two-way edit goes: the node's single `Bind` (`None`) or a `ViewBindings` slot.
#[derive(Debug, Clone, PartialEq)]
struct PendingWrite {
    node: Entity,
    slot: Option<usize>,
    path: String,
    /// `path` is relative to the node's collection item.
    item: bool,
    invert: bool,
    converter: Option<BindConverter>,
    value: UiValue,
}

/// Two-way edits read this frame, applied by [`apply_two_way_edits`].
#[derive(Resource, Debug, Default)]
pub struct PendingTwoWayWrites(Vec<PendingWrite>);

/// Whether a widget value feeds a target (`Checked` ← bool, `Value` ← number, `Selected` ←
/// index, `Input` ← submitted text).
fn value_fits(target: BindTarget, value: &UiValue) -> bool {
    matches!(
        (target, value),
        (BindTarget::Checked, UiValue::Bool(_))
            | (BindTarget::Value, UiValue::Number(_))
            | (BindTarget::Selected, UiValue::Index(_))
            | (BindTarget::Input, UiValue::Text(_))
    )
}

/// `Update`, after the widgets: collects this frame's widget edits on nodes with a two-way
/// binding whose target takes the edited value.
pub fn collect_two_way_edits(
    mut events: MessageReader<UiEvent>,
    binds: Query<(Option<&Bind>, Option<&ViewBindings>)>,
    mut pending: ResMut<PendingTwoWayWrites>,
) {
    let _span = tracing::debug_span!("collect_two_way_edits").entered();
    for event in events.read() {
        let value = match &event.kind {
            UiEventKind::ValueChanged(v @ (UiValue::Bool(_) | UiValue::Number(_) | UiValue::Index(_))) => {
                v.clone()
            }
            UiEventKind::Submitted(text) => UiValue::Text(text.clone()),
            _ => continue,
        };
        let Ok((single, list)) = binds.get(event.target) else {
            continue;
        };
        let candidates = single.map(|b| (None, b)).into_iter().chain(
            list.into_iter()
                .flat_map(|l| l.0.iter().enumerate().map(|(i, b)| (Some(i), b))),
        );
        for (slot, bind) in candidates {
            // A theme token or an animation has no field to write back (ADR-7751).
            let writable = !matches!(bind.source, BindSource::Theme(_) | BindSource::Animation(_));
            if writable && bind.mode == BindMode::TwoWay && value_fits(bind.target, &value) {
                pending.0.push(PendingWrite {
                    node: event.target,
                    slot,
                    path: bind.path().to_owned(),
                    item: matches!(bind.source, BindSource::Item(_)),
                    invert: bind.invert,
                    converter: bind.converter.clone(),
                    value: value.clone(),
                });
            }
        }
    }
}

/// `Update`, after [`collect_two_way_edits`]: writes each edit into its view-model field through
/// reflection, then records the written value as the binding's `last` so the next refresh does
/// not push it back. A failure is kept as the binding's error (`binding_not_view_model`,
/// `binding_unresolved`, `binding_type_mismatch`) and nothing is written.
pub fn apply_two_way_edits(world: &mut World) {
    let writes = std::mem::take(&mut world.resource_mut::<PendingTwoWayWrites>().0);
    if writes.is_empty() {
        return;
    }
    let _span = tracing::trace_span!("ui_two_way_writes").entered();
    let Some(registry) = world
        .get_resource::<regia_ecs::prelude::AppTypeRegistry>()
        .map(|r| r.0.clone())
    else {
        return;
    };
    let registry = registry.read();
    let models = world.get_resource::<ViewModels>().cloned().unwrap_or_default();
    for write in writes {
        let outcome = write_field(world, &registry, &models, &write);
        if let Err(reason) = &outcome {
            tracing::debug!(target: "regia_ui", "two-way binding {}: {reason}", write.path);
        }
        let update = |b: &mut Bind| match &outcome {
            Ok(raw) => {
                b.last = Some(b.display(raw));
                // The model now holds `raw`: a later change back to the old value (a game that
                // refuses the edit) must not look unchanged to the refresh.
                b.last_raw = Some(raw.clone());
                b.error = None;
            }
            Err(reason) => b.error = Some(reason.clone()),
        };
        match write.slot {
            None => {
                if let Some(mut b) = world.get_mut::<Bind>(write.node) {
                    update(&mut b);
                }
            }
            Some(i) => {
                if let Some(mut list) = world.get_mut::<ViewBindings>(write.node)
                    && let Some(b) = list.0.get_mut(i)
                {
                    update(b);
                }
            }
        }
    }
}

/// Writes one edit; returns the field's new raw (unformatted) value.
fn write_field(
    world: &mut World,
    registry: &TypeRegistry,
    models: &ViewModels,
    write: &PendingWrite,
) -> Result<String, String> {
    let absolute = if write.item {
        crate::repeat::absolute_item_path(world, write.node, &write.path)?
    } else {
        write.path.clone()
    };
    let Some((container, path)) = absolute.split_once('.') else {
        return Err(format!("binding_unresolved: path {absolute:?} has no field"));
    };
    if models.get(container).is_none() {
        return Err(format!(
            "binding_not_view_model: {container:?} is not a registered view model (App::register_view_model); the binding stays one way"
        ));
    }
    let found = crate::binding::registrations_named(registry, container).find_map(|registration| {
        let type_id = registration.type_id();
        let component_id = world.components().get_valid_id(type_id)?;
        Some((type_id, world.resource_entities().get(component_id)?))
    });
    let Some((type_id, entity)) = found else {
        return Err(format!(
            "binding_unresolved: resource {container:?} is not in the world"
        ));
    };
    let mut model = world
        .get_reflect_mut(entity, type_id)
        .map_err(|e| format!("binding_unresolved: {container:?}: {e}"))?;
    let value = match &write.converter {
        Some(converter) => converter
            .invert(&write.value)
            .map_err(|reason| format!("binding_converter_one_way: {}: {reason}", write.path))?,
        None => write.value.clone(),
    };
    let field = resolve_path_mut(model.as_partial_reflect_mut(), path)
        .map_err(|reason| format!("binding_unresolved: {reason}"))?;
    set_field(field, &value, write.invert)
        .map_err(|reason| format!("binding_type_mismatch: {}: {reason}", write.path))?;
    Ok(reflect_to_string(field))
}

/// `Container.field.path` resolved mutably.
fn resolve_path_mut<'a>(
    value: &'a mut dyn PartialReflect,
    path: &str,
) -> Result<&'a mut dyn PartialReflect, String> {
    let mut current = value;
    for segment in path.split('.') {
        let type_path = current.get_represented_type_info().map_or("?", |i| i.type_path());
        let next = match current.reflect_mut() {
            ReflectMut::Struct(s) => s.field_mut(segment),
            ReflectMut::TupleStruct(s) => segment.parse::<usize>().ok().and_then(|i| s.field_mut(i)),
            ReflectMut::Enum(e) => e.field_mut(segment),
            ReflectMut::List(l) => segment.parse::<usize>().ok().and_then(|i| l.get_mut(i)),
            ReflectMut::Array(a) => segment.parse::<usize>().ok().and_then(|i| a.get_mut(i)),
            _ => None,
        };
        current = next.ok_or_else(|| format!("no field {segment:?} on {type_path}"))?;
    }
    Ok(current)
}

/// Converts a widget value to the field's type and stores it. Integers round to nearest and
/// must fit the field's type.
fn set_field(field: &mut dyn PartialReflect, value: &UiValue, invert: bool) -> Result<(), String> {
    let field_type = field.reflect_type_path().to_owned();
    match value {
        UiValue::Bool(b) => {
            let b = *b != invert;
            if let Some(f) = field.try_downcast_mut::<bool>() {
                *f = b;
                return Ok(());
            }
        }
        UiValue::Text(t) => {
            if let Some(f) = field.try_downcast_mut::<String>() {
                f.clone_from(t);
                return Ok(());
            }
        }
        UiValue::Number(_) | UiValue::Index(_) => {
            let n = match value {
                UiValue::Number(n) => f64::from(*n),
                UiValue::Index(i) => *i as f64,
                _ => 0.0,
            };
            if let Some(f) = field.try_downcast_mut::<f32>() {
                *f = n as f32;
                return Ok(());
            }
            if let Some(f) = field.try_downcast_mut::<f64>() {
                *f = n;
                return Ok(());
            }
            let rounded = n.round();
            if !rounded.is_finite() {
                return Err(format!("{n} is not a finite number"));
            }
            let wide = rounded as i128;
            macro_rules! ints {
                ($($t:ty),*) => {$(
                    if let Some(f) = field.try_downcast_mut::<$t>() {
                        *f = <$t>::try_from(wide)
                            .map_err(|_| format!("{wide} does not fit {}", stringify!($t)))?;
                        return Ok(());
                    }
                )*};
            }
            ints!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);
        }
        UiValue::None => {}
    }
    Err(format!(
        "a {} value cannot be stored in a {field_type} field",
        value_kind(value)
    ))
}

fn value_kind(value: &UiValue) -> &'static str {
    match value {
        UiValue::None => "empty",
        UiValue::Bool(_) => "bool",
        UiValue::Number(_) => "number",
        UiValue::Index(_) => "index",
        UiValue::Text(_) => "text",
    }
}

/// `Update`, after the widgets: a click on, or a submit from, a node with a [`UiCommandName`]
/// sends a [`UiCommand`].
pub fn emit_ui_commands(
    mut events: MessageReader<UiEvent>,
    names: Query<(&UiCommandName, Option<&crate::repeat::RepeatBound>)>,
    mut out: MessageWriter<UiCommand>,
) {
    let _span = tracing::debug_span!("emit_ui_commands").entered();
    for event in events.read() {
        let value = match &event.kind {
            UiEventKind::Click => UiValue::None,
            UiEventKind::Submitted(text) => UiValue::Text(text.clone()),
            _ => continue,
        };
        if let Ok((name, bound)) = names.get(event.target) {
            out.write(UiCommand {
                name: name.0.clone(),
                node: event.target,
                value,
                item: bound.map(|b| b.key.to_string()),
            });
        }
    }
}

/// `ui.view_models`: every registered view model with its fields and current values, and every
/// binding's status (`book/agents/ui-designer.md`).
pub fn agent_view_models(world: &mut World) -> serde_json::Value {
    use serde_json::json;
    let models = world.get_resource::<ViewModels>().cloned().unwrap_or_default();
    let registry = world
        .get_resource::<regia_ecs::prelude::AppTypeRegistry>()
        .map(|r| r.0.clone());
    let mut out_models = Vec::new();
    for model in models.models() {
        let fields: Vec<serde_json::Value> = model
            .fields
            .iter()
            .map(|f| {
                let value = registry.as_ref().and_then(|r| {
                    crate::binding::resolve_resource(world, &r.read(), &model.name, &f.name).ok()
                });
                let items: Vec<&str> = f.item_fields.iter().map(|i| i.name.as_str()).collect();
                json!({ "name": f.name, "kind": f.kind, "type": f.type_path, "value": value, "item_fields": items })
            })
            .collect();
        out_models.push(json!({ "name": model.name, "type": model.type_path, "fields": fields }));
    }
    let bindings: Vec<serde_json::Value> = ViewModels::status(world)
        .into_iter()
        .map(|s| {
            let code = s
                .error
                .as_deref()
                .map(|e| e.split(':').next().unwrap_or(e).to_owned());
            json!({
                "node": s.node.to_bits(),
                "name": s.name,
                "index": s.index,
                "source": s.path,
                "target": s.target.name(),
                "mode": s.mode.name(),
                "invert": s.invert,
                "last": s.last,
                "code": code,
                "error": s.error,
            })
        })
        .collect();
    let errors = bindings.iter().filter(|b| !b["code"].is_null()).count();
    json!({ "models": out_models, "bindings": bindings, "errors": errors })
}

/// Installs the view-model systems (added by [`crate::UiPlugin`]).
pub struct ViewModelPlugin;

impl Plugin for ViewModelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ViewModels>()
            .init_resource::<PendingTwoWayWrites>()
            .add_message::<UiCommand>()
            .add_systems(
                Update,
                (
                    (collect_two_way_edits, apply_two_way_edits).chain(),
                    emit_ui_commands,
                )
                    .after(crate::widgets::ui_widgets),
            );
    }
}
