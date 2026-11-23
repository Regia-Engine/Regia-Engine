// SPDX-License-Identifier: MPL-2.0
//! v0.1.18 (lane v0118-emissive-hitch, ADR-225): emissive fits keyed by mesh content, and the
//! fits and probe traces of new meshes off the frame thread.
//!
//! DO27 v0.1.1 (Regia Engine v0.1.16) logged 60–598 ms frames while shooting hulls, all inside
//! `emissive_lights` > `emissive_probes`: every damage step landed the same mesh again under a
//! new `Arc` (a new [`crate::FrameMesh::mesh_key`]), and the cache, keyed by that address,
//! fitted and traced it again on the frame thread.
//!
//! * **Content key.** A mesh's fit is cached under a 64-bit hash of what the fit reads (vertex
//!   positions, the texture coordinates when the material has an emissive map, the indices)
//!   plus the vertex and index counts and the material's [`super::material_signature`]
//!   ([`ContentKey`]). The hash is computed once per new mesh key and memoized on the key
//!   ([`Ident`]): the memo holds a `Weak` of the mesh when the host lent one
//!   ([`FitQueue::hold`], the runtime path), trusted while the weak lives and names the same
//!   buffers — the weaks pin the allocation and `Arc::get_mut` refuses while any weak lives,
//!   so the public buffers cannot change in safe code — and otherwise checks the vertex and
//!   index buffers' addresses and lengths plus a fingerprint of 32 sampled vertices and
//!   indices every frame.
//! * **Probes per object.** A distance probe is traced in world space, so it is reused only for
//!   the same object ([`crate::FrameMesh::object_key`]) landing the same content again: the
//!   probe its previous mesh had. Two objects with equal meshes still trace their own.
//! * **Background work.** Past the load frame, a fit or a probe trace that is not cached runs on
//!   one worker thread. The frame thread hashes, copies the fit's inputs and gathers the probe
//!   triangles, and starts no new piece of work once the collection has spent
//!   [`EMISSIVE_FRAME_BUDGET_MS`] (one piece always starts, so the queue moves). Until its
//!   lights land, a mesh that replaced another on the same object draws with the previous
//!   mesh's lights ("stand-in"); a mesh new to its object draws without them, as a mesh whose
//!   probes waited did before. The load frame, and a cache without the worker (the unit tests,
//!   `REGIA_EMISSIVE_SYNC=1`), do everything on the frame thread as v0.1.17 did.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak, mpsc};
use std::time::Instant;

use regia_core::glam::Vec3;

use super::{
    Bvh, CacheEntry, Emission, EmissiveLight, FitParams, FitVertices, GatherInput, ProbeSet, fit_emission,
    light_probe,
};
use crate::lab_frame::{LabTexture, LabVertex};
use crate::runtime::CpuMesh;

/// Frame-thread time (ms) after which an emissive collection past the load frame starts no new
/// hash, fit or probe trace (the first piece of a frame always starts).
pub const EMISSIVE_FRAME_BUDGET_MS: f32 = 1.0;

/// Frames a replaced mesh's lights stay available as its object's stand-in.
const STAND_IN_FRAMES: u64 = 120;

/// Frames an object's landed lights and probes are kept for a later mesh of the same content.
const OBJECT_KEEP_FRAMES: u64 = 600;

/// Meshes remembered per object.
const OBJECT_SLOTS: usize = 4;

/// Vertices and indices sampled into the per-frame fingerprint of a key whose mesh the host
/// never lent.
const SAMPLES: usize = 32;

/// What a fit depends on: the content hash of the mesh, its counts and the material's
/// emission signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ContentKey {
    hash: u64,
    vertices: usize,
    indices: usize,
    signature: u64,
}

/// Cumulative counters of the emissive fit cache and its background worker
/// ([`crate::FrameRenderer::emissive_fit_stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EmissiveFitStats {
    /// Fits computed (on the frame thread or the worker).
    pub fits_done: u64,
    /// Fits sent to the background worker (they land in a later frame).
    pub fits_deferred: u64,
    /// Mesh-frames whose hash, fit or probe trace waited because the frame had spent
    /// [`EMISSIVE_FRAME_BUDGET_MS`].
    pub budget_holds: u64,
    /// New mesh keys whose fit came from the content cache (no fit ran).
    pub cache_hits: u64,
    /// New mesh keys whose probes came from the same object's earlier mesh of the same content.
    pub probe_hits: u64,
    /// Lights whose distance probe was traced.
    pub probes_traced: u64,
    /// Mesh-frames drawn with the lights of the mesh they replaced while their own were pending.
    pub stand_ins: u64,
    /// Content hashes computed (one per new mesh key or changed mesh).
    pub hashes: u64,
    /// Frame-thread time in the emissive collection, milliseconds.
    pub frame_ms: f64,
    /// Worker time in fits and probe traces, milliseconds.
    pub worker_ms: f64,
    /// Fits and probe batches queued or running now.
    pub pending: u32,
    /// The worker is in use (false on the load frame, without a renderer, with
    /// `REGIA_EMISSIVE_SYNC=1`, or when its thread could not start).
    pub background: bool,
}

impl EmissiveFitStats {
    /// The counters accumulated since `earlier` (the gauges `pending` and `background` are
    /// this snapshot's).
    #[must_use]
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            fits_done: self.fits_done.saturating_sub(earlier.fits_done),
            fits_deferred: self.fits_deferred.saturating_sub(earlier.fits_deferred),
            budget_holds: self.budget_holds.saturating_sub(earlier.budget_holds),
            cache_hits: self.cache_hits.saturating_sub(earlier.cache_hits),
            probe_hits: self.probe_hits.saturating_sub(earlier.probe_hits),
            probes_traced: self.probes_traced.saturating_sub(earlier.probes_traced),
            stand_ins: self.stand_ins.saturating_sub(earlier.stand_ins),
            hashes: self.hashes.saturating_sub(earlier.hashes),
            frame_ms: (self.frame_ms - earlier.frame_ms).max(0.0),
            worker_ms: (self.worker_ms - earlier.worker_ms).max(0.0),
            pending: self.pending,
            background: self.background,
        }
    }
}

