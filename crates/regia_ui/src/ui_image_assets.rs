// SPDX-License-Identifier: MPL-2.0
//! Named pictures in UI documents (ADR-3148; `specs/18` § 5, `specs/81` § 4.6): a `.ui.ron` `image` or
//! `mask` names its picture by asset path (`asset: "assets/ui/panel.png"`) or by icon name
//! (`icon: "gear"`) instead of a renderer image id, so a designer-made document needs no game code.
//!
//! - [`UiImageAsset`] (spawned from an `image` with `asset` or `icon`), [`crate::UiMaskAsset`] and
//!   [`UiMaskIcon`] (from a `mask`) name the picture.
//! - [`resolve_ui_image_assets`] (`PostUpdate`, before layout) resolves them. An asset path goes
//!   through the [`AssetServer`]'s manifest; the cooked file is read and decoded on the async task pool
//!   ([`regia_core::tasks::spawn`]), never on the frame thread. A cooked RGBA8 image is used as is, a
//!   block-compressed texture is decoded to RGBA8 on the CPU. Until the load lands the node draws the
//!   placeholder ([`UI_IMAGE_PLACEHOLDER`]); then the picture, registered with the UI renderer under
//!   an id derived from the asset id ([`asset_image_id`]). An icon resolves through the
//!   [`IconAtlas`] at once: the drawing whose size best fits the node's laid-out height.
//! - The node holds a [`UiImageHandle`] (masks a [`UiMaskHandle`]): the asset id, the renderer image,
//!   the status and, after a failure, the error code.
//! - Hot reload: when the asset server reloads a held asset (its generation moves), the file is
//!   read again in the background and the new pixels replace the old under the same image id.
//! - A missing asset, an asset of the wrong kind, a decode error or an unknown icon logs one warning
//!   (per picture and code) and draws the fallback ([`UI_IMAGE_FALLBACK`]); a mask that cannot
//!   resolve leaves the node unmasked. Nothing panics.
//!
//! Error codes: `ui_image_not_found`, `ui_image_wrong_kind`, `ui_image_load_failed`,
//! `ui_image_icon_missing`, `ui_image_no_asset_server` (book `agents/ui-image-assets.md`). The cook
//! side (`regia assets validate`) reports a path no asset matches as `ui_image_missing`.
//!
//! Agent method: `ui.image_assets` ([`agent_image_assets`]).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

use regia_app::prelude::*;
use regia_asset::{AssetId, AssetServer, AssetTypeTag, PayloadReader};
use serde_json::{Value, json};

use crate::authoring::{UiFile, UiNodeDef};
use crate::color::UiColor;
use crate::group_layer::{MaskFit, UiMask, UiMaskAsset};
use crate::icons::{IconAtlas, pick_variant};
use crate::node::{Background, BackgroundKind, ComputedNode, UiImage};
use crate::render::SharedUiRenderer;

/// Renderer image id of the built-in placeholder drawn while a picture loads (a faint grey).
pub const UI_IMAGE_PLACEHOLDER: u64 = 0x1C1_0000_0000;
/// Renderer image id of the built-in fallback drawn for a picture that cannot load (a magenta and
/// black checker, so a broken reference is easy to spot).
pub const UI_IMAGE_FALLBACK: u64 = 0x1C1_0000_0001;

/// Every error code the run time reports for a named picture.
pub const UI_IMAGE_ERROR_CODES: &[&str] = &[
    "ui_image_not_found",
    "ui_image_wrong_kind",
    "ui_image_load_failed",
    "ui_image_icon_missing",
    "ui_image_no_asset_server",
];

/// The full UV rect.
const FULL_UV: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// The renderer image id of the picture of asset `asset`: derived from the asset id, with the top
/// bit set to stay out of the small ids games pick by hand (the same id a mask of that asset uses).
#[must_use]
pub fn asset_image_id(asset: AssetId) -> u64 {
    let guid = asset.0.0;
    ((guid >> 64) as u64 ^ guid as u64) | (1 << 63)
}

/// What a named picture points at.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UiImageSource {
    /// Manifest path of a cooked image or texture (`"assets/ui/panel.png"`).
    Asset(String),
    /// Name of an icon in the [`IconAtlas`] (without `@size`).
    Icon(String),
}

impl UiImageSource {
    /// `"asset"` or `"icon"`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Asset(_) => "asset",
            Self::Icon(_) => "icon",
        }
    }

    /// The path or the icon name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Asset(s) | Self::Icon(s) => s,
        }
    }
}

/// A node's image named by asset path or icon name (a `.ui.ron` `image` with `asset` or `icon`).
/// [`resolve_ui_image_assets`] writes the node's [`UiImage`] (or, with `nine_slice`, its
/// [`Background`]) from it.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct UiImageAsset {
    /// The picture.
    pub source: UiImageSource,
    /// Multiplicative tint (not applied with `nine_slice`).
    pub tint: UiColor,
    /// Keep the picture's aspect ratio inside the rect (not applied with `nine_slice`).
    pub preserve_aspect: bool,
    /// Nine-slice insets (left, top, right, bottom) in source pixels: the picture then fills the
    /// node as its background.
    pub nine_slice: Option<[f32; 4]>,
}

