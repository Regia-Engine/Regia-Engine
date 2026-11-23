// SPDX-License-Identifier: MPL-2.0
//! Debug draw of the deterministic simulation indexes (ADR-8562).
//!
//! `regia_detsim` stays render-free: [`regia_detsim::broadphase::DetBroadphase`] and
//! [`regia_detsim::spatial::DetSpatial`] describe the grid, its occupancy, the last pass's candidate
//! pairs and contacts, and the last queries through a [`DebugSink`]. This module carries that to the
//! screen:
//!
//! - [`DetDebugDraw`] is the switch and the latest recorded frame. An ECS game calls
//!   [`DetDebugDraw::record`] with its indexes once per tick (it turns their capture on or off to
//!   match [`DetDebugDraw::what`]).
//! - A sim on [`crate::sim_host::ThreadedSimHost`] implements
//!   [`FixedStepSim::det_debug`](crate::sim_host::FixedStepSim::det_debug); the host records a frame
//!   after each step while drawing is on, and [`sync_det_debug_from_host`] copies it into
//!   [`DetDebugDraw`] each frame and passes the switch back to the host.
//! - With the `client` feature, [`DetGizmos`] (gizmo group `lockstep`) replays the frame into the
//!   gizmo buffer through [`GizmoDebugSink`].
//!
//! Capture and drawing read state only; nothing here is simulation state or part of a desync hash.
//! The agent method is `sim.det_debug_draw`; the editor draws the same frame (Show ▸ Lockstep).

use regia_app::prelude::*;
pub use regia_detsim::debug::{
    DebugCapture, DebugCounts, DebugKind, DebugPrim, DebugRecorder, DebugSink, DebugSource, DebugWhat,
};

use crate::sim_host::SimHostHandle;

/// The deterministic-index debug draw switch and the latest frame (ADR-8562).
#[derive(Resource, Debug, Clone, Default)]
pub struct DetDebugDraw {
    /// What to draw. Empty draws nothing and turns capture off.
    pub what: DebugWhat,
    /// The latest recorded primitives.
    pub frame: DebugRecorder,
    /// The tick the frame was recorded at, `None` before the first frame.
    pub tick: Option<u64>,
}

impl DetDebugDraw {
    /// A switch that draws `what`.
    pub fn new(what: DebugWhat) -> Self {
        Self {
            what,
            ..Self::default()
        }
    }

    /// The capture an index needs for the current switch: the default caps while anything is drawn,
    /// `None` (capture off) otherwise.
    pub fn capture(&self) -> Option<DebugCapture> {
        (!self.what.is_empty()).then(DebugCapture::default)
    }

    /// Match each source's capture to the switch and record their drawing for `tick`.
    ///
    /// Call it once per tick after the passes and queries to show. Capture is turned on the first
    /// time the switch is on, so contacts and queries appear from the next tick. With the switch
    /// off it turns capture off and clears the frame.
    pub fn record(&mut self, tick: u64, sources: &mut [&mut dyn DebugSource]) {
        let capture = self.capture();
        for source in sources.iter_mut() {
            source.set_debug_capture(capture);
        }
        self.frame.clear();
        if self.what.is_empty() {
            self.tick = None;
            return;
        }
        for source in sources.iter() {
            source.debug_emit(&mut self.frame, self.what);
        }
        self.tick = Some(tick);
    }

    /// Primitives of the frame by role, in [`DebugKind::ALL`] order.
    pub fn kind_counts(&self) -> [(DebugKind, usize); 7] {
        DebugKind::ALL.map(|kind| (kind, self.frame.count_kind(kind)))
    }
}

/// Copies the sim host's det debug frame into [`DetDebugDraw`] and passes the switch to the host.
/// Runs only when both resources exist.
pub fn sync_det_debug_from_host(handle: Option<Res<SimHostHandle>>, draw: Option<ResMut<DetDebugDraw>>) {
    let (Some(handle), Some(mut draw)) = (handle, draw) else {
        return;
    };
    if !handle.det_debug_supported() {
        return;
    }
    if handle.det_debug() != draw.what {
        handle.set_det_debug(draw.what);
    }
    match handle.det_debug_frame() {
        Some((tick, frame)) if draw.tick != Some(tick) => {
            draw.frame = frame;
            draw.tick = Some(tick);
        }
        None if draw.tick.is_some() => {
            draw.frame.clear();
            draw.tick = None;
        }
        _ => {}
    }
}

