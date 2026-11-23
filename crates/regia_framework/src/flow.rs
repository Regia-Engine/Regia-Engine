// SPDX-License-Identifier: MPL-2.0
//! [`GameFlow`]: a ready-made top-level game state machine,
//! Boot → MainMenu → Loading → Playing ⇄ Paused, built on `regia_ecs::state`.
//!
//! [`GameFlowPlugin`] registers [`GameFlow`], the computed state [`InGame`] (playing or paused)
//! and the sub-state [`PausePage`] (which pause-menu page shows), and drives the automatic
//! steps:
//!
//! - **Boot** lasts one frame (its `OnEnter` runs before `Startup`), then goes to `MainMenu`, or
//!   straight to `Loading` with [`GameFlowSettings::skip_main_menu`].
//! - **MainMenu** waits for the game (its UI) to request `Loading`.
//! - **Loading** goes to `Playing` on the first frame [`LoadingTasks`] is empty. Systems in
//!   `OnEnter(GameFlow::Loading)` call [`LoadingTasks::begin`]; whatever finishes the work calls
//!   [`LoadingTasks::finish`].
//! - **Playing ⇄ Paused**: [`request_pause`] / [`toggle_pause`]. Entering `Paused` sets
//!   `Time::paused` (with [`GameFlowSettings::pause_time`]); leaving it clears it.
//!
//! - **Pacing**: each transition sets `regia_app::PacingState` ([`GameFlow::pacing_state`]:
//!   MainMenu → menu, Boot and Loading → loading, Paused → paused, Playing → play), so the
//!   host's `regia_app::PacingCaps` cap menus, loading pages and pauses (ADR-7889). The host may
//!   still set the state itself between transitions.
//!
//! Entities tagged `DespawnOnExit(InGame)` (level content) are despawned when play ends;
//! `DespawnOnExit(GameFlow::MainMenu)` scopes menu UI. Gate gameplay systems with
//! `.run_if(in_state(GameFlow::Playing))` or `.run_if(in_state(InGame))`. The agent protocol
//! reads and drives the flow through `state.get` / `state.set` (`book/agents/states.md`).

use std::collections::BTreeSet;

use regia_app::PacingState;
use regia_app::prelude::*;

/// The top-level flow of a game.
#[derive(States, Reflect, Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[reflect(Default, Debug, PartialEq)]
pub enum GameFlow {
    /// First frame: engine and game start-up.
    #[default]
    Boot,
    /// Title / main menu.
    MainMenu,
    /// Loading a level; ends when [`LoadingTasks`] is empty.
    Loading,
    /// In play.
    Playing,
    /// In play, paused.
    Paused,
}

impl GameFlow {
    /// True while playing or paused (the [`InGame`] computed state exists).
    pub fn is_in_game(self) -> bool {
        matches!(self, Self::Playing | Self::Paused)
    }

    /// The frame-cap state this flow state paces at (`regia_app::PacingCaps`): MainMenu is
    /// [`PacingState::Menu`], Boot and Loading are [`PacingState::Loading`], Paused is
    /// [`PacingState::Paused`] and Playing is [`PacingState::Play`].
    pub fn pacing_state(self) -> PacingState {
        match self {
            Self::Boot | Self::Loading => PacingState::Loading,
            Self::MainMenu => PacingState::Menu,
            Self::Playing => PacingState::Play,
            Self::Paused => PacingState::Paused,
        }
    }
}

/// Computed from [`GameFlow`]: exists while playing or paused. Scope level content with
/// `DespawnOnExit(InGame)` so pausing keeps it and quitting to the menu removes it.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[reflect(Debug, PartialEq)]
pub struct InGame;

impl ComputedStates for InGame {
    type SourceStates = GameFlow;

    // Playing ⇄ Paused recomputes the same `Some(InGame)`; that must not count as leaving it.
    const ALLOW_SAME_STATE_TRANSITIONS: bool = false;

    fn compute(flow: GameFlow) -> Option<Self> {
        flow.is_in_game().then_some(InGame)
    }
}

/// Sub-state of [`GameFlow::Paused`]: the pause-menu page. Exists only while paused and starts
/// at [`PausePage::Main`] each time the game pauses.
#[derive(SubStates, Reflect, Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[reflect(Default, Debug, PartialEq)]
#[source(GameFlow = GameFlow::Paused)]
pub enum PausePage {
    /// Resume / save / load / quit.
    #[default]
    Main,
    /// Settings.
    Options,
}

