// SPDX-License-Identifier: MPL-2.0
//! The one pretty-printer configuration every RON writer in the workspace uses.
//!
//! `ron` writes the platform line ending by default (`\r\n` on Windows). Every file Regia
//! writes must be byte-identical on Linux, Windows, macOS and Android, so content hashes,
//! diffs and golden comparisons agree across hosts. Route every pretty RON writer through
//! [`ron_pretty()`]; the `xtask` lint `ron-pretty` fails on a direct `PrettyConfig::default()`
//! or `PrettyConfig::new()` outside this file.

use ron::ser::PrettyConfig;

/// A [`PrettyConfig`] with the default layout and a `"\n"` line ending on every platform.
///
/// Chain further options (`.struct_names(true)`, `.indentor(..)`) onto the result as needed.
#[must_use]
pub fn ron_pretty() -> PrettyConfig {
    PrettyConfig::default().new_line("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize)]
    struct Sample {
        a: u32,
        b: Vec<u32>,
    }

    #[test]
    fn ron_pretty_writes_lf_only() {
        let text =
            ron::ser::to_string_pretty(&Sample { a: 1, b: vec![2, 3] }, ron_pretty()).unwrap_or_default();
        assert!(text.contains('\n'), "multi-line output expected: {text:?}");
        assert!(!text.contains('\r'), "no carriage returns: {text:?}");
    }

    #[test]
    fn ron_pretty_matches_default_layout_on_lf_hosts() {
        let value = Sample { a: 7, b: vec![1] };
        let ours = ron::ser::to_string_pretty(&value, ron_pretty()).unwrap_or_default();
        let theirs =
            ron::ser::to_string_pretty(&value, PrettyConfig::default().new_line("\n")).unwrap_or_default();
        assert_eq!(ours, theirs);
    }
}
