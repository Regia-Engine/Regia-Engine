// SPDX-License-Identifier: MPL-2.0
//! Theme resolution (`docs/specs/ui-default-design.md` §§ 3, 6.3, 6.4; lane D6, ADR-289).
//!
//! [`apply_themes`] turns the active `.theme.ron` theme ([`crate::theme_file`]) and the widget
//! style surface ([`StyleClass`], [`StyleState`]) into each node's [`Themed`] look. For one
//! property of one node the first hit wins:
//!
//! 1. the node's [`ThemeOverride`],
//! 2. the theme's style rules that match the node (`.theme.ron` v2, ADR-8105 (e), ADR-8303),
//!    the most specific first, a later rule before an earlier one of equal specificity,
//! 3. the variation chain of the node's classes, nearest first (`Button.danger` →
//!    `Button.primary`), each layer state-composed in the § 3 order,
//! 4. the widget type (`Button`),
//! 5. the theme defaults (`@color.text`, `@color.border`, `@radius.md`, the disabled rule).
//!
//! A rule composes like one more variation (every state layer of it in the § 3 order), so a
//! rule's normal-state property also covers the type's hover value of that property; a rule
//! that only sets other properties keeps the type's hover and pressed feedback. Game classes
//! built in code ([`UiThemes::classes`]) still layer over the rules.
//!
//! Every `@token` resolves through the theme's `extends` chain. A [`ThemeScope`] on a node
//! resolves its subtree against another theme. Classes pick types and variations through
//! [`class_target`] (`button` → `Button`, `primary` → `Button.primary`); a class may also name a
//! type or variation directly (`"Button.ghost"`).
//!
//! Work is change-driven. A theme change (the [`UiThemes`] set, [`ActiveTheme`], the density)
//! bumps the generation, clears the style cache and re-resolves every themed node once, with a
//! colour cross-fade over `motion.base` unless reduced motion is on. Otherwise only nodes whose
//! inputs changed are resolved, through a cache keyed by (theme, classes, state). An unchanged
//! frame resolves nothing and allocates nothing.
//!
//! The focus ring (lane D9, ADR-372) takes `@color.focus-ring`, `@border.width.focus` and
//! `@border.width.focus-offset` from the theme defaults; [`Themed::focus_visible`] is set on the
//! focused node while [`FocusRingPolicy`] shows the ring in the current [`UiInputMode`]. A mode
//! flip re-resolves only the focused nodes, from the style cache.

use std::collections::HashMap;
use std::hash::BuildHasher;

use regia_app::prelude::*;

use crate::color::UiColor;
use crate::focus::{FocusRingPolicy, UiInputMode};
use crate::interaction::UiPointer;
use crate::node::{
    Background, BackgroundKind, Classes, Disabled, Focused, Hovered, Invalid, Pressed, Selected, UiNode,
};
use crate::scale::UiAccessibility;
use crate::theme::{StyleClass, ThemeClasses, Themed, UiThemes, VisualOverrides};
use crate::theme_file::{
    BUILTIN_THEME_SOURCES, Density, PropMap, PropValue, ResolvedTheme, RuleState, StyleRule, ThemeError,
    ThemeSet, TypeDef, looks_like_expression,
};
use crate::theme_migrate::{class_target, migrate_v0};
use crate::theme_motion as motion;
use crate::theme_runtime::{BlendSpace, ThemeTransition, blend_swap_color, ease_progress};
use crate::theme_tokens::{Easing, Expr, Value};
use crate::widget_style::{StyleState, UiDensity, WidgetSize};

/// The theme selected when nothing else is (owner default: dark, spec § 9 Q2).
pub const DEFAULT_THEME: &str = "regia-dark";

/// The built-in high-contrast variant, used when [`ActiveTheme::high_contrast`] is on and the
/// active theme has no `<name>-high-contrast` sibling.
pub const HIGH_CONTRAST_THEME: &str = "regia-high-contrast";

/// Ancestor levels searched for a [`ThemeScope`].
const MAX_SCOPE_DEPTH: usize = 256;

const CONTROL_HEIGHT: [&str; 3] = ["size.control.sm", "size.control.md", "size.control.lg"];
const PAD_X: [&str; 3] = ["size.pad-x.sm", "size.pad-x.md", "size.pad-x.lg"];

/// The built-in themes (`regia-dark`, `regia-light`, `regia-high-contrast`,
/// `regia-light-high-contrast`), parsed once per process and cloned.
pub fn builtin_theme_set() -> ThemeSet {
    static SET: std::sync::OnceLock<ThemeSet> = std::sync::OnceLock::new();
    SET.get_or_init(|| {
        let set = ThemeSet::with_builtins();
        if set.names().count() != BUILTIN_THEME_SOURCES.len() {
            tracing::error!(target: "regia_ui", "a built-in theme failed to load");
        }
        set
    })
    .clone()
}

/// Selects the theme the UI resolves against (spec § 6.4). Writing it is the whole runtime
/// swap: the next frame re-resolves every themed node once and cross-fades the colours.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct ActiveTheme {
    /// Theme name in [`UiThemes::set`]; an unknown name falls back to [`DEFAULT_THEME`].
    pub name: String,
    /// Density override. `None` follows the [`UiDensity`] resource, then the theme's own
    /// `density`.
    pub density: Option<Density>,
    /// Use the high-contrast variant: `<name>-high-contrast` when loaded, else
    /// [`HIGH_CONTRAST_THEME`].
    pub high_contrast: bool,
    /// Reduced motion: a theme swap applies at once instead of cross-fading. The player's
    /// [`UiAccessibility::reduced_motion`] does the same; either one is enough
    /// ([`crate::motion::reduced_motion`]).
    pub reduced_motion: bool,
}

impl Default for ActiveTheme {
    fn default() -> Self {
        Self::named(DEFAULT_THEME)
    }
}

impl ActiveTheme {
    /// Selects `name` with the default density, contrast and motion.
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            density: None,
            high_contrast: false,
            reduced_motion: false,
        }
    }
}

/// Resolves this node and its descendants against another theme (a light in-world terminal
/// inside a dark HUD). The nearest scope wins; an unknown theme falls back to the active one.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ThemeScope {
    /// Theme name in [`UiThemes::set`].
    pub theme: String,
}

impl ThemeScope {
    /// A scope for `theme`.
    pub fn new(theme: &str) -> Self {
        Self {
            theme: theme.to_owned(),
        }
    }
}

/// Per-node style override: wins over every theme layer (precedence row 1). Its state layers
/// compose like any class (`hover`, `disabled`, ...).
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct ThemeOverride(pub StyleClass);

impl ThemeOverride {
    /// An override of the normal state only.
    pub fn new(base: VisualOverrides) -> Self {
        Self(StyleClass {
            base,
            ..Default::default()
        })
    }
}

/// What the resolver did in the last frame (for tests, profiling and the agent surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThemeStats {
    /// Theme generation: bumped once per theme change.
    pub generation: u64,
    /// Themed nodes resolved in the last frame.
    pub nodes_resolved: u32,
    /// Styles computed in the last frame (style cache misses).
    pub styles_computed: u32,
    /// Nodes with a colour transition or cross-fade still running.
    pub animating: u32,
}

/// One resolved theme the resolver draws from (slot 0 is the active theme).
#[derive(Debug)]
struct ThemeSlot {
    name: String,
    theme: ResolvedTheme,
    defaults: StyleClass,
    accent: Option<UiColor>,
    /// The distinct `inside` types the theme's rules name, sorted: bit `i` of a node's ancestor
    /// mask says some ancestor has `inside[i]`.
    inside: Vec<String>,
    /// Whether any type, variation or rule of the theme has a `focus_visible` layer.
    focus_visible: bool,
}

