// SPDX-License-Identifier: MPL-2.0
//! The code-signature page check for Mach-O libraries (ADR-3373).
//!
//! On Apple silicon every page of executable code must match its hash in the library's code
//! signature. The kernel checks each page when it is first mapped in and kills the whole process
//! (SIGKILL) on a mismatch, so a damaged extension file that the dynamic loader opens ends the
//! editor instead of being refused. [`check_code_signature`] does the same comparison on the file
//! bytes before anything is loaded: a library whose signed pages do not match is
//! `not_an_extension`, like any other damaged file.
//!
//! The constants and field offsets read here (the Mach-O and universal headers, the
//! `LC_CODE_SIGNATURE` load command, the embedded-signature super blob, the code directory and
//! its page hash slots) match LLVM's `llvm/include/llvm/BinaryFormat/MachO.h` and `MachO.def`
//! (Apache-2.0 WITH LLVM-exception), from which they were re-derived (ADR-4931; see
//! `docs/provenance.md`). The code is original. Only what the check needs is read,
//! with every offset bounds-checked: the `LC_CODE_SIGNATURE` load command, the super blob's index,
//! and each code directory's page size, code limit, hash type and code slots. A code directory
//! with a hash this check does not compute (SHA-1, SHA-384) or with a scatter list is left to the
//! dynamic loader, as are files that are not Mach-O (ELF, PE) and Mach-O files with no
//! signature.

use crate::kit::Sha256;

const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_MAGIC: u32 = 0xfeed_face;
const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;
const LC_CODE_SIGNATURE: u32 = 0x1d;
const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
const CSMAGIC_CODEDIRECTORY: u32 = 0xfade_0c02;
const CSSLOT_CODEDIRECTORY: u32 = 0;
const CSSLOT_ALTERNATE_CODEDIRECTORIES: u32 = 0x1000;
const CSSLOT_ALTERNATE_CODEDIRECTORY_MAX: u32 = 5;
const CS_HASHTYPE_SHA256: u8 = 2;
const CS_HASHTYPE_SHA256_TRUNCATED: u8 = 3;
const CS_SUPPORTSSCATTER: u32 = 0x2_0100;
const CS_SUPPORTSCODELIMIT64: u32 = 0x2_0300;
/// More architectures than any real universal file holds; a larger count is a damaged header.
const MAX_FAT_ARCHES: u32 = 64;

/// What [`check_code_signature`] found in a file it does not refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureCheck {
    /// Not a Mach-O file (ELF, PE, anything else): there is nothing to check.
    NotMachO,
    /// A Mach-O file (or every slice of a universal one) with no `LC_CODE_SIGNATURE`.
    Unsigned,
    /// Every SHA-256 code page hash matched; `pages` hashes were compared over all slices.
    Verified {
        /// Code page hashes compared.
        pages: usize,
    },
    /// Signed, but with no code directory this check can verify (another hash type or a
    /// scatter list): the dynamic loader decides.
    NotChecked,
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    Some(u32::from_be_bytes(bytes.get(at..end)?.try_into().ok()?))
}

fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    Some(u32::from_le_bytes(bytes.get(at..end)?.try_into().ok()?))
}

fn be64(bytes: &[u8], at: usize) -> Option<u64> {
    let end = at.checked_add(8)?;
    Some(u64::from_be_bytes(bytes.get(at..end)?.try_into().ok()?))
}

fn damaged(what: &str) -> String {
    format!("the Mach-O code signature is damaged: {what}")
}

