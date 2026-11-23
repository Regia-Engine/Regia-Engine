// SPDX-License-Identifier: MPL-2.0
//! Plan row W-25 (ADR-8416): portal sound is a pure function of the cooked file, with the same bits on
//! every host. The digest folds every path between the Switchbacks section centres and points around
//! its mouths (kind, portals, length, apparent source, occlusion, range scale); it is pinned, and the
//! same pin must hold on x86_64 and aarch64.
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used)]

use regia_world_geom::WorldGeom;
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, GroundInput, cook};
use regia_world_geom::cooked::CookedZoneShape;
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::portal_sound::{PortalSoundMap, PortalSoundSettings, SoundCell, SoundPathKind};

/// FNV-1a 64 of the folded words, recorded on x86-64 Linux (the build server) and equal on aarch64 macOS (the Mac).
/// Re-pin only for an intended change, with the reason in the commit. Re-pinned by lane v06-section-yaw-canon
/// (ADR-8579): section zone half extents cross through `m_to_mm`, which moves the zone cells some paths resolve
/// (same path-kind counts); equal on x86-64 Linux (the build server) and aarch64 Android (the test Pixel 9 Pro).
const PIN: u64 = 0x599a_b4a3_51fb_f944;

fn switchbacks() -> WorldGeom {
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
    WorldGeom::from_cooked(&out.file.encode().unwrap()).unwrap()
}

fn fnv(h: &mut u64, word: u32) {
    for b in word.to_le_bytes() {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(0x0100_0000_01b3);
    }
}

fn cell(c: SoundCell) -> u32 {
    match c {
        SoundCell::Outdoor => 0x1_0000,
        SoundCell::Rock => 0x2_0000,
        SoundCell::Section(s) => u32::from(s),
    }
}

#[test]
fn portal_sound_bits_are_pinned() {
    let geom = switchbacks();
    let map = PortalSoundMap::new(&geom);
    let state = geom.new_state();
    let settings = PortalSoundSettings::default();
    let mut points: Vec<[f32; 3]> = geom
        .zones()
        .iter()
        .filter_map(|z| match z.shape {
            CookedZoneShape::OrientedBox { center_mm, .. } => Some(center_mm.map(|c| c as f32 / 1000.0)),
            CookedZoneShape::Capsule { .. } => None,
        })
        .collect();
    for p in geom.portals() {
        if map.portal_is_mouth(p.id) {
            let c = map.portal_centre(p.id).unwrap();
            for d in [
                [6.0, 3.0, 0.0],
                [-6.0, 3.0, 0.0],
                [0.0, 3.0, 6.0],
                [0.0, 3.0, -6.0],
            ] {
                points.push([c[0] + d[0], c[1] + d[1], c[2] + d[2]]);
            }
        }
    }
    points.push([-500.0, -271.0, -500.0]);
    points.push([-500.0, -254.3, -500.0]);
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let (mut portal, mut occluded, mut direct) = (0u32, 0u32, 0u32);
    for l in &points {
        for s in &points {
            let p = map.path(&geom, &state, &settings, *s, *l);
            match p.kind {
                SoundPathKind::Portal => portal += 1,
                SoundPathKind::Occluded => occluded += 1,
                SoundPathKind::Direct => direct += 1,
            }
            fnv(&mut h, p.kind as u32);
            fnv(&mut h, cell(p.source_cell));
            fnv(&mut h, cell(p.listener_cell));
            for id in &p.portals {
                fnv(&mut h, u32::from(*id));
            }
            fnv(&mut h, p.length_m.to_bits());
            for c in p.apparent {
                fnv(&mut h, c.to_bits());
            }
            fnv(&mut h, p.occlusion_m.to_bits());
            fnv(&mut h, p.range_scale.to_bits());
        }
    }
    eprintln!(
        "portal_sound digest {h:#018x} over {} points: {portal} portal, {occluded} occluded, {direct} direct",
        points.len()
    );
    assert!(
        portal > 0 && occluded > 0 && direct > 0,
        "every path kind is covered"
    );
    assert_eq!(h, PIN, "portal sound bits moved: {h:#018x}");
}
