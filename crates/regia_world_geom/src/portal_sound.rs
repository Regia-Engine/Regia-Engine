// SPDX-License-Identifier: MPL-2.0
//! Sound over the cooked portal graph (plan row W-25, ADR-8416): a source inside a passage is heard
//! from the mouth it leaves by, at the length of the path through the portals; rock on a straight
//! line with no portal path occludes it by its thickness; the section the listener stands in arms
//! that section's baked reverb preset.
//!
//! Everything reads the cooked canonical section ([`crate::cooked::WorldGeomCooked`]: sections,
//! portals, zones, blocker plugs) and the core's segment test ([`WorldGeom::segment`],
//! [`WorldGeom::rock_between`]). There is no wall clock, no hashing container and no std
//! transcendental: the same cooked file and the same points give the same bits on every host.
//!
//! **Cells.** A point is in a [`SoundCell::Section`] when a section zone contains it (the smallest
//! zone wins, then the lowest section id), in [`SoundCell::Rock`] when uncut lattice ground lies
//! above it, and [`SoundCell::Outdoor`] otherwise. Portals join two cells; a mouth or an open end
//! joins a section to the outdoors. A portal whose blocker plug starts active is closed.
//!
//! **Paths** ([`PortalSoundMap::path`]). The source and the listener are joined by:
//! 1. a [`SoundPathKind::Direct`] path when no rock lies on the straight segment, or when both stand
//!    in the same section (a cooked section is cut so its chord stays inside it);
//! 2. else the shortest [`SoundPathKind::Portal`] path through portal centres (Dijkstra over the
//!    portal graph, Euclidean leg lengths, ties to the lower portal id), when one exists within
//!    [`PortalSoundSettings::max_path_m`];
//! 3. else an [`SoundPathKind::Occluded`] straight path, with the rock thickness on it.
//!
//! The sound arrives from the last portal before the listener ([`SoundPath::arrival`]); the apparent
//! source stands that way at the path length ([`SoundPath::apparent`]), so a distance curve evaluated
//! at the apparent source attenuates by the path length. This is the shortest-path portal
//! propagation of Tsingos et al., "Modeling acoustics in virtual environments using the uniform
//! theory of diffraction" (SIGGRAPH 2001) and Funkhouser et al., "A beam tracing approach to
//! acoustic modeling for interactive virtual environments" (SIGGRAPH 1998), reduced to one path per
//! source through portal centres: the original code here keeps no beams and no diffraction terms.
//!
//! **Hearing range.** A listener in a covered section hears further: the range scale is
//! [`PortalSoundSettings::underground_hear_mult`] (DO27's underground multiplier,
//! [`crate::compat::sense::hear_range_m`], as a setting).

use regia_detsim::float::{det_max, det_min};
use regia_detsim::math;

use crate::WorldGeom;
use crate::cooked::CookedZoneShape;
use crate::query::GeomState;
use crate::query_segment::FaceMask;

/// What a [`PortalSoundMap`] path may do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PortalSoundSettings {
    /// Hearing-range multiplier for a listener in a covered section (DO27's underground rule,
    /// default [`crate::compat::sense::UNDERGROUND_HEAR_MULT`]).
    pub underground_hear_mult: u32,
    /// Longest portal path searched, metres; a longer one counts as no path.
    pub max_path_m: f32,
    /// Least rock reported on a straight segment that hits rock at all, metres (a single crossing
    /// such as one wall has no measurable span).
    pub min_occlusion_m: f32,
    /// How far above a point the ground probe looks for uncut lattice ground, metres.
    pub ground_probe_m: f32,
}

impl Default for PortalSoundSettings {
    fn default() -> Self {
        Self {
            underground_hear_mult: crate::compat::sense::UNDERGROUND_HEAR_MULT,
            max_path_m: 4000.0,
            min_occlusion_m: 0.5,
            ground_probe_m: 4096.0,
        }
    }
}

