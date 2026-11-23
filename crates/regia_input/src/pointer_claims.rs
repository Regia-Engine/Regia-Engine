// SPDX-License-Identifier: MPL-2.0
//! Mouse buttons the runtime UI has claimed (`docs/specs/18-runtime-ui.md` § 4 "Input routing",
//! ADR-7976).
//!
//! A mouse press that lands on a UI widget belongs to the UI from its press to its release: the
//! action evaluation reads a claimed button as up for every binding that names it, so neither the
//! press nor the release edge reaches gameplay. `regia_ui` writes the claims in `PreUpdate`
//! before [`crate::InputSet::Evaluation`]; this crate only honors them. With no UI installed the
//! set stays empty and nothing changes.

use regia_app::prelude::*;
use regia_platform::MouseButton;

use crate::device::Mouse;

/// The mouse buttons whose current press belongs to the UI (ADR-7976).
///
/// The latch is press-to-release: a button is claimed on its press edge when the pointer is over a
/// hit-testable UI node, and it stays claimed until that button is up again, so a drag that leaves
/// the widget stays the UI's. A press that started on the world stays the game's when the pointer
/// later moves over UI. Claims are derived state: recordings and replays never store them.
///
/// # Example
///
/// ```
/// use regia_input::{Mouse, PointerClaims};
/// use regia_platform::MouseButton;
///
/// let mut claims = PointerClaims::default();
/// assert!(claims.enabled());
/// claims.claim(MouseButton::Left);
/// assert!(claims.is_claimed(MouseButton::Left));
/// // Nothing is held on this mouse, so the next tracking pass releases the claim.
/// claims.track(&Mouse::default(), false);
/// assert!(claims.claimed().is_empty());
/// ```
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct PointerClaims {
    /// Claimed buttons, in claim order, no duplicates.
    claimed: Vec<MouseButton>,
    /// Whether claims are written and honored.
    enabled: bool,
    /// Presses claimed since start-up (agent reporting).
    presses: u64,
}

impl Default for PointerClaims {
    fn default() -> Self {
        Self {
            claimed: Vec::new(),
            enabled: true,
            presses: 0,
        }
    }
}

impl PointerClaims {
    /// The claimed buttons, in claim order. Empty while claims are disabled.
    pub fn claimed(&self) -> &[MouseButton] {
        if self.enabled { &self.claimed } else { &[] }
    }

    /// Whether `button`'s current press belongs to the UI (always false while disabled).
    pub fn is_claimed(&self, button: MouseButton) -> bool {
        self.enabled && self.claimed.contains(&button)
    }

    /// Whether claims are written and honored (default true).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Turns claims on or off. Off drops every current claim, so gameplay reads the mouse as it is
    /// (the behavior before ADR-7976). A button that was claimed when this turns off reads as held
    /// on the next evaluation, which gameplay sees as a fresh press.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.claimed.clear();
        }
    }

    /// How many presses the UI has claimed since start-up.
    pub fn claimed_presses(&self) -> u64 {
        self.presses
    }

    /// Claims `button` until its release (see [`PointerClaims::track`]). Returns whether it was not
    /// already claimed. Does nothing while disabled.
    pub fn claim(&mut self, button: MouseButton) -> bool {
        if !self.enabled || self.claimed.contains(&button) {
            return false;
        }
        self.claimed.push(button);
        self.presses += 1;
        true
    }

    /// Drops the claim on `button`; returns whether it was claimed.
    pub fn release(&mut self, button: MouseButton) -> bool {
        let before = self.claimed.len();
        self.claimed.retain(|b| *b != button);
        self.claimed.len() != before
    }

    /// Whether any mouse button's press edge is this frame (the only frames a claim can start on).
    pub fn has_press_edge(mouse: &Mouse) -> bool {
        !mouse.just_pressed.is_empty()
    }

    /// One frame of the press-to-release latch, from this frame's mouse state: every claimed button
    /// that is no longer held, or was released this frame, is released; then every button whose
    /// press edge is this frame is claimed when `over_ui` (the pointer is over a hit-testable UI
    /// node). A press and release in one frame's batch (still down for that frame, see
    /// [`crate::InputSource::raw`]) is claimed for that frame and released on the next; a release
    /// and a new press in one batch is a new press, claimed only over UI.
    pub fn track(&mut self, mouse: &Mouse, over_ui: bool) {
        self.claimed
            .retain(|b| mouse.pressed(*b) && !mouse.just_released(*b));
        if !over_ui || !self.enabled {
            return;
        }
        let mut pressed: Vec<MouseButton> = mouse.just_pressed.iter().copied().collect();
        // A hash set has no stable order; claim order is reported to agents.
        pressed.sort_by_key(|b| button_order(*b));
        for button in pressed {
            self.claim(button);
        }
    }

    /// `mouse` with every claimed button up (no held bit, no edge), or `None` when nothing is
    /// claimed. The action evaluation reads this copy instead of the device.
    pub(crate) fn masked(&self, mouse: &Mouse) -> Option<Mouse> {
        if self.claimed().is_empty() {
            return None;
        }
        let mut masked = mouse.clone();
        for button in self.claimed() {
            masked.held.remove(button);
            masked.just_pressed.remove(button);
            masked.just_released.remove(button);
        }
        Some(masked)
    }
}

