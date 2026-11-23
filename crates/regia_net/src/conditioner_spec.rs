// SPDX-License-Identifier: MPL-2.0
//! Conditioner specs: one line of text that names stated network conditions (ADR-3075).
//!
//! A match across real machines needs the same conditions on every process, typed on a command
//! line or passed in the environment. A spec is a base followed by `key=value` overrides, comma
//! separated:
//!
//! ```text
//! wan                                   a link profile (`CONDITIONER_PROFILES`)
//! average                               a session preset (spec 77 § 10.1) mapped onto the link
//! delay=40,jitter=10,loss=0.02          stated conditions on top of `off`
//! wan,loss=0.05,seed=7                  a profile with overrides
//! ```
//!
//! Keys: `delay` and `jitter` (milliseconds; jitter is the extra hold, uniform in `0..=jitter`),
//! `loss`, `dup` and `reorder` (fractions in `[0, 1]`, or percent with a `%` suffix),
//! `reorder_max` (milliseconds), `burst_start`, `burst_len` (milliseconds), `burst_loss`
//! (fraction) and `seed`. Any key turns the conditioner on; `off` alone (or an empty spec) is the
//! pass-through. Names that are both a link profile and a session preset (`off`, `lan`, `bad`)
//! mean the link profile.
//!
//! [`ConditionerConfig::from_env`] reads `REGIA_NET_CONDITIONER` and `REGIA_NET_CONDITIONER_SEED`
//! (the variables play-in-editor and `cargo xtask net-match` pass to every process) in dev builds
//! only: a build without debug assertions (`release`, `shipping`) ignores them, so a player cannot
//! add loss to a shipped game from the environment.

use crate::conditioner::{CONDITIONER_PROFILES, ConditionerConfig, MAX_CONDITIONER_MS};
use crate::session::ShaperSettings;

/// The environment variable that holds a conditioner spec (dev builds only).
pub const CONDITIONER_ENV: &str = "REGIA_NET_CONDITIONER";
/// The environment variable that overrides the spec's seed (dev builds only).
pub const CONDITIONER_SEED_ENV: &str = "REGIA_NET_CONDITIONER_SEED";

/// Session presets that are not also link profiles, accepted as a spec's base.
const SESSION_ONLY_PRESETS: &[&str] = &["good", "average", "terrible"];

impl ConditionerConfig {
    /// Parses a conditioner spec (see the module docs). The result is sanitized.
    ///
    /// # Errors
    ///
    /// An unknown base name or key, a value that is not a number, a probability outside `[0, 1]`
    /// or a time over one minute; the message names the part.
    pub fn parse_spec(spec: &str) -> Result<Self, String> {
        let mut parts = spec
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .peekable();
        let mut config = match parts.peek() {
            Some(first) if !first.contains('=') => {
                let name = *first;
                parts.next();
                base(name).ok_or_else(|| {
                    format!(
                        "unknown conditioner `{name}` (one of {}, {}, or key=value pairs)",
                        CONDITIONER_PROFILES.join(", "),
                        SESSION_ONLY_PRESETS.join(", ")
                    )
                })?
            }
            _ => Self::default(),
        };
        for part in parts {
            let Some((key, value)) = part.split_once('=') else {
                return Err(format!("`{part}`: expected key=value"));
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "delay" => config.delay_ms = millis(key, value)?,
                "jitter" => config.jitter_ms = millis(key, value)?,
                "reorder_max" => config.reorder_max_ms = millis(key, value)?,
                "burst_start" => config.burst_start_ms = millis(key, value)?,
                "burst_len" => config.burst_len_ms = millis(key, value)?,
                "loss" => config.loss = fraction(key, value)?,
                "dup" => config.duplicate = fraction(key, value)?,
                "reorder" => config.reorder = fraction(key, value)?,
                "burst_loss" => config.burst_loss = fraction(key, value)?,
                "seed" => {
                    config.seed = value
                        .parse()
                        .map_err(|_| format!("seed: `{value}` is not a whole number"))?;
                }
                other => {
                    return Err(format!(
                        "unknown key `{other}` (delay, jitter, loss, dup, reorder, reorder_max, burst_start, \
                         burst_len, burst_loss, seed)"
                    ));
                }
            }
            config.enabled = true;
        }
        Ok(config.sanitized())
    }

