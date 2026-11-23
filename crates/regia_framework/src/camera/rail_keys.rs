// SPDX-License-Identifier: MPL-2.0
//! Sequencer properties for the rail camera and the crane (ADR-3231): a sequence keys a dolly
//! move by writing the camera driver's runtime variables.
//!
//! The appliers write a [`CameraDriver`] variable on the bound entity, so a rig reads the key with
//! `Var("rail_position")` in its [`Node::Rail`](super::asset::Node::Rail) or
//! [`Node::Crane`](super::asset::Node::Crane) nodes. The asset declares the variable (a `Float`);
//! an undeclared one makes the track report an apply error instead of writing nothing silently.
//!
//! | Property path | Variable | Unit |
//! |---|---|---|
//! | `camera.rail.position` | `rail_position` | meters along the rail |
//! | `camera.crane.yaw` | `crane_yaw` | degrees |
//! | `camera.crane.pitch` | `crane_pitch` | degrees |
//! | `camera.crane.length` | `crane_length` | meters |
//! | `camera.crane.mount_yaw` | `crane_mount_yaw` | degrees |
//! | `camera.crane.mount_pitch` | `crane_mount_pitch` | degrees |

use regia_app::prelude::*;

use super::asset::VarValue;
use super::driver::CameraDriver;
use crate::sequencer::player::{ApplyError, PropertyApplier, PropertyAppliers};
use crate::sequencer::sample::ChannelValue;

/// The property paths this file registers, with the driver variable each writes.
pub const KEYED: [(&str, &str); 6] = [
    ("camera.rail.position", "rail_position"),
    ("camera.crane.yaw", "crane_yaw"),
    ("camera.crane.pitch", "crane_pitch"),
    ("camera.crane.length", "crane_length"),
    ("camera.crane.mount_yaw", "crane_mount_yaw"),
    ("camera.crane.mount_pitch", "crane_mount_pitch"),
];

fn write_var(entity: &mut EntityWorldMut<'_>, value: ChannelValue, name: &str) -> Result<(), ApplyError> {
    let ChannelValue::Float(v) = value else {
        return Err(ApplyError::WrongKind { expected: "Float" });
    };
    let mut driver = entity
        .get_mut::<CameraDriver>()
        .ok_or(ApplyError::MissingComponent("CameraDriver"))?;
    let core = driver
        .core_mut()
        .ok_or(ApplyError::MissingComponent("a bound camera asset"))?;
    core.set_var(name, VarValue::Float(v))
        .map_err(|_| ApplyError::MissingComponent("the declared Float camera variable"))
}

macro_rules! applier {
    ($name:ident, $var:literal) => {
        fn $name(entity: &mut EntityWorldMut<'_>, value: ChannelValue) -> Result<(), ApplyError> {
            write_var(entity, value, $var)
        }
    };
}

applier!(apply_rail_position, "rail_position");
applier!(apply_crane_yaw, "crane_yaw");
applier!(apply_crane_pitch, "crane_pitch");
applier!(apply_crane_length, "crane_length");
applier!(apply_crane_mount_yaw, "crane_mount_yaw");
applier!(apply_crane_mount_pitch, "crane_mount_pitch");

/// Registers the six properties in `appliers`.
pub fn register(appliers: &mut PropertyAppliers) {
    let table: [(&str, PropertyApplier); 6] = [
        (KEYED[0].0, apply_rail_position),
        (KEYED[1].0, apply_crane_yaw),
        (KEYED[2].0, apply_crane_pitch),
        (KEYED[3].0, apply_crane_length),
        (KEYED[4].0, apply_crane_mount_yaw),
        (KEYED[5].0, apply_crane_mount_pitch),
    ];
    for (path, applier) in table {
        appliers.register(path, applier);
    }
}

/// Adds the properties to the app's [`PropertyAppliers`] (created if absent; the sequencer's
/// own install keeps it when it runs later).
pub fn install(app: &mut App) {
    app.init_resource::<PropertyAppliers>();
    register(&mut app.world_mut().resource_mut::<PropertyAppliers>());
}
