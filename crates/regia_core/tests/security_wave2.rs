// SPDX-License-Identifier: MPL-2.0
//! Threat-model finding 17 (ADR-3285): the scrubber kept the user name after a `file://` scheme and
//! let common secret-key spellings through. The first two tests are the threat-model reproducers,
//! un-ignored; the tables cover each form, case variants and URL-encoded forms.

use std::borrow::Cow;

use regia_core::privacy::{home_spellings, redact_emails, redact_secrets, scrub_report_with, scrub_with};

/// Finding 17: the home-folder rule skipped a match after `/`, so `file:///home/ann/x` kept the name.
#[test]
fn finding_17_a_file_url_keeps_no_home_folder() {
    let homes = ["/home/ann".to_string()];
    let scrubbed = scrub_with("cannot open file:///home/ann/project/x.ron", &homes);
    assert!(!scrubbed.contains("/home/ann"), "{scrubbed}");
}

/// Finding 17: a secret key name had to stand alone as a word, so these values stayed in the text.
#[test]
fn finding_17_common_secret_key_spellings_are_redacted() {
    for line in [
        "secret_key=abc123",
        "accesstoken=abc123",
        "sessionToken: abc123",
        "Cookie: abc123",
    ] {
        let scrubbed = redact_secrets(line);
        assert!(!scrubbed.contains("abc123"), "{line}: {scrubbed}");
    }
}

#[test]
fn a_home_folder_in_every_url_form_is_scrubbed() {
    let unix = home_spellings("/home/ann");
    let spaced = home_spellings("/home/ann lee");
    let windows = home_spellings(r"C:\Users\ann");
    for (homes, text, expected) in [
        (&unix, "file:///home/ann/x.ron", "file://~/x.ron"),
        (&unix, "FILE:///home/ann/x.ron", "FILE://~/x.ron"),
        (&unix, "file:/home/ann/x.ron", "file:~/x.ron"),
        (&unix, "file://localhost/home/ann/x", "file://localhost~/x"),
        (&unix, "open <file:///home/ann>", "open <file://~>"),
        (&unix, "path=%2Fhome%2Fann%2Fx.ron", "path=~%2Fx.ron"),
        (&unix, "path=%2fhome%2fann%2fx.ron", "path=~%2fx.ron"),
        (&spaced, "file:///home/ann%20lee/x", "file://~/x"),
        (&spaced, "file:///home/ann%20Lee/x", "file:///home/ann%20Lee/x"),
        (&windows, "file:///C:/Users/ann/x.log", "file:///~/x.log"),
        (&windows, "file:///c:/users/ANN/x.log", "file:///~/x.log"),
        (&windows, "q=C%3A%5CUsers%5Cann%5Cx", "q=~%5Cx"),
        (&windows, "q=c%3a%5cusers%5cann%5cx", "q=~%5cx"),
    ] {
        assert_eq!(scrub_with(text, homes), expected, "{text}");
    }
}

#[test]
fn a_file_url_of_another_folder_or_scheme_is_left_alone() {
    let unix = home_spellings("/home/ann");
    for text in [
        "file:///tmp/home/ann/x",
        "file:///home/annabel/x",
        "profile:///home/annx",
        "myfile:///home/ann2",
        // A scheme glued to a longer word is not `file:`: the path stays as text after a `/`.
        "myfile:///home/ann/x",
    ] {
        assert_eq!(scrub_with(text, &unix), text);
    }
}

