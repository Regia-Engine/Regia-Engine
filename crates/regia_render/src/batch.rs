// SPDX-License-Identifier: MPL-2.0
//! Draw sorting, instancing and batching (`specs/13` § 5 step 4).

/// Key for pipeline state sorting: mesh id + material id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BatchKey {
    /// Mesh handle (stable index in the mesh arena).
    pub mesh: u32,
    /// Material handle.
    pub material: u32,
}

/// One logical draw before instancing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawItem {
    /// Batch key.
    pub key: BatchKey,
    /// Index into the object storage buffer.
    pub object_index: u32,
    /// Depth key for transparent sorting (view-space Z).
    pub depth: f32,
}

/// Groups consecutive draws with the same [`BatchKey`] into instanced runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstancedBatch {
    /// Shared mesh/material.
    pub key: BatchKey,
    /// Object indices drawn in one instanced call.
    pub object_indices: Vec<u32>,
}

/// Sorts opaque draws by batch key (and front-to-back within the same key when `front_to_back`).
pub fn sort_opaque(draws: &mut [DrawItem], front_to_back: bool) {
    draws.sort_by(|a, b| {
        let key_cmp = a.key.cmp(&b.key);
        if key_cmp != std::cmp::Ordering::Equal {
            return key_cmp;
        }
        if front_to_back {
            a.depth.total_cmp(&b.depth)
        } else {
            std::cmp::Ordering::Equal
        }
    });
}

/// Sorts transparent draws back-to-front.
pub fn sort_transparent(draws: &mut [DrawItem]) {
    draws.sort_by(|a, b| b.depth.total_cmp(&a.depth));
}

/// Merges sorted draws into instanced batches.
#[must_use]
pub fn batch_instanced(draws: &[DrawItem]) -> Vec<InstancedBatch> {
    let mut out: Vec<InstancedBatch> = Vec::new();
    for item in draws {
        if let Some(last) = out.last_mut()
            && last.key == item.key
        {
            last.object_indices.push(item.object_index);
            continue;
        }
        out.push(InstancedBatch {
            key: item.key,
            object_indices: vec![item.object_index],
        });
    }
    out
}

/// Returns the number of draw calls after instancing (one per batch).
#[must_use]
pub fn draw_call_count(batches: &[InstancedBatch]) -> u32 {
    batches.len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_k_cubes_fifty_materials_batches_under_sixty() {
        let mut draws = Vec::with_capacity(10_000);
        for i in 0..10_000u32 {
            let mesh = 0; // same cube mesh
            let material = i % 50;
            draws.push(DrawItem {
                key: BatchKey { mesh, material },
                object_index: i,
                depth: (i as f32) * 0.01,
            });
        }
        sort_opaque(&mut draws, true);
        let batches = batch_instanced(&draws);
        assert_eq!(batches.len(), 50, "one instanced draw per material");
        assert!(draw_call_count(&batches) <= 60);
    }
}
