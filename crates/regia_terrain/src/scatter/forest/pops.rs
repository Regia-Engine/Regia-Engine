// SPDX-License-Identifier: MPL-2.0
//! A pop meter over the emitted scatter draws (ADR-1503): counts the discontinuities a viewer
//! can see between two frames, by object key.
//!
//! - **Hard switch**: an object drawn whole on both frames changes mesh (LOD) on screen. A
//!   switch that begins a cross-fade (the new frame's draw is dithered) is soft.
//! - **Pop-in**: an object drawn whole appears on screen. Instances that grow in from a distance
//!   fade, or dither in, start too small or dithered to count.
//! - **Vanish**: an object drawn whole on the last frame is gone while its sphere is still on
//!   screen (an impostor hand-off dithers out first, a leaving instance leaves the frustum).
//!
//! Only events whose projected radius is at least [`PopMeter::threshold_px`] count, at the
//! meter's output height, and only while the draw's bounding-sphere centre is inside the view
//! frustum ("on screen"; the same centre the scatter field decides its cross-fades on, so a
//! switch the field makes at once because the instance is off screen is never counted). The
//! first frame observed sets the baseline (no events).

use std::collections::HashMap;

use regia_core::glam::{Mat4, Vec3};
use regia_render::{ExtractedMesh, FrameMesh, Frustum, LabVertex};

use super::super::field::ScatterView;

/// Kind of a [`PopEvent`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PopKind {
    /// A whole-to-whole mesh change on screen.
    HardSwitch,
    /// A whole draw appearing on screen.
    PopIn,
    /// A whole draw disappearing while on screen.
    Vanish,
}

/// One visible discontinuity.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct PopEvent {
    /// Frame (meter frames, from 1).
    pub frame: u64,
    /// What happened.
    pub kind: PopKind,
    /// Object key of the draw.
    pub object_key: u64,
    /// Projected radius, pixels.
    pub pixels: f32,
    /// Distance from the eye, metres.
    pub distance_m: f32,
}

/// Counters of a [`PopMeter`] (one frame, or the run so far).
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct PopStats {
    /// Frames observed.
    pub frames: u64,
    /// Draws observed.
    pub draws: u64,
    /// Hard LOD switches on screen above the threshold.
    pub hard_switches: u32,
    /// LOD switches that began a cross-fade.
    pub soft_switches: u32,
    /// Whole draws appearing on screen above the threshold.
    pub pop_ins: u32,
    /// Draws appearing dithered or below the threshold.
    pub fade_ins: u32,
    /// Whole draws vanishing on screen above the threshold.
    pub vanishes: u32,
    /// Largest projected radius of any event, pixels.
    pub worst_px: f32,
    /// Largest projected radius at which any LOD switch (soft or hard) began on screen, pixels.
    pub max_switch_px: f32,
}

impl PopStats {
    /// Hard switches, pop-ins and vanishes.
    #[must_use]
    pub fn pops(&self) -> u32 {
        self.hard_switches + self.pop_ins + self.vanishes
    }

    fn add(&mut self, o: &Self) {
        self.frames += o.frames;
        self.draws += o.draws;
        self.hard_switches += o.hard_switches;
        self.soft_switches += o.soft_switches;
        self.pop_ins += o.pop_ins;
        self.fade_ins += o.fade_ins;
        self.vanishes += o.vanishes;
        self.worst_px = self.worst_px.max(o.worst_px);
        self.max_switch_px = self.max_switch_px.max(o.max_switch_px);
    }
}

#[derive(Clone, Copy)]
struct DrawState {
    mesh_key: u64,
    dithered: bool,
    centre: Vec3,
    radius: f32,
}

/// One draw as the meter reads it: object key, mesh key, dithered, model, vertices.
type DrawItem<'v> = (u64, u64, bool, [[f32; 4]; 4], &'v [LabVertex]);

/// Most events a meter keeps (the counters keep counting).
const MAX_EVENTS: usize = 512;
/// Most switches the switch log keeps.
const MAX_SWITCHES: usize = 200_000;

/// One LOD switch seen on screen ([`PopMeter::record_switches`]).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct SwitchRecord {
    /// Meter frame.
    pub frame: u64,
    /// Distance from the eye to the draw's bounding-sphere centre, metres.
    pub distance_m: f32,
    /// Bounding radius of the draw, metres.
    pub radius_m: f32,
    /// Projected radius, pixels.
    pub pixels: f32,
    /// A whole-to-whole switch (no cross-fade).
    pub hard: bool,
}

