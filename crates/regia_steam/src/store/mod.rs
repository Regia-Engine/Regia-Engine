// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/platform.rs, commit d6ed37677), ADR-8012.
// Adapted under the owner's 2026-11-16 exception: borrow the existing Regia client (ADR-8159).
//! Store-neutral platform services (spec 24 § 5).
//!
//! [`select`] borrows the existing [`Steam`] resource; it never initializes a client.
//! Hosts install [`crate::SteamPlugin`] only when requesting Steam, finish plugins, and keep
//! updating the app for callbacks. None and unavailable stores are inert. GOG has no Galaxy SDK.
//! [`StorePlugin`] exposes the requested and active store through agent method `store.status`.

mod agent;
mod identity;

use regia_app::prelude::*;

use crate::{Steam, SteamState};

pub use identity::{
    AccountKind, PREFIX_BOT, PREFIX_GOG, PREFIX_LOCAL, PREFIX_STEAM, account_id_from_galaxy, account_kind,
    galaxy_from_account_id, is_local_account_id, local_account_id, relay_allowed, store_of,
};

/// Store requested by the host, or supplied by an active platform.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Store {
    /// Standalone: no launcher or account required.
    #[default]
    None,
    /// The existing Regia Steam client.
    Steam,
    /// GOG Galaxy; currently unavailable, so selection falls back to None.
    Gog,
}

/// Parse a package or host setting. Unknown text selects None; no environment is read.
pub fn parse_store(text: &str) -> Store {
    match text.trim().to_ascii_lowercase().as_str() {
        "steam" => Store::Steam,
        "gog" => Store::Gog,
        _ => Store::None,
    }
}

/// Services a store offers the game. Defaults are inert, including the cloud notification.
///
/// Store refusal is nonfatal: achievements return false; other requests do nothing.
/// Use the underlying [`Steam`] API when the caller needs detailed errors.
pub trait Platform: Send {
    /// Active store; an unavailable Steam adapter reports None.
    fn store(&self) -> Store;
    /// Short log-safe description, without a player name or other personal details.
    fn describe(&self) -> String;
    /// Store-prefixed account id, or None when unavailable. Never a display name.
    fn account_id(&self) -> Option<String> {
        None
    }
    /// Set or remove one presence key (`None` removes it).
    fn set_presence(&self, _key: &str, _value: Option<&str>) {}
    /// Open a known overlay page; unknown pages are ignored.
    fn open_overlay(&self, _page: &str) {}
    /// Unlock an achievement; true only when the store accepted the request.
    fn unlock_achievement(&self, _id: &str) -> bool {
        false
    }
    /// Notify that a save was written, relative to its save root.
    ///
    /// All supplied adapters leave this inert. Actual synchronization belongs to the existing
    /// save backend or configured Auto-Cloud. This hook never reads or uploads the file.
    fn cloud_save_written(&self, _relative_path: &str) {}
    /// Shut down the selected store. Repeated calls are harmless.
    fn shutdown(&mut self) {}
}

/// Standalone platform: no client, account, filesystem access, or store calls.
#[derive(Debug, Default, Clone, Copy)]
pub struct NonePlatform;

impl Platform for NonePlatform {
    fn store(&self) -> Store {
        Store::None
    }
    fn describe(&self) -> String {
        "none (DRM-free)".to_owned()
    }
}

/// A borrowed adapter over the resource initialized and pumped by [`crate::SteamPlugin`].
///
/// No client is initialized or cloned. Dropping the adapter leaves the resource alive;
/// explicitly calling [`Platform::shutdown`] shuts it down through the original resource.
pub struct SteamPlatform<'a> {
    inner: &'a mut Steam,
}

impl<'a> SteamPlatform<'a> {
    /// Borrow a Steam resource. Unavailable resources behave as None.
    pub fn new(inner: &'a mut Steam) -> Self {
        Self { inner }
    }

    fn ready(&self) -> bool {
        steam_ready(self.inner)
    }
}

fn steam_ready(steam: &Steam) -> bool {
    steam.is_ready() && steam.backend.state() == SteamState::Ready
}

impl Platform for SteamPlatform<'_> {
    fn store(&self) -> Store {
        if self.ready() { Store::Steam } else { Store::None }
    }
    fn describe(&self) -> String {
        if self.ready() {
            "steam (ready)"
        } else {
            "steam (unavailable)"
        }
        .to_owned()
    }
    fn account_id(&self) -> Option<String> {
        if !self.ready() {
            return None;
        }
        self.inner
            .steam_id()
            .filter(|id| !id.is_invalid())
            .map(|id| format!("{PREFIX_STEAM}{}", id.0))
    }
    fn set_presence(&self, key: &str, value: Option<&str>) {
        if self.ready() {
            let _ = self.inner.set_presence(key, value);
        }
    }
    fn open_overlay(&self, page: &str) {
        if self.ready() {
            let _ = self.inner.open_overlay(page);
        }
    }
    fn unlock_achievement(&self, id: &str) -> bool {
        self.ready() && self.inner.unlock(id).is_ok()
    }
    fn shutdown(&mut self) {
        if self.inner.is_ready() {
            self.inner.shutdown();
        }
    }
}

/// Explicit unavailable GOG adapter. No Galaxy SDK is linked and no client is started.
/// [`select`] falls back to [`NonePlatform`] for GOG requests.
#[derive(Debug, Default, Clone, Copy)]
pub struct GogPlatform;

impl Platform for GogPlatform {
    fn store(&self) -> Store {
        Store::Gog
    }
    fn describe(&self) -> String {
        "gog (unavailable: Galaxy SDK not linked)".to_owned()
    }
}

/// Select a platform using an already initialized Steam resource, when supplied.
///
/// None and GOG never touch the resource. Missing/unavailable Steam falls back to None.
/// There is no filesystem or environment lookup and no implicit client initialization.
pub fn select(requested: Store, steam: Option<&mut Steam>) -> Box<dyn Platform + '_> {
    match (requested, steam) {
        (Store::Steam, Some(steam)) if steam_ready(steam) => Box::new(SteamPlatform::new(steam)),
        _ => Box::new(NonePlatform),
    }
}

/// The host's selection, exposed through `store.status`. It does not own a native client.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreSelection {
    /// Requested store; the live adapter can fall back to None.
    pub requested: Store,
}

/// Publish the host's selection and register read-only agent method `store.status`.
///
/// This plugin never adds [`crate::SteamPlugin`]. Hosts add it explicitly for Steam, with their
/// existing app-id/settings configuration; omit it for standalone and GOG builds.
#[derive(Debug, Clone, Copy, Default)]
pub struct StorePlugin {
    requested: Store,
}

impl StorePlugin {
    /// Publish this requested store. The default is [`Store::None`].
    pub fn new(requested: Store) -> Self {
        Self { requested }
    }
}

impl Plugin for StorePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(StoreSelection {
            requested: self.requested,
        });
        agent::install(app.world_mut());
    }
}
