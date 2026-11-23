// SPDX-License-Identifier: MPL-2.0
//! The editor's persisted files as frozen formats (ADR-7902, lead decision on review 3202 #22):
//! the live name, version, serde fingerprint and migration sources of each, read from the real
//! types. `regia_core::editor_formats::EDITOR_FORMATS` is the copy `cargo xtask check-formats` and
//! `regia upgrade-project` read (neither may depend on this desktop-only crate); the tests here keep
//! the two equal and make every new versioned editor file join the table.
//!
//! After a deliberate shape change: bump the file's version, register its migrator, then
//! `REGIA_UPDATE_EDITOR_FORMATS=1 cargo test -p regia_editor --lib user_formats` (rewrites the
//! generated block of `crates/regia_core/src/editor_formats.rs`) and
//! `cargo xtask check-formats --update`.

/// One editor file format as the editor's types define it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveEditorFormat {
    /// The registry name.
    pub name: &'static str,
    /// The file names (`/name`) and suffixes (`.suffix`) the format owns.
    pub files: &'static [&'static str],
    /// The name of the version constant in this crate (for the coverage test).
    pub version_const: &'static str,
    /// The current version.
    pub version: u32,
    /// The `from` versions of the registered migration steps.
    pub migration_sources: Vec<u32>,
    /// The serde shape.
    pub fingerprint: String,
}

/// Every editor file format, sorted by name.
#[must_use]
pub fn user_format_fingerprints() -> Vec<LiveEditorFormat> {
    let mut all = vec![
        LiveEditorFormat {
            name: "editor-keymap",
            files: &["/editor_keymap.ron"],
            version_const: "EDITOR_KEYMAP_FORMAT_VERSION",
            version: crate::editor_keymap::EDITOR_KEYMAP_FORMAT_VERSION,
            migration_sources: crate::editor_keymap::keymap_migration_sources(),
            fingerprint: crate::editor_keymap::keymap_schema_fingerprint(),
        },
        LiveEditorFormat {
            name: "editor-layout",
            files: &["/editor_layout.ron", ".editor_layout.ron"],
            version_const: "EDITOR_LAYOUT_FORMAT_VERSION",
            version: crate::editor_layout::EDITOR_LAYOUT_FORMAT_VERSION,
            migration_sources: crate::editor_layout::layout_migration_sources(),
            fingerprint: crate::editor_layout::layout_schema_fingerprint(),
        },
        LiveEditorFormat {
            name: "editor-prefs",
            files: &["/editor_prefs.ron"],
            version_const: "EDITOR_PREFS_FORMAT_VERSION",
            version: crate::editor_prefs::EDITOR_PREFS_FORMAT_VERSION,
            migration_sources: crate::editor_prefs::prefs_migration_sources(),
            fingerprint: crate::editor_prefs::prefs_schema_fingerprint(),
        },
        LiveEditorFormat {
            name: "editor-profile",
            files: &["/editor_profile.ron", ".editor_profile.ron"],
            version_const: "PROFILE_FORMAT_VERSION",
            version: crate::feature_profiles::PROFILE_FORMAT_VERSION,
            migration_sources: crate::feature_profiles::profile_migration_sources(),
            fingerprint: crate::feature_profiles::profile_schema_fingerprint(),
        },
        LiveEditorFormat {
            name: "editor-theme",
            files: &[".editor_theme.ron"],
            version_const: "EDITOR_THEME_FORMAT_VERSION",
            version: crate::theme::EDITOR_THEME_FORMAT_VERSION,
            migration_sources: crate::theme::theme_migration_sources(),
            fingerprint: crate::theme::theme_schema_fingerprint(),
        },
        LiveEditorFormat {
            name: "editor-view-state",
            files: &["/editor_view.ron"],
            version_const: "VIEW_STATE_FORMAT_VERSION",
            version: crate::view_state::VIEW_STATE_FORMAT_VERSION,
            migration_sources: crate::view_state::view_state_migration_sources(),
            fingerprint: crate::view_state::view_state_schema_fingerprint(),
        },
        LiveEditorFormat {
            name: "editor-window-state",
            files: &["/editor_window_state.ron"],
            version_const: "WINDOW_STATE_FORMAT_VERSION",
            version: crate::window_state::WINDOW_STATE_FORMAT_VERSION,
            migration_sources: crate::window_state::window_state_migration_sources(),
            fingerprint: crate::window_state::window_state_schema_fingerprint(),
        },
        LiveEditorFormat {
            name: "inspector-pins",
            files: &["/inspector_pins.ron"],
            version_const: "INSPECTOR_PINS_FORMAT_VERSION",
            version: crate::inspector::pinned::INSPECTOR_PINS_FORMAT_VERSION,
            migration_sources: crate::inspector::pinned::pins_migration_sources(),
            fingerprint: crate::inspector::pinned::pins_schema_fingerprint(),
        },
        LiveEditorFormat {
            name: "saved-searches",
            files: &["/saved_searches.ron"],
            version_const: "SAVED_SEARCHES_FORMAT_VERSION",
            version: crate::saved_searches::SAVED_SEARCHES_FORMAT_VERSION,
            migration_sources: crate::saved_searches::saved_searches_migration_sources(),
            fingerprint: crate::saved_searches::saved_searches_schema_fingerprint(),
        },
        LiveEditorFormat {
            name: "editor-panel",
            files: &[".editorpanel.ron"],
            version_const: "EDITOR_PANEL_FORMAT_VERSION",
            version: crate::utility_panel::format::EDITOR_PANEL_FORMAT_VERSION,
            migration_sources: crate::utility_panel::format::editor_panel_migration_sources(),
            fingerprint: crate::utility_panel::format::editor_panel_schema_fingerprint(),
        },
        LiveEditorFormat {
            name: "viewport-snap",
            files: &["/viewport_snap.ron"],
            version_const: "SNAP_STEPS_FORMAT_VERSION",
            version: crate::viewport_ops::SNAP_STEPS_FORMAT_VERSION,
            migration_sources: crate::viewport_ops::snap_steps_migration_sources(),
            fingerprint: crate::viewport_ops::snap_steps_schema_fingerprint(),
        },
    ];
    all.sort_by_key(|f| f.name);
    all
}