/// The frame thread's share of new work in one collection.
pub(crate) struct FrameBudget {
    started: Instant,
    background: bool,
    admitted: bool,
    /// The mesh key of the last piece started: its hash and its fit copy are one piece.
    current: Option<u64>,
}

impl FrameBudget {
    pub(crate) fn new(started: Instant, background: bool) -> Self {
        Self {
            started,
            background,
            admitted: false,
            current: None,
        }
    }

    /// New work waits for a later frame: the collection is past the budget and has already
    /// started a piece. Never on the frame-thread path.
    pub(crate) fn exhausted(&self) -> bool {
        self.background
            && self.admitted
            && self.started.elapsed().as_secs_f32() * 1000.0 >= EMISSIVE_FRAME_BUDGET_MS
    }

    /// [`Self::exhausted`], except for the mesh whose piece already started.
    fn exhausted_for(&self, key: u64) -> bool {
        self.current != Some(key) && self.exhausted()
    }

    /// Starts a piece of new work (`key`: its mesh, `None` for a probe batch).
    pub(crate) fn admit(&mut self, key: Option<u64>) {
        self.admitted = true;
        self.current = key;
    }
}

/// wyhash's folded multiply (Wang Yi, public domain): the 128-bit product's halves xored.
#[inline]
fn fold(a: u64, b: u64) -> u64 {
    let r = u128::from(a) * u128::from(b);
    (r as u64) ^ ((r >> 64) as u64)
}

/// Odd 64-bit constants. The first three are SplitMix64's (Steele, Lea & Flood 2014): its
/// golden-ratio increment and its two finalizer multipliers. The fourth is an odd constant
/// with no cited source (any odd constant with well-mixed bits serves).
const K: [u64; 4] = [
    0x9e37_79b9_7f4a_7c15,
    0xbf58_476d_1ce4_e5b9,
    0x94d0_49bb_1331_11eb,
    0xd6e8_feb8_6659_fd93,
];

#[inline]
fn pair(a: f32, b: f32) -> u64 {
    u64::from(a.to_bits()) | (u64::from(b.to_bits()) << 32)
}

/// 64-bit hash of what a fit reads: positions, texture coordinates when `with_uv`, indices.
/// Four independent lanes (vertex or index word `i` feeds lane `i mod 4`) so the multiplies
/// overlap.
pub(crate) fn content_hash(vertices: &[LabVertex], indices: &[u32], with_uv: bool) -> u64 {
    let mut lanes = K;
    for (i, v) in vertices.iter().enumerate() {
        let [x, y, z] = v.position;
        let l = &mut lanes[i & 3];
        if with_uv {
            *l = fold(*l ^ pair(x, y), K[1] ^ pair(z, v.uv[0]));
            *l = fold(*l ^ u64::from(v.uv[1].to_bits()), K[2]);
        } else {
            *l = fold(*l ^ pair(x, y), K[1] ^ u64::from(z.to_bits()));
        }
    }
    let (quads, rest) = indices.as_chunks::<4>();
    for (j, q) in quads.iter().enumerate() {
        let a = u64::from(q[0]) | (u64::from(q[1]) << 32);
        let b = u64::from(q[2]) | (u64::from(q[3]) << 32);
        let l = &mut lanes[j & 3];
        *l = fold(*l ^ a, K[3] ^ b);
    }
    for (j, &i) in rest.iter().enumerate() {
        lanes[j] = fold(lanes[j] ^ u64::from(i), K[0]);
    }
    let h = fold(lanes[0] ^ K[2], lanes[1] ^ K[3]) ^ fold(lanes[2] ^ K[0], lanes[3] ^ K[1]);
    let counts = (vertices.len() as u64) ^ ((indices.len() as u64) << 32) ^ u64::from(with_uv);
    fold(h ^ K[1], counts ^ K[2])
}

/// Fingerprint of [`SAMPLES`] vertices and indices spread over the buffers (first and last
/// included): cheap enough to check every frame.
fn sample_fingerprint(vertices: &[LabVertex], indices: &[u32]) -> u64 {
    let mut h = K[0];
    if let Some(last) = vertices.len().checked_sub(1) {
        for s in 0..SAMPLES {
            let v = &vertices[s * last / (SAMPLES - 1)];
            let [x, y, z] = v.position;
            h = fold(h ^ pair(x, y), K[1] ^ pair(z, v.uv[0]));
            h ^= u64::from(v.uv[1].to_bits());
        }
    }
    if let Some(last) = indices.len().checked_sub(1) {
        for s in 0..SAMPLES {
            h = fold(h ^ u64::from(indices[s * last / (SAMPLES - 1)]), K[2]);
        }
    }
    h
}

/// The memoized content hash of one mesh key, and how the key is known to still name it.
struct Ident {
    /// The host's mesh when it lent one ([`FitQueue::hold`]): trusted while this `Weak` lives
    /// and names the same buffers. The weaks (this and the mesh registry's) pin the
    /// allocation, and `Arc::get_mut` / `Arc::try_unwrap` refuse while any weak lives, so the
    /// public buffers cannot change in safe code; a successful upgrade means the registry's
    /// weak is alive too (one strong count per allocation).
    weak: Option<Weak<CpuMesh>>,
    vertices_at: usize,
    vertices: usize,
    indices_at: usize,
    indices: usize,
    with_uv: bool,
    sample: u64,
    hash: u64,
    last_frame: u64,
}

