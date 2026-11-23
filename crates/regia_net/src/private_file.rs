// SPDX-License-Identifier: MPL-2.0
//! Files that must stay private to their owner on every platform: the server's identity key,
//! the known-hosts store, the agent endpoint token and other `.regia/agent/` secrets, the
//! Android signing file with a literal secret and the content-signing private key. A local
//! account that can read the key can complete the Noise handshake as the server; one that can
//! rewrite the store can steer whom a client trusts.
//!
//! On Unix the file is created mode 0600 and any group/other mode bit on an existing file is
//! refused. On Windows the file is created with a DACL protected from inheritance that grants
//! the process token's user, `SYSTEM` and `Administrators` only, and a DACL that grants any
//! principal other than those and the file's owner access is refused — the same class of error
//! as a group/other mode bit on Unix (ADR-4828, hardened per audit fixrev-15-k net2-platform
//! H2/H3/M4/M5; ADR-5880). The DACL is in the `CreateFileW` call itself and the handle is opened
//! with no sharing, so no other account can open the file between its creation and the secret
//! being written (ADR-5616: this one helper replaces the CLI's `icacls` copy and the agent's
//! Unix-only copy).
//!
//! Provenance: docs/provenance.md.

use std::path::{Path, PathBuf};

/// Creates `path` for writing; the file must not exist yet and is readable only by its owner.
///
/// Unix: `open(O_CREAT | O_EXCL)` with mode `0600` (the umask may clear bits, never add them).
/// Windows: `CreateFileW(CREATE_NEW)` with a protected DACL granting the process token's user
/// SID, `SYSTEM` and `Administrators` full control and no one else (the folder's inheritable
/// entries never apply), opened write-only with no sharing. The account is read from the token,
/// never from `USERNAME`, so a LocalSystem service gets the `SYSTEM` entry. A relative or long
/// path is made absolute and verbatim (`\\?\`) first. Other hosts create the file with their
/// default permissions.
///
/// # Errors
///
/// The OS error of creating the file (`AlreadyExists` when it exists), or, on Windows, an
/// `Other` error naming the security call that failed while the DACL was being built.
pub fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    imp::create_private(path)
}

/// True when `path` grants other local accounts any access: on Unix, any group or other mode
/// bit in `meta`; on Windows, any access-allowed ACE for a principal other than the file's
/// owner, the current user, `SYSTEM` and `Administrators` (a null DACL counts). A failed check
/// is an error, not `false`: a check that could not run must not quietly allow a secret to be
/// used.
///
/// # Errors
///
/// Windows only: the file's security descriptor could not be read, or its DACL holds an entry
/// the check cannot judge (a conditional ACE, or one too small for its SID).
pub fn exposed_to_other_users(path: &Path, meta: &std::fs::Metadata) -> Result<bool, String> {
    imp::exposed_to_other_users(path, meta)
}

/// The principals an owner-only DACL grants, in ACE order: `Administrators`, `SYSTEM`, then the
/// token user unless it is one of those two (a LocalSystem service's token user is `SYSTEM`, so
/// its DACL names `SYSTEM` once). SIDs are compared byte for byte, which is how `EqualSid`
/// compares well-formed SIDs. Pure, so every host tests the grant set.
#[cfg(any(windows, test))]
fn owner_only_grants<'a>(user: &'a [u8], system: &'a [u8], admins: &'a [u8]) -> Vec<&'a [u8]> {
    let mut grants = vec![admins, system];
    if !grants.contains(&user) {
        grants.push(user);
    }
    grants
}

#[cfg(test)]
mod grant_tests {
    use super::owner_only_grants;

    /// S-1-5-18 (`SYSTEM`) in its binary form: revision 1, one sub-authority, NT authority 5.
    const SYSTEM: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
    /// S-1-5-32-544 (`BUILTIN\Administrators`).
    const ADMINS: [u8; 16] = [1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 0x20, 2, 0, 0];
    /// S-1-5-21-1-2-3-1001, an ordinary local account.
    const USER: [u8; 28] = [
        1, 5, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 0xe9, 3, 0, 0,
    ];

    /// Review sprint-rev-v05-windows-close finding 2: the grant comes from the process token's
    /// SID. An ordinary account gets its own entry next to `SYSTEM` and `Administrators`.
    #[test]
    fn owner_only_acl_grants_the_token_user_system_and_administrators() {
        let grants = owner_only_grants(&USER, &SYSTEM, &ADMINS);
        assert_eq!(grants, [&ADMINS[..], &SYSTEM[..], &USER[..]]);
    }

    /// Under LocalSystem (a CI agent service) the token user is `SYSTEM` itself: the DACL grants
    /// the account the process runs as (no machine-account name such as `WORKGROUP\HOST$` that
    /// does not map), and names it once.
    #[test]
    fn owner_only_acl_grants_the_token_sid_under_local_system() {
        let grants = owner_only_grants(&SYSTEM, &SYSTEM, &ADMINS);
        assert_eq!(grants, [&ADMINS[..], &SYSTEM[..]]);
    }
}

/// A unique temporary name in `path`'s directory, deliberately short so a long destination
/// plus the suffix stays under the path length limit (audit G net2-platform L3); the writer
/// renames it onto `path`. `unique` distinguishes concurrent writes in one process.
pub(crate) fn temp_path(path: &Path, unique: &str) -> PathBuf {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    dir.join(format!(".{unique}.tmp"))
}

