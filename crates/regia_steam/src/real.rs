// SPDX-License-Identifier: MPL-2.0
//! The real Steamworks backend (`steamworks` 0.13 bindings over Valve's SDK). Compiled only on
//! desktop with the `steamworks` feature; links the SDK redistributable from `STEAM_SDK_LOCATION`
//! (see the crate README). Every fallible operation maps onto [`crate::SteamError`] — nothing
//! here panics on runtime data.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::backend::{AppId, EventQueue, SteamBackend, SteamError, SteamEvent, SteamId, SteamState};
use crate::real_lobby::{RealLobbies, RealP2p};

/// One initialized Steamworks client: the client handle plus the callback registrations that must
/// stay alive for the callbacks to fire.
struct Connected {
    client: steamworks::Client,
    #[allow(dead_code)] // dropping these unregisters the callbacks; they are intentionally held
    callbacks: Vec<steamworks::CallbackHandle>,
}

/// The Steamworks-backed [`SteamBackend`]. Created offline by [`RealBackend::offline`] (the
/// default backend under the `steamworks` feature) and connected by
/// [`SteamBackend::initialize`] from the plugin's `finish`.
pub struct RealBackend {
    connected: Mutex<Option<Arc<Connected>>>,
    app_id: Mutex<Option<AppId>>,
    events: Arc<EventQueue>,
    deck: Mutex<bool>,
    /// The lobby backend and messages endpoint (spec 89), made once per connection so their
    /// callbacks are registered once.
    net: Mutex<Option<RealNet>>,
    /// The leaderboard service (ADR-2340), made once per connection.
    leaderboards: Mutex<Option<Arc<crate::real_leaderboards::RealLeaderboards>>>,
    /// Action handles and the last controller list. The game thread fills controllers from
    /// `input_connected` and reads them from the handle path, so those two locks are never held
    /// together with `connected`.
    input_cache: Mutex<InputCache>,
}

/// Steam Input handles resolved once per action name, plus the controller list from the last
/// `input_connected` call. Only non-zero handles are kept: Steam answers `0` until the action
/// manifest is loaded, so a miss is asked again (`store_handle`).
struct InputCache {
    digital: BTreeMap<String, u64>,
    analog: BTreeMap<String, u64>,
    controllers: [u64; STEAM_INPUT_MAX_COUNT],
    controller_count: usize,
}

impl Default for InputCache {
    fn default() -> Self {
        Self {
            digital: BTreeMap::new(),
            analog: BTreeMap::new(),
            controllers: [0; STEAM_INPUT_MAX_COUNT],
            controller_count: 0,
        }
    }
}

/// The cached lobby backend and messages endpoint of one connection.
struct RealNet {
    lobbies: Arc<RealLobbies>,
    p2p: Arc<RealP2p>,
}

impl Default for RealBackend {
    fn default() -> Self {
        Self::offline()
    }
}

impl RealBackend {
    /// A backend that has not talked to Steam yet; [`SteamBackend::initialize`] connects it.
    pub fn offline() -> Self {
        Self {
            connected: Mutex::new(None),
            app_id: Mutex::new(None),
            events: Arc::new(EventQueue::default()),
            deck: Mutex::new(false),
            net: Mutex::new(None),
            leaderboards: Mutex::new(None),
            input_cache: Mutex::new(InputCache::default()),
        }
    }

    /// Steamworks aborts (panics) on interior NUL bytes in names; user data must come back as an
    /// error. Empty names are never valid either.
    fn name_ok(what: &str, name: &str) -> Result<(), SteamError> {
        crate::resource::check_name(what, name)
    }

    fn with_client<T>(
        &self,
        op: impl FnOnce(&steamworks::Client) -> Result<T, SteamError>,
    ) -> Result<T, SteamError> {
        let guard = self.connected.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(connected) => op(&connected.client),
            None => Err(SteamError::Unavailable),
        }
    }

    /// The lobby backend and messages endpoint of the live connection, made on first use.
    fn real_net(&self) -> Option<(Arc<RealLobbies>, Arc<RealP2p>)> {
        let client = self
            .connected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|c| c.client.clone())?;
        let mut net = self.net.lock().unwrap_or_else(|e| e.into_inner());
        let net = net.get_or_insert_with(|| RealNet {
            lobbies: Arc::new(RealLobbies::new(client.clone())),
            p2p: Arc::new(RealP2p::new(client)),
        });
        Some((net.lobbies.clone(), net.p2p.clone()))
    }

    /// `Err` for Steam's `Result<(), ()>` returns, with the operation's name for the error text.
    fn rejected(what: &str) -> SteamError {
        SteamError::Rejected(format!(
            "Steam rejected {what} (not configured on the partner site, or stats not received yet)"
        ))
    }
}

