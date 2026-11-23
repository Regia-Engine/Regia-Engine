// SPDX-License-Identifier: MPL-2.0
//! Connects the game widgets of `regia_ui` to the framework and the network (ADR-2958).
//!
//! `regia_ui` knows neither crate, so its widgets are fed plain data. [`GameUiBridgePlugin`] does the
//! feeding:
//!
//! - A [`Scoreboard`] with `from_rules` shows
//!   [`GameRules::scoreboard`](crate::rules::GameRules::scoreboard) of the current rules.
//! - With the `net` feature, a [`LobbyList`](regia_ui::game_widgets::LobbyList) that is not `manual`
//!   shows `regia_net::Lobbies` (the same state `lobby.*` drives) and its buttons act on it.
//!   `LobbyListBridge` holds the choices.
//!
//! `regia::DefaultPlugins` installs the plugin with the `ui` feature; both halves idle until such a
//! widget exists.

use regia_app::prelude::*;
use regia_ui::game_widgets::{ScoreColumn, ScoreRow, ScoreTable, ScoreTeam, ScoreValue, Scoreboard};

use crate::rules::{GameRulesResource, RulesCtx, ScoreCell, ScoreSheet};

/// See the module documentation.
pub struct GameUiBridgePlugin;

impl Plugin for GameUiBridgePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            scoreboards_from_rules.run_if(any_with_component::<Scoreboard>),
        );
        #[cfg(feature = "net")]
        lobby::install(app);
    }
}

/// The widget's table for a sheet.
#[must_use]
pub fn score_table(sheet: &ScoreSheet) -> ScoreTable {
    ScoreTable {
        columns: sheet
            .columns
            .iter()
            .map(|c| ScoreColumn {
                key: c.key.clone(),
                title: c.title.clone(),
                width: c.width,
            })
            .collect(),
        rows: sheet
            .rows
            .iter()
            .map(|r| ScoreRow {
                id: r.id,
                name: r.name.clone(),
                team: r.team,
                local: r.local,
                values: r
                    .values
                    .iter()
                    .map(|v| match v {
                        ScoreCell::Int(n) => ScoreValue::Int(*n),
                        ScoreCell::Text(t) => ScoreValue::Text(t.clone()),
                    })
                    .collect(),
            })
            .collect(),
        teams: sheet
            .teams
            .iter()
            .map(|t| ScoreTeam {
                id: t.id,
                name: t.name.clone(),
                color: t.color.map(regia_ui::UiColor),
            })
            .collect(),
    }
}

/// `Update`: asks the rules for the standings while a board that shows them is on screen. A sheet
/// with no columns keeps the columns the board was authored with.
fn scoreboards_from_rules(world: &mut World) {
    let boards: Vec<Entity> = world
        .query::<(
            Entity,
            &Scoreboard,
            Option<&regia_ui::game_widgets::ScoreboardState>,
        )>()
        .iter(world)
        .filter(|(_, b, state)| {
            b.from_rules && (b.visible || state.is_none_or(|s| s.shown) || !b.hold_action.is_empty())
        })
        .map(|(e, ..)| e)
        .collect();
    if boards.is_empty() {
        return;
    }
    let _span = tracing::trace_span!("ui_scoreboard_rules").entered();
    let Some(mut rules) = world.remove_resource::<GameRulesResource>() else {
        return;
    };
    let sheet = rules.rules.scoreboard(&mut RulesCtx::new(world));
    world.insert_resource(rules);
    let Some(sheet) = sheet else { return };
    let table = score_table(&sheet);
    for entity in boards {
        let Some(mut board) = world.get_mut::<Scoreboard>(entity) else {
            continue;
        };
        // Each board keeps its own columns when the sheet has none.
        let same = (table.rows == board.table.rows)
            && (table.teams == board.table.teams)
            && (table.columns.is_empty() || table.columns == board.table.columns);
        if same {
            continue;
        }
        let authored = std::mem::take(&mut board.table.columns);
        board.table = table.clone();
        if table.columns.is_empty() {
            board.table.columns = authored;
        }
    }
}

#[cfg(feature = "net")]
pub use lobby::LobbyListBridge;

