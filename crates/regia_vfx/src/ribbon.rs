// SPDX-License-Identifier: MPL-2.0
//! Ribbon emitters (`.vfx.ron` v7, [`RenderMode::Ribbon`](crate::format::RenderMode::Ribbon)):
//! particles simulated on the CPU and drawn as camera-facing strips through the trail pipelines
//! ([`crate::gpu::VfxGpu::draw_trails`]).
//!
//! A [`RibbonTrack`] holds one emitter's particles of one instance in spawn order (a ring, capped,
//! the oldest dropped first). Each frame the scene moves the emitter's new spawn jobs into it,
//! steps it with the GPU kernel's force model (gravity, drag, wind, vortex, attractor, the
//! `kill_below` plane and the kill volume: [`crate::kill_volume`]) and appends its strips to the
//! scene's trail vertices. A strip runs from the newest particle (the head, `t = 0`) to the
//! oldest (the tail, `t = 1`); its side vector at each point is perpendicular to the strip's
//! tangent and to the direction to the camera (a view-facing ribbon). Every strip of `n >= 2`
//! points draws `6 (n - 1)` triangle-list vertices ([`strip_vertices`]); the scene stops at
//! [`crate::gpu::layout::MAX_TRAIL_VERTICES`] and counts what it left out.
//!
//! Nothing here allocates once the track's ring and the vertex buffer reached their working
//! size: the tracks are `VecDeque`s that keep their capacity, the strips are built in one pass.

use std::collections::VecDeque;

use regia_core::math::Vec3;

use crate::format::{BlendMode, EmitterDef, RibbonLink, RibbonTexture};
use crate::gpu::TrailVertex;
use crate::kill_volume::ResolvedKillVolume;
use crate::sim::SpawnJob;

/// Points one [`RibbonLink::SpawnGroup`] track holds across its strips; past it the oldest
/// points go.
pub const MAX_GROUP_TRACK_POINTS: usize = 4096;

/// Vertices a strip of `points` points draws: `6 (points - 1)`, and none below two points.
#[must_use]
pub fn strip_vertices(points: usize) -> usize {
    6 * points.saturating_sub(1)
}

/// One ribbon particle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RibbonPoint {
    /// World position.
    pub position: Vec3,
    /// Velocity, m/s.
    pub velocity: Vec3,
    /// Age, seconds.
    pub age: f32,
    /// Lifetime, seconds.
    pub life: f32,
    /// Spawn size (full width), metres.
    pub size: f32,
    /// Spawn linear RGBA.
    pub color: [f32; 4],
    /// The frame (spawn group) it spawned in.
    pub group: u32,
}

/// What one step of a ribbon track needs from the world and the instance.
#[derive(Debug, Clone, Copy)]
pub struct RibbonWorld {
    /// Step, seconds.
    pub dt: f32,
    /// Scene wind, m/s.
    pub wind: Vec3,
    /// Instance origin (a point on the vortex axis).
    pub origin: Vec3,
    /// The attractor's world position.
    pub attractor: Vec3,
    /// The emitter's kill volume, resolved for the instance.
    pub kill: ResolvedKillVolume,
}

/// Counts of one ribbon build ([`crate::scene::VfxScene::ribbon_stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RibbonStats {
    /// Strips with two or more points.
    pub strips: u32,
    /// Ribbon points alive.
    pub points: u32,
    /// Vertices written.
    pub vertices: u32,
    /// Vertices left out at the trail-vertex cap.
    pub dropped_vertices: u32,
}

/// One emitter's ribbon particles in one instance.
#[derive(Debug, Default, Clone)]
pub struct RibbonTrack {
    points: VecDeque<RibbonPoint>,
    /// Spawn groups pushed so far.
    group: u32,
}

impl RibbonTrack {
    /// Points alive.
    #[must_use]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Whether no point is alive.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// The points, oldest first.
    pub fn points(&self) -> impl Iterator<Item = &RibbonPoint> {
        self.points.iter()
    }

