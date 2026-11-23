// SPDX-License-Identifier: MPL-2.0
//! Image identity for material textures, separate from the material's cache key.
//!
//! A material key owns the bind group and the uniform. Textures that several materials share
//! (a paint scan, a cooked graph slot) are uploaded once, keyed by the [`SharedImage`] or
//! [`CompressedTexture`](crate::CompressedTexture) allocation, and released when the last
//! material that bound them is dropped or evicted. A [`DynamicTexture`] is that same kind of
//! handle with one extra property: the host replaces its level-0 bytes in place, so a display
//! that changes every frame does not allocate a texture or a bind group. Each renderer copies
//! a write once onto the texture it owns; the bytes stay on the handle for any other renderer.
//!
//! Nothing here content-hashes pixels. Hashing a 2048² image on the frame thread is the hitch
//! [`crate::frame`] already refuses. Two identical `Vec`s are two images until the host puts
//! them in one [`SharedImage`].
//!
//! The cache key is the allocation address plus a generation chosen when that allocation was
//! created. The cache entry holds the `Arc`, and a hit is taken only when that `Arc` is the
//! caller's. A freed address the allocator hands out again therefore cannot return the previous
//! GPU texture (ADR-179).

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Allocation kept alive for as long as a cache entry or a material entry references it.
pub(crate) type Owner = Arc<dyn Any + Send + Sync>;

fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Data address of an `Arc`, including one coerced to [`Owner`].
fn concrete_addr<T: ?Sized>(arc: &Arc<T>) -> usize {
    Arc::as_ptr(arc) as *const () as usize
}

use crate::compressed_texture::CompressedTexture;
use crate::surface_graph::GraphTexture;

/// A shared RGBA8 image, optionally with a cooked block-compressed chain.
///
/// Clones share the allocation. The renderer uploads one GPU texture per ([`Self::identity`],
/// color space) pair and keeps it while at least one resident material binds it.
#[derive(Clone, Debug)]
pub struct SharedImage {
    inner: Arc<SharedInner>,
}

#[derive(Debug)]
struct SharedInner {
    width: u32,
    height: u32,
    /// Row-major RGBA8. Empty when the host kept only the cooked chain. An `Arc` so a
    /// consumer (the path tracer's `PtTexture`) shares the allocation instead of copying
    /// the pixels on every extract (`SharedImage::pixels_arc`).
    rgba: Arc<[u8]>,
    compressed: Option<Arc<CompressedTexture>>,
    srgb: bool,
    /// Chosen at construction. A later allocation, even at this address, gets another.
    generation: u64,
}

/// [`SharedImage`] could not be built.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SharedImageError {
    /// Width or height was zero.
    #[error("shared image size must be at least 1x1")]
    Empty,
    /// The RGBA8 buffer is present but shorter than `width * height * 4`.
    #[error("shared image is {width}x{height} but the RGBA8 buffer is {got} bytes")]
    Size {
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// Bytes the caller passed.
        got: usize,
    },
}

impl SharedImage {
    /// An RGBA8 image. `srgb` selects `Rgba8UnormSrgb` (color) or `Rgba8Unorm` (data).
    ///
    /// # Errors
    ///
    /// [`SharedImageError::Empty`] when a side is 0, [`SharedImageError::Size`] when `rgba`
    /// is shorter than the image.
    pub fn rgba(width: u32, height: u32, rgba: Vec<u8>, srgb: bool) -> Result<Self, SharedImageError> {
        Self::new(width, height, rgba, None, srgb)
    }

    /// RGBA8 pixels plus an optional cooked chain. The cooked chain wins on a device that can
    /// sample it; the RGBA8 bytes are the fallback and the source a later cook can replace.
    /// An empty `rgba` is allowed only when `compressed` is present (the chain is the image).
    ///
    /// # Errors
    ///
    /// As [`Self::rgba`], and [`SharedImageError::Size`] when both buffers are absent.
    pub fn new(
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        compressed: Option<Arc<CompressedTexture>>,
        srgb: bool,
    ) -> Result<Self, SharedImageError> {
        if width == 0 || height == 0 {
            return Err(SharedImageError::Empty);
        }
        let expected = (width as usize).saturating_mul(height as usize).saturating_mul(4);
        if rgba.is_empty() && compressed.is_none() {
            return Err(SharedImageError::Size {
                width,
                height,
                got: 0,
            });
        }
        if !rgba.is_empty() && rgba.len() < expected {
            return Err(SharedImageError::Size {
                width,
                height,
                got: rgba.len(),
            });
        }
        Ok(Self {
            inner: Arc::new(SharedInner {
                width,
                height,
                rgba: Arc::from(rgba),
                compressed,
                srgb,
                generation: next_generation(),
            }),
        })
    }

    /// Allocation address. Equal for every clone; not a content hash.
    ///
    /// Pair it with [`Self::generation`] before using it as a cache key. The allocator may
    /// recycle the address after the last `Arc` is dropped.
    #[must_use]
    pub fn identity(&self) -> usize {
        concrete_addr(&self.inner)
    }

    /// Creation counter for this allocation. Clones share it. A new image never does, including
    /// one the allocator places at a recycled [`Self::identity`].
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    pub(crate) fn owner(&self) -> Owner {
        self.inner.clone()
    }

    pub(crate) fn share_key(&self) -> ShareKey {
        ShareKey::new(self.identity(), self.generation(), self.is_srgb())
    }

    /// Width and height in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.inner.width, self.inner.height)
    }

    /// Uploaded as sRGB when true.
    #[must_use]
    pub fn is_srgb(&self) -> bool {
        self.inner.srgb
    }

    /// Row-major RGBA8, possibly empty when only a cooked chain is stored.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.inner.rgba
    }

    /// The pixel allocation shared, or `None` when only a cooked chain is stored (then
    /// [`Self::pixels`] is empty). Clones of the returned `Arc` point at these bytes: a
    /// per-frame consumer (the path tracer's scene extract) raises one refcount instead of
    /// copying the image every time.
    #[must_use]
    pub fn pixels_arc(&self) -> Option<Arc<[u8]>> {
        (!self.inner.rgba.is_empty()).then(|| Arc::clone(&self.inner.rgba))
    }

    /// The image has a full RGBA8 level 0.
    #[must_use]
    pub fn has_pixels(&self) -> bool {
        let (w, h) = self.size();
        self.inner.rgba.len() >= (w as usize) * (h as usize) * 4
    }

    /// Cooked chain, when the host attached one.
    #[must_use]
    pub fn compressed(&self) -> Option<Arc<CompressedTexture>> {
        self.inner.compressed.clone()
    }
}

