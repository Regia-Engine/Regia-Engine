// SPDX-License-Identifier: MPL-2.0
//! Legacy-parity presets (`docs/specs/35-camera-framework.md` § 10): camera assets equivalent to
//! the four [`CameraMode`]s. They are code-built values, also shipped as
//! `assets/camera/presets/*.camera.ron`, and are the parity oracle for the driver tests.
//!
//! Every preset declares one `Axis2` input slot, `look`, whose accumulated yaw and pitch play the
//! legacy rig's `yaw` and `pitch`; the free preset adds a `move` slot (strafe, forward) and the
//! variables `rise` and `sprint`.

use std::collections::BTreeMap;

use super::asset::{
    CAMERA_FORMAT_VERSION, CamSpace, CameraAsset, CollisionAnchor, DirectorDef, InputSlotDef, Node,
    PivotSpace, RigDef, SlotKind, TargetRef, Value, VarDef, VarValue,
};
use super::nodes::SNAP_SPEED;
use super::{CameraMode, CameraRig, FirstPerson, Free, Overhead, ThirdPerson};

fn look_slot(pitch_clamp_deg: (f32, f32)) -> InputSlotDef {
    InputSlotDef {
        name: "look".into(),
        kind: SlotKind::Axis2,
        speed: (120.0, 90.0),
        pitch_clamp: pitch_clamp_deg,
        invert_pitch: false,
        normalize: false,
        pull_toward_var: None,
        pull_weight: 0.1,
    }
}

fn single_rig(name: &str, inputs: Vec<InputSlotDef>, variables: Vec<VarDef>, root: Vec<Node>) -> CameraAsset {
    CameraAsset {
        format_version: CAMERA_FORMAT_VERSION,
        name: name.into(),
        params: BTreeMap::new(),
        variables,
        inputs,
        rigs: vec![RigDef {
            name: "main".into(),
            tags: vec![],
            root,
            transitions: vec![],
            blend_out: None,
        }],
        transitions: vec![],
        director: DirectorDef::Fixed("main".into()),
    }
}

/// The asset equivalent to [`CameraMode::ThirdPerson`] with `mode`'s parameters: pivot, look,
/// boom, collision probe, position and rotation lag, lean. A `lag_speed` or `rotation_lag` of 0
/// (a frozen camera in the legacy rig) maps to no smoothing, not a frozen camera.
pub fn third_person_from(mode: &ThirdPerson) -> CameraAsset {
    let mut root = vec![
        Node::Pivot {
            target: TargetRef::DriveTarget,
            offset: mode.socket_offset.to_array(),
            space: PivotSpace::Target,
            aim_offset: (mode.target_offset - mode.socket_offset).to_array(),
            inherit_rotation: false,
        },
        Node::LookInput {
            slot: "look".into(),
            additive: false,
        },
        Node::Boom {
            length: Value::Literal(mode.boom_length),
            side_offset: Value::Literal(0.0),
            up_offset: Value::Literal(0.0),
            length_speed: Value::Literal(0.0),
        },
    ];
    if mode.probe_radius > 0.0 {
        root.push(Node::Collision {
            radius: mode.probe_radius,
            layers: vec![],
            close_speed: SNAP_SPEED,
            open_speed: 0.0,
            open_rate: mode.lag_speed * 0.5,
            anchor: CollisionAnchor::Pivot,
            reuse_last_frame: false,
        });
    }
    root.push(Node::SmoothPosition {
        depth: mode.lag_speed,
        side: mode.lag_speed,
        height: mode.lag_speed,
        space: CamSpace::World,
    });
    root.push(Node::SmoothRotation {
        pitch: mode.rotation_lag,
        yaw: mode.rotation_lag,
        roll: mode.rotation_lag,
    });
    root.push(Node::Lean {
        gain: mode.lean_gain,
        max: mode.max_lean,
        speed: mode.lean_speed,
    });
    single_rig(
        "third_person",
        vec![look_slot((
            mode.min_pitch.to_degrees(),
            mode.max_pitch.to_degrees(),
        ))],
        vec![],
        root,
    )
}

/// The asset equivalent to [`CameraMode::Overhead`] with `mode`'s parameters (the pitch is
/// fixed through the `look` slot's clamp).
pub fn overhead_from(mode: &Overhead) -> CameraAsset {
    let pitch = mode.pitch.clamp(-std::f32::consts::FRAC_PI_2, 0.0).to_degrees();
    single_rig(
        "overhead",
        vec![look_slot((pitch, pitch))],
        vec![],
        vec![
            Node::Pivot {
                target: TargetRef::DriveTarget,
                offset: [0.0; 3],
                space: PivotSpace::World,
                aim_offset: [0.0; 3],
                inherit_rotation: false,
            },
            Node::LookInput {
                slot: "look".into(),
                additive: false,
            },
            Node::Boom {
                length: Value::Literal(mode.height),
                side_offset: Value::Literal(0.0),
                up_offset: Value::Literal(0.0),
                length_speed: Value::Literal(0.0),
            },
            Node::SmoothPosition {
                depth: mode.lag_speed,
                side: mode.lag_speed,
                height: mode.lag_speed,
                space: CamSpace::World,
            },
        ],
    )
}