impl UiImageAsset {
    /// The cooked image at the manifest path `path`, untinted, aspect kept.
    #[must_use]
    pub fn asset(path: impl Into<String>) -> Self {
        Self::of(UiImageSource::Asset(path.into()))
    }

    /// The icon `name` of the [`IconAtlas`], untinted, aspect kept.
    #[must_use]
    pub fn icon(name: impl Into<String>) -> Self {
        Self::of(UiImageSource::Icon(name.into()))
    }

    fn of(source: UiImageSource) -> Self {
        Self {
            source,
            tint: UiColor::WHITE,
            preserve_aspect: true,
            nine_slice: None,
        }
    }

    /// With nine-slice insets (left, top, right, bottom) in source pixels.
    #[must_use]
    pub fn with_nine_slice(mut self, insets: [f32; 4]) -> Self {
        self.nine_slice = Some(insets);
        self
    }

    /// The [`UiImage`] drawn before the picture resolves: the placeholder, stretched over the node.
    #[must_use]
    pub fn placeholder_image(&self) -> UiImage {
        UiImage {
            id: UI_IMAGE_PLACEHOLDER,
            uv: FULL_UV,
            tint: UiColor::WHITE,
            preserve_aspect: false,
        }
    }
}

/// A node's mask named by icon (a `.ui.ron` `mask` with `icon`): [`resolve_ui_image_assets`] turns it
/// into a [`UiMask`] on the icon's page and UV rect.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct UiMaskIcon {
    /// The icon name.
    pub name: String,
    /// Placement in the node's rect.
    pub fit: MaskFit,
}

impl UiMaskIcon {
    /// The icon `name`, stretched over the node.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            fit: MaskFit::Stretch,
        }
    }

    /// With a placement mode.
    #[must_use]
    pub fn with_fit(mut self, fit: MaskFit) -> Self {
        self.fit = fit;
        self
    }
}

/// Where a named picture is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UiImageStatus {
    /// Loading in the background: the placeholder draws.
    #[default]
    Loading,
    /// Drawn.
    Ready,
    /// Could not load (the code says why): the fallback draws.
    Failed,
}

impl UiImageStatus {
    /// `"loading"`, `"ready"` or `"failed"`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

/// What a node's named picture resolved to.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UiPictureState {
    /// The asset held (asset paths only).
    pub asset: Option<AssetId>,
    /// Renderer image id drawn (the placeholder or fallback until ready).
    pub image: u64,
    /// UV rect on that image.
    pub uv: [f32; 4],
    /// Loading, ready or failed.
    pub status: UiImageStatus,
    /// The error code after a failure.
    pub code: Option<&'static str>,
    /// How many times the picture was replaced by a hot reload.
    pub reloads: u64,
    /// Drawn as the node's nine-slice background.
    pub as_background: bool,
}

/// The handle of a node's [`UiImageAsset`]: holds the asset and reports its state.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct UiImageHandle(pub UiPictureState);

/// The handle of a node's [`UiMaskAsset`] or [`UiMaskIcon`].
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct UiMaskHandle(pub UiPictureState);

/// Counters of the named-picture loader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UiImageAssetStats {
    /// Background loads started (first loads and reloads).
    pub loads_started: u64,
    /// Background loads that finished (either way).
    pub loads_finished: u64,
    /// Loads that failed.
    pub failures: u64,
    /// Hot reloads started.
    pub reloads: u64,
    /// Loads still running.
    pub in_flight: u64,
    /// Distinct assets known.
    pub assets: u64,
}

