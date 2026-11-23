// SPDX-License-Identifier: MPL-2.0
//! Privacy helpers for what the engine writes into logs a tester sends back: the user's home
//! directory (which carries their account name) is replaced by `~`, values that read as secrets
//! are replaced by `[redacted]`, and email addresses by `[email]`.
//!
//! A home directory is found in every spelling a log or a URL gives it: either path separator,
//! JSON-escaped, after a `file://` scheme, and percent-encoded (ADR-3285, threat model finding 17).
//!
//! Crash-report fields and unclean-session recovery reports both use [`scrub_report`]. The on-disk
//! peer log and checkpoint files stay as the host wrote them (ADR-8494).

use std::borrow::Cow;
use std::sync::OnceLock;

/// The home directory spellings to scrub (`$HOME`, `%USERPROFILE%`, and the same with the other
/// path separator), longest first. Empty when the OS gives none or it is a bare root.
fn home_forms() -> &'static [String] {
    static FORMS: OnceLock<Vec<String>> = OnceLock::new();
    FORMS.get_or_init(|| {
        let mut forms = Vec::new();
        for var in ["HOME", "USERPROFILE"] {
            if let Ok(home) = crate::brand::env_var(var) {
                add_forms(&mut forms, &home);
            }
        }
        forms.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        forms.dedup();
        forms
    })
}

fn add_forms(forms: &mut Vec<String>, home: &str) {
    let home = home.trim_end_matches(['/', '\\']);
    // "/" or "C:" alone would scrub half of every path.
    if home.len() < 3 {
        return;
    }
    forms.push(home.to_owned());
    forms.push(home.replace('\\', "/"));
    forms.push(home.replace('/', "\\"));
    // The same path inside a JSON string: backslashes doubled.
    forms.push(home.replace('/', "\\").replace('\\', "\\\\"));
    // The same path in a URL: as a `file://` path (`/home/ann%20lee`, `C:/Users/ann`) and fully
    // percent-encoded (`%2Fhome%2Fann`, a query value), with upper- and lower-case hex digits.
    let forward = home.replace('\\', "/");
    for upper in [true, false] {
        forms.push(percent_encode(&forward, b"/:", upper));
        forms.push(percent_encode(&forward, b"", upper));
        forms.push(percent_encode(home, b"", upper));
    }
}

/// `text` percent-encoded: every byte but ASCII letters, digits, `-`, `.`, `_`, `~` and the bytes
/// in `keep` becomes `%XX` (`upper` picks the hex digits' case).
fn percent_encode(text: &str, keep: &[u8], upper: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for &b in text.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') || keep.contains(&b) {
            out.push(char::from(b));
        } else if upper {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push_str(&format!("%{b:02x}"));
        }
    }
    out
}

/// Every spelling of `home` the scrubber looks for (both separators, JSON-escaped, percent-encoded),
/// longest first: what [`scrub_home`] uses for the user's own home. Empty for a bare root (`/`,
/// `C:`). For [`scrub_with`] in tests and tools that scrub for another user.
#[must_use]
pub fn home_spellings(home: &str) -> Vec<String> {
    let mut forms = Vec::new();
    add_forms(&mut forms, home);
    forms.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    forms.dedup();
    forms
}

/// `text` with the user's home directory replaced by `~` (`C:\Users\<name>\x` → `~\x`,
/// `/home/<name>/x` → `~/x`). Borrows `text` when it holds no home path.
#[must_use]
pub fn scrub_home(text: &str) -> Cow<'_, str> {
    scrub_with(text, home_forms())
}

/// [`scrub_home`] for an explicit list of home spellings (tests).
#[must_use]
pub fn scrub_with<'a>(text: &'a str, homes: &[String]) -> Cow<'a, str> {
    let mut out: Cow<'a, str> = Cow::Borrowed(text);
    for home in homes {
        if home.is_empty() || find_home(&out, home).is_none() {
            continue;
        }
        out = Cow::Owned(replace_path_prefix(&out, home));
    }
    out
}

/// What stands in for a redacted value.
const REDACTED: &str = "[redacted]";

/// What stands in for a redacted email address.
const REDACTED_EMAIL: &str = "[email]";

/// How a key name ends when its value in a log line is a secret, compared without case and without
/// the `_`, `-` and `.` separators: `password=hunter2`, `"token": "abc"`, `secret_key=`,
/// `accessToken:`, `X-Api-Key:`, `Cookie:`. A key is a whole word (`secretary=` and `token_count=`
/// are not secrets).
const SECRET_KEY_ENDINGS: [&str; 13] = [
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "apikey",
    "secretkey",
    "privatekey",
    "accesskey",
    "credential",
    "credentials",
    "authorization",
    "cookie",
];

