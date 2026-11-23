// SPDX-License-Identifier: MPL-2.0
//! The framework's save store over the Steam backends (`docs/specs/91-character-save-templates.md`
//! § 2.2): [`SteamRemoteStorage`] and [`LocalSaveBackend`] implement
//! `regia_framework::save_flow::SaveStorage`, so a game moves every save (its own requests,
//! quicksave, autosave and the agent's `save.*` methods) to Steam Remote Storage with one line:
//!
//! ```ignore
//! app.insert_resource(SaveStore::new(SteamRemoteStorage::new(&steam)));
//! ```
//!
//! Errors map onto the framework's `SaveError::Io`: `Unavailable` becomes kind `NotConnected`,
//! `InvalidSlot` kind `InvalidInput`, and IO errors keep their kind; the path names the slot's
//! file (`saves/<slot>.frts` in the cloud, `<root>/<slot>.sav` on disk).

use std::path::PathBuf;

use regia_framework::save_flow::SaveStorage;
use regia_framework::save_game::SaveError as FrameworkError;

use crate::cloud::{LocalSaveBackend, SaveBackend, SaveError, SteamRemoteStorage};

/// Maps a Steam-side [`SaveError`] onto the framework's error, naming `path`.
fn framework_error(error: SaveError, path: PathBuf) -> FrameworkError {
    let io = match error {
        SaveError::Io(io) => io,
        SaveError::InvalidSlot(slot) => std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid save slot name `{slot}`"),
        ),
        SaveError::Unavailable => std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "Steam Remote Storage is unavailable",
        ),
    };
    FrameworkError::Io(io, path)
}

/// The cloud file a slot maps to, for error messages.
fn cloud_path(slot: &str) -> PathBuf {
    PathBuf::from(format!("saves/{slot}.frts"))
}

impl SaveStorage for SteamRemoteStorage {
    fn kind(&self) -> &'static str {
        "steam_remote"
    }

    fn write(&self, slot: &str, bytes: &[u8]) -> Result<(), FrameworkError> {
        SaveBackend::save(self, slot, bytes).map_err(|e| framework_error(e, cloud_path(slot)))
    }

    fn read(&self, slot: &str) -> Result<Option<Vec<u8>>, FrameworkError> {
        SaveBackend::load(self, slot).map_err(|e| framework_error(e, cloud_path(slot)))
    }

    fn delete(&self, slot: &str) -> Result<bool, FrameworkError> {
        let existed = SaveBackend::exists(self, slot).map_err(|e| framework_error(e, cloud_path(slot)))?;
        if existed {
            SaveBackend::delete(self, slot).map_err(|e| framework_error(e, cloud_path(slot)))?;
        }
        Ok(existed)
    }

    fn list(&self) -> Result<Vec<String>, FrameworkError> {
        let mut slots = SaveBackend::slots(self).map_err(|e| framework_error(e, PathBuf::from("saves")))?;
        slots.sort();
        Ok(slots)
    }
}

impl SaveStorage for LocalSaveBackend {
    fn kind(&self) -> &'static str {
        "user_dir"
    }

    fn write(&self, slot: &str, bytes: &[u8]) -> Result<(), FrameworkError> {
        SaveBackend::save(self, slot, bytes).map_err(|e| framework_error(e, self.slot_file(slot)))
    }

    fn read(&self, slot: &str) -> Result<Option<Vec<u8>>, FrameworkError> {
        SaveBackend::load(self, slot).map_err(|e| framework_error(e, self.slot_file(slot)))
    }

    fn delete(&self, slot: &str) -> Result<bool, FrameworkError> {
        let existed =
            SaveBackend::exists(self, slot).map_err(|e| framework_error(e, self.slot_file(slot)))?;
        if existed {
            SaveBackend::delete(self, slot).map_err(|e| framework_error(e, self.slot_file(slot)))?;
        }
        Ok(existed)
    }

    fn list(&self) -> Result<Vec<String>, FrameworkError> {
        SaveBackend::slots(self).map_err(|e| framework_error(e, self.root().to_path_buf()))
    }
}

impl LocalSaveBackend {
    /// The file a slot maps to, for error messages.
    fn slot_file(&self, slot: &str) -> PathBuf {
        self.root().join(format!("{slot}.sav"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_keep_their_meaning() {
        let unavailable = framework_error(SaveError::Unavailable, cloud_path("A"));
        assert!(
            matches!(&unavailable, FrameworkError::Io(io, path)
                if io.kind() == std::io::ErrorKind::NotConnected && path == &PathBuf::from("saves/A.frts")),
            "{unavailable:?}"
        );
        let invalid = framework_error(SaveError::InvalidSlot("nul".into()), cloud_path("nul"));
        assert!(
            matches!(&invalid, FrameworkError::Io(io, _) if io.kind() == std::io::ErrorKind::InvalidInput)
        );
    }

    #[test]
    fn local_backend_is_a_framework_store() {
        let root = regia_test_utils::unique_temp_dir("steam-local-store");
        let local = LocalSaveBackend::new(&root);
        assert_eq!(SaveStorage::kind(&local), "user_dir");
        SaveStorage::write(&local, "B", b"bb").expect("write");
        SaveStorage::write(&local, "A", b"aa").expect("write");
        assert_eq!(
            SaveStorage::list(&local).ok(),
            Some(vec!["A".to_owned(), "B".to_owned()])
        );
        assert_eq!(
            SaveStorage::read(&local, "A").ok().flatten(),
            Some(b"aa".to_vec())
        );
        // ADR-3102: the Steam stores keep no previous generation (the trait defaults).
        SaveStorage::write(&local, "A", b"a2").expect("rewrite");
        assert!(!SaveStorage::has_previous(&local, "A"));
        assert_eq!(SaveStorage::read_previous(&local, "A").ok(), Some(None));
        assert_eq!(SaveStorage::delete(&local, "A").ok(), Some(true));
        assert_eq!(SaveStorage::delete(&local, "A").ok(), Some(false));
        let windows_device = SaveStorage::write(&local, "nul", b"x").expect_err("a Windows device name");
        assert!(
            matches!(windows_device, FrameworkError::Io(io, _) if io.kind() == std::io::ErrorKind::InvalidInput)
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