    /// Moves every point by `-shift` (the floating-origin step of spec 78 § 12.2).
    pub fn shift_origin(&mut self, shift: Vec3) {
        for p in &mut self.points {
            p.position -= shift;
        }
    }

    /// Adds this frame's spawn jobs as one spawn group, then drops the oldest points past the
    /// cap: `max_points` for [`RibbonLink::Emitter`]; for [`RibbonLink::SpawnGroup`] a group keeps
    /// its first `max_points` jobs and the track at most [`MAX_GROUP_TRACK_POINTS`].
    pub fn push_jobs(&mut self, jobs: &[SpawnJob], emitter: &EmitterDef) {
        let ribbon = &emitter.render.ribbon;
        let max_points = ribbon.max_points.max(2) as usize;
        let (take, cap) = match ribbon.link {
            RibbonLink::Emitter => (jobs.len(), max_points),
            RibbonLink::SpawnGroup => (jobs.len().min(max_points), MAX_GROUP_TRACK_POINTS),
        };
        if take == 0 {
            return;
        }
        self.group = self.group.wrapping_add(1);
        for job in &jobs[..take] {
            self.points.push_back(RibbonPoint {
                position: Vec3::from_array(job.position),
                velocity: Vec3::from_array(job.velocity),
                age: 0.0,
                life: job.lifetime,
                size: job.size,
                color: job.color,
                group: self.group,
            });
        }
        while self.points.len() > cap {
            self.points.pop_front();
        }
    }

    /// One step of the kernel's force model; dead and killed points leave.
    pub fn step(&mut self, emitter: &EmitterDef, world: RibbonWorld) {
        let u = &emitter.update;
        let dt = world.dt;
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        let g = Vec3::new(u.gravity.0, u.gravity.1, u.gravity.2);
        let c = u.wind_response * u.wind_drag;
        let vortex = u.vortex.as_ref().map(|v| {
            let radial = if v.radial.is_finite() { v.radial } else { 0.0 };
            (crate::format::dir_from_tuple(v.axis), v.strength, radial)
        });
        let attractor = u.attractor.as_ref().map(|a| a.strength);
        self.points.retain_mut(|p| {
            p.age += dt;
            if p.age >= p.life {
                return false;
            }
            let mut v = (p.velocity + (g + world.wind * c) * dt) / (1.0 + (u.drag.max(0.0) + c) * dt);
            if let Some((axis, strength, extra)) = vortex
                && (strength != 0.0 || extra != 0.0)
            {
                let rel = p.position - world.origin;
                let tangential = axis.cross(rel);
                let radial = rel - axis * rel.dot(axis);
                // The kernel's rule: a built-in outward 0.25 × strength plus `VortexDef::radial`.
                v += (tangential / (tangential.length() + 1e-5) * strength
                    + radial / (radial.length() + 1e-5) * (strength * 0.25 + extra))
                    * dt;
            }
            if let Some(strength) = attractor
                && strength != 0.0
            {
                let d = world.attractor - p.position;
                let dist2 = d.dot(d) + 1.0;
                v += d / dist2.sqrt() * (strength / dist2) * dt;
            }
            p.velocity = v;
            p.position += v * dt;
            if u.kill_below.is_some_and(|y| p.position.y < y) || world.kill.kills(p.position) {
                return false;
            }
            true
        });
    }

    /// Appends this track's strips (triangle list) to `out`, facing `camera`, writing at most up
    /// to `cap` vertices in total; returns the counts.
    pub fn build(
        &self,
        emitter: &EmitterDef,
        camera: Vec3,
        out: &mut Vec<TrailVertex>,
        cap: usize,
    ) -> RibbonStats {
        let mut stats = RibbonStats {
            points: self.points.len() as u32,
            ..RibbonStats::default()
        };
        let mut start = 0;
        while start < self.points.len() {
            let end = match emitter.render.ribbon.link {
                RibbonLink::Emitter => self.points.len(),
                RibbonLink::SpawnGroup => {
                    let group = self.points[start].group;
                    let mut end = start + 1;
                    while end < self.points.len() && self.points[end].group == group {
                        end += 1;
                    }
                    end
                }
            };
            let n = end - start;
            if n >= 2 {
                stats.strips += 1;
                let want = strip_vertices(n);
                if out.len() + want > cap {
                    stats.dropped_vertices += want as u32;
                } else {
                    self.build_strip(emitter, start, end, camera, out);
                    stats.vertices += want as u32;
                }
            }
            start = end;
        }
        stats
    }