/// A decoded picture or why it failed.
type Decoded = Result<(u32, u32, Vec<u8>), (&'static str, String)>;

/// One finished background load.
struct LoadResult {
    id: AssetId,
    decoded: Decoded,
}

#[derive(Debug, Clone, PartialEq)]
enum EntryState {
    Loading,
    Ready,
    Failed { code: &'static str, message: String },
}

/// One asset the loader knows.
#[derive(Debug)]
struct Entry {
    path: String,
    image: u64,
    state: EntryState,
    /// A load of it is running.
    loading: bool,
    /// The asset server generation the last load read.
    generation: u64,
    /// Hot reloads applied.
    reloads: u64,
    size: Option<[u32; 2]>,
}

/// The named-picture loader: the assets it holds, the background loads in flight and the warnings
/// already logged. [`crate::UiPlugin`] inserts it.
#[derive(Resource)]
pub struct UiImageAssets {
    entries: HashMap<AssetId, Entry>,
    /// Manifest lookups: `None` when the manifest has no such path.
    paths: HashMap<String, Option<AssetId>>,
    tx: Sender<LoadResult>,
    rx: Mutex<Receiver<LoadResult>>,
    /// Results received by [`Self::finish_loads`], applied by the next resolve.
    received: Vec<LoadResult>,
    /// Bumped whenever an entry changes state, so nodes re-resolve.
    revision: u64,
    /// `(kind, name, code)` already logged.
    logged: HashSet<(&'static str, String, &'static str)>,
    builtins_queued: bool,
    atlas_generation: u64,
    stats: UiImageAssetStats,
    /// Renderer image drawn while a picture loads ([`UI_IMAGE_PLACEHOLDER`] by default; a game may
    /// register its own image and put its id here).
    pub placeholder: u64,
    /// Renderer image drawn for a picture that cannot load ([`UI_IMAGE_FALLBACK`] by default).
    pub fallback: u64,
}

impl Default for UiImageAssets {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            entries: HashMap::new(),
            paths: HashMap::new(),
            tx,
            rx: Mutex::new(rx),
            received: Vec::new(),
            revision: 0,
            logged: HashSet::new(),
            builtins_queued: false,
            atlas_generation: u64::MAX,
            stats: UiImageAssetStats::default(),
            placeholder: UI_IMAGE_PLACEHOLDER,
            fallback: UI_IMAGE_FALLBACK,
        }
    }
}

impl std::fmt::Debug for UiImageAssets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiImageAssets")
            .field("entries", &self.entries.len())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl UiImageAssets {
    /// The loader's counters.
    #[must_use]
    pub fn stats(&self) -> UiImageAssetStats {
        UiImageAssetStats {
            assets: self.entries.len() as u64,
            ..self.stats
        }
    }

    /// Background loads still running.
    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.stats.in_flight
    }

    /// How many distinct warnings (picture and code) were logged so far: each is logged once.
    #[must_use]
    pub fn warnings_logged(&self) -> usize {
        self.logged.len()
    }

    /// Waits until every background load started so far has finished; the next
    /// [`resolve_ui_image_assets`] applies them. For tools that need a finished picture in a fixed
    /// number of updates (tests, the designer's one-shot preview, captures); a game never calls it.
    pub fn finish_loads(&mut self) {
        let _span = tracing::debug_span!("ui_image_finish_loads").entered();
        let mut waiting =
            self.stats.in_flight as usize - self.received.len().min(self.stats.in_flight as usize);
        let Ok(rx) = self.rx.lock() else {
            return;
        };
        while waiting > 0 {
            // Every task sends exactly one result (its guard sends on unwind too), so this ends.
            match rx.recv_timeout(std::time::Duration::from_secs(120)) {
                Ok(result) => {
                    self.received.push(result);
                    waiting -= 1;
                }
                Err(_) => break,
            }
        }
    }

    /// Logs `message` once per picture and code.
    fn warn_once(&mut self, kind: &'static str, name: &str, code: &'static str, message: &str) {
        if self.logged.insert((kind, name.to_owned(), code)) {
            tracing::warn!(target: "regia_ui", code, kind, name, "ui image: {message}");
        }
    }

    /// The asset id of `path` (also tried with an `assets/` prefix), cached.
    fn lookup(&mut self, server: &AssetServer, path: &str) -> Option<AssetId> {
        if let Some(found) = self.paths.get(path) {
            return *found;
        }
        let mut norm = path.replace('\\', "/");
        if let Some(rest) = norm.strip_prefix("assets://") {
            norm = format!("assets/{rest}");
        }
        let found = server.id_for_path(&norm).or_else(|| {
            (!norm.starts_with("assets/"))
                .then(|| server.id_for_path(&format!("assets/{norm}")))
                .flatten()
        });
        self.paths.insert(path.to_owned(), found);
        found
    }

    /// Starts a background load of `id`.
    fn spawn_load(&mut self, reader: PayloadReader, id: AssetId) {
        self.stats.loads_started += 1;
        self.stats.in_flight += 1;
        let tx = self.tx.clone();
        regia_core::tasks::spawn(async move {
            let mut guard = SendGuard { tx, id, sent: false };
            let decoded = decode_picture(&reader, id);
            guard.send(decoded);
        })
        .detach();
    }

    /// Applies finished loads: uploads the pixels and updates the entries.
    fn apply_results(&mut self, renderer: Option<&SharedUiRenderer>) {
        let mut results = std::mem::take(&mut self.received);
        if let Ok(rx) = self.rx.lock() {
            results.extend(rx.try_iter());
        }
        for LoadResult { id, decoded } in results {
            self.stats.in_flight = self.stats.in_flight.saturating_sub(1);
            self.stats.loads_finished += 1;
            let Some(entry) = self.entries.get_mut(&id) else {
                continue;
            };
            entry.loading = false;
            let was_ready = entry.state == EntryState::Ready;
            match decoded {
                Ok((width, height, rgba)) => {
                    if let Some(renderer) = renderer {
                        renderer.lock().queue_image_rgba(entry.image, width, height, rgba);
                    }
                    if was_ready {
                        entry.reloads += 1;
                    }
                    entry.size = Some([width, height]);
                    entry.state = EntryState::Ready;
                }
                Err((code, message)) => {
                    self.stats.failures += 1;
                    let path = entry.path.clone();
                    if was_ready {
                        // A failed reload keeps the picture already drawn.
                        self.warn_once(
                            "asset",
                            &path,
                            code,
                            &format!("reloading `{path}` failed ({message}); keeping the previous picture"),
                        );
                    } else {
                        entry.state = EntryState::Failed { code, message };
                    }
                }
            }
            self.revision += 1;
        }
    }
}

