// SPDX-License-Identifier: MPL-2.0
//! Overlay notifications (specs/24 § 2 "Overlay"): the [`OverlayActive`] resource, the
//! [`OverlayOpened`]/[`OverlayClosed`] messages, and the plugin's reaction — suspend gameplay
//! input contexts and (opt-in) pause game time while the overlay is open.

use regia_app::prelude::*;

/// Whether the Steam overlay is currently open. Updated by the frame pump from
/// `GameOverlayActivated`.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlayActive(pub bool);

/// The overlay just opened. Gameplay input contexts are suspended (see
/// [`crate::SteamSettings::suspend_contexts`]); the game may pause (see
/// [`crate::SteamSettings::pause_on_overlay`]).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayOpened;

/// The overlay just closed. Previously suspended input contexts are restored.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayClosed;

/// The input contexts that were disabled while the overlay was open, per player, so
/// [`OverlayClosed`] can restore exactly what was suspended.
#[derive(Resource, Debug, Default)]
pub(crate) struct SuspendedContexts(pub(crate) Vec<(u8, Vec<String>)>);

/// How many local players the overlay suspension covers (players 0..=7). A game with more local
/// players or a dynamic context stack should set [`crate::SteamSettings::suspend_contexts`] to
/// `None` and react to [`OverlayOpened`] itself.
pub(crate) const SUSPENDED_PLAYER_RANGE: std::ops::Range<u8> = 0..8;

/// Disables the configured gameplay contexts for every covered player and remembers what was
/// actually disabled, so the restore is exact.
pub(crate) fn suspend_contexts(
    contexts: &mut regia_input::InputContexts,
    names: &[String],
) -> Vec<(u8, Vec<String>)> {
    let mut suspended = Vec::new();
    for player in SUSPENDED_PLAYER_RANGE {
        let disabled: Vec<String> = names
            .iter()
            .filter(|name| contexts.pop(player, name))
            .cloned()
            .collect();
        if !disabled.is_empty() {
            suspended.push((player, disabled));
        }
    }
    suspended
}

/// Restores contexts suspended by [`suspend_contexts`].
pub(crate) fn restore_contexts(contexts: &mut regia_input::InputContexts, suspended: &[(u8, Vec<String>)]) {
    for (player, names) in suspended {
        for name in names {
            contexts.push(*player, name.clone());
        }
    }
}

/// `PreUpdate`, after the pump: holds [`regia_platform::CursorRelease`] while the overlay is open
/// and [`crate::SteamSettings::release_cursor_on_overlay`] is on, and lets go when it closes
/// (ADR-7349). It moves the hold only on the overlay's edges, so a game may set or clear it
/// itself between them.
pub(crate) fn release_cursor_with_overlay(
    overlay: Res<OverlayActive>,
    settings: Res<crate::SteamSettings>,
    mut release: ResMut<regia_platform::CursorRelease>,
    mut holding: Local<bool>,
) {
    let want = overlay.0 && settings.release_cursor_on_overlay;
    if want == *holding {
        return;
    }
    *holding = want;
    if release.0 != want {
        release.0 = want;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspend_and_restore_is_exact() {
        let mut contexts = regia_input::InputContexts::default();
        contexts.push(0, "Gameplay");
        contexts.push(0, "Hud");
        contexts.push(1, "Gameplay");
        let names = vec!["Gameplay".to_owned()];
        let suspended = suspend_contexts(&mut contexts, &names);
        assert_eq!(
            suspended,
            vec![(0, vec!["Gameplay".to_owned()]), (1, vec!["Gameplay".to_owned()])],
            "only the configured contexts are suspended, on every covered player"
        );
        assert!(contexts.is_enabled(0, "Hud"), "unrelated contexts keep running");
        assert!(!contexts.is_enabled(0, "Gameplay"));

        restore_contexts(&mut contexts, &suspended);
        assert!(contexts.is_enabled(0, "Gameplay"));
        assert!(contexts.is_enabled(1, "Gameplay"));
        assert!(contexts.is_enabled(0, "Hud"));

        // Suspending again now finds the restored contexts (the restore really put them back).
        let again = suspend_contexts(&mut contexts, &names);
        assert_eq!(
            again,
            vec![(0, vec!["Gameplay".to_owned()]), (1, vec!["Gameplay".to_owned()])],
            "a restore followed by a suspend suspends the same contexts"
        );
        // And suspending twice in a row (without restore) finds nothing the second time.
        assert!(
            suspend_contexts(&mut contexts, &names).is_empty(),
            "pop removed them"
        );
        restore_contexts(&mut contexts, &again);
    }
}