#[cfg(feature = "net")]
mod lobby {
    use std::collections::BTreeMap;

    use regia_app::prelude::*;
    use regia_ecs::ecs::message::MessageCursor;
    use regia_net::Lobbies;
    use regia_net::lobby::plugin::LobbyMessage;
    use regia_net::lobby::{
        LaunchError, LobbyError, LobbyEvent, LobbyId, LobbyInfo, LobbyQuery, LobbyServer, RequestId, keys,
    };
    use regia_net::session::NetSession;
    use regia_net::session::lifecycle::HostOptions;
    use regia_ui::game_widgets::{
        CurrentLobby, LobbyList, LobbyListAction, LobbyListDriver, LobbyListRequest, LobbyListStatus,
        LobbyMemberRow, LobbyRow,
    };

    /// [`LobbyListDriver::name`] of this driver.
    const DRIVER_NAME: &str = "regia_net";

    /// How [`super::GameUiBridgePlugin`] drives a lobby list.
    #[derive(Resource, Debug, Clone, PartialEq, Eq)]
    pub struct LobbyListBridge {
        /// Lobby data key shown as the lobby's name.
        pub name_key: String,
        /// Lobby data key shown beside the name.
        pub detail_key: String,
        /// Filters of the list requests (the engine adds its compatibility filters).
        pub query: LobbyQuery,
        /// Name written into a lobby the list creates (under [`Self::name_key`]); empty writes none.
        pub lobby_name: String,
        /// Refresh the list this often while not in a lobby, in frames; `0` only on request.
        pub refresh_every_frames: u32,
        /// Where "start match" sends the members. `None` hosts a listen session on the lobby
        /// transport (needs a `NetSession`), as `lobby.launch` does without `address`.
        pub server: Option<LobbyServer>,
    }

    impl Default for LobbyListBridge {
        fn default() -> Self {
            Self {
                name_key: "name".to_owned(),
                detail_key: "map".to_owned(),
                query: LobbyQuery::default(),
                lobby_name: String::new(),
                refresh_every_frames: 300,
                server: None,
            }
        }
    }

    /// What the driver remembers between frames.
    #[derive(Resource, Default)]
    struct DriverState {
        requests: MessageCursor<LobbyListRequest>,
        events: MessageCursor<LobbyMessage>,
        frames_since_refresh: u32,
        listed_once: bool,
        /// The request whose answer the status shows as busy; a periodic list is not one.
        waiting: Option<RequestId>,
        /// Why a player's request failed. Stays until their next request or a success.
        error: Option<String>,
        /// Why the last list failed. A later list that works clears it.
        list_error: Option<String>,
    }

    pub(super) fn install(app: &mut App) {
        app.init_resource::<DriverState>()
            .init_resource::<LobbyListBridge>()
            .add_systems(Update, drive_lobby_lists.run_if(any_with_component::<LobbyList>));
    }

    fn launch(world: &mut World) -> Result<(), String> {
        let server = world.resource::<LobbyListBridge>().server;
        if let Some(server) = server {
            return world
                .resource_mut::<Lobbies>()
                .launch(server)
                .map_err(|e| e.code().to_owned());
        }
        if !world.contains_resource::<NetSession>() {
            return Err("net_not_running".to_owned());
        }
        let result: Result<LobbyServer, LaunchError> =
            world.resource_scope(|world, mut lobbies: Mut<Lobbies>| {
                let opts = HostOptions {
                    max_players: lobbies.settings().max_members.max(1),
                    ..HostOptions::default()
                };
                let mut session = world.resource_mut::<NetSession>();
                lobbies.launch_hosting(&mut session, opts)
            });
        match result {
            Ok(_) => Ok(()),
            Err(LaunchError::Lobby(e)) => Err(e.code().to_owned()),
            Err(LaunchError::NoTransport) => Err("no_transport".to_owned()),
            Err(LaunchError::Session(_)) => Err("session_error".to_owned()),
        }
    }

