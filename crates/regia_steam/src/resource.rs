// SPDX-License-Identifier: MPL-2.0
//! The [`Steam`] resource: the game-facing API (`steam.unlock("id")`, `steam.stat_add("kills", 1)`,
//! rich presence, overlay queries). Calls take `&self` — gameplay systems use `Res<Steam>` — and
//! everything degrades to [`SteamError::Unavailable`] when Steam is not running.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use regia_app::prelude::*;

use crate::achievements::{AchievementsConfig, Thresholds};
use crate::backend::{AppId, EventQueue, SteamBackend, SteamError, SteamEvent, SteamId, SteamState};

/// The Steam API. Inserted by [`crate::SteamPlugin`]; read it as `Res<Steam>`.
///
/// When Steam is unavailable (client not running, `steam` feature off, Android) every method
/// returns [`SteamError::Unavailable`] and the plugin has logged one warning — features are off,
/// the game continues (specs/24 § 2 "graceful degradation").
#[derive(Resource, Clone)]
pub struct Steam {
    pub(crate) backend: Arc<dyn SteamBackend>,
    pub(crate) state: SteamState,
    pub(crate) achievements: Arc<AchievementsConfig>,
    pub(crate) thresholds: Arc<Thresholds>,
    pub(crate) events: Arc<EventQueue>,
    pub(crate) overlay_open: bool,
    pub(crate) deck: bool,
    /// Whether this plugin set `Time::paused` for the overlay (so it only unpauses what it
    /// paused, never a game-initiated pause).
    pub(crate) paused_by_overlay: bool,
    /// Achievement ids already surfaced as [`crate::AchievementUnlocked`] (the local threshold
    /// event or Steam's own `UserAchievementStored` callback, whichever comes first), so the
    /// second report of the same unlock is dropped. Shared between clones.
    pub(crate) announced: Arc<Mutex<BTreeSet<String>>>,
    /// Threshold achievements already seen unlocked, so a stat tick skips their Steam query.
    pub(crate) known_unlocked: Arc<Mutex<BTreeSet<String>>>,
    /// What the engine last sent to Steam's timeline (`Steam::timeline_log`). Shared between
    /// clones.
    pub(crate) timeline: Arc<Mutex<crate::services::TimelineLog>>,
}

/// Rejects ids and names the native Steamworks bindings would abort on (they `unwrap` a C-string
/// conversion): empty strings and interior NUL bytes. The engine ships `panic=abort`, so user
/// data must come back as an error instead.
pub(crate) fn check_name(what: &str, name: &str) -> Result<(), SteamError> {
    if name.is_empty() || name.contains('\0') {
        return Err(SteamError::Rejected(format!(
            "{what} must not be empty or contain NUL"
        )));
    }
    Ok(())
}

/// The supported public overlay pages, checked before the bindings convert to a C string.
pub(crate) fn check_overlay_page(page: &str) -> Result<(), SteamError> {
    if matches!(
        page,
        "friends" | "community" | "players" | "settings" | "achievements"
    ) {
        Ok(())
    } else {
        Err(SteamError::Rejected("unknown overlay page".to_owned()))
    }
}

// Manual: the backend trait object is not `Debug` (the real client would drag the whole
// Steamworks API into the impl), and a derived `Debug` on it fails the derive as a whole.
impl std::fmt::Debug for Steam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Steam")
            .field("state", &self.state)
            .field("achievements", &self.achievements)
            .field("overlay_open", &self.overlay_open)
            .field("deck", &self.deck)
            .finish_non_exhaustive()
    }
}

impl Steam {
    /// The live state of the Steam connection.
    pub fn state(&self) -> SteamState {
        self.state
    }

    /// Whether the backend is connected and ready.
    pub fn is_ready(&self) -> bool {
        self.state == SteamState::Ready
    }

    /// The one shared native Steamworks client, for hosts that need raw Steamworks calls
    /// (networking sockets, matchmaking, ...). **Never call `Client::init` yourself**: Steam
    /// allows one API instance per process and the engine already owns it. The handle is a cheap
    /// clone; `None` when Steam is unavailable (not running, or the mock backend). Hosts must
    /// build against the same `steamworks` version as `regia_steam` (see `docs/03`), or the
    /// type does not unify.
    #[cfg(all(feature = "steamworks", not(target_os = "android")))]
    pub fn client(&self) -> Option<steamworks::Client> {
        if self.state != SteamState::Ready {
            return None;
        }
        self.backend
            .raw_client()
            .and_then(|any| any.downcast::<steamworks::Client>().ok())
            .map(|client| *client)
    }