impl ThemeSlot {
    fn new(
        set: &ThemeSet,
        name: &str,
        density: Option<Density>,
        disabled_opacity: f32,
    ) -> Result<Self, ThemeError> {
        let theme = set.resolve(name, density)?;
        let defaults = defaults_class(&theme, disabled_opacity);
        let accent = theme.color("color.accent");
        let mut inside: Vec<String> = theme.rules.iter().filter_map(|r| r.inside.clone()).collect();
        inside.sort_unstable();
        inside.dedup();
        let focus_visible = theme
            .types
            .values()
            .chain(theme.variations.values())
            .any(|d| !d.focus_visible.is_empty())
            || theme
                .rules
                .iter()
                .any(|r| r.state == Some(RuleState::FocusVisible) && !r.props.is_empty());
        Ok(Self {
            name: name.to_owned(),
            theme,
            defaults,
            accent,
            inside,
            focus_visible,
        })
    }
}

/// A cached style: theme layers composed for one (theme, classes, state).
#[derive(Debug, Clone, Copy, PartialEq)]
struct CachedStyle {
    visuals: VisualOverrides,
    transition: f32,
    ease: Option<[f32; 4]>,
}

#[derive(Debug)]
struct CacheEntry {
    slot: usize,
    state: u16,
    inside: u64,
    classes: Box<[String]>,
    style: CachedStyle,
}

/// Resolver state: the theme generation, resolved theme slots, the style cache and the
/// per-frame work lists (reused, so an idle frame allocates nothing).
#[derive(Resource, Debug, Default)]
pub struct ThemeResolver {
    generation: u64,
    stats: ThemeStats,
    active: String,
    error: Option<String>,
    density: Option<Density>,
    disabled_opacity: f32,
    fade: f32,
    reduced_motion: bool,
    fast: f32,
    // Theme-swap fade shaping (UI D15, ADR-1309): settings, the curve and space of the running
    // swap, its clock, and the nodes still on the swap fade (sorted when `swap_sorted`).
    transition: ThemeTransition,
    fade_ease: Option<Easing>,
    fade_space: BlendSpace,
    swap_remaining: f32,
    swap_fading: Vec<Entity>,
    swap_sorted: bool,
    previous: Option<String>,
    slots: Vec<ThemeSlot>,
    failed_scopes: Vec<String>,
    cache: HashMap<u64, Vec<CacheEntry>>,
    legacy_custom: Vec<String>,
    /// The custom classes with a `focus_visible` layer (ADR-7827): only a node carrying one keys
    /// its style by the visible-focus state, so a mode flip elsewhere stays a cache hit.
    legacy_focus_visible: Vec<String>,
    legacy_active_seen: Option<usize>,
    last_density: Option<Option<UiDensity>>,
    last_touch: bool,
    last_ring: Option<bool>,
    dirty: Vec<Entity>,
    stack: Vec<Entity>,
    animating: Vec<Entity>,
    /// Nodes whose theme layout box (padding, min and max size) changed since layout last took
    /// them (ADR-7208); `layout::mark_layout_dirty` drains it into the layout's change list.
    layout_moved: Vec<Entity>,
}

impl ThemeResolver {
    /// Takes the nodes whose theme padding or min/max size changed since the last call: the
    /// layout re-reads them (ADR-7208).
    pub fn take_layout_changes(&mut self) -> Vec<Entity> {
        std::mem::take(&mut self.layout_moved)
    }

    /// Current theme generation (0 before the first frame).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// What the last frame did.
    pub fn stats(&self) -> ThemeStats {
        self.stats
    }

    /// Name of the theme nodes resolve against (after the high-contrast and fallback rules).
    pub fn active(&self) -> &str {
        &self.active
    }

    /// Why the active theme could not be resolved, if it could not (nodes keep their look).
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The active theme, resolved for the current density.
    pub fn resolved_theme(&self) -> Option<&ResolvedTheme> {
        self.slots.first().map(|s| &s.theme)
    }

    /// The theme nodes resolved against before the last generation (`None` before the second).
    pub fn previous(&self) -> Option<&str> {
        self.previous.as_deref()
    }

    /// The swap fade settings the next swap uses.
    pub fn transition(&self) -> ThemeTransition {
        self.transition
    }

    /// Sets the swap fade settings; they apply from the next swap on (a running fade keeps its
    /// own). `theme_runtime::sync_theme_transition` calls it from the `ThemeTransition` resource.
    pub fn set_transition(&mut self, transition: ThemeTransition) {
        self.transition = transition;
    }

    /// Length of the last swap's cross-fade in seconds (0 when it was instant).
    pub fn fade_secs(&self) -> f32 {
        self.fade
    }

    /// The curve of the last swap's cross-fade (`None` = linear).
    pub fn fade_ease(&self) -> Option<Easing> {
        self.fade_ease
    }

    /// The blend space of the last swap's cross-fade.
    pub fn fade_space(&self) -> BlendSpace {
        self.fade_space
    }

    /// Seconds left of the running swap cross-fade (0 when none runs).
    pub fn fade_remaining(&self) -> f32 {
        if self.fading() { self.swap_remaining } else { 0.0 }
    }

    /// Whether a theme-swap cross-fade is still running on at least one node.
    pub fn fading(&self) -> bool {
        self.swap_remaining > 0.0 && !self.swap_fading.is_empty()
    }

    /// Nodes still on the running swap cross-fade.
    pub fn fading_nodes(&self) -> usize {
        if self.fading() { self.swap_fading.len() } else { 0 }
    }

    /// Takes a node off the swap fade (its colours now follow a state transition).
    fn unmark_swap(&mut self, entity: Entity) {
        if self.swap_sorted {
            if let Ok(i) = self.swap_fading.binary_search(&entity) {
                self.swap_fading.remove(i);
            }
        } else {
            self.swap_fading.retain(|&e| e != entity);
        }
    }

    fn sort_swap(&mut self) {
        if !self.swap_sorted {
            self.swap_fading.sort_unstable();
            self.swap_fading.dedup();
            self.swap_sorted = true;
        }
    }

    /// Starts a new generation: the selection, density or theme set changed.
    fn rebuild(&mut self, themes: &UiThemes, active: &ActiveTheme, density: Option<UiDensity>) {
        self.generation += 1;
        self.slots.clear();
        self.cache.clear();
        self.failed_scopes.clear();
        self.density = active.density.or(density.map(density_of));
        self.disabled_opacity = themes.disabled_opacity;
        self.reduced_motion = active.reduced_motion;
        let name = effective_theme_name(active, &themes.set);
        match ThemeSlot::new(&themes.set, &name, self.density, self.disabled_opacity) {
            Ok(slot) => {
                self.fade = if active.reduced_motion {
                    0.0
                } else {
                    self.transition.fade_secs(slot.theme.duration("motion.base"))
                };
                self.fast = slot.theme.duration("motion.fast").unwrap_or(0.0);
                self.fade_ease = self
                    .transition
                    .ease
                    .or_else(|| slot.theme.easing("ease.standard"));
                self.fade_space = self.transition.space;
                self.swap_remaining = self.fade;
                self.slots.push(slot);
                self.error = None;
            }
            Err(e) => {
                tracing::warn!(target: "regia_ui", "theme `{name}` failed to resolve: {e}");
                self.error = Some(e.to_string());
            }
        }
        if self.generation > 1 {
            self.previous = Some(std::mem::replace(&mut self.active, name));
        } else {
            self.active = name;
        }
        self.legacy_custom = custom_legacy_classes(&themes.classes);
        self.legacy_focus_visible = self
            .legacy_custom
            .iter()
            .filter(|c| {
                themes
                    .classes
                    .get(c)
                    .is_some_and(|class| class.focus_visible != VisualOverrides::default())
            })
            .cloned()
            .collect();
    }

    /// Slot of a scoped theme, resolving it on first use. Falls back to the active slot.
    fn slot_index(&mut self, set: &ThemeSet, name: &str) -> usize {
        if let Some(i) = self.slots.iter().position(|s| s.name == name) {
            return i;
        }
        if self.failed_scopes.iter().any(|n| n == name) {
            return 0;
        }
        match ThemeSlot::new(set, name, self.density, self.disabled_opacity) {
            Ok(slot) => {
                self.slots.push(slot);
                self.slots.len() - 1
            }
            Err(e) => {
                tracing::warn!(target: "regia_ui", "ThemeScope `{name}`: {e}; using the active theme");
                self.failed_scopes.push(name.to_owned());
                0
            }
        }
    }

