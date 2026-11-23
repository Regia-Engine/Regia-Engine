// SPDX-License-Identifier: MPL-2.0
//! Locale hints (ADR-1618): the raw language values the host's subsystems know before
//! `regia_ui` picks the start-up locale. The saved settings plugin writes [`LocaleHints::settings`],
//! the Steam plugin writes [`LocaleHints::steam`]; the launch flag, the environment override and
//! the OS locale are read here, unless a test sets [`LocaleHints::isolated`].
//!
//! Nothing in this module decides anything: the order and the match against the loaded locales
//! live in `regia_ui::locale_startup`.

use regia_app::prelude::*;

/// Environment variable that overrides the locale (CI, tests). Read through
/// [`regia_core::brand::env_var`], so the pre-rename spelling works too.
pub const LOCALE_ENV: &str = "REGIA_LOCALE";

/// The launch flag that names the locale (`--locale fr-CA` or `--locale=fr-CA`).
pub const LOCALE_FLAG: &str = "--locale";

/// Raw language values by source, in the order the start-up picks them (the flag first). Empty
/// strings count as absent. Insert it before `Startup`; plugins use
/// `get_resource_or_insert_with`, so any order of plugins works.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct LocaleHints {
    /// A launch flag value; `None` reads the process arguments.
    pub flag: Option<String>,
    /// The environment override; `None` reads [`LOCALE_ENV`].
    pub env: Option<String>,
    /// The saved settings language (`GameSettings::language`); written by the settings plugin.
    pub settings: Option<String>,
    /// The Steam per-game language as a BCP 47 tag; written by the Steam plugin when connected.
    pub steam: Option<String>,
    /// The OS locale; `None` reads [`crate::system_locale`].
    pub os: Option<String>,
    /// Tests: when `true` the process arguments, the environment and the OS are not read, so
    /// only the fields above count.
    pub isolated: bool,
}

impl LocaleHints {
    /// Hints that read nothing from the real process: tests fill the fields they want.
    #[must_use]
    pub fn isolated() -> Self {
        Self {
            isolated: true,
            ..Self::default()
        }
    }

    /// The flag value: the field, else (unless isolated) the process arguments.
    #[must_use]
    pub fn flag_value(&self) -> Option<String> {
        non_empty(self.flag.as_deref()).or_else(|| {
            if self.isolated {
                None
            } else {
                // `std::env::args` panics on an argument that is not valid Unicode (a Latin-1
                // file name dropped onto the executable); read the OS strings lossily instead.
                flag_from_args(lossy_args(std::env::args_os()))
            }
        })
    }

    /// The environment override: the field, else (unless isolated) [`LOCALE_ENV`].
    #[must_use]
    pub fn env_value(&self) -> Option<String> {
        non_empty(self.env.as_deref()).or_else(|| {
            if self.isolated {
                None
            } else {
                non_empty(regia_core::brand::env_var(LOCALE_ENV).ok().as_deref())
            }
        })
    }

    /// The OS locale: the field, else (unless isolated) [`crate::system_locale`].
    #[must_use]
    pub fn os_value(&self) -> Option<String> {
        non_empty(self.os.as_deref()).or_else(|| {
            if self.isolated {
                None
            } else {
                non_empty(crate::system_locale().as_deref())
            }
        })
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned)
}

/// Process arguments as text, with invalid Unicode replaced by U+FFFD instead of a panic.
fn lossy_args<I>(args: I) -> impl Iterator<Item = String>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    args.into_iter().map(|a| a.to_string_lossy().into_owned())
}

