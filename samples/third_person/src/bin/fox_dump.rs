// SPDX-License-Identifier: MIT-0
//! Dev-time dump of the cooked Fox: joint names, clip names, mesh bounds. Not shipped.
use regia_animation::cooked::{load_cooked_clip, load_cooked_skeleton};
use regia_asset::AssetManifest;
use std::path::PathBuf;

fn main() {
    // Runtime path: `env!("CARGO_MANIFEST_DIR")` would embed the build machine (ADR-10370).
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        PathBuf::from,
    );
    let cache = manifest.join(".regia/cooked/linux");
    let manifest =
        AssetManifest::from_postcard(&std::fs::read(cache.join("asset_manifest.bin")).expect("manifest"))
            .expect("parse");
    for e in &manifest.entries {
        let bytes = std::fs::read(cache.join(format!("{}.fckd", e.id))).expect("fckd");
        if e.type_path.contains("Skeleton") {
            let (_, sk) = load_cooked_skeleton(&bytes).expect("skeleton");
            println!("SKELETON {} joints={} path={}", e.id, sk.len(), e.path);
            for i in 0..sk.len() {
                let name = sk.joint_names[i].as_str();
                let bind = &sk.bind_pose[i];
                println!(
                    "  {i:>2} {name:<22} t=({:.3},{:.3},{:.3})",
                    bind.translation.x, bind.translation.y, bind.translation.z
                );
            }
        } else if e.type_path.contains("AnimationClip") {
            let (_, clip) = load_cooked_clip(&bytes).expect("clip");
            println!(
                "CLIP {:?} dur={:.3} tracks={}",
                clip.name,
                clip.duration,
                clip.tracks.len()
            );
        } else {
            println!("{} {} ({})", e.type_path, e.id, e.path);
            if e.type_path.contains("Mesh") {
                match regia_asset::load_mesh_bytes(&bytes) {
                    Ok(loaded) => {
                        let m = &loaded.mesh;
                        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
                        for p in &m.positions {
                            for i in 0..3 {
                                lo[i] = lo[i].min(p[i]);
                                hi[i] = hi[i].max(p[i]);
                            }
                        }
                        println!(
                            "  runtime mesh: verts={} tris={} lo={lo:?} hi={hi:?}",
                            m.positions.len(),
                            m.indices.len() / 3
                        );
                    }
                    Err(err) => println!("  runtime mesh FAILED: {err}"),
                }
            }
        }
    }
}
