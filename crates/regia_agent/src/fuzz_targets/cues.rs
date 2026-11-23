// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for the localization tool formats (ADR-3359): the `glossary` frozen format and
//! the subtitle cue exchange (WebVTT, TTML; spec 56 § 11). The cue readers are bounded
//! (`regia_ui::cue_exchange::CueLimits`) and round-trip stable.

use regia_ui::cue_exchange::{CueLimits, read_ttml, read_webvtt, to_ttml, to_webvtt};
use regia_ui::loc_glossary::{self, GlossaryFile};

use super::{FuzzInput, FuzzTarget};

// The harness truncates a text fixture at half its length and requires a rejection
// (`fuzz_text_loader`), so the fixture is shaped so the midpoint lands inside the second cue's
// timestamp line; a cut there leaves a dangling partial line the parser refuses.
const CUE_FIXTURE: &str =
    "WEBVTT\n\n00:00:00.500 --> 00:00:02.250\nA\n\n00:00:03.000 --> 00:00:04.000\nSecond cue!!!\n";

fn webvtt_fixture() -> String {
    // invariant: CUE_FIXTURE is a checked-in constant the harness also truncates and mutates.
    CUE_FIXTURE.to_owned()
}

fn webvtt_load(text: &str) -> Result<String, String> {
    let cues = read_webvtt(text, &CueLimits::default()).map_err(|e| e.to_string())?;
    Ok(to_webvtt(&cues))
}

fn ttml_fixture() -> String {
    to_ttml(
        // invariant: CUE_FIXTURE is a checked-in constant the harness also truncates and mutates.
        &read_webvtt(CUE_FIXTURE, &CueLimits::default()).expect("the checked-in fixture parses"),
        "en",
    )
}

fn ttml_load(text: &str) -> Result<String, String> {
    let cues = read_ttml(text, &CueLimits::default()).map_err(|e| e.to_string())?;
    Ok(to_ttml(&cues, "en"))
}

fn glossary_fixture() -> String {
    let file = loc_glossary::parse_glossary(
        "(format_version: 1, terms: [(term: \"Save\", translations: {\"de\": \"Speichern\"}, note: \"verb\")])",
    )
    // invariant: a checked-in constant, not input.
    .expect("the checked-in fixture parses");
    canonical(&file)
}

fn canonical(file: &GlossaryFile) -> String {
    ron::ser::to_string_pretty(file, regia_core::ron_pretty().struct_names(false))
        .map_err(|e| e.to_string())
        .unwrap_or_default()
}

fn glossary_load(text: &str) -> Result<String, String> {
    let file = loc_glossary::parse_glossary(text).map_err(|e| e.to_string())?;
    Ok(canonical(&file))
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        FuzzTarget {
            name: "glossary",
            format: Some("glossary"),
            format_version: loc_glossary::GLOSSARY_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: glossary_fixture,
                load: glossary_load,
            },
        },
        FuzzTarget {
            name: "subtitles.webvtt",
            format: None,
            format_version: 0,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: webvtt_fixture,
                load: webvtt_load,
            },
        },
        FuzzTarget {
            name: "subtitles.ttml",
            format: None,
            format_version: 0,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: ttml_fixture,
                load: ttml_load,
            },
        },
    ]
}