/// A content-keyed fit.
struct FitSlot {
    lights: Arc<Vec<EmissiveLight>>,
    last_frame: u64,
}

/// One landed mesh of an object: its probes for a later mesh of the same content, and its
/// lights and probes as the stand-in for the mesh that replaces it.
struct ObjectSlot {
    mesh_key: u64,
    content: ContentKey,
    lights: Arc<Vec<EmissiveLight>>,
    probes: ProbeSet,
    last_frame: u64,
}

/// One emitter's lights (world space) and probe reaches in a background trace.
pub(crate) struct TraceItem {
    pub(crate) key: u64,
    pub(crate) content: ContentKey,
    pub(crate) lights: Vec<EmissiveLight>,
    pub(crate) reach: Vec<f32>,
}

/// A fit's inputs, copied off the mesh.
struct FitGeometry {
    positions: Vec<[f32; 3]>,
    /// Empty without an emissive map (the fit does not read them then).
    uvs: Vec<[f32; 2]>,
}

impl FitVertices for FitGeometry {
    fn count(&self) -> usize {
        self.positions.len()
    }
    fn position(&self, i: usize) -> [f32; 3] {
        self.positions[i]
    }
    fn uv(&self, i: usize) -> [f32; 2] {
        self.uvs.get(i).copied().unwrap_or([0.0; 2])
    }
}

enum Job {
    Fit {
        content: ContentKey,
        geometry: FitGeometry,
        indices: Vec<u32>,
        base: [f32; 3],
        double_sided: bool,
        map: Option<LabTexture>,
        params: FitParams,
    },
    Trace {
        items: Vec<TraceItem>,
        spheres: Vec<(Vec3, f32, f32)>,
        /// The reached meshes, copied: the worker gathers their triangles.
        input: GatherInput,
    },
}

/// Per light of a traced emitter: the probe and open share, `None` when buried.
type Probes = Vec<Option<(Vec<f32>, f32)>>;

enum Done {
    Fit {
        content: ContentKey,
        lights: Vec<EmissiveLight>,
        ms: f64,
    },
    Trace {
        items: Vec<(u64, ContentKey, Probes)>,
        ms: f64,
    },
}

fn run(job: Job) -> Done {
    let started = Instant::now();
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
    match job {
        Job::Fit {
            content,
            geometry,
            indices,
            base,
            double_sided,
            map,
            params,
        } => {
            let _span = tracing::info_span!("emissive_fit_worker").entered();
            let emission = Emission {
                base,
                double_sided,
                map: map.as_ref(),
            };
            let lights = fit_emission(&geometry, &indices, &emission, &params);
            Done::Fit {
                content,
                lights,
                ms: ms(started),
            }
        }
        Job::Trace {
            items,
            spheres,
            input,
        } => {
            let _span = tracing::info_span!("emissive_probes_worker").entered();
            let bvh = Bvh::from_tris(input.gather().near(&spheres));
            let items = items
                .into_iter()
                .map(|item| {
                    let probes = item
                        .lights
                        .iter()
                        .zip(&item.reach)
                        .map(|(light, &reach)| light_probe(light, &bvh, reach))
                        .collect();
                    (item.key, item.content, probes)
                })
                .collect();
            Done::Trace {
                items,
                ms: ms(started),
            }
        }
    }
}

struct Worker {
    jobs: mpsc::Sender<Job>,
    /// Behind a mutex only so the renderer stays `Sync` (a receiver is not); one reader.
    done: std::sync::Mutex<mpsc::Receiver<Done>>,
}

impl Worker {
    fn spawn() -> Option<Self> {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (done_tx, done) = mpsc::channel::<Done>();
        // Registered (ADR-287) as CPU work: listed at exit, never waited for.
        let spawned = regia_platform::GpuWorkerBuilder::new("regia-emissive")
            .driver_calls_marked()
            .spawn(move |_| {
                // Ends when the cache (the sender) is dropped; the renderer's teardown never waits
                // for it.
                while let Ok(job) = job_rx.recv() {
                    if done_tx.send(run(job)).is_err() {
                        break;
                    }
                }
            });
        match spawned {
            Ok(_) => Some(Self {
                jobs,
                done: std::sync::Mutex::new(done),
            }),
            Err(err) => {
                tracing::warn!(
                    target: "regia_render",
                    "emissive fit worker did not start ({err}); fits stay on the frame thread"
                );
                None
            }
        }
    }
}

/// `REGIA_EMISSIVE_SYNC=1`: every fit and probe trace on the frame thread (v0.1.17 behaviour).
fn sync_env() -> bool {
    static SYNC: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SYNC.get_or_init(|| regia_core::brand::env_var("REGIA_EMISSIVE_SYNC").is_ok_and(|v| v == "1"))
}

/// Content keys, content-keyed fits, per-object probes and the background worker of an
/// [`super::EmissiveCache`].
#[derive(Default)]
pub(crate) struct FitQueue {
    idents: HashMap<u64, Ident>,
    fits: HashMap<ContentKey, FitSlot>,
    objects: HashMap<u64, Vec<ObjectSlot>>,
    /// Meshes the host lent for this frame, by key ([`Self::hold`]).
    pins: HashMap<u64, Weak<CpuMesh>>,
    /// Use the worker past the load frame ([`Self::set_background`]).
    background: bool,
    worker: Option<Worker>,
    worker_failed: bool,
    pending_fits: HashSet<ContentKey>,
    /// Keys whose probes the worker is tracing (one batch at a time).
    tracing: HashSet<u64>,
    pub(crate) stats: EmissiveFitStats,
    /// Fits run on the frame thread (`fits_done` also counts the worker's).
    pub(crate) frame_thread_fits: u64,
    /// Distance probes traced on the frame thread (`probes_traced` also counts the worker's).
    pub(crate) frame_thread_probes: u64,
    /// Candidate probe-occluder triangles the frame thread world-transformed and gathered (a
    /// background trace gathers on the worker, [`super::GatherInput`]).
    pub(crate) frame_thread_gathered: u64,
    /// Test hook: [`Self::land`] waits for the worker while it has work in flight, so a test
    /// steps frames by count instead of polling the clock (no runtime caller sets it).
    pub(crate) land_waits: bool,
    /// Complete frames (ADR-2582): nothing new goes to the worker, and each frame waits for the
    /// work already there, so the frame a fit lands on does not depend on the CPU's speed.
    pub(crate) complete: bool,
}

