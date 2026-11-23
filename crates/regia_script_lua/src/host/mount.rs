// SPDX-License-Identifier: MPL-2.0
//! Mount claim isolation and frozen v1 state preservation (ADR-7881, amended by ADR-7951
//! and ADR-7992).
//!
//! Mounts identify loaded sources, but luastate v1 entries retain their manifest ids. Since v1
//! carries no proof of mount ownership, a Workshop mount (`mods:workshop.<item id>`) never
//! receives a manifest entry. Local `mods:` folders, `.fpak` archives, and other mounts restore
//! by manifest id. Contested entries are also held back. A save preserves held entries instead
//! of the claimant's live state. Held bytes are reserved first; a live mod that does not fit is
//! left out. The save fails only when the held entries alone pass the cap. An entry id that
//! contains `:` is skipped with a warning.

use super::{LuaMod, LuaMods};
use crate::error::{LuaErrorCode, ModError};
use crate::files::ModFiles;
use crate::state::{self, LuaStateFile};

impl LuaMod {
    /// Where the mod is mounted (its files' [`ModFiles::mount`]); `None` for files read with no
    /// mount.
    #[must_use]
    pub fn mount(&self) -> Option<&str> {
        self.files.mount()
    }

    /// The frozen luastate v1 entry key: the manifest id (ADR-7951).
    ///
    /// Mount identity is available through [`Self::mount`]; v1 has no mount ownership field.
    #[must_use]
    pub fn save_key(&self) -> &str {
        &self.manifest.id
    }
}

/// The source identity of files claiming manifest id `id`.
fn key_of<'a>(files: &'a ModFiles, id: &'a str) -> &'a str {
    files.mount().unwrap_or(id)
}

/// `regia_steam::workshop` mounts a subscribed item as `workshop.<item id>`, which
/// `ModFiles::from_mods_vfs` records as `mods:workshop.<item id>`.
/// Local `mods:` folders and `.fpak` archives use other ids. No separate trust flag:
/// the mount id is the only signal the loader records.
fn is_workshop_mount(mount: &str) -> bool {
    mount.starts_with("mods:workshop.")
}

impl LuaMods {
    /// Manifest ids more than one mount claimed this session.
    pub fn contested_ids(&self) -> impl Iterator<Item = &str> {
        self.contested.iter().map(String::as_str)
    }

    /// Saved entries held back from Workshop or contested claims, by manifest id.
    pub fn held_state(&self) -> impl Iterator<Item = &str> {
        self.held.keys().map(String::as_str)
    }

    /// Load hook: refuses `files` when another loaded mod holds the manifest id they claim, or
    /// the same mount, unless it is the very same mod again (same id, same mount), which the
    /// load's own duplicate check reports as already loaded. On success, a waiting v1 entry is
    /// held back when this claimant is not allowed to restore it.
    pub(super) fn claim_mount(&mut self, id: &str, files: &ModFiles) -> Result<(), ModError> {
        let key = key_of(files, id);
        let holder = self
            .mods
            .iter()
            .find(|m| {
                let mounted = key_of(&m.files, m.id());
                (m.id() == id || mounted == key) && !(m.id() == id && mounted == key)
            })
            .map(|m| {
                (
                    m.id().to_owned(),
                    key_of(&m.files, m.id()).to_owned(),
                    m.mount().is_some(),
                )
            });
        if let Some((holder_id, holder_key, holder_mounted)) = holder {
            let error = ModError::new(
                LuaErrorCode::IdTaken,
                format!(
                    "mod id `{id}` from {} is taken: mod `{holder_id}` from {} is loaded",
                    files.mount().unwrap_or("files with no mount"),
                    if holder_mounted {
                        holder_key.as_str()
                    } else {
                        "files with no mount"
                    },
                ),
            );
            tracing::warn!(target: "regia_script_lua", id, mount = files.mount(), holder = %holder_key, "mod refused: {}", error.message);
            // Only a second claim of the id makes it contested (not a second mod on one mount).
            if holder_id == id {
                self.contested.insert(id.to_owned());
                // A legacy entry still waiting for the contested id is nobody's now.
                if let Some(saved) = self.kept.remove(id) {
                    self.held.insert(id.to_owned(), saved);
                }
            }
            return Err(error);
        }
        if self.hold_manifest_entry(id, files.mount())
            && let Some(saved) = self.kept.remove(id)
        {
            self.held.insert(id.to_owned(), saved);
        }
        Ok(())
    }