    fn apply(world: &mut World, action: LobbyListAction, state: &mut DriverState) {
        let query = world.resource::<LobbyListBridge>().query.clone();
        let result: Result<Option<RequestId>, String> = match action {
            LobbyListAction::Refresh => {
                state.frames_since_refresh = 0;
                state.listed_once = true;
                world
                    .resource_mut::<Lobbies>()
                    .list(query)
                    .map(Some)
                    .map_err(|e: LobbyError| e.code().to_owned())
            }
            LobbyListAction::Create => {
                let bridge = world.resource::<LobbyListBridge>().clone();
                let mut lobbies = world.resource_mut::<Lobbies>();
                let mut spec = lobbies.default_spec();
                if !bridge.lobby_name.is_empty() {
                    let data: &mut BTreeMap<String, String> = &mut spec.data;
                    data.insert(bridge.name_key, bridge.lobby_name);
                }
                lobbies.create(spec).map(Some).map_err(|e| e.code().to_owned())
            }
            LobbyListAction::Join(id) => world
                .resource_mut::<Lobbies>()
                .join(LobbyId(id))
                .map(Some)
                .map_err(|e| e.code().to_owned()),
            LobbyListAction::Leave => {
                world.resource_mut::<Lobbies>().leave();
                // Back on the list: show what is out there now.
                state.listed_once = false;
                Ok(None)
            }
            LobbyListAction::SetReady(ready) => world
                .resource_mut::<Lobbies>()
                .set_ready(ready)
                .map(|()| None)
                .map_err(|e| e.code().to_owned()),
            LobbyListAction::Launch => launch(world).map(|()| None),
        };
        match result {
            Ok(wait) => {
                state.error = None;
                state.list_error = None;
                state.waiting = wait;
            }
            Err(code) => {
                state.error = Some(code);
                state.waiting = None;
            }
        }
    }

    /// A list nobody pressed a button for: it says nothing about any other request. Only the first
    /// one, with nothing yet to show, reads as "searching".
    fn refresh_quietly(world: &mut World, state: &mut DriverState, first: bool) {
        let query = world.resource::<LobbyListBridge>().query.clone();
        state.frames_since_refresh = 0;
        state.listed_once = true;
        match world.resource_mut::<Lobbies>().list(query) {
            Ok(request) => {
                if first {
                    state.waiting = Some(request);
                }
            }
            Err(e) => state.list_error = Some(e.code().to_owned()),
        }
    }

    fn row_of(info: &LobbyInfo, bridge: &LobbyListBridge) -> LobbyRow {
        LobbyRow {
            id: info.id.0,
            name: info.data.get(&bridge.name_key).cloned().unwrap_or_default(),
            detail: info.data.get(&bridge.detail_key).cloned().unwrap_or_default(),
            players: info.member_count,
            max_players: info.max_members,
            joinable: info.joinable && info.open_slots() > 0,
        }
    }

    fn current_of(lobbies: &Lobbies, info: &LobbyInfo, bridge: &LobbyListBridge) -> CurrentLobby {
        let local = lobbies.local_member();
        CurrentLobby {
            id: info.id.0,
            name: info.data.get(&bridge.name_key).cloned().unwrap_or_default(),
            max_players: info.max_members,
            members: info
                .members
                .iter()
                .map(|m| LobbyMemberRow {
                    id: m.id.0,
                    name: m.name.clone(),
                    ready: m.data.get(keys::READY).map(String::as_str) == Some("1"),
                    owner: m.id == info.owner,
                    local: Some(m.id) == local,
                })
                .collect(),
            is_owner: lobbies.is_owner(),
            can_launch: lobbies.ready_to_launch(),
            launched: info.server.is_some(),
        }
    }

