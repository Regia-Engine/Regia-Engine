// SPDX-License-Identifier: MPL-2.0
//! One registry of fuzz targets for every untrusted parser the agent crate can reach (ADR-1917,
//! `docs/specs/97-fuzz-hardening.md`).
//!
//! A [`FuzzTarget`] pairs a valid fixture with a `load` function that parses, consumes (validates,
//! compiles or instantiates where a CPU-only consumer exists) and re-encodes the input. [`run`]
//! mutates the fixture with the seed-stable mutators of `regia_core::fuzz`, counts what loads and
//! what is rejected, catches panics, and checks the round-trip property on every input that loads:
//! the re-encoding loads again and re-encodes to the same text or bytes.
//!
//! The registry covers every frozen format in `docs/formats/format-hashes.ron` (a gate test keeps it
//! complete), every `regia_net` message kind, the agent request text, the parameter parsing of the 30
//! largest agent method groups and a few unfrozen formats. Crates above this one (the Lua host, the
//! editor's importers) add their own targets through [`FuzzTargetExtensions`]. The agent methods
//! `fuzz.targets` and `fuzz.run` (`book/agents/fuzzing.md`) expose it in dev builds.

mod agent_params;
mod files;
mod formats;
mod gameplay;
mod late;
mod net;
mod net_kinds;
// The evaluating `.seq.ron` v2 target (ADR-2135).
mod sequence;
// The formats frozen by the format-registry sweep (ADR-2316).
mod sweep;
mod wire;
mod world;
// The spec 23 gap parsers (ADR-2346): MCP HTTP request heads, cargo test output, the review queue.
mod agent_gaps;
// The entropy-coded state frame (spec 72 § 18, ADR-3315).
mod net_entropy;
// Prefab variants, the physical camera and profile captures (spec 83, ADR-2323).
mod variants;
// IES LM-63 photometric profiles, text and cooked (ADR-2592).
mod light_profiles;
// The baked particle-liquid run (`.fluidcache`, ADR-2441).
mod fluid;
// Sealed records across a rekey and the known-hosts store (ADR-2416).
mod net_trust;
// The level that names its terrain and the grass density file (ADR-2411).
mod terrain_level;
// The product viewer's baked environment (ADR-2539).
mod viewer_env;
// Cooked voxel far proxies (ADR-2445, ADR-2655).
mod voxel_far;
// The cooked payload of a `.webm` / `.mkv` source (ADR-2410).
mod cooked_video;
// The product viewer contracts Regia owns (ADR-2535).
mod jewelry;
// `.ecology.ron` and the cooked voxel far blob (ADR-2456, ADR-2445; integ-v037, ADR-2562).
mod showcase_formats;
// Trailer configurations (`.trailer.ron`, ADR-3281).
mod trailer;
// A level's local reflection capture bake (`.fcap`, ADR-2860).
mod fcap;

// Bake checkpoints (`.bakeck`, ADR-3095).
mod bake_checkpoint;
// Subtitle cue exchange (WebVTT, TTML; spec 56 § 11, ADR-3359).
mod cues;
// The contact-sound bank (`.impactbank.ron`, ADR-3270).
mod impactbank;
// The meshlayers formats (spec 121 § 9, ADR-8112).
mod world_geom;
// A hover hull and ticks (`regia_detsim::hover`, lane mp-hover-robust-inputs).
mod hover;
// The lockstep state snapshot reader (DC-3.7a).
mod det_state;
// Bounded terrain authoring sources (ADR-8362).
mod terrain_source;
// The lockstep input log reader (DC-3.7b).
mod det_input_log;
// Stateful net sequences: one connection, a sealed endpoint, the shared port, the voice relay (spec 120 § 15.5,
// ADR-8468).
mod net_stateful;
// Two honest connections over the impairment model: the reassembly stall deadline against real resends
// (ADR-8521).
mod net_pair;
// Match-flow JSON and `rules.match_*` parameters (ADR-8503).
mod match_rules;
// `.rules.ron` rule sets (ADR-8694).
mod ruleset;
// `anim.gait_simulate` and `anim.foot_plant_simulate` parameters (ADR-8708).
mod locomotion;