    /// Whether any resolved theme slot has style rules with an `inside` condition (then a
    /// node's ancestors are part of its style key).
    fn has_inside_rules(&self) -> bool {
        self.slots.iter().any(|s| !s.inside.is_empty())
    }

    /// The composed theme layers for one (theme, classes, state, ancestor mask), from the cache
    /// when possible.
    fn style(
        &mut self,
        slot: usize,
        classes: &[String],
        state: &StyleState,
        inside: u64,
        legacy: &ThemeClasses,
    ) -> CachedStyle {
        // The visible-focus state matters only to a node whose theme has a `focus_visible`
        // layer somewhere or that carries a custom class with one (ADR-7827, ADR-8105 (e)), so a
        // mode flip elsewhere stays a cache hit.
        let masked;
        let state = if state.focus_visible
            && !self.slots[slot].focus_visible
            && !classes.iter().any(|c| self.legacy_focus_visible.contains(c))
        {
            masked = StyleState {
                focus_visible: false,
                ..*state
            };
            &masked
        } else {
            state
        };
        let bits = state_bits(state);
        let hash = self.cache.hasher().hash_one((slot, classes, bits, inside));
        if let Some(found) = self.cache.get(&hash).and_then(|bucket| {
            bucket
                .iter()
                .find(|e| e.slot == slot && e.state == bits && e.inside == inside && *e.classes == *classes)
        }) {
            return found.style;
        }
        self.stats.styles_computed += 1;
        let style = compute_style(
            &self.slots[slot],
            classes,
            state,
            inside,
            legacy,
            &self.legacy_custom,
        );
        self.cache.entry(hash).or_default().push(CacheEntry {
            slot,
            state: bits,
            inside,
            classes: classes.into(),
            style,
        });
        style
    }

    /// The new look of one node.
    fn resolve_themed(
        &mut self,
        themes: &UiThemes,
        (parents, scopes, node_classes): (&Query<&ChildOf>, &Query<&ThemeScope>, &Query<&Classes>),
        input: &NodeInputs,
        current: &Themed,
    ) -> Themed {
        self.stats.nodes_resolved += 1;
        let slot = match scope_of(input.entity, parents, scopes) {
            Some(name) => self.slot_index(&themes.set, name),
            None => 0,
        };
        let classes: &[String] = input.classes.map(|c| c.0.as_slice()).unwrap_or_default();
        let inside = {
            let s = &self.slots[slot];
            if s.inside.is_empty() {
                0
            } else {
                inside_mask(&s.theme, &s.inside, input.entity, parents, node_classes)
            }
        };
        let style = self.style(slot, classes, &input.state, inside, &themes.classes);
        let mut v = style.visuals;
        let mut transition = style.transition;
        let mut ease = style.ease;
        if let Some(o) = input.ovr {
            v.apply(&o.0.resolve_state(&input.state));
            if o.0.transition > 0.0 {
                transition = transition.max(o.0.transition);
                ease = o.0.transition_ease.or(ease);
            }
        }
        let accent = self.slots[slot].accent;
        // Reduced motion: a state change jumps to its end value.
        let transition = motion::reduced_duration(
            transition,
            motion::MotionKind::Transform,
            self.reduced_motion,
            self.fast,
        );
        let theme = &self.slots[slot].theme;
        let size_token = |keys: &[&str; 3]| input.size.and_then(|s| theme.number(keys[size_index(s)]));
        let background = v.background.or_else(|| {
            input.background.and_then(|b| match &b.kind {
                BackgroundKind::Color(c) => Some(*c),
                _ => None,
            })
        });
        let mut next = Themed {
            border_width: v.border_width,
            radius: v.radius,
            focus_ring: v.focus_ring,
            opacity: v.opacity,
            height: v.height.or_else(|| size_token(&CONTROL_HEIGHT)),
            padding_x: v.padding_x.or_else(|| size_token(&PAD_X)),
            layout_height: v.height,
            layout_padding_x: v.padding_x,
            translate: v.translate,
            scale: v.scale,
            focus_ring_width: v.focus_ring_width,
            focus_ring_offset: v.focus_ring_offset,
            min_width: v.min_width,
            padding: v.padding,
            min_height: v.min_height,
            max_width: v.max_width,
            max_height: v.max_height,
            gap: v.gap,
            text_px: v.text_px,
            icon_color: v.icon_color,
            elevation: v.elevation,
            focused: input.state.focused,
            focus_visible: input.ring && input.state.focused,
            accent,
            generation: self.generation,
            ..*current
        };
        // A node's first look applies at once; a theme change cross-fades every colour; a state
        // change animates the fill over the widget's `transition`.
        let (duration, all_channels) = if current.generation == 0 {
            (0.0, false)
        } else if current.generation != self.generation {
            (self.fade, true)
        } else {
            (transition, false)
        };
        let target = [
            Some(background.unwrap_or(UiColor::TRANSPARENT)),
            v.border_color,
            v.text_color,
        ];
        next.anim_ease = if all_channels { None } else { ease };
        let started = retarget(&mut next, target, duration, all_channels);
        if started && current.anim_remaining <= 0.0 {
            self.animating.push(input.entity);
        }
        // Swap fades follow the swap's curve and blend space; a state change takes a node off.
        if all_channels {
            if started {
                self.swap_fading.push(input.entity);
                self.swap_sorted = false;
            }
        } else if next.anim_to != current.anim_to {
            self.unmark_swap(input.entity);
        }
        next
    }
}

/// Everything resolution reads from one node.
struct NodeInputs<'a> {
    entity: Entity,
    classes: Option<&'a Classes>,
    background: Option<&'a Background>,
    ovr: Option<&'a ThemeOverride>,
    size: Option<WidgetSize>,
    state: StyleState,
    /// Whether a focused node shows its ring this frame.
    ring: bool,
}

fn node_inputs<'a>(
    entity: Entity,
    classes: Option<&'a Classes>,
    background: Option<&'a Background>,
    (touch, ring): (bool, bool),
    (hovered, pressed, focused, disabled): (
        Option<&Hovered>,
        Option<&Pressed>,
        Option<&Focused>,
        Option<&Disabled>,
    ),
    (selected, invalid, size, ovr): (
        Option<&Selected>,
        Option<&Invalid>,
        Option<&WidgetSize>,
        Option<&'a ThemeOverride>,
    ),
) -> NodeInputs<'a> {
    NodeInputs {
        entity,
        classes,
        background,
        ovr,
        size: size.copied(),
        state: StyleState {
            hovered: hovered.is_some(),
            pressed: pressed.is_some(),
            focused: focused.is_some(),
            disabled: disabled.is_some(),
            selected: selected.is_some(),
            error: invalid.is_some(),
            size: size.copied().unwrap_or_default(),
            touch,
            focus_visible: ring && focused.is_some(),
        },
        ring,
    }
}

/// Resolves one node and writes [`Themed`] only when the look changed (no change-detection
/// churn for layout and draw-list damage).
fn resolve_node(
    r: &mut ThemeResolver,
    themes: &UiThemes,
    tree: (&Query<&ChildOf>, &Query<&ThemeScope>, &Query<&Classes>),
    input: &NodeInputs,
    mut themed: Mut<Themed>,
) {
    if input.classes.is_none() && input.ovr.is_none() {
        // A node that lost its classes drops its theme look; never-themed nodes are left alone.
        if themed.generation != 0 {
            if layout_box(&themed) != layout_box(&Themed::default()) {
                r.layout_moved.push(input.entity);
            }
            *themed = Themed::default();
        }
        return;
    }
    let current = *themed;
    let next = r.resolve_themed(themes, tree, input, &current);
    if layout_box(&next) != layout_box(&current) {
        r.layout_moved.push(input.entity);
    }
    if next != current {
        *themed = next;
    }
}

