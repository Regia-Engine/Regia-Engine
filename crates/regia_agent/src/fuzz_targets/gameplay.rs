// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for gameplay formats: abilities, conversations, curve tables, choosers, functional tests,
//! ragdolls and spatial queries. Fixtures are the format docs' examples (`docs/formats/`) or the owning crate's
//! golden text, copied into `fixtures/` so the crate packages alone (ADR-1485).

use std::fmt::{Debug, Display};

use regia_framework::abilities::{
    self, AbilityAsset, AbilityAssets, AttributesAsset, Diagnostic, EffectAsset,
};
use regia_framework::conversation::asset as conversation;
use regia_framework::curve_table::CurveTable;
use regia_framework::functional::expected::ExpectedStore;
use regia_framework::functional::script::FuncTestScript;
use regia_framework::functional::{FunctionalTest, TestBodyRef, TestReport, Verdict};

use super::formats::{pretty, reflected_fixture, reflected_load, text};
use super::{FuzzInput, FuzzTarget};

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

fn e_dbg<E: Debug>(err: E) -> String {
    format!("{err:?}")
}

/// A validation error ends the load, as it does in `AbilityAssets::load_text`. A reference to an asset that is not
/// loaded is not a fault of the input: the fuzz run has no other assets to resolve it against.
fn refuse(diagnostics: &[Diagnostic]) -> Result<(), String> {
    match diagnostics
        .iter()
        .find(|d| d.severity == abilities::Severity::Error && d.code != "reference_missing")
    {
        Some(d) => Err(format!("{}: {}", d.code, d.message)),
        None => Ok(()),
    }
}

fn attrs_load(s: &str) -> Result<String, String> {
    let asset: AttributesAsset =
        abilities::parse_ron(s, abilities::ATTRIBUTES_FORMAT_VERSION).map_err(|d| d.message)?;
    refuse(&abilities::validate_attributes(&asset))?;
    abilities::to_ron(&asset).map_err(e)
}

fn effect_load(s: &str) -> Result<String, String> {
    let asset: EffectAsset =
        abilities::parse_ron(s, abilities::EFFECT_FORMAT_VERSION).map_err(|d| d.message)?;
    refuse(&abilities::validate_effect(&asset, None))?;
    abilities::to_ron(&asset).map_err(e)
}

fn ability_load(s: &str) -> Result<String, String> {
    let asset: AbilityAsset =
        abilities::parse_ron(s, abilities::ABILITY_FORMAT_VERSION).map_err(|d| d.message)?;
    refuse(&abilities::validate_ability(
        &asset,
        &AbilityAssets::default(),
        None,
        None,
    ))?;
    abilities::to_ron(&asset).map_err(e)
}

/// The cooked ability asset a pak holds (`abilities-cooked`, spec 53 § 4.1, ADR-7216): decoded, validated as its
/// text format would be, then encoded again.
fn abilities_cooked_load(b: &[u8]) -> Result<Vec<u8>, String> {
    use abilities::CookedAbilityAsset as A;
    let asset = abilities::from_cooked_bytes(b).map_err(|d| d.message)?;
    match &asset {
        A::Attributes(a) => refuse(&abilities::validate_attributes(a))?,
        A::Effect(a) => refuse(&abilities::validate_effect(a, None))?,
        A::Ability(a) => refuse(&abilities::validate_ability(
            a,
            &AbilityAssets::default(),
            None,
            None,
        ))?,
    }
    abilities::cooked_bytes(&asset).map_err(|d| d.message)
}

/// The cooked form of the `effect` fixture.
fn abilities_cooked_fixture() -> Vec<u8> {
    abilities::parse_ron::<EffectAsset>(
        include_str!("fixtures/effect.ron"),
        abilities::EFFECT_FORMAT_VERSION,
    )
    .ok()
    .and_then(|a| abilities::cooked_bytes(&abilities::CookedAbilityAsset::Effect(a)).ok())
    .unwrap_or_default()
}

/// A curve table goes past the parse: every curve is sampled at both ends of its range, between its keys and
/// outside them. A table that validated must not answer NaN.
fn curves_load(s: &str) -> Result<String, String> {
    let table = CurveTable::from_ron_str(s).map_err(e_dbg)?;
    sample_every_curve(&table)?;
    Ok(table.to_ron_string())
}