/// Key endings whose value is the rest of the line: a scheme and its credential
/// (`Basic dXNlcjpwYXNz`), a cookie list (`a=1; b=2`).
const WHOLE_LINE_KEY_ENDINGS: [&str; 2] = ["authorization", "cookie"];

/// `text` with values that read as secrets replaced by `[redacted]`: a value after a secret key
/// and `=` or `:` (also percent-encoded, `secret%5Fkey%3D...`), a `Bearer` credential, a
/// private-key block. A game's logs are not the engine's to trust, and a report leaves the
/// machine; a false positive only hides a log value. Borrows `text` when nothing matched.
#[must_use]
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    let mut out = String::new();
    let mut copied = 0;
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let Some(span) = secret_at(text, at) else {
            at += 1;
            continue;
        };
        out.push_str(&text[copied..span.0]);
        out.push_str(REDACTED);
        copied = span.1;
        at = span.1.max(at + 1);
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// [`scrub_home`], [`redact_secrets`] and [`redact_emails`] together: what a crash report or a
/// feedback bundle keeps of a log line, and what the crash uploader sends of every field.
#[must_use]
pub fn scrub_report(text: &str) -> String {
    scrub_report_with(text, home_forms())
}

/// [`scrub_report`] with an explicit list of home spellings ([`home_spellings`]), for tests and
/// tools that scrub for another user.
#[must_use]
pub fn scrub_report_with(text: &str, homes: &[String]) -> String {
    redact_emails(&redact_secrets(&scrub_with(text, homes))).into_owned()
}

/// `text` with email addresses (`ann@example.com`, percent-encoded `ann%40example.com`) replaced
/// by `[email]`. An address needs a dot in its domain and a final label of at least two letters,
/// so `user@localhost` and `crate@1.2.3` stay. Borrows `text` when nothing matched.
#[must_use]
pub fn redact_emails(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut copied = 0;
    let mut at = 0;
    while at < bytes.len() {
        let sep = if bytes[at] == b'@' {
            1
        } else if bytes[at] == b'%'
            && bytes
                .get(at + 1..at + 3)
                .is_some_and(|h| h.eq_ignore_ascii_case(b"40"))
        {
            3
        } else {
            at += 1;
            continue;
        };
        let local_start = {
            let mut i = at;
            while i > copied && is_email_local_byte(bytes[i - 1]) {
                i -= 1;
            }
            i
        };
        let domain_start = at + sep;
        let mut domain_end = domain_start;
        while domain_end < bytes.len()
            && (bytes[domain_end].is_ascii_alphanumeric() || matches!(bytes[domain_end], b'.' | b'-'))
        {
            domain_end += 1;
        }
        // Sentence punctuation after the address is not part of it.
        while domain_end > domain_start && matches!(bytes[domain_end - 1], b'.' | b'-') {
            domain_end -= 1;
        }
        let domain = &text[domain_start..domain_end];
        let tld_ok = domain.rsplit_once('.').is_some_and(|(host, tld)| {
            !host.is_empty() && tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_alphabetic())
        });
        if local_start < at && tld_ok {
            out.push_str(&text[copied..local_start]);
            out.push_str(REDACTED_EMAIL);
            copied = domain_end;
            at = domain_end;
        } else {
            at += sep;
        }
    }
    if copied == 0 && out.is_empty() {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// Whether `b` may be part of an email's local part (`ann.lee+tag`, percent escapes).
fn is_email_local_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-')
}

/// Whether `b` may be part of a key name: ASCII letters, digits and the separators `_`, `-`, `.`.
fn is_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

/// Whether `bytes` at `i` is the percent escape `%` + `hex` (either case).
fn escape_at(bytes: &[u8], i: usize, hex: &[u8; 2]) -> bool {
    bytes.get(i) == Some(&b'%')
        && bytes
            .get(i + 1..i + 3)
            .is_some_and(|h| h.eq_ignore_ascii_case(hex))
}

