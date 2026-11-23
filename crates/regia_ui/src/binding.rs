// SPDX-License-Identifier: MPL-2.0
//! Data binding (`specs/18` § 5, Tier 1): `Bind` components poll a reflected resource field each
//! frame (change-detected by formatting) and push the formatted value into a target property of
//! the same entity's UI components. DO27 HUDs bind score/health/waypoint data this way.
//!
//! View-model binding (ADR-1811, `docs/specs/81-ui-designer.md`) extends it: a [`BindMode`]
//! (one way, two way, one time), `invert` for booleans, the targets `Value`, `Input`, `Selected`,
//! `Visible` and `Enabled`, and several bindings per node ([`crate::viewmodel::ViewBindings`]).
//! Two-way writes live in [`crate::viewmodel`]. ADR-2489 adds [`BindConverter`] between the field and
//! the format, and sources relative to a collection item ([`BindSource::Item`], `crate::repeat`).
//!
//! A binding whose source resource or component has not changed since this system last ran is not
//! formatted again. A format or converter edit on an unchanged source applies on the next source
//! change. When the source is marked changed but the raw field text is the same, `display` is skipped.

use std::any::TypeId;

use regia_app::prelude::*;
use regia_ecs::reflect::{PartialReflect, ReflectRef, TypeRegistration, TypeRegistry};
use serde::{Deserialize, Serialize};

pub use crate::converter::BindConverter;
use crate::style::Display;
use crate::widgets::ProgressBar;
use crate::{UiError, text::TextSpan};

#[cfg(test)]
thread_local! {
    /// Reflect calls on this test thread (bindings are not formatted when the source is unchanged).
    pub(crate) static REFLECT_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Display calls on this test thread (skipped when the raw field text is unchanged).
    pub(crate) static DISPLAY_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Where a binding reads from.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum BindSource {
    /// No source (binding disabled).
    #[default]
    None,
    /// A world resource: `"TypeName.field.path"` — the resource is found by short type name,
    /// the value by reflecting into its fields.
    Resource(String),
    /// A component on the same entity: `"ComponentName.field.path"`.
    Component(String),
    /// A field of the collection item this node was generated for: `".field.path"`, or `"."` for
    /// the item itself (ADR-2489). The enclosing repeat node refreshes it, only when the model
    /// changes; outside a repeat it reports `binding_item_outside_repeat`.
    Item(String),
    /// A field of a script on this entity or the nearest ancestor that has it:
    /// `"ScriptName.field.path"`, the script found by the short name of its type (ADR-7070). The
    /// script exposes its fields by returning itself from `Script::reflect_fields`
    /// (`regia_framework`); the framework's [`ScriptFieldReader`] resolves the path. Without that
    /// reader (no script plugin) it reports `binding_unresolved`.
    ScriptField(String),
    /// A token of the active theme (`"color.accent"`, `"size.control.md"`; `.ui.ron` v11 `theme`,
    /// ADR-7751). Re-read when the theme generation moves, so a theme switch (or a density or
    /// contrast change) pushes the new value. A colour shows as `#rrggbb` (sRGB; `#rrggbbaa` below full alpha), a number or a
    /// duration as its number, text as itself. An unknown token reports `binding_theme_token`.
    Theme(String),
    /// A value animated over time (`.ui.ron` v11 `animation`, ADR-7751): the binding's own clock,
    /// fed by the game clock (`Time`), samples [`BindAnimation::sample`] each frame and pushes the
    /// number. Reduced motion pushes the end value at once.
    Animation(BindAnimation),
}

/// How an animation binding repeats (ADR-7751).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnimationRepeat {
    /// Play once and hold `to`.
    #[default]
    Once,
    /// Start again from `from` after each pass.
    Loop,
    /// Alternate `from` → `to` and back.
    PingPong,
}

impl AnimationRepeat {
    /// Every mode, in declaration order (designer pickers).
    pub const ALL: [AnimationRepeat; 3] = [Self::Once, Self::Loop, Self::PingPong];

    /// The `.ui.ron` spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Loop => "loop",
            Self::PingPong => "ping_pong",
        }
    }
}

/// The runtime form of an animation binding (ADR-7751): a number eased from `from` to `to` over
/// `duration` seconds after `delay`, repeated by `repeat`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BindAnimation {
    /// Value at the start of a pass.
    pub from: f32,
    /// Value at the end of a pass.
    pub to: f32,
    /// Seconds per pass; zero, negative or non-finite jumps to `to`.
    pub duration: f32,
    /// Seconds before the first pass (the value holds `from` meanwhile).
    pub delay: f32,
    /// Easing of each pass.
    pub ease: crate::tween::Ease,
    /// What happens after a pass.
    pub repeat: AnimationRepeat,
}

impl BindAnimation {
    /// The value at `t` seconds of the binding's clock: a pure function, so a replayed clock gives
    /// the same values. Non-finite input never panics: a NaN clock reads as 0, and a non-finite
    /// result shows 0.
    pub fn sample(&self, t: f32) -> f32 {
        let t = if t.is_nan() { 0.0 } else { t };
        let delay = if self.delay.is_finite() {
            self.delay.max(0.0)
        } else {
            0.0
        };
        let local = t - delay;
        let progress = if local <= 0.0 {
            0.0
        } else if !(self.duration.is_finite() && self.duration > 0.0) {
            1.0
        } else {
            let passes = local / self.duration;
            match self.repeat {
                AnimationRepeat::Once => passes.min(1.0),
                AnimationRepeat::Loop if passes.is_finite() => passes.fract(),
                AnimationRepeat::PingPong if passes.is_finite() => {
                    let pass = passes.floor();
                    let f = passes - pass;
                    if (pass as u64).is_multiple_of(2) {
                        f
                    } else {
                        1.0 - f
                    }
                }
                AnimationRepeat::Loop | AnimationRepeat::PingPong => 1.0,
            }
        };
        let eased = self.ease.apply(progress);
        let value = self.from + (self.to - self.from) * eased;
        if value.is_finite() { value } else { 0.0 }
    }

    /// Whether the value can still change after `t` seconds (a `once` animation past its end
    /// cannot).
    pub fn running_at(&self, t: f32) -> bool {
        match self.repeat {
            AnimationRepeat::Once => {
                let delay = if self.delay.is_finite() {
                    self.delay.max(0.0)
                } else {
                    0.0
                };
                let duration = if self.duration.is_finite() {
                    self.duration.max(0.0)
                } else {
                    0.0
                };
                t.is_nan() || t < delay + duration
            }
            AnimationRepeat::Loop | AnimationRepeat::PingPong => true,
        }
    }
}