/// Registers [`sync_det_debug_from_host`] and, with the `client` feature, the `lockstep` gizmo
/// group and its draw system. [`crate::debug_gizmos::DebugGizmosPlugin`] adds it.
#[derive(Default)]
pub struct DetDebugDrawPlugin;

impl Plugin for DetDebugDrawPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PreUpdate, sync_det_debug_from_host);
        #[cfg(feature = "client")]
        client::build(app);
    }
}

#[cfg(feature = "client")]
pub use client::{DetDebugColors, DetGizmos, GizmoDebugSink, draw_det_debug};

#[cfg(feature = "client")]
mod client {
    use std::f32::consts::FRAC_PI_2;

    use regia_app::prelude::*;
    use regia_core::math::{Color, Isometry, Quat, Vec2, Vec3};
    use regia_render::gizmos::{
        AppGizmoBuilder, GizmoBuffer, GizmoConfig, GizmoConfigGroup, Gizmos, gizmo_group_enabled,
    };

    use super::{DebugKind, DebugSink, DetDebugDraw};

    /// Gizmo group `lockstep`: the deterministic-index debug draw. On by default; nothing is drawn
    /// until [`DetDebugDraw::what`] is non-empty. Hide it with `debug.set_gizmos`.
    #[derive(Default, Debug, Clone, Copy)]
    pub struct DetGizmos;

    impl GizmoConfigGroup for DetGizmos {
        const NAME: &'static str = "lockstep";
        fn default_config() -> GizmoConfig {
            GizmoConfig::default()
        }
    }

    /// Colors per [`DebugKind`].
    #[derive(Resource, Debug, Clone, Copy)]
    pub struct DetDebugColors {
        /// Grid bounds and cells in use.
        pub grid: Color,
        /// Cell counts.
        pub occupancy: Color,
        /// Candidate pairs.
        pub candidate: Color,
        /// Contacts.
        pub contact: Color,
        /// Query shapes.
        pub query: Color,
        /// Query hits.
        pub query_hit: Color,
        /// Flow field arrows (ADR-8839).
        pub flow: Color,
    }

    impl Default for DetDebugColors {
        fn default() -> Self {
            Self {
                grid: Color::rgb(0.35, 0.55, 0.75),
                occupancy: Color::rgb(0.85, 0.9, 1.0),
                candidate: Color::rgb(0.55, 0.55, 0.55),
                contact: Color::rgb(1.0, 0.25, 0.2),
                query: Color::rgb(0.3, 0.9, 1.0),
                query_hit: Color::rgb(1.0, 0.85, 0.2),
                flow: Color::rgb(0.45, 1.0, 0.45),
            }
        }
    }

    impl DetDebugColors {
        /// The color of one role.
        pub fn of(&self, kind: DebugKind) -> Color {
            match kind {
                DebugKind::Grid => self.grid,
                DebugKind::Occupancy => self.occupancy,
                DebugKind::Candidate => self.candidate,
                DebugKind::Contact => self.contact,
                DebugKind::Query => self.query,
                DebugKind::QueryHit => self.query_hit,
                DebugKind::Flow => self.flow,
            }
        }
    }

