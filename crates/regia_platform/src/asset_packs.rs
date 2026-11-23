// SPDX-License-Identifier: MPL-2.0
//! Store asset packs on Android (Play Asset Delivery; `docs/specs/40-content-chunks-dlc-asset-packs.md`
//! § 2.6, ADR-1876).
//!
//! `regia package --platform android` turns every `fast_follow` and `on_demand` chunk into one asset
//! pack and generates small Java helpers on the activity (`regiaPackState`, `regiaPackFetch`,
//! `regiaPackCancel`, `regiaPackRemove`, `regiaPackLocation`, `regiaPackConfirm`) that wrap the
//! store's public asset-pack Java API. The functions here call those helpers over JNI. They run on
//! the calling thread, never block on the store (states arrive through a listener on the Java side
//! and are polled), and report every failure as a [`ServiceError`]; on other platforms they return
//! [`ServiceError::Unsupported`].
//!
//! `regia_asset::content::StorePackSource` maps these states onto chunk states. Pack names are chunk
//! names with `-` replaced by `_` (`regia_asset::content::pack_name`).

use std::path::PathBuf;

use crate::services::ServiceError;

/// Store pack status codes (the `AssetPackStatus` constants of the store's public asset-pack API).
pub mod status {
    /// The store does not know the pack yet.
    pub const UNKNOWN: i32 = 0;
    /// A request is queued.
    pub const PENDING: i32 = 1;
    /// Bytes are downloading.
    pub const DOWNLOADING: i32 = 2;
    /// The download is being moved into app storage.
    pub const TRANSFERRING: i32 = 3;
    /// The pack is on the device.
    pub const COMPLETED: i32 = 4;
    /// The request failed (`error_code` says why).
    pub const FAILED: i32 = 5;
    /// The request was cancelled.
    pub const CANCELED: i32 = 6;
    /// Waiting for an unmetered network (or the player's approval).
    pub const WAITING_FOR_WIFI: i32 = 7;
    /// The pack is not on the device.
    pub const NOT_INSTALLED: i32 = 8;
    /// The store needs the player to confirm (the confirmation dialog).
    pub const REQUIRES_USER_CONFIRMATION: i32 = 9;
}

/// One pack's state as the store reported it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PackState {
    /// A [`status`] code (unlisted `i32` codes are passed through; values outside `i32` become `-1`).
    pub status: i32,
    /// The store's error code (0 = none).
    /// Values outside `i32` become `-1`.
    pub error_code: i32,
    /// Bytes downloaded so far.
    pub bytes_downloaded: u64,
    /// Bytes the download needs in all.
    pub total_bytes: u64,
}

impl PackState {
    /// Builds a state from the activity helper row
    /// (`{status, errorCode, bytesDownloaded, totalBytesToDownload}`). Rows with at least four
    /// values are accepted and later values are ignored; shorter rows return `None`. Status and
    /// error values outside `i32` become `-1`, and negative byte counts become zero. A downloaded
    /// count greater than the total is preserved as reported.
    #[must_use]
    pub fn from_row(row: &[i64]) -> Option<Self> {
        let [status, error, done, total] = *row.first_chunk::<4>()?;
        Some(Self {
            status: i32::try_from(status).unwrap_or(-1),
            error_code: i32::try_from(error).unwrap_or(-1),
            bytes_downloaded: u64::try_from(done).unwrap_or(0),
            total_bytes: u64::try_from(total).unwrap_or(0),
        })
    }
}

/// The store's last known state of `pack`; `Ok(None)` until the store has answered (the first call
/// asks it, and the answer arrives on a later call).
///
/// # Errors
/// Not Android, the build has no asset packs (no helper on the activity), or a JNI failure.
pub fn pack_state(pack: &str) -> Result<Option<PackState>, ServiceError> {
    #[cfg(target_os = "android")]
    {
        crate::platform::asset_pack_state(pack)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = pack;
        Err(unsupported())
    }
}

/// Asks the store to download `pack` (asynchronous; poll [`pack_state`]).
///
/// # Errors
/// As [`pack_state`].
pub fn fetch_pack(pack: &str) -> Result<(), ServiceError> {
    call(pack, PackCall::Fetch)
}

/// Cancels a running download of `pack`.
///
/// # Errors
/// As [`pack_state`].
pub fn cancel_pack(pack: &str) -> Result<(), ServiceError> {
    call(pack, PackCall::Cancel)
}

/// Removes `pack` from the device (the store frees its storage).
///
/// # Errors
/// As [`pack_state`].
pub fn remove_pack(pack: &str) -> Result<(), ServiceError> {
    call(pack, PackCall::Remove)
}

/// Shows the store's confirmation dialog (cellular download or other player approval). Returns at
/// once; the dialog runs on the UI thread.
///
/// # Errors
/// As [`pack_state`].
pub fn show_confirmation() -> Result<(), ServiceError> {
    call("", PackCall::Confirm)
}

/// The directory holding `pack`'s files once it is on the device; `Ok(None)` while it is not, or when
/// the store keeps it inside the APK (install-time packs, which mount through the APK asset reader).
///
/// # Errors
/// As [`pack_state`].
pub fn pack_location(pack: &str) -> Result<Option<PathBuf>, ServiceError> {
    #[cfg(target_os = "android")]
    {
        crate::platform::asset_pack_location(pack)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = pack;
        Err(unsupported())
    }
}