/// The cooked curve table a pak holds (`curves-cooked`, spec 51): the same sampling check as the source form, then
/// the cooked bytes again.
fn curves_cooked_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let table = CurveTable::from_cooked(b).map_err(e_dbg)?;
    sample_every_curve(&table)?;
    table.to_cooked().map_err(e_dbg)
}

/// The cooked form of the `curves` fixture.
fn curves_cooked_fixture() -> Vec<u8> {
    CurveTable::from_ron_str(include_str!("fixtures/curves.ron"))
        .ok()
        .and_then(|t| t.to_cooked().ok())
        .unwrap_or_default()
}

/// The cooked form of the `chooser` fixture.
fn chooser_cooked_fixture() -> Vec<u8> {
    regia_framework::chooser::ChooserTable::from_ron_str(include_str!("fixtures/chooser.ron"))
        .ok()
        .and_then(|t| t.to_cooked().ok())
        .unwrap_or_default()
}

/// Samples every curve of a table that validated at its keys and far outside them: none may answer NaN.
fn sample_every_curve(table: &CurveTable) -> Result<(), String> {
    for curve in table.curves() {
        let Some(handle) = table.handle(&curve.name) else {
            return Err(format!("{}no handle for {:?}", super::BUG_PREFIX, curve.name));
        };
        let keys = curve.keys.iter().map(|key| key.x);
        for x in [f32::MIN, -1.0, 0.0, 0.5, 1.0, 1000.0, f32::MAX]
            .into_iter()
            .chain(keys)
        {
            if table.sample(handle, x).is_nan() {
                return Err(format!(
                    "{}{:?} samples NaN at {x}",
                    super::BUG_PREFIX,
                    curve.name
                ));
            }
        }
    }
    Ok(())
}

fn functional_register(registry: &mut regia_ecs::prelude::TypeRegistry) {
    registry.register::<FunctionalTest>();
    registry.register::<TestBodyRef>();
    registry.register::<Verdict>();
}

fn functional_fixture() -> String {
    reflected_fixture(
        functional_register,
        &FunctionalTest {
            description: "door opens".into(),
            owner: "gameplay".into(),
            tags: vec!["doors".into(), "smoke".into()],
            settings: vec![("render.quality".into(), "low".into())],
            body: TestBodyRef::Script("tests/door_opens.functest.ron".into()),
            ..FunctionalTest::default()
        },
    )
}

fn functional_load(s: &str) -> Result<String, String> {
    reflected_load::<FunctionalTest>(s, functional_register, |_| Ok(()))
}