/// The pop meter (see the module docs).
#[derive(Clone, Default)]
pub struct PopMeter {
    /// Smallest projected radius (pixels) an event counts at.
    pub threshold_px: f32,
    /// Output height the pixels are measured at.
    pub height_px: f32,
    prev: HashMap<u64, DrawState>,
    bounds: HashMap<u64, (Vec3, f32)>,
    events: Vec<PopEvent>,
    total: PopStats,
    /// Keep a [`SwitchRecord`] of every on-screen LOD switch (off by default).
    pub record_switches: bool,
    switches: Vec<SwitchRecord>,
}

fn mesh_bounds(vertices: &[LabVertex]) -> (Vec3, f32) {
    let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
    for v in vertices {
        let p = Vec3::from_array(v.position);
        lo = lo.min(p);
        hi = hi.max(p);
    }
    if vertices.is_empty() {
        return (Vec3::ZERO, 0.0);
    }
    ((lo + hi) * 0.5, (hi - lo).length() * 0.5)
}

impl PopMeter {
    /// A meter counting events of at least `threshold_px` pixels at an output `height_px` tall.
    #[must_use]
    pub fn new(threshold_px: f32, height_px: f32) -> Self {
        Self {
            threshold_px: threshold_px.max(0.0),
            height_px: height_px.max(1.0),
            ..Self::default()
        }
    }

    /// The run's counters so far.
    #[must_use]
    pub fn total(&self) -> PopStats {
        self.total
    }

    /// The events kept (the first `MAX_EVENTS`).
    #[must_use]
    pub fn events(&self) -> &[PopEvent] {
        &self.events
    }

    /// The on-screen LOD switches logged while [`Self::record_switches`] was on.
    #[must_use]
    pub fn switches(&self) -> &[SwitchRecord] {
        &self.switches
    }