/// Sends a task's result exactly once, also when the decode unwinds.
struct SendGuard {
    tx: Sender<LoadResult>,
    id: AssetId,
    sent: bool,
}

impl SendGuard {
    fn send(&mut self, decoded: Decoded) {
        self.sent = true;
        let _ = self.tx.send(LoadResult { id: self.id, decoded });
    }
}

impl Drop for SendGuard {
    fn drop(&mut self) {
        if !self.sent {
            let _ = self.tx.send(LoadResult {
                id: self.id,
                decoded: Err((
                    "ui_image_load_failed",
                    "the load task ended without a result".to_owned(),
                )),
            });
        }
    }
}

/// Reads and decodes the cooked file of `id` to RGBA8 (runs on a pool thread).
fn decode_picture(reader: &PayloadReader, id: AssetId) -> Decoded {
    let _span = tracing::debug_span!("ui_image_decode").entered();
    let (bytes, source) = reader.read(id).map_err(|e| {
        (
            "ui_image_load_failed",
            format!("reading the cooked file failed: {e}"),
        )
    })?;
    decode_cooked_picture(&bytes).inspect(|_| reader.note_image(&source))
}

/// Decodes a cooked file (wrapper header + payload) to RGBA8: a cooked image as is, a
/// block-compressed texture's top level on the CPU. Any other asset kind is `ui_image_wrong_kind`.
///
/// # Errors
///
/// `(code, message)`: `ui_image_wrong_kind` for another asset kind, `ui_image_load_failed` for a
/// file that does not decode.
pub fn decode_cooked_picture(
    bytes: &regia_asset::PakPayload,
) -> Result<(u32, u32, Vec<u8>), (&'static str, String)> {
    let (header, _) = regia_asset::decode_cooked_file(&bytes[..])
        .map_err(|e| ("ui_image_load_failed", format!("not a cooked file: {e}")))?;
    if header.type_tag == AssetTypeTag::image().0 {
        let (_, image) =
            regia_asset::load_image_bytes(&bytes[..]).map_err(|e| ("ui_image_load_failed", e.to_string()))?;
        return Ok((image.width, image.height, image.rgba8));
    }
    if header.type_tag == AssetTypeTag::texture().0 {
        let (_, texture) =
            regia_asset::load_texture_shared(bytes).map_err(|e| ("ui_image_load_failed", e.to_string()))?;
        let compressed = regia_render::CompressedTexture::from_ktx2_shared(&texture.ktx2)
            .map_err(|e| ("ui_image_load_failed", format!("texture container: {e}")))?;
        let level = compressed.decode_rgba8().ok_or_else(|| {
            (
                "ui_image_load_failed",
                "the texture format has no CPU decoder".to_owned(),
            )
        })?;
        return Ok((level.width, level.height, level.rgba));
    }
    Err((
        "ui_image_wrong_kind",
        "the asset is not an image or a texture".to_owned(),
    ))
}

/// Registers the built-in placeholder (1x1 faint grey, [`UI_IMAGE_PLACEHOLDER`]) and fallback
/// (2x2 magenta and black, [`UI_IMAGE_FALLBACK`]) pictures with `renderer` (the loader does this
/// itself; a tool that draws a document without the loader, such as the UI designer's preview, calls
/// it).
pub fn queue_builtin_pictures(renderer: &SharedUiRenderer) {
    queue_builtins(renderer);
}

fn queue_builtins(renderer: &SharedUiRenderer) {
    let mut ui = renderer.lock();
    ui.queue_image_rgba(UI_IMAGE_PLACEHOLDER, 1, 1, vec![128, 128, 128, 48]);
    ui.queue_image_rgba(
        UI_IMAGE_FALLBACK,
        2,
        2,
        vec![255, 0, 255, 255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 0, 255, 255],
    );
}

/// A picture component changed.
type PictureChanged = Or<(Changed<UiImageAsset>, Changed<UiMaskAsset>, Changed<UiMaskIcon>)>;
/// A node with a named picture was laid out anew (an icon picks its drawing by height).
type PictureResized = (Changed<ComputedNode>, Or<(With<UiImageAsset>, With<UiMaskIcon>)>);

