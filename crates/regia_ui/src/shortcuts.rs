// SPDX-License-Identifier: MPL-2.0
//! Shortcut registry (ADR-273 lane 7, gap I02; ADR-374): key chords bound to named actions, with
//! scopes, display strings and conflict reporting.
//!
//! A [`UiShortcut`] binds a [`Chord`] (modifiers plus one key) to an action name inside a
//! [`ShortcutScope`]: `Global`, or a named context the game switches on and off with
//! [`UiShortcuts::set_contexts`] (a text editor panel, a pause menu). A context shortcut wins over
//! a global one on the same chord. [`dispatch_shortcuts`] reads keyboard messages each frame and
//! writes a [`ShortcutTriggered`] message for the one shortcut the chord resolves to; game and
//! widget code react to the action name. Menus show chords with [`UiShortcuts::chord_label`].
//!
//! Registering never fails because of a conflict (two actions on one chord and scope); the
//! registry reports them with [`UiShortcuts::conflicts`] and refuses to guess: an ambiguous chord
//! triggers nothing. An exact duplicate (same chord, scope and action) is rejected.

use regia_app::prelude::*;

use crate::focus::FocusState;
use crate::interaction::UiModifiers;
use crate::widgets::TextInput;

/// Why a shortcut call failed. [`ShortcutError::code`] is the lowercase protocol code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShortcutError {
    /// The chord text could not be parsed.
    #[error("bad chord {0}")]
    BadChord(String),
    /// The same chord, scope and action are already registered.
    #[error("shortcut `{0}` is already registered")]
    Duplicate(String),
    /// No shortcut has this action name.
    #[error("no shortcut has the action `{0}`")]
    UnknownAction(String),
    /// The action's shortcuts are all out of scope (their context is not active) or shadowed.
    #[error("the shortcuts of `{0}` are not active right now")]
    Inactive(String),
    /// Another action shares the chord in the same scope, so the chord triggers nothing.
    #[error("the chord of `{0}` is shared with another action in the same scope")]
    Ambiguous(String),
    /// The app has no [`UiShortcuts`] resource.
    #[error("the shortcut registry is not installed")]
    NotInstalled,
}

impl ShortcutError {
    /// The lowercase protocol code (`book/agents/ui-shortcuts.md`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadChord(_) => "bad_chord",
            Self::Duplicate(_) => "duplicate_shortcut",
            Self::UnknownAction(_) => "unknown_action",
            Self::Inactive(_) => "shortcut_inactive",
            Self::Ambiguous(_) => "shortcut_ambiguous",
            Self::NotInstalled => "shortcuts_not_installed",
        }
    }
}

/// Canonical name of a key: letters upper case, named keys in their `KeyboardInput` spelling
/// (`Escape`, `Enter`, `ArrowUp`, `F5`), the space bar as `Space`. `None` for an unknown name.
fn normalize_key(name: &str) -> Option<String> {
    let mut chars = name.chars();
    let first = chars.next()?;
    if chars.next().is_none() {
        return match first {
            ' ' => Some("Space".to_owned()),
            // A letter whose upper case is several characters (`ß` is `SS`) stays as written: the
            // canonical text must parse back to the same key.
            c if c.is_alphabetic() => {
                let mut upper = c.to_uppercase();
                match (upper.next(), upper.next()) {
                    (Some(u), None) => Some(u.to_string()),
                    _ => Some(c.to_string()),
                }
            }
            c if c.is_control() => None,
            c => Some(c.to_string()),
        };
    }
    let lower = name.to_ascii_lowercase();
    let named = match lower.as_str() {
        "esc" | "escape" => "Escape",
        "enter" | "return" => "Enter",
        "space" | "spacebar" => "Space",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" | "del" => "Delete",
        "insert" | "ins" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" | "pgup" => "PageUp",
        "pagedown" | "pgdn" => "PageDown",
        "up" | "arrowup" => "ArrowUp",
        "down" | "arrowdown" => "ArrowDown",
        "left" | "arrowleft" => "ArrowLeft",
        "right" | "arrowright" => "ArrowRight",
        _ => {
            let n = lower.strip_prefix('f')?.parse::<u8>().ok()?;
            return (1..=24).contains(&n).then(|| format!("F{n}"));
        }
    };
    Some(named.to_owned())
}