    /// `Update`: acts on the lists' requests, follows the platform's answers, and writes the state
    /// into every list.
    fn drive_lobby_lists(world: &mut World) {
        if !world.contains_resource::<Lobbies>() {
            // A game's own driver stays; ours goes with the lobbies.
            if world
                .get_resource::<LobbyListDriver>()
                .is_some_and(|d| d.name == DRIVER_NAME)
            {
                world.remove_resource::<LobbyListDriver>();
            }
            return;
        }
        let _span = tracing::trace_span!("ui_lobby_list_driver").entered();
        if !world.contains_resource::<LobbyListDriver>() {
            world.insert_resource(LobbyListDriver {
                name: DRIVER_NAME.to_owned(),
            });
        }
        let mut state = world.remove_resource::<DriverState>().unwrap_or_default();
        let requests: Vec<LobbyListRequest> = world
            .get_resource::<Messages<LobbyListRequest>>()
            .map(|m| state.requests.read(m).copied().collect())
            .unwrap_or_default();
        for request in requests {
            // A manual list's game answers its own requests.
            if world.get::<LobbyList>(request.list).is_some_and(|l| !l.manual) {
                apply(world, request.action, &mut state);
            }
        }
        // Answers.
        let answers: Vec<LobbyEvent> = world
            .get_resource::<Messages<LobbyMessage>>()
            .map(|m| state.events.read(m).map(|e| e.0.clone()).collect())
            .unwrap_or_default();
        for event in &answers {
            match event {
                LobbyEvent::Created { request, result } | LobbyEvent::Entered { request, result } => {
                    if state.waiting == Some(*request) {
                        state.waiting = None;
                    }
                    state.error = result.as_ref().err().map(|e| e.code().to_owned());
                }
                LobbyEvent::List { request, result } => {
                    if state.waiting == Some(*request) {
                        state.waiting = None;
                    }
                    state.list_error = result.as_ref().err().map(|e| e.code().to_owned());
                }
                _ => {}
            }
        }
        // Keep the list fresh while the player looks at it.
        let in_lobby = world.resource::<Lobbies>().current().is_some();
        if !in_lobby {
            let every = world.resource::<LobbyListBridge>().refresh_every_frames;
            state.frames_since_refresh = state.frames_since_refresh.saturating_add(1);
            let first = !state.listed_once;
            // Not over a request in flight: a newer list may replace the one it waits for.
            if state.waiting.is_none() && (first || (every > 0 && state.frames_since_refresh >= every)) {
                refresh_quietly(world, &mut state, first);
            }
        }
        // Write the widgets.
        let bridge = world.resource::<LobbyListBridge>().clone();
        let (rows, current) = {
            let lobbies = world.resource::<Lobbies>();
            let rows: Vec<LobbyRow> = match lobbies.last_list() {
                Some(Ok(list)) => list.iter().map(|l| row_of(l, &bridge)).collect(),
                _ => Vec::new(),
            };
            let current = lobbies.snapshot().map(|s| current_of(lobbies, s, &bridge));
            (rows, current)
        };
        let status = match (state.error.as_ref().or(state.list_error.as_ref()), state.waiting) {
            (Some(code), _) => LobbyListStatus::Error(code.clone()),
            (None, Some(_)) => LobbyListStatus::Busy,
            (None, None) => LobbyListStatus::Idle,
        };
        let mut lists = world.query::<&mut LobbyList>();
        for mut list in lists.iter_mut(world) {
            if list.manual {
                continue;
            }
            if list.rows != rows {
                list.rows.clone_from(&rows);
            }
            if list.current != current {
                list.current.clone_from(&current);
            }
            if list.status != status {
                list.status = status.clone();
            }
        }
        world.insert_resource(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{GameRules, ScoreSheetColumn, ScoreSheetRow, ScoreSheetTeam, SpawnRequest};

    #[test]
    fn a_sheet_becomes_the_widgets_table_cell_for_cell() {
        let sheet = ScoreSheet {
            columns: vec![ScoreSheetColumn::new("tags", "Tags")],
            rows: vec![ScoreSheetRow {
                id: 3,
                name: "Ann".into(),
                team: Some(1),
                local: true,
                values: vec![ScoreCell::Int(5), ScoreCell::Text("x".into())],
            }],
            teams: vec![ScoreSheetTeam {
                id: 1,
                name: "Blue".into(),
                color: Some([0.0, 0.0, 1.0, 1.0]),
            }],
        };
        let table = score_table(&sheet);
        assert_eq!(table.columns[0].key, "tags");
        assert_eq!(table.columns[0].title, "Tags");
        assert_eq!(table.rows[0].values[0], ScoreValue::Int(5));
        assert_eq!(table.rows[0].values[1], ScoreValue::Text("x".into()));
        assert!(table.rows[0].local);
        assert_eq!(
            table.teams[0].color,
            Some(regia_ui::UiColor([0.0, 0.0, 1.0, 1.0]))
        );
    }

    #[derive(Default)]
    struct TwoPlayers;

    impl GameRules for TwoPlayers {
        fn on_player_joined(&mut self, _: &mut RulesCtx, _: Entity) -> Option<SpawnRequest> {
            None
        }
        fn scoreboard(&mut self, _: &mut RulesCtx) -> Option<ScoreSheet> {
            Some(ScoreSheet {
                columns: vec![ScoreSheetColumn::new("tags", "Tags")],
                rows: vec![
                    ScoreSheetRow {
                        id: 1,
                        name: "One".into(),
                        values: vec![ScoreCell::Int(1)],
                        ..Default::default()
                    },
                    ScoreSheetRow {
                        id: 2,
                        name: "Two".into(),
                        values: vec![ScoreCell::Int(2)],
                        ..Default::default()
                    },
                ],
                teams: Vec::new(),
            })
        }
    }

    /// `lobbies` opted in to an empty identity: these apps have no `NetSession` to take one
    /// from (ADR-7881).
    #[cfg(feature = "net")]
    fn unidentified(mut lobbies: regia_net::Lobbies) -> regia_net::Lobbies {
        lobbies.allow_unidentified(true);
        lobbies
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_ui::UiPlugin::code_only()));
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        app.add_plugins(GameUiBridgePlugin);
        app
    }

