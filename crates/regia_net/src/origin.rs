// SPDX-License-Identifier: MPL-2.0
//! Replication and the floating origin (`docs/specs/78-world-streaming.md` §§ 12.2, 14, ADR-2022).
//!
//! Each peer has its own origin. Root positions cross the wire in the shared world frame: the
//! sender adds its [`OriginOffset`] to a root's local position, the receiver subtracts its own.
//! Parent-relative positions (replicated children) cross unchanged. With no streamed level the offset
//! is zero and nothing is converted, so the wire bytes are the same as before. The wire keeps `f32`,
//! about 1 mm inside ± 8 km of the world origin (spec 78 § 14).
//!
//! A local rebase moves what replication keeps in the local frame: the interpolation buffers of
//! root proxies, the predicted `Transform` history of root entities and the server's lag
//! compensation history.

use glam::Vec3;
use regia_ecs::prelude::*;
use regia_scene::streaming::{OriginOffset, OriginShifted};

use crate::interp::NetSnapshotBuffer;
use crate::lagcomp::LagCompHistory;
use crate::predict::ClientPrediction;

/// The origin offset to convert with, or `None` when it is zero (nothing converts).
pub(crate) fn wire_offset(world: &World) -> Option<[f64; 3]> {
    world
        .get_resource::<OriginOffset>()
        .map(|o| o.0)
        .filter(|o| *o != [0.0; 3])
}

/// A local position in the shared world frame of the wire.
pub fn local_to_wire(local: Vec3, offset: [f64; 3]) -> Vec3 {
    let [x, y, z] = local.to_array();
    Vec3::new(
        (f64::from(x) + offset[0]) as f32,
        (f64::from(y) + offset[1]) as f32,
        (f64::from(z) + offset[2]) as f32,
    )
}

/// A wire position in this peer's local frame.
pub fn wire_to_local(wire: Vec3, offset: [f64; 3]) -> Vec3 {
    let [x, y, z] = wire.to_array();
    Vec3::new(
        (f64::from(x) - offset[0]) as f32,
        (f64::from(y) - offset[1]) as f32,
        (f64::from(z) - offset[2]) as f32,
    )
}

/// The stores a rebase moves.
type NetOriginStores<'w> = (
    Option<ResMut<'w, ClientPrediction>>,
    Option<ResMut<'w, LagCompHistory>>,
);

/// `FixedUpdate`, after `StreamSet::Rebase`: moves the local-frame stores by `-shift`.
pub(crate) fn shift_net_origin(
    mut shifted: MessageReader<OriginShifted>,
    mut buffers: Query<&mut NetSnapshotBuffer, Without<ChildOf>>,
    children: Query<(), With<ChildOf>>,
    (mut prediction, mut lagcomp): NetOriginStores,
) {
    for message in shifted.read() {
        let _span = tracing::info_span!("net_shift_origin").entered();
        let delta = -Vec3::from_array(message.rebase.shift);
        for mut buffer in &mut buffers {
            buffer.0.shift_origin(delta);
        }
        if let Some(prediction) = prediction.as_deref_mut() {
            prediction.shift_origin(delta, &|e| !children.contains(e));
        }
        if let Some(lagcomp) = lagcomp.as_deref_mut() {
            lagcomp.shift_origin(delta);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_crosses_the_wire_in_the_world_frame() {
        let offset = [4096.0, 0.0, -3072.0];
        let local = Vec3::new(12.5, 3.0, -7.25);
        let wire = local_to_wire(local, offset);
        assert_eq!(wire, Vec3::new(4108.5, 3.0, -3079.25));
        // Another peer with its origin elsewhere sees the same world point.
        let other = [1024.0, 0.0, 0.0];
        assert_eq!(wire_to_local(wire, other), Vec3::new(3084.5, 3.0, -3079.25));
        assert_eq!(wire_to_local(wire, offset), local);
    }

    #[test]
    fn no_streamed_level_converts_nothing() {
        let mut world = World::new();
        assert_eq!(wire_offset(&world), None);
        world.insert_resource(OriginOffset([0.0; 3]));
        assert_eq!(wire_offset(&world), None);
        world.insert_resource(OriginOffset([1024.0, 0.0, 0.0]));
        assert_eq!(wire_offset(&world), Some([1024.0, 0.0, 0.0]));
    }
}
