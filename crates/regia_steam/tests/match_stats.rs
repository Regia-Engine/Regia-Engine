// SPDX-License-Identifier: MPL-2.0
//! Audit gap GR-10.3: a finished-match record reaches platform stats through
//! [`regia_steam::match_stats::PlatformStatsSink`], configured by stat name.

use regia_app::prelude::*;
use regia_framework::match_results::{MatchResult, MatchResultSink, ResultValue};
use regia_input::InputContexts;
use regia_platform::AppLifecycle;
use regia_steam::match_stats::{PlatformStat, PlatformStatsSink};
use regia_steam::{MockSteam, SteamPlugin, SteamSettings};

fn app_with(backend: &std::sync::Arc<MockSteam>) -> App {
    let mut app = App::new();
    app.insert_resource(SteamSettings::default())
        .insert_resource(AppLifecycle::default())
        .insert_resource(InputContexts::default());
    app.add_plugins(
        SteamPlugin::from_project_settings()
            .with_backend(std::sync::Arc::clone(backend) as std::sync::Arc<dyn regia_steam::SteamBackend>),
    );
    app.finish_plugins();
    app
}

#[test]
fn platform_stats_sink_adds_the_local_players_counters() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock);
    let mut sink = PlatformStatsSink {
        player: "steam:76561190000000001".to_owned(),
        stats: vec![
            PlatformStat {
                stat: "kills".to_owned(),
                value: ResultValue::Kills,
            },
            PlatformStat {
                stat: "revives".to_owned(),
                value: ResultValue::Counter("revives".to_owned()),
            },
            PlatformStat {
                stat: "wins".to_owned(),
                value: ResultValue::Won,
            },
        ],
    };
    let result = MatchResult::example();
    sink.send(app.world_mut(), &result).expect("sink");
    sink.send(app.world_mut(), &result)
        .expect("a second match adds again");
    assert_eq!(mock.stat_i32("kills"), Some(8));
    assert_eq!(mock.stat_i32("revives"), Some(4));
    assert_eq!(mock.stat_i32("wins"), Some(2));

    // A peer that did not play writes nothing.
    let mut host = PlatformStatsSink {
        player: "steam:1".to_owned(),
        ..sink.clone()
    };
    host.send(app.world_mut(), &result)
        .expect("no row is not an error");
    assert_eq!(mock.stat_i32("kills"), Some(8));
}
