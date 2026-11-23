// SPDX-License-Identifier: MPL-2.0
//! GPU upload of counted textures (`docs/specs/12-assets.md` § 3, ADR-7330).
//!
//! [`AssetTextureUploads`] uploads counted `regia_asset::CookedTexture` (KTX2 block-compressed
//! chains) and `regia_asset::CookedImage` (RGBA8) loads once each, through two
//! [`regia_asset::PrepareAsset`] views ([`AssetTextureUploads::cooked_textures`],
//! [`AssetTextureUploads::images`]) that share one table. The asset server counts each texture's
//! bytes once (per-type budget, `assets.handles`) and drops the CPU copy after the upload unless the
//! load kept it; when the asset unloads (its last handle dropped and the grace frames ran out) the
//! texture is released (a bind group that still holds its view keeps it until that group goes).
//!
//! A material slot samples it through [`AssetImage`] ([`crate::MaterialImage::Asset`]): every
//! material holding the same counted handle binds the one texture, instead of the renderer's own
//! upload keyed by the material's `Arc`. The texture is uploaded once with a sibling view format
//! (wgpu `view_formats`: one texture, an sRGB and a linear view, ADR-8610), and each slot binds the
//! view of its own color space: a base-color slot the sRGB view, a mask or ORM slot the linear one,
//! so one counted image in both kinds of slot samples like the CPU path's two uploads. The mip
//! chain is filtered once, in the space chosen before the load with
//! [`AssetTextureUploads::set_srgb`] (the counted cooked-model path sets it from the slots that
//! name the texture; the default is linear): level 0 is exact in both views, the smaller levels are
//! exact in that space. A device without view formats (GLES, WebGL2) or a format with no sRGB
//! variant (BC4, BC5) has one view, in the upload's space, for every slot.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use regia_asset::{AssetId, CookedImage, CookedTexture, PrepareAsset, StrongHandle};

struct ResidentTexture {
    texture: wgpu::Texture,
    /// The linear view (`[0]`) and the sRGB view (`[1]`); the same view twice when the texture has
    /// no sibling format.
    views: [wgpu::TextureView; 2],
    /// The two views differ (the texture has a sibling format).
    sibling: bool,
    serial: u64,
}

#[derive(Default)]
struct Table {
    resident: HashMap<AssetId, ResidentTexture>,
    srgb: HashSet<AssetId>,
    uploads: u64,
    frees: u64,
}

/// Uploads counted textures and images and frees them when they unload. Cheap to clone: clones
/// share the device, queue and table.
#[derive(Clone)]
pub struct AssetTextureUploads {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// The device creates views in a texture's other color space (`DownlevelFlags::VIEW_FORMATS`).
    view_formats: bool,
    table: Arc<Mutex<Table>>,
}

impl std::fmt::Debug for AssetTextureUploads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssetTextureUploads")
            .field("resident", &self.resident_count())
            .finish_non_exhaustive()
    }
}

/// The [`PrepareAsset`] of counted [`CookedTexture`] loads ([`AssetTextureUploads::cooked_textures`]).
#[derive(Clone, Debug)]
pub struct AssetCookedTexturePrepare(AssetTextureUploads);

/// The [`PrepareAsset`] of counted [`CookedImage`] loads ([`AssetTextureUploads::images`]).
#[derive(Clone, Debug)]
pub struct AssetImagePrepare(AssetTextureUploads);

impl AssetTextureUploads {
    /// An uploader on `device`'s device and queue.
    #[must_use]
    pub fn new(device: &crate::RenderDevice) -> Self {
        let view_formats = device
            .adapter
            .get_downlevel_capabilities()
            .flags
            .contains(wgpu::DownlevelFlags::VIEW_FORMATS);
        Self {
            device: device.device.clone(),
            queue: device.queue.clone(),
            view_formats,
            table: Arc::default(),
        }
    }

    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        // A poisoned lock means a panic elsewhere while holding it; the table itself stays valid.
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The preparer to register for counted `CookedTexture` loads.
    #[must_use]
    pub fn cooked_textures(&self) -> AssetCookedTexturePrepare {
        AssetCookedTexturePrepare(self.clone())
    }

