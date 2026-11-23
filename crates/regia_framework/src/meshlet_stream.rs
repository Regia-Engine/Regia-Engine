// SPDX-License-Identifier: MPL-2.0
//! Streaming band reads of cooked meshlets (ADR-3708, `docs/specs/98-visibility-buffer.md` § 8e).
//!
//! [`PakBandDisk`] is the `BandDisk` of a stream-shaped [`MeshletMesh`]: it keeps the cooked
//! payload (a zero-copy view of the mapped pak, through [`PayloadReader`]) and answers the
//! residency pass's band reads on the existing IO path (`regia_core::tasks::spawn_io`, the
//! `regia_world` cell-source pair — no new IO thread pool). `read_sections` is the blocking
//! admission/materialize read; `request_band` reads one band's cluster lists and triangles
//! asynchronously and posts a [`BandDelivery`] per finished read.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use regia_asset::{AssetId, PakPayload, PayloadReader};
use regia_render::visbuffer::{
    BandDelivery, BandDisk, BandDiskError, BandPayload, BandSections, MeshletFormatError, MeshletLoadError,
    MeshletMesh, MeshletStreamHead,
};

/// A cooked meshlet payload opened for band streaming: the stream-shaped mesh and its disk.
pub struct OpenMeshletStream {
    /// The mesh: cluster records in RAM, lists and triangles on the disk.
    pub mesh: MeshletMesh,
    /// The disk the residency pass reads band payloads from.
    pub disk: Arc<PakBandDisk>,
}

/// Opens `id` (a cooked `MESHLET_TYPE_PATH` sub-asset) for band streaming through `reader`:
/// parses the head, keeps the payload bytes, and faults the mapping's pages in on the IO pool
/// (so the frame thread's reads hit the page cache). The payload stays mapped (no heap copy of
/// the cooked lists) until every view of it drops.
///
/// # Errors
///
/// The asset is not cooked, the wrapper or head is unreadable, of another version, or
/// structurally invalid (the same codes as [`MeshletMesh::load`]).
pub fn open_stream(reader: &PayloadReader, id: AssetId) -> Result<OpenMeshletStream, MeshletLoadError> {
    let (file, _source) = reader
        .read(id)
        .map_err(|e| MeshletLoadError::NotCooked(e.to_string()))?;
    let (header, blob) =
        regia_asset::decode_cooked_file(&file).map_err(|e| MeshletFormatError::Cooked(e.to_string()))?;
    if header.type_tag != regia_core::id::TypePathHash::of(regia_render::visbuffer::MESHLET_TYPE_PATH) {
        return Err(MeshletFormatError::WrongType.into());
    }
    let head = MeshletMesh::parse_head(blob)?;
    // The section ranges are relative to the `.fmlt` blob: keep the payload view on the blob,
    // not the cooked file's wrapper bytes.
    let payload = file.subslice(blob).unwrap_or(file);
    let disk = Arc::new(PakBandDisk::new(reader.clone(), id, head.clone(), payload));
    // Fault the mapping's pages in off the frame thread (the pak-stream reader's trick), so the
    // admission read and the band slices are page-cache hits.
    {
        let state = Arc::clone(&disk.state);
        let reader = disk.reader.clone();
        let id = disk.id;
        regia_core::tasks::spawn_io(async move {
            let _ = with_payload(&state, &reader, id, fault_in);
        })
        .detach();
    }
    let mesh = MeshletMesh::from_head(head, disk.clone());
    Ok(OpenMeshletStream { mesh, disk })
}

