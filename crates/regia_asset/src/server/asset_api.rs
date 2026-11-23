// SPDX-License-Identifier: MPL-2.0
//! The generic asset API (`docs/specs/12-assets.md` §§ 2-3, ADR-7202).
//!
//! A crate registers an [`AssetLoader`] for its asset type; [`AssetServer::load`] (by id) and
//! [`AssetServer::load_path`] (through the path manifest) then return a reference-counted
//! [`StrongHandle`] at once and decode on the IO task pool, reading through the same mounted
//! paks and cooked cache as the typed loads ([`PayloadReader`]; no second IO stack). A loader
//! loads what it depends on through its [`LoadContext`]: the edges are recorded both ways
//! ([`AssetServer::dependencies_of`], [`AssetServer::dependents_of`]), so a hot reload of a
//! dependency reloads the assets that use it.
//!
//! Lifetime is count-driven: when the last [`StrongHandle`] of an asset drops, the asset unloads
//! after a grace period counted in frames ([`AssetServer::update`] calls, default
//! [`DEFAULT_UNLOAD_GRACE_FRAMES`]); taking a handle again inside the grace keeps it. Unloading
//! drops the value, and with it the handles it held to its dependencies, which then run their own
//! grace. A per-type memory budget ([`AssetServer::set_memory_budget`]) evicts released assets
//! early, oldest release first; an asset with a live handle is never evicted. A reference cycle
//! (A depends on B, B on A) keeps both alive, like any cycle of shared pointers;
//! [`AssetServer::reference_cycles`] and the `assets.handles` report list such cycles (ADR-7256).
//!
//! The built-in kinds (`CpuMesh`, `CookedTexture`, `CookedImage`, `CookedStandardMaterial`) have
//! loaders registered when the server opens (`asset_builtin`), so counted loads of them work like any
//! generic asset and store into the built-in slabs. A typed load (`load_mesh`, ...) pins its id
//! ([`AssetServer::is_pinned`]): its uncounted `Handle<T>` keeps the value, which then never unloads
//! by count and keeps its CPU copy.
//!
//! Load groups ([`AssetServer::load_group`]) are [`crate::LoadSets`] sets, so a loading screen
//! reads one progress source (`LoadSets::load_progress`, the agent's `assets.load_progress`)
//! for groups and hand-made sets alike. The server records [`AssetEvent`]s in each store once a
//! consumer turns them on ([`AssetServer::enable_events`]); the [`crate::AssetPlugin`] forwards
//! them as ECS messages.

use std::any::{Any, TypeId};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::marker::PhantomData;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use regia_core::tasks::Task;

use super::{AssetServer, AssetSource, PayloadReader};
use crate::cooked::{CookedImage, CookedTexture, CpuMesh};
use crate::cooked_header::{CookedHeader, decode_cooked_file};
use crate::id::AssetId;
use crate::load_set::{LoadKind, LoadSet, LoadSets};
use crate::loader::{AssetLoadError, AssetTypeTag};
use crate::material::CookedStandardMaterial;
use crate::storage::{AssetEvent, Assets, LoadState};

/// Frames an asset stays loaded after its last [`StrongHandle`] drops, unless
/// [`AssetServer::set_unload_grace_frames`] says otherwise: two seconds at 60 frames per second
/// (spec 12 § 2). Counted in [`AssetServer::update`] calls, so tests need no clock.
pub const DEFAULT_UNLOAD_GRACE_FRAMES: u32 = 120;

/// Decodes one asset type from its cooked payload (spec 12 § 3).
///
/// `load` runs on an IO pool thread; it receives the payload after the cooked header (the
/// server checked the header's type tag against [`Self::type_tag`]). Dependencies are loaded
/// through `ctx`, which returns their handles at once; the server starts those loads when this
/// one finishes, so they run in parallel.
pub trait AssetLoader: Send + Sync + 'static {
    /// The value this loader produces.
    type Asset: Send + Sync + 'static;

    /// The cooked type tag this loader reads.
    fn type_tag(&self) -> AssetTypeTag;

    /// Whether this loader reads payloads cooked with `tag`. Defaults to `tag ==`
    /// [`Self::type_tag`]; a loader that reads several cooked forms (the built-in mesh loader:
    /// compact, GPU-ready and legacy meshes, ADR-7256) lists them here.
    fn accepts(&self, tag: AssetTypeTag) -> bool {
        tag == self.type_tag()
    }

    /// Decodes `payload`.
    ///
    /// # Errors
    ///
    /// Any [`AssetLoadError`]; the asset's state becomes [`LoadState::Failed`] (a reload keeps
    /// the previous value).
    fn load(&self, payload: &[u8], ctx: &mut LoadContext<'_>) -> Result<Self::Asset, AssetLoadError>;

    /// The CPU memory a value holds, for the per-type budget and `assets.handles`. Defaults to 0.
    fn cpu_bytes(&self, asset: &Self::Asset) -> u64 {
        let _ = asset;
        0
    }

    /// The kind name the agent reports and filters by. Defaults to the asset type's name without
    /// its module path.
    fn kind(&self) -> &'static str {
        short_type_name::<Self::Asset>()
    }

    /// Drops the CPU data a [`PrepareAsset`] uploaded, in place (spec 12 § 3: the CPU copy goes
    /// after the upload unless the load asked for [`LoadSettings::keep_cpu_data`]). Keep the
    /// dependency handles: the GPU copy may use them. Defaults to keeping everything.
    fn release_cpu_data(&self, asset: &mut Self::Asset) {
        let _ = asset;
    }
}

/// The second step of a GPU-backed asset (spec 12 § 3): called once for every load and reload of
/// a value of [`Self::Asset`], on the thread that runs [`AssetServer::update`] (the main thread,
/// `PreUpdate`), before the value is stored. A renderer queues its upload here. Afterwards the CPU
/// copy is released ([`AssetLoader::release_cpu_data`]) unless the load asked for
/// [`LoadSettings::keep_cpu_data`].
pub trait PrepareAsset: Send + Sync + 'static {
    /// The asset type this prepares.
    type Asset: Send + Sync + 'static;

    /// Prepares `asset`; returns its GPU memory estimate in bytes.
    ///
    /// # Errors
    ///
    /// A message: the load fails with it (a reload keeps the previous value).
    fn prepare(&self, id: AssetId, asset: &Self::Asset) -> Result<u64, String>;

    /// Asset `id` left memory: its grace ran out, a budget evicted it, or its slab dropped it.
    /// Free the GPU copy [`Self::prepare`] made. Called once per unload, on the thread that runs
    /// [`AssetServer::update`]; a reload calls [`Self::prepare`] again without an unload first,
    /// so a preparer replaces its copy there. Defaults to doing nothing (ADR-7256).
    fn unload(&self, id: AssetId) {
        let _ = id;
    }
}

/// Options of [`AssetServer::load_with`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadSettings {
    /// Keep the CPU copy after [`PrepareAsset`] (physics trimeshes, picking). Once an asset was
    /// loaded with it, it stays kept; asking for it on a loaded asset whose CPU copy was released
    /// reads the asset again.
    pub keep_cpu_data: bool,
}

/// `std::any::type_name` without the module path (and without generic arguments).
fn short_type_name<T: ?Sized>() -> &'static str {
    let full = std::any::type_name::<T>();
    let base = full.split('<').next().unwrap_or(full);
    base.rsplit("::").next().unwrap_or(base)
}

/// What a loader sees while it decodes one asset: the asset's id and cooked header, and a way to
/// load what it depends on.
pub struct LoadContext<'a> {
    id: AssetId,
    header: &'a CookedHeader,
    payload: &'a crate::pak::PakPayload,
    handles: &'a HandleTable,
    loaders: &'a LoaderMap,
    deps: Vec<(AssetId, TypeId)>,
}

impl LoadContext<'_> {
    /// The id being loaded.
    #[must_use]
    pub fn id(&self) -> AssetId {
        self.id
    }

    /// The cooked header of the asset being loaded (its `dependencies` list what the cooker saw).
    #[must_use]
    pub fn header(&self) -> &CookedHeader {
        self.header
    }

    /// The payload `load` receives, as shared bytes: a view of the mapped pak entry when the pak
    /// serves views (no copy), else of the owned buffer the file was read into. Cloning or
    /// slicing it shares the owner, so a loader can keep large bytes (KTX2 levels) without
    /// copying them out of the pak (ADR-7284).
    #[must_use]
    pub fn payload_view(&self) -> crate::pak::PakPayload {
        self.payload.clone()
    }

    /// A strong handle to dependency `id` of type `D`. The edge is recorded, and the dependency
    /// loads when this asset finishes (at once if it is loaded already). Asking twice for the same
    /// id records one edge.
    ///
    /// # Errors
    ///
    /// [`AssetLoadError::NoLoader`] when no loader is registered for `D`;
    /// [`AssetLoadError::Invalid`] when `id` is the asset itself or is held as another type.
    pub fn load<D: Send + Sync + 'static>(&mut self, id: AssetId) -> Result<StrongHandle<D>, AssetLoadError> {
        let type_id = TypeId::of::<D>();
        if !self.loaders.contains_key(&type_id) {
            return Err(AssetLoadError::NoLoader(std::any::type_name::<D>().to_owned()));
        }
        if id == self.id {
            return Err(AssetLoadError::Invalid(format!(
                "asset {id} names itself as a dependency"
            )));
        }
        let inner = self.handles.strong(id, type_id, std::any::type_name::<D>())?;
        if !self.deps.iter().any(|(d, _)| *d == id) {
            self.deps.push((id, type_id));
        }
        Ok(StrongHandle::from_inner(inner))
    }

    /// The dependencies asked for so far, in order.
    pub fn dependencies(&self) -> impl Iterator<Item = AssetId> + '_ {
        self.deps.iter().map(|(id, _)| *id)
    }
}