    /// One strip over `points[start..end]`, head (newest, `end - 1`) first.
    fn build_strip(
        &self,
        emitter: &EmitterDef,
        start: usize,
        end: usize,
        camera: Vec3,
        out: &mut Vec<TrailVertex>,
    ) {
        let r = &emitter.render;
        let u = &emitter.update;
        let n = end - start;
        let point = |k: usize| &self.points[end - 1 - k];
        let additive = if r.blend == BlendMode::Additive { 1.0 } else { 0.0 };
        let tile = crate::atlas::tile_index(r.sprite);
        let textured = match r.ribbon.texture {
            RibbonTexture::None => 0.0,
            // The fireball sheet is a separate texture the trail pipeline does not bind.
            _ if tile >= crate::atlas::FIREBALL_ANIM_TILE => 0.0,
            _ => (tile + 1) as f32,
        };
        let mut previous_side = Vec3::Y;
        let mut distance = 0.0f32;
        let mut previous: Option<(TrailVertex, TrailVertex)> = None;
        for k in 0..n {
            let p = point(k);
            let t = k as f32 / (n - 1) as f32;
            if k > 0 {
                distance += point(k - 1).position.distance(p.position);
            }
            let tangent = point(k.saturating_sub(1)).position - point((k + 1).min(n - 1)).position;
            let side = tangent.cross(camera - p.position).normalize_or_zero();
            let side = if side == Vec3::ZERO { previous_side } else { side };
            previous_side = side;
            let life = (p.age / p.life.max(1e-6)).clamp(0.0, 1.0);
            let size = p.size * crate::curves::sample_scalar(&u.size_over_life, life, 1.0);
            let half = 0.5 * size * crate::curves::sample_scalar(&r.ribbon.width, t, 1.0).max(0.0);
            let tint = crate::curves::sample_color(&u.color_over_life, life);
            let along = crate::curves::sample_color(&r.ribbon.color, t);
            let alpha = p.color[3]
                * crate::curves::sample_scalar(&u.alpha_over_life, life, 1.0)
                * r.alpha_scale
                * along[3];
            let rgb = [0, 1, 2].map(|i| p.color[i] * tint[i] * along[i] * r.emissive);
            let u_coord = match r.ribbon.texture {
                RibbonTexture::Tile(length) => distance / length.max(1e-4),
                _ => t,
            };
            let vertex = |s: f32| TrailVertex {
                position: (p.position + side * (s * half)).to_array(),
                _pad: additive,
                uv: [u_coord, s],
                _pad2: [0.0, textured],
                color: [rgb[0], rgb[1], rgb[2], alpha.clamp(0.0, 1.0)],
            };
            let (left, right) = (vertex(-1.0), vertex(1.0));
            if let Some((a_l, a_r)) = previous {
                out.extend_from_slice(&[a_l, left, a_r, a_r, left, right]);
            }
            previous = Some((left, right));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vertex_formula() {
        assert_eq!(strip_vertices(0), 0);
        assert_eq!(strip_vertices(1), 0);
        assert_eq!(strip_vertices(2), 6);
        assert_eq!(strip_vertices(64), 378);
    }

    /// A spawn group keeps the first `max_points` jobs. An emitter strip drops the oldest.
    #[test]
    fn spawn_group_keeps_the_first_jobs_and_emitter_drops_the_oldest() {
        let text = r#"(
            version: 1,
            name: "ribbon",
            system: (budget: 64),
            emitters: [(
                name: "strip",
                spawn: (mode: Burst(6), lifetime: (min: 1.0, max: 1.0)),
                init: (shape: Point, size: (min: 0.1, max: 0.1)),
                render: (mode: Billboard),
            )],
        )"#;
        let mut def = crate::format::VfxSystemDef::from_ron(text).expect("ron");
        def.emitters[0].render.ribbon.max_points = 4;
        let job = |x: f32| SpawnJob {
            position: [x, 0.0, 0.0],
            velocity: [0.0; 3],
            lifetime: 1.0,
            size: 1.0,
            rotation: 0.0,
            spin: 0.0,
            color: [1.0; 4],
            phase: 0.0,
        };
        let jobs: Vec<SpawnJob> = (0..6).map(|i| job(i as f32)).collect();
        def.emitters[0].render.ribbon.link = RibbonLink::SpawnGroup;
        let mut group = RibbonTrack::default();
        group.push_jobs(&jobs, &def.emitters[0]);
        let xs: Vec<f32> = group.points().map(|point| point.position.x).collect();
        assert_eq!(xs, vec![0.0, 1.0, 2.0, 3.0]);
        group.push_jobs(&jobs, &def.emitters[0]);
        assert_eq!(
            group.len(),
            8,
            "later frames stay; the track is not one ring of max_points"
        );

        def.emitters[0].render.ribbon.link = RibbonLink::Emitter;
        let mut emitter = RibbonTrack::default();
        emitter.push_jobs(&jobs, &def.emitters[0]);
        let xs: Vec<f32> = emitter.points().map(|point| point.position.x).collect();
        assert_eq!(xs, vec![2.0, 3.0, 4.0, 5.0]);
    }

    /// `VortexDef::radial` adds to the built-in outward `0.25 × strength` (ADR-5629): inward when
    /// negative, and it acts on its own when `strength` is 0. Before the fix the CPU ribbons (and
    /// the GPU kernel) ignored the field.
    #[test]
    fn the_vortex_radial_term_moves_a_point_along_the_radius() {
        let velocity_after_one_step = |strength: f32, radial: f32| {
            let text = format!(
                r#"(
                version: 7,
                name: "ribbon",
                system: (budget: 64),
                emitters: [(
                    name: "strip",
                    spawn: (mode: Burst(1), lifetime: (min: 10.0, max: 10.0)),
                    init: (shape: Point, size: (min: 0.1, max: 0.1)),
                    update: (vortex: Some((axis: (0.0, 1.0, 0.0), strength: {strength:?}, radial: {radial:?}))),
                    render: (mode: Ribbon),
                )],
            )"#
            );
            let def = crate::format::VfxSystemDef::from_ron(&text).expect("ron");
            let job = SpawnJob {
                position: [2.0, 0.0, 0.0],
                velocity: [0.0; 3],
                lifetime: 10.0,
                size: 0.1,
                rotation: 0.0,
                spin: 0.0,
                color: [1.0; 4],
                phase: 0.0,
            };
            let mut track = RibbonTrack::default();
            track.push_jobs(&[job], &def.emitters[0]);
            let world = RibbonWorld {
                dt: 0.1,
                wind: Vec3::ZERO,
                origin: Vec3::ZERO,
                attractor: Vec3::ZERO,
                kill: ResolvedKillVolume::default(),
            };
            track.step(&def.emitters[0], world);
            track.points().next().expect("alive").velocity
        };
        // Radial alone (no swirl): 4 m/s² inward for 0.1 s.
        let v = velocity_after_one_step(0.0, -4.0);
        assert!((v.x + 0.4).abs() < 1e-4 && v.z.abs() < 1e-6, "{v}");
        // With swirl: the built-in outward 0.25 × 2 plus -4 gives -3.5 m/s² along +x.
        let v = velocity_after_one_step(2.0, -4.0);
        assert!((v.x + 0.35).abs() < 1e-4, "{v}");
        // radial 0 keeps the built-in outward push (shipped effects are unchanged).
        let v = velocity_after_one_step(2.0, 0.0);
        assert!((v.x - 0.05).abs() < 1e-4, "{v}");
    }
}
