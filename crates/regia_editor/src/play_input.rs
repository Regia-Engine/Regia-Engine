// SPDX-License-Identifier: MPL-2.0
//! Play input capture (`docs/specs/19-editor.md` § 4, ADR-7205): a click on a possessed play
//! viewport captures the mouse (the cursor is confined and hidden, as the game expects), and
//! **Shift+F1** releases it so the editor's panels are reachable again without stopping play.
//! Stopping play, losing window focus or un-possessing the view release it too.
//!
//! The decision is [`on_event`], a pure function over [`PlayInputCapture`]; the windowed editor's
//! winit loop applies its answer to the window (`set_cursor_grab`, `set_cursor_visible`). Tests
//! drive [`on_event`] and [`is_release_key`] directly, never a window.

use crate::engine::ecs::prelude::*;

/// Whether the play viewport holds the mouse.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayInputCapture {
    /// The cursor is grabbed by the play viewport.
    pub captured: bool,
}

/// Set in the drawn world by a click on a possessed play viewport; the editor loop takes it and
/// feeds [`CaptureEvent::ViewportClicked`] to [`on_event`].
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct CaptureClick;

/// What happened that may change the capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureEvent {
    /// The possessed play viewport was clicked.
    ViewportClicked,
    /// Shift+F1 was pressed.
    ReleaseKey,
    /// Play stopped, the window lost focus or the view stopped being possessed.
    Lost,
}

/// Applies `event` to `capture`. Returns the new grab state when it changed (the window must be
/// grabbed or released), `None` when nothing changes. A click captures only while a session runs.
pub fn on_event(capture: &mut PlayInputCapture, session_active: bool, event: CaptureEvent) -> Option<bool> {
    let wanted = match event {
        CaptureEvent::ViewportClicked => session_active,
        CaptureEvent::ReleaseKey | CaptureEvent::Lost => false,
    };
    if wanted == capture.captured {
        return None;
    }
    capture.captured = wanted;
    Some(wanted)
}

/// Whether a pressed key is the release shortcut: F1 with Shift held (and no Ctrl or Alt, which
/// other bindings own).
#[must_use]
pub fn is_release_key(key_is_f1: bool, shift: bool, ctrl: bool, alt: bool) -> bool {
    key_is_f1 && shift && !ctrl && !alt
}

/// The capture event one editor frame implies, if any.
///
/// `clicked` is whether the drawn play view was clicked this frame. A session playing in the
/// separate play window owns the mouse there ([`crate::play_window`], ADR-7272), so a click on
/// the editor viewport — which keeps showing the possessed play world — must not grab the
/// editor window's cursor. Play ending releases a held grab.
#[must_use]
pub fn frame_event(session_active: bool, session_in_window: bool, clicked: bool) -> Option<CaptureEvent> {
    if !session_active {
        // As before the play window existed: every stopped frame releases a held grab.
        return Some(CaptureEvent::Lost);
    }
    (clicked && !session_in_window).then_some(CaptureEvent::ViewportClicked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_click_captures_during_play_and_shift_f1_releases() {
        let mut capture = PlayInputCapture::default();
        assert_eq!(
            on_event(&mut capture, false, CaptureEvent::ViewportClicked),
            None,
            "no capture while stopped"
        );
        assert_eq!(
            on_event(&mut capture, true, CaptureEvent::ViewportClicked),
            Some(true)
        );
        assert!(capture.captured);
        assert_eq!(
            on_event(&mut capture, true, CaptureEvent::ViewportClicked),
            None,
            "already captured"
        );
        assert!(is_release_key(true, true, false, false), "Shift+F1");
        assert!(
            !is_release_key(true, false, false, false),
            "plain F1 is help, not release"
        );
        assert!(
            !is_release_key(true, true, true, false),
            "Ctrl+Shift+F1 is not it"
        );
        assert!(!is_release_key(false, true, false, false));
        assert_eq!(
            on_event(&mut capture, true, CaptureEvent::ReleaseKey),
            Some(false)
        );
        assert!(!capture.captured);
        assert_eq!(on_event(&mut capture, true, CaptureEvent::ReleaseKey), None);
        // Stopping play releases a held capture.
        on_event(&mut capture, true, CaptureEvent::ViewportClicked);
        assert_eq!(on_event(&mut capture, false, CaptureEvent::Lost), Some(false));
    }

    /// fixrev8 finding 1: a click on the editor viewport grabs the editor window's cursor only
    /// while the session plays in the viewport; a session in the separate play window owns the
    /// mouse there, so the viewport's click (the viewport keeps drawing the possessed play world)
    /// is drained but must not capture the editor window.
    #[test]
    fn a_windowed_sessions_viewport_click_captures_nothing() {
        use super::CaptureEvent;
        use super::frame_event;

        // Viewport play: a click captures; no click changes nothing; stopping play releases.
        assert_eq!(
            frame_event(true, false, true),
            Some(CaptureEvent::ViewportClicked)
        );
        assert_eq!(frame_event(true, false, false), None);
        assert_eq!(frame_event(false, false, false), Some(CaptureEvent::Lost));
        // A stale click drained after play stopped still only releases.
        assert_eq!(frame_event(false, false, true), Some(CaptureEvent::Lost));

        // Windowed play: the editor viewport's click never captures the editor window.
        assert_eq!(
            frame_event(true, true, true),
            None,
            "a click while the session plays in the play window grabs nothing on the editor"
        );
        assert_eq!(frame_event(true, true, false), None);
    }
}
