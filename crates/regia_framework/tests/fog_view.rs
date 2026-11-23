// SPDX-License-Identifier: MPL-2.0
//! Per-viewer fog of war over the sim bridge (plan row S-16, ADR-8154).
//!
//! A toy sim with two teams runs behind `FogViewSim` inside a real `SimBridgePlugin` app. The
//! acceptance test checks that a hidden enemy never becomes an entity, a transform or extras in the
//! viewer's world, that it appears when a friendly watcher comes in range and disappears again,
//! and that events placed on it are dropped.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::fog_view::{
    FogEntity, FogEventPolicy, FogRules, FogViewControl, FogViewPlugin, FogViewSim, FogViewStatus, FogWorld,
    SightClass, TeamId, ViewerSnapshot,
};
use regia_framework::sim_bridge::{
    BridgeInput, EventPlace, ExternalSim, SimBridgePlugin, SimEntityView, SimEvent, SimExtras, SimId,
    SimSnapshot, SnapshotTick,
};

/// One sim entity.
#[derive(Debug, Clone)]
struct Row {
    id: u64,
    kind: &'static str,
    class: SightClass,
    team: TeamId,
    position: [f32; 3],
    alive: bool,
}

fn unit(id: u64, team: TeamId, x: f32) -> Row {
    Row {
        id,
        kind: "scout",
        class: SightClass::Unit,
        team,
        position: [x, 0.0, 0.0],
        alive: true,
    }
}

/// The full snapshot: every row, plus one extra per entity.
#[derive(Default)]
struct FullSnap {
    rows: Vec<Row>,
    /// Ids present in the render rows but given no fog row (to check "unlisted").
    unlisted: Vec<u64>,
}

impl SimSnapshot for FullSnap {
    fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
        for row in &self.rows {
            f(SimEntityView {
                id: row.id,
                kind: row.kind,
                team: row.team,
                translation: Vec3::from_array(row.position),
                rotation: Quat::IDENTITY,
            });
        }
        for &id in &self.unlisted {
            f(SimEntityView {
                id,
                kind: "ghost",
                team: 2,
                translation: Vec3::ZERO,
                rotation: Quat::IDENTITY,
            });
        }
    }

    fn extras_for(&self, id: u64, out: &mut Vec<(String, f32)>) {
        out.push(("turret_yaw".to_string(), id as f32));
    }
}

impl FogWorld for FullSnap {
    fn fog_entities(&self) -> impl Iterator<Item = FogEntity<'_>> {
        self.rows.iter().map(|row| FogEntity {
            id: row.id,
            kind: row.kind,
            class: row.class,
            team: row.team,
            position: row.position,
            alive: row.alive,
            kind_id: None,
        })
    }
}

/// An event the sim raises, placed where it says.
#[derive(Debug, Clone, PartialEq)]
struct Bang {
    label: &'static str,
    place: EventPlace,
}

/// The toy sim: rows it reports, events it raises once.
#[derive(Default)]
struct ToySim {
    rows: Vec<Row>,
    unlisted: Vec<u64>,
    events: Vec<Bang>,
    tick: u64,
}

impl ToySim {
    fn row_mut(&mut self, id: u64) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.id == id)
    }
}

impl ExternalSim for ToySim {
    type Snapshot = FullSnap;
    type Event = Bang;

    fn submit_input(&mut self, _input: &BridgeInput) {}

    fn read_snapshot(&mut self, _frame_time: Duration, out: &mut FullSnap) -> SnapshotTick {
        self.tick += 1;
        out.rows = self.rows.clone();
        out.unlisted = self.unlisted.clone();
        SnapshotTick(self.tick)
    }

    fn drain_events(&mut self, out: &mut Vec<Bang>) {
        out.append(&mut self.events);
    }

    fn event_kind(event: &Bang) -> String {
        event.label.to_string()
    }

    fn event_place(event: &Bang) -> EventPlace {
        event.place
    }
}

const OWN: u64 = 101;
const NEAR_ENEMY: u64 = 202;
const HIDDEN: u64 = 203;

/// Team 1's scout at the origin; team 2's scouts at 100 m (in sight) and 400 m (out of sight).
fn toy() -> ToySim {
    ToySim {
        rows: vec![
            unit(OWN, 1, 0.0),
            unit(NEAR_ENEMY, 2, 100.0),
            unit(HIDDEN, 2, 400.0),
        ],
        ..ToySim::default()
    }
}

fn app_viewing(sim: ToySim, viewer: TeamId) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        SimBridgePlugin::new(FogViewSim::new(sim, FogRules::default(), viewer)),
        FogViewPlugin::<ToySim>::default(),
    ));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app
}

