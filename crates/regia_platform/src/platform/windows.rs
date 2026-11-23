// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/winplat.rs,
// commit d6ed37677, regia-absorb-1), ADR-8012; adapted under ADR-8156.
// Portions copied from Direct Order 27 (shell-regia/game/src/single_instance.rs, commit c9513623f), ADR-8012.
//! Windows services through `windows-sys`.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::UI::Shell::{FOLDERID_SavedGames, SHGetKnownFolderPath, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHOWSOUNDS, SW_SHOWNORMAL};

use crate::services::parse::normalise_language_tag;
use crate::services::{OsInfo, ServiceError};

/// `LOCALE_NAME_MAX_LENGTH` (in UTF-16 units, including the terminator).
const LOCALE_NAME_MAX_LENGTH: usize = 85;

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// A `MEMORYSTATUSEX` ready for `GlobalMemoryStatusEx`: zeroed, with `dwLength` set.
fn empty_memory_status() -> MEMORYSTATUSEX {
    // SAFETY: MEMORYSTATUSEX is plain data; all-zero is a valid value before `dwLength` is set.
    let mut mem: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    mem.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    mem
}

/// The user's `Saved Games` known folder (`%USERPROFILE%\Saved Games`, or wherever the user moved
/// it), through `SHGetKnownFolderPath(FOLDERID_SavedGames)`. `None` if the shell cannot resolve it.
pub(crate) fn saved_games_dir() -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let mut raw: *mut u16 = std::ptr::null_mut();
    // SAFETY: `FOLDERID_SavedGames` is a valid GUID; `raw` receives a CoTaskMem-allocated,
    // NUL-terminated UTF-16 string (also on failure it may be set, so it is always freed).
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_SavedGames, 0, std::ptr::null_mut(), &mut raw) };
    let path = if hr >= 0 && !raw.is_null() {
        // SAFETY: on success `raw` points at a NUL-terminated buffer, copied before it is freed.
        let slice = unsafe { crate::wide::until_nul(raw) };
        Some(std::path::PathBuf::from(std::ffi::OsString::from_wide(slice)))
    } else {
        None
    };
    if !raw.is_null() {
        // SAFETY: `raw` came from `SHGetKnownFolderPath`, which documents `CoTaskMemFree`.
        unsafe { CoTaskMemFree(raw.cast()) };
    }
    path
}

/// `ShellExecuteW(NULL, "open", url, ...)`: the URL goes straight to the shell's protocol handler,
/// with no command interpreter in between.
pub(crate) fn open_url(url: &str) -> Result<(), ServiceError> {
    let verb = wide("open");
    let file = wide(url);
    // SAFETY: `verb` and `file` are NUL-terminated UTF-16 buffers that outlive the call; the other
    // pointer arguments are documented as optional (null).
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Values greater than 32 mean success.
    if result as usize > 32 {
        Ok(())
    } else {
        Err(ServiceError::Os {
            operation: "open_url (ShellExecuteW)",
            message: format!("error code {}", result as usize),
        })
    }
}

/// `GetUserDefaultLocaleName`, already a BCP-47 tag such as `en-US`.
pub(crate) fn system_locale() -> Option<String> {
    let mut buf = [0u16; LOCALE_NAME_MAX_LENGTH];
    // SAFETY: `buf` is writable for `buf.len()` UTF-16 units, the length we pass.
    let len = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 0 {
        return None;
    }
    normalise_language_tag(&from_wide(&buf))
}

/// The Show Sounds accessibility flag: the user wants sounds presented visually.
pub(crate) fn system_captions() -> Option<bool> {
    // SAFETY: GetSystemMetrics reads a system value by index; it takes no pointers.
    Some(unsafe { GetSystemMetrics(SM_SHOWSOUNDS) } != 0)
}