/// Creates a fresh private temporary sibling of `path` ([`temp_path`], made verbatim like
/// [`verbatim`]) for an atomic write, and returns its path and the open file. The name carries
/// the process id, a per-process counter and a few random bytes, and a name that already exists
/// is skipped: a temporary that a crashed earlier run left behind (a restart with the same PID,
/// PID 1 in a container) no longer blocks every later write (corr-low, ADR-5789).
///
/// # Errors
///
/// As [`create_private`]; `AlreadyExists` only when every one of a few fresh names exists.
pub(crate) fn create_private_temp(path: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut last = None;
    for _ in 0..8 {
        let salt = crate::crypto::os_random::<4>().map_or(0, u32::from_le_bytes);
        let unique = format!(
            "{}-{}-{salt:08x}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let tmp = verbatim(&temp_path(path, &unique))?;
        match create_private(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::ErrorKind::AlreadyExists.into()))
}

/// `path` as an absolute path, verbatim (`\\?\`) on Windows so the 260-character `MAX_PATH`
/// limit does not reject a legal long file name or its temporary sibling.
pub(crate) fn verbatim(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
        // Device paths and paths that already carry the prefix are left alone.
        if has_device_prefix(&wide) {
            return Ok(absolute);
        }
        let out: Vec<u16> = if wide.starts_with(&[b'\\' as u16, b'\\' as u16]) {
            // UNC: \\server\share -> \\?\UNC\server\share.
            let mut v: Vec<u16> = "\\\\?\\UNC\\".encode_utf16().collect();
            v.extend_from_slice(&wide[2..]);
            v
        } else {
            let mut v: Vec<u16> = "\\\\?\\".encode_utf16().collect();
            v.extend(
                wide.iter()
                    .map(|c| if *c == b'/' as u16 { b'\\' as u16 } else { *c }),
            );
            v
        };
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&out)))
    }
    #[cfg(not(windows))]
    Ok(absolute)
}

/// Whether the UTF-16 path already has a verbatim or device prefix.
#[cfg(any(windows, test))]
fn has_device_prefix(wide: &[u16]) -> bool {
    wide.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16])
        || wide.starts_with(&[b'\\' as u16, b'\\' as u16, b'.' as u16, b'\\' as u16])
}

/// fixrev2-v048c-merge-audit's object-ACE cases (ADR-5673), run against the one DACL walk that
/// both Windows checks share since ADR-5880 (`acl_scan::exposes`): its bounded `Flags` read
/// replaced `private_file`'s own `object_ace_sid_offset` in the integ-v049 merge.
#[cfg(test)]
mod object_ace_tests {
    use crate::acl_scan::{ACCESS_ALLOWED_OBJECT, exposes};

    /// `FILE_GENERIC_READ`.
    const READ: u32 = 0x0012_0089;
    /// S-1-5-32-545 (`BUILTIN\Users`).
    const USERS: [u8; 16] = [1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 0x21, 2, 0, 0];
    /// S-1-5-21-1-2-3-1001, the file's owner.
    const ME: [u8; 28] = [
        1, 5, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 0xe9, 3, 0, 0,
    ];

    /// An ACL holding exactly the bytes `ace` (its `AceSize` is whatever `ace` says).
    fn acl_of(ace: &[u8]) -> Vec<u8> {
        let size = (8 + ace.len()) as u16;
        let mut out = vec![2, 0];
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(ace);
        out
    }

    /// An object allow ACE granting read with `flags`, `filler` bytes (standing in for GUIDs)
    /// after the flags word, then `sid`.
    fn object_ace(flags: u32, filler: usize, sid: &[u8]) -> Vec<u8> {
        let size = (12 + filler + sid.len()) as u16;
        let mut out = vec![ACCESS_ALLOWED_OBJECT, 0];
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&READ.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend(std::iter::repeat_n(0xAB, filler));
        out.extend_from_slice(sid);
        out
    }

    /// fixrev2-v048c-merge-audit: the SID follows only the GUIDs the flags announce. integ-v048
    /// started from `size_of::<ACCESS_ALLOWED_OBJECT_ACE>() - 4` (44, already past both GUIDs)
    /// and added the announced GUIDs again, through a reference to a struct larger than an ACE
    /// that carries no GUID. A SID read at any other offset lands in the 0xAB filler (a
    /// sub-authority count of 171 overruns the ACE, an error), so `Ok` pins the offset.
    #[test]
    fn the_sid_follows_the_guids_the_flags_announce() {
        let trusted: [&[u8]; 1] = [&ME];
        // Other flag bits (0xffff_fff0) announce nothing.
        for (flags, sid_at) in [(0u32, 12usize), (1, 28), (2, 28), (3, 44), (0xffff_fff0, 12)] {
            let users = acl_of(&object_ace(flags, sid_at - 12, &USERS));
            assert_eq!(
                exposes(Some(&users), &trusted, u32::MAX),
                Ok(true),
                "flags {flags:#x}: Users read at {sid_at}"
            );
            let mine = acl_of(&object_ace(flags, sid_at - 12, &ME));
            assert_eq!(
                exposes(Some(&mine), &trusted, u32::MAX),
                Ok(false),
                "flags {flags:#x}: owner read at {sid_at}"
            );
        }
    }

