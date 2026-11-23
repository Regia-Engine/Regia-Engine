// SPDX-License-Identifier: MPL-2.0
//! `render.get_exposure` lists every baked section volume through the dispatcher (plan row W-24, ADR-8384).

use regia_agent::{BakedZoneList, LocalReply, call_local};
use regia_app::prelude::*;
use regia_render::RuntimeRenderer;
use regia_world_geom::zones::SectionVolumes;
use serde_json::{Value, json};

fn done(reply: LocalReply) -> Value {
    match reply {
        LocalReply::Done(Ok(value)) => value,
        LocalReply::Done(Err(err)) => panic!("render.get_exposure failed: {err:?}"),
        LocalReply::Pending(_) => panic!("render.get_exposure deferred"),
    }
}

fn volume(section: u16) -> SectionVolumes {
    SectionVolumes {
        section,
        exposure_ev: [8.5, 10.5],
        blend_m: 25.0,
        reverb_preset: "tube".to_string(),
        reverb_size_m: 16.0,
        interior_center_m: [f32::from(section), 0.0, 0.0],
        interior_half_m: [1.0, 2.0, 3.0],
    }
}

#[test]
fn render_get_exposure_lists_every_baked_zone() {
    let mut world = World::new();
    world.init_resource::<RuntimeRenderer>();
    let missing = done(call_local(&mut world, "render.get_exposure", json!({}), None));
    assert!(
        missing["zones"].is_null(),
        "zones without a list: {}",
        missing["zones"]
    );

    let zones: Vec<SectionVolumes> = (0..5).map(volume).collect();
    world.insert_resource(BakedZoneList { zones });
    let got = done(call_local(&mut world, "render.get_exposure", json!({}), None));
    let Some(list) = got["zones"].as_array() else {
        panic!("zones was not an array: {}", got["zones"]);
    };
    assert_eq!(list.len(), 5, "the list stopped at four");
    assert_eq!(list[4]["section"], json!(4));
    assert_eq!(list[4]["exposure_ev"], json!([8.5, 10.5]));
    assert_eq!(list[4]["blend_m"], json!(25.0));
    assert_eq!(list[4]["reverb_preset"], json!("tube"));
    assert_eq!(list[4]["reverb_size_m"], json!(16.0));
    assert_eq!(list[4]["interior_center_m"], json!([4.0, 0.0, 0.0]));
    assert_eq!(list[4]["interior_half_m"], json!([1.0, 2.0, 3.0]));
}
