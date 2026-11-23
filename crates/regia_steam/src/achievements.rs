// SPDX-License-Identifier: MPL-2.0
//! Achievements and stats (`docs/specs/24-steam.md` § 2): the `config/achievements.ron` project
//! file, the `Steam` API (`steam.unlock("id")`, `steam.stat_add("kills", 1)`) and the threshold
//! auto-unlock that turns a stat crossing into an achievement.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The parsed `config/achievements.ron`: every achievement of the project, with the localization
/// key Steam shows and the stat threshold that unlocks it.
///
/// ```ron
/// (
///     achievements: [
///         (id: "FIRST_BLOOD", display: "ach.first_blood", stat: "kills", threshold: 1),
///         (id: "MARATHON", display: "ach.marathon", stat: "distance", threshold: 42195, hidden: true),
///     ],
/// )
/// ```
///
/// The `id`s and stat names must match the app's configuration on the Steamworks partner site;
/// Steam rejects unknown ids at runtime (surfaced as
/// [`crate::SteamError::Rejected`](crate::SteamError)).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AchievementsConfig {
    /// Every achievement of the project, in file order.
    #[serde(default)]
    pub achievements: Vec<AchievementDef>,
}

/// One achievement in `config/achievements.ron`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AchievementDef {
    /// The Steam API name (as configured on the Steamworks partner site), e.g. `FIRST_BLOOD`.
    pub id: String,
    /// The localization key for the display name shown in the UI, e.g. `ach.first_blood`.
    pub display: String,
    /// The stat whose threshold unlocks this achievement, e.g. `kills`.
    pub stat: Option<String>,
    /// The stat value that unlocks this achievement; `None` means "manual unlock only".
    pub threshold: Option<i64>,
    /// Hidden achievements show no description until unlocked (Steam admin flag mirror).
    #[serde(default)]
    pub hidden: bool,
}

/// Problems in `config/achievements.ron`. None are fatal: the plugin logs one warning and runs
/// with the achievements it could salvage.
#[derive(thiserror::Error, Debug)]
pub enum AchievementsConfigError {
    /// The file could not be read or parsed as RON.
    #[error("cannot read achievements config {path}: {source}")]
    Read {
        /// The file that failed.
        path: std::path::PathBuf,
        /// The underlying error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The file's content is inconsistent (duplicate ids, a threshold without a stat...).
    #[error("achievements config: {0}")]
    Invalid(String),
}

/// Decodes a text file written by a Windows editor: UTF-8 (BOM stripped) or UTF-16 (LE or BE,
/// recognised by its BOM). `None` when the bytes are neither.
pub(crate) fn decode_text(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, u16::from_be_bytes);
    }
    let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8(rest.to_vec()).ok()
}

fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Option<String> {
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    String::from_utf16(&pairs.iter().map(|pair| unit(*pair)).collect::<Vec<_>>()).ok()
}

impl AchievementsConfig {
    /// RON options for `achievements.ron`: bare values fill `Option<T>` (`stat: "kills"` →
    /// `Some("kills")`), the same convention as `.ui.ron` (`regia_ui::authoring`).
    fn ron_options() -> ron::Options {
        ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
    }

    /// Parses and validates RON text (`config/achievements.ron`).
    pub fn from_ron(text: &str) -> Result<Self, AchievementsConfigError> {
        let config: AchievementsConfig =
            Self::ron_options()
                .from_str(text)
                .map_err(|err| AchievementsConfigError::Read {
                    path: "config/achievements.ron".into(),
                    source: Box::new(err),
                })?;
        config.validate()?;
        Ok(config)
    }