    /// An ACE too short for its flags word is refused, not skipped (fail-closed).
    #[test]
    fn an_object_ace_too_short_for_its_flags_is_an_error() {
        let trusted: [&[u8]; 1] = [&ME];
        for len in [0usize, 4, 8, 11] {
            let mut ace = vec![0u8; len];
            if len >= 4 {
                ace[0] = ACCESS_ALLOWED_OBJECT;
                ace[2..4].copy_from_slice(&(len as u16).to_le_bytes());
            }
            if len >= 8 {
                ace[4..8].copy_from_slice(&READ.to_le_bytes());
            }
            assert!(
                exposes(Some(&acl_of(&ace)), &trusted, u32::MAX).is_err(),
                "{len} bytes"
            );
        }
        // Twelve bytes hold the flags word; with no granted bit the ACE is judged, not refused.
        let mut flags_only = vec![0u8; 12];
        flags_only[0] = ACCESS_ALLOWED_OBJECT;
        flags_only[2] = 12;
        assert!(exposes(Some(&acl_of(&flags_only)), &trusted, u32::MAX).is_ok());
    }
}

#[cfg(test)]
mod prefix_tests {
    use super::has_device_prefix;

    #[cfg(windows)]
    #[test]
    fn verbatim_preserves_already_prefixed_windows_paths() {
        for text in [
            r"\\?\C:\regia\server.key",
            r"\\?\UNC\server\share\server.key",
            r"\\.\pipe\regia",
        ] {
            let path = std::path::Path::new(text);
            assert_eq!(super::verbatim(path).expect("absolute device path"), path);
        }
    }

    #[test]
    fn accepts_the_verbatim_prefix_emitted_by_the_formatter() {
        assert!(has_device_prefix(
            &r"\\?\C:\server\server.key".encode_utf16().collect::<Vec<_>>()
        ));
    }

    #[test]
    fn accepts_device_paths_without_treating_normal_unc_paths_as_devices() {
        assert!(has_device_prefix(
            &r"\\.\pipe\regia".encode_utf16().collect::<Vec<_>>()
        ));
        for normal in [r"\\server\share\server.key", r"C:\server\server.key"] {
            assert!(!has_device_prefix(&normal.encode_utf16().collect::<Vec<_>>()));
        }
    }
}

/// Windows-only test backdoor: grants a well-known SID an access mask on `path`'s DACL,
/// standing in for another local account loosening the file so `exposed_to_other_users` can
/// be seen refusing it.
#[cfg(all(windows, test))]
pub(crate) use imp::grant_well_known_for_test;

/// Re-exported from the Windows implementation: an owner-only security descriptor for call
/// sites that create a kernel object other than a file — the agent protocol's named pipe
/// passes these attributes to `CreateNamedPipeW` (`regia_agent`'s local transport, spec 23 § 2).
#[cfg(windows)]
pub use imp::{OwnerOnlyDescriptor, owner_only_descriptor};

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    pub(super) fn create_private(path: &Path) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }

    pub(super) fn exposed_to_other_users(_path: &Path, meta: &std::fs::Metadata) -> Result<bool, String> {
        use std::os::unix::fs::PermissionsExt;
        Ok(meta.permissions().mode() & 0o077 != 0)
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::io;
    use std::path::Path;

    pub(super) fn create_private(path: &Path) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    }

    pub(super) fn exposed_to_other_users(_path: &Path, _meta: &std::fs::Metadata) -> Result<bool, String> {
        Ok(false)
    }
}

#[cfg(windows)]
mod imp {
    // Windows DACL calls through windows-sys (ADR-4828, docs/04-conventions.md § 5).
    #![allow(unsafe_code)]

