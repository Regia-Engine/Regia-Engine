// SPDX-License-Identifier: MPL-2.0
//! The floating origin of a streamed world (spec 78 § 12): the [`WorldOrigin`] resource, the local
//! and world conversions at the edges, and the whole-cell rebase of everything the ECS holds.
//!
//! `Transform` and `GlobalTransform` stay `f32` local positions. A rebase shifts them at a step
//! boundary and writes [`OriginShifted`], which every subsystem holding positions outside the ECS
//! handles in the same step.

use regia_core::math::{Affine3A, Vec3, Vec3A};
use regia_ecs::prelude::*;
use regia_scene::floating_origin::{FloatingOrigin, LargeSpace, OriginError, Rebase};
use regia_scene::streaming::{
    OriginOffset, OriginSettings, OriginShifted, PrimaryStreamView, RebaseBlockers,
};
use regia_scene::{GlobalTransform, PreviousGlobalTransform, Transform};

use crate::runtime::{TraceKind, WorldStreaming};

/// The floating origin of this process. There is one per process (spec 78 § 12.3).
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct WorldOrigin(pub FloatingOrigin);

impl Default for WorldOrigin {
    fn default() -> Self {
        let settings = OriginSettings::default();
        Self::from_settings(&settings).unwrap_or_else(|_| {
            unreachable!("the default origin settings are valid");
        })
    }
}

impl WorldOrigin {
    /// An origin at cell zero for `settings`.
    pub fn from_settings(settings: &OriginSettings) -> Result<Self, OriginError> {
        Self::at_cell(settings, [0; 3])
    }

    /// An origin at `cell` for `settings`.
    pub fn at_cell(settings: &OriginSettings, cell: [i64; 3]) -> Result<Self, OriginError> {
        Ok(Self(FloatingOrigin::new(
            LargeSpace::new(settings.cell_size_m)?,
            cell,
            settings.threshold_m,
        )?))
    }

    /// The world position of a local point, `f64` meters.
    pub fn to_world(&self, local: [f32; 3]) -> [f64; 3] {
        let size = self.0.space().cell_size();
        let cell = self.0.cell();
        std::array::from_fn(|a| cell[a] as f64 * size + f64::from(local[a]))
    }

    /// The local position of a world point.
    pub fn from_world(&self, p: [f64; 3]) -> Result<[f32; 3], OriginError> {
        Ok(self.0.to_local(&self.0.space().from_world(p)?))
    }
}

/// Shifts one world matrix by `shift`.
fn shifted(matrix: Affine3A, shift: Vec3A) -> Affine3A {
    Affine3A {
        translation: matrix.translation - shift,
        ..matrix
    }
}

/// Root transforms: children are relative to their parents and do not change.
type Roots<'w, 's> = Query<'w, 's, &'static mut Transform, Without<ChildOf>>;

/// `StreamSet::Rebase`: moves the origin when the focus strayed (or when `stream.rebase` asked), and
/// shifts root, global and previous transforms, so motion vectors see no movement across the frame.
pub(crate) fn rebase_step(
    mut streaming: ResMut<WorldStreaming>,
    mut origin: ResMut<WorldOrigin>,
    mut roots: Roots,
    mut globals: Query<&mut GlobalTransform>,
    mut previous: Query<&mut PreviousGlobalTransform>,
    mut shifted_writer: MessageWriter<OriginShifted>,
    (primary, blockers): (Option<ResMut<PrimaryStreamView>>, Option<Res<RebaseBlockers>>),
) {
    if !streaming.is_active() {
        return;
    }
    let _span = tracing::trace_span!("stream_rebase").entered();
    let rebase = match streaming.take_rebase_request() {
        Some(cell) => Some(origin.0.rebase_to(cell)),
        None => streaming
            .focus()
            .filter(|_| streaming.origin_enabled())
            .and_then(|focus| {
                let large = origin.0.space().from_world(focus).ok()?;
                origin.0.update(&large).ok().flatten()
            }),
    };
    let Some(rebase) = rebase.filter(|r| r.cell_delta != [0; 3]) else {
        return;
    };
    // A store that cannot move yet keeps the origin where it is (ADR-2022). `update` already moved
    // the origin's cell, so it goes back.
    if let Some(names) = blockers.as_deref().filter(|b| !b.0.is_empty()) {
        let back = std::array::from_fn(|a| origin.0.cell()[a] - rebase.cell_delta[a]);
        origin.0.rebase_to(back);
        let stats = streaming.stats_mut();
        stats.rebases_blocked += 1;
        if stats.rebases_blocked == 1 {
            tracing::warn!(
                target: "regia_world",
                "stream_rebase_blocked: {:?} cannot follow a floating-origin rebase; the origin stays put",
                names.0
            );
        }
        return;
    }
    apply_rebase(&rebase, &mut roots, &mut globals, &mut previous);
    // The primary view is written once per frame and a frame can run several steps.
    if let Some(mut primary) = primary
        && let Some((position, _)) = &mut primary.view
    {
        rebase.apply(position);
    }
    shifted_writer.write(OriginShifted {
        rebase,
        origin_cell: origin.0.cell(),
    });
    streaming.stats_mut().rebases += 1;
    let step = streaming.stats().step;
    streaming.note_rebase(step, rebase.cell_delta);
    streaming.note(TraceKind::Rebase, Some(format!("{:?}", rebase.cell_delta)));
}

