// SPDX-License-Identifier: MPL-2.0
//! Windows ACLs for the server's secret files (audit net1, platform): the Unix builds refuse a
//! `server.key` or `admin.token` that its group or others can read (`mode & 0o077`,
//! `refuse_if_exposed` / `read_admin_token`); Windows has no mode bits, so the same wall is a
//! DACL. A key or token created under a directory with an inherited ACL (e.g.
//! `C:\Program Files`, `C:\ProgramData`) would otherwise be readable by every local user: the
//! key completes the handshake as this server, and the token is full remote admin.
//!
//! The test-only `create_private_file` retains the owner/SYSTEM writer regression; production
//! key writes use `private_file`. The admin-token `exposed` check reports whether a DACL grants
//! any access (read or write) to a principal other than the owner, the current user, SYSTEM
//! and Administrators, or is absent (an absent DACL grants everyone everything); the ACE walk
//! is `acl_scan`'s, shared with `private_file` (ADR-5880). Derived from the public Win32
//! security reference (Microsoft Learn: "Access Control Lists", "File Security and Access
//! Rights"); the code is original.

use std::ffi::c_void;
use std::path::Path;

#[cfg(test)]
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
};
#[cfg(test)]
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
    AddAccessAllowedAce, AddAce, GetAce, GetAclInformation, InitializeAcl, InitializeSecurityDescriptor,
    MakeSelfRelativeSD, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetFileSecurityW,
    SetSecurityDescriptorDacl, WinAuthenticatedUserSid, WinBuiltinUsersSid, WinWorldSid,
};
use windows_sys::Win32::Security::{
    ACL, CopySid, CreateWellKnownSid, DACL_SECURITY_INFORMATION, GetFileSecurityW, GetLengthSid,
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation, OWNER_SECURITY_INFORMATION,
    TOKEN_QUERY, TOKEN_USER, TokenUser, WELL_KNOWN_SID_TYPE, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
#[cfg(test)]
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL, FlushFileBuffers, WriteFile,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_GENERIC_WRITE};
use windows_sys::Win32::System::SystemServices::MAXIMUM_ALLOWED;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `SECURITY_DESCRIPTOR_REVISION` (the ABI value; not exported by windows-sys).
#[cfg(test)]
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

fn last_error(what: &str) -> String {
    // SAFETY: `GetLastError` reads the calling thread's last-error value; no pointers.
    format!("{what}: GetLastError {}", unsafe { GetLastError() })
}

fn to_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// A zero-filled buffer with 8-byte alignment for Win32 security structures. `TOKEN_USER` and
/// `SECURITY_DESCRIPTOR` hold pointers (alignment 8 on 64-bit targets) and `ACL`/ACEs need
/// their natural alignment above 1; a `Vec<u8>` promises alignment 1, so referencing or
/// dereferencing those structs over it would be unsound no matter where the allocator happens
/// to place the allocation.
fn aligned_buf(bytes: usize) -> Vec<u64> {
    vec![0u64; bytes.div_ceil(8)]
}

/// An owned buffer holding the SID for `kind`.
fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Result<Vec<u8>, String> {
    // SECURITY_MAX_SID_SIZE (68) always fits a well-known SID; `size` receives the exact length.
    let mut size = 68u32;
    let mut buf = vec![0u8; size as usize];
    // SAFETY: `buf` is at least SECURITY_MAX_SID_SIZE bytes and `size` names its length; on
    // success the SID was written within those bounds.
    let ok = unsafe { CreateWellKnownSid(kind, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size) };
    if ok == 0 {
        return Err(last_error("CreateWellKnownSid"));
    }
    buf.truncate(size as usize);
    Ok(buf)
}

