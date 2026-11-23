// SPDX-License-Identifier: MPL-2.0
//! Steam integration (`docs/specs/24-steam.md`).
//!
//! Desktop only. The umbrella crate's `steam` feature (`regia = { features = ["steam"] }`)
//! re-exports this crate; the real client needs this crate's own `steamworks` feature, which links
//! Valve's Steamworks SDK (see the crate README for where the SDK goes — it is never committed). Without the feature
//! — and on Android always — the crate compiles to a graceful no-op: [`SteamState::Unavailable`],
//! one warning at startup, every call degrades to [`SteamError::Unavailable`], the game keeps
//! running.
//!
//! - [`SteamPlugin`] initializes the backend: app id from project settings (explicit →
//!   `SteamSettings` → `Regia.toml` `[steam]` → `STEAM_APP_ID` → `steam_appid.txt`), and in
//!   shipping `restart_app_if_necessary` relaunches the game through Steam when it was not
//!   launched there.
//! - The plugin's frame pump runs Steamworks callbacks once per frame in `PreUpdate` and
//!   forwards them to the messages and resources below.
//! - [`Steam`] is the game-facing API: `steam.unlock("FIRST_BLOOD")`,
//!   `steam.stat_add("kills", 1)` (with threshold auto-unlock from `config/achievements.ron`),
//!   rich presence, overlay queries, dev reset tooling.
//! - [`OverlayActive`] + [`OverlayOpened`]/[`OverlayClosed`]: the overlay suspends gameplay
//!   input contexts and can pause game time (opt-in).
//! - [`JoinRequested`] arrives when a friend joins through the overlay or friends list (DO27 maps
//!   it to its connect flow).
//! - [`SteamDeck`] carries Deck detection and the `Deck` render profile flag.
//! - [`cloud`] defines [`SaveBackend`] (the seam for the framework's `SaveGame`, M20) and emits
//!   the Steam Auto-Cloud configuration for `user://`.
//! - [`input`] is the Steam Input glue (ADR-350): input contexts become action sets, the in-game
//!   actions manifest is generated from `config/input.ron`, and [`SteamInputSettings`] turns the
//!   bridge on (off by default).
//! - [`lobby`] and [`sockets`] (spec 89): Steam lobbies behind `regia_net`'s `LobbyBackend`,
//!   and [`SteamSocket`], a `regia_net` datagram socket over Steam peer-to-peer messages;
//!   [`SteamLobbyPlugin`] wires both into `regia_net::Lobbies`. [`MockSteamNet`] runs several
//!   mock users on one network for tests.
//! - [`deck_glyphs`] turns an action's gamepad bindings into controller prompts per glyph family
//!   and checks a game's glyph set; [`deck_perf`] holds the Deck performance presets and the
//!   gamescope FSR 1 profile (ADR-526).
//! - [`deck_layout`] lays out an input context as a controls screen per glyph family and checks
//!   that a config is playable on a controller alone (ADR-1332).
//! - [`leaderboards`] and [`services`] (spec 24 § 2a, ADR-2340): [`SteamLeaderboardPlugin`] gives
//!   the framework's `Leaderboard` resource the Steam leaderboard service; [`Steam::owns_app`],
//!   [`Steam::dlc_installed`] and [`Steam::app_install_dir`] answer ownership;
//!   [`Steam::timeline_event`] and friends mark the game-recording timeline.
//!
//! The real-client integration test (`tests/real_client.rs`) runs only when the `steamworks`
//! feature is linked and Steam is running; it skips otherwise and never fails. Its two-account
//! run is `#[ignore]`d (not run, never a pass) and drives [`real_run`], the owner-session harness.

pub mod achievements;
mod agent;
pub mod backend;
pub mod cloud;
// The framework save store over the Steam backends (spec 91 § 2.2, ADR-1691).
mod cloud_store;
pub mod deck;
pub mod deck_glyphs;
pub mod deck_layout;
pub mod deck_perf;
pub mod deck_prep;
pub mod input;
mod language;
pub mod lobby;
// Platform stats from finished-match records (audit gap GR-10.3).
pub mod match_stats;
pub mod mock;
pub mod mock_net;
mod overlay;
// REGIA_STEAM_OVERLAY_TRACE: the overlay bug-report trace (ADR-7349).
pub mod overlay_trace;
mod plugin;
mod presence;
mod pump;
mod resource;
pub mod sockets;
mod status;

