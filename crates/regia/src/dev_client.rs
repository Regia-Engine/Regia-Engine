// SPDX-License-Identifier: MPL-2.0
//! A client that `regia server --clients N` started (spec 76 § 10): it joins the dev server on
//! loopback by itself, with no window. Only the environment turns this on: `REGIA_CONNECT` names
//! the server, `REGIA_CLIENT_OFFSCREEN=1` runs without a window, `REGIA_CLIENT_NAME` names the
//! player, and `REGIA_CLIENT_TITLE` titles a windowed client's window (play-in-editor's separate
//! processes, ADR-2682). A normal run has none of them and this adds nothing.

use regia_app::prelude::*;
use regia_net::session::{ConnectOptions, ConnectTarget, NetSession};
use regia_net::{NetPlugin, NetSessionPlugin};

/// The server a dev client joins, `address:port`.
pub const CONNECT_ENV: &str = "REGIA_CONNECT";
/// Set to `1` to run the client without a window.
pub const OFFSCREEN_ENV: &str = "REGIA_CLIENT_OFFSCREEN";
/// The player's name.
pub const NAME_ENV: &str = "REGIA_CLIENT_NAME";
/// The window title of a windowed dev client ("PIE client 2").
pub const TITLE_ENV: &str = "REGIA_CLIENT_TITLE";

/// The join request of a dev client.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
struct DevJoin {
    address: String,
    name: String,
}

/// Installs what the environment asks for; nothing in a normal run.
pub fn install(app: &mut App) {
    let var = |key: &str| regia_core::brand::env_var(key).ok().filter(|v| !v.is_empty());
    if var(OFFSCREEN_ENV).as_deref() == Some("1") {
        app.set_runner(regia_app::run_headless);
        // No vsync without a window: without a cap the client takes a whole core.
        app.insert_resource(FramePacing::new(Some(60.0)));
    }
    if let Some(title) = var(TITLE_ENV) {
        set_title(app, title);
    }
    let Some(address) = var(CONNECT_ENV) else {
        return;
    };
    add_join(app, address, var(NAME_ENV).unwrap_or_else(|| "client".to_owned()));
}

/// Titles the primary window before it is created.
fn set_title(app: &mut App, title: String) {
    app.world_mut()
        .get_resource_or_insert_with::<regia_platform::WindowSettings>(Default::default)
        .window
        .title = title;
}

fn add_join(app: &mut App, address: String, name: String) {
    app.add_plugins((
        NetPlugin {
            mode: regia_net::model::NetMode::Client,
        },
        NetSessionPlugin,
    ));
    // A game that already added `NetPlugin` (say `Offline`, for single player) made this one skip
    // its build: the session below would run as a client under a mode that still says server.
    app.insert_resource(regia_net::model::NetMode::Client);
    app.insert_resource(DevJoin { address, name });
    app.add_systems(Update, join_once);
}

/// `Update`: connects once. A refused start is logged; the session reports the rest.
fn join_once(mut join: Option<ResMut<DevJoin>>, mut session: ResMut<NetSession>, mut done: Local<bool>) {
    if *done {
        return;
    }
    *done = true;
    let Some(join) = join.as_deref_mut() else {
        return;
    };
    let options = ConnectOptions {
        player_names: vec![join.name.clone()],
        ..ConnectOptions::default()
    };
    match join.address.parse() {
        Ok(addr) => {
            if let Err(err) = session.connect(ConnectTarget::Address(addr), options) {
                tracing::error!(target: "regia", address = %join.address, "dev client: {err}");
            }
        }
        Err(err) => tracing::error!(target: "regia", address = %join.address, "dev client: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_run_installs_nothing() {
        let mut app = App::new();
        if regia_core::brand::env_var_os(CONNECT_ENV).is_none() {
            install(&mut app);
            assert!(!app.world().contains_resource::<DevJoin>());
            assert!(!app.world().contains_resource::<NetSession>());
        }
    }

    #[test]
    fn a_windowed_dev_client_gets_its_title() {
        let mut app = App::new();
        set_title(&mut app, "PIE client 2".into());
        assert_eq!(
            app.world()
                .resource::<regia_platform::WindowSettings>()
                .window
                .title,
            "PIE client 2"
        );
    }

    #[test]
    fn a_dev_client_connects_once() {
        let mut app = App::new();
        add_join(&mut app, "127.0.0.1:47777".into(), "client3".into());
        app.update();
        app.update();
        let session = app.world().resource::<NetSession>();
        assert!(session.client().is_some(), "connecting");
    }

    #[test]
    fn a_game_that_added_an_offline_net_plugin_still_runs_as_a_client() {
        let mut app = App::new();
        app.add_plugins(NetPlugin {
            mode: regia_net::model::NetMode::Offline,
        });
        add_join(&mut app, "127.0.0.1:47777".into(), "c".into());
        assert_eq!(
            *app.world().resource::<regia_net::model::NetMode>(),
            regia_net::model::NetMode::Client
        );
    }

    #[test]
    fn a_bad_address_is_logged_not_fatal() {
        let mut app = App::new();
        add_join(&mut app, "not an address".into(), "client".into());
        app.update();
        assert!(app.world().resource::<NetSession>().client().is_none());
    }
}