    /// The preparer to register for counted `CookedImage` loads.
    #[must_use]
    pub fn images(&self) -> AssetImagePrepare {
        AssetImagePrepare(self.clone())
    }

    /// Filters texture `id`'s mip chain for sRGB sampling (color slots: base color, emissive) from
    /// its next load or reload on; `false` (the default) for linear data (normal, occlusion,
    /// masks). Every slot still binds the view of its own color space (ADR-8610); this picks the
    /// space the smaller mip levels are exact in.
    pub fn set_srgb(&self, id: AssetId, srgb: bool) {
        let mut table = self.table();
        if srgb {
            table.srgb.insert(id);
        } else {
            table.srgb.remove(&id);
        }
    }

    /// The view of texture `id` a slot of color space `srgb` samples, while it is loaded: the sRGB
    /// view for a color slot, the linear view for a data slot (one view for both when the texture
    /// has no sibling format, ADR-8610).
    #[must_use]
    pub fn view(&self, id: AssetId, srgb: bool) -> Option<wgpu::TextureView> {
        self.table()
            .resident
            .get(&id)
            .map(|r| r.views[usize::from(srgb)].clone())
    }

    /// Whether texture `id` has a view in each color space (`false` while it is not loaded, on a
    /// device without view formats, or for a format with no sRGB variant).
    #[must_use]
    pub fn has_sibling_views(&self, id: AssetId) -> bool {
        self.table().resident.get(&id).is_some_and(|r| r.sibling)
    }

    /// The upload serial of texture `id` (a reload uploads again), while it is loaded.
    #[must_use]
    pub fn serial(&self, id: AssetId) -> Option<u64> {
        self.table().resident.get(&id).map(|r| r.serial)
    }

    /// Resident textures with a view in each color space (ADR-8610).
    #[must_use]
    pub fn sibling_view_count(&self) -> usize {
        self.table().resident.values().filter(|r| r.sibling).count()
    }

    /// Textures on the GPU.
    #[must_use]
    pub fn resident_count(&self) -> usize {
        self.table().resident.len()
    }

    /// Bytes of every resident texture (all levels).
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        self.table()
            .resident
            .values()
            .map(|r| texture_bytes(&r.texture))
            .sum()
    }

    /// Uploads so far (a reload uploads again) and frees so far.
    #[must_use]
    pub fn counters(&self) -> (u64, u64) {
        let t = self.table();
        (t.uploads, t.frees)
    }

    /// Hands the GPU work queued by uploads to the device (tests; a frame submits it anyway).
    pub fn flush(&self) {
        self.queue.submit(std::iter::empty());
    }

    fn srgb_of(&self, id: AssetId) -> bool {
        self.table().srgb.contains(&id)
    }

    /// Stores `texture` as `id`'s GPU copy, replacing a reload's old one; returns its bytes.
    /// `view` is the upload's own view; the other color space's comes from the texture's sibling
    /// format when it has one.
    fn store(&self, id: AssetId, texture: wgpu::Texture, view: wgpu::TextureView, srgb: bool) -> u64 {
        let bytes = texture_bytes(&texture);
        let (views, sibling) = sibling_views(&texture, view, srgb, self.view_formats);
        let mut table = self.table();
        table.uploads += 1;
        let serial = table.uploads;
        // A reload replaces the old copy; it is dropped, not destroyed (a bind group may hold it).
        table.resident.insert(
            id,
            ResidentTexture {
                texture,
                views,
                sibling,
                serial,
            },
        );
        bytes
    }

    fn release(&self, id: AssetId) {
        let mut table = self.table();
        if table.resident.remove(&id).is_some() {
            table.frees += 1;
        }
    }
}

