// SPDX-License-Identifier: MPL-2.0
//! `terrain.seam_report` per drawn pass through the public agent dispatcher (brief W-15 item 4,
//! ADR-8470): with the draw's `TerrainDrawPasses` linked, every pass the draw ran last frame is
//! measured on the selection it drew and around its own morph centre, and the reply lists the
//! passes. No GPU: the passes are published as a draw publishes them.

// Test fixtures panic on a broken fixture.
#![allow(clippy::expect_used)]

use regia_agent::protocol::RpcError;
use regia_agent::{LocalReply, call_local};
use regia_app::prelude::*;
use regia_terrain::{
    BandMesh, CdlodConfig, LayerMouth, LodSelection, NodeDraw, QuadNode, Terrain, TerrainDrawPasses,
    TerrainLayers, TerrainMeshLayers, select_nodes,
};
use serde_json::{Value, json};

fn call(app: &mut App, method: &str, params: Value) -> Result<Value, RpcError> {
    match call_local(app.world_mut(), method, params, Some("terrain-seam-passes-test")) {
        LocalReply::Done(result) => result,
        LocalReply::Pending(_) => panic!("{method} answered later"),
    }
}

fn app() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(regia_agent::AgentPermissions::default());
    let n = 65u32;
    let hf = regia_asset::Heightfield {
        width: n,
        height: n,
        samples: vec![32768; (n * n) as usize],
    };
    let terrain = Terrain::new(
        &hf,
        64.0,
        100.0,
        0.0,
        CdlodConfig::default(),
        3,
        TerrainLayers::default(),
    )
    .expect("terrain");
    let mut layers = TerrainMeshLayers::new();
    layers.push_mouth(LayerMouth {
        hole: "gate".to_owned(),
        rings: vec![vec![[4.0, 0.0, 0.0], [5.0, 0.0, 0.0], [4.0, 0.0, 1.0]]],
        band: BandMesh::default(),
        collar: Vec::new(),
        throat: Vec::new(),
        body: Vec::new(),
        open: true,
    });
    let entity = app.world_mut().spawn((terrain, layers)).id();
    (app, entity)
}

#[test]
fn seam_report_measures_every_drawn_pass() {
    let (mut app, entity) = app();
    // Without the handle: every pass selects again and no pass is listed.
    let plain = call(&mut app, "terrain.seam_report", json!({ "pass": "cascade2" })).expect("plain");
    assert_eq!(plain["source"], json!("selected"));
    assert_eq!(plain["passes"], json!([]));

    // The draw ran main and three cascades last frame, each with its own selection and eye.
    let (root_level, main) = {
        let terrain = app.world().get::<Terrain>(entity).expect("terrain");
        let main = select_nodes(
            &terrain.map,
            &terrain.minmax,
            &terrain.cdlod,
            terrain.root_level,
            [1.0, 2.0],
            None,
        );
        (terrain.root_level, main)
    };
    let root = [NodeDraw {
        node: QuadNode::root(root_level),
        morph_start: 0.0,
        morph_end: 0.0,
    }];
    let handle = TerrainDrawPasses::linked();
    handle.publish([
        ("main", [1.0, 2.0], main.nodes.as_slice()),
        ("cascade0", [3.0, 4.0], main.nodes.as_slice()),
        ("cascade1", [5.0, 6.0], root.as_slice()),
        ("cascade2", [7.0, 8.0], root.as_slice()),
    ]);
    app.world_mut().insert_resource(handle);

    for (pass, eye, nodes) in [
        ("main", [1.0f32, 2.0f32], main.nodes.as_slice()),
        ("cascade0", [3.0, 4.0], main.nodes.as_slice()),
        ("cascade1", [5.0, 6.0], root.as_slice()),
        ("cascade2", [7.0, 8.0], root.as_slice()),
    ] {
        let report = call(&mut app, "terrain.seam_report", json!({ "pass": pass })).expect(pass);
        assert_eq!(report["pass"], json!(pass));
        assert_eq!(report["source"], json!("draw"), "{pass}");
        assert_eq!(report["eye"], json!(eye), "{pass}");
        assert_eq!(
            report["passes"],
            json!(["main", "cascade0", "cascade1", "cascade2"]),
            "{pass}"
        );
        // The measurement is the drawn selection's, not a fresh one.
        let expected = {
            let world = app.world_mut();
            let mut query = world.query::<&TerrainMeshLayers>();
            let layers = query.iter(world).next().expect("layers").clone();
            let terrain = world.get::<Terrain>(entity).expect("terrain");
            regia_terrain::seam_report(
                terrain,
                &layers,
                &LodSelection {
                    nodes: nodes.to_vec(),
                },
                eye,
            )
        };
        assert_eq!(report["ring_vertices"], json!(expected.ring_vertices), "{pass}");
        assert_eq!(report["max_gap_m"], json!(expected.max_gap_m), "{pass}");
        assert_eq!(report["max_lip_m"], json!(expected.max_lip_m), "{pass}");
    }
    // cascade3 did not run: it selects again.
    let report = call(&mut app, "terrain.seam_report", json!({ "pass": "cascade3" })).expect("cascade3");
    assert_eq!(report["source"], json!("selected"));
}