/// A key plus the modifiers held with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    /// Modifiers that must be held.
    pub modifiers: UiModifiers,
    /// The canonical key name (see [`Chord::parse`]).
    pub key: String,
}

impl Chord {
    /// Parses `Ctrl+Shift+S`, `Alt+F4`, `Escape`, `Ctrl++`. Modifier names are `Ctrl`/`Control`,
    /// `Alt`/`Option`, `Shift`, `Meta`/`Super`/`Cmd`/`Win` (any case). The key is one character or
    /// a name such as `Esc`, `Return`, `Del`, `PgUp`, `Up`, `F1` to `F24`.
    ///
    /// Letters are matched case-insensitively and Shift is an explicit modifier, so write
    /// `Shift+1`, not `!`.
    pub fn parse(text: &str) -> Result<Self, ShortcutError> {
        let bad = |why: &str| ShortcutError::BadChord(format!("`{text}`: {why}"));
        let trimmed = text.trim();
        let (head, key_text) = if trimmed == "+" {
            ("", "+")
        } else if let Some(head) = trimmed.strip_suffix("++") {
            (head, "+")
        } else if let Some((head, key)) = trimmed.rsplit_once('+') {
            (head, key)
        } else {
            ("", trimmed)
        };
        // Modifier tokens are trimmed, so the key is too (`Ctrl + S`).
        let key_text = key_text.trim();
        let mut modifiers = UiModifiers::default();
        if !head.is_empty() {
            for token in head.split('+') {
                let slot = match token.trim().to_ascii_lowercase().as_str() {
                    "ctrl" | "control" => &mut modifiers.ctrl,
                    "alt" | "option" => &mut modifiers.alt,
                    "shift" => &mut modifiers.shift,
                    "meta" | "super" | "cmd" | "command" | "win" => &mut modifiers.meta,
                    _ => return Err(bad("unknown modifier")),
                };
                if *slot {
                    return Err(bad("repeated modifier"));
                }
                *slot = true;
            }
        }
        let key = normalize_key(key_text).ok_or_else(|| bad("unknown key"))?;
        let mut probe = UiModifiers::default();
        if probe.apply_key(&key, true) {
            return Err(bad("the key cannot be a modifier"));
        }
        Ok(Self { modifiers, key })
    }

    /// The chord a key press makes with the modifiers held; `None` for a modifier key itself or a
    /// key with no canonical name.
    pub fn from_event(modifiers: UiModifiers, logical_key: &str) -> Option<Self> {
        let key = normalize_key(logical_key)?;
        let mut probe = UiModifiers::default();
        if probe.apply_key(&key, true) {
            return None;
        }
        Some(Self { modifiers, key })
    }

    /// The parseable form, modifiers in a fixed order: `Ctrl+Alt+Shift+Meta+Key`.
    pub fn canonical(&self) -> String {
        let m = self.modifiers;
        let mut out = String::new();
        for (held, name) in [
            (m.ctrl, "Ctrl"),
            (m.alt, "Alt"),
            (m.shift, "Shift"),
            (m.meta, "Meta"),
        ] {
            if held {
                out.push_str(name);
                out.push('+');
            }
        }
        out.push_str(&self.key);
        out
    }

