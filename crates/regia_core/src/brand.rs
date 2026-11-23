// SPDX-License-Identifier: MPL-2.0
//! The engine's brand names, and the one-release compatibility layer for the rename to Regia
//! Engine (ADR-1388).
//!
//! Every name a user, a project or a player's disk can carry is defined here twice: the current
//! spelling and the legacy one. Before the rename both are the same and every helper below is a
//! plain lookup. `cargo xtask rename-brand` flips [`BRAND_RENAMED`] to `true` (this file is listed
//! in `xtask/rename-keep.txt`, so the tool changes nothing else in it); from then on the helpers
//! read the current name first and fall back to the legacy one with a one-time deprecation
//! warning that names the replacement:
//!
//! - environment variables ([`env_var`], [`env_var_os`]): the current prefix wins;
//! - the project manifest ([`project_manifest`]) and the per-project state directory
//!   ([`project_state_dir`]);
//! - the per-user data directories ([`user_app_dir`]): the legacy directory is moved to the new
//!   name once, or used in place (with a warning) when the move fails;
//! - reflected type paths stored in scenes, prefabs and cooked data ([`current_type_path`],
//!   [`lookup_type_path`]): files written before the rename name the legacy crates;
//! - engine-owned keys inside files ([`find_key`], [`key_matches`]): EXR header attributes,
//!   manifest schemas and sidecar format ids (ADR-8670).
//!
//! On-disk formats keep every byte: magics, versions and field names never carry the brand.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::env::VarError;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// `true` once `cargo xtask rename-brand` has been applied to the tree. Selects the current
/// spelling of every name below; the legacy spellings never change.
pub const BRAND_RENAMED: bool = true;

/// The product name in user-facing text ("Regia Engine"; never the short form alone).
pub const PRODUCT_NAME: &str = if BRAND_RENAMED { "Regia Engine" } else { "Ferrite" };
/// The product name before the rename.
pub const LEGACY_PRODUCT_NAME: &str = "Ferrite";

/// The prefix of every engine environment variable.
pub const ENV_PREFIX: &str = if BRAND_RENAMED { "REGIA_" } else { "FERRITE_" };
/// The environment variable prefix before the rename; still read for one release.
pub const LEGACY_ENV_PREFIX: &str = "FERRITE_";

/// The project manifest file name at a project's root.
pub const MANIFEST_FILE: &str = if BRAND_RENAMED {
    "Regia.toml"
} else {
    "Ferrite.toml"
};
/// The project manifest file name before the rename; still read for one release.
pub const LEGACY_MANIFEST_FILE: &str = "Ferrite.toml";

/// The per-project state directory (cooked cache, asset database, agent endpoint, autosaves).
pub const PROJECT_STATE_DIR: &str = if BRAND_RENAMED { ".regia" } else { ".ferrite" };
/// The per-project state directory before the rename; still used in place for one release.
pub const LEGACY_PROJECT_STATE_DIR: &str = ".ferrite";

/// The directory under the OS data/cache roots that holds one directory per game.
pub const USER_DIR_NAME: &str = if BRAND_RENAMED { "RegiaEngine" } else { "Ferrite" };
/// The per-user directory name before the rename; moved to [`USER_DIR_NAME`] on first use.
pub const LEGACY_USER_DIR_NAME: &str = "Ferrite";

/// The prefix of every engine crate name (`<prefix>core`, `<prefix>render`, ...).
pub const CRATE_PREFIX: &str = if BRAND_RENAMED { "regia_" } else { "ferrite_" };
/// The engine crate prefix before the rename (it appears in reflected type paths on disk).
pub const LEGACY_CRATE_PREFIX: &str = "ferrite_";

/// The umbrella crate's name (also the CLI binary's name).
pub const MAIN_CRATE: &str = if BRAND_RENAMED { "regia" } else { "ferrite" };
/// The umbrella crate's name before the rename.
pub const LEGACY_MAIN_CRATE: &str = "ferrite";

/// The line in a game's `lib.rs` where the editor registers new scripts.
pub const SCRIPTS_MARKER: &str = if BRAND_RENAMED {
    "// REGIA:SCRIPTS"
} else {
    "// FERRITE:SCRIPTS"
};
/// The scripts marker before the rename; projects created earlier still carry it.
pub const LEGACY_SCRIPTS_MARKER: &str = "// FERRITE:SCRIPTS";

