// SPDX-License-Identifier: MPL-2.0
//! The editor's persisted per-user and per-project files as frozen formats (ADR-7902, lead decision
//! on review 3202 #22).
//!
//! The editor's own files (`editor_prefs.ron`, `editor_keymap.ron`, the layouts, the window state,
//! the Inspector pins, feature profiles, the project's view state and snap steps) outlive an editor
//! build like project content does, so `cargo xtask check-formats` guards their serde shapes and
//! `regia upgrade-project` reads their versions. Their types live in `regia_editor`, which is
//! desktop-only and never a dependency of `xtask` or `regia_cli`; this table is the bridge: one
//! descriptor per format with its version, migration sources and serde fingerprint.
//!
//! The table is checked against the real types by `regia_editor`'s
//! `user_formats::tests::the_core_descriptors_match_the_editor_types` (run it with
//! `REGIA_UPDATE_EDITOR_FORMATS=1` to rewrite the generated block below after a deliberate change:
//! then bump the version and add a migrator, and `cargo xtask check-formats --update`).

/// One editor file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorFormat {
    /// The registry name (`docs/formats/format-hashes.ron`, `regia upgrade-project`).
    pub name: &'static str,
    /// The file names (`/name`, a whole file name) and suffixes (`.suffix`) the format owns.
    pub files: &'static [&'static str],
    /// The current version.
    pub version: u32,
    /// The `from` versions of the registered migration steps.
    pub migration_sources: &'static [u32],
    /// The serde shape (`crate::schema::join_field_lists` of the root and every nested type).
    pub fingerprint: &'static str,
}

/// The descriptor named `name`.
#[must_use]
pub fn editor_format(name: &str) -> Option<&'static EditorFormat> {
    EDITOR_FORMATS.iter().find(|f| f.name == name)
}

// BEGIN GENERATED (regia_editor user_formats; REGIA_UPDATE_EDITOR_FORMATS=1)
/// Every editor file format.
pub const EDITOR_FORMATS: &[EditorFormat] = &[
    EditorFormat {
        name: "editor-keymap",
        files: &["/editor_keymap.ron"],
        version: 1,
        migration_sources: &[],
        fingerprint: "format_version,bindings",
    },
    EditorFormat {
        name: "editor-layout",
        files: &["/editor_layout.ron", ".editor_layout.ron"],
        version: 3,
        migration_sources: &[1, 2],
        fingerprint: "format_version,root,known_panels,floating,instances;tabs,active,rect;filter,folder,columns,locked;Tabs,Split;0:{tabs,active},1:{axis,fraction,first,second};Horizontal,Vertical",
    },
    EditorFormat {
        name: "editor-panel",
        files: &[".editorpanel.ron"],
        version: 1,
        migration_sources: &[],
        fingerprint: "format_version,title,layout,driver,size;Logic,Lua",
    },
    EditorFormat {
        name: "editor-prefs",
        files: &["/editor_prefs.ron"],
        version: 1,
        migration_sources: &[],
        fingerprint: "format_version,values;Bool,Int,Float,Text;0:newtype,1:newtype,2:newtype,3:newtype",
    },
    EditorFormat {
        name: "editor-profile",
        files: &["/editor_profile.ron", ".editor_profile.ron"],
        version: 1,
        migration_sources: &[],
        fingerprint: "format_version,name,hidden_areas,hidden_panels,hidden_kinds;3d,2d,animation,ui,networking,terrain,procgen,audio,scripting",
    },
    EditorFormat {
        name: "editor-theme",
        files: &[".editor_theme.ron"],
        version: 1,
        migration_sources: &[],
        fingerprint: "format_version,name,tokens",
    },
    EditorFormat {
        name: "editor-view-state",
        files: &["/editor_view.ron"],
        version: 1,
        migration_sources: &[],
        fingerprint: "format_version,recent_levels,levels,ui_designer;camera,bookmarks,locked,hidden,isolated,locked_layers,guides_2d;target,distance,yaw,pitch,speed,fov_deg;size,safe_area,theme",
    },
    EditorFormat {
        name: "editor-window-state",
        files: &["/editor_window_state.ron"],
        version: 1,
        migration_sources: &[],
        fingerprint: "format_version,projects,order;monitor,position,size,fullscreen,maximized,level,asset_tabs",
    },
    EditorFormat {
        name: "inspector-pins",
        files: &["/inspector_pins.ron"],
        version: 1,
        migration_sources: &[],
        fingerprint: "format_version,pins",
    },
    EditorFormat {
        name: "saved-searches",
        files: &["/saved_searches.ron"],
        version: 1,
        migration_sources: &[],
        fingerprint: "format_version,searches,favorites;name,query,providers",
    },
    EditorFormat {
        name: "viewport-snap",
        files: &["/viewport_snap.ron"],
        version: 1,
        migration_sources: &[],
        fingerprint: "format_version,distance,angle_deg,scale",
    },
];
// END GENERATED

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_sorted_and_files_are_names_or_suffixes() {
        let names: Vec<&str> = EDITOR_FORMATS.iter().map(|f| f.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted, "sorted by name, no repeats");
        for f in EDITOR_FORMATS {
            assert!(f.version >= 1, "{}", f.name);
            assert!(!f.files.is_empty(), "{}", f.name);
            assert!(
                f.files.iter().all(|p| p.starts_with('/') || p.starts_with('.')),
                "{}: {:?}",
                f.name,
                f.files
            );
            assert!(
                f.migration_sources.iter().all(|v| *v < f.version),
                "{}: a migration source at or above the current version",
                f.name
            );
            assert!(!f.fingerprint.is_empty(), "{}", f.name);
        }
        assert_eq!(editor_format("inspector-pins").map(|f| f.version), Some(1));
        assert!(editor_format("nope").is_none());
    }
}