/// Run condition of [`resolve_ui_image_assets`]: a picture component changed, a load is running or
/// finished, the icon atlas or the asset server changed (a hot reload), or a node with a named
/// picture moved or resized.
pub fn ui_image_assets_due(
    assets: Option<Res<UiImageAssets>>,
    changed: Query<(), PictureChanged>,
    resized: Query<(), PictureResized>,
    atlas: Option<Res<IconAtlas>>,
    server: Option<Res<AssetServer>>,
) -> bool {
    let Some(assets) = assets else {
        return false;
    };
    !changed.is_empty()
        || assets.stats.in_flight > 0
        || !assets.received.is_empty()
        || atlas.is_some_and(|a| a.generation != assets.atlas_generation)
        || (!assets.entries.is_empty() && server.is_some_and(|s| s.is_changed()))
        || !resized.is_empty()
}

/// What one picture resolves to this frame.
struct Resolved {
    asset: Option<AssetId>,
    image: u64,
    uv: [f32; 4],
    status: UiImageStatus,
    code: Option<&'static str>,
    reloads: u64,
}

/// Resolves one source for a node of physical height `height_px`.
fn resolve_source(
    assets: &mut UiImageAssets,
    server: Option<&AssetServer>,
    atlas: Option<&IconAtlas>,
    renderer: Option<&SharedUiRenderer>,
    source: &UiImageSource,
    height_px: f32,
) -> Resolved {
    let failed = |assets: &mut UiImageAssets, asset, code, message: String| {
        assets.warn_once(source.kind(), source.name(), code, &message);
        Resolved {
            asset,
            image: assets.fallback,
            uv: FULL_UV,
            status: UiImageStatus::Failed,
            code: Some(code),
            reloads: 0,
        }
    };
    match source {
        UiImageSource::Icon(name) => {
            let variants = atlas.map_or(&[][..], |a| a.variants(name));
            let target = if height_px.is_finite() && height_px > 0.0 {
                height_px
            } else {
                variants.last().map_or(1.0, |v| v.size_px as f32)
            };
            match pick_variant(variants, target) {
                Some(pick) => Resolved {
                    asset: None,
                    image: pick.variant.image,
                    uv: pick.variant.uv,
                    status: UiImageStatus::Ready,
                    code: None,
                    reloads: 0,
                },
                None => failed(
                    assets,
                    None,
                    "ui_image_icon_missing",
                    format!("no icon `{name}` in the icon atlas"),
                ),
            }
        }
        UiImageSource::Asset(path) => {
            let Some(server) = server else {
                return failed(
                    assets,
                    None,
                    "ui_image_no_asset_server",
                    format!("`{path}` cannot load: no asset server is running"),
                );
            };
            let Some(id) = assets.lookup(server, path) else {
                return failed(
                    assets,
                    None,
                    "ui_image_not_found",
                    format!("no manifest entry for `{path}`"),
                );
            };
            if let std::collections::hash_map::Entry::Vacant(slot) = assets.entries.entry(id) {
                let image = asset_image_id(id);
                // Registered already (`ui.set_group` loads a mask at once): not read again.
                let size = renderer.and_then(|r| r.lock().image_size(image));
                slot.insert(Entry {
                    path: path.clone(),
                    image,
                    state: if size.is_some() {
                        EntryState::Ready
                    } else {
                        EntryState::Loading
                    },
                    loading: size.is_none(),
                    generation: server.generation(id),
                    reloads: 0,
                    size,
                });
                if size.is_none() {
                    assets.spawn_load(server.payload_reader(), id);
                }
            }
            let (state, image, reloads) = match assets.entries.get(&id) {
                Some(e) => (e.state.clone(), e.image, e.reloads),
                None => (EntryState::Loading, 0, 0),
            };
            match state {
                EntryState::Loading => Resolved {
                    asset: Some(id),
                    image: assets.placeholder,
                    uv: FULL_UV,
                    status: UiImageStatus::Loading,
                    code: None,
                    reloads,
                },
                EntryState::Ready => Resolved {
                    asset: Some(id),
                    image,
                    uv: FULL_UV,
                    status: UiImageStatus::Ready,
                    code: None,
                    reloads,
                },
                EntryState::Failed { code, message } => {
                    failed(assets, Some(id), code, format!("`{path}`: {message}"))
                }
            }
        }
    }
}