    /// Shuts the Steam backend down: flushes pending stats, clears rich presence and drops the
    /// engine's callbacks and client handle. **Idempotent**; the plugin calls it on app
    /// termination, so hosts only need it to leave Steam early. Afterwards every call degrades
    /// to [`SteamError::Unavailable`].
    pub fn shutdown(&mut self) {
        if self.state == SteamState::Ready {
            match self.backend.store_stats() {
                // The upload is asynchronous: let Steam acknowledge it before the client goes.
                Ok(()) => self.backend.await_stats_stored(),
                Err(err) => {
                    tracing::warn!(target: "regia_steam", "cannot flush stats on shutdown: {err}");
                }
            }
            self.backend.clear_rich_presence();
        }
        self.backend.shutdown();
        // `overlay_open` stays as it is: the next pump sees the backend down, replays the
        // overlay-closed path (input contexts, `Time` pause, `OverlayActive`) and then clears it.
        self.state = SteamState::Unavailable;
    }

    /// `Err(Unavailable)` unless the connection is live: after [`Steam::shutdown`] a backend that
    /// keeps its own state must not see further calls.
    pub(crate) fn live(&self) -> Result<(), SteamError> {
        if self.state == SteamState::Ready {
            Ok(())
        } else {
            Err(SteamError::Unavailable)
        }
    }

    /// The app id the backend initialized with, if known.
    pub fn app_id(&self) -> Option<AppId> {
        self.backend.app_id()
    }

    /// The local user's Steam id, if signed in.
    pub fn steam_id(&self) -> Option<SteamId> {
        self.backend.steam_id()
    }

    /// The player's per-game language from the game's Steam properties (a Steam API language name
    /// such as `japanese`); empty when unavailable. Falls back to nothing: the Steam client's UI
    /// language is [`Steam::language`], which is not the game's language.
    pub fn game_language(&self) -> String {
        if self.state == SteamState::Ready {
            self.backend.game_language()
        } else {
            String::new()
        }
    }

    /// The Steam client's UI language tag (e.g. `english`); empty unless the state is `Ready`.
    /// This is not the player's per-game language: for Localization use [`Steam::game_language`]
    /// (published into `LocaleHints::steam` by the plugin).
    pub fn language(&self) -> String {
        if self.state == SteamState::Ready {
            self.backend.ui_language()
        } else {
            String::new()
        }
    }

    /// Unlocks an achievement by its Steam API name (`steam.unlock("FIRST_BLOOD")`). The id must
    /// be configured in `config/achievements.ron` (and on the Steamworks partner site). The unlock
    /// reaches Steam's servers with the next [`Steam::store_stats`] or automatic flush.
    pub fn unlock(&self, id: &str) -> Result<(), SteamError> {
        check_name("achievement id", id)?;
        if !self.achievements.contains(id) {
            return Err(SteamError::UnknownAchievement(id.to_owned()));
        }
        self.live()?;
        self.backend.unlock(id)
    }

    /// Whether the achievement is already unlocked (`Ok(false)` when it is not).
    pub fn achievement(&self, id: &str) -> Result<bool, SteamError> {
        check_name("achievement id", id)?;
        if !self.achievements.contains(id) {
            return Err(SteamError::UnknownAchievement(id.to_owned()));
        }
        self.live()?;
        self.backend.achievement_unlocked(id)
    }

    /// Clears an achievement (dev tooling for the test account; Steam only honours it for
    /// accounts with the app's developer privileges).
    pub fn clear_achievement(&self, id: &str) -> Result<(), SteamError> {
        check_name("achievement id", id)?;
        if !self.achievements.contains(id) {
            return Err(SteamError::UnknownAchievement(id.to_owned()));
        }
        self.live()?;
        self.backend.clear_achievement(id)?;
        self.known_unlocked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        self.announced
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        Ok(())
    }

    /// **Dev tooling.** Resets every stat (and every achievement too when `achievements_too`) of
    /// the signed-in test account — specs/24 § 2 "reset all for the test account".
    pub fn reset_all_stats(&self, achievements_too: bool) -> Result<(), SteamError> {
        self.live()?;
        self.backend.reset_all_stats(achievements_too)?;
        if achievements_too {
            self.announced.lock().unwrap_or_else(|e| e.into_inner()).clear();
            self.known_unlocked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
        Ok(())
    }

    /// Adds `delta` to an integer stat and returns the new value
    /// (`steam.stat_add("kills", 1)`). Steam rejects unknown stat names (the partner site is the
    /// source of truth). Achievements whose configured threshold the new value crosses unlock
    /// automatically this frame and raise [`crate::AchievementUnlocked`] (once per unlock, even
    /// when Steam later confirms it).
    ///
    /// Once the stat is written the call returns `Ok(new value)`: a threshold unlock that Steam
    /// refuses (an id missing on the partner site) is logged and skipped, never turned into an
    /// error, because the stat change is already applied and a retry would count it twice.
    pub fn stat_add(&self, name: &str, delta: i32) -> Result<i32, SteamError> {
        check_name("stat name", name)?;
        self.live()?;
        let value = self.backend.add_stat_i32(name, delta)?;
        self.unlock_crossed(name, value);
        Ok(value)
    }

    /// Unlocks every achievement whose threshold `value` of stat `name` crossed. The stat is
    /// already committed, so a failing unlock is logged and does not fail the stat call (a retry
    /// would add the delta twice). Each unlock is announced at once; the pump drops the backend's
    /// own `AchievementStored` for the same id (`announced`), so it fires once.
    fn unlock_crossed(&self, name: &str, value: i32) {
        self.unlock_ids(name, self.thresholds.crossed_by(name, value));
    }

    /// The unlock half of a threshold crossing, shared by the integer and float stat writes.
    fn unlock_ids(&self, name: &str, ids: Vec<String>) {
        for id in ids {
            // Only unlock what is verifiably locked: Steam (and the UI) treats a repeated unlock
            // as noise, and a failed query is not a license to unlock.
            if self
                .known_unlocked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&id)
            {
                continue;
            }
            match self.backend.achievement_unlocked(&id) {
                Ok(false) => {}
                Ok(true) => {
                    self.known_unlocked
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(id);
                    continue;
                }
                Err(_) => continue,
            }
            match self.unlock(&id) {
                Ok(()) => {
                    self.known_unlocked
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(id.clone());
                    self.events.push(SteamEvent::AchievementStored { id });
                }
                Err(err) => {
                    tracing::warn!(
                        target: "regia_steam",
                        achievement = %id,
                        stat = name,
                        "threshold unlock failed: {err}"
                    );
                }
            }
        }
    }