/// The theme properties layout reads (ADR-7208, ADR-9340): padding, min and max size, and the
/// theme's own height, inline padding and gap.
type LayoutBox = (Option<[f32; 4]>, [Option<f32>; 4], [Option<f32>; 3]);

fn layout_box(t: &Themed) -> LayoutBox {
    (
        t.padding,
        [t.min_width, t.min_height, t.max_width, t.max_height],
        [t.layout_height, t.layout_padding_x, t.gap],
    )
}

type NodeQueryData = (
    Entity,
    Option<&'static Classes>,
    Option<&'static Background>,
    (
        Option<&'static Hovered>,
        Option<&'static Pressed>,
        Option<&'static Focused>,
        Option<&'static Disabled>,
    ),
    (
        Option<&'static Selected>,
        Option<&'static Invalid>,
        Option<&'static WidgetSize>,
        Option<&'static ThemeOverride>,
    ),
    &'static mut Themed,
);

type DirtyFilter = (
    With<Themed>,
    Or<(
        Or<(
            Changed<Classes>,
            Changed<Background>,
            Changed<WidgetSize>,
            Changed<ThemeOverride>,
        )>,
        Or<(
            Added<Hovered>,
            Added<Pressed>,
            Added<Focused>,
            Added<Disabled>,
            Added<Selected>,
            Added<Invalid>,
            Added<Themed>,
        )>,
    )>,
);

/// `PreUpdate`: resolve [`Themed`] for every classed node (ADR-289). See the module docs for the
/// precedence and the change-driven schedule.
pub fn apply_themes(
    mut themes: ResMut<UiThemes>,
    mut active: ResMut<ActiveTheme>,
    density: Option<Res<UiDensity>>,
    pointer: Option<Res<UiPointer>>,
    access: Option<Res<UiAccessibility>>,
    time: Option<Res<regia_app::RealTime>>,
    ring_inputs: (Option<Res<UiInputMode>>, Option<Res<FocusRingPolicy>>),
    mut resolver: ResMut<ThemeResolver>,
    mut nodes: ParamSet<(Query<NodeQueryData>, Query<Entity, DirtyFilter>)>,
    mut removed: (
        RemovedComponents<Hovered>,
        RemovedComponents<Pressed>,
        RemovedComponents<Focused>,
        RemovedComponents<Disabled>,
        RemovedComponents<Selected>,
        RemovedComponents<Invalid>,
        RemovedComponents<WidgetSize>,
        RemovedComponents<ThemeOverride>,
        RemovedComponents<Background>,
        RemovedComponents<Classes>,
    ),
    mut moved: (
        Query<Entity, Or<(Changed<ThemeScope>, Changed<ChildOf>)>>,
        RemovedComponents<ThemeScope>,
        RemovedComponents<ChildOf>,
        Query<Entity, (Changed<Classes>, With<Children>)>,
    ),
    tree: (
        Query<&ChildOf>,
        Query<&Children>,
        Query<&ThemeScope>,
        Query<&Classes>,
    ),
    unthemed: Query<
        Entity,
        (
            With<UiNode>,
            Without<Themed>,
            Or<(With<Classes>, With<ThemeOverride>)>,
        ),
    >,
    mut commands: Commands,
) {
    let _span = tracing::trace_span!("apply_themes").entered();
    let r = &mut *resolver;
    r.stats.nodes_resolved = 0;
    r.stats.styles_computed = 0;

    // 1. Selection and generation.
    let density_now = density.as_deref().copied();
    let density_changed = r.last_density != Some(density_now);
    r.last_density = Some(density_now);
    let touch = pointer.is_some_and(|p| p.active_touch.is_some());
    let touch_changed = r.last_touch != touch;
    r.last_touch = touch;
    let mode = ring_inputs.0.as_deref().copied().unwrap_or_default();
    let ring = ring_inputs
        .1
        .as_deref()
        .copied()
        .unwrap_or_default()
        .shows_ring(mode);
    let ring_changed = r.last_ring.is_some_and(|last| last != ring);
    r.last_ring = Some(ring);
    let full = themes.is_changed() || active.is_changed() || density_changed || r.generation == 0;
    if full {
        follow_legacy_selection(&mut themes, &mut active, r);
        r.rebuild(&themes, &active, density_now);
        // The player's reduced-motion setting also skips the swap cross-fade (ADR-522).
        if crate::motion::reduced_motion(None, access.as_deref()) {
            r.fade = 0.0;
            r.swap_remaining = 0.0;
        }
    }

    // 2. Which nodes need work.
    let mut dirty = std::mem::take(&mut r.dirty);
    dirty.clear();
    if full {
        removed.0.clear();
        removed.1.clear();
        removed.2.clear();
        removed.3.clear();
        removed.4.clear();
        removed.5.clear();
        removed.6.clear();
        removed.7.clear();
        removed.8.clear();
        removed.9.clear();
        moved.1.clear();
        moved.2.clear();
    } else {
        dirty.extend(nodes.p1().iter());
        if touch_changed {
            // Touch suppresses the hover layer, so hovered nodes re-resolve when it flips.
            dirty.extend(
                nodes
                    .p0()
                    .iter()
                    .filter(|(_, _, _, (hovered, ..), ..)| hovered.is_some())
                    .map(|n| n.0),
            );
        }
        if ring_changed {
            // The ring shows or hides with the input mode: only focused nodes change (I05).
            dirty.extend(
                nodes
                    .p0()
                    .iter()
                    .filter(|(_, _, _, (_, _, focused, _), ..)| focused.is_some())
                    .map(|n| n.0),
            );
        }
        dirty.extend(removed.0.read());
        dirty.extend(removed.1.read());
        dirty.extend(removed.2.read());
        dirty.extend(removed.3.read());
        dirty.extend(removed.4.read());
        dirty.extend(removed.5.read());
        dirty.extend(removed.6.read());
        dirty.extend(removed.7.read());
        dirty.extend(removed.8.read());
        // Empty in an idle frame: collecting nothing allocates nothing.
        let lost_classes: Vec<Entity> = removed.9.read().collect();
        dirty.extend_from_slice(&lost_classes);
        // A scope change or a reparent can change the theme of a whole subtree.
        let mut stack = std::mem::take(&mut r.stack);
        for root in moved.0.iter().chain(moved.1.read()).chain(moved.2.read()) {
            push_subtree(root, &tree.1, &mut stack, &mut dirty);
        }
        // With `inside` rules, a class change on a node can restyle its descendants.
        if r.has_inside_rules() {
            for root in lost_classes.into_iter().chain(&moved.3) {
                push_subtree(root, &tree.1, &mut stack, &mut dirty);
            }
        }
        r.stack = stack;
    }

    // 3. Resolve.
    let tree_reads = (&tree.0, &tree.2, &tree.3);
    let mut q = nodes.p0();
    if !r.slots.is_empty() {
        if full {
            for (entity, classes, background, markers, extra, themed) in &mut q {
                let input = node_inputs(entity, classes, background, (touch, ring), markers, extra);
                resolve_node(r, &themes, tree_reads, &input, themed);
            }
        } else if !dirty.is_empty() {
            dirty.sort_unstable();
            dirty.dedup();
            for &e in &dirty {
                if let Ok((entity, classes, background, markers, extra, themed)) = q.get_mut(e) {
                    let input = node_inputs(entity, classes, background, (touch, ring), markers, extra);
                    resolve_node(r, &themes, tree_reads, &input, themed);
                }
            }
        }
    }
    r.dirty = dirty;
    r.sort_swap();

    // 4. Advance running transitions and cross-fades.
    // A world without the app clock (agent tests, tools) resolves with no transition time.
    let dt = time.map_or(0.0, |t| t.delta().as_secs_f32());
    if !r.animating.is_empty() {
        let mut list = std::mem::take(&mut r.animating);
        let (swap, ease, space) = (&r.swap_fading, r.fade_ease, r.fade_space);
        list.retain(|&e| {
            let Ok((.., mut themed)) = q.get_mut(e) else {
                return false;
            };
            let mut t = *themed;
            if t.anim_total <= 0.0 {
                // Reset or settled since it was queued (classes removed mid-fade): nothing to show.
                return false;
            }
            t.anim_remaining = (t.anim_remaining - dt).max(0.0);
            let k = if t.anim_total > 0.0 {
                motion::sample_points(t.anim_ease, 1.0 - t.anim_remaining / t.anim_total)
            } else {
                1.0
            };
            if !swap.is_empty() && swap.binary_search(&e).is_ok() {
                show_swap(&mut t, k, ease, space);
            } else {
                show(&mut t, k);
            }
            if *themed != t {
                *themed = t;
            }
            t.anim_remaining > 0.0
        });
        r.animating = list;
    }
    if r.swap_remaining > 0.0 {
        r.swap_remaining = (r.swap_remaining - dt).max(0.0);
    }
    if r.swap_remaining <= 0.0 && !r.swap_fading.is_empty() {
        r.swap_fading.clear();
    }
    r.stats.generation = r.generation;
    r.stats.animating = u32::try_from(r.animating.len()).unwrap_or(u32::MAX);

    // 5. New classed nodes get a `Themed` (resolved next frame through `Added<Themed>`).
    for entity in &unthemed {
        commands.entity(entity).insert(Themed::default());
    }
}