/// A texture whose level 0 the host replaces without building a new material.
///
/// The handle stores the latest RGBA8 bytes only. The renderer that draws a material
/// binding this handle creates the GPU texture (one mip, `COPY_DST`) and copies those
/// bytes in before the draw. The handle holds no [`wgpu::Texture`], so it does not keep
/// a device alive after that renderer is dropped, and a second renderer creates its own
/// texture on its own device and copies the same bytes. A write stays until the next
/// write, and each renderer copies it once. The material's bind group stays across writes.
#[derive(Clone)]
pub struct DynamicTexture {
    inner: Arc<DynamicInner>,
}

struct DynamicInner {
    width: u32,
    height: u32,
    srgb: bool,
    /// Chosen at construction. A later allocation, even at this address, gets another.
    generation: u64,
    /// Latest full image, and how many writes have stored one. The bytes stay so every
    /// renderer can copy them; `serial` is what tells a renderer it already did.
    pending: Mutex<DynamicPending>,
    /// `pending.serial`, stored with [`Ordering::Release`] after the bytes are in place so a
    /// later frame can skip the lock when it already copied that write.
    published: AtomicU64,
}

struct DynamicPending {
    serial: u64,
    /// Shared so a renderer uploads outside the lock without taking the bytes off the handle:
    /// another renderer (or a texture creation) in that window still sees the latest image.
    bytes: Option<std::sync::Arc<[u8]>>,
}

#[cfg(test)]
thread_local! {
    static DYNAMIC_CLONE_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static DYNAMIC_LOCKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static DYNAMIC_LAST_UPLOAD: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn note_dynamic_lock() {
    #[cfg(test)]
    DYNAMIC_LOCKS.with(|count| count.set(count.get().saturating_add(1)));
}

// Test hook: runs once in `copy_into` after the pending lock is released and before the upload.
#[cfg(test)]
thread_local! {
    static DYNAMIC_UPLOAD_GAP: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

fn note_dynamic_upload_gap() {
    #[cfg(test)]
    if let Some(hook) = DYNAMIC_UPLOAD_GAP.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

fn note_dynamic_upload(bytes: &[u8]) {
    #[cfg(test)]
    DYNAMIC_LAST_UPLOAD.with(|slot| *slot.borrow_mut() = bytes.to_vec());
    #[cfg(not(test))]
    let _ = bytes;
}

#[cfg(test)]
fn dynamic_clone_bytes() -> u64 {
    DYNAMIC_CLONE_BYTES.with(std::cell::Cell::get)
}

#[cfg(test)]
fn dynamic_locks() -> u64 {
    DYNAMIC_LOCKS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn dynamic_last_upload() -> Vec<u8> {
    DYNAMIC_LAST_UPLOAD.with(|slot| slot.borrow().clone())
}

#[cfg(test)]
fn reset_dynamic_counts() {
    DYNAMIC_CLONE_BYTES.with(|count| count.set(0));
    DYNAMIC_LOCKS.with(|count| count.set(0));
    DYNAMIC_LAST_UPLOAD.with(|slot| slot.borrow_mut().clear());
}

impl std::fmt::Debug for DynamicTexture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pending = lock(&self.inner.pending);
        f.debug_struct("DynamicTexture")
            .field("width", &self.inner.width)
            .field("height", &self.inner.height)
            .field("srgb", &self.inner.srgb)
            .field("generation", &self.inner.generation)
            .field("pending_bytes", &pending.bytes.as_ref().map(|b| b.len()))
            .finish()
    }
}

/// [`DynamicTexture::write`] was not a full RGBA8 image of the texture's size.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DynamicTextureError {
    /// `rgba` was not `width * height * 4` bytes.
    #[error("dynamic texture write is {got} bytes; {width}x{height} RGBA8 is {expected} bytes")]
    Size {
        /// Bytes the caller passed.
        got: usize,
        /// Bytes a full image of this texture holds.
        expected: usize,
        /// Texture width.
        width: u32,
        /// Texture height.
        height: u32,
    },
}

impl DynamicTexture {
    /// A texture of `width` by `height`. A side of 0 is treated as 1. `srgb` selects the view
    /// format, matching a color slot (`true`) or a data slot (`false`).
    #[must_use]
    pub fn new(width: u32, height: u32, srgb: bool) -> Self {
        Self {
            inner: Arc::new(DynamicInner {
                width: width.max(1),
                height: height.max(1),
                srgb,
                generation: next_generation(),
                pending: Mutex::new(DynamicPending {
                    serial: 0,
                    bytes: None,
                }),
                published: AtomicU64::new(0),
            }),
        }
    }

    /// Width and height the GPU texture is created at.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.inner.width, self.inner.height)
    }

    /// Uploaded as sRGB when true.
    #[must_use]
    pub fn is_srgb(&self) -> bool {
        self.inner.srgb
    }

    /// Allocation address. Equal for every clone; stable across writes.
    ///
    /// Pair it with [`Self::generation`]. The allocator may recycle the address after the
    /// last clone is dropped.
    #[must_use]
    pub fn identity(&self) -> usize {
        concrete_addr(&self.inner)
    }

    /// Creation counter for this allocation. Clones share it. A new texture never does.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    pub(crate) fn owner(&self) -> Owner {
        self.inner.clone()
    }

    pub(crate) fn share_key(&self) -> ShareKey {
        ShareKey::new(self.identity(), self.generation(), self.is_srgb())
    }

    /// Keep `rgba` for the next copy into this texture.
    ///
    /// The renderer performs the copy on the frame that draws the material, before the draw.
    ///
    /// # Errors
    ///
    /// [`DynamicTextureError::Size`] when `rgba` is not a full image.
    pub fn write(&self, rgba: &[u8]) -> Result<(), DynamicTextureError> {
        self.store_pending(rgba)
    }

    /// Keep `rgba` for the renderer's next copy, the same as [`Self::write`].
    ///
    /// `queue` is accepted so a host can pass [`crate::RenderDevice::queue`]. The handle does
    /// not own the GPU texture (the renderer that binds it does, on that renderer's device),
    /// so this method does not copy on `queue`. [`Queue::write_texture`](wgpu::Queue::write_texture)
    /// does not require row padding; `width * 4` is the stride either way.
    ///
    /// # Errors
    ///
    /// [`DynamicTextureError::Size`] when `rgba` is not a full image.
    pub fn write_with_queue(&self, queue: &wgpu::Queue, rgba: &[u8]) -> Result<(), DynamicTextureError> {
        let _ = queue;
        self.store_pending(rgba)
    }

    fn store_pending(&self, rgba: &[u8]) -> Result<(), DynamicTextureError> {
        self.check(rgba)?;
        let mut pending = lock(&self.inner.pending);
        pending.serial = pending.serial.wrapping_add(1);
        pending.bytes = Some(std::sync::Arc::from(rgba));
        self.inner.published.store(pending.serial, Ordering::Release);
        Ok(())
    }

    fn check(&self, rgba: &[u8]) -> Result<(u32, u32), DynamicTextureError> {
        let (width, height) = self.size();
        let expected = (width as usize) * (height as usize) * 4;
        if rgba.len() != expected {
            return Err(DynamicTextureError::Size {
                got: rgba.len(),
                expected,
                width,
                height,
            });
        }
        Ok((width, height))
    }

    /// `(serial, pixels)` of the latest write. `pixels` is black when the host has not written.
    fn snapshot(&self) -> (u64, Vec<u8>) {
        let (width, height) = self.size();
        let pending = lock(&self.inner.pending);
        let pixels = match &pending.bytes {
            Some(bytes) => bytes.to_vec(),
            None => vec![0u8; (width as usize) * (height as usize) * 4],
        };
        (pending.serial, pixels)
    }

    /// Copy a write this renderer has not copied yet. No span and no copy when `applied`
    /// is already the latest serial. The bytes stay on the handle.
    pub(crate) fn copy_into(
        &self,
        queue: &crate::device::TrackedQueue,
        texture: &wgpu::Texture,
        applied: &mut u64,
    ) {
        if *applied == self.inner.published.load(Ordering::Acquire) {
            return;
        }
        note_dynamic_lock();
        let pending = lock(&self.inner.pending);
        if *applied == pending.serial {
            return;
        }
        let Some(pixels) = pending.bytes.clone() else {
            *applied = pending.serial;
            return;
        };
        let serial = pending.serial;
        drop(pending);
        note_dynamic_upload_gap();
        note_dynamic_upload(&pixels);
        let (width, height) = self.size();
        let _span = tracing::info_span!("dynamic_texture_write", width, height).entered();
        queue.write_texture(
            copy_of(texture),
            &pixels,
            row_layout(width, height),
            extent(width, height),
        );
        *applied = serial;
    }

    /// Create the GPU texture and copy the latest write into that first image.
    ///
    /// The caller (the renderer's image cache) owns the returned texture. Nothing is stored
    /// on the handle. The `u64` is the write serial now on the GPU texture.
    pub(crate) fn upload_new(
        &self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
    ) -> (wgpu::Texture, wgpu::TextureView, u64) {
        let (width, height) = self.size();
        let format = if self.inner.srgb {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        // Counted: material RGBA8 uploads go through `&wgpu::Device` and are not, but the
        // dynamic-texture test has to see that later writes do not create another one.
        let tracked = crate::device::TrackedDevice::observe(device);
        let texture = tracked.create_texture(&wgpu::TextureDescriptor {
            label: Some("dynamic material texture"),
            size: extent(width, height),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let (serial, pixels) = self.snapshot();
        {
            let _span = tracing::info_span!("dynamic_texture_write", width, height).entered();
            queue.write_texture(
                copy_of(&texture),
                &pixels,
                row_layout(width, height),
                extent(width, height),
            );
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view, serial)
    }
}

fn lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn extent(width: u32, height: u32) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    }
}