    fn hold_manifest_entry(&self, id: &str, mount: Option<&str>) -> bool {
        self.contested.contains(id) || mount.is_some_and(is_workshop_mount)
    }

    /// Bytes of every held entry. The save reserves these before live mods are measured.
    pub(super) fn held_encoded_total(&self) -> Result<u64, ModError> {
        let mut total = 0u64;
        for saved in self.held.values() {
            total = total.saturating_add(state::encoded_size(saved)?);
        }
        Ok(total)
    }

    /// Held entries win over current captures of the same manifest id. Never put a mount key
    /// into frozen v1. A restored id that contains `:` is quarantined instead of failing the
    /// save. The save still fails when the entries that remain cannot fit the cap, which after
    /// live mods have been dropped is the held entries alone.
    pub(super) fn bind_saved_keys(&mut self, file: &mut LuaStateFile) -> Result<Vec<String>, ModError> {
        let mut skipped = std::mem::take(&mut self.colon_skipped);
        file.mods.retain(|saved| {
            if saved.id.contains(':') {
                skipped.push(saved.id.clone());
                false
            } else {
                !self.held.contains_key(&saved.id)
            }
        });
        let kept_colon: Vec<String> = self.kept.keys().filter(|id| id.contains(':')).cloned().collect();
        for id in kept_colon {
            self.kept.remove(&id);
            skipped.push(id);
        }
        skipped.sort();
        skipped.dedup();
        for id in &skipped {
            tracing::warn!(
                target: "regia_script_lua",
                %id,
                "skipping a restored luastate entry whose id contains ':'; it is not a v1 manifest id"
            );
        }
        file.mods.extend(self.held.values().cloned());
        let mut total = 0u64;
        for saved in &file.mods {
            debug_assert!(
                !saved.id.contains(':'),
                "host-produced luastate ids are manifest ids"
            );
            let bytes = match state::encoded_size(saved) {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.colon_skipped = skipped;
                    return Err(error);
                }
            };
            total = total.saturating_add(bytes);
            if total > self.limits.max_state_total_bytes {
                self.colon_skipped = skipped;
                return Err(ModError::new(
                    LuaErrorCode::TooLarge,
                    format!(
                        "the mods' states including held entries pass the {} bytes a save holds",
                        self.limits.max_state_total_bytes
                    ),
                ));
            }
        }
        Ok(skipped)
    }

    /// Withhold entries from Workshop and contested claimants already loaded at restore time.
    /// Entries for mods loaded later stay in `kept`, where `claim_mount` applies the same rule.
    /// An id containing `:` is quarantined: no v1 mod can own one, and it must not fail saves.
    pub(super) fn bind_restored_keys(&mut self, mut file: LuaStateFile) -> LuaStateFile {
        self.held.clear();
        self.colon_skipped.clear();
        let mut available = Vec::with_capacity(file.mods.len());
        for saved in file.mods {
            if saved.id.contains(':') {
                tracing::warn!(
                    target: "regia_script_lua",
                    id = %saved.id,
                    "quarantining a restored luastate entry whose id contains ':'; it is not a v1 manifest id"
                );
                self.colon_skipped.push(saved.id);
                continue;
            }
            let module = self.mods.iter().find(|module| module.id() == saved.id);
            if self.contested.contains(&saved.id)
                || module.is_some_and(|module| self.hold_manifest_entry(&saved.id, module.mount()))
            {
                self.held.insert(saved.id.clone(), saved);
            } else {
                available.push(saved);
            }
        }
        file.mods = available;
        file
    }

    /// Reload hook: the new files keep the mod's mount (a reload replaces code, not identity).
    pub(super) fn keep_mount(&self, index: usize, files: Option<ModFiles>) -> Option<ModFiles> {
        files.map(|mut files| {
            files.set_mount(self.mods[index].files.mount().map(str::to_owned));
            files
        })
    }
}
