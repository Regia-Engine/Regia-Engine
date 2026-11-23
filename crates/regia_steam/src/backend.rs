// SPDX-License-Identifier: MPL-2.0
//! The backend seam: everything `regia_steam` needs from the Steamworks API behind one trait,
//! so the plugin, systems and games are testable against [`crate::MockSteam`] and degrade to a
//! no-op when the real client is absent.

use std::sync::{Arc, Mutex};

/// A Steam application id (Valve's public test app is `480`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AppId(pub u32);

impl std::fmt::Display for AppId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A Steam user id. `SteamId(0)` is Steam's "invalid" id (a rich-presence join requested without
/// a friend carries one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SteamId(pub u64);

impl SteamId {
    /// Steam's invalid id (no known user).
    pub const INVALID: SteamId = SteamId(0);

    /// Whether this id is Steam's invalid id.
    pub fn is_invalid(self) -> bool {
        self.0 == 0
    }
}

/// Something went wrong talking to Steam. None of these are fatal: the game keeps running.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum SteamError {
    /// Steam is not running, the feature is off, or the platform has no Steam support. Every
    /// `Steam` call degrades to this when the backend is unavailable.
    #[error("Steam is unavailable in this build or not running")]
    Unavailable,
    /// The backend refused the operation (e.g. the achievement or stat is not configured for the
    /// app on the Steamworks partner site, or stats have not been received from Steam yet).
    #[error("Steam rejected the operation: {0}")]
    Rejected(String),
    /// The requested achievement id is not in the project's `config/achievements.ron`.
    #[error("unknown achievement id `{0}` (config/achievements.ron)")]
    UnknownAchievement(String),
}

/// Live connection state of the Steam backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SteamState {
    /// No backend: Steam is not running, the `steam` feature is off, or this is not a desktop
    /// build. The game continues without Steam features (specs/24 § 2).
    #[default]
    Unavailable,
    /// The backend initialized and is talking to a running Steam client.
    Ready,
    /// `restart_app_if_necessary` relaunched the game through Steam; this process is exiting.
    Restarting,
}

/// A Steam callback, drained once per frame by the plugin's `PreUpdate` pump and forwarded to
/// ECS messages and resources.
#[derive(Debug, Clone, PartialEq)]
pub enum SteamEvent {
    /// The Steam overlay opened or closed (`OverlayActive` becomes `value`).
    OverlayActivated {
        /// Whether the overlay is now open.
        active: bool,
    },
    /// A friend joined the game through the overlay or friends list (rich presence connect
    /// string). Forwarded to the game as [`crate::JoinRequested`].
    JoinRequested {
        /// The friend that joined; [`SteamId::INVALID`] when not coming from a friend.
        friend: SteamId,
        /// The game-defined connect string (DO27 maps it to its own connect flow).
        connect: String,
    },
    /// An achievement was stored to Steam's servers (following [`crate::Steam::unlock`] +
    /// `store_stats`).
    AchievementStored {
        /// The achievement's API name.
        id: String,
    },
    /// A stat upload finished (`store_stats`).
    StatsStored {
        /// Whether Steam accepted the upload.
        ok: bool,
    },
    /// A cloud file conflicts with the copy Steam has (another machine, or a session that
    /// was suspended). The pump forwards this as [`crate::CloudConflict`].
    CloudConflict {
        /// Cloud file name (`saves/Chapter1.frts`).
        file: String,
        /// `true` when the remote copy is newer than the one the game last wrote.
        remote_newer: bool,
    },
}

/// The queue shared by backend callbacks, the [`crate::Steam`] API and the frame pump: producers
/// push, [`crate::pump_steam`] drains once per frame. Never grows in a healthy frame; capacity is
/// not bounded because a stall must not drop Steam callbacks.
#[derive(Debug, Default)]
pub(crate) struct EventQueue(Mutex<Vec<SteamEvent>>);

impl EventQueue {
    pub(crate) fn push(&self, event: SteamEvent) {
        self.0.lock().map(|mut q| q.push(event)).ok();
    }

    pub(crate) fn take(&self) -> Vec<SteamEvent> {
        self.0
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }
}