    /// A [`DebugSink`] that draws into a gizmo buffer: circles on XZ, boxes as edges, text as
    /// stroke text facing +Z.
    pub struct GizmoDebugSink<'a, G: GizmoConfigGroup> {
        /// The buffer drawn into.
        pub buffer: &'a mut GizmoBuffer<G>,
        /// Colors per role.
        pub colors: DetDebugColors,
        /// Text cap height, world units.
        pub text_size: f32,
    }

    impl<G: GizmoConfigGroup> DebugSink for GizmoDebugSink<'_, G> {
        fn line(&mut self, kind: DebugKind, a: [f32; 3], b: [f32; 3]) {
            self.buffer
                .line(Vec3::from_array(a), Vec3::from_array(b), self.colors.of(kind));
        }
        fn circle(&mut self, kind: DebugKind, center: [f32; 3], radius: f32) {
            let pose = Isometry::new(Quat::from_rotation_x(FRAC_PI_2), Vec3::from_array(center));
            self.buffer.circle(pose, radius, self.colors.of(kind));
        }
        fn aabb(&mut self, kind: DebugKind, min: [f32; 3], max: [f32; 3]) {
            let color = self.colors.of(kind);
            let ys: &[f32] = if min[1] == max[1] {
                &[min[1]]
            } else {
                &[min[1], max[1]]
            };
            for &y in ys {
                let c = [
                    Vec3::new(min[0], y, min[2]),
                    Vec3::new(max[0], y, min[2]),
                    Vec3::new(max[0], y, max[2]),
                    Vec3::new(min[0], y, max[2]),
                ];
                for (i, &corner) in c.iter().enumerate() {
                    self.buffer.line(corner, c[(i + 1) % 4], color);
                }
            }
            if ys.len() == 2 {
                for (x, z) in [
                    (min[0], min[2]),
                    (max[0], min[2]),
                    (max[0], max[2]),
                    (min[0], max[2]),
                ] {
                    self.buffer
                        .line(Vec3::new(x, min[1], z), Vec3::new(x, max[1], z), color);
                }
            }
        }
        fn text(&mut self, kind: DebugKind, at: [f32; 3], text: &str) {
            self.buffer.text(
                Vec3::from_array(at),
                text,
                self.text_size,
                Vec2::ZERO,
                self.colors.of(kind),
            );
        }
    }

    /// Replays [`DetDebugDraw::frame`] into the `lockstep` gizmo group.
    pub fn draw_det_debug(
        draw: Res<DetDebugDraw>,
        colors: Option<Res<DetDebugColors>>,
        mut gizmos: Gizmos<DetGizmos>,
    ) {
        if draw.what.is_empty() || draw.frame.is_empty() {
            return;
        }
        let _span = tracing::trace_span!("det_debug_draw").entered();
        let mut sink = GizmoDebugSink {
            buffer: &mut gizmos,
            colors: colors.as_deref().copied().unwrap_or_default(),
            text_size: 0.4,
        };
        draw.frame.replay(&mut sink);
    }

    pub(super) fn build(app: &mut App) {
        app.init_gizmo_group::<DetGizmos>();
        app.add_systems(
            PostUpdate,
            draw_det_debug
                .run_if(resource_exists::<DetDebugDraw>)
                .run_if(gizmo_group_enabled::<DetGizmos>),
        );
    }
}

#[cfg(test)]
mod tests {
    use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, DetBroadphase};

    use super::*;

    #[test]
    fn record_turns_capture_on_and_off_with_the_switch() {
        let mut grid = DetBroadphase::new(BroadphaseConfig::default());
        let mut bodies = vec![
            BroadBody::new([0.0, 0.0, 0.0], 1.0, [-1.0, 2.0]),
            BroadBody::new([1.5, 0.0, 0.0], 1.0, [-1.0, 2.0]),
        ];
        let mut draw = DetDebugDraw::new(DebugWhat::CONTACTS);
        draw.record(0, &mut [&mut grid]);
        assert!(grid.debug_capture().is_some(), "the switch turns capture on");
        grid.for_each_contact(&mut bodies, |_, _| {});
        draw.record(1, &mut [&mut grid]);
        assert_eq!(draw.frame.count_kind(DebugKind::Contact), 1);
        assert_eq!(draw.tick, Some(1));
        draw.what = DebugWhat::NONE;
        draw.record(2, &mut [&mut grid]);
        assert!(
            grid.debug_capture().is_none(),
            "an empty switch turns capture off"
        );
        assert!(draw.frame.is_empty() && draw.tick.is_none());
    }

    /// The gizmo adapter draws lines as one segment and flat boxes as four edges.
    #[cfg(feature = "client")]
    #[test]
    fn gizmo_sink_draws_lines_and_flat_boxes_as_segments() {
        use regia_render::gizmos::GizmoBuffer;
        let mut frame = DebugRecorder::default();
        frame.line(DebugKind::Contact, [0.0; 3], [1.0, 0.0, 0.0]);
        frame.aabb(DebugKind::Grid, [0.0; 3], [4.0, 0.0, 4.0]);
        frame.aabb(DebugKind::Grid, [0.0; 3], [1.0, 1.0, 1.0]);
        let mut buffer = GizmoBuffer::<DetGizmos>::new();
        frame.replay(&mut GizmoDebugSink {
            buffer: &mut buffer,
            colors: DetDebugColors::default(),
            text_size: 0.4,
        });
        // 1 line + 4 edges + 12 edges, two points each.
        assert_eq!(buffer.list_positions.len(), 2 * (1 + 4 + 12));
        assert_eq!(buffer.list_colors[0], DetDebugColors::default().contact);
    }
}