    /// The text a menu shows: `Ctrl+Shift+S` (Meta as `Super`), or on macOS style `⌃⌥⇧⌘S`.
    pub fn display(&self, mac: bool) -> String {
        let m = self.modifiers;
        if mac {
            let mut out = String::new();
            for (held, symbol) in [(m.ctrl, "⌃"), (m.alt, "⌥"), (m.shift, "⇧"), (m.meta, "⌘")] {
                if held {
                    out.push_str(symbol);
                }
            }
            out.push_str(&self.key);
            return out;
        }
        let mut out = String::new();
        for (held, name) in [
            (m.ctrl, "Ctrl"),
            (m.alt, "Alt"),
            (m.shift, "Shift"),
            (m.meta, "Super"),
        ] {
            if held {
                out.push_str(name);
                out.push('+');
            }
        }
        out.push_str(&self.key);
        out
    }

    /// [`Chord::display`] in the style of the platform this build targets.
    pub fn platform_display(&self) -> String {
        self.display(cfg!(target_os = "macos"))
    }
}

/// Where a shortcut is live.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ShortcutScope {
    /// Always live, unless a context shortcut on the same chord is active.
    #[default]
    Global,
    /// Live while the named context is active ([`UiShortcuts::set_contexts`]).
    Context(String),
}

impl ShortcutScope {
    /// The protocol name: `global`, or the context name.
    pub fn name(&self) -> &str {
        match self {
            Self::Global => "global",
            Self::Context(name) => name,
        }
    }

    /// Parses a protocol name: `global` (or empty) is [`ShortcutScope::Global`], anything else a context.
    pub fn parse(name: &str) -> Self {
        let name = name.trim();
        if name.is_empty() || name.eq_ignore_ascii_case("global") {
            Self::Global
        } else {
            Self::Context(name.to_owned())
        }
    }
}

/// One binding: a chord that triggers an action in a scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiShortcut {
    /// The keys.
    pub chord: Chord,
    /// The action name game code listens for.
    pub action: String,
    /// Where it is live.
    pub scope: ShortcutScope,
    /// Human-readable name for menus and the agent listing (may be empty).
    pub label: String,
}

impl UiShortcut {
    /// A global shortcut with no label.
    pub fn new(chord: Chord, action: impl Into<String>) -> Self {
        Self {
            chord,
            action: action.into(),
            scope: ShortcutScope::Global,
            label: String::new(),
        }
    }

    /// Sets the scope.
    #[must_use]
    pub fn in_scope(mut self, scope: ShortcutScope) -> Self {
        self.scope = scope;
        self
    }

    /// Sets the label.
    #[must_use]
    pub fn labeled(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }
}

/// How two shortcuts on one chord collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// Same chord, same scope, different actions: the chord triggers nothing in that scope.
    SameScope,
    /// A context shortcut hides a global one while the context is active. Usually intended.
    Shadow,
    /// Same chord in two different contexts; ambiguous only when both are active.
    Overlap,
}

impl ConflictKind {
    /// The protocol name.
    pub fn name(self) -> &'static str {
        match self {
            Self::SameScope => "same_scope",
            Self::Shadow => "shadow",
            Self::Overlap => "overlap",
        }
    }
}

/// Two shortcuts that share a chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutConflict {
    /// The shared chord.
    pub chord: Chord,
    /// How they collide.
    pub kind: ConflictKind,
    /// The two actions.
    pub actions: [String; 2],
    /// The two scopes, in the same order as `actions`.
    pub scopes: [ShortcutScope; 2],
}

/// What a chord resolves to right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution<'a> {
    /// No live shortcut has this chord.
    None,
    /// Exactly one live shortcut.
    One(&'a UiShortcut),
    /// Several shortcuts in the winning tier: nothing triggers.
    Conflict(Vec<&'a UiShortcut>),
}

/// A shortcut fired (key press or [`UiShortcuts::activation`]).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ShortcutTriggered {
    /// The action name.
    pub action: String,
    /// The chord that fired it.
    pub chord: Chord,
    /// The scope of the shortcut that fired.
    pub scope: ShortcutScope,
}

/// The registry: shortcuts, active contexts and the tracked modifier state.
#[derive(Resource, Debug, Default)]
pub struct UiShortcuts {
    shortcuts: Vec<UiShortcut>,
    contexts: Vec<String>,
    modifiers: UiModifiers,
}