/// How [`GameFlowPlugin`] steps the flow.
#[derive(Resource, Reflect, Debug, Clone, PartialEq)]
#[reflect(Resource, Default, Debug)]
pub struct GameFlowSettings {
    /// Boot goes straight to `Loading` (games and samples without a title screen).
    pub skip_main_menu: bool,
    /// Entering `Paused` sets `Time::paused`; leaving it clears it.
    pub pause_time: bool,
    /// Entering `Paused` turns the renderer's scene hold on (`regia_render::SceneHold`, ADR-7890):
    /// the paused scene renders once and every later paused frame redraws only the UI over that
    /// image. Leaving `Paused` turns it off. Off by default (a pause screen that animates the
    /// world behind it needs full frames). Needs the `client` feature; without it the setting
    /// does nothing.
    pub hold_scene_when_paused: bool,
}

impl Default for GameFlowSettings {
    fn default() -> Self {
        Self {
            skip_main_menu: false,
            pause_time: true,
            hold_scene_when_paused: false,
        }
    }
}

/// Named pieces of work `Loading` waits for. `begin` in `OnEnter(GameFlow::Loading)` (or any time
/// while loading), `finish` when done; the flow moves to `Playing` on the first frame none are
/// pending.
#[derive(Resource, Default, Debug, Clone)]
pub struct LoadingTasks {
    pending: BTreeSet<String>,
}

impl LoadingTasks {
    /// Marks `task` as pending.
    pub fn begin(&mut self, task: impl Into<String>) {
        self.pending.insert(task.into());
    }

    /// Marks `task` as done. Unknown names are ignored.
    pub fn finish(&mut self, task: &str) {
        self.pending.remove(task);
    }

    /// True when nothing is pending.
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty()
    }

    /// The pending task names, sorted.
    pub fn pending(&self) -> impl Iterator<Item = &str> {
        self.pending.iter().map(String::as_str)
    }
}

/// Registers [`GameFlow`], [`InGame`], [`PausePage`], [`GameFlowSettings`] (kept if already
/// inserted) and [`LoadingTasks`], and the systems that step Boot and Loading and mirror `Paused`
/// into `Time::paused`.
#[derive(Default)]
pub struct GameFlowPlugin;

impl Plugin for GameFlowPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GameFlowSettings>()
            .register_type::<GameFlowSettings>()
            .init_resource::<LoadingTasks>()
            .init_state::<GameFlow>()
            .add_computed_state::<InGame>()
            .add_sub_state::<PausePage>()
            .add_systems(
                Update,
                (
                    leave_boot.run_if(in_state(GameFlow::Boot)),
                    finish_loading.run_if(in_state(GameFlow::Loading)),
                ),
            )
            .add_systems(OnEnter(GameFlow::Paused), freeze_time)
            .add_systems(OnExit(GameFlow::Paused), thaw_time);
        #[cfg(feature = "client")]
        app.add_systems(OnEnter(GameFlow::Paused), hold_scene)
            .add_systems(OnExit(GameFlow::Paused), release_scene);
        for flow in [
            GameFlow::Boot,
            GameFlow::MainMenu,
            GameFlow::Loading,
            GameFlow::Playing,
            GameFlow::Paused,
        ] {
            app.add_systems(OnEnter(flow), mirror_pacing_state);
        }
    }
}

/// `OnEnter(Paused)` with [`GameFlowSettings::hold_scene_when_paused`]: the scene hold on.
#[cfg(feature = "client")]
fn hold_scene(settings: Res<GameFlowSettings>, hold: Option<ResMut<regia_render::SceneHold>>) {
    if settings.hold_scene_when_paused
        && let Some(mut hold) = hold
    {
        hold.set(true);
    }
}

/// `OnExit(Paused)` with [`GameFlowSettings::hold_scene_when_paused`]: the scene hold off.
#[cfg(feature = "client")]
fn release_scene(settings: Res<GameFlowSettings>, hold: Option<ResMut<regia_render::SceneHold>>) {
    if settings.hold_scene_when_paused
        && let Some(mut hold) = hold
    {
        hold.set(false);
    }
}

/// `OnEnter` of every [`GameFlow`] state: sets `regia_app::PacingState` from the new state.
fn mirror_pacing_state(
    flow: Res<State<GameFlow>>,
    pacing: Option<ResMut<PacingState>>,
    mut commands: Commands,
) {
    let state = flow.get().pacing_state();
    match pacing {
        Some(mut pacing) => {
            pacing.set_if_neq(state);
        }
        None => commands.insert_resource(state),
    }
}

