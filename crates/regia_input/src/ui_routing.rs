// SPDX-License-Identifier: MPL-2.0
//! UI routing and typed actions (`docs/specs/11` § 7, `docs/specs/18` § 4, ADR-7070).
//!
//! * [`UiPointerGate`]: while the runtime UI consumes the pointer (a press on a button, a hover over
//!   a panel), mouse-button bindings of gameplay contexts read as released, so a click on a button
//!   does not also fire. `regia_ui` writes it each frame; contexts named in
//!   [`UiPointerGate::exempt_contexts`] (the [`MENU_CONTEXT`] by default) still see the pointer.
//! * The [`MENU_CONTEXT`] and its actions ([`MENU_UP`] ... [`MENU_NAVIGATE`]): `regia_ui` drives
//!   focus navigation from them while the context is enabled, so rebinding them rebinds UI
//!   navigation. [`InputConfig::with_menu_context`] declares the defaults.
//! * [`InputAction`]: a typed action enum, usually `#[derive(InputAction)]`, instead of strings.

use regia_app::prelude::*;
use regia_platform::GamepadButton;

use crate::actions::Actions;
use crate::config::{ActionKind, Binding, InputConfig, InputContext, InputSource, Stick};

/// The input context UI navigation reads (`docs/specs/18` § 4).
pub const MENU_CONTEXT: &str = "Menu";
/// Menu action: move focus up.
pub const MENU_UP: &str = "Up";
/// Menu action: move focus down.
pub const MENU_DOWN: &str = "Down";
/// Menu action: move focus left (or lower a focused slider).
pub const MENU_LEFT: &str = "Left";
/// Menu action: move focus right (or raise a focused slider).
pub const MENU_RIGHT: &str = "Right";
/// Menu action: next node in the focus order (Tab). With Shift held it is "previous".
pub const MENU_NEXT: &str = "Next";
/// Menu action: previous node in the focus order.
pub const MENU_PREV: &str = "Prev";
/// Menu action: activate the focused node.
pub const MENU_SUBMIT: &str = "Submit";
/// Menu action: cancel (close the active scope).
pub const MENU_CANCEL: &str = "Cancel";
/// Menu action (`Axis2D`): a stick that moves focus in its dominant direction, with repeat.
pub const MENU_NAVIGATE: &str = "Navigate";

/// The button actions of the [`MENU_CONTEXT`], in declaration order.
pub const MENU_BUTTON_ACTIONS: [&str; 8] = [
    MENU_UP,
    MENU_DOWN,
    MENU_LEFT,
    MENU_RIGHT,
    MENU_NEXT,
    MENU_PREV,
    MENU_SUBMIT,
    MENU_CANCEL,
];

/// Gameplay gating by the runtime UI (`docs/specs/18` § 4 input routing).
///
/// While `ui_consumed` is set, every [`InputSource::MouseButton`] binding of a context that is not
/// in `exempt_contexts` contributes nothing, as if the button were up; a held action ends the same
/// way as when its button is released. `regia_ui` sets the flag at the end of its interaction pass
/// (`Update`) from `UiPointer::consumed_pointer`, so evaluation reads the previous frame's hit:
/// a press on a button the cursor already hovers is hidden, and a press stays hidden until it is
/// released (the UI keeps the pointer while its press lasts). Without `regia_ui` the flag stays
/// false and nothing changes. While [`crate::PointerClaims`] is enabled, `regia_ui` sets the flag
/// only while a UI press lasts, not on hover, so a world press that moves over UI stays the
/// game's (ADR-8786).
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct UiPointerGate {
    /// The UI consumed the pointer on the last interaction pass.
    pub ui_consumed: bool,
    /// Contexts that keep seeing the pointer while the UI consumes it (default: [`MENU_CONTEXT`]).
    pub exempt_contexts: Vec<String>,
}

impl Default for UiPointerGate {
    fn default() -> Self {
        Self {
            ui_consumed: false,
            exempt_contexts: vec![MENU_CONTEXT.to_owned()],
        }
    }
}

impl UiPointerGate {
    /// Whether `context` keeps seeing the pointer while the UI consumes it.
    pub fn exempt(&self, context: &str) -> bool {
        self.exempt_contexts.iter().any(|c| c == context)
    }

    /// Whether `source`, bound in `context`, is hidden this evaluation.
    pub fn hides(&self, context: &str, source: &InputSource) -> bool {
        self.ui_consumed && matches!(source, InputSource::MouseButton(_)) && !self.exempt(context)
    }
}