    #[derive(Default)]
    struct NoColumns;

    impl GameRules for NoColumns {
        fn on_player_joined(&mut self, _: &mut RulesCtx, _: Entity) -> Option<SpawnRequest> {
            None
        }
        fn scoreboard(&mut self, _: &mut RulesCtx) -> Option<ScoreSheet> {
            Some(ScoreSheet {
                columns: Vec::new(),
                rows: vec![ScoreSheetRow {
                    id: 1,
                    name: "One".into(),
                    values: vec![ScoreCell::Int(1)],
                    ..Default::default()
                }],
                teams: Vec::new(),
            })
        }
    }

    /// A sheet with no columns leaves each board its own, not the first board's.
    #[test]
    fn a_sheet_without_columns_keeps_each_boards_own() {
        let mut app = app();
        app.insert_resource(GameRulesResource {
            rules: Box::new(NoColumns),
        });
        let with_column = |key: &str| Scoreboard {
            table: ScoreTable {
                columns: vec![ScoreColumn {
                    key: key.into(),
                    title: key.into(),
                    width: 0.0,
                }],
                ..ScoreTable::default()
            },
            ..Scoreboard::default()
        };
        let a = app.world_mut().spawn(with_column("a")).id();
        let b = app.world_mut().spawn(with_column("b")).id();
        for _ in 0..2 {
            app.update();
        }
        for (entity, key) in [(a, "a"), (b, "b")] {
            let board = app.world().get::<Scoreboard>(entity).expect("board");
            assert_eq!(board.table.rows.len(), 1);
            assert_eq!(board.table.columns[0].key, key);
        }
    }

    #[test]
    fn a_board_shows_the_rules_and_a_game_filled_board_is_left_alone() {
        let mut app = app();
        app.insert_resource(GameRulesResource {
            rules: Box::new(TwoPlayers),
        });
        let from_rules = app.world_mut().spawn(Scoreboard::default()).id();
        let manual = app
            .world_mut()
            .spawn(Scoreboard {
                from_rules: false,
                ..Scoreboard::default()
            })
            .id();
        app.update();
        let board = app.world().get::<Scoreboard>(from_rules).expect("board");
        assert_eq!(board.table.rows.len(), 2);
        assert_eq!(board.table.columns[0].title, "Tags");
        let other = app.world().get::<Scoreboard>(manual).expect("board");
        assert!(other.table.rows.is_empty());
        assert!(app.world().contains_resource::<GameRulesResource>());
    }