/// Reads a `REG_SZ` value under `HKEY_LOCAL_MACHINE`.
fn hklm_string(subkey: &str, value: &str) -> Option<String> {
    let subkey = wide(subkey);
    let value = wide(value);
    let mut buf = [0u16; 256];
    let mut size = std::mem::size_of_val(&buf) as u32;
    // SAFETY: the key and value names are NUL-terminated UTF-16; `buf` is writable for `size` bytes,
    // and RegGetValueW writes at most that many and updates `size`.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    (status == 0)
        .then(|| from_wide(&buf))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Reads a `REG_DWORD` value under `HKEY_LOCAL_MACHINE`.
fn hklm_dword(subkey: &str, value: &str) -> Option<u32> {
    let subkey = wide(subkey);
    let value = wide(value);
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: as in `hklm_string`; `data` is writable for 4 bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (status == 0).then_some(data)
}

/// The local UTC offset in seconds (east positive), from the time zone's current bias in the
/// registry (`ActiveTimeBias`, minutes west of UTC, daylight saving included; ADR-8586). The bias is
/// the one in force now, so an instant on the other side of a daylight-saving change is off by that
/// change; the editor shows wall times of the current session.
pub(crate) fn local_utc_offset_seconds(_unix_secs: i64) -> Option<i32> {
    let bias = hklm_dword(
        r"SYSTEM\CurrentControlSet\Control\TimeZoneInformation",
        "ActiveTimeBias",
    )?;
    // A REG_DWORD holds the signed bias as its two's-complement bits.
    let minutes = i32::from_ne_bytes(bias.to_ne_bytes());
    (minutes.unsigned_abs() <= 24 * 60).then_some(-minutes * 60)
}

/// The processor name from the registry (`ProcessorNameString` of CPU 0).
pub(crate) fn cpu_model() -> String {
    hklm_string(
        r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
        "ProcessorNameString",
    )
    .unwrap_or_default()
}

/// Registry version keys, `GlobalMemoryStatusEx` and the BIOS product name.
pub(crate) fn os_info() -> OsInfo {
    const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let mut info = OsInfo::unknown();
    let build = hklm_string(CURRENT_VERSION, "CurrentBuildNumber")
        .or_else(|| hklm_string(CURRENT_VERSION, "CurrentBuild"));
    let mut product = hklm_string(CURRENT_VERSION, "ProductName").unwrap_or_else(|| "Windows".into());
    // ProductName still says "Windows 10" on Windows 11; builds from 22000 on are Windows 11.
    if let Some(b) = build.as_deref().and_then(|b| b.parse::<u32>().ok())
        && b >= 22000
    {
        product = product.replacen("Windows 10", "Windows 11", 1);
    }
    let display = hklm_string(CURRENT_VERSION, "DisplayVersion");
    let ubr = hklm_dword(CURRENT_VERSION, "UBR");
    info.version = match (display, build) {
        (Some(d), Some(b)) => format!(
            "{product} {d} (build {b}{})",
            ubr.map(|u| format!(".{u}")).unwrap_or_default()
        ),
        (None, Some(b)) => format!("{product} (build {b})"),
        (Some(d), None) => format!("{product} {d}"),
        (None, None) => product,
    };
    const BIOS: &str = r"HARDWARE\DESCRIPTION\System\BIOS";
    let vendor = hklm_string(BIOS, "SystemManufacturer");
    let model = hklm_string(BIOS, "SystemProductName");
    info.device_model = match (vendor, model) {
        (Some(v), Some(m)) if !m.starts_with(&v) => format!("{v} {m}"),
        (_, Some(m)) => m,
        (Some(v), None) => v,
        (None, None) => String::new(),
    };
    let mut mem = empty_memory_status();
    // SAFETY: `mem` is a valid, writable MEMORYSTATUSEX with `dwLength` set, as the API requires.
    if unsafe { GlobalMemoryStatusEx(&mut mem) } != 0 {
        info.total_ram_bytes = mem.ullTotalPhys;
    }
    info
}

/// Ends the process now with `code`, without running exit handlers or DLL detach (the clean
/// exit's watchdog).
pub(crate) fn exit_immediately(code: i32) -> ! {
    // SAFETY: plain Win32 calls with the current-process pseudo handle; `TerminateProcess` on it
    // does not return when it succeeds.
    unsafe {
        windows_sys::Win32::System::Threading::TerminateProcess(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            code as u32,
        );
    }
    std::process::abort()
}

/// The screen-reader bridge's UI Automation adapter (`accesskit` feature, ADR-1998): AccessKit's
/// subclassing adapter answers `WM_GETOBJECT` on the window. It must subclass the window before the
/// window is first shown, so the runner creates the window hidden and shows it after attaching.
#[cfg(feature = "accesskit-adapters")]
pub(crate) mod accesskit_adapter {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    use crate::accessibility::{AccessKitBridge, BridgeHandler, OsAccessKitAdapter};

    /// The adapter's name in the agent's report.
    pub(crate) const NAME: &str = "uia";
    /// The subclassing adapter refuses (panics on) a window that was already shown.
    pub(crate) const CREATE_HIDDEN: bool = true;

    struct Uia(accesskit_windows::SubclassingAdapter);

    impl OsAccessKitAdapter for Uia {
        fn update(&mut self, factory: Box<dyn FnOnce() -> accesskit::TreeUpdate + '_>) {
            if let Some(events) = self.0.update_if_active(factory) {
                events.raise();
            }
        }

        // The subclassed window procedure follows WM_SETFOCUS / WM_KILLFOCUS itself.
        fn set_window_focused(&mut self, _focused: bool) {}
    }

    /// The UI Automation adapter of `window`; it activates on the first `WM_GETOBJECT`.
    pub(crate) fn create(
        window: &winit::window::Window,
        bridge: &AccessKitBridge,
    ) -> Result<Box<dyn OsAccessKitAdapter>, String> {
        let handle = window
            .window_handle()
            .map_err(|e| format!("no window handle: {e}"))?;
        let RawWindowHandle::Win32(win32) = handle.as_raw() else {
            return Err("the window handle is not a Win32 handle".to_owned());
        };
        let hwnd = win32.hwnd.get() as *mut std::ffi::c_void;
        // SAFETY: `hwnd` is the live window winit just created on this thread; IsWindowVisible only
        // reads its style bits.
        if unsafe { IsWindowVisible(hwnd) } != 0 {
            // The adapter would panic: it must subclass the window before it is first shown.
            return Err("the window is already visible".to_owned());
        }
        let handler = BridgeHandler::new(bridge.clone());
        Ok(Box::new(Uia(accesskit_windows::SubclassingAdapter::new(
            accesskit_windows::HWND(hwnd),
            handler.clone(),
            handler,
        ))))
    }
}

/// The volume serial and nonzero file index of an open Windows file, if available.
///
/// A failed handle query or a zero file index returns `None` (ADR-4821, ADR-4852).
pub fn open_file_identity(file: &std::fs::File) -> Option<(u32, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `info` is writable for the call; the handle is borrowed from `file`, which
    // remains open throughout the call.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), info.as_mut_ptr()) };
    if ok == 0 {
        return None;
    }
    // SAFETY: a nonzero return means the call initialized the complete structure.
    let info = unsafe { info.assume_init() };
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    (index != 0).then_some((info.dwVolumeSerialNumber, index))
}

/// Moves the file `from` to `to` only when `to` does not exist yet: `MoveFileExW` without
/// `MOVEFILE_REPLACE_EXISTING`, so a taken `to` fails with [`std::io::ErrorKind::AlreadyExists`]
/// and both files stay as they were. A reader sees no file or the whole file at `to`.
///
/// This is the Windows form of "publish a new file unless another writer was first" that other
/// systems do with a hard link and an unlink of the temporary name. On Windows, files published
/// that way were refused a replacing rename with access denied for up to 1.3 s after the publish
/// (measured on the a test machine with the on-access scanner running); files published by a move
/// never were (ADR-8504).
///
/// # Errors
///
/// `AlreadyExists` when `to` exists; otherwise the move's OS error (a missing `from`, a
/// different volume, no permission).
pub fn rename_no_replace(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let from = verbatim_wide(from)?;
    let to = verbatim_wide(to)?;
    // SAFETY: both arguments are NUL-terminated UTF-16 buffers that outlive the call; flags 0
    // asks for a plain move that never replaces.
    let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `path` as an absolute, NUL-terminated UTF-16 string, with the `\\?\` (or `\\?\UNC\`) prefix
/// once it is too long for the plain Win32 form, as the standard library's own file calls do.
fn verbatim_wide(path: &std::path::Path) -> std::io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    /// The longest path the plain form takes for a file (`MAX_PATH` less the 8.3 headroom the
    /// directory APIs keep).
    const PLAIN_MAX: usize = 247;
    let absolute = std::path::absolute(path)?;
    let units: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    let prefixed = |unit: &[u16], prefix: &str| unit.starts_with(&prefix.encode_utf16().collect::<Vec<_>>());
    let mut wide = if units.len() <= PLAIN_MAX || prefixed(&units, r"\\?\") || prefixed(&units, r"\\.\") {
        units
    } else if prefixed(&units, r"\\") {
        let mut long: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
        long.extend_from_slice(&units[2..]);
        long
    } else {
        let mut long: Vec<u16> = r"\\?\".encode_utf16().collect();
        long.extend_from_slice(&units);
        long
    };
    if wide.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a path with a NUL character",
        ));
    }
    wide.push(0);
    Ok(wide)
}

/// Installs the menu-free runtime window's bare-Alt subclass (ADR-8156).
#[cfg(has_window)]
pub(crate) fn install_alt_menu(window: &winit::window::Window) -> std::io::Result<()> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle().map_err(std::io::Error::other)?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return Err(std::io::Error::other("expected a Win32 window"));
    };
    let hwnd = handle.hwnd.get() as *mut std::ffi::c_void;
    // SAFETY: borrowed live window handle, kept alive by `window` for this call.
    // The helper checks thread ownership before subclassing.
    unsafe { alt_menu_native::install(hwnd) }
}

#[cfg(has_window)]
#[path = "windows/alt_menu.rs"]
mod alt_menu_native;

#[cfg(test)]
mod miri_tests {
    /// Miri checks that all zeroes is a valid value of the type. `docs/06-testing-and-ci.md` § 14.
    #[test]
    fn the_empty_memory_status_is_a_valid_value() {
        let mem = super::empty_memory_status();
        assert_eq!(mem.dwLength as usize, size_of_val(&mem));
        assert_eq!(mem.ullTotalPhys, 0);
    }
}

#[cfg(test)]
mod rename_no_replace_tests {
    use super::rename_no_replace;
    use std::path::Path;

    fn read(path: &Path) -> Vec<u8> {
        std::fs::read(path).expect("read")
    }

    /// A free name takes the file (the source is gone); a taken name is refused with
    /// `AlreadyExists` and neither file changes.
    #[test]
    fn a_free_name_takes_the_file_and_a_taken_one_is_refused() {
        let dir = regia_test_utils::unique_temp_dir("platform-rename-no-replace");
        let (a, b, c) = (dir.join("a.tmp"), dir.join("b.meta"), dir.join("c.tmp"));
        std::fs::write(&a, b"first").expect("write a");
        rename_no_replace(&a, &b).expect("free name");
        assert!(!a.exists(), "the source moved");
        assert_eq!(read(&b), b"first");
        std::fs::write(&c, b"second").expect("write c");
        let err = rename_no_replace(&c, &b).expect_err("taken name");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(read(&b), b"first", "the taken name keeps its file");
        assert_eq!(read(&c), b"second", "the refused source stays");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Paths past `MAX_PATH` go through the verbatim form.
    #[test]
    fn a_long_path_moves() {
        let dir = regia_test_utils::unique_temp_dir("platform-rename-no-replace-long");
        let deep = (0..12).fold(dir.clone(), |p, i| {
            p.join(format!("folder-number-{i:02}-xxxxxxxx"))
        });
        std::fs::create_dir_all(&deep).expect("deep dirs");
        let (from, to) = (deep.join("source.tmp"), deep.join("published.meta"));
        assert!(to.as_os_str().len() > 260, "{}", to.display());
        std::fs::write(&from, b"long").expect("write");
        rename_no_replace(&from, &to).expect("long move");
        assert_eq!(read(&to), b"long");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ADR-8504: a file published while another handle still has its temporary name open (a
    /// reader of the just-written file, here a handle the test holds with read, write and delete
    /// sharing) can be replaced at once. The hard-link publish this replaces is run as a printed
    /// control (`--nocapture`), not asserted: with a user-mode handle like this one it was not
    /// refused on the a test machine; the refusals came from a kernel-level open (the scanner) and
    /// were measured with a separate replay (ADR-8504).
    #[test]
    fn a_published_file_is_replaceable_while_its_temp_is_still_open() {
        let dir = regia_test_utils::unique_temp_dir("platform-rename-no-replace-held");

        // The fix: move the temporary name into place.
        let (tmp, meta, next) = (
            dir.join("x.meta.1.tmp"),
            dir.join("x.meta"),
            dir.join("x.meta.2.tmp"),
        );
        std::fs::write(&tmp, b"created").expect("write tmp");
        let holder = std::fs::File::open(&tmp).expect("hold the temp open");
        rename_no_replace(&tmp, &meta).expect("publish");
        std::fs::write(&next, b"replaced").expect("write next");
        let replaced = std::fs::rename(&next, &meta);
        drop(holder);
        replaced.expect("the published file is replaceable while the old temp handle is open");
        assert_eq!(read(&meta), b"replaced");

        // The control: the hard-link publish.
        let (tmp, meta, next) = (
            dir.join("y.meta.1.tmp"),
            dir.join("y.meta"),
            dir.join("y.meta.2.tmp"),
        );
        std::fs::write(&tmp, b"created").expect("write tmp");
        let holder = std::fs::File::open(&tmp).expect("hold the temp open");
        std::fs::hard_link(&tmp, &meta).expect("link");
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(&next, b"replaced").expect("write next");
        let control = std::fs::rename(&next, &meta);
        drop(holder);
        eprintln!("control (hard link + unlink, temp held): replacing rename -> {control:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// The per-user directory of the single-instance lock (ADR-8833):
/// `%LOCALAPPDATA%\RegiaEngine\instance`, else the per-user temp dir.
pub(crate) fn instance_lock_dir() -> Option<(std::path::PathBuf, String)> {
    match std::env::var_os("LOCALAPPDATA").filter(|d| !d.is_empty()) {
        Some(local) => Some((
            std::path::PathBuf::from(local)
                .join(regia_core::brand::USER_DIR_NAME)
                .join("instance"),
            String::new(),
        )),
        None => Some((std::env::temp_dir(), "regia-".to_owned())),
    }
}

/// Whether process `pid` is alive: `OpenProcess` and `GetExitCodeProcess` (`STILL_ACTIVE`).
pub(crate) fn process_alive(pid: u32) -> Option<bool> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: plain Win32 calls; the handle is checked for null and closed once.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // Another user's process refuses the query: it exists.
            return Some(GetLastError() == ERROR_ACCESS_DENIED);
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        Some(code == STILL_ACTIVE as u32)
    }
}

/// Brings the running instance's first visible window forward (restored when minimized). Best
/// effort: Windows may only flash its taskbar button (foreground lock).
pub(crate) fn raise_instance_window(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, EnumWindows, GetWindowThreadProcessId, IsWindowVisible, SW_RESTORE,
        SetForegroundWindow, ShowWindow,
    };
    struct Ctx {
        pid: u32,
        raised: bool,
    }
    // SAFETY: contract of this callback: only EnumWindows below calls it, synchronously, with
    // `param` set to the address of the caller's live `Ctx` and `hwnd` a window it enumerates.
    unsafe extern "system" fn each(hwnd: HWND, param: LPARAM) -> i32 {
        // SAFETY: `param` is the caller's live `Ctx` (EnumWindows calls back synchronously);
        // `hwnd` comes from EnumWindows.
        unsafe {
            let ctx = &mut *(param as *mut Ctx);
            let mut owner = 0u32;
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner != ctx.pid || IsWindowVisible(hwnd) == 0 {
                return 1;
            }
            ShowWindow(hwnd, SW_RESTORE);
            AllowSetForegroundWindow(ctx.pid);
            ctx.raised = SetForegroundWindow(hwnd) != 0;
            0
        }
    }
    if pid == 0 || pid == std::process::id() {
        return false;
    }
    let mut ctx = Ctx { pid, raised: false };
    // SAFETY: the callback only touches `ctx`, which outlives the synchronous enumeration.
    unsafe {
        EnumWindows(Some(each), &mut ctx as *mut Ctx as LPARAM);
    }
    ctx.raised
}

/// Takes `window` off screen now (the fast exit, ADR-8833): `ShowWindow(SW_HIDE)` through winit,
/// on the window's own thread.
#[cfg(has_window)]
pub(crate) fn hide_window_now(window: &winit::window::Window) -> &'static str {
    window.set_visible(false);
    "win32: hidden"
}