/// The value of `--locale <tag>` or `--locale=<tag>` in `args` (the first argument is the
/// program name). The last occurrence wins; a flag without a value is ignored.
#[must_use]
pub fn flag_from_args<I>(args: I) -> Option<String>
where
    I: IntoIterator<Item = String>,
{
    let mut found = None;
    let mut args = args.into_iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix(LOCALE_FLAG).and_then(|r| r.strip_prefix('=')) {
            found = non_empty(Some(value)).or(found);
        } else if arg == LOCALE_FLAG
            // The next flag is not a value (audit v033b: `--locale --locale=de` recorded the
            // literal `--locale=de`); it is read on its own turn.
            && let Some(value) = args.next_if(|next| !next.starts_with('-'))
        {
            found = non_empty(Some(&value)).or(found);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valueless_flag_does_not_swallow_the_next_flag() {
        assert_eq!(
            flag_from_args(args(&["game", "--locale", "--locale=de"])),
            Some("de".into())
        );
        assert_eq!(flag_from_args(args(&["game", "--locale", "--fullscreen"])), None);
        assert_eq!(
            flag_from_args(args(&["game", "--locale", "fr"])),
            Some("fr".into())
        );
    }

    /// A non-Unicode process argument (Latin-1 bytes on Unix, an unpaired surrogate on Windows)
    /// must not panic the locale startup; the flag around it still parses.
    #[test]
    fn a_non_unicode_argument_does_not_panic_the_flag_read() {
        #[cfg(unix)]
        let bad = {
            use std::os::unix::ffi::OsStringExt as _;
            std::ffi::OsString::from_vec(vec![b'f', 0xff, b'.', b'x'])
        };
        #[cfg(windows)]
        let bad = {
            use std::os::windows::ffi::OsStringExt as _;
            std::ffi::OsString::from_wide(&[0x66, 0xD800, 0x78])
        };
        #[cfg(not(any(unix, windows)))]
        let bad = std::ffi::OsString::from("f.x");
        let os_args = vec![
            std::ffi::OsString::from("game"),
            bad,
            std::ffi::OsString::from("--locale=de"),
        ];
        assert_eq!(flag_from_args(lossy_args(os_args)), Some("de".into()));
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn flag_forms_and_last_wins() {
        assert_eq!(
            flag_from_args(args(&["game", "--locale", "ja"])),
            Some("ja".into())
        );
        assert_eq!(
            flag_from_args(args(&["game", "--locale=fr-CA"])),
            Some("fr-CA".into())
        );
        assert_eq!(
            flag_from_args(args(&["game", "--locale", "ja", "--locale=de"])),
            Some("de".into())
        );
        // The program name is never a flag; a flag with no value or an empty one is ignored.
        assert_eq!(flag_from_args(args(&["--locale", "ja"])), None);
        assert_eq!(flag_from_args(args(&["game", "--locale"])), None);
        assert_eq!(flag_from_args(args(&["game", "--locale="])), None);
        assert_eq!(flag_from_args(args(&["game", "--localex=ja"])), None);
    }

    #[test]
    fn generated_flag_sequences_match_argument_grammar() {
        // Tokens are grammar atoms, not a copy of the parser's iterator/peek logic. The
        // oracle below records whether a split flag is awaiting a positional value and
        // independently evaluates every token in the following sequence.
        let atoms = [
            "--locale",
            "--locale=fr",
            "--locale=",
            "--fullscreen",
            "--localex=ja",
            "fr",
            "",
            "  de  ",
            "\u{2003}",
            "\u{2003}fr\u{2003}",
            "--locale= ja ",
        ];
        for length in 0..=4_usize {
            let count = atoms.len().pow(length as u32);
            for mut encoded in 0..count {
                let mut sequence = Vec::with_capacity(length + 1);
                sequence.push("--locale=program-name-must-be-ignored");
                for _ in 0..length {
                    sequence.push(atoms[encoded % atoms.len()]);
                    encoded /= atoms.len();
                }
                let expected = grammar_oracle(&sequence);
                let actual = flag_from_args(sequence.iter().map(|part| (*part).to_owned()));
                assert_eq!(actual.as_deref(), expected, "arguments: {sequence:?}");
            }
        }
    }

    fn grammar_oracle<'a>(arguments: &[&'a str]) -> Option<&'a str> {
        let mut selected = None;
        let mut split_value_pending = false;
        for token in arguments.iter().skip(1).copied() {
            if token == LOCALE_FLAG {
                split_value_pending = true;
                continue;
            }
            if let Some(value) = token.strip_prefix("--locale=") {
                selected = trimmed_nonempty(value).or(selected);
                split_value_pending = false;
                continue;
            }
            if split_value_pending {
                split_value_pending = false;
                if !token.starts_with('-') {
                    selected = trimmed_nonempty(token).or(selected);
                }
            }
        }
        selected
    }

    fn trimmed_nonempty(value: &str) -> Option<&str> {
        let value = value.trim();
        (!value.is_empty()).then_some(value)
    }

    #[test]
    fn isolated_hints_read_only_their_fields() {
        let hints = LocaleHints {
            settings: Some("de".into()),
            ..LocaleHints::isolated()
        };
        assert_eq!(hints.flag_value(), None);
        assert_eq!(hints.env_value(), None);
        assert_eq!(hints.os_value(), None);
        let hints = LocaleHints {
            flag: Some(" ja ".into()),
            os: Some(String::new()),
            ..LocaleHints::isolated()
        };
        assert_eq!(hints.flag_value(), Some("ja".into()));
        assert_eq!(hints.os_value(), None);
    }

    #[test]
    fn isolated_getters_never_fall_through_for_empty_or_whitespace_fields() {
        let cases = [None, Some(""), Some(" \u{2003} "), Some(" fr ")];
        for flag in cases {
            for env in cases {
                for os in cases {
                    let hints = LocaleHints {
                        flag: flag.map(str::to_owned),
                        env: env.map(str::to_owned),
                        os: os.map(str::to_owned),
                        ..LocaleHints::isolated()
                    };
                    assert_eq!(hints.flag_value().as_deref(), normalized(flag));
                    assert_eq!(hints.env_value().as_deref(), normalized(env));
                    assert_eq!(hints.os_value().as_deref(), normalized(os));
                }
            }
        }
    }

    fn normalized(value: Option<&str>) -> Option<&str> {
        value.map(str::trim).filter(|value| !value.is_empty())
    }
}