/// The asset equivalent to [`CameraMode::FirstPerson`]: welded to the target at the eye offset,
/// the look composed onto the target's rotation.
pub fn first_person_from(mode: &FirstPerson) -> CameraAsset {
    single_rig(
        "first_person",
        vec![look_slot((-90.0, 90.0))],
        vec![],
        vec![
            Node::Pivot {
                target: TargetRef::DriveTarget,
                offset: mode.eye_offset.to_array(),
                space: PivotSpace::Target,
                aim_offset: [0.0; 3],
                inherit_rotation: true,
            },
            Node::LookInput {
                slot: "look".into(),
                additive: true,
            },
        ],
    )
}

/// The asset equivalent to [`CameraMode::Free`]: the `look` slot turns, the `move` slot flies
/// (x = strafe, y = forward), the `rise` and `sprint` variables lift and boost.
pub fn free_from(mode: &Free) -> CameraAsset {
    let mut move_slot = look_slot((-90.0, 90.0));
    move_slot.name = "move".into();
    single_rig(
        "free",
        vec![look_slot((-90.0, 90.0)), move_slot],
        vec![
            VarDef {
                name: "rise".into(),
                default: VarValue::Float(0.0),
                local_only: false,
            },
            VarDef {
                name: "sprint".into(),
                default: VarValue::Bool(false),
                local_only: false,
            },
        ],
        vec![
            Node::LookInput {
                slot: "look".into(),
                additive: false,
            },
            Node::Fly {
                slot: "move".into(),
                speed: mode.speed,
                sprint_multiplier: mode.sprint_multiplier,
                accel: mode.accel,
                rise: Value::Var("rise".into()),
                sprint: Value::Var("sprint".into()),
            },
        ],
    )
}

/// [`third_person_from`] with the default parameters.
pub fn third_person() -> CameraAsset {
    third_person_from(&ThirdPerson::default())
}

/// [`overhead_from`] with the default parameters.
pub fn overhead() -> CameraAsset {
    overhead_from(&Overhead::default())
}

/// [`first_person_from`] with the default parameters.
pub fn first_person() -> CameraAsset {
    first_person_from(&FirstPerson::default())
}

/// [`free_from`] with the default parameters.
pub fn free() -> CameraAsset {
    free_from(&Free::default())
}

/// The asset equivalent to a legacy rig's mode and settings.
pub fn from_legacy(rig: &CameraRig) -> CameraAsset {
    match &rig.mode {
        CameraMode::ThirdPerson(mode) => third_person_from(mode),
        CameraMode::Overhead(mode) => overhead_from(mode),
        CameraMode::FirstPerson(mode) => first_person_from(mode),
        CameraMode::Free(mode) => free_from(mode),
    }
}

/// The four presets as `(file stem, asset)` pairs, in a stable order.
pub fn all() -> [(&'static str, CameraAsset); 4] {
    [
        ("third_person", third_person()),
        ("overhead", overhead()),
        ("first_person", first_person()),
        ("free", free()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_validate_clean() {
        for (name, asset) in all() {
            let issues = asset.validate(&|_| false);
            assert!(issues.is_empty(), "{name}: {issues:?}");
            let text = asset.to_ron().expect("serializes");
            assert_eq!(CameraAsset::from_ron(&text).expect("re-parses"), asset, "{name}");
        }
    }

    #[test]
    fn from_legacy_picks_the_mode() {
        let rig = CameraRig {
            mode: CameraMode::Overhead(Overhead::default()),
            ..CameraRig::default()
        };
        assert_eq!(from_legacy(&rig).name, "overhead");
        assert_eq!(from_legacy(&CameraRig::default()).name, "third_person");
    }

    /// The shipped starter files equal the code-built presets. Set `REGIA_BLESS_CAMERA_PRESETS`
    /// to rewrite them (the lead blesses after an intended change).
    #[test]
    fn shipped_files_match_the_presets() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/camera/presets");
        for (stem, asset) in all() {
            let path = dir.join(format!("{stem}.camera.ron"));
            let expected = asset.to_ron().expect("serializes");
            if std::env::var_os("REGIA_BLESS_CAMERA_PRESETS").is_some() {
                std::fs::create_dir_all(&dir).expect("create presets dir");
                std::fs::write(&path, &expected).expect("write preset");
            }
            let on_disk =
                std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(on_disk, expected, "{stem}.camera.ron is stale");
        }
    }
}