use std::panic::{AssertUnwindSafe, catch_unwind};

use regia_app::prelude::*;
use regia_core::fuzz::{FuzzRng, mutate_bytes, mutate_text, mutate_text_values};

pub use agent_params::{GROUP_COUNT, Group, groups as agent_param_groups, handler_runs};

/// A loader error starting with this prefix is an engine bug the loader detected (for example generated code
/// that fails validation), not a rejection of bad input: a run counts it as a [`FailureKind::Bug`].
pub const BUG_PREFIX: &str = "bug: ";

/// What a target reads, with its valid fixture and its loader.
#[derive(Clone, Copy)]
pub enum FuzzInput {
    /// RON or JSON text. `load` parses, consumes and re-encodes to canonical text.
    Text {
        /// A valid input.
        fixture: fn() -> String,
        /// Parse, consume and re-encode.
        load: fn(&str) -> Result<String, String>,
    },
    /// Binary data. `load` decodes, consumes and re-encodes to canonical bytes.
    Bytes {
        /// A valid input.
        fixture: fn() -> Vec<u8>,
        /// Decode, consume and re-encode.
        load: fn(&[u8]) -> Result<Vec<u8>, String>,
    },
}

/// One registered fuzz target.
#[derive(Clone, Copy)]
pub struct FuzzTarget {
    /// Registry name (`"statetree"`, `"net.relay"`, `"agent.request"`, ...).
    pub name: &'static str,
    /// The frozen-format registry name (`docs/formats/format-hashes.ron`) this target covers.
    pub format: Option<&'static str>,
    /// Current version of what the target parses; 0 for unversioned wire data.
    pub format_version: u32,
    /// Whether `load`'s output is a real re-encoding the round-trip property applies to (`false`
    /// when the format has no writer; `load` then returns an empty encoding).
    pub round_trip: bool,
    /// Fixture and loader.
    pub input: FuzzInput,
}

impl FuzzTarget {
    /// `"text"` or `"bytes"`.
    pub fn input_kind(&self) -> &'static str {
        match self.input {
            FuzzInput::Text { .. } => "text",
            FuzzInput::Bytes { .. } => "bytes",
        }
    }
}

impl std::fmt::Debug for FuzzTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuzzTarget")
            .field("name", &self.name)
            .field("format", &self.format)
            .field("format_version", &self.format_version)
            .field("round_trip", &self.round_trip)
            .field("input", &self.input_kind())
            .finish()
    }
}

/// Every registered target, formats first, then wire data.
pub fn targets() -> &'static [FuzzTarget] {
    static TARGETS: std::sync::OnceLock<Vec<FuzzTarget>> = std::sync::OnceLock::new();
    TARGETS.get_or_init(|| {
        let mut all = formats::targets();
        all.extend(gameplay::targets());
        all.extend(world::targets());
        all.extend(files::targets());
        all.extend(wire::targets());
        all.extend(late::targets());
        all.extend(net_kinds::targets());
        all.extend(agent_params::targets());
        all.extend(sequence::targets());
        all.extend(sweep::targets());
        all.extend(agent_gaps::targets());
        all.extend(net_entropy::targets());
        all.extend(variants::targets());
        all.extend(voxel_far::targets());
        all.extend(light_profiles::targets());
        all.extend(fluid::targets());
        all.extend(net_trust::targets());
        all.extend(terrain_level::targets());
        all.extend(viewer_env::targets());
        all.extend(cooked_video::targets());
        all.extend(jewelry::targets());
        all.extend(showcase_formats::targets());
        all.extend(trailer::targets());
        all.extend(fcap::targets());
        // The dedicated server's kept player records (ADR-3354).
        all.extend(crate::net_server_persistence::fuzz_targets());

        all.extend(bake_checkpoint::targets());
        all.extend(cues::targets());
        all.extend(impactbank::targets());
        all.extend(world_geom::targets());
        all.extend(hover::targets());
        all.extend(det_state::targets());
        all.extend(terrain_source::targets());
        all.extend(det_input_log::targets());
        all.extend(net_stateful::targets());
        all.extend(net_pair::targets());
        all.extend(match_rules::targets());
        all.extend(ruleset::targets());
        all.extend(locomotion::targets());
        all
    })
}

