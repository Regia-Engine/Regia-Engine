// SPDX-License-Identifier: MPL-2.0
//! Audio-zone mouths from the cells' portals (stage 2; `docs/specs/25-sim-bridge-and-do27.md`
//! § 7.6 and § 7.7). DO27's bake lists each cavity's mouths in `regia_audio::AudioZonePortals`
//! (zone-local). A map that already ships `<map>.viscells.ron` has those mouths as portals:
//! [`audio_zone_mouths`] derives them so the bake does not author them twice.
//!
//! `regia_render` and `regia_audio` do not depend on each other, so the function speaks in plain
//! values: the caller says which audio zone (an index of its own) each cell belongs to and where
//! each zone stands, and turns every [`AudioZoneMouth`] of zone `i` into a
//! `ZonePortal { position, radius }` on zone `i`'s `AudioZonePortals`.

use regia_core::math::{Quat, Vec3};

use super::VisCells;

/// Where an audio zone stands: its entity's world translation and rotation (the frame
/// `regia_audio::PlacedZone` places mouths in; scale is not applied there either).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioZonePose {
    /// World-space centre, metres.
    pub centre: Vec3,
    /// World-space orientation.
    pub rotation: Quat,
}

/// One mouth of an audio zone, derived from a portal ([`audio_zone_mouths`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioZoneMouth {
    /// The zone, as an index into the caller's zone list.
    pub zone: usize,
    /// The portal's centroid, local to the zone (`rotation⁻¹ · (centroid − centre)`).
    pub position: Vec3,
    /// The portal's circumradius from its centroid (the farthest corner), metres: a listener
    /// within it stands in the mouth.
    pub radius: f32,
    /// The portal's index in [`VisCells::portals`].
    pub portal: usize,
}

