// SPDX-License-Identifier: MPL-2.0
//! Pure parsing helpers behind the platform services, testable on any host.

use super::ServiceError;

/// Turns a POSIX locale name (`en_US.UTF-8`, `de_DE@euro`, `pt_BR`) into a BCP-47 tag (`en-US`,
/// `de-DE`, `pt-BR`). `C`, `POSIX` and empty values mean "no preference" and give `None`.
pub(crate) fn posix_locale_to_bcp47(value: &str) -> Option<String> {
    let base = value.split(['.', '@']).next().unwrap_or_default().trim();
    if base.is_empty() || base == "C" || base == "POSIX" {
        return None;
    }
    let mut parts = base.split(['_', '-']).filter(|p| !p.is_empty());
    let language = parts.next()?;
    if !language.chars().all(|c| c.is_ascii_alphabetic()) || !(2..=8).contains(&language.len()) {
        return None;
    }
    let mut tag = language.to_ascii_lowercase();
    for part in parts {
        if !part.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        tag.push('-');
        match part.len() {
            // Region (`US`, `419`).
            2 | 3 => tag.push_str(&part.to_ascii_uppercase()),
            // Script (`Latn`).
            4 => {
                let mut chars = part.chars();
                if let Some(first) = chars.next() {
                    tag.push(first.to_ascii_uppercase());
                    tag.push_str(&chars.as_str().to_ascii_lowercase());
                }
            }
            _ => tag.push_str(part),
        }
    }
    Some(tag)
}

/// The locale from POSIX environment variables, in priority order `LC_ALL`, `LC_MESSAGES`, `LANG`.
/// The first variable that is set and non-empty decides (as the C library does).
/// `fr_CA.UTF-8` becomes `fr-CA`. `C`, `POSIX` and empty give [`None`].
pub fn locale_from_env(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|var| get(var).filter(|v| !v.is_empty()))
        .next()
        .and_then(|v| posix_locale_to_bcp47(&v))
}

/// Normalises a BCP-47 tag reported by the OS (`en_US` → `en-US`); `und` and empty give `None`.
#[cfg_attr(all(unix, not(target_os = "android")), allow(dead_code))]
pub(crate) fn normalise_language_tag(tag: &str) -> Option<String> {
    let tag = tag.trim();
    if tag.is_empty() || tag == "und" {
        return None;
    }
    posix_locale_to_bcp47(tag)
}

/// The first entry of `defaults read -g AppleLanguages` output (`(\n    "fr-FR",\n    "en-US"\n)`) as a
/// BCP 47 tag: the UI language, unlike `AppleLocale`, which is the region format.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn first_apple_language(output: &str) -> Option<String> {
    let entry = output
        .lines()
        .map(|l| l.trim().trim_matches(['(', ')', ',', ' ']).trim_matches('"'))
        .find(|l| !l.is_empty())?;
    normalise_language_tag(entry)
}

/// `MemTotal` from `/proc/meminfo`, in bytes.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
pub(crate) fn meminfo_total_bytes(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let mut fields = line["MemTotal:".len()..].split_whitespace();
    let value: u64 = fields.next()?.parse().ok()?;
    let scale = match fields.next() {
        Some("kB") | Some("KB") | Some("kb") => 1024,
        Some("MB") => 1024 * 1024,
        Some("GB") => 1024 * 1024 * 1024,
        None => 1,
        Some(_) => return None,
    };
    value.checked_mul(scale)
}

/// A value from an `os-release` file (`KEY=value`, `KEY="quoted value"`), with shell-style quotes
/// and backslash escapes removed.
#[cfg_attr(
    not(all(unix, not(any(target_os = "android", target_os = "macos")))),
    allow(dead_code)
)]
pub(crate) fn os_release_value(content: &str, key: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let v = v.trim();
        let unquoted = if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
        {
            &v[1..v.len() - 1]
        } else {
            v
        };
        let mut out = String::with_capacity(unquoted.len());
        let mut chars = unquoted.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else {
                out.push(c);
            }
        }
        return (!out.is_empty()).then_some(out);
    }
    None
}