/// The prefix of the built-in UI theme names (`<prefix>dark`, `<prefix>light`, ...).
pub const THEME_PREFIX: &str = if BRAND_RENAMED { "regia-" } else { "ferrite-" };
/// The built-in theme prefix before the rename; `.theme.ron` files (`extends`), saved settings
/// and agent calls written earlier name the built-ins with it.
pub const LEGACY_THEME_PREFIX: &str = "ferrite-";

/// The brand word that starts the engine's own keys and format ids in files it writes: EXR header
/// attributes (`regia.seed`), PNG text keywords, manifest schemas (`regia.render_out/2`) and
/// sidecar formats (`regia-bake-stamp/1`). ADR-8670 moved these from the legacy spelling.
pub const KEY_PREFIX: &str = if BRAND_RENAMED { "regia" } else { "ferrite" };
/// The brand word of those keys before ADR-8670; files written earlier carry it, so every reader
/// accepts it through [`key_matches`] / [`find_key`].
pub const LEGACY_KEY_PREFIX: &str = "ferrite";

/// The legacy spelling of an engine-owned key: `regia.seed` → `ferrite.seed`,
/// `regia-bake-stamp/1` → `ferrite-bake-stamp/1`. `None` when `key` does not start with
/// [`KEY_PREFIX`] followed by `.` or `-`, and when both spellings are the same.
#[must_use]
pub fn legacy_key(key: &str) -> Option<String> {
    if KEY_PREFIX == LEGACY_KEY_PREFIX {
        return None;
    }
    let rest = key.strip_prefix(KEY_PREFIX)?;
    (rest.starts_with('.') || rest.starts_with('-')).then(|| format!("{LEGACY_KEY_PREFIX}{rest}"))
}

/// Whether `found` (read from a file) names the engine-owned `key` (current spelling) in either
/// spelling: `key` itself or its [`legacy_key`].
#[must_use]
pub fn key_matches(found: &str, key: &str) -> bool {
    found == key || legacy_key(key).is_some_and(|legacy| found == legacy)
}

/// The value of the engine-owned `key` (current spelling) among `pairs` read from a file: the
/// current spelling wins, the [`legacy_key`] spelling is the fallback (a file written before
/// ADR-8670). `None` when neither is present.
pub fn find_key<'a, V>(pairs: impl IntoIterator<Item = (&'a str, V)> + Clone, key: &str) -> Option<V> {
    if let Some((_, v)) = pairs.clone().into_iter().find(|(k, _)| *k == key) {
        return Some(v);
    }
    let legacy = legacy_key(key)?;
    pairs.into_iter().find(|(k, _)| *k == legacy).map(|(_, v)| v)
}

/// The current name of a built-in theme named with [`LEGACY_THEME_PREFIX`] (`ferrite-dark` →
/// `regia-dark`); `None` for every other name and before the rename. The caller checks that the
/// mapped theme exists and warns with [`warn_legacy_once`].
#[must_use]
pub fn current_theme_name(name: &str) -> Option<String> {
    if THEME_PREFIX == LEGACY_THEME_PREFIX {
        return None;
    }
    name.strip_prefix(LEGACY_THEME_PREFIX)
        .map(|rest| format!("{THEME_PREFIX}{rest}"))
}

/// The current spelling of a log target, or target prefix, that names an engine crate with its
/// legacy spelling (`ferrite_render` → `regia_render`, `ferrite::boot` → `regia::boot`,
/// `ferrite` → `regia`). `None` for every other target and before the rename.
#[must_use]
pub fn current_log_target(target: &str) -> Option<String> {
    if CRATE_PREFIX == LEGACY_CRATE_PREFIX && MAIN_CRATE == LEGACY_MAIN_CRATE {
        return None;
    }
    if let Some(rest) = target.strip_prefix(LEGACY_CRATE_PREFIX) {
        return Some(format!("{CRATE_PREFIX}{rest}"));
    }
    let rest = target.strip_prefix(LEGACY_MAIN_CRATE)?;
    (rest.is_empty() || rest.starts_with("::")).then(|| format!("{MAIN_CRATE}{rest}"))
}

/// A `tracing` filter (`info,ferrite_render=debug`) in which every directive whose target names
/// an engine crate with its legacy spelling is followed by the same directive for the current
/// name (`...,ferrite_render=debug,regia_render=debug`). The legacy directive stays, so a game
/// crate that happens to carry the legacy prefix keeps its own directive. Borrows `filter` when
/// nothing changes (always before the rename).
#[must_use]
pub fn current_log_filter(filter: &str) -> Cow<'_, str> {
    let mut out: Vec<Cow<'_, str>> = Vec::new();
    let mut changed = false;
    for directive in filter.split(',') {
        out.push(Cow::Borrowed(directive));
        let trimmed = directive.trim();
        let end = trimmed.find(['=', '[']).unwrap_or(trimmed.len());
        let (target, rest) = trimmed.split_at(end);
        if let Some(current) = current_log_target(target) {
            warn_legacy_once("log filter target", target, &current);
            out.push(Cow::Owned(format!("{current}{rest}")));
            changed = true;
        }
    }
    if changed {
        Cow::Owned(out.join(","))
    } else {
        Cow::Borrowed(filter)
    }
}