fn row_layout(width: u32, height: u32) -> wgpu::TexelCopyBufferLayout {
    wgpu::TexelCopyBufferLayout {
        offset: 0,
        bytes_per_row: Some(width * 4),
        rows_per_image: Some(height),
    }
}

fn copy_of(texture: &wgpu::Texture) -> wgpu::TexelCopyTextureInfo<'_> {
    wgpu::TexelCopyTextureInfo {
        texture,
        mip_level: 0,
        origin: wgpu::Origin3d::ZERO,
        aspect: wgpu::TextureAspect::All,
    }
}

/// One image a material slot can bind: shared and immutable, or dynamic.
#[derive(Clone, Debug)]
pub enum MaterialImage {
    /// Uploaded once for every material that holds this image.
    Shared(SharedImage),
    /// Rewritten in place. The material `Arc` stays.
    Dynamic(DynamicTexture),
    /// A counted texture asset, uploaded once by [`crate::AssetTextureUploads`] for every
    /// material that holds it (ADR-7330). Draws the fallback while the asset is not loaded.
    Asset(crate::AssetImage),
}

impl MaterialImage {
    pub(crate) fn owner(&self) -> Owner {
        match self {
            Self::Shared(image) => image.owner(),
            Self::Dynamic(texture) => texture.owner(),
            Self::Asset(image) => image.owner(),
        }
    }
}

/// Per-slot images that override the RGBA8 [`crate::LabTexture`] fields.
///
/// A `Some` slot wins over streaming, [`crate::CompressedMaps`] and the RGBA8 map for that
/// slot. `None` leaves today's path alone, so an empty value does not change a frame.
/// A material graph does not read this: it binds [`GraphTexture`](crate::GraphTexture).
#[derive(Clone, Debug, Default)]
pub struct MaterialImages {
    /// Base color. sRGB when the image says so.
    pub albedo: Option<MaterialImage>,
    /// Tangent-space normal, linear.
    pub normal: Option<MaterialImage>,
    /// Occlusion or packed ORM, linear.
    pub occlusion: Option<MaterialImage>,
    /// Emissive, sRGB when the image says so.
    pub emissive: Option<MaterialImage>,
    /// Packed detail albedo (RGB) and roughness (A), linear.
    pub detail: Option<MaterialImage>,
    /// Packed detail normal (RGB) and mask (A), linear.
    pub detail_normal: Option<MaterialImage>,
    /// Packed height (R) and curvature (G), linear.
    pub aux: Option<MaterialImage>,
}

impl MaterialImages {
    /// Every slot empty. The const form of [`Default`].
    pub const EMPTY: Self = Self {
        albedo: None,
        normal: None,
        occlusion: None,
        emissive: None,
        detail: None,
        detail_normal: None,
        aux: None,
    };

    /// Streamed slot `slot` (albedo, normal, occlusion, emissive), if overridden.
    #[must_use]
    pub fn get(&self, slot: usize) -> Option<&MaterialImage> {
        match slot {
            0 => self.albedo.as_ref(),
            1 => self.normal.as_ref(),
            2 => self.occlusion.as_ref(),
            3 => self.emissive.as_ref(),
            _ => None,
        }
    }

    /// Every set slot, in bind order.
    pub fn iter(&self) -> impl Iterator<Item = &MaterialImage> {
        [
            self.albedo.as_ref(),
            self.normal.as_ref(),
            self.occlusion.as_ref(),
            self.emissive.as_ref(),
            self.detail.as_ref(),
            self.detail_normal.as_ref(),
            self.aux.as_ref(),
        ]
        .into_iter()
        .flatten()
    }

