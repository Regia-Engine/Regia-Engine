// SPDX-License-Identifier: MPL-2.0
//! The mock backend: an in-memory [`SteamBackend`] for tests and dev harnesses. Inject
//! [`SteamEvent`]s and inspect what the game asked Steam to do — no Steam client, no SDK.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use regia_framework::leaderboard::{
    LeaderboardError, LeaderboardHandle, LeaderboardRange, LeaderboardService, LeaderboardSpec,
    LocalLeaderboards, RequestId, ServiceEvent, UgcHandle, UploadMethod,
};

use crate::backend::{AppId, SteamBackend, SteamError, SteamEvent, SteamId, SteamState};
use crate::services::{AppOwnership, TimelineCall};

/// In-memory Steam backend. Share it between the test (via [`MockSteam::shared`]) and the plugin
/// (`SteamPlugin::with_backend`), drive the frame pump, and assert on the recorded calls.
#[derive(Debug, Default)]
pub struct MockSteam {
    state: Mutex<SteamState>,
    app_id: Mutex<Option<AppId>>,
    events: Mutex<Vec<SteamEvent>>,
    achievements: Mutex<BTreeMap<String, bool>>,
    stats_i32: Mutex<BTreeMap<String, i32>>,
    stats_f32: Mutex<BTreeMap<String, f32>>,
    presence: Mutex<BTreeMap<String, Option<String>>>,
    deck: Mutex<bool>,
    language: Mutex<String>,
    game_language: Mutex<String>,
    steam_id: Mutex<Option<SteamId>>,
    unlocked_calls: Mutex<Vec<String>>,
    presence_calls: Mutex<Vec<(String, Option<String>)>>,
    store_calls: Mutex<u32>,
    stats_waits: Mutex<u32>,
    reject_unlocks: Mutex<bool>,
    reset_calls: Mutex<Vec<bool>>,
    cloud: Mutex<BTreeMap<String, Vec<u8>>>,
    offline: Mutex<bool>,
    init_calls: Mutex<u32>,
    shutdown_calls: Mutex<u32>,
    restart_required: Mutex<bool>,
    restart_checks: Mutex<Vec<AppId>>,
    achievement_query_fails: Mutex<bool>,
    achievement_queries: Mutex<u32>,
    input: Mutex<MockInput>,
    keyboard_shown: Mutex<Vec<crate::deck_prep::KeyboardRequest>>,
    keyboard_dismissed: Mutex<u32>,
    net: Mutex<Option<MockNetLink>>,
    leaderboards: Mutex<Option<Arc<LocalLeaderboards>>>,
    apps: Mutex<BTreeMap<u32, AppOwnership>>,
    timeline_calls: Mutex<Vec<TimelineCall>>,
    overlay_calls: Mutex<Vec<String>>,
}

/// The mock's leaderboard service as Steam: a [`LocalLeaderboards`] that reports `steam`, so a
/// game or agent sees exactly what it would on the real client.
struct MockLeaderboards(Arc<LocalLeaderboards>);

impl LeaderboardService for MockLeaderboards {
    fn name(&self) -> &'static str {
        "steam"
    }
    fn local_user(&self) -> Option<u64> {
        self.0.local_user()
    }
    fn find(&self, spec: &LeaderboardSpec) -> Result<RequestId, LeaderboardError> {
        self.0.find(spec)
    }
    fn upload(
        &self,
        board: LeaderboardHandle,
        method: UploadMethod,
        score: i32,
        details: &[i32],
    ) -> Result<RequestId, LeaderboardError> {
        self.0.upload(board, method, score, details)
    }
    fn download(
        &self,
        board: LeaderboardHandle,
        range: LeaderboardRange,
    ) -> Result<RequestId, LeaderboardError> {
        self.0.download(board, range)
    }
    fn attach_ugc(&self, board: LeaderboardHandle, ugc: UgcHandle) -> Result<RequestId, LeaderboardError> {
        self.0.attach_ugc(board, ugc)
    }
    fn poll(&self) -> Vec<ServiceEvent> {
        self.0.poll()
    }
}

/// This user's link to a [`crate::mock_net::MockSteamNet`] (lobbies and messages, spec 89).
struct MockNetLink {
    lobbies: Arc<dyn regia_net::lobby::LobbyBackend>,
    messages: Arc<dyn crate::sockets::P2pMessages>,
}