    /// Loads, parses and validates `<path>` (normally `config/achievements.ron` under the project
    /// root). The error carries the actual path.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, AchievementsConfigError> {
        let path = path.as_ref();
        let text = std::fs::read(path)
            .and_then(|bytes| {
                decode_text(&bytes).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "not UTF-8 or UTF-16 text")
                })
            })
            .map_err(|err| AchievementsConfigError::Read {
                path: path.to_owned(),
                source: Box::new(err),
            })?;
        let config: AchievementsConfig =
            Self::ron_options()
                .from_str(&text)
                .map_err(|err| AchievementsConfigError::Read {
                    path: path.to_owned(),
                    source: Box::new(err),
                })?;
        config.validate()?;
        Ok(config)
    }

    /// Like [`AchievementsConfig::load`], but keeps what is valid instead of failing on the first
    /// bad entry: returns the salvaged config plus one message per problem (see
    /// [`AchievementsConfig::salvage`]). A file that cannot be read or parsed at all is still an
    /// error.
    pub fn load_salvaged(path: impl AsRef<Path>) -> Result<(Self, Vec<String>), AchievementsConfigError> {
        let path = path.as_ref();
        let text = std::fs::read(path)
            .and_then(|bytes| {
                decode_text(&bytes).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "not UTF-8 or UTF-16 text")
                })
            })
            .map_err(|err| AchievementsConfigError::Read {
                path: path.to_owned(),
                source: Box::new(err),
            })?;
        let config: AchievementsConfig =
            Self::ron_options()
                .from_str(&text)
                .map_err(|err| AchievementsConfigError::Read {
                    path: path.to_owned(),
                    source: Box::new(err),
                })?;
        Ok(config.salvage())
    }

    /// Drops what [`AchievementsConfig::validate`] would reject, entry by entry: entries with an
    /// empty id and later duplicates of an id are removed; a threshold without a stat is cleared
    /// (the achievement stays, unlockable by hand). Returns the cleaned config and one message
    /// per problem.
    pub fn salvage(self) -> (Self, Vec<String>) {
        let mut problems = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut achievements = Vec::with_capacity(self.achievements.len());
        for mut def in self.achievements {
            if def.id.trim().is_empty() {
                problems.push("achievement id must not be empty (entry dropped)".to_owned());
                continue;
            }
            if !seen.insert(def.id.clone()) {
                problems.push(format!("duplicate achievement id `{}` (entry dropped)", def.id));
                continue;
            }
            if def.threshold.is_some() && def.stat.is_none() {
                problems.push(format!(
                    "achievement `{}` has a threshold but no stat (threshold ignored)",
                    def.id
                ));
                def.threshold = None;
            }
            if let Some(threshold) = def.threshold
                && threshold_cannot_be_reached(threshold)
            {
                problems.push(format!(
                    "achievement `{}` threshold {threshold} is outside the i32 stat range (threshold ignored)",
                    def.id
                ));
                def.threshold = None;
            }
            achievements.push(def);
        }
        (Self { achievements }, problems)
    }

    /// Rejects duplicate ids, empty ids, and thresholds without a stat.
    pub fn validate(&self) -> Result<(), AchievementsConfigError> {
        let mut seen = std::collections::BTreeSet::new();
        for def in &self.achievements {
            if def.id.trim().is_empty() {
                return Err(AchievementsConfigError::Invalid(
                    "achievement id must not be empty".into(),
                ));
            }
            if !seen.insert(def.id.clone()) {
                return Err(AchievementsConfigError::Invalid(format!(
                    "duplicate achievement id `{}`",
                    def.id
                )));
            }
            if def.threshold.is_some() && def.stat.is_none() {
                return Err(AchievementsConfigError::Invalid(format!(
                    "achievement `{}` has a threshold but no stat",
                    def.id
                )));
            }
            if let Some(threshold) = def.threshold
                && threshold_cannot_be_reached(threshold)
            {
                return Err(AchievementsConfigError::Invalid(format!(
                    "achievement `{}` threshold {threshold} is outside the i32 stat range",
                    def.id
                )));
            }
        }
        Ok(())
    }

    /// Whether the project knows this achievement id.
    pub fn contains(&self, id: &str) -> bool {
        self.achievements.iter().any(|def| def.id == id)
    }

    /// The achievements watching `stat`, cheapest lookup for the threshold auto-unlock in
    /// [`crate::Steam::stat_add`].
    pub fn watchers_of(&self, stat: &str) -> Vec<&AchievementDef> {
        self.achievements
            .iter()
            .filter(|def| def.stat.as_deref() == Some(stat))
            .collect()
    }

    /// The distinct stat names the config watches, in config order (useful to check against the
    /// Steamworks partner setup).
    pub fn stats(&self) -> impl Iterator<Item = &str> {
        let mut seen = std::collections::BTreeSet::new();
        self.achievements
            .iter()
            .filter_map(|def| def.stat.as_deref())
            .filter(move |stat| seen.insert(*stat))
    }
}

