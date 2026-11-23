// SPDX-License-Identifier: MPL-2.0
//! Regression tests for audit-v035-formats findings in `regia_asset` (each failed before its fix):
//!
//! - 11: `content-manifest.ron` with `format_version: 0` loaded as a current file (only a newer
//!   peeked version was refused, and the parsed version was never rechecked).

use regia_asset::content::{CONTENT_MANIFEST_FORMAT_VERSION, ContentManifest};

#[test]
fn a_content_manifest_with_version_zero_is_refused() {
    let text = ContentManifest::new("1.0.0-1-abc", "linux")
        .to_ron_string()
        .expect("serialize");
    let current = format!("format_version: {CONTENT_MANIFEST_FORMAT_VERSION}");
    assert!(text.contains(&current), "{text}");
    ContentManifest::from_ron_str(&text).expect("the current manifest loads");
    let zero = text.replace(&current, "format_version: 0");
    let err = ContentManifest::from_ron_str(&zero).expect_err("a zero version word is damage");
    assert!(err.to_string().contains('0'), "{err}");
}
