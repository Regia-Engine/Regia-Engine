// SPDX-License-Identifier: MPL-2.0
//! The `dialogue_assets` validator (spec 36 § 11 contributed to spec 32's framework, ADR-2755).
//!
//! It does not reimplement a check: [`regia_framework::dialogue::validate_dialogues`] runs once when
//! the validator is built (validators never touch the disk), and each issue becomes a finding with the
//! issue's own code (`dialogue_duplicate_line`, `dialogue_unknown_voice`, `dialogue_missing_text`,
//! `dialogue_missing_sound`, `subtitle_past_end`, `dialogue_parse_error`) on the `.dialogue.ron` file it
//! names. `regia assets validate`, the editor's Problems panel and the save trigger all register it
//! through [`dialogue_assets_validator`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use regia_framework::dialogue::{DialogueIssue, DialogueLibrary, DialogueValidateEnv, validate_dialogues};
use regia_ui::loc_config::{LOCALIZATION_CONFIG_PATH, LocalizationConfig};

use crate::asset_graph::{AssetEntry, AssetKind, AssetSet};
use crate::validators::{Finding, Severity, ValidationContext, Validator, Verdict};

/// The registry name of the validator.
pub const DIALOGUE_ASSETS: &str = "dialogue_assets";

/// The project's localization directory, relative to the project root.
const LOCALIZATION_DIR: &str = "assets/localization";

/// One locale's `.ftl` files as `(project-relative path, text)`.
pub type LocaleFiles = (String, Vec<(String, String)>);

/// `localization.ron` of `project`, or the defaults (a file that does not read or parse is logged and
/// treated as absent: the dialogue checks still run on the defaults).
#[must_use]
pub fn read_localization_config(project: &Path) -> LocalizationConfig {
    let path = project.join(LOCALIZATION_CONFIG_PATH);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return LocalizationConfig::default();
    };
    match LocalizationConfig::from_ron(&text) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "dialogue_assets: localization.ron ignored");
            LocalizationConfig::default()
        }
    }
}

/// Every locale directory under `assets/localization` with its `.ftl` files, sorted.
#[must_use]
pub fn read_locale_files(project: &Path, config: &LocalizationConfig) -> Vec<LocaleFiles> {
    let dir = project.join(LOCALIZATION_DIR);
    let mut tags: BTreeSet<String> = config.locales.iter().cloned().collect();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with('.') && (config.locales.is_empty() || tags.contains(&name)) {
                    tags.insert(name);
                }
            }
        }
    }
    tags.into_iter()
        .map(|tag| {
            let mut names: Vec<String> = std::fs::read_dir(dir.join(&tag))
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".ftl"))
                .collect();
            names.sort();
            let files = names
                .into_iter()
                .filter_map(|name| {
                    let rel = format!("{LOCALIZATION_DIR}/{tag}/{name}");
                    let text = std::fs::read_to_string(project.join(&rel)).ok()?;
                    Some((rel, text))
                })
                .collect();
            (tag, files)
        })
        .collect()
}

/// The message keys a locale's files define (a later definition wins; a placeholder removes the key).
fn defined_keys(files: &[(String, String)]) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for (_, text) in files {
        for block in regia_ui::loc_po::ftl_blocks(text) {
            if block.placeholder {
                keys.remove(&block.key);
            } else {
                keys.insert(block.key);
            }
        }
    }
    keys
}

/// The spec 36 § 11 checks over the dialogue library under `project/assets`, with the localization
/// read as `regia loc validate-conv` reads it. Empty without an `assets` directory or any dialogue.
#[must_use]
pub fn dialogue_issues(
    project: &Path,
    config: &LocalizationConfig,
    locales: &[LocaleFiles],
) -> Vec<DialogueIssue> {
    let assets = project.join("assets");
    if !assets.is_dir() {
        return Vec::new();
    }
    let library = DialogueLibrary::load_dir(&assets);
    if library.dialogues.is_empty() && library.errors.is_empty() {
        return Vec::new();
    }
    let source = if config.source.is_empty() {
        "en"
    } else {
        config.source.as_str()
    };
    let source_files = locales.iter().find(|(tag, _)| tag == source);
    // Without the source locale read, the key check has nothing to check.
    let source_keys = source_files.map(|(_, files)| defined_keys(files));
    let has = |k: &str| source_keys.as_ref().is_none_or(|keys| keys.contains(k));
    let available: BTreeSet<String> = locales.iter().map(|(tag, _)| tag.clone()).collect();
    let shipping = if config.locales.is_empty() {
        available.iter().cloned().collect::<Vec<_>>()
    } else {
        config.locales.clone()
    };
    let chains: Vec<(String, Vec<String>)> = shipping
        .iter()
        .map(|l| {
            let mut loaded = available.clone();
            loaded.insert(l.clone());
            (
                l.clone(),
                regia_ui::localization::fallback_chain(l, &loaded, &config.fallbacks, source),
            )
        })
        .collect();
    let lengths = |sound: &str| regia_framework::conversation::clip_duration_us(&assets.join(sound));
    let env = DialogueValidateEnv {
        has_source_key: &has,
        audio_chains: &chains,
        sound_us: &lengths,
    };
    validate_dialogues(&library, &env)
}