    /// Publication serials used by dynamic uploads (ADR-7869). Set-slot order is stable for an
    /// immutable material generation; shared images have no mutable content and contribute zero.
    pub(crate) fn dynamic_write_epochs(&self) -> [u64; 7] {
        let mut epochs = [0; 7];
        for (epoch, image) in epochs.iter_mut().zip(self.iter()) {
            if let MaterialImage::Dynamic(texture) = image {
                *epoch = texture.inner.published.load(Ordering::Acquire);
            }
        }
        epochs
    }
}

/// What [`crate::frame`] passes into a texture bind.
pub(crate) struct UploadEnv<'a> {
    /// Device features (block-compression support).
    pub features: wgpu::Features,
    /// Material cache key, recorded on the texture report.
    pub material_key: u64,
    /// Asset name, when the host set one.
    pub name: Option<&'a std::sync::Arc<str>>,
    /// Frame index.
    pub now: u64,
    /// Device the texture is created on.
    pub device: &'a wgpu::Device,
    /// Queue the first upload is written to.
    pub queue: &'a crate::device::TrackedQueue,
}

/// One bound view, plus where the texture lives for the memory report.
pub(crate) struct SlotBind {
    /// View to put in the material bind group.
    pub view: wgpu::TextureView,
    /// A private upload. Shared and dynamic textures are reported from the cache instead.
    pub fixed: Option<(&'static str, wgpu::Texture)>,
    /// Cache key this bind holds a reference on. Released with the material entry.
    pub share: Option<ShareKey>,
}

/// Identity of one GPU texture: an allocation address, its creation generation, and the view's
/// color space.
///
/// The generation is what makes a recycled address a different key. [`ResidentGpu::owner`] is
/// checked as well, so a key that somehow collides still cannot return the other image.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ShareKey {
    ptr: usize,
    generation: u64,
    srgb: u8,
}

impl ShareKey {
    pub(crate) fn new(ptr: usize, generation: u64, srgb: bool) -> Self {
        Self {
            ptr,
            generation,
            srgb: u8::from(srgb),
        }
    }
}

struct ResidentGpu {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    users: u32,
    slot: &'static str,
    material_key: u64,
    name: Option<std::sync::Arc<str>>,
    last_used: u64,
    /// The allocation `key` names. Held so that address cannot be recycled, and compared
    /// with the caller before the cached view is returned.
    owner: Owner,
    /// Dynamic write already copied into `texture`. Shared images leave this at 0.
    content_serial: u64,
}

/// GPU textures keyed by image identity.
#[derive(Default)]
pub(crate) struct ResidentTextures {
    shares: HashMap<ShareKey, ResidentGpu>,
}

impl ResidentTextures {
    /// The view for `key`, uploading through `upload` only on a miss.
    ///
    /// A hit requires [`Arc::ptr_eq`] with `owner`. The same address and generation with a
    /// different allocation is not a hit: the stale row is dropped and `upload` runs. Callers
    /// prevent that by holding `owner` for the material entry's life; the check is what stops
    /// a recycled address from sampling the old image if that pin is missing.
    pub(crate) fn acquire(
        &mut self,
        key: ShareKey,
        owner: &Owner,
        slot: &'static str,
        env: &UploadEnv<'_>,
        upload: impl FnOnce() -> (wgpu::Texture, wgpu::TextureView),
    ) -> wgpu::TextureView {
        if let Some(gpu) = self.shares.get_mut(&key) {
            if Arc::ptr_eq(&gpu.owner, owner) {
                gpu.users = gpu.users.saturating_add(1);
                gpu.last_used = env.now;
                return gpu.view.clone();
            }
            tracing::warn!(
                target: "regia_render",
                slot,
                key.ptr,
                key.generation,
                "shared texture key collided with a different allocation; replacing the GPU texture"
            );
            self.shares.remove(&key);
        }
        let (texture, view) = upload();
        let out = view.clone();
        self.shares.insert(
            key,
            ResidentGpu {
                texture,
                view,
                users: 1,
                slot,
                material_key: env.material_key,
                name: env.name.cloned(),
                last_used: env.now,
                owner: owner.clone(),
                content_serial: 0,
            },
        );
        out
    }

    /// The GPU texture for `tex`, created now when no material has bound it yet (no user is
    /// counted). A texture view copies its image into it before any material binds it, so a
    /// screen never shows a black first frame. `None` when the key names another allocation.
    pub(crate) fn prepare_dynamic(
        &mut self,
        env: &UploadEnv<'_>,
        tex: &DynamicTexture,
    ) -> Option<wgpu::Texture> {
        let owner = tex.owner();
        let key = tex.share_key();
        if let Some(gpu) = self.shares.get(&key) {
            return Arc::ptr_eq(&gpu.owner, &owner).then(|| gpu.texture.clone());
        }
        let (texture, view, serial) = tex.upload_new(env.device, env.queue);
        self.shares.insert(
            key,
            ResidentGpu {
                texture: texture.clone(),
                view,
                users: 0,
                slot: "texture view",
                material_key: env.material_key,
                name: env.name.cloned(),
                last_used: env.now,
                owner,
                content_serial: serial,
            },
        );
        Some(texture)
    }

    /// Drops the texture [`Self::prepare_dynamic`] created for `tex` if no material uses it.
    pub(crate) fn release_unbound(&mut self, tex: &DynamicTexture) {
        let key = tex.share_key();
        if self
            .shares
            .get(&key)
            .is_some_and(|gpu| gpu.users == 0 && Arc::ptr_eq(&gpu.owner, &tex.owner()))
        {
            self.shares.remove(&key);
        }
    }

    /// Record which dynamic write `upload` just put in the new texture.
    fn set_content_serial(&mut self, key: ShareKey, owner: &Owner, serial: u64) {
        if let Some(gpu) = self.shares.get_mut(&key)
            && Arc::ptr_eq(&gpu.owner, owner)
        {
            gpu.content_serial = serial;
        }
    }

    /// Copy `tex`'s latest write when this row has not copied it yet. True when the row's
    /// content moved to a newer write.
    fn copy_dynamic(
        &mut self,
        key: ShareKey,
        owner: &Owner,
        tex: &DynamicTexture,
        queue: &crate::device::TrackedQueue,
    ) -> bool {
        let published = tex.inner.published.load(Ordering::Acquire);
        let needs_copy = {
            let Some(gpu) = self.shares.get(&key) else {
                return false;
            };
            Arc::ptr_eq(&gpu.owner, owner) && gpu.content_serial != published
        };
        if !needs_copy {
            return false;
        }
        let Some(gpu) = self.shares.get_mut(&key) else {
            return false;
        };
        if !Arc::ptr_eq(&gpu.owner, owner) {
            return false;
        }
        let before = gpu.content_serial;
        tex.copy_into(queue, &gpu.texture, &mut gpu.content_serial);
        gpu.content_serial != before
    }