impl UiShortcuts {
    /// Adds a shortcut. Conflicts with other actions are allowed (see [`UiShortcuts::conflicts`]);
    /// an exact duplicate is an error.
    pub fn register(&mut self, shortcut: UiShortcut) -> Result<(), ShortcutError> {
        let duplicate = self
            .shortcuts
            .iter()
            .any(|s| s.chord == shortcut.chord && s.scope == shortcut.scope && s.action == shortcut.action);
        if duplicate {
            return Err(ShortcutError::Duplicate(format!(
                "{} -> {}",
                shortcut.chord.canonical(),
                shortcut.action
            )));
        }
        self.shortcuts.push(shortcut);
        Ok(())
    }

    /// Removes every shortcut bound to `action`; returns how many.
    pub fn unregister(&mut self, action: &str) -> usize {
        let before = self.shortcuts.len();
        self.shortcuts.retain(|s| s.action != action);
        before - self.shortcuts.len()
    }

    /// Every registered shortcut, in registration order.
    pub fn shortcuts(&self) -> &[UiShortcut] {
        &self.shortcuts
    }

    /// Replaces the set of active contexts.
    pub fn set_contexts<I, S>(&mut self, contexts: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.contexts = contexts.into_iter().map(Into::into).collect();
    }

    /// The active contexts.
    pub fn contexts(&self) -> &[String] {
        &self.contexts
    }

    /// Modifiers the dispatch system currently sees held.
    pub fn modifiers(&self) -> UiModifiers {
        self.modifiers
    }

    fn matching(&self, chord: &Chord, in_context: bool) -> Vec<&UiShortcut> {
        self.shortcuts
            .iter()
            .filter(|s| {
                s.chord == *chord
                    && match &s.scope {
                        ShortcutScope::Global => !in_context,
                        ShortcutScope::Context(name) => in_context && self.contexts.iter().any(|c| c == name),
                    }
            })
            .collect()
    }

    /// What `chord` triggers now: active context shortcuts win over global ones, and several
    /// shortcuts in the winning tier are a [`Resolution::Conflict`].
    pub fn resolve(&self, chord: &Chord) -> Resolution<'_> {
        let mut hits = self.matching(chord, true);
        if hits.is_empty() {
            hits = self.matching(chord, false);
        }
        match hits.len() {
            0 => Resolution::None,
            1 => Resolution::One(hits[0]),
            _ => Resolution::Conflict(hits),
        }
    }

    /// Every pair of shortcuts that share a chord (regardless of the active contexts).
    pub fn conflicts(&self) -> Vec<ShortcutConflict> {
        let mut out = Vec::new();
        for (i, a) in self.shortcuts.iter().enumerate() {
            for b in self.shortcuts.iter().skip(i + 1) {
                // A conflict is between two actions (`book/agents/ui-shortcuts.md`): one action bound on
                // the same chord in two scopes does not collide with itself.
                if a.chord != b.chord || a.action == b.action {
                    continue;
                }
                let kind = if a.scope == b.scope {
                    ConflictKind::SameScope
                } else if a.scope == ShortcutScope::Global || b.scope == ShortcutScope::Global {
                    ConflictKind::Shadow
                } else {
                    ConflictKind::Overlap
                };
                out.push(ShortcutConflict {
                    chord: a.chord.clone(),
                    kind,
                    actions: [a.action.clone(), b.action.clone()],
                    scopes: [a.scope.clone(), b.scope.clone()],
                });
            }
        }
        out
    }

    /// The menu text of the chord bound to `action` (platform style), preferring a shortcut that is
    /// live now, then a global one, then the first registered. `None` when nothing is bound.
    pub fn chord_label(&self, action: &str) -> Option<String> {
        let mut bound = self.shortcuts.iter().filter(|s| s.action == action);
        let first = bound.next()?;
        let live = |s: &UiShortcut| match &s.scope {
            ShortcutScope::Global => true,
            ShortcutScope::Context(name) => self.contexts.iter().any(|c| c == name),
        };
        let chosen = std::iter::once(first)
            .chain(bound)
            .min_by_key(|s| (!live(s), s.scope != ShortcutScope::Global))
            .unwrap_or(first);
        Some(chosen.chord.platform_display())
    }

    /// What pressing the keys of `action` would fire right now, without pressing anything.
    pub fn activation(&self, action: &str) -> Result<ShortcutTriggered, ShortcutError> {
        let mut known = false;
        let mut ambiguous = false;
        for s in self.shortcuts.iter().filter(|s| s.action == action) {
            known = true;
            match self.resolve(&s.chord) {
                Resolution::One(hit) if hit.action == action => {
                    return Ok(ShortcutTriggered {
                        action: hit.action.clone(),
                        chord: hit.chord.clone(),
                        scope: hit.scope.clone(),
                    });
                }
                Resolution::Conflict(hits) if hits.iter().any(|h| h.action == action) => ambiguous = true,
                _ => {}
            }
        }
        if !known {
            Err(ShortcutError::UnknownAction(action.to_owned()))
        } else if ambiguous {
            Err(ShortcutError::Ambiguous(action.to_owned()))
        } else {
            Err(ShortcutError::Inactive(action.to_owned()))
        }
    }
}