/// Keeps the legacy [`UiThemes`] selection (`select`, `toggle`) and [`ActiveTheme`] in step,
/// without tripping either one's change detection (which would bump the generation twice).
fn follow_legacy_selection(
    themes: &mut ResMut<UiThemes>,
    active: &mut ResMut<ActiveTheme>,
    r: &mut ThemeResolver,
) {
    let legacy_now = themes.active;
    let legacy_moved = themes.is_changed() && r.legacy_active_seen.is_some_and(|seen| seen != legacy_now);
    // Before the first frame a game may have picked a legacy palette while leaving
    // `ActiveTheme` at its default: the legacy choice wins then.
    let legacy_preset =
        r.legacy_active_seen.is_none() && legacy_now != 0 && **active == ActiveTheme::default();
    if legacy_moved || legacy_preset {
        if let Some(name) = legacy_theme_name(themes.bypass_change_detection()) {
            active.bypass_change_detection().name = name;
        }
    } else if active.is_changed() {
        let found = themes
            .themes
            .iter()
            .position(|t| legacy_name_of(&t.name) == active.name.as_str());
        if let Some(i) = found
            && i != themes.active
        {
            themes.bypass_change_detection().active = i;
        }
    }
    r.legacy_active_seen = Some(themes.active);
}

/// The `.theme.ron` name for a legacy palette name: the built-in `dark` and `light` palettes are
/// views of `regia-dark` and `regia-light`.
fn legacy_name_of(name: &str) -> &str {
    match name {
        "dark" => DEFAULT_THEME,
        "light" => "regia-light",
        other => other,
    }
}

/// The theme for the legacy palette now selected in `themes`; a game palette with no theme of
/// its name is migrated (v0 → v1) and added to the set.
fn legacy_theme_name(themes: &mut UiThemes) -> Option<String> {
    let current = themes.current().clone();
    let mapped = legacy_name_of(&current.name);
    if themes.set.get(mapped).is_some() {
        return Some(mapped.to_owned());
    }
    match themes.set.insert(migrate_v0(&current, &ThemeClasses::default())) {
        Ok(()) => Some(current.name),
        Err(e) => {
            tracing::warn!(target: "regia_ui", "legacy theme `{}` could not be migrated: {e}", current.name);
            None
        }
    }
}

fn effective_theme_name(active: &ActiveTheme, set: &ThemeSet) -> String {
    let base = if set.get(&active.name).is_some() {
        active.name.as_str()
    } else {
        tracing::warn!(target: "regia_ui", "theme `{}` is not loaded; using {DEFAULT_THEME}", active.name);
        DEFAULT_THEME
    };
    if !active.high_contrast {
        return base.to_owned();
    }
    let own = format!("{base}-high-contrast");
    if set.get(&own).is_some() {
        own
    } else if set.get(HIGH_CONTRAST_THEME).is_some() {
        HIGH_CONTRAST_THEME.to_owned()
    } else {
        base.to_owned()
    }
}

fn density_of(d: UiDensity) -> Density {
    match d {
        UiDensity::Comfortable => Density::Comfortable,
        UiDensity::Compact => Density::Compact,
    }
}

/// Names of game classes in [`UiThemes::classes`] that differ from the built-in class set: they
/// still layer over the theme (over the variations, under the node override). Unchanged
/// built-in classes are superseded by the theme's widget types.
pub(crate) fn custom_legacy_classes(classes: &ThemeClasses) -> Vec<String> {
    let builtin = ThemeClasses::builtin();
    classes
        .0
        .iter()
        .filter(|(name, class)| builtin.get(name) != Some(class))
        .map(|(name, _)| name.clone())
        .collect()
}

fn push_subtree(root: Entity, children: &Query<&Children>, stack: &mut Vec<Entity>, out: &mut Vec<Entity>) {
    stack.clear();
    stack.push(root);
    while let Some(e) = stack.pop() {
        out.push(e);
        if let Ok(kids) = children.get(e) {
            stack.extend(kids.iter());
        }
    }
}

/// The nearest [`ThemeScope`] on the node or an ancestor.
fn scope_of<'a>(
    entity: Entity,
    parents: &Query<&ChildOf>,
    scopes: &'a Query<&ThemeScope>,
) -> Option<&'a str> {
    let mut cur = Some(entity);
    let mut depth = 0;
    while let Some(e) = cur {
        if let Ok(scope) = scopes.get(e) {
            return Some(scope.theme.as_str());
        }
        depth += 1;
        if depth > MAX_SCOPE_DEPTH {
            break;
        }
        cur = parents.get(e).ok().map(ChildOf::parent);
    }
    None
}

fn size_index(s: WidgetSize) -> usize {
    match s {
        WidgetSize::Sm => 0,
        WidgetSize::Md => 1,
        WidgetSize::Lg => 2,
    }
}

/// The cache key of a state (every field of [`StyleState`]).
fn state_bits(s: &StyleState) -> u16 {
    let size = size_index(s.size) as u16;
    u16::from(s.hovered)
        | (u16::from(s.pressed) << 1)
        | (u16::from(s.focused) << 2)
        | (u16::from(s.disabled) << 3)
        | (u16::from(s.selected) << 4)
        | (u16::from(s.error) << 5)
        | (u16::from(s.touch) << 6)
        | (size << 7)
        | (u16::from(s.focus_visible) << 9)
}

/// Points the colour animation at `to` (`[background, border, text]`). Returns true when an
/// animation starts. `all_channels` cross-fades every colour (a theme swap); otherwise only the
/// fill animates and border and text change at once (a state change).
fn retarget(next: &mut Themed, to: [Option<UiColor>; 3], duration: f32, all_channels: bool) -> bool {
    if next.anim_to == to {
        // Same target: a running animation keeps going, a settled one stays.
        return false;
    }
    let shown = [next.background, next.border_color, next.text_color];
    next.anim_to = to;
    next.anim_target = to[0].unwrap_or(UiColor::TRANSPARENT);
    let moves = if all_channels {
        shown != to
    } else {
        shown[0] != to[0]
    };
    if duration > 0.0 && moves {
        next.anim_from = shown.map(|c| c.unwrap_or(UiColor::TRANSPARENT));
        if !all_channels {
            next.anim_from[1] = to[1].unwrap_or(UiColor::TRANSPARENT);
            next.anim_from[2] = to[2].unwrap_or(UiColor::TRANSPARENT);
        }
        next.anim_total = duration;
        next.anim_remaining = duration;
        show(next, 0.0);
        true
    } else {
        next.anim_total = 0.0;
        next.anim_remaining = 0.0;
        show(next, 1.0);
        false
    }
}