/// The `[linear, sRGB]` views of `texture`: `view` (the upload's own, in the `srgb` space) in its
/// slot, and a view in the other space when the upload declared that sibling format
/// (`siblings`, [`crate::frame::sibling_view_format`]); `view` twice when there is none.
fn sibling_views(
    texture: &wgpu::Texture,
    view: wgpu::TextureView,
    srgb: bool,
    siblings: bool,
) -> ([wgpu::TextureView; 2], bool) {
    let own = if srgb {
        texture.format().add_srgb_suffix()
    } else {
        texture.format().remove_srgb_suffix()
    };
    let Some(other) = crate::frame::sibling_view_format(own, siblings) else {
        return ([view.clone(), view], false);
    };
    let other = texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some("counted texture sibling view"),
        format: Some(other),
        ..Default::default()
    });
    (if srgb { [other, view] } else { [view, other] }, true)
}

/// GPU bytes of every level of `texture`.
fn texture_bytes(texture: &wgpu::Texture) -> u64 {
    let size = texture.size();
    crate::texture_budget::texture_bytes(
        texture.format(),
        size.width,
        size.height,
        texture.mip_level_count(),
    )
}

impl PrepareAsset for AssetCookedTexturePrepare {
    type Asset = CookedTexture;

    fn prepare(&self, id: AssetId, asset: &CookedTexture) -> Result<u64, String> {
        let _span = tracing::info_span!("asset_texture_upload").entered();
        let uploads = &self.0;
        let srgb = uploads.srgb_of(id);
        let compressed = crate::compressed_texture::CompressedTexture::from_cooked(asset)
            .map_err(|err| format!("texture {id} is not a format the renderer samples: {err}"))?;
        let queue = crate::device::TrackedQueue::observe(&uploads.queue);
        let siblings = uploads.view_formats;
        let (texture, view) = if compressed.supported(uploads.device.features()) {
            compressed.upload_views(&uploads.device, &queue, 0, srgb, siblings)
        } else {
            let chain = compressed.decode_chain_rgba8().ok_or_else(|| {
                format!("texture {id}: the device has no block compression and the chain does not decode")
            })?;
            crate::frame::upload_rgba8_chain_views(&uploads.device, &queue, &chain, srgb, siblings)
                .ok_or_else(|| format!("texture {id}: its decoded levels do not form a chain"))?
        };
        Ok(uploads.store(id, texture, view, srgb))
    }

    fn unload(&self, id: AssetId) {
        self.0.release(id);
    }
}

impl PrepareAsset for AssetImagePrepare {
    type Asset = CookedImage;

    fn prepare(&self, id: AssetId, image: &CookedImage) -> Result<u64, String> {
        let _span = tracing::info_span!("asset_image_upload").entered();
        let uploads = &self.0;
        let (width, height) = (image.width, image.height);
        let need = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4));
        if width == 0 || height == 0 || need.is_none_or(|n| image.rgba8.len() < n) {
            return Err(format!("image {id} has no pixels for {width}x{height}"));
        }
        let srgb = uploads.srgb_of(id);
        let queue = crate::device::TrackedQueue::observe(&uploads.queue);
        // The CPU path's upload (`LabTexture` slots): the same mip chain, filtered in `srgb`.
        let (texture, view) = crate::frame::upload_material_texture_views(
            &uploads.device,
            &queue,
            width,
            height,
            &image.rgba8,
            srgb,
            uploads.view_formats,
        );
        Ok(uploads.store(id, texture, view, srgb))
    }

    fn unload(&self, id: AssetId) {
        self.0.release(id);
    }
}

/// A counted texture in a material slot ([`crate::MaterialImage::Asset`], ADR-7330): it holds
/// the asset's handle (the texture stays loaded while a material holds it) and binds the one GPU
/// texture [`AssetTextureUploads`] made for it. Clones share one identity.
#[derive(Clone)]
pub struct AssetImage {
    inner: Arc<AssetImageInner>,
}

/// What an [`AssetImage`] reads from its uploader. A trait object, so the auto traits of a
/// material holding the image stop here instead of walking wgpu's device types (a material graph
/// slot nests it one level deeper, past the compiler's recursion limit in crates that hold
/// materials, ADR-8610).
trait CountedTextureLookup: Send + Sync {
    fn view(&self, id: AssetId, srgb: bool) -> Option<wgpu::TextureView>;
    fn serial(&self, id: AssetId) -> Option<u64>;
}

