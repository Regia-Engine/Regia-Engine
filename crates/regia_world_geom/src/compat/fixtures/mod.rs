// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/tunnel_layout.rs, commit d6ed37677), ADR-8012.
// Lines 201-305, mark: as-is (fixture).
// Stage A (spec 121 § 4, ADR-8050): arithmetic unchanged. Edits: module paths, doc comments (DO27
// file and engine names dropped, missing docs added), lint attributes, visibility of helpers the
// sibling compat modules call.
// DO27's map layouts are test fixtures here, not engine API (plan row W-01).

//! DO27's three armed tunnel layouts as test fixtures (plan row W-01): The Switchbacks, Atacama Rail and
//! The Cinder Steps. Compiled only for this crate's tests and behind the `fixtures` feature; a host
//! registers its own layouts (`TunnelLayout` values) instead of calling [`for_map`].

pub mod atacama_rail;
pub mod cinder_steps;
mod onfoot;

pub use onfoot::ON_FOOT_PARAMS;

use crate::compat::layout::{
    Junction, LAYOUT_VERSION, Opening, Passage, PassageKind, SAMPLE_STEP_M, TunnelLayout,
};

/// The map's tunnel layout, if it has one.
pub fn for_map(map_id: &str) -> Option<TunnelLayout> {
    match map_id {
        "switchbacks" => Some(switchbacks()),
        "atacama_rail" => Some(atacama_rail::atacama_rail()),
        "cinder_steps" => Some(cinder_steps::tunnel_layout()),
        _ => None,
    }
}

/// The Switchbacks (`spec/maps/tunnels-switchbacks.md`): one lava tube between
/// the bases, a mine gallery joined to it at two junctions, four openings.
/// Mirror-fair: every point has its twin at (-x, z); the bases start on the
/// canyon floor at x = ±550, z = -126 (the host's team starts).
pub fn switchbacks() -> TunnelLayout {
    // The canyon floor is the lowest ground on the map (-256 m). The tube floor sits 30 m
    // under it, so the 12 m ceiling keeps 18 m of rock everywhere.
    const FLOOR: f32 = -286.0;
    const SURFACE: f32 = -256.0;
    let mirror = |half: &[[f32; 2]]| -> Vec<[f32; 3]> {
        // West half then its mirror, the centre point once.
        let mut pts: Vec<[f32; 3]> = half.iter().map(|p| [p[0], FLOOR, p[1]]).collect();
        let n = half.len();
        for i in (0..n).rev() {
            if half[i][0] == 0.0 {
                continue;
            }
            pts.push([-half[i][0], FLOOR, half[i][1]]);
        }
        pts
    };
    let tube = mirror(&[
        [-470.0, -40.0],
        [-420.0, 60.0],
        [-300.0, 150.0],
        [-235.0, 165.0],
        [-190.0, 160.0],
        [-120.0, 40.0],
        [-60.0, -110.0],
        [0.0, -150.0],
    ]);
    let gallery = mirror(&[[-120.0, 40.0], [-100.0, 120.0], [-50.0, 150.0], [0.0, 150.0]]);
    // A ramp: its bottom on the tube's centreline, any bends, its top at the opening. It leaves the tube
    // steeply (not along it) and stays level with the tube floor until its whole width is out of the
    // tube; then it climbs (see `sample`). v13 ramps left the tube at 21° from its centreline and climbed
    // at once: a sloped slab sat inside the tube for 30-60 m, two floors at different heights in one
    // space, and hulls were thrown between them (owner, v13 preview).
    let ramp = |pts: &[[f32; 2]]| -> Vec<[f32; 3]> {
        let n = pts.len();
        pts.iter()
            .enumerate()
            .map(|(i, p)| [p[0], if i + 1 == n { SURFACE } else { FLOOR }, p[1]])
            .collect()
    };
    TunnelLayout {
        map_id: "switchbacks".to_string(),
        version: LAYOUT_VERSION,
        sample_step_m: SAMPLE_STEP_M,
        passages: vec![
            Passage {
                id: "tube".into(),
                kind: PassageKind::Tube,
                width_m: 22.0,
                ceiling_m: 12.0,
                points: tube,
            },
            Passage {
                id: "gallery".into(),
                kind: PassageKind::Gallery,
                width_m: 18.0,
                ceiling_m: 10.0,
                points: gallery,
            },
            Passage {
                id: "ramp_west".into(),
                kind: PassageKind::Ramp,
                width_m: 20.0,
                ceiling_m: 12.0,
                points: ramp(&[[-142.8, 85.1], [-166.0, 82.0], [-178.0, 96.0], [-170.0, 130.0]]),
            },
            Passage {
                id: "ramp_east".into(),
                kind: PassageKind::Ramp,
                width_m: 20.0,
                ceiling_m: 12.0,
                points: ramp(&[[142.8, 85.1], [166.0, 82.0], [178.0, 96.0], [170.0, 130.0]]),
            },
            Passage {
                id: "ramp_mid_west".into(),
                kind: PassageKind::Ramp,
                width_m: 20.0,
                ceiling_m: 12.0,
                points: ramp(&[[0.0, -150.0], [-48.0, -78.0]]),
            },
            Passage {
                id: "ramp_mid_east".into(),
                kind: PassageKind::Ramp,
                width_m: 20.0,
                ceiling_m: 12.0,
                points: ramp(&[[0.0, -150.0], [48.0, -78.0]]),
            },
        ],
        openings: vec![
            Opening {
                id: "west".into(),
                kind: "skylight_rubble".into(),
                ramp: "ramp_west".into(),
                top: [-170.0, SURFACE, 130.0],
            },
            Opening {
                id: "east".into(),
                kind: "mine_adit".into(),
                ramp: "ramp_east".into(),
                top: [170.0, SURFACE, 130.0],
            },
            Opening {
                id: "mid_west".into(),
                kind: "fracture_ramp".into(),
                ramp: "ramp_mid_west".into(),
                top: [-48.0, SURFACE, -78.0],
            },
            Opening {
                id: "mid_east".into(),
                kind: "skylight".into(),
                ramp: "ramp_mid_east".into(),
                top: [48.0, SURFACE, -78.0],
            },
        ],
        junctions: vec![
            Junction {
                id: "west".into(),
                at: [-120.0, FLOOR, 40.0],
                passages: vec!["tube".into(), "gallery".into()],
            },
            Junction {
                id: "east".into(),
                at: [120.0, FLOOR, 40.0],
                passages: vec!["tube".into(), "gallery".into()],
            },
        ],
        crawlways: Vec::new(),
    }
}