/// Writes the displayed colours at animation progress `k` (`1.0` = the target exactly).
fn show(t: &mut Themed, k: f32) {
    let k = k.clamp(0.0, 1.0);
    let [bg, border, text]: [Option<UiColor>; 3] = std::array::from_fn(|i| {
        t.anim_to[i].map(|to| if k >= 1.0 { to } else { t.anim_from[i].lerp(to, k) })
    });
    t.background = Some(bg.unwrap_or(UiColor::TRANSPARENT));
    t.anim_background = bg.unwrap_or(UiColor::TRANSPARENT);
    t.border_color = border;
    t.text_color = text;
}

/// Writes the displayed colours of a theme-swap cross-fade at linear progress `k`: eased by the
/// swap curve and blended in the swap's space (`1.0` = the target exactly, as [`show`]).
fn show_swap(t: &mut Themed, k: f32, ease: Option<Easing>, space: BlendSpace) {
    let k = k.clamp(0.0, 1.0);
    if k >= 1.0 {
        show(t, 1.0);
        return;
    }
    let e = ease.map_or(k, |ease| ease_progress(ease, k));
    let [bg, border, text]: [Option<UiColor>; 3] =
        std::array::from_fn(|i| t.anim_to[i].map(|to| blend_swap_color(t.anim_from[i], to, e, space)));
    t.background = Some(bg.unwrap_or(UiColor::TRANSPARENT));
    t.anim_background = bg.unwrap_or(UiColor::TRANSPARENT);
    t.border_color = border;
    t.text_color = text;
}

/// Composes theme layers (root first): each layer state-composed in the § 3 order, later layers
/// winning per field; then the disabled layers of every layer, so disabled wins a contested
/// property. Hover and pressed are suppressed while disabled; the focused ring still applies.
/// Returns the visuals and the longest transition.
fn compose(levels: &[&StyleClass], state: &StyleState) -> (VisualOverrides, f32, Option<[f32; 4]>) {
    let enabled = StyleState {
        disabled: false,
        hovered: state.hovered && !state.disabled,
        pressed: state.pressed && !state.disabled,
        focused: state.focused,
        ..*state
    };
    let mut out = VisualOverrides::default();
    let mut transition = 0.0_f32;
    let mut ease = None;
    for level in levels {
        out.apply(&level.resolve_state(&enabled));
        if level.transition > 0.0 {
            transition = transition.max(level.transition);
            ease = level.transition_ease.or(ease);
        }
    }
    if state.disabled {
        for level in levels {
            out.apply(&level.disabled);
        }
    }
    (out, transition, ease)
}

fn compute_style(
    slot: &ThemeSlot,
    classes: &[String],
    state: &StyleState,
    inside: u64,
    legacy: &ThemeClasses,
    legacy_custom: &[String],
) -> CachedStyle {
    let theme = &slot.theme;
    let has_inside = |name: &str| {
        slot.inside
            .binary_search_by(|n| n.as_str().cmp(name))
            .is_ok_and(|i| inside & (1 << i) != 0)
    };
    let owned: Vec<StyleClass> = theme_levels(theme, classes, &has_inside)
        .into_iter()
        .map(|(_, class)| class)
        .collect();
    let mut levels: Vec<&StyleClass> = Vec::with_capacity(owned.len() + 1 + classes.len());
    levels.push(&slot.defaults);
    levels.extend(owned.iter());
    for class in classes {
        if legacy_custom.iter().any(|c| c == class)
            && let Some(custom) = legacy.get(class)
        {
            levels.push(custom);
        }
    }
    let (visuals, transition, ease) = compose(&levels, state);
    CachedStyle {
        visuals,
        transition,
        ease,
    }
}

/// The theme layers of a node with `classes`, labeled, in apply order: the type and variation
/// keys of the classes (root first, a later class winning), then the style rules that match,
/// by ascending specificity and then chain and file order (ADR-8303). A rule's label is
/// `rule <selector>` with the selector's display text, without its state. `has_inside` says
/// whether some ancestor of the node has a widget type.
pub(crate) fn theme_levels(
    theme: &ResolvedTheme,
    classes: &[String],
    has_inside: &dyn Fn(&str) -> bool,
) -> Vec<(String, StyleClass)> {
    let mut keys = Vec::new();
    for class in classes {
        class_chain(theme, class, &mut keys);
    }
    dedup_keep_last(&mut keys);
    let mut out: Vec<(String, StyleClass)> = keys
        .iter()
        .filter_map(|k| {
            let def = theme.variations.get(k).or_else(|| theme.types.get(k))?;
            Some((k.clone(), class_of(theme, def)))
        })
        .collect();
    if theme.rules.is_empty() {
        return out;
    }
    let types = class_types(theme, classes);
    let mut matching: Vec<(usize, &StyleRule)> = theme
        .rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule_matches(rule, classes, &keys, &types, has_inside))
        .collect();
    // Stable: equal specificity keeps chain and file order.
    matching.sort_by_key(|(i, rule)| (rule.specificity(), *i));
    for (_, rule) in matching {
        let label = StyleRule {
            state: None,
            props: PropMap::new(),
            ..rule.clone()
        }
        .display();
        out.push((format!("rule {label}"), rule_class(theme, rule)));
    }
    out
}

/// The style rules of `theme` that match a node, as display text with their state, in apply
/// order (for inspectors and the agent).
pub(crate) fn matching_rule_texts(
    theme: &ResolvedTheme,
    classes: &[String],
    has_inside: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let mut keys = Vec::new();
    for class in classes {
        class_chain(theme, class, &mut keys);
    }
    let types = class_types(theme, classes);
    let mut matching: Vec<(usize, &StyleRule)> = theme
        .rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule_matches(rule, classes, &keys, &types, has_inside))
        .collect();
    matching.sort_by_key(|(i, rule)| (rule.specificity(), *i));
    matching.into_iter().map(|(_, r)| r.display()).collect()
}

