// SPDX-License-Identifier: MPL-2.0
//! End-to-end glTF convention cook: LOD groups, sockets, Rem/A52 variants, ORM pack.

use regia_asset::AssetId;
use regia_asset_pipeline::{
    Forward, TextureCookQuality, build_lod_groups, build_material_variant_sets, import_gltf_glb_bytes,
    pack_orm_rgba8, socket_from_node,
};

/// Compact GLB with three LOD meshes + a socket empty (matches the lib test helper style).
fn make_lod_socket_glb() -> Vec<u8> {
    let positions = [[0.0f32; 3]; 3];
    let indices = [0u32, 1, 2];
    let vc = positions.len();
    let mut bin = Vec::new();
    for p in &positions {
        bin.extend_from_slice(&p[0].to_le_bytes());
        bin.extend_from_slice(&p[1].to_le_bytes());
        bin.extend_from_slice(&p[2].to_le_bytes());
    }
    for &i in &indices {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    let pl = vc * 12;
    let io = vc * 12;
    let bl = bin.len();
    let json = format!(
        r#"{{"accessors":[{{"bufferView":0,"byteOffset":0,"componentType":5126,"count":{vc},"type":"VEC3","min":[0.0,0.0,0.0],"max":[0.0,0.0,0.0]}},{{"bufferView":1,"byteOffset":0,"componentType":5125,"count":3,"type":"SCALAR"}}],"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":{pl},"byteStride":12,"target":34962}},{{"buffer":0,"byteOffset":{io},"byteLength":12,"target":34963}}],"buffers":[{{"byteLength":{bl}}}],"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1,"mode":4}}]}},{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1,"mode":4}}]}},{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1,"mode":4}}]}}],"nodes":[{{"mesh":0,"name":"Body_LOD0"}},{{"mesh":1,"name":"Body_LOD1"}},{{"mesh":2,"name":"Body_LOD2"}},{{"name":"socket_turret","translation":[0.0,1.5,0.0]}}],"scenes":[{{"nodes":[0,1,2,3]}}],"scene":0,"asset":{{"version":"2.0"}}}}"#
    );
    let mut json_bytes = json.into_bytes();
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let total_len = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut glb = Vec::new();
    glb.extend_from_slice(&0x46546C67u32.to_le_bytes());
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total_len as u32).to_le_bytes());
    glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json_bytes);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    glb.extend_from_slice(&bin);
    glb
}

#[test]
fn cook_path_collects_lod_groups_and_sockets() {
    let glb = make_lod_socket_glb();
    let parent = AssetId::new_random();
    let imported =
        import_gltf_glb_bytes(&glb, parent, Forward::Keep, "linux", TextureCookQuality::Low).expect("import");

    assert_eq!(imported.lod_groups.len(), 1, "Body LOD0-2 → one group");
    assert_eq!(imported.lod_groups[0].name, "Body");
    assert_eq!(imported.lod_groups[0].levels.len(), 3);

    assert_eq!(imported.sockets.sockets.len(), 1);
    assert_eq!(imported.sockets.sockets[0].name, "turret");

    let types: Vec<_> = imported.sub_assets.iter().map(|s| s.type_path.as_str()).collect();
    assert!(types.contains(&"regia_asset::LodGroup"));
    assert!(types.contains(&"regia_asset::PrefabSocketSet"));
}

#[test]
fn rem_a52_and_orm_helpers() {
    let a = AssetId::new_random();
    let b = AssetId::new_random();
    let groups = build_lod_groups(&[("Hull_LOD0".into(), a), ("Hull_LOD1".into(), b)]);
    assert_eq!(groups[0].levels.len(), 2);
    let sock = socket_from_node("socket_gun", [0.0; 16]).expect("socket");
    assert_eq!(sock.name, "gun");
    let packed = pack_orm_rgba8(1, 1, Some(&[200, 0, 0, 255]), Some(&[0, 10, 20, 255]));
    assert_eq!(packed, [200, 10, 20, 255]);
    let sets = build_material_variant_sets(&[("hull_Rem".into(), a), ("hull_A52".into(), b)]);
    assert_eq!(sets[0].slot, "hull");
    assert_eq!(sets[0].get("Rem"), Some(a));
    assert_eq!(sets[0].get("A52"), Some(b));
}