/// The `assets/`-relative `.dialogue.ron` files of `assets` by dialogue name. A name defined twice
/// maps to the file `DialogueLibrary::load_dir` keeps: the first in plain path-string order, which
/// is not walkdir's per-directory order (`a-b/x` sorts before `a/y`).
fn dialogue_files(assets: &Path) -> BTreeMap<String, String> {
    let mut found: Vec<(String, String)> = Vec::new();
    for entry in walkdir::WalkDir::new(assets)
        .sort_by_file_name()
        .into_iter()
        .flatten()
    {
        let Ok(rel) = entry.path().strip_prefix(assets) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if entry.file_type().is_file()
            && rel.to_ascii_lowercase().ends_with(".dialogue.ron")
            && let Ok(text) = std::fs::read_to_string(entry.path())
            && let Ok(asset) = regia_asset::dialogue::parse_dialogue(&rel, &text)
        {
            found.push((rel, asset.name));
        }
    }
    found.sort();
    let mut out = BTreeMap::new();
    for (rel, name) in found {
        out.entry(name).or_insert(rel);
    }
    out
}

/// The `dialogue_assets` validator: the issues of one run of the dialogue checks, looked up by file.
#[derive(Debug, Clone, Default)]
pub struct DialogueAssetsValidator {
    /// `assets/`-relative file to its findings.
    findings: BTreeMap<String, Vec<Finding>>,
    /// Every dialogue, voice and cue file seen (for the "checked" verdicts).
    checked: BTreeSet<String>,
}

impl DialogueAssetsValidator {
    /// The validator for `project`: runs the checks now.
    #[must_use]
    pub fn for_project(project: &Path) -> Self {
        let config = read_localization_config(project);
        let locales = read_locale_files(project, &config);
        let issues = dialogue_issues(project, &config, &locales);
        let assets = project.join("assets");
        let files = dialogue_files(&assets);
        let mut v = Self::default();
        v.checked.extend(files.values().cloned());
        for issue in issues {
            // A file-level issue (a parse error) names the file's path, which may itself hold a
            // `#`; only a line issue's subject is `<dialogue name>#<line id>`.
            let is_path = issue.subject.to_ascii_lowercase().ends_with(".ron");
            let file = match issue.subject.split_once('#').filter(|_| !is_path) {
                Some((name, _)) => files
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| format!("dialogue/{name}.dialogue.ron")),
                None => issue.subject.clone(),
            };
            let severity = if issue.error {
                Severity::Error
            } else {
                Severity::Warning
            };
            let mut text = issue.message.clone();
            if let Some(locale) = &issue.locale
                && !text.contains(locale.as_str())
            {
                text = format!("[{locale}] {text}");
            }
            v.checked.insert(file.clone());
            v.findings
                .entry(file)
                .or_default()
                .push(Finding::new(issue.code, severity, text));
        }
        v
    }

    /// Whether any file has a finding.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// The `assets/`-relative paths with findings.
    pub fn files_with_findings(&self) -> impl Iterator<Item = &str> {
        self.findings.keys().map(String::as_str)
    }

    /// Add an `Other` entry (`assets/<file>`, as the registry spells source paths) to `set` for each
    /// file with findings that the set does not hold (dialogue files are not cooked assets), and make
    /// it a root so the orphan check leaves it alone. Returns the paths added.
    pub fn augment_set(&self, set: &mut AssetSet) -> Vec<String> {
        let mut added = Vec::new();
        for file in self.findings.keys() {
            let path = format!("assets/{file}");
            if set.get(file).is_none() && set.get(&path).is_none() {
                added.push(path);
            }
        }
        if !added.is_empty() {
            if set.roots.is_empty() {
                set.roots = set.effective_roots();
            }
            for file in &added {
                set.assets.push(AssetEntry::new(file.clone(), AssetKind::Other));
                set.roots.push(file.clone());
            }
        }
        added
    }
}

impl Validator for DialogueAssetsValidator {
    fn name(&self) -> &str {
        DIALOGUE_ASSETS
    }