#[test]
fn every_secret_key_form_is_redacted() {
    for (line, expected) in [
        // Separators and word forms.
        ("secret_key=abc123 next", "secret_key=[redacted] next"),
        ("secret-key: abc123", "secret-key: [redacted]"),
        ("secret.key=abc123", "secret.key=[redacted]"),
        ("secretKey=abc123", "secretKey=[redacted]"),
        ("aws_secret_access_key=abc123", "aws_secret_access_key=[redacted]"),
        ("private_key=abc123", "private_key=[redacted]"),
        ("access_key_id_secret=abc123", "access_key_id_secret=[redacted]"),
        ("accesstoken=abc123", "accesstoken=[redacted]"),
        ("access_token=abc123&user=ann", "access_token=[redacted]&user=ann"),
        ("refresh-token: abc123", "refresh-token: [redacted]"),
        ("sessionToken: abc123", "sessionToken: [redacted]"),
        ("session_token=abc123", "session_token=[redacted]"),
        ("--token=abc123 --verbose", "--token=[redacted] --verbose"),
        ("db.password = abc123", "db.password = [redacted]"),
        ("passphrase: abc123", "passphrase: [redacted]"),
        ("credentials=abc123", "credentials=[redacted]"),
        (
            r#"{"client_secret": "abc123"}"#,
            r#"{"client_secret": [redacted]}"#,
        ),
        // Case variants.
        ("SECRET_KEY=abc123", "SECRET_KEY=[redacted]"),
        ("Secret-Key: abc123", "Secret-Key: [redacted]"),
        ("ACCESSTOKEN=abc123", "ACCESSTOKEN=[redacted]"),
        ("AccessToken: abc123", "AccessToken: [redacted]"),
        ("SESSIONTOKEN: abc123", "SESSIONTOKEN: [redacted]"),
        ("X-API-KEY: abc123", "X-API-KEY: [redacted]"),
        // Cookies keep nothing of the line.
        ("Cookie: abc123", "Cookie: [redacted]"),
        ("cookie: sid=abc123; theme=dark\nnext", "cookie: [redacted]\nnext"),
        ("COOKIE: sid=abc123", "COOKIE: [redacted]"),
        ("Set-Cookie: sid=abc123; HttpOnly", "Set-Cookie: [redacted]"),
        // URL-encoded keys and separators.
        (
            "q=1&secret%5Fkey%3Dabc123&n=2",
            "q=1&secret%5Fkey%3D[redacted]&n=2",
        ),
        ("q=1&secret%5fkey%3dabc123", "q=1&secret%5fkey%3d[redacted]"),
        ("access%2Dtoken%3Aabc123", "access%2Dtoken%3A[redacted]"),
        (
            "next=%2Fx%3Ftoken%3Dabc123%26user%3Dann",
            "next=%2Fx%3Ftoken%3D[redacted]%26user%3Dann",
        ),
        ("session%5Ftoken=abc123", "session%5Ftoken=[redacted]"),
    ] {
        assert_eq!(redact_secrets(line), expected, "{line}");
    }
}

#[test]
fn key_names_that_only_contain_a_secret_word_are_left_alone() {
    for line in [
        "token_count=5",
        "tokens: 12",
        "secretary=ann",
        "cookies_eaten=3",
        "password_hint_shown: true",
        "max_tokens=4096",
        "pwd=/srv/game",
        "the password prompt opened",
        "token%5Fcount%3D5",
    ] {
        assert!(matches!(redact_secrets(line), Cow::Borrowed(_)), "{line}");
    }
}

#[test]
fn email_addresses_are_redacted_in_plain_and_encoded_form() {
    for (line, expected) in [
        ("from ann@example.com today", "from [email] today"),
        ("<Ann.Lee+tag@Mail.Example.co.uk>", "<[email]>"),
        ("mailto:ann@example.org.", "mailto:[email]."),
        ("q=ann%40example.com&x=1", "q=[email]&x=1"),
        ("q=ann%40EXAMPLE.COM", "q=[email]"),
        ("a@b.io and c@d.io", "[email] and [email]"),
    ] {
        assert_eq!(redact_emails(line), expected, "{line}");
    }
    for line in [
        "user@localhost",
        "serde@1.0.200",
        "@everyone",
        "a @ b.com",
        "x@.com",
    ] {
        assert!(matches!(redact_emails(line), Cow::Borrowed(_)), "{line}");
    }
}

/// The function the crash reporter sends every field through removes all three kinds at once.
#[test]
fn the_report_scrubber_removes_a_home_a_token_and_an_email_together() {
    let homes = home_spellings("/home/ann");
    let text = "panicked at /home/ann/game/src/main.rs:3: token=abc123 sent to ann@example.com\n\
                file:///home/ann/x Cookie: sid=abc123";
    let scrubbed = scrub_report_with(text, &homes);
    for planted in ["/home/ann", "abc123", "ann@example.com"] {
        assert!(!scrubbed.contains(planted), "{planted}: {scrubbed}");
    }
    assert!(scrubbed.contains("~/game/src/main.rs:3"), "{scrubbed}");
}