/// Fires `action` as if its keys were pressed: checks scope and conflicts like a key press does
/// and writes a [`ShortcutTriggered`] message. The message reaches readers on their next update.
pub fn trigger_action(world: &mut World, action: &str) -> Result<ShortcutTriggered, ShortcutError> {
    let triggered = world
        .get_resource::<UiShortcuts>()
        .ok_or(ShortcutError::NotInstalled)?
        .activation(action)?;
    let _ = world.write_message(triggered.clone());
    Ok(triggered)
}

/// `Update`: turns key presses into [`ShortcutTriggered`] messages. Modifier keys only update the
/// tracked state; auto-repeat is ignored. While a [`TextInput`] has focus, only chords with Ctrl,
/// Alt or Meta fire, so typing a letter never triggers a bare-key shortcut.
pub fn dispatch_shortcuts(
    mut shortcuts: ResMut<UiShortcuts>,
    mut keys: MessageReader<regia_platform::KeyboardInput>,
    mut out: MessageWriter<ShortcutTriggered>,
    focus: Res<FocusState>,
    text_inputs: Query<(), With<TextInput>>,
) {
    let _span = tracing::debug_span!("dispatch_shortcuts").entered();
    let typing = focus.focused.is_some_and(|e| text_inputs.contains(e));
    for e in keys.read() {
        let down = e.state == regia_platform::ButtonState::Pressed;
        if shortcuts.modifiers.apply_key(&e.logical_key, down) || !down || e.repeat {
            continue;
        }
        let modifiers = shortcuts.modifiers;
        if typing && !(modifiers.ctrl || modifiers.alt || modifiers.meta) {
            continue;
        }
        let Some(chord) = Chord::from_event(modifiers, &e.logical_key) else {
            continue;
        };
        if let Resolution::One(hit) = shortcuts.resolve(&chord) {
            out.write(ShortcutTriggered {
                action: hit.action.clone(),
                chord,
                scope: hit.scope.clone(),
            });
        }
    }
}

/// Installs [`UiShortcuts`], the [`ShortcutTriggered`] message and [`dispatch_shortcuts`].
/// [`crate::UiPlugin`] adds it.
pub struct UiShortcutsPlugin;

