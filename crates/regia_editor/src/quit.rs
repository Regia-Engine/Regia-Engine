// SPDX-License-Identifier: MPL-2.0
//! Quitting the editor from the agent protocol (`quit.request`, ADR-1582).
//!
//! The edit world runs on `MinimalPlugins`, without the platform runner that turns a close
//! request into an exit, so the editor does it itself: [`install`] registers the close message
//! the protocol writes, and [`observe_quit`] (in `Last`) turns it, or an [`AppExit`], into the
//! [`EditorQuit`] flag. The event loop in `main.rs` reads the flag after each edit-world frame and
//! exits the loop.

use regia_app::prelude::*;
use regia_platform::{ClosePolicy, PrimaryWindow, WindowCloseRequested};

/// Set when the agent protocol (or an [`AppExit`]) asked the editor to quit. The event loop
/// takes it with [`EditorQuit::take`] and exits.
#[derive(Resource, Debug, Default)]
pub struct EditorQuit {
    requested: bool,
}

impl EditorQuit {
    /// True once a quit was requested and not yet taken.
    pub fn is_requested(&self) -> bool {
        self.requested
    }

    /// Returns whether a quit was requested and clears the flag.
    pub fn take(&mut self) -> bool {
        std::mem::take(&mut self.requested)
    }
}

/// Registers the close message, the [`ClosePolicy`] and the [`EditorQuit`] flag in the edit
/// world, and the system that sets it.
pub fn install(app: &mut App) {
    app.add_message::<WindowCloseRequested>()
        .init_resource::<ClosePolicy>()
        .init_resource::<EditorQuit>()
        .add_systems(Last, observe_quit);
}

/// `Last`: a close request for the primary window (when [`ClosePolicy`] exits on close) or any
/// [`AppExit`] sets [`EditorQuit`].
pub fn observe_quit(
    policy: Res<ClosePolicy>,
    mut close: MessageReader<WindowCloseRequested>,
    mut exit: MessageReader<AppExit>,
    primary: Query<Entity, With<PrimaryWindow>>,
    mut quit: ResMut<EditorQuit>,
) {
    let primary = primary.iter().next();
    let closed = close.read().any(|r| Some(r.window) == primary);
    let exited = exit.read().count() > 0;
    if exited || (closed && policy.exit_on_primary_close) {
        quit.requested = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor_like_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.world_mut().spawn(PrimaryWindow);
        install(&mut app);
        app
    }

    #[test]
    fn a_quit_request_sets_the_flag() {
        let mut app = editor_like_app();
        app.update();
        assert!(!app.world().resource::<EditorQuit>().is_requested());
        let request = regia_platform::request_quit(app.world_mut());
        assert!(request.will_exit);
        assert!(request.window.is_some());
        assert_eq!(
            app.world().resource::<Messages<WindowCloseRequested>>().len(),
            1,
            "the close message is registered, so the request is delivered"
        );
        app.update();
        assert!(app.world_mut().resource_mut::<EditorQuit>().take());
        assert!(
            !app.world().resource::<EditorQuit>().is_requested(),
            "take clears the flag"
        );
    }

    #[test]
    fn the_close_policy_can_leave_the_decision_to_the_user() {
        let mut app = editor_like_app();
        app.insert_resource(ClosePolicy {
            exit_on_primary_close: false,
            back_closes_primary: true,
        });
        let request = regia_platform::request_quit(app.world_mut());
        assert!(!request.will_exit);
        app.update();
        assert!(!app.world().resource::<EditorQuit>().is_requested());
    }

    #[test]
    fn an_app_exit_message_sets_the_flag() {
        let mut app = editor_like_app();
        app.world_mut().write_message(AppExit::Success);
        app.update();
        assert!(app.world().resource::<EditorQuit>().is_requested());
    }
}
