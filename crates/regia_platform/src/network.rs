// SPDX-License-Identifier: MPL-2.0
//! Whether the active network is metered (`docs/specs/40-content-chunks-dlc-asset-packs.md` § 2.5,
//! ADR-7236). Content transfers ask before they start: on a metered connection a request waits for
//! the player's approval.
//!
//! Android asks `ConnectivityManager.isActiveNetworkMetered()` over JNI (the
//! `ACCESS_NETWORK_STATE` permission is declared by `regia package`). Desktop platforms have no
//! network-cost API wired, so they answer "cannot tell" (`Ok(None)`), which the content manager
//! reads as unmetered. Tests replace the query with a fake through
//! `regia_asset::content::ContentManager::set_metered_probe`.

use crate::services::ServiceError;

/// `Ok(Some(true))` on a metered network, `Ok(Some(false))` on an unmetered one, `Ok(None)` when
/// the platform cannot tell (no active network, or no network-cost API on this platform).
///
/// Cheap but not free on Android (one JNI round trip): call it on a request or every few seconds,
/// not every frame.
///
/// # Errors
/// Android only: the activity is not initialized yet, or a JNI failure (permission missing, a Java
/// exception); callers treat an error like `Ok(None)`.
pub fn active_network_metered() -> Result<Option<bool>, ServiceError> {
    #[cfg(target_os = "android")]
    {
        crate::platform::active_network_metered()
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "android"))]
    #[test]
    fn desktop_cannot_tell() {
        assert_eq!(super::active_network_metered().ok(), Some(None));
    }
}