/// Reads a script field for [`BindSource::ScriptField`]: `(world, bound entity, script short name,
/// field path)` to the reflected field, or why not. `regia_framework::script::ScriptPlugin` installs
/// it; the UI crate cannot see scripts itself.
pub type ScriptFieldFn = for<'w> fn(&'w World, Entity, &str, &str) -> Result<&'w dyn PartialReflect, String>;

/// The installed [`ScriptFieldFn`] (ADR-7070).
#[derive(Resource, Clone, Copy)]
pub struct ScriptFieldReader(pub ScriptFieldFn);

impl std::fmt::Debug for ScriptFieldReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScriptFieldReader")
    }
}

impl BindSource {
    /// Parses the path into `(container, path)`.
    fn split(&self) -> Option<(&str, &str)> {
        match self {
            BindSource::None | BindSource::Item(_) | BindSource::Theme(_) | BindSource::Animation(_) => None,
            BindSource::Resource(s) | BindSource::Component(s) | BindSource::ScriptField(s) => {
                s.split_once('.')
            }
        }
    }
}

/// Where a binding writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BindTarget {
    /// The entity's `Text` (first span) — replaces its content.
    #[default]
    Text,
    /// The entity's `ProgressBar.value` (parses the number).
    Progress,
    /// The entity's `Opacity` (0..1).
    Opacity,
    /// The entity's `Toggle.checked`.
    Checked,
    /// The entity's `Slider.value` (clamped to the slider's range).
    Value,
    /// The entity's `TextInput.text` (the caret moves to the end).
    Input,
    /// The entity's `Dropdown.selected` or `TabBar.active` (an index past the end is ignored).
    Selected,
    /// Shown when `true`; `false` sets `display: none` and `true` restores the authored display.
    Visible,
    /// Enabled when `true`; `false` adds the `Disabled` marker.
    Enabled,
}

impl BindTarget {
    /// Whether the target takes a boolean (`invert` applies).
    pub fn is_bool(self) -> bool {
        matches!(self, Self::Checked | Self::Visible | Self::Enabled)
    }

    /// Whether a widget edit can flow back from this target (two-way bindings).
    pub fn is_editable(self) -> bool {
        matches!(self, Self::Checked | Self::Value | Self::Input | Self::Selected)
    }

    /// The `.ui.ron` spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Progress => "progress",
            Self::Opacity => "opacity",
            Self::Checked => "checked",
            Self::Value => "value",
            Self::Input => "input",
            Self::Selected => "selected",
            Self::Visible => "visible",
            Self::Enabled => "enabled",
        }
    }
}

/// Which way a binding flows (ADR-1811).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindMode {
    /// Model → widget whenever the formatted value changes (the Tier 1 behavior).
    #[default]
    OneWay,
    /// As `OneWay`, plus widget edits write the model field back (registered view models only).
    TwoWay,
    /// Model → widget once, when the source first resolves.
    OneTime,
}

impl BindMode {
    /// The `.ui.ron` spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::OneWay => "one_way",
            Self::TwoWay => "two_way",
            Self::OneTime => "one_time",
        }
    }
}

/// The authored `display` of a node a `Visible` binding hid, restored when it shows again.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct BindHidden(pub Display);

/// One binding: source path, target property and an optional format.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Bind {
    /// Source path.
    pub source: BindSource,
    /// Target property.
    pub target: BindTarget,
    /// Format with one positional argument, e.g. `"{:03}"` (zero-padded width 3) or `"{:.1}"`
    /// (one decimal); literal text around the placeholder is kept (`"Score: {:03}"`). `None` shows
    /// the reflected value's `Debug` form (strings and chars bare).
    pub format: Option<String>,
    /// Direction (ADR-1811).
    pub mode: BindMode,
    /// Negate a boolean before a `Checked`, `Visible` or `Enabled` target (and before a two-way
    /// write of one).
    pub invert: bool,
    /// Transforms the value before `format` and `invert` (ADR-2489).
    pub converter: Option<BindConverter>,
    /// Last formatted value (change detection).
    pub(crate) last: Option<String>,
    /// Last raw field text, before format and converter. Skips `display` when a changed source
    /// still reflects the same text. `None` until a value resolves.
    pub(crate) last_raw: Option<String>,
    /// [`BindSource::Animation`]: seconds on the binding's clock. [`BindSource::Theme`]: unused.
    pub(crate) clock: f32,
    /// [`BindSource::Theme`]: the theme generation the value was read at (`None` before).
    pub(crate) theme_generation: Option<u64>,
    /// Why the source did not resolve or a write-back failed (`None` when it did).
    pub(crate) error: Option<String>,
}

impl Bind {
    /// A resource-field → text binding with a format.
    pub fn text(source: &str, format: Option<&str>) -> Self {
        Self {
            source: BindSource::Resource(source.to_owned()),
            target: BindTarget::Text,
            format: format.map(str::to_owned),
            mode: BindMode::OneWay,
            invert: false,
            converter: None,
            last: None,
            last_raw: None,
            clock: 0.0,
            theme_generation: None,
            error: None,
        }
    }

    /// A theme-token → text binding (ADR-7751): `"color.accent"` of the active theme.
    pub fn theme(token: &str, format: Option<&str>) -> Self {
        let mut bind = Self::text(token, format);
        bind.source = BindSource::Theme(token.to_owned());
        bind
    }

    /// An animation → text binding (ADR-7751); set `target` for opacity, progress or value.
    pub fn animation(animation: BindAnimation, format: Option<&str>) -> Self {
        let mut bind = Self::text("", format);
        bind.source = BindSource::Animation(animation);
        bind
    }

    /// Seconds on an animation binding's clock (0 for other sources).
    pub fn clock(&self) -> f32 {
        self.clock
    }

    /// A script-field → text binding with a format: `"Hud.score"` reads the field `score` of the
    /// script `Hud` on this entity or its nearest ancestor that has one (ADR-7070).
    pub fn script_field(source: &str, format: Option<&str>) -> Self {
        let mut bind = Self::text(source, format);
        bind.source = BindSource::ScriptField(source.to_owned());
        bind
    }

