// SPDX-License-Identifier: MPL-2.0
//! On-screen keyboard and IME follow text focus (ADR-273 I11, ADR-1842).
//!
//! The platform layer already shows the Android soft keyboard and enables the desktop IME when the
//! primary window's `ime_enabled` flag turns on (`regia_platform::services::sync_soft_input`,
//! `apply_window_changes`). Nothing in the UI set that flag, so a focused text field never raised the
//! keyboard. [`sync_on_screen_keyboard`] closes the gap: while the focused node is a
//! [`TextInput`], the flag is on; when focus leaves, it is off. [`KeyboardPolicy`] lets a game keep
//! it always off (a console build with its own keyboard UI) or always on.
//!
//! The agent surface is `ui.keyboard` (`book/agents/ui-keyboard.md`). The raising of a real keyboard
//! needs a device; the logic is tested through the flag.

use regia_app::prelude::*;
use regia_platform::{PrimaryWindow, Window};

use crate::focus::FocusState;
use crate::widgets::TextInput;

/// When the on-screen keyboard (and the IME) should be active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardMode {
    /// Follow focus: active while a text field has focus.
    #[default]
    Auto,
    /// Never raised by the UI.
    Off,
    /// Always active.
    On,
}

impl KeyboardMode {
    /// Lowercase name for the agent protocol.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
            Self::On => "on",
        }
    }

    /// The mode called `name`.
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Auto, Self::Off, Self::On]
            .into_iter()
            .find(|m| m.name() == name)
    }
}

/// The keyboard policy and what the UI last asked for.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyboardPolicy {
    /// The mode.
    pub mode: KeyboardMode,
    /// Whether the UI currently wants the keyboard (what it wrote to the window).
    pub wanted: bool,
}

/// Whether the keyboard should be active for `mode` when a text field has focus (`text_focus`).
pub fn wants_keyboard(mode: KeyboardMode, text_focus: bool) -> bool {
    match mode {
        KeyboardMode::Auto => text_focus,
        KeyboardMode::Off => false,
        KeyboardMode::On => true,
    }
}

/// `PostUpdate`: writes `ime_enabled` on the primary window when the wanted state changes.
pub fn sync_on_screen_keyboard(
    mut policy: ResMut<KeyboardPolicy>,
    focus: Res<FocusState>,
    inputs: Query<&TextInput>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
) {
    // A read-only field refuses every edit, so raising the IME would slide the soft keyboard
    // over the game for nothing (its commits are dropped by the read-only guard).
    let text_focus = focus
        .focused
        .is_some_and(|e| inputs.get(e).is_ok_and(|i| !i.read_only));
    let wanted = wants_keyboard(policy.mode, text_focus);
    if policy.wanted != wanted {
        policy.wanted = wanted;
    }
    for mut window in &mut windows {
        // Compare first: assigning marks the window changed and costs a platform call.
        if window.ime_enabled != wanted {
            window.ime_enabled = wanted;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_table() {
        assert!(wants_keyboard(KeyboardMode::Auto, true));
        assert!(!wants_keyboard(KeyboardMode::Auto, false));
        assert!(!wants_keyboard(KeyboardMode::Off, true));
        assert!(wants_keyboard(KeyboardMode::On, false));
        for m in [KeyboardMode::Auto, KeyboardMode::Off, KeyboardMode::On] {
            assert_eq!(KeyboardMode::parse(m.name()), Some(m));
        }
        assert_eq!(KeyboardMode::parse("x"), None);
    }

    fn app() -> (App, Entity) {
        let mut app = App::new();
        app.init_resource::<KeyboardPolicy>()
            .init_resource::<FocusState>()
            .add_systems(Update, sync_on_screen_keyboard);
        let window = app.world_mut().spawn((Window::default(), PrimaryWindow)).id();
        (app, window)
    }

    #[test]
    fn focus_on_a_text_field_raises_the_flag_and_leaving_lowers_it() {
        let (mut app, window) = app();
        let field = app.world_mut().spawn(TextInput::default()).id();
        let button = app.world_mut().spawn_empty().id();
        app.update();
        assert!(!app.world().get::<Window>(window).unwrap().ime_enabled);
        app.world_mut().resource_mut::<FocusState>().focused = Some(field);
        app.update();
        assert!(app.world().get::<Window>(window).unwrap().ime_enabled);
        assert!(app.world().resource::<KeyboardPolicy>().wanted);
        app.world_mut().resource_mut::<FocusState>().focused = Some(button);
        app.update();
        assert!(!app.world().get::<Window>(window).unwrap().ime_enabled);
    }

    #[test]
    fn focus_on_a_read_only_field_does_not_raise_the_flag() {
        let (mut app, window) = app();
        let mut ro = TextInput::default();
        ro.read_only = true;
        let field = app.world_mut().spawn(ro).id();
        app.world_mut().resource_mut::<FocusState>().focused = Some(field);
        app.update();
        assert!(
            !app.world().get::<Window>(window).unwrap().ime_enabled,
            "a read-only field refuses edits, so the IME must stay off"
        );
        assert!(!app.world().resource::<KeyboardPolicy>().wanted);
        // KeyboardMode::On forces the keyboard regardless of focus, as before.
        app.world_mut().resource_mut::<KeyboardPolicy>().mode = KeyboardMode::On;
        app.update();
        assert!(app.world().get::<Window>(window).unwrap().ime_enabled);
    }

    #[test]
    fn off_mode_never_raises_and_on_mode_always_does() {
        let (mut app, window) = app();
        let field = app.world_mut().spawn(TextInput::default()).id();
        app.world_mut().resource_mut::<FocusState>().focused = Some(field);
        app.world_mut().resource_mut::<KeyboardPolicy>().mode = KeyboardMode::Off;
        app.update();
        assert!(!app.world().get::<Window>(window).unwrap().ime_enabled);
        app.world_mut().resource_mut::<KeyboardPolicy>().mode = KeyboardMode::On;
        app.world_mut().resource_mut::<FocusState>().focused = None;
        app.update();
        assert!(app.world().get::<Window>(window).unwrap().ime_enabled);
    }
}