/// The target called `name`.
pub fn target(name: &str) -> Option<&'static FuzzTarget> {
    targets().iter().find(|t| t.name == name)
}

/// Fuzz targets that crates depending on `regia_agent` add at app build time (ADR-2084): the Lua host's
/// `luamod` and `luastate`, the editor's glTF and OBJ importers. A resource; `fuzz.targets` lists them after the
/// built-in [`targets`] and `fuzz.run` runs them the same way.
#[derive(Resource, Debug, Clone, Default)]
pub struct FuzzTargetExtensions {
    targets: Vec<FuzzTarget>,
}

impl FuzzTargetExtensions {
    /// Adds `target`. Fails when a built-in or an already added target has its name (adding the same target
    /// twice, as a plugin built twice would, is not an error).
    pub fn add(&mut self, target: FuzzTarget) -> Result<(), String> {
        if self.targets.iter().any(|t| t.name == target.name) {
            return Ok(());
        }
        if self::target(target.name).is_some() {
            return Err(format!("fuzz target {} is built in", target.name));
        }
        self.targets.push(target);
        Ok(())
    }

    /// The added targets, in the order they were added.
    pub fn targets(&self) -> &[FuzzTarget] {
        &self.targets
    }

    /// The added target called `name`.
    pub fn target(&self, name: &str) -> Option<&FuzzTarget> {
        self.targets.iter().find(|t| t.name == name)
    }
}

/// Why one input failed a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The loader panicked.
    Panic,
    /// The input loaded but its re-encoding did not load again, or re-encoded differently.
    RoundTrip,
    /// The loader rejected the input with an empty error message.
    EmptyError,
    /// The loader reported an engine bug ([`BUG_PREFIX`]).
    Bug,
}

impl FailureKind {
    /// Wire name: `"panic"`, `"round_trip"`, `"empty_error"` or `"bug"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Panic => "panic",
            Self::RoundTrip => "round_trip",
            Self::EmptyError => "empty_error",
            Self::Bug => "bug",
        }
    }
}

/// The first failing input of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzFailure {
    /// 0-based mutation index (the same seed reproduces it).
    pub iteration: u32,
    /// What went wrong.
    pub kind: FailureKind,
    /// The panic or property message.
    pub message: String,
    /// Length of the failing input in bytes.
    pub input_len: usize,
}

/// The counts of one [`run`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FuzzReport {
    /// Mutations that loaded.
    pub accepted: u32,
    /// Mutations rejected with a non-empty error.
    pub rejected: u32,
    /// Mutations whose loader panicked.
    pub panicked: u32,
    /// Accepted mutations that broke the round-trip property.
    pub round_trip_failures: u32,
    /// Rejections with an empty error message.
    pub empty_errors: u32,
    /// Engine bugs the loader detected ([`BUG_PREFIX`]).
    pub bugs: u32,
    /// The first failure, if any.
    pub first_failure: Option<FuzzFailure>,
}

impl FuzzReport {
    /// No panic, no round-trip failure, no empty error and no detected bug.
    pub fn is_clean(&self) -> bool {
        self.panicked == 0 && self.round_trip_failures == 0 && self.empty_errors == 0 && self.bugs == 0
    }