    /// A binding to a field of the collection item (`".name"`, or `"."` for the item itself).
    /// Until a repeat node claims it, it reports `binding_item_outside_repeat`.
    pub fn item(source: &str, format: Option<&str>) -> Self {
        let mut bind = Self::text(source, format);
        bind.source = BindSource::Item(source.to_owned());
        bind.error = Some(format!(
            "{ITEM_OUTSIDE_REPEAT}: source {source:?} reads a collection item, but this node is not in a repeat template"
        ));
        bind
    }

    /// The source path (`"Model.field"`, `".field"`), empty when disabled.
    pub fn path(&self) -> &str {
        match &self.source {
            BindSource::None | BindSource::Animation(_) => "",
            BindSource::Resource(s)
            | BindSource::Component(s)
            | BindSource::Item(s)
            | BindSource::ScriptField(s)
            | BindSource::Theme(s) => s,
        }
    }

    /// The text this binding shows for the field text `raw`: converter, then format, then invert.
    pub(crate) fn display(&self, raw: &str) -> String {
        #[cfg(test)]
        DISPLAY_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
        display_value(
            raw,
            self.converter.as_ref(),
            self.format.as_deref(),
            self.invert && self.target.is_bool(),
        )
    }

    /// Whether the pushed value is a localization key for a text span (`loc_key` on `text`).
    fn pushes_loc_key(&self) -> bool {
        self.target == BindTarget::Text && matches!(self.converter, Some(BindConverter::LocKey { .. }))
    }

    /// What the field text `raw` changes on this binding: `None` when the shown value stays.
    pub(crate) fn change_for(&self, entity: Entity, slot: Option<usize>, raw: &str) -> Option<BindingChange> {
        let shown = self.display(raw);
        let change = |push, last, error| BindingChange {
            entity,
            slot,
            target: self.target,
            loc_key: self.pushes_loc_key(),
            push,
            last,
            last_raw: Some(raw.to_owned()),
            error,
            clock: None,
            theme_generation: None,
        };
        if self.last.as_deref() == Some(shown.as_str()) {
            // Unchanged: clear a stale resolve error; a two-way write error stays until the next
            // successful write or a new model value. Record the raw text either way.
            let stale = self.error.as_deref().is_some_and(|e| e.starts_with(UNRESOLVED));
            let raw_changed = self.last_raw.as_deref() != Some(raw);
            if !stale && !raw_changed {
                return None;
            }
            let error = if stale { None } else { self.error.clone() };
            return Some(change(false, self.last.clone(), error));
        }
        Some(change(true, Some(shown), None))
    }

    /// The last value pushed to the target (`None` before the first push).
    pub fn last_value(&self) -> Option<&str> {
        self.last.as_deref()
    }

    /// The last resolve or write-back error.
    pub fn last_error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// Whether `source` is relative to a component instance's `bind_root` (`"$"`, `"$.field"`; spec 111
/// § 3).
pub fn is_prefixed(source: &str) -> bool {
    source.starts_with('$')
}

/// The prefix hook of UI components (spec 111 § 3, ADR-3147): `source` (`"$"` or `"$.a.b"`)
/// resolved against an instance's `prefix` (`"Party.p1"`, or item-relative `"."` / `".stats"`).
/// `None` when `source` is not a prefixed path. Resolution happens once, when a document is
/// expanded, so the binding pass sees plain paths and costs what the inline form costs.
pub fn prefixed_source(prefix: &str, source: &str) -> Option<String> {
    let rest = source.strip_prefix('$')?;
    if !(rest.is_empty() || rest.starts_with('.')) {
        return None;
    }
    if prefix == "." {
        return Some(if rest.is_empty() {
            ".".to_owned()
        } else {
            rest.to_owned()
        });
    }
    Some(format!("{prefix}{rest}"))
}

/// The text a binding shows for the field text `raw`: `converter`, then `format`, then `invert`
/// (a boolean target's negation). The designer's preview runs the same steps on sample values.
pub fn display_value(
    raw: &str,
    converter: Option<&BindConverter>,
    format: Option<&str>,
    invert: bool,
) -> String {
    let converted = converter.map(|c| c.apply(raw));
    let value = format_value(converted.as_deref().unwrap_or(raw), format);
    if invert { invert_bool(&value) } else { value }
}

/// Largest width or precision a binding format honors. `format!` panics on a runtime width or
/// precision above `u16::MAX`, and a `.ui.ron` file must not be able to crash the UI.
const MAX_FORMAT_FIELD: usize = 256;

/// Formats `value` (the reflected value's debug-ish string) with `spec`: supports `{:03}`,
/// `{:.1}` and `{}`, with literal text around the placeholder. Returns the formatted string. A width or precision above 256 counts as 256.
/// Whole numbers keep every digit (a `u64` above 2^53 is not rounded through a float).
pub fn format_value(raw: &str, spec: Option<&str>) -> String {
    let Some(spec) = spec else {
        return raw.to_owned();
    };
    // A literal prefix and suffix around one `{...}` placeholder ("Score: {:03} pts") are kept
    // verbatim; the placeholder alone formats the value.
    if let Some(open) = spec.find('{')
        && let Some(close) = spec[open..].find('}').map(|i| open + i)
        && (open > 0 || close + 1 < spec.len())
    {
        let body = format_value(raw, Some(&spec[open..=close]));
        return format!("{}{body}{}", &spec[..open], &spec[close + 1..]);
    }
    // Accept "{...}" wrappers.
    let inner = spec
        .trim_start_matches('{')
        .trim_end_matches('}')
        .trim_start_matches(':');
    let field = |text: &str| text.parse::<usize>().unwrap_or(0).min(MAX_FORMAT_FIELD);
    let trimmed = raw.trim();
    if let Some(dot) = inner.find('.') {
        let precision = field(&inner[dot + 1..]);
        return match trimmed.parse::<f64>() {
            Ok(n) => format!("{n:.precision$}"),
            Err(_) => raw.to_owned(),
        };
    }
    if let Ok(int) = trimmed.parse::<i128>() {
        return if inner.starts_with('0') {
            let width = field(inner);
            format!("{int:0width$}")
        } else if let Ok(width) = inner.parse::<usize>() {
            let width = width.min(MAX_FORMAT_FIELD);
            format!("{int:>width$}")
        } else {
            int.to_string()
        };
    }
    let Ok(n) = trimmed.parse::<f64>() else {
        return raw.to_owned();
    };
    if inner.starts_with('0') {
        let width = field(inner);
        format_zero_padded(raw, width)
    } else if let Ok(width) = inner.parse::<usize>() {
        let width = width.min(MAX_FORMAT_FIELD);
        format!("{n:>width$}")
    } else {
        format!("{n}")
    }
}

/// Formats with zero padding: `"{:03}"` on `7` → `"007"`. The width includes the sign and the
/// decimals (`-7` at 3 → `-07`, `7.5` at 5 → `007.5`); text that is not a number is returned as is.
pub fn format_zero_padded(raw: &str, width: usize) -> String {
    let width = width.min(MAX_FORMAT_FIELD);
    let trimmed = raw.trim();
    if let Ok(int) = trimmed.parse::<i128>() {
        return format!("{int:0width$}");
    }
    match trimmed.parse::<f64>() {
        Ok(n) => format!("{n:0width$}"),
        Err(_) => raw.to_owned(),
    }
}

/// Resolves `field.path` against a reflected value. A number indexes a tuple struct, a list or an
/// array (`items.0.name`).
pub fn resolve_path<'a>(value: &'a dyn PartialReflect, path: &str) -> Result<&'a dyn PartialReflect, String> {
    let mut current = value;
    for segment in path.split('.') {
        let found = match current.reflect_ref() {
            ReflectRef::Struct(s) => s.field(segment),
            ReflectRef::TupleStruct(s) => segment.parse::<usize>().ok().and_then(|i| s.field(i)),
            ReflectRef::Enum(e) => e.field(segment),
            ReflectRef::List(l) => segment.parse::<usize>().ok().and_then(|i| l.get(i)),
            ReflectRef::Array(a) => segment.parse::<usize>().ok().and_then(|i| a.get(i)),
            _ => None,
        };
        match found {
            Some(next) => current = next,
            None => {
                return Err(format!("no field {segment:?} on {}", current.reflect_type_path()));
            }
        }
    }
    Ok(current)
}