/// Only `http`, `https` and `mailto` URLs without whitespace or control characters may be opened, so
/// a game cannot be tricked into launching local programs or files.
pub(crate) fn validate_url(url: &str) -> Result<(), ServiceError> {
    let invalid = |why: &str| ServiceError::InvalidUrl {
        url: url.to_owned(),
        reason: why.to_owned(),
    };
    let (scheme, rest) = url.split_once(':').ok_or_else(|| invalid("no scheme"))?;
    let scheme = scheme.to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" => {
            let host = rest
                .strip_prefix("//")
                .ok_or_else(|| invalid("expected `//` after the scheme"))?;
            if host.is_empty() || host.starts_with('/') {
                return Err(invalid("no host"));
            }
        }
        "mailto" => {
            if rest.is_empty() {
                return Err(invalid("no address"));
            }
        }
        _ => return Err(invalid("only http, https and mailto URLs can be opened")),
    }
    if let Some(c) = url
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || *c == '"')
    {
        return Err(invalid(&format!("invalid character {c:?}")));
    }
    Ok(())
}

/// Maps a vibration strength in `0.0..=1.0` to Android's amplitude range `1..=255`; `None` for
/// zero (or negative/NaN) strength, which means "don't vibrate".
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn android_amplitude(amplitude: f32) -> Option<i32> {
    if amplitude.is_nan() || amplitude <= 0.0 {
        return None;
    }
    Some(((amplitude.min(1.0) * 255.0).round() as i32).clamp(1, 255))
}

/// A vibration duration in whole milliseconds for Android (at least 1, saturating).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn android_duration_ms(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX).max(1)
}

/// A gamepad rumble as `(duration_ms, amplitude)` for `VibrationEffect.createOneShot`; `None` for a
/// zero duration or a zero/NaN strength, which mean "don't vibrate" (a 0 ms one-shot throws).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn gamepad_rumble_request(strength: f32, duration: std::time::Duration) -> Option<(i64, i32)> {
    if duration.is_zero() {
        return None;
    }
    let amplitude = android_amplitude(strength)?;
    Some((android_duration_ms(duration), amplitude))
}

/// Name of an `ApplicationExitInfo.REASON_*` constant (public SDK values).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn android_exit_reason(reason: i32) -> &'static str {
    match reason {
        1 => "exit-self",
        2 => "signaled",
        3 => "low-memory",
        4 => "crash",
        5 => "native-crash",
        6 => "anr",
        7 => "initialization-failure",
        8 => "permission-change",
        9 => "excessive-resource-usage",
        10 => "user-requested",
        11 => "user-stopped",
        12 => "dependency-died",
        _ => "other",
    }
}