/// The key name starting at `at`: its end, and its letters and digits lower-cased with the
/// separators (`_`, `-`, `.`, and their escapes `%5F`, `%2D`, `%2E`) left out.
fn key_name(text: &str, at: usize) -> (usize, String) {
    let bytes = text.as_bytes();
    let mut i = at;
    let mut normalized = String::new();
    loop {
        match bytes.get(i) {
            Some(&b) if b.is_ascii_alphanumeric() => {
                normalized.push(char::from(b.to_ascii_lowercase()));
                i += 1;
            }
            Some(b'_' | b'-' | b'.') => i += 1,
            Some(b'%') if [b"5F", b"2D", b"2E"].iter().any(|hex| escape_at(bytes, i, hex)) => i += 3,
            _ => break,
        }
    }
    (i, normalized)
}

/// The byte range of the secret value starting from the key or marker at `at`.
fn secret_at(text: &str, at: usize) -> Option<(usize, usize)> {
    let rest = text.get(at..)?;
    if rest.starts_with("-----BEGIN ") {
        let header_end = rest.find('\n').unwrap_or(rest.len());
        if rest[..header_end].contains("PRIVATE KEY") {
            let end = rest.find("-----END ").map_or(rest.len(), |e| {
                rest[e + 9..].find("-----").map_or(rest.len(), |f| e + 9 + f + 5)
            });
            return Some((at, at + end));
        }
    }
    let word_start = at == 0 || !text.as_bytes()[at - 1].is_ascii_alphanumeric();
    if !word_start {
        return None;
    }
    // `Bearer`, then any run of spaces or tabs, then the credential.
    if rest.len() > 7
        && rest.as_bytes()[..6].eq_ignore_ascii_case(b"bearer")
        && matches!(rest.as_bytes()[6], b' ' | b'\t')
    {
        let gap = rest.as_bytes()[6..]
            .iter()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count();
        return value_span(text, at + 6 + gap);
    }
    let bytes = text.as_bytes();
    // A key name is a whole word: never the tail of a longer one (`x-api-key` is read from `x`).
    if at > 0 && is_key_byte(bytes[at - 1]) {
        return None;
    }
    let (key_end, key) = key_name(text, at);
    let ending = SECRET_KEY_ENDINGS.iter().find(|ending| key.ends_with(*ending))?;
    let mut i = key_end;
    while bytes.get(i).is_some_and(|b| matches!(b, b'"' | b'\'' | b' ')) {
        i += 1;
    }
    if matches!(bytes.get(i), Some(b'=' | b':')) {
        i += 1;
    } else if escape_at(bytes, i, b"3D") || escape_at(bytes, i, b"3A") {
        i += 3;
    } else {
        return None;
    }
    while bytes.get(i).is_some_and(|b| matches!(b, b' ' | b'\t')) {
        i += 1;
    }
    // A scheme and its credential (`Basic dXNlcjpwYXNz`) are two words, a cookie list is several:
    // all of the line goes.
    if WHOLE_LINE_KEY_ENDINGS.contains(ending) {
        let rest = text.get(i..)?;
        let len = rest.find(['\n', '\r']).unwrap_or(rest.len());
        return (len > 0 && !rest.starts_with(REDACTED)).then_some((i, i + len));
    }
    value_span(text, i)
}

/// How far a quoted value may run past its first line break looking for the closing quote.
const MAX_QUOTED_SPAN: usize = 4096;

/// The value at `start`, `None` when it is empty or already redacted. A quote holds separators
/// (`"a b"`, a debug print's `Some("abc")`) and line breaks up to its closing quote (within
/// `MAX_QUOTED_SPAN` bytes); otherwise the line ends the value.
fn value_span(text: &str, start: usize) -> Option<(usize, usize)> {
    let rest = text.get(start..)?;
    if rest.starts_with(REDACTED) {
        return None;
    }
    let mut quote = None;
    let mut len = rest.len();
    // The first line break seen inside an open quote: where the value ends if the quote never closes.
    let mut first_break: Option<usize> = None;
    for (i, c) in rest.char_indices() {
        if matches!(c, '\n' | '\r') {
            match (quote, first_break) {
                (Some(_), Some(at)) if i - at > MAX_QUOTED_SPAN => {
                    len = at;
                    break;
                }
                (Some(_), None) => first_break = Some(i),
                (Some(_), Some(_)) => {}
                (None, _) => {
                    len = i;
                    break;
                }
            }
            continue;
        }
        if first_break.is_some_and(|at| i - at > MAX_QUOTED_SPAN) {
            len = first_break.unwrap_or(i);
            break;
        }
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => {}
            None if matches!(c, '"' | '\'') => quote = Some(c),
            // `%26` is an encoded `&`: the next query parameter.
            None if c.is_whitespace()
                || matches!(c, ',' | ';' | '&' | '}')
                || (c == '%' && escape_at(rest.as_bytes(), i, b"26")) =>
            {
                len = i;
                break;
            }
            None => {}
        }
    }
    // A quote that never closed ends at its first line break: the log goes on after it.
    if quote.is_some()
        && let Some(at) = first_break
    {
        len = len.min(at);
    }
    (len > 0).then_some((start, start + len))
}