/// `PostUpdate`, before layout: resolves every named picture (module docs). Writes a node's
/// [`UiImage`], [`Background`], [`UiMask`] and handle only when they change, so a steady frame
/// marks nothing changed.
pub fn resolve_ui_image_assets(world: &mut World) {
    let _span = tracing::debug_span!("ui_image_assets").entered();
    let Some(mut assets) = world.remove_resource::<UiImageAssets>() else {
        return;
    };
    let renderer = world.get_resource::<SharedUiRenderer>().cloned();
    if let Some(renderer) = &renderer
        && !assets.builtins_queued
    {
        queue_builtins(renderer);
        assets.builtins_queued = true;
    }
    assets.apply_results(renderer.as_ref());

    // Hot reload: a held asset whose server generation moved is read again.
    if let Some(server) = world.get_resource::<AssetServer>() {
        let due: Vec<(AssetId, u64)> = assets
            .entries
            .iter()
            .filter(|(_, e)| !e.loading)
            .filter_map(|(id, e)| {
                let generation = server.generation(*id);
                (generation != e.generation).then_some((*id, generation))
            })
            .collect();
        for (id, generation) in due {
            if let Some(entry) = assets.entries.get_mut(&id) {
                entry.loading = true;
                entry.generation = generation;
            }
            assets.stats.reloads += 1;
            assets.spawn_load(server.payload_reader(), id);
        }
    }
    let atlas_generation = world.get_resource::<IconAtlas>().map_or(0, |a| a.generation);
    assets.atlas_generation = atlas_generation;

    resolve_images(world, &mut assets, renderer.as_ref());
    resolve_masks(world, &mut assets, renderer.as_ref());
    world.insert_resource(assets);
}

/// The images half of [`resolve_ui_image_assets`].
fn resolve_images(world: &mut World, assets: &mut UiImageAssets, renderer: Option<&SharedUiRenderer>) {
    let nodes: Vec<(Entity, UiImageAsset, f32)> = world
        .query::<(Entity, &UiImageAsset, Option<&ComputedNode>)>()
        .iter(world)
        .map(|(e, a, c)| (e, a.clone(), c.map_or(0.0, |c| c.rect[3])))
        .collect();
    for (entity, asset, height) in nodes {
        let resolved = resolve_source(
            assets,
            world.get_resource::<AssetServer>(),
            world.get_resource::<IconAtlas>(),
            renderer,
            &asset.source,
            height,
        );
        let as_background = asset.nine_slice.is_some() && resolved.status == UiImageStatus::Ready;
        let state = UiPictureState {
            asset: resolved.asset,
            image: resolved.image,
            uv: resolved.uv,
            status: resolved.status,
            code: resolved.code,
            reloads: resolved.reloads,
            as_background,
        };
        let Ok(mut e) = world.get_entity_mut(entity) else {
            continue;
        };
        let was_background = e.get::<UiImageHandle>().is_some_and(|h| h.0.as_background);
        if as_background {
            let background = Background {
                kind: BackgroundKind::Image {
                    id: resolved.image,
                    uv: resolved.uv,
                    nine_slice: asset.nine_slice,
                },
            };
            if e.get::<Background>() != Some(&background) {
                e.insert(background);
            }
            if e.contains::<UiImage>() {
                e.remove::<UiImage>();
            }
        } else {
            if was_background {
                e.remove::<Background>();
            }
            let ready = resolved.status == UiImageStatus::Ready;
            let image = UiImage {
                id: resolved.image,
                uv: resolved.uv,
                tint: if ready { asset.tint } else { UiColor::WHITE },
                preserve_aspect: ready && asset.preserve_aspect,
            };
            if e.get::<UiImage>() != Some(&image) {
                e.insert(image);
            }
        }
        if e.get::<UiImageHandle>().map(|h| &h.0) != Some(&state) {
            e.insert(UiImageHandle(state));
        }
    }
}

/// The masks half of [`resolve_ui_image_assets`]: a mask that is not ready leaves the node unmasked.
fn resolve_masks(world: &mut World, assets: &mut UiImageAssets, renderer: Option<&SharedUiRenderer>) {
    let nodes: Vec<(Entity, UiImageSource, MaskFit, f32)> = world
        .query::<(
            Entity,
            Option<&UiMaskAsset>,
            Option<&UiMaskIcon>,
            Option<&ComputedNode>,
        )>()
        .iter(world)
        .filter_map(|(e, asset, icon, c)| {
            let height = c.map_or(0.0, |c| c.rect[3]);
            match (asset, icon) {
                (Some(a), _) => Some((e, UiImageSource::Asset(a.path.clone()), a.fit, height)),
                (None, Some(i)) => Some((e, UiImageSource::Icon(i.name.clone()), i.fit, height)),
                (None, None) => None,
            }
        })
        .collect();
    for (entity, source, fit, height) in nodes {
        let resolved = resolve_source(
            assets,
            world.get_resource::<AssetServer>(),
            world.get_resource::<IconAtlas>(),
            renderer,
            &source,
            height,
        );
        let state = UiPictureState {
            asset: resolved.asset,
            image: resolved.image,
            uv: resolved.uv,
            status: resolved.status,
            code: resolved.code,
            reloads: resolved.reloads,
            as_background: false,
        };
        let Ok(mut e) = world.get_entity_mut(entity) else {
            continue;
        };
        if resolved.status == UiImageStatus::Ready {
            let mask = UiMask {
                image: resolved.image,
                uv: resolved.uv,
                fit,
            };
            if e.get::<UiMask>() != Some(&mask) {
                e.insert(mask);
            }
        } else if e.contains::<UiMask>() {
            e.remove::<UiMask>();
        }
        if e.get::<UiMaskHandle>().map(|h| &h.0) != Some(&state) {
            e.insert(UiMaskHandle(state));
        }
    }
}