    use std::io;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_SUCCESS, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, CreateWellKnownSid,
        DACL_SECURITY_INFORMATION, GetLengthSid, GetTokenInformation, InitializeAcl,
        InitializeSecurityDescriptor, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
        SECURITY_DESCRIPTOR, SetSecurityDescriptorControl, SetSecurityDescriptorDacl, TOKEN_QUERY,
        TOKEN_USER, TokenUser, WELL_KNOWN_SID_TYPE, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    #[cfg(test)]
    use windows_sys::Win32::Security::{ACE_HEADER, EqualSid, GetAce};
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
    };
    #[cfg(test)]
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
    use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Maximum SID length; `CreateWellKnownSid` and the token query both use fixed buffers.
    const SID_BUFFER_BYTES: usize = 0x44;

    // `acl_scan` reads the object ACE layout from its bytes: the flag values and the full struct
    // (header, mask, flags, two GUIDs, `SidStart`) it assumes (fixrev2-v048c, ADR-5673).
    const _: () = assert!(
        windows_sys::Win32::Security::ACE_OBJECT_TYPE_PRESENT == 1
            && windows_sys::Win32::Security::ACE_INHERITED_OBJECT_TYPE_PRESENT == 2
    );
    const _: () =
        assert!(size_of::<windows_sys::Win32::Security::ACCESS_ALLOWED_OBJECT_ACE>() == 12 + 2 * 16 + 4);

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

    fn last_error(context: &str) -> String {
        format!("{context}: {}", std::io::Error::last_os_error())
    }

    /// The address of a SID held in a byte buffer, as the Win32 APIs want it.
    fn sid_ptr(buf: &[u8]) -> *mut core::ffi::c_void {
        buf.as_ptr().cast::<core::ffi::c_void>().cast_mut()
    }

    /// A byte buffer whose start is 8-byte aligned. The Win32 security structs
    /// (`SECURITY_DESCRIPTOR`, `ACL`, `TOKEN_USER`) carry pointers or multi-byte fields and are
    /// cast from these buffers; a plain `Vec<u8>` guarantees only 1-byte alignment
    /// (audit fixrev-15-k net2-platform L9).
    struct AlignedBuf {
        words: Vec<u64>,
    }

    impl AlignedBuf {
        fn new(bytes: usize) -> Self {
            Self {
                words: vec![0u64; bytes.div_ceil(8)],
            }
        }

        fn len(&self) -> usize {
            self.words.len() * 8
        }

        fn as_mut_ptr(&mut self) -> *mut u8 {
            self.words.as_mut_ptr().cast()
        }

        /// The buffer start as a raw pointer (the Win32 security calls take `*mut` even where
        /// they only read).
        fn as_ptr(&self) -> *mut u8 {
            self.words.as_ptr().cast_mut().cast()
        }

        /// The buffer as initialized bytes (`len()` covers the whole allocation).
        fn bytes(&self) -> &[u8] {
            // SAFETY: `words` is 8-byte aligned, live and untouched while the slice is; the
            // length covers exactly the allocation.
            unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), self.len()) }
        }
    }

    /// A freshly allocated SID for `kind`. A SID the OS cannot build is an error: the checks
    /// that compare against it must not silently pass (audit fixrev-15-k net2-platform M4).
    fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Result<Vec<u8>, String> {
        let mut buf = AlignedBuf::new(SID_BUFFER_BYTES);
        let mut len = buf.len() as u32;
        // SAFETY: `buf` is `SID_BUFFER_BYTES` long, the documented maximum SID size, and
        // `len` starts at its length; the API writes the SID and its length.
        let ok = unsafe { CreateWellKnownSid(kind, ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len) };
        if ok == 0 {
            return Err(last_error("CreateWellKnownSid"));
        }
        Ok(buf.bytes()[..len as usize].to_vec())
    }

    /// The current user's SID, copied out of their process token.
    fn current_user_sid() -> Result<Vec<u8>, String> {
        let mut token = ptr::null_mut();
        // SAFETY: `token` is a valid out-pointer for the handle the API writes.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if ok == 0 {
            return Err(last_error("OpenProcessToken"));
        }
        // TOKEN_USER holds one SID_AND_ATTRIBUTES followed by the SID inside the same
        // buffer; 256 bytes covers the header plus the maximum SID length with slack. The
        // buffer is 8-byte aligned for the struct's pointer field (audit L9).
        let mut info = AlignedBuf::new(256);
        let mut returned = 0u32;
        // SAFETY: `info` is 256 bytes and `returned` a valid out-pointer; the API writes at
        // most `info.len()` bytes and the length it used.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                info.as_mut_ptr().cast(),
                info.len() as u32,
                &mut returned,
            )
        };
        let result = if ok == 0 {
            Err(last_error("GetTokenInformation(TokenUser)"))
        } else {
            // SAFETY: the call succeeded, so `info` holds a TOKEN_USER whose SID points into
            // the same allocation and is valid for the length `GetLengthSid` reports.
            let user = unsafe { &*(info.bytes().as_ptr() as *const TOKEN_USER) };
            // SAFETY: `user.User.Sid` is a valid SID inside `info`.
            let len = unsafe { GetLengthSid(user.User.Sid) } as usize;
            let sid = vec![0u8; len];
            // SAFETY: `sid` is `len` bytes, the exact SID length, and both pointers are valid.
            let copied =
                unsafe { windows_sys::Win32::Security::CopySid(len as u32, sid_ptr(&sid), user.User.Sid) };
            if copied == 0 {
                Err(last_error("CopySid"))
            } else {
                Ok(sid)
            }
        };
        // SAFETY: `token` is a handle this function opened and owns.
        unsafe { CloseHandle(token) };
        result
    }

    pub(super) fn create_private(path: &Path) -> io::Result<std::fs::File> {
        // Relative and long paths: `CreateFileW` has no `MAX_PATH` lift of its own (std's opens
        // do this conversion internally). An already verbatim path is returned unchanged.
        let path = super::verbatim(path)?;
        create_private_inner(&path)
    }

    /// A security descriptor that grants access to no one but the current user, `SYSTEM` and
    /// `Administrators`, protected from inheritance (ADR-4828). `create_private` builds a
    /// file itself; this exists for call sites that create a different kernel object with the
    /// same need — the agent protocol's named pipe passes these attributes to
    /// `CreateNamedPipeW` (`regia_agent`'s local transport, spec 23 § 2).
    pub struct OwnerOnlyDescriptor {
        sd: AlignedBuf,
        _acl: AlignedBuf,
    }

    impl OwnerOnlyDescriptor {
        /// `SECURITY_ATTRIBUTES` naming this descriptor, for a one-shot creation call made
        /// while `self` is alive (the descriptor must outlive the call that consumes the
        /// attributes). The handle is not inheritable.
        pub fn security_attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                // The Win32 call reads the descriptor; the pointer is not used to mutate it.
                lpSecurityDescriptor: self.sd.as_ptr().cast(),
                bInheritHandle: 0,
            }
        }
    }

    /// Builds the owner-only security descriptor: `Administrators`, `SYSTEM`, then the process
    /// token's user unless it is one of those two (`owner_only_grants`), each with full access,
    /// in a DACL protected from inheritance so a parent folder's inheritable ACEs can never
    /// merge into it.
    ///
    /// # Errors
    ///
    /// The name of the security call that failed while the descriptor was being built.
    pub fn owner_only_descriptor() -> Result<OwnerOnlyDescriptor, String> {
        let user = current_user_sid()?;
        let admins = well_known_sid(WinBuiltinAdministratorsSid)?;
        let system = well_known_sid(WinLocalSystemSid)?;
        let sids = super::owner_only_grants(&user, &system, &admins);

        let acl_size = size_of::<ACL>()
            + sids
                .iter()
                .map(|sid| size_of::<ACCESS_ALLOWED_ACE>() - 4 + sid.len())
                .sum::<usize>()
            + 16; // alignment slack
        let mut acl = AlignedBuf::new(acl_size);
        let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
        // SAFETY: `acl` is 4-byte aligned (8 in fact) and large enough for the header plus one
        // ACE per SID.
        if unsafe { InitializeAcl(acl_ptr, acl.len() as u32, ACL_REVISION) } == 0 {
            return Err(last_error("InitializeAcl"));
        }
        for sid in &sids {
            // SAFETY: `acl_ptr` is initialized and `sid` a valid SID of the right length.
            if unsafe { AddAccessAllowedAce(acl_ptr, ACL_REVISION, FILE_ALL_ACCESS, sid_ptr(sid)) } == 0 {
                return Err(last_error("AddAccessAllowedAce"));
            }
        }

        let mut sd = AlignedBuf::new(size_of::<SECURITY_DESCRIPTOR>());
        let sd_ptr = sd.as_mut_ptr().cast::<SECURITY_DESCRIPTOR>().cast();
        // SAFETY: `sd` is the size of SECURITY_DESCRIPTOR, which the API initializes.
        if unsafe { InitializeSecurityDescriptor(sd_ptr, SECURITY_DESCRIPTOR_REVISION) } == 0 {
            return Err(last_error("InitializeSecurityDescriptor"));
        }
        // SAFETY: `sd_ptr` was initialized above and `acl_ptr` holds the DACL.
        if unsafe { SetSecurityDescriptorDacl(sd_ptr, 1, acl_ptr, 0) } == 0 {
            return Err(last_error("SetSecurityDescriptorDacl"));
        }
        // Protected, so the folder's inheritable entries (`BUILTIN\Users`, `Authenticated Users` on
        // a typical `C:\` tree) are never merged into this DACL, now or when the folder's ACL
        // later changes (ADR-5489).
        // SAFETY: `sd_ptr` was initialized above; the call only sets one control bit.
        if unsafe { SetSecurityDescriptorControl(sd_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED) } == 0 {
            return Err(last_error("SetSecurityDescriptorControl"));
        }

        Ok(OwnerOnlyDescriptor { sd, _acl: acl })
    }

    /// `CreateFileW` with a DACL that names no one but the current user, `SYSTEM` and
    /// `Administrators`; a non-null DACL that names no other principal denies everyone else.
    /// The DACL is protected from inheritance, so a parent directory's inheritable ACEs (the
    /// `C:\ProgramData` case) cannot merge into it and reopen the file to other accounts or
    /// make the next start refuse its own key (audit fixrev-15-k net2-platform H2).
    fn create_private_inner(path: &Path) -> io::Result<std::fs::File> {
        let desc = owner_only_descriptor().map_err(io::Error::other)?;
        let attrs = desc.security_attributes();
        let name = wide(path);
        // SAFETY: `name` is NUL-terminated and `attrs` a valid SECURITY_ATTRIBUTES with a
        // valid absolute security descriptor; the returned handle is checked below.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                0, // exclusive while the rename lands
                &attrs,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // Keep the OS error's kind: callers tell an existing file (`AlreadyExists`) apart.
            let os = io::Error::last_os_error();
            return Err(io::Error::new(
                os.kind(),
                format!("CreateFileW({}): {os}", path.display()),
            ));
        }
        // SAFETY: `handle` is a live file handle this function owns and does not use again;
        // the File takes over and closes it.
        Ok(unsafe { std::fs::File::from_raw_handle(handle) })
    }

    pub(super) fn exposed_to_other_users(path: &Path, _meta: &std::fs::Metadata) -> Result<bool, String> {
        // Verbatim, as the writer opened it: a 260+ character path fails the plain call.
        let long = super::verbatim(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let name = wide(&long);
        let mut owner = ptr::null_mut();
        let mut dacl = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        // SAFETY: `owner`, `dacl` and `descriptor` are valid out-pointers; `descriptor` owns
        // both the owner SID and the DACL until `LocalFree` below.
        let error = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if error != ERROR_SUCCESS {
            return Err(format!(
                "{}: reading the file's security descriptor failed: {error}",
                path.display()
            ));
        }
        let owner = sid_bytes(owner);
        // The descriptor owns the DACL; copy nothing out before it is freed.
        let exposed = dacl_exposes(dacl, &owner);
        // SAFETY: `descriptor` was returned by GetNamedSecurityInfoW and must be freed with
        // LocalFree; the DACL and owner copy are not used after this point.
        unsafe { LocalFree(descriptor.cast()) };
        exposed
    }

    /// A copy of the SID at `sid` (an empty vector when `sid` is null or malformed).
    fn sid_bytes(sid: *const core::ffi::c_void) -> Vec<u8> {
        if sid.is_null() {
            return Vec::new();
        }
        // SAFETY: `sid` points at a SID owned by the security descriptor.
        let len = unsafe { GetLengthSid(sid.cast_mut()) } as usize;
        if len < 8 {
            return Vec::new();
        }
        // SAFETY: `sid` is valid for `len` bytes, the length `GetLengthSid` just reported.
        unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), len) }.to_vec()
    }

    /// Whether a DACL grants any access to a principal other than the file's owner, the
    /// current user, `SYSTEM` and `Administrators` — the set `create_private_inner` grants,
    /// plus the owner. The current user is trusted even when another principal owns the file:
    /// an elevated administrator's fresh key may be owned by `Administrators` (the
    /// server-SKU default owner policy) while its DACL names the user who created it, and the
    /// next start must not refuse it (audit rev-kimi-17 fixrev-15-k finding 1). The ACE walk is
    /// `acl_scan::exposes` (ADR-5880): a null DACL is exposed; object ACEs are read after their
    /// GUIDs; callback, compound and unknown allow ACEs are an error; a check that cannot run
    /// is an error, not a pass (audit fixrev-15-k net2-platform H3/M4/M5, rev-kimi-17
    /// fixrev-15-k finding 6).
    fn dacl_exposes(dacl: *const ACL, owner: &[u8]) -> Result<bool, String> {
        if dacl.is_null() {
            return Ok(true);
        }
        // The allowlist; a SID the OS cannot build fails the check.
        let system = well_known_sid(WinLocalSystemSid)?;
        let admins = well_known_sid(WinBuiltinAdministratorsSid)?;
        let me = current_user_sid()?;
        // SAFETY: `dacl` is a valid ACL owned by the caller's security descriptor; its header's
        // `AclSize` counts the whole ACL, which the descriptor holds.
        let acl = unsafe { std::slice::from_raw_parts(dacl.cast::<u8>(), usize::from((*dacl).AclSize)) };
        let trusted: [&[u8]; 4] = [&system, &admins, owner, &me];
        crate::acl_scan::exposes(Some(acl), &trusted, u32::MAX)
    }

    /// Grants `kind` `mask` on `path`'s DACL; test-only backdoor standing in for another
    /// local account loosening the file, so `exposed_to_other_users` can be seen refusing it.
    #[cfg(test)]
    pub(crate) fn grant_well_known_for_test(
        path: &Path,
        kind: WELL_KNOWN_SID_TYPE,
        mask: u32,
    ) -> Result<(), String> {
        let added = well_known_sid(kind)?;
        let name = wide(path);
        let mut old_dacl = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        // SAFETY: the out-pointers are valid; `descriptor` owns the DACL until LocalFree.
        let error = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut old_dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if error != ERROR_SUCCESS {
            return Err(format!("reading {}'s DACL failed: {error}", path.display()));
        }
        let result = dacl_with_added_ace(old_dacl, &added, mask).and_then(|new_dacl| {
            // SAFETY: `new_dacl` holds a valid ACL; the other pointers are unused (null).
            let error = unsafe {
                windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW(
                    name.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    new_dacl.as_ptr().cast(),
                    ptr::null_mut(),
                )
            };
            (error == ERROR_SUCCESS)
                .then_some(())
                .ok_or_else(|| format!("SetNamedSecurityInfoW failed: {error}"))
        });
        // SAFETY: `descriptor` was returned by GetNamedSecurityInfoW and must be freed.
        unsafe { LocalFree(descriptor.cast()) };
        result
    }

    /// A copy of `old_dacl` with one access-allowed ACE for `sid` appended. Only the test
    /// backdoor above uses it.
    #[cfg(test)]
    fn dacl_with_added_ace(old_dacl: *const ACL, sid: &[u8], mask: u32) -> Result<Vec<u8>, String> {
        if old_dacl.is_null() {
            return Err("the file has a null DACL".into());
        }
        // SAFETY: `old_dacl` is a valid ACL owned by the caller's security descriptor.
        let old = unsafe { &*old_dacl };
        let new_len = usize::from(old.AclSize) + size_of::<ACCESS_ALLOWED_ACE>() - 4 + sid.len() + 8;
        let mut new_dacl = vec![0u8; new_len];
        let new_ptr = new_dacl.as_mut_ptr().cast::<ACL>();
        // SAFETY: `new_dacl` is large enough for the header plus the copied ACEs plus one new.
        if unsafe { InitializeAcl(new_ptr, new_dacl.len() as u32, ACL_REVISION) } == 0 {
            return Err(last_error("InitializeAcl"));
        }
        for index in 0..old.AceCount {
            let mut ace = ptr::null_mut();
            // SAFETY: `old_dacl` is valid and `index` is below its AceCount.
            if unsafe { GetAce(old_dacl, u32::from(index), &mut ace) } == 0 {
                return Err(last_error("GetAce"));
            }
            // SAFETY: `ace` points at an ACE of `old_dacl`.
            let size = usize::from(unsafe { &*(ace as *const ACE_HEADER) }.AceSize);
            // SAFETY: both ACLs are valid and the ACE bytes are `size` long.
            if unsafe {
                windows_sys::Win32::Security::AddAce(new_ptr, ACL_REVISION, u32::MAX, ace.cast(), size as u32)
            } == 0
            {
                return Err(last_error("AddAce"));
            }
        }
        // SAFETY: `new_ptr` is initialized and `sid` a valid SID of the right length.
        if unsafe { AddAccessAllowedAce(new_ptr, ACL_REVISION, mask, sid_ptr(sid)) } == 0 {
            return Err(last_error("AddAccessAllowedAce"));
        }
        Ok(new_dacl)
    }

    #[cfg(test)]
    pub(crate) mod tests {
        use super::*;
        use std::path::PathBuf;

        use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
        use windows_sys::Win32::Security::{
            ACL_SIZE_INFORMATION, AclSizeInformation, CONTAINER_INHERIT_ACE, GetAclInformation,
            GetSecurityDescriptorControl, OBJECT_INHERIT_ACE, WinBuiltinUsersSid,
        };

        fn temp_dir(tag: &str) -> PathBuf {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("regia-net-private-file-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            dir
        }

        /// Audit fixrev-15-k net2-platform H2/H3, executed on Windows: a file created in a
        /// directory that carries an inheritable ACE must keep a DACL of exactly the owner,
        /// `SYSTEM` and `Administrators`, protected from inheritance — an unprotected DACL
        /// would merge the inherited `Users` ACE in (reopening the file to every local
        /// account) and the next start would refuse its own key.
        #[test]
        fn a_created_file_dacl_is_protected_and_owner_only() {
            let dir = temp_dir("inherit");
            // An inheritable `Users: read` ACE on the directory: a merge would be visible.
            let users = well_known_sid(WinBuiltinUsersSid).expect("Users SID");
            let mut dir_acl =
                AlignedBuf::new(size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - 4 + users.len() + 8);
            let dir_acl_ptr = dir_acl.as_mut_ptr().cast::<ACL>();
            // SAFETY: `dir_acl` is large enough for the header plus one ACE.
            if unsafe { InitializeAcl(dir_acl_ptr, dir_acl.len() as u32, ACL_REVISION) } == 0 {
                panic!("{}", last_error("InitializeAcl"));
            }
            // SAFETY: the ACL is initialized; `users` is a valid SID; the flags make the ACE
            // inheritable by files created in the directory.
            if unsafe {
                windows_sys::Win32::Security::AddAccessAllowedAceEx(
                    dir_acl_ptr,
                    ACL_REVISION,
                    OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                    windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ,
                    sid_ptr(&users),
                )
            } == 0
            {
                panic!("{}", last_error("AddAccessAllowedAceEx"));
            }
            let dir_wide = wide(&dir);
            // SAFETY: `dir_wide` is NUL-terminated; `dir_acl_ptr` names a live ACL; the other
            // arguments are unused (null).
            let error = unsafe {
                SetNamedSecurityInfoW(
                    dir_wide.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    dir_acl_ptr.cast(),
                    ptr::null_mut(),
                )
            };
            assert!(error == ERROR_SUCCESS, "inheritable ACE on the dir: {error}");

            let file = dir.join("server.key");
            create_private(&file)
                .expect("create the private file")
                .sync_all()
                .expect("sync");

            // Read the file's descriptor back: protected control bit, exactly three
            // access-allowed ACEs for the owner, SYSTEM and Administrators.
            let name = wide(&file);
            let mut owner = ptr::null_mut();
            let mut dacl = ptr::null_mut();
            let mut descriptor = ptr::null_mut();
            // SAFETY: the out-pointers are valid; `descriptor` owns the results until freed.
            let error = unsafe {
                GetNamedSecurityInfoW(
                    name.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
                    &mut owner,
                    ptr::null_mut(),
                    &mut dacl,
                    ptr::null_mut(),
                    &mut descriptor,
                )
            };
            assert!(error == ERROR_SUCCESS, "reading back the DACL: {error}");
            let mut control = 0u16;
            let mut revision = 0u32;
            // SAFETY: `descriptor` names the returned security descriptor; `control` and
            // `revision` are valid out-pointers.
            let ok = unsafe { GetSecurityDescriptorControl(descriptor.cast(), &mut control, &mut revision) };
            assert!(ok != 0, "GetSecurityDescriptorControl: {}", last_error("control"));
            assert!(
                control & SE_DACL_PROTECTED != 0,
                "SE_DACL_PROTECTED keeps inherited ACEs out of the created DACL"
            );
            // SAFETY: `info` is plain data filled by GetAclInformation.
            let mut info: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: `dacl` is a live ACL; `info` receives its size information.
            let ok = unsafe {
                GetAclInformation(
                    dacl.cast(),
                    (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                    size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            };
            assert!(ok != 0, "GetAclInformation: {}", last_error("AclInformation"));
            assert_eq!(info.AceCount, 3, "exactly owner, SYSTEM and Administrators");
            let system = well_known_sid(WinLocalSystemSid).expect("SYSTEM SID");
            let admins = well_known_sid(WinBuiltinAdministratorsSid).expect("Administrators SID");
            let known: [*const core::ffi::c_void; 3] =
                [owner, system.as_ptr().cast(), admins.as_ptr().cast()];
            for index in 0..info.AceCount {
                let mut ace = ptr::null_mut();
                // SAFETY: `dacl` is valid and `index` is below its AceCount.
                assert!(unsafe { GetAce(dacl.cast(), index, &mut ace) } != 0);
                // SAFETY: `ace` points at an ACE of `dacl`.
                let header = unsafe { &*(ace as *const ACE_HEADER) };
                assert_eq!(u32::from(header.AceType), ACCESS_ALLOWED_ACE_TYPE, "ACE {index}");
                // SAFETY: an ACCESS_ALLOWED_ACE's SID follows the header and mask, inside the
                // ACE the bounds checks above already accounted for.
                let sid = unsafe { (ace as *const u8).add(size_of::<ACCESS_ALLOWED_ACE>() - 4) };
                let matches = known.iter().any(|known| {
                    // SAFETY: both pointers name valid SIDs.
                    (unsafe { EqualSid(sid.cast::<core::ffi::c_void>().cast_mut(), known.cast_mut()) }) != 0
                });
                assert!(matches, "ACE {index} grants the owner, SYSTEM or Administrators");
            }
            // SAFETY: `descriptor` was returned by GetNamedSecurityInfoW and must be freed.
            unsafe { LocalFree(descriptor.cast()) };

            // The exposure check agrees with the readback.
            let meta = std::fs::metadata(&file).expect("metadata");
            assert!(
                !exposed_to_other_users(&file, &meta).expect("exposure check"),
                "the protected owner-only DACL is not exposed"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Review sprint-rev-v05-windows-close finding 1, executed on Windows: the file is private
        /// from the moment it exists. While the creating handle is still open and nothing has
        /// been written, the DACL already names no other principal, and an open for reading
        /// fails with a sharing violation, so no handle can be taken during the write (the old
        /// CLI path created the file with the folder's ACL and default sharing and restricted it
        /// afterwards with `icacls`). An existing file is `AlreadyExists`.
        #[test]
        fn a_created_file_is_private_and_unshared_before_anything_is_written() {
            let dir = temp_dir("window");
            let file = dir.join("build.gradle.kts");
            let handle = create_private(&file).expect("create the private file");
            let meta = std::fs::metadata(&file)
                .expect("metadata (an attribute-only open is not blocked by sharing)");
            assert!(
                !exposed_to_other_users(&file, &meta).expect("exposure check"),
                "private before the first byte is written"
            );
            let reopened = std::fs::File::open(&file).map_err(|e| e.raw_os_error());
            assert_eq!(
                reopened.err(),
                Some(Some(32)),
                "ERROR_SHARING_VIOLATION while the creator holds the handle"
            );
            drop(handle);
            assert_eq!(
                create_private(&file).map_err(|e| e.kind()).err(),
                Some(io::ErrorKind::AlreadyExists),
                "an existing file is refused with its OS kind"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Sets `path`'s owner to `BUILTIN\Administrators`, as Windows does for objects an
        /// elevated administrator creates when the policy "Default owner for objects created by
        /// members of the Administrators group" is "Administrators group". `None` when this
        /// process may not (it is not elevated: `ERROR_INVALID_OWNER` or access denied).
        pub(crate) fn set_owner_administrators_for_test(path: &Path) -> Option<()> {
            let admins = well_known_sid(WinBuiltinAdministratorsSid).expect("Administrators SID");
            let name = wide(path);
            // SAFETY: `name` is NUL-terminated and `admins` a valid SID; the other pointers are
            // unused (null).
            let error = unsafe {
                SetNamedSecurityInfoW(
                    name.as_ptr(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION,
                    sid_ptr(&admins),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            match error {
                ERROR_SUCCESS => Some(()),
                // ERROR_INVALID_OWNER (1307) / ERROR_ACCESS_DENIED (5): not elevated.
                1307 | 5 => None,
                other => panic!("SetNamedSecurityInfoW(owner): {other}"),
            }
        }

        /// Audit rev-kimi-17 fixrev-15-k finding 1: `create_private` grants the *current user*,
        /// but an elevated administrator's file may be owned by `Administrators`. The engine's
        /// own fresh key must not read as exposed on the next start (that refused the key and
        /// broke every client pin). Needs an elevated process to set the owner; a non-elevated
        /// run says so and checks only the creator-owned case.
        #[test]
        fn a_fresh_key_owned_by_administrators_is_not_exposed() {
            let dir = temp_dir("admins-owner");
            let file = dir.join("server.key");
            create_private(&file).expect("create").sync_all().expect("sync");
            let meta = std::fs::metadata(&file).expect("metadata");
            assert_eq!(
                exposed_to_other_users(&file, &meta),
                Ok(false),
                "creator-owned fresh key"
            );
            if set_owner_administrators_for_test(&file).is_none() {
                eprintln!("SKIPPED the Administrators-owner half: this process is not elevated");
            } else {
                assert_eq!(
                    exposed_to_other_users(&file, &meta),
                    Ok(false),
                    "an Administrators-owned fresh key whose DACL grants the creating user"
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// The test-only owner change above, for the `windows_acl` and `server_host` tests.
#[cfg(all(windows, test))]
pub(crate) use imp::tests::set_owner_administrators_for_test;