/// The shared part of every handle of one asset. Its drop (the last strong handle going away)
/// tells the server through the release channel.
struct HandleInner {
    id: AssetId,
    /// The value type the count is held for. A hot reload that changes a built-in texture into an
    /// image (or back) retypes it ([`HandleTable::retype`]), so the one count follows the asset.
    type_id: Mutex<TypeId>,
    releases: Sender<AssetId>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        // The server may be gone (shutdown): nothing to tell then.
        let _ = self.releases.send(self.id);
    }
}

/// A reference-counted runtime handle to a generic asset (spec 12 § 2). Cloning adds a reference;
/// when the last one drops, the asset unloads after the grace period. Resolve it with
/// [`AssetServer::get`].
pub struct StrongHandle<T> {
    inner: Arc<HandleInner>,
    _ty: PhantomData<fn() -> T>,
}

impl<T> StrongHandle<T> {
    fn from_inner(inner: Arc<HandleInner>) -> Self {
        Self {
            inner,
            _ty: PhantomData,
        }
    }

    /// The asset's id.
    #[must_use]
    pub fn id(&self) -> AssetId {
        self.inner.id
    }

    /// A weak handle: it does not keep the asset loaded.
    #[must_use]
    pub fn downgrade(&self) -> WeakAssetHandle<T> {
        WeakAssetHandle {
            inner: Arc::downgrade(&self.inner),
            id: self.inner.id,
            _ty: PhantomData,
        }
    }
}

impl<T> Clone for StrongHandle<T> {
    fn clone(&self) -> Self {
        Self::from_inner(Arc::clone(&self.inner))
    }
}

impl<T> PartialEq for StrongHandle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.inner.id == other.inner.id
    }
}

impl<T> Eq for StrongHandle<T> {}

impl<T> std::hash::Hash for StrongHandle<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.inner.id.hash(state);
    }
}

impl<T> std::fmt::Debug for StrongHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StrongHandle<{}>({})", short_type_name::<T>(), self.inner.id)
    }
}

/// A handle that does not keep its asset loaded. [`Self::upgrade`] works while a strong handle
/// exists; [`AssetServer::upgrade`] also inside the grace period after the last one dropped.
pub struct WeakAssetHandle<T> {
    inner: Weak<HandleInner>,
    id: AssetId,
    _ty: PhantomData<fn() -> T>,
}

impl<T> WeakAssetHandle<T> {
    /// The asset's id.
    #[must_use]
    pub fn id(&self) -> AssetId {
        self.id
    }

    /// A strong handle while another strong handle exists.
    #[must_use]
    pub fn upgrade(&self) -> Option<StrongHandle<T>> {
        self.inner.upgrade().map(StrongHandle::from_inner)
    }
}

impl<T> Clone for WeakAssetHandle<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Weak::clone(&self.inner),
            id: self.id,
            _ty: PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for WeakAssetHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WeakAssetHandle<{}>({})", short_type_name::<T>(), self.id)
    }
}

/// Every live handle by id, shared with the load tasks (a loader takes dependency handles on its
/// own thread).
struct HandleTable {
    live: Mutex<HashMap<AssetId, Weak<HandleInner>>>,
    releases: Mutex<Sender<AssetId>>,
}

impl HandleTable {
    /// The live strong handle of `id`, or a new one.
    fn strong(
        &self,
        id: AssetId,
        type_id: TypeId,
        type_name: &str,
    ) -> Result<Arc<HandleInner>, AssetLoadError> {
        let mut live = self.live.lock();
        if let Some(inner) = live.get(&id).and_then(Weak::upgrade) {
            if *inner.type_id.lock() != type_id {
                return Err(AssetLoadError::Invalid(format!(
                    "asset {id} is held as another type than {type_name}"
                )));
            }
            return Ok(inner);
        }
        let inner = Arc::new(HandleInner {
            id,
            type_id: Mutex::new(type_id),
            releases: self.releases.lock().clone(),
        });
        live.insert(id, Arc::downgrade(&inner));
        Ok(inner)
    }

    fn strong_count(&self, id: AssetId) -> usize {
        self.live.lock().get(&id).map_or(0, Weak::strong_count)
    }

    /// Holds the live count of `id` for `type_id` from now on (a kind change on hot reload).
    fn retype(&self, id: AssetId, type_id: TypeId) {
        if let Some(inner) = self.live.lock().get(&id).and_then(Weak::upgrade) {
            *inner.type_id.lock() = type_id;
        }
    }

    /// Forgets the entry of `id` when no strong handle is left; true when one is still live.
    fn prune(&self, id: AssetId) -> bool {
        let mut live = self.live.lock();
        let alive = live.get(&id).is_some_and(|w| w.strong_count() > 0);
        if !alive {
            live.remove(&id);
        }
        alive
    }
}

/// A loader with its asset type erased.
trait ErasedLoader: Send + Sync {
    fn type_tag(&self) -> AssetTypeTag;
    fn accepts(&self, tag: AssetTypeTag) -> bool;
    fn kind(&self) -> &'static str;
    fn load(&self, payload: &[u8], ctx: &mut LoadContext<'_>) -> Result<(BoxedAsset, u64), AssetLoadError>;
    /// Releases the CPU data of `asset` and returns its CPU bytes afterwards.
    fn release(&self, asset: &mut dyn Any) -> u64;
    /// The CPU bytes of `asset` (0 for another type).
    fn measure(&self, asset: &dyn Any) -> u64;
}

/// A preparer with its asset type erased.
trait ErasedPrepare: Send + Sync {
    fn prepare(&self, id: AssetId, asset: &dyn Any) -> Result<u64, String>;
    fn unload(&self, id: AssetId);
}

struct PrepareBox<P>(P);

impl<P: PrepareAsset> ErasedPrepare for PrepareBox<P> {
    fn prepare(&self, id: AssetId, asset: &dyn Any) -> Result<u64, String> {
        match asset.downcast_ref::<P::Asset>() {
            Some(asset) => self.0.prepare(id, asset),
            None => Err("prepared value is not the preparer's asset type".into()),
        }
    }

    fn unload(&self, id: AssetId) {
        self.0.unload(id);
    }
}

type BoxedAsset = Box<dyn Any + Send + Sync>;
type LoaderMap = HashMap<TypeId, Arc<dyn ErasedLoader>>;

struct LoaderBox<L>(L);

impl<L: AssetLoader> ErasedLoader for LoaderBox<L> {
    fn type_tag(&self) -> AssetTypeTag {
        self.0.type_tag()
    }

    fn accepts(&self, tag: AssetTypeTag) -> bool {
        self.0.accepts(tag)
    }

    fn kind(&self) -> &'static str {
        self.0.kind()
    }

    fn measure(&self, asset: &dyn Any) -> u64 {
        asset
            .downcast_ref::<L::Asset>()
            .map_or(0, |a| self.0.cpu_bytes(a))
    }

    fn load(&self, payload: &[u8], ctx: &mut LoadContext<'_>) -> Result<(BoxedAsset, u64), AssetLoadError> {
        let asset = self.0.load(payload, ctx)?;
        let bytes = self.0.cpu_bytes(&asset);
        Ok((Box::new(asset), bytes))
    }

    fn release(&self, asset: &mut dyn Any) -> u64 {
        match asset.downcast_mut::<L::Asset>() {
            Some(asset) => {
                self.0.release_cpu_data(asset);
                self.0.cpu_bytes(asset)
            }
            None => 0,
        }
    }
}

/// One generic store with its value type erased.
trait ErasedStore: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    /// False when the value is not this store's type (never expected: the loader map and the
    /// store map share their keys).
    fn insert_boxed(&mut self, id: AssetId, value: BoxedAsset) -> bool;
    fn remove(&mut self, id: AssetId) -> bool;
    fn push_failed(&mut self, id: AssetId, error: String);
    /// The stored value of `id`.
    fn value_of(&self, id: AssetId) -> Option<&dyn Any>;
    /// The stored ids, in slot order.
    fn ids(&self) -> Vec<AssetId>;
}

impl<T: Send + Sync + 'static> ErasedStore for Assets<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn insert_boxed(&mut self, id: AssetId, value: BoxedAsset) -> bool {
        match value.downcast::<T>() {
            Ok(value) => {
                self.insert(id, *value);
                true
            }
            Err(_) => false,
        }
    }

    fn remove(&mut self, id: AssetId) -> bool {
        Assets::remove(self, id)
    }

    fn push_failed(&mut self, id: AssetId, error: String) {
        self.push_event(AssetEvent::LoadFailed { id, error });
    }

    fn value_of(&self, id: AssetId) -> Option<&dyn Any> {
        let slot = self.handle_for_id(id)?;
        self.get(&slot).map(|v| v as &dyn Any)
    }

    fn ids(&self) -> Vec<AssetId> {
        self.entries_from(0).map(|(id, _)| id).collect()
    }
}

/// What the server knows about one loaded generic asset.
struct LoadedMeta {
    type_id: TypeId,
    deps: Vec<AssetId>,
    bytes: u64,
    /// GPU estimate from [`PrepareAsset::prepare`] (0 without a preparer).
    gpu_bytes: u64,
    /// False once [`AssetLoader::release_cpu_data`] ran on the stored value.
    cpu_kept: bool,
}

/// A released asset counting down its grace.
struct PendingUnload {
    remaining: u32,
    /// Release order (budget eviction takes the oldest first).
    seq: u64,
}

/// One load in flight.
struct InFlight {
    id: AssetId,
    task: Task<LoadOutcome>,
}

/// What a load task hands back.
struct LoadOutcome {
    id: AssetId,
    type_id: TypeId,
    result: Result<Decoded, AssetLoadError>,
    /// The read and decode times and bytes of the load (P-314; `read_ns` 0 when the caller read).
    timing: JobTiming,
}