/// The gameplay targets.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        // Attributes, effects and abilities (spec 53).
        text(
            "attrs",
            Some("attrs"),
            abilities::ATTRIBUTES_FORMAT_VERSION,
            true,
            || include_str!("fixtures/attrs.ron").into(),
            attrs_load,
        ),
        text(
            "effect",
            Some("effect"),
            abilities::EFFECT_FORMAT_VERSION,
            true,
            || include_str!("fixtures/effect.ron").into(),
            effect_load,
        ),
        text(
            "ability",
            Some("ability"),
            abilities::ABILITY_FORMAT_VERSION,
            true,
            || include_str!("fixtures/ability.ron").into(),
            ability_load,
        ),
        // Their cooked form in a pak (ADR-7216): version word plus postcard.
        FuzzTarget {
            name: "abilities.cooked",
            format: Some("abilities-cooked"),
            format_version: abilities::ABILITIES_COOKED_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: abilities_cooked_fixture,
                load: abilities_cooked_load,
            },
        },
        // Dialogue (spec 56).
        text(
            "conversation",
            Some("conversation"),
            regia_framework::conversation::CONVERSATION_FORMAT_VERSION,
            true,
            || include_str!("fixtures/conversation.ron").into(),
            |s| {
                conversation::parse_conversation("fuzz.conv.ron", s)
                    .and_then(|a| conversation::to_ron(&a))
                    .map_err(e)
            },
        ),
        text(
            "conversation-speakers",
            Some("conversation-speakers"),
            regia_framework::conversation::CONVERSATION_FORMAT_VERSION,
            true,
            || include_str!("fixtures/speakers.ron").into(),
            |s| {
                let set = conversation::parse_speakers("speakers.ron", s).map_err(e)?;
                pretty(&set)
            },
        ),
        text(
            "conversation-settings",
            Some("conversation-settings"),
            regia_framework::conversation::CONVERSATION_FORMAT_VERSION,
            true,
            || include_str!("fixtures/conversation-settings.ron").into(),
            |s| {
                let settings = conversation::parse_settings("conversation.ron", s).map_err(e)?;
                pretty(&settings)
            },
        ),
        text(
            "conversation-cue-events",
            Some("conversation-cue-events"),
            regia_framework::conversation::CONVERSATION_FORMAT_VERSION,
            true,
            || {
                "(format_version: 1, events: {\"door_slam\": (key: \"cue-door-slam\", kind: Sound, planned_ms: 900)})"
                    .into()
            },
            |s| {
                let file = conversation::parse_cue_events("cue_events.ron", s).map_err(e)?;
                pretty(&file)
            },
        ),
        // Curve tables and choosers (spec 51).
        text(
            "curves",
            Some("curves"),
            regia_framework::curve_table::CURVE_TABLE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/curves.ron").into(),
            curves_load,
        ),
        text(
            "chooser",
            Some("chooser"),
            regia_framework::chooser::CHOOSER_FORMAT_VERSION,
            true,
            || include_str!("fixtures/chooser.ron").into(),
            |s| {
                regia_framework::chooser::ChooserTable::from_ron_str(s)
                    .map(|t| t.to_ron_string())
                    .map_err(e_dbg)
            },
        ),
        // Their cooked forms in a pak (spec 51 § cooked): version word plus postcard.
        FuzzTarget {
            name: "curves.cooked",
            format: Some("curves-cooked"),
            format_version: regia_framework::curve_table::CURVE_TABLE_COOKED_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: curves_cooked_fixture,
                load: curves_cooked_load,
            },
        },
        FuzzTarget {
            name: "chooser.cooked",
            format: Some("chooser-cooked"),
            format_version: regia_framework::chooser::CHOOSER_COOKED_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: chooser_cooked_fixture,
                load: |b| {
                    regia_framework::chooser::ChooserTable::from_cooked(b)
                        .and_then(|t| t.to_cooked())
                        .map_err(e_dbg)
                },
            },
        },
        // Functional tests (spec 41).
        text(
            "functional-test",
            Some("functional-test"),
            regia_framework::functional::formats::FUNCTIONAL_TEST_VERSION,
            true,
            functional_fixture,
            functional_load,
        ),
        text(
            "functest-script",
            Some("functest-script"),
            regia_framework::functional::formats::FUNCTEST_SCRIPT_VERSION,
            true,
            || include_str!("fixtures/functest.ron").into(),
            |s| {
                let script = FuncTestScript::parse(s)?;
                pretty(&script)
            },
        ),
        text(
            "expected-values",
            Some("expected-values"),
            regia_framework::functional::formats::EXPECTED_VALUES_VERSION,
            false,
            || include_str!("fixtures/expected.ron").into(),
            |s| {
                ExpectedStore::from_ron_str(s)?;
                Ok(String::new())
            },
        ),
        text(
            "test-report",
            Some("test-report"),
            regia_framework::functional::formats::TEST_REPORT_VERSION,
            true,
            || include_str!("fixtures/test-report.json").into(),
            |s| TestReport::from_json(s).and_then(|r| r.to_json().map_err(e)),
        ),
        // Ragdolls (spec 93) and spatial queries (spec 33).
        text(
            "ragdoll",
            Some("ragdoll"),
            regia_framework::ragdoll::file::RAGDOLL_FORMAT_VERSION,
            true,
            || include_str!("fixtures/ragdoll.ron").into(),
            |s| {
                regia_framework::ragdoll::RagdollProfile::from_ron(s)
                    .and_then(|r| r.to_ron())
                    .map_err(e)
            },
        ),
        FuzzTarget {
            name: "spatialquery",
            format: Some("spatialquery"),
            format_version: regia_ai::spatial_query::SPATIAL_QUERY_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: || include_str!("fixtures/spatialquery.ron").into(),
                load: |s| {
                    regia_ai::spatial_query::SpatialQueryAsset::parse(s)
                        .map_err(e)
                        .and_then(|q| q.to_ron().map_err(e))
                },
            },
        },
    ]
}
