// SPDX-License-Identifier: MPL-2.0
//! `.rules.ron` files: rule sets as content (format `rules-ron` v1, ADR-8694).
//!
//! [`parse_rules_ron`] reads one file: the size cap first (mods and downloaded content are untrusted),
//! then the version (peeked before the typed parse, so a newer file names its version), then the
//! typed parse (unknown fields refused; every float read straight to `f32`), then
//! [`RuleSet::check`]. [`RuleSetRegistry`] is the app's library: the shipped defaults
//! ([`DEFAULT_RULE_SETS`]: `tdm`, `ffa`, `ctf`, `koth`, `dom`, which reproduce the Rust defaults bit
//! for bit) plus the project's `assets/rules/*.rules.ron`, which may replace a default or extend it.
//!
//! The plain-data rule set and its resolution live in `regia_rules::ruleset`.

use std::collections::BTreeMap;
use std::path::Path;

use regia_app::prelude::*;
use regia_rules::ruleset::{
    AllianceDef, CheckpointDef, DifficultyRowDef, EliminationPatch, FlowPatch, ObjectivePatch, OptionDef,
    OptionKind, PhasePatch, RULES_RON_FORMAT_VERSION, RacePatch, ResolvedRules, RespawnPatch, RuleSet,
    RuleSetError, RuleSetLibrary, SessionPatch, SpawnPolicyDef, SurrenderPatch, TeamsPatch,
    VictoryConditionDef, VictoryPatch, VolumeDef, WaveGroupDef, WaveRowDef, WaveStartDef, WavesPatch,
    WinConditionDef, ZonePatch, ZonePhaseDef, ZonesPatch, codes,
};

pub use regia_rules::ruleset::MAX_RULES_RON_BYTES;

/// File name suffix of a rule set (`ctf_long.rules.ron`).
pub const RULES_RON_SUFFIX: &str = ".rules.ron";

/// The folder under `assets/` a project keeps its rule sets in.
pub const RULES_DIR: &str = "rules";

/// The shipped default rule sets: `(file name, text)`. Each resolves to `ResolvedRules::skeleton` of
/// its mode, bit for bit (`default_rule_sets_resolve_to_the_rust_defaults_bit_for_bit`).
pub const DEFAULT_RULE_SETS: [(&str, &str); 5] = [
    ("tdm.rules.ron", include_str!("../rules/tdm.rules.ron")),
    ("ffa.rules.ron", include_str!("../rules/ffa.rules.ron")),
    ("ctf.rules.ron", include_str!("../rules/ctf.rules.ron")),
    ("koth.rules.ron", include_str!("../rules/koth.rules.ron")),
    ("dom.rules.ron", include_str!("../rules/dom.rules.ron")),
];

fn ron_options() -> ron::Options {
    ron::Options::default()
        .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
        .with_default_extension(ron::extensions::Extensions::UNWRAP_VARIANT_NEWTYPES)
}

/// The error for a version this build has no migrator from.
fn no_migrator(found: u32) -> RuleSetError {
    RuleSetError::new(
        codes::VERSION,
        "format_version",
        format!(
            "no migrator from rules.ron v{found} (this build reads v{RULES_RON_FORMAT_VERSION}; upgrade the engine)"
        ),
    )
}

/// Parse and check one `.rules.ron`.
///
/// Optional values may be written bare (`capture_limit: 5`, `touch_height: 1.5`); to clear a value a
/// parent set, write `Some(None)` (`zone: Some(None)` turns a parent's shrinking zone off). A spawn
/// policy is `First`, `RoundRobin`, `Random` or `Scored(enemy: 10, ...)`. The genre sections
/// (`waves`, `race`, `zone`, `elimination`, `victory`, ADR-8748) write their enums with the session's
/// `kind` words: `at: (kind: "at_tick", tick: 600)`, `(kind: "hold_for", objective: 3, ticks: 6000)`.
///
/// # Errors
///
/// `ruleset_too_large`, `ruleset_version`, `ruleset_parse` (with the line and column) or the
/// check's error.
pub fn parse_rules_ron(text: &str) -> Result<RuleSet, RuleSetError> {
    if text.len() > MAX_RULES_RON_BYTES {
        return Err(RuleSetError::new(
            codes::TOO_LARGE,
            "",
            format!(
                "{} bytes; a rule set holds at most {MAX_RULES_RON_BYTES}",
                text.len()
            ),
        ));
    }
    // A peek that fails (no top-level map, no version) falls through to the typed parse, whose
    // error is the precise one.
    if let Ok(found) = regia_core::version::peek_ron_version(text)
        && found != RULES_RON_FORMAT_VERSION
    {
        return Err(no_migrator(found));
    }
    let set: RuleSet = ron_options()
        .from_str(text)
        .map_err(|e| RuleSetError::new(codes::PARSE, "", e.to_string()))?;
    if set.format_version != RULES_RON_FORMAT_VERSION {
        return Err(no_migrator(set.format_version));
    }
    set.check()?;
    Ok(set)
}

/// Pretty RON of `set` (what [`parse_rules_ron`] reads back to the same value).
///
/// # Errors
///
/// The serializer's message.
pub fn rules_ron_text(set: &RuleSet) -> Result<String, String> {
    ron_options()
        .to_string_pretty(set, regia_core::ron_pretty())
        .map_err(|e| e.to_string())
}

/// Reads a text file of at most `cap` bytes. The read itself is capped (it stops one byte past
/// `cap`), so a file whose reported length is wrong (a link to `/dev/zero` reports 0) cannot read
/// without end; anything that is not a regular file (a device, a pipe) is refused before it is
/// opened. A refusal for size says "at most `cap`".
///
/// # Errors
///
/// The IO error, "not a regular file", "more than `cap` bytes; at most `cap`" or "not UTF-8", as
/// text.
pub fn read_text_capped(file: &Path, cap: usize) -> Result<String, String> {
    use std::io::Read as _;
    let meta = std::fs::metadata(file).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".to_owned());
    }
    if usize::try_from(meta.len()).unwrap_or(usize::MAX) > cap {
        return Err(format!("{} bytes; at most {cap}", meta.len()));
    }
    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .and_then(|f| f.take(limit).read_to_end(&mut bytes))
        .map_err(|e| e.to_string())?;
    if bytes.len() > cap {
        return Err(format!("more than {cap} bytes; at most {cap}"));
    }
    String::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))
}