    /// One line that states the conditions, for logs and match reports:
    /// `off`, or `delay=60 jitter=10 loss=0.020 dup=0.010 reorder=0.050 reorder_max=40 seed=1`
    /// (burst fields only when a burst is set).
    pub fn describe(&self) -> String {
        if !self.enabled {
            return "off".into();
        }
        let mut text = format!(
            "delay={} jitter={} loss={:.3} dup={:.3} reorder={:.3} reorder_max={} seed={}",
            self.delay_ms,
            self.jitter_ms,
            self.loss,
            self.duplicate,
            self.reorder,
            self.reorder_max_ms,
            self.seed
        );
        if self.burst_start_ms > 0 {
            text.push_str(&format!(
                " burst_start={} burst_len={} burst_loss={:.3}",
                self.burst_start_ms, self.burst_len_ms, self.burst_loss
            ));
        }
        text
    }

    /// The conditioner the environment asks for: `REGIA_NET_CONDITIONER` (a spec) and
    /// `REGIA_NET_CONDITIONER_SEED`, read in dev builds only (see the module docs). `Ok(None)` when
    /// neither is set, and always in a build without debug assertions.
    ///
    /// # Errors
    ///
    /// The spec does not parse, or the seed is not a whole number.
    pub fn from_env() -> Result<Option<Self>, String> {
        if !cfg!(debug_assertions) {
            return Ok(None);
        }
        let get = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
        Self::from_vars(
            get(CONDITIONER_ENV).as_deref(),
            get(CONDITIONER_SEED_ENV).as_deref(),
        )
    }

    /// [`ConditionerConfig::from_env`] on given values (`spec`, `seed`), whatever the build.
    ///
    /// # Errors
    ///
    /// The spec does not parse, or the seed is not a whole number.
    pub fn from_vars(spec: Option<&str>, seed: Option<&str>) -> Result<Option<Self>, String> {
        if spec.is_none() && seed.is_none() {
            return Ok(None);
        }
        let mut config = match spec {
            Some(text) => Self::parse_spec(text).map_err(|e| format!("{CONDITIONER_ENV}: {e}"))?,
            None => Self::default(),
        };
        if let Some(text) = seed {
            config.seed = text
                .trim()
                .parse()
                .map_err(|_| format!("{CONDITIONER_SEED_ENV}: `{text}` is not a whole number"))?;
        }
        Ok(Some(config))
    }
}

/// A base name: a link profile first, else a session preset mapped onto the link.
fn base(name: &str) -> Option<ConditionerConfig> {
    if let Some(config) = ConditionerConfig::from_profile(name) {
        return Some(config);
    }
    if !SESSION_ONLY_PRESETS.contains(&name) {
        return None;
    }
    ShaperSettings::preset(name).map(|s| from_shaper(&s))
}

/// A session preset on the link conditioner. The session's jitter is `±j` around the latency; the
/// link's is an extra hold in `0..=jitter`, so `latency - j` plus `0..=2j` keeps the same range.
fn from_shaper(s: &ShaperSettings) -> ConditionerConfig {
    let jitter = u64::from(s.jitter_ms).min(u64::from(s.latency_ms));
    ConditionerConfig {
        enabled: true,
        seed: 1,
        delay_ms: u64::from(s.latency_ms) - jitter,
        jitter_ms: 2 * jitter,
        loss: s.loss_pct / 100.0,
        duplicate: s.duplicate_pct / 100.0,
        reorder: s.reorder_pct / 100.0,
        reorder_max_ms: u64::from(s.reorder_extra_ms),
        ..ConditionerConfig::default()
    }
}

fn millis(key: &str, value: &str) -> Result<u64, String> {
    let ms: u64 = value
        .trim_end_matches("ms")
        .parse()
        .map_err(|_| format!("{key}: `{value}` is not a whole number of milliseconds"))?;
    if ms > MAX_CONDITIONER_MS {
        return Err(format!(
            "{key}: {ms} ms is over the {MAX_CONDITIONER_MS} ms limit"
        ));
    }
    Ok(ms)
}

