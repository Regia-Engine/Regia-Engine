// SPDX-License-Identifier: MPL-2.0
//! The JSON Schema shapes of the structured `render.out_start` params (ADR-1572): `audio`,
//! `aovs`, `time_map` and `cuts`. `agent.schema` shows them, so a client learns the keys, the
//! enums and the limits without trial and error. Every enum and limit comes from the constant or
//! enum the parser reads, so a new bus, AOV, ease or format shows up here without an edit.

use regia_audio::{BusLayout, MAX_MIXDOWN_STEMS, MIXDOWN_SAMPLE_RATES};
use serde_json::{Value, json};

use super::aov::Aov;
use super::audio::{DEFAULT_SAMPLE_RATE, SampleFormat, Timeline};
use super::plan::MAX_CUTS;
use super::time_map::{Ease, MAX_KEY_SCALE, MAX_TIME_KEYS};

/// The bus names `audio.stems` can name in a default bus layout: the mixer buses (including
/// `Master`) and the send buses (`Reverb`), in layout order. A project's `config/audio.ron` may
/// define others; the mixer checks every stem against the layout it runs.
pub(crate) fn stem_bus_names() -> Vec<String> {
    let layout = BusLayout::default_tree();
    layout
        .buses
        .iter()
        .map(|bus| bus.name.clone())
        .chain(layout.send_buses.iter().map(|bus| bus.name.clone()))
        .collect()
}

fn audio_shape() -> Value {
    json!({
        "type": ["boolean", "object"],
        "properties": {
            "sample_rate": {
                "type": "integer",
                "minimum": MIXDOWN_SAMPLE_RATES.start(),
                "maximum": MIXDOWN_SAMPLE_RATES.end(),
                "default": DEFAULT_SAMPLE_RATE,
                "description": "Mixdown sample rate, Hz.",
            },
            "format": {
                "type": "string",
                "enum": SampleFormat::ALL.map(SampleFormat::name),
                "default": SampleFormat::Pcm24.name(),
                "description": "How each sample is stored.",
            },
            "stems": {
                "type": "array",
                "maxItems": MAX_MIXDOWN_STEMS,
                "uniqueItems": true,
                "items": { "type": "string", "enum": stem_bus_names() },
                "description": "Buses also written to their own WAV (<name>_audio_<bus>.wav, after the bus fader). The enum lists the default layout; a project's config/audio.ron may define more buses.",
            },
            "timeline": {
                "type": "string",
                "enum": Timeline::ALL.map(Timeline::name),
                "default": Timeline::Render.name(),
                "description": "Which clock the WAV follows: render (sounds keep speed and pitch under a time_map), output (the simulation clock resampled to the video's length) or sim (the simulation clock unresampled).",
            },
        },
        "additionalProperties": false,
    })
}

fn aovs_shape() -> Value {
    json!({
        "type": "array",
        "items": { "type": "string", "enum": Aov::ALL.map(Aov::name) },
        "uniqueItems": true,
    })
}

fn time_map_shape() -> Value {
    json!({
        "type": "array",
        "maxItems": MAX_TIME_KEYS,
        "items": {
            "type": "object",
            "properties": {
                "frame": { "type": "integer", "minimum": 0, "description": "Delivered frame the key sits on." },
                "scale": {
                    "type": "number",
                    "minimum": 0,
                    "maximum": MAX_KEY_SCALE,
                    "description": "Simulation time per unit of video time (1 = real time).",
                },
                "ease": {
                    "type": "string",
                    "enum": Ease::ALL.map(Ease::name),
                    "default": Ease::Smooth.name(),
                    "description": "How the scale ramps into this key from the key before it.",
                },
            },
            "required": ["frame", "scale"],
            "additionalProperties": false,
        },
    })
}

fn cuts_shape() -> Value {
    json!({
        "type": "array",
        "maxItems": MAX_CUTS,
        "items": { "type": "integer", "minimum": 0 },
    })
}