/// Reads one `u32` array out of a payload range (the blob is not aligned for `u32`).
fn words(payload: &[u8], range: std::ops::Range<usize>) -> Vec<u32> {
    let Some(bytes) = payload.get(range) else {
        return Vec::new();
    };
    let (words, _) = bytes.as_chunks::<4>();
    words
        .iter()
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Touches one byte per page of the mapped payload so the disk reads behind them happen here
/// (the IO worker) and not on the frame thread. Returns the xor so the loads are not optimised
/// away.
fn fault_in(payload: &PakPayload) -> u8 {
    const PAGE: usize = 4096;
    if !payload.is_mapped() {
        return 0;
    }
    let bytes: &[u8] = payload;
    let mut acc = bytes.last().copied().unwrap_or(0);
    for &b in bytes.iter().step_by(PAGE) {
        acc ^= b;
    }
    std::hint::black_box(acc)
}

/// The payload bytes: the cached view, reading through the mounted paks otherwise.
fn with_payload<R>(
    state: &Mutex<PakBandState>,
    reader: &PayloadReader,
    id: AssetId,
    f: impl FnOnce(&PakPayload) -> R,
) -> Result<R, BandDiskError> {
    let mut state = state.lock().map_err(|_| BandDiskError {
        code: "band_unreadable",
        message: "payload cache lock poisoned".to_string(),
    })?;
    if state.payload.is_none() {
        let (payload, _) = reader.read(id).map_err(|e| BandDiskError {
            code: "band_not_found",
            message: e.to_string(),
        })?;
        state.payload = Some(Arc::new(payload));
    }
    let payload = state.payload.as_ref().ok_or(BandDiskError {
        code: "band_unreadable",
        message: "payload cache empty".to_string(),
    })?;
    Ok(f(payload))
}

/// A `BandDisk` over the cooked payload of one meshlet asset (ADR-3708).
pub struct PakBandDisk {
    reader: PayloadReader,
    id: AssetId,
    head: Arc<MeshletStreamHead>,
    state: Arc<Mutex<PakBandState>>,
}

struct PakBandState {
    /// The payload, cached after the first read (a mapped view: an `Arc` bump, not a copy).
    payload: Option<Arc<PakPayload>>,
    /// Finished reads, in request order.
    out_tx: Sender<BandDelivery>,
    out_rx: Receiver<BandDelivery>,
}

impl PakBandDisk {
    fn new(reader: PayloadReader, id: AssetId, head: MeshletStreamHead, payload: PakPayload) -> Self {
        let (out_tx, out_rx) = std::sync::mpsc::channel();
        Self {
            reader,
            id,
            head: Arc::new(head),
            state: Arc::new(Mutex::new(PakBandState {
                payload: Some(Arc::new(payload)),
                out_tx,
                out_rx,
            })),
        }
    }
}

impl BandDisk for PakBandDisk {
    fn read_sections(&self) -> Result<BandSections, BandDiskError> {
        with_payload(&self.state, &self.reader, self.id, |payload| {
            let head = &self.head;
            BandSections {
                vertices: words(payload, head.vertices_range.clone()),
                triangles: words(payload, head.triangles_range.clone()),
            }
        })
    }

    fn request_band(&self, tag: u64, clusters: &[u32]) {
        if clusters
            .iter()
            .any(|&c| self.head.meshlets.get(c as usize).is_none())
        {
            // The pass sends its own plan's bands; a miss is corruption, answered immediately.
            let tx = self.state.lock().map(|s| s.out_tx.clone());
            if let Ok(tx) = tx {
                let _ = tx.send(BandDelivery {
                    tag,
                    result: Err(BandDiskError {
                        code: "band_corrupt",
                        message: "cluster index past the records".to_string(),
                    }),
                });
            }
            return;
        }
        let state = Arc::clone(&self.state);
        let reader = self.reader.clone();
        let id = self.id;
        let head = Arc::clone(&self.head);
        let tx = self.state.lock().ok().map(|s| s.out_tx.clone());
        let clusters = clusters.to_vec();
        regia_core::tasks::spawn_io(async move {
            let Some(tx) = tx else {
                return;
            };
            let _ = tx.send(read_band(state, reader, id, head, tag, clusters));
        })
        .detach();
    }

    fn try_deliver(&self) -> Option<BandDelivery> {
        let state = self.state.lock().ok()?;
        state.out_rx.try_recv().ok()
    }
}

/// The worker's read: slice the clusters' vertex lists and triangles out of the payload and post
/// them. Out-of-section ranges map to `band_corrupt`; an unreadable payload maps to
/// `band_not_found`.
fn read_band(
    state: Arc<Mutex<PakBandState>>,
    reader: PayloadReader,
    id: AssetId,
    head: Arc<MeshletStreamHead>,
    tag: u64,
    clusters: Vec<u32>,
) -> BandDelivery {
    let result = with_payload(&state, &reader, id, |payload| {
        let bytes: &[u8] = payload;
        let lists_len: usize = clusters
            .iter()
            .map(|&c| head.meshlets[c as usize].vertex_count as usize)
            .sum();
        let tris_len: usize = clusters
            .iter()
            .map(|&c| head.meshlets[c as usize].triangle_count as usize)
            .sum();
        let mut lists = Vec::with_capacity(lists_len);
        let mut tris = Vec::with_capacity(tris_len);
        for &c in &clusters {
            // The cluster index was checked when the request was queued.
            let m = &head.meshlets[c as usize];
            let list_start = head
                .vertices_range
                .start
                .checked_add(m.vertex_offset as usize * 4);
            let list_end = list_start.and_then(|s| s.checked_add(m.vertex_count as usize * 4));
            let tri_start = head
                .triangles_range
                .start
                .checked_add(m.triangle_offset as usize * 4);
            let tri_end = tri_start.and_then(|s| s.checked_add(m.triangle_count as usize * 4));
            let (Some(list_end), Some(tri_end)) = (list_end, tri_end) else {
                return Err(BandDiskError {
                    code: "band_corrupt",
                    message: "cluster range overflow".to_string(),
                });
            };
            let Some(list) = bytes.get(list_start.unwrap_or(0)..list_end) else {
                return Err(BandDiskError {
                    code: "band_corrupt",
                    message: "vertex list past the section".to_string(),
                });
            };
            let Some(tri) = bytes.get(tri_start.unwrap_or(0)..tri_end) else {
                return Err(BandDiskError {
                    code: "band_corrupt",
                    message: "triangles past the section".to_string(),
                });
            };
            let (list_words, _) = list.as_chunks::<4>();
            lists.extend(
                list_words
                    .iter()
                    .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])),
            );
            let (tri_words, _) = tri.as_chunks::<4>();
            tris.extend(
                tri_words
                    .iter()
                    .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])),
            );
        }
        Ok(BandPayload { lists, tris })
    })
    .and_then(std::convert::identity);
    BandDelivery { tag, result }
}