    fn fail(&mut self, iteration: u32, kind: FailureKind, message: String, input_len: usize) {
        match kind {
            FailureKind::Panic => self.panicked += 1,
            FailureKind::RoundTrip => self.round_trip_failures += 1,
            FailureKind::EmptyError => self.empty_errors += 1,
            FailureKind::Bug => self.bugs += 1,
        }
        if self.first_failure.is_none() {
            self.first_failure = Some(FuzzFailure {
                iteration,
                kind,
                message,
                input_len,
            });
        }
    }
}

/// Run `target` over `iterations` mutations of its fixture (or of `fixture`, when given), seeded
/// with `seed`. Text targets alternate syntax mutations (`mutate_text`) and well-formed value mutations
/// (`mutate_text_values`); byte targets use `mutate_bytes`. Panics are caught (the build must unwind; `fuzz.run` refuses `panic = "abort"`
/// builds). Fails only when `fixture` is not UTF-8 for a text target.
pub fn run(
    target: &FuzzTarget,
    fixture: Option<&[u8]>,
    seed: u64,
    iterations: u32,
) -> Result<FuzzReport, String> {
    let _span = tracing::info_span!("fuzz.run", target = target.name, iterations).entered();
    let mut rng = FuzzRng::new(seed);
    let mut report = FuzzReport::default();
    match target.input {
        FuzzInput::Text { fixture: make, load } => {
            let base = match fixture {
                Some(bytes) => std::str::from_utf8(bytes)
                    .map_err(|e| format!("target {} reads text; the fixture is not UTF-8: {e}", target.name))?
                    .to_string(),
                None => make(),
            };
            for i in 0..iterations {
                // Even iterations corrupt the syntax; odd ones keep it well formed and corrupt the values, so
                // the input reaches validation and the code that uses it.
                let input = if i % 2 == 0 {
                    mutate_text(&base, &mut rng)
                } else {
                    mutate_text_values(&base, &mut rng)
                };
                match guarded(|| load(&input)) {
                    Ok(Ok(canonical)) => {
                        report.accepted += 1;
                        if target.round_trip
                            && let Err(message) = guarded(|| check_round_trip_text(load, &canonical))
                                .unwrap_or_else(|panic| Err(format!("panicked on the re-encoding: {panic}")))
                        {
                            report.fail(i, FailureKind::RoundTrip, message, input.len());
                        }
                    }
                    Ok(Err(e)) if e.is_empty() => {
                        report.fail(i, FailureKind::EmptyError, String::new(), input.len());
                    }
                    Ok(Err(e)) if e.starts_with(BUG_PREFIX) => {
                        report.fail(i, FailureKind::Bug, e, input.len());
                    }
                    Ok(Err(_)) => report.rejected += 1,
                    Err(panic) => report.fail(i, FailureKind::Panic, panic, input.len()),
                }
            }
        }
        FuzzInput::Bytes { fixture: make, load } => {
            let base = fixture.map_or_else(make, <[u8]>::to_vec);
            for i in 0..iterations {
                let input = mutate_bytes(&base, &mut rng);
                match guarded(|| load(&input)) {
                    Ok(Ok(canonical)) => {
                        report.accepted += 1;
                        if target.round_trip
                            && let Err(message) = guarded(|| check_round_trip_bytes(load, &canonical))
                                .unwrap_or_else(|panic| Err(format!("panicked on the re-encoding: {panic}")))
                        {
                            report.fail(i, FailureKind::RoundTrip, message, input.len());
                        }
                    }
                    Ok(Err(e)) if e.is_empty() => {
                        report.fail(i, FailureKind::EmptyError, String::new(), input.len());
                    }
                    Ok(Err(e)) if e.starts_with(BUG_PREFIX) => {
                        report.fail(i, FailureKind::Bug, e, input.len());
                    }
                    Ok(Err(_)) => report.rejected += 1,
                    Err(panic) => report.fail(i, FailureKind::Panic, panic, input.len()),
                }
            }
        }
    }
    Ok(report)
}