/// `Update` in `Boot`: on to `MainMenu`, or `Loading` when the menu is skipped.
fn leave_boot(settings: Res<GameFlowSettings>, mut next: ResMut<NextState<GameFlow>>) {
    next.set_if_different(if settings.skip_main_menu {
        GameFlow::Loading
    } else {
        GameFlow::MainMenu
    });
}

/// `Update` in `Loading`: on to `Playing` once no loading task is pending.
fn finish_loading(tasks: Res<LoadingTasks>, mut next: ResMut<NextState<GameFlow>>) {
    if tasks.is_idle() {
        next.set_if_different(GameFlow::Playing);
    }
}

fn freeze_time(settings: Res<GameFlowSettings>, mut time: ResMut<Time>) {
    if settings.pause_time {
        time.paused = true;
    }
}

fn thaw_time(settings: Res<GameFlowSettings>, mut time: ResMut<Time>) {
    if settings.pause_time {
        time.paused = false;
    }
}

/// Requests `Paused` (`paused = true`) or `Playing`, applied on the next `StateTransition`
/// (after the next `PreUpdate`). Does nothing outside play (Boot, MainMenu, Loading), when the
/// flow is already there, or when [`GameFlowPlugin`] is not installed. Returns whether a
/// transition was requested.
pub fn request_pause(world: &mut World, paused: bool) -> bool {
    let Some(current) = world.get_resource::<State<GameFlow>>().map(|s| *s.get()) else {
        return false;
    };
    let target = if paused {
        GameFlow::Paused
    } else {
        GameFlow::Playing
    };
    if !current.is_in_game() || current == target {
        return false;
    }
    match world.get_resource_mut::<NextState<GameFlow>>() {
        Some(mut next) => {
            next.set_if_different(target);
            true
        }
        None => false,
    }
}