/// Where a point is, as sound sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SoundCell {
    /// Open air above the ground.
    Outdoor,
    /// Inside the rock: under uncut ground and in no section zone. No portal reaches it.
    Rock,
    /// Inside the zone of this cooked section.
    Section(u16),
}

/// How a [`SoundPath`] reaches the listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoundPathKind {
    /// The straight segment: no rock on it, or both ends in one section.
    Direct,
    /// Through one or more portals.
    Portal,
    /// The straight segment through rock: no portal path exists.
    Occluded,
}

impl SoundPathKind {
    /// The protocol name: `direct`, `portal` or `occluded`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Portal => "portal",
            Self::Occluded => "occluded",
        }
    }
}

/// One source heard by one listener.
#[derive(Clone, Debug, PartialEq)]
pub struct SoundPath {
    /// How the sound travels.
    pub kind: SoundPathKind,
    /// The source's cell.
    pub source_cell: SoundCell,
    /// The listener's cell.
    pub listener_cell: SoundCell,
    /// Portal ids crossed, source side first (empty unless [`SoundPathKind::Portal`]).
    pub portals: Vec<u16>,
    /// The polyline: source, each portal centre, listener; world metres.
    pub points: Vec<[f32; 3]>,
    /// Length of the polyline, metres (the distance the attenuation curve sees).
    pub length_m: f32,
    /// Straight-line distance, metres.
    pub direct_m: f32,
    /// Unit vector from the listener toward where the sound arrives (zero when they coincide).
    pub arrival: [f32; 3],
    /// The apparent source: `listener + arrival * length_m`.
    pub apparent: [f32; 3],
    /// Metres of rock on the straight segment (0 unless [`SoundPathKind::Occluded`]).
    pub occlusion_m: f32,
    /// Hearing-range scale for this listener (the underground multiplier in a covered section).
    pub range_scale: f32,
}

