// SPDX-License-Identifier: MPL-2.0
//! Standard directories behind the `user://`, `cache://` and `logs://` VFS mounts (specs/11 § 4).

use std::path::{Path, PathBuf};

/// Name of the running executable without extension, used to separate apps' data on desktop.
pub fn exe_stem() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "regia-app".into())
}

/// Desktop `user://`: `<data dir>/RegiaEngine/<exe name>` (`~/.local/share/...` on Linux,
/// `%APPDATA%\...` on Windows). A directory under the name from before the rename is moved to the
/// current name on first use (`regia_core::brand::user_app_dir`, ADR-1388).
#[cfg(not(target_os = "android"))]
pub fn user_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| regia_core::brand::user_app_dir(&d, &exe_stem()))
}

/// Android `user://`: the app's internal data path. `None` until `PlatformPlugin` has been built
/// (it records the activity), or when the OS reports no internal path.
#[cfg(target_os = "android")]
pub fn user_dir() -> Option<PathBuf> {
    crate::platform::internal_data_path()
}

/// Desktop `cache://`: `<cache dir>/RegiaEngine/<exe name>` (`~/.cache/...` on Linux,
/// `%LOCALAPPDATA%\...` on Windows).
#[cfg(not(target_os = "android"))]
pub fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| regia_core::brand::user_app_dir(&d, &exe_stem()))
}

/// Android `cache://`: `Context.getCacheDir()` when the activity answers, `<internal data>/cache`
/// otherwise — the same fallback the default VFS mounts use (audit-g-agent6-platform).
#[cfg(target_os = "android")]
pub fn cache_dir(app: &crate::AndroidApp) -> Option<PathBuf> {
    crate::platform::cache_dir_for(app)
}

/// Desktop local (non-roaming) app data: `<local data dir>/RegiaEngine/<exe name>`
/// (`%LOCALAPPDATA%\...` on Windows, the same directory as [`user_dir`] on Linux and macOS).
/// Machine-specific files that should not travel with a roaming Windows profile live here:
/// crash bundles and minidumps ([`feedback_dir_for`], ADR-525).
#[cfg(not(target_os = "android"))]
pub fn local_data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| regia_core::brand::user_app_dir(&d, &exe_stem()))
}