/// Formats a reflected value for display.
pub(crate) fn reflect_to_string(value: &dyn PartialReflect) -> String {
    #[cfg(test)]
    REFLECT_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
    // Strings Debug-print with quotes; display them bare. Everything else: Debug (numbers,
    // bools, char, small structs) reads exactly like users expect.
    let type_path = value.reflect_type_path();
    if type_path.ends_with("String") || type_path.ends_with("&str") || type_path == "char" {
        let debug = format!("{value:?}");
        debug.trim_matches('"').to_owned()
    } else {
        format!("{value:?}")
    }
}

/// Per-frame binding refresh: an exclusive system (needs `&World` for resources plus writes).
/// Refreshes the single [`Bind`] components and every entry of
/// [`crate::viewmodel::ViewBindings`].
pub fn update_bindings(world: &mut World) {
    let _span = tracing::trace_span!("ui_bindings").entered();
    let registry = world
        .get_resource::<regia_ecs::prelude::AppTypeRegistry>()
        .map(|r| r.0.clone());
    let Some(registry) = registry else {
        return;
    };
    let registry = registry.read();
    // Collection items first: a new row's resource bindings then refresh this same frame.
    if world.contains_resource::<crate::repeat::RepeatHosts>() {
        crate::repeat::update_repeats(world, &registry);
    }
    // Read phase: borrows every binding, so a frame where nothing changed clones none of them.
    let mut changes = Vec::new();
    let mut singles = world.query::<(Entity, &Bind)>();
    let mut lists = world.query::<(Entity, &crate::viewmodel::ViewBindings)>();
    let read: &World = world;
    let frame = BindFrame::of(read);
    for (entity, bind) in singles.iter(read) {
        changes.extend(refresh(read, &registry, entity, None, bind, frame));
    }
    for (entity, list) in lists.iter(read) {
        for (i, bind) in list.0.iter().enumerate() {
            changes.extend(refresh(read, &registry, entity, Some(i), bind, frame));
        }
    }
    // Write phase: entity order within each query, the single `Bind`s first.
    write_changes(world, changes);
}

/// Applies binding changes in order: pushes the values to their targets and records them.
pub(crate) fn write_changes(world: &mut World, changes: Vec<BindingChange>) {
    for change in changes {
        if let (true, Some(value)) = (change.push, &change.last) {
            if change.loc_key {
                apply_loc_key(world, change.entity, value);
            } else {
                apply_target(world, change.entity, change.target, value);
            }
        }
        let record = |b: &mut Bind| {
            (b.last, b.last_raw, b.error) = (change.last, change.last_raw, change.error);
            if let Some(clock) = change.clock {
                b.clock = clock;
            }
            if change.theme_generation.is_some() {
                b.theme_generation = change.theme_generation;
            }
        };
        match change.slot {
            None => {
                if let Some(mut b) = world.get_mut::<Bind>(change.entity) {
                    record(&mut b);
                }
            }
            Some(i) => {
                if let Some(mut list) = world.get_mut::<crate::viewmodel::ViewBindings>(change.entity)
                    && let Some(b) = list.0.get_mut(i)
                {
                    record(b);
                }
            }
        }
    }
}

/// What one refresh changes on a binding.
pub(crate) struct BindingChange {
    entity: Entity,
    /// The `ViewBindings` slot; `None` for the single `Bind`.
    slot: Option<usize>,
    target: BindTarget,
    /// The value is a localization key for the text span (`loc_key` converter).
    loc_key: bool,
    /// `last` is a new value to push to the target.
    push: bool,
    last: Option<String>,
    /// Raw field text to remember. Kept across an error so the previous raw is not forgotten.
    last_raw: Option<String>,
    error: Option<String>,
    /// A new animation clock or theme generation to record (`None` keeps the binding's).
    clock: Option<f32>,
    theme_generation: Option<u64>,
}

impl BindingChange {
    /// The binding did not resolve: keeps nothing, records `reason`.
    pub(crate) fn error(entity: Entity, slot: Option<usize>, bind: &Bind, reason: String) -> Self {
        Self {
            entity,
            slot,
            target: bind.target,
            loc_key: false,
            push: false,
            last: None,
            last_raw: bind.last_raw.clone(),
            error: Some(reason),
            clock: None,
            theme_generation: None,
        }
    }
}