    #[test]
    fn rules_without_a_scoreboard_keep_the_authored_columns() {
        let mut app = app();
        app.insert_resource(GameRulesResource::default());
        let board = app
            .world_mut()
            .spawn(Scoreboard {
                table: ScoreTable {
                    columns: vec![ScoreColumn::new("Score")],
                    ..Default::default()
                },
                ..Scoreboard::default()
            })
            .id();
        app.update();
        let b = app.world().get::<Scoreboard>(board).expect("board");
        assert_eq!(b.table.columns.len(), 1);
        assert!(b.table.rows.is_empty());
    }

    #[cfg(feature = "net")]
    #[test]
    fn a_list_follows_the_lobbies_and_its_requests_reach_them() {
        use std::sync::Arc;

        use regia_net::lobby::LocalLobbyHub;
        use regia_ui::game_widgets::{LobbyList, LobbyListAction, LobbyListDriver, LobbyListRequest};

        let hub = LocalLobbyHub::default();
        let mut owner = app();
        let mut guest = app();
        owner.insert_resource(unidentified(regia_net::Lobbies::new(Arc::new(
            hub.member(1, "owner"),
        ))));
        guest.insert_resource(unidentified(regia_net::Lobbies::new(Arc::new(
            hub.member(2, "guest"),
        ))));
        owner.add_plugins(regia_net::LobbyPlugin);
        guest.add_plugins(regia_net::LobbyPlugin);
        let owner_list = owner.world_mut().spawn(LobbyList::default()).id();
        let guest_list = guest.world_mut().spawn(LobbyList::default()).id();
        let step = |a: &mut App, b: &mut App| {
            for _ in 0..4 {
                a.update();
                b.update();
            }
        };
        step(&mut owner, &mut guest);
        assert!(owner.world().contains_resource::<LobbyListDriver>());
        owner.world_mut().write_message(LobbyListRequest {
            list: owner_list,
            action: LobbyListAction::Create,
        });
        step(&mut owner, &mut guest);
        let created = owner.world().get::<LobbyList>(owner_list).expect("list");
        let current = created.current.as_ref().expect("the owner is in its lobby");
        assert!(current.is_owner);
        assert_eq!(current.members.len(), 1);
        // The guest refreshes and sees it, then joins through the same request a row sends.
        guest.world_mut().write_message(LobbyListRequest {
            list: guest_list,
            action: LobbyListAction::Refresh,
        });
        step(&mut owner, &mut guest);
        let seen = guest.world().get::<LobbyList>(guest_list).expect("list").clone();
        assert_eq!(seen.rows.len(), 1, "{:?}", seen.status);
        assert_eq!(seen.rows[0].players, 1);
        assert!(seen.rows[0].joinable);
        guest.world_mut().write_message(LobbyListRequest {
            list: guest_list,
            action: LobbyListAction::Join(seen.rows[0].id),
        });
        step(&mut owner, &mut guest);
        let joined = guest.world().get::<LobbyList>(guest_list).expect("list").clone();
        let lobby = joined.current.expect("the guest is in");
        assert_eq!(lobby.members.len(), 2);
        assert!(!lobby.is_owner);
        assert!(lobby.members.iter().any(|m| m.local && m.name == "guest"));
        // Ready flags travel; the owner may launch once all are ready.
        for (app, list) in [(&mut owner, owner_list), (&mut guest, guest_list)] {
            app.world_mut().write_message(LobbyListRequest {
                list,
                action: LobbyListAction::SetReady(true),
            });
        }
        step(&mut owner, &mut guest);
        let ready = owner.world().get::<LobbyList>(owner_list).expect("list").clone();
        assert!(ready.current.expect("in").members.iter().all(|m| m.ready));
        // A launch with nowhere to go reports a stable code, and leaves the state alone.
        owner.world_mut().write_message(LobbyListRequest {
            list: owner_list,
            action: LobbyListAction::Launch,
        });
        step(&mut owner, &mut guest);
        let refused = owner.world().get::<LobbyList>(owner_list).expect("list").clone();
        assert_eq!(
            refused.status,
            regia_ui::game_widgets::LobbyListStatus::Error("net_not_running".into())
        );
        // A failure is visible and leaving goes back to the list.
        guest.world_mut().write_message(LobbyListRequest {
            list: guest_list,
            action: LobbyListAction::Leave,
        });
        step(&mut owner, &mut guest);
        let left = guest.world().get::<LobbyList>(guest_list).expect("list");
        assert!(left.current.is_none());
    }

