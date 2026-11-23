// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/tunnel_layout/atacama_rail.rs, commit d6ed37677), ADR-8012.
// Lines all, mark: as-is (fixture).
// Stage A (spec 121 § 4, ADR-8050): arithmetic unchanged. Edits: module paths, doc comments (DO27
// file and engine names dropped, missing docs added), lint attributes, visibility of helpers the
// sibling compat modules call.

//! Atacama Rail's hull-level galleries (`spec/maps/atacama_rail.md` section 3.6).
//!
//! Built from the kinds the sim has today: the drift (a tube), the Hall (a wide tube), two mine adits
//! and two glory holes (ramps to the surface) and two cross-cuts (galleries). Mirror-fair: every point
//! has its twin at (-x, z). y is metres above the pit floor (the heightfield datum).
//!
//! Not here, because the sim has no kind for them yet (round 2 engineering): the five pilot ladder
//! raises per wall, `raise_array`, `ore_gallery`, and the level bench-portal openings at the cross-cut
//! ends. Until those land the cross-cuts end in a wall at the bench face.

use crate::compat::layout::{
    Junction, LAYOUT_VERSION, Opening, Passage, PassageKind, SAMPLE_STEP_M, TunnelLayout,
};

/// The drift floor: bench 3.
pub const DRIFT_Y: f32 = 45.0;

fn mirror_pts(pts: &[[f32; 3]]) -> Vec<[f32; 3]> {
    pts.iter().map(|p| [-p[0], p[1], p[2]]).collect()
}