/// A stable sort key for mouse buttons (left, right, middle, back, forward, other by code).
fn button_order(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
        MouseButton::Other(code) => 5 + u32::from(code),
    }
}

/// The lower-case name agents see for a mouse button (`left`, `right`, `middle`, `back`,
/// `forward`, `other_<code>`).
pub fn mouse_button_name(button: MouseButton) -> String {
    match button {
        MouseButton::Left => "left".to_owned(),
        MouseButton::Right => "right".to_owned(),
        MouseButton::Middle => "middle".to_owned(),
        MouseButton::Back => "back".to_owned(),
        MouseButton::Forward => "forward".to_owned(),
        MouseButton::Other(code) => format!("other_{code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse(held: &[MouseButton], just: &[MouseButton]) -> Mouse {
        let mut mouse = Mouse::default();
        mouse.held.extend(held.iter().copied());
        mouse.just_pressed.extend(just.iter().copied());
        mouse
    }

    #[test]
    fn a_press_over_ui_is_claimed_until_its_release() {
        let mut claims = PointerClaims::default();
        claims.track(&mouse(&[MouseButton::Left], &[MouseButton::Left]), true);
        assert_eq!(claims.claimed(), &[MouseButton::Left]);
        // Held, pointer now off the UI: still claimed.
        claims.track(&mouse(&[MouseButton::Left], &[]), false);
        assert!(claims.is_claimed(MouseButton::Left));
        claims.track(&mouse(&[], &[]), false);
        assert!(claims.claimed().is_empty());
        assert_eq!(claims.claimed_presses(), 1);
    }

    #[test]
    fn a_held_world_press_is_never_claimed_over_ui() {
        let mut claims = PointerClaims::default();
        claims.track(&mouse(&[MouseButton::Left], &[MouseButton::Left]), false);
        claims.track(&mouse(&[MouseButton::Left], &[]), true);
        assert!(claims.claimed().is_empty());
    }

    #[test]
    fn a_batched_click_is_claimed_for_its_frame() {
        let mut claims = PointerClaims::default();
        // Pressed and released in one batch: an edge without the held bit.
        claims.track(&mouse(&[], &[MouseButton::Right]), true);
        assert!(claims.is_claimed(MouseButton::Right));
        let masked = claims.masked(&mouse(&[], &[MouseButton::Right]));
        assert_eq!(masked.map(|m| m.just_pressed(MouseButton::Right)), Some(false));
        claims.track(&mouse(&[], &[]), true);
        assert!(claims.claimed().is_empty());
    }

    #[test]
    fn a_release_and_new_press_in_one_batch_is_a_new_press() {
        let mut claims = PointerClaims::default();
        claims.track(&mouse(&[MouseButton::Left], &[MouseButton::Left]), true);
        let mut again = mouse(&[MouseButton::Left], &[MouseButton::Left]);
        again.just_released.insert(MouseButton::Left);
        claims.track(&again, false);
        assert!(claims.claimed().is_empty(), "the new press started off the UI");
    }

    #[test]
    fn disabled_claims_neither_claim_nor_mask() {
        let mut claims = PointerClaims::default();
        claims.claim(MouseButton::Left);
        claims.set_enabled(false);
        assert!(claims.claimed().is_empty());
        claims.track(&mouse(&[MouseButton::Left], &[MouseButton::Left]), true);
        assert!(!claims.is_claimed(MouseButton::Left));
        assert!(claims.masked(&mouse(&[MouseButton::Left], &[])).is_none());
    }

    #[test]
    fn claim_order_is_stable() {
        let mut claims = PointerClaims::default();
        claims.track(
            &mouse(
                &[MouseButton::Middle, MouseButton::Left, MouseButton::Right],
                &[MouseButton::Middle, MouseButton::Left, MouseButton::Right],
            ),
            true,
        );
        assert_eq!(
            claims.claimed(),
            &[MouseButton::Left, MouseButton::Right, MouseButton::Middle]
        );
        assert_eq!(mouse_button_name(MouseButton::Other(7)), "other_7");
    }
}