impl std::fmt::Debug for MockNetLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MockNetLink")
    }
}

/// The mock's Steam Input state: what the "controller" reports and what the game asked for.
#[derive(Debug, Default)]
struct MockInput {
    init_calls: u32,
    controllers: usize,
    digital: BTreeMap<String, bool>,
    analog: BTreeMap<String, (f32, f32)>,
    activated: Vec<String>,
    activation_fails: bool,
    /// Calls that looked an action up by a freshly built name (`input_digital` / `input_analog`
    /// and the handle resolvers). Handle reads do not count.
    name_lookups: u64,
    /// Stable non-zero handle per Steam action name. `0` is reserved for "resolved absent".
    handles: BTreeMap<String, u64>,
    next_handle: u64,
    digital_by_handle: BTreeMap<u64, bool>,
    analog_by_handle: BTreeMap<u64, (f32, f32)>,
    /// The `ESteamInputType` number `input_controller_type` reports (ADR-1332); `None` by default.
    controller_type: Option<u32>,
}

fn assign_handle(input: &mut MockInput, action: &str) -> u64 {
    if let Some(handle) = input.handles.get(action).copied() {
        return handle;
    }
    let handle = input.next_handle.max(1);
    input.next_handle = handle + 1;
    input.handles.insert(action.to_owned(), handle);
    if let Some(down) = input.digital.get(action).copied() {
        input.digital_by_handle.insert(handle, down);
    }
    if let Some(value) = input.analog.get(action).copied() {
        input.analog_by_handle.insert(handle, value);
    }
    handle
}