    /// The resident texture when `key` still names `owner`.
    #[cfg(test)]
    pub(crate) fn texture(&self, key: &ShareKey, owner: &Owner) -> Option<&wgpu::Texture> {
        let gpu = self.shares.get(key)?;
        Arc::ptr_eq(&gpu.owner, owner).then_some(&gpu.texture)
    }

    /// Drop one user. The second release of a missing key does nothing.
    pub(crate) fn release(&mut self, key: ShareKey) {
        let Some(gpu) = self.shares.get_mut(&key) else {
            return;
        };
        gpu.users = gpu.users.saturating_sub(1);
        if gpu.users == 0 {
            self.shares.remove(&key);
        }
    }

    /// The material that shares `keys` was drawn this frame.
    pub(crate) fn touch(&mut self, keys: &[ShareKey], now: u64) {
        for key in keys {
            if let Some(gpu) = self.shares.get_mut(key) {
                gpu.last_used = now;
            }
        }
    }

    /// One report row per resident shared texture.
    pub(crate) fn append_memory(&self, report: &mut crate::TextureMemoryReport) {
        for gpu in self.shares.values() {
            let size = gpu.texture.size();
            report.textures.push(crate::TextureMemoryEntry {
                material_key: gpu.material_key,
                name: gpu.name.as_deref().map(str::to_string),
                slot: gpu.slot,
                width: size.width,
                height: size.height,
                resident_width: size.width,
                resident_height: size.height,
                format: format!("{:?}", gpu.texture.format()),
                compressed: crate::texture_budget::is_compressed(gpu.texture.format()),
                streamed: false,
                gpu_bytes: crate::texture_budget::texture_bytes(
                    gpu.texture.format(),
                    size.width,
                    size.height,
                    gpu.texture.mip_level_count(),
                ),
                cpu_bytes: 0,
                last_seen_frame: gpu.last_used,
            });
        }
    }

    /// Resident shared textures. Tests use this; the memory report is the host-facing count.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shares.len()
    }
}

/// Bind a graph slot. Missing or empty data returns `fallback` (the graph's white view).
pub(crate) fn bind_graph_texture(
    cache: &mut ResidentTextures,
    env: &UploadEnv<'_>,
    tex: &GraphTexture,
    slot: &'static str,
    fallback: &wgpu::TextureView,
) -> SlotBind {
    // A counted texture: the asset server's one upload, in the slot's color space (ADR-8610).
    if let Some(asset) = &tex.asset
        && let Some(view) = asset.view(tex.srgb)
    {
        return SlotBind {
            view,
            fixed: None,
            share: None,
        };
    }
    if let Some(dynamic) = &tex.dynamic {
        return bind_dynamic(cache, env, dynamic, slot);
    }
    if let Some(shared) = &tex.shared
        && let Some(bind) = bind_shared(cache, env, shared, slot)
    {
        return bind;
    }
    if let Some(compressed) = &tex.compressed
        && compressed.supported(env.features)
    {
        return bind_compressed(cache, env, compressed, tex.srgb, slot);
    }
    if rgba_level_ok(&tex.image) {
        let (texture, view) = crate::frame::upload_material_texture(
            env.device,
            env.queue,
            tex.image.width,
            tex.image.height,
            &tex.image.rgba,
            tex.srgb,
        );
        return SlotBind {
            view,
            fixed: Some((slot, texture)),
            share: None,
        };
    }
    if let Some(compressed) = &tex.compressed
        && let Some(chain) = compressed.decode_chain_rgba8()
        && let Some((texture, view)) =
            crate::frame::upload_rgba8_chain(env.device, env.queue, &chain, tex.srgb)
    {
        return SlotBind {
            view,
            fixed: Some((slot, texture)),
            share: None,
        };
    }
    SlotBind {
        view: fallback.clone(),
        fixed: None,
        share: None,
    }
}

/// Bind a [`MaterialImage`] in a slot of color space `srgb`. `None` when a shared image has
/// nothing the device can sample. A shared or dynamic image keeps its own color space; a counted
/// texture binds the view of the slot's (ADR-8610).
pub(crate) fn bind_material_image(
    cache: &mut ResidentTextures,
    env: &UploadEnv<'_>,
    image: &MaterialImage,
    slot: &'static str,
    srgb: bool,
) -> Option<SlotBind> {
    match image {
        MaterialImage::Dynamic(dynamic) => Some(bind_dynamic(cache, env, dynamic, slot)),
        MaterialImage::Shared(shared) => bind_shared(cache, env, shared, slot),
        // The asset server owns and counts the texture; the renderer only binds its view.
        MaterialImage::Asset(asset) => asset.view(srgb).map(|view| SlotBind {
            view,
            fixed: None,
            share: None,
        }),
    }
}

/// Inputs of one standard (non-graph) color or data slot.
pub(crate) struct RgbaSource<'a> {
    /// Shared or dynamic image. Wins when it can be sampled.
    pub image: Option<&'a MaterialImage>,
    /// The material's RGBA8 map.
    pub rgba: Option<&'a crate::LabTexture>,
    /// sRGB view for the RGBA8 map and a counted texture (ADR-8610). A shared image uses its own
    /// flag.
    pub srgb: bool,
    /// Texture-report slot name.
    pub slot: &'static str,
    /// View used when the slot has no image.
    pub fallback: &'a wgpu::TextureView,
}

/// The override, else the stream or the RGBA8 map, else [`RgbaSource::fallback`].
pub(crate) fn bind_rgba_slot(
    cache: &mut ResidentTextures,
    env: &UploadEnv<'_>,
    source: RgbaSource<'_>,
    stream_view: Option<wgpu::TextureView>,
) -> SlotBind {
    if let Some(image) = source.image
        && let Some(bind) = bind_material_image(cache, env, image, source.slot, source.srgb)
    {
        return bind;
    }
    match (stream_view, source.rgba) {
        (Some(view), _) => SlotBind {
            view,
            fixed: None,
            share: None,
        },
        (None, Some(tex)) => {
            let (texture, view) = crate::frame::upload_material_texture(
                env.device,
                env.queue,
                tex.width,
                tex.height,
                &tex.rgba,
                source.srgb,
            );
            SlotBind {
                view,
                fixed: Some((source.slot, texture)),
                share: None,
            }
        }
        (None, None) => SlotBind {
            view: source.fallback.clone(),
            fixed: None,
            share: None,
        },
    }
}