fn fraction(key: &str, value: &str) -> Result<f64, String> {
    let (text, scale) = match value.strip_suffix('%') {
        Some(pct) => (pct, 100.0),
        None => (value, 1.0),
    };
    let v: f64 = text
        .parse()
        .map_err(|_| format!("{key}: `{value}` is not a number"))?;
    let v = v / scale;
    if !(0.0..=1.0).contains(&v) {
        return Err(format!("{key}: `{value}` is outside 0..1 (or 0%..100%)"));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_name_is_the_profile() {
        assert_eq!(
            ConditionerConfig::parse_spec("wan"),
            Ok(ConditionerConfig::from_profile("wan").unwrap_or_default())
        );
        assert_eq!(
            ConditionerConfig::parse_spec("off"),
            Ok(ConditionerConfig::default())
        );
        assert_eq!(
            ConditionerConfig::parse_spec(""),
            Ok(ConditionerConfig::default())
        );
    }

    #[test]
    fn stated_conditions_turn_the_conditioner_on() {
        let c = ConditionerConfig::parse_spec(
            "delay=40, jitter=10,loss=2%,dup=0.01,reorder=0.03,reorder_max=25,seed=9",
        )
        .unwrap_or_default();
        assert!(c.enabled);
        assert_eq!(
            (c.delay_ms, c.jitter_ms, c.reorder_max_ms, c.seed),
            (40, 10, 25, 9)
        );
        assert!((c.loss - 0.02).abs() < 1e-12);
        assert!((c.duplicate - 0.01).abs() < 1e-12);
        assert!((c.reorder - 0.03).abs() < 1e-12);
    }

    #[test]
    fn overrides_apply_on_top_of_a_profile() {
        let c = ConditionerConfig::parse_spec("wan,loss=0.05,seed=7").unwrap_or_default();
        let wan = ConditionerConfig::from_profile("wan").unwrap_or_default();
        assert_eq!(
            (c.delay_ms, c.jitter_ms, c.seed),
            (wan.delay_ms, wan.jitter_ms, 7)
        );
        assert!((c.loss - 0.05).abs() < 1e-12);
    }

    #[test]
    fn session_presets_keep_their_latency_range() {
        let c = ConditionerConfig::parse_spec("average").unwrap_or_default();
        // average: 60 ms ± 8 ms, 1 % loss.
        assert!(c.enabled);
        assert_eq!((c.delay_ms, c.delay_ms + c.jitter_ms), (52, 68));
        assert!((c.loss - 0.01).abs() < 1e-12);
        // `bad` is a link profile name as well; the link profile wins.
        assert_eq!(
            ConditionerConfig::parse_spec("bad"),
            Ok(ConditionerConfig::from_profile("bad").unwrap_or_default())
        );
    }

    #[test]
    fn bad_specs_name_the_part() {
        for (spec, needle) in [
            ("dialup", "unknown conditioner `dialup`"),
            ("delay=40,lag=3", "unknown key `lag`"),
            ("loss=1.5", "loss: `1.5` is outside"),
            ("loss=abc", "loss: `abc` is not a number"),
            ("delay=-1", "delay: `-1`"),
            ("delay=60001", "over the 60000 ms limit"),
            ("wan,loss", "`loss`: expected key=value"),
            ("seed=x", "seed: `x`"),
        ] {
            match ConditionerConfig::parse_spec(spec) {
                Err(e) => assert!(e.contains(needle), "{spec}: {e}"),
                Ok(c) => panic!("{spec} parsed: {c:?}"),
            }
        }
    }

    #[test]
    fn describe_round_trips_through_parse() {
        let c = ConditionerConfig::parse_spec("wan,seed=3").unwrap_or_default();
        let line = c.describe().replace(' ', ",");
        assert_eq!(ConditionerConfig::parse_spec(&line), Ok(c));
        assert_eq!(ConditionerConfig::default().describe(), "off");
    }

    #[test]
    fn the_environment_values_parse_with_a_seed_override() {
        assert_eq!(ConditionerConfig::from_vars(None, None), Ok(None));
        let c = ConditionerConfig::from_vars(Some("home"), Some("42"))
            .ok()
            .flatten()
            .unwrap_or_default();
        assert!(c.enabled);
        assert_eq!((c.delay_ms, c.seed), (30, 42));
        assert!(ConditionerConfig::from_vars(Some("nope"), None).is_err());
        assert!(ConditionerConfig::from_vars(None, Some("x")).is_err());
    }
}
