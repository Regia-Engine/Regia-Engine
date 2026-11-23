// SPDX-License-Identifier: MPL-2.0
//! `regia bake-lightmaps --meshlets` (ADR-2107): visibility-buffer clusters over each entry's
//! unwrap, stored in the `.flmap`. An entry the builder refuses is reported and has no clusters: its
//! receiver draws in the GPU-driven batches, so it does not lose the bake.

use std::num::NonZero;
use std::sync::atomic::{AtomicUsize, Ordering};

use regia_asset::lightmap::LightmapLayout;
use regia_asset_pipeline::lightmap_uv::{SourceAttributes, meshlets_for_unwrap};
use regia_pt::PtScene;
use serde_json::{Value, json};

/// What one entry's build left.
#[derive(Default)]
struct Built {
    blob: Vec<u8>,
    clusters: usize,
    levels: u32,
    /// Why it has no clusters.
    refused: Option<String>,
}

/// The clusters of a layout, one blob per entry in entry order.
pub(super) struct EntryMeshlets {
    /// `Lightmap::meshlets`.
    pub(super) blobs: Vec<Vec<u8>>,
    clusters: usize,
    levels: u32,
    refused: Vec<(String, String)>,
}

impl EntryMeshlets {
    /// The `meshlets` object of the command's JSON.
    pub(super) fn describe(&self) -> Value {
        json!({
            "entries": self.blobs.iter().filter(|b| !b.is_empty()).count(),
            "clusters": self.clusters,
            "levels": self.levels,
            "bytes": self.blobs.iter().map(Vec::len).sum::<usize>(),
            "refused": self.refused.iter().map(|(entry, message)| json!({
                "entry": entry,
                "code": "lightmap_meshlets",
                "message": message,
            })).collect::<Vec<_>>(),
        })
    }
}

fn build_entry(scene: &PtScene, receiver: usize, layout: &LightmapLayout, entry: usize) -> Built {
    let refused = |message: String| Built {
        refused: Some(message),
        ..Built::default()
    };
    let name = &layout.entries[entry].name;
    let _span = tracing::info_span!("lightmap_entry_meshlets", entry = %name).entered();
    let Some(mesh) = scene
        .instances
        .get(receiver)
        .and_then(|i| scene.meshes.get(i.mesh))
    else {
        return refused(format!("instance {receiver} has no mesh in the scene"));
    };
    let source = SourceAttributes {
        positions: &mesh.positions,
        normals: &mesh.normals,
        uvs: &mesh.uvs,
    };
    match meshlets_for_unwrap(source, &layout.unwraps[entry]) {
        Ok(built) => Built {
            blob: built.to_bytes(),
            clusters: built.meshlets.len(),
            levels: built.level_count(),
            refused: None,
        },
        Err(e) => refused(e.to_string()),
    }
}

/// Builds every entry's clusters on `threads` workers (0 = all cores). The result does not depend
/// on the number of workers: each entry's build is on its own and lands in its own slot.
pub(super) fn build(
    scene: &PtScene,
    receivers: &[usize],
    layout: &LightmapLayout,
    threads: usize,
) -> EntryMeshlets {
    let _span = tracing::info_span!("lightmap_meshlets", entries = layout.entries.len()).entered();
    let workers = if threads == 0 {
        std::thread::available_parallelism().map_or(1, NonZero::get)
    } else {
        threads
    }
    .clamp(1, receivers.len().max(1));
    let next = AtomicUsize::new(0);
    let run = || {
        let mut done = Vec::new();
        loop {
            let entry = next.fetch_add(1, Ordering::Relaxed);
            let Some(&receiver) = receivers.get(entry) else {
                return done;
            };
            done.push((entry, build_entry(scene, receiver, layout, entry)));
        }
    };
    let mut slots: Vec<Built> = std::iter::repeat_with(Built::default)
        .take(receivers.len())
        .collect();
    let lost = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers).map(|_| s.spawn(run)).collect();
        let mut lost = false;
        for handle in handles {
            match handle.join() {
                Ok(done) => {
                    for (entry, built) in done {
                        slots[entry] = built;
                    }
                }
                Err(_) => lost = true,
            }
        }
        lost
    });
    let mut out = EntryMeshlets {
        blobs: Vec::with_capacity(slots.len()),
        clusters: 0,
        levels: 0,
        refused: Vec::new(),
    };
    for (entry, built) in slots.into_iter().enumerate() {
        out.clusters += built.clusters;
        out.levels = out.levels.max(built.levels);
        if let Some(message) = built.refused {
            out.refused.push((layout.entries[entry].name.clone(), message));
        } else if built.blob.is_empty() && lost {
            out.refused.push((
                layout.entries[entry].name.clone(),
                "the cluster worker stopped".to_string(),
            ));
        }
        out.blobs.push(built.blob);
    }
    if out.blobs.iter().all(Vec::is_empty) {
        out.blobs.clear();
    }
    out
}
