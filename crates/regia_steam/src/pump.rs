// SPDX-License-Identifier: MPL-2.0
//! The frame pump: Steamworks callbacks once per frame in `PreUpdate` (specs/24 § 2 "Init"),
//! forwarded to ECS messages and resources, and the stat flush on app exit.

use regia_app::prelude::*;

use crate::SteamSettings;
use crate::backend::{SteamEvent, SteamState};
use crate::cloud::{self, CloudConflict};
use crate::overlay::{OverlayActive, SuspendedContexts, restore_contexts, suspend_contexts};
use crate::presence::{AchievementUnlocked, JoinRequested, MAX_CONNECT_BYTES, StatsStored};
use crate::resource::Steam;

/// `PreUpdate`: runs pending Steam callbacks and forwards the events. Per-frame work: one
/// Steamworks callback batch plus a few resource writes — measured, and a no-op when Steam is
/// unavailable.
// ECS systems take one parameter per resource/message they touch; fourteen is the pump's honest
// signature, not a design smell.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip_all, target = "regia_steam", name = "steam_pump")]
pub(crate) fn pump_steam(
    mut steam: ResMut<Steam>,
    mut status: ResMut<crate::SteamStatus>,
    settings: Res<SteamSettings>,
    mut overlay: ResMut<OverlayActive>,
    mut suspended: ResMut<SuspendedContexts>,
    mut contexts: Option<ResMut<regia_input::InputContexts>>,
    mut time: ResMut<Time>,
    mut opened: MessageWriter<crate::overlay::OverlayOpened>,
    mut closed: MessageWriter<crate::overlay::OverlayClosed>,
    mut joins: MessageWriter<JoinRequested>,
    mut unlocked: MessageWriter<AchievementUnlocked>,
    mut stored: MessageWriter<StatsStored>,
    mut conflicts: MessageWriter<CloudConflict>,
    mut trace: Option<ResMut<crate::overlay_trace::SteamOverlayTrace>>,
) {
    let mut events = steam.backend.pump();
    events.extend(steam.events.take());
    if steam.state != SteamState::Ready {
        // Drained above so nothing queues up, but callbacks that arrive after `Steam::shutdown`
        // (or without a client) must not pause the game or fire messages.
        events.clear();
        if steam.overlay_open {
            // The backend went down with the overlay open: replay the close so input contexts,
            // the `Time` pause and `OverlayActive` are restored.
            events.push(SteamEvent::OverlayActivated { active: false });
            if let Some(trace) = trace.as_deref_mut() {
                trace.note("steam went down with the overlay open: closing it");
            }
        }
    }
    for event in events {
        match event {
            SteamEvent::OverlayActivated { active } => {
                if let Some(trace) = trace.as_deref_mut() {
                    let kind = if active == steam.overlay_open {
                        "repeat"
                    } else {
                        "edge"
                    };
                    trace.note(format_args!(
                        "steam callback GameOverlayActivated active={active} ({kind})"
                    ));
                }
                if active == steam.overlay_open {
                    continue; // Steam repeats the callback; only edges matter
                }
                steam.overlay_open = active;
                overlay.0 = active;
                if active {
                    if let Some(names) = &settings.suspend_contexts
                        && let Some(contexts) = contexts.as_deref_mut()
                    {
                        suspended.0 = suspend_contexts(contexts, names);
                    }
                    // Only take over a pause the game does not already hold, so closing the
                    // overlay never unpauses a game-initiated pause (pause menu).
                    if settings.pause_on_overlay && !time.paused {
                        time.paused = true;
                        steam.paused_by_overlay = true;
                    }
                    opened.write(crate::overlay::OverlayOpened);
                    tracing::info!(target: "regia_steam", "Steam overlay opened");
                } else {
                    if let Some(contexts) = contexts.as_deref_mut() {
                        restore_contexts(contexts, &suspended.0);
                    }
                    suspended.0 = Vec::new();
                    if steam.paused_by_overlay {
                        time.paused = false;
                        steam.paused_by_overlay = false;
                    }
                    closed.write(crate::overlay::OverlayClosed);
                    tracing::info!(target: "regia_steam", "Steam overlay closed");
                }
            }
            SteamEvent::JoinRequested { friend, connect } => {
                if connect.len() > MAX_CONNECT_BYTES {
                    tracing::warn!(target: "regia_steam", friend = friend.0, bytes = connect.len(), "join request dropped: its connect string is too long");
                    continue;
                }
                tracing::info!(target: "regia_steam", friend = friend.0, "join requested");
                joins.write(JoinRequested { friend, connect });
            }
            SteamEvent::AchievementStored { id } => {
                // Threshold unlocks are announced locally at once; Steam's own callback for the
                // same unlock (after `store_stats`) must not grant the reward twice.
                let first = steam
                    .announced
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id.clone());
                if !first {
                    continue;
                }
                unlocked.write(AchievementUnlocked { id });
            }
            SteamEvent::StatsStored { ok } => {
                if !ok {
                    tracing::warn!(target: "regia_steam", "Steam rejected a stats upload");
                }
                stored.write(StatsStored {
                    result: if ok {
                        Ok(())
                    } else {
                        Err("steam rejected the upload".into())
                    },
                });
            }
            SteamEvent::CloudConflict { file, remote_newer } => {
                let slot = cloud::slot_from_cloud_file(&file);
                tracing::info!(
                    target: "regia_steam",
                    slot = %slot,
                    remote_newer,
                    "cloud save conflict"
                );
                conflicts.write(CloudConflict {
                    slot,
                    file,
                    remote_newer,
                });
            }
        }
    }
    let fresh = crate::SteamStatus::read(steam.backend.as_ref(), steam.state, steam.overlay_open);
    if *status != fresh {
        *status = fresh;
    }
}

/// `Lifecycle`: flush pending stats and achievements, then shut Steam down, when the app terminates, so the last
/// `stat_add` of a session is not lost (Steam's own shutdown ordering requires the upload before
/// the API shuts down).
pub(crate) fn flush_stats_on_exit(
    lifecycle: Res<regia_platform::AppLifecycle>,
    mut steam: ResMut<Steam>,
    mut status: ResMut<crate::SteamStatus>,
) {
    if lifecycle.last_event == Some(regia_platform::LifecycleEvent::WillTerminate)
        && steam.state == SteamState::Ready
    {
        // Flush, clear presence and shut the API down (idempotent), in Steam's required order.
        steam.shutdown();
        *status = crate::SteamStatus::default();
    }
}
