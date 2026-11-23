// SPDX-License-Identifier: MPL-2.0
//! Game states on Regia Engine's `App` (`bevy_state` adopted, lane adopt-states): transition order,
//! computed and sub-state correctness, state-scoped despawn, and the frame position of the
//! `StateTransition` schedule.

use regia_app::prelude::*;
use regia_ecs::state::{ReflectFreelyMutableState, ReflectState};

#[derive(States, Reflect, Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Flow {
    #[default]
    Menu,
    Playing,
    Paused,
}

/// Exists only while `Flow::Paused`.
#[derive(SubStates, Reflect, Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[source(Flow = Flow::Paused)]
enum PausePage {
    #[default]
    Main,
    Options,
}

/// `Some` while playing or paused.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct InGame;

impl ComputedStates for InGame {
    type SourceStates = Flow;

    // Playing <-> Paused recomputes `Some(InGame)` again; without this, `bevy_state` treats
    // that as a same-value transition and re-runs OnExit/OnEnter (and DespawnOnExit) of InGame.
    const ALLOW_SAME_STATE_TRANSITIONS: bool = false;

    fn compute(flow: Flow) -> Option<Self> {
        matches!(flow, Flow::Playing | Flow::Paused).then_some(InGame)
    }
}

#[derive(Resource, Default)]
struct Log(Vec<&'static str>);

fn log(name: &'static str) -> impl FnMut(ResMut<Log>) {
    move |mut l: ResMut<Log>| l.0.push(name)
}

fn app() -> App {
    let mut app = App::new();
    app.init_resource::<Log>().init_state::<Flow>();
    app
}

fn set(app: &mut App, flow: Flow) {
    app.world_mut().resource_mut::<NextState<Flow>>().set(flow);
}

fn current(app: &App) -> Flow {
    *app.world().resource::<State<Flow>>().get()
}

#[test]
fn initial_state_enters_before_startup() {
    let mut app = app();
    app.add_systems(OnEnter(Flow::Menu), log("enter menu"))
        .add_systems(Startup, log("startup"))
        .add_systems(Update, log("update"));
    app.update();
    assert_eq!(
        app.world().resource::<Log>().0,
        ["enter menu", "startup", "update"]
    );
    app.update();
    assert_eq!(
        app.world().resource::<Log>().0,
        ["enter menu", "startup", "update", "update"],
        "OnEnter runs once"
    );
}

#[test]
fn transition_runs_exit_then_transition_then_enter() {
    let mut app = app();
    app.add_systems(OnExit(Flow::Menu), log("exit menu"))
        .add_systems(
            OnTransition {
                exited: Flow::Menu,
                entered: Flow::Playing,
            },
            log("menu -> playing"),
        )
        .add_systems(OnEnter(Flow::Playing), log("enter playing"))
        .add_systems(Update, log("update"));
    app.update();
    app.world_mut().resource_mut::<Log>().0.clear();

    // A request made during Update is applied after the next PreUpdate, before that frame's Update.
    set(&mut app, Flow::Playing);
    app.update();
    assert_eq!(
        app.world().resource::<Log>().0,
        ["exit menu", "menu -> playing", "enter playing", "update"]
    );
    assert_eq!(current(&app), Flow::Playing);
    assert_eq!(app.world().resource::<PreviousState<Flow>>().get(), &Flow::Menu);
}

#[test]
fn state_transition_runs_after_pre_update() {
    let mut app = app();
    app.update();
    app.add_systems(PreUpdate, |mut next: ResMut<NextState<Flow>>| {
        next.set(Flow::Playing)
    })
    .add_systems(OnEnter(Flow::Playing), log("enter playing"))
    .add_systems(PreUpdate, log("pre"))
    .add_systems(Update, log("update"));
    app.update();
    assert_eq!(
        app.world().resource::<Log>().0,
        ["pre", "enter playing", "update"]
    );
}

#[test]
fn in_state_gates_systems() {
    let mut app = app();
    app.add_systems(Update, log("playing only").run_if(in_state(Flow::Playing)));
    app.update();
    assert!(app.world().resource::<Log>().0.is_empty());
    set(&mut app, Flow::Playing);
    app.update();
    app.update();
    assert_eq!(app.world().resource::<Log>().0, ["playing only", "playing only"]);
}

#[test]
fn computed_state_follows_its_source() {
    let mut app = app();
    app.add_computed_state::<InGame>()
        .add_systems(OnEnter(InGame), log("enter in-game"))
        .add_systems(OnExit(InGame), log("exit in-game"));
    app.update();
    assert!(
        app.world().get_resource::<State<InGame>>().is_none(),
        "absent in the menu"
    );

    set(&mut app, Flow::Playing);
    app.update();
    assert!(app.world().get_resource::<State<InGame>>().is_some());
    // Playing -> Paused keeps InGame: no exit/enter.
    set(&mut app, Flow::Paused);
    app.update();
    assert!(app.world().get_resource::<State<InGame>>().is_some());
    set(&mut app, Flow::Menu);
    app.update();
    assert!(app.world().get_resource::<State<InGame>>().is_none());
    assert_eq!(app.world().resource::<Log>().0, ["enter in-game", "exit in-game"]);
}

#[test]
fn sub_state_exists_only_under_its_source() {
    let mut app = app();
    app.add_sub_state::<PausePage>()
        .add_systems(OnEnter(PausePage::Options), log("enter options"))
        .add_systems(OnExit(PausePage::Options), log("exit options"));
    app.update();
    assert!(app.world().get_resource::<State<PausePage>>().is_none());

    // Setting a sub-state whose source does not match does nothing.
    app.world_mut()
        .resource_mut::<NextState<PausePage>>()
        .set(PausePage::Options);
    app.update();
    assert!(app.world().get_resource::<State<PausePage>>().is_none());

    set(&mut app, Flow::Paused);
    app.update();
    assert_eq!(
        app.world().resource::<State<PausePage>>().get(),
        &PausePage::Main,
        "starts at its default"
    );
    app.world_mut()
        .resource_mut::<NextState<PausePage>>()
        .set(PausePage::Options);
    app.update();
    assert_eq!(
        app.world().resource::<State<PausePage>>().get(),
        &PausePage::Options
    );

    // Leaving the source removes the sub-state and runs its OnExit.
    set(&mut app, Flow::Playing);
    app.update();
    assert!(app.world().get_resource::<State<PausePage>>().is_none());
    // Re-entering starts at the default again.
    set(&mut app, Flow::Paused);
    app.update();
    assert_eq!(app.world().resource::<State<PausePage>>().get(), &PausePage::Main);
    assert_eq!(app.world().resource::<Log>().0, ["enter options", "exit options"]);
}

#[test]
fn despawn_on_exit_scopes_entities() {
    let mut app = app();
    app.add_computed_state::<InGame>();
    app.update();
    set(&mut app, Flow::Playing);
    app.update();
    let level = app.world_mut().spawn(DespawnOnExit(InGame)).id();
    let hud = app.world_mut().spawn(DespawnOnExit(Flow::Playing)).id();
    let keep = app.world_mut().spawn_empty().id();

    // Playing -> Paused: leaves Flow::Playing, stays InGame.
    set(&mut app, Flow::Paused);
    app.update();
    assert!(
        app.world().get_entity(hud).is_err(),
        "despawned on exit of Flow::Playing"
    );
    assert!(app.world().get_entity(level).is_ok(), "InGame not left yet");

    set(&mut app, Flow::Menu);
    app.update();
    assert!(
        app.world().get_entity(level).is_err(),
        "despawned on exit of InGame"
    );
    assert!(app.world().get_entity(keep).is_ok());
}

#[test]
fn insert_state_sets_the_initial_value() {
    let mut app = App::new();
    app.init_resource::<Log>()
        .insert_state(Flow::Playing)
        .add_systems(OnEnter(Flow::Playing), log("enter playing"))
        .add_systems(OnEnter(Flow::Menu), log("enter menu"));
    app.update();
    assert_eq!(current(&app), Flow::Playing);
    assert_eq!(app.world().resource::<Log>().0, ["enter playing"]);
}

#[test]
fn double_init_keeps_the_first() {
    let mut app = app();
    app.update();
    set(&mut app, Flow::Paused);
    app.update();
    app.init_state::<Flow>();
    app.update();
    assert_eq!(current(&app), Flow::Paused);
}

#[test]
fn states_are_reflected_for_the_agent() {
    let mut app = app();
    app.add_computed_state::<InGame>().add_sub_state::<PausePage>();
    app.update();
    let registry = app.type_registry().read();
    let flow = registry.get(std::any::TypeId::of::<Flow>());
    assert!(flow.is_some_and(|r| r.data::<ReflectState>().is_some()));
    assert!(flow.is_some_and(|r| r.data::<ReflectFreelyMutableState>().is_some()));
    let in_game = registry.get(std::any::TypeId::of::<InGame>());
    assert!(in_game.is_some_and(|r| r.data::<ReflectState>().is_some()));
    assert!(
        in_game.is_some_and(|r| r.data::<ReflectFreelyMutableState>().is_none()),
        "computed states are not settable"
    );
    let page = registry.get(std::any::TypeId::of::<PausePage>());
    assert!(page.is_some_and(|r| r.data::<ReflectFreelyMutableState>().is_some()));
    let reflected = flow
        .and_then(|r| r.data::<ReflectState>())
        .and_then(|data| data.reflect(app.world()))
        .and_then(|value| value.downcast_ref::<Flow>().copied());
    assert_eq!(reflected, Some(Flow::Menu));
}

#[test]
fn apply_state_transitions_outside_the_frame() {
    let mut app = app();
    app.update();
    set(&mut app, Flow::Playing);
    app.apply_state_transitions();
    assert_eq!(current(&app), Flow::Playing);
}

#[test]
fn a_sub_state_registered_before_its_source_does_not_abort() {
    let mut app = App::new();
    app.add_sub_state::<PausePage>().add_computed_state::<InGame>();
    app.init_state::<Flow>();
    app.update();
    assert!(app.world().get_resource::<State<PausePage>>().is_none());
    set(&mut app, Flow::Paused);
    app.update();
    assert!(app.world().get_resource::<State<PausePage>>().is_some());
    assert!(app.world().get_resource::<State<InGame>>().is_some());
}

#[test]
fn insert_state_over_a_registered_state_cancels_a_pending_request() {
    #[derive(Resource, Default)]
    struct Entered(Vec<Flow>);
    let mut app = app();
    app.init_resource::<Entered>();
    app.add_systems(OnEnter(Flow::Menu), |mut e: ResMut<Entered>| e.0.push(Flow::Menu))
        .add_systems(OnEnter(Flow::Paused), |mut e: ResMut<Entered>| {
            e.0.push(Flow::Paused)
        })
        .add_systems(OnExit(Flow::Paused), |mut e: ResMut<Entered>| {
            e.0.push(Flow::Playing)
        });
    app.update();
    app.world_mut().resource_mut::<Entered>().0.clear();
    set(&mut app, Flow::Playing); // pending request
    app.insert_state(Flow::Paused); // level load replaces the state
    app.apply_state_transitions();
    assert_eq!(current(&app), Flow::Paused, "the pending request is dropped");
    assert_eq!(
        app.world().resource::<Entered>().0,
        [Flow::Paused],
        "enter runs, no stray exit"
    );
}