/// Waits (bounded) for the `StatsStored` acknowledgement of a `store_stats` issued just before.
///
/// `pump_round` runs one round of platform callbacks (the real backend runs `run_callbacks` and
/// then sleeps between rounds). Events already queued when the wait begins belong to an earlier
/// upload and must not satisfy it — only a `StatsStored` that arrives after the wait started
/// acknowledges this flush. Everything seen, old and new, is pushed back so a later pump replays
/// it.
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) fn wait_stats_stored(events: &EventQueue, rounds: u32, mut pump_round: impl FnMut()) {
    let mut seen = events.take();
    for _ in 0..rounds {
        pump_round();
        let mut stored = false;
        for event in events.take() {
            stored |= matches!(event, SteamEvent::StatsStored { .. });
            seen.push(event);
        }
        if stored {
            break;
        }
    }
    for event in seen {
        events.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::{EventQueue, SteamEvent, wait_stats_stored};

    #[test]
    fn stats_flush_wait_ignores_a_callback_already_queued() {
        let events = EventQueue::default();
        // A StatsStored from an earlier store_stats the frame pump has not drained yet.
        events.push(SteamEvent::StatsStored { ok: true });
        let mut rounds = 0;
        wait_stats_stored(&events, 3, || rounds += 1);
        assert_eq!(
            rounds, 3,
            "a queued StatsStored must not acknowledge the flush: all rounds run"
        );
        assert_eq!(
            events.take(),
            [SteamEvent::StatsStored { ok: true }],
            "the queued event is kept for the next pump"
        );
    }

    #[test]
    fn stats_flush_wait_returns_on_a_fresh_callback() {
        let events = EventQueue::default();
        let mut rounds = 0;
        wait_stats_stored(&events, 5, || {
            rounds += 1;
            if rounds == 2 {
                events.push(SteamEvent::StatsStored { ok: true });
            }
        });
        assert_eq!(
            rounds, 2,
            "a StatsStored that arrives after the wait acknowledges it"
        );
    }

    #[test]
    fn stats_flush_wait_keeps_other_events() {
        let events = EventQueue::default();
        let mut rounds = 0;
        wait_stats_stored(&events, 4, || {
            rounds += 1;
            events.push(SteamEvent::AchievementStored { id: "A".to_owned() });
            if rounds == 3 {
                events.push(SteamEvent::StatsStored { ok: true });
            }
        });
        assert_eq!(rounds, 3);
        assert_eq!(
            events.take(),
            [
                SteamEvent::AchievementStored { id: "A".to_owned() },
                SteamEvent::AchievementStored { id: "A".to_owned() },
                SteamEvent::AchievementStored { id: "A".to_owned() },
                SteamEvent::StatsStored { ok: true },
            ],
            "non-stats events of every round are preserved in order"
        );
    }
}

/// Everything the plugin needs from Steamworks, behind one object so tests can drive a mock and
/// the real client stays in the (feature-gated) `real` module. All methods take `&self` (the
/// implementations are internally synchronised): gameplay systems call them through
/// `Res<Steam>`.
pub trait SteamBackend: Send + Sync {
    /// Connects to a running Steam client (SteamAPI init; `app_id` `None` lets the API resolve
    /// the app id itself). Called once from the plugin's `finish`, before the first frame. `Err`
    /// means Steam is not usable in this build or session — never fatal.
    fn initialize(&self, app_id: Option<AppId>) -> Result<SteamState, String>;
    /// Valve's shipping-init step, asked once from the plugin's `finish` before
    /// [`SteamBackend::initialize`] when an app id is known: `true` means Steam is relaunching
    /// the game through the client, so this process must exit without initializing.
    ///
    /// The backend answers, not the build: the real backend asks the Steamworks SDK, and every
    /// other backend (the mock, a host's own) defaults to `false`, so an injected backend is
    /// initialized even when the `steamworks` feature is compiled in (ADR-5375).
    fn restart_app_if_necessary(&self, app_id: AppId) -> bool {
        let _ = app_id;
        false
    }
    /// The live state of this backend.
    fn state(&self) -> SteamState;
    /// Runs pending Steamworks callbacks and returns the events they produced. Called once per
    /// frame from `PreUpdate`.
    fn pump(&self) -> Vec<SteamEvent>;
    /// The app id the backend initialized with, if known.
    fn app_id(&self) -> Option<AppId>;
    /// Unlocks an achievement by its Steam API name.
    fn unlock(&self, id: &str) -> Result<(), SteamError>;
    /// Clears an achievement (dev tooling; the Steamworks partner site gates this).
    fn clear_achievement(&self, id: &str) -> Result<(), SteamError>;
    /// Whether the achievement is already unlocked.
    fn achievement_unlocked(&self, id: &str) -> Result<bool, SteamError>;
    /// Resets all stats (and achievements too when `achievements_too`) for the test account.
    fn reset_all_stats(&self, achievements_too: bool) -> Result<(), SteamError>;
    /// Returns the new value of an integer stat after adding `delta`.
    fn add_stat_i32(&self, name: &str, delta: i32) -> Result<i32, SteamError>;
    /// Sets an integer stat.
    fn set_stat_i32(&self, name: &str, value: i32) -> Result<(), SteamError>;
    /// Sets a float stat.
    fn set_stat_f32(&self, name: &str, value: f32) -> Result<(), SteamError>;
    /// Reads an integer stat.
    fn stat_i32(&self, name: &str) -> Result<i32, SteamError>;
    /// Reads a float stat.
    fn stat_f32(&self, name: &str) -> Result<f32, SteamError>;
    /// Uploads changed stats and achievements to Steam (async; result arrives as
    /// [`SteamEvent::StatsStored`]).
    fn store_stats(&self) -> Result<(), SteamError>;
    /// Sets one rich presence key (`None` unsets it).
    fn set_rich_presence(&self, key: &str, value: Option<&str>) -> Result<(), SteamError>;
    /// Clears every rich presence key.
    fn clear_rich_presence(&self);
    /// Whether the game is running on a Steam Deck.
    fn is_steam_deck(&self) -> bool;
    /// Whether the user has the overlay enabled.
    fn overlay_enabled(&self) -> bool;
    /// Opens a validated overlay page (`friends`, `community`, `players`, `settings`, or
    /// `achievements`). Custom backends without overlay support remain unavailable.
    fn open_overlay(&self, _page: &str) -> Result<(), SteamError> {
        Err(SteamError::Unavailable)
    }
    /// The Steam client's language tag (e.g. `english`).
    fn ui_language(&self) -> String;
    /// The language the player chose for this game in its Steam properties, as a Steam API
    /// language name (`english`, `schinese`, `koreana`; see [`crate::steam_language_tag`]).
    /// Empty when unknown. The default is empty, so existing backends keep compiling.
    fn game_language(&self) -> String {
        String::new()
    }
    /// The local user's Steam id, if signed in.
    fn steam_id(&self) -> Option<SteamId>;
    /// Writes `data` to a cloud file named `name` (Steam Remote Storage). The name must not
    /// be empty or contain NUL; `/` makes folders (`saves/slot1.frts`). Backends may impose their
    /// own length limits.
    fn cloud_write(&self, name: &str, data: &[u8]) -> Result<(), SteamError>;
    /// Reads a cloud file; `Ok(None)` when the file does not exist.
    fn cloud_read(&self, name: &str) -> Result<Option<Vec<u8>>, SteamError>;
    /// Deletes a cloud file; deleting a missing file is `Ok(())`.
    fn cloud_delete(&self, name: &str) -> Result<(), SteamError>;
    /// Whether a cloud file exists.
    fn cloud_exists(&self, name: &str) -> Result<bool, SteamError>;
    /// Every cloud file name, sorted.
    fn cloud_list(&self) -> Result<Vec<String>, SteamError>;
    /// Whether the Steam client is logged on to Steam's servers. `false` while running means the
    /// client is in offline mode ([`crate::SteamStatus::offline`]). Defaults to "ready".
    fn logged_on(&self) -> bool {
        self.state() == SteamState::Ready
    }
    /// Blocks (bounded) until a `store_stats` upload queued just before has been acknowledged, so
    /// shutting the API down right after does not drop it. The real backend pumps Steam's
    /// callbacks for a fixed number of rounds until `UserStatsStored` arrives; the default is a
    /// no-op (the mock acknowledges through its event queue).
    fn await_stats_stored(&self) {}
    /// Shuts the backend down (SteamAPI shutdown) and drops its callback registrations.
    /// **Idempotent**: calling it twice, or before `initialize`, is a no-op. Afterwards
    /// [`SteamBackend::state`] is [`SteamState::Unavailable`]. The shared client a host cloned
    /// out of `Steam::client` (with the `steamworks` feature) keeps the API alive until the host drops it too.
    fn shutdown(&self) {}
    /// Initializes Steam Input (ISteamInput). Returns whether the interface is usable; the
    /// default (mock-less, unavailable backends) is `false`. Safe to call repeatedly.
    fn input_init(&self) -> bool {
        false
    }
    /// How many controllers Steam Input currently reports.
    fn input_connected(&self) -> usize {
        0
    }
    /// Activates the Steam Input action set named `set` on every connected controller. Cheap
    /// enough to call when the set changes.
    fn input_activate_action_set(&self, set: &str) -> Result<(), SteamError> {
        let _ = set;
        Err(SteamError::Unavailable)
    }
    /// The state of a digital (button) action on the first controller; `None` when the action
    /// is unknown, inactive in the current set, or no controller is connected.
    fn input_digital(&self, action: &str) -> Option<bool> {
        let _ = action;
        None
    }
    /// The state of an analog action `(x, y)` on the first controller; `None` as for
    /// [`SteamBackend::input_digital`].
    fn input_analog(&self, action: &str) -> Option<(f32, f32)> {
        let _ = action;
        None
    }
    /// Resolves a digital action to a handle the caller can cache.
    ///
    /// `None` means this backend has no handle cache: keep calling [`Self::input_digital`] every
    /// frame. `Some(0)` means the name was resolved and the action is absent. Any other value is
    /// a handle for [`Self::input_read_digital`]. The default is `None`, so existing backends keep
    /// the name path.
    fn input_digital_handle(&self, action: &str) -> Option<u64> {
        let _ = action;
        None
    }
    /// Resolves an analog action to a handle. See [`Self::input_digital_handle`].
    fn input_analog_handle(&self, action: &str) -> Option<u64> {
        let _ = action;
        None
    }
    /// Reads a digital action by a handle from [`Self::input_digital_handle`]. `None` when the
    /// handle is `0`, the action is inactive, or no controller is connected.
    fn input_read_digital(&self, handle: u64) -> Option<bool> {
        let _ = handle;
        None
    }
    /// Reads an analog action by a handle from [`Self::input_analog_handle`].
    fn input_read_analog(&self, handle: u64) -> Option<(f32, f32)> {
        let _ = handle;
        None
    }
    /// The `ESteamInputType` number (public Steamworks documentation: 2 Xbox 360, 5 PS4, 10
    /// Switch Pro, 13 PS5, 14 Steam Deck, ...) of the first controller Steam Input reports;
    /// `None` when unknown or no controller is connected. Steam Input knows the physical pad
    /// behind its virtual one, so this is what picks the glyphs while the bridge is active
    /// (ADR-1332). Defaults to `None`.
    fn input_controller_type(&self) -> Option<u32> {
        None
    }
    /// Shows the Deck's floating on-screen keyboard over a text field. Defaults to
    /// [`SteamError::Unavailable`]; the real backend overrides it.
    fn show_floating_keyboard(&self, request: &crate::deck_prep::KeyboardRequest) -> Result<(), SteamError> {
        let _ = request;
        Err(SteamError::Unavailable)
    }
    /// Dismisses the floating keyboard. Defaults to a no-op.
    fn dismiss_floating_keyboard(&self) {}
    /// The shared native client handle, type-erased (`steamworks::Client` for the real backend,
    /// `None` for the mock and the unavailable backend). Hosts use the typed
    /// `Steam::client` (with the `steamworks` feature) instead.
    fn raw_client(&self) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        None
    }
    /// The lobby backend over ISteamMatchmaking (spec 89); `None` when Steam is not ready. The
    /// default (backends without lobbies) is `None`.
    fn lobbies(&self) -> Option<Arc<dyn regia_net::lobby::LobbyBackend>> {
        None
    }
    /// Peer-to-peer messages over ISteamNetworkingMessages for the Steam sockets transport
    /// (spec 89 § 7); `None` when Steam is not ready. The default is `None`.
    fn p2p(&self) -> Option<Arc<dyn crate::sockets::P2pMessages>> {
        None
    }
    /// The leaderboard service over ISteamUserStats (spec 24 § 2a, ADR-2340); `None` when Steam
    /// is not ready. The default (backends without leaderboards) is `None`.
    fn leaderboards(&self) -> Option<Arc<dyn regia_framework::leaderboard::LeaderboardService>> {
        None
    }
    /// Whether the user owns `app` (`BIsSubscribedApp`). Defaults to
    /// [`SteamError::Unavailable`].
    fn is_subscribed_app(&self, app: AppId) -> Result<bool, SteamError> {
        let _ = app;
        Err(SteamError::Unavailable)
    }
    /// Whether the user owns DLC `app` and has it installed (`BIsDlcInstalled`). Defaults to
    /// [`SteamError::Unavailable`].
    fn is_dlc_installed(&self, app: AppId) -> Result<bool, SteamError> {
        let _ = app;
        Err(SteamError::Unavailable)
    }
    /// Whether `app` is installed (`BIsAppInstalled`). Defaults to [`SteamError::Unavailable`].
    fn is_app_installed(&self, app: AppId) -> Result<bool, SteamError> {
        let _ = app;
        Err(SteamError::Unavailable)
    }
    /// Where `app` is or would be installed (`GetAppInstallDir`); `Ok(None)` when Steam returns
    /// nothing. Defaults to [`SteamError::Unavailable`].
    fn app_install_dir(&self, app: AppId) -> Result<Option<String>, SteamError> {
        let _ = app;
        Err(SteamError::Unavailable)
    }
    /// Sends one game-recording timeline call (ISteamTimeline). The caller has validated it.
    /// Defaults to [`SteamError::Unavailable`].
    fn timeline(&self, call: &crate::services::TimelineCall) -> Result<(), SteamError> {
        let _ = call;
        Err(SteamError::Unavailable)
    }
}

