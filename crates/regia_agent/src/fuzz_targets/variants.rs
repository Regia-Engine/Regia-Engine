// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for the formats of spec 83 (ADR-2323): prefab variants (`.variant.ron`,
//! `prefab-variant`), the reflected physical camera and lens effects inside scene files (`physical-camera`,
//! `lens-effects`, ADR-3196) and
//! profile captures (`.rprof.json`, `profile-capture`), and level variants (`.levelvariant.ron`,
//! `level-variant`, ADR-7823). Fixtures are values the formats' own code builds.

use std::fmt::Display;

use regia_core::id::Guid;
use regia_ecs::prelude::TypeRegistry;
use regia_render::{LensEffects, PhysicalCamera};
use regia_scene::prefab_variant::PrefabVariantFile;
use regia_telemetry::profile_capture::{
    CaptureNode, CaptureSpan, CaptureThread, CaptureTimelineFrame, GpuPassTime, ProfileCapture,
};

use super::FuzzTarget;
use super::formats::{reflected_fixture, reflected_load, text};

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

/// A variant of a lamp prefab with one override, one added component, one removed component and
/// one removed child (every edit kind but an added child, whose entity the scene fuzz covers).
fn prefab_variant_fixture() -> String {
    let base = Guid::from_u128(0x3f7a_1c2e_8b64_4a35_9d20_6f1e_2b3c_4d5a);
    let light = Guid::from_u128(0x9b2e_6f0a_1e42_4c11_8d5a_3c2f_7e10_d0aa);
    let knob = Guid::from_u128(0x1c0f_fee0_0000_4000_8000_0000_0000_0001);
    format!(
        r#"(
    format_version: {version},
    base: (
        prefab: "asset:{base}",
        overrides: [(target: "{light}", component: "game::Lamp", path: ".intensity", value: 250.0)],
        added_components: [(target: "{light}", component: "game::Flicker", value: (rate: 2.5))],
        removed_components: [(target: "{knob}", component: "game::Tags")],
        removed_children: ["{knob}"],
    ),
)"#,
        version = regia_scene::prefab_variant::PREFAB_VARIANT_FORMAT_VERSION,
    )
}

fn prefab_variant_load(s: &str) -> Result<String, String> {
    PrefabVariantFile::from_ron_str(s)
        .and_then(|f| f.to_ron_string())
        .map_err(e)
}

/// A level variant with every kind of difference (ADR-7823): an override, a component added and
/// one removed, a record edit, an added and a removed entity, a resource set and one dropped.
fn level_variant_fixture() -> String {
    let base = Guid::from_u128(0x3f7a_1c2e_8b64_4a35_9d20_6f1e_2b3c_4d5b);
    let gate = Guid::from_u128(0x9b2e_6f0a_1e42_4c11_8d5a_3c2f_7e10_d0ab);
    let crate_id = Guid::from_u128(0x1c0f_fee0_0000_4000_8000_0000_0000_0002);
    format!(
        r#"LevelVariant(
    format_version: {version},
    base: "asset:{base}",
    overrides: [(target: "{gate}", component: "regia_scene::transform::Transform", path: ".translation", value: (5.0, 0.0, 0.0))],
    added_components: [(target: "{gate}", component: "game::Flicker", value: (rate: 2.5))],
    removed_components: [(target: "{gate}", component: "game::Tags")],
    record_edits: [(target: "{gate}", field: "name", value: Some("Night gate"))],
    added_entities: [(guid: "{crate_id}", name: "Crate", parent: Some("{gate}"), components: {{}})],
    removed_entities: ["{base}"],
    resources: {{"game::Weather": (rain: true)}},
    removed_resources: ["game::Fog"],
)"#,
        version = regia_scene::level_variant::LEVEL_VARIANT_FORMAT_VERSION,
    )
}

fn level_variant_load(s: &str) -> Result<String, String> {
    regia_scene::level_variant::LevelVariantFile::from_ron_str(s)
        .and_then(|f| f.to_ron_string())
        .map_err(e)
}

fn physical_camera_register(registry: &mut TypeRegistry) {
    regia_render::register_physical_camera_types(registry);
}

fn physical_camera_fixture() -> String {
    reflected_fixture(
        physical_camera_register,
        &PhysicalCamera {
            shutter_angle: 0.25,
            aperture_f_stops: 2.8,
            focal_length: Some(0.035),
            exposure: regia_render::CameraExposureMode::Physical,
            ..PhysicalCamera::default()
        },
    )
}

fn physical_camera_load(s: &str) -> Result<String, String> {
    reflected_load::<PhysicalCamera>(s, physical_camera_register, |lens| {
        *lens = lens.migrate().map_err(e)?;
        Ok(())
    })
}

