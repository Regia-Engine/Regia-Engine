// SPDX-License-Identifier: MPL-2.0
//! Repeated game-owner plugin lifetimes in one app.

use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_ecs::ecs::schedule::ScheduleCleanupPolicy;
use regia_ecs::owner::{OwnerSet, TypeOwner};

type Trace = Arc<Mutex<Vec<&'static str>>>;

fn record(trace: &Trace, event: &'static str) {
    if let Ok(mut events) = trace.lock() {
        events.push(event);
    }
}

fn snapshot(trace: &Trace) -> Vec<&'static str> {
    trace.lock().map(|events| events.clone()).unwrap_or_default()
}

struct EnginePlugin(Trace);

impl Plugin for EnginePlugin {
    fn build(&self, _app: &mut App) {
        record(&self.0, "engine.build");
    }

    fn finish(&self, _app: &mut App) {
        record(&self.0, "engine.finish");
    }

    fn cleanup(&self, _app: &mut App) {
        record(&self.0, "engine.cleanup");
    }

    fn name(&self) -> &str {
        "engine"
    }
}

struct GameParent(Trace, &'static str);

impl Plugin for GameParent {
    fn build(&self, _app: &mut App) {
        record(&self.0, "parent.build");
    }

    fn finish(&self, app: &mut App) {
        record(&self.0, "parent.finish");
        app.add_plugins(GameChild(Arc::clone(&self.0)));
    }

    fn cleanup(&self, _app: &mut App) {
        record(&self.0, "parent.cleanup");
    }

    fn name(&self) -> &str {
        self.1
    }
}

struct GameChild(Trace);

impl Plugin for GameChild {
    fn build(&self, _app: &mut App) {
        record(&self.0, "child.build");
    }

    fn finish(&self, _app: &mut App) {
        record(&self.0, "child.finish");
    }

    fn cleanup(&self, _app: &mut App) {
        record(&self.0, "child.cleanup");
    }

    fn name(&self) -> &str {
        "game-child"
    }
}

fn game_system() {}

fn teardown_owner(app: &mut App, owner: TypeOwner) {
    let world = app.world_mut();
    let Some(mut schedules) = world.remove_resource::<Schedules>() else {
        panic!("App keeps its Schedules resource installed during owner teardown");
    };
    let removed = schedules.remove_systems_in_set(
        Update,
        OwnerSet(owner),
        world,
        ScheduleCleanupPolicy::RemoveSystemsOnly,
    );
    world.remove_resource::<Schedules>();
    world.insert_resource(schedules);
    assert_eq!(removed.ok(), Some(1));
}

#[test]
fn two_game_owner_epochs_have_one_ordered_lifecycle_per_accepted_instance() {
    let trace = Trace::default();
    let mut app = App::new();
    app.add_plugins(EnginePlugin(Arc::clone(&trace)));

    let first = TypeOwner::Game(1);
    app.with_owner(first, |app| {
        app.add_plugins((
            (GameParent(Arc::clone(&trace), "game-parent"),),
            GameParent(Arc::clone(&trace), "game-parent"),
        ));
        app.add_systems(Update, game_system);
    });
    assert_eq!(snapshot(&trace), ["engine.build", "parent.build"]);

    app.with_owner(first, |app| app.finish_plugins());
    app.finish_plugins();
    app.finish_plugins();
    assert_eq!(
        snapshot(&trace),
        [
            "engine.build",
            "parent.build",
            "engine.finish",
            "parent.finish",
            "child.build",
            "child.finish",
            "engine.cleanup",
            "parent.cleanup",
            "child.cleanup",
        ]
    );
    assert!(app.has_plugin("engine"));
    assert!(app.has_plugin("game-parent"));
    assert!(app.has_plugin("game-child"));

    // Owner teardown is performed outside plugin callbacks; only then may its names be forgotten.
    teardown_owner(&mut app, first);
    app.forget_owner_plugins(first);
    assert!(app.has_plugin("engine"));
    assert!(!app.has_plugin("game-parent"));
    assert!(!app.has_plugin("game-child"));

    let second = TypeOwner::Game(2);
    app.with_owner(second, |app| {
        app.add_plugins((
            GameParent(Arc::clone(&trace), "game-parent"),
            GameParent(Arc::clone(&trace), "game-parent"),
        ));
        app.add_systems(Update, game_system);
    });
    app.finish_plugins();
    app.finish_plugins();
    assert_eq!(
        snapshot(&trace),
        [
            "engine.build",
            "parent.build",
            "engine.finish",
            "parent.finish",
            "child.build",
            "child.finish",
            "engine.cleanup",
            "parent.cleanup",
            "child.cleanup",
            "parent.build",
            "parent.finish",
            "child.build",
            "child.finish",
            "child.cleanup",
            "parent.cleanup",
        ]
    );
    assert!(app.has_plugin("engine"));
    assert!(app.has_plugin("game-parent"));
    assert!(app.has_plugin("game-child"));

    teardown_owner(&mut app, second);
    app.forget_owner_plugins(second);
    assert!(app.has_plugin("engine"));
    assert!(!app.has_plugin("game-parent"));
    assert!(!app.has_plugin("game-child"));
}