/// The `rules-ron` schema fingerprint (`cargo xtask check-formats`): the serde names of the file, its
/// patches, its option and win-condition rows, and every settings type a patch reaches.
#[must_use]
pub fn rules_ron_schema_fingerprint() -> String {
    use regia_core::schema::{
        join_field_lists, serde_field_names, serde_variant_names, serde_variant_shapes,
    };
    use regia_rules::match_flow::MatchClock;
    use regia_rules::phases::{RoundSettings, TimeUp};
    use regia_rules::spawn::{RespawnPolicy, SpawnWeights, TeamRespawn};
    use regia_rules::teams::{TeamDef, TeamSwitch};
    use regia_rules::zones::ZoneMode;
    fn variants<T: serde::Deserialize<'static>>() -> Vec<String> {
        let mut list = serde_variant_names::<T>();
        list.extend(serde_variant_shapes::<T>());
        list
    }
    join_field_lists(&[
        serde_field_names::<RuleSet>(),
        serde_field_names::<SessionPatch>(),
        serde_field_names::<FlowPatch>(),
        serde_field_names::<PhasePatch>(),
        serde_field_names::<ObjectivePatch>(),
        serde_field_names::<TeamsPatch>(),
        serde_field_names::<RespawnPatch>(),
        serde_field_names::<ZonesPatch>(),
        serde_field_names::<WinConditionDef>(),
        serde_field_names::<OptionDef>(),
        variants::<OptionKind>(),
        variants::<SpawnPolicyDef>(),
        serde_field_names::<SpawnWeights>(),
        serde_field_names::<TeamDef>(),
        serde_field_names::<RoundSettings>(),
        serde_field_names::<TeamRespawn>(),
        variants::<TimeUp>(),
        variants::<MatchClock>(),
        variants::<TeamSwitch>(),
        variants::<RespawnPolicy>(),
        variants::<ZoneMode>(),
        // The genre sections (ADR-8748).
        serde_field_names::<WavesPatch>(),
        serde_field_names::<WaveRowDef>(),
        serde_field_names::<WaveGroupDef>(),
        serde_field_names::<DifficultyRowDef>(),
        variants::<WaveStartDef>(),
        variants::<regia_rules::waves::CoopSuccess>(),
        serde_field_names::<RacePatch>(),
        serde_field_names::<CheckpointDef>(),
        serde_field_names::<VolumeDef>(),
        serde_field_names::<ZonePatch>(),
        serde_field_names::<ZonePhaseDef>(),
        serde_field_names::<EliminationPatch>(),
        serde_field_names::<AllianceDef>(),
        serde_field_names::<VictoryPatch>(),
        serde_field_names::<SurrenderPatch>(),
        variants::<VictoryConditionDef>(),
    ])
}

/// v1 has no older `.rules.ron`.
#[must_use]
pub fn rules_ron_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// `regia upgrade-project`: a current file stays as it is (after a full parse and check); any other
/// version is `no migrator from rules.ron vN`.
///
/// # Errors
///
/// The parse or check error, as text.
pub fn migrate_rules_ron(text: &str) -> Result<String, String> {
    parse_rules_ron(text).map_err(|e| e.to_string())?;
    Ok(text.to_owned())
}

/// Where a loaded rule set came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSetSource {
    /// The file (an asset path, a disk path, or `engine:rules/<file>` for a shipped default).
    pub path: String,
    /// True for a shipped default.
    pub builtin: bool,
}

/// The app's rule sets (ADR-8694): the shipped defaults plus the project's files, by id.
///
/// [`crate::rules::MatchRulesPlugin`] inserts one with the defaults. A host loads
/// its project with [`RuleSetRegistry::load_dir`] (or [`RuleSetRegistry::load_text`] for text it
/// read itself) before the match, then resolves playlist rows with [`RuleSetRegistry::resolve`].
#[derive(Resource, Debug, Clone)]
pub struct RuleSetRegistry {
    library: RuleSetLibrary,
    sources: BTreeMap<String, RuleSetSource>,
    errors: Vec<(String, RuleSetError)>,
}