/// An owned copy of the current user's SID.
fn current_user_sid() -> Result<Vec<u8>, String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: opens this process's token with query access; `token` receives a handle owned by
    // this function (closed below).
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    let result = (|| {
        let mut len = 0u32;
        // SAFETY: the null-buffer call is the documented length query; it fails with
        // ERROR_INSUFFICIENT_BUFFER and sets `len` to the required size.
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return Err(last_error("GetTokenInformation(length)"));
        }
        let mut buf = aligned_buf(len as usize);
        // SAFETY: `buf` holds `len` bytes (8-byte-aligned, so a `TOKEN_USER` reference over it
        // is sound); a successful `TokenUser` query fills a TOKEN_USER at its start, and its SID
        // pointer stays valid while `buf` lives.
        if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0 {
            return Err(last_error("GetTokenInformation"));
        }
        let sid_len = {
            // SAFETY: `buf` holds a TOKEN_USER; `User.Sid` is the SID the token returned.
            let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
            // SAFETY: `user.User.Sid` is a valid SID.
            (unsafe { GetLengthSid(user.User.Sid) }) as usize
        };
        let mut sid = vec![0u8; sid_len];
        // SAFETY: `sid` is exactly `sid_len` bytes; the copy happens before `buf` drops.
        if unsafe {
            CopySid(
                sid_len as u32,
                sid.as_mut_ptr().cast(),
                buf.as_ptr().cast::<TOKEN_USER>().read().User.Sid,
            )
        } == 0
        {
            return Err(last_error("CopySid"));
        }
        Ok(sid)
    })();
    // SAFETY: closes the token handle opened above; it is not used afterwards.
    unsafe { CloseHandle(token) };
    result
}

/// An owned buffer holding an initialized ACL with one access-allowed ACE per grant.
#[cfg(test)]
fn build_acl(grants: &[(Vec<u8>, u32)]) -> Result<Vec<u8>, String> {
    // sizeof(ACL) + one ACCESS_ALLOWED_ACE (header + mask, without the SID) + the SID, per grant.
    let ace_overhead = std::mem::size_of::<ACCESS_ALLOWED_ACE>() as u32 - 4;
    let size = std::mem::size_of::<ACL>() as u32
        + grants
            .iter()
            .map(|(sid, _)| ace_overhead + sid.len() as u32)
            .sum::<u32>();
    let mut buf = vec![0u8; size as usize];
    // SAFETY: `buf` is exactly `size` bytes as InitializeAcl requires.
    if unsafe { InitializeAcl(buf.as_mut_ptr().cast(), size, ACL_REVISION) } == 0 {
        return Err(last_error("InitializeAcl"));
    }
    for (sid, mask) in grants {
        // SAFETY: the ACL was initialized above with room for each grant; the SID outlives the
        // call.
        if unsafe {
            AddAccessAllowedAce(
                buf.as_mut_ptr().cast(),
                ACL_REVISION,
                *mask,
                sid.as_ptr() as *mut c_void,
            )
        } == 0
        {
            return Err(last_error("AddAccessAllowedAce"));
        }
    }
    Ok(buf)
}

/// An owned buffer holding an absolute SECURITY_DESCRIPTOR whose DACL is `acl` (which must
/// outlive the descriptor's use).
#[cfg(test)]
fn absolute_sd_with_acl(acl: &[u8]) -> Result<Vec<u8>, String> {
    let mut sd = vec![0u8; std::mem::size_of::<SECURITY_DESCRIPTOR>()];
    // SAFETY: `sd` is a SECURITY_DESCRIPTOR-sized buffer.
    if unsafe { InitializeSecurityDescriptor(sd.as_mut_ptr().cast(), SECURITY_DESCRIPTOR_REVISION) } == 0 {
        return Err(last_error("InitializeSecurityDescriptor"));
    }
    // SAFETY: `sd` is initialized and `acl` names a live ACL.
    if unsafe { SetSecurityDescriptorDacl(sd.as_mut_ptr().cast(), 1, acl.as_ptr().cast(), 0) } == 0 {
        return Err(last_error("SetSecurityDescriptorDacl"));
    }
    Ok(sd)
}