impl FitQueue {
    pub(crate) fn set_background(&mut self, on: bool) {
        self.background = on;
    }

    /// Whether new fits and traces go to the worker (once the cache is primed).
    pub(crate) fn background(&self) -> bool {
        self.background && !self.complete && !self.worker_failed && !sync_env()
    }

    /// Lends this frame's emissive meshes: a key first hashed while its mesh is lent keeps a
    /// `Weak` of it ([`Ident::weak`]). The pins are keyed like a [`crate::FrameMesh`]'s key at
    /// LOD 0 — the generation id [`crate::runtime::mesh_key`] gives the `Arc`, never its
    /// address and never reused by another `Arc`.
    pub(crate) fn hold<'a>(&mut self, meshes: impl IntoIterator<Item = &'a Arc<CpuMesh>>) {
        self.pins.clear();
        for mesh in meshes {
            self.pins
                .insert(crate::runtime::mesh_key(mesh), Arc::downgrade(mesh));
        }
    }

    /// The content key of `mesh` (keyed, emitting), hashing it when its key is new or no longer
    /// names the mesh it named. `None` when that hash waits for a later frame (the budget).
    pub(crate) fn content_key(
        &mut self,
        mesh: &crate::FrameMesh<'_>,
        signature: u64,
        frame: u64,
        budget: &mut FrameBudget,
    ) -> Option<ContentKey> {
        let with_uv = mesh.material.emissive_map.is_some();
        let key = |hash| ContentKey {
            hash,
            vertices: mesh.vertices.len(),
            indices: mesh.indices.len(),
            signature,
        };
        if let Some(id) = self.idents.get_mut(&mesh.mesh_key) {
            let same_buffers = id.vertices_at == mesh.vertices.as_ptr() as usize
                && id.vertices == mesh.vertices.len()
                && id.indices_at == mesh.indices.as_ptr() as usize
                && id.indices == mesh.indices.len()
                && id.with_uv == with_uv;
            // A lent mesh is trusted while its weak lives and names the same buffers. The
            // registry's weak (and this one) pin the allocation, and `Arc::get_mut` /
            // `Arc::try_unwrap` refuse while any weak lives, so the public buffers cannot
            // change in safe code; an upgrade that succeeds means the registry's weak is
            // alive too (one strong count per allocation).
            let held = id.weak.as_ref().is_some_and(|w| {
                w.upgrade().is_some_and(|m| {
                    m.vertices.as_ptr() == mesh.vertices.as_ptr()
                        && m.vertices.len() == mesh.vertices.len()
                        && m.indices.as_ptr() == mesh.indices.as_ptr()
                        && m.indices.len() == mesh.indices.len()
                })
            });
            // A key whose mesh was never lent keeps the documented fallback: same buffers,
            // same 32-sample fingerprint.
            let fingerprinted =
                id.weak.is_none() && id.sample == sample_fingerprint(mesh.vertices, mesh.indices);
            if same_buffers && (held || fingerprinted) {
                id.last_frame = frame;
                return Some(key(id.hash));
            }
        }
        if budget.exhausted() {
            self.stats.budget_holds += 1;
            return None;
        }
        budget.admit(Some(mesh.mesh_key));
        let hash = content_hash(mesh.vertices, mesh.indices, with_uv);
        self.stats.hashes += 1;
        let weak = self
            .pins
            .get(&mesh.mesh_key)
            .filter(|w| w.strong_count() > 0)
            .and_then(Weak::upgrade)
            .filter(|m| {
                m.vertices.as_ptr() == mesh.vertices.as_ptr()
                    && m.vertices.len() == mesh.vertices.len()
                    && m.indices.as_ptr() == mesh.indices.as_ptr()
                    && m.indices.len() == mesh.indices.len()
            })
            .map(|m| Arc::downgrade(&m));
        self.idents.insert(
            mesh.mesh_key,
            Ident {
                weak,
                vertices_at: mesh.vertices.as_ptr() as usize,
                vertices: mesh.vertices.len(),
                indices_at: mesh.indices.as_ptr() as usize,
                indices: mesh.indices.len(),
                with_uv,
                sample: sample_fingerprint(mesh.vertices, mesh.indices),
                hash,
                last_frame: frame,
            },
        );
        Some(key(hash))
    }

    /// The fitted lights of `content` (from `mesh`): cached, fitted here (no worker), or
    /// `None` while the worker fits them or the budget holds the copy back. `new_key`: the
    /// mesh key just took this content (a cache hit counts).
    pub(crate) fn lights(
        &mut self,
        content: ContentKey,
        mesh: &crate::FrameMesh<'_>,
        params: &FitParams,
        new_key: bool,
        frame: u64,
        budget: &mut FrameBudget,
    ) -> Option<Arc<Vec<EmissiveLight>>> {
        if let Some(slot) = self.fits.get_mut(&content) {
            slot.last_frame = frame;
            if new_key {
                self.stats.cache_hits += 1;
            }
            return Some(slot.lights.clone());
        }
        if self.pending_fits.contains(&content) {
            return None;
        }
        if !budget.background || self.worker().is_none() {
            let lights = Arc::new(super::fit_mesh(
                mesh.vertices,
                mesh.indices,
                mesh.material,
                params,
            ));
            self.stats.fits_done += 1;
            self.frame_thread_fits += 1;
            self.fits.insert(
                content,
                FitSlot {
                    lights: lights.clone(),
                    last_frame: frame,
                },
            );
            return Some(lights);
        }
        if budget.exhausted_for(mesh.mesh_key) {
            self.stats.budget_holds += 1;
            return None;
        }
        budget.admit(Some(mesh.mesh_key));
        let map = mesh.material.emissive_map.clone();
        let geometry = FitGeometry {
            positions: mesh.vertices.iter().map(|v| v.position).collect(),
            uvs: if map.is_some() {
                mesh.vertices.iter().map(|v| v.uv).collect()
            } else {
                Vec::new()
            },
        };
        let emission = Emission::of(mesh.material);
        let job = Job::Fit {
            content,
            geometry,
            indices: mesh.indices.to_vec(),
            base: emission.base,
            double_sided: emission.double_sided,
            map,
            params: *params,
        };
        if self.send(job) {
            self.pending_fits.insert(content);
            self.stats.fits_deferred += 1;
        }
        None
    }

    /// Probes of `object`'s earlier mesh with the same `content`, for a key that just took it.
    /// Object key 0 has no identity across frames: never shared.
    pub(crate) fn inherited_probes(&mut self, object: u64, content: ContentKey) -> Option<ProbeSet> {
        if object == 0 {
            return None;
        }
        let probes = self
            .objects
            .get(&object)?
            .iter()
            .find(|s| s.content == content)
            .map(|s| s.probes.clone());
        if probes.is_some() {
            self.stats.probe_hits += 1;
        }
        probes
    }

    /// The lights and probes `object` drew with its newest mesh no longer drawn (`drawn` holds
    /// this frame's emissive keys): the stand-in while a replacing mesh's own are pending.
    pub(crate) fn stand_in(
        &mut self,
        object: u64,
        drawn: &HashSet<u64>,
        frame: u64,
    ) -> Option<(Arc<Vec<EmissiveLight>>, ProbeSet)> {
        if object == 0 {
            return None;
        }
        let slot = self
            .objects
            .get(&object)?
            .iter()
            .filter(|s| !drawn.contains(&s.mesh_key) && frame.saturating_sub(s.last_frame) <= STAND_IN_FRAMES)
            .max_by_key(|s| s.last_frame)?;
        self.stats.stand_ins += 1;
        Some((slot.lights.clone(), slot.probes.clone()))
    }

    /// Records `object`'s mesh `mesh_key` drawn this frame with its own landed lights and probes.
    pub(crate) fn note_object(
        &mut self,
        object: u64,
        mesh_key: u64,
        content: ContentKey,
        lights: &Arc<Vec<EmissiveLight>>,
        probes: &ProbeSet,
        frame: u64,
    ) {
        if object == 0 {
            return;
        }
        let slots = self.objects.entry(object).or_default();
        if let Some(slot) = slots.iter_mut().find(|s| s.mesh_key == mesh_key) {
            slot.content = content;
            slot.lights = lights.clone();
            slot.probes = probes.clone();
            slot.last_frame = frame;
            return;
        }
        if slots.len() >= OBJECT_SLOTS
            && let Some(oldest) = slots
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.last_frame)
                .map(|(i, _)| i)
        {
            slots.swap_remove(oldest);
        }
        slots.push(ObjectSlot {
            mesh_key,
            content,
            lights: lights.clone(),
            probes: probes.clone(),
            last_frame: frame,
        });
    }

    /// Whether the worker is tracing `key`'s probes.
    pub(crate) fn is_tracing(&self, key: u64) -> bool {
        self.tracing.contains(&key)
    }

    /// Whether a probe batch is in flight (one at a time).
    pub(crate) fn trace_in_flight(&self) -> bool {
        !self.tracing.is_empty()
    }

    /// Sends a probe batch to the worker; `false` when it could not (trace it here instead).
    pub(crate) fn trace(
        &mut self,
        items: Vec<TraceItem>,
        spheres: Vec<(Vec3, f32, f32)>,
        input: GatherInput,
    ) -> bool {
        let keys: Vec<u64> = items.iter().map(|i| i.key).collect();
        if self.send(Job::Trace {
            items,
            spheres,
            input,
        }) {
            self.tracing.extend(keys);
            true
        } else {
            false
        }
    }

    /// Lands finished work: fits into the content cache, probes into the entries whose key
    /// still holds the traced content. Returns the lights traced.
    pub(crate) fn land(&mut self, entries: &mut HashMap<u64, CacheEntry>, frame: u64) -> u32 {
        let mut traced = self.land_once(entries, frame);
        // Complete frames wait for every piece in flight (a dead worker answers at once with
        // nothing, which ends the wait).
        while self.complete && self.in_flight() {
            let before = (self.pending_fits.len(), self.tracing.len());
            traced += self.land_once(entries, frame);
            if (self.pending_fits.len(), self.tracing.len()) == before {
                break;
            }
        }
        traced
    }

    fn in_flight(&self) -> bool {
        !self.pending_fits.is_empty() || !self.tracing.is_empty()
    }

    fn land_once(&mut self, entries: &mut HashMap<u64, CacheEntry>, frame: u64) -> u32 {
        let mut traced = 0u32;
        let Some(worker) = self.worker.as_ref() else {
            return 0;
        };
        let finished: Vec<Done> = {
            let done = worker
                .done
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // A worker that died drops its sender: `recv` then returns at once.
            let in_flight = !self.pending_fits.is_empty() || !self.tracing.is_empty();
            let first = if (self.land_waits || self.complete) && in_flight {
                done.recv().ok()
            } else {
                None
            };
            first.into_iter().chain(done.try_iter()).collect()
        };
        for done in finished {
            match done {
                Done::Fit { content, lights, ms } => {
                    self.pending_fits.remove(&content);
                    self.stats.fits_done += 1;
                    self.stats.worker_ms += ms;
                    self.fits.insert(
                        content,
                        FitSlot {
                            lights: Arc::new(lights),
                            last_frame: frame,
                        },
                    );
                }
                Done::Trace { items, ms } => {
                    self.stats.worker_ms += ms;
                    for (key, content, probes) in items {
                        self.tracing.remove(&key);
                        traced += probes.len() as u32;
                        if let Some(entry) = entries.get_mut(&key).filter(|e| e.content == content) {
                            entry.probes = Some(Arc::new(probes));
                        }
                    }
                }
            }
        }
        self.stats.probes_traced += u64::from(traced);
        traced
    }

    fn worker(&mut self) -> Option<&Worker> {
        if self.worker.is_none() && !self.worker_failed {
            self.worker = Worker::spawn();
            self.worker_failed = self.worker.is_none();
        }
        self.worker.as_ref()
    }

    fn send(&mut self, job: Job) -> bool {
        let sent = self.worker().is_some_and(|w| w.jobs.send(job).is_ok());
        if !sent {
            // The thread is gone (it cannot panic on its own; a send fails only without it):
            // the frame thread takes over.
            self.worker = None;
            self.worker_failed = true;
            self.pending_fits.clear();
            self.tracing.clear();
        }
        sent
    }

    /// Drops what has not been used for `keep` frames, and memos whose lent mesh is gone (which
    /// releases its allocation).
    pub(crate) fn prune(&mut self, frame: u64, keep: u64, max: usize) {
        self.idents.retain(|_, id| {
            frame.saturating_sub(id.last_frame) < keep
                && id.weak.as_ref().is_none_or(|w| w.strong_count() > 0)
        });
        self.fits.retain(|_, s| frame.saturating_sub(s.last_frame) < keep);
        if self.idents.len() > max {
            let mut ages: Vec<(u64, u64)> = self.idents.iter().map(|(k, i)| (i.last_frame, *k)).collect();
            ages.sort_unstable();
            for (_, key) in ages.iter().take(self.idents.len() - max) {
                self.idents.remove(key);
            }
        }
        if self.fits.len() > max {
            let mut ages: Vec<(u64, ContentKey)> =
                self.fits.iter().map(|(k, s)| (s.last_frame, *k)).collect();
            // A total order: sorting equal ages alone kept a permutation of the hash map's
            // iteration order (the process hash seed), so which fits a prune dropped differed
            // between runs of the same meshes.
            ages.sort_unstable_by_key(|(age, k)| (*age, k.hash, k.vertices, k.indices, k.signature));
            for (_, key) in ages.iter().take(self.fits.len() - max) {
                self.fits.remove(key);
            }
        }
        self.objects.retain(|_, slots| {
            slots.retain(|s| frame.saturating_sub(s.last_frame) < OBJECT_KEEP_FRAMES);
            !slots.is_empty()
        });
        self.stats.pending = (self.pending_fits.len() + usize::from(!self.tracing.is_empty())) as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab_frame::LabMaterial;

    fn vertex(p: [f32; 3]) -> LabVertex {
        LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], [0.25, 0.75])
    }

    fn emitting_material() -> LabMaterial {
        LabMaterial {
            emissive: [1.0, 1.0, 1.0],
            emissive_intensity: 100.0,
            ..LabMaterial::EMPTY
        }
    }

    fn frame_mesh<'a>(
        key: u64,
        vertices: &'a [LabVertex],
        indices: &'a [u32],
        material: &'a LabMaterial,
    ) -> crate::FrameMesh<'a> {
        crate::FrameMesh {
            is_static: false,
            mesh_key: key,
            material_key: key,
            object_key: 0,
            vertices,
            indices,
            material,
            model: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            joints: &[],
            receiver_layer: 0,
            shadow: crate::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }
    }

    fn strip(count: usize) -> (Vec<LabVertex>, Vec<u32>) {
        let vs: Vec<LabVertex> = (0..count).map(|i| vertex([i as f32, 0.5, -1.0])).collect();
        let is: Vec<u32> = (0..count.saturating_sub(2) as u32)
            .flat_map(|i| [i, i + 1, i + 2])
            .collect();
        (vs, is)
    }

    #[test]
    fn the_content_hash_sees_one_moved_vertex_and_ignores_everything_else() {
        let vs: Vec<LabVertex> = (0..100).map(|i| vertex([i as f32, 0.5, -1.0])).collect();
        let is: Vec<u32> = (0..98).flat_map(|i| [i, i + 1, i + 2]).collect();
        let h = content_hash(&vs, &is, false);
        // A copy elsewhere in memory hashes the same.
        assert_eq!(content_hash(&vs.clone(), &is.clone(), false), h);
        // Normals do not reach the fit.
        let mut normals = vs.clone();
        normals[7].normal = [1.0, 0.0, 0.0];
        assert_eq!(content_hash(&normals, &is, false), h);
        // One position, one index, the texture coordinates under an emissive map: all change it.
        let mut moved = vs.clone();
        moved[50].position[1] += 1e-3;
        assert_ne!(content_hash(&moved, &is, false), h);
        let mut swapped = is.clone();
        swapped.swap(10, 11);
        assert_ne!(content_hash(&vs, &swapped, false), h);
        let mut uv = vs.clone();
        uv[3].uv[1] = 0.5;
        assert_eq!(content_hash(&uv, &is, false), h);
        assert_ne!(content_hash(&uv, &is, true), content_hash(&vs, &is, true));
        // Vertex 50 is not one of the 32 samples of 100: the fingerprint misses that move, the
        // hash does not. Why an unlent key is only trusted while its buffers stay where they were
        // (a new mesh is hashed in full).
        assert_eq!(sample_fingerprint(&moved, &is), sample_fingerprint(&vs, &is));
        let mut first = vs.clone();
        first[0].position[0] += 1e-3;
        assert_ne!(sample_fingerprint(&first, &is), sample_fingerprint(&vs, &is));
    }

    /// Audit fix-render4 #4 (medium): the 1 ms wall-clock cutoff gates only the background
    /// (live) path. The complete path — what complete frames and offscreen captures take —
    /// is `background == false`, and a frame-thread budget with a clock already past the
    /// cutoff still admits every new mesh.
    #[test]
    fn a_frame_thread_budget_ignores_the_wall_clock_on_the_complete_path() {
        let started = Instant::now() - std::time::Duration::from_millis(50);
        // Complete frames and captures: the clock is never read.
        let mut budget = FrameBudget::new(started, false);
        for key in [1u64, 2, 3] {
            assert!(!budget.exhausted());
            budget.admit(Some(key));
        }
        assert!(
            !budget.exhausted(),
            "the complete path never holds new work, however long the frame has run"
        );
        // Live background path, unchanged: past the cutoff, work admitted earlier holds
        // later work (the first piece always starts, so the queue moves).
        let mut budget = FrameBudget::new(started, true);
        assert!(
            !budget.exhausted(),
            "nothing admitted yet: the first piece starts"
        );
        budget.admit(Some(1));
        assert!(budget.exhausted(), "the wall clock now holds the next piece");
        assert!(!budget.exhausted_for(1), "the piece that started finishes");
        assert!(budget.exhausted_for(2));
    }

    /// ADR-2582: complete frames send nothing to the worker, and a queue with nothing in flight
    /// lands without waiting.
    #[test]
    fn a_complete_queue_keeps_work_on_the_frame_thread() {
        let mut queue = FitQueue::default();
        queue.set_background(true);
        let background = queue.background();
        queue.complete = true;
        assert!(!queue.background());
        let mut entries = HashMap::new();
        assert_eq!(queue.land(&mut entries, 1), 0);
        queue.complete = false;
        assert_eq!(queue.background(), background);
    }

    #[test]
    fn stats_since_subtracts_the_counters_and_keeps_the_gauges() {
        let a = EmissiveFitStats {
            fits_done: 3,
            frame_ms: 1.5,
            pending: 9,
            ..EmissiveFitStats::default()
        };
        let b = EmissiveFitStats {
            fits_done: 5,
            frame_ms: 2.0,
            pending: 1,
            background: true,
            ..EmissiveFitStats::default()
        };
        let d = b.since(&a);
        assert_eq!((d.fits_done, d.pending, d.background), (2, 1, true));
        assert!((d.frame_ms - 0.5).abs() < 1e-9);
    }

    /// The lent weak must attach to the content memo: `hold` pins under the same key the
    /// frame's mesh carries, so a lent mesh is trusted by its weak — the allocation is pinned
    /// and `Arc::get_mut` refuses while any weak lives — instead of falling back to the
    /// 32-sample fingerprint that misses unsampled vertices.
    #[test]
    fn a_lent_mesh_attaches_its_weak_and_keeps_one_hash() {
        let (vs, is) = strip(100);
        let mut cpu = Arc::new(CpuMesh {
            vertices: vs,
            indices: is,
        });
        let key = crate::runtime::mesh_key(&cpu);
        let material = emitting_material();
        let mut queue = FitQueue::default();
        queue.hold([&cpu]);
        let mut budget = FrameBudget::new(Instant::now(), false);
        let first = queue
            .content_key(
                &frame_mesh(key, &cpu.vertices, &cpu.indices, &material),
                9,
                1,
                &mut budget,
            )
            .expect("the first hash starts");
        // The load-bearing claim of the trust (ADR-4783): with the registry's weak and the
        // memo's weak alive, even a sole owner's `Arc::get_mut` must refuse, so the public
        // buffers cannot change in safe code while the memo trusts the hash.
        assert!(
            Arc::get_mut(&mut cpu).is_none(),
            "the lent weaks must pin the buffers against Arc::get_mut"
        );
        let second = queue
            .content_key(
                &frame_mesh(key, &cpu.vertices, &cpu.indices, &material),
                9,
                2,
                &mut budget,
            )
            .expect("memoized");
        assert_eq!(first, second);
        assert_eq!(queue.stats.hashes, 1, "one hash over the lent mesh's life");
        assert!(
            queue.idents[&key]
                .weak
                .as_ref()
                .is_some_and(|w| w.strong_count() > 0),
            "the lent weak never attached to the memo (hold keys the pins differently from              content_key's lookup)"
        );
    }

    /// The lent-mesh trust engages only for a mesh drawn under its base generation key: the
    /// runtime draws a `MeshLodChain` under `lod_variant_key` with the level's own index
    /// buffer, so a chained draw (every LOD ≥ 1 by key, and LOD 0 by index buffer) keeps the
    /// documented 32-sample fingerprint fallback (review of fix-render8, finding 2, scoped by
    /// ADR-5451).
    #[test]
    fn a_chained_lod_draw_keeps_the_fingerprint_fallback() {
        let (vs, is) = strip(100);
        let cpu = Arc::new(CpuMesh {
            vertices: vs,
            indices: is,
        });
        let base = crate::runtime::mesh_key(&cpu);
        // LOD 1's index buffer: a different allocation, as `draw_indices` submits.
        let lod_indices: &[u32] = Box::leak(cpu.indices.clone().into_boxed_slice());
        let chained_key =
            crate::mesh_lod::lod_variant_key(base, crate::mesh_lod::mesh_lod_cache_key(1, false));
        assert_ne!(chained_key, base);
        let material = emitting_material();
        let mut queue = FitQueue::default();
        queue.hold([&cpu]);
        let mut budget = FrameBudget::new(Instant::now(), false);
        let mesh = frame_mesh(chained_key, &cpu.vertices, lod_indices, &material);
        let first = queue
            .content_key(&mesh, 9, 1, &mut budget)
            .expect("the first hash starts");
        let second = queue.content_key(&mesh, 9, 2, &mut budget).expect("memoized");
        assert_eq!(first, second);
        assert_eq!(queue.stats.hashes, 1, "the fingerprint fallback memoizes");
        assert!(
            queue.idents[&chained_key].weak.is_none(),
            "the base-keyed pin must not attach to a chained draw's key"
        );
    }

    /// The millisecond budget is a wall-clock cutoff for the interactive worker path: with one
    /// piece admitted and the clock past it, new work waits. The clock starts in the past, so
    /// the test needs no sleep and asserts no `elapsed()`.
    #[test]
    fn the_frame_budget_holds_new_work_past_its_millisecond() {
        let started = Instant::now() - std::time::Duration::from_millis(2);
        let mut budget = FrameBudget::new(started, true);
        assert!(
            !budget.exhausted(),
            "nothing admitted yet: the first piece of a frame always starts"
        );
        budget.admit(Some(1));
        assert!(budget.exhausted());
        assert!(budget.exhausted_for(2), "another mesh's piece waits");
        assert!(!budget.exhausted_for(1), "the admitted mesh continues");
        let mut sync = FrameBudget::new(started, false);
        sync.admit(Some(1));
        assert!(!sync.exhausted(), "never on the frame-thread path");
    }

    /// A complete frame (ADR-2582) keeps every fit on the frame thread: both meshes' lights are
    /// present on the same `collect_frame_lights` call whatever the clock says, and nothing
    /// defers to the worker. Driven through the collection itself — a test that builds its own
    /// `FrameBudget` bypasses the clock path it claims to pin (review of fix-render8,
    /// finding 4).
    #[test]
    fn a_complete_frame_fits_both_meshes_without_the_worker() {
        let material = emitting_material();
        let leak = |v: Vec<LabVertex>, i: Vec<u32>| -> (&'static [LabVertex], &'static [u32]) {
            (Box::leak(v.into_boxed_slice()), Box::leak(i.into_boxed_slice()))
        };
        let tri = |x: f32| {
            leak(
                vec![
                    vertex([x, 0.0, 0.0]),
                    vertex([x + 1.0, 0.0, 0.0]),
                    vertex([x, 1.0, 0.0]),
                ],
                vec![0, 1, 2],
            )
        };
        let (va, ia) = tri(0.0);
        let (vb, ib) = tri(10.0);
        let meshes = [
            frame_mesh(0x1000, va, ia, &material),
            frame_mesh(0x2000, vb, ib, &material),
        ];
        let view = crate::light_budget::LightView::new(
            [5.0, 2.0, 8.0],
            [5.0, 0.5, 0.0],
            1.0,
            16.0 / 9.0,
            0.1,
            100.0,
        );
        let mut cache = super::super::EmissiveCache::default();
        cache.set_background(true);
        cache.set_complete(true);
        assert!(
            !cache.queue.background(),
            "a complete queue sends nothing to the worker even with background on"
        );
        let (records, _, report) = super::super::collect_frame_lights(&mut cache, &meshes, &view, 8, 0.01);
        assert_eq!(report.meshes, 2, "{report:?}");
        assert!(
            records.len() >= 2,
            "both meshes' lights land on the same call: {report:?}"
        );
        assert_eq!(cache.queue.stats.fits_deferred, 0, "nothing went to the worker");
        assert!(
            cache.mesh_ranges().iter().all(|&r| r > 0.0),
            "each mesh uploaded a light: {:?}",
            cache.mesh_ranges()
        );
    }

    /// Eviction among fits of equal frame age must not follow `HashMap` iteration order (the
    /// process hash seed): the same content survives whatever order the fits went in.
    #[test]
    fn prune_drops_the_same_fits_whatever_order_they_went_in() {
        const MAX: usize = 8;
        let fill = |queue: &mut FitQueue, hashes: &[u64]| {
            for &hash in hashes {
                let content = ContentKey {
                    hash,
                    vertices: 3,
                    indices: 3,
                    signature: 0,
                };
                queue.fits.insert(
                    content,
                    FitSlot {
                        lights: Arc::new(Vec::new()),
                        last_frame: 100,
                    },
                );
            }
        };
        let live = |queue: &FitQueue| {
            let mut hashes: Vec<u64> = queue.fits.keys().map(|k| k.hash).collect();
            hashes.sort_unstable();
            hashes
        };
        let forward: Vec<u64> = (0..64).collect();
        let mut a = FitQueue::default();
        fill(&mut a, &forward);
        let mut b = FitQueue::default();
        fill(&mut b, &forward.iter().rev().copied().collect::<Vec<_>>());
        a.prune(100, u64::MAX, MAX);
        b.prune(100, u64::MAX, MAX);
        assert_eq!(a.fits.len(), MAX);
        assert_eq!(
            live(&a),
            live(&b),
            "equal-age eviction followed the hash map's iteration order"
        );
    }
}