impl Plugin for UiShortcutsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<UiShortcuts>()
            .add_message::<ShortcutTriggered>()
            .add_systems(Update, dispatch_shortcuts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(text: &str) -> Chord {
        Chord::parse(text).expect("chord parses")
    }

    fn shortcut(text: &str, action: &str) -> UiShortcut {
        UiShortcut::new(chord(text), action)
    }

    fn in_context(text: &str, action: &str, context: &str) -> UiShortcut {
        shortcut(text, action).in_scope(ShortcutScope::Context(context.to_owned()))
    }

    #[test]
    fn chords_round_trip_through_their_canonical_form() {
        for (input, canonical) in [
            ("ctrl+s", "Ctrl+S"),
            ("Shift+Control+z", "Ctrl+Shift+Z"),
            ("alt+f4", "Alt+F4"),
            ("Esc", "Escape"),
            ("cmd+return", "Meta+Enter"),
            ("Ctrl+PgUp", "Ctrl+PageUp"),
            ("Ctrl++", "Ctrl++"),
            ("+", "+"),
            ("Ctrl+Space", "Ctrl+Space"),
            ("Ctrl+Alt+Shift+Meta+Delete", "Ctrl+Alt+Shift+Meta+Delete"),
        ] {
            let parsed = chord(input);
            assert_eq!(parsed.canonical(), canonical, "{input}");
            assert_eq!(chord(canonical), parsed, "{canonical} reparses");
        }
    }

    #[test]
    fn bad_chords_are_rejected_with_a_code() {
        for input in [
            "",
            "Ctrl+",
            "Hyper+S",
            "Ctrl+Ctrl+S",
            "Ctrl+Shift",
            "F25",
            "Ctrl+Foo",
            "++S",
        ] {
            let err = Chord::parse(input).expect_err(input);
            assert_eq!(err.code(), "bad_chord", "{input}");
        }
    }

    #[test]
    fn display_strings_follow_the_platform_style() {
        let c = chord("Ctrl+Alt+Shift+Meta+S");
        assert_eq!(c.display(false), "Ctrl+Alt+Shift+Super+S");
        assert_eq!(c.display(true), "⌃⌥⇧⌘S");
        assert_eq!(chord("F5").display(false), "F5");
    }

    #[test]
    fn events_make_chords_and_modifier_keys_make_none() {
        let held = UiModifiers {
            ctrl: true,
            ..UiModifiers::default()
        };
        assert_eq!(Chord::from_event(held, "s"), Some(chord("Ctrl+S")));
        assert_eq!(Chord::from_event(held, "S"), Some(chord("Ctrl+S")));
        assert_eq!(Chord::from_event(held, " "), Some(chord("Ctrl+Space")));
        assert_eq!(Chord::from_event(held, "Shift"), None);
        assert_eq!(Chord::from_event(held, "Unidentified"), None);
    }

    #[test]
    fn exact_duplicates_are_refused_but_conflicts_are_not() {
        let mut reg = UiShortcuts::default();
        reg.register(shortcut("Ctrl+S", "save")).expect("first");
        let err = reg.register(shortcut("ctrl+s", "save")).expect_err("duplicate");
        assert_eq!(err.code(), "duplicate_shortcut");
        reg.register(shortcut("Ctrl+S", "snap"))
            .expect("conflict is allowed");
        reg.register(in_context("Ctrl+S", "save_as_text", "editor"))
            .expect("context");
        assert_eq!(reg.shortcuts().len(), 3);
        // The same action on the same chord in another scope is not a duplicate.
        reg.register(in_context("Ctrl+S", "save", "menu"))
            .expect("other scope");
    }

    #[test]
    fn contexts_win_over_global_and_ambiguity_triggers_nothing() {
        let mut reg = UiShortcuts::default();
        reg.register(shortcut("Ctrl+F", "find_global")).expect("global");
        reg.register(in_context("Ctrl+F", "find_in_editor", "editor"))
            .expect("context");
        reg.register(shortcut("F1", "help")).expect("help");
        reg.register(shortcut("F2", "rename")).expect("rename a");
        reg.register(shortcut("F2", "rename_all")).expect("rename b");
        let f = chord("Ctrl+F");
        assert!(matches!(reg.resolve(&f), Resolution::One(s) if s.action == "find_global"));
        reg.set_contexts(["editor"]);
        assert!(matches!(reg.resolve(&f), Resolution::One(s) if s.action == "find_in_editor"));
        reg.set_contexts(["other"]);
        assert!(matches!(reg.resolve(&f), Resolution::One(s) if s.action == "find_global"));
        assert!(matches!(reg.resolve(&chord("F1")), Resolution::One(s) if s.action == "help"));
        assert!(matches!(reg.resolve(&chord("F9")), Resolution::None));
        match reg.resolve(&chord("F2")) {
            Resolution::Conflict(hits) => assert_eq!(hits.len(), 2),
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    #[test]
    fn conflicts_name_kind_actions_and_scopes() {
        let mut reg = UiShortcuts::default();
        reg.register(shortcut("F2", "rename")).expect("a");
        reg.register(shortcut("F2", "rename_all")).expect("b");
        reg.register(shortcut("Ctrl+F", "find")).expect("c");
        reg.register(in_context("Ctrl+F", "find_in_editor", "editor"))
            .expect("d");
        reg.register(in_context("Ctrl+F", "find_in_log", "log"))
            .expect("e");
        reg.register(shortcut("F3", "next")).expect("no conflict");
        let found = reg.conflicts();
        let kinds: Vec<&str> = found.iter().map(|c| c.kind.name()).collect();
        assert_eq!(kinds, ["same_scope", "shadow", "shadow", "overlap"]);
        assert_eq!(found[0].actions, ["rename".to_owned(), "rename_all".to_owned()]);
        assert_eq!(found[0].scopes, [ShortcutScope::Global, ShortcutScope::Global]);
        assert_eq!(found[3].scopes[0].name(), "editor");
        assert_eq!(found[3].scopes[1].name(), "log");
    }

    #[test]
    fn labels_prefer_a_live_shortcut_and_none_when_unbound() {
        let mut reg = UiShortcuts::default();
        reg.register(in_context("Ctrl+K", "palette", "editor"))
            .expect("context");
        reg.register(shortcut("F1", "palette")).expect("global");
        let expected_f1 = chord("F1").platform_display();
        // Context inactive: the global binding is the live one.
        assert_eq!(reg.chord_label("palette"), Some(expected_f1));
        reg.set_contexts(["editor"]);
        // Both live: the global one still sorts first.
        assert_eq!(reg.chord_label("palette"), Some(chord("F1").platform_display()));
        assert_eq!(reg.chord_label("missing"), None);
    }

    #[test]
    fn activation_checks_scope_and_conflicts() {
        let mut reg = UiShortcuts::default();
        reg.register(in_context("Ctrl+B", "bold", "editor"))
            .expect("bold");
        reg.register(shortcut("F2", "rename")).expect("a");
        reg.register(shortcut("F2", "rename_all")).expect("b");
        reg.register(shortcut("F5", "run")).expect("run");
        assert_eq!(
            reg.activation("nope").expect_err("unknown").code(),
            "unknown_action"
        );
        assert_eq!(
            reg.activation("bold").expect_err("inactive").code(),
            "shortcut_inactive"
        );
        assert_eq!(
            reg.activation("rename").expect_err("ambiguous").code(),
            "shortcut_ambiguous"
        );
        assert_eq!(reg.activation("run").expect("run").chord, chord("F5"));
        reg.set_contexts(["editor"]);
        let bold = reg.activation("bold").expect("live");
        assert_eq!(bold.scope.name(), "editor");
        assert_eq!(reg.unregister("bold"), 1);
        assert_eq!(reg.activation("bold").expect_err("gone").code(), "unknown_action");
    }

    #[test]
    fn scope_names_round_trip() {
        assert_eq!(ShortcutScope::parse("Global"), ShortcutScope::Global);
        assert_eq!(ShortcutScope::parse(""), ShortcutScope::Global);
        assert_eq!(
            ShortcutScope::parse("editor"),
            ShortcutScope::Context("editor".to_owned())
        );
        assert_eq!(ShortcutScope::Global.name(), "global");
    }
}