/// Checks the code-signature page hashes of a Mach-O file, or of every slice of a universal
/// file. Not a Mach-O file, or no signature: nothing to check.
///
/// # Errors
///
/// A signed page whose bytes no longer match its hash, or a signature whose structure points
/// outside the file. The message names the slice offset and the page.
pub fn check_code_signature(bytes: &[u8]) -> Result<SignatureCheck, String> {
    let Some(magic_be) = be32(bytes, 0) else {
        return Ok(SignatureCheck::NotMachO);
    };
    if magic_be == FAT_MAGIC || magic_be == FAT_MAGIC_64 {
        let wide = magic_be == FAT_MAGIC_64;
        let count = be32(bytes, 4).ok_or_else(|| damaged("short universal header"))?;
        if count == 0 || count > MAX_FAT_ARCHES {
            // `0xcafebabe` also starts other formats; with no sane arch count it is not a
            // universal library.
            return Ok(SignatureCheck::NotMachO);
        }
        let entry = if wide { 32 } else { 20 };
        let mut total = Overall::default();
        for i in 0..count as usize {
            let at = 8 + i * entry;
            let (offset, size) = if wide {
                (be64(bytes, at + 8), be64(bytes, at + 16))
            } else {
                (
                    be32(bytes, at + 8).map(u64::from),
                    be32(bytes, at + 12).map(u64::from),
                )
            };
            let (Some(offset), Some(size)) = (offset, size) else {
                return Err(damaged("short universal arch table"));
            };
            let start = usize::try_from(offset).map_err(|_| damaged("slice offset"))?;
            let end = usize::try_from(offset.saturating_add(size)).map_err(|_| damaged("slice size"))?;
            let slice = bytes
                .get(start..end)
                .ok_or_else(|| damaged("a slice lies outside the file"))?;
            total.add(check_thin(slice, start)?);
        }
        return Ok(total.result());
    }
    match le32(bytes, 0) {
        Some(MH_MAGIC_64 | MH_MAGIC) => check_thin(bytes, 0),
        _ => Ok(SignatureCheck::NotMachO),
    }
}

/// Folds the slices of a universal file into one result: any verified page counts.
#[derive(Default)]
struct Overall {
    pages: usize,
    signed: bool,
    checked: bool,
    macho: bool,
}

impl Overall {
    fn add(&mut self, slice: SignatureCheck) {
        match slice {
            SignatureCheck::NotMachO => {}
            SignatureCheck::Unsigned => self.macho = true,
            SignatureCheck::NotChecked => {
                self.macho = true;
                self.signed = true;
            }
            SignatureCheck::Verified { pages } => {
                self.macho = true;
                self.signed = true;
                self.checked = true;
                self.pages += pages;
            }
        }
    }

    fn result(&self) -> SignatureCheck {
        if self.checked {
            SignatureCheck::Verified { pages: self.pages }
        } else if self.signed {
            SignatureCheck::NotChecked
        } else if self.macho {
            SignatureCheck::Unsigned
        } else {
            SignatureCheck::NotMachO
        }
    }
}

/// One Mach-O image; `base` is its offset in the file (for messages only).
fn check_thin(image: &[u8], base: usize) -> Result<SignatureCheck, String> {
    let header = match le32(image, 0) {
        Some(MH_MAGIC_64) => 32,
        Some(MH_MAGIC) => 28,
        _ => return Ok(SignatureCheck::NotMachO),
    };
    let ncmds = le32(image, 16).ok_or_else(|| damaged("short Mach-O header"))?;
    let mut at = header;
    let mut signature = None;
    for _ in 0..ncmds {
        let cmd = le32(image, at).ok_or_else(|| damaged("load commands run past the file"))?;
        let size = le32(image, at + 4).ok_or_else(|| damaged("load commands run past the file"))? as usize;
        if size < 8 {
            return Err(damaged("a load command is shorter than its header"));
        }
        if cmd == LC_CODE_SIGNATURE {
            let off = le32(image, at + 8).ok_or_else(|| damaged("short LC_CODE_SIGNATURE"))? as usize;
            let len = le32(image, at + 12).ok_or_else(|| damaged("short LC_CODE_SIGNATURE"))? as usize;
            signature = Some((off, len));
        }
        at = at.checked_add(size).ok_or_else(|| damaged("load command size"))?;
    }
    let Some((off, len)) = signature else {
        return Ok(SignatureCheck::Unsigned);
    };
    let blob = off
        .checked_add(len)
        .and_then(|end| image.get(off..end))
        .ok_or_else(|| damaged("the signature lies outside the file"))?;
    if be32(blob, 0) != Some(CSMAGIC_EMBEDDED_SIGNATURE) {
        return Err(damaged("no embedded-signature magic"));
    }
    let count = be32(blob, 8).ok_or_else(|| damaged("short super blob"))?;
    let mut pages = 0usize;
    let mut checked = false;
    for i in 0..count as usize {
        let index = 12 + i * 8;
        let slot = be32(blob, index).ok_or_else(|| damaged("short super blob index"))?;
        let blob_at = be32(blob, index + 4).ok_or_else(|| damaged("short super blob index"))? as usize;
        let alternate = (CSSLOT_ALTERNATE_CODEDIRECTORIES
            ..CSSLOT_ALTERNATE_CODEDIRECTORIES + CSSLOT_ALTERNATE_CODEDIRECTORY_MAX)
            .contains(&slot);
        if slot != CSSLOT_CODEDIRECTORY && !alternate {
            continue;
        }
        if let Some(compared) = check_code_directory(image, blob, blob_at, base)? {
            pages += compared;
            checked = true;
        }
    }
    Ok(if checked {
        SignatureCheck::Verified { pages }
    } else {
        SignatureCheck::NotChecked
    })
}

