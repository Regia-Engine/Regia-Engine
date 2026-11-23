// SPDX-License-Identifier: MPL-2.0
//! Named capability markers for `agent.hello` and `agent.schema` (ADR-1559).
//!
//! A client that must work against more than one engine build asks for a marker instead of
//! probing parameter names. Markers whose feature is compiled in are always listed; the audio
//! markers depend on an `Audio` resource, mirroring `F0013`. A host (the editor) adds its own
//! markers through [`crate::extension::AgentExtension::add_capability`].

use regia_app::prelude::World;
use serde_json::{Value, json};

/// The `.shots.ron` format version this build reads and writes (`agent.hello`'s
/// `shots_ron_version`).
pub const SHOTS_RON_VERSION: u32 = regia_framework::shots::SHOT_SEQUENCE_FORMAT_VERSION;

/// Markers whose feature is compiled into every build: `(name, what it means)`.
const ALWAYS: &[(&str, &str)] = &[
    (
        "shot_fade_layer",
        "Shot fades are drawn by the engine (layer \"fade\" above the UI); render.sequence reports fade_layer.",
    ),
    (
        "render_out_aov_id",
        "render.out_start aovs accepts \"id\" (id.instance and id.class, manifest aov.ids).",
    ),
    (
        "render_out_aov_depth",
        "render.out_start aovs accepts \"depth\" (channel Z).",
    ),
    (
        "render_out_aov_motion",
        "render.out_start aovs accepts \"motion\" (channels motion.u and motion.v).",
    ),
    (
        "render_out_aov_material",
        "render.out_start aovs accepts the material AOVs normal, normal_view, roughness, metalness, base_color and material_id (aov_format png and png16 for the 0..1 ones).",
    ),
    (
        "shot_rotation_keys",
        "camera.set_path accepts rotation keys and .shots.ron reads and writes them (shots_ron_version 2 and later).",
    ),
    (
        "configurator_contracts_v1",
        "configurator.contract_check and configurator.contract_schema validate the jewelry.<name>/1 contracts (ADR-2480, ADR-2535).",
    ),
    (
        "configurator_runtime_v1",
        "configurator.open_product/options/set_pick/build/resolve run the product configurator: picks the option graph sells, the look, jewelry.build/1 (ADR-2534).",
    ),
    (
        "viewer_view_pack_v1",
        "viewer.view_simulate and viewer.pack_verify: the product viewer's drag view and packages (ADR-2540).",
    ),
    (
        "entity_kinds",
        "Entity kinds: kinds.list, kinds.of, kinds.lint, project.describe; scene.query_entities accepts kind and scene.get_entity reports kind, role and unclassified.",
    ),
];

/// Markers that need an `Audio` resource.
const AUDIO: &[(&str, &str)] = &[
    (
        "render_out_audio",
        "render.out_start accepts audio and writes a WAV mixdown (this world has an Audio resource).",
    ),
    (
        "render_out_audio_stems",
        "render.out_start audio.stems writes one WAV per bus (this world has an Audio resource).",
    ),
];

/// Every marker the world offers, with its description, in a fixed order: the compiled-in ones,
/// the audio ones when the world has an `Audio` resource, then the host's.
pub(crate) fn markers(world: &World) -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str)> = ALWAYS.to_vec();
    if world.get_resource::<regia_audio::Audio>().is_some() {
        out.extend_from_slice(AUDIO);
    }
    if let Some(ext) = world.get_resource::<crate::extension::AgentExtension>() {
        out.extend(ext.capabilities().iter().copied());
    }
    out
}

/// The `agent.schema` view of the markers: `[{name, description}]`.
pub(crate) fn markers_json(world: &World) -> Value {
    Value::Array(
        markers(world)
            .into_iter()
            .map(|(name, description)| json!({ "name": name, "description": description }))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(world: &World) -> Vec<&'static str> {
        markers(world).into_iter().map(|(name, _)| name).collect()
    }

    #[test]
    fn compiled_in_markers_are_always_listed() {
        let world = World::new();
        let names = names(&world);
        for expected in [
            "shot_fade_layer",
            "render_out_aov_id",
            "render_out_aov_depth",
            "render_out_aov_motion",
            "render_out_aov_material",
            "shot_rotation_keys",
            "entity_kinds",
        ] {
            assert!(names.contains(&expected), "{expected} in {names:?}");
        }
        assert!(!names.contains(&"render_out_audio"), "no Audio resource");
        assert!(!names.contains(&"editor_tools"), "not hosted by the editor");
    }

    #[test]
    fn audio_markers_need_the_audio_resource() {
        let mut world = World::new();
        world.insert_resource(regia_audio::Audio::new(&regia_audio::AudioSettings {
            headless: true,
            ..regia_audio::AudioSettings::default()
        }));
        let names = names(&world);
        assert!(names.contains(&"render_out_audio"));
        assert!(names.contains(&"render_out_audio_stems"));
    }

    #[test]
    fn the_shots_version_matches_the_format_constant() {
        // v3: ADR-8640's tail keys (the v0.6 format bump, w11-seq-preview-fixes).
        assert_eq!(SHOTS_RON_VERSION, 3);
    }

    #[test]
    fn extension_markers_join_the_list() {
        let mut world = World::new();
        let mut ext = crate::extension::AgentExtension::new();
        ext.add_capability("editor_tools", "The editor hosts external tools.");
        world.insert_resource(ext);
        assert!(names(&world).contains(&"editor_tools"));
        let json = markers_json(&world);
        assert!(json.as_array().is_some_and(|a| {
            a.iter()
                .any(|m| m["name"] == "editor_tools" && m["description"].is_string())
        }));
    }
}
