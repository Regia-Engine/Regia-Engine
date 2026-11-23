// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/maps/cinder_steps.rs, commit d6ed37677), ADR-8012.
// Lines all, mark: as-is (fixture).
// Stage A (spec 121 § 4, ADR-8050): arithmetic unchanged. Edits: module paths, doc comments (DO27
// file and engine names dropped, missing docs added), lint attributes, visibility of helpers the
// sibling compat modules call.

//! The Cinder Steps (`cinder_steps`): lava tubes under the stepped basalt shield, with the
//! Sounding Line pits as skylights. Design: `spec/maps/cinder_steps.md`; method:
//! `spec/maps/tunnels-switchbacks.md`. The points come from the design's
//! `tunnels_proposal.json`; the sim lane regenerates `tunnels.json` with `tunnels_export`.
//!
//! Floor heights are absolute (the Great Pit floor is the datum, y = 0). The map is
//! mirror-fair about x = 0 and the bases sit at x = +-438 (the host's start table).

use crate::compat::layout::{
    Junction, LAYOUT_VERSION, Opening, Passage, PassageKind, SAMPLE_STEP_M, TunnelLayout,
};

/// Playfield side (m): 204 cells of 5 m.
pub const SIDE_M: f32 = 1020.0;

/// Skylight shafts over the strands: (id, x, z, floor y, radius m). 28 m across. The shaft floor
/// is the tube floor, so a fall lands on it; none is a kill volume (design Q8c).
pub const SKYLIGHTS: [(&str, f32, f32, f32, f32); 6] = [
    ("skylight_w1", -92.3, -89.5, -1.95, 14.0),
    ("skylight_w2", -111.2, -23.6, 0.29, 14.0),
    ("skylight_w3", -45.6, 73.3, 3.58, 14.0),
    ("skylight_e1", 92.3, -89.5, -1.95, 14.0),
    ("skylight_e2", 111.2, -23.6, 0.29, 14.0),
    ("skylight_e3", 45.6, 73.3, 3.58, 14.0),
];