impl MockSteam {
    /// Valid overlay page requests received by this backend, in call order.
    pub fn overlay_calls(&self) -> Vec<String> {
        self.overlay_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    /// A shared mock in the [`SteamState::Ready`] state.
    pub fn shared() -> Arc<Self> {
        let mock = Arc::new(Self::default());
        *mock.state.lock().unwrap_or_else(|e| e.into_inner()) = SteamState::Ready;
        mock
    }

    /// A shared mock in the [`SteamState::Unavailable`] state: Steam is not running.
    pub fn shared_unavailable() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Makes the shipping restart check ([`SteamBackend::restart_app_if_necessary`]) answer
    /// `restart`: `true` plays a game that was not launched through Steam, so the plugin exits
    /// before initializing (ADR-5375).
    pub fn set_restart_required(&self, restart: bool) {
        *self.restart_required.lock().unwrap_or_else(|e| e.into_inner()) = restart;
    }

    /// The app ids the plugin asked the restart check with, in order.
    pub fn restart_checks(&self) -> Vec<AppId> {
        self.restart_checks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Queues an event for the next `pump_steam` pass (the plugin's frame pump), as if Steam had
    /// sent it.
    pub fn inject(&self, event: SteamEvent) {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push(event);
    }

    /// Stores `data` as the remote copy of `name` and raises [`SteamEvent::CloudConflict`].
    ///
    /// `remote_newer` is the flag the game reads on [`crate::CloudConflict`]: the mock does
    /// not invent timestamps. Tests use this to surface a conflict the way a second machine
    /// (or a suspended session) would.
    pub fn sync_remote_file(&self, name: &str, data: &[u8], remote_newer: bool) {
        self.cloud
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_owned(), data.to_vec());
        self.inject(SteamEvent::CloudConflict {
            file: name.to_owned(),
            remote_newer,
        });
    }

    /// Makes the mock report Steam's offline mode (running but not logged on).
    pub fn set_offline(&self, offline: bool) {
        *self.offline.lock().unwrap_or_else(|e| e.into_inner()) = offline;
    }

    /// Makes `achievement_unlocked` fail with a rejection while the mock is otherwise ready (a
    /// Steam query error, as opposed to "not unlocked").
    pub fn set_achievement_query_fails(&self, fails: bool) {
        *self
            .achievement_query_fails
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = fails;
    }

    fn input_state(&self) -> std::sync::MutexGuard<'_, MockInput> {
        self.input.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sets how many controllers the mock's Steam Input reports.
    pub fn set_input_controllers(&self, count: usize) {
        self.input_state().controllers = count;
    }

    /// Sets a digital Steam Input action (by its Steam action name).
    pub fn set_input_digital(&self, action: &str, down: bool) {
        let mut input = self.input_state();
        if let Some(handle) = input.handles.get(action).copied() {
            input.digital_by_handle.insert(handle, down);
        }
        input.digital.insert(action.to_owned(), down);
    }

    /// Sets an analog Steam Input action (by its Steam action name).
    pub fn set_input_analog(&self, action: &str, value: (f32, f32)) {
        let mut input = self.input_state();
        if let Some(handle) = input.handles.get(action).copied() {
            input.analog_by_handle.insert(handle, value);
        }
        input.analog.insert(action.to_owned(), value);
    }

    /// Sets the `ESteamInputType` number the mock's first controller reports (`None`: unknown).
    pub fn set_input_controller_type(&self, input_type: Option<u32>) {
        self.input_state().controller_type = input_type;
    }

    /// Every action set the game activated, in order (repeats included).
    pub fn input_activations(&self) -> Vec<String> {
        self.input_state().activated.clone()
    }

    /// Makes `input_activate_action_set` fail with a rejection (Steam does this while the action
    /// manifest is not loaded yet). Failed calls are not recorded in [`Self::input_activations`].
    /// While it fails the manifest counts as not loaded, so action handles resolve to `0` and
    /// name reads return `None`, as Steam does before the configuration arrives.
    pub fn set_input_activation_fails(&self, fails: bool) {
        self.input_state().activation_fails = fails;
    }

    /// How many times Steam Input was initialized.
    pub fn input_init_calls(&self) -> u32 {
        self.input_state().init_calls
    }

    /// How many times an action was looked up by name (not by a cached handle).
    pub fn input_name_lookups(&self) -> u64 {
        self.input_state().name_lookups
    }

    /// How many times the plugin initialized the backend (a host must see exactly one).
    pub fn init_calls(&self) -> u32 {
        *self.init_calls.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How many times the backend was asked to shut down (including repeats).
    pub fn shutdown_calls(&self) -> u32 {
        *self.shutdown_calls.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Changes the state the backend reports (e.g. simulate Steam shutting down mid-session).
    pub fn set_state(&self, state: SteamState) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = state;
    }

    /// Sets the app id the backend reports.
    pub fn set_app_id(&self, app_id: Option<AppId>) {
        *self.app_id.lock().unwrap_or_else(|e| e.into_inner()) = app_id;
    }

    /// Puts this user on a shared [`crate::mock_net::MockSteamNet`] as Steam user `id` (display
    /// name `name`): its lobbies and peer-to-peer messages then reach every other user on that
    /// network (spec 89 § 11). Also sets the reported Steam id.
    pub fn join_net(&self, net: &crate::mock_net::MockSteamNet, id: SteamId, name: &str) {
        let (lobbies, messages) = net.attach(id, name);
        *self.net.lock().unwrap_or_else(|e| e.into_inner()) = Some(MockNetLink { lobbies, messages });
        self.set_steam_id(Some(id));
    }

    /// Sets the local user's Steam id the backend reports.
    pub fn set_steam_id(&self, id: Option<SteamId>) {
        *self.steam_id.lock().unwrap_or_else(|e| e.into_inner()) = id;
        if let (Some(id), Some(store)) = (
            id,
            self.leaderboards
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref(),
        ) {
            store.set_local_user(id.0);
        }
    }

    /// How many times the game asked the backend whether an achievement is unlocked.
    pub fn achievement_query_count(&self) -> u32 {
        *self.achievement_queries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every floating-keyboard request the game made, in order.
    pub fn keyboard_requests(&self) -> Vec<crate::deck_prep::KeyboardRequest> {
        self.keyboard_shown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// How many times the game dismissed the floating keyboard.
    pub fn keyboard_dismissals(&self) -> u32 {
        *self.keyboard_dismissed.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sets the Steam Deck detection the backend reports.
    pub fn set_deck(&self, deck: bool) {
        *self.deck.lock().unwrap_or_else(|e| e.into_inner()) = deck;
    }

    /// Sets the language the backend reports.
    pub fn set_language(&self, language: &str) {
        *self.language.lock().unwrap_or_else(|e| e.into_inner()) = language.to_owned();
    }

    /// Sets the per-game language the backend reports (a Steam API language name).
    pub fn set_game_language(&self, language: &str) {
        *self.game_language.lock().unwrap_or_else(|e| e.into_inner()) = language.to_owned();
    }

    /// Pre-seeds an integer stat (as if Steam had downloaded it at startup).
    pub fn set_stat_i32(&self, name: &str, value: i32) {
        self.stats_i32
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_owned(), value);
    }

    /// The current value of an integer stat.
    pub fn stat_i32(&self, name: &str) -> Option<i32> {
        self.stats_i32
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .copied()
    }

    /// Whether the mock marked the achievement unlocked.
    pub fn achievement_unlocked(&self, id: &str) -> bool {
        self.achievements
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .copied()
            .unwrap_or(false)
    }

    /// The achievements the game asked to unlock, in call order.
    pub fn unlocked_calls(&self) -> Vec<String> {
        self.unlocked_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The rich presence keys the game set, in call order.
    pub fn presence_calls(&self) -> Vec<(String, Option<String>)> {
        self.presence_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// How many times the game asked to upload stats.
    pub fn store_calls(&self) -> u32 {
        *self.store_calls.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Makes every `unlock` fail as Steam does for an id missing on the partner site.
    pub fn reject_unlocks(&self, reject: bool) {
        *self.reject_unlocks.lock().unwrap_or_else(|e| e.into_inner()) = reject;
    }

    /// How many times a shutdown waited for the stats upload acknowledgement (after a
    /// `store_stats`, before the client is dropped).
    pub fn stats_waits(&self) -> u32 {
        *self.stats_waits.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The `achievements_too` flags of every reset call.
    pub fn reset_calls(&self) -> Vec<bool> {
        self.reset_calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The bytes stored in a cloud file, if any (test assertions).
    pub fn cloud_file(&self, name: &str) -> Option<Vec<u8>> {
        self.cloud
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// Every cloud file name currently stored (sorted, like the real listing).
    pub fn cloud_files(&self) -> Vec<String> {
        self.cloud
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

impl MockSteam {
    /// The in-memory leaderboard store behind the mock's Steam leaderboards (made on first use;
    /// its local user follows [`MockSteam::set_steam_id`], default 1). Seed other players'
    /// scores, set friends or inject failures on it.
    pub fn leaderboard_store(&self) -> Arc<LocalLeaderboards> {
        let mut slot = self.leaderboards.lock().unwrap_or_else(|e| e.into_inner());
        slot.get_or_insert_with(|| Arc::new(LocalLeaderboards::new(self.steam_id().map_or(1, |id| id.0))))
            .clone()
    }

    /// Sets what the mock reports for `ownership.app` (unknown apps are not owned, not installed
    /// and have no install dir).
    pub fn set_app(&self, ownership: AppOwnership) {
        self.apps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ownership.app.0, ownership);
    }

    /// Every timeline call the game made, in order.
    pub fn timeline_calls(&self) -> Vec<TimelineCall> {
        self.timeline_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn app(&self, app: AppId) -> Result<Option<AppOwnership>, SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        Ok(self
            .apps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&app.0)
            .cloned())
    }
}

impl SteamBackend for MockSteam {
    /// The mock is its own source of truth: initialization succeeds and reports the state the
    /// test set ([`MockSteam::shared`] is `Ready`, [`MockSteam::shared_unavailable`] is not).
    fn restart_app_if_necessary(&self, app_id: AppId) -> bool {
        self.restart_checks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(app_id);
        *self.restart_required.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn initialize(&self, app_id: Option<AppId>) -> Result<SteamState, String> {
        *self.init_calls.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        if app_id.is_some() {
            self.set_app_id(app_id);
        }
        Ok(self.state())
    }

    fn logged_on(&self) -> bool {
        self.state() == SteamState::Ready && !*self.offline.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn await_stats_stored(&self) {
        *self.stats_waits.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    }

    fn shutdown(&self) {
        // A second call, or one before init, is a no-op (the trait's contract).
        let initialized = *self.init_calls.lock().unwrap_or_else(|e| e.into_inner()) > 0;
        if !initialized || self.state() == SteamState::Unavailable {
            return;
        }
        *self.shutdown_calls.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.set_state(SteamState::Unavailable);
    }

    fn input_init(&self) -> bool {
        self.input_state().init_calls += 1;
        true
    }
    fn input_connected(&self) -> usize {
        self.input_state().controllers
    }
    fn input_controller_type(&self) -> Option<u32> {
        let input = self.input_state();
        if input.controllers == 0 {
            return None;
        }
        input.controller_type
    }
    fn input_activate_action_set(&self, set: &str) -> Result<(), SteamError> {
        let mut input = self.input_state();
        if input.activation_fails {
            return Err(SteamError::Rejected("an unknown action set".to_owned()));
        }
        input.activated.push(set.to_owned());
        Ok(())
    }
    fn input_digital(&self, action: &str) -> Option<bool> {
        let mut input = self.input_state();
        input.name_lookups += 1;
        if input.controllers == 0 || input.activation_fails {
            return None;
        }
        input.digital.get(action).copied()
    }
    fn input_analog(&self, action: &str) -> Option<(f32, f32)> {
        let mut input = self.input_state();
        input.name_lookups += 1;
        if input.controllers == 0 || input.activation_fails {
            return None;
        }
        input.analog.get(action).copied()
    }
    fn input_digital_handle(&self, action: &str) -> Option<u64> {
        let mut input = self.input_state();
        input.name_lookups += 1;
        if input.controllers == 0 || input.activation_fails {
            return Some(0);
        }
        Some(assign_handle(&mut input, action))
    }
    fn input_analog_handle(&self, action: &str) -> Option<u64> {
        let mut input = self.input_state();
        input.name_lookups += 1;
        if input.controllers == 0 || input.activation_fails {
            return Some(0);
        }
        Some(assign_handle(&mut input, action))
    }
    fn input_read_digital(&self, handle: u64) -> Option<bool> {
        if handle == 0 {
            return None;
        }
        let input = self.input_state();
        if input.controllers == 0 {
            return None;
        }
        input.digital_by_handle.get(&handle).copied()
    }
    fn input_read_analog(&self, handle: u64) -> Option<(f32, f32)> {
        if handle == 0 {
            return None;
        }
        let input = self.input_state();
        if input.controllers == 0 {
            return None;
        }
        input.analog_by_handle.get(&handle).copied()
    }
    fn state(&self) -> SteamState {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pump(&self) -> Vec<SteamEvent> {
        std::mem::take(self.events.lock().unwrap_or_else(|e| e.into_inner()).as_mut())
    }

    fn app_id(&self) -> Option<AppId> {
        *self.app_id.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn unlock(&self, id: &str) -> Result<(), SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        if *self.reject_unlocks.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err(SteamError::Rejected(format!("unlock {id} refused")));
        }
        self.unlocked_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(id.to_owned());
        let was_unlocked = self
            .achievements
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_owned(), true)
            .unwrap_or(false);
        // The real backend reports the stored unlock through a callback; the mock does it at once.
        if !was_unlocked {
            self.inject(SteamEvent::AchievementStored { id: id.to_owned() });
        }
        Ok(())
    }

    fn clear_achievement(&self, id: &str) -> Result<(), SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        self.achievements
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_owned(), false);
        Ok(())
    }

    fn achievement_unlocked(&self, id: &str) -> Result<bool, SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        *self.achievement_queries.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        if *self
            .achievement_query_fails
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            return Err(SteamError::Rejected("the achievement query".into()));
        }
        Ok(self
            .achievements
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .copied()
            .unwrap_or(false))
    }

    fn reset_all_stats(&self, achievements_too: bool) -> Result<(), SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        self.reset_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(achievements_too);
        self.stats_i32.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.stats_f32.lock().unwrap_or_else(|e| e.into_inner()).clear();
        if achievements_too {
            self.achievements
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
        Ok(())
    }

    fn add_stat_i32(&self, name: &str, delta: i32) -> Result<i32, SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        let mut stats = self.stats_i32.lock().unwrap_or_else(|e| e.into_inner());
        let slot = stats.entry(name.to_owned()).or_insert(0);
        *slot = slot.saturating_add(delta);
        Ok(*slot)
    }

    fn set_stat_i32(&self, name: &str, value: i32) -> Result<(), SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        self.stats_i32
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_owned(), value);
        Ok(())
    }

    fn set_stat_f32(&self, name: &str, value: f32) -> Result<(), SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        self.stats_f32
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_owned(), value);
        Ok(())
    }

    fn stat_i32(&self, name: &str) -> Result<i32, SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        Ok(self
            .stats_i32
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .copied()
            .unwrap_or(0))
    }

    fn stat_f32(&self, name: &str) -> Result<f32, SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        Ok(self
            .stats_f32
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .copied()
            .unwrap_or(0.0))
    }

    fn store_stats(&self) -> Result<(), SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        *self.store_calls.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        // Mirror the async round trip: the upload result arrives as an event on the next pump.
        self.inject(SteamEvent::StatsStored { ok: true });
        Ok(())
    }