/// A load job's read and decode times, ns, the cooked bytes read, its source and kind.
#[derive(Clone, Copy, Default)]
struct JobTiming {
    read_ns: u64,
    decode_ns: u64,
    bytes: u64,
    source: &'static str,
    kind: &'static str,
}

struct Decoded {
    value: BoxedAsset,
    bytes: u64,
    deps: Vec<(AssetId, TypeId)>,
    source: AssetSource,
}

/// Everything one load needs on its worker thread.
struct LoadJob {
    id: AssetId,
    type_id: TypeId,
    loader: Arc<dyn ErasedLoader>,
    loaders: Arc<LoaderMap>,
    handles: Arc<HandleTable>,
    reader: PayloadReader,
}

/// Reads, checks and decodes one asset (on a pool thread).
fn run_job(job: LoadJob) -> LoadOutcome {
    let started = std::time::Instant::now();
    let read = job.reader.read(job.id);
    let read_ns = crate::load_times::since(started);
    let mut outcome = decode_job(job, read);
    outcome.timing.read_ns = read_ns;
    outcome
}

/// Checks and decodes one asset from its read cooked file (`run_job`, or a hot reload that read
/// the file itself to see its kind). A built-in kind records its load form in the server's
/// [`AssetServer::load_forms`], as the typed loads do (ADR-7284).
fn decode_job(
    job: LoadJob,
    read: Result<(crate::pak::PakPayload, AssetSource), AssetLoadError>,
) -> LoadOutcome {
    let _span = tracing::info_span!("asset_load", kind = job.loader.kind()).entered();
    let started = std::time::Instant::now();
    let mut timing = JobTiming {
        kind: job.loader.kind(),
        ..JobTiming::default()
    };
    if let Ok((bytes, source)) = &read {
        timing.bytes = bytes.len() as u64;
        timing.source = match source {
            AssetSource::Pak(_) => "pak",
            AssetSource::DevCache => "cache",
            AssetSource::Unknown => "",
        };
    }
    let result = (|| -> Result<Decoded, AssetLoadError> {
        let (bytes, source) = read?;
        let (header, payload) = decode_cooked_file(&bytes)?;
        let expected = job.loader.type_tag();
        if !job.loader.accepts(AssetTypeTag(header.type_tag)) {
            return Err(AssetLoadError::TypeMismatch {
                id: job.id,
                found: header.type_tag.get(),
                expected: expected.0.get(),
            });
        }
        // The payload as shared bytes: a view of the pak mapping when the pak serves views.
        let shared = bytes
            .subslice(payload)
            .unwrap_or_else(|| crate::pak::PakPayload::from(payload));
        let mut ctx = LoadContext {
            id: job.id,
            header: &header,
            payload: &shared,
            handles: &job.handles,
            loaders: &job.loaders,
            deps: Vec::new(),
        };
        let (value, bytes) = job.loader.load(payload, &mut ctx)?;
        note_builtin_form(&job.reader, job.type_id, &header, &source);
        Ok(Decoded {
            value,
            bytes,
            deps: ctx.deps,
            source,
        })
    })();
    timing.decode_ns = crate::load_times::since(started);
    LoadOutcome {
        id: job.id,
        type_id: job.type_id,
        result,
        timing,
    }
}

/// The built-in kind a counted texture or image takes when its cooked file now holds the other
/// one (`None`: no change).
fn switched_builtin_kind(current: TypeId, tag: AssetTypeTag) -> Option<TypeId> {
    if current == TypeId::of::<CookedTexture>() && tag == AssetTypeTag::image() {
        Some(TypeId::of::<CookedImage>())
    } else if current == TypeId::of::<CookedImage>() && tag == AssetTypeTag::texture() {
        Some(TypeId::of::<CookedTexture>())
    } else {
        None
    }
}

/// Counts a decoded built-in value in the load forms (mesh form, KTX2 texture, RGBA8 image, pak
/// or cache read), exactly as `load_mesh`, `load_texture` and `load_image` count theirs. Other
/// kinds (materials, game types) are not counted, as the typed material load is not.
fn note_builtin_form(reader: &PayloadReader, type_id: TypeId, header: &CookedHeader, source: &AssetSource) {
    if type_id == TypeId::of::<CpuMesh>() {
        if let Some(form) = crate::cooked_pack::MeshForm::of(header.type_tag) {
            reader.note_mesh_form(form, source);
        }
    } else if type_id == TypeId::of::<CookedTexture>() {
        reader.note_texture(source);
    } else if type_id == TypeId::of::<CookedImage>() {
        reader.note_image(source);
    }
}

/// A load group's record in the server.
struct GroupRecord {
    name: String,
    /// Members, without duplicates.
    ids: Vec<AssetId>,
    holder: Weak<GroupInner>,
    complete: bool,
}

struct GroupInner {
    name: String,
    _handles: Vec<Arc<HandleInner>>,
}

/// A handle to a load group ([`AssetServer::load_group`]): it keeps every member loaded, and its
/// progress is a [`LoadSets`] set of the same name. Dropping the last clone forgets the set.
#[derive(Clone)]
pub struct LoadGroupHandle {
    inner: Arc<GroupInner>,
}

impl LoadGroupHandle {
    /// The group's name: the [`LoadSets`] set its progress is read from (`load_group:<n>`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }
}

impl std::fmt::Debug for LoadGroupHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LoadGroupHandle({})", self.inner.name)
    }
}

/// Progress of a load group.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadGroupProgress {
    /// The group's set name.
    pub name: String,
    /// Members loaded.
    pub loaded: u32,
    /// Members whose load failed.
    pub failed: u32,
    /// Members.
    pub total: u32,
    /// `0..=1`, from [`LoadSets::load_progress`] (failed members count as not loaded).
    pub fraction: f32,
    /// Every member loaded or failed. Set once and kept.
    pub complete: bool,
    /// The failures (`other:<id>: <error>`).
    pub failures: Vec<String>,
}

/// One kind's line of [`AssetServer::handle_report`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindHandles {
    /// The loader's kind name ([`AssetLoader::kind`]).
    pub kind: String,
    /// Loaded assets.
    pub loaded: usize,
    /// Loads in flight.
    pub loading: usize,
    /// Strong handles over every asset of the kind.
    pub strong_handles: usize,
    /// Loaded assets with no strong handle, counting down their grace.
    pub pending_unloads: usize,
    /// Loaded assets a typed load (`load_mesh`, `load_texture`, ...) pinned: their uncounted
    /// `Handle<T>` keeps them loaded, so counted handles never unload them (ADR-7256).
    pub pinned: usize,
    /// CPU bytes of the loaded assets ([`AssetLoader::cpu_bytes`]).
    pub cpu_bytes: u64,
    /// GPU estimate of the loaded assets ([`PrepareAsset::prepare`]).
    pub gpu_bytes: u64,
    /// The kind's memory budget, if one is set. It is held against CPU plus GPU bytes
    /// ([`AssetServer::budgeted_memory_use`]).
    pub budget: Option<u64>,
}

/// One asset's line of [`AssetServer::handle_report`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandleRow {
    /// Asset id.
    pub id: AssetId,
    /// Kind name.
    pub kind: String,
    /// Live strong handles.
    pub strong: usize,
    /// Frames left before the unload, when no strong handle is left.
    pub unload_in_frames: Option<u32>,
    /// CPU bytes.
    pub cpu_bytes: u64,
    /// GPU estimate.
    pub gpu_bytes: u64,
    /// Whether the CPU copy is kept (false once released after [`PrepareAsset`]).
    pub cpu_data_kept: bool,
    /// Dependencies loaded through its context.
    pub dependencies: usize,
    /// Pinned by a typed load (see [`KindHandles::pinned`]): never unloaded by count.
    pub pinned: bool,
}

/// The `assets.handles` answer: reference counts, pending unloads and load groups.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HandleReport {
    /// Unload grace in frames.
    pub grace_frames: u32,
    /// Per kind, in kind-name order. A built-in kind (`mesh`, `texture`, `image`, `material`) is
    /// listed only while it holds or loads something.
    pub kinds: Vec<KindHandles>,
    /// Loaded assets, by kind name then id.
    pub assets: Vec<HandleRow>,
    /// Every live load group, oldest first (not filtered by kind).
    pub groups: Vec<LoadGroupProgress>,
    /// Reference cycles among loaded assets ([`AssetServer::reference_cycles`]): each keeps its
    /// members loaded after every outside handle drops. A warning, not filtered by kind.
    pub cycles: Vec<Vec<AssetId>>,
}

/// The generic-asset state of an [`AssetServer`].
pub(crate) struct GenericAssets {
    loaders: Arc<LoaderMap>,
    preparers: HashMap<TypeId, Arc<dyn ErasedPrepare>>,
    /// Ids loaded with [`LoadSettings::keep_cpu_data`].
    keep_cpu: HashSet<AssetId>,
    /// Built-in ids a typed load pinned: never unloaded by count, CPU copy kept (ADR-7256).
    pinned: HashSet<AssetId>,
    stores: HashMap<TypeId, Box<dyn ErasedStore>>,
    handles: Arc<HandleTable>,
    releases: Mutex<Receiver<AssetId>>,
    loaded: HashMap<AssetId, LoadedMeta>,
    dependents: HashMap<AssetId, BTreeSet<AssetId>>,
    in_flight: VecDeque<InFlight>,
    pending: HashMap<AssetId, PendingUnload>,
    grace: u32,
    release_seq: u64,
    loads_started: u64,
    groups: Vec<GroupRecord>,
    next_group: u64,
    completed_groups: Vec<String>,
    load_sets: LoadSets,
    budgets: HashMap<TypeId, u64>,
    budget_evictions: Vec<AssetId>,
    reloaded_dependents: Vec<AssetId>,
}