impl SteamBackend for RealBackend {
    /// Valve's shipping-init step: when the game was **not** launched through Steam, the SDK
    /// relaunches it through the client and this returns `true`; the caller must exit.
    fn restart_app_if_necessary(&self, app_id: AppId) -> bool {
        steamworks::restart_app_if_necessary(steamworks::AppId(app_id.0))
    }

    fn initialize(&self, app_id: Option<AppId>) -> Result<SteamState, String> {
        let mut guard = self.connected.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return Ok(SteamState::Ready); // already initialized (double plugin finish is a no-op)
        }
        let client = match app_id {
            Some(id) => steamworks::Client::init_app(steamworks::AppId(id.0)),
            None => steamworks::Client::init(),
        }
        .map_err(|err| err.to_string())?;

        // Steamworks delivers its callbacks only while registrations are alive.
        let mut callbacks = Vec::new();
        let events = self.events.clone();
        callbacks.push(
            client.register_callback(move |cb: steamworks::GameOverlayActivated| {
                events.push(SteamEvent::OverlayActivated { active: cb.active });
            }),
        );
        let join_events = self.events.clone();
        callbacks.push(
            client.register_callback(move |cb: steamworks::GameRichPresenceJoinRequested| {
                join_events.push(SteamEvent::JoinRequested {
                    friend: SteamId(cb.friend_steam_id.raw()),
                    connect: cb.connect,
                });
            }),
        );
        let stored_events = self.events.clone();
        callbacks.push(
            client.register_callback(move |cb: steamworks::UserAchievementStored| {
                // Progress callbacks (a non-zero progress pair) are not unlocks.
                if cb.current_progress == 0 && cb.max_progress == 0 {
                    stored_events.push(SteamEvent::AchievementStored {
                        id: cb.achievement_name,
                    });
                }
            }),
        );
        let stats_events = self.events.clone();
        callbacks.push(client.register_callback(move |cb: steamworks::UserStatsStored| {
            stats_events.push(SteamEvent::StatsStored {
                ok: cb.result.is_ok(),
            });
        }));