/// Names already warned about, so each legacy name is reported once per process.
static WARNED: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());

/// Logs a deprecation warning the first time `legacy` is used in this process. Returns whether a
/// warning was logged (`false` on every later call with the same `legacy`).
pub fn warn_legacy_once(kind: &str, legacy: &str, replacement: &str) -> bool {
    let first = WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(legacy.to_owned());
    if !first {
        return false;
    }
    tracing::warn!(
        target: "brand",
        "{kind} `{legacy}` uses the name from before the rename to {PRODUCT_NAME}; it is still read \
         in this release, use `{replacement}` instead"
    );
    true
}

/// The legacy spelling of an environment variable name: `Some("<legacy prefix>X")` for
/// `"<current prefix>X"` when the two prefixes differ, otherwise `None`.
#[must_use]
pub fn legacy_env_name(key: &str, current_prefix: &str, legacy_prefix: &str) -> Option<String> {
    if current_prefix == legacy_prefix {
        return None;
    }
    key.strip_prefix(current_prefix)
        .map(|rest| format!("{legacy_prefix}{rest}"))
}

/// Looks `key` up through `lookup`, falling back to its legacy spelling. Returns the value and,
/// when it came from the legacy name, that name. The current name always wins.
pub fn resolve_env_with<T>(
    key: &str,
    current_prefix: &str,
    legacy_prefix: &str,
    lookup: impl Fn(&str) -> Option<T>,
) -> Option<(T, Option<String>)> {
    if let Some(value) = lookup(key) {
        return Some((value, None));
    }
    let legacy = legacy_env_name(key, current_prefix, legacy_prefix)?;
    lookup(&legacy).map(|value| (value, Some(legacy)))
}

/// The other spelling of an engine environment variable: `(legacy name, true)` for a name with
/// the current prefix, `(current name, false)` for a name with the legacy prefix (a manifest
/// written before the rename may name `env:<legacy>_X`). `None` before the rename and for keys
/// without either prefix.
#[must_use]
pub fn alternate_env_name(key: &str) -> Option<(String, bool)> {
    if let Some(legacy) = legacy_env_name(key, ENV_PREFIX, LEGACY_ENV_PREFIX) {
        return Some((legacy, true));
    }
    legacy_env_name(key, LEGACY_ENV_PREFIX, ENV_PREFIX).map(|current| (current, false))
}

/// [`std::env::var`] that also reads the other spelling of an engine variable: `REGIA_X` is read
/// first; when it is not set, `FERRITE_X` is read and a one-time warning names `REGIA_X`. Keys
/// without the engine prefix behave exactly like [`std::env::var`].
pub fn env_var<K: AsRef<OsStr>>(key: K) -> Result<String, VarError> {
    let key = key.as_ref();
    let first = std::env::var(key);
    if !matches!(first, Err(VarError::NotPresent)) {
        return first;
    }
    let Some(name) = key.to_str() else {
        return first;
    };
    let Some((other, other_is_legacy)) = alternate_env_name(name) else {
        return first;
    };
    match std::env::var(&other) {
        Err(VarError::NotPresent) => first,
        found => {
            if other_is_legacy {
                warn_legacy_once("environment variable", &other, name);
            }
            found
        }
    }
}

/// [`std::env::var_os`] that also reads the other spelling of an engine variable (see
/// [`env_var`]).
pub fn env_var_os<K: AsRef<OsStr>>(key: K) -> Option<OsString> {
    let key = key.as_ref();
    if let Some(value) = std::env::var_os(key) {
        return Some(value);
    }
    let (other, other_is_legacy) = alternate_env_name(key.to_str()?)?;
    let value = std::env::var_os(&other)?;
    if other_is_legacy {
        warn_legacy_once("environment variable", &other, &key.to_string_lossy());
    }
    Some(value)
}