    fn set_rich_presence(&self, key: &str, value: Option<&str>) -> Result<(), SteamError> {
        // Same rule as the real backend, whose bindings would abort on an interior NUL. The
        // real backend runs this check before its client lookup, so the mock does too: a NUL
        // key is `Rejected` even while unavailable.
        if key.contains('\0') || value.is_some_and(|value| value.contains('\0')) {
            return Err(SteamError::Rejected(
                "rich presence keys must not contain NUL".into(),
            ));
        }
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        self.presence_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((key.to_owned(), value.map(str::to_owned)));
        self.presence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_owned(), value.map(str::to_owned));
        Ok(())
    }

    fn clear_rich_presence(&self) {
        self.presence.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn show_floating_keyboard(&self, request: &crate::deck_prep::KeyboardRequest) -> Result<(), SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        self.keyboard_shown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(*request);
        Ok(())
    }

    fn dismiss_floating_keyboard(&self) {
        *self.keyboard_dismissed.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    }

    fn is_steam_deck(&self) -> bool {
        *self.deck.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn overlay_enabled(&self) -> bool {
        self.state() == SteamState::Ready
    }

    fn open_overlay(&self, page: &str) -> Result<(), SteamError> {
        crate::resource::check_overlay_page(page)?;
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        self.overlay_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(page.to_owned());
        Ok(())
    }

    fn ui_language(&self) -> String {
        self.language.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn game_language(&self) -> String {
        self.game_language
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn steam_id(&self) -> Option<SteamId> {
        *self.steam_id.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cloud_write(&self, name: &str, data: &[u8]) -> Result<(), SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        crate::resource::check_name("cloud file name", name)?;
        self.cloud
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_owned(), data.to_vec());
        Ok(())
    }

    fn cloud_read(&self, name: &str) -> Result<Option<Vec<u8>>, SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        crate::resource::check_name("cloud file name", name)?;
        Ok(self
            .cloud
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned())
    }

    fn cloud_delete(&self, name: &str) -> Result<(), SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        crate::resource::check_name("cloud file name", name)?;
        self.cloud.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
        Ok(())
    }

    fn cloud_exists(&self, name: &str) -> Result<bool, SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        crate::resource::check_name("cloud file name", name)?;
        Ok(self
            .cloud
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name))
    }

    fn cloud_list(&self) -> Result<Vec<String>, SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        Ok(self.cloud_files())
    }

    fn lobbies(&self) -> Option<Arc<dyn regia_net::lobby::LobbyBackend>> {
        if self.state() != SteamState::Ready {
            return None;
        }
        let link = self.net.lock().unwrap_or_else(|e| e.into_inner());
        link.as_ref().map(|l| l.lobbies.clone())
    }

    fn p2p(&self) -> Option<Arc<dyn crate::sockets::P2pMessages>> {
        if self.state() != SteamState::Ready {
            return None;
        }
        let link = self.net.lock().unwrap_or_else(|e| e.into_inner());
        link.as_ref().map(|l| l.messages.clone())
    }
    fn leaderboards(&self) -> Option<Arc<dyn LeaderboardService>> {
        if self.state() != SteamState::Ready {
            return None;
        }
        Some(Arc::new(MockLeaderboards(self.leaderboard_store())))
    }

    fn is_subscribed_app(&self, app: AppId) -> Result<bool, SteamError> {
        Ok(self.app(app)?.is_some_and(|a| a.subscribed))
    }

    fn is_dlc_installed(&self, app: AppId) -> Result<bool, SteamError> {
        Ok(self.app(app)?.is_some_and(|a| a.dlc_installed))
    }

    fn is_app_installed(&self, app: AppId) -> Result<bool, SteamError> {
        Ok(self.app(app)?.is_some_and(|a| a.installed))
    }

    fn app_install_dir(&self, app: AppId) -> Result<Option<String>, SteamError> {
        Ok(self.app(app)?.and_then(|a| a.install_dir))
    }

    fn timeline(&self, call: &TimelineCall) -> Result<(), SteamError> {
        if self.state() != SteamState::Ready {
            return Err(SteamError::Unavailable);
        }
        self.timeline_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(call.clone());
        Ok(())
    }
}