/// Bytes this process may still write on the volume that holds `path`. `path` need not exist yet:
/// the nearest existing ancestor is asked (`regia_asset`'s content manager checks a chunk's install
/// directory before a transfer, spec 40 § 2.5).
///
/// # Errors
/// The system query fails, or the platform has none ([`std::io::ErrorKind::Unsupported`]).
pub fn free_space(path: &Path) -> std::io::Result<u64> {
    let existing = path
        .ancestors()
        .find(|p| !p.as_os_str().is_empty() && p.exists())
        .unwrap_or(Path::new("."));
    volume_free_space(existing)
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn volume_free_space(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    // The `statvfs` field types differ per platform; all of them widen to u64.
    fn widen(value: impl Into<u64>) -> u64 {
        value.into()
    }
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `c_path` is a NUL-terminated string and `stat` is valid for one write.
    if unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `statvfs` returned 0, so it filled the struct (and it started zeroed).
    let stat = unsafe { stat.assume_init() };
    Ok(widen(stat.f_bavail).saturating_mul(widen(stat.f_frsize)))
}

#[cfg(windows)]
fn volume_free_space(path: &Path) -> std::io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    // SAFETY: `wide` is NUL-terminated, `free` is valid for one write and the other two
    // out-parameters are optional.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(free)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
fn volume_free_space(_path: &Path) -> std::io::Result<u64> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// The default feedback folder (feedback bundles, crash bundles and minidumps) for an app whose
/// `user://` is `user_host`, on `os` (`std::env::consts::OS`).
///
/// It is `<user_host>/feedback`, except on Windows when `user://` is the standard roaming
/// directory (`standard_user`, `user_dir()`): then it is `<local>/feedback` under the
/// non-roaming `%LOCALAPPDATA%` twin (`local`, `local_data_dir()`), so minidumps never sync with a
/// roaming profile (ADR-525). An app or test that mounts its own `user://` keeps its folder there.
#[must_use]
pub fn default_feedback_dir(
    os: &str,
    user_host: Option<&std::path::Path>,
    standard_user: Option<&std::path::Path>,
    local: Option<&std::path::Path>,
) -> Option<PathBuf> {
    let user_host = user_host?;
    let root = match (os, standard_user, local) {
        ("windows", Some(standard), Some(local)) if standard == user_host => local,
        _ => user_host,
    };
    Some(root.join("feedback"))
}

/// [`default_feedback_dir`] for the running platform, with `user_host` the host path of the
/// app's `user://` mount. Android keeps `<user_host>/feedback`.
#[must_use]
pub fn feedback_dir_for(user_host: Option<PathBuf>) -> Option<PathBuf> {
    #[cfg(not(target_os = "android"))]
    {
        default_feedback_dir(
            std::env::consts::OS,
            user_host.as_deref(),
            user_dir().as_deref(),
            local_data_dir().as_deref(),
        )
    }
    #[cfg(target_os = "android")]
    {
        user_host.map(|user| user.join("feedback"))
    }
}

/// The Windows `Saved Games` known folder, where games conventionally keep save files; `None`
/// elsewhere (Linux and Android saves live under [`user_dir`]). Settings stay in [`user_dir`]
/// (`%APPDATA%`); only a game that wants its saves where Windows players look for them asks for
/// this folder.
#[cfg(not(target_os = "android"))]
pub fn saved_games_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        crate::platform::saved_games_dir()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Default directory for session logs on desktop: `logs://`, i.e. `<user dir>/logs`.
#[cfg(not(target_os = "android"))]
pub fn default_log_dir() -> Option<PathBuf> {
    user_dir().map(|d| d.join("logs"))
}

/// Default directory for session logs on Android: `logs://`, i.e. `<internal data path>/logs`.
#[cfg(target_os = "android")]
pub fn default_log_dir(app: &crate::AndroidApp) -> Option<PathBuf> {
    app.internal_data_path().map(|p| p.join("logs"))
}

/// Platform pictures directory for photo stills (ADR-8415).
///
/// Desktop: the OS pictures folder (`dirs::picture_dir`). Android: `<internal data>/pictures`.
/// There is no game-named subfolder. Callers that must not touch the real folder pass their own
/// directory to photo mode instead of calling this.
#[must_use]
pub fn pictures_dir() -> Option<PathBuf> {
    #[cfg(not(target_os = "android"))]
    {
        dirs::picture_dir()
    }
    #[cfg(target_os = "android")]
    {
        user_dir().map(|dir| dir.join("pictures"))
    }
}

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use super::*;

    #[test]
    fn free_space_asks_the_nearest_existing_ancestor() {
        let here = std::env::temp_dir();
        let missing = here.join("regia-no-such-dir").join("deeper");
        assert!(!missing.exists());
        let free = free_space(&missing).expect("free space");
        assert!(free > 0, "the temp volume has room");
        assert!(free_space(Path::new("relative-no-such-dir/x")).is_ok());
    }

    #[test]
    fn user_and_cache_dirs_are_per_app_under_regia() {
        let (Some(user), Some(cache)) = (user_dir(), cache_dir()) else {
            return; // no home directory (minimal CI container)
        };
        // The current name, or the legacy one when an old directory could not be moved.
        let under_brand = |dir: &std::path::Path| {
            [
                regia_core::brand::USER_DIR_NAME,
                regia_core::brand::LEGACY_USER_DIR_NAME,
            ]
            .iter()
            .any(|name| dir.ends_with(std::path::Path::new(name).join(exe_stem())))
        };
        assert!(under_brand(&user), "{}", user.display());
        assert!(under_brand(&cache), "{}", cache.display());
        assert_eq!(default_log_dir(), Some(user.join("logs")));
        if cfg!(windows) {
            // `%APPDATA%` (roaming) for settings, `%LOCALAPPDATA%` for caches.
            assert_ne!(user.parent(), cache.parent());
        }
    }

    #[test]
    fn windows_crash_data_goes_to_local_app_data_only_for_the_standard_user_dir() {
        let roaming = std::path::Path::new(r"C:\Users\p\AppData\Roaming\RegiaEngine\game");
        let local = std::path::Path::new(r"C:\Users\p\AppData\Local\RegiaEngine\game");
        assert_eq!(
            default_feedback_dir("windows", Some(roaming), Some(roaming), Some(local)),
            Some(local.join("feedback")),
            "standard user:// on Windows: the non-roaming twin"
        );
        let custom = std::path::Path::new(r"D:\tmp\test-user");
        assert_eq!(
            default_feedback_dir("windows", Some(custom), Some(roaming), Some(local)),
            Some(custom.join("feedback")),
            "a mounted user:// keeps its own folder"
        );
        assert_eq!(
            default_feedback_dir("windows", Some(roaming), Some(roaming), None),
            Some(roaming.join("feedback")),
            "no local app data: stay under user://"
        );
        let home = std::path::Path::new("/home/p/.local/share/RegiaEngine/game");
        for os in ["linux", "macos", "android"] {
            assert_eq!(
                default_feedback_dir(os, Some(home), Some(home), Some(home)),
                Some(home.join("feedback")),
                "{os} is unchanged"
            );
        }
        assert_eq!(
            default_feedback_dir("windows", None, Some(roaming), Some(local)),
            None
        );
    }

    #[test]
    fn the_feedback_folder_follows_the_mounted_user_dir() {
        let Some(user) = user_dir() else {
            return; // no home directory (minimal CI container)
        };
        let feedback = feedback_dir_for(Some(user.clone()));
        if cfg!(windows) {
            let local = local_data_dir().map(|d| d.join("feedback"));
            assert_eq!(feedback, local, "%LOCALAPPDATA%, not the roaming %APPDATA%");
            assert_ne!(local_data_dir().as_deref(), Some(user.as_path()));
        } else {
            assert_eq!(feedback, Some(user.join("feedback")));
        }
        let custom = std::env::temp_dir().join("regia-feedback-root");
        assert_eq!(
            feedback_dir_for(Some(custom.clone())),
            Some(custom.join("feedback"))
        );
        assert_eq!(feedback_dir_for(None), None);
    }

    #[test]
    fn saved_games_exists_only_on_windows() {
        if cfg!(windows) {
            if let Some(dir) = saved_games_dir() {
                assert!(dir.is_absolute(), "{dir:?}");
            }
        } else {
            assert_eq!(saved_games_dir(), None);
        }
    }

    /// Paths past the classic 260-character `MAX_PATH` still work through `std::fs` (Rust adds the
    /// `\\?\` prefix itself on Windows), which the user directory under a deep profile path needs.
    #[test]
    fn std_fs_handles_paths_over_260_characters() {
        let mut dir = std::env::temp_dir().join(format!("regia-longpath-{}", std::process::id()));
        let root = dir.clone();
        while dir.as_os_str().len() < 300 {
            dir.push("a-rather-long-directory-name-for-the-test");
        }
        let created = std::fs::create_dir_all(&dir);
        let file = dir.join("settings.ron");
        let wrote = created.and_then(|()| std::fs::write(&file, b"ok"));
        let read = wrote.and_then(|()| std::fs::read(&file));
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(read.ok().as_deref(), Some(&b"ok"[..]), "long path round trip");
    }
}
