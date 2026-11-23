// SPDX-License-Identifier: MPL-2.0
//! Plan row W-25 (ADR-8416): sound travels the cooked portal path, rock occludes through the core's
//! segment test, and each section arms its baked reverb preset. The geometry is DO27's Switchbacks
//! layout, cooked by `regia_world_geom::cook` the way the passage-section tests cook it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use regia_app::{App, TimeUpdateStrategy};
use regia_audio::{
    Audio, AudioEmitter, AudioListener, AudioPlugin, AudioRockField, AudioSettings, GeomSoundField,
    ReverbPreset, RockField, RockTuning, RolloffCurve, SoundAsset, SoundAssets, SoundClip, ZoneField,
    ZoneHearing, scaled_range,
};
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

const RATE: u32 = 48_000;

/// Flat ground at -256 m (the Switchbacks canyon floor), as the Stage A sampler sees it.
fn canyon_terrain() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384)
}

fn canyon_ground() -> GroundInput {
    GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; 16],
    }
}

/// The cooked Switchbacks file.
fn switchbacks_bytes() -> Vec<u8> {
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
    let terrain = canyon_terrain();
    let ground = canyon_ground();
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
    out.file.encode().unwrap()
}

fn switchbacks() -> Arc<WorldGeom> {
    Arc::new(WorldGeom::from_cooked(&switchbacks_bytes()).unwrap())
}

fn v(a: [f32; 3]) -> Vec3 {
    Vec3::new(a[0], a[1], a[2])
}

fn mm(a: [i32; 3]) -> Vec3 {
    Vec3::new(a[0] as f32, a[1] as f32, a[2] as f32) / 1000.0
}

fn angle_deg(a: Vec3, b: Vec3) -> f32 {
    a.normalize()
        .dot(b.normalize())
        .clamp(-1.0, 1.0)
        .acos()
        .to_degrees()
}

/// Every section zone: section id, centre (m), the box's local z axis in world (unit), its half
/// width along that axis (m).
fn zones(geom: &WorldGeom) -> Vec<(u16, Vec3, Vec3, Vec3)> {
    geom.zones()
        .iter()
        .filter_map(|z| match z.shape {
            CookedZoneShape::OrientedBox {
                center_mm,
                half_mm,
                yaw,
            } => Some((
                z.section,
                mm(center_mm),
                Vec3::new(-yaw.sin(), 0.0, yaw.cos()),
                mm(half_mm),
            )),
            CookedZoneShape::Capsule { .. } => None,
        })
        .collect()
}

fn gun() -> AudioEmitter {
    AudioEmitter {
        min_distance: 2.0,
        max_distance: 600.0,
        rolloff: RolloffCurve::Inverse,
        ..AudioEmitter::default()
    }
}

/// Outdoor listener points just outside each mouth: (mouth portal id, mouth centre, listener).
fn mouth_listeners(field: &GeomSoundField, zone_list: &[(u16, Vec3, Vec3, Vec3)]) -> Vec<(u16, Vec3, Vec3)> {
    let geom = field.geom();
    let mut out = Vec::new();
    for portal in geom.portals() {
        if !field.map().portal_is_mouth(portal.id) {
            continue;
        }
        let Some(inside) = portal.sides.iter().flatten().next() else {
            continue;
        };
        let Some((_, centre, ..)) = zone_list.iter().find(|z| z.0 == *inside) else {
            continue;
        };
        let mouth = v(field.map().portal_centre(portal.id).unwrap());
        let mut outward = mouth - *centre;
        outward.y = 0.0;
        let outward = outward.normalize_or_zero();
        'found: for step in [4.0, 6.0, 9.0, 12.0, 16.0, 24.0] {
            for dy in [0.0, 1.7, 3.0, 5.0, 8.0, 12.0] {
                let listener = mouth + outward * step + Vec3::Y * dy;
                if field.cell_at(listener) == SoundCell::Outdoor {
                    out.push((portal.id, mouth, listener));
                    break 'found;
                }
            }
        }
    }
    out
}

