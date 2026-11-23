// SPDX-License-Identifier: MPL-2.0
//! The player's refusals: a stable code plus a message. The codes are part of the JS API
//! (`book/agents/web-player.md`).

/// What the player refuses or reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebError {
    /// Lowercase ascii code, digits and `_`; one of [`ERROR_CODES`].
    pub code: &'static str,
    /// What went wrong, for a person.
    pub message: String,
}

impl WebError {
    /// A new error.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        WebError {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for WebError {}

/// A page argument from the text `JSON.stringify` gave for it. `None` (the argument was a function or a
/// symbol, which stringify to `undefined`) and text that does not parse are both `bad_argument`.
pub fn argument_json(text: Option<&str>) -> Result<serde_json::Value, WebError> {
    let text = text.ok_or_else(|| {
        WebError::new(
            "bad_argument",
            "the argument has no JSON form (a function or a symbol)",
        )
    })?;
    serde_json::from_str(text).map_err(|e| WebError::new("bad_argument", e.to_string()))
}

/// Every code the player reports, with its meaning. The book page lists the same.
pub const ERROR_CODES: &[(&str, &str)] = &[
    (
        "backend_unavailable",
        "the browser offers neither WebGPU nor WebGL2, or the requested one",
    ),
    ("bad_argument", "a call got an argument of the wrong shape"),
    ("destroyed", "the player was destroyed"),
    (
        "device_lost",
        "the GPU device or WebGL context was lost; the player restores it when it can",
    ),
    ("no_product", "the call needs an open product"),
    (
        "open_superseded",
        "a later open() replaced this one before it finished",
    ),
    ("package_fetch_failed", "a package file could not be fetched"),
    (
        "package_hash_mismatch",
        "a package file's size or hash does not match the manifest",
    ),
    (
        "package_invalid",
        "the package is damaged, or its recipe, options or materials are refused",
    ),
    (
        "package_version_unsupported",
        "the package is newer than this player reads",
    ),
    (
        "pick_not_sold",
        "the snapshot does not sell the combination this pick would make",
    ),
    ("pick_unknown_group", "the product has no such option group"),
    ("pick_unknown_value", "the option group has no such value"),
    (
        "product_not_orderable",
        "the snapshot does not sell the product at all",
    ),
    ("render_failed", "the renderer refused a frame"),
    ("studio_fetch_failed", "the studio package could not be fetched"),
    ("studio_invalid", "the studio package is damaged"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_protocol_style_and_sorted() {
        for (code, meaning) in ERROR_CODES {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
            assert!(!meaning.is_empty());
        }
        assert!(
            ERROR_CODES.windows(2).all(|w| w[0].0 < w[1].0),
            "keep the table sorted"
        );
    }

    /// Audit fix (sprint-fix-audit-jw, finding 9): `JSON.stringify` of a function or a symbol answers
    /// `undefined`; the player refuses it with `bad_argument` instead of throwing an uncoded error.
    #[test]
    fn an_argument_with_no_json_form_is_a_bad_argument() {
        let e = argument_json(None).expect_err("no text");
        assert_eq!(e.code, "bad_argument");
        assert_eq!(
            argument_json(Some("{")).expect_err("bad text").code,
            "bad_argument"
        );
        assert_eq!(
            argument_json(Some(r#"{"zoom":1}"#)).expect("parses"),
            serde_json::json!({"zoom": 1})
        );
    }
}
