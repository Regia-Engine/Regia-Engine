// SPDX-License-Identifier: MPL-2.0
//! One walk over a Windows DACL, shared by the two Windows secret-file checks
//! (`private_file::exposed_to_other_users` for `server.key` / `known_hosts.ron` and
//! `windows_acl::exposed` for `admin.token`). It parses the ACL bytes itself, with no Win32
//! call, so the rules run as unit tests on every host (ADR-5880).
//!
//! The rules: an access-allowed ACE (plain or object) that grants any of the relevant bits to a
//! principal outside the trusted set exposes the file; an ACE type that grants access under a
//! condition this check cannot evaluate (callback, callback-object, compound) or a type it does
//! not know is an error, never a pass; deny, audit, alarm, label and attribute ACEs grant
//! nothing. Layouts are from the public Win32 reference (Microsoft Learn: "ACL structure",
//! "ACE_HEADER structure", "ACCESS_ALLOWED_ACE", "ACCESS_ALLOWED_OBJECT_ACE", "SID
//! structure"); the code is original.

/// `ACL` header bytes: revision, Sbz1, `AclSize` (u16), `AceCount` (u16), Sbz2.
const ACL_HEADER_BYTES: usize = 8;
/// `ACE_HEADER` bytes: type, flags, `AceSize` (u16).
const ACE_HEADER_BYTES: usize = 4;
/// A SID's fixed part: revision, sub-authority count, the 6-byte identifier authority.
const SID_FIXED_BYTES: usize = 8;
/// `ACE_OBJECT_TYPE_PRESENT`: an object ACE carries its `ObjectType` GUID.
const ACE_OBJECT_TYPE_PRESENT: u32 = 0x1;
/// `ACE_INHERITED_OBJECT_TYPE_PRESENT`: an object ACE carries its `InheritedObjectType` GUID.
const ACE_INHERITED_OBJECT_TYPE_PRESENT: u32 = 0x2;
/// A GUID's size inside an object ACE.
const GUID_BYTES: usize = 16;

/// `ACCESS_ALLOWED_ACE_TYPE`.
pub(crate) const ACCESS_ALLOWED: u8 = 0x00;
/// `ACCESS_ALLOWED_COMPOUND_ACE_TYPE`: grants access to a server acting for a client.
pub(crate) const ACCESS_ALLOWED_COMPOUND: u8 = 0x04;
/// `ACCESS_ALLOWED_OBJECT_ACE_TYPE`.
pub(crate) const ACCESS_ALLOWED_OBJECT: u8 = 0x05;
/// `ACCESS_ALLOWED_CALLBACK_ACE_TYPE`: grants access when a condition holds.
pub(crate) const ACCESS_ALLOWED_CALLBACK: u8 = 0x09;
/// `ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE`.
pub(crate) const ACCESS_ALLOWED_CALLBACK_OBJECT: u8 = 0x0B;
/// The last ACE type the public reference defines (`SYSTEM_ACCESS_FILTER_ACE_TYPE`).
const LAST_KNOWN_ACE_TYPE: u8 = 0x15;