/// `StreamSet::Rebase` with no streamed level open: takes the origin back to cell zero through the
/// normal path, so a plain level opened after a streamed one sits where it was authored and at full
/// precision. Local space is the process's, so what stayed (persistent entities, the plain level,
/// converted as it opened) moves with it.
pub(crate) fn return_origin_home(
    mut streaming: ResMut<WorldStreaming>,
    mut origin: ResMut<WorldOrigin>,
    mut roots: Roots,
    mut globals: Query<&mut GlobalTransform>,
    mut previous: Query<&mut PreviousGlobalTransform>,
    mut shifted_writer: MessageWriter<OriginShifted>,
    blockers: Option<Res<RebaseBlockers>>,
) {
    if streaming.is_active() || origin.0.cell() == [0; 3] {
        return;
    }
    // A store that cannot move keeps the origin where it is, as in `rebase_step`; the trip home is
    // retried each step until the blocker goes.
    if let Some(names) = blockers.as_deref().filter(|b| !b.0.is_empty()) {
        let stats = streaming.stats_mut();
        stats.rebases_blocked += 1;
        if stats.rebases_blocked == 1 {
            tracing::warn!(
                target: "regia_world",
                "stream_rebase_blocked: {:?} cannot follow a floating-origin rebase; the origin stays put",
                names.0
            );
        }
        return;
    }
    let _span = tracing::trace_span!("stream_rebase").entered();
    let rebase = origin.0.rebase_to([0; 3]);
    apply_rebase(&rebase, &mut roots, &mut globals, &mut previous);
    shifted_writer.write(OriginShifted {
        rebase,
        origin_cell: origin.0.cell(),
    });
    streaming.stats_mut().rebases += 1;
    let step = streaming.stats().step;
    streaming.note_rebase(step, rebase.cell_delta);
    streaming.note(TraceKind::Rebase, Some(format!("{:?}", rebase.cell_delta)));
}

/// `StreamSet::Rebase`, after the rebase: publishes the origin as [`OriginOffset`] for crates that
/// cannot name the runtime. Writes only on a change.
pub(crate) fn publish_origin_offset(origin: Res<WorldOrigin>, offset: Option<ResMut<OriginOffset>>) {
    let now = OriginOffset(origin.to_world([0.0; 3]));
    if let Some(mut offset) = offset
        && *offset != now
    {
        *offset = now;
    }
}

fn apply_rebase(
    rebase: &Rebase,
    roots: &mut Roots,
    globals: &mut Query<&mut GlobalTransform>,
    previous: &mut Query<&mut PreviousGlobalTransform>,
) {
    let shift = Vec3::from_array(rebase.shift);
    let shift_a = Vec3A::from(shift);
    roots.par_iter_mut().for_each(|mut t| t.translation -= shift);
    globals
        .par_iter_mut()
        .for_each(|mut g| g.0 = shifted(g.0, shift_a));
    previous
        .par_iter_mut()
        .for_each(|mut p| p.0 = shifted(p.0, shift_a));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_and_world_round_trip_at_the_default_origin() {
        let origin = WorldOrigin::default();
        assert_eq!(origin.to_world([1.5, 2.0, -3.0]), [1.5, 2.0, -3.0]);
        assert_eq!(
            origin.from_world([1.5, 2.0, -3.0]).expect("local"),
            [1.5, 2.0, -3.0]
        );
    }

    #[test]
    fn a_moved_origin_keeps_world_positions_exact() {
        let mut origin = WorldOrigin::default();
        let rebase = origin.0.rebase_to([4, 0, -2]);
        assert_eq!(rebase.shift, [4096.0, 0.0, -2048.0]);
        assert_eq!(origin.to_world([1.0, 0.0, 1.0]), [4097.0, 0.0, -2047.0]);
        assert_eq!(
            origin.from_world([4097.0, 0.0, -2047.0]).expect("local"),
            [1.0, 0.0, 1.0]
        );
    }
}