/// One picture a `.ui.ron` document names ([`ui_image_references`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiImageRef {
    /// Node path in the designer's index form (`"/"`, `"/0/2"`).
    pub node: String,
    /// The node's name, if any.
    pub node_name: Option<String>,
    /// `"image"` or `"mask"`.
    pub field: &'static str,
    /// `"asset"`, `"icon"` or `"id"`.
    pub kind: &'static str,
    /// The asset path, the icon name or the renderer id as text.
    pub value: String,
}

/// Every picture `file` names, in depth-first order (repeat templates included): what the cook's
/// dependency scan, the `ui_image_missing` validator and the designer's image list read.
#[must_use]
pub fn ui_image_references(file: &UiFile) -> Vec<UiImageRef> {
    let mut out = Vec::new();
    walk_refs(&file.root, "/".to_owned(), &mut out);
    out
}

fn walk_refs(node: &UiNodeDef, path: String, out: &mut Vec<UiImageRef>) {
    let mut push = |field: &'static str, kind: &'static str, value: String| {
        out.push(UiImageRef {
            node: path.clone(),
            node_name: node.name.clone(),
            field,
            kind,
            value,
        });
    };
    if let Some(image) = &node.image {
        match (&image.asset, &image.icon) {
            (Some(p), _) => push("image", "asset", p.clone()),
            (None, Some(n)) => push("image", "icon", n.clone()),
            (None, None) => push("image", "id", image.id.to_string()),
        }
    }
    if let Some(mask) = &node.mask {
        if let Some(p) = &mask.asset {
            push("mask", "asset", p.clone());
        } else if let Some(n) = &mask.icon {
            push("mask", "icon", n.clone());
        } else if let Some(id) = mask.image {
            push("mask", "id", id.to_string());
        }
    }
    for (i, child) in node.children.iter().enumerate() {
        let child_path = if path == "/" {
            format!("/{i}")
        } else {
            format!("{path}/{i}")
        };
        walk_refs(child, child_path, out);
    }
    // Component instance slot content (ADR-3147) names pictures too.
    for (slot, children) in &node.slots {
        for (i, child) in children.iter().enumerate() {
            let child_path = if path == "/" {
                format!("/{slot}/{i}")
            } else {
                format!("{path}/{slot}/{i}")
            };
            walk_refs(child, child_path, out);
        }
    }
}