    #[cfg(feature = "net")]
    #[test]
    fn a_games_own_driver_and_requests_stay_with_the_game() {
        use std::sync::Arc;

        use regia_net::lobby::LocalLobbyHub;
        use regia_ui::game_widgets::{LobbyList, LobbyListAction, LobbyListDriver, LobbyListRequest};

        // Without lobbies, a game's own driver survives.
        let mut own = app();
        own.world_mut().spawn(LobbyList {
            manual: true,
            ..Default::default()
        });
        own.insert_resource(LobbyListDriver {
            name: "my browser".into(),
        });
        own.update();
        assert_eq!(own.world().resource::<LobbyListDriver>().name, "my browser");

        // With lobbies, a manual list's request is not ours to act on.
        let hub = LocalLobbyHub::default();
        let mut app = app();
        app.insert_resource(unidentified(regia_net::Lobbies::new(Arc::new(
            hub.member(1, "one"),
        ))));
        app.add_plugins(regia_net::LobbyPlugin);
        let manual = app
            .world_mut()
            .spawn(LobbyList {
                manual: true,
                ..Default::default()
            })
            .id();
        for _ in 0..2 {
            app.update();
        }
        app.world_mut().write_message(LobbyListRequest {
            list: manual,
            action: LobbyListAction::Create,
        });
        for _ in 0..4 {
            app.update();
        }
        assert!(app.world().resource::<regia_net::Lobbies>().current().is_none());
    }

    #[cfg(feature = "net")]
    #[test]
    fn a_failed_request_shows_its_code_and_a_periodic_refresh_says_nothing() {
        use std::sync::Arc;

        use regia_net::lobby::LocalLobbyHub;
        use regia_ui::game_widgets::{LobbyList, LobbyListAction, LobbyListRequest, LobbyListStatus};

        let hub = LocalLobbyHub::default();
        let mut app = app();
        app.insert_resource(unidentified(regia_net::Lobbies::new(Arc::new(
            hub.member(1, "one"),
        ))));
        app.add_plugins(regia_net::LobbyPlugin);
        app.world_mut()
            .resource_mut::<LobbyListBridge>()
            .refresh_every_frames = 2;
        let list = app.world_mut().spawn(LobbyList::default()).id();
        // No such lobby.
        for _ in 0..3 {
            app.update();
        }
        app.world_mut().write_message(LobbyListRequest {
            list,
            action: LobbyListAction::Join(999),
        });
        for _ in 0..3 {
            app.update();
        }
        // It stays through the periodic lists, every 2 frames here, until the player does something.
        for _ in 0..12 {
            let status = app.world().get::<LobbyList>(list).expect("list").status.clone();
            assert!(
                matches!(&status, LobbyListStatus::Error(code) if code == "lobby_not_found"),
                "{status:?}"
            );
            app.update();
        }
        // A later refresh that works clears it, and busy never shows for a periodic one.
        app.world_mut().write_message(LobbyListRequest {
            list,
            action: LobbyListAction::Refresh,
        });
        for _ in 0..3 {
            app.update();
        }
        for _ in 0..10 {
            app.update();
            let status = &app.world().get::<LobbyList>(list).expect("list").status;
            assert_eq!(*status, LobbyListStatus::Idle);
        }
    }

    #[cfg(feature = "net")]
    #[test]
    fn a_manual_list_is_left_alone_and_without_lobbies_there_is_no_driver() {
        use regia_ui::game_widgets::{LobbyList, LobbyListDriver, LobbyRow};
        let mut app = app();
        let rows = vec![LobbyRow {
            id: 9,
            name: "mine".into(),
            ..Default::default()
        }];
        let list = app
            .world_mut()
            .spawn(LobbyList {
                rows: rows.clone(),
                manual: true,
                ..Default::default()
            })
            .id();
        app.update();
        assert!(!app.world().contains_resource::<LobbyListDriver>());
        assert_eq!(app.world().get::<LobbyList>(list).expect("list").rows, rows);
    }
}
