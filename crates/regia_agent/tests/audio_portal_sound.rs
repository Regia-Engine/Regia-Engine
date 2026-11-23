// SPDX-License-Identifier: MPL-2.0
//! Plan row W-25 (ADR-8416) through the dispatcher: over cooked world geometry, `audio.probe_zones`
//! reports the portal path a source takes and the section reverb armed at the listener, and
//! `audio.list_zones` reports the reverb armed at player 0.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use regia_agent::{LocalReply, call_local};
use regia_app::{App, TimeUpdateStrategy};
use regia_audio::{AudioListener, AudioPlugin, AudioRockField, AudioSettings, GeomSoundField};
use regia_core::math::Vec3;
use regia_scene::{ScenePlugin, Transform};
use regia_world_geom::WorldGeom;
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, GroundInput, cook};
use regia_world_geom::cooked::CookedZoneShape;
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::portal_sound::{SoundCell, SoundPathKind};
use serde_json::{Value, json};

fn switchbacks() -> Arc<WorldGeom> {
    let passages = PassagesDocument::from_layout(for_map("switchbacks").unwrap())
        .to_ron()
        .unwrap();
    let doc = "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"map.passages.ron\"))])";
    let read = |p: &str| {
        if p == "map.passages.ron" {
            Ok(passages.as_bytes().to_vec())
        } else {
            Err(format!("{p}: not found"))
        }
    };
    let terrain =
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384);
    let ground = GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; 16],
    };
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    // The Switchbacks test cook authors ADR-8359's ramp-facet waiver, as the other test cooks do (ADR-8436).
    let out = match run(doc) {
        Ok(out) => out,
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            run(&waive_ramp_facets(doc, &report).unwrap()).unwrap()
        }
        Err(e) => panic!("{e} ({})", e.code()),
    };
    Arc::new(WorldGeom::from_cooked(&out.file.encode().unwrap()).unwrap())
}

fn call(app: &mut App, method: &str, params: Value) -> Value {
    match call_local(app.world_mut(), method, params, Some("portal-sound-test")) {
        LocalReply::Done(Ok(value)) => value,
        reply => panic!("{method}: {reply:?}"),
    }
}

fn arr(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

#[test]
fn probe_and_list_report_the_portal_path_and_the_armed_section_reverb() {
    let geom = switchbacks();
    let field = GeomSoundField::new(geom.clone());
    // A covered section to stand in, and a mouth listener that hears it through the portals.
    let sections: Vec<(u16, Vec3)> = geom
        .zones()
        .iter()
        .filter_map(|z| match z.shape {
            CookedZoneShape::OrientedBox { center_mm, .. } => Some((
                z.section,
                Vec3::new(center_mm[0] as f32, center_mm[1] as f32, center_mm[2] as f32) / 1000.0,
            )),
            CookedZoneShape::Capsule { .. } => None,
        })
        .filter(|(s, c)| field.cell_at(*c) == SoundCell::Section(*s))
        .collect();
    let (section, inside) = sections[0];
    let preset = field.map().section_reverb(section).unwrap().preset.clone();
    let mut routed = None;
    'outer: for portal in geom.portals() {
        if !field.map().portal_is_mouth(portal.id) {
            continue;
        }
        let mouth = field.map().portal_centre(portal.id).unwrap();
        let mouth = Vec3::new(mouth[0], mouth[1], mouth[2]);
        for (dir, dy) in [Vec3::X, Vec3::NEG_X, Vec3::Z, Vec3::NEG_Z]
            .into_iter()
            .flat_map(|d| [0.0, 1.7, 5.0, 12.0].map(|dy| (d, dy)))
        {
            let listener = mouth + dir * 8.0 + Vec3::Y * dy;
            if field.cell_at(listener) != SoundCell::Outdoor {
                continue;
            }
            for (_, source) in &sections {
                let path = field.path(listener, *source);
                if path.kind == SoundPathKind::Portal {
                    routed = Some((listener, *source, path));
                    break 'outer;
                }
            }
        }
    }
    let (listener, source, path) = routed.expect("a mouth listener with a portal path");

    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    app.add_plugins((ScenePlugin, AudioPlugin));
    app.world_mut()
        .resource_mut::<AudioRockField>()
        .set(Arc::new(GeomSoundField::new(geom)));
    app.world_mut()
        .spawn((AudioListener::default(), Transform::from_translation(inside)));
    app.update();

    let list = call(&mut app, "audio.list_zones", json!({}));
    assert_eq!(list["rock_field"], json!(true));
    assert_eq!(list["geom_field"], json!(true));
    assert_eq!(list["armed_reverb"]["section"], json!(section));
    assert_eq!(list["armed_reverb"]["preset"], json!(preset));
    assert_eq!(list["reverb"]["weight"], json!(1.0f32));

    let probe = call(
        &mut app,
        "audio.probe_zones",
        json!({ "listener": arr(listener), "emitter": arr(source), "max_distance": 600.0 }),
    );
    let p = &probe["portal_path"];
    assert_eq!(p["kind"], json!("portal"));
    assert_eq!(p["listener_cell"]["kind"], json!("outdoor"));
    assert_eq!(p["source_cell"]["kind"], json!("section"));
    assert_eq!(p["portals"], json!(path.portals));
    assert_eq!(p["length_m"], json!(path.length_m));
    assert_eq!(probe["distance"], json!(path.length_m));
    assert!(probe["route"].is_object(), "{probe}");
    assert!(probe["armed_reverb"].is_null(), "outdoors arms nothing: {probe}");

    let inside_probe = call(
        &mut app,
        "audio.probe_zones",
        json!({ "listener": arr(inside), "emitter": arr(inside + Vec3::X), "max_distance": 600.0 }),
    );
    assert_eq!(inside_probe["portal_path"]["kind"], json!("direct"));
    assert_eq!(inside_probe["armed_reverb"]["preset"], json!(preset));
    assert_eq!(inside_probe["reverb"]["weight"], json!(1.0f32));

    // Without a geometry field the new keys are null and nothing else changes.
    app.world_mut().resource_mut::<AudioRockField>().clear();
    let plain = call(
        &mut app,
        "audio.probe_zones",
        json!({ "listener": arr(listener), "emitter": arr(source), "max_distance": 600.0 }),
    );
    assert!(plain["portal_path"].is_null());
    assert!(plain["armed_reverb"].is_null());
    assert_eq!(
        call(&mut app, "audio.list_zones", json!({}))["geom_field"],
        json!(false)
    );
}