/// Sim ids spawned in the presentation world, sorted.
fn spawned(app: &mut App) -> Vec<u64> {
    let world = app.world_mut();
    let mut ids: Vec<u64> = world.query::<&SimId>().iter(world).map(|s| s.0).collect();
    ids.sort_unstable();
    ids
}

/// Sim ids that carry extras in the presentation world, sorted.
fn with_extras(app: &mut App) -> Vec<u64> {
    let world = app.world_mut();
    let mut ids: Vec<u64> = world
        .query::<(&SimId, &SimExtras)>()
        .iter(world)
        .map(|(s, _)| s.0)
        .collect();
    ids.sort_unstable();
    ids
}

fn sim_mut(app: &mut App) -> Mut<'_, FogViewSim<ToySim>> {
    app.world_mut().resource_mut::<FogViewSim<ToySim>>()
}

#[derive(Resource, Default)]
struct Heard(Vec<&'static str>);

fn listen(mut reader: MessageReader<SimEvent<Bang>>, mut heard: ResMut<Heard>) {
    for event in reader.read() {
        heard.0.push(event.0.label);
    }
}

#[test]
fn hidden_unit_never_reaches_the_viewer_snapshot() {
    let mut app = app_viewing(toy(), 1);
    app.init_resource::<Heard>().add_systems(Update, listen);
    // Events on the hidden unit, at an unseen point, on a visible unit, and unplaced.
    sim_mut(&mut app).inner_mut().events = vec![
        Bang {
            label: "on_hidden",
            place: EventPlace::Entity(HIDDEN),
        },
        Bang {
            label: "at_unseen",
            place: EventPlace::At(Vec3::new(400.0, 0.0, 0.0)),
        },
        Bang {
            label: "on_seen",
            place: EventPlace::Entity(NEAR_ENEMY),
        },
        Bang {
            label: "unplaced",
            place: EventPlace::Unplaced,
        },
    ];
    for frame in 0..3 {
        app.update();
        // Never spawned, never given extras, on any frame.
        assert_eq!(spawned(&mut app), vec![OWN, NEAR_ENEMY], "frame {frame}");
        // A bare entity (no kind registry) gets its extras from its second snapshot on.
        let expect_extras = if frame == 0 { vec![] } else { vec![OWN, NEAR_ENEMY] };
        assert_eq!(with_extras(&mut app), expect_extras, "frame {frame}");
    }
    assert_eq!(app.world().resource::<Heard>().0, vec!["on_seen", "unplaced"]);

    // The viewer's snapshot itself holds no trace of it.
    let mut out = ViewerSnapshot::default();
    let full = {
        let mut sim = sim_mut(&mut app);
        let mut full = FullSnap::default();
        sim.inner_mut()
            .read_snapshot(Duration::from_millis(16), &mut full);
        sim.filter_into(&full, &mut out);
        full
    };
    assert!(!out.contains(HIDDEN));
    let mut extras = Vec::new();
    out.extras_for(HIDDEN, &mut extras);
    assert!(extras.is_empty());
    assert_eq!(full.rows.len(), 3, "the sim keeps the full state");

    let status = app.world().resource::<FogViewStatus>().clone();
    assert!(status.enabled);
    assert_eq!((status.viewer, status.observer), (1, false));
    assert_eq!(
        (status.stats.visible, status.stats.hidden, status.stats.unlisted),
        (2, 1, 0)
    );
    assert_eq!((status.stats.events_passed, status.stats.events_dropped), (2, 2));
    assert_eq!(status.stats.watchers, 1);
    assert_eq!(status.line_of_sight, "open_ground");
    assert_eq!(status.event_policy, "hide_unseen");
}

#[test]
fn the_hidden_unit_appears_in_range_and_despawns_out_of_range() {
    let mut app = app_viewing(toy(), 1);
    app.update();
    assert_eq!(spawned(&mut app), vec![OWN, NEAR_ENEMY]);
    // The friendly scout drives to 250 m: both enemies exactly at its 150 m sight radius.
    if let Some(row) = sim_mut(&mut app).inner_mut().row_mut(OWN) {
        row.position = [250.0, 0.0, 0.0];
    }
    app.update();
    assert_eq!(spawned(&mut app), vec![OWN, NEAR_ENEMY, HIDDEN]);
    // And back: the far enemy leaves the viewer's world again.
    if let Some(row) = sim_mut(&mut app).inner_mut().row_mut(OWN) {
        row.position = [0.0, 0.0, 0.0];
    }
    app.update();
    assert_eq!(spawned(&mut app), vec![OWN, NEAR_ENEMY]);
}

#[test]
fn each_viewer_gets_its_own_view_and_observers_see_all() {
    let mut team2 = app_viewing(toy(), 2);
    team2.update();
    // Team 2's scout at 100 m sees team 1's at the origin.
    assert_eq!(spawned(&mut team2), vec![OWN, NEAR_ENEMY, HIDDEN]);
    // Move team 1's scout away: team 2 loses it.
    if let Some(row) = sim_mut(&mut team2).inner_mut().row_mut(OWN) {
        row.position = [-500.0, 0.0, 0.0];
    }
    team2.update();
    assert_eq!(spawned(&mut team2), vec![NEAR_ENEMY, HIDDEN]);

    let mut observer = app_viewing(toy(), 0);
    observer.update();
    assert_eq!(spawned(&mut observer), vec![OWN, NEAR_ENEMY, HIDDEN]);
    assert!(observer.world().resource::<FogViewStatus>().observer);
}

#[test]
fn control_switches_the_viewer_and_the_fog() {
    let mut app = app_viewing(toy(), 1);
    app.update();
    assert_eq!(spawned(&mut app), vec![OWN, NEAR_ENEMY]);
    app.world_mut().resource_mut::<FogViewControl>().enabled = Some(false);
    app.update();
    assert_eq!(spawned(&mut app), vec![OWN, NEAR_ENEMY, HIDDEN]);
    assert!(!app.world().resource::<FogViewStatus>().enabled);
    {
        let mut control = app.world_mut().resource_mut::<FogViewControl>();
        control.enabled = Some(true);
        control.viewer = Some(3);
    }
    app.update();
    // Team 3 has no watchers: every team 1 and team 2 unit is out of its sight.
    assert_eq!(spawned(&mut app), Vec::<u64>::new());
    assert_eq!(app.world().resource::<FogViewStatus>().viewer, 3);
    assert_eq!(
        app.world().resource::<FogViewControl>().viewer,
        None,
        "requests are consumed"
    );
}

#[test]
fn unlisted_snapshot_rows_are_held_back_and_counted() {
    let mut sim = toy();
    sim.unlisted = vec![999];
    let mut app = app_viewing(sim, 1);
    app.update();
    assert!(!spawned(&mut app).contains(&999));
    let stats = app.world().resource::<FogViewStatus>().stats;
    assert_eq!((stats.hidden, stats.unlisted), (2, 1));
}

#[test]
fn pass_all_keeps_every_event_and_a_line_of_sight_hook_hides() {
    let mut sim = toy();
    sim.events = vec![Bang {
        label: "on_hidden",
        place: EventPlace::Entity(HIDDEN),
    }];
    let mut app = App::new();
    // A wall at x = 50 hides the near enemy too.
    let wall = |from: [f32; 3], to: [f32; 3]| (from[0] < 50.0) == (to[0] < 50.0);
    app.add_plugins((
        MinimalPlugins,
        SimBridgePlugin::new(
            FogViewSim::new(sim, FogRules::default(), 1)
                .with_event_policy(FogEventPolicy::PassAll)
                .with_line_of_sight("wall_x50", wall),
        ),
        FogViewPlugin::<ToySim>::default(),
    ));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.update();
    assert_eq!(spawned(&mut app), vec![OWN]);
    let status = app.world().resource::<FogViewStatus>().clone();
    assert_eq!(status.line_of_sight, "wall_x50");
    assert_eq!(status.event_policy, "pass_all");
    assert_eq!((status.stats.events_passed, status.stats.events_dropped), (1, 0));
}

#[test]
fn a_dead_enemy_shows_with_its_death_event() {
    let mut sim = toy();
    if let Some(row) = sim.row_mut(HIDDEN) {
        row.alive = false;
    }
    sim.events = vec![Bang {
        label: "wreck",
        place: EventPlace::Entity(HIDDEN),
    }];
    let mut app = app_viewing(sim, 1);
    app.init_resource::<Heard>().add_systems(Update, listen);
    app.update();
    assert_eq!(spawned(&mut app), vec![OWN, NEAR_ENEMY, HIDDEN]);
    assert_eq!(app.world().resource::<Heard>().0, vec!["wreck"]);
}

/// Fog v2 (plan ai-nav-18): a remembered structure leaves a ghost in the viewer's snapshot that
/// carries only its last sighting.
#[test]
fn a_ghost_in_the_viewer_snapshot_carries_only_the_last_sighting() {
    use regia_framework::fog_view::KindSight;

    const DEPOT: u64 = 301;
    let mut sim = toy();
    sim.rows.push(Row {
        id: DEPOT,
        kind: "depot",
        class: SightClass::Building,
        team: 2,
        position: [120.0, 0.0, 0.0],
        alive: true,
    });
    let mut rules = FogRules::default();
    rules.sight = rules.sight.with_row(KindSight::new("depot", 100.0).remembered());
    let mut view = FogViewSim::new(sim, rules, 1);
    let mut out = ViewerSnapshot::default();
    // The bridge's own path: one sim read, then the filter.
    let frame = |view: &mut FogViewSim<ToySim>, out: &mut ViewerSnapshot| {
        view.read_snapshot(Duration::from_millis(16), out);
    };
    // Seen: a live entity, no ghost.
    frame(&mut view, &mut out);
    assert!(out.contains(DEPOT));
    assert!(out.ghosts().is_empty());
    // The scout leaves; unseen, the depot is moved and renamed: the ghost keeps the sighting.
    let seen_tick = view.stats().tick;
    if let Some(row) = view.inner_mut().row_mut(OWN) {
        row.position = [-2000.0, 0.0, 0.0];
    }
    frame(&mut view, &mut out);
    if let Some(row) = view.inner_mut().row_mut(DEPOT) {
        row.position = [900.0, 0.0, 900.0];
        row.kind = "fortress";
    }
    frame(&mut view, &mut out);
    assert!(!out.contains(DEPOT), "the live depot never reaches the viewer");
    assert!(!out.contains(HIDDEN));
    assert_eq!(out.ghosts().len(), 1);
    let ghost = &out.ghosts()[0];
    assert_eq!(ghost.id, DEPOT);
    assert_eq!(ghost.kind, "depot");
    assert_eq!(ghost.translation, Vec3::new(120.0, 0.0, 0.0));
    assert_eq!(ghost.tick, seen_tick, "stamped with the sighting's sim tick");
    assert!(ghost.tick < view.stats().tick);
    let mut extras = Vec::new();
    out.extras_for(DEPOT, &mut extras);
    assert!(extras.is_empty(), "a ghost carries no extras");
    assert_eq!(view.stats().ghosts, 1);
    // The scout comes back and sees the old spot empty: the ghost goes.
    if let Some(row) = view.inner_mut().row_mut(OWN) {
        row.position = [0.0, 0.0, 0.0];
    }
    frame(&mut view, &mut out);
    assert!(out.ghosts().is_empty());
    assert_eq!(view.stats().ghosts_dropped, 1);
    assert!(view.stats().queries > 0);
}

/// KI-GP-018 (ADR-10070): with `with_explored_per_team` every player team's explored ground
/// accumulates each snapshot, and switching the viewer shows the new team's explored ground at once
/// and keeps the old team's; without it a switch starts an unexplored grid (the default, unchanged).
#[test]
fn explored_ground_accumulates_for_every_team_when_asked() {
    use regia_framework::fog_view::FogGrid;

    let full = FullSnap {
        rows: toy().rows,
        ..FullSnap::default()
    };
    let mut out = ViewerSnapshot::default();

    let mut plain = FogViewSim::new(toy(), FogRules::default(), 1).with_grid(FogGrid::covering(640.0, 640.0));
    plain.filter_into(&full, &mut out);
    assert!(
        plain.team_grid(2).is_none(),
        "only the viewer is tracked by default"
    );
    plain.set_viewer(2);
    assert_eq!(
        plain.grid().expect("grid").seen_count(),
        0,
        "a switch starts unexplored"
    );

    let mut sim = FogViewSim::new(toy(), FogRules::default(), 1)
        .with_grid(FogGrid::covering(640.0, 640.0))
        .with_explored_per_team();
    sim.filter_into(&full, &mut out);
    let team2 = sim.team_grid(2).expect("team 2 tracked");
    assert!(team2.seen_at(400.0, 0.0) && team2.seen_at(100.0, 0.0));
    assert!(!team2.seen_at(-300.0, 0.0), "team 2 never saw the west");
    let team1_seen = sim.grid().expect("grid").seen_count();
    assert!(sim.grid().expect("grid").seen_at(0.0, 0.0));

    sim.set_viewer(2);
    let now = sim.grid().expect("grid");
    assert!(
        now.seen_at(400.0, 0.0),
        "the new viewer's explored ground shows before the next snapshot"
    );
    assert!(!now.seen_at(-300.0, 0.0));
    let kept = sim.team_grid(1).expect("team 1 kept");
    assert_eq!(kept.seen_count(), team1_seen, "the old viewer's ground is kept");
    sim.set_viewer(1);
    assert_eq!(sim.grid().expect("grid").seen_count(), team1_seen);
}
