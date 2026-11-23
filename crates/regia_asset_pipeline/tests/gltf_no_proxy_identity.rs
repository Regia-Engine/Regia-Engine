// SPDX-License-Identifier: MPL-2.0
//! A glTF without `collision_*` proxies imports exactly as it did before ADR-2341: the digest of
//! every output (the primary mesh, each sub-asset's path, id, type, payload and dependencies, and the
//! `.meta` entries) is pinned. The pin was taken from the base (integ/v034, f612a1ada) with this
//! same file, which uses no API the lane added.

#[path = "support/collision_glb.rs"]
mod collision_glb;

use collision_glb::{Node, cuboid, glb, octahedron};
use regia_asset::AssetId;
use regia_asset_pipeline::{Forward, TextureCookQuality, import_gltf_glb_bytes};

/// The digest of the import of a two-mesh prop with a socket and no proxies, from the base.
const PINNED: &str = "df85619c016baf032a64d70cbc9fe9bfe67d2381dc0b516967c7130bdcf52df3/06577b4efba1ca34954c8cd944a778319b7397268c1467286be1e998bbf4970d";

fn digest(forward: Forward) -> String {
    let bytes = glb(&[
        Node {
            name: "Crate",
            translation: [0.0, 0.0, 0.0],
            mesh: Some(cuboid([-0.5, 0.0, -0.5], [0.5, 1.0, 0.5])),
        },
        Node {
            name: "Knob",
            translation: [0.0, 1.2, 0.0],
            mesh: Some(octahedron(0.2)),
        },
        Node {
            name: "socket_top",
            translation: [0.0, 1.5, 0.0],
            mesh: None,
        },
    ]);
    let parent = AssetId::sub_asset(AssetId::default(), "no-proxy-identity");
    let imported =
        import_gltf_glb_bytes(&bytes, parent, forward, "linux", TextureCookQuality::Low).expect("import");
    let mut h = blake3::Hasher::new();
    let mut field = |b: &[u8]| {
        h.update(&(b.len() as u64).to_le_bytes());
        h.update(b);
    };
    field(&imported.primary_mesh.to_postcard().expect("primary mesh"));
    for sub in &imported.sub_assets {
        field(sub.path.as_bytes());
        field(sub.id.to_string().as_bytes());
        field(sub.type_path.as_bytes());
        field(&sub.payload);
        for dep in &sub.dependencies {
            field(dep.to_string().as_bytes());
        }
    }
    for entry in &imported.meta_entries {
        field(entry.path.as_bytes());
        field(entry.id.to_string().as_bytes());
        field(entry.type_path.as_bytes());
    }
    h.finalize().to_hex().to_string()
}

#[test]
fn an_import_without_proxies_is_byte_identical_to_the_base() {
    let convert = digest(Forward::Convert);
    let keep = digest(Forward::Keep);
    assert_eq!(
        format!("{convert}/{keep}"),
        PINNED,
        "the import of a file without proxies changed"
    );
}