/// One binding's refresh: resolves and formats. Returns what changed, `None` when nothing did.
fn refresh(
    world: &World,
    registry: &TypeRegistry,
    entity: Entity,
    slot: Option<usize>,
    bind: &Bind,
    frame: BindFrame,
) -> Option<BindingChange> {
    if matches!(bind.source, BindSource::Theme(_) | BindSource::Animation(_)) {
        return refresh_dynamic(world, entity, slot, bind, frame);
    }
    let (container, path) = bind.source.split()?;
    if bind.mode == BindMode::OneTime && bind.last.is_some() {
        return None;
    }
    if bind.last.is_some() && bind.error.is_none() && source_unchanged(world, registry, entity, bind) {
        return None;
    }
    let resolved = match &bind.source {
        BindSource::Resource(_) => resolve_resource(world, registry, container, path),
        BindSource::Component(_) => resolve_component(world, registry, entity, container, path),
        BindSource::ScriptField(_) => resolve_script_field(world, entity, container, path),
        BindSource::None | BindSource::Item(_) | BindSource::Theme(_) | BindSource::Animation(_) => {
            return None;
        }
    };
    let raw = match resolved {
        Ok(raw) => raw,
        Err(err) => {
            let reason = format!("{UNRESOLVED}: {err}");
            if bind.last.is_none() && bind.error.as_deref() == Some(reason.as_str()) {
                return None;
            }
            return Some(BindingChange::error(entity, slot, bind, reason));
        }
    };
    if bind.last.is_some() && bind.last_raw.as_deref() == Some(raw.as_str()) {
        let stale = bind.error.as_deref().is_some_and(|e| e.starts_with(UNRESOLVED));
        return stale.then(|| BindingChange {
            entity,
            slot,
            target: bind.target,
            loc_key: false,
            push: false,
            last: bind.last.clone(),
            last_raw: bind.last_raw.clone(),
            error: None,
            clock: None,
            theme_generation: None,
        });
    }
    bind.change_for(entity, slot, &raw)
}

/// What the theme and the clock look like this frame, for theme and animation bindings.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BindFrame {
    /// Seconds since the last frame (`Time`; 0 without a clock).
    pub(crate) dt: f32,
    /// Reduced motion is on (animation bindings push their end value).
    pub(crate) reduced_motion: bool,
    /// The theme resolver's generation (`None` without a resolver).
    pub(crate) theme_generation: Option<u64>,
}

impl BindFrame {
    pub(crate) fn of(world: &World) -> Self {
        Self {
            dt: world.get_resource::<Time>().map_or(0.0, |t| t.delta_secs()),
            reduced_motion: crate::motion::reduced_motion(
                world.get_resource::<crate::theme_resolve::ActiveTheme>(),
                world.get_resource::<crate::scale::UiAccessibility>(),
            ),
            theme_generation: world
                .get_resource::<crate::theme_resolve::ThemeResolver>()
                .map(crate::theme_resolve::ThemeResolver::generation),
        }
    }
}

/// The raw text of theme token `token` of the active theme.
fn theme_token_text(world: &World, token: &str) -> Result<String, String> {
    let theme = world
        .get_resource::<crate::theme_resolve::ThemeResolver>()
        .and_then(crate::theme_resolve::ThemeResolver::resolved_theme)
        .ok_or_else(|| "no resolved theme (is the theme resolver running?)".to_owned())?;
    let value = theme
        .values
        .get(token)
        .ok_or_else(|| format!("theme {:?} has no token {token:?}", theme.name))?;
    Ok(match value {
        crate::theme_tokens::Value::Color(c) => crate::theme_color_ops::to_hex(*c),
        crate::theme_tokens::Value::Number(n) | crate::theme_tokens::Value::Duration(n) => format!("{n}"),
        crate::theme_tokens::Value::Text(t) => t.clone(),
        crate::theme_tokens::Value::Easing(e) => format!("{e:?}"),
    })
}

/// A theme or animation binding's refresh (ADR-7751). `None` when nothing changed.
fn refresh_dynamic(
    world: &World,
    entity: Entity,
    slot: Option<usize>,
    bind: &Bind,
    frame: BindFrame,
) -> Option<BindingChange> {
    if bind.mode == BindMode::OneTime && bind.last.is_some() {
        return None;
    }
    match &bind.source {
        BindSource::Theme(token) => {
            if bind.last.is_some() && bind.error.is_none() && bind.theme_generation == frame.theme_generation
            {
                return None;
            }
            match theme_token_text(world, token) {
                Ok(raw) => {
                    let mut change = bind
                        .change_for(entity, slot, &raw)
                        .unwrap_or_else(|| BindingChange {
                            entity,
                            slot,
                            target: bind.target,
                            loc_key: false,
                            push: false,
                            last: bind.last.clone(),
                            last_raw: bind.last_raw.clone(),
                            error: None,
                            clock: None,
                            theme_generation: None,
                        });
                    change.theme_generation = frame.theme_generation;
                    Some(change)
                }
                Err(err) => {
                    let reason = format!("{THEME_TOKEN}: {err}");
                    if bind.error.as_deref() == Some(reason.as_str())
                        && bind.theme_generation == frame.theme_generation
                    {
                        return None;
                    }
                    let mut change = BindingChange::error(entity, slot, bind, reason);
                    change.theme_generation = frame.theme_generation;
                    Some(change)
                }
            }
        }
        BindSource::Animation(animation) => {
            let started = bind.last.is_some();
            if started && !frame.reduced_motion && !animation.running_at(bind.clock) {
                return None;
            }
            let clock = if started {
                bind.clock + frame.dt.max(0.0)
            } else {
                bind.clock
            };
            let value = if frame.reduced_motion {
                animation.sample(f32::INFINITY)
            } else {
                animation.sample(clock)
            };
            let raw = format!("{value}");
            let mut change = bind
                .change_for(entity, slot, &raw)
                .unwrap_or_else(|| BindingChange {
                    entity,
                    slot,
                    target: bind.target,
                    loc_key: false,
                    push: false,
                    last: bind.last.clone(),
                    last_raw: bind.last_raw.clone(),
                    error: bind.error.clone(),
                    clock: None,
                    theme_generation: None,
                });
            change.clock = Some(clock);
            Some(change)
        }
        _ => None,
    }
}