        *self.deck.lock().unwrap_or_else(|e| e.into_inner()) =
            client.utils().is_steam_running_on_steam_deck();
        *self.app_id.lock().unwrap_or_else(|e| e.into_inner()) = app_id;
        *guard = Some(Arc::new(Connected { client, callbacks }));
        Ok(SteamState::Ready)
    }

    fn logged_on(&self) -> bool {
        self.with_client(|client| Ok(client.user().logged_on()))
            .unwrap_or(false)
    }

    fn await_stats_stored(&self) {
        // Rounds, not wall-clock: `store_stats` queues the upload and Steam answers with
        // `UserStatsStored` through `run_callbacks`. Give it a bounded number of 10 ms rounds.
        const ROUNDS: u32 = 100;
        let Some(connected) = self.connected.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return;
        };
        // `wait_stats_stored` only accepts a StatsStored that arrives after this point: an
        // acknowledgement of an earlier store_stats still sitting in the queue must not pass for
        // the shutdown flush, or the final stats upload would be dropped with the client.
        crate::backend::wait_stats_stored(&self.events, ROUNDS, || {
            connected.client.run_callbacks();
            std::thread::sleep(std::time::Duration::from_millis(10));
        });
    }

    fn shutdown(&self) {
        // Dropping the callback registrations and our client clone; the SDK shuts down when the
        // last clone (ours plus any a host took through `Steam::client`) is gone.
        self.net.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.leaderboards.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.connected.lock().unwrap_or_else(|e| e.into_inner()).take();
        // Handles and controllers belong to the old connection's Steam Input session.
        *self.input_cache.lock().unwrap_or_else(|e| e.into_inner()) = InputCache::default();
        *self.app_id.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn lobbies(&self) -> Option<Arc<dyn regia_net::lobby::LobbyBackend>> {
        self.real_net()
            .map(|n| n.0 as Arc<dyn regia_net::lobby::LobbyBackend>)
    }

    fn p2p(&self) -> Option<Arc<dyn crate::sockets::P2pMessages>> {
        self.real_net()
            .map(|n| n.1 as Arc<dyn crate::sockets::P2pMessages>)
    }

    fn leaderboards(&self) -> Option<Arc<dyn regia_framework::leaderboard::LeaderboardService>> {
        let client = self
            .connected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|c| c.client.clone())?;
        let mut slot = self.leaderboards.lock().unwrap_or_else(|e| e.into_inner());
        let service = slot
            .get_or_insert_with(|| Arc::new(crate::real_leaderboards::RealLeaderboards::new(client)))
            .clone();
        Some(service as Arc<dyn regia_framework::leaderboard::LeaderboardService>)
    }

    fn is_subscribed_app(&self, app: AppId) -> Result<bool, SteamError> {
        self.with_client(|client| Ok(client.apps().is_subscribed_app(steamworks::AppId(app.0))))
    }

    fn is_dlc_installed(&self, app: AppId) -> Result<bool, SteamError> {
        self.with_client(|client| Ok(client.apps().is_dlc_installed(steamworks::AppId(app.0))))
    }

    fn is_app_installed(&self, app: AppId) -> Result<bool, SteamError> {
        self.with_client(|client| Ok(client.apps().is_app_installed(steamworks::AppId(app.0))))
    }

    fn app_install_dir(&self, app: AppId) -> Result<Option<String>, SteamError> {
        self.with_client(|client| {
            let dir = client.apps().app_install_dir(steamworks::AppId(app.0));
            Ok((!dir.is_empty()).then_some(dir))
        })
    }

    fn timeline(&self, call: &crate::services::TimelineCall) -> Result<(), SteamError> {
        use crate::services::{TimelineCall, TimelineClip, TimelineMode};
        use std::time::Duration;
        // `Steam::timeline_*` validated the call: names hold no NUL (the bindings unwrap a
        // CString), deltas and durations are finite and non-negative where the bindings take a
        // `Duration`.
        self.with_client(|client| {
            let timeline = client.timeline();
            match call {
                TimelineCall::Mode(mode) => timeline.set_timeline_game_mode(match mode {
                    TimelineMode::Playing => steamworks::TimelineGameMode::Playing,
                    TimelineMode::Staging => steamworks::TimelineGameMode::Staging,
                    TimelineMode::Menus => steamworks::TimelineGameMode::Menus,
                    TimelineMode::LoadingScreen => steamworks::TimelineGameMode::LoadingScreen,
                }),
                TimelineCall::State { description, delta } => {
                    Self::name_ok("timeline state", description)?;
                    timeline.set_timeline_state_description(
                        description,
                        Duration::try_from_secs_f32(*delta).unwrap_or_default(),
                    );
                }
                TimelineCall::ClearState { delta } => {
                    timeline.clear_timeline_state_description(
                        Duration::try_from_secs_f32(*delta).unwrap_or_default(),
                    );
                }
                TimelineCall::Event(event) => {
                    Self::name_ok("timeline icon", &event.icon)?;
                    Self::name_ok("timeline title", &event.title)?;
                    if event.description.contains('\0') {
                        return Err(SteamError::Rejected("timeline description holds a NUL".into()));
                    }
                    if !event.start_offset.is_finite() || !event.duration.is_finite() || event.duration < 0.0
                    {
                        return Err(SteamError::Rejected("timeline times must be finite".into()));
                    }
                    timeline.add_timeline_event(
                        &event.icon,
                        &event.title,
                        &event.description,
                        event.priority,
                        event.start_offset,
                        Duration::try_from_secs_f32(event.duration).unwrap_or_default(),
                        match event.clip {
                            TimelineClip::None => steamworks::TimelineEventClipPriority::None,
                            TimelineClip::Standard => steamworks::TimelineEventClipPriority::Standard,
                            TimelineClip::Featured => steamworks::TimelineEventClipPriority::Featured,
                        },
                    );
                }
            }
            Ok(())
        })
    }

    fn raw_client(&self) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        let guard = self.connected.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .map(|connected| Box::new(connected.client.clone()) as Box<dyn std::any::Any + Send + Sync>)
    }

    fn input_init(&self) -> bool {
        self.with_client(|client| Ok(client.input().init(false)))
            .unwrap_or(false)
    }

    fn input_connected(&self) -> usize {
        let found = self.with_client(|client| {
            let (handles, count) = connected_controllers(&client.input());
            Ok((handles, count))
        });
        match found {
            Ok((handles, count)) => {
                let mut cache = self.input_cache.lock().unwrap_or_else(|e| e.into_inner());
                cache.controllers = handles;
                cache.controller_count = count;
                count
            }
            Err(_) => 0,
        }
    }

    fn input_activate_action_set(&self, set: &str) -> Result<(), SteamError> {
        if set.contains('\0') {
            return Err(Self::rejected("an action set name with a NUL byte"));
        }
        let (handles, count) = {
            let cache = self.input_cache.lock().unwrap_or_else(|e| e.into_inner());
            (cache.controllers, cache.controller_count)
        };
        self.with_client(|client| {
            let input = client.input();
            let handle = input.get_action_set_handle(set);
            if handle == 0 {
                return Err(Self::rejected("an unknown action set"));
            }
            for controller in handles.iter().take(count).copied() {
                input.activate_action_set_handle(controller, handle);
            }
            Ok(())
        })
    }

    fn input_digital(&self, action: &str) -> Option<bool> {
        let handle = self.input_digital_handle(action)?;
        self.input_read_digital(handle)
    }

    fn input_analog(&self, action: &str) -> Option<(f32, f32)> {
        let handle = self.input_analog_handle(action)?;
        self.input_read_analog(handle)
    }

    fn input_digital_handle(&self, action: &str) -> Option<u64> {
        if action.contains('\0') {
            return Some(0);
        }
        if let Some(handle) = self
            .input_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .digital
            .get(action)
            .copied()
        {
            return Some(handle);
        }
        let resolved = self
            .with_client(|client| Ok(client.input().get_digital_action_handle(action)))
            .unwrap_or(0);
        store_handle(
            &mut self.input_cache.lock().unwrap_or_else(|e| e.into_inner()).digital,
            action,
            resolved,
        );
        Some(resolved)
    }

    fn input_analog_handle(&self, action: &str) -> Option<u64> {
        if action.contains('\0') {
            return Some(0);
        }
        if let Some(handle) = self
            .input_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .analog
            .get(action)
            .copied()
        {
            return Some(handle);
        }
        let resolved = self
            .with_client(|client| Ok(client.input().get_analog_action_handle(action)))
            .unwrap_or(0);
        store_handle(
            &mut self.input_cache.lock().unwrap_or_else(|e| e.into_inner()).analog,
            action,
            resolved,
        );
        Some(resolved)
    }

    fn input_read_digital(&self, handle: u64) -> Option<bool> {
        if handle == 0 {
            return None;
        }
        let controller = {
            let cache = self.input_cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.controller_count == 0 {
                return None;
            }
            cache.controllers[0]
        };
        self.with_client(|client| {
            let data = client.input().get_digital_action_data(controller, handle);
            if data.bActive {
                Ok(data.bState)
            } else {
                Err(SteamError::Unavailable)
            }
        })
        .ok()
    }

    fn input_controller_type(&self) -> Option<u32> {
        // The first controller of the last `input_connected` list, as the action reads use
        // (rc1-rev-net-k 2: without this override the glyphs never followed Steam Input).
        let controller = {
            let cache = self.input_cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.controller_count == 0 {
                return None;
            }
            cache.controllers[0]
        };
        self.with_client(|client| Ok(client.input().get_input_type_for_handle(controller)))
            .ok()
            .and_then(steam_input_type_number)
    }

    fn input_read_analog(&self, handle: u64) -> Option<(f32, f32)> {
        if handle == 0 {
            return None;
        }
        let controller = {
            let cache = self.input_cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.controller_count == 0 {
                return None;
            }
            cache.controllers[0]
        };
        self.with_client(|client| {
            let data = client.input().get_analog_action_data(controller, handle);
            if data.bActive {
                Ok((data.x, data.y))
            } else {
                Err(SteamError::Unavailable)
            }
        })
        .ok()
    }

    fn state(&self) -> SteamState {
        let guard = self.connected.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            SteamState::Ready
        } else {
            SteamState::Unavailable
        }
    }

    fn pump(&self) -> Vec<SteamEvent> {
        // Callbacks run host code (networking, matchmaking) that may call back into this backend
        // through `Steam`; the lock must not be held while they run.
        let connected = self.connected.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(connected) = connected {
            connected.client.run_callbacks();
        }
        self.events.take()
    }

    fn app_id(&self) -> Option<AppId> {
        *self.app_id.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn unlock(&self, id: &str) -> Result<(), SteamError> {
        Self::name_ok("achievement id", id)?;
        self.with_client(|client| {
            client
                .user_stats()
                .achievement(id)
                .set()
                .map_err(|_| Self::rejected("the achievement unlock"))
        })
    }

    fn clear_achievement(&self, id: &str) -> Result<(), SteamError> {
        Self::name_ok("achievement id", id)?;
        self.with_client(|client| {
            client
                .user_stats()
                .achievement(id)
                .clear()
                .map_err(|_| Self::rejected("the achievement clear"))
        })
    }

    fn achievement_unlocked(&self, id: &str) -> Result<bool, SteamError> {
        Self::name_ok("achievement id", id)?;
        self.with_client(|client| {
            client
                .user_stats()
                .achievement(id)
                .get()
                .map_err(|_| Self::rejected("the achievement query"))
        })
    }

    fn reset_all_stats(&self, achievements_too: bool) -> Result<(), SteamError> {
        self.with_client(|client| {
            client
                .user_stats()
                .reset_all_stats(achievements_too)
                .map_err(|_| Self::rejected("the stats reset"))
        })
    }

    fn add_stat_i32(&self, name: &str, delta: i32) -> Result<i32, SteamError> {
        Self::name_ok("stat name", name)?;
        self.with_client(|client| {
            let stats = client.user_stats();
            let value = stats
                .get_stat_i32(name)
                .map_err(|_| Self::rejected("the stat read"))?
                .saturating_add(delta);
            stats
                .set_stat_i32(name, value)
                .map_err(|_| Self::rejected("the stat write"))?;
            Ok(value)
        })
    }

    fn set_stat_i32(&self, name: &str, value: i32) -> Result<(), SteamError> {
        Self::name_ok("stat name", name)?;
        self.with_client(|client| {
            client
                .user_stats()
                .set_stat_i32(name, value)
                .map_err(|_| Self::rejected("the stat write"))
        })
    }

    fn set_stat_f32(&self, name: &str, value: f32) -> Result<(), SteamError> {
        Self::name_ok("stat name", name)?;
        self.with_client(|client| {
            client
                .user_stats()
                .set_stat_f32(name, value)
                .map_err(|_| Self::rejected("the stat write"))
        })
    }

    fn stat_i32(&self, name: &str) -> Result<i32, SteamError> {
        Self::name_ok("stat name", name)?;
        self.with_client(|client| {
            client
                .user_stats()
                .get_stat_i32(name)
                .map_err(|_| Self::rejected("the stat read"))
        })
    }

    fn stat_f32(&self, name: &str) -> Result<f32, SteamError> {
        Self::name_ok("stat name", name)?;
        self.with_client(|client| {
            client
                .user_stats()
                .get_stat_f32(name)
                .map_err(|_| Self::rejected("the stat read"))
        })
    }

    fn store_stats(&self) -> Result<(), SteamError> {
        self.with_client(|client| {
            client
                .user_stats()
                .store_stats()
                .map_err(|_| Self::rejected("the stats upload"))
        })
    }

    fn set_rich_presence(&self, key: &str, value: Option<&str>) -> Result<(), SteamError> {
        // steamworks panics on interior NUL bytes; user data must never panic the game.
        if key.contains('\0') || value.is_some_and(|value| value.contains('\0')) {
            return Err(SteamError::Rejected(
                "rich presence keys must not contain NUL".into(),
            ));
        }
        self.with_client(|client| {
            if !client.friends().set_rich_presence(key, value) {
                return Err(Self::rejected("the rich presence key"));
            }
            Ok(())
        })
    }

    fn clear_rich_presence(&self) {
        let _ = self.with_client(|client| {
            client.friends().clear_rich_presence();
            Ok(())
        });
    }

    fn is_steam_deck(&self) -> bool {
        *self.deck.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn overlay_enabled(&self) -> bool {
        let guard = self.connected.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .is_some_and(|connected| connected.client.utils().is_overlay_enabled())
    }

    fn open_overlay(&self, page: &str) -> Result<(), SteamError> {
        crate::resource::check_overlay_page(page)?;
        self.with_client(|client| {
            client.friends().activate_game_overlay(page);
            Ok(())
        })
    }

    fn ui_language(&self) -> String {
        let guard = self.connected.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .map_or_else(String::new, |connected| connected.client.utils().ui_language())
    }

    fn game_language(&self) -> String {
        let guard = self.connected.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map_or_else(String::new, |connected| {
            connected.client.apps().current_game_language()
        })
    }

    fn steam_id(&self) -> Option<SteamId> {
        let guard = self.connected.lock().unwrap_or_else(|e| e.into_inner());
        let connected = guard.as_ref()?;
        let id = connected.client.user().steam_id();
        (id.raw() != 0).then_some(SteamId(id.raw()))
    }

    fn cloud_write(&self, name: &str, data: &[u8]) -> Result<(), SteamError> {
        use std::io::{Read as _, Write as _};
        Self::name_ok("cloud file name", name)?;
        self.with_client(|client| {
            let storage = client.remote_storage();
            // `SteamFileWriter` commits its stream on drop, even after a failed chunk, and
            // steamworks 0.13 exposes no cancel — so remember the previous payload first. On a
            // failed write the torn bytes are committed and then undone by `recover_failed_write`:
            // the old bytes are written back, and the file is deleted only when the slot did not
            // exist before (SaveBackend::save: a failed write leaves the previous slot intact).
            let previous = if storage.file(name).exists() {
                let mut bytes = Vec::new();
                storage
                    .file(name)
                    .read()
                    .read_to_end(&mut bytes)
                    .map_err(|e| SteamError::Rejected(format!("cloud read-back {name:?}: {e}")))?;
                Some(bytes)
            } else {
                None
            };
            let mut writer = storage.file(name).write();
            if let Err(e) = writer.write_all(data) {
                // Drop commits the torn stream (FileWriteStreamClose); undo it below.
                drop(writer);
                let restore = crate::cloud::recover_failed_write(
                    previous.as_deref(),
                    |bytes| {
                        let mut rewrite = storage.file(name).write();
                        rewrite.write_all(bytes)
                    },
                    || {
                        storage.file(name).delete();
                    },
                );
                return match restore {
                    Ok(()) => Err(SteamError::Rejected(format!("cloud write {name:?}: {e}"))),
                    Err(restore_err) => Err(SteamError::Rejected(format!(
                        "cloud write {name:?}: {e}; restoring the previous file failed: {restore_err}"
                    ))),
                };
            }
            Ok(())
        })
    }

    fn cloud_read(&self, name: &str) -> Result<Option<Vec<u8>>, SteamError> {
        use std::io::Read;
        Self::name_ok("cloud file name", name)?;
        self.with_client(|client| {
            let storage = client.remote_storage();
            if !storage.file(name).exists() {
                return Ok(None);
            }
            let mut bytes = Vec::new();
            storage
                .file(name)
                .read()
                .read_to_end(&mut bytes)
                .map_err(|e| SteamError::Rejected(format!("cloud read {name:?}: {e}")))?;
            Ok(Some(bytes))
        })
    }

    fn cloud_delete(&self, name: &str) -> Result<(), SteamError> {
        Self::name_ok("cloud file name", name)?;
        self.with_client(|client| {
            // `delete` reports whether a file existed; deleting a missing slot is not an error.
            client.remote_storage().file(name).delete();
            Ok(())
        })
    }

    fn cloud_exists(&self, name: &str) -> Result<bool, SteamError> {
        Self::name_ok("cloud file name", name)?;
        self.with_client(|client| Ok(client.remote_storage().file(name).exists()))
    }

    fn cloud_list(&self) -> Result<Vec<String>, SteamError> {
        self.with_client(|client| {
            let mut names: Vec<String> = client
                .remote_storage()
                .files()
                .into_iter()
                .map(|file| file.name)
                .collect();
            names.sort();
            Ok(names)
        })
    }
}

/// Caches a resolved action handle. A `0` is not kept: Steam answers `0` until the action
/// manifest is loaded, and a cached `0` would leave the action dead for the whole session.
fn store_handle(cache: &mut BTreeMap<String, u64>, action: &str, handle: u64) {
    if handle != 0 {
        cache.insert(action.to_owned(), handle);
    }
}

/// ISteamInput's `STEAM_INPUT_MAX_COUNT` (Steamworks docs). `steamworks::sys` is private without
/// the bindings' `raw-bindings` feature.
const STEAM_INPUT_MAX_COUNT: usize = 16;

/// The `ESteamInputType` number of a controller type (the public Steamworks numbering that
/// [`crate::ControllerKind::from_steam_input_type`] reads); `None` for an unknown one.
fn steam_input_type_number(kind: steamworks::InputType) -> Option<u32> {
    use steamworks::InputType;
    match kind {
        InputType::Unknown => None,
        InputType::SteamController => Some(1),
        InputType::XBox360Controller => Some(2),
        InputType::XBoxOneController => Some(3),
        InputType::GenericGamepad => Some(4),
        InputType::PS4Controller => Some(5),
        InputType::AppleMFiController => Some(6),
        InputType::AndroidController => Some(7),
        InputType::SwitchJoyConPair => Some(8),
        InputType::SwitchJoyConSingle => Some(9),
        InputType::SwitchProController => Some(10),
        InputType::MobileTouch => Some(11),
        InputType::PS3Controller => Some(12),
        InputType::PS5Controller => Some(13),
        InputType::SteamDeckController => Some(14),
    }
}

/// The handles of the connected controllers, on the stack. `steamworks`' own list is padded to
/// the interface maximum, so the count is the real length.
fn connected_controllers(input: &steamworks::Input) -> ([u64; STEAM_INPUT_MAX_COUNT], usize) {
    let mut handles = [0_u64; STEAM_INPUT_MAX_COUNT];
    let count = input.get_connected_controllers_slice(&mut handles);
    (handles, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// rc1-rev-net-k 2: every Steam Input type maps to its documented number, and the glyph
    /// families follow them (a DualSense through Steam Input on a Deck shows PlayStation glyphs).
    #[test]
    fn steam_input_types_map_to_their_numbers_and_glyph_families() {
        use crate::ControllerKind;
        use steamworks::InputType;
        assert_eq!(steam_input_type_number(InputType::Unknown), None);
        let rows = [
            (InputType::SteamController, 1, ControllerKind::SteamController),
            (InputType::XBox360Controller, 2, ControllerKind::Xbox),
            (InputType::XBoxOneController, 3, ControllerKind::Xbox),
            (InputType::GenericGamepad, 4, ControllerKind::Generic),
            (InputType::PS4Controller, 5, ControllerKind::PlayStation),
            (InputType::SwitchJoyConPair, 8, ControllerKind::Switch),
            (InputType::SwitchJoyConSingle, 9, ControllerKind::Switch),
            (InputType::SwitchProController, 10, ControllerKind::Switch),
            (InputType::PS3Controller, 12, ControllerKind::PlayStation),
            (InputType::PS5Controller, 13, ControllerKind::PlayStation),
            (InputType::SteamDeckController, 14, ControllerKind::SteamDeck),
        ];
        for (kind, number, family) in rows {
            assert_eq!(steam_input_type_number(kind), Some(number));
            assert_eq!(ControllerKind::from_steam_input_type(number), family, "{number}");
        }
        for (kind, number) in [
            (InputType::AppleMFiController, 6),
            (InputType::AndroidController, 7),
            (InputType::MobileTouch, 11),
        ] {
            assert_eq!(steam_input_type_number(kind), Some(number));
        }
        // Offline, no controller list: nothing to report.
        assert_eq!(RealBackend::offline().input_controller_type(), None);
    }

    #[test]
    fn a_zero_action_handle_is_not_cached() {
        let mut cache = BTreeMap::new();
        store_handle(&mut cache, "gameplay_jump", 0);
        assert!(
            cache.is_empty(),
            "a miss before the manifest loads must be asked again"
        );
        store_handle(&mut cache, "gameplay_jump", 7);
        assert_eq!(cache.get("gameplay_jump"), Some(&7));
    }
}