impl CountedTextureLookup for AssetTextureUploads {
    fn view(&self, id: AssetId, srgb: bool) -> Option<wgpu::TextureView> {
        AssetTextureUploads::view(self, id, srgb)
    }

    fn serial(&self, id: AssetId) -> Option<u64> {
        AssetTextureUploads::serial(self, id)
    }
}

struct AssetImageInner {
    id: AssetId,
    uploads: Box<dyn CountedTextureLookup>,
    /// Kept for its count: a `StrongHandle<CookedTexture>` or `StrongHandle<CookedImage>`.
    _handle: Box<dyn std::any::Any + Send + Sync>,
}

impl std::fmt::Debug for AssetImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssetImage")
            .field("id", &self.inner.id)
            .finish_non_exhaustive()
    }
}

impl AssetImage {
    /// A counted KTX2 texture (`load::<CookedTexture>`) drawn from `uploads`.
    #[must_use]
    pub fn texture(handle: StrongHandle<CookedTexture>, uploads: &AssetTextureUploads) -> Self {
        Self::new(handle.id(), Box::new(handle), uploads)
    }

    /// A counted RGBA8 image (`load::<CookedImage>`) drawn from `uploads`.
    #[must_use]
    pub fn image(handle: StrongHandle<CookedImage>, uploads: &AssetTextureUploads) -> Self {
        Self::new(handle.id(), Box::new(handle), uploads)
    }

    fn new(id: AssetId, handle: Box<dyn std::any::Any + Send + Sync>, uploads: &AssetTextureUploads) -> Self {
        Self {
            inner: Arc::new(AssetImageInner {
                id,
                uploads: Box::new(uploads.clone()),
                _handle: handle,
            }),
        }
    }

    /// The counted asset.
    #[must_use]
    pub fn id(&self) -> AssetId {
        self.inner.id
    }

    /// The upload serial while the texture is loaded (a material's bind group follows it).
    #[must_use]
    pub fn serial(&self) -> Option<u64> {
        self.inner.uploads.serial(self.inner.id)
    }

    /// The view a slot of color space `srgb` binds, while the texture is loaded (ADR-8610).
    pub(crate) fn view(&self, srgb: bool) -> Option<wgpu::TextureView> {
        self.inner.uploads.view(self.inner.id, srgb)
    }

    /// The allocation a material entry pins (keeps the handle, so the asset, alive while the
    /// renderer's entry exists).
    pub(crate) fn owner(&self) -> crate::texture_residency::Owner {
        self.inner.clone()
    }

    /// Address of the shared allocation (material-key identity).
    #[must_use]
    pub fn identity(&self) -> usize {
        Arc::as_ptr(&self.inner) as *const () as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-8610: a counted image is one texture with a view in each color space where the device
    /// has view formats (one view for both where it has not), whichever space its mip chain was
    /// filtered in; the bytes are one texture's.
    #[test]
    fn asset_texture_upload_makes_one_texture_with_a_view_in_each_color_space() {
        let Ok(device) = crate::RenderDevice::shared_headless() else {
            eprintln!("SKIP: no adapter");
            return;
        };
        let uploads = AssetTextureUploads::new(&device);
        let image = CookedImage {
            width: 4,
            height: 4,
            rgba8: [200u8, 90, 30, 255].repeat(16),
        };
        for (n, srgb) in [false, true].into_iter().enumerate() {
            let id = AssetId::new_random();
            uploads.set_srgb(id, srgb);
            let bytes = uploads.images().prepare(id, &image).expect("upload");
            assert_eq!(bytes, (16 + 4 + 1) * 4, "one 4 × 4 RGBA8 chain");
            assert!(uploads.view(id, true).is_some() && uploads.view(id, false).is_some());
            assert_eq!(
                uploads.has_sibling_views(id),
                uploads.view_formats,
                "a view in each space exactly where the device has view formats"
            );
            assert_eq!(uploads.resident_count(), n + 1);
        }
        assert_eq!(
            uploads.sibling_view_count(),
            if uploads.view_formats { 2 } else { 0 }
        );
        assert_eq!(uploads.counters(), (2, 0));
        assert_eq!(uploads.resident_bytes(), 2 * (16 + 4 + 1) * 4);
    }
}