/// Picks `root/current` or `root/legacy`: the current entry when it exists, when the legacy one
/// does not, or when both names are the same; otherwise the legacy entry. The flag is `true` when
/// the legacy entry was picked.
pub fn pick_legacy_path(
    root: &Path,
    current: &str,
    legacy: &str,
    exists: impl Fn(&Path) -> bool,
) -> (PathBuf, bool) {
    let new = root.join(current);
    if current == legacy || exists(&new) {
        return (new, false);
    }
    let old = root.join(legacy);
    if exists(&old) { (old, true) } else { (new, false) }
}

/// The project manifest at `root`: [`MANIFEST_FILE`], or [`LEGACY_MANIFEST_FILE`] (with a one-time
/// warning) when only the legacy file exists. `None` when `root` holds neither.
#[must_use]
pub fn project_manifest(root: &Path) -> Option<PathBuf> {
    let (path, legacy) = pick_legacy_path(root, MANIFEST_FILE, LEGACY_MANIFEST_FILE, Path::is_file);
    if !path.is_file() {
        return None;
    }
    if legacy {
        warn_legacy_once("project manifest", LEGACY_MANIFEST_FILE, MANIFEST_FILE);
    }
    Some(path)
}

/// Whether `root` is a project root (holds a current or legacy manifest).
#[must_use]
pub fn is_project_root(root: &Path) -> bool {
    project_manifest(root).is_some()
}

/// The per-project state directory of the project at `root`: [`PROJECT_STATE_DIR`], or the
/// legacy directory used in place (with a one-time warning) when only that one exists.
/// `upgrade-project` moves a legacy directory to the new name.
///
/// A project not upgraded yet (only [`LEGACY_MANIFEST_FILE`]) that has neither directory, such
/// as a fresh clone, gets the legacy name too: its `.gitignore` ignores that one, and the state
/// directory holds the cook cache and the agent's bearer token (ADR-1429).
#[must_use]
pub fn project_state_dir(root: &Path) -> PathBuf {
    let (path, legacy) = pick_legacy_path(root, PROJECT_STATE_DIR, LEGACY_PROJECT_STATE_DIR, Path::is_dir);
    let not_upgraded = PROJECT_STATE_DIR != LEGACY_PROJECT_STATE_DIR
        && !legacy
        && !path.is_dir()
        && root.join(LEGACY_MANIFEST_FILE).is_file()
        && !root.join(MANIFEST_FILE).is_file();
    let (path, legacy) = if not_upgraded {
        (root.join(LEGACY_PROJECT_STATE_DIR), true)
    } else {
        (path, legacy)
    };
    if legacy {
        warn_legacy_once(
            "project state directory",
            LEGACY_PROJECT_STATE_DIR,
            PROJECT_STATE_DIR,
        );
    }
    path
}

/// How [`resolve_user_app_dir`] found a per-game user directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserDirSource {
    /// The directory has the current name (or neither directory exists yet).
    Current,
    /// The legacy directory was moved to the current name.
    Migrated,
    /// The legacy directory exists but could not be moved; it is used in place.
    LegacyInPlace,
}

/// The per-game directory `base/<current>/<app>`. When it does not exist but
/// `base/<legacy>/<app>` does, the legacy directory is renamed to the current path (one atomic
/// rename on the same volume); if that fails, the legacy path is returned and used in place.
pub fn resolve_user_app_dir(base: &Path, app: &str, current: &str, legacy: &str) -> (PathBuf, UserDirSource) {
    let new = base.join(current).join(app);
    if current == legacy || new.exists() {
        return (new, UserDirSource::Current);
    }
    let old = base.join(legacy).join(app);
    if !old.is_dir() {
        return (new, UserDirSource::Current);
    }
    let moved = std::fs::create_dir_all(base.join(current)).and_then(|()| std::fs::rename(&old, &new));
    match moved {
        Ok(()) => (new, UserDirSource::Migrated),
        Err(_) => (old, UserDirSource::LegacyInPlace),
    }
}

/// [`resolve_user_app_dir`] with the brand names, logging what happened once per process.
#[must_use]
pub fn user_app_dir(base: &Path, app: &str) -> PathBuf {
    let (path, source) = resolve_user_app_dir(base, app, USER_DIR_NAME, LEGACY_USER_DIR_NAME);
    match source {
        UserDirSource::Current => {}
        UserDirSource::Migrated => {
            let old = base.join(LEGACY_USER_DIR_NAME).join(app);
            tracing::info!(target: "brand", "moved {} to {}", old.display(), path.display());
        }
        UserDirSource::LegacyInPlace => {
            let legacy = path.display().to_string();
            let replacement = base.join(USER_DIR_NAME).join(app).display().to_string();
            warn_legacy_once("user directory", &legacy, &replacement);
        }
    }
    path
}