    /// Sets an integer stat. Achievements whose threshold the new value reaches unlock like in
    /// [`Steam::stat_add`].
    pub fn stat_set_i32(&self, name: &str, value: i32) -> Result<(), SteamError> {
        check_name("stat name", name)?;
        self.live()?;
        self.backend.set_stat_i32(name, value)?;
        self.unlock_crossed(name, value);
        Ok(())
    }

    /// Sets a float stat. Achievements whose threshold the new value reaches unlock like in
    /// [`Steam::stat_add`]; a non-finite value is stored but unlocks nothing.
    pub fn stat_set_f32(&self, name: &str, value: f32) -> Result<(), SteamError> {
        check_name("stat name", name)?;
        self.live()?;
        self.backend.set_stat_f32(name, value)?;
        self.unlock_ids(name, self.thresholds.crossed_by_f32(name, value));
        Ok(())
    }

    /// Reads an integer stat.
    pub fn stat_i32(&self, name: &str) -> Result<i32, SteamError> {
        check_name("stat name", name)?;
        self.live()?;
        self.backend.stat_i32(name)
    }

    /// Reads a float stat.
    pub fn stat_f32(&self, name: &str) -> Result<f32, SteamError> {
        check_name("stat name", name)?;
        self.live()?;
        self.backend.stat_f32(name)
    }

    /// Uploads every changed stat and achievement to Steam. The plugin flushes pending stats on
    /// app exit; call this when the game wants immediate persistence (level end, match end).
    /// The result arrives as [`crate::StatsStored`].
    pub fn store_stats(&self) -> Result<(), SteamError> {
        self.live()?;
        self.backend.store_stats()
    }

    /// Sets one rich presence key (`steam.set_presence("status", Some("In the menus"))`). Steam
    /// shows the `status` and `connect` keys to friends; `connect` is what a friend's
    /// [`crate::JoinRequested`] carries back to the game.
    pub fn set_presence(&self, key: &str, value: Option<&str>) -> Result<(), SteamError> {
        self.live()?;
        self.backend.set_rich_presence(key, value)
    }

    /// Clears every rich presence key (call on disconnect, so the friends list does not show a
    /// stale session).
    pub fn clear_presence(&self) {
        if self.state != SteamState::Ready {
            return;
        }
        self.backend.clear_rich_presence();
    }

    /// Whether the Steam overlay is currently open (mirrors [`crate::OverlayActive`]; updated by
    /// the frame pump).
    pub fn overlay_open(&self) -> bool {
        self.overlay_open
    }

    /// Whether the user has the overlay enabled at all (for UI hints like "press Shift-Tab").
    pub fn overlay_enabled(&self) -> bool {
        self.state == SteamState::Ready && self.backend.overlay_enabled()
    }

    /// Opens `friends`, `community`, `players`, `settings`, or `achievements` in the overlay.
    /// Unknown pages (including empty or NUL-bearing input) return [`SteamError::Rejected`].
    /// An unavailable client returns [`SteamError::Unavailable`].
    pub fn open_overlay(&self, page: &str) -> Result<(), SteamError> {
        check_overlay_page(page)?;
        self.live()?;
        self.backend.open_overlay(page)
    }

    /// Shows the Deck's floating keyboard over a text field (`Unavailable` without Steam).
    pub fn show_floating_keyboard(
        &self,
        request: &crate::deck_prep::KeyboardRequest,
    ) -> Result<(), SteamError> {
        self.backend.show_floating_keyboard(request)
    }

    /// Dismisses the floating keyboard.
    pub fn dismiss_floating_keyboard(&self) {
        self.backend.dismiss_floating_keyboard();
    }

    /// Whether the game is running on a Steam Deck (mirrors [`crate::SteamDeck`]; specs/24 § 2
    /// "Steam Deck detection → Deck render profile flag").
    pub fn is_steam_deck(&self) -> bool {
        self.deck
    }
}