    /// Observes one frame's draws (as emitted by `ScatterField::collect` for `view`) and returns
    /// this frame's counters.
    pub fn observe(&mut self, view: &ScatterView, draws: &[FrameMesh<'_>]) -> PopStats {
        let items = draws
            .iter()
            .filter(|d| d.mesh_key >> 56 != regia_render::SHADOW_LOD_MESH_TAG >> 56)
            .map(|d| (d.object_key, d.mesh_key, d.lod_dither != 0.0, d.model, d.vertices));
        self.observe_items(view, items)
    }

    /// [`Self::observe`] for the ECS frame's extracted meshes.
    pub fn observe_extracted(&mut self, view: &ScatterView, draws: &[ExtractedMesh]) -> PopStats {
        let items = draws
            .iter()
            .filter(|d| d.lod_key != regia_render::SHADOW_LOD_MESH_TAG)
            .map(|d| {
                (
                    d.object_key_xor ^ d.entity.to_bits(),
                    std::sync::Arc::as_ptr(&d.mesh) as u64 ^ d.lod_key,
                    d.lod_dither != 0.0,
                    d.model,
                    d.mesh.vertices.as_slice(),
                )
            });
        self.observe_items(view, items)
    }

    fn observe_items<'v>(
        &mut self,
        view: &ScatterView,
        items: impl Iterator<Item = DrawItem<'v>>,
    ) -> PopStats {
        let _span = tracing::info_span!(target: "regia_terrain", "forest_pop_meter").entered();
        let frustum = Frustum::from_view_proj(&Mat4::from_cols_array_2d(&view.view_proj));
        let eye = Vec3::from_array(view.eye);
        let px_scale = self.height_px * 0.5 / (view.fov_y * 0.5).tan().max(1e-4);
        let first = self.total.frames == 0;
        let mut stats = PopStats {
            frames: 1,
            ..PopStats::default()
        };
        let frame = self.total.frames + 1;
        let mut current: HashMap<u64, DrawState> = HashMap::with_capacity(self.prev.len());
        let mut events = Vec::new();
        for (key, mesh_key, dithered, model, vertices) in items {
            stats.draws += 1;
            let (c, r) = *self
                .bounds
                .entry(mesh_key)
                .or_insert_with(|| mesh_bounds(vertices));
            let m = Mat4::from_cols_array_2d(&model);
            let scale = m
                .x_axis
                .truncate()
                .length()
                .max(m.y_axis.truncate().length())
                .max(m.z_axis.truncate().length());
            let state = DrawState {
                mesh_key,
                dithered,
                centre: m.transform_point3(c),
                radius: r * scale,
            };
            current.insert(key, state);
            if first {
                continue;
            }
            let d = state.centre.distance(eye).max(1e-3);
            let px = state.radius * px_scale / d;
            let on_screen = frustum.intersects_sphere(state.centre.to_array(), 0.0);
            let visible = on_screen && px >= self.threshold_px;
            match self.prev.get(&key) {
                Some(prev) if prev.mesh_key != mesh_key => {
                    let hard = !(dithered || prev.dithered);
                    if on_screen {
                        stats.max_switch_px = stats.max_switch_px.max(px);
                        if self.record_switches && self.switches.len() < MAX_SWITCHES {
                            self.switches.push(SwitchRecord {
                                frame,
                                distance_m: d,
                                radius_m: state.radius,
                                pixels: px,
                                hard,
                            });
                        }
                    }
                    if !hard {
                        stats.soft_switches += 1;
                    } else if visible {
                        stats.hard_switches += 1;
                        events.push((PopKind::HardSwitch, key, px, d));
                    }
                }
                Some(_) => {}
                None => {
                    if !dithered && visible {
                        stats.pop_ins += 1;
                        events.push((PopKind::PopIn, key, px, d));
                    } else {
                        stats.fade_ins += 1;
                    }
                }
            }
        }
        if !first {
            let mut gone: Vec<u64> = self
                .prev
                .keys()
                .filter(|k| !current.contains_key(k))
                .copied()
                .collect();
            gone.sort_unstable();
            for key in gone {
                let Some(prev) = self.prev.get(&key) else { continue };
                if prev.dithered {
                    continue;
                }
                let d = prev.centre.distance(eye).max(1e-3);
                let px = prev.radius * px_scale / d;
                if frustum.intersects_sphere(prev.centre.to_array(), 0.0) && px >= self.threshold_px {
                    stats.vanishes += 1;
                    events.push((PopKind::Vanish, key, px, d));
                }
            }
        }
        for (kind, object_key, pixels, distance_m) in events {
            stats.worst_px = stats.worst_px.max(pixels);
            if self.events.len() < MAX_EVENTS {
                self.events.push(PopEvent {
                    frame,
                    kind,
                    object_key,
                    pixels,
                    distance_m,
                });
            }
        }
        self.prev = current;
        self.total.add(&stats);
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{LabMaterial, ShadowCast};

    fn draw<'a>(
        key: u64,
        mesh_key: u64,
        vertices: &'a [LabVertex],
        material: &'a LabMaterial,
        at: [f32; 3],
        dither: f32,
    ) -> FrameMesh<'a> {
        FrameMesh {
            is_static: false,
            mesh_key,
            material_key: 1,
            object_key: key,
            vertices,
            indices: &[0, 1, 2],
            material,
            model: Mat4::from_translation(Vec3::from_array(at)).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::Off,
            uv1: &[],
            lod_dither: dither,
        }
    }

    #[test]
    fn counts_hard_switches_pop_ins_and_vanishes_on_screen_only() {
        let vertices = vec![
            LabVertex::from_pos_normal_uv([-1.0, 0.0, -1.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([1.0, 4.0, 1.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([1.0, 0.0, -1.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
        ];
        let material = LabMaterial::default();
        let view = ScatterView::look_at([0.0, 2.0, 0.0], [0.0, 2.0, -10.0], 1.0, 16.0 / 9.0, 0.1, 1000.0);
        let mut meter = PopMeter::new(2.0, 1080.0);
        let ahead = [0.0, 0.0, -20.0];
        let behind = [0.0, 0.0, 20.0];
        // Baseline.
        let f1 = [
            draw(1, 10, &vertices, &material, ahead, 0.0),
            draw(2, 10, &vertices, &material, behind, 0.0),
            draw(3, 10, &vertices, &material, [3.0, 0.0, -30.0], 0.0),
        ];
        assert_eq!(meter.observe(&view, &f1).pops(), 0);
        // Object 1 switches mesh whole on screen (hard); 2 switches behind the camera (not seen);
        // 3 begins a cross-fade (soft); 4 pops in ahead; 5 appears dithered.
        let f2 = [
            draw(1, 11, &vertices, &material, ahead, 0.0),
            draw(2, 11, &vertices, &material, behind, 0.0),
            draw(3, 11, &vertices, &material, [3.0, 0.0, -30.0], 0.3),
            draw(4, 10, &vertices, &material, [-2.0, 0.0, -25.0], 0.0),
            draw(5, 10, &vertices, &material, [2.0, 0.0, -25.0], -0.5),
        ];
        let s = meter.observe(&view, &f2);
        assert_eq!(
            (s.hard_switches, s.soft_switches, s.pop_ins, s.fade_ins),
            (1, 1, 1, 1)
        );
        // Object 4 vanishes on screen; 5 (dithered) and 2 (behind) leave quietly.
        let f3 = [
            draw(1, 11, &vertices, &material, ahead, 0.0),
            draw(3, 11, &vertices, &material, [3.0, 0.0, -30.0], 0.0),
        ];
        let s = meter.observe(&view, &f3);
        assert_eq!((s.vanishes, s.hard_switches, s.pop_ins), (1, 0, 0));
        assert_eq!(meter.total().pops(), 3);
        assert_eq!(meter.events().len(), 3);
        assert!(meter.events().iter().all(|e| e.pixels >= 2.0));
    }
}