/// Compares one code directory's code page hashes; `None` when it uses a hash this check does
/// not compute or a scatter list.
fn check_code_directory(image: &[u8], blob: &[u8], at: usize, base: usize) -> Result<Option<usize>, String> {
    let field = |offset: usize| be32(blob, at + offset).ok_or_else(|| damaged("short code directory"));
    if field(0)? != CSMAGIC_CODEDIRECTORY {
        return Err(damaged("a code directory slot has no code directory magic"));
    }
    let version = field(8)?;
    let hash_offset = field(16)? as usize;
    let code_slots = field(28)? as usize;
    let mut code_limit = u64::from(field(32)?);
    let tail = blob
        .get(at + 36..at + 40)
        .ok_or_else(|| damaged("short code directory"))?;
    let (hash_size, hash_type, page_shift) = (usize::from(tail[0]), tail[1], tail[3]);
    if version >= CS_SUPPORTSSCATTER && field(44)? != 0 {
        return Ok(None);
    }
    if version >= CS_SUPPORTSCODELIMIT64 {
        let wide = be64(blob, at + 56).ok_or_else(|| damaged("short code directory"))?;
        if wide != 0 {
            code_limit = wide;
        }
    }
    if hash_type != CS_HASHTYPE_SHA256 && hash_type != CS_HASHTYPE_SHA256_TRUNCATED {
        return Ok(None);
    }
    if hash_size == 0 || hash_size > 32 || page_shift >= 32 {
        return Err(damaged("hash size or page size out of range"));
    }
    let code_limit = usize::try_from(code_limit).map_err(|_| damaged("code limit"))?;
    if code_limit > image.len() {
        return Err(damaged("the signed range runs past the file"));
    }
    // A page size of 0 means one page covering the whole signed range.
    let page = if page_shift == 0 {
        code_limit.max(1)
    } else {
        1usize << page_shift
    };
    if code_slots != code_limit.div_ceil(page) {
        return Err(damaged("the code slot count does not cover the signed range"));
    }
    for i in 0..code_slots {
        let start = i * page;
        let end = (start + page).min(code_limit);
        let want_at = at + hash_offset + i * hash_size;
        let want = blob
            .get(want_at..want_at + hash_size)
            .ok_or_else(|| damaged("a page hash lies outside the signature"))?;
        let got = Sha256::digest(&image[start..end]);
        if got[..hash_size] != *want {
            return Err(format!(
                "code signature page {i} (bytes {}..{} of the file) does not match its hash: the file was changed after it was signed, and the system would stop the process that maps it",
                base + start,
                base + end
            ));
        }
    }
    Ok(Some(code_slots))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    const PAGE_SHIFT: u8 = 12;

    /// A minimal thin 64-bit Mach-O with `body` after the header and an embedded signature of
    /// one SHA-256 code directory (and, when `alternate`, a second one in an alternate slot),
    /// laid out as `CS_SuperBlob` and `CS_CodeDirectory` in LLVM's `BinaryFormat/MachO.h`.
    pub(in crate::extension) fn signed_image(body: &[u8], hash_type: u8, alternate: bool) -> Vec<u8> {
        signed_image_with(&[], 0, body, hash_type, alternate)
    }

    /// A signed Mach-O dylib that exports `stamp` as `_REGIA_EXTENSION_STAMP` (an `LC_SEGMENT_64`
    /// over the file and an `LC_SYMTAB`, as the stamp reader requires since ADR-2844), with
    /// `filler` bytes of code in front of it. The load commands sit in page 0.
    pub(in crate::extension) fn signed_stamped_dylib(filler: &[u8], stamp: &[u8]) -> Vec<u8> {
        const BASE: u64 = 0x1_0000_0000;
        const SEGMENT: usize = 72;
        const SYMTAB: usize = 24;
        // Header, the two commands above and the LC_CODE_SIGNATURE `signed_image_with` adds.
        let body_at = 32 + SEGMENT + SYMTAB + 16;
        let mut body = filler.to_vec();
        while !(body_at + body.len()).is_multiple_of(16) {
            body.push(0);
        }
        let stamp_at = body_at + body.len();
        body.extend_from_slice(stamp);
        while !(body_at + body.len()).is_multiple_of(8) {
            body.push(0);
        }
        let symoff = body_at + body.len();
        body.extend_from_slice(&1u32.to_le_bytes()); // n_strx
        body.extend_from_slice(&[0x0f, 1]); // N_SECT | N_EXT, section 1
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&(BASE + stamp_at as u64).to_le_bytes());
        let stroff = body_at + body.len();
        let strings = b"\0_REGIA_EXTENSION_STAMP\0";
        body.extend_from_slice(strings);
        let total = (body_at + body.len()) as u64;
        let mut commands = Vec::new();
        commands.extend_from_slice(&0x19u32.to_le_bytes()); // LC_SEGMENT_64, no sections
        commands.extend_from_slice(&(SEGMENT as u32).to_le_bytes());
        commands.extend_from_slice(b"__TEXT\0\0\0\0\0\0\0\0\0\0");
        for value in [BASE, total, 0, total] {
            commands.extend_from_slice(&value.to_le_bytes());
        }
        for value in [5u32, 5, 0, 0] {
            commands.extend_from_slice(&value.to_le_bytes());
        }
        for value in [2u32, SYMTAB as u32, symoff as u32, 1, stroff as u32] {
            commands.extend_from_slice(&value.to_le_bytes());
        }
        commands.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        signed_image_with(&commands, 2, &body, CS_HASHTYPE_SHA256, false)
    }

    /// [`signed_image`] with `commands` (`count` load commands) in front of the
    /// `LC_CODE_SIGNATURE` command.
    fn signed_image_with(
        commands: &[u8],
        count: u32,
        body: &[u8],
        hash_type: u8,
        alternate: bool,
    ) -> Vec<u8> {
        let mut image = Vec::new();
        image.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
        image.extend_from_slice(&0x0100_000c_u32.to_le_bytes()); // arm64
        image.extend_from_slice(&0u32.to_le_bytes());
        image.extend_from_slice(&6u32.to_le_bytes()); // MH_DYLIB
        image.extend_from_slice(&(count + 1).to_le_bytes()); // ncmds
        image.extend_from_slice(&(commands.len() as u32 + 16).to_le_bytes()); // sizeofcmds
        image.extend_from_slice(&0u32.to_le_bytes());
        image.extend_from_slice(&0u32.to_le_bytes());
        image.extend_from_slice(commands);
        let command_at = image.len();
        image.extend_from_slice(&[0; 16]);
        image.extend_from_slice(body);
        while !image.len().is_multiple_of(16) {
            image.push(0);
        }
        let code_limit = image.len();
        let page = 1usize << PAGE_SHIFT;
        let slots = code_limit.div_ceil(page);
        let hash_offset = 88usize;
        let directory_len = hash_offset + 32 * slots;
        let slot_types: &[u32] = if alternate {
            &[CSSLOT_CODEDIRECTORY, CSSLOT_ALTERNATE_CODEDIRECTORIES]
        } else {
            &[CSSLOT_CODEDIRECTORY]
        };
        let index_len = 12 + 8 * slot_types.len();
        let blob_len = index_len + directory_len * slot_types.len();
        // The load command lies inside the signed range, so it is written before hashing.
        let mut command = Vec::new();
        for value in [LC_CODE_SIGNATURE, 16, code_limit as u32, blob_len as u32] {
            command.extend_from_slice(&value.to_le_bytes());
        }
        image[command_at..command_at + 16].copy_from_slice(&command);
        let mut directory = Vec::new();
        for value in [
            CSMAGIC_CODEDIRECTORY,
            directory_len as u32,
            CS_SUPPORTSCODELIMIT64,
            0,
            hash_offset as u32,
            0,
            0,
            slots as u32,
            code_limit as u32,
        ] {
            directory.extend_from_slice(&value.to_be_bytes());
        }
        directory.extend_from_slice(&[32, hash_type, 0, PAGE_SHIFT]);
        // spare2, scatterOffset, teamOffset, spare3, codeLimit64: all zero.
        directory.resize(hash_offset, 0);
        for i in 0..slots {
            let end = ((i + 1) * page).min(code_limit);
            directory.extend_from_slice(&Sha256::digest(&image[i * page..end]));
        }
        let mut blob = Vec::new();
        for value in [
            CSMAGIC_EMBEDDED_SIGNATURE,
            blob_len as u32,
            slot_types.len() as u32,
        ] {
            blob.extend_from_slice(&value.to_be_bytes());
        }
        for (i, slot) in slot_types.iter().enumerate() {
            blob.extend_from_slice(&slot.to_be_bytes());
            blob.extend_from_slice(&((index_len + i * directory_len) as u32).to_be_bytes());
        }
        for _ in slot_types {
            blob.extend_from_slice(&directory);
        }
        image.extend_from_slice(&blob);
        image
    }

    fn body(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn an_intact_signed_library_verifies_every_page() {
        let image = signed_image(&body(10_000), CS_HASHTYPE_SHA256, false);
        assert_eq!(
            check_code_signature(&image),
            Ok(SignatureCheck::Verified { pages: 3 })
        );
        let both = signed_image(&body(10_000), CS_HASHTYPE_SHA256, true);
        assert_eq!(
            check_code_signature(&both),
            Ok(SignatureCheck::Verified { pages: 6 })
        );
    }

    /// The Mac pass's `flip_magic` case: one changed byte inside the signed range is refused
    /// before the loader runs, naming the page.
    #[test]
    fn one_changed_byte_in_a_signed_page_is_refused() {
        let image = signed_image(&body(10_000), CS_HASHTYPE_SHA256, false);
        for at in [100usize, 4096, 9000] {
            let mut damaged = image.clone();
            damaged[at] ^= 0x41;
            let error = check_code_signature(&damaged).expect_err("refused");
            let page = at / 4096;
            assert!(error.contains(&format!("page {page} ")), "{error}");
        }
    }

    #[test]
    fn other_formats_and_unsigned_images_pass_through() {
        assert_eq!(
            check_code_signature(b"\x7fELF and the rest"),
            Ok(SignatureCheck::NotMachO)
        );
        assert_eq!(check_code_signature(b"MZ"), Ok(SignatureCheck::NotMachO));
        assert_eq!(check_code_signature(&[]), Ok(SignatureCheck::NotMachO));
        let mut unsigned = signed_image(&body(100), CS_HASHTYPE_SHA256, false);
        unsigned[16..20].copy_from_slice(&0u32.to_le_bytes()); // ncmds = 0
        assert_eq!(check_code_signature(&unsigned), Ok(SignatureCheck::Unsigned));
        // A hash this check does not compute is left to the loader.
        let sha1 = signed_image(&body(100), 1, false);
        assert_eq!(check_code_signature(&sha1), Ok(SignatureCheck::NotChecked));
    }

    #[test]
    fn a_universal_file_checks_each_slice() {
        let slice = signed_image(&body(5_000), CS_HASHTYPE_SHA256, false);
        let mut fat = Vec::new();
        fat.extend_from_slice(&FAT_MAGIC.to_be_bytes());
        fat.extend_from_slice(&1u32.to_be_bytes());
        let offset = 4096u32;
        for value in [0x0100_000c_u32, 0, offset, slice.len() as u32, 12] {
            fat.extend_from_slice(&value.to_be_bytes());
        }
        fat.resize(offset as usize, 0);
        fat.extend_from_slice(&slice);
        assert_eq!(
            check_code_signature(&fat),
            Ok(SignatureCheck::Verified { pages: 2 })
        );
        fat[offset as usize + 200] ^= 1;
        assert!(check_code_signature(&fat).is_err());
    }

    /// Damaged structure is refused, never read out of bounds: every truncation of a signed
    /// image either passes through, verifies, or errs, and none panics.
    #[test]
    fn truncated_and_corrupt_signatures_never_panic() {
        let image = signed_image(&body(6_000), CS_HASHTYPE_SHA256, true);
        for len in (0..image.len()).step_by(7) {
            let _ = check_code_signature(&image[..len]);
        }
        let mut wild = image.clone();
        let n = wild.len();
        for at in (0..n).step_by(13) {
            wild[at] = 0xff;
            let _ = check_code_signature(&wild);
        }
    }
}