/// The source has not changed since this exclusive system last ran.
///
/// The binding's own change tick is ignored: this system writes `last` and `last_raw`, so that
/// tick would stay set and defeat the skip.
fn source_unchanged(world: &World, registry: &TypeRegistry, entity: Entity, bind: &Bind) -> bool {
    let Some((container, _)) = bind.source.split() else {
        return false;
    };
    match &bind.source {
        BindSource::Resource(_) => {
            for registration in registrations_named(registry, container) {
                let Some(id) = world.components().get_valid_id(registration.type_id()) else {
                    continue;
                };
                if world.get_resource_change_ticks_by_id(id).is_some() {
                    return !world.is_resource_changed_by_id(id);
                }
            }
            false
        }
        BindSource::Component(_) => {
            let Ok(entity_ref) = world.get_entity(entity) else {
                return false;
            };
            for registration in registrations_named(registry, container) {
                let Some(id) = world.components().get_valid_id(registration.type_id()) else {
                    continue;
                };
                if let Some(ticks) = entity_ref.get_change_ticks_by_id(id) {
                    return !ticks.is_changed(world.last_change_tick(), world.read_change_tick());
                }
            }
            false
        }
        // Scripts change through take-run-reinsert every frame, so there is no tick to trust:
        // the field is read each frame and only a changed text is pushed.
        BindSource::None
        | BindSource::Item(_)
        | BindSource::ScriptField(_)
        | BindSource::Theme(_)
        | BindSource::Animation(_) => false,
    }
}

/// Resolves a [`BindSource::ScriptField`] through the installed [`ScriptFieldReader`].
fn resolve_script_field(
    world: &World,
    entity: Entity,
    container: &str,
    path: &str,
) -> Result<String, UiError> {
    let failed = |reason: String| UiError::Binding {
        path: format!("{container}.{path}"),
        reason,
    };
    let Some(reader) = world.get_resource::<ScriptFieldReader>() else {
        return Err(failed(
            "no script field reader (add regia_framework's ScriptPlugin)".to_owned(),
        ));
    };
    (reader.0)(world, entity, container, path)
        .map(reflect_to_string)
        .map_err(failed)
}

/// The registrations with the short name `short`: a hash lookup when the name is unique, the full
/// scan only when several types share it. Runs per bound field per frame, and a game's registry
/// holds thousands of types.
pub(crate) fn registrations_named<'a>(
    registry: &'a TypeRegistry,
    short: &'a str,
) -> impl Iterator<Item = &'a TypeRegistration> {
    let unique = registry.get_with_short_type_path(short);
    let shared = (unique.is_none() && registry.is_ambiguous(short)).then(|| {
        registry
            .iter()
            .filter(move |r| r.type_info().type_path_table().short_path() == short)
    });
    unique.into_iter().chain(shared.into_iter().flatten())
}

/// The status code of a binding whose source does not resolve.
pub(crate) const UNRESOLVED: &str = "binding_unresolved";
/// A theme binding names a token the active theme does not have (ADR-7751).
pub(crate) const THEME_TOKEN: &str = "binding_theme_token";

/// The status code of an item binding on a node that no repeat node generated.
pub(crate) const ITEM_OUTSIDE_REPEAT: &str = "binding_item_outside_repeat";

/// `"true"` ↔ `"false"`; anything else passes through.
pub(crate) fn invert_bool(v: &str) -> String {
    match v.trim() {
        "true" => "false".to_owned(),
        "false" => "true".to_owned(),
        other => other.to_owned(),
    }
}

pub(crate) fn resolve_resource(
    world: &World,
    registry: &TypeRegistry,
    container: &str,
    path: &str,
) -> Result<String, UiError> {
    resource_field(world, registry, container, path).map(reflect_to_string)
}

/// The field at `path` of the resource with the short name `container`.
pub(crate) fn resource_field<'w>(
    world: &'w World,
    registry: &TypeRegistry,
    container: &str,
    path: &str,
) -> Result<&'w dyn PartialReflect, UiError> {
    // Error strings are built only on failure: this runs per bound field per frame.
    let mut registered_absent = false;
    for registration in registrations_named(registry, container) {
        let type_id = registration.type_id();
        let Some(component_id) = world.components().get_valid_id(type_id) else {
            continue;
        };
        let Some(entity) = world.resource_entities().get(component_id) else {
            registered_absent = true;
            continue;
        };
        // A removed resource can keep its entity (bevy_ecs 0.20) without the value.
        let Ok(value) = world.get_reflect(entity, type_id) else {
            registered_absent = true;
            continue;
        };
        return resolve_path(value.as_partial_reflect(), path).map_err(|reason| UiError::Binding {
            path: format!("{container}.{path}"),
            reason,
        });
    }
    let reason = if registered_absent {
        format!("resource {container:?} is registered but not present in the world")
    } else {
        format!("no registered resource named {container:?}")
    };
    Err(UiError::Binding {
        path: format!("{container}.{path}"),
        reason,
    })
}

fn resolve_component(
    world: &World,
    registry: &TypeRegistry,
    entity: Entity,
    container: &str,
    path: &str,
) -> Result<String, UiError> {
    let entity_ref = world.get_entity(entity).map_err(|_| UiError::Binding {
        path: format!("{container}.{path}"),
        reason: format!("entity {entity:?} is gone"),
    })?;
    for registration in registrations_named(registry, container) {
        let Some(reflect_component) = registration.data::<regia_ecs::reflect::ReflectComponent>() else {
            continue;
        };
        if let Some(value) = reflect_component.reflect(entity_ref) {
            let field =
                resolve_path(value.as_partial_reflect(), path).map_err(|reason| UiError::Binding {
                    path: format!("{container}.{path}"),
                    reason,
                })?;
            return Ok(reflect_to_string(field));
        }
    }
    let _ = TypeId::of::<()>();
    Err(UiError::Binding {
        path: format!("{container}.{path}"),
        reason: format!("no component {container:?} on entity {entity:?}"),
    })
}

/// Puts a localization key in the node's first text span (the `loc_key` converter), so it resolves
/// through the Fluent path and again on a language switch.
fn apply_loc_key(world: &mut World, entity: Entity, key: &str) {
    if let Some(mut text) = world.get_mut::<crate::text::Text>(entity) {
        let content = crate::text::TextContent::Loc {
            key: key.to_owned(),
            args: Vec::new(),
        };
        match text.spans.first_mut() {
            Some(span) => span.content = content,
            None => text.spans.push(TextSpan::loc(key.to_owned())),
        }
    }
}