/// Steam stats are `i32` (or `f32`); a threshold outside the `i32` range can never be crossed
/// by an integer stat and reads as a float comparison, so it is rejected instead of silently
/// locking the achievement forever.
fn threshold_cannot_be_reached(threshold: i64) -> bool {
    threshold > i64::from(i32::MAX) || threshold < i64::from(i32::MIN)
}

/// Maps each watched stat to its (achievement, threshold) pairs; kept by [`crate::Steam`] for the
/// auto-unlock check. `Default` is an empty map (a project without achievements config).
#[derive(Debug, Clone, Default)]
pub(crate) struct Thresholds {
    by_stat: BTreeMap<String, Vec<(String, i64)>>,
}

impl From<&AchievementsConfig> for Thresholds {
    fn from(config: &AchievementsConfig) -> Self {
        let mut by_stat: BTreeMap<String, Vec<(String, i64)>> = BTreeMap::new();
        for def in &config.achievements {
            if let (Some(stat), Some(threshold)) = (&def.stat, def.threshold) {
                by_stat
                    .entry(stat.clone())
                    .or_default()
                    .push((def.id.clone(), threshold));
            }
        }
        Self { by_stat }
    }
}

impl Thresholds {
    /// The achievements whose threshold `value` just crossed, in config order.
    pub(crate) fn crossed_by(&self, stat: &str, value: i32) -> Vec<String> {
        self.by_stat
            .get(stat)
            .into_iter()
            .flatten()
            .filter(|(_, threshold)| i64::from(value) >= *threshold)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// The achievements whose threshold a finite `value` of stat `stat` just reached, in config
    /// order. The comparison happens in `f64` so an `i64` threshold converts exactly; a
    /// non-finite value crosses nothing.
    pub(crate) fn crossed_by_f32(&self, stat: &str, value: f32) -> Vec<String> {
        if !value.is_finite() {
            return Vec::new();
        }
        let value = f64::from(value);
        self.by_stat
            .get(stat)
            .into_iter()
            .flatten()
            .filter(|(_, threshold)| value >= *threshold as f64)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
        (
            achievements: [
                (id: "FIRST_BLOOD", display: "ach.first_blood", stat: "kills", threshold: 1),
                (id: "RAMPAGE", display: "ach.rampage", stat: "kills", threshold: 10),
                (id: "MANUAL", display: "ach.manual"),
                (id: "SECRET", display: "ach.secret", stat: "distance", threshold: 1000, hidden: true),
            ],
        )
    "#;

    #[test]
    fn parses_and_validates() {
        let config = AchievementsConfig::from_ron(CONFIG).expect("valid config");
        assert_eq!(config.achievements.len(), 4);
        assert!(config.contains("FIRST_BLOOD"));
        assert!(!config.contains("NOPE"));
        assert_eq!(config.watchers_of("kills").len(), 2);
        let mut stats: Vec<&str> = config.stats().collect();
        stats.sort();
        assert_eq!(stats, ["distance", "kills"]);
    }

    #[test]
    fn rejects_duplicates_and_threshold_without_stat() {
        let dup = r#"(achievements: [
            (id: "A", display: "a", stat: "kills", threshold: 1),
            (id: "A", display: "a"),
        ])"#;
        assert!(AchievementsConfig::from_ron(dup).is_err());
        let statless = r#"(achievements: [(id: "A", display: "a", threshold: 1)])"#;
        assert!(AchievementsConfig::from_ron(statless).is_err());
        let empty = r#"(achievements: [(id: "", display: "a")])"#;
        assert!(AchievementsConfig::from_ron(empty).is_err());
    }

    #[test]
    fn thresholds_cross_by_stat_value() {
        let config = AchievementsConfig::from_ron(CONFIG).expect("valid config");
        let thresholds = Thresholds::from(&config);
        assert_eq!(thresholds.crossed_by("kills", 0), Vec::<String>::new());
        assert_eq!(thresholds.crossed_by("kills", 1), ["FIRST_BLOOD"]);
        assert_eq!(thresholds.crossed_by("kills", 25), ["FIRST_BLOOD", "RAMPAGE"]);
        assert_eq!(thresholds.crossed_by("distance", 1000), ["SECRET"]);
        assert!(thresholds.crossed_by("unrelated", 100).is_empty());
    }