/// Whether the DACL `acl` (the whole ACL, header first; `None` for a null DACL) grants any of
/// `relevant` to a principal whose SID is not in `trusted`. A null DACL grants everyone
/// everything, so it is exposed. A DACL this walk cannot judge (malformed bytes, a conditional
/// or compound allow ACE, an unknown ACE type) is an error: a check that cannot run must not
/// let a secret through.
pub(crate) fn exposes(acl: Option<&[u8]>, trusted: &[&[u8]], relevant: u32) -> Result<bool, String> {
    let Some(acl) = acl else {
        return Ok(true);
    };
    if acl.len() < ACL_HEADER_BYTES {
        return Err(format!("the ACL is {} bytes, shorter than its header", acl.len()));
    }
    let acl_size = usize::from(u16::from_le_bytes([acl[2], acl[3]]));
    let ace_count = u16::from_le_bytes([acl[4], acl[5]]);
    if acl_size < ACL_HEADER_BYTES || acl_size > acl.len() {
        return Err(format!(
            "the ACL names a size of {acl_size} bytes but {} are readable",
            acl.len()
        ));
    }
    let acl = &acl[..acl_size];
    let mut at = ACL_HEADER_BYTES;
    for index in 0..ace_count {
        let Some(header) = acl.get(at..at + ACE_HEADER_BYTES) else {
            return Err(format!("ACE {index} starts past the end of the ACL"));
        };
        let ace_type = header[0];
        let ace_size = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let Some(ace) = acl
            .get(at..at + ace_size)
            .filter(|_| ace_size >= ACE_HEADER_BYTES)
        else {
            return Err(format!("ACE {index} ({ace_size} bytes) overruns the ACL"));
        };
        at += ace_size;
        let sid_offset = match ace_type {
            ACCESS_ALLOWED => 8,
            ACCESS_ALLOWED_OBJECT => {
                let flags = read_u32(ace, 8).ok_or_else(|| format!("object ACE {index} is too short"))?;
                let mut offset = 12;
                // The SID follows zero, one or two GUIDs, depending on the flags.
                if flags & ACE_OBJECT_TYPE_PRESENT != 0 {
                    offset += GUID_BYTES;
                }
                if flags & ACE_INHERITED_OBJECT_TYPE_PRESENT != 0 {
                    offset += GUID_BYTES;
                }
                offset
            }
            ACCESS_ALLOWED_COMPOUND | ACCESS_ALLOWED_CALLBACK | ACCESS_ALLOWED_CALLBACK_OBJECT => {
                return Err(format!(
                    "the DACL holds a conditional or compound access-allowed ACE (index {index}, type {ace_type:#04x}); refusing to judge it"
                ));
            }
            t if t > LAST_KNOWN_ACE_TYPE => {
                return Err(format!(
                    "the DACL holds an ACE of unknown type {t:#04x} (index {index}); refusing to judge it"
                ));
            }
            // Deny, audit, alarm, mandatory-label, resource-attribute, scoped-policy,
            // trust-label and access-filter ACEs grant nothing to anyone.
            _ => continue,
        };
        let mask = read_u32(ace, 4).ok_or_else(|| format!("ACE {index} is too short for its mask"))?;
        if mask & relevant == 0 {
            continue;
        }
        let sid =
            sid_at(ace, sid_offset).ok_or_else(|| format!("ACE {index} carries a SID that overruns it"))?;
        if !trusted.iter().any(|known| !known.is_empty() && *known == sid) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The little-endian u32 at `at`, when it fits.
fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let word = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
}

/// The whole SID starting at `at` (its length from its sub-authority count), when it fits.
fn sid_at(ace: &[u8], at: usize) -> Option<&[u8]> {
    let count = usize::from(*ace.get(at + 1)?);
    ace.get(at..at + SID_FIXED_BYTES + 4 * count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A SID with authority 5 (NT) and the given sub-authorities.
    fn sid(subs: &[u32]) -> Vec<u8> {
        let mut out = vec![1, subs.len() as u8, 0, 0, 0, 0, 0, 5];
        for s in subs {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    fn system() -> Vec<u8> {
        sid(&[18])
    }
    fn admins() -> Vec<u8> {
        sid(&[32, 544])
    }
    fn users() -> Vec<u8> {
        sid(&[32, 545])
    }
    fn creator() -> Vec<u8> {
        sid(&[21, 1, 2, 3, 1001])
    }

    fn check(acl: &[u8], trusted: &[&[u8]], relevant: u32) -> Result<bool, String> {
        exposes(Some(acl), trusted, relevant)
    }

    const READ: u32 = 0x0012_0089; // FILE_GENERIC_READ

    /// An ACE of `ace_type` with `mask` and `body` (what follows the mask).
    fn ace(ace_type: u8, mask: u32, body: &[u8]) -> Vec<u8> {
        let size = (ACE_HEADER_BYTES + 4 + body.len()) as u16;
        let mut out = vec![ace_type, 0];
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&mask.to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    fn allow(mask: u32, who: &[u8]) -> Vec<u8> {
        ace(ACCESS_ALLOWED, mask, who)
    }

    fn acl(aces: &[Vec<u8>]) -> Vec<u8> {
        let size = (ACL_HEADER_BYTES + aces.iter().map(Vec::len).sum::<usize>()) as u16;
        let mut out = vec![2, 0];
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(aces.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        for a in aces {
            out.extend_from_slice(a);
        }
        out
    }

    /// The trusted set `private_file` and `windows_acl` pass: SYSTEM, Administrators, the file's
    /// owner and the current user.
    fn trusted<'a>(owner: &'a [u8], me: &'a [u8], sys: &'a [u8], adm: &'a [u8]) -> [&'a [u8]; 4] {
        [sys, adm, owner, me]
    }

    #[test]
    fn the_private_file_shape_is_not_exposed_and_a_users_grant_is() {
        let (sys, adm, me) = (system(), admins(), creator());
        let private = acl(&[allow(READ, &adm), allow(READ, &sys), allow(READ, &me)]);
        let t = trusted(&me, &me, &sys, &adm);
        assert_eq!(check(&private, &t, u32::MAX), Ok(false));
        let opened = acl(&[
            allow(READ, &adm),
            allow(READ, &sys),
            allow(READ, &me),
            allow(READ, &users()),
        ]);
        assert_eq!(check(&opened, &t, u32::MAX), Ok(true));
        assert_eq!(
            exposes(None, &t, u32::MAX),
            Ok(true),
            "a null DACL grants everyone"
        );
    }

    /// Audit rev-kimi-17 fixrev-15-k finding 1: with the owner policy "Administrators group", an
    /// elevated server's fresh key is owned by Administrators while its DACL grants the creating
    /// user. The creating user is the private-file shape, not another principal.
    #[test]
    fn the_creating_user_is_trusted_when_administrators_own_the_file() {
        let (sys, adm, me) = (system(), admins(), creator());
        let created = acl(&[allow(READ, &adm), allow(READ, &sys), allow(READ, &me)]);
        assert_eq!(
            check(&created, &trusted(&adm, &me, &sys, &adm), u32::MAX),
            Ok(false)
        );
        // Another account is still another account.
        let other = sid(&[21, 1, 2, 3, 1002]);
        let opened = acl(&[allow(READ, &adm), allow(READ, &sys), allow(READ, &other)]);
        assert_eq!(
            check(&opened, &trusted(&adm, &me, &sys, &adm), u32::MAX),
            Ok(true)
        );
    }

    /// An empty trusted entry (an owner the descriptor did not name) never matches anything.
    #[test]
    fn an_empty_trusted_sid_trusts_no_one() {
        let (sys, adm) = (system(), admins());
        let opened = acl(&[allow(READ, &users())]);
        let t: [&[u8]; 4] = [&sys, &adm, &[], &[]];
        assert_eq!(check(&opened, &t, u32::MAX), Ok(true));
    }

    /// Audit rev-kimi-16 fixrev-14-k finding 3: an object ACE granting Users read exposes the
    /// file in both checks; its SID follows zero, one or two GUIDs.
    #[test]
    fn an_object_allow_ace_is_read_after_its_guids() {
        let (sys, adm, me) = (system(), admins(), creator());
        let t = trusted(&me, &me, &sys, &adm);
        for (flags, guids) in [(0u32, 0usize), (1, 1), (2, 1), (3, 2)] {
            let mut body = flags.to_le_bytes().to_vec();
            body.extend(std::iter::repeat_n(0xAB, guids * GUID_BYTES));
            let mut for_users = body.clone();
            for_users.extend_from_slice(&users());
            let opened = acl(&[allow(READ, &me), ace(ACCESS_ALLOWED_OBJECT, READ, &for_users)]);
            assert_eq!(check(&opened, &t, u32::MAX), Ok(true), "flags {flags}");
            let mut for_me = body;
            for_me.extend_from_slice(&me);
            let private = acl(&[allow(READ, &me), ace(ACCESS_ALLOWED_OBJECT, READ, &for_me)]);
            assert_eq!(check(&private, &t, u32::MAX), Ok(false), "flags {flags}");
        }
    }

    /// Audit rev-kimi-17 fixrev-15-k finding 6 (compound, 0x04) and rev-kimi-16 fixrev-14-k
    /// finding 3 (callback): an allow ACE whose grant this walk cannot evaluate is an error.
    #[test]
    fn conditional_compound_and_unknown_allow_aces_are_errors() {
        let (sys, adm, me) = (system(), admins(), creator());
        let t = trusted(&me, &me, &sys, &adm);
        for ace_type in [
            ACCESS_ALLOWED_COMPOUND,
            ACCESS_ALLOWED_CALLBACK,
            ACCESS_ALLOWED_CALLBACK_OBJECT,
            0x16,
            0xFF,
        ] {
            let dacl = acl(&[allow(READ, &me), ace(ace_type, READ, &users())]);
            assert!(check(&dacl, &t, u32::MAX).is_err(), "type {ace_type:#04x}");
        }
    }

    #[test]
    fn deny_audit_and_label_aces_grant_nothing() {
        let (sys, adm, me) = (system(), admins(), creator());
        let t = trusted(&me, &me, &sys, &adm);
        for ace_type in [0x01u8, 0x02, 0x03, 0x06, 0x0A, 0x11, 0x15] {
            let dacl = acl(&[allow(READ, &me), ace(ace_type, READ, &users())]);
            assert_eq!(check(&dacl, &t, u32::MAX), Ok(false), "type {ace_type:#04x}");
        }
    }

    /// `windows_acl` passes only the read/write/control bits: an ACE granting Users bits
    /// outside them (here `FILE_EXECUTE` alone, in neither `FILE_GENERIC_READ` nor
    /// `FILE_GENERIC_WRITE`) does not count there, but does for `u32::MAX`.
    #[test]
    fn the_relevant_mask_filters_grants() {
        let (sys, adm, me) = (system(), admins(), creator());
        let t = trusted(&me, &me, &sys, &adm);
        let dacl = acl(&[allow(READ, &me), allow(0x0000_0020, &users())]);
        assert_eq!(check(&dacl, &t, READ), Ok(false));
        assert_eq!(check(&dacl, &t, u32::MAX), Ok(true));
    }

    #[test]
    fn malformed_acls_are_errors() {
        let (sys, adm, me) = (system(), admins(), creator());
        let t = trusted(&me, &me, &sys, &adm);
        assert!(check(&[2, 0, 8], &t, u32::MAX).is_err(), "short header");
        let mut too_big = acl(&[allow(READ, &me)]);
        too_big[2] = 0xFF;
        assert!(check(&too_big, &t, u32::MAX).is_err(), "AclSize past the buffer");
        let mut extra_count = acl(&[allow(READ, &me)]);
        extra_count[4] = 2;
        assert!(
            check(&extra_count, &t, u32::MAX).is_err(),
            "AceCount past the ACEs"
        );
        let mut long_sid = acl(&[allow(READ, &me)]);
        long_sid[ACL_HEADER_BYTES + 8 + 1] = 15;
        assert!(check(&long_sid, &t, u32::MAX).is_err(), "SID past the ACE");
        let mut tiny_ace = acl(&[allow(READ, &me)]);
        tiny_ace[ACL_HEADER_BYTES + 2] = 2;
        assert!(
            check(&tiny_ace, &t, u32::MAX).is_err(),
            "AceSize below its header"
        );
    }
}