/// Requests the opposite of the current Playing/Paused flow (see [`request_pause`]).
pub fn toggle_pause(world: &mut World) -> bool {
    let paused = world
        .get_resource::<State<GameFlow>>()
        .is_some_and(|s| *s.get() == GameFlow::Paused);
    request_pause(world, !paused)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(settings: GameFlowSettings) -> App {
        let mut app = App::new();
        app.insert_resource(settings).add_plugins(GameFlowPlugin);
        app
    }

    fn flow(app: &App) -> GameFlow {
        *app.world().resource::<State<GameFlow>>().get()
    }

    #[test]
    fn boot_goes_to_the_main_menu_then_waits() {
        let mut app = app(GameFlowSettings::default());
        app.update();
        assert_eq!(flow(&app), GameFlow::Boot);
        app.update();
        assert_eq!(flow(&app), GameFlow::MainMenu);
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(flow(&app), GameFlow::MainMenu, "the menu waits for the game");
        app.world_mut()
            .resource_mut::<NextState<GameFlow>>()
            .set(GameFlow::Loading);
        app.update();
        assert_eq!(flow(&app), GameFlow::Loading);
        app.update();
        assert_eq!(flow(&app), GameFlow::Playing);
    }

    #[test]
    fn loading_waits_for_its_tasks() {
        let mut app = app(GameFlowSettings {
            skip_main_menu: true,
            ..Default::default()
        });
        app.add_systems(OnEnter(GameFlow::Loading), |mut tasks: ResMut<LoadingTasks>| {
            tasks.begin("level");
            tasks.begin("audio");
        });
        app.update();
        app.update();
        assert_eq!(flow(&app), GameFlow::Loading);
        app.world_mut().resource_mut::<LoadingTasks>().finish("level");
        app.update();
        app.update();
        assert_eq!(flow(&app), GameFlow::Loading);
        assert_eq!(
            app.world()
                .resource::<LoadingTasks>()
                .pending()
                .collect::<Vec<_>>(),
            ["audio"]
        );
        app.world_mut().resource_mut::<LoadingTasks>().finish("audio");
        app.update();
        app.update();
        assert_eq!(flow(&app), GameFlow::Playing);
    }

    fn effective_fps(app: &App) -> Option<f32> {
        app.world().resource::<FramePacing>().effective_fps()
    }

    #[test]
    fn the_flow_sets_the_pacing_state_and_its_cap() {
        let mut app = app(GameFlowSettings::default());
        app.insert_resource(regia_app::PacingCaps {
            menu: Some(60.0),
            loading: Some(50.0),
            paused: Some(30.0),
            play: None,
        });
        app.world_mut().resource_mut::<FramePacing>().target_fps = Some(144.0);
        app.update();
        assert_eq!(
            *app.world().resource::<PacingState>(),
            PacingState::Loading,
            "boot"
        );
        assert_eq!(effective_fps(&app), Some(50.0));
        app.update();
        assert_eq!(flow(&app), GameFlow::MainMenu);
        assert_eq!(*app.world().resource::<PacingState>(), PacingState::Menu);
        assert_eq!(effective_fps(&app), Some(60.0), "MainMenu paces at the menu cap");
        app.world_mut()
            .resource_mut::<NextState<GameFlow>>()
            .set(GameFlow::Loading);
        app.update();
        assert_eq!(effective_fps(&app), Some(50.0));
        app.update();
        assert_eq!(flow(&app), GameFlow::Playing);
        assert_eq!(*app.world().resource::<PacingState>(), PacingState::Play);
        assert_eq!(
            effective_fps(&app),
            Some(144.0),
            "Playing paces at the player cap"
        );
        assert!(toggle_pause(app.world_mut()));
        app.update();
        assert_eq!(effective_fps(&app), Some(30.0));
        assert!(toggle_pause(app.world_mut()));
        app.update();
        assert_eq!(effective_fps(&app), Some(144.0));
    }

    #[test]
    fn pacing_states_of_every_flow_state() {
        assert_eq!(GameFlow::Boot.pacing_state(), PacingState::Loading);
        assert_eq!(GameFlow::MainMenu.pacing_state(), PacingState::Menu);
        assert_eq!(GameFlow::Loading.pacing_state(), PacingState::Loading);
        assert_eq!(GameFlow::Playing.pacing_state(), PacingState::Play);
        assert_eq!(GameFlow::Paused.pacing_state(), PacingState::Paused);
    }

    #[test]
    fn pause_freezes_time_and_keeps_in_game_content() {
        let mut app = app(GameFlowSettings {
            skip_main_menu: true,
            ..Default::default()
        });
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(flow(&app), GameFlow::Playing);
        let level = app.world_mut().spawn(DespawnOnExit(InGame)).id();

        assert!(toggle_pause(app.world_mut()));
        app.update();
        assert_eq!(flow(&app), GameFlow::Paused);
        assert!(app.world().resource::<Time>().paused);
        assert_eq!(app.world().resource::<State<PausePage>>().get(), &PausePage::Main);
        assert!(
            app.world().get_entity(level).is_ok(),
            "pausing keeps level content"
        );
        assert!(!request_pause(app.world_mut(), true), "already paused");

        assert!(toggle_pause(app.world_mut()));
        app.update();
        assert_eq!(flow(&app), GameFlow::Playing);
        assert!(!app.world().resource::<Time>().paused);
        assert!(app.world().get_resource::<State<PausePage>>().is_none());

        app.world_mut()
            .resource_mut::<NextState<GameFlow>>()
            .set(GameFlow::MainMenu);
        app.update();
        assert!(
            app.world().get_entity(level).is_err(),
            "quitting play removes level content"
        );
        assert!(!request_pause(app.world_mut(), true), "no pausing in the menu");
    }

    /// `hold_scene_when_paused` (ADR-7890) turns the renderer's scene hold on for the pause and
    /// off after it; without it the pause leaves the hold alone.
    #[cfg(feature = "client")]
    #[test]
    fn hold_scene_when_paused_turns_the_scene_hold_on_for_the_pause() {
        for setting in [true, false] {
            let mut app = app(GameFlowSettings {
                skip_main_menu: true,
                hold_scene_when_paused: setting,
                ..Default::default()
            });
            app.init_resource::<regia_render::SceneHold>();
            for _ in 0..3 {
                app.update();
            }
            assert!(!app.world().resource::<regia_render::SceneHold>().is_on());
            assert!(toggle_pause(app.world_mut()));
            app.update();
            assert_eq!(flow(&app), GameFlow::Paused);
            assert_eq!(
                app.world().resource::<regia_render::SceneHold>().is_on(),
                setting,
                "hold_scene_when_paused: {setting}"
            );
            assert!(toggle_pause(app.world_mut()));
            app.update();
            assert_eq!(flow(&app), GameFlow::Playing);
            assert!(
                !app.world().resource::<regia_render::SceneHold>().is_on(),
                "playing again renders full frames"
            );
        }
    }
}