    #[test]
    fn threshold_outside_i32_is_rejected() {
        // Steam stats are i32: a threshold outside that range can never be crossed, and loading
        // it would silently lock the achievement forever.
        let over = format!(
            r#"(achievements: [(id: "A", display: "a", stat: "distance", threshold: {})])"#,
            i64::from(i32::MAX) + 1
        );
        assert!(
            AchievementsConfig::from_ron(&over).is_err(),
            "an unreachable threshold must not load"
        );
        let under = format!(
            r#"(achievements: [(id: "A", display: "a", stat: "distance", threshold: {})])"#,
            i64::from(i32::MIN) - 1
        );
        assert!(
            AchievementsConfig::from_ron(&under).is_err(),
            "a below-range threshold must not load"
        );

        // Salvage clears the threshold and names the problem, like every other bad entry.
        let raw: AchievementsConfig = AchievementsConfig::ron_options().from_str(&over).expect("parses");
        let (salvaged, problems) = raw.salvage();
        assert_eq!(salvaged.achievements[0].threshold, None);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(salvaged.validate().is_ok());

        // The i32 bounds themselves stay valid.
        let at_bounds = r#"(achievements: [
            (id: "LO", display: "lo", stat: "s", threshold: -2147483648),
            (id: "HI", display: "hi", stat: "s", threshold: 2147483647),
        ])"#;
        assert!(AchievementsConfig::from_ron(at_bounds).is_ok());
    }

    #[test]
    fn crossed_by_compares_float_stats_in_f64() {
        let config = AchievementsConfig::from_ron(CONFIG).expect("valid config");
        let thresholds = Thresholds::from(&config);
        assert_eq!(thresholds.crossed_by_f32("distance", 1000.0), ["SECRET"]);
        assert_eq!(thresholds.crossed_by_f32("distance", 999.5), Vec::<String>::new());
        assert!(
            thresholds.crossed_by_f32("distance", f32::NAN).is_empty(),
            "NaN unlocks nothing"
        );
        assert!(
            thresholds.crossed_by_f32("distance", f32::INFINITY).is_empty(),
            "a non-finite stat must not unlock"
        );
    }

    #[test]
    fn empty_config_is_valid() {
        let config = AchievementsConfig::from_ron("()").expect("empty config");
        assert!(config.achievements.is_empty());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn salvage_keeps_the_valid_entries_and_reports_each_problem() {
        let config = AchievementsConfig::from_ron(
            r#"(achievements: [
                (id: "A", display: "a", stat: "kills", threshold: 1),
                (id: "A", display: "dup"),
                (id: " ", display: "blank"),
                (id: "B", display: "b", threshold: 5),
            ])"#,
        );
        // Strict parsing still rejects the file as a whole...
        assert!(config.is_err());
        // ...while salvage keeps A (first) and B (threshold cleared) and names three problems.
        let raw: AchievementsConfig = AchievementsConfig::ron_options()
            .from_str(
                r#"(achievements: [
                    (id: "A", display: "a", stat: "kills", threshold: 1),
                    (id: "A", display: "dup"),
                    (id: " ", display: "blank"),
                    (id: "B", display: "b", threshold: 5),
                ])"#,
            )
            .expect("parse");
        let (salvaged, problems) = raw.salvage();
        let ids: Vec<&str> = salvaged.achievements.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["A", "B"]);
        assert_eq!(salvaged.achievements[1].threshold, None);
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(salvaged.validate().is_ok());
    }

    #[test]
    fn decode_text_accepts_bom_and_utf16() {
        assert_eq!(decode_text(b"480\n").as_deref(), Some("480\n"));
        assert_eq!(decode_text(b"\xEF\xBB\xBF480").as_deref(), Some("480"));
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("480\r\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode_text(&le).as_deref(), Some("480\r\n"));
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain("é".encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        assert_eq!(decode_text(&be).as_deref(), Some("é"));
        assert_eq!(decode_text(&[0xFF, 0xFE, 0x34]), None, "odd UTF-16 length");
        assert_eq!(decode_text(&[0x80, 0x81]), None, "invalid UTF-8");
    }

    #[test]
    fn load_reads_a_utf16_file_with_bom() {
        let dir = regia_test_utils::unique_temp_dir("steam-ach-utf16");
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("achievements.ron");
        let bytes: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(
                "(achievements: [(id: \"A\", display: \"a\")])"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes),
            )
            .collect();
        std::fs::write(&path, bytes).expect("write");
        let config = AchievementsConfig::load(&path).expect("loads");
        assert!(config.contains("A"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