/// Versioned editor files that are not in the table, each with the reason (a new versioned editor
/// file is either a row of [`user_format_fingerprints`] or listed here).
pub const NOT_EDITOR_FORMATS: &[(&str, &str)] = &[
    (
        "LAYOUT_FORMAT_VERSION",
        "the material graph's layout sidecar: editor-only, spec 80 § 2.8 keeps it out of the frozen formats (old versions load with empty lists)",
    ),
    (
        "CLIP_FORMAT_VERSION",
        "the material graph clipboard text: never persisted, lives on the system clipboard",
    ),
    (
        "THUMB_CACHE_VERSION",
        "a cache key: a new recipe simply stops matching old thumbnails, which are regenerated",
    ),
    (
        "LAYOUT_SIDECAR_VERSION",
        "the conversation graph's layout sidecar: editor-only node positions, regenerated by auto layout when unreadable",
    ),
    (
        "INDEX_VERSION",
        "the document autosave index: session recovery state, an unknown version is ignored",
    ),
    (
        "DATA_ASSET_FORMAT_VERSION",
        "project content: the `data-asset` format row (`regia_asset::data_asset`) guards it",
    ),
    (
        "BATCH_IMPORTER_VERSION",
        "the retarget batch's importer version in the clip cook key, not a file: a change only recooks",
    ),
    (
        "META_VERSION",
        "recent_projects_meta.ron, the project browser's tags and last-opened sidecar (3265, ADR-7856): user config next to the recent list, not a content format (review g17); unreadable files are refused, never overwritten",
    ),
    (
        "BREAKPOINTS_VERSION",
        ".regia/logic_breakpoints.ron: per-user logic debugger state under the ignored .regia/ folder, never shipped; an unreadable or unknown version is reported and never overwritten (ADR-8021, review 3616 L1/L3)",
    ),
];