/// The mouths of every audio zone, from the cells' portals.
///
/// `cell_zones` maps a cell id to a zone index (the first entry for a cell wins; a cell not
/// listed belongs to no zone, typically cell 0, the outdoors); `zones[i]` is zone `i`'s pose. A
/// portal whose two cells are in the same zone is inside it and is no mouth. Otherwise each side
/// in a zone gets one mouth at the portal: a tunnel cell next to the outdoors gets one, two
/// adjacent cavities with zones of their own get one each. An entry naming a zone index past
/// `zones` is skipped, as is a cell id the file does not list.
///
/// `out` is cleared first and filled in portal order (side `a` before side `b`); it keeps its
/// capacity, so a caller that reuses it allocates nothing once warm.
pub fn audio_zone_mouths(
    cells: &VisCells,
    cell_zones: &[(u16, usize)],
    zones: &[AudioZonePose],
    out: &mut Vec<AudioZoneMouth>,
) {
    out.clear();
    let zone_of = |cell_index: u32| -> Option<usize> {
        let id = cells.cells.get(cell_index as usize)?.id;
        cell_zones
            .iter()
            .find(|(cell, _)| *cell == id)
            .map(|(_, zone)| *zone)
            .filter(|zone| *zone < zones.len())
    };
    for (index, portal) in cells.portals.iter().enumerate() {
        let (a, b) = (zone_of(portal.a), zone_of(portal.b));
        if a.is_some() && a == b {
            continue;
        }
        let radius = portal
            .quad
            .iter()
            .map(|corner| corner.distance(portal.center))
            .fold(0.0f32, f32::max);
        for zone in [a, b].into_iter().flatten() {
            let pose = zones[zone];
            out.push(AudioZoneMouth {
                zone,
                position: pose.rotation.inverse() * (portal.center - pose.centre),
                radius,
                portal: index,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tunnel (cell 1) opening to the outdoors at z +4, a gallery (cell 2) behind it through a
    /// 2 × 2 m door at z −4, and a side room (cell 3) off the gallery at x +4 (same zone as the
    /// gallery).
    const MINE: &str = "(
        cells: [
            (id: 1, bounds: [(center: (0.0, 1.0, 0.0), half_extents: (2.0, 1.0, 4.0))]),
            (id: 2, bounds: [(center: (0.0, 1.0, -8.0), half_extents: (4.0, 1.0, 4.0))]),
            (id: 3, bounds: [(center: (6.0, 1.0, -8.0), half_extents: (2.0, 1.0, 2.0))]),
        ],
        portals: [
            (a: 1, b: 0, quad: [(-1.0, 0.0, 4.0), (1.0, 0.0, 4.0), (1.0, 2.0, 4.0), (-1.0, 2.0, 4.0)]),
            (a: 1, b: 2, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)]),
            (a: 2, b: 3, quad: [(4.0, 0.0, -9.0), (4.0, 0.0, -7.0), (4.0, 2.0, -7.0), (4.0, 2.0, -9.0)]),
        ],
    )";

    fn close(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-4
    }

    #[test]
    fn mouths_are_the_portals_that_leave_a_zone_in_zone_local_space() {
        let cells = VisCells::from_ron(MINE).expect("loads");
        // Zone 0: the tunnel, turned a quarter about +Y and centred at (0, 1, 0). Zone 1: the
        // gallery and the side room, unturned, centred at (0, 1, −8). The outdoors has no zone.
        let zones = [
            AudioZonePose {
                centre: Vec3::new(0.0, 1.0, 0.0),
                rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            },
            AudioZonePose {
                centre: Vec3::new(0.0, 1.0, -8.0),
                rotation: Quat::IDENTITY,
            },
        ];
        let mut out = Vec::new();
        audio_zone_mouths(&cells, &[(1, 0), (2, 1), (3, 1)], &zones, &mut out);
        // The tunnel's outdoor mouth, then the door between tunnel and gallery for each zone. The
        // gallery-to-side-room door is inside zone 1: no mouth.
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!((out[0].zone, out[0].portal), (0, 0));
        assert_eq!((out[1].zone, out[1].portal), (0, 1));
        assert_eq!((out[2].zone, out[2].portal), (1, 1));
        // The outdoor mouth's centroid (0, 1, 4) is (0, 0, 4) from the tunnel's centre; the
        // zone's quarter turn about +Y maps that offset to (−4, 0, 0) zone-local.
        let rotated = zones[0].rotation * out[0].position;
        assert!(close(rotated, Vec3::new(0.0, 0.0, 4.0)), "{out:?}");
        assert!(close(out[0].position, Vec3::new(-4.0, 0.0, 0.0)), "{out:?}");
        // The door seen from the gallery: (0, 1, −4) is (0, 0, 4) from its centre.
        assert!(close(out[2].position, Vec3::new(0.0, 0.0, 4.0)), "{out:?}");
        // A 2 × 2 m quad: the corners sit √2 m from the centroid.
        for mouth in &out {
            assert!(
                (mouth.radius - std::f32::consts::SQRT_2).abs() < 1e-5,
                "{mouth:?}"
            );
        }
        // Round trip into the frame regia_audio::PlacedZone uses: centre + rotation · local.
        for mouth in &out {
            let pose = zones[mouth.zone];
            let world = pose.centre + pose.rotation * mouth.position;
            assert!(close(world, cells.portals[mouth.portal].center), "{mouth:?}");
        }
    }

    #[test]
    fn unmapped_cells_and_bad_zone_indices_give_no_mouths() {
        let cells = VisCells::from_ron(MINE).expect("loads");
        let zones = [AudioZonePose {
            centre: Vec3::ZERO,
            rotation: Quat::IDENTITY,
        }];
        let mut out = vec![AudioZoneMouth {
            zone: 9,
            position: Vec3::ZERO,
            radius: 0.0,
            portal: 9,
        }];
        // No mapping: no zone, no mouth (and the stale entry is cleared).
        audio_zone_mouths(&cells, &[], &zones, &mut out);
        assert!(out.is_empty());
        // Zone index 5 does not exist; an unlisted cell id maps nothing; the first entry for
        // cell 2 wins over the second.
        audio_zone_mouths(&cells, &[(1, 5), (77, 0), (2, 0), (2, 5)], &zones, &mut out);
        // Cell 2 is zone 0 and cells 1 and 3 have none: the doors 1-2 and 2-3 are its mouths.
        let portals: Vec<(usize, usize)> = out.iter().map(|m| (m.zone, m.portal)).collect();
        assert_eq!(portals, vec![(0, 1), (0, 2)]);
    }
}