/// Inputs of a packed detail or aux slot.
pub(crate) struct PackedSource<'a> {
    /// Shared or dynamic image. Wins when it can be sampled.
    pub image: Option<&'a MaterialImage>,
    /// Cooked linear chain (BC7 for detail, BC5 for aux). Wins when the device can sample it.
    pub compressed: Option<&'a Arc<CompressedTexture>>,
    /// Packed RGBA8, used when there is no cooked chain the device can sample.
    pub rgba: Option<&'a crate::LabTexture>,
    /// Texture-report slot name.
    pub slot: &'static str,
    /// Neutral view used when the slot has no image.
    pub fallback: &'a wgpu::TextureView,
}

/// A packed detail or aux slot. The cooked chain is linear.
pub(crate) fn bind_packed_slot(
    cache: &mut ResidentTextures,
    env: &UploadEnv<'_>,
    source: PackedSource<'_>,
) -> SlotBind {
    // Packed detail and aux maps are linear.
    if let Some(image) = source.image
        && let Some(bind) = bind_material_image(cache, env, image, source.slot, false)
    {
        return bind;
    }
    if let Some(map) = source.compressed
        && map.supported(env.features)
    {
        return bind_compressed(cache, env, map, false, source.slot);
    }
    bind_rgba_slot(
        cache,
        env,
        RgbaSource {
            image: None,
            rgba: source.rgba,
            srgb: false,
            slot: source.slot,
            fallback: source.fallback,
        },
        None,
    )
}

fn bind_dynamic(
    cache: &mut ResidentTextures,
    env: &UploadEnv<'_>,
    tex: &DynamicTexture,
    slot: &'static str,
) -> SlotBind {
    let owner = tex.owner();
    let key = tex.share_key();
    let mut uploaded = None;
    let view = cache.acquire(key, &owner, slot, env, || {
        let (texture, view, serial) = tex.upload_new(env.device, env.queue);
        uploaded = Some(serial);
        (texture, view)
    });
    if let Some(serial) = uploaded {
        cache.set_content_serial(key, &owner, serial);
    } else {
        cache.copy_dynamic(key, &owner, tex, env.queue);
    }
    SlotBind {
        view,
        fixed: None,
        share: Some(key),
    }
}

fn bind_shared(
    cache: &mut ResidentTextures,
    env: &UploadEnv<'_>,
    image: &SharedImage,
    slot: &'static str,
) -> Option<SlotBind> {
    let srgb = image.is_srgb();
    if let Some(compressed) = image.compressed()
        && compressed.supported(env.features)
    {
        return Some(bind_compressed(cache, env, &compressed, srgb, slot));
    }
    if image.has_pixels() {
        let owner = image.owner();
        let key = image.share_key();
        let (width, height) = image.size();
        let view = cache.acquire(key, &owner, slot, env, || {
            crate::frame::upload_material_texture(env.device, env.queue, width, height, image.pixels(), srgb)
        });
        return Some(SlotBind {
            view,
            fixed: None,
            share: Some(key),
        });
    }
    let compressed = image.compressed()?;
    let chain = compressed.decode_chain_rgba8()?;
    if !rgba_chain_ok(&chain) {
        return None;
    }
    let owner = image.owner();
    let key = image.share_key();
    let view = cache.acquire(key, &owner, slot, env, || {
        crate::frame::upload_rgba8_chain(env.device, env.queue, &chain, srgb).unwrap_or_else(|| {
            crate::frame::upload_material_texture(env.device, env.queue, 1, 1, &[0, 0, 0, 255], srgb)
        })
    });
    Some(SlotBind {
        view,
        fixed: None,
        share: Some(key),
    })
}

fn bind_compressed(
    cache: &mut ResidentTextures,
    env: &UploadEnv<'_>,
    map: &Arc<CompressedTexture>,
    srgb: bool,
    slot: &'static str,
) -> SlotBind {
    // `CompressedTexture` has no generation field (it is `PartialEq` by content). The `Arc`
    // stored on the cache entry is the pin: the address cannot be recycled while the entry
    // lives, and `acquire` rejects the view when the caller is a different allocation.
    let owner: Owner = map.clone();
    let key = ShareKey::new(concrete_addr(&owner), 0, srgb);
    let view = cache.acquire(key, &owner, slot, env, || {
        map.upload(env.device, env.queue, 0, srgb)
    });
    SlotBind {
        view,
        fixed: None,
        share: Some(key),
    }
}

fn rgba_level_ok(image: &crate::LabTexture) -> bool {
    image.width > 0
        && image.height > 0
        && image.rgba.len() >= (image.width as usize) * (image.height as usize) * 4
}

fn rgba_chain_ok(levels: &[crate::LabTexture]) -> bool {
    let Some(top) = levels.first() else {
        return false;
    };
    let (width, height) = (top.width.max(1), top.height.max(1));
    levels.iter().enumerate().all(|(i, level)| {
        let (w, h) = ((width >> i).max(1), (height >> i).max(1));
        level.width == w && level.height == h && level.rgba.len() >= (w as usize) * (h as usize) * 4
    })
}

/// Copy any dynamic slot of `mat` into the texture the cache owns for it. True when a slot took
/// a write it had not copied yet: what the material draws changed under its key (ADR-9603).
pub(crate) fn flush_material_dynamics(
    cache: &mut ResidentTextures,
    queue: &crate::device::TrackedQueue,
    mat: &crate::LabMaterial,
) -> bool {
    let mut copied = false;
    let mut flush_one = |tex: &DynamicTexture| {
        copied |= cache.copy_dynamic(tex.share_key(), &tex.owner(), tex, queue);
    };
    for image in mat.images.iter() {
        if let MaterialImage::Dynamic(tex) = image {
            flush_one(tex);
        }
    }
    if let Some(graph) = mat.surface_graph.as_deref() {
        for slot in &graph.textures {
            if let Some(tex) = slot
                && let Some(dynamic) = &tex.dynamic
            {
                flush_one(dynamic);
            }
        }
    }
    copied
}