    fn description(&self) -> &str {
        "dialogue files: duplicate lines, unknown voices, missing text keys and sounds, cues past a sound's end"
    }

    fn validate(&self, _ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        let path = asset.path.strip_prefix("assets/").unwrap_or(&asset.path);
        if !self.checked.contains(path) {
            return Verdict::not_validated("not a dialogue file");
        }
        Verdict::from_findings(self.findings.get(path).cloned().unwrap_or_default())
    }
}

/// The validator for `project` when it has dialogue files, else `None` (a project without dialogue
/// validates as before).
#[must_use]
pub fn dialogue_assets_validator(project: &Path) -> Option<DialogueAssetsValidator> {
    let v = DialogueAssetsValidator::for_project(project);
    (!v.checked.is_empty()).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validators::{RunSettings, ValidatorRegistry};
    use std::path::PathBuf;

    const HELLO_OK: &str = r#"(format_version: 1, name: "intro", lines: [
        (id: "hello", speaker: "ranger", variants: [(sound: "audio/hello.wav")]),
    ])"#;

    /// A one-second silent 8 kHz mono 16-bit WAV.
    fn wav() -> Vec<u8> {
        let data_len: u32 = 16_000;
        let mut b = Vec::new();
        b.extend(b"RIFF");
        b.extend((36 + data_len).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(8000u32.to_le_bytes());
        b.extend(16_000u32.to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend(b"data");
        b.extend(data_len.to_le_bytes());
        b.extend(vec![0u8; data_len as usize]);
        b
    }

    /// A clean project: one voice, one localized text, one dialogue, one sound.
    fn project(dialogue: &str) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir("dialogue-assets");
        let assets = root.join("assets");
        for dir in ["localization/en", "dialogue", "audio"] {
            std::fs::create_dir_all(assets.join(dir)).expect("mkdir");
        }
        std::fs::write(
            assets.join("localization/en/main.ftl"),
            "dlg-intro-hello = Hello.\n",
        )
        .expect("ftl");
        std::fs::write(
            assets.join("dialogue/ranger.voice.ron"),
            r#"(format_version: 1, id: "ranger", name_key: "voice-ranger-name")"#,
        )
        .expect("voice");
        std::fs::write(assets.join("dialogue/intro.dialogue.ron"), dialogue).expect("dialogue");
        std::fs::write(assets.join("audio/hello.wav"), wav()).expect("wav");
        root
    }

    /// Every finding code the framework reports for `root`, with the file it sits on.
    fn run(root: &Path) -> Vec<(String, String, Severity)> {
        let validator = dialogue_assets_validator(root).expect("the project has dialogue files");
        let mut set = AssetSet::new();
        validator.augment_set(&mut set);
        let mut registry = ValidatorRegistry::new();
        registry.register(Box::new(validator)).expect("registers");
        let report = registry.run_with(&set, &RunSettings::default());
        report
            .results
            .iter()
            .flat_map(|row| {
                row.findings
                    .iter()
                    .map(|f| (f.code.clone(), row.asset.clone(), f.severity))
            })
            .collect()
    }

    #[test]
    fn a_clean_project_reports_nothing() {
        let root = project(HELLO_OK);
        assert_eq!(run(&root), vec![]);
    }

    fn assert_one(dialogue: &str, code: &str, severity: Severity) {
        let root = project(dialogue);
        let found = run(&root);
        assert_eq!(
            found,
            vec![(
                code.to_owned(),
                "assets/dialogue/intro.dialogue.ron".to_owned(),
                severity
            )],
            "{dialogue}"
        );
    }

    #[test]
    fn duplicate_line_is_reported() {
        assert_one(
            r#"(format_version: 1, name: "intro", lines: [
                (id: "hello", speaker: "ranger", variants: [(sound: "audio/hello.wav")]),
                (id: "hello", speaker: "ranger", variants: [(sound: "audio/hello.wav")]),
            ])"#,
            "dialogue_duplicate_line",
            Severity::Error,
        );
    }

    #[test]
    fn parse_error_is_reported() {
        assert_one(
            "(format_version: 1, name: ",
            "dialogue_parse_error",
            Severity::Error,
        );
    }

    #[test]
    fn unknown_voice_is_reported() {
        assert_one(
            r#"(format_version: 1, name: "intro", lines: [
                (id: "hello", speaker: "ghost", variants: [(sound: "audio/hello.wav")]),
            ])"#,
            "dialogue_unknown_voice",
            Severity::Error,
        );
    }

    #[test]
    fn missing_text_is_reported() {
        assert_one(
            r#"(format_version: 1, name: "intro", lines: [
                (id: "hello", speaker: "ranger", variants: [(sound: "audio/hello.wav")]),
                (id: "bye", speaker: "ranger", variants: [(sound: "audio/hello.wav")]),
            ])"#,
            "dialogue_missing_text",
            Severity::Error,
        );
    }

    #[test]
    fn missing_sound_is_reported() {
        assert_one(
            r#"(format_version: 1, name: "intro", lines: [
                (id: "hello", speaker: "ranger", variants: [(sound: "audio/nope.wav")]),
            ])"#,
            "dialogue_missing_sound",
            Severity::Error,
        );
    }

    #[test]
    fn cue_past_the_end_is_a_warning() {
        assert_one(
            r#"(format_version: 1, name: "intro", lines: [
                (id: "hello", speaker: "ranger", variants: [(sound: "audio/hello.wav",
                    cues: Some([(at: 0.0, text: Some("Hi")), (at: 5.0, text: Some("late"))]))]),
            ])"#,
            "subtitle_past_end",
            Severity::Warning,
        );
    }

    #[test]
    fn a_cue_inside_the_sound_is_clean() {
        let root = project(
            r#"(format_version: 1, name: "intro", lines: [
                (id: "hello", speaker: "ranger", variants: [(sound: "audio/hello.wav",
                    cues: Some([(at: 0.0, text: Some("Hi")), (at: 0.5, text: Some("there"))]))]),
            ])"#,
        );
        assert_eq!(run(&root), vec![]);
    }

    #[test]
    fn a_project_without_dialogue_adds_no_validator() {
        let root = regia_test_utils::unique_temp_dir("dialogue-assets-none");
        std::fs::create_dir_all(root.join("assets")).expect("mkdir");
        assert!(dialogue_assets_validator(&root).is_none());
    }

    #[test]
    fn a_registry_asset_with_the_assets_prefix_gets_the_files_findings() {
        let root = project(
            r#"(format_version: 1, name: "intro", lines: [
                (id: "hello", speaker: "ghost", variants: [(sound: "audio/hello.wav")]),
            ])"#,
        );
        let validator = dialogue_assets_validator(&root).expect("validator");
        let set = AssetSet {
            assets: vec![AssetEntry::new(
                "assets/dialogue/intro.dialogue.ron",
                AssetKind::Other,
            )],
            ..AssetSet::new()
        };
        let mut registry = ValidatorRegistry::new();
        registry.register(Box::new(validator)).expect("registers");
        let report = registry.run_with(&set, &RunSettings::default());
        assert_eq!(report.summary.failed, 1, "{report:?}");
    }

    /// Two files define `intro`: the library keeps `dialogue-x/…` (it sorts first as a string)
    /// and skips `dialogue/…`, so the kept file's line findings must sit on `dialogue-x/…`.
    #[test]
    fn findings_of_a_duplicated_name_land_on_the_file_the_library_keeps() {
        let root = project(HELLO_OK);
        let other = root.join("assets/dialogue-x");
        std::fs::create_dir_all(&other).expect("mkdir");
        std::fs::write(
            other.join("intro.dialogue.ron"),
            r#"(format_version: 1, name: "intro", lines: [
                (id: "hello", speaker: "ghost", variants: [(sound: "audio/hello.wav")]),
            ])"#,
        )
        .expect("dialogue");
        let found = run(&root);
        let voice: Vec<&String> = found
            .iter()
            .filter(|(code, _, _)| code == "dialogue_unknown_voice")
            .map(|(_, file, _)| file)
            .collect();
        assert_eq!(
            voice,
            vec!["assets/dialogue-x/intro.dialogue.ron"],
            "all findings: {found:?}"
        );
    }

    /// Corr-low (pipeline-a #22): a parse error's subject is the file's path, which may hold a
    /// `#`; it used to be split as `<name>#<line>` and the finding landed on a file that does not
    /// exist (`dialogue/dialogue/a.dialogue.ron`).
    #[test]
    fn corr_low_a_parse_error_in_a_file_named_with_a_hash_lands_on_that_file() {
        let root = project(HELLO_OK);
        std::fs::write(
            root.join("assets/dialogue/a#b.dialogue.ron"),
            "(format_version: 1, name: ",
        )
        .expect("bad dialogue");
        let validator = DialogueAssetsValidator::for_project(&root);
        let files: Vec<&str> = validator.files_with_findings().collect();
        assert!(
            files.contains(&"dialogue/a#b.dialogue.ron"),
            "the parse error must land on the file itself: {files:?}"
        );
    }
}