/// True for the reasons that mean the app failed (crash, native crash, ANR, a signal, a failed
/// start, or too much resource use); a user swipe or the app's own exit is not a failure.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn android_exit_is_failure(reason: i32) -> bool {
    matches!(reason, 2 | 4 | 5 | 6 | 7 | 9)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_locales() {
        assert_eq!(posix_locale_to_bcp47("en_US.UTF-8").as_deref(), Some("en-US"));
        assert_eq!(posix_locale_to_bcp47("de_DE@euro").as_deref(), Some("de-DE"));
        assert_eq!(posix_locale_to_bcp47("pt_BR").as_deref(), Some("pt-BR"));
        assert_eq!(posix_locale_to_bcp47("fr").as_deref(), Some("fr"));
        assert_eq!(posix_locale_to_bcp47("es_419.UTF-8").as_deref(), Some("es-419"));
        assert_eq!(posix_locale_to_bcp47("sr_latn_RS").as_deref(), Some("sr-Latn-RS"));
        assert_eq!(posix_locale_to_bcp47("C.UTF-8"), None);
        assert_eq!(posix_locale_to_bcp47("POSIX"), None);
        assert_eq!(posix_locale_to_bcp47(""), None);
        assert_eq!(posix_locale_to_bcp47("/usr/lib/locale/x"), None);
    }

    #[test]
    fn env_priority() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(
            locale_from_env(env(&[("LANG", "en_GB.UTF-8"), ("LC_MESSAGES", "de_DE.UTF-8")])).as_deref(),
            Some("de-DE")
        );
        assert_eq!(
            locale_from_env(env(&[("LANG", "en_GB.UTF-8"), ("LC_ALL", "ja_JP.UTF-8")])).as_deref(),
            Some("ja-JP")
        );
        assert_eq!(
            locale_from_env(env(&[("LANG", "en_GB.UTF-8"), ("LC_ALL", "")])).as_deref(),
            Some("en-GB")
        );
        assert_eq!(locale_from_env(env(&[("LANG", "C")])), None);
        assert_eq!(locale_from_env(env(&[])), None);
        // The same converter is the public `regia_platform::locale_from_env` (spec 36).
        assert_eq!(
            crate::locale_from_env(|key| (key == "LANG").then(|| "fr_CA.UTF-8".into())).as_deref(),
            Some("fr-CA")
        );
        assert_eq!(
            crate::locale_from_env(|key| (key == "LANG").then(|| "C".into())),
            None
        );
    }

    #[test]
    fn os_tags() {
        assert_eq!(normalise_language_tag("en-US").as_deref(), Some("en-US"));
        assert_eq!(
            normalise_language_tag("zh-Hant-TW").as_deref(),
            Some("zh-Hant-TW")
        );
        assert_eq!(normalise_language_tag("und"), None);
        assert_eq!(normalise_language_tag(""), None);
    }

    #[test]
    fn meminfo() {
        let sample = "MemTotal:       32617040 kB\nMemFree:         1234 kB\n";
        assert_eq!(meminfo_total_bytes(sample), Some(32_617_040 * 1024));
        assert_eq!(meminfo_total_bytes("MemFree: 1 kB\n"), None);
        assert_eq!(meminfo_total_bytes("MemTotal: lots kB\n"), None);
    }

    #[test]
    fn os_release() {
        let sample = "NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\nID=arch\nVERSION_ID='24.04'\n\
                      BUILD_ID=rolling\nX=\"say \\\"hi\\\"\"\n";
        assert_eq!(
            os_release_value(sample, "PRETTY_NAME").as_deref(),
            Some("Arch Linux")
        );
        assert_eq!(os_release_value(sample, "ID").as_deref(), Some("arch"));
        assert_eq!(os_release_value(sample, "VERSION_ID").as_deref(), Some("24.04"));
        assert_eq!(os_release_value(sample, "X").as_deref(), Some("say \"hi\""));
        assert_eq!(os_release_value(sample, "MISSING"), None);
    }

    #[test]
    fn urls() {
        for ok in [
            "https://example.com",
            "http://example.com/a?b=c&d=%20",
            "HTTPS://EXAMPLE.COM/",
            "mailto:someone@example.com",
        ] {
            assert!(validate_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "example.com",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https:/example.com",
            "https://",
            "https:///path",
            "https://example.com/a b",
            "https://example.com/\"&calc",
            "https://example.com/\n",
            "mailto:",
            "C:\\Windows\\system32\\calc.exe",
        ] {
            assert!(
                matches!(validate_url(bad), Err(ServiceError::InvalidUrl { .. })),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn apple_languages_first_entry() {
        assert_eq!(
            first_apple_language("(\n    \"fr-FR\",\n    \"en-US\"\n)").as_deref(),
            Some("fr-FR")
        );
        assert_eq!(
            first_apple_language("(\n    \"zh-Hans-CN\"\n)").as_deref(),
            Some("zh-Hans-CN")
        );
        assert_eq!(first_apple_language("(\n)"), None);
        assert_eq!(first_apple_language(""), None);
    }

    #[test]
    fn haptics_mapping() {
        assert_eq!(android_amplitude(0.0), None);
        assert_eq!(android_amplitude(-1.0), None);
        assert_eq!(android_amplitude(f32::NAN), None);
        assert_eq!(android_amplitude(0.001), Some(1));
        assert_eq!(android_amplitude(0.5), Some(128));
        assert_eq!(android_amplitude(1.0), Some(255));
        assert_eq!(android_amplitude(7.0), Some(255));
        use std::time::Duration;
        assert_eq!(android_duration_ms(Duration::ZERO), 1);
        assert_eq!(android_duration_ms(Duration::from_millis(40)), 40);
        assert_eq!(android_duration_ms(Duration::MAX), i64::MAX);
        assert_eq!(gamepad_rumble_request(0.5, Duration::ZERO), None);
        assert_eq!(gamepad_rumble_request(0.0, Duration::from_millis(40)), None);
        assert_eq!(gamepad_rumble_request(f32::NAN, Duration::from_millis(40)), None);
        assert_eq!(
            gamepad_rumble_request(1.0, Duration::from_micros(10)),
            Some((1, 255))
        );
        assert_eq!(
            gamepad_rumble_request(1.0, Duration::from_millis(40)),
            Some((40, 255))
        );
    }

    #[test]
    fn exit_reasons_are_named_and_classified() {
        assert_eq!(android_exit_reason(5), "native-crash");
        assert_eq!(android_exit_reason(99), "other");
        assert!(android_exit_is_failure(5) && android_exit_is_failure(6));
        assert!(!android_exit_is_failure(10) && !android_exit_is_failure(1));
    }
}
