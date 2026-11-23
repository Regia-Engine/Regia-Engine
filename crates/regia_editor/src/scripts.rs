// SPDX-License-Identifier: MPL-2.0
//! "New script" (`docs/specs/19-editor.md` § 5).
//!
//! Writes `game/src/scripts/<name>.rs` and registers it in `game/src/lib.rs` by inserting a
//! `#[path]` module at the `// REGIA:SCRIPTS` marker. The Blank template ships that marker.

use std::path::{Path, PathBuf};

/// Creates `game/src/scripts/<name>.rs` and registers the module.
///
/// # Errors
///
/// The name is not a Rust identifier, the project has no `game/src/lib.rs` or no marker,
/// the script already exists, or a write failed.
pub fn create_script(project_root: &Path, name: &str) -> Result<PathBuf, String> {
    create_script_from(project_root, name, crate::creators::ScriptTemplate::Empty)
}

/// [`create_script`] with the file body from `template` (the New script wizard's choice,
/// ADR-7234).
///
/// # Errors
///
/// As [`create_script`].
pub fn create_script_from(
    project_root: &Path,
    name: &str,
    template: crate::creators::ScriptTemplate,
) -> Result<PathBuf, String> {
    let name = crate::scaffold::crate_name_for(name).map_err(|err| err.to_string())?;
    let lib_path = project_root.join("game/src/lib.rs");
    let mut lib = std::fs::read_to_string(&lib_path)
        .map_err(|err| format!("cannot read {}: {err}", lib_path.display()))?;
    // Projects created before the rename carry the legacy marker (ADR-1388).
    let marker = [
        regia_core::brand::SCRIPTS_MARKER,
        regia_core::brand::LEGACY_SCRIPTS_MARKER,
    ]
    .into_iter()
    .find(|marker| lib.contains(marker));
    let Some(marker) = marker else {
        return Err(format!(
            "{} has no `{}` marker to register the script at",
            lib_path.display(),
            regia_core::brand::SCRIPTS_MARKER
        ));
    };
    let registration = format!("#[path = \"scripts/{name}.rs\"]\nmod {name};\n");
    if lib.contains(&format!("mod {name};")) {
        return Err(format!("script `{name}` is already registered"));
    }
    let script_dir = project_root.join("game/src/scripts");
    std::fs::create_dir_all(&script_dir)
        .map_err(|err| format!("cannot create {}: {err}", script_dir.display()))?;
    let script = script_dir.join(format!("{name}.rs"));
    if script.exists() {
        return Err(format!("{} already exists", script.display()));
    }
    let body = template.body(&name);
    std::fs::write(&script, body).map_err(|err| format!("cannot write {}: {err}", script.display()))?;
    lib = lib.replacen(marker, &format!("{registration}{marker}"), 1);
    std::fs::write(&lib_path, lib).map_err(|err| format!("cannot write {}: {err}", lib_path.display()))?;
    tracing::info!(target: "regia_editor", "created script {}", script.display());
    Ok(script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_script_inserts_at_the_marker_and_refuses_duplicates() {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("regia-script-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("game/src")).expect("dir");
        std::fs::write(root.join("game/src/lib.rs"), "//! game\n\n// REGIA:SCRIPTS\n").expect("lib");

        let path = create_script(&root, "Door").expect("script");
        assert!(path.ends_with("game/src/scripts/door.rs"));
        let lib = std::fs::read_to_string(root.join("game/src/lib.rs")).expect("lib");
        assert!(lib.contains("mod door;"), "{lib}");
        assert!(lib.contains("REGIA:SCRIPTS"), "the marker stays: {lib}");
        assert!(create_script(&root, "door").is_err(), "duplicate refused");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_script_registers_at_the_legacy_marker() {
        // A project created before the rename (ADR-1388) carries the legacy marker.
        let legacy = regia_core::brand::LEGACY_SCRIPTS_MARKER;
        let root = std::env::temp_dir().join(format!("regia-script-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("game/src")).expect("dir");
        std::fs::write(root.join("game/src/lib.rs"), format!("//! game\n\n{legacy}\n")).expect("lib");

        create_script(&root, "Gate").expect("script");
        let lib = std::fs::read_to_string(root.join("game/src/lib.rs")).expect("lib");
        assert!(
            lib.contains(&format!("mod gate;\n{legacy}")),
            "registered above the marker: {lib}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