#[test]
fn gunfire_around_a_bend_comes_from_the_mouth_direction() {
    let geom = switchbacks();
    let field = GeomSoundField::new(geom.clone());
    let zone_list = zones(&geom);
    let listeners = mouth_listeners(&field, &zone_list);
    let mouths: Vec<(u16, [f32; 3], [Option<u16>; 2])> = geom
        .portals()
        .iter()
        .filter(|p| field.map().portal_is_mouth(p.id))
        .map(|p| (p.id, field.map().portal_centre(p.id).unwrap(), p.sides))
        .collect();
    assert!(
        !listeners.is_empty(),
        "Switchbacks has outdoor points by its mouths; mouths {mouths:?}, {} portals, {} zones",
        geom.portals().len(),
        zone_list.len()
    );
    let tuning = RockTuning::default();
    let mut checked = 0;
    for (mouth_id, mouth, listener) in &listeners {
        for (section, source, ..) in &zone_list {
            if field.cell_at(*source) != SoundCell::Section(*section) {
                continue;
            }
            let path = field.path(*listener, *source);
            if path.kind != SoundPathKind::Portal || path.portals.last() != Some(mouth_id) {
                continue;
            }
            let to_mouth = *mouth - *listener;
            let straight = *source - *listener;
            if angle_deg(to_mouth, straight) < 20.0 {
                continue; // in line with the mouth: not around a bend
            }
            // The straight line runs through rock.
            assert!(field.rock_between(*listener, *source).unwrap() > 0.0);
            // What the per-frame emitter update does with it.
            let heard = ZoneHearing::evaluate(
                &ZoneField::default(),
                Some(&field),
                &tuning,
                &gun(),
                *listener,
                *source,
            );
            let route = heard.route.expect("routed through the portals");
            let from = route.position - *listener;
            assert!(
                angle_deg(from, to_mouth) <= 10.0,
                "section {section}: heard {:.1}° off the mouth",
                angle_deg(from, to_mouth)
            );
            assert!(
                angle_deg(from, straight) > 10.0,
                "section {section}: heard from the straight line through rock"
            );
            // Attenuated by the path length, not the straight distance.
            assert!(
                (heard.distance - path.length_m).abs() < 1e-2,
                "{} vs {}",
                heard.distance,
                path.length_m
            );
            assert!(path.length_m >= path.direct_m);
            assert_eq!(
                heard.rock_m.map(|m| m < 0.6),
                Some(true),
                "the mouth leg is open air: {:?}",
                heard.rock_m
            );
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "no source around a bend from any mouth listener: {listeners:?}"
    );
}

#[test]
fn rock_between_occludes_a_direct_path() {
    let geom = switchbacks();
    let field = GeomSoundField::new(geom.clone());
    let state = geom.new_state();
    let tuning = RockTuning::default();
    let mut checked = 0;
    let mut through_roof = 0;
    for (section, centre, side, half) in zones(&geom) {
        let covered = field.map().section_reverb(section).is_some_and(|r| r.covered);
        if field.cell_at(centre) != SoundCell::Section(section) || !covered {
            continue;
        }
        // A source in the rock above the passage (through its roof) and beside it (through its
        // wall): out of every zone, under the ground.
        let above = [2.0f32, 4.0, 6.0]
            .into_iter()
            .map(|k| (centre + Vec3::Y * (half.y + k), half.y + k))
            .find(|(p, _)| field.cell_at(*p) == SoundCell::Rock);
        let beside = Some((centre + side * (half.z + 4.0), half.z + 4.0))
            .filter(|(p, _)| field.cell_at(*p) == SoundCell::Rock);
        for (source, reach, roof) in above
            .map(|(p, r)| (p, r, true))
            .into_iter()
            .chain(beside.map(|(p, r)| (p, r, false)))
        {
            if roof {
                // W-11's rock test sees the passage roof on the straight line.
                let t = geom.rock_between(&state, centre.to_array(), source.to_array());
                assert!(t.is_some(), "section {section}: the roof is between");
                through_roof += 1;
            }
            let path = field.path(centre, source);
            assert_eq!(
                path.kind,
                SoundPathKind::Occluded,
                "section {section}: no portal reaches the rock"
            );
            assert!(path.portals.is_empty());
            assert!(
                path.occlusion_m >= 0.5 && path.occlusion_m <= reach + 0.01,
                "section {section} (roof {roof}): {} m of rock within {reach} m",
                path.occlusion_m
            );
            let blocked = ZoneHearing::evaluate(
                &ZoneField::default(),
                Some(&field),
                &tuning,
                &gun(),
                centre,
                source,
            );
            // The same emitter with no field, stretched by the field's range scale (the listener is
            // underground): only the rock differs.
            let free = ZoneHearing::evaluate(
                &ZoneField::default(),
                None,
                &tuning,
                &scaled_range(&gun(), path.range_scale),
                centre,
                source,
            );
            assert_eq!(blocked.rock_m, Some(path.occlusion_m));
            assert!(
                blocked.route.is_none(),
                "an occluded path keeps the source where it is"
            );
            let expected = free.gain * tuning.gain(path.occlusion_m);
            assert!(
                (blocked.gain - expected).abs() <= expected * 1e-4,
                "attenuated by the occlusion amount: {} vs {expected}",
                blocked.gain
            );
            assert!(blocked.gain < free.gain);
            checked += 1;
        }
    }
    assert!(through_roof > 0, "no covered section with rock above it");
    assert!(checked > 0, "no section with rock beside it");

    // A source buried under open ground: the ground segment and the buried end give the depth.
    let (x, z) = (-500.0, -500.0);
    let listener = Vec3::new(x, -256.0 + 1.7, z);
    let source = Vec3::new(x, -256.0 - 15.0, z);
    assert_eq!(field.cell_at(listener), SoundCell::Outdoor);
    assert_eq!(field.cell_at(source), SoundCell::Rock);
    let path = field.path(listener, source);
    assert_eq!(path.kind, SoundPathKind::Occluded);
    assert!((path.occlusion_m - 15.0).abs() < 0.05, "{}", path.occlusion_m);
}

fn app_with(field: GeomSoundField, listener: Vec3) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    app.add_plugins((ScenePlugin, AudioPlugin));
    let samples: Vec<f32> = (0..RATE as usize)
        .map(|i| if i % 2 == 0 { 0.25 } else { -0.25 })
        .collect();
    app.world_mut().resource_mut::<SoundAssets>().add(
        "shot",
        SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("clip")),
    );
    app.world_mut()
        .resource_mut::<AudioRockField>()
        .set(Arc::new(field));
    app.world_mut()
        .spawn((AudioListener::default(), Transform::from_translation(listener)));
    app
}