impl Default for GenericAssets {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            loaders: Arc::default(),
            preparers: HashMap::new(),
            keep_cpu: HashSet::new(),
            pinned: HashSet::new(),
            stores: HashMap::new(),
            handles: Arc::new(HandleTable {
                live: Mutex::new(HashMap::new()),
                releases: Mutex::new(tx),
            }),
            releases: Mutex::new(rx),
            loaded: HashMap::new(),
            dependents: HashMap::new(),
            in_flight: VecDeque::new(),
            pending: HashMap::new(),
            grace: DEFAULT_UNLOAD_GRACE_FRAMES,
            release_seq: 0,
            loads_started: 0,
            groups: Vec::new(),
            next_group: 0,
            completed_groups: Vec::new(),
            load_sets: LoadSets::default(),
            budgets: HashMap::new(),
            budget_evictions: Vec::new(),
            reloaded_dependents: Vec::new(),
        }
    }
}

impl GenericAssets {
    fn kind_of(&self, type_id: TypeId) -> &'static str {
        self.loaders.get(&type_id).map_or("unknown", |l| l.kind())
    }

    fn is_in_flight(&self, id: AssetId) -> bool {
        self.in_flight.iter().any(|f| f.id == id)
    }

    /// Whether a live load group lists `id` (only those are tracked in [`LoadSets`]).
    fn is_grouped(&self, id: AssetId) -> bool {
        self.groups.iter().any(|g| g.ids.contains(&id))
    }

    /// Records the edges of `id` (replacing the old ones).
    fn set_edges(&mut self, id: AssetId, deps: &[AssetId]) {
        if let Some(old) = self.loaded.get(&id) {
            for dep in &old.deps {
                if let Some(set) = self.dependents.get_mut(dep) {
                    set.remove(&id);
                    if set.is_empty() {
                        self.dependents.remove(dep);
                    }
                }
            }
        }
        for dep in deps {
            self.dependents.entry(*dep).or_default().insert(id);
        }
    }
}

fn group_item_key(id: AssetId) -> String {
    id.to_string()
}

impl AssetServer {
    /// Registers the loader for `L::Asset` (replacing an earlier one for that type).
    pub fn register_loader<L: AssetLoader>(&mut self, loader: L) {
        let type_id = TypeId::of::<L::Asset>();
        let mut loaders: LoaderMap = (*self.generic.loaders).clone();
        loaders.insert(type_id, Arc::new(LoaderBox(loader)));
        self.generic.loaders = Arc::new(loaders);
        // A built-in kind's values go to its slab (`meshes`, `textures`, ...), not a new store.
        if self.builtin_store(type_id).is_none() {
            self.generic
                .stores
                .entry(type_id)
                .or_insert_with(|| Box::new(Assets::<L::Asset>::new()));
        }
    }

    /// The store of `type_id` with its value type erased: a built-in slab or a registered one.
    fn erased_store(&self, type_id: TypeId) -> Option<&dyn ErasedStore> {
        if type_id == TypeId::of::<CpuMesh>() {
            Some(&self.meshes)
        } else if type_id == TypeId::of::<CookedTexture>() {
            Some(&self.textures)
        } else if type_id == TypeId::of::<CookedImage>() {
            Some(&self.images)
        } else if type_id == TypeId::of::<CookedStandardMaterial>() {
            Some(&self.materials)
        } else {
            self.generic.stores.get(&type_id).map(|s| &**s)
        }
    }

    /// [`Self::erased_store`], mutable.
    fn erased_store_mut(&mut self, type_id: TypeId) -> Option<&mut dyn ErasedStore> {
        if type_id == TypeId::of::<CpuMesh>() {
            Some(&mut self.meshes)
        } else if type_id == TypeId::of::<CookedTexture>() {
            Some(&mut self.textures)
        } else if type_id == TypeId::of::<CookedImage>() {
            Some(&mut self.images)
        } else if type_id == TypeId::of::<CookedStandardMaterial>() {
            Some(&mut self.materials)
        } else {
            match self.generic.stores.get_mut(&type_id) {
                Some(store) => Some(&mut **store),
                None => None,
            }
        }
    }

    /// Keeps the slab-handle → id maps of the typed API (`mesh_state`, ...) in step with a
    /// counted built-in insert (`present`) or unload. Textures and images keep no such map (the
    /// v0.5 line removed the write-only image map, ADR-7413), so they have nothing to update.
    fn note_builtin_slot(&mut self, type_id: TypeId, id: AssetId, present: bool) {
        let (map, slot) = if type_id == TypeId::of::<CpuMesh>() {
            (
                &mut self.mesh_id_by_handle,
                self.meshes.handle_for_id(id).map(|h| h.index),
            )
        } else if type_id == TypeId::of::<CookedStandardMaterial>() {
            (
                &mut self.material_id_by_handle,
                self.materials.handle_for_id(id).map(|h| h.index),
            )
        } else {
            return;
        };
        match (present, slot) {
            (true, Some(index)) => {
                map.insert(index, id);
            }
            (false, _) => map.retain(|_, v| *v != id),
            (true, None) => {}
        }
    }

    /// A typed load (`load_mesh`, `load_texture`, `load_image`, `load_material`) of `id`
    /// succeeded: pin it, so its uncounted `Handle<T>` stays valid, and keep its CPU copy
    /// (ADR-7256). A counted load of it already stored refreshes its byte count.
    pub(super) fn pin_builtin(&mut self, id: AssetId, type_id: TypeId) {
        self.generic.pinned.insert(id);
        self.generic.pending.remove(&id);
        let bytes = self.measure_stored(id, type_id);
        if let Some(meta) = self.generic.loaded.get_mut(&id) {
            meta.cpu_kept = true;
            meta.bytes = bytes;
        }
    }

    /// The CPU bytes of the stored value of `id` (0 when not stored or the type has no loader).
    fn measure_stored(&self, id: AssetId, type_id: TypeId) -> u64 {
        let Some(loader) = self.generic.loaders.get(&type_id) else {
            return 0;
        };
        self.erased_store(type_id)
            .and_then(|s| s.value_of(id))
            .map_or(0, |v| loader.measure(v))
    }

    /// Whether a typed load pinned `id` (ADR-7256): counted handles never unload it.
    #[must_use]
    pub fn is_pinned(&self, id: AssetId) -> bool {
        self.generic.pinned.contains(&id)
    }

    /// A counted load of a built-in id a typed load already stored: take the stored value over
    /// without reading it again, run the type's [`PrepareAsset`] (the CPU copy stays: the id is
    /// pinned). False when `id` is not in the slab of `type_id`.
    fn adopt_stored(&mut self, id: AssetId, type_id: TypeId) -> bool {
        if self.builtin_store(type_id).is_none() {
            return false;
        }
        let gpu_bytes = {
            let Some(value) = self.erased_store(type_id).and_then(|s| s.value_of(id)) else {
                return false;
            };
            match self.generic.preparers.get(&type_id) {
                Some(prepare) => prepare.prepare(id, value).unwrap_or_else(|err| {
                    tracing::warn!(target: "regia_asset", %id, %err, "prepare of a pinned asset failed");
                    0
                }),
                None => 0,
            }
        };
        let bytes = self.measure_stored(id, type_id);
        self.generic.loaded.insert(
            id,
            LoadedMeta {
                type_id,
                deps: Vec::new(),
                bytes,
                gpu_bytes,
                cpu_kept: true,
            },
        );
        self.generic.pinned.insert(id);
        self.load_state.insert(id, LoadState::Loaded);
        true
    }