/// An absolute SECURITY_DESCRIPTOR granting exactly `grants`, with the ACL kept alive next to
/// it for as long as the descriptor is used.
#[cfg(test)]
fn build_absolute_sd(grants: &[(Vec<u8>, u32)]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let acl = build_acl(grants)?;
    let sd = absolute_sd_with_acl(&acl)?;
    Ok((sd, acl))
}

/// Reads the self-relative security descriptor of `path`, requesting its owner and DACL. The
/// owner is needed: `exposed` trusts it, and a descriptor read without it names no owner, so
/// the owner's own ACE counted as another principal and every admin token read as exposed
/// (audit rev-kimi-16 fixrev-14-k finding 1, rev-kimi-17 fixrev-15-k finding 0). The returned buffer is 8-byte-aligned (`Vec<u64>`), so the descriptor header, DACL and ACEs
/// can be referenced and dereferenced soundly (see `aligned_buf`).
fn read_sd(path: &Path, what: &str) -> Result<Vec<u64>, String> {
    // Verbatim (`\\?\`), as the writers open the file: a path of 260 characters or more fails
    // the plain Win32 call without a long-path manifest (corr-net, ADR-5580).
    let long = crate::private_file::verbatim(path).map_err(|e| format!("{what}: {e}"))?;
    let wide = to_wide(&long);
    let mut needed = 0u32;
    // SAFETY: the null-buffer call is the documented length query; it fails with
    // ERROR_INSUFFICIENT_BUFFER and sets `needed`.
    unsafe {
        GetFileSecurityW(
            wide.as_ptr(),
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed == 0 {
        return Err(last_error("GetFileSecurityW(length)"));
    }
    let mut buf = aligned_buf(needed as usize);
    // SAFETY: `buf` holds `needed` bytes (8-byte-aligned); on success it holds the
    // self-relative descriptor.
    if unsafe {
        GetFileSecurityW(
            wide.as_ptr(),
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(last_error(&format!("{what}: read the ACL")));
    }
    Ok(buf)
}

/// Creates `path` (failing when it already exists) readable and writable only by the current
/// user and SYSTEM, writes `contents` and flushes them. Mirrors the Unix mode-0600 creation of
/// `server.key`: without it the key inherits the directory ACL (e.g. under `C:\ProgramData`),
/// where every local user can read the server's private key.
#[cfg(test)]
pub(crate) fn create_private_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    let user = current_user_sid()?;
    let system = well_known_sid(WinLocalSystemSid)?;
    let (sd, _acl) = build_absolute_sd(&[(user, FILE_ALL_ACCESS), (system, FILE_ALL_ACCESS)])?;
    let wide = to_wide(path);
    // SAFETY: SECURITY_ATTRIBUTES is plain data (all fields are set below before use).
    let mut attrs: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
    attrs.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
    attrs.lpSecurityDescriptor = sd.as_ptr() as *mut c_void;
    attrs.bInheritHandle = 0;
    // SAFETY: `wide` is null-terminated; `attrs` names the descriptor and ACL kept alive above;
    // no sharing lets another process open the file before the DACL is in place.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_WRITE,
            0,
            &attrs,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error(&format!("{}: create", path.display())));
    }
    let mut written = 0u32;
    // SAFETY: `handle` is a live file handle; `contents` is readable for its length.
    let wrote = unsafe {
        WriteFile(
            handle,
            contents.as_ptr(),
            contents.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    };
    // `GetLastError` is thread-local and `CloseHandle` may reset it: capture the codes now,
    // before the handle closes.
    let write_err = (wrote == 0).then(|| last_error(&format!("{}: write", path.display())));
    let mut flush_err = None;
    if write_err.is_none() {
        // SAFETY: `handle` is a live file handle.
        let flushed = unsafe { FlushFileBuffers(handle) };
        if flushed == 0 {
            flush_err = Some(last_error(&format!("{}: flush", path.display())));
        }
    }
    // SAFETY: closes the handle opened above; it is not used afterwards.
    unsafe { CloseHandle(handle) };
    if let Some(e) = write_err.or(flush_err) {
        return Err(e);
    }
    if written as usize != contents.len() {
        return Err(format!(
            "{}: short write ({written} of {} bytes)",
            path.display(),
            contents.len()
        ));
    }
    Ok(())
}

/// Access bits, beyond the generic read/write masks, that hand a principal control of the file
/// content or its DACL (ABI values; windows-sys does not export these `FILE_ACCESS_RIGHTS`
/// members as constants).
const EXPOSED_WRITE_BITS: u32 = 0x0000_0004 /* FILE_APPEND_DATA */
    | 0x0001_0000 /* DELETE */
    | 0x0004_0000 /* WRITE_DAC */
    | 0x0008_0000 /* WRITE_OWNER */;

/// Whether `path`'s DACL grants any access — read *or* write — to a principal other than the
/// owner, the current user, SYSTEM and Administrators, or is absent (an absent DACL grants
/// everyone everything). An allow ACE whose grant cannot be judged (object ACEs are read;
/// callback, compound and unknown ACE types are not) is an error, which the caller treats as
/// exposed.
/// This is the Unix `mode & 0o077` wall for Windows: a `server.key` another local user can read
/// completes the handshake as this server, and an `admin.token` they can write, append to,
/// delete or re-protect can be replaced with a known token (full remote admin).
pub(crate) fn exposed(path: &Path) -> Result<bool, String> {
    let buf = read_sd(path, &path.display().to_string())?;
    let sd = buf.as_ptr().cast::<c_void>() as *mut c_void;
    let mut present = 0i32;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut defaulted = 0i32;
    // SAFETY: `sd` names the aligned descriptor just read; `dacl` points into `buf` (alive
    // throughout).
    if unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) } == 0 {
        return Err(last_error("GetSecurityDescriptorDacl"));
    }
    if present == 0 || dacl.is_null() {
        return Ok(true);
    }
    // The DACL lies inside the self-relative descriptor `buf`; its header's `AclSize` counts
    // the whole ACL. Bound it by the buffer before viewing it as bytes.
    let buf_start = buf.as_ptr() as usize;
    let buf_len = buf.len() * 8;
    let offset = (dacl as usize).wrapping_sub(buf_start);
    if offset
        .checked_add(std::mem::size_of::<ACL>())
        .is_none_or(|end| end > buf_len)
    {
        return Err(format!(
            "{}: the DACL lies outside its security descriptor",
            path.display()
        ));
    }
    // SAFETY: the ACL header lies inside `buf` (checked above); `buf` is 8-byte aligned and the
    // ACL starts at a 4-byte-aligned offset within the self-relative descriptor.
    let acl_size = usize::from(unsafe { (*dacl).AclSize });
    if offset + acl_size > buf_len {
        return Err(format!(
            "{}: the DACL overruns its security descriptor",
            path.display()
        ));
    }
    // SAFETY: `offset..offset + acl_size` lies inside `buf` (checked above), which stays alive
    // and unmodified while the slice is used.
    let acl = unsafe { std::slice::from_raw_parts(dacl.cast::<u8>().cast_const(), acl_size) };
    let system = well_known_sid(WinLocalSystemSid)?;
    let admins = well_known_sid(WinBuiltinAdministratorsSid)?;
    let owner = owner_sid(sd)?;
    // The creating user too, not only the owner: an elevated administrator's file may be owned
    // by `Administrators` (the server-SKU default owner policy) while its DACL names the user
    // who created it (audit rev-kimi-17 fixrev-15-k finding 1).
    let me = current_user_sid()?;
    // The owner, the current user, SYSTEM and Administrators holding the file is the
    // private-file shape itself; any other principal with any of these rights exposes it.
    let trusted: [&[u8]; 4] = [&system, &admins, &owner, &me];
    crate::acl_scan::exposes(Some(acl), &trusted, EXPOSED_RIGHTS)
}

