// SPDX-License-Identifier: MPL-2.0
//! `anim.node_drivers` through the real dispatcher (ADR-8410).

use regia_agent::{LocalReply, call_local};
use regia_animation::node_driver::{
    DriveKind, NodeDrive, NodeDriveAngles, NodeDriveParams, NodeDriveState, NodeDriver, SlewLimits,
};
use regia_app::App;
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use serde_json::{Value, json};

fn call(app: &mut App, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    match call_local(app.world_mut(), "anim.node_drivers", params, None) {
        LocalReply::Done(result) => result,
        LocalReply::Pending(_) => panic!("inspection must be synchronous"),
    }
}

fn code(error: &regia_agent::protocol::RpcError) -> &str {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

#[test]
fn anim_node_drivers_reports_drives_and_angles() {
    let mut app = App::new();
    let empty = call(&mut app, json!({})).expect("empty list");
    assert_eq!(empty["entities"].as_array().map(Vec::len), Some(0));

    let entity = app
        .world_mut()
        .spawn((
            NodeDriver {
                drives: vec![
                    NodeDrive {
                        node: 4,
                        kind: DriveKind::Hinge {
                            axis: Vec3::Y,
                            min: 0.0,
                            max: 55.0,
                            rate: 1000.0,
                        },
                        input: "open".into(),
                    },
                    NodeDrive {
                        node: 8,
                        kind: DriveKind::Slew {
                            yaw_axis: Vec3::Y,
                            pitch_axis: Vec3::NEG_X,
                            max_rate: 20.0,
                            limits: SlewLimits {
                                yaw_min: -30.0,
                                yaw_max: 30.0,
                                pitch_min: -10.0,
                                pitch_max: 10.0,
                            },
                        },
                        input: "aim".into(),
                    },
                ],
            },
            NodeDriveParams::from_pairs([("open", 12.0), ("open", 99.0), ("aim", 4.0), ("aim.pitch", 1.5)]),
            NodeDriveState {
                angles: vec![
                    NodeDriveAngles {
                        primary: 12.0,
                        secondary: 0.0,
                    },
                    NodeDriveAngles {
                        primary: 4.0,
                        secondary: 1.5,
                    },
                ],
            },
        ))
        .id();
    let bare = app.world_mut().spawn(()).id();

    let report = call(&mut app, json!({"entity": entity.to_bits()})).expect("one entity");
    assert_eq!(report["entity"], entity.to_bits());
    assert_eq!(report["drives"][0]["kind"], "hinge");
    assert_eq!(report["drives"][0]["node"], 4);
    assert_eq!(report["drives"][0]["primary"], 12.0);
    assert_eq!(report["drives"][0]["value"], 12.0);
    assert_eq!(report["params"][0]["value"], 12.0);
    assert_eq!(report["params"][1]["value"], 99.0);
    assert_eq!(report["drives"][1]["kind"], "slew");
    assert_eq!(report["drives"][1]["pitch_value"], 1.5);
    assert_eq!(report["drives"][1]["limits"]["yaw_max"], 30.0);

    let listed = call(&mut app, json!({})).expect("list");
    assert_eq!(listed["entities"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["entities"][0]["entity"], entity.to_bits());

    let missing = call(&mut app, json!({"entity": bare.to_bits()})).expect_err("no driver");
    assert_eq!(code(&missing), "F0003");
    let absent =
        call(&mut app, json!({"entity": Entity::from_bits(0x100).to_bits()})).expect_err("no entity");
    assert_eq!(code(&absent), "F0003");
    let bad = call(&mut app, json!({"entity": "door"})).expect_err("bad id");
    assert_eq!(code(&bad), "F0002");
}