fn lens_effects_register(registry: &mut TypeRegistry) {
    regia_render::register_lens_effects_types(registry);
}

fn lens_effects_fixture() -> String {
    reflected_fixture(
        lens_effects_register,
        &LensEffects {
            flare_intensity: 1.5,
            starburst: 2.0,
            after_dof: true,
            ..LensEffects::OFF
        },
    )
}

fn lens_effects_load(s: &str) -> Result<String, String> {
    reflected_load::<LensEffects>(s, lens_effects_register, |effects| {
        *effects = effects.migrate().map_err(e)?;
        Ok(())
    })
}

/// A two-level call tree with a GPU pass list, the folded stacks and a one-frame timeline.
fn profile_capture_fixture() -> String {
    let leaf = |name: &str, ms: f64| CaptureNode {
        name: name.into(),
        inclusive_ms: ms,
        self_ms: ms,
        calls: 1.0,
        children: Vec::new(),
    };
    let capture = ProfileCapture {
        format_version: regia_telemetry::profile_capture::PROFILE_CAPTURE_FORMAT_VERSION,
        frames: 8,
        frame_ms: 4.5,
        cpu: vec![CaptureNode {
            name: "frame".into(),
            inclusive_ms: 4.0,
            self_ms: 1.0,
            calls: 1.0,
            children: vec![leaf("extract", 1.25), leaf("render", 1.75)],
        }],
        truncated: 0,
        gpu: vec![GpuPassTime {
            pass: "shadow".into(),
            ms: 0.5,
        }],
        folded: "frame 1000\nframe;extract 1250\nframe;render 1750\n".into(),
        // Version 2 (ADR-7325): one frame of the per-thread timeline.
        timeline: vec![CaptureTimelineFrame {
            number: 41,
            frame_ns: 4_500_000,
            dropped: 0,
            threads: vec![CaptureThread {
                index: 0,
                name: "main".into(),
                spans: vec![
                    CaptureSpan {
                        name: "frame".into(),
                        start_ns: 0,
                        end_ns: 4_000_000,
                        depth: 0,
                    },
                    CaptureSpan {
                        name: "extract".into(),
                        start_ns: 1_000_000,
                        end_ns: 2_250_000,
                        depth: 1,
                    },
                ],
            }],
        }],
        timeline_truncated: 0,
    };
    capture.to_json().unwrap_or_default()
}

fn profile_capture_load(s: &str) -> Result<String, String> {
    ProfileCapture::from_json(s).and_then(|c| c.to_json()).map_err(e)
}

/// The spec 83 targets.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        text(
            "level-variant",
            Some("level-variant"),
            regia_scene::level_variant::LEVEL_VARIANT_FORMAT_VERSION,
            true,
            level_variant_fixture,
            level_variant_load,
        ),
        text(
            "prefab-variant",
            Some("prefab-variant"),
            regia_scene::prefab_variant::PREFAB_VARIANT_FORMAT_VERSION,
            true,
            prefab_variant_fixture,
            prefab_variant_load,
        ),
        text(
            "physical-camera",
            Some("physical-camera"),
            regia_render::PHYSICAL_CAMERA_FORMAT_VERSION,
            true,
            physical_camera_fixture,
            physical_camera_load,
        ),
        text(
            "lens-effects",
            Some("lens-effects"),
            regia_render::LENS_EFFECTS_FORMAT_VERSION,
            true,
            lens_effects_fixture,
            lens_effects_load,
        ),
        text(
            "profile-capture",
            Some("profile-capture"),
            regia_telemetry::profile_capture::PROFILE_CAPTURE_FORMAT_VERSION,
            true,
            profile_capture_fixture,
            profile_capture_load,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fixture_loads_and_round_trips() {
        for target in targets() {
            let super::super::FuzzInput::Text { fixture, load } = target.input else {
                panic!("{}: text target", target.name);
            };
            let text = fixture();
            assert!(!text.is_empty(), "{}: empty fixture", target.name);
            let once = load(&text).unwrap_or_else(|err| panic!("{}: {err}\n{text}", target.name));
            let twice = load(&once).unwrap_or_else(|err| panic!("{}: {err}\n{once}", target.name));
            assert_eq!(once, twice, "{}: canonical re-encoding", target.name);
        }
    }

    #[test]
    fn mutated_inputs_never_panic() {
        for target in targets() {
            let report = super::super::run(&target, None, 11, 300).expect("run");
            assert!(report.is_clean(), "{}: {:?}", target.name, report.first_failure);
            assert!(report.accepted > 0, "{}: some mutations load", target.name);
        }
    }
}