/// The round-trip property for text: `canonical` (a loader's output) loads again and re-encodes to
/// the same text, or to text whose parsed RON value is equal (map order may differ).
pub fn check_round_trip_text(
    load: fn(&str) -> Result<String, String>,
    canonical: &str,
) -> Result<(), String> {
    let again = load(canonical).map_err(|e| format!("the re-encoded text no longer loads: {e}"))?;
    if again == canonical {
        return Ok(());
    }
    let same_value = matches!(
        (ron::from_str::<ron::Value>(canonical), ron::from_str::<ron::Value>(&again)),
        (Ok(a), Ok(b)) if a == b
    );
    if same_value {
        Ok(())
    } else {
        let at = first_difference(canonical.as_bytes(), again.as_bytes());
        Err(format!(
            "re-encoding is not stable: first {} bytes, second {} bytes, first difference at byte {at}: {:?} then {:?}",
            canonical.len(),
            again.len(),
            around(canonical, at),
            around(&again, at)
        ))
    }
}

/// The round-trip property for bytes: `canonical` decodes again and re-encodes to the same bytes.
pub fn check_round_trip_bytes(
    load: fn(&[u8]) -> Result<Vec<u8>, String>,
    canonical: &[u8],
) -> Result<(), String> {
    let again = load(canonical).map_err(|e| format!("the re-encoded bytes no longer decode: {e}"))?;
    if again == canonical {
        Ok(())
    } else {
        Err(format!(
            "re-encoding is not stable: first {} bytes, second {} bytes, first difference at byte {}",
            canonical.len(),
            again.len(),
            first_difference(canonical, &again)
        ))
    }
}

/// Up to 40 bytes either side of `at`, snapped to char boundaries.
fn around(text: &str, at: usize) -> &str {
    let start = text.floor_char_boundary(at.saturating_sub(40));
    let end = text.ceil_char_boundary((at + 40).min(text.len()));
    &text[start..end]
}

fn first_difference(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()))
}

/// Run `f`, turning a panic into its message.
fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "(non-string panic payload)".to_string()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_lookup_finds_them() {
        let mut names: Vec<&str> = targets().iter().map(|t| t.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate target names");
        for t in targets() {
            assert_eq!(target(t.name).map(|f| f.name), Some(t.name));
        }
        assert!(target("no-such-target").is_none());
    }

    #[test]
    fn a_run_is_a_pure_function_of_its_inputs() {
        let t = targets().first().expect("at least one target");
        let a = run(t, None, 7, 50).expect("run");
        let b = run(t, None, 7, 50).expect("run");
        assert_eq!(a, b);
        assert_eq!(a.accepted + a.rejected + a.panicked + a.empty_errors + a.bugs, 50);
    }

    #[test]
    fn a_text_target_refuses_a_non_utf8_fixture() {
        let t = targets()
            .iter()
            .find(|t| t.input_kind() == "text")
            .expect("a text target");
        assert!(run(t, Some(&[0xff, 0xfe]), 1, 1).is_err());
    }

    #[test]
    fn panics_and_unstable_encodings_are_counted() {
        fn panics(_: &str) -> Result<String, String> {
            panic!("boom")
        }
        fn unstable(s: &str) -> Result<String, String> {
            Ok(format!("{s}x"))
        }
        let base = FuzzTarget {
            name: "test",
            format: None,
            format_version: 0,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: || "(a: 1)".into(),
                load: panics,
            },
        };
        let report = run(&base, None, 1, 3).expect("run");
        assert_eq!(report.panicked, 3);
        assert_eq!(report.first_failure.map(|f| f.kind), Some(FailureKind::Panic));
        let unstable_target = FuzzTarget {
            input: FuzzInput::Text {
                fixture: || "(a: 1)".into(),
                load: unstable,
            },
            ..base
        };
        let report = run(&unstable_target, None, 1, 3).expect("run");
        assert_eq!(report.round_trip_failures, 3);
        assert!(!report.is_clean());
    }
}