/// The rights whose grant to another principal exposes a secret file: read, write and the
/// control bits (`MAXIMUM_ALLOWED` is a request flag no stored ACE carries; it stays for
/// robustness).
const EXPOSED_RIGHTS: u32 = FILE_GENERIC_READ
    | GENERIC_READ
    | FILE_GENERIC_WRITE
    | GENERIC_WRITE
    | GENERIC_ALL
    | MAXIMUM_ALLOWED
    | EXPOSED_WRITE_BITS;

/// The descriptor's owner SID. A descriptor that names no owner is an error, so a check that
/// cannot identify the owner fails closed explicitly instead of quietly trusting no one.
fn owner_sid(sd: *mut c_void) -> Result<Vec<u8>, String> {
    let mut owner: *mut c_void = std::ptr::null_mut();
    let mut defaulted = 0i32;
    // SAFETY: `sd` names a self-relative descriptor; `owner` points into it and is copied below.
    if unsafe { GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted) } == 0 {
        return Err(last_error("GetSecurityDescriptorOwner"));
    }
    if owner.is_null() {
        return Err("the security descriptor names no owner".into());
    }
    // SAFETY: `owner` is the descriptor's owner SID, valid while `sd` lives.
    let len = unsafe { GetLengthSid(owner) } as usize;
    let mut sid = vec![0u8; len];
    // SAFETY: `sid` is `len` bytes; the copy happens before the descriptor is dropped.
    if unsafe { CopySid(len as u32, sid.as_mut_ptr().cast(), owner) } == 0 {
        return Err(last_error("CopySid(owner)"));
    }
    Ok(sid)
}

