// SPDX-License-Identifier: MPL-2.0
//! A lap-time leaderboard, DLC check and timeline markers on the mock Steam backend (ADR-2340).
//! No SDK and no Steam client needed:
//!
//! ```console
//! cargo run -p regia_steam --example steam_leaderboard
//! ```
//!
//! A game swaps `MockSteam` for the default backend (`SteamPlugin::new(app_id)` alone) and keeps
//! every other line.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_framework::leaderboard::{
    Leaderboard, LeaderboardDisplay, LeaderboardMessage, LeaderboardRange, LeaderboardSort, LeaderboardSpec,
};
use regia_platform::AppLifecycle;
use regia_steam::{
    AppId, AppOwnership, MockSteam, Steam, SteamBackend, SteamId, SteamLeaderboardPlugin, SteamPlugin,
    TimelineClip, TimelineEvent, TimelineMode,
};

const SOUNDTRACK_DLC: AppId = AppId(1_234_560);

/// The game's side: declare the board, submit the lap, ask for the rows around the player.
fn finish_lap(mut boards: ResMut<Leaderboard>, steam: Res<Steam>, mut done: Local<bool>) {
    if *done {
        return;
    }
    *done = true;
    let spec = LeaderboardSpec::new("fastest_lap")
        .ascending()
        .display(LeaderboardDisplay::TimeMilliseconds);
    if let Err(err) = boards.declare(spec) {
        eprintln!("declare: {err}");
    }
    boards.submit("fastest_lap", 63_500, &[2, 1]).ok();
    boards
        .request(
            "fastest_lap",
            LeaderboardRange::AroundUser { before: 1, after: 1 },
        )
        .ok();
    steam.timeline_mode(TimelineMode::Playing).ok();
    steam
        .timeline_event(
            &TimelineEvent::new("Personal best")
                .icon("steam_flag")
                .span(-63.5, 63.5)
                .clip(TimelineClip::Featured),
        )
        .ok();
}

fn report(mut messages: MessageReader<LeaderboardMessage>, boards: Res<Leaderboard>) {
    for message in messages.read() {
        match message {
            LeaderboardMessage::Uploaded { board, outcome } => {
                println!("{board}: uploaded {} -> rank {}", outcome.score, outcome.rank_new);
            }
            LeaderboardMessage::Entries { board, .. } => {
                for row in boards.entries(board) {
                    println!("{board}: #{} user {} {} ms", row.rank, row.user, row.score);
                }
            }
            other => println!("{other:?}"),
        }
    }
}

fn main() {
    // The mock stands in for Steam: two rivals already on the board.
    let mock = MockSteam::shared();
    mock.set_steam_id(Some(SteamId(76_561_197_960_287_930)));
    let store = mock.leaderboard_store();
    store.create_board(
        "fastest_lap",
        LeaderboardSort::Ascending,
        LeaderboardDisplay::TimeMilliseconds,
    );
    store.seed_score("fastest_lap", 76_561_197_960_265_728, 61_200, &[]);
    store.seed_score("fastest_lap", 76_561_197_960_265_729, 65_900, &[]);
    mock.set_app(AppOwnership {
        app: SOUNDTRACK_DLC,
        subscribed: true,
        installed: true,
        dlc_installed: true,
        install_dir: None,
    });

    let mut app = App::new();
    app.insert_resource(AppLifecycle::default());
    app.add_plugins((
        SteamPlugin::new(AppId(480)).with_backend(Arc::clone(&mock) as Arc<dyn SteamBackend>),
        SteamLeaderboardPlugin,
    ));
    app.add_systems(Update, (finish_lap, report));
    app.finish_plugins();
    for _ in 0..4 {
        app.update();
    }
    let steam = app.world().resource::<Steam>();
    println!(
        "soundtrack DLC installed: {}",
        steam.dlc_installed(SOUNDTRACK_DLC).unwrap_or(false)
    );
    println!("timeline: {:?}", steam.timeline_log());
}