/// The asset paths `file` names (images and masks), deduplicated and sorted: a `.ui.ron`'s
/// dependencies for the cook.
#[must_use]
pub fn ui_asset_dependencies(file: &UiFile) -> Vec<String> {
    let mut paths: Vec<String> = ui_image_references(file)
        .into_iter()
        .filter(|r| r.kind == "asset")
        .map(|r| r.value)
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// JSON of one reference (the designer's image list).
#[must_use]
pub fn image_ref_json(r: &UiImageRef) -> Value {
    json!({
        "node": r.node,
        "name": r.node_name,
        "field": r.field,
        "kind": r.kind,
        "value": r.value,
    })
}

fn state_json(state: &UiPictureState) -> Value {
    json!({
        "asset": state.asset.map(|a| a.to_string()),
        "image": state.image,
        "uv": state.uv,
        "status": state.status.name(),
        "code": state.code,
        "reloads": state.reloads,
        "nine_slice_background": state.as_background,
    })
}

/// `ui.image_assets {status?}`: every node with a named picture (images and masks) with its source,
/// status, renderer image and error code, plus the loader's counters. `status` filters by
/// `loading`, `ready` or `failed`.
///
/// # Errors
///
/// `ui_image_status_unknown` for a `status` other than `loading`, `ready` and `failed`.
pub fn agent_image_assets(
    world: &mut World,
    params: &Value,
) -> Result<Value, crate::agent_bridge::UiBridgeError> {
    let filter = match params.get("status").and_then(Value::as_str) {
        None => None,
        Some(s @ ("loading" | "ready" | "failed")) => Some(s.to_owned()),
        Some(other) => {
            return Err(crate::agent_bridge::UiBridgeError::invalid(
                "ui_image_status_unknown",
                format!("unknown status `{other}`"),
                "pass `loading`, `ready` or `failed`, or leave `status` out",
            ));
        }
    };
    let keep = |state: &UiPictureState| filter.as_deref().is_none_or(|f| f == state.status.name());
    let mut nodes = Vec::new();
    let mut images = world.query::<(
        Entity,
        Option<&crate::node::NodeName>,
        &UiImageAsset,
        Option<&UiImageHandle>,
    )>();
    for (entity, name, asset, handle) in images.iter(world) {
        let state = handle.map(|h| h.0.clone()).unwrap_or_default();
        if keep(&state) {
            nodes.push(json!({
                "entity": entity.to_bits(),
                "name": name.map(|n| n.0.clone()),
                "field": "image",
                "kind": asset.source.kind(),
                "source": asset.source.name(),
                "nine_slice": asset.nine_slice,
                "state": state_json(&state),
            }));
        }
    }
    let mut masks = world.query::<(
        Entity,
        Option<&crate::node::NodeName>,
        Option<&UiMaskAsset>,
        Option<&UiMaskIcon>,
        Option<&UiMaskHandle>,
    )>();
    for (entity, name, asset, icon, handle) in masks.iter(world) {
        let source = match (asset, icon) {
            (Some(a), _) => UiImageSource::Asset(a.path.clone()),
            (None, Some(i)) => UiImageSource::Icon(i.name.clone()),
            (None, None) => continue,
        };
        let state = handle.map(|h| h.0.clone()).unwrap_or_default();
        if keep(&state) {
            nodes.push(json!({
                "entity": entity.to_bits(),
                "name": name.map(|n| n.0.clone()),
                "field": "mask",
                "kind": source.kind(),
                "source": source.name(),
                "state": state_json(&state),
            }));
        }
    }
    let stats = world
        .get_resource::<UiImageAssets>()
        .map(UiImageAssets::stats)
        .unwrap_or_default();
    Ok(json!({
        "nodes": nodes,
        "loads_started": stats.loads_started,
        "loads_finished": stats.loads_finished,
        "failures": stats.failures,
        "reloads": stats.reloads,
        "in_flight": stats.in_flight,
        "assets": stats.assets,
    }))
}

/// Adds [`UiImageAssets`] and [`resolve_ui_image_assets`] (before layout). [`crate::UiPlugin`] adds it.
pub struct UiImageAssetsPlugin;

impl Plugin for UiImageAssetsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<UiImageAssets>().add_systems(
            PostUpdate,
            resolve_ui_image_assets
                .run_if(ui_image_assets_due)
                .before(PostUpdateSet::UiLayout),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authoring::{ImageDef, MaskDef, parse_ui_ron};

    #[test]
    fn references_walk_images_and_masks_in_depth_first_order() {
        let text = r#"(version: 9, root: (children: [
            (name: "logo", image: (asset: "assets/ui/logo.png")),
            (children: [(image: (icon: "gear")), (image: (id: 7))]),
            (name: "star", mask: (icon: "star"), children: [(mask: (asset: "assets/ui/m.png"))]),
        ]))"#;
        let file = parse_ui_ron(text, "t", &crate::UiMigrators::default()).expect("parses");
        let refs = ui_image_references(&file);
        let got: Vec<(&str, &str, &str, &str)> = refs
            .iter()
            .map(|r| (r.node.as_str(), r.field, r.kind, r.value.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("/0", "image", "asset", "assets/ui/logo.png"),
                ("/1/0", "image", "icon", "gear"),
                ("/1/1", "image", "id", "7"),
                ("/2", "mask", "icon", "star"),
                ("/2/0", "mask", "asset", "assets/ui/m.png"),
            ]
        );
        assert_eq!(
            ui_asset_dependencies(&file),
            vec!["assets/ui/logo.png".to_owned(), "assets/ui/m.png".to_owned()]
        );
    }

    #[test]
    fn image_def_sources() {
        assert_eq!(ImageDef::raw(3).source(), None);
        assert_eq!(
            ImageDef::asset("a.png").source(),
            Some(UiImageSource::Asset("a.png".into()))
        );
        assert_eq!(
            ImageDef::icon("x").source(),
            Some(UiImageSource::Icon("x".into()))
        );
        let _ = MaskDef {
            image: None,
            asset: None,
            icon: Some("x".into()),
            fit: MaskFit::Stretch,
        };
    }

    #[test]
    fn asset_image_ids_set_the_top_bit() {
        let id = AssetId::new_random();
        assert_ne!(asset_image_id(id) & (1 << 63), 0);
        assert_eq!(asset_image_id(id), asset_image_id(id));
    }

    #[test]
    fn a_non_picture_cooked_file_is_the_wrong_kind() {
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::heightfield().0,
            asset_id: AssetId::new_random(),
            importer_version: 1,
            dependencies: Vec::new(),
            payload_len: 0,
        };
        let bytes = regia_asset::encode_cooked_file(&header, &[]).expect("encode");
        let err = decode_cooked_picture(&regia_asset::PakPayload::from(bytes)).unwrap_err();
        assert_eq!(err.0, "ui_image_wrong_kind");
        let err = decode_cooked_picture(&regia_asset::PakPayload::from(vec![1, 2, 3])).unwrap_err();
        assert_eq!(err.0, "ui_image_load_failed");
    }
}