/// Rewrites every crate name in `path` that uses a legacy spelling (`<legacy prefix>x::` or
/// `<legacy main>::`) to the current spelling. Borrows `path` when nothing changes (always before
/// the rename).
#[must_use]
pub fn rebrand_type_path<'a>(
    path: &'a str,
    current_prefix: &str,
    legacy_prefix: &str,
    current_main: &str,
    legacy_main: &str,
) -> Cow<'a, str> {
    if current_prefix == legacy_prefix && current_main == legacy_main {
        return Cow::Borrowed(path);
    }
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(path.len());
    let mut changed = false;
    let mut rest = path;
    while !rest.is_empty() {
        let token_len = rest.find(|c: char| !is_ident(c)).unwrap_or(rest.len());
        if token_len == 0 {
            let c = rest.chars().next().unwrap_or(' ');
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let (token, after) = rest.split_at(token_len);
        let is_crate = after.starts_with("::") && !out.ends_with("::");
        if is_crate && token == legacy_main {
            out.push_str(current_main);
            changed = true;
        } else if let Some(name) = token.strip_prefix(legacy_prefix).filter(|_| is_crate) {
            out.push_str(current_prefix);
            out.push_str(name);
            changed = true;
        } else {
            out.push_str(token);
        }
        rest = after;
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(path)
    }
}

/// [`rebrand_type_path`] with the brand names: a type path read from a file written before the
/// rename, spelled with the current crate names.
#[must_use]
pub fn current_type_path(path: &str) -> Cow<'_, str> {
    rebrand_type_path(
        path,
        CRATE_PREFIX,
        LEGACY_CRATE_PREFIX,
        MAIN_CRATE,
        LEGACY_MAIN_CRATE,
    )
}

/// A type path spelled with the legacy crate names, for schema fingerprints: a format's frozen
/// hash (`cargo xtask check-formats`) must not change because the crates were renamed, since the
/// bytes on disk did not.
#[must_use]
pub fn fingerprint_type_path(path: &str) -> Cow<'_, str> {
    rebrand_type_path(
        path,
        LEGACY_CRATE_PREFIX,
        CRATE_PREFIX,
        LEGACY_MAIN_CRATE,
        MAIN_CRATE,
    )
}