#[test]
fn section_reverb_armed_from_the_bake() {
    let geom = switchbacks();
    let field = GeomSoundField::new(geom.clone());
    let mut arm = regia_world_geom::portal_sound::ReverbArm::default();
    let mut entered = 0u32;
    let mut presets = Vec::new();
    for (section, centre, ..) in zones(&geom) {
        if field.cell_at(centre) != SoundCell::Section(section) {
            continue;
        }
        let cooked = geom
            .zones()
            .iter()
            .find(|z| z.section == section)
            .map(|z| z.reverb_preset.clone())
            .unwrap();
        // Entering the section arms it, with the cooked preset id.
        assert!(arm.update(field.map(), field.cell_at(centre)));
        entered += 1;
        assert_eq!(arm.active().map(|r| r.section), Some(section));
        assert_eq!(arm.active().map(|r| r.preset.clone()), Some(cooked.clone()));
        let armed = field.armed_zone(centre).expect("armed");
        assert_eq!(armed.section, u32::from(section));
        assert_eq!(armed.preset_id, cooked);
        assert_eq!(Some(armed.zone.preset), ReverbPreset::from_name(&cooked));
        presets.push(cooked);
        // Staying put arms nothing new; stepping outside disarms.
        assert!(!arm.update(field.map(), field.cell_at(centre)));
        assert!(arm.update(field.map(), SoundCell::Outdoor));
        assert!(arm.active().is_none());
    }
    assert!(entered > 1, "Switchbacks has sections");
    assert_eq!(arm.changes(), entered * 2);
    assert!(presets.iter().any(|p| p == "tube"), "{presets:?}");

    // The running app applies the armed preset to player 0's reverb.
    let (section, centre, ..) = zones(&geom)
        .into_iter()
        .find(|(s, c, ..)| field.cell_at(*c) == SoundCell::Section(*s))
        .unwrap();
    let preset = ReverbPreset::from_name(&field.map().section_reverb(section).unwrap().preset).unwrap();
    let mut app = app_with(GeomSoundField::new(geom.clone()), centre);
    for _ in 0..3 {
        app.update();
    }
    let mix = app
        .world()
        .resource::<Audio>()
        .reverb_mix()
        .expect("armed reverb");
    assert!((mix.feedback - preset.params().feedback).abs() < 1e-4, "{mix:?}");
    assert!((mix.weight - 1.0).abs() < 1e-4, "{mix:?}");

    // Outdoors, far from every section, nothing is armed.
    let mut outside = app_with(GeomSoundField::new(geom), Vec3::new(-500.0, -250.0, -500.0));
    outside.update();
    assert!(outside.world().resource::<Audio>().reverb_mix().is_none());
}

#[test]
fn portal_sound_is_a_pure_function_of_the_cooked_file() {
    let bytes = switchbacks_bytes();
    assert_eq!(bytes, switchbacks_bytes(), "the cook is byte-stable");
    let a = GeomSoundField::new(Arc::new(WorldGeom::from_cooked(&bytes).unwrap()));
    let b = GeomSoundField::new(Arc::new(WorldGeom::from_cooked(&bytes).unwrap()));
    let geom = a.geom();
    let zone_list = zones(geom);
    let mut points: Vec<Vec3> = zone_list.iter().map(|z| z.1).collect();
    points.extend(mouth_listeners(&a, &zone_list).into_iter().map(|m| m.2));
    points.push(Vec3::new(-500.0, -271.0, -500.0));
    let bits = |field: &GeomSoundField| -> Vec<u32> {
        let mut out = Vec::new();
        for l in &points {
            for s in &points {
                let p = field.path(*l, *s);
                out.push(p.kind as u32);
                out.extend(p.portals.iter().map(|&id| u32::from(id)));
                out.push(p.length_m.to_bits());
                out.extend(p.apparent.iter().map(|c| c.to_bits()));
                out.push(p.occlusion_m.to_bits());
                out.push(p.range_scale.to_bits());
            }
        }
        out
    };
    let first = bits(&a);
    assert_eq!(first, bits(&a), "the same field answers the same twice");
    assert_eq!(first, bits(&b), "a second load answers the same");
    // Under a covered section the listener hears further (the underground rule as a setting).
    let covered = zone_list
        .iter()
        .find(|(s, c, ..)| {
            a.cell_at(*c) == SoundCell::Section(*s) && a.map().section_reverb(*s).is_some_and(|r| r.covered)
        })
        .expect("a covered section");
    let path = a.path(covered.1, covered.1 + Vec3::X);
    assert_eq!(
        path.range_scale,
        regia_world_geom::compat::sense::hear_range_m(1000, true) as f32 / 1000.0
    );
}