/// Whether `home` is a Windows path (a drive letter or a UNC share). Windows compares paths
/// without regard to ASCII case, and tools disagree on the casing they print.
fn is_windows_home(home: &str) -> bool {
    let b = home.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && (b[1] == b':' || escape_at(b, 1, b"3A")))
        || home.starts_with(r"\\")
        || home.starts_with("%5C%5C")
}

/// The first byte offset of `home` in `text`: exact for a Unix home, ASCII case-insensitive for
/// a Windows one. A match starts on a byte equal (up to ASCII case) to `home`'s first byte, which
/// is never a UTF-8 continuation byte, so the offset is a char boundary.
fn find_home(text: &str, home: &str) -> Option<usize> {
    if !is_windows_home(home) {
        return text.find(home);
    }
    let (hay, needle) = (text.as_bytes(), home.as_bytes());
    (0..=hay.len().checked_sub(needle.len())?)
        .find(|&i| hay[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// Whether `text` ends with a `file:` URL's scheme and authority (`file:`, `file://`, `file:///`,
/// `file://localhost/`, any case), so a path that follows it starts at the root.
fn after_file_scheme(text: &str) -> bool {
    let text = text.trim_end_matches('/');
    ["file:", "file://localhost"].iter().any(|scheme| {
        text.len() >= scheme.len()
            && text.as_bytes()[text.len() - scheme.len()..].eq_ignore_ascii_case(scheme.as_bytes())
            && !text.as_bytes()[..text.len() - scheme.len()]
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
    })
}

/// Replaces `home` where it starts a path component, so `/home/ann` does not rewrite
/// `/home/annabel`.
fn replace_path_prefix(text: &str, home: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = find_home(rest, home) {
        let after = &rest[at + home.len()..];
        let mut next = after.chars();
        let ends_component = match next.next() {
            None => true,
            // A dot ends the name only as sentence punctuation (end of text or before a space).
            Some('.') => next
                .next()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.')),
            Some(c) => !(c.is_alphanumeric() || c == '_' || c == '-'),
        };
        out.push_str(&rest[..at]);
        // A match inside a longer path (`/tmp/home/ann`) is not the home directory, but one right
        // after a `file:` scheme (`file:///home/ann/x`, `file://localhost/C:/Users/ann`) is.
        let starts_component = out
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '/' | '_' | '-' | '.')))
            || after_file_scheme(&out);
        if ends_component && starts_component {
            out.push('~');
        } else {
            // The text's own spelling (a Windows match may differ from `home` in case).
            out.push_str(&rest[at..at + home.len()]);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_in_log_lines_are_redacted() {
        for (line, expected) in [
            ("password=hunter2 next", "password=[redacted] next"),
            ("login with PASSWORD: hunter2", "login with PASSWORD: [redacted]"),
            (
                r#"{"token": "abc123", "n": 1}"#,
                r#"{"token": [redacted], "n": 1}"#,
            ),
            (r#"token: Some("abc")"#, "token: [redacted]"),
            (r#"password="a b" next"#, "password=[redacted] next"),
            ("api_key=k1&user=ann", "api_key=[redacted]&user=ann"),
            ("Authorization: Bearer abc.def", "Authorization: [redacted]"),
            (
                "Authorization: Basic dXNlcjpwYXNz\nnext",
                "Authorization: [redacted]\nnext",
            ),
            ("sent bearer abc.def to", "sent bearer [redacted] to"),
            ("client_secret = s3", "client_secret = [redacted]"),
        ] {
            assert_eq!(redact_secrets(line), expected, "{line}");
        }
        let key = "a\n-----BEGIN RSA PRIVATE KEY-----\nMIIE\nabc\n-----END RSA PRIVATE KEY-----\nb";
        assert_eq!(redact_secrets(key), "a\n[redacted]\nb");
    }

    #[test]
    fn ordinary_log_lines_are_left_alone() {
        for line in [
            "token_count=5",
            "the password prompt opened",
            "tokens: 12 passwords",
            "secretary=ann",
            "-----BEGIN CERTIFICATE-----",
            "password=[redacted]",
        ] {
            assert!(matches!(redact_secrets(line), Cow::Borrowed(_)), "{line}");
        }
    }

    #[test]
    fn redaction_keeps_multibyte_text_intact() {
        assert_eq!(
            redact_secrets("pässword ☃ token=ñandú end"),
            "pässword ☃ token=[redacted] end"
        );
    }

    fn homes(h: &str) -> Vec<String> {
        let mut forms = Vec::new();
        add_forms(&mut forms, h);
        forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
        forms.dedup();
        forms
    }

    #[test]
    fn a_home_inside_a_longer_path_is_not_redacted() {
        let h = homes("/home/ann");
        assert_eq!(
            scrub_with("read /tmp/home/ann/save", &h),
            "read /tmp/home/ann/save"
        );
        assert_eq!(scrub_with("read /home/ann/save", &h), "read ~/save");
    }

    #[test]
    fn windows_and_unix_homes_become_a_tilde() {
        let w = homes(r"C:\Users\ann");
        assert_eq!(
            scrub_with(r"cannot open C:\Users\ann\AppData\x.log", &w),
            r"cannot open ~\AppData\x.log"
        );
        assert_eq!(scrub_with("C:/Users/ann/AppData/x.log", &w), "~/AppData/x.log");
        let u = homes("/home/ann/");
        assert_eq!(
            scrub_with("read /home/ann/.config/f and /home/ann", &u),
            "read ~/.config/f and ~"
        );
    }

    #[test]
    fn a_longer_name_sharing_the_prefix_is_left_alone() {
        let u = homes("/home/ann");
        assert_eq!(scrub_with("/home/annabel/x", &u), "/home/annabel/x");
    }

    #[test]
    fn a_dot_ends_the_name_only_as_punctuation() {
        let u = homes("/home/ann");
        assert_eq!(scrub_with("read /home/ann.", &u), "read ~.");
        assert_eq!(scrub_with("read /home/ann. Then", &u), "read ~. Then");
        assert_eq!(scrub_with("/home/ann.bak/x", &u), "/home/ann.bak/x");
    }

    /// audit-v034b-conc M3: `Bearer` took exactly one space, and the hyphenated `api-key` header
    /// spelling was not a secret key.
    #[test]
    fn bearer_spacing_and_hyphenated_keys_are_redacted() {
        for (line, expected) in [
            ("sent Bearer  eyJabc to", "sent Bearer  [redacted] to"),
            ("sent Bearer\teyJabc to", "sent Bearer\t[redacted] to"),
            ("x-api-key: s3cr3t", "x-api-key: [redacted]"),
            ("api-key=s3cr3t&n=1", "api-key=[redacted]&n=1"),
        ] {
            assert_eq!(redact_secrets(line), expected, "{line:?}");
        }
        // `Bearer` with no value on its line marks nothing.
        assert!(matches!(redact_secrets("bearer\nnext"), Cow::Borrowed(_)));
        assert!(matches!(redact_secrets("bearer  \nnext"), Cow::Borrowed(_)));
    }

    /// audit-v034b-conc L1: a quoted value that spans lines was redacted only to the first line
    /// break; an unterminated quote still ends at the line.
    #[test]
    fn a_quoted_secret_across_lines_is_redacted_whole() {
        assert_eq!(
            redact_secrets("token=\"line1\nline2\" next"),
            "token=[redacted] next"
        );
        assert_eq!(
            redact_secrets("token=\"open\nnext line"),
            "token=[redacted]\nnext line"
        );
    }

    /// audit-v034b-conc L2: Windows paths are case-insensitive, so a home spelled with another
    /// case is still the user's home; Unix homes stay case-sensitive.
    #[test]
    fn a_windows_home_matches_in_any_case() {
        let w = homes(r"C:\Users\Ann");
        assert_eq!(scrub_with(r"open c:\users\ann\save.log", &w), r"open ~\save.log");
        assert_eq!(scrub_with("open C:/USERS/ANN/save.log", &w), "open ~/save.log");
        assert_eq!(
            scrub_with(r"open c:\users\annabel\x", &w),
            r"open c:\users\annabel\x"
        );
        let u = homes("/home/Ann");
        assert_eq!(scrub_with("/home/ann/x", &u), "/home/ann/x");
    }

    #[test]
    fn text_without_a_home_borrows_and_a_bare_root_is_never_a_home() {
        let u = homes("/home/ann");
        assert!(matches!(scrub_with("nothing here", &u), Cow::Borrowed(_)));
        assert!(homes("/").is_empty());
        assert!(homes("C:").is_empty());
    }
}