/// A stamp of the counted textures `mat` binds (ADR-8610): each one's identity and upload serial,
/// in its standard and graph slots, mixed in slot order; 0 when it binds none. Allocates nothing;
/// one lock of the uploader's table per counted texture.
pub(crate) fn counted_stamp(mat: &crate::LabMaterial) -> u64 {
    let graph = mat
        .surface_graph
        .as_deref()
        .into_iter()
        .flat_map(|graph| graph.textures.iter().flatten())
        .filter_map(|tex| tex.asset.as_ref());
    let standard = mat.images.iter().filter_map(|image| match image {
        MaterialImage::Asset(asset) => Some(asset),
        MaterialImage::Shared(_) | MaterialImage::Dynamic(_) => None,
    });
    let mut stamp = 0u64;
    for asset in standard.chain(graph) {
        // FNV-1a style fold of (identity, serial); `| 1` keeps a counted material off 0.
        for word in [asset.identity() as u64, asset.serial().unwrap_or(0)] {
            stamp = (stamp ^ word).wrapping_mul(0x0000_0100_0000_01B3);
        }
        stamp |= 1;
    }
    stamp
}

/// Allocations whose addresses are mixed into this material's cache key.
///
/// Held by the material entry until it is dropped or evicted, so the allocator cannot recycle
/// one of those addresses into a new image that would hash to the same key and keep the old
/// bind group.
pub(crate) fn pin_material(mat: &crate::LabMaterial) -> Vec<Owner> {
    let mut pins: Vec<Owner> = Vec::new();
    if let Some(maps) = &mat.compressed {
        pins.push(maps.clone());
        if let Some(tex) = &maps.detail {
            pins.push(tex.clone());
        }
        if let Some(tex) = &maps.detail_normal {
            pins.push(tex.clone());
        }
        if let Some(tex) = &maps.aux {
            pins.push(tex.clone());
        }
    }
    for image in mat.images.iter() {
        pins.push(image.owner());
    }
    if let Some(graph) = mat.surface_graph.as_deref() {
        for slot in &graph.textures {
            let Some(tex) = slot else { continue };
            if let Some(shared) = &tex.shared {
                pins.push(shared.owner());
            }
            if let Some(dynamic) = &tex.dynamic {
                pins.push(dynamic.owner());
            }
            if let Some(compressed) = &tex.compressed {
                pins.push(compressed.clone());
            }
            if let Some(asset) = &tex.asset {
                pins.push(asset.owner());
            }
        }
    }
    pins
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_an_identity_and_a_short_buffer_is_rejected() {
        let rgba = vec![1u8, 2, 3, 4];
        let image = SharedImage::rgba(1, 1, rgba.clone(), true).expect("image");
        assert_eq!(image.identity(), image.clone().identity());
        let other = SharedImage::rgba(1, 1, rgba, true).expect("other");
        assert_ne!(image.identity(), other.identity());
        assert_ne!(image.generation(), other.generation());
        assert_eq!(concrete_addr(&image.owner()), image.identity());
        assert!(matches!(
            SharedImage::rgba(2, 2, vec![0; 4], false),
            Err(SharedImageError::Size { got: 4, .. })
        ));
        assert!(matches!(
            SharedImage::rgba(0, 4, Vec::new(), false),
            Err(SharedImageError::Empty)
        ));
    }

    #[test]
    fn a_dynamic_write_rejects_the_wrong_size() {
        let tex = DynamicTexture::new(4, 2, true);
        let err = tex.write(&[0u8; 8]).expect_err("short");
        assert_eq!(
            err,
            DynamicTextureError::Size {
                got: 8,
                expected: 32,
                width: 4,
                height: 2,
            }
        );
        assert!(tex.write(&[9u8; 32]).is_ok());
    }

    fn tiny(device: &wgpu::Device) -> (wgpu::Texture, wgpu::TextureView) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("share test"),
            size: extent(1, 1),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }

    #[test]
    fn acquire_uploads_once_and_the_last_release_drops_it() {
        let device = crate::device::test_device();
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let env = UploadEnv {
            features: device.device.features(),
            material_key: 7,
            name: None,
            now: 3,
            device: &device.device,
            queue: &queue,
        };
        let mut cache = ResidentTextures::default();
        let mut uploads = 0u32;
        let owner: Owner = Arc::new(0xA11u32);
        let key = ShareKey::new(concrete_addr(&owner), 1, false);
        let _first = cache.acquire(key, &owner, "base_color", &env, || {
            uploads += 1;
            tiny(&device.device)
        });
        let _second = cache.acquire(key, &owner, "base_color", &env, || {
            uploads += 1;
            tiny(&device.device)
        });
        assert_eq!(uploads, 1, "the second material reuses the GPU texture");
        assert_eq!(cache.len(), 1);
        cache.release(key);
        assert_eq!(cache.len(), 1, "one material still holds it");
        cache.release(key);
        assert_eq!(cache.len(), 0);
        cache.release(key);
        assert_eq!(cache.len(), 0, "a release past zero is a no-op");
    }

    #[test]
    fn a_reused_address_with_a_new_generation_is_a_different_texture() {
        let device = crate::device::test_device();
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let env = UploadEnv {
            features: device.device.features(),
            material_key: 7,
            name: None,
            now: 3,
            device: &device.device,
            queue: &queue,
        };
        let mut cache = ResidentTextures::default();
        let mut uploads = 0u32;
        let old_owner: Owner = Arc::new(1u8);
        let new_owner: Owner = Arc::new(2u8);
        let old_key = ShareKey::new(0xA11, 1, false);
        let new_key = ShareKey::new(0xA11, 2, false);
        let _old = cache.acquire(old_key, &old_owner, "base_color", &env, || {
            uploads += 1;
            tiny(&device.device)
        });
        let _new = cache.acquire(new_key, &new_owner, "base_color", &env, || {
            uploads += 1;
            tiny(&device.device)
        });
        assert_eq!(uploads, 2, "same address, new generation: upload again");
        assert_eq!(cache.len(), 2);
        cache.release(old_key);
        assert_eq!(cache.len(), 1, "releasing the old image leaves the new one");
        assert!(cache.texture(&new_key, &new_owner).is_some());
        assert!(cache.texture(&new_key, &old_owner).is_none());
    }

    #[test]
    fn a_mismatched_owner_does_not_return_the_cached_texture() {
        let device = crate::device::test_device();
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let env = UploadEnv {
            features: device.device.features(),
            material_key: 7,
            name: None,
            now: 3,
            device: &device.device,
            queue: &queue,
        };
        let mut cache = ResidentTextures::default();
        let mut uploads = 0u32;
        let first: Owner = Arc::new(1u8);
        let second: Owner = Arc::new(2u8);
        let key = ShareKey::new(0xA11, 1, false);
        let _stale = cache.acquire(key, &first, "base_color", &env, || {
            uploads += 1;
            tiny(&device.device)
        });
        let _fresh = cache.acquire(key, &second, "base_color", &env, || {
            uploads += 1;
            tiny(&device.device)
        });
        assert_eq!(uploads, 2, "a different allocation at the same key uploads");
        assert_eq!(cache.len(), 1, "the stale row is replaced");
        assert!(cache.texture(&key, &second).is_some());
        assert!(cache.texture(&key, &first).is_none());
        cache.release(key);
        assert_eq!(
            cache.len(),
            0,
            "one release drops the replacement, not a shared refcount"
        );
    }

    #[test]
    fn a_pinned_image_keeps_its_address_and_a_new_image_gets_a_new_generation() {
        let sample = |byte: u8| SharedImage::rgba(8, 8, vec![byte; 8 * 8 * 4], true).expect("image");
        let control = sample(1);
        let control_addr = control.identity();
        drop(control);
        let mut recycled = false;
        for _ in 0..32 {
            if sample(1).identity() == control_addr {
                recycled = true;
                break;
            }
        }

        let image = sample(7);
        let addr = image.identity();
        let generation = image.generation();
        let mat = crate::LabMaterial {
            images: MaterialImages {
                albedo: Some(MaterialImage::Shared(image.clone())),
                ..MaterialImages::EMPTY
            },
            ..crate::LabMaterial::default()
        };
        let pins = pin_material(&mat);
        assert!(!pins.is_empty());
        drop(mat);
        drop(image);
        for _ in 0..32 {
            let next = sample(8);
            assert_ne!(next.identity(), addr, "the pin must keep this allocation");
            assert_ne!(next.generation(), generation);
        }
        if !recycled {
            eprintln!("allocator did not recycle a dropped SharedImage in the control loop");
        }
        drop(pins);
    }

    #[test]
    fn two_renderers_each_copy_one_write_and_a_repeat_copies_nothing() {
        let device = crate::device::test_device();
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let env = UploadEnv {
            features: device.device.features(),
            material_key: 7,
            name: None,
            now: 3,
            device: &device.device,
            queue: &queue,
        };
        let tex = DynamicTexture::new(1, 1, false);
        tex.write(&[1, 2, 3, 4]).expect("write");
        let before = crate::frame_stats::counters();
        let mut first = ResidentTextures::default();
        let mut second = ResidentTextures::default();
        let _ = bind_dynamic(&mut first, &env, &tex, "emissive");
        let _ = bind_dynamic(&mut second, &env, &tex, "emissive");
        let once = crate::frame_stats::counters().since(before);
        assert_eq!(once.upload_bytes, 8, "each renderer copies the 4-byte write");
        let _ = bind_dynamic(&mut first, &env, &tex, "emissive");
        let repeated = crate::frame_stats::counters().since(before);
        assert_eq!(
            repeated.upload_bytes, 8,
            "a renderer that already copied this write does not copy it again"
        );
        tex.write(&[5, 6, 7, 8]).expect("second write");
        let _ = bind_dynamic(&mut first, &env, &tex, "emissive");
        let _ = bind_dynamic(&mut second, &env, &tex, "emissive");
        let next = crate::frame_stats::counters().since(before);
        assert_eq!(
            next.upload_bytes, 16,
            "the next write is copied once per renderer"
        );
    }

    /// While one renderer uploads outside the lock, a second renderer's copy and a new texture's
    /// first image still see the written bytes (they were taken off the handle for the upload,
    /// so the second copy was marked done without uploading and the snapshot was black).
    #[test]
    fn a_second_renderer_inside_an_upload_still_copies_the_write() {
        let device = crate::device::test_device();
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let tex = DynamicTexture::new(4, 2, false);
        let image: Vec<u8> = (0..32).map(|i| i as u8 + 1).collect();
        tex.write(&image).expect("write");
        let (first_texture, _, _) = tex.upload_new(&device.device, &queue);
        let (second_texture, _, _) = tex.upload_new(&device.device, &queue);
        let next: Vec<u8> = (0..32).map(|i| 200 - i as u8).collect();
        tex.write(&next).expect("next write");
        let seen = std::rc::Rc::new(std::cell::RefCell::new(None));
        {
            let (tex, seen, queue_ref) = (tex.clone(), std::rc::Rc::clone(&seen), device.queue.clone());
            let second = second_texture.clone();
            DYNAMIC_UPLOAD_GAP.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    let queue = crate::device::TrackedQueue::observe(&queue_ref);
                    let (_, snapshot) = tex.snapshot();
                    DYNAMIC_LAST_UPLOAD.with(|last| last.borrow_mut().clear());
                    let mut applied = 1;
                    tex.copy_into(&queue, &second, &mut applied);
                    *seen.borrow_mut() = Some((snapshot, dynamic_last_upload(), applied));
                }));
            });
        }
        let mut applied = 1;
        tex.copy_into(&queue, &first_texture, &mut applied);
        assert_eq!(applied, 2);
        let (snapshot, uploaded, applied_second) = seen.borrow_mut().take().expect("the gap hook ran");
        assert_eq!(
            snapshot, next,
            "a texture made during the upload got a black first image"
        );
        assert_eq!(applied_second, 2);
        assert_eq!(
            uploaded, next,
            "the second renderer was marked done without the upload"
        );
    }

    #[test]
    fn one_dynamic_write_is_uploaded_once_across_two_flushes() {
        let device = crate::device::test_device();
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let env = UploadEnv {
            features: device.device.features(),
            material_key: 11,
            name: None,
            now: 1,
            device: &device.device,
            queue: &queue,
        };
        let tex = DynamicTexture::new(4, 2, false);
        let first = vec![1u8; 32];
        tex.write(&first).expect("first write");
        let mut cache = ResidentTextures::default();
        let _ = bind_dynamic(&mut cache, &env, &tex, "albedo");
        let image: Vec<u8> = (0..32).map(|i| i as u8).collect();
        tex.write(&image).expect("frame write");
        let mat = crate::LabMaterial {
            images: MaterialImages {
                albedo: Some(MaterialImage::Dynamic(tex.clone())),
                ..MaterialImages::EMPTY
            },
            ..crate::LabMaterial::default()
        };
        reset_dynamic_counts();
        let before = crate::frame_stats::counters();
        flush_material_dynamics(&mut cache, &queue, &mat);
        let mid_locks = dynamic_locks();
        flush_material_dynamics(&mut cache, &queue, &mat);
        let uploaded = crate::frame_stats::counters().since(before);
        assert_eq!(uploaded.upload_bytes, 32, "two flushes upload the image once");
        assert_eq!(
            dynamic_clone_bytes(),
            0,
            "the pending image was cloned for the upload"
        );
        assert!(mid_locks > 0, "the first flush takes the pending lock");
        assert_eq!(
            dynamic_locks(),
            mid_locks,
            "the second flush locked the pending image again"
        );
        assert_eq!(
            dynamic_last_upload(),
            image,
            "the uploaded bytes are the written image"
        );
    }
}
