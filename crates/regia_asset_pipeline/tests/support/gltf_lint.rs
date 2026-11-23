// SPDX-License-Identifier: MPL-2.0
//! Hand-authored glTF fixtures shared by pipeline and editor lint acceptance.

use serde_json::{Value, json};

/// Minimal document and binary payload for lint acceptance tests.
pub struct Fixture {
    /// Editable glTF document.
    pub json: Value,
    /// Bytes referenced by the document's first buffer.
    pub bin: Vec<u8>,
}

impl Fixture {
    /// Two nearby triangles whose second UV layout is stretched.
    pub fn stretched() -> Self {
        let mut fixture = Self {
            json: json!({
                "asset": {"version": "2.0"}, "scene": 0,
                "scenes": [{"nodes": [0]}], "nodes": [{"mesh": 0}],
                "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "TEXCOORD_0": 1}}]}],
                "accessors": [], "bufferViews": [], "buffers": [{"byteLength": 0}]
            }),
            bin: Vec::new(),
        };
        fixture.accessor(
            &[
                0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.00002, 1.0, 0.0, 0.00002, 0.0, 1.0,
                0.00002,
            ],
            3,
        );
        fixture.json["accessors"][0]["min"] = json!([0, 0, 0]);
        fixture.json["accessors"][0]["max"] = json!([1, 1, 0.00002]);
        fixture.accessor(&[0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 5.0, 0.0, 105.0, 0.0, 5.0, 1.0], 2);
        fixture
    }

    /// Appends a tightly packed float accessor and returns its document index.
    pub fn accessor(&mut self, values: &[f32], dimensions: usize) -> usize {
        let offset = self.bin.len();
        self.bin.extend(values.iter().flat_map(|v| v.to_le_bytes()));
        let views = self.json["bufferViews"].as_array_mut().expect("views");
        let view = views.len();
        views.push(json!({"buffer": 0, "byteOffset": offset, "byteLength": std::mem::size_of_val(values)}));
        let accessors = self.json["accessors"].as_array_mut().expect("accessors");
        let index = accessors.len();
        accessors.push(json!({"bufferView": view, "componentType": 5126,
        "count": values.len() / dimensions, "type": match dimensions {
            1 => "SCALAR", 2 => "VEC2", 3 => "VEC3", 4 => "VEC4", _ => panic!("fixture dimension"),
        }}));
        self.json["buffers"][0]["byteLength"] = json!(self.bin.len());
        index
    }

    /// Encodes this fixture as a GLB with padded JSON and binary chunks.
    pub fn glb(&self) -> Vec<u8> {
        let mut text = serde_json::to_vec(&self.json).expect("json");
        text.resize(text.len().next_multiple_of(4), b' ');
        let mut bin = self.bin.clone();
        bin.resize(bin.len().next_multiple_of(4), 0);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"glTF");
        bytes.extend_from_slice(&2_u32.to_le_bytes());
        bytes.extend_from_slice(&((28 + text.len() + bin.len()) as u32).to_le_bytes());
        bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"JSON");
        bytes.extend(text);
        bytes.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"BIN\0");
        bytes.extend(bin);
        bytes
    }
}