/// The helper calls that return nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PackCall {
    /// `regiaPackFetch(String)`.
    Fetch,
    /// `regiaPackCancel(String)`.
    Cancel,
    /// `regiaPackRemove(String)`.
    Remove,
    /// `regiaPackConfirm()`.
    Confirm,
}

fn call(pack: &str, what: PackCall) -> Result<(), ServiceError> {
    #[cfg(target_os = "android")]
    {
        crate::platform::asset_pack_call(pack, what)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (pack, what);
        Err(unsupported())
    }
}

#[cfg(not(target_os = "android"))]
fn unsupported() -> ServiceError {
    ServiceError::Unsupported("store asset packs are Android-only")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_lengths_and_trailing_values_follow_the_four_field_contract() {
        for length in 0..=3 {
            assert_eq!(PackState::from_row(&[4, 0, 1, 2, 99][..length]), None);
        }

        let expected = PackState {
            status: status::COMPLETED,
            error_code: 0,
            bytes_downloaded: 1,
            total_bytes: 2,
        };
        assert_eq!(PackState::from_row(&[4, 0, 1, 2]), Some(expected));
        assert_eq!(PackState::from_row(&[4, 0, 1, 2, 99]), Some(expected));
        assert_eq!(
            PackState::from_row(&[4, 0, 9, 2]).map(|row| (row.bytes_downloaded, row.total_bytes)),
            Some((9, 2))
        );
    }

    #[test]
    fn documented_statuses_and_unknown_statuses_are_passed_through() {
        for (raw, expected) in [
            (status::UNKNOWN, status::UNKNOWN),
            (status::PENDING, status::PENDING),
            (status::DOWNLOADING, status::DOWNLOADING),
            (status::TRANSFERRING, status::TRANSFERRING),
            (status::COMPLETED, status::COMPLETED),
            (status::FAILED, status::FAILED),
            (status::CANCELED, status::CANCELED),
            (status::WAITING_FOR_WIFI, status::WAITING_FOR_WIFI),
            (status::NOT_INSTALLED, status::NOT_INSTALLED),
            (
                status::REQUIRES_USER_CONFIRMATION,
                status::REQUIRES_USER_CONFIRMATION,
            ),
            (-73, -73),
            (i32::MIN, i32::MIN),
            (i32::MAX, i32::MAX),
        ] {
            assert_eq!(
                PackState::from_row(&[raw as i64, -11, 4, 8]).map(|row| row.status),
                Some(expected)
            );
        }
    }

    #[test]
    fn signed_width_boundaries_use_sentinels_and_saturate_negative_counts_to_zero() {
        let status_edges = [
            (i64::MIN, -1),
            (i64::from(i32::MIN), i32::MIN),
            (i64::from(i32::MAX), i32::MAX),
            (i64::MAX, -1),
        ];
        for (input, expected) in status_edges {
            assert_eq!(
                PackState::from_row(&[input, 0, 1, 2]).map(|row| row.status),
                Some(expected)
            );
            assert_eq!(
                PackState::from_row(&[0, input, 1, 2]).map(|row| row.error_code),
                Some(expected)
            );
        }

        for input in [i64::MIN, -1, 0, 1, i64::MAX] {
            let expected = u64::try_from(input).unwrap_or(0);
            assert_eq!(
                PackState::from_row(&[0, 0, input, 7]).map(|row| row.bytes_downloaded),
                Some(expected)
            );
            assert_eq!(
                PackState::from_row(&[0, 0, 7, input]).map(|row| row.total_bytes),
                Some(expected)
            );
        }
    }

    #[test]
    fn deterministic_generated_rows_never_wrap_negative_counts() {
        let values = [i64::MIN, -17, -1, 0, 1, 17, i64::MAX];
        for status_value in values {
            for error_value in values {
                for downloaded in values {
                    for total in values {
                        let parsed = PackState::from_row(&[status_value, error_value, downloaded, total]);
                        let Some(parsed) = parsed else {
                            panic!("four-element row must parse");
                        };
                        assert_eq!(parsed.status, i32::try_from(status_value).unwrap_or(-1));
                        assert_eq!(parsed.error_code, i32::try_from(error_value).unwrap_or(-1));
                        assert_eq!(parsed.bytes_downloaded, u64::try_from(downloaded).unwrap_or(0));
                        assert_eq!(parsed.total_bytes, u64::try_from(total).unwrap_or(0));
                    }
                }
            }
        }
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn desktop_is_unsupported() {
        assert!(matches!(pack_state("arena"), Err(ServiceError::Unsupported(_))));
        assert!(matches!(fetch_pack("arena"), Err(ServiceError::Unsupported(_))));
        assert!(matches!(cancel_pack("arena"), Err(ServiceError::Unsupported(_))));
        assert!(matches!(remove_pack("arena"), Err(ServiceError::Unsupported(_))));
        assert!(matches!(
            pack_location("arena"),
            Err(ServiceError::Unsupported(_))
        ));
        assert!(matches!(show_confirmation(), Err(ServiceError::Unsupported(_))));
    }
}