/// A backend that exists but does nothing: the state every `Steam` call reports when Steam is
/// unavailable. Used when the `steam` feature is off, on Android, or after a failed init.
// Unused (outside tests) when the real client is compiled in.
#[cfg(any(test, not(all(feature = "steamworks", not(target_os = "android")))))]
#[derive(Debug, Default)]
pub(crate) struct UnavailableBackend;

#[cfg(any(test, not(all(feature = "steamworks", not(target_os = "android")))))]
impl SteamBackend for UnavailableBackend {
    fn initialize(&self, _app_id: Option<AppId>) -> Result<SteamState, String> {
        Err("no Steamworks backend in this build (steam feature off or not desktop)".into())
    }
    fn state(&self) -> SteamState {
        SteamState::Unavailable
    }
    fn pump(&self) -> Vec<SteamEvent> {
        Vec::new()
    }
    fn app_id(&self) -> Option<AppId> {
        None
    }
    fn unlock(&self, id: &str) -> Result<(), SteamError> {
        let _ = id;
        Err(SteamError::Unavailable)
    }
    fn clear_achievement(&self, id: &str) -> Result<(), SteamError> {
        let _ = id;
        Err(SteamError::Unavailable)
    }
    fn achievement_unlocked(&self, id: &str) -> Result<bool, SteamError> {
        let _ = id;
        Err(SteamError::Unavailable)
    }
    fn reset_all_stats(&self, achievements_too: bool) -> Result<(), SteamError> {
        let _ = achievements_too;
        Err(SteamError::Unavailable)
    }
    fn add_stat_i32(&self, name: &str, delta: i32) -> Result<i32, SteamError> {
        let _ = (name, delta);
        Err(SteamError::Unavailable)
    }
    fn set_stat_i32(&self, name: &str, value: i32) -> Result<(), SteamError> {
        let _ = (name, value);
        Err(SteamError::Unavailable)
    }
    fn set_stat_f32(&self, name: &str, value: f32) -> Result<(), SteamError> {
        let _ = (name, value);
        Err(SteamError::Unavailable)
    }
    fn stat_i32(&self, name: &str) -> Result<i32, SteamError> {
        let _ = name;
        Err(SteamError::Unavailable)
    }
    fn stat_f32(&self, name: &str) -> Result<f32, SteamError> {
        let _ = name;
        Err(SteamError::Unavailable)
    }
    fn store_stats(&self) -> Result<(), SteamError> {
        Err(SteamError::Unavailable)
    }
    fn set_rich_presence(&self, key: &str, value: Option<&str>) -> Result<(), SteamError> {
        let _ = (key, value);
        Err(SteamError::Unavailable)
    }
    fn clear_rich_presence(&self) {}
    fn is_steam_deck(&self) -> bool {
        false
    }
    fn overlay_enabled(&self) -> bool {
        false
    }
    fn ui_language(&self) -> String {
        String::new()
    }
    fn steam_id(&self) -> Option<SteamId> {
        None
    }
    fn cloud_write(&self, name: &str, data: &[u8]) -> Result<(), SteamError> {
        let _ = (name, data);
        Err(SteamError::Unavailable)
    }
    fn cloud_read(&self, name: &str) -> Result<Option<Vec<u8>>, SteamError> {
        let _ = name;
        Err(SteamError::Unavailable)
    }
    fn cloud_delete(&self, name: &str) -> Result<(), SteamError> {
        let _ = name;
        Err(SteamError::Unavailable)
    }
    fn cloud_exists(&self, name: &str) -> Result<bool, SteamError> {
        let _ = name;
        Err(SteamError::Unavailable)
    }
    fn cloud_list(&self) -> Result<Vec<String>, SteamError> {
        Err(SteamError::Unavailable)
    }
}

/// Picks the backend the plugin runs with: an explicitly injected one (tests, replays), else the
/// real Steamworks client when the feature and platform allow it, else `UnavailableBackend`.
pub(crate) fn default_backend() -> Arc<dyn SteamBackend> {
    #[cfg(all(feature = "steamworks", not(target_os = "android")))]
    {
        Arc::new(crate::real::RealBackend::offline())
    }
    #[cfg(not(all(feature = "steamworks", not(target_os = "android"))))]
    {
        Arc::new(UnavailableBackend)
    }
}