    /// Whether a loader is registered for `T`.
    #[must_use]
    pub fn has_loader<T: Send + Sync + 'static>(&self) -> bool {
        self.generic.loaders.contains_key(&TypeId::of::<T>())
    }

    /// Starts loading `id` as a `T` on the IO pool and returns its handle at once (spec 12 § 3).
    /// A loaded or loading asset is not read again; a released one inside its grace is kept. The
    /// outcome arrives with [`Self::update`] (or [`Self::wait_for_loads`]): [`Self::state`] reads
    /// `Loaded` or `Failed`, and an [`AssetEvent`] is recorded.
    ///
    /// # Errors
    ///
    /// [`AssetLoadError::NoLoader`] when no loader is registered for `T`;
    /// [`AssetLoadError::Invalid`] when `id` is held as another type. A missing or undecodable
    /// asset is not an error here: it fails through its state.
    pub fn load<T: Send + Sync + 'static>(&mut self, id: AssetId) -> Result<StrongHandle<T>, AssetLoadError> {
        let type_id = TypeId::of::<T>();
        if !self.generic.loaders.contains_key(&type_id) {
            return Err(AssetLoadError::NoLoader(std::any::type_name::<T>().to_owned()));
        }
        let inner = self
            .generic
            .handles
            .strong(id, type_id, std::any::type_name::<T>())?;
        self.generic.pending.remove(&id);
        if !self.generic.loaded.contains_key(&id) && !self.adopt_stored(id, type_id) {
            self.start_generic_load(id, type_id);
        }
        Ok(StrongHandle::from_inner(inner))
    }

    /// [`Self::load`] of a typed reference.
    ///
    /// # Errors
    ///
    /// As [`Self::load`].
    pub fn load_ref<T: Send + Sync + 'static>(
        &mut self,
        asset: crate::id::AssetRef<T>,
    ) -> Result<StrongHandle<T>, AssetLoadError> {
        self.load::<T>(asset.id)
    }

    /// [`Self::load`] of the asset at a project-relative path, through the path manifest.
    ///
    /// # Errors
    ///
    /// [`AssetLoadError::NotFound`] (`asset_not_found`) when the manifest lists no such path;
    /// otherwise as [`Self::load`].
    pub fn load_path<T: Send + Sync + 'static>(
        &mut self,
        path: &str,
    ) -> Result<StrongHandle<T>, AssetLoadError> {
        if !self.has_loader::<T>() {
            return Err(AssetLoadError::NoLoader(std::any::type_name::<T>().to_owned()));
        }
        let id = self
            .id_for_path(path)
            .ok_or_else(|| AssetLoadError::NotFound(path.to_owned()))?;
        self.load::<T>(id)
    }

    /// The value of a generic handle, once loaded.
    #[must_use]
    pub fn get<T: Send + Sync + 'static>(&self, handle: &StrongHandle<T>) -> Option<&T> {
        let assets = self.assets::<T>()?;
        let slot = assets.handle_for_id(handle.id())?;
        assets.get(&slot)
    }

    /// The store of `T`: a registered loader's, or one of the built-in slabs (`CpuMesh`,
    /// `CookedTexture`, `CookedImage`, `CookedStandardMaterial`).
    #[must_use]
    pub fn assets<T: Send + Sync + 'static>(&self) -> Option<&Assets<T>> {
        if let Some(builtin) = self.builtin_store(TypeId::of::<T>()) {
            return builtin.downcast_ref::<Assets<T>>();
        }
        self.generic
            .stores
            .get(&TypeId::of::<T>())?
            .as_any()
            .downcast_ref::<Assets<T>>()
    }

    fn builtin_store(&self, type_id: TypeId) -> Option<&dyn Any> {
        if type_id == TypeId::of::<CpuMesh>() {
            Some(&self.meshes)
        } else if type_id == TypeId::of::<CookedTexture>() {
            Some(&self.textures)
        } else if type_id == TypeId::of::<CookedImage>() {
            Some(&self.images)
        } else if type_id == TypeId::of::<CookedStandardMaterial>() {
            Some(&self.materials)
        } else {
            None
        }
    }

    fn store_mut<T: Send + Sync + 'static>(&mut self) -> &mut Assets<T> {
        let type_id = TypeId::of::<T>();
        let builtin: Option<&mut dyn Any> = if type_id == TypeId::of::<CpuMesh>() {
            Some(&mut self.meshes)
        } else if type_id == TypeId::of::<CookedTexture>() {
            Some(&mut self.textures)
        } else if type_id == TypeId::of::<CookedImage>() {
            Some(&mut self.images)
        } else if type_id == TypeId::of::<CookedStandardMaterial>() {
            Some(&mut self.materials)
        } else {
            None
        };
        if let Some(store) = builtin.and_then(|s| s.downcast_mut::<Assets<T>>()) {
            return store;
        }
        let store = self
            .generic
            .stores
            .entry(type_id)
            .or_insert_with(|| Box::new(Assets::<T>::new()));
        // The map is keyed by the value's type id, so this never replaces a store; it keeps the
        // downcast below infallible without trusting that.
        if !store.as_any().is::<Assets<T>>() {
            *store = Box::new(Assets::<T>::new());
        }
        store
            .as_any_mut()
            .downcast_mut::<Assets<T>>()
            .unwrap_or_else(|| unreachable!("invariant: the store was just made an Assets<T>"))
    }

    /// Turns on [`AssetEvent`] recording for `T` (a registered loader's type or a built-in slab).
    /// Events wait in the store until [`Self::drain_events`]; the [`crate::AssetPlugin`] drains
    /// them every frame into ECS messages.
    pub fn enable_events<T: Send + Sync + 'static>(&mut self) {
        self.store_mut::<T>().enable_events();
    }

    /// The events recorded for `T` since the last call, in order.
    pub fn drain_events<T: Send + Sync + 'static>(&mut self) -> Vec<AssetEvent<T>> {
        let type_id = TypeId::of::<T>();
        if self.builtin_store(type_id).is_none() && !self.generic.stores.contains_key(&type_id) {
            return Vec::new();
        }
        self.store_mut::<T>().drain_events()
    }

    /// Starts the load task of `id` (nothing when one is already in flight).
    fn start_generic_load(&mut self, id: AssetId, type_id: TypeId) {
        if self.generic.is_in_flight(id) {
            return;
        }
        let Some(loader) = self.generic.loaders.get(&type_id).cloned() else {
            return;
        };
        if let Some(err) = self.unavailable_error(id) {
            self.fail_generic(id, type_id, &err);
            return;
        }
        self.load_state.insert(id, LoadState::Loading);
        let job = LoadJob {
            id,
            type_id,
            loader,
            loaders: Arc::clone(&self.generic.loaders),
            handles: Arc::clone(&self.generic.handles),
            reader: self.payload_reader(),
        };
        let task = regia_core::tasks::spawn_io(async move { run_job(job) });
        self.generic.in_flight.push_back(InFlight { id, task });
        self.generic.loads_started += 1;
    }

    /// Records a failed first load of `id`.
    fn fail_generic(&mut self, id: AssetId, type_id: TypeId, err: &AssetLoadError) {
        let text = err.to_string();
        self.load_state.insert(id, LoadState::Failed(text.clone()));
        if let Some(store) = self.erased_store_mut(type_id) {
            store.push_failed(id, text.clone());
        }
        if self.generic.is_grouped(id) {
            self.generic
                .load_sets
                .mark_failed(LoadKind::Other, &group_item_key(id), text);
        }
    }

    /// Applies a finished load: stores the value, records edges, starts dependency loads.
    /// An error is the decode or prepare failure (the failure is recorded here too).
    fn apply_generic_outcome(&mut self, outcome: LoadOutcome) -> Result<(), AssetLoadError> {
        let LoadOutcome {
            id,
            type_id,
            result,
            timing,
        } = outcome;
        crate::load_stats::note_load(result.is_ok());
        let was_loaded = self.generic.loaded.contains_key(&id);
        let prepare_started = std::time::Instant::now();
        let result = result.and_then(|decoded| self.prepare_decoded(id, type_id, decoded));
        self.load_times.record(crate::load_times::LoadTime {
            id,
            kind: timing.kind,
            bytes: timing.bytes,
            read_ns: timing.read_ns,
            decode_ns: timing.decode_ns,
            upload_ns: crate::load_times::since(prepare_started),
            source: timing.source,
            ok: result.is_ok(),
            seq: 0,
        });
        let result = match result {
            Ok((decoded, gpu_bytes, cpu_kept)) => {
                let Some(store) = self.erased_store_mut(type_id) else {
                    return Ok(());
                };
                if !store.insert_boxed(id, decoded.value) {
                    tracing::warn!(target: "regia_asset", %id, "loader produced another type; dropped");
                    return Ok(());
                }
                self.note_builtin_slot(type_id, id, true);
                let deps: Vec<AssetId> = decoded.deps.iter().map(|(d, _)| *d).collect();
                self.generic.set_edges(id, &deps);
                self.generic.loaded.insert(
                    id,
                    LoadedMeta {
                        type_id,
                        deps,
                        bytes: decoded.bytes,
                        gpu_bytes,
                        cpu_kept,
                    },
                );
                self.load_state.insert(id, LoadState::Loaded);
                self.asset_sources.insert(id, decoded.source);
                if self.generic.is_grouped(id) {
                    self.generic
                        .load_sets
                        .mark_resident(LoadKind::Other, &group_item_key(id));
                }
                for (dep, dep_type) in decoded.deps {
                    if !self.generic.loaded.contains_key(&dep) {
                        self.start_generic_load(dep, dep_type);
                    }
                }
                // Nobody holds it any more (every handle dropped while it loaded): count down.
                if self.generic.handles.strong_count(id) == 0 {
                    self.schedule_unload(id);
                }
                Ok(())
            }
            Err(err) if was_loaded => {
                tracing::warn!(target: "regia_asset", %id, %err, "reload failed; keeping the previous value");
                self.load_state.insert(id, LoadState::Loaded);
                Err(err)
            }
            Err(err) => {
                tracing::warn!(target: "regia_asset", %id, %err, "asset load failed");
                self.fail_generic(id, type_id, &err);
                Err(err)
            }
        };
        self.check_groups();
        result
    }

    /// Runs the type's [`PrepareAsset`] on a decoded value and releases its CPU data unless the id
    /// keeps it. Returns the value with its GPU bytes and whether the CPU copy is kept.
    fn prepare_decoded(
        &self,
        id: AssetId,
        type_id: TypeId,
        mut decoded: Decoded,
    ) -> Result<(Decoded, u64, bool), AssetLoadError> {
        let Some(prepare) = self.generic.preparers.get(&type_id) else {
            return Ok((decoded, 0, true));
        };
        let _span = tracing::info_span!("asset_prepare", kind = self.generic.kind_of(type_id)).entered();
        let gpu_bytes = prepare
            .prepare(id, &*decoded.value)
            .map_err(|e| AssetLoadError::Invalid(format!("prepare: {e}")))?;
        if self.generic.keep_cpu.contains(&id) || self.generic.pinned.contains(&id) {
            return Ok((decoded, gpu_bytes, true));
        }
        if let Some(loader) = self.generic.loaders.get(&type_id) {
            decoded.bytes = loader.release(&mut *decoded.value);
        }
        Ok((decoded, gpu_bytes, false))
    }

    /// Registers the [`PrepareAsset`] step of `P::Asset` (replacing an earlier one). Loads that
    /// finish from now on are prepared; values already stored are not.
    pub fn register_prepare<P: PrepareAsset>(&mut self, prepare: P) {
        self.generic
            .preparers
            .insert(TypeId::of::<P::Asset>(), Arc::new(PrepareBox(prepare)));
    }

    /// [`Self::load`] with options. With [`LoadSettings::keep_cpu_data`] on an asset that is loaded
    /// with its CPU copy released, the asset is read again and keeps it.
    ///
    /// # Errors
    ///
    /// As [`Self::load`].
    pub fn load_with<T: Send + Sync + 'static>(
        &mut self,
        id: AssetId,
        settings: LoadSettings,
    ) -> Result<StrongHandle<T>, AssetLoadError> {
        if settings.keep_cpu_data && self.has_loader::<T>() {
            self.generic.keep_cpu.insert(id);
        }
        let handle = self.load::<T>(id)?;
        if settings.keep_cpu_data && self.generic.loaded.get(&id).is_some_and(|m| !m.cpu_kept) {
            self.start_generic_load(id, TypeId::of::<T>());
        }
        Ok(handle)
    }

    /// Whether the CPU copy of a loaded generic asset is kept (`None` when not loaded).
    #[must_use]
    pub fn cpu_data_kept(&self, id: AssetId) -> Option<bool> {
        self.generic.loaded.get(&id).map(|m| m.cpu_kept)
    }

    /// GPU estimate of the loaded assets of `T` ([`PrepareAsset::prepare`]).
    #[must_use]
    pub fn gpu_memory_use<T: Send + Sync + 'static>(&self) -> u64 {
        let type_id = TypeId::of::<T>();
        self.generic
            .loaded
            .values()
            .filter(|m| m.type_id == type_id)
            .fold(0u64, |a, m| a.saturating_add(m.gpu_bytes))
    }

    /// Applies every finished load (in start order) without waiting.
    fn poll_generic_loads(&mut self) {
        loop {
            let Some(at) = self.generic.in_flight.iter().position(|f| f.task.is_finished()) else {
                return;
            };
            let Some(done) = self.generic.in_flight.remove(at) else {
                return;
            };
            let outcome = regia_core::tasks::block_on(done.task);
            // The failure is recorded inside; the poll order does not change.
            let _ = self.apply_generic_outcome(outcome);
        }
    }

    /// Blocks on the oldest load in flight and applies it. False when nothing is in flight. For
    /// loading screens that step one asset at a time, and for tests (no clock involved).
    pub fn wait_for_next_load(&mut self) -> bool {
        let Some(done) = self.generic.in_flight.pop_front() else {
            return false;
        };
        let outcome = regia_core::tasks::block_on(done.task);
        // The failure is recorded inside; callers step loads one at a time, not by error.
        let _ = self.apply_generic_outcome(outcome);
        true
    }

    /// Blocks until every load, including the dependency loads they start, has finished and been
    /// applied.
    pub fn wait_for_loads(&mut self) {
        while self.wait_for_next_load() {}
    }

    /// Generic loads in flight.
    #[must_use]
    pub fn loads_in_flight(&self) -> usize {
        self.generic.in_flight.len()
    }

    /// Generic load tasks started since the server opened (a hot reload is not counted).
    #[must_use]
    pub fn generic_loads_started(&self) -> u64 {
        self.generic.loads_started
    }

    /// One frame of the generic API: applies finished loads, counts down released assets and
    /// unloads those whose grace ran out, enforces the memory budgets, and forgets dropped load
    /// groups. [`crate::AssetPlugin`] runs it in `PreUpdate`.
    pub fn update(&mut self) {
        let g = &self.generic;
        if g.in_flight.is_empty() && g.pending.is_empty() && g.groups.is_empty() && g.loaded.is_empty() {
            return;
        }
        let _span = tracing::info_span!("asset_server_update").entered();
        self.poll_generic_loads();
        self.drain_releases();
        let mut due: Vec<(u64, AssetId)> = Vec::new();
        for (id, p) in &mut self.generic.pending {
            p.remaining = p.remaining.saturating_sub(1);
            if p.remaining == 0 {
                due.push((p.seq, *id));
            }
        }
        due.sort_unstable();
        for (_, id) in due {
            self.unload_generic(id);
        }
        // What those unloads released starts counting from the next frame.
        self.drain_releases();
        self.enforce_budgets();
        self.drain_releases();
        self.prune_groups();
        self.check_groups();
    }

    /// Takes the release notices: a loaded asset with no strong handle left starts its grace.
    fn drain_releases(&mut self) {
        let released: Vec<AssetId> = self.generic.releases.lock().try_iter().collect();
        for id in released {
            if self.generic.handles.prune(id) {
                continue; // taken again before this frame
            }
            if self.generic.loaded.contains_key(&id) {
                self.schedule_unload(id);
            }
        }
    }

    fn schedule_unload(&mut self, id: AssetId) {
        if self.generic.pinned.contains(&id) {
            return; // a typed load's uncounted handle keeps it
        }
        let seq = self.generic.release_seq;
        let grace = self.generic.grace;
        if let std::collections::hash_map::Entry::Vacant(slot) = self.generic.pending.entry(id) {
            slot.insert(PendingUnload {
                remaining: grace,
                seq,
            });
            self.generic.release_seq += 1;
        }
    }

    /// Drops a generic asset: its value (and the handles it held), its edges and its state.
    fn unload_generic(&mut self, id: AssetId) {
        self.generic.pending.remove(&id);
        let Some(meta) = self.generic.loaded.get(&id) else {
            return;
        };
        let type_id = meta.type_id;
        self.generic.set_edges(id, &[]);
        self.generic.loaded.remove(&id);
        self.load_state.remove(&id);
        self.asset_sources.remove(&id);
        self.generations.remove(&id);
        self.generic.keep_cpu.remove(&id);
        self.generic.pinned.remove(&id);
        self.note_builtin_slot(type_id, id, false);
        if let Some(store) = self.erased_store_mut(type_id) {
            store.remove(id);
        }
        if let Some(prepare) = self.generic.preparers.get(&type_id) {
            prepare.unload(id);
        }
        tracing::debug!(target: "regia_asset", %id, "generic asset unloaded");
    }

    /// Evicts released assets of each over-budget type, oldest release first.
    ///
    /// A budget holds CPU plus GPU bytes ([`Self::budgeted_memory_use`]), so a texture whose CPU
    /// copy went after its upload still counts. The check allocates nothing while every type is
    /// under budget (a steady frame).
    fn enforce_budgets(&mut self) {
        let over = self
            .generic
            .budgets
            .iter()
            .any(|(type_id, budget)| self.budgeted_use_of(*type_id) > *budget);
        if !over {
            return;
        }
        let budgets: Vec<(TypeId, u64)> = self.generic.budgets.iter().map(|(t, b)| (*t, *b)).collect();
        for (type_id, budget) in budgets {
            let mut used = self.budgeted_use_of(type_id);
            if used <= budget {
                continue;
            }
            let mut candidates: Vec<(u64, AssetId, u64)> = self
                .generic
                .pending
                .iter()
                .filter_map(|(id, p)| {
                    let meta = self.generic.loaded.get(id)?;
                    (meta.type_id == type_id).then_some((
                        p.seq,
                        *id,
                        meta.bytes.saturating_add(meta.gpu_bytes),
                    ))
                })
                .collect();
            candidates.sort_unstable();
            for (_, id, bytes) in candidates {
                if used <= budget {
                    break;
                }
                self.unload_generic(id);
                used = used.saturating_sub(bytes);
                self.generic.budget_evictions.push(id);
            }
        }
    }

    fn memory_use_of(&self, type_id: TypeId) -> u64 {
        self.generic
            .loaded
            .values()
            .filter(|m| m.type_id == type_id)
            .fold(0u64, |a, m| a.saturating_add(m.bytes))
    }

    fn budgeted_use_of(&self, type_id: TypeId) -> u64 {
        self.generic
            .loaded
            .values()
            .filter(|m| m.type_id == type_id)
            .fold(0u64, |a, m| a.saturating_add(m.bytes).saturating_add(m.gpu_bytes))
    }

    /// CPU bytes of the loaded assets of `T` ([`AssetLoader::cpu_bytes`]).
    #[must_use]
    pub fn memory_use<T: Send + Sync + 'static>(&self) -> u64 {
        self.memory_use_of(TypeId::of::<T>())
    }

    /// What the memory budget of `T` is held against: the CPU bytes ([`Self::memory_use`]) plus
    /// the GPU estimate ([`Self::gpu_memory_use`]) of its loaded assets. Each byte is counted
    /// once: a CPU copy released after its upload counts as GPU only (ADR-7256).
    #[must_use]
    pub fn budgeted_memory_use<T: Send + Sync + 'static>(&self) -> u64 {
        self.budgeted_use_of(TypeId::of::<T>())
    }

    /// Sets (or clears) the memory budget of `T` ([`Self::budgeted_memory_use`]: CPU plus GPU
    /// bytes): past it, released assets of `T` unload before their grace runs out, oldest release
    /// first. Assets with a strong handle, and pinned ones, stay. Built-in kinds (`CpuMesh`,
    /// `CookedTexture`, ...) take a budget like any registered type.
    pub fn set_memory_budget<T: Send + Sync + 'static>(&mut self, bytes: Option<u64>) {
        match bytes {
            Some(b) => self.generic.budgets.insert(TypeId::of::<T>(), b),
            None => self.generic.budgets.remove(&TypeId::of::<T>()),
        };
    }

    /// The ids evicted by a budget since the last call, in eviction order.
    pub fn take_budget_evictions(&mut self) -> Vec<AssetId> {
        std::mem::take(&mut self.generic.budget_evictions)
    }

    /// Sets the unload grace in frames (0: unload on the next [`Self::update`]; shipping
    /// low-memory mode).
    pub fn set_unload_grace_frames(&mut self, frames: u32) {
        self.generic.grace = frames;
    }

    /// The unload grace in frames.
    #[must_use]
    pub fn unload_grace_frames(&self) -> u32 {
        self.generic.grace
    }

    /// Live strong handles of `id`.
    #[must_use]
    pub fn strong_count(&self, id: AssetId) -> usize {
        self.generic.handles.strong_count(id)
    }

    /// Whether `id` is loaded with no strong handle and counting down its grace. Releases are
    /// noticed by [`Self::update`].
    #[must_use]
    pub fn is_pending_unload(&self, id: AssetId) -> bool {
        self.generic.pending.contains_key(&id)
    }

    /// A strong handle from a weak one: while a strong handle exists, or inside the grace after
    /// the last one dropped (which cancels the unload). `None` once unloaded.
    pub fn upgrade<T: Send + Sync + 'static>(
        &mut self,
        weak: &WeakAssetHandle<T>,
    ) -> Option<StrongHandle<T>> {
        if let Some(strong) = weak.upgrade() {
            return Some(strong);
        }
        if !self.generic.loaded.contains_key(&weak.id) {
            return None;
        }
        let inner = self
            .generic
            .handles
            .strong(weak.id, TypeId::of::<T>(), std::any::type_name::<T>())
            .ok()?;
        self.generic.pending.remove(&weak.id);
        Some(StrongHandle::from_inner(inner))
    }

    /// The generic assets `id` loaded through its context, in the order it asked.
    #[must_use]
    pub fn dependencies_of(&self, id: AssetId) -> Vec<AssetId> {
        self.generic
            .loaded
            .get(&id)
            .map(|m| m.deps.clone())
            .unwrap_or_default()
    }

    /// The loaded generic assets that depend on `id` (the runtime referencer scan), by id.
    #[must_use]
    pub fn dependents_of(&self, id: AssetId) -> Vec<AssetId> {
        self.generic
            .dependents
            .get(&id)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Whether `id` is a loaded generic asset.
    #[must_use]
    pub fn is_generic_loaded(&self, id: AssetId) -> bool {
        self.generic.loaded.contains_key(&id)
    }

    /// The value type of a loaded generic (or counted built-in) asset.
    pub(super) fn generic_type_of(&self, id: AssetId) -> Option<TypeId> {
        self.generic.loaded.get(&id).map(|m| m.type_id)
    }

    /// Hot reload of a loaded generic asset (from [`Self::reload_id`]): re-decodes it on this
    /// thread, then every asset that depends on it, transitively, each once. A failed decode keeps
    /// the previous value.
    pub(super) fn reload_generic(&mut self, id: AssetId) -> Result<(), AssetLoadError> {
        let first = self.reload_generic_one(id);
        let mut queue: VecDeque<AssetId> = self.dependents_of(id).into();
        let mut seen: BTreeSet<AssetId> = BTreeSet::from([id]);
        while let Some(next) = queue.pop_front() {
            if !seen.insert(next) {
                continue;
            }
            *self.generations.entry(next).or_insert(0) += 1;
            if let Err(err) = self.reload_generic_one(next) {
                tracing::warn!(target: "regia_asset", id = %next, %err, "dependent reload failed; keeping the previous value");
            }
            self.generic.reloaded_dependents.push(next);
            queue.extend(self.dependents_of(next));
        }
        first
    }

    fn reload_generic_one(&mut self, id: AssetId) -> Result<(), AssetLoadError> {
        let Some(old_type) = self.generic.loaded.get(&id).map(|m| m.type_id) else {
            return Ok(());
        };
        let reader = self.payload_reader();
        let read = reader.read(id);
        // A built-in texture whose cooked file is now an RGBA8 image (it shrank under the
        // block-compression minimum), or the reverse, takes the new kind (ADR-7284).
        let type_id = read
            .as_ref()
            .ok()
            .and_then(|(bytes, _)| decode_cooked_file(bytes).ok())
            .and_then(|(header, _)| switched_builtin_kind(old_type, AssetTypeTag(header.type_tag)))
            .filter(|new| self.generic.loaders.contains_key(new))
            .unwrap_or(old_type);
        let Some(loader) = self.generic.loaders.get(&type_id).cloned() else {
            return Ok(());
        };
        let outcome = decode_job(
            LoadJob {
                id,
                type_id,
                loader,
                loaders: Arc::clone(&self.generic.loaders),
                handles: Arc::clone(&self.generic.handles),
                reader,
            },
            read,
        );
        let err = outcome.result.as_ref().err().map(ToString::to_string);
        // The kind switch moves the old value out only after the new one applied: if the new
        // kind's decode or prepare fails, apply takes the "keep the previous value" branch and
        // the old value, its GPU copy and its kind all survive (a prepare failure used to run
        // after the old value was already dropped).
        let applied = self.apply_generic_outcome(outcome);
        if type_id != old_type && applied.is_ok() {
            self.leave_builtin_kind(id, old_type, type_id);
        }
        match err {
            Some(e) => Err(AssetLoadError::Invalid(e)),
            None => applied,
        }
    }

    /// Moves `id` out of the slab of `old_type` once its value of `new_type` applied: the old
    /// value and its GPU copy go ([`PrepareAsset::unload`]), and the live count is held for the new
    /// type, so `load::<New>(id)` shares it and old handles resolve to nothing. Runs only after a
    /// successful apply so a failed decode or prepare keeps the old value and kind.
    fn leave_builtin_kind(&mut self, id: AssetId, old_type: TypeId, new_type: TypeId) {
        self.note_builtin_slot(old_type, id, false);
        if let Some(store) = self.erased_store_mut(old_type) {
            store.remove(id);
        }
        if let Some(prepare) = self.generic.preparers.get(&old_type) {
            prepare.unload(id);
        }
        if let Some(meta) = self.generic.loaded.get_mut(&id) {
            meta.type_id = new_type;
        }
        self.generic.handles.retype(id, new_type);
        tracing::debug!(target: "regia_asset", %id, kind = self.generic.kind_of(new_type), "counted asset changed kind on reload");
    }

    /// The dependents a hot reload reloaded since the last call (besides the ids it was asked
    /// for), in reload order. [`super::apply_reload_messages`] reports them as `AssetReloaded`.
    pub fn take_reloaded_dependents(&mut self) -> Vec<AssetId> {
        std::mem::take(&mut self.generic.reloaded_dependents)
    }

    /// Uses `sets` (the app's [`LoadSets`] resource) for load-group progress. The
    /// [`crate::AssetPlugin`] shares its resource; a bare server has a private registry.
    pub fn set_load_sets(&mut self, sets: LoadSets) {
        self.generic.load_sets = sets;
    }

    /// The registry load groups report to.
    #[must_use]
    pub fn load_sets(&self) -> &LoadSets {
        &self.generic.load_sets
    }

    /// Loads every id as a `T` and returns a group handle whose progress is one [`LoadSets`] set
    /// (spec 12 § 3: loading screens). The group keeps its members loaded until its last clone
    /// drops.
    ///
    /// # Errors
    ///
    /// As [`Self::load`].
    pub fn load_group<T: Send + Sync + 'static>(
        &mut self,
        ids: &[AssetId],
    ) -> Result<LoadGroupHandle, AssetLoadError> {
        let mut unique: Vec<AssetId> = Vec::with_capacity(ids.len());
        for id in ids {
            if !unique.contains(id) {
                unique.push(*id);
            }
        }
        let mut handles = Vec::with_capacity(unique.len());
        for id in &unique {
            handles.push(self.load::<T>(*id)?.inner);
        }
        let name = format!("load_group:{}", self.generic.next_group);
        self.generic.next_group += 1;
        let mut set = LoadSet::new(name.clone());
        for id in &unique {
            set.push(LoadKind::Other, group_item_key(*id), 0, 0);
        }
        self.generic.load_sets.define(set);
        for id in &unique {
            match self.state(*id) {
                LoadState::Loaded => self
                    .generic
                    .load_sets
                    .mark_resident(LoadKind::Other, &group_item_key(*id)),
                LoadState::Failed(err) => {
                    self.generic
                        .load_sets
                        .mark_failed(LoadKind::Other, &group_item_key(*id), err);
                }
                _ => {}
            }
        }
        let inner = Arc::new(GroupInner {
            name: name.clone(),
            _handles: handles,
        });
        self.generic.groups.push(GroupRecord {
            name,
            ids: unique,
            holder: Arc::downgrade(&inner),
            complete: false,
        });
        self.check_groups();
        Ok(LoadGroupHandle { inner })
    }

    /// The group's progress (`None` for a group this server does not know).
    #[must_use]
    pub fn group_progress(&self, group: &LoadGroupHandle) -> Option<LoadGroupProgress> {
        let record = self.generic.groups.iter().find(|g| g.name == group.name())?;
        self.progress_of(record)
    }

    fn progress_of(&self, record: &GroupRecord) -> Option<LoadGroupProgress> {
        let p = self.generic.load_sets.load_progress(&record.name)?;
        let failed = p.failed.len() as u32;
        Some(LoadGroupProgress {
            name: record.name.clone(),
            loaded: p.resident,
            failed,
            total: p.total,
            fraction: p.fraction,
            complete: record.complete || p.resident + failed >= p.total,
            failures: p.failed,
        })
    }

    /// Latches completion of each group whose members all settled.
    fn check_groups(&mut self) {
        for i in 0..self.generic.groups.len() {
            if self.generic.groups[i].complete {
                continue;
            }
            let done = self
                .progress_of(&self.generic.groups[i])
                .is_some_and(|p| p.loaded + p.failed >= p.total);
            if done {
                let record = &mut self.generic.groups[i];
                record.complete = true;
                self.generic.completed_groups.push(record.name.clone());
            }
        }
    }

    /// Forgets groups whose handles all dropped (and their [`LoadSets`] sets).
    fn prune_groups(&mut self) {
        let (alive, dead): (Vec<GroupRecord>, Vec<GroupRecord>) = std::mem::take(&mut self.generic.groups)
            .into_iter()
            .partition(|g| g.holder.strong_count() > 0);
        self.generic.groups = alive;
        for group in dead {
            self.generic.load_sets.remove(&group.name);
            // Its members leave the resident list unless another group still tracks them, so a
            // later group over an unloaded member does not start as resident.
            for id in group.ids {
                if !self.generic.is_grouped(id) {
                    self.generic
                        .load_sets
                        .mark_evicted(LoadKind::Other, &group_item_key(id));
                }
            }
        }
    }

    /// The groups that completed since the last call, in completion order. Each group appears
    /// once in its lifetime.
    pub fn take_completed_groups(&mut self) -> Vec<String> {
        std::mem::take(&mut self.generic.completed_groups)
    }

    /// Reference counts, pending unloads, group progress and reference cycles (agent
    /// `assets.handles`). `kind` keeps the kinds whose name matches, case-insensitively. Built-in
    /// kinds (`mesh`, `texture`, `image`, `material`) list their counted assets and the ones a
    /// typed load pinned, and appear only while they hold or load something.
    #[must_use]
    pub fn handle_report(&self, kind: Option<&str>) -> HandleReport {
        let wanted = |name: &str| kind.is_none_or(|k| k.eq_ignore_ascii_case(name));
        let mut kinds: Vec<KindHandles> = Vec::new();
        let mut assets: Vec<HandleRow> = Vec::new();
        for (type_id, loader) in self.generic.loaders.iter() {
            let name = loader.kind();
            if !wanted(name) {
                continue;
            }
            let mut line = KindHandles {
                kind: name.to_owned(),
                loaded: 0,
                loading: 0,
                strong_handles: 0,
                pending_unloads: 0,
                pinned: 0,
                cpu_bytes: 0,
                gpu_bytes: 0,
                budget: self.generic.budgets.get(type_id).copied(),
            };
            let mut push = |line: &mut KindHandles, row: HandleRow| {
                line.loaded += 1;
                line.strong_handles += row.strong;
                line.pending_unloads += usize::from(row.unload_in_frames.is_some());
                line.pinned += usize::from(row.pinned);
                line.cpu_bytes = line.cpu_bytes.saturating_add(row.cpu_bytes);
                line.gpu_bytes = line.gpu_bytes.saturating_add(row.gpu_bytes);
                assets.push(row);
            };
            for (id, meta) in self.generic.loaded.iter().filter(|(_, m)| m.type_id == *type_id) {
                let row = HandleRow {
                    id: *id,
                    kind: name.to_owned(),
                    strong: self.generic.handles.strong_count(*id),
                    unload_in_frames: self.generic.pending.get(id).map(|p| p.remaining),
                    cpu_bytes: meta.bytes,
                    gpu_bytes: meta.gpu_bytes,
                    cpu_data_kept: meta.cpu_kept,
                    dependencies: meta.deps.len(),
                    pinned: self.generic.pinned.contains(id),
                };
                push(&mut line, row);
            }
            let builtin = self.builtin_store(*type_id).is_some();
            if builtin && let Some(store) = self.erased_store(*type_id) {
                // Values only a typed load stored: pinned, uncounted, CPU copy kept.
                for id in store.ids() {
                    if self.generic.loaded.contains_key(&id) {
                        continue;
                    }
                    let row = HandleRow {
                        id,
                        kind: name.to_owned(),
                        strong: 0,
                        unload_in_frames: None,
                        cpu_bytes: store.value_of(id).map_or(0, |v| loader.measure(v)),
                        gpu_bytes: 0,
                        cpu_data_kept: true,
                        dependencies: 0,
                        pinned: true,
                    };
                    push(&mut line, row);
                }
            }
            line.loading = self
                .generic
                .in_flight
                .iter()
                .filter(|f| self.load_type_of(f.id) == Some(*type_id))
                .count();
            if builtin && line.loaded == 0 && line.loading == 0 {
                continue;
            }
            kinds.push(line);
        }
        kinds.sort_by(|a, b| a.kind.cmp(&b.kind));
        assets.sort_by(|a, b| (a.kind.as_str(), a.id).cmp(&(b.kind.as_str(), b.id)));
        let groups = self
            .generic
            .groups
            .iter()
            .filter_map(|g| self.progress_of(g))
            .collect();
        let cycles = self.reference_cycles();
        if !cycles.is_empty() {
            tracing::warn!(target: "regia_asset", cycles = cycles.len(), "assets hold each other in reference cycles; they never unload by count");
        }
        HandleReport {
            grace_frames: self.generic.grace,
            kinds,
            assets,
            groups,
            cycles,
        }
    }

    /// The reference cycles among loaded assets (A needs B, B needs A): the strongly connected
    /// components of the dependency graph with more than one member, found with Tarjan's
    /// algorithm. Each cycle's members keep each other's handles, so the cycle stays loaded after
    /// every outside handle drops (ADR-7256). Members in id order; cycles by their first member.
    #[must_use]
    pub fn reference_cycles(&self) -> Vec<Vec<AssetId>> {
        let mut nodes: Vec<AssetId> = self
            .generic
            .loaded
            .iter()
            .filter(|(_, m)| !m.deps.is_empty())
            .map(|(id, _)| *id)
            .collect();
        nodes.sort_unstable();
        strongly_connected_cycles(&nodes, |id| {
            self.generic
                .loaded
                .get(&id)
                .map(|m| m.deps.as_slice())
                .unwrap_or_default()
        })
    }

    /// The type a load in flight decodes, from its live handle.
    fn load_type_of(&self, id: AssetId) -> Option<TypeId> {
        self.generic
            .handles
            .live
            .lock()
            .get(&id)
            .and_then(Weak::upgrade)
            .map(|inner| *inner.type_id.lock())
    }

    /// The kind name of a loaded generic asset.
    #[must_use]
    pub fn generic_kind_of(&self, id: AssetId) -> Option<&'static str> {
        self.generic
            .loaded
            .get(&id)
            .map(|m| self.generic.kind_of(m.type_id))
    }
}