/// Grants `kind` the `mask` access on `path` by rewriting the DACL with the existing ACEs plus
/// the new one. Test-only counterpart of the production checks.
#[cfg(test)]
pub(crate) fn grant_for_test(path: &Path, kind: WELL_KNOWN_SID_TYPE, mask: u32) -> Result<(), String> {
    let grant = well_known_sid(kind)?;
    grant_sid_for_test(path, &grant, mask)
}

/// Grants the `sid` the `mask` access on `path` by rewriting the DACL with the existing ACEs
/// plus the new one.
#[cfg(test)]
fn grant_sid_for_test(path: &Path, grant: &[u8], mask: u32) -> Result<(), String> {
    let wide = to_wide(path);
    let buf = read_sd(path, &path.display().to_string())?;
    let sd = buf.as_ptr().cast::<c_void>() as *mut c_void;
    let mut present = 0i32;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut defaulted = 0i32;
    // SAFETY: `sd` names the descriptor just read; `dacl` points into `buf` (alive throughout).
    if unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) } == 0 {
        return Err(last_error("GetSecurityDescriptorDacl"));
    }
    // SAFETY: ACL_SIZE_INFORMATION is plain data filled by GetAclInformation.
    let mut info: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
    let mut new_size = std::mem::size_of::<ACL>() as u32 + std::mem::size_of::<ACCESS_ALLOWED_ACE>() as u32
        - 4
        + grant.len() as u32;
    let has_dacl = present != 0 && !dacl.is_null();
    if has_dacl {
        // SAFETY: `dacl` is a live ACL; `info` receives its size information.
        if unsafe {
            GetAclInformation(
                dacl.cast(),
                (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
        {
            return Err(last_error("GetAclInformation"));
        }
        new_size += info.AclBytesInUse - std::mem::size_of::<ACL>() as u32;
    }
    let mut new_acl = vec![0u8; new_size as usize];
    // SAFETY: `new_acl` is `new_size` bytes as InitializeAcl requires.
    if unsafe { InitializeAcl(new_acl.as_mut_ptr().cast(), new_size, ACL_REVISION) } == 0 {
        return Err(last_error("InitializeAcl"));
    }
    if has_dacl {
        for i in 0..info.AceCount {
            let mut ace: *mut c_void = std::ptr::null_mut();
            // SAFETY: `i` is below the ACE count; `ace` points at a live ACE in the ACL.
            if unsafe { GetAce(dacl.cast(), i, &mut ace) } == 0 {
                return Err(last_error("GetAce"));
            }
            // SAFETY: `ace` is a live ACE; its header gives its size within the ACL.
            let size = unsafe { (*(ace as *const ACE_HEADER)).AceSize } as u32;
            // SAFETY: `new_acl` has room (it counts every existing ACE plus the new one).
            if unsafe { AddAce(new_acl.as_mut_ptr().cast(), ACL_REVISION, u32::MAX, ace, size) } == 0 {
                return Err(last_error("AddAce"));
            }
        }
    }
    // SAFETY: `new_acl` was initialized with room for one more ACE; the grant SID outlives it.
    if unsafe {
        AddAccessAllowedAce(
            new_acl.as_mut_ptr().cast(),
            ACL_REVISION,
            mask,
            grant.as_ptr() as *mut c_void,
        )
    } == 0
    {
        return Err(last_error("AddAccessAllowedAce"));
    }
    let sd_abs = absolute_sd_with_acl(&new_acl)?;
    let mut rel_size = 0u32;
    // SAFETY: the null call is the documented length query for the self-relative copy.
    unsafe {
        MakeSelfRelativeSD(
            sd_abs.as_ptr() as *mut c_void,
            std::ptr::null_mut(),
            &mut rel_size,
        )
    };
    if rel_size == 0 {
        return Err(last_error("MakeSelfRelativeSD(length)"));
    }
    let mut rel = vec![0u8; rel_size as usize];
    // SAFETY: `rel` holds `rel_size` bytes for the self-relative descriptor; the absolute SD and
    // its ACL stay alive until after the copy.
    if unsafe {
        MakeSelfRelativeSD(
            sd_abs.as_ptr() as *mut c_void,
            rel.as_mut_ptr() as *mut c_void,
            &mut rel_size,
        )
    } == 0
    {
        return Err(last_error("MakeSelfRelativeSD"));
    }
    // SAFETY: `rel` holds a self-relative descriptor; `wide` is null-terminated.
    if unsafe {
        SetFileSecurityW(
            wide.as_ptr(),
            DACL_SECURITY_INFORMATION,
            rel.as_ptr() as *mut c_void,
        )
    } == 0
    {
        return Err(last_error("SetFileSecurityW"));
    }
    Ok(())
}

/// Gives the directory `dir` a DACL protected from inheritance that grants only the current
/// user, SYSTEM and Administrators, inherited by the files created in it: a plain
/// `std::fs::write` there yields an owner-only file wherever `%TEMP%` points (audit
/// rev-kimi-17 fixrev-15-k finding 3). Test-only.
#[cfg(test)]
pub(crate) fn protect_dir_for_test(dir: &Path) -> Result<(), String> {
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows_sys::Win32::Security::{
        AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE, OBJECT_INHERIT_ACE, PROTECTED_DACL_SECURITY_INFORMATION,
    };
    let sids = [
        current_user_sid()?,
        well_known_sid(WinLocalSystemSid)?,
        well_known_sid(WinBuiltinAdministratorsSid)?,
    ];
    let ace_overhead = std::mem::size_of::<ACCESS_ALLOWED_ACE>() - 4;
    let size = std::mem::size_of::<ACL>() + sids.iter().map(|s| ace_overhead + s.len()).sum::<usize>();
    let mut acl = aligned_buf(size);
    let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
    // SAFETY: `acl` is at least `size` bytes, 8-byte aligned.
    if unsafe { InitializeAcl(acl_ptr, size as u32, ACL_REVISION) } == 0 {
        return Err(last_error("InitializeAcl"));
    }
    for sid in &sids {
        // SAFETY: the ACL was initialized with room for each ACE; `sid` is a valid SID.
        if unsafe {
            AddAccessAllowedAceEx(
                acl_ptr,
                ACL_REVISION,
                OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                FILE_ALL_ACCESS,
                sid.as_ptr() as *mut c_void,
            )
        } == 0
        {
            return Err(last_error("AddAccessAllowedAceEx"));
        }
    }
    let wide = to_wide(dir);
    // SAFETY: `wide` is NUL-terminated and `acl_ptr` a live ACL; the other pointers are unused.
    let error = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl_ptr,
            std::ptr::null_mut(),
        )
    };
    if error != 0 {
        return Err(format!("SetNamedSecurityInfoW({}): {error}", dir.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("regia-net-acl-{tag}-{}-{n}", std::process::id()))
    }

    /// Each grant gets a fresh file: `grant_for_test` rewrites the DACL, so one file per case
    /// isolates the ACEs (on a shared file an earlier grant's ACE would satisfy a later check).
    #[test]
    fn a_private_file_is_not_exposed_and_grants_are() {
        let path = temp_path("private");
        let _ = std::fs::remove_file(&path);
        create_private_file(&path, b"secret").expect("create");
        assert!(!exposed(&path).expect("check"), "owner + SYSTEM only");
        let _ = std::fs::remove_file(&path);

        for (sid, who) in [
            (WinWorldSid, "Everyone"),
            (WinBuiltinUsersSid, "Users"),
            (WinAuthenticatedUserSid, "Authenticated Users"),
        ] {
            let path = temp_path("read");
            let _ = std::fs::remove_file(&path);
            create_private_file(&path, b"secret").expect("create");
            grant_for_test(&path, sid, FILE_GENERIC_READ).unwrap_or_else(|e| panic!("{who}: grant: {e}"));
            assert!(exposed(&path).expect("check"), "{who} read exposes the file");
            let _ = std::fs::remove_file(&path);
        }

        // Write-only is exposure too: such a local user can overwrite an `admin.token` with a
        // known one (the Unix wall refuses group/other *any* bits, not only read).
        // `MAXIMUM_ALLOWED` is an access-request flag, not a grantable right: an ACE added with
        // it does not carry the bit, so it is no test case here (audit rev-kimi-16 fixrev-14-k
        // finding 2).
        for (mask, what) in [(FILE_GENERIC_WRITE, "write"), (GENERIC_ALL, "GENERIC_ALL")] {
            let path = temp_path("write");
            let _ = std::fs::remove_file(&path);
            create_private_file(&path, b"secret").expect("create");
            grant_for_test(&path, WinBuiltinUsersSid, mask).unwrap_or_else(|e| panic!("{what}: grant: {e}"));
            assert!(exposed(&path).expect("check"), "Users {what} exposes the file");
            let _ = std::fs::remove_file(&path);
        }

        // The owner holding the file is the private-file shape itself, not exposure.
        let path = temp_path("owner-write");
        let _ = std::fs::remove_file(&path);
        create_private_file(&path, b"secret").expect("create");
        let me = current_user_sid().expect("current user sid");
        grant_sid_for_test(&path, &me, FILE_GENERIC_WRITE).expect("grant owner write");
        assert!(!exposed(&path).expect("check"), "owner write is not exposure");
        let _ = std::fs::remove_file(&path);
    }

    /// Audit rev-kimi-17 fixrev-15-k findings 0 and 1: a token the current user created, whose
    /// DACL grants that user and SYSTEM, is private whether the user or (an elevated
    /// administrator under the "Administrators group" owner policy) `Administrators` owns it.
    #[test]
    fn a_private_token_is_not_exposed_whoever_of_the_trusted_owns_it() {
        let path = temp_path("admins-owner");
        let _ = std::fs::remove_file(&path);
        create_private_file(&path, b"secret").expect("create");
        assert_eq!(exposed(&path), Ok(false), "creator-owned token");
        if crate::private_file::set_owner_administrators_for_test(&path).is_none() {
            eprintln!("SKIPPED the Administrators-owner half: this process is not elevated");
        } else {
            assert_eq!(exposed(&path), Ok(false), "Administrators-owned token");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn create_private_file_refuses_existing() {
        let path = temp_path("exists");
        std::fs::write(&path, b"old").expect("write");
        assert!(create_private_file(&path, b"new").is_err(), "CREATE_NEW");
        assert_eq!(std::fs::read(&path).expect("read"), b"old");
        let _ = std::fs::remove_file(&path);
    }
}