impl Default for RuleSetRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl RuleSetRegistry {
    /// A registry holding no rule set (bare skeleton ids still resolve).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            library: RuleSetLibrary::new(),
            sources: BTreeMap::new(),
            errors: Vec::new(),
        }
    }

    /// A registry holding the shipped defaults.
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut registry = Self::empty();
        for (file, text) in DEFAULT_RULE_SETS {
            let path = format!("engine:{RULES_DIR}/{file}");
            match parse_rules_ron(text) {
                Ok(set) => {
                    registry
                        .sources
                        .insert(set.id.clone(), RuleSetSource { path, builtin: true });
                    if let Err(error) = registry.library.replace(set) {
                        registry
                            .errors
                            .push((format!("engine:{RULES_DIR}/{file}"), error));
                    }
                }
                Err(error) => registry.errors.push((path, error)),
            }
        }
        registry
    }

    /// Load one file's text. A project file may replace a shipped default of the same id; two
    /// project files with one id are `ruleset_duplicate`. The error is also kept in
    /// [`RuleSetRegistry::errors`].
    ///
    /// # Errors
    ///
    /// The parse, check or duplicate error.
    pub fn load_text(&mut self, path: &str, text: &str) -> Result<String, RuleSetError> {
        let result = parse_rules_ron(text).and_then(|set| {
            let id = set.id.clone();
            if let Some(source) = self.sources.get(&id)
                && !source.builtin
            {
                return Err(RuleSetError::new(
                    codes::DUPLICATE,
                    format!("{id}: id"),
                    format!("rule set `{id}` is already loaded from {}", source.path),
                ));
            }
            self.library.replace(set)?;
            self.sources.insert(
                id.clone(),
                RuleSetSource {
                    path: path.to_owned(),
                    builtin: false,
                },
            );
            Ok(id)
        });
        if let Err(error) = &result {
            self.errors.push((path.to_owned(), error.clone()));
        }
        result
    }

    /// Load one file's text, replacing any rule set of the same id (a live edit from the editor or
    /// the agent). The error is also kept in [`RuleSetRegistry::errors`].
    ///
    /// # Errors
    ///
    /// The parse or check error.
    pub fn replace_text(&mut self, path: &str, text: &str) -> Result<String, RuleSetError> {
        let result = parse_rules_ron(text).and_then(|set| {
            let id = set.id.clone();
            self.library.replace(set)?;
            self.sources.insert(
                id.clone(),
                RuleSetSource {
                    path: path.to_owned(),
                    builtin: false,
                },
            );
            Ok(id)
        });
        if let Err(error) = &result {
            self.errors.push((path.to_owned(), error.clone()));
        }
        result
    }

    /// Load every `*.rules.ron` directly in `dir`, in file-name order. A missing folder loads
    /// nothing. Returns how many files loaded; the failures are in [`RuleSetRegistry::errors`].
    pub fn load_dir(&mut self, dir: &Path) -> usize {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        let mut files: Vec<std::path::PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.to_ascii_lowercase().ends_with(RULES_RON_SUFFIX))
            })
            .collect();
        files.sort();
        let mut loaded = 0;
        for file in files {
            let shown = file.display().to_string();
            // Capped while reading: a link to a device reports length 0 and never ends.
            let read = read_text_capped(&file, MAX_RULES_RON_BYTES);
            match read {
                Ok(text) => {
                    if self.load_text(&shown, &text).is_ok() {
                        loaded += 1;
                    }
                }
                Err(message) => {
                    let code = if message.contains("at most") {
                        codes::TOO_LARGE
                    } else {
                        codes::PARSE
                    };
                    self.errors.push((shown, RuleSetError::new(code, "", message)));
                }
            }
        }
        loaded
    }

    /// Load the project's `assets/rules/` folder under `project_root`.
    pub fn load_project(&mut self, project_root: &Path) -> usize {
        self.load_dir(&project_root.join("assets").join(RULES_DIR))
    }

    /// The library.
    #[must_use]
    pub fn library(&self) -> &RuleSetLibrary {
        &self.library
    }

    /// Where rule set `id` came from.
    #[must_use]
    pub fn source(&self, id: &str) -> Option<&RuleSetSource> {
        self.sources.get(id)
    }

    /// Files that did not load: `(path, error)`, oldest first.
    #[must_use]
    pub fn errors(&self) -> &[(String, RuleSetError)] {
        &self.errors
    }

    /// Resolve rule set `id` (or a bare skeleton id) with a playlist row's options.
    ///
    /// # Errors
    ///
    /// As `RuleSetLibrary::resolve`.
    pub fn resolve(
        &self,
        id: &str,
        options: &BTreeMap<String, String>,
    ) -> Result<ResolvedRules, RuleSetError> {
        self.library.resolve(id, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_rules::match_flow::{FlowInput, SeatId};
    use regia_rules::mode::MatchMode;
    use regia_rules::objective::{Contestant, EndReason, RulesEvent};
    use regia_rules::session::MatchSession;
    use regia_rules::waves::CoopCause;

    #[test]
    fn default_rule_sets_resolve_to_the_rust_defaults_bit_for_bit() {
        let registry = RuleSetRegistry::with_defaults();
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        for (id, mode) in [
            ("tdm", MatchMode::TeamDeathmatch),
            ("ffa", MatchMode::FreeForAll),
            ("ctf", MatchMode::CaptureTheFlag),
            ("koth", MatchMode::KingOfTheHill),
            ("dom", MatchMode::Domination),
        ] {
            assert!(registry.library().get(id).is_some(), "{id} is shipped");
            let resolved = registry.resolve(id, &BTreeMap::new()).expect(id);
            let rust = ResolvedRules::skeleton(mode);
            assert_eq!(resolved.mode, mode, "{id}");
            assert_eq!(resolved.settings, rust.settings, "{id}");
            assert_eq!(resolved.zones, rust.zones, "{id}");
            assert_eq!(resolved.win_conditions, rust.win_conditions, "{id}");
            assert_eq!(
                resolved.canonical_bytes().expect("bytes"),
                rust.canonical_bytes().expect("bytes"),
                "{id}: the shipped data is the Rust defaults, bit for bit"
            );
            assert_eq!(resolved.content_hash(), rust.content_hash(), "{id}");
        }
    }

    #[test]
    fn ctf_ruleset_resolves_to_the_rust_defaults_bit_for_bit() {
        let registry = RuleSetRegistry::with_defaults();
        let ctf = registry.resolve("ctf", &BTreeMap::new()).expect("ctf");
        let rust = regia_rules::session::SessionSettings::default();
        assert_eq!(
            ctf.settings.objectives.flag_touch_radius.to_bits(),
            rust.objectives.flag_touch_radius.to_bits()
        );
        assert_eq!(ctf.settings, rust);
        assert_eq!(ctf.mode, MatchMode::CaptureTheFlag);
        assert_eq!(
            ctf.content_hash(),
            ResolvedRules::skeleton(MatchMode::CaptureTheFlag).content_hash()
        );
    }

    #[test]
    fn rules_ron_v1_old_fixture_loads() {
        let text = include_str!("../../regia_agent/tests/fixtures/format-corpus/rules-ron/v1.ron");
        let set = parse_rules_ron(text).expect("v1 loads");
        assert_eq!(set.id, "ctf_long");
        assert_eq!(set.extends.as_deref(), Some("ctf"));
        assert_eq!(set.session.objectives.capture_limit, Some(5));
        assert_eq!(set.session.objectives.flag_touch_radius, Some(2.5));
        assert_eq!(set.session.objectives.touch_height, Some(Some(1.75)));
        assert_eq!(migrate_rules_ron(text).expect("v1 stays"), text);
        let mut registry = RuleSetRegistry::with_defaults();
        registry
            .load_text("assets/rules/ctf_long.rules.ron", text)
            .expect("loads over the defaults");
        let resolved = registry
            .resolve(
                "ctf_long",
                &[("capture_limit".to_owned(), "4".to_owned())]
                    .into_iter()
                    .collect(),
            )
            .expect("resolves");
        assert_eq!(resolved.mode, MatchMode::CaptureTheFlag);
        assert_eq!(resolved.settings.objectives.capture_limit, 5);
        assert_eq!(resolved.chain, ["ctf", "ctf_long"]);
        let future = text.replacen("format_version: 1", "format_version: 2", 1);
        let err = parse_rules_ron(&future).expect_err("v2");
        assert_eq!(err.code, codes::VERSION);
        assert!(err.message.contains("no migrator from rules.ron v2"), "{err}");
    }

    #[test]
    fn floats_parse_straight_to_f32_and_round_trip() {
        // Just below the halfway point between 1 + 2^-23 and 1 + 2^-22: parsed straight to f32 it is
        // 1 + 2^-23 (0x3f800001); read as an f64 first it lands on the halfway point and the narrowing
        // cast rounds it to even, 1 + 2^-22 (0x3f800002). The loader must give the first.
        let x = "1.0000001788139343261718749";
        let text = format!(
            "(format_version: 1, id: \"f\", skeleton: \"ctf\", session: (objectives: (flag_touch_radius: {x}, touch_height: {x}), respawn: (spawn: Scored(enemy_cap: {x}, team_radius: 0.3))))"
        );
        let set = parse_rules_ron(&text).expect("parses");
        assert_eq!(
            set.session.objectives.flag_touch_radius.map(f32::to_bits),
            Some(0x3f80_0001)
        );
        assert_eq!(
            set.session.objectives.touch_height.flatten().map(f32::to_bits),
            Some(0x3f80_0001)
        );
        let Some(SpawnPolicyDef::Scored(weights)) = set.session.respawn.spawn else {
            panic!("scored");
        };
        assert_eq!(
            weights.enemy_cap.to_bits(),
            0x3f80_0001,
            "inside the spawn policy too"
        );
        assert_eq!(weights.team_radius.to_bits(), 0.3_f32.to_bits());
        assert_eq!(weights.enemy, 10, "an absent weight keeps its default");
        let again = parse_rules_ron(&rules_ron_text(&set).expect("text")).expect("round trip");
        assert_eq!(again, set);
    }

    #[test]
    fn unknown_fields_and_big_files_are_refused() {
        let err = parse_rules_ron("(format_version: 1, id: \"x\", skeleton: \"ctf\", sesion: ())")
            .expect_err("typo");
        assert_eq!(err.code, codes::PARSE);
        assert!(err.message.contains("sesion"), "{err}");
        let err = parse_rules_ron(
            "(format_version: 1, id: \"x\", skeleton: \"ctf\", session: (objectives: (capture_limt: 3)))",
        )
        .expect_err("nested typo");
        assert!(err.message.contains("capture_limt"), "{err}");
        let big = format!(
            "(format_version: 1, id: \"x\", skeleton: \"ctf\", name_key: \"{}\")",
            "a".repeat(MAX_RULES_RON_BYTES)
        );
        assert_eq!(parse_rules_ron(&big).expect_err("big").code, codes::TOO_LARGE);
        let err = parse_rules_ron("(format_version: 2, totally: [\"new\"])").expect_err("newer");
        assert_eq!(err.code, codes::VERSION);
    }

    #[test]
    fn a_project_dir_replaces_a_default_and_reports_bad_files() {
        let dir = regia_test_utils::unique_temp_dir("ruleset-dir");
        let write = |name: &str, text: &str| {
            let tmp = dir.join(format!("{name}.tmp"));
            std::fs::write(&tmp, text).expect("write");
            std::fs::rename(&tmp, dir.join(name)).expect("rename");
        };
        write(
            "ctf.rules.ron",
            "(format_version: 1, id: \"ctf\", skeleton: \"ctf\", session: (objectives: (capture_limit: 9)))",
        );
        write("broken.rules.ron", "(format_version: 1, id: \"broken\"");
        write("notes.txt", "ignored");
        let mut registry = RuleSetRegistry::with_defaults();
        assert_eq!(registry.load_dir(&dir), 1);
        assert_eq!(registry.errors().len(), 1);
        assert_eq!(registry.errors()[0].1.code, codes::PARSE);
        let ctf = registry.resolve("ctf", &BTreeMap::new()).expect("ctf");
        assert_eq!(ctf.settings.objectives.capture_limit, 9);
        assert!(!registry.source("ctf").expect("source").builtin);
        let err = registry
            .load_text(
                "other/ctf.rules.ron",
                "(format_version: 1, id: \"ctf\", skeleton: \"ctf\")",
            )
            .expect_err("two project files");
        assert_eq!(err.code, codes::DUPLICATE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The genre fixtures (ADR-8748): one `.rules.ron` per mode the genre sections make.
    const GENRES: [(&str, &str); 5] = [
        (
            "coop_waves.rules.ron",
            include_str!("../tests/fixtures/rules/coop_waves.rules.ron"),
        ),
        (
            "race_sprint.rules.ron",
            include_str!("../tests/fixtures/rules/race_sprint.rules.ron"),
        ),
        (
            "br_zone.rules.ron",
            include_str!("../tests/fixtures/rules/br_zone.rules.ron"),
        ),
        (
            "allied_elimination.rules.ron",
            include_str!("../tests/fixtures/rules/allied_elimination.rules.ron"),
        ),
        (
            "strategy.rules.ron",
            include_str!("../tests/fixtures/rules/strategy.rules.ron"),
        ),
    ];

    fn genre(id: &str) -> ResolvedRules {
        let mut registry = RuleSetRegistry::with_defaults();
        for (file, text) in GENRES {
            registry
                .load_text(&format!("assets/rules/{file}"), text)
                .unwrap_or_else(|e| panic!("{file}: {e}"));
        }
        let resolved = registry.resolve(id, &BTreeMap::new()).expect(id);
        assert_ne!(
            resolved.content_hash(),
            ResolvedRules::skeleton(MatchMode::Custom).content_hash(),
            "{id}: the genre section is part of the rules hash"
        );
        resolved
    }

    fn started(rules: &ResolvedRules, seats: &[(u8, u8)]) -> MatchSession {
        let mut session = rules.session();
        for &(team, slot) in seats {
            session.apply(&FlowInput::Seat { team, slot }).expect("seat");
        }
        session.start_now();
        let _ = session.drain_events();
        session
    }

    fn match_over(events: &[RulesEvent]) -> Option<(u8, EndReason)> {
        events.iter().find_map(|e| match e {
            RulesEvent::MatchOver { team, reason, .. } => Some((*team, *reason)),
            _ => None,
        })
    }

    #[test]
    fn genre_rule_sets_round_trip_and_refuse_unknown_fields() {
        for (file, text) in GENRES {
            let set = parse_rules_ron(text).unwrap_or_else(|e| panic!("{file}: {e}"));
            let again = parse_rules_ron(&rules_ron_text(&set).expect("text")).expect("round trip");
            assert_eq!(again, set, "{file}");
        }
        for (typo, field) in [
            ("waves: (wave: [])", "wave"),
            (
                "waves: (waves: [(groups: [(archetype: \"a\", count: 1, tag: \"x\")])])",
                "tag",
            ),
            (
                "waves: (waves: [(at: (kind: \"at_tick\", tick: 5, delay_ticks: 1), groups: [])])",
                "delay_ticks",
            ),
            (
                "race: (checkpoints: [(id: 1, volume: (min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0), mid: (0.0, 0.0, 0.0)), next: [2])])",
                "mid",
            ),
            ("zone: (radius: 5.0, phase: [])", "phase"),
            ("elimination: (last_standin: true)", "last_standin"),
            (
                "victory: (conditions: [(kind: \"hold_for\", objective: 1, ticks: 5, team: 2)])",
                "team",
            ),
            ("victory: (surrender: (threshold: 50))", "threshold"),
        ] {
            let text = format!("(format_version: 1, id: \"t\", skeleton: \"custom\", session: ({typo}))");
            let err = parse_rules_ron(&text).expect_err(typo);
            assert_eq!(err.code, codes::PARSE, "{typo}: {err}");
            assert!(err.message.contains(field), "{typo}: {err}");
        }
        // The corpus fixture with every section resolves.
        let corpus = include_str!("../../regia_agent/tests/fixtures/format-corpus/rules-ron/v1-sections.ron");
        let mut registry = RuleSetRegistry::with_defaults();
        registry
            .load_text("v1-sections.ron", corpus)
            .expect("corpus loads");
        let every = registry
            .resolve("every_section", &BTreeMap::new())
            .expect("corpus resolves");
        assert_eq!(every.settings.victory.conditions.len(), 5);
        assert_eq!(every.settings.race.checkpoints[0].next, [2, 3]);
        assert_eq!(every.settings.zone.as_ref().map(|z| z.seed), Some(2701));
        assert_eq!(every.settings.waves.shared_lives, None);
        let err = parse_rules_ron(
            "(format_version: 1, id: \"t\", skeleton: \"custom\", session: (victory: (conditions: [(kind: \"capture_all\")])))",
        )
        .expect_err("unknown kind");
        assert!(err.message.contains("capture_all"), "{err}");
        // A float in the zone is read straight to `f32` (see `floats_parse_straight_to_f32_and_round_trip`).
        let x = "1.0000001788139343261718749";
        let set = parse_rules_ron(&format!(
            "(format_version: 1, id: \"z\", skeleton: \"custom\", session: (zone: (radius: {x}, phases: [(wait_ticks: 1, shrink_ticks: 1, end_radius: {x}, damage_per_tick: 1)])))"
        ))
        .expect("parses");
        let Some(Some(zone)) = &set.session.zone else {
            panic!("zone");
        };
        assert_eq!(zone.radius.map(f32::to_bits), Some(0x3f80_0001));
        let phases = zone.phases.as_deref().unwrap_or_default();
        assert_eq!(phases[0].end_radius.to_bits(), 0x3f80_0001);
    }

    #[test]
    fn coop_waves_rules_ron_runs_a_match_to_the_last_wave() {
        let rules = genre("coop_waves");
        assert_eq!(rules.settings.waves.waves.len(), 2);
        assert_eq!(rules.settings.waves.shared_lives, Some(5));
        let mut session = started(&rules, &[(1, 0), (1, 1)]);
        let mut spawned = 0;
        let mut events = Vec::new();
        for _ in 0..200 {
            let step = session.step(&[]);
            for event in &step.events {
                // Every enemy dies as soon as it spawns.
                if let RulesEvent::WaveSpawn { wave, count, .. } = event {
                    spawned += count;
                    session.note_wave_down(*wave, *count);
                }
            }
            events.extend(step.events);
            if session.board.match_over {
                break;
            }
        }
        assert_eq!(spawned, 4 + 2 + 1, "two players play the 100 % row");
        assert!(events.iter().any(|e| matches!(
            e,
            RulesEvent::CoopEnded {
                won: true,
                cause: CoopCause::AllWavesCleared,
                ..
            }
        )));
        assert_eq!(match_over(&events), Some((1, EndReason::Ended)));
    }

    #[test]
    fn race_rules_ron_runs_a_lap_and_a_dnf() {
        let rules = genre("race_sprint");
        assert_eq!(rules.settings.race.checkpoints.len(), 4);
        let (a, b) = (SeatId::new(1, 0), SeatId::new(1, 1));
        let mut session = started(&rules, &[(1, 0), (1, 1)]);
        session.race_enter(a, 1);
        session.race_enter(b, 2);
        let car = |id: u32, x: f32, z: f32| Contestant {
            id,
            team: 1,
            position: [x, 0.0, z],
            alive: true,
        };
        let mut events = Vec::new();
        events.extend(session.step(&[car(1, 1.0, 0.0), car(2, 1.0, 0.0)]).events);
        for (x, z) in [(100.0, 0.0), (100.0, 100.0), (0.0, 100.0), (0.0, -1.0)] {
            events.extend(session.step(&[car(1, x, z), car(2, 50.0, 0.0)]).events);
        }
        for _ in 0..20 {
            events.extend(session.step(&[car(2, 50.0, 0.0)]).events);
            if session.board.match_over {
                break;
            }
        }
        assert!(events.iter().any(|e| matches!(
            e,
            RulesEvent::RacerFinished {
                team: 1,
                slot: 0,
                place: 1,
                ..
            }
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RulesEvent::RacerDnf { team: 1, slot: 1, .. }))
        );
        assert!(match_over(&events).is_some(), "the window closes the race");
    }

    #[test]
    fn br_zone_rules_ron_shrinks_and_the_last_team_standing_wins() {
        let rules = genre("br_zone");
        let zone = rules.settings.zone.clone().expect("zone");
        assert_eq!((zone.radius, zone.seed, zone.phases.len()), (100.0, 42, 2));
        let mut session = started(&rules, &[(1, 0), (2, 0), (3, 0)]);
        let unit = |id: u32, team: u8| Contestant {
            id,
            team,
            position: [0.0, 0.0, 0.0],
            alive: true,
        };
        let units = [unit(1, 1), unit(2, 2), unit(3, 3)];
        let mut events = Vec::new();
        for _ in 0..60 {
            events.extend(session.step(&units).events);
        }
        assert!(
            events.iter().any(|e| matches!(e, RulesEvent::ZonePhase { .. })),
            "the zone runs its phases"
        );
        assert!(match_over(&events).is_none());
        session.note_death(SeatId::new(3, 0));
        events.extend(session.step(&units).events);
        session.note_death(SeatId::new(1, 0));
        let last = session.step(&units).events;
        assert_eq!(match_over(&last), Some((2, EndReason::LastStanding)));
        let places: Vec<(u8, u8)> = session.placements().iter().map(|p| (p.team, p.place)).collect();
        assert_eq!(places, [(2, 1), (1, 2), (3, 3)]);
    }

    #[test]
    fn allied_elimination_rules_ron_shares_the_win() {
        let rules = genre("allied_elimination");
        let mut session = started(&rules, &[(1, 0), (2, 0), (3, 0)]);
        session.note_death(SeatId::new(1, 0));
        let first = session.step(&[]).events;
        assert!(match_over(&first).is_none(), "teams 2 and 3 are two sides");
        session.note_death(SeatId::new(3, 0));
        let last = session.step(&[]).events;
        assert_eq!(match_over(&last), Some((2, EndReason::LastStanding)));
        assert!(last.iter().any(|e| matches!(
            e,
            RulesEvent::Victory { teams, .. } if teams == &[1, 2]
        )));
    }

    #[test]
    fn strategy_rules_ron_ends_when_a_capital_falls() {
        let rules = genre("strategy");
        assert_eq!(rules.settings.victory.conditions.len(), 4);
        assert_eq!(rules.settings.victory.surrender.threshold_percent, 66);
        assert_eq!(rules.settings.victory.surrender.window_ticks, 900);
        let mut session = started(&rules, &[(1, 0), (2, 0)]);
        for team in 1..=2 {
            session.report_tagged(team, "capital", 1).expect("report");
        }
        let mut events = Vec::new();
        for _ in 0..5 {
            events.extend(session.step(&[]).events);
        }
        assert!(match_over(&events).is_none());
        session.report_tagged(1, "capital", 0).expect("report");
        let last = session.step(&[]).events;
        assert_eq!(match_over(&last), Some((2, EndReason::Destroyed)));
    }

    #[test]
    fn every_rule_set_in_the_game_modes_guide_loads_and_resolves() {
        let page = include_str!("../../../book/user-guide/game-modes.md");
        let mut checked = Vec::new();
        let mut not_rule_sets = Vec::new();
        for block in page.split("```ron").skip(1) {
            let text = block.split("```").next().unwrap_or_default();
            if !text.contains("format_version") {
                continue;
            }
            // A rule set always names its id (`docs/formats/rules-ron.md`); a `.playlist.ron`
            // example does not (its rows carry `map_id`). The playlist examples are loaded by
            // `regia_net`'s `the_game_modes_guide_playlist_example_loads` and
            // `every_playlist_in_the_game_modes_guide_loads` (ADR-10064) instead.
            if !text.lines().any(|line| line.trim_start().starts_with("id:")) {
                not_rule_sets.push(text);
                continue;
            }
            let mut registry = RuleSetRegistry::with_defaults();
            let id = registry
                .load_text("book/user-guide/game-modes.md", text)
                .unwrap_or_else(|e| panic!("{e}\n{text}"));
            registry
                .resolve(&id, &BTreeMap::new())
                .unwrap_or_else(|e| panic!("{e}\n{text}"));
            checked.push(id);
        }
        assert!(
            not_rule_sets.iter().all(|text| text.contains("entries")),
            "a versioned ron block of the guide that is neither a rule set nor a playlist \
             example must be validated where its format lives\n{not_rule_sets:?}"
        );
        assert_eq!(
            checked,
            [
                "ctf_long",
                "coop_waves",
                "race_sprint",
                "br_zone",
                "allied_elimination",
                "strategy"
            ],
            "one example per mode"
        );
    }

    /// Review proof (rules-ruleset-format-review). `docs/formats/rules-ron.md` says unknown fields
    /// anywhere are `ruleset_parse`. The patch structs deny them; the nested value types the file
    /// embeds (`TeamDef`, `RoundSettings`, `SpawnWeights`, `TeamRespawn`, and the tagged enums
    /// `ZoneMode`, `TimeUp`, `RespawnPolicy`, `TeamSwitch`) do not, so a typo is a silent default.
    #[test]
    fn unknown_fields_inside_nested_structs_are_refused() {
        let cases = [
            (
                "teams",
                r#"(format_version: 1, id: "x", skeleton: "dom", teams: [(id: 1, name_key: "a", color: (1, 2, 3, 4), max_seats: 4, extra: 1)])"#,
            ),
            (
                "rounds",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (phases: (rounds: (rounds_to_win: 2, max_rounds: 3, swap_sides_every: 0, round_end_ticks: 60, extra: 1))))"#,
            ),
            (
                "spawn weights",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (respawn: (spawn: Scored(enemy: 10, extra: 1.0))))"#,
            ),
            (
                "respawn team",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (respawn: (teams: [(team: 1, policy: (kind: "instant"), extra: 1)])))"#,
            ),
            (
                "hardpoint",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (zones: (mode: (kind: "hardpoint", rotation: [1], rotate_ticks: 10, extra: 1))))"#,
            ),
            (
                "overtime",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (phases: (on_time_up: (kind: "overtime", ticks: 10, extra: 1))))"#,
            ),
            (
                "respawn delay",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (respawn: (default: (kind: "delay", ticks: 10, extra: 1))))"#,
            ),
            (
                "team switch",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (teams: (switch: (policy: between_matches, extra: 1))))"#,
            ),
        ];
        for (name, text) in cases {
            let err = parse_rules_ron(text).expect_err(name);
            assert_eq!(err.code, codes::PARSE, "{name}: {err}");
            assert!(
                err.message.contains("extra"),
                "{name} should name the unknown field `extra`: {err}"
            );
        }
    }

    /// Same contract as `unknown_fields_inside_nested_structs_are_refused`, on the unit variants
    /// next to the struct variants that test covers. `docs/formats/rules-ron.md` says an unknown
    /// field on every tagged value (`kind` or `policy`) is `ruleset_parse`.
    ///
    /// `#[serde(deny_unknown_fields)]` does not reach an internally tagged unit variant: serde
    /// 1.0.229's `InternallyTaggedUnitVisitor::visit_map` drains the rest of the map with
    /// `IgnoredAny` and never reads the container attribute (`serde/src/private/de.rs`). The
    /// mirrors and `VictoryConditionDef` are internally tagged, so `End`, `Draw`, `SuddenDeath`,
    /// `Instant`, `Never`, `Domination`, `TeamElimination` and `Surrender` still load and drop
    /// the field. A struct variant of the same enum (`after_cleared`) is the control: that path
    /// does refuse.
    #[test]
    fn unknown_fields_on_internally_tagged_unit_variants_are_refused() {
        let cases = [
            (
                "time up end",
                "extra",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (phases: (on_time_up: (kind: "end", extra: 1))))"#,
            ),
            (
                "time up end with overtime's ticks",
                "ticks",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (phases: (on_time_up: (kind: "end", ticks: 30))))"#,
            ),
            (
                "time up draw",
                "extra",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (phases: (on_time_up: (kind: "draw", extra: 1))))"#,
            ),
            (
                "time up sudden death",
                "extra",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (phases: (on_time_up: (kind: "sudden_death", extra: 1))))"#,
            ),
            (
                "respawn instant",
                "ticks",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (respawn: (default: (kind: "instant", ticks: 10))))"#,
            ),
            (
                "respawn never",
                "lives",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (respawn: (default: (kind: "never", lives: 3))))"#,
            ),
            (
                "respawn team instant",
                "ticks",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (respawn: (teams: [(team: 1, policy: (kind: "instant", ticks: 10))])))"#,
            ),
            (
                "domination",
                "rotation",
                r#"(format_version: 1, id: "x", skeleton: "dom", session: (zones: (mode: (kind: "domination", rotation: [1], rotate_ticks: 10))))"#,
            ),
            (
                "team elimination",
                "tag",
                r#"(format_version: 1, id: "x", skeleton: "custom", session: (victory: (conditions: [(kind: "team_elimination", tag: "capital")])))"#,
            ),
            (
                "surrender",
                "extra",
                r#"(format_version: 1, id: "x", skeleton: "custom", session: (victory: (conditions: [(kind: "surrender", extra: 1)])))"#,
            ),
            (
                "after cleared",
                "extra",
                r#"(format_version: 1, id: "x", skeleton: "custom", session: (waves: (waves: [(at: (kind: "after_cleared", extra: 1), groups: [(archetype: "a", count: 1)])])))"#,
            ),
        ];
        let mut missed = Vec::new();
        for (name, field, text) in cases {
            match parse_rules_ron(text) {
                Err(err) if err.code == codes::PARSE && err.message.contains(field) => {}
                Err(err) => missed.push(format!(
                    "{name}: expected ruleset_parse naming `{field}`, got {err}"
                )),
                Ok(_) => missed.push(format!("{name}: parsed and dropped unknown field `{field}`")),
            }
        }
        assert!(missed.is_empty(), "{}", missed.join("\n"));
    }

    /// Review proof. `rules_ron_schema_fingerprint` is what `cargo xtask check-formats` hashes.
    /// `ZoneMode`, `TimeUp` and `RespawnPolicy` are internally tagged and `TeamSwitch` is adjacently
    /// tagged, so `serde_variant_names` / `serde_variant_shapes` see none of their payload fields.
    /// Adding `hold_ticks` to `ZoneMode::Hardpoint` would not move the `rules-ron` hash.
    #[test]
    fn rules_ron_fingerprint_includes_tagged_enum_shapes() {
        let fp = rules_ron_schema_fingerprint();
        for needle in [
            "rotate_ticks",
            "attackers",
            "overtime",
            "when_unbalanced_by",
            "sudden_death",
        ] {
            assert!(
                fp.contains(needle),
                "rules-ron fingerprint missing `{needle}` (tagged enum shape). fingerprint: {fp}"
            );
        }
    }

    /// `rules_ron_schema_fingerprint` lists `WaveStartDef` and `VictoryConditionDef` through
    /// `serde_variant_names` / `serde_variant_shapes`. Those enums are `#[serde(tag = "kind")]`,
    /// so the derive deserializes with `deserialize_any` and both captures stay empty. Renaming
    /// `tick` or adding a field on `HoldFor` then leaves the `rules-ron` format hash unchanged.
    /// The same capture is empty for the older tagged enums the fingerprint already passes in
    /// (`TimeUp`, `RespawnPolicy`, `ZoneMode`, `TeamSwitch`).
    #[test]
    fn rules_ron_fingerprint_covers_tagged_enum_payloads() {
        use regia_core::schema::{serde_variant_names, serde_variant_shapes};
        use regia_rules::phases::TimeUp;
        use regia_rules::spawn::RespawnPolicy;
        use regia_rules::teams::TeamSwitch;
        use regia_rules::zones::ZoneMode;

        // Externally tagged: this capture already works. A failure here means the helper
        // changed, not the tag hole below.
        assert_eq!(
            serde_variant_shapes::<SpawnPolicyDef>(),
            vec!["0:unit", "1:unit", "2:unit", "3:newtype"]
        );

        assert_eq!(
            serde_variant_names::<WaveStartDef>(),
            vec!["after_cleared", "at_tick"]
        );
        assert_eq!(
            serde_variant_shapes::<WaveStartDef>(),
            vec!["0:{delay_ticks}", "1:{tick}"]
        );
        assert_eq!(
            serde_variant_names::<VictoryConditionDef>(),
            vec![
                "destroy_all_tagged",
                "hold_for",
                "score_threshold",
                "team_elimination",
                "surrender"
            ]
        );
        assert_eq!(
            serde_variant_shapes::<VictoryConditionDef>(),
            vec!["0:{tag}", "1:{objective,ticks}", "2:{score}", "3:unit", "4:unit"]
        );

        let fingerprint = rules_ron_schema_fingerprint();
        for needle in [
            "after_cleared",
            "0:{delay_ticks}",
            "1:{tick}",
            "destroy_all_tagged",
            "1:{objective,ticks}",
        ] {
            assert!(
                fingerprint.contains(needle),
                "rules-ron fingerprint omits `{needle}`; a rename of that wire field would not move format-hashes"
            );
        }

        assert_eq!(
            serde_variant_names::<TimeUp>(),
            vec!["end", "draw", "overtime", "sudden_death"]
        );
        assert_eq!(
            serde_variant_shapes::<TimeUp>(),
            vec!["0:unit", "1:unit", "2:{ticks}", "3:unit"]
        );
        assert_eq!(
            serde_variant_names::<RespawnPolicy>(),
            vec!["instant", "delay", "waves", "lives", "never"]
        );
        assert_eq!(
            serde_variant_shapes::<RespawnPolicy>(),
            vec!["0:unit", "1:{ticks}", "2:{period}", "3:{lives}", "4:unit"]
        );
        assert_eq!(
            serde_variant_names::<ZoneMode>(),
            vec!["domination", "hardpoint", "attack_defend"]
        );
        assert_eq!(
            serde_variant_shapes::<ZoneMode>(),
            vec![
                "0:unit",
                "1:{rotation,rotate_ticks}",
                "2:{attackers,defenders,order}"
            ]
        );
        assert_eq!(
            serde_variant_names::<TeamSwitch>(),
            vec!["never", "between_matches", "when_unbalanced_by"]
        );
        assert_eq!(
            serde_variant_shapes::<TeamSwitch>(),
            vec!["0:unit", "1:unit", "2:newtype"]
        );
    }

    /// A rule-set file whose reported length is 0 but whose read never ends (a link to
    /// `/dev/zero`) is refused by the capped read, with a named code, instead of reading until
    /// memory runs out (rules-editor-authoring review finding 7).
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_device_is_refused_without_reading_it() {
        let dir = regia_test_utils::unique_temp_dir("ruleset-device");
        std::os::unix::fs::symlink("/dev/zero", dir.join("bomb.rules.ron")).expect("symlink");
        let mut registry = RuleSetRegistry::with_defaults();
        assert_eq!(registry.load_dir(&dir), 0);
        assert_eq!(registry.errors().len(), 1, "{:?}", registry.errors());
        assert_eq!(registry.errors()[0].1.code, codes::PARSE);
        assert!(registry.errors()[0].1.message.contains("not a regular file"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The capped read stops one byte past the cap whatever the file system reports.
    #[test]
    fn the_capped_read_refuses_past_the_cap() {
        let dir = regia_test_utils::unique_temp_dir("ruleset-cap");
        let file = dir.join("big.rules.ron");
        std::fs::write(&file, "x".repeat(65)).expect("write");
        let err = read_text_capped(&file, 64).expect_err("past the cap");
        assert!(err.contains("at most 64"), "{err}");
        assert_eq!(read_text_capped(&file, 65).expect("at the cap").len(), 65);
        let mut registry = RuleSetRegistry::with_defaults();
        std::fs::write(&file, "x".repeat(MAX_RULES_RON_BYTES + 1)).expect("write");
        assert_eq!(registry.load_dir(&dir), 0);
        assert_eq!(registry.errors()[0].1.code, codes::TOO_LARGE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R-04 (6), ADR-8881: the bind corpus fixture parses, its bound options set their session
    /// fields (defaults, then a row's values), and the text round-trips with `bind` kept (and no
    /// `bind` key written for an unbound option).
    #[test]
    fn the_bind_corpus_resolves_bound_options_into_the_session() {
        let corpus = include_str!("../../regia_agent/tests/fixtures/format-corpus/rules-ron/v1-binds.ron");
        let mut registry = RuleSetRegistry::with_defaults();
        registry.load_text("v1-binds.ron", corpus).expect("corpus loads");
        let tuned = registry
            .resolve("ctf_tuned", &BTreeMap::new())
            .expect("defaults resolve");
        assert_eq!(
            tuned.settings.objectives.capture_limit, 5,
            "the default beats the patch"
        );
        assert_eq!(tuned.settings.flow.phases.time_limit_ticks, None);
        assert_eq!(
            tuned.settings.objectives.flag_touch_radius.to_bits(),
            2.5f32.to_bits()
        );
        let row: BTreeMap<String, String> = [("caps", "8"), ("time_limit", "18000"), ("home_flag", "false")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let tuned = registry.resolve("ctf_tuned", &row).expect("row resolves");
        assert_eq!(tuned.settings.objectives.capture_limit, 8);
        assert_eq!(tuned.settings.flow.phases.time_limit_ticks, Some(18_000));
        assert!(!tuned.settings.objectives.capture_needs_home_flag);
        let set = parse_rules_ron(corpus).expect("parses");
        let text = rules_ron_text(&set).expect("writes");
        assert_eq!(parse_rules_ron(&text).expect("reads back"), set);
        assert_eq!(text.matches("bind:").count(), 4, "{text}");
    }
}