/// The widget types a node with `classes` has, for rule matching: every type and variation key
/// its classes pick, each class's own key (the class itself when it names a type: it has a dot
/// or starts with an upper-case letter; else its v0 class-table home), and each key's part before
/// its first dot. Sorted, without repeats.
pub(crate) fn class_types(theme: &ResolvedTheme, classes: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for class in classes {
        let start = out.len();
        class_chain(theme, class, &mut out);
        // A class that names a type key (`Toolbar`, `Button.ghost`) keeps it; a lower-case class
        // name goes through the v0 class table (`button` → `Button`).
        let names_key = theme.types.contains_key(class)
            || theme.variations.contains_key(class)
            || class.contains('.')
            || class.starts_with(|c: char| c.is_ascii_uppercase());
        let home = if names_key {
            class.clone()
        } else {
            class_target(class).key
        };
        out.push(home);
        let added: Vec<String> = out[start..]
            .iter()
            .filter_map(|k| k.split_once('.').map(|(t, _)| t.to_owned()))
            .collect();
        out.extend(added);
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Whether one rule matches a node (every condition that is set holds).
fn rule_matches(
    rule: &StyleRule,
    classes: &[String],
    keys: &[String],
    types: &[String],
    has_inside: &dyn Fn(&str) -> bool,
) -> bool {
    if let Some(widget) = &rule.widget {
        if types.binary_search(widget).is_err() {
            return false;
        }
        if let Some(variation) = &rule.variation {
            let key = format!("{widget}.{variation}");
            if !keys.contains(&key) && types.binary_search(&key).is_err() {
                return false;
            }
        }
    }
    if !rule.classes.iter().all(|c| classes.contains(c)) {
        return false;
    }
    rule.inside.as_deref().is_none_or(has_inside)
}

/// A rule as a [`StyleClass`]: its properties in the layer of its state.
fn rule_class(theme: &ResolvedTheme, rule: &StyleRule) -> StyleClass {
    let v = visuals(theme, &rule.props);
    let mut class = StyleClass::default();
    match rule.state {
        None => {
            class.base = v;
            class.transition = transition_secs(theme, &rule.props);
            class.transition_ease = transition_ease(theme, &rule.props);
        }
        Some(RuleState::Hover) => class.hover = v,
        Some(RuleState::Pressed) => class.pressed = v,
        Some(RuleState::Focused) => class.focused = v,
        Some(RuleState::FocusVisible) => class.focus_visible = v,
        Some(RuleState::Disabled) => class.disabled = v,
        Some(RuleState::Selected) => class.selected = v,
        Some(RuleState::Error) => class.error = v,
    }
    class
}

/// Bit `i` set when some ancestor of `entity` has the widget type `inside[i]` (`inside` is
/// sorted and at most 64 long, `MAX_RULE_INSIDE_TYPES`).
fn inside_mask(
    theme: &ResolvedTheme,
    inside: &[String],
    entity: Entity,
    parents: &Query<&ChildOf>,
    classes: &Query<&Classes>,
) -> u64 {
    let mut mask = 0_u64;
    let mut cur = parents.get(entity).ok().map(ChildOf::parent);
    let mut depth = 0;
    while let Some(e) = cur {
        if let Ok(c) = classes.get(e) {
            mask |= types_mask(theme, inside, &c.0);
        }
        depth += 1;
        if depth > MAX_SCOPE_DEPTH {
            break;
        }
        cur = parents.get(e).ok().map(ChildOf::parent);
    }
    mask
}

/// The `inside` bits one ancestor's classes contribute.
pub(crate) fn types_mask(theme: &ResolvedTheme, inside: &[String], classes: &[String]) -> u64 {
    let types = class_types(theme, classes);
    inside
        .iter()
        .take(64)
        .enumerate()
        .filter(|(_, name)| types.binary_search(name).is_ok())
        .fold(0, |m, (i, _)| m | (1 << i))
}

/// Appends the type/variation keys one class contributes, root first (`Button`,
/// `Button.primary`, `Button.danger`). A key the theme does not define contributes nothing.
pub(crate) fn class_chain(theme: &ResolvedTheme, class: &str, out: &mut Vec<String>) {
    let mut key = if theme.types.contains_key(class) || theme.variations.contains_key(class) {
        class.to_owned()
    } else {
        class_target(class).key
    };
    let start = out.len();
    loop {
        if out[start..].contains(&key) {
            break;
        }
        if let Some(def) = theme.variations.get(&key) {
            let base = def
                .extends
                .clone()
                .or_else(|| key.rsplit_once('.').map(|(b, _)| b.to_owned()));
            out.push(key);
            match base {
                Some(b) => key = b,
                None => break,
            }
        } else if theme.types.contains_key(&key) {
            out.push(key);
            break;
        } else {
            break;
        }
    }
    out[start..].reverse();
}

/// Removes repeated keys, keeping each key's last position (a later class wins).
pub(crate) fn dedup_keep_last(keys: &mut Vec<String>) {
    let mut i = 0;
    while i < keys.len() {
        if keys[i + 1..].contains(&keys[i]) {
            keys.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Precedence row 4: the theme defaults every classed node starts from, and the one disabled
/// rule (spec § 3: the disabled opacity over the normal look plus `color.text-disabled`).
pub(crate) fn defaults_class(theme: &ResolvedTheme, disabled_opacity: f32) -> StyleClass {
    StyleClass {
        base: VisualOverrides {
            border_color: theme.color("color.border"),
            radius: theme.number("radius.md"),
            text_color: theme.color("color.text"),
            // The ring has its own role (D9); the fills read `Themed::accent`. A theme without
            // the role (a migrated v0 palette) keeps the old accent ring.
            focus_ring: theme
                .color("color.focus-ring")
                .or_else(|| theme.color("color.accent")),
            focus_ring_width: theme.number("border.width.focus"),
            focus_ring_offset: theme.number("border.width.focus-offset"),
            ..Default::default()
        },
        selected: VisualOverrides {
            background: theme.color("color.selection"),
            ..Default::default()
        },
        error: VisualOverrides {
            border_color: theme.color("color.danger"),
            ..Default::default()
        },
        disabled: VisualOverrides {
            text_color: theme.color("color.text-disabled"),
            opacity: (disabled_opacity < 1.0).then_some(disabled_opacity.max(0.0)),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A theme type or variation as a [`StyleClass`] with resolved values.
pub(crate) fn class_of(theme: &ResolvedTheme, def: &TypeDef) -> StyleClass {
    let size = |name: &str| def.sizes.get(name).map(|p| visuals(theme, p)).unwrap_or_default();
    StyleClass {
        base: visuals(theme, &def.props),
        hover: visuals(theme, &def.hover),
        pressed: visuals(theme, &def.pressed),
        focused: visuals(theme, &def.focused),
        // The keyboard-only focus layer (`.theme.ron` v2, ADR-8105 (e)).
        focus_visible: visuals(theme, &def.focus_visible),
        disabled: visuals(theme, &def.disabled),
        selected: visuals(theme, &def.selected),
        error: visuals(theme, &def.error),
        sm: size("sm"),
        lg: size("lg"),
        parts: Vec::new(),
        transition: transition_secs(theme, &def.props),
        transition_ease: transition_ease(theme, &def.props),
    }
}

/// Evaluates one property value: a number, a token expression, or a keyword (text).
fn eval(theme: &ResolvedTheme, owner: &str, value: &PropValue) -> Option<Value> {
    match value {
        PropValue::Number(n) => Some(Value::Number(*n)),
        PropValue::Text(t) if looks_like_expression(t) => {
            let expr = Expr::parse(t).ok()?;
            theme.table.resolve_expr(owner, &expr).ok()
        }
        PropValue::Text(t) => Some(Value::Text(t.clone())),
        PropValue::List(_) => None,
    }
}

fn color_of(theme: &ResolvedTheme, owner: &str, value: &PropValue) -> Option<UiColor> {
    eval(theme, owner, value)?.as_color()
}

fn number_of(theme: &ResolvedTheme, owner: &str, value: &PropValue) -> Option<f32> {
    eval(theme, owner, value)?.as_number()
}

/// The number of a scalar, or of the first entry of a list (`radius: [8, 8, 4, 4]`).
fn first_number(theme: &ResolvedTheme, owner: &str, value: &PropValue) -> Option<f32> {
    match value {
        PropValue::List(items) => items.first().and_then(|i| number_of(theme, owner, i)),
        _ => number_of(theme, owner, value),
    }
}

fn transition_secs(theme: &ResolvedTheme, props: &PropMap) -> f32 {
    let duration = |v: &PropValue| eval(theme, "transition", v)?.as_duration();
    let found = match props.get("transition") {
        Some(PropValue::List(items)) => items.iter().find_map(duration),
        Some(v) => duration(v),
        None => None,
    };
    found.unwrap_or(0.0)
}

/// The easing of a `transition` property written as a list (`["@motion.fast", "@ease.standard"]`);
/// `None` (linear) when no entry is an easing.
fn transition_ease(theme: &ResolvedTheme, props: &PropMap) -> Option<[f32; 4]> {
    let easing = |v: &PropValue| eval(theme, "transition", v)?.as_easing();
    match props.get("transition") {
        Some(PropValue::List(items)) => items.iter().find_map(easing),
        Some(v) => easing(v),
        None => None,
    }
    .and_then(motion::points_of)
}

/// Maps a property layer onto the widget property set (spec § 6.2). Unknown properties are
/// ignored here (kept in the file; the linter reports them).
fn visuals(theme: &ResolvedTheme, props: &PropMap) -> VisualOverrides {
    let mut v = VisualOverrides::default();
    for (key, value) in props {
        let k = key.as_str();
        match k {
            "background" => v.background = color_of(theme, k, value),
            "border_color" => v.border_color = color_of(theme, k, value),
            "border_width" => v.border_width = number_of(theme, k, value),
            "border" => {
                let items = match value {
                    PropValue::List(items) => items.as_slice(),
                    single => std::slice::from_ref(single),
                };
                for item in items {
                    match eval(theme, k, item) {
                        Some(Value::Number(n)) => v.border_width = Some(n),
                        Some(Value::Color(c)) => v.border_color = Some(c),
                        _ => {}
                    }
                }
            }
            "radius" => v.radius = first_number(theme, k, value),
            "text_color" => v.text_color = color_of(theme, k, value),
            "icon_color" => v.icon_color = color_of(theme, k, value),
            "focus_ring" => {
                let items = match value {
                    PropValue::List(items) => items.as_slice(),
                    single => std::slice::from_ref(single),
                };
                let mut numbers = 0;
                for item in items {
                    match eval(theme, k, item) {
                        Some(Value::Color(c)) => v.focus_ring = Some(c),
                        Some(Value::Number(n)) if numbers == 0 => {
                            v.focus_ring_width = Some(n);
                            numbers += 1;
                        }
                        Some(Value::Number(n)) => v.focus_ring_offset = Some(n),
                        _ => {}
                    }
                }
            }
            "focus_ring_width" => v.focus_ring_width = number_of(theme, k, value),
            "focus_ring_offset" => v.focus_ring_offset = number_of(theme, k, value),
            "padding_x" => v.padding_x = number_of(theme, k, value),
            "height" => v.height = number_of(theme, k, value),
            "min_width" => v.min_width = number_of(theme, k, value),
            "min_height" => v.min_height = number_of(theme, k, value),
            "max_width" => v.max_width = number_of(theme, k, value),
            "max_height" => v.max_height = number_of(theme, k, value),
            "padding" => v.padding = padding_of(theme, k, value),
            "gap" => v.gap = number_of(theme, k, value),
            "text_px" | "font_size" => v.text_px = number_of(theme, k, value),
            "elevation" => v.elevation = number_of(theme, k, value).map(|n| n.clamp(0.0, 255.0) as u8),
            "opacity" => v.opacity = number_of(theme, k, value).map(|n| n.clamp(0.0, 1.0)),
            "scale" => v.scale = number_of(theme, k, value),
            "translate" => {
                if let PropValue::List(items) = value
                    && let [x, y] = items.as_slice()
                    && let (Some(x), Some(y)) = (number_of(theme, k, x), number_of(theme, k, y))
                {
                    v.translate = Some([x, y]);
                }
            }
            _ => {}
        }
    }
    v
}

/// The `padding` property (ADR-7208): one number pads every edge, two are vertical then
/// horizontal, four are top, right, bottom, left (CSS shorthand order). Returned as left, top,
/// right, bottom (the [`crate::style::Edges`] order). Any other count, or a value that is not a
/// number, leaves the property unset.
fn padding_of(theme: &ResolvedTheme, k: &str, value: &PropValue) -> Option<[f32; 4]> {
    let items = match value {
        PropValue::List(items) => items.as_slice(),
        single => std::slice::from_ref(single),
    };
    let numbers: Option<Vec<f32>> = items.iter().map(|item| number_of(theme, k, item)).collect();
    match numbers?.as_slice() {
        [all] => Some([*all; 4]),
        [vertical, horizontal] => Some([*horizontal, *vertical, *horizontal, *vertical]),
        [top, right, bottom, left] => Some([*left, *top, *right, *bottom]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dark() -> ResolvedTheme {
        builtin_theme_set().resolve(DEFAULT_THEME, None).unwrap()
    }

    fn keys(theme: &ResolvedTheme, classes: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for c in classes {
            class_chain(theme, c, &mut out);
        }
        dedup_keep_last(&mut out);
        out
    }

    #[test]
    fn classes_pick_types_and_variation_chains_root_first() {
        let t = dark();
        assert_eq!(keys(&t, &["button"]), ["Button"]);
        assert_eq!(keys(&t, &["button", "primary"]), ["Button", "Button.primary"]);
        assert_eq!(
            keys(&t, &["danger"]),
            ["Button", "Button.primary", "Button.danger"]
        );
        assert_eq!(keys(&t, &["Button.ghost"]), ["Button", "Button.ghost"]);
        assert_eq!(keys(&t, &["surface"]), ["Panel", "Panel.surface"]);
        assert!(keys(&t, &["made-up"]).is_empty(), "unknown classes add nothing");
        // A later class wins: `primary` then `button` ends on the plain button.
        assert_eq!(keys(&t, &["primary", "button"]), ["Button.primary", "Button"]);
    }

    #[test]
    fn state_bits_are_distinct_for_every_field() {
        let base = StyleState::default();
        let variants = [
            StyleState {
                hovered: true,
                ..base
            },
            StyleState {
                pressed: true,
                ..base
            },
            StyleState {
                focused: true,
                ..base
            },
            StyleState {
                disabled: true,
                ..base
            },
            StyleState {
                selected: true,
                ..base
            },
            StyleState { error: true, ..base },
            StyleState { touch: true, ..base },
            StyleState {
                size: WidgetSize::Sm,
                ..base
            },
            StyleState {
                size: WidgetSize::Lg,
                ..base
            },
        ];
        let mut seen = vec![state_bits(&base)];
        for v in variants {
            let bits = state_bits(&v);
            assert!(!seen.contains(&bits), "{v:?}");
            seen.push(bits);
        }
    }

    #[test]
    fn property_values_map_onto_the_widget_property_set() {
        let t = dark();
        let mut props = PropMap::new();
        props.insert(
            "border".to_owned(),
            PropValue::List(vec![
                PropValue::Number(2.0),
                PropValue::Text("@color.danger".to_owned()),
            ]),
        );
        props.insert(
            "translate".to_owned(),
            PropValue::List(vec![PropValue::Number(0.0), PropValue::Number(1.0)]),
        );
        props.insert("radius".to_owned(), PropValue::Text("@radius.lg".to_owned()));
        props.insert("opacity".to_owned(), PropValue::Number(3.0));
        props.insert("cursor".to_owned(), PropValue::Text("pointer".to_owned()));
        let v = visuals(&t, &props);
        assert_eq!(v.border_width, Some(2.0));
        assert_eq!(v.border_color, t.color("color.danger"));
        assert_eq!(v.translate, Some([0.0, 1.0]));
        assert_eq!(v.radius, Some(12.0));
        assert_eq!(v.opacity, Some(1.0), "opacity clamps to 0..=1");
    }

    #[test]
    fn disabled_layers_apply_after_every_state_layer() {
        let accent = UiColor::linear(0.1, 0.2, 0.9, 1.0);
        let hover = UiColor::linear(0.3, 0.3, 0.3, 1.0);
        let base = StyleClass {
            hover: VisualOverrides {
                background: Some(hover),
                ..Default::default()
            },
            disabled: VisualOverrides {
                opacity: Some(0.45),
                ..Default::default()
            },
            ..Default::default()
        };
        let variation = StyleClass {
            base: VisualOverrides {
                background: Some(accent),
                ..Default::default()
            },
            ..Default::default()
        };
        let state = StyleState {
            hovered: true,
            disabled: true,
            ..Default::default()
        };
        let (v, ..) = compose(&[&base, &variation], &state);
        assert_eq!(
            v.background,
            Some(accent),
            "hover suppressed, the normal look stays"
        );
        assert_eq!(
            v.opacity,
            Some(0.45),
            "the root disabled rule reaches the variation"
        );
    }

    #[test]
    fn retarget_animates_the_fill_only_for_state_changes() {
        let a = UiColor::linear(0.0, 0.0, 0.0, 1.0);
        let b = UiColor::linear(1.0, 1.0, 1.0, 1.0);
        let mut t = Themed::default();
        assert!(!retarget(&mut t, [Some(a), Some(a), Some(a)], 0.0, false));
        assert_eq!(t.background, Some(a));
        assert!(retarget(&mut t, [Some(b), Some(b), Some(b)], 0.1, false));
        assert_eq!(t.background, Some(a), "the fill starts where it was");
        assert_eq!(t.border_color, Some(b), "border changes at once");
        assert!(
            !retarget(&mut t, [Some(b), Some(b), Some(b)], 0.1, false),
            "same target"
        );
        show(&mut t, 1.0);
        assert_eq!(t.background, Some(b));
        assert!(retarget(&mut t, [Some(a), Some(a), Some(a)], 0.1, true));
        assert_eq!(t.border_color, Some(b), "a swap fades every colour");
    }
}