/// Looks a reflected type path up through `get`, retrying with [`current_type_path`] when the
/// path as written is unknown. Use it wherever a type path read from disk meets the registry.
pub fn lookup_type_path<T>(path: &str, get: impl Fn(&str) -> Option<T>) -> Option<T> {
    get(path).or_else(|| match current_type_path(path) {
        Cow::Owned(current) => get(&current),
        Cow::Borrowed(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("brand-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn current_names_follow_the_flag_and_legacy_names_never_change() {
        assert_eq!(LEGACY_ENV_PREFIX, "FERRITE_");
        assert_eq!(LEGACY_MANIFEST_FILE, "Ferrite.toml");
        assert_eq!(LEGACY_PROJECT_STATE_DIR, ".ferrite");
        assert_eq!(LEGACY_USER_DIR_NAME, "Ferrite");
        assert_eq!(LEGACY_CRATE_PREFIX, "ferrite_");
        assert_eq!(LEGACY_MAIN_CRATE, "ferrite");
        if BRAND_RENAMED {
            assert_eq!(ENV_PREFIX, "REGIA_");
            assert_eq!(MANIFEST_FILE, "Regia.toml");
            assert_eq!(PROJECT_STATE_DIR, ".regia");
            assert_eq!(USER_DIR_NAME, "RegiaEngine");
            assert_eq!(CRATE_PREFIX, "regia_");
            assert_eq!(MAIN_CRATE, "regia");
            assert_eq!(PRODUCT_NAME, "Regia Engine");
            assert_eq!(SCRIPTS_MARKER, "// REGIA:SCRIPTS");
        } else {
            assert_eq!(ENV_PREFIX, LEGACY_ENV_PREFIX);
            assert_eq!(MANIFEST_FILE, LEGACY_MANIFEST_FILE);
            assert_eq!(PROJECT_STATE_DIR, LEGACY_PROJECT_STATE_DIR);
            assert_eq!(USER_DIR_NAME, LEGACY_USER_DIR_NAME);
            assert_eq!(SCRIPTS_MARKER, LEGACY_SCRIPTS_MARKER);
        }
    }

    #[test]
    fn env_current_name_wins_and_legacy_is_the_fallback() {
        let vars = [
            ("REGIA_LOG", "debug"),
            ("FERRITE_LOG", "trace"),
            ("FERRITE_PROJECT", "/p"),
        ];
        let lookup = |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned());
        let hit = resolve_env_with("REGIA_LOG", "REGIA_", "FERRITE_", lookup);
        assert_eq!(hit, Some(("debug".to_owned(), None)), "the new name wins");
        let hit = resolve_env_with("REGIA_PROJECT", "REGIA_", "FERRITE_", lookup);
        assert_eq!(hit, Some(("/p".to_owned(), Some("FERRITE_PROJECT".to_owned()))));
        assert_eq!(resolve_env_with("REGIA_NONE", "REGIA_", "FERRITE_", lookup), None);
        // Keys outside the engine prefix never fall back.
        assert_eq!(resolve_env_with("HOME", "REGIA_", "FERRITE_", lookup), None);
        // Before the rename the prefixes agree: a plain lookup.
        assert_eq!(legacy_env_name("FERRITE_LOG", "FERRITE_", "FERRITE_"), None);
        // A legacy name (from an old manifest's `env:` reference) finds the renamed variable.
        let hit = resolve_env_with("FERRITE_KEYSTORE", "FERRITE_", "REGIA_", |k: &str| {
            (k == "REGIA_KEYSTORE").then(|| "/k".to_owned())
        });
        assert_eq!(hit, Some(("/k".to_owned(), Some("REGIA_KEYSTORE".to_owned()))));
        if ENV_PREFIX == LEGACY_ENV_PREFIX {
            assert_eq!(alternate_env_name("FERRITE_LOG"), None);
        } else {
            assert_eq!(
                alternate_env_name("REGIA_LOG"),
                Some(("FERRITE_LOG".to_owned(), true))
            );
            assert_eq!(
                alternate_env_name("FERRITE_LOG"),
                Some(("REGIA_LOG".to_owned(), false))
            );
        }
    }

    #[test]
    fn env_var_reads_the_process_environment_like_std() {
        // PATH is set in every test environment and carries no engine prefix.
        assert_eq!(env_var("PATH").ok(), std::env::var("PATH").ok());
        assert_eq!(env_var_os("PATH"), std::env::var_os("PATH"));
        let missing = format!("{ENV_PREFIX}BRAND_TEST_UNSET_{}", std::process::id());
        assert_eq!(env_var(&missing), Err(VarError::NotPresent));
        assert_eq!(env_var_os(&missing), None);
    }

    #[test]
    fn legacy_warning_is_logged_once_per_name() {
        let name = format!("FERRITE_ONCE_{}", std::process::id());
        assert!(warn_legacy_once("environment variable", &name, "REGIA_ONCE"));
        assert!(!warn_legacy_once("environment variable", &name, "REGIA_ONCE"));
    }

    #[test]
    fn manifest_and_state_dir_prefer_new_names_and_read_legacy_ones() {
        let root = temp_dir("manifest");
        std::fs::write(root.join("Ferrite.toml"), "[project]\n").expect("write");
        std::fs::create_dir_all(root.join(".ferrite/cooked")).expect("mkdir");
        let (path, legacy) = pick_legacy_path(&root, "Regia.toml", "Ferrite.toml", Path::is_file);
        assert_eq!(
            (path, legacy),
            (root.join("Ferrite.toml"), true),
            "only the old file exists"
        );
        let (path, legacy) = pick_legacy_path(&root, ".regia", ".ferrite", Path::is_dir);
        assert_eq!((path, legacy), (root.join(".ferrite"), true));

        std::fs::write(root.join("Regia.toml"), "[project]\n").expect("write");
        std::fs::create_dir_all(root.join(".regia")).expect("mkdir");
        let (path, legacy) = pick_legacy_path(&root, "Regia.toml", "Ferrite.toml", Path::is_file);
        assert_eq!(
            (path, legacy),
            (root.join("Regia.toml"), false),
            "the new file wins"
        );
        let (path, legacy) = pick_legacy_path(&root, ".regia", ".ferrite", Path::is_dir);
        assert_eq!((path, legacy), (root.join(".regia"), false));

        let empty = temp_dir("manifest-empty");
        let (path, legacy) = pick_legacy_path(&empty, ".regia", ".ferrite", Path::is_dir);
        assert_eq!(
            (path, legacy),
            (empty.join(".regia"), false),
            "a new project gets the new name"
        );
        assert!(project_manifest(&empty).is_none());
        assert!(
            is_project_root(&root),
            "a legacy or current manifest makes a project root"
        );
        assert_eq!(project_state_dir(&empty), empty.join(PROJECT_STATE_DIR));

        // A project not upgraded yet (legacy manifest, no state directory: a fresh clone) keeps
        // the legacy name, which its `.gitignore` covers; once upgraded, the new name.
        let clone = temp_dir("manifest-clone");
        std::fs::write(clone.join(LEGACY_MANIFEST_FILE), "[project]\n").expect("write");
        assert_eq!(project_state_dir(&clone), clone.join(LEGACY_PROJECT_STATE_DIR));
        std::fs::rename(clone.join(LEGACY_MANIFEST_FILE), clone.join(MANIFEST_FILE)).expect("upgrade");
        assert_eq!(project_state_dir(&clone), clone.join(PROJECT_STATE_DIR));
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(empty);
        let _ = std::fs::remove_dir_all(clone);
    }

    #[test]
    fn legacy_user_dir_is_moved_once_with_its_files() {
        let base = temp_dir("userdir");
        let old_save = base.join("Ferrite/do27/saves/slot0.sav");
        std::fs::create_dir_all(old_save.parent().expect("parent")).expect("mkdir");
        std::fs::write(&old_save, b"FRTS\x01").expect("write");

        let (path, source) = resolve_user_app_dir(&base, "do27", "RegiaEngine", "Ferrite");
        assert_eq!(source, UserDirSource::Migrated);
        assert_eq!(path, base.join("RegiaEngine/do27"));
        assert_eq!(
            std::fs::read(path.join("saves/slot0.sav")).expect("read"),
            b"FRTS\x01"
        );
        assert!(!old_save.exists(), "moved, not copied");

        let (again, source) = resolve_user_app_dir(&base, "do27", "RegiaEngine", "Ferrite");
        assert_eq!(
            (again, source),
            (path, UserDirSource::Current),
            "second run: nothing to move"
        );

        let (fresh, source) = resolve_user_app_dir(&base, "new_game", "RegiaEngine", "Ferrite");
        assert_eq!(
            (fresh, source),
            (base.join("RegiaEngine/new_game"), UserDirSource::Current)
        );
        assert!(
            !base.join("RegiaEngine/new_game").exists(),
            "resolving never creates the dir"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn user_dir_both_present_keeps_the_new_one() {
        let base = temp_dir("userdir-both");
        std::fs::create_dir_all(base.join("Ferrite/game")).expect("mkdir");
        std::fs::create_dir_all(base.join("RegiaEngine/game")).expect("mkdir");
        let (path, source) = resolve_user_app_dir(&base, "game", "RegiaEngine", "Ferrite");
        assert_eq!(
            (path, source),
            (base.join("RegiaEngine/game"), UserDirSource::Current)
        );
        assert!(base.join("Ferrite/game").is_dir(), "the old dir is left alone");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn legacy_type_paths_map_to_current_crates() {
        let map = |p| rebrand_type_path(p, "regia_", "ferrite_", "regia", "ferrite");
        assert_eq!(
            map("ferrite_scene::transform::Transform"),
            "regia_scene::transform::Transform"
        );
        assert_eq!(map("ferrite::prelude::Thing"), "regia::prelude::Thing");
        assert_eq!(
            map("ferrite_core::Handle<ferrite_asset::CpuMesh>"),
            "regia_core::Handle<regia_asset::CpuMesh>"
        );
        assert_eq!(
            map("core::option::Option<ferrite_render::PointLight>"),
            "core::option::Option<regia_render::PointLight>"
        );
        // Only crate segments: a type or module that merely starts with the prefix is kept.
        assert_eq!(
            map("my_game::ferrite_bits::Thing"),
            "my_game::ferrite_bits::Thing"
        );
        assert_eq!(map("my_game::Ferrite"), "my_game::Ferrite");
        assert!(matches!(
            map("regia_scene::transform::Transform"),
            Cow::Borrowed(_)
        ));
        // Before the rename nothing is rewritten.
        let same = rebrand_type_path("ferrite_scene::X", "ferrite_", "ferrite_", "ferrite", "ferrite");
        assert!(matches!(same, Cow::Borrowed("ferrite_scene::X")));
    }

    #[test]
    fn fingerprints_use_the_legacy_spelling() {
        let current = format!("{CRATE_PREFIX}render::primitives::PrimitiveShape");
        let legacy = format!("{LEGACY_CRATE_PREFIX}render::primitives::PrimitiveShape");
        assert_eq!(fingerprint_type_path(&current), legacy);
        assert_eq!(fingerprint_type_path("f32"), "f32");
        let to_legacy = |p| rebrand_type_path(p, "ferrite_", "regia_", "ferrite", "regia");
        assert_eq!(
            to_legacy("Option<regia_render::Shape>"),
            "Option<ferrite_render::Shape>"
        );
    }

    #[test]
    fn log_filters_naming_legacy_crates_also_name_the_current_ones() {
        if !BRAND_RENAMED {
            assert_eq!(current_log_target("ferrite_render"), None);
            assert!(matches!(
                current_log_filter("info,ferrite_render=debug"),
                Cow::Borrowed(_)
            ));
            return;
        }
        assert_eq!(
            current_log_target("ferrite_render").as_deref(),
            Some("regia_render")
        );
        assert_eq!(
            current_log_target("ferrite::boot").as_deref(),
            Some("regia::boot")
        );
        assert_eq!(current_log_target("ferrite").as_deref(), Some("regia"));
        assert_eq!(current_log_target("ferrites"), None);
        assert_eq!(current_log_target("regia_render"), None);
        assert_eq!(current_log_target("wgpu_core"), None);
        assert_eq!(
            current_log_filter("info,ferrite_render=debug,wgpu_core=warn"),
            "info,ferrite_render=debug,regia_render=debug,wgpu_core=warn"
        );
        assert_eq!(
            current_log_filter("ferrite[frame]=trace, ferrite::stream=debug"),
            "ferrite[frame]=trace,regia[frame]=trace, ferrite::stream=debug,regia::stream=debug"
        );
        assert!(matches!(
            current_log_filter("info,regia_render=debug"),
            Cow::Borrowed("info,regia_render=debug")
        ));
    }

    #[test]
    fn legacy_theme_names_map_to_the_renamed_builtins() {
        if BRAND_RENAMED {
            assert_eq!(current_theme_name("ferrite-dark").as_deref(), Some("regia-dark"));
            assert_eq!(
                current_theme_name("ferrite-light-high-contrast").as_deref(),
                Some("regia-light-high-contrast")
            );
        } else {
            assert_eq!(current_theme_name("ferrite-dark"), None);
        }
        assert_eq!(current_theme_name("regia-dark"), None);
        assert_eq!(current_theme_name("my-studio-dark"), None);
    }

    #[test]
    fn engine_keys_read_both_spellings_current_first() {
        assert_eq!(LEGACY_KEY_PREFIX, "ferrite");
        if !BRAND_RENAMED {
            assert_eq!(legacy_key("ferrite.seed"), None);
            return;
        }
        assert_eq!(KEY_PREFIX, "regia");
        assert_eq!(legacy_key("regia.seed").as_deref(), Some("ferrite.seed"));
        assert_eq!(
            legacy_key("regia-bake-stamp/1").as_deref(),
            Some("ferrite-bake-stamp/1")
        );
        assert_eq!(legacy_key("regia_render"), None, "crate paths are not keys");
        assert_eq!(legacy_key("regiax.seed"), None);
        assert_eq!(legacy_key("jewelry.render_out/1"), None);
        assert!(key_matches("regia.seed", "regia.seed"));
        assert!(key_matches("ferrite.seed", "regia.seed"));
        assert!(!key_matches("ferrite.spp", "regia.seed"));
        assert!(!key_matches("other.seed", "regia.seed"));

        let old = [("ferrite.seed", "7"), ("ferrite.spp", "16")];
        assert_eq!(find_key(old.iter().copied(), "regia.seed"), Some("7"), "old file");
        let new = [("regia.seed", "8")];
        assert_eq!(find_key(new.iter().copied(), "regia.seed"), Some("8"), "new file");
        let both = [("ferrite.seed", "7"), ("regia.seed", "8")];
        assert_eq!(
            find_key(both.iter().copied(), "regia.seed"),
            Some("8"),
            "the current spelling wins wherever it is"
        );
        assert_eq!(find_key(old.iter().copied(), "regia.device"), None);
    }

    #[test]
    fn lookup_type_path_tries_the_written_path_first() {
        let known = [
            CRATE_PREFIX.to_owned() + "scene::transform::Transform",
            "my_game::Legacy".to_owned(),
        ];
        let get = |p: &str| known.iter().position(|k| k == p);
        assert_eq!(lookup_type_path(&known[0], get), Some(0));
        assert_eq!(lookup_type_path("my_game::Legacy", get), Some(1));
        let legacy = format!("{LEGACY_CRATE_PREFIX}scene::transform::Transform");
        assert_eq!(
            lookup_type_path(&legacy, get),
            Some(0),
            "legacy spelling resolves"
        );
        assert_eq!(lookup_type_path("my_game::Missing", get), None);
    }
}