/// The strongly connected components with more than one member reachable from `roots`, by
/// Tarjan's algorithm (iterative, so a long dependency chain cannot overflow the stack). `edges`
/// lists a node's successors. Members of each component are sorted; components are sorted.
fn strongly_connected_cycles<'a>(
    roots: &[AssetId],
    edges: impl Fn(AssetId) -> &'a [AssetId],
) -> Vec<Vec<AssetId>> {
    struct Frame {
        node: AssetId,
        next_edge: usize,
    }
    let mut index_of: HashMap<AssetId, (u32, u32)> = HashMap::new(); // (index, lowlink)
    let mut on_stack: HashSet<AssetId> = HashSet::new();
    let mut stack: Vec<AssetId> = Vec::new();
    let mut next_index = 0u32;
    let mut out: Vec<Vec<AssetId>> = Vec::new();
    for &root in roots {
        if index_of.contains_key(&root) {
            continue;
        }
        let mut calls = vec![Frame {
            node: root,
            next_edge: 0,
        }];
        index_of.insert(root, (next_index, next_index));
        next_index += 1;
        stack.push(root);
        on_stack.insert(root);
        while let Some(frame) = calls.last_mut() {
            let node = frame.node;
            let succ = edges(node);
            if let Some(&next) = succ.get(frame.next_edge) {
                frame.next_edge += 1;
                match index_of.get(&next).copied() {
                    None => {
                        index_of.insert(next, (next_index, next_index));
                        next_index += 1;
                        stack.push(next);
                        on_stack.insert(next);
                        calls.push(Frame {
                            node: next,
                            next_edge: 0,
                        });
                    }
                    Some((next_idx, _)) if on_stack.contains(&next) => {
                        if let Some(entry) = index_of.get_mut(&node) {
                            entry.1 = entry.1.min(next_idx);
                        }
                    }
                    Some(_) => {}
                }
                continue;
            }
            // Every successor visited: close the node.
            calls.pop();
            let (idx, low) = index_of.get(&node).copied().unwrap_or((0, 0));
            if let Some(parent) = calls.last()
                && let Some(entry) = index_of.get_mut(&parent.node)
            {
                entry.1 = entry.1.min(low);
            }
            if low == idx {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack.remove(&member);
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                if component.len() > 1 {
                    component.sort_unstable();
                    out.push(component);
                }
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_type_names_drop_the_module_path_and_generics() {
        assert_eq!(short_type_name::<CpuMesh>(), "CpuMesh");
        assert_eq!(short_type_name::<Vec<u8>>(), "Vec");
        assert_eq!(short_type_name::<u32>(), "u32");
    }

    #[test]
    fn a_handle_table_reuses_the_live_handle_and_refuses_another_type() {
        let (tx, rx) = std::sync::mpsc::channel();
        let table = HandleTable {
            live: Mutex::new(HashMap::new()),
            releases: Mutex::new(tx),
        };
        let id = AssetId::new_random();
        let a = table.strong(id, TypeId::of::<u32>(), "u32").expect("new");
        let b = table.strong(id, TypeId::of::<u32>(), "u32").expect("same");
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(table.strong_count(id), 2);
        assert!(table.strong(id, TypeId::of::<u8>(), "u8").is_err());
        drop(a);
        assert!(rx.try_recv().is_err(), "not the last handle");
        drop(b);
        assert_eq!(rx.try_recv().ok(), Some(id));
        assert!(!table.prune(id));
        assert_eq!(table.strong_count(id), 0);
    }

    #[test]
    fn tarjan_finds_two_and_three_member_cycles_and_skips_chains() {
        let ids: Vec<AssetId> = (0..7).map(|_| AssetId::new_random()).collect();
        let (a, b, c, d, e, f, g) = (ids[0], ids[1], ids[2], ids[3], ids[4], ids[5], ids[6]);
        // a <-> b; c -> d -> e -> c; f -> a (enters a cycle, is not in it); g alone.
        let graph: HashMap<AssetId, Vec<AssetId>> = HashMap::from([
            (a, vec![b]),
            (b, vec![a]),
            (c, vec![d]),
            (d, vec![e]),
            (e, vec![c]),
            (f, vec![a]),
            (g, vec![]),
        ]);
        let empty: Vec<AssetId> = Vec::new();
        let got = strongly_connected_cycles(&ids, |id| graph.get(&id).unwrap_or(&empty).as_slice());
        let mut two = vec![a, b];
        two.sort_unstable();
        let mut three = vec![c, d, e];
        three.sort_unstable();
        let mut want = vec![two, three];
        want.sort();
        assert_eq!(got, want);
    }
}
