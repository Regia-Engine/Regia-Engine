// SPDX-License-Identifier: MPL-2.0
//! NUL-terminated UTF-16 strings the Windows API hands back (`SHGetKnownFolderPath`,
//! `GetThreadDescription`).
//!
//! The walk is plain memory work with nothing Windows-specific in it, so it is also built for the
//! tests of every host, where Miri can run it (`docs/06-testing-and-ci.md` § 14, ADR-2726).

/// The units of the string at `text`, without the NUL.
///
/// # Safety
///
/// `text` points at readable, initialized `u16`s up to and including a NUL, and they stay valid and
/// unchanged for `'a`. It must be aligned for `u16`, as every string from the Windows API is.
pub(crate) unsafe fn until_nul<'a>(text: *const u16) -> &'a [u16] {
    let mut len = 0;
    // SAFETY: the caller's contract: every unit up to the first NUL is readable, and the walk stops
    // at that NUL, so `len` never passes it.
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
        std::slice::from_raw_parts(text, len)
    }
}

#[cfg(test)]
mod tests {
    use super::until_nul;

    /// The whole string comes back, the NUL stays out, and nothing past the NUL is read: under Miri
    /// a read of the uninitialized tail of `buffer` would be undefined behavior.
    #[test]
    fn the_walk_stops_at_the_first_nul() {
        let mut buffer = [std::mem::MaybeUninit::<u16>::uninit(); 8];
        for (slot, unit) in buffer.iter_mut().zip([0x52u16, 0x65, 0x67, 0]) {
            slot.write(unit);
        }
        // SAFETY: four initialized units, the last one a NUL; `buffer` outlives the slice.
        let units = unsafe { until_nul(buffer.as_ptr().cast()) };
        assert_eq!(units, [0x52, 0x65, 0x67]);
    }

    #[test]
    fn an_empty_string_has_no_units() {
        let text = [0u16];
        // SAFETY: one initialized NUL.
        assert!(unsafe { until_nul(text.as_ptr()) }.is_empty());
    }
}