fn apply_target(world: &mut World, entity: Entity, target: BindTarget, value: &str) {
    match target {
        BindTarget::Text => {
            if let Some(mut text) = world.get_mut::<crate::text::Text>(entity) {
                if text.spans.is_empty() {
                    text.spans.push(TextSpan::lit(value.to_owned()));
                } else {
                    // Keep the span style; replace the content of literal spans.
                    let style = text.spans[0].style.clone();
                    text.spans[0] = TextSpan {
                        content: crate::text::TextContent::Lit(value.to_owned()),
                        style,
                    };
                }
            }
        }
        BindTarget::Progress => {
            if let Ok(v) = value.trim().parse::<f32>()
                && let Some(mut bar) = world.get_mut::<ProgressBar>(entity)
            {
                bar.value = v.clamp(0.0, 1.0);
            }
        }
        BindTarget::Opacity => {
            if let Ok(v) = value.trim().parse::<f32>()
                && let Some(mut opacity) = world.get_mut::<crate::node::Opacity>(entity)
            {
                opacity.0 = v.clamp(0.0, 1.0);
            }
        }
        BindTarget::Checked => {
            if let Some(mut toggle) = world.get_mut::<crate::widgets::Toggle>(entity) {
                toggle.checked = value.trim() == "true";
            }
        }
        BindTarget::Value => {
            if let Ok(v) = value.trim().parse::<f32>()
                && let Some(mut slider) = world.get_mut::<crate::widgets::Slider>(entity)
            {
                // `f32::clamp` aborts on NaN bounds; authored slider ends may be non-finite.
                if slider.min.is_finite() && slider.max.is_finite() {
                    let (lo, hi) = (slider.min.min(slider.max), slider.min.max(slider.max));
                    slider.value = v.clamp(lo, hi);
                } else {
                    slider.value = 0.0;
                }
            }
        }
        BindTarget::Input => {
            if let Some(mut input) = world.get_mut::<crate::text_input::TextInput>(entity)
                && let Some(clean) = input.external_text(value)
                && input.text != clean
            {
                input.set_text(&clean);
            }
        }
        BindTarget::Selected => {
            let Ok(index) = value.trim().parse::<usize>() else {
                return;
            };
            if let Some(mut dropdown) = world.get_mut::<crate::widgets::Dropdown>(entity) {
                if index < dropdown.options.len() {
                    dropdown.selected = Some(index);
                }
            } else if let Some(mut tabs) = world.get_mut::<crate::widgets::TabBar>(entity)
                && index < tabs.tabs.len()
            {
                tabs.active = index;
            }
        }
        BindTarget::Visible => {
            let show = value.trim() == "true";
            let hidden = world.get::<BindHidden>(entity).copied();
            let Some(display) = world.get::<crate::node::NodeStyle>(entity).map(|s| s.0.display) else {
                return;
            };
            let next = match (show, hidden) {
                (false, None) => {
                    world.entity_mut(entity).insert(BindHidden(display));
                    Display::None
                }
                (true, Some(BindHidden(authored))) => {
                    world.entity_mut(entity).remove::<BindHidden>();
                    authored
                }
                _ => return,
            };
            if let Some(mut style) = world.get_mut::<crate::node::NodeStyle>(entity) {
                style.0.display = next;
            }
        }
        BindTarget::Enabled => {
            let enabled = value.trim() == "true";
            let disabled = world.get::<crate::node::Disabled>(entity).is_some();
            if enabled && disabled {
                world.entity_mut(entity).remove::<crate::node::Disabled>();
            } else if !enabled && !disabled {
                world.entity_mut(entity).insert(crate::node::Disabled);
            }
        }
    }
}

#[cfg(test)]
mod animation_tests {
    use super::{AnimationRepeat, BindAnimation};
    use crate::tween::Ease;

    fn anim(repeat: AnimationRepeat) -> BindAnimation {
        BindAnimation {
            from: 1.0,
            to: 3.0,
            duration: 2.0,
            delay: 1.0,
            ease: Ease::Linear,
            repeat,
        }
    }

