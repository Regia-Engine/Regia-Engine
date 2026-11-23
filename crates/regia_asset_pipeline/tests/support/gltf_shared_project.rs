// SPDX-License-Identifier: MPL-2.0
//! Shared external glTF textures (CP_SIZE, ADR-8055). All fixtures are synthetic.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use image::ImageEncoder;
use regia_asset::registry::{AssetRegistry, cache_registry_path};
use regia_asset::{
    AssetId, AssetManifest, CookedStandardMaterial, MetaSidecar, TextureSlot, cooked_cache_dir,
    cooked_file_name, decode_cooked_file, meta_path_for,
};
use regia_asset_pipeline::{CookOptions, cook_project, pack_cooked_directory_with};
use regia_test_utils::{unique_temp_dir, write_atomic};

pub(crate) struct Project(pub(crate) PathBuf);

impl Project {
    pub(crate) fn new() -> Self {
        let root = unique_temp_dir("gltf-texture-dedupe");
        std::fs::create_dir_all(root.join("assets/models")).expect("assets");
        // A project manifest: a model outside any project may only reach files below its own
        // folder (ADR-4922 finding 19), and these models name `../shared.png`.
        write_atomic(
            &root.join("Regia.toml"),
            b"[project]\nname = \"gltf-texture-dedupe\"\nid = \"dev.regia.gltf_texture_dedupe\"\nversion = \"0.1.0\"\n",
        )
        .expect("Regia.toml");
        let this = Self(root);
        this.image(7);
        let positions: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let bytes: Vec<u8> = positions.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
        write_atomic(&this.0.join("assets/models/triangle.bin"), &bytes).expect("buffer");
        this.model("a", "../shared.png", false);
        this.model("b", "../shared.png", false);
        this
    }

    pub(crate) fn image(&self, seed: u8) {
        let pixels: Vec<u8> = (0..256 * 256u32)
            .flat_map(|i| [(i as u8).wrapping_mul(seed), (i >> 8) as u8, seed, 255])
            .collect();
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&pixels, 256, 256, image::ExtendedColorType::Rgba8)
            .expect("encode PNG");
        write_atomic(&self.0.join("assets/shared.png"), &png).expect("image");
    }

    pub(crate) fn model(&self, name: &str, uri: &str, normal: bool) {
        let material = if normal {
            serde_json::json!({"name":"surface", "normalTexture":{"index":0}})
        } else {
            serde_json::json!({"name":"surface", "pbrMetallicRoughness":{"baseColorTexture":{"index":0}}})
        };
        let doc = serde_json::json!({
            "asset":{"version":"2.0"}, "scene":0, "scenes":[{"nodes":[0]}],
            "nodes":[{"mesh":0}],
            "meshes":[{"primitives":[{"attributes":{"POSITION":0},"material":0}]}],
            "materials":[material], "textures":[{"source":0}], "images":[{"uri":uri,"mimeType":"image/png"}],
            "buffers":[{"uri":"triangle.bin","byteLength":36}],
            "bufferViews":[{"buffer":0,"byteLength":36}],
            "accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3",
                          "min":[0,0,0],"max":[1,1,0]}]
        });
        write_atomic(
            &self.0.join(format!("assets/models/{name}.gltf")),
            &serde_json::to_vec(&doc).expect("JSON"),
        )
        .expect("model");
    }

    pub(crate) fn cook(&self) -> regia_asset_pipeline::CookReport {
        let report = cook_project(CookOptions {
            project_root: self.0.clone(),
            platform: "linux".into(),
            incremental: true,
        })
        .expect("cook");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        report
    }

    pub(crate) fn cache(&self) -> PathBuf {
        cooked_cache_dir(&self.0, "linux")
    }

    pub(crate) fn manifest(&self) -> AssetManifest {
        AssetManifest::read_from_cache(&self.cache())
            .expect("manifest")
            .expect("present")
    }

    pub(crate) fn texture_ids(&self) -> Vec<AssetId> {
        let registry = AssetRegistry::read_from(&cache_registry_path(&self.cache()))
            .expect("registry")
            .expect("present");
        registry
            .entries()
            .filter(|e| e.type_path == "regia_asset::CookedTexture")
            .map(|e| e.id)
            .collect()
    }

    pub(crate) fn material_texture(&self, name: &str, normal: bool) -> AssetId {
        let manifest = self.manifest();
        let row = manifest
            .entries
            .iter()
            .find(|e| {
                e.path.starts_with(&format!("assets/models/{name}.gltf#"))
                    && e.type_path == "regia_asset::CookedStandardMaterial"
            })
            .expect("material row");
        let bytes = std::fs::read(self.cache().join(cooked_file_name(row.id))).expect("material file");
        let (header, payload) = decode_cooked_file(&bytes).expect("header");
        let material = CookedStandardMaterial::from_postcard(payload).expect("material");
        let slot = if normal {
            material.authored.base.normal_map
        } else {
            material.authored.base.base_color_map
        };
        let Some(TextureSlot::Asset(id)) = slot else {
            panic!("asset texture slot")
        };
        assert!(
            header.dependencies.contains(&id),
            "chunk dependency includes shared image"
        );
        id
    }

    pub(crate) fn pack(&self, compress: bool) -> PathBuf {
        pack_cooked_directory_with(
            &self.cache(),
            &self.0.join("paks"),
            &BTreeMap::new(),
            compress,
            false,
        )
        .expect("pack")
        .remove(0)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn image_meta(root: &Path) -> MetaSidecar {
    MetaSidecar::from_ron(&std::fs::read(meta_path_for(&root.join("assets/shared.png"))).expect("image meta"))
        .expect("parse image meta")
}