/// The Cinder Steps layout (fixture).
pub fn tunnel_layout() -> TunnelLayout {
    TunnelLayout {
        map_id: "cinder_steps".to_string(),
        version: LAYOUT_VERSION,
        sample_step_m: SAMPLE_STEP_M,
        passages: vec![
            Passage {
                id: "core".into(),
                kind: PassageKind::Tube,
                width_m: 22.0,
                ceiling_m: 12.0,
                points: vec![
                    [0.0, -4.86, -175.0],
                    [0.0, -2.92, -118.0],
                    [0.0, 0.0, -32.0],
                    [0.0, 2.92, 54.0],
                    [0.0, 6.56, 161.0],
                    [0.0, 10.2, 268.0],
                    [0.0, 12.31, 330.0],
                ],
            },
            Passage {
                id: "strand_w".into(),
                kind: PassageKind::Tube,
                width_m: 20.0,
                ceiling_m: 12.0,
                points: vec![
                    [0.0, -2.92, -118.0],
                    [-45.0, -3.26, -128.0],
                    [-85.0, -2.31, -100.0],
                    [-108.0, -0.61, -50.0],
                    [-112.0, 1.43, 10.0],
                    [-112.0, 2.38, 38.0],
                    [-100.0, 3.3, 65.0],
                    [-60.0, 3.67, 76.0],
                    [-28.0, 3.33, 66.0],
                    [0.0, 2.92, 54.0],
                ],
            },
            Passage {
                id: "strand_e".into(),
                kind: PassageKind::Tube,
                width_m: 20.0,
                ceiling_m: 12.0,
                points: vec![
                    [0.0, -2.92, -118.0],
                    [45.0, -3.26, -128.0],
                    [85.0, -2.31, -100.0],
                    [108.0, -0.61, -50.0],
                    [112.0, 1.43, 10.0],
                    [112.0, 2.38, 38.0],
                    [100.0, 3.3, 65.0],
                    [60.0, 3.67, 76.0],
                    [28.0, 3.33, 66.0],
                    [0.0, 2.92, 54.0],
                ],
            },
            Passage {
                id: "pinch_south".into(),
                kind: PassageKind::Gallery,
                width_m: 14.0,
                ceiling_m: 10.0,
                points: vec![[0.0, -7.07, -240.0], [0.0, -9.11, -300.0], [0.0, -11.15, -360.0]],
            },
            Passage {
                id: "ramp_base_w".into(),
                kind: PassageKind::Ramp,
                width_m: 20.0,
                ceiling_m: 12.0,
                points: vec![[-112.0, 2.38, 38.0], [-152.0, 2.38, 40.0], [-190.0, 28.81, 40.0]],
            },
            Passage {
                id: "ramp_base_e".into(),
                kind: PassageKind::Ramp,
                width_m: 20.0,
                ceiling_m: 12.0,
                points: vec![[112.0, 2.38, 38.0], [152.0, 2.38, 40.0], [190.0, 30.72, 40.0]],
            },
            Passage {
                id: "ramp_mid_w".into(),
                kind: PassageKind::Ramp,
                width_m: 20.0,
                ceiling_m: 12.0,
                points: vec![[0.0, 10.2, 268.0], [-88.0, 38.55, 268.0]],
            },
            Passage {
                id: "ramp_mid_e".into(),
                kind: PassageKind::Ramp,
                width_m: 20.0,
                ceiling_m: 12.0,
                points: vec![[0.0, 10.2, 268.0], [88.0, 39.61, 268.0]],
            },
            Passage {
                id: "portal_core_s".into(),
                kind: PassageKind::Ramp,
                width_m: 22.0,
                ceiling_m: 12.0,
                points: vec![[0.0, -4.86, -175.0], [0.0, -5.71, -200.0]],
            },
            Passage {
                id: "portal_core_n".into(),
                kind: PassageKind::Ramp,
                width_m: 22.0,
                ceiling_m: 12.0,
                points: vec![[0.0, 12.31, 330.0], [0.0, 13.16, 355.0]],
            },
            Passage {
                id: "portal_pinch_n".into(),
                kind: PassageKind::Ramp,
                width_m: 14.0,
                ceiling_m: 10.0,
                points: vec![[0.0, -7.07, -240.0], [0.0, -6.46, -222.0]],
            },
            Passage {
                id: "portal_pinch_s".into(),
                kind: PassageKind::Ramp,
                width_m: 14.0,
                ceiling_m: 10.0,
                points: vec![[0.0, -11.15, -360.0], [0.0, -11.76, -378.0]],
            },
        ],
        openings: vec![
            Opening {
                id: "base_w".into(),
                kind: "skylight_rubble".into(),
                ramp: "ramp_base_w".into(),
                top: [-190.0, 28.81, 40.0],
            },
            Opening {
                id: "base_e".into(),
                kind: "skylight_rubble".into(),
                ramp: "ramp_base_e".into(),
                top: [190.0, 30.72, 40.0],
            },
            Opening {
                id: "mid_w".into(),
                kind: "fracture_ramp".into(),
                ramp: "ramp_mid_w".into(),
                top: [-88.0, 38.55, 268.0],
            },
            Opening {
                id: "mid_e".into(),
                kind: "fracture_ramp".into(),
                ramp: "ramp_mid_e".into(),
                top: [88.0, 39.61, 268.0],
            },
            Opening {
                id: "pit_ledge_core".into(),
                kind: "pit_portal".into(),
                ramp: "portal_core_s".into(),
                top: [0.0, -5.71, -200.0],
            },
            Opening {
                id: "pit_north_core".into(),
                kind: "pit_portal".into(),
                ramp: "portal_core_n".into(),
                top: [0.0, 13.16, 355.0],
            },
            Opening {
                id: "pit_ledge_pinch".into(),
                kind: "pit_portal".into(),
                ramp: "portal_pinch_n".into(),
                top: [0.0, -6.46, -222.0],
            },
            Opening {
                id: "pit_sand_pinch".into(),
                kind: "pit_portal".into(),
                ramp: "portal_pinch_s".into(),
                top: [0.0, -11.76, -378.0],
            },
        ],
        junctions: vec![
            Junction {
                id: "south".into(),
                at: [0.0, -2.92, -118.0],
                passages: vec!["core".into(), "strand_w".into(), "strand_e".into()],
            },
            Junction {
                id: "north".into(),
                at: [0.0, 2.92, 54.0],
                passages: vec!["core".into(), "strand_w".into(), "strand_e".into()],
            },
        ],
        crawlways: Vec::new(),
    }
}