    /// ADR-7751: the sample is a pure function of the clock: the delay holds `from`, `once`
    /// holds `to`, `loop` restarts, `ping_pong` comes back, an ease shapes each pass, and
    /// non-finite input never panics.
    #[test]
    fn animation_samples_follow_delay_repeat_and_ease() {
        let once = anim(AnimationRepeat::Once);
        assert_eq!(once.sample(0.5), 1.0);
        assert_eq!(once.sample(2.0), 2.0);
        assert_eq!(once.sample(10.0), 3.0);
        assert!(once.running_at(2.9) && !once.running_at(3.0));
        let looped = anim(AnimationRepeat::Loop);
        assert_eq!(looped.sample(4.0), 2.0, "1.5 passes: half way");
        assert!(looped.running_at(1e6));
        let pong = anim(AnimationRepeat::PingPong);
        assert_eq!(pong.sample(4.0), 2.0);
        assert_eq!(pong.sample(4.5), 1.5, "1.75 passes: on the way back");
        let eased = BindAnimation {
            ease: Ease::QuadIn,
            ..anim(AnimationRepeat::Once)
        };
        assert_eq!(eased.sample(2.0), 1.5, "quad_in at half way = a quarter");
        for t in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX] {
            for repeat in AnimationRepeat::ALL {
                assert!(anim(repeat).sample(t).is_finite());
            }
        }
        let broken = BindAnimation {
            from: f32::NAN,
            duration: 0.0,
            delay: f32::INFINITY,
            ..anim(AnimationRepeat::Loop)
        };
        assert!(broken.sample(1.0).is_finite());
        assert_eq!(
            BindAnimation {
                duration: -1.0,
                ..once
            }
            .sample(1.5),
            3.0,
            "no duration jumps to `to`"
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn input_binding_applies_single_line_and_max_chars_limits() {
        let mut world = World::new();
        let e = world
            .spawn(crate::text_input::TextInput {
                max_chars: 4,
                ..Default::default()
            })
            .id();
        apply_target(&mut world, e, BindTarget::Input, "a\nbcdef");
        let t = world.get::<crate::text_input::TextInput>(e).unwrap();
        assert_eq!(t.text, "a bc");
    }

    #[test]
    fn format_keeps_literal_prefix_and_suffix() {
        assert_eq!(format_value("7", Some("Score: {:03}")), "Score: 007");
        assert_eq!(format_value("7.25", Some("{:.1} pts")), "7.2 pts");
        assert_eq!(format_value("7", Some("[{}]")), "[7]");
        assert_eq!(format_value("7", Some("{:03}")), "007");
    }

    use super::*;

    #[test]
    fn value_target_with_non_finite_slider_ends_does_not_abort() {
        use crate::widgets::Slider;
        let mut world = World::new();
        let e = world
            .spawn(Slider {
                value: 0.5,
                min: f32::NAN,
                max: f32::NAN,
                step: 0.0,
            })
            .id();
        apply_target(&mut world, e, BindTarget::Value, "1");
        assert_eq!(world.get::<Slider>(e).map(|s| s.value), Some(0.0));
        let ok = world
            .spawn(Slider {
                value: 0.0,
                min: 10.0,
                max: 0.0,
                step: 0.0,
            })
            .id();
        apply_target(&mut world, ok, BindTarget::Value, "50");
        assert_eq!(world.get::<Slider>(ok).map(|s| s.value), Some(10.0));
    }

    #[test]
    fn formats_precision_and_width() {
        assert_eq!(format_value("3.14159", Some("{:.1}")), "3.1");
        assert_eq!(format_value("7", Some("{:3}")), "  7");
        assert_eq!(format_value("7", None), "7");
        assert_eq!(format_value("text", Some("{:.1}")), "text");
    }

    #[test]
    fn zero_padding() {
        assert_eq!(format_zero_padded("7", 3), "007");
        assert_eq!(format_zero_padded("-7", 3), "-07");
        assert_eq!(format_zero_padded("1234", 3), "1234");
        assert_eq!(format_zero_padded("abc", 3), "abc");
    }

    #[derive(Resource, Reflect, Debug, Default)]
    #[reflect(Resource, Default)]
    struct Score {
        value: u32,
        label: String,
    }

    #[test]
    fn resource_binding_updates_text() {
        use crate::text::Text;
        let mut app = App::new();
        app.init_resource::<regia_ecs::prelude::AppTypeRegistry>();
        app.register_type::<Score>();
        app.insert_resource(Score {
            value: 7,
            label: "score".into(),
        });
        app.init_resource::<crate::layout::LayoutDirty>();
        app.add_systems(regia_app::PostUpdate, update_bindings);
        let entity = app
            .world_mut()
            .spawn((Text::default(), Bind::text("Score.value", Some("{:03}"))))
            .id();
        app.update();
        assert_eq!(
            app.world().get::<Text>(entity).unwrap().spans[0].content,
            crate::text::TextContent::Lit("007".into())
        );
        // Change the resource; text updates; change detection skips identical values.
        app.world_mut().resource_mut::<Score>().value = 42;
        app.update();
        assert_eq!(
            app.world().get::<Text>(entity).unwrap().spans[0].content,
            crate::text::TextContent::Lit("042".into())
        );
    }

    #[test]
    fn stable_binding_formats_once() {
        use crate::text::Text;
        let mut app = App::new();
        app.init_resource::<regia_ecs::prelude::AppTypeRegistry>();
        app.register_type::<Score>();
        app.insert_resource(Score {
            value: 7,
            label: "score".into(),
        });
        app.init_resource::<crate::layout::LayoutDirty>();
        app.add_systems(regia_app::PostUpdate, update_bindings);
        let entity = app
            .world_mut()
            .spawn((Text::default(), Bind::text("Score.value", Some("{:03}"))))
            .id();
        REFLECT_CALLS.with(|calls| calls.set(0));
        DISPLAY_CALLS.with(|calls| calls.set(0));
        app.update();
        let reflect = REFLECT_CALLS.with(|calls| calls.get());
        let display = DISPLAY_CALLS.with(|calls| calls.get());
        assert!(reflect >= 1, "the first frame formats the field ({reflect})");
        assert!(display >= 1, "the first frame runs display ({display})");
        app.update();
        assert_eq!(
            REFLECT_CALLS.with(|calls| calls.get()),
            reflect,
            "an unchanged source is not reflected again ({reflect} calls)"
        );
        assert_eq!(DISPLAY_CALLS.with(|calls| calls.get()), display);
        app.world_mut().resource_mut::<Score>().value = 42;
        app.update();
        assert!(REFLECT_CALLS.with(|calls| calls.get()) > reflect);
        assert_eq!(
            app.world().get::<Text>(entity).unwrap().spans[0].content,
            crate::text::TextContent::Lit("042".into())
        );
        let reflect_same = REFLECT_CALLS.with(|calls| calls.get());
        let display_same = DISPLAY_CALLS.with(|calls| calls.get());
        app.world_mut().resource_mut::<Score>().value = 42;
        app.update();
        assert!(
            REFLECT_CALLS.with(|calls| calls.get()) > reflect_same,
            "a same-value write still reflects, because the source changed"
        );
        assert_eq!(
            DISPLAY_CALLS.with(|calls| calls.get()),
            display_same,
            "the same raw text does not run display"
        );
    }

    #[test]
    fn absent_resource_reports_registered_but_missing() {
        // As in `update_bindings`: the registry is the world's `AppTypeRegistry`, which
        // `World::get_reflect` reads the value through.
        let mut world = World::new();
        let app_registry = regia_ecs::prelude::AppTypeRegistry::default();
        app_registry.0.write().register::<Score>();
        world.insert_resource(app_registry.clone());
        let registry = app_registry.0.read();
        world.insert_resource(Score::default());
        world.remove_resource::<Score>();
        let err = resolve_resource(&world, &registry, "Score", "value").unwrap_err();
        assert!(err.to_string().contains("registered but not present"), "{err}");
        let err = resolve_resource(&world, &registry, "Nope", "value").unwrap_err();
        assert!(err.to_string().contains("no registered resource"), "{err}");
        world.insert_resource(Score::default());
        assert_eq!(
            resolve_resource(&world, &registry, "Score", "value").unwrap(),
            "0"
        );
    }

    #[test]
    fn progress_binding_parses() {
        let mut world = World::new();
        let e = world.spawn((ProgressBar { value: 0.0 },)).id();
        apply_target(&mut world, e, BindTarget::Progress, "0.75");
        assert_eq!(world.get::<ProgressBar>(e).unwrap().value, 0.75);
        apply_target(&mut world, e, BindTarget::Progress, "2.0");
        assert_eq!(world.get::<ProgressBar>(e).unwrap().value, 1.0);
    }
}