/// The structured schema of `render.out_start`'s param `name`, if it has one beyond its plain type.
/// The caller adds the description.
pub(crate) fn param_shape(name: &str) -> Option<Value> {
    match name {
        "audio" => Some(audio_shape()),
        "aovs" => Some(aovs_shape()),
        "time_map" => Some(time_map_shape()),
        "cuts" => Some(cuts_shape()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::audio::{AudioRequest, check};
    use super::*;
    use crate::protocol;
    use crate::schema::METHODS;
    use regia_app::prelude::*;
    use regia_audio::Audio;

    fn mixer_world() -> World {
        let mut world = World::new();
        world.insert_resource(Audio::new(&regia_audio::AudioSettings {
            headless: true,
            ..regia_audio::AudioSettings::default()
        }));
        world
    }

    fn method() -> &'static crate::schema::MethodSchema {
        METHODS
            .iter()
            .find(|m| m.name == "render.out_start")
            .expect("render.out_start is in the table")
    }

    #[test]
    fn stem_enum_names_the_default_buses() {
        let names = stem_bus_names();
        for expected in [
            "Master", "Music", "Sfx", "World", "Ui", "Voice", "Ambience", "Reverb",
        ] {
            assert!(names.iter().any(|n| n == expected), "{expected} in {names:?}");
        }
    }

    #[test]
    fn every_stem_in_the_enum_is_accepted_and_an_unknown_one_is_invalid_params() {
        let schema = method().params_json_schema();
        let listed: Vec<String> = schema["properties"]["audio"]["properties"]["stems"]["items"]["enum"]
            .as_array()
            .expect("stems enum")
            .iter()
            .map(|v| v.as_str().expect("string").to_string())
            .collect();
        assert_eq!(listed, stem_bus_names());
        let world = mixer_world();
        for bus in &listed {
            let request = AudioRequest::parse(Some(&json!({ "stems": [bus] })))
                .unwrap_or_else(|e| panic!("{bus} parses: {e}"))
                .expect("audio");
            check(&world, &request).unwrap_or_else(|e| panic!("{bus} checks: {}", e.message));
        }
        let all = AudioRequest::parse(Some(&json!({ "stems": listed })))
            .expect("all stems parse")
            .expect("audio");
        check(&world, &all).expect("all stems check together");
        let unknown = AudioRequest::parse(Some(&json!({ "stems": ["Nope"] })))
            .expect("file-safe name parses")
            .expect("audio");
        let error = check(&world, &unknown).expect_err("unknown bus");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn the_other_audio_enums_round_trip_through_the_parser() {
        let schema = method().params_json_schema();
        let props = &schema["properties"]["audio"]["properties"];
        for format in props["format"]["enum"].as_array().expect("format enum") {
            let request = json!({ "format": format });
            assert!(AudioRequest::parse(Some(&request)).is_ok(), "{format}");
        }
        for timeline in props["timeline"]["enum"].as_array().expect("timeline enum") {
            let request = json!({ "timeline": timeline });
            assert!(AudioRequest::parse(Some(&request)).is_ok(), "{timeline}");
        }
        assert!(AudioRequest::parse(Some(&json!({ "format": "mp3" }))).is_err());
        assert!(AudioRequest::parse(Some(&json!({ "timeline": "later" }))).is_err());
        for rate in [
            props["sample_rate"]["minimum"].as_u64().expect("min"),
            props["sample_rate"]["maximum"].as_u64().expect("max"),
        ] {
            assert!(AudioRequest::parse(Some(&json!({ "sample_rate": rate }))).is_ok());
        }
    }

    #[test]
    fn aov_and_ease_enums_round_trip_through_the_parsers() {
        for aov in Aov::ALL {
            assert_eq!(Aov::parse(aov.name()), Some(aov));
        }
        for ease in Ease::ALL {
            assert_eq!(Ease::parse(ease.name()), Some(ease));
        }
        assert!(Aov::parse("normals").is_none());
        let shape = aovs_shape();
        assert_eq!(
            shape["items"]["enum"],
            json!([
                "depth",
                "id",
                "motion",
                "normal",
                "normal_view",
                "roughness",
                "metalness",
                "base_color",
                "material_id"
            ])
        );
    }

    #[test]
    fn render_out_start_params_have_no_any_and_every_array_has_items() {
        let schema = method().params_json_schema();
        let text = schema.to_string();
        assert!(!text.contains("\"any\""), "{text}");
        for (name, prop) in schema["properties"].as_object().expect("properties") {
            assert!(prop.get("type").is_some(), "{name} untyped");
            if prop["type"] == "array" {
                assert!(prop["items"].is_object(), "{name} array without items");
            }
        }
        for name in ["audio", "aovs", "time_map", "cuts"] {
            assert!(
                schema["properties"][name]["description"].is_string(),
                "{name} keeps its description"
            );
        }
    }
}