impl InputConfig {
    /// Declares the [`MENU_CONTEXT`] UI navigation reads, with its default bindings, unless the
    /// config already has a context of that name (the project's own bindings win). The button
    /// actions are declared as `Button` and [`MENU_NAVIGATE`] as `Axis2D` where not declared.
    ///
    /// Defaults: arrows and the D-pad move, Tab and the right shoulder go to the next node, the left
    /// shoulder to the previous one, Enter, numpad Enter, Space and South submit, Escape and East
    /// cancel, the left stick navigates. The context does not consume lower contexts; it is
    /// evaluated only while enabled (`InputContexts::push(player, MENU_CONTEXT)`).
    ///
    /// ```
    /// use regia_input::prelude::*;
    /// use regia_input::{MENU_CONTEXT, MENU_UP};
    ///
    /// let config = InputConfig::new().with_menu_context();
    /// assert_eq!(config.kind(MENU_UP), Some(ActionKind::Button));
    /// assert!(config.contexts.contains_key(MENU_CONTEXT));
    /// ```
    #[must_use]
    pub fn with_menu_context(mut self) -> Self {
        for name in MENU_BUTTON_ACTIONS {
            self.actions.entry(name.to_owned()).or_insert(ActionKind::Button);
        }
        self.actions
            .entry(MENU_NAVIGATE.to_owned())
            .or_insert(ActionKind::Axis2D);
        if self.contexts.contains_key(MENU_CONTEXT) {
            return self;
        }
        let key = InputSource::key;
        let pad = InputSource::gamepad_button;
        let defaults: [(&str, InputSource); 18] = [
            (MENU_UP, key("ArrowUp")),
            (MENU_UP, pad(GamepadButton::DPadUp)),
            (MENU_DOWN, key("ArrowDown")),
            (MENU_DOWN, pad(GamepadButton::DPadDown)),
            (MENU_LEFT, key("ArrowLeft")),
            (MENU_LEFT, pad(GamepadButton::DPadLeft)),
            (MENU_RIGHT, key("ArrowRight")),
            (MENU_RIGHT, pad(GamepadButton::DPadRight)),
            (MENU_NEXT, key("Tab")),
            (MENU_NEXT, pad(GamepadButton::RightShoulder)),
            (MENU_PREV, pad(GamepadButton::LeftShoulder)),
            (MENU_SUBMIT, key("Enter")),
            (MENU_SUBMIT, key("NumpadEnter")),
            (MENU_SUBMIT, key("Space")),
            (MENU_SUBMIT, pad(GamepadButton::South)),
            (MENU_CANCEL, key("Escape")),
            (MENU_CANCEL, pad(GamepadButton::East)),
            (MENU_NAVIGATE, InputSource::gamepad_stick(Stick::Left)),
        ];
        let mut context = InputContext::new(100);
        for (action, source) in defaults {
            context.bindings.push(Binding::new(action, source));
        }
        self.contexts.insert(MENU_CONTEXT.to_owned(), context);
        self
    }

    /// Declares every action of `A` with its kind (an existing declaration is replaced).
    #[must_use]
    pub fn declare<A: InputAction>(mut self) -> Self {
        for action in A::ALL {
            self.actions.insert(action.name().to_owned(), action.kind());
        }
        self
    }
}

/// A typed set of actions (`docs/specs/11` § 7): one enum variant per action, so a typo is a
/// compile error instead of an action that never fires. Derive it with `#[derive(InputAction)]` on
/// a fieldless enum: the name is the variant's name unless `#[action(name = "...")]` renames it, and
/// the kind is `Button` unless `#[action(kind = Axis2D)]` (any [`ActionKind`] variant) says otherwise.
///
/// ```
/// use regia_input::prelude::*;
/// use regia_input::InputAction;
///
/// #[derive(InputAction, Clone, Copy, PartialEq, Debug)]
/// enum Act {
///     Jump,
///     #[action(kind = Axis2D)]
///     Move,
/// }
///
/// assert_eq!(Act::Move.name(), "Move");
/// let config = InputConfig::new().declare::<Act>();
/// assert_eq!(config.kind("Move"), Some(ActionKind::Axis2D));
/// assert!(!Actions::default().pressed_action(Act::Jump));
/// ```
pub trait InputAction: Copy + 'static {
    /// Every action, in declaration order.
    const ALL: &'static [Self];

    /// The action's name in [`InputConfig`] and [`Actions`].
    fn name(self) -> &'static str;

    /// The action's value kind.
    fn kind(self) -> ActionKind;

    /// The action named `name`, if any.
    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|a| a.name() == name)
    }
}

impl Actions {
    /// [`Actions::pressed`] for a typed action.
    pub fn pressed_action<A: InputAction>(&self, action: A) -> bool {
        self.pressed(action.name())
    }

    /// [`Actions::just_pressed`] for a typed action.
    pub fn just_pressed_action<A: InputAction>(&self, action: A) -> bool {
        self.just_pressed(action.name())
    }

    /// [`Actions::just_pressed_this_frame`] for a typed action.
    pub fn just_pressed_this_frame_action<A: InputAction>(&self, action: A) -> bool {
        self.just_pressed_this_frame(action.name())
    }

    /// [`Actions::axis1d`] for a typed action.
    pub fn axis1d_action<A: InputAction>(&self, action: A) -> f32 {
        self.axis1d(action.name())
    }

    /// [`Actions::axis2d`] for a typed action.
    pub fn axis2d_action<A: InputAction>(&self, action: A) -> regia_core::math::Vec2 {
        self.axis2d(action.name())
    }
}

#[cfg(test)]
mod tests {
    use regia_platform::MouseButton;

    use super::*;

    #[test]
    fn the_gate_hides_only_mouse_buttons_outside_exempt_contexts() {
        let mut gate = UiPointerGate::default();
        let click = InputSource::mouse_button(MouseButton::Left);
        assert!(
            !gate.hides("Gameplay", &click),
            "nothing hidden while the UI is idle"
        );
        gate.ui_consumed = true;
        assert!(gate.hides("Gameplay", &click));
        assert!(
            !gate.hides(MENU_CONTEXT, &click),
            "the menu context still sees it"
        );
        assert!(!gate.hides("Gameplay", &InputSource::key("Space")), "keys pass");
        assert!(
            !gate.hides("Gameplay", &InputSource::MouseDelta),
            "mouse look passes"
        );
    }

    #[test]
    fn a_project_menu_context_is_kept() {
        let mine = InputContext::consuming(7).with_binding(Binding::new(MENU_UP, InputSource::key("KeyW")));
        let mut config = InputConfig::new();
        config.contexts.insert(MENU_CONTEXT.to_owned(), mine.clone());
        let config = config.with_menu_context();
        assert_eq!(config.contexts.get(MENU_CONTEXT), Some(&mine));
        assert_eq!(config.kind(MENU_NAVIGATE), Some(ActionKind::Axis2D));
    }
}