// The real Steamworks client: desktop only, and only when the feature links the SDK
// (steamworks-sys has no Android support; without the feature there is nothing to talk to).
#[cfg(all(feature = "steamworks", not(target_os = "android")))]
pub mod real;
#[cfg(all(feature = "steamworks", not(target_os = "android")))]
mod real_lobby;
// The `LobbyInfo` the real lobby backend builds from raw Steam data: pure, so its hostile-data test
// runs without the SDK (w7-runtime-smalls-z item 6).
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
mod real_lobby_info;
// Steam leaderboards over the bindings (ADR-2340).
#[cfg(all(feature = "steamworks", not(target_os = "android")))]
mod real_leaderboards;
// Leaderboards, app and DLC ownership, timeline markers (spec 24 § 2a, ADR-2340).
mod agent_services;
pub mod leaderboards;
pub mod services;
// Steam Workshop upload and subscribe, subscribed items mounted at mods:// (ADR-2418).
pub mod workshop;
mod workshop_agent;
// The two-account Steam acceptance harness: setup, host/guest script, evidence logs, verdict (ADR-4896).
pub mod real_run;
// Its template-match leg (N08): net_game over Steam, judged from two endpoint logs (ADR-5375).
pub mod real_run_net;
// Store-ticket hook for `regia_net` admission (spec 120 § 14, ADR-7954).
pub mod join_ticket;

#[cfg(all(feature = "steamworks", not(target_os = "android")))]
pub use real::RealBackend;

pub use backend::{AppId, SteamBackend, SteamError, SteamEvent, SteamId, SteamState};
/// The Steam Auto-Cloud configuration text `regia package --steam` writes as `steamcloud.md`.
pub use cloud::autocloud_document;
pub use cloud::{CloudConflict, LocalSaveBackend, SaveBackend, SaveError, SteamRemoteStorage};
pub use deck::{DeckFrameCap, DeckFrameCapSetting, DeckPanel, DeckProfile, SteamDeck, SteamDeckDetected};
pub use deck_glyphs::{Prompt, PromptControl, action_prompts, glyph_set_keys, missing_glyphs};
pub use deck_layout::{
    LayoutCheck, LayoutIssue, LayoutRow, LayoutSeverity, LayoutSheet, check_layout, layout_sheet,
    layout_sheets,
};
pub use deck_perf::{DeckDisplay, DeckPerfKnobs, DeckPerfPreset, DeckPerfSettings, DeckPerfState};
pub use deck_prep::{
    ActiveGlyphs, ControllerKind, DeckPowerPolicy, DeckRenderDefaults, DeckSensorConfig, DeckSensorSample,
    DeckUpscaler, GlyphFamily, KeyboardMode, KeyboardRequest, KeyboardState, PowerReason, PowerSource,
    TextFieldFocus, ThermalGovernor, ThermalState,
};
pub use input::{
    ManifestError, STEAM_INPUT_PAD_DEVICE, SteamInputSettings, SteamInputStatus, UI_ACTIONS, action_manifest,
};
pub use input::{action_manifest_file_name, action_manifest_from_ron};
pub use language::{STEAM_LANGUAGES, steam_language_tag};
pub use leaderboards::{SteamLeaderboardPlugin, steam_leaderboards};
pub use lobby::{SteamLobbyPlugin, steam_lobbies};
pub use mock::MockSteam;
pub use mock_net::{
    MOCK_FLAP_FIRST_MS, MOCK_FLAP_LONG_PAST_TIMEOUT_MS, MOCK_FLAP_OUTAGE_MS, MOCK_FLAP_PERIOD_MS,
    MOCK_HEADER_BYTES, MOCK_LINK_PROFILES, MOCK_MOBILE_MTU_BYTES, MOCK_REBIND_GAP_MS, MockBurst,
    MockDirection, MockLink, MockNetStats, MockSteamNet,
};
pub use overlay::{OverlayActive, OverlayClosed, OverlayOpened};
pub use overlay_trace::{STEAM_OVERLAY_TRACE_ENV, SteamOverlayTrace};
pub use plugin::{STEAM_APP_ID_ENV, STEAM_APPID_FILE, SteamPlugin, SteamSettings};
/// Steam's own rich-presence key names (`status`, `connect`, `steam_display`).
pub use presence::presence_keys;
pub use presence::{AchievementUnlocked, JoinRequested, MAX_CONNECT_BYTES, StatsStored};
pub use resource::Steam;
pub use services::{AppOwnership, TimelineCall, TimelineClip, TimelineEvent, TimelineLog, TimelineMode};
pub use sockets::{
    P2pMessages, SteamAccept, SteamLobbyTransport, SteamSocket, SteamSocketStats, steam_id_of,
    steam_peer_addr,
};
pub use status::SteamStatus;
pub use workshop::{
    MockWorkshop, SteamWorkshop, SteamWorkshopPlugin, UnavailableWorkshop, WorkshopError, WorkshopEvent,
    WorkshopItemId, WorkshopService, WorkshopUpload, workshop_mod_id,
};

pub mod store;

// Store-ticket hook (spec 120 § 14, ADR-7954). The live session type is behind `steamworks`.
#[cfg(all(feature = "steamworks", not(target_os = "android")))]
pub use join_ticket::LiveSteamTickets;
pub use join_ticket::{
    BeginAuthError, STEAM_AUTH_TICKET_MAGIC, SteamAuthAnswer, SteamAuthApi, SteamAuthSessions,
    SteamJoinTicketCheck, SteamTicketSource, TableTickets, decode_steam_auth_ticket,
    encode_steam_auth_ticket,
};