/// The Rust source of the generated block of `regia_core::editor_formats` for `formats`.
#[must_use]
pub fn descriptor_block(formats: &[LiveEditorFormat]) -> String {
    let mut out = String::from(
        "// BEGIN GENERATED (regia_editor user_formats; REGIA_UPDATE_EDITOR_FORMATS=1)\n\
         /// Every editor file format.\n\
         pub const EDITOR_FORMATS: &[EditorFormat] = &[\n",
    );
    for f in formats {
        let files = f
            .files
            .iter()
            .map(|p| format!("{p:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sources = f
            .migration_sources
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "    EditorFormat {{\n        name: {:?},\n        files: &[{files}],\n        version: {},\n        \
             migration_sources: &[{sources}],\n        fingerprint: {:?},\n    }},\n",
            f.name, f.version, f.fingerprint
        ));
    }
    out.push_str("];\n// END GENERATED");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core_file() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../regia_core/src/editor_formats.rs")
    }

    /// The core copy is what `check-formats` hashes: it must be the editor's real shapes, so a field
    /// added to `editor_prefs.ron` (or any table row) fails here until the copy is regenerated, and
    /// then `check-formats` demands the version bump.
    #[test]
    fn the_core_descriptors_match_the_editor_types() {
        let live = user_format_fingerprints();
        if std::env::var_os("REGIA_UPDATE_EDITOR_FORMATS").is_some() {
            let path = core_file();
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let (Some(a), Some(b)) = (text.find("// BEGIN GENERATED"), text.find("// END GENERATED")) else {
                panic!("{}: no generated block", path.display());
            };
            let end = b + "// END GENERATED".len();
            let next = format!("{}{}{}", &text[..a], descriptor_block(&live), &text[end..]);
            let tmp = path.with_extension(format!("rs.tmp{}", std::process::id()));
            std::fs::write(&tmp, next).unwrap_or_else(|e| panic!("{}: {e}", tmp.display()));
            std::fs::rename(&tmp, &path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            return;
        }
        let core = regia_core::editor_formats::EDITOR_FORMATS;
        let mut problems = Vec::new();
        for f in &live {
            match core.iter().find(|c| c.name == f.name) {
                None => problems.push(format!("{}: not in regia_core::editor_formats", f.name)),
                Some(c) => {
                    if c.version != f.version {
                        problems.push(format!(
                            "{}: version {} there, {} here",
                            f.name, c.version, f.version
                        ));
                    }
                    if c.files != f.files {
                        problems.push(format!(
                            "{}: files {:?} there, {:?} here",
                            f.name, c.files, f.files
                        ));
                    }
                    if c.migration_sources != f.migration_sources.as_slice() {
                        problems.push(format!(
                            "{}: migration sources {:?} there, {:?} here",
                            f.name, c.migration_sources, f.migration_sources
                        ));
                    }
                    if c.fingerprint != f.fingerprint {
                        problems.push(format!(
                            "{}: fingerprint {:?} there, {:?} here",
                            f.name, c.fingerprint, f.fingerprint
                        ));
                    }
                }
            }
        }
        for c in core {
            if !live.iter().any(|f| f.name == c.name) {
                problems.push(format!("{}: in regia_core but not an editor format", c.name));
            }
        }
        assert!(
            problems.is_empty(),
            "regia_core::editor_formats is stale; rerun with REGIA_UPDATE_EDITOR_FORMATS=1, then bump the \
             changed format's version and run `cargo xtask check-formats --update`: {problems:#?}"
        );
    }

    /// Every fingerprint is captured (an empty list would hash as "no fields" and hide a change).
    #[test]
    fn every_fingerprint_lists_its_root_fields() {
        for f in user_format_fingerprints() {
            assert!(
                f.fingerprint.starts_with("format_version,"),
                "{}: {:?}",
                f.name,
                f.fingerprint
            );
            assert!(
                !f.fingerprint.split(';').any(str::is_empty),
                "{}: an empty list",
                f.name
            );
        }
    }

    /// The files the editor writes are the files the table names, and every versioned editor file
    /// is a row or listed in `NOT_EDITOR_FORMATS` with a reason: a new prefs, workspace or
    /// collection file must join the table.
    #[test]
    fn every_versioned_editor_file_is_a_format_or_has_a_reason() {
        let live = user_format_fingerprints();
        let owned = |name: &str| {
            live.iter().any(|f| {
                f.files.iter().any(|p| match p.strip_prefix('/') {
                    Some(whole) => name.rsplit('/').next() == Some(whole),
                    None => name.ends_with(p),
                })
            })
        };
        for file in [
            crate::editor_prefs::PREFS_FILE,
            crate::editor_prefs::KEYMAP_FILE,
            crate::editor_prefs::USER_LAYOUT_FILE,
            crate::editor_layout::PROJECT_LAYOUT_FILE,
            crate::window_state::WINDOW_STATE_FILE,
            crate::inspector::pinned::PINS_FILE,
            crate::feature_profiles::PROJECT_PROFILE_FILE,
            crate::view_state::VIEW_STATE_FILE,
            crate::viewport_ops::SNAP_STEPS_FILE,
            crate::saved_searches::PERSONAL_FILE,
            crate::saved_searches::PROJECT_FILE,
            "layouts/arena.editor_layout.ron",
            "layouts/arena.workspace.editor_layout.ron",
            "profiles/ui.editor_profile.ron",
            "themes/Night.editor_theme.ron",
        ] {
            assert!(owned(file), "{file}: no editor format owns it");
        }
        // Every `pub const *_VERSION: u32` in this crate's sources.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![src];
        let mut unlisted = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir)
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
                .flatten()
            {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                for line in text.lines() {
                    let Some(rest) = line.strip_prefix("pub const ") else {
                        continue;
                    };
                    let Some((name, ty)) = rest.split_once(':') else {
                        continue;
                    };
                    if !name.ends_with("_VERSION") || !ty.trim_start().starts_with("u32") {
                        continue;
                    }
                    if !live.iter().any(|f| f.version_const == name)
                        && !NOT_EDITOR_FORMATS.iter().any(|(n, _)| *n == name)
                    {
                        unlisted.push(format!("{}: {name}", path.display()));
                    }
                }
            }
        }
        assert!(
            unlisted.is_empty(),
            "versioned editor files that are neither an editor format nor in NOT_EDITOR_FORMATS: {unlisted:?}"
        );
        for (_, why) in NOT_EDITOR_FORMATS {
            assert!(!why.is_empty());
        }
    }

    #[test]
    fn the_generated_block_round_trips_the_core_table() {
        // The block the update writes, from the core table itself, is the core file's block: the
        // writer and the checked-in file use one layout (no churn on a no-op update).
        let core: Vec<LiveEditorFormat> = regia_core::editor_formats::EDITOR_FORMATS
            .iter()
            .map(|c| LiveEditorFormat {
                name: c.name,
                files: c.files,
                version_const: "",
                version: c.version,
                migration_sources: c.migration_sources.to_vec(),
                fingerprint: c.fingerprint.to_owned(),
            })
            .collect();
        let text = std::fs::read_to_string(core_file()).unwrap_or_default();
        assert!(
            text.contains(&descriptor_block(&core)),
            "the generated block's layout drifted"
        );
    }
}