/// The reverb a section arms (its cooked zone's preset).
#[derive(Clone, Debug, PartialEq)]
pub struct SectionReverb {
    /// The cooked section id.
    pub section: u16,
    /// The preset id from the cook: `tube`, `gallery` or `open`.
    pub preset: String,
    /// The cavity size, metres.
    pub size_m: f32,
    /// Whether the section is covered (underground).
    pub covered: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct PortalNode {
    id: u16,
    sides: [Option<u16>; 2],
    centre: [f32; 3],
    open: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum ZoneShapeM {
    Box {
        centre: [f32; 3],
        half: [f32; 3],
        cos: f32,
        sin: f32,
    },
    Capsule {
        a: [f32; 3],
        b: [f32; 3],
        radius: f32,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct ZoneCell {
    section: u16,
    shape: ZoneShapeM,
    volume: f32,
}

/// The cooked portal graph and section zones of one [`WorldGeom`], in metres, built once at load.
#[derive(Clone, Debug, PartialEq)]
pub struct PortalSoundMap {
    portals: Vec<PortalNode>,
    zones: Vec<ZoneCell>,
    reverbs: Vec<Option<SectionReverb>>,
}

fn m(mm: i32) -> f32 {
    crate::units::mm_to_m(0, mm)
}

fn v_m(p: [i32; 3]) -> [f32; 3] {
    p.map(m)
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = sub(a, b);
    math::sqrt(dot(d, d))
}

fn normalize_or_zero(v: [f32; 3]) -> [f32; 3] {
    let len = math::sqrt(dot(v, v));
    if len > 1e-6 && len.is_finite() {
        v.map(|c| c / len)
    } else {
        [0.0; 3]
    }
}

impl ZoneShapeM {
    fn of(shape: &CookedZoneShape) -> Self {
        match *shape {
            CookedZoneShape::OrientedBox {
                center_mm,
                half_mm,
                yaw,
            } => {
                // The cook builds the box in a frame turned by `-yaw` about +y (`sections::zone_box`):
                // local x = c·dx + s·dz, local z = −s·dx + c·dz.
                let yaw = if yaw == 0.0 { 0.0 } else { yaw };
                Self::Box {
                    centre: v_m(center_mm),
                    half: v_m(half_mm),
                    cos: math::cos(yaw),
                    sin: math::sin(yaw),
                }
            }
            CookedZoneShape::Capsule {
                a_mm,
                b_mm,
                radius_mm,
            } => Self::Capsule {
                a: v_m(a_mm),
                b: v_m(b_mm),
                radius: m(radius_mm),
            },
        }
    }

    fn contains(&self, p: [f32; 3]) -> bool {
        match self {
            Self::Box {
                centre,
                half,
                cos,
                sin,
            } => {
                let d = sub(p, *centre);
                let lx = cos * d[0] + sin * d[2];
                let lz = -sin * d[0] + cos * d[2];
                lx.abs() <= half[0] && d[1].abs() <= half[1] && lz.abs() <= half[2]
            }
            Self::Capsule { a, b, radius } => {
                let ab = sub(*b, *a);
                let len2 = dot(ab, ab);
                let t = if len2 > 0.0 {
                    det_min(det_max(dot(sub(p, *a), ab) / len2, 0.0), 1.0)
                } else {
                    0.0
                };
                let q = [a[0] + ab[0] * t, a[1] + ab[1] * t, a[2] + ab[2] * t];
                let d = sub(p, q);
                *radius > 0.0 && dot(d, d) <= radius * radius
            }
        }
    }

    fn volume(&self) -> f32 {
        match self {
            Self::Box { half, .. } => 8.0 * half[0] * half[1] * half[2],
            Self::Capsule { a, b, radius } => {
                let r2 = radius * radius;
                std::f32::consts::PI * r2 * (dist(*a, *b) + radius * (4.0 / 3.0))
            }
        }
    }
}

impl PortalSoundMap {
    /// Builds the map from the cooked sections, portals, zones and blocker plugs of `geom`.
    #[must_use]
    pub fn new(geom: &WorldGeom) -> Self {
        let plugs = geom.blockers();
        let portals = geom
            .portals()
            .iter()
            .map(|p| {
                let count = p.polygon_mm.len();
                let n = if count == 0 { 1.0 } else { count as f32 };
                let mut c = [0.0f32; 3];
                for v in &p.polygon_mm {
                    let v = v_m(*v);
                    c = [c[0] + v[0], c[1] + v[1], c[2] + v[2]];
                }
                let centre = if p.polygon_mm.is_empty() {
                    v_m(p.nav_mm)
                } else {
                    c.map(|x| x / n)
                };
                let closed = p
                    .blocker
                    .and_then(|b| plugs.iter().find(|plug| plug.id == b))
                    .is_some_and(|plug| plug.initially_active);
                PortalNode {
                    id: p.id,
                    sides: p.sides,
                    centre,
                    open: !closed,
                }
            })
            .collect();
        let zones = geom
            .zones()
            .iter()
            .map(|z| {
                let shape = ZoneShapeM::of(&z.shape);
                ZoneCell {
                    section: z.section,
                    volume: shape.volume(),
                    shape,
                }
            })
            .collect();
        let sections = geom.sections();
        let mut reverbs: Vec<Option<SectionReverb>> = vec![None; sections.len()];
        for z in geom.zones() {
            let Some(slot) = reverbs.get_mut(usize::from(z.section)) else {
                continue;
            };
            if slot.is_none() {
                *slot = Some(SectionReverb {
                    section: z.section,
                    preset: z.reverb_preset.clone(),
                    size_m: z.reverb_size_m,
                    covered: sections[usize::from(z.section)].covered,
                });
            }
        }
        Self {
            portals,
            zones,
            reverbs,
        }
    }

    /// Number of portals in the graph.
    #[must_use]
    pub fn portal_count(&self) -> usize {
        self.portals.len()
    }

    /// The centre of portal `id` (the mean of its polygon), world metres.
    #[must_use]
    pub fn portal_centre(&self, id: u16) -> Option<[f32; 3]> {
        self.portals.get(usize::from(id)).map(|p| p.centre)
    }

    /// Whether portal `id` joins a section to the outdoors (a mouth or an open end).
    #[must_use]
    pub fn portal_is_mouth(&self, id: u16) -> bool {
        self.portals
            .get(usize::from(id))
            .is_some_and(|p| p.sides.iter().any(Option::is_none))
    }

    /// The baked reverb of `section`.
    #[must_use]
    pub fn section_reverb(&self, section: u16) -> Option<&SectionReverb> {
        self.reverbs.get(usize::from(section)).and_then(Option::as_ref)
    }

    /// The cell `p` is in.
    #[must_use]
    pub fn cell_at(
        &self,
        geom: &WorldGeom,
        state: &GeomState,
        settings: &PortalSoundSettings,
        p: [f32; 3],
    ) -> SoundCell {
        let mut best: Option<(f32, u16)> = None;
        for z in &self.zones {
            if z.shape.contains(p) {
                let better = match best {
                    None => true,
                    Some((v, s)) => z.volume.total_cmp(&v).then(z.section.cmp(&s)).is_lt(),
                };
                if better {
                    best = Some((z.volume, z.section));
                }
            }
        }
        if let Some((_, section)) = best {
            return SoundCell::Section(section);
        }
        let up = [p[0], p[1] + settings.ground_probe_m, p[2]];
        if geom.segment(state, p, up, FaceMask::GROUND).is_some() {
            SoundCell::Rock
        } else {
            SoundCell::Outdoor
        }
    }

    fn touches(node: &PortalNode, cell: SoundCell) -> bool {
        match cell {
            SoundCell::Outdoor => node.sides.iter().any(Option::is_none),
            SoundCell::Rock => false,
            SoundCell::Section(s) => node.sides.contains(&Some(s)),
        }
    }

    fn share_cell(a: &PortalNode, b: &PortalNode) -> bool {
        a.sides.iter().any(|s| b.sides.contains(s))
    }

    /// Metres of rock on the straight segment `from → to`: the span between the first rock face
    /// seen from each end (roofs and covered walls through [`WorldGeom::rock_between`], uncut ground
    /// and open-cut walls through [`WorldGeom::segment`]); an end in [`SoundCell::Rock`] is itself in rock, so its side
    /// of the span starts at that end. At least [`PortalSoundSettings::min_occlusion_m`] when any
    /// rock is hit; 0 when the segment is clear.
    #[must_use]
    pub fn occlusion_m(
        &self,
        geom: &WorldGeom,
        state: &GeomState,
        settings: &PortalSoundSettings,
        from: [f32; 3],
        to: [f32; 3],
    ) -> f32 {
        let from_rock = self.cell_at(geom, state, settings, from) == SoundCell::Rock;
        let to_rock = self.cell_at(geom, state, settings, to) == SoundCell::Rock;
        Self::rock_span(geom, state, settings, (from, from_rock), (to, to_rock))
    }

    fn rock_span(
        geom: &WorldGeom,
        state: &GeomState,
        settings: &PortalSoundSettings,
        (from, from_rock): ([f32; 3], bool),
        (to, to_rock): ([f32; 3], bool),
    ) -> f32 {
        let first = |a: [f32; 3], b: [f32; 3]| -> Option<f32> {
            let shell = geom.rock_between(state, a, b);
            // `rock_between` counts covered walls only (what hides a unit from above); for sound an
            // open cut's wall is rock too, as is uncut ground.
            let ground = geom
                .segment(state, a, b, FaceMask(FaceMask::GROUND.0 | FaceMask::WALL.0))
                .map(|h| h.t);
            match (shell, ground) {
                (Some(x), Some(y)) => Some(det_min(x, y)),
                (x, y) => x.or(y),
            }
        };
        let t_in = if from_rock { Some(0.0) } else { first(from, to) };
        let t_out = if to_rock { Some(0.0) } else { first(to, from) };
        let (t_in, t_out) = match (t_in, t_out) {
            (None, None) => return 0.0,
            (Some(a), Some(b)) => (a, b),
            (Some(a), None) => (a, 1.0 - a),
            (None, Some(b)) => (1.0 - b, b),
        };
        let span = (1.0 - t_in - t_out) * dist(from, to);
        det_max(span, settings.min_occlusion_m)
    }

    /// The path from `source` to `listener` (see the module docs for the rules).
    #[must_use]
    pub fn path(
        &self,
        geom: &WorldGeom,
        state: &GeomState,
        settings: &PortalSoundSettings,
        source: [f32; 3],
        listener: [f32; 3],
    ) -> SoundPath {
        let source_cell = self.cell_at(geom, state, settings, source);
        let listener_cell = self.cell_at(geom, state, settings, listener);
        let direct_m = dist(source, listener);
        let range_scale = match listener_cell {
            SoundCell::Section(s) if self.section_reverb(s).is_some_and(|r| r.covered) => {
                settings.underground_hear_mult as f32
            }
            _ => 1.0,
        };
        let straight = |kind: SoundPathKind, occlusion_m: f32| {
            let arrival = normalize_or_zero(sub(source, listener));
            SoundPath {
                kind,
                source_cell,
                listener_cell,
                portals: Vec::new(),
                points: vec![source, listener],
                length_m: direct_m,
                direct_m,
                arrival,
                apparent: source,
                occlusion_m,
                range_scale,
            }
        };
        let same_section =
            matches!((source_cell, listener_cell), (SoundCell::Section(a), SoundCell::Section(b)) if a == b);
        let occlusion = Self::rock_span(
            geom,
            state,
            settings,
            (listener, listener_cell == SoundCell::Rock),
            (source, source_cell == SoundCell::Rock),
        );
        if occlusion == 0.0 || same_section {
            return straight(SoundPathKind::Direct, 0.0);
        }
        match self.portal_route(source_cell, listener_cell, source, listener, settings.max_path_m) {
            Some((portals, length_m)) => {
                let mut points = Vec::with_capacity(portals.len() + 2);
                points.push(source);
                points.extend(portals.iter().map(|&p| self.portals[usize::from(p)].centre));
                points.push(listener);
                // The sound arrives from the last point before the listener that is not on it.
                let arrival = points
                    .iter()
                    .rev()
                    .skip(1)
                    .map(|p| sub(*p, listener))
                    .find(|d| dot(*d, *d) > 0.0625)
                    .map_or([0.0; 3], normalize_or_zero);
                let apparent = [
                    listener[0] + arrival[0] * length_m,
                    listener[1] + arrival[1] * length_m,
                    listener[2] + arrival[2] * length_m,
                ];
                SoundPath {
                    kind: SoundPathKind::Portal,
                    source_cell,
                    listener_cell,
                    portals,
                    points,
                    length_m,
                    direct_m,
                    arrival,
                    apparent,
                    occlusion_m: 0.0,
                    range_scale,
                }
            }
            None => straight(SoundPathKind::Occluded, occlusion),
        }
    }

    /// Shortest portal sequence from a point in `from` to a point in `to` (Dijkstra, Euclidean legs,
    /// ties to the lower portal id).
    fn portal_route(
        &self,
        from: SoundCell,
        to: SoundCell,
        source: [f32; 3],
        listener: [f32; 3],
        max_m: f32,
    ) -> Option<(Vec<u16>, f32)> {
        if from == SoundCell::Rock || to == SoundCell::Rock || from == to {
            return None;
        }
        let n = self.portals.len();
        let mut best = vec![f32::INFINITY; n];
        let mut prev: Vec<Option<usize>> = vec![None; n];
        let mut done = vec![false; n];
        for (i, p) in self.portals.iter().enumerate() {
            if p.open && Self::touches(p, from) {
                best[i] = dist(source, p.centre);
            }
        }
        loop {
            let mut pick: Option<(usize, f32)> = None;
            for (i, (&b, &d)) in best.iter().zip(&done).enumerate() {
                if !d && b.is_finite() && pick.is_none_or(|(_, p)| b < p) {
                    pick = Some((i, b));
                }
            }
            let Some((i, reached)) = pick else { break };
            if reached > max_m {
                break;
            }
            done[i] = true;
            let a = &self.portals[i];
            for (j, b) in self.portals.iter().enumerate() {
                if done[j] || !b.open || !Self::share_cell(a, b) {
                    continue;
                }
                let d = reached + dist(a.centre, b.centre);
                if d < best[j] {
                    best[j] = d;
                    prev[j] = Some(i);
                }
            }
        }
        let mut end: Option<(usize, f32)> = None;
        for (i, p) in self.portals.iter().enumerate() {
            if !done[i] || !Self::touches(p, to) {
                continue;
            }
            let total = best[i] + dist(p.centre, listener);
            if total <= max_m && end.is_none_or(|(_, t)| total < t) {
                end = Some((i, total));
            }
        }
        let (mut i, total) = end?;
        let mut ids = vec![self.portals[i].id];
        while let Some(j) = prev[i] {
            ids.push(self.portals[j].id);
            i = j;
        }
        ids.reverse();
        Some((ids, total))
    }
}

/// Arms the reverb of the section the listener is in: [`ReverbArm::update`] reports each change of
/// section, with the cooked preset of the section entered (`None` when the listener leaves every
/// section).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReverbArm {
    active: Option<SectionReverb>,
    changes: u32,
}

impl ReverbArm {
    /// The armed section reverb.
    #[must_use]
    pub fn active(&self) -> Option<&SectionReverb> {
        self.active.as_ref()
    }

    /// How many times the armed reverb changed.
    #[must_use]
    pub fn changes(&self) -> u32 {
        self.changes
    }

    /// Re-arms for a listener at `cell`; returns `true` when the armed section changed.
    pub fn update(&mut self, map: &PortalSoundMap, cell: SoundCell) -> bool {
        let next = match cell {
            SoundCell::Section(s) => map.section_reverb(s).cloned(),
            SoundCell::Outdoor | SoundCell::Rock => None,
        };
        let changed = next.as_ref().map(|r| r.section) != self.active.as_ref().map(|r| r.section);
        if changed {
            self.active = next;
            self.changes += 1;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_hear_mult_is_the_sense_rule() {
        let s = PortalSoundSettings::default();
        assert_eq!(
            crate::compat::sense::hear_range_m(1000, true),
            1000 * s.underground_hear_mult
        );
    }

    #[test]
    fn box_zone_uses_the_cook_frame() {
        // A 10 m × 2 m box along +x+z (yaw = atan2(dz, dx) = 45°): a point 4 m along the chord is in,
        // the mirrored point (4 m along +x−z) is out.
        let yaw = math::atan2(1.0, 1.0);
        let shape = ZoneShapeM::of(&CookedZoneShape::OrientedBox {
            center_mm: [0, 0, 0],
            half_mm: [5000, 1000, 1000],
            yaw,
        });
        let r = 4.0 / math::sqrt(2.0);
        assert!(shape.contains([r, 0.0, r]));
        assert!(!shape.contains([r, 0.0, -r]));
    }

    #[test]
    fn capsule_zone_contains_its_axis() {
        let shape = ZoneShapeM::of(&CookedZoneShape::Capsule {
            a_mm: [0, 0, 0],
            b_mm: [10_000, 0, 0],
            radius_mm: 2000,
        });
        assert!(shape.contains([5.0, 1.5, 0.0]));
        assert!(!shape.contains([5.0, 2.5, 0.0]));
        assert!(shape.contains([-1.5, 0.0, 0.0]));
    }
}