/// The Atacama Rail layout (fixture).
#[expect(
    clippy::unwrap_used,
    reason = "fixture data: every point list is a non-empty literal"
)]
pub fn atacama_rail() -> TunnelLayout {
    let drift = vec![
        [-300.0, DRIFT_Y, 345.0],
        [-240.0, DRIFT_Y, 360.0],
        [-170.0, DRIFT_Y, 372.0],
        [-90.0, DRIFT_Y, 382.0],
        [0.0, DRIFT_Y, 388.0],
        [90.0, DRIFT_Y, 382.0],
        [170.0, DRIFT_Y, 372.0],
        [240.0, DRIFT_Y, 360.0],
        [300.0, DRIFT_Y, 345.0],
    ];
    let adit_w = vec![
        [-300.0, DRIFT_Y, 345.0],
        [-330.0, DRIFT_Y, 340.0],
        [-405.0, 82.75, 325.0],
    ];
    let glory_w = vec![
        [-170.0, DRIFT_Y, 372.0],
        [-172.0, DRIFT_Y, 393.0],
        [-190.0, 73.62, 455.0],
    ];
    let xcut_w = vec![
        [-90.0, DRIFT_Y, 382.0],
        [-90.0, DRIFT_Y, 300.0],
        [-90.0, DRIFT_Y, 181.9],
    ];
    let ramp = |id: &str, points: Vec<[f32; 3]>| Passage {
        id: id.into(),
        kind: PassageKind::Ramp,
        width_m: 20.0,
        ceiling_m: 12.0,
        points,
    };
    let gallery = |id: &str, points: Vec<[f32; 3]>| Passage {
        id: id.into(),
        kind: PassageKind::Gallery,
        width_m: 20.0,
        ceiling_m: 10.0,
        points,
    };
    let top = |p: &[[f32; 3]]| *p.last().unwrap();
    let tw = |p: [f32; 3]| [-p[0], p[1], p[2]];
    let (adit_top, glory_top) = (top(&adit_w), top(&glory_w));
    TunnelLayout {
        map_id: "atacama_rail".to_string(),
        version: LAYOUT_VERSION,
        sample_step_m: SAMPLE_STEP_M,
        passages: vec![
            Passage {
                id: "drift".into(),
                kind: PassageKind::Tube,
                width_m: 22.0,
                ceiling_m: 12.0,
                points: drift,
            },
            Passage {
                id: "hall".into(),
                kind: PassageKind::Tube,
                width_m: 40.0,
                ceiling_m: 14.0,
                points: vec![[0.0, DRIFT_Y, 360.0], [0.0, DRIFT_Y, 420.0]],
            },
            ramp("adit_w", adit_w.clone()),
            ramp("glory_w", glory_w.clone()),
            gallery("xcut_w", xcut_w.clone()),
            ramp("adit_e", mirror_pts(&adit_w)),
            ramp("glory_e", mirror_pts(&glory_w)),
            gallery("xcut_e", mirror_pts(&xcut_w)),
        ],
        openings: vec![
            Opening {
                id: "adit_w".into(),
                kind: "mine_adit".into(),
                ramp: "adit_w".into(),
                top: adit_top,
            },
            Opening {
                id: "glory_w".into(),
                kind: "skylight_rubble".into(),
                ramp: "glory_w".into(),
                top: glory_top,
            },
            Opening {
                id: "adit_e".into(),
                kind: "mine_adit".into(),
                ramp: "adit_e".into(),
                top: tw(adit_top),
            },
            Opening {
                id: "glory_e".into(),
                kind: "skylight_rubble".into(),
                ramp: "glory_e".into(),
                top: tw(glory_top),
            },
        ],
        junctions: vec![
            Junction {
                id: "hall".into(),
                at: [0.0, DRIFT_Y, 388.0],
                passages: vec!["drift".into(), "hall".into()],
            },
            Junction {
                id: "xcut_w".into(),
                at: [-90.0, DRIFT_Y, 382.0],
                passages: vec!["drift".into(), "xcut_w".into()],
            },
            Junction {
                id: "glory_w".into(),
                at: [-170.0, DRIFT_Y, 372.0],
                passages: vec!["drift".into(), "glory_w".into()],
            },
            Junction {
                id: "adit_w".into(),
                at: [-300.0, DRIFT_Y, 345.0],
                passages: vec!["drift".into(), "adit_w".into()],
            },
            Junction {
                id: "xcut_e".into(),
                at: [90.0, DRIFT_Y, 382.0],
                passages: vec!["drift".into(), "xcut_e".into()],
            },
            Junction {
                id: "glory_e".into(),
                at: [170.0, DRIFT_Y, 372.0],
                passages: vec!["drift".into(), "glory_e".into()],
            },
            Junction {
                id: "adit_e".into(),
                at: [300.0, DRIFT_Y, 345.0],
                passages: vec!["drift".into(), "adit_e".into()],
            },
        ],
        crawlways: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![expect(
        clippy::disallowed_methods,
        reason = "test reference values; DO27's asserts are kept as copied (ADR-8050)"
    )]
    use super::*;

    fn xz(a: [f32; 3], b: [f32; 3]) -> f32 {
        ((a[0] - b[0]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    }

    #[test]
    fn the_hull_galleries_match_the_design_and_are_mirror_fair() {
        let l = atacama_rail();
        assert_eq!(l.map_id, "atacama_rail");
        assert_eq!(l.passages.len(), 8);
        assert_eq!(l.openings.len(), 4);
        assert_eq!(l.junctions.len(), 7);
        let s = l.sample(None);
        let len = |id: &str| s.passage(id).unwrap().samples.last().unwrap().s;
        assert!(
            (590.0..=630.0).contains(&len("drift")),
            "drift {} m",
            len("drift")
        );
        assert!((len("adit_w") - len("adit_e")).abs() < 0.01);
        assert!((len("glory_w") - len("glory_e")).abs() < 0.01);
        assert!((len("xcut_w") - len("xcut_e")).abs() < 0.01);
        let drift = s.passage("drift").unwrap();
        let (a, b) = (drift.samples.first().unwrap().p, drift.samples.last().unwrap().p);
        assert!((a[0] + b[0]).abs() < 0.01 && (a[2] - b[2]).abs() < 0.01);
        for (w, e) in [("adit_w", "adit_e"), ("glory_w", "glory_e")] {
            let (ow, oe) = (
                l.openings.iter().find(|o| o.id == w).unwrap(),
                l.openings.iter().find(|o| o.id == e).unwrap(),
            );
            assert!((ow.top[0] + oe.top[0]).abs() < 0.01 && ow.top[2] == oe.top[2] && ow.top[1] == oe.top[1]);
            // Mouths face the bases, about 400 m from the starts (design 3.6: the guide's 300-400 m x 1.2).
        }
        let base_w = [-660.0, 0.0, 25.0];
        let adit = l.openings.iter().find(|o| o.id == "adit_w").unwrap().top;
        assert!(
            (385.0..=425.0).contains(&xz(adit, base_w)),
            "{}",
            xz(adit, base_w)
        );
    }

    #[test]
    fn every_ramp_is_under_the_hover_pathing_steep_limit() {
        let l = atacama_rail();
        let s = l.sample(None);
        for o in &l.openings {
            let ramp = s.passage(&o.ramp).unwrap();
            for w in ramp.samples.windows(2) {
                let rise = (w[1].p[1] - w[0].p[1]).abs();
                assert!(
                    rise <= 0.53 * xz(w[0].p, w[1].p) + 1e-3,
                    "{}: slope {}",
                    o.id,
                    rise / xz(w[0].p, w[1].p)
                );
            }
            let top = ramp.samples.last().unwrap().p;
            assert!(
                xz(top, o.top) < 0.01 && (top[1] - o.top[1]).abs() < 0.01,
                "{}",
                o.id
            );
        }
    }

    #[test]
    fn the_mesh_builds_with_portals() {
        let mesh = atacama_rail().mesh(None);
        assert!(mesh.triangles.len() > 1000, "{} triangles", mesh.triangles.len());
        assert!(mesh.triangles.iter().any(|t| t.portal));
    }
}
