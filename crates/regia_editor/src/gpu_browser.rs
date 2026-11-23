// SPDX-License-Identifier: MPL-2.0
//! The GPU resource browser, the second half of the Profiler window's Memory tab
//! (`docs/specs/19-editor.md` § 10, ADR-8142).
//!
//! It lists the renderer's textures and render targets (the `regia_render::resource_inspector`
//! registry: size, format, usage, mips, layers, bytes) and every other live GPU allocation of the
//! allocator report by label (buffers and textures the registry does not track: bytes and count),
//! each with its owner (the engine subsystem and memory category of its label), largest first.
//! Selecting a texture or render target of an in-process session shows a preview: the chosen mip
//! and layer copied back from the GPU ([`regia_render::read_texels`]) and point-sampled down to at
//! most [`gpu_browser::PREVIEW_MAX`] texels on its longer side, all channels or one as gray (values clamped to
//! 0..1, sRGB formats shown as stored). Up and Down step through the list; the preview follows.
//!
//! The agent twin is `editor.gpu_resources { action: list | preview }` (`book/agents/profiler.md`).

use regia_agent::protocol::{INVALID_PARAMS, RpcError, UNSUPPORTED};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_render::{MemoryReport, ResourceInfo, Texels};
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// The longest side of a preview, texels.
pub const PREVIEW_MAX: u32 = 256;

/// Editor frames between two reads of a live preview.
pub const LIVE_EVERY_FRAMES: u64 = 30;

/// Height of the resource list, points.
pub const RESOURCE_LIST_HEIGHT: f32 = 200.0;

/// What a row of the browser is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// A registered texture that is only sampled.
    Texture,
    /// A registered texture the renderer draws into (`RENDER_ATTACHMENT`).
    RenderTarget,
    /// Allocator-report allocations under one label the registry does not track: buffers, and
    /// textures created without registering.
    Allocation,
}

impl RowKind {
    /// Stable snake-case name (agent protocol).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Texture => "texture",
            Self::RenderTarget => "render_target",
            Self::Allocation => "allocation",
        }
    }
}

/// One row of the browser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuRow {
    /// The wgpu label.
    pub label: String,
    /// What it is.
    pub kind: RowKind,
    /// The texel format (`Rgba8Unorm`); `None` for an allocation row.
    pub format: Option<String>,
    /// The usage flags (`RENDER_ATTACHMENT | TEXTURE_BINDING`); `None` for an allocation row.
    pub usage: Option<String>,
    /// Width, height, layers and mips of a texture.
    pub extent: Option<(u32, u32, u32, u32)>,
    /// Bytes (all mips, layers and samples of a texture; the sum of an allocation row's
    /// allocations).
    pub bytes: u64,
    /// Allocations under the label (1 for a texture).
    pub count: u32,
    /// The engine subsystem its label belongs to (`render`, `terrain`, ...).
    pub owner: String,
    /// The memory category of its label (`render_targets`, `meshes`, ...).
    pub category: String,
    /// Whether a preview can be read (a registered, copyable texture with a reader).
    pub previewable: bool,
}

impl GpuRow {
    /// The accessible name of the row: label, kind, size, format and owner.
    #[must_use]
    pub fn row_name(&self) -> String {
        let size = crate::memory_browser::bytes_text(Some(self.bytes));
        match (self.extent, &self.format) {
            (Some((w, h, layers, mips)), Some(format)) => {
                let layers = if layers > 1 {
                    format!(" x{layers}")
                } else {
                    String::new()
                };
                format!(
                    "{}, {}, {w}x{h}{layers}, {mips} mips, {format}, {size}, {}",
                    self.label,
                    self.kind.name().replace('_', " "),
                    self.owner
                )
            }
            _ => format!(
                "{}, allocation x{}, {size}, {}",
                self.label, self.count, self.owner
            ),
        }
    }
}

/// The browser's rows: every registered texture (render target when drawn into) and every other
/// allocator-report label, largest first (ties by label).
#[must_use]
pub fn gpu_rows(
    textures: &[(ResourceInfo, wgpu::TextureUsages)],
    report: Option<&MemoryReport>,
) -> Vec<GpuRow> {
    let owner = |label: &str| regia_render::memory::gpu_subsystem(label).name().to_owned();
    let mut rows: Vec<GpuRow> = textures
        .iter()
        .map(|(t, usage)| GpuRow {
            label: t.label.clone(),
            kind: if usage.contains(wgpu::TextureUsages::RENDER_ATTACHMENT) {
                RowKind::RenderTarget
            } else {
                RowKind::Texture
            },
            format: Some(t.format.clone()),
            usage: Some(regia_render::usage_names(*usage)),
            extent: Some((t.width, t.height, t.layers, t.mips)),
            bytes: t.bytes,
            count: 1,
            owner: owner(&t.label),
            category: t.category.name().to_owned(),
            previewable: t.copyable && t.dump_format.is_some(),
        })
        .collect();
    for l in report
        .filter(|r| r.available)
        .map(|r| r.labels.as_slice())
        .unwrap_or_default()
    {
        if textures.iter().any(|(t, _)| t.label == l.label) {
            continue;
        }
        rows.push(GpuRow {
            label: l.label.clone(),
            kind: RowKind::Allocation,
            format: None,
            usage: None,
            extent: None,
            bytes: l.bytes,
            count: l.count,
            owner: owner(&l.label),
            category: l.category.name().to_owned(),
            previewable: false,
        });
    }
    rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.label.cmp(&b.label)));
    rows
}

/// Which channels a preview shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Channel {
    /// Every channel as color (a missing blue is 0, a missing alpha opaque).
    #[default]
    Rgba,
    /// Red as gray.
    R,
    /// Green as gray.
    G,
    /// Blue as gray.
    B,
    /// Alpha as gray.
    A,
}

impl Channel {
    /// Every channel choice, in selector order.
    pub const ALL: [Self; 5] = [Self::Rgba, Self::R, Self::G, Self::B, Self::A];

    /// Stable lowercase name (agent protocol).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Rgba => "rgba",
            Self::R => "r",
            Self::G => "g",
            Self::B => "b",
            Self::A => "a",
        }
    }

    /// The selector's text.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Rgba => "RGBA",
            Self::R => "R",
            Self::G => "G",
            Self::B => "B",
            Self::A => "A",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }
}

/// A preview image: RGBA8, `width * height * 4` bytes, row-major from the top-left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewImage {
    /// Width, texels (at most [`PREVIEW_MAX`]).
    pub width: u32,
    /// Height, texels.
    pub height: u32,
    /// Width of the mip it was read from.
    pub source_width: u32,
    /// Height of the mip it was read from.
    pub source_height: u32,
    /// RGBA8 pixels.
    pub rgba: Vec<u8>,
}

fn to_u8(v: f32) -> u8 {
    if v.is_nan() {
        return 0;
    }
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The preview size of a `w` x `h` mip: the same aspect, at most `max` on the longer side, at
/// least 1.
#[must_use]
pub fn preview_size(w: u32, h: u32, max: u32) -> (u32, u32) {
    let max = max.max(1);
    let long = w.max(h).max(1);
    if long <= max {
        return (w.max(1), h.max(1));
    }
    let scale = |v: u32| ((u64::from(v) * u64::from(max)) / u64::from(long)).max(1) as u32;
    (scale(w), scale(h))
}

/// Turns read-back texels into a preview at most `max` texels on its longer side: texel
/// (`x * w / pw`, `y * h / ph`) of the mip for preview pixel (`x`, `y`) (point sampling, so a
/// texel's stored value shows unchanged), `channel` picked, values clamped to 0..1.
#[must_use]
pub fn preview_image(texels: &Texels, channel: Channel, max: u32) -> PreviewImage {
    let (w, h) = (texels.width.max(1), texels.height.max(1));
    let (pw, ph) = preview_size(w, h, max);
    let mut rgba = Vec::with_capacity(pw as usize * ph as usize * 4);
    let get = |x: u32, y: u32, c: u32| texels.get(x, y, c);
    for y in 0..ph {
        let sy = ((u64::from(y) * u64::from(h)) / u64::from(ph)) as u32;
        for x in 0..pw {
            let sx = ((u64::from(x) * u64::from(w)) / u64::from(pw)) as u32;
            let px = match channel {
                Channel::Rgba => {
                    let r = get(sx, sy, 0).unwrap_or(0.0);
                    // A one-channel format shows as gray.
                    let g = get(sx, sy, 1).unwrap_or(if texels.channels == 1 { r } else { 0.0 });
                    let b = get(sx, sy, 2).unwrap_or(if texels.channels == 1 { r } else { 0.0 });
                    let a = get(sx, sy, 3).unwrap_or(1.0);
                    [to_u8(r), to_u8(g), to_u8(b), to_u8(a)]
                }
                one => {
                    let c = match one {
                        Channel::R => 0,
                        Channel::G => 1,
                        Channel::B => 2,
                        _ => 3,
                    };
                    let missing = if c == 3 { 1.0 } else { 0.0 };
                    let v = to_u8(get(sx, sy, c).unwrap_or(missing));
                    [v, v, v, 255]
                }
            };
            rgba.extend_from_slice(&px);
        }
    }
    PreviewImage {
        width: pw,
        height: ph,
        source_width: w,
        source_height: h,
        rgba,
    }
}

/// Why a preview could not be read; [`Self::code`] is the agent protocol's reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewError {
    /// The editor has no render device (a headless session).
    NoDevice,
    /// The inspector refused or failed ([`regia_render::InspectError::code`]).
    Inspect(regia_render::InspectError),
}

impl PreviewError {
    /// `gpu_no_device`, or the inspector's code (`resource_not_found`, `resource_not_copyable`,
    /// `format_unsupported`, `subresource_out_of_range`, `readback_failed`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoDevice => "gpu_no_device",
            Self::Inspect(e) => e.code(),
        }
    }

    /// The human-readable reason.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::NoDevice => "the editor has no render device (headless)".to_owned(),
            Self::Inspect(e) => e.message().to_owned(),
        }
    }
}

/// Reads a preview of mip `mip`, layer `layer` of the texture labelled `label`.
///
/// **Blocking** (a GPU copy and a wait): call it on a selection or a refresh, not every frame.
///
/// # Errors
///
/// [`PreviewError`].
pub fn read_preview(
    world: &World,
    label: &str,
    mip: u32,
    layer: u32,
    channel: Channel,
    max: u32,
) -> Result<PreviewImage, PreviewError> {
    let _span = tracing::info_span!("gpu_browser_preview").entered();
    let device = world
        .get_resource::<regia_render::RenderDevice>()
        .ok_or(PreviewError::NoDevice)?;
    let texels = regia_render::read_texels(device, label, mip, layer).map_err(PreviewError::Inspect)?;
    Ok(preview_image(&texels, channel, max))
}

/// Every registered target with its usage flags.
fn registered() -> Vec<(ResourceInfo, wgpu::TextureUsages)> {
    regia_render::list_resources()
        .into_iter()
        .map(|info| {
            let usage = regia_render::resource_usage(&info.label).unwrap_or(wgpu::TextureUsages::empty());
            (info, usage)
        })
        .collect()
}

/// The rows of the editor's renderer now: the registry and, with a device, the allocator report.
#[must_use]
pub fn current_rows(world: &World) -> (Vec<GpuRow>, Option<String>) {
    let report = world
        .get_resource::<regia_render::RenderDevice>()
        .map(regia_render::memory_report);
    let backend = report.as_ref().map(|r| r.backend.clone());
    (gpu_rows(&registered(), report.as_ref()), backend)
}

/// The preview the browser shows and what it was read from.
#[derive(Clone)]
pub struct ShownPreview {
    /// `(label, mip, layer, channel)` it was read for.
    pub key: (String, u32, u32, Channel),
    /// The editor frame it was read in.
    pub frame: u64,
    /// The image, or why there is none.
    pub image: Result<PreviewImage, String>,
    /// The texture egui draws it from.
    pub texture: Option<egui::TextureHandle>,
}

impl std::fmt::Debug for ShownPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShownPreview")
            .field("key", &self.key)
            .field("frame", &self.frame)
            .finish_non_exhaustive()
    }
}

/// The browser's state (a resource of the editor world, created on first use).
#[derive(Resource, Clone, Debug, Default)]
pub struct GpuBrowser {
    /// The focusable resource list; its current row is the selection.
    pub keys: crate::canvas_keys::ListKeys,
    /// The selected row's label (kept when the list reorders).
    pub selected: Option<String>,
    /// The previewed mip.
    pub mip: u32,
    /// The previewed layer or depth slice.
    pub layer: u32,
    /// The previewed channels.
    pub channel: Channel,
    /// Read the preview again every [`LIVE_EVERY_FRAMES`] frames.
    pub live: bool,
    /// A refresh was asked for (read on the next draw).
    pub refresh: bool,
    /// The preview shown.
    pub shown: Option<ShownPreview>,
    /// The listed rows, the backend and the editor frame they were read in (kept
    /// [`ROWS_EVERY_FRAMES`] frames: the allocator report is not a per-frame call).
    pub rows: Option<(u64, Vec<GpuRow>, Option<String>)>,
}

/// Editor frames the browser keeps its list before reading the registry and the allocator report
/// again (**Refresh preview** reads both at once).
pub const ROWS_EVERY_FRAMES: u64 = 30;

/// Draws the browser: the resource list and the preview of the selected row.
pub fn browser_ui(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::info_span!("gpu_browser").entered();
    let frame = ui.ctx().cumulative_frame_nr();
    let cached = world
        .get_resource::<GpuBrowser>()
        .and_then(|b| b.rows.clone())
        .filter(|(at, _, _)| frame.saturating_sub(*at) < ROWS_EVERY_FRAMES);
    let (rows, backend) = match cached {
        Some((_, rows, backend)) => (rows, backend),
        None => {
            let (rows, backend) = current_rows(world);
            world.get_resource_or_insert_with(GpuBrowser::default).rows =
                Some((frame, rows.clone(), backend.clone()));
            (rows, backend)
        }
    };
    rows_ui(ui, world, &rows, backend.as_deref());
}

/// [`browser_ui`] over `rows` (the tests pass a fixed list: the registry is process-wide).
fn rows_ui(ui: &mut egui::Ui, world: &mut World, rows: &[GpuRow], backend: Option<&str>) {
    use crate::agent_parity::{
        PROFILER_GPU_CHANNEL, PROFILER_GPU_LIVE, PROFILER_GPU_MIP_DOWN, PROFILER_GPU_MIP_UP,
        PROFILER_GPU_REFRESH, action_button, action_enabled, action_selectable, action_toggle,
    };
    let mut b = world.get_resource_or_insert_with(GpuBrowser::default).clone();
    let total: u64 = rows.iter().map(|r| r.bytes).sum();
    ui.label(format!(
        "{} resources · {} · {}",
        rows.len(),
        crate::memory_browser::bytes_text(Some(total)),
        backend.unwrap_or("no render device")
    ));
    // Keep the selection on its label when the list reorders.
    if let Some(label) = &b.selected
        && let Some(i) = rows.iter().position(|r| &r.label == label)
        && b.keys.current != Some(i)
    {
        b.keys.retarget(Some(i));
    }
    let names: Vec<String> = rows.iter().map(GpuRow::row_name).collect();
    let name = |i: usize| names.get(i).cloned().unwrap_or_default();
    let current = b.keys.current;
    let _ = crate::canvas_keys::list_rows(
        ui,
        "gpu-resources",
        &mut b.keys,
        rows.len(),
        RESOURCE_LIST_HEIGHT,
        current,
        &name,
    );
    let row = b.keys.current.and_then(|i| rows.get(i));
    if row.map(|r| &r.label) != b.selected.as_ref() {
        b.selected = row.map(|r| r.label.clone());
        b.mip = 0;
        b.layer = 0;
    }
    ui.separator();
    let Some(row) = row else {
        ui.weak("Select a texture or render target to preview it.");
        world.insert_resource(b);
        return;
    };
    ui.monospace(format!(
        "{} · {} · usage {} · owner {} ({})",
        row.label,
        row.format.as_deref().unwrap_or("-"),
        row.usage.as_deref().unwrap_or("-"),
        row.owner,
        row.category
    ));
    if !row.previewable {
        ui.weak(match row.kind {
            RowKind::Allocation => "No preview: an allocation the texture registry does not track (a buffer, or an unregistered texture).",
            _ => "No preview: the texture cannot be copied out (multisampled, a depth-stencil format, no reader for its format, or created while debug readback was off).",
        });
        world.insert_resource(b);
        return;
    }
    let mips = row.extent.map_or(1, |(_, _, _, m)| m);
    ui.horizontal(|ui| {
        if action_enabled(ui, b.mip > 0, &PROFILER_GPU_MIP_DOWN, None).clicked()
            && crate::canvas_keys::first_in_this_frame(ui, egui::Id::new("gpu-mip-down"))
        {
            b.mip = b.mip.saturating_sub(1);
        }
        ui.monospace(format!("mip {} of {mips}", b.mip));
        if action_enabled(ui, b.mip + 1 < mips, &PROFILER_GPU_MIP_UP, None).clicked()
            && crate::canvas_keys::first_in_this_frame(ui, egui::Id::new("gpu-mip-up"))
        {
            b.mip = (b.mip + 1).min(mips.saturating_sub(1));
        }
        ui.separator();
        for c in Channel::ALL {
            if action_selectable(ui, &PROFILER_GPU_CHANNEL, c.label(), b.channel == c).clicked() {
                b.channel = c;
            }
        }
        ui.separator();
        // egui may run this pass twice in a frame: the toggle commits once.
        let mut live = b.live;
        if action_toggle(ui, &PROFILER_GPU_LIVE, &mut live).clicked()
            && crate::canvas_keys::first_in_this_frame(ui, egui::Id::new("gpu-live"))
        {
            b.live = live;
        }
        if action_button(ui, &PROFILER_GPU_REFRESH).clicked() {
            b.refresh = true;
            b.rows = None;
        }
    });
    let frame = ui.ctx().cumulative_frame_nr();
    let key = (row.label.clone(), b.mip, b.layer, b.channel);
    let stale = match &b.shown {
        None => true,
        Some(shown) => {
            shown.key != key
                || b.refresh
                || (b.live && frame.saturating_sub(shown.frame) >= LIVE_EVERY_FRAMES)
        }
    };
    if stale && crate::canvas_keys::first_in_this_frame(ui, egui::Id::new("gpu-preview-read")) {
        b.refresh = false;
        let image =
            read_preview(world, &row.label, b.mip, b.layer, b.channel, PREVIEW_MAX).map_err(|e| e.message());
        let texture = image.as_ref().ok().map(|img| {
            let color = egui::ColorImage::from_rgba_unmultiplied(
                [img.width as usize, img.height as usize],
                &img.rgba,
            );
            ui.ctx()
                .load_texture("gpu-browser-preview", color, egui::TextureOptions::NEAREST)
        });
        b.shown = Some(ShownPreview {
            key,
            frame,
            image,
            texture,
        });
    }
    if let Some(shown) = &b.shown {
        match (&shown.image, &shown.texture) {
            (Ok(img), Some(texture)) => {
                ui.weak(format!(
                    "{}x{} shown of {}x{} (mip {}, {})",
                    img.width,
                    img.height,
                    img.source_width,
                    img.source_height,
                    shown.key.1,
                    shown.key.3.label()
                ));
                let size = egui::vec2(img.width as f32, img.height as f32);
                let response = ui.add(egui::Image::new((texture.id(), size)));
                let spoken = format!("Preview of {}, mip {}", shown.key.0, shown.key.1);
                let _ = crate::a11y_audit::spoken_only(response, egui::WidgetType::Image, &spoken);
            }
            (Err(message), _) => {
                ui.weak(message);
            }
            _ => {}
        }
    }
    world.insert_resource(b);
}

fn refused(code: &'static str, message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, "see book/agents/profiler.md")
}

fn row_json(r: &GpuRow) -> Value {
    json!({
        "label": r.label,
        "kind": r.kind.name(),
        "format": r.format,
        "usage": r.usage,
        "width": r.extent.map(|e| e.0),
        "height": r.extent.map(|e| e.1),
        "layers": r.extent.map(|e| e.2),
        "mips": r.extent.map(|e| e.3),
        "bytes": r.bytes,
        "count": r.count,
        "owner": r.owner,
        "category": r.category,
        "previewable": r.previewable,
    })
}

fn u32_param(params: &Value, key: &str, default: u32, code: &'static str) -> Result<u32, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| refused(code, format!("`{key}` must be a non-negative integer"))),
    }
}

/// `editor.gpu_resources { action, label?, mip?, layer?, channel?, max_size?, top?, pixels? }`.
fn gpu_resources(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    match params.get("action").and_then(Value::as_str).unwrap_or_default() {
        "list" => {
            let top = u32_param(params, "top", 200, "gpu_bad_top")?.clamp(1, 5000) as usize;
            let (rows, backend) = current_rows(world);
            let total: u64 = rows.iter().map(|r| r.bytes).sum();
            Ok(json!({
                "backend": backend,
                "total_bytes": total,
                "count": rows.len(),
                "resources": rows.iter().take(top).map(row_json).collect::<Vec<_>>(),
            }))
        }
        "preview" => {
            let label = params
                .get("label")
                .and_then(Value::as_str)
                .ok_or_else(|| refused("gpu_label_required", "preview needs `label`"))?;
            let mip = u32_param(params, "mip", 0, "gpu_bad_subresource")?;
            let layer = u32_param(params, "layer", 0, "gpu_bad_subresource")?;
            let max = u32_param(params, "max_size", 64, "gpu_bad_max_size")?;
            if !(1..=PREVIEW_MAX).contains(&max) {
                return Err(refused(
                    "gpu_bad_max_size",
                    format!("`max_size` must be 1 to {PREVIEW_MAX}"),
                ));
            }
            let channel = match params.get("channel").and_then(Value::as_str) {
                None => Channel::Rgba,
                Some(name) => Channel::parse(name)
                    .ok_or_else(|| refused("gpu_bad_channel", "`channel` must be rgba, r, g, b or a"))?,
            };
            let image = read_preview(world, label, mip, layer, channel, max).map_err(|e| match e {
                PreviewError::NoDevice => RpcError::new(
                    UNSUPPORTED,
                    e.code(),
                    e.message(),
                    "previews need an editor with a render device",
                ),
                PreviewError::Inspect(_) => refused(e.code(), e.message()),
            })?;
            // The panel previews what the agent asked for.
            if let Some(mut b) = world.get_resource_mut::<GpuBrowser>() {
                b.selected = Some(label.to_owned());
                b.mip = mip;
                b.layer = layer;
                b.channel = channel;
                b.refresh = true;
            }
            let pixels = params.get("pixels").and_then(Value::as_bool).unwrap_or(false);
            let (pixel_values, _) = image.rgba.as_chunks::<4>();
            Ok(json!({
                "label": label,
                "mip": mip,
                "layer": layer,
                "channel": channel.name(),
                "width": image.width,
                "height": image.height,
                "source_width": image.source_width,
                "source_height": image.source_height,
                "pixels": pixels.then(|| pixel_values.iter().map(|p| p.to_vec()).collect::<Vec<_>>()),
            }))
        }
        _ => Err(refused("gpu_bad_action", "`action` must be list or preview")),
    }
}

/// The GPU resource browser's agent methods (ADR-8142).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.gpu_resources",
            tool: "gpu_resources",
            description: "The editor Profiler's GPU resource browser: list answers every registered texture and render target (format, usage, size, mips, layers, bytes) and every other live GPU allocation by label (buffers and untracked textures: bytes, count), each with its owner subsystem and memory category, largest first; preview reads one mip and layer of a texture back from the GPU, point-sampled to at most max_size texels on its longer side, and shows it in the panel.",
            params: &[
                ParamSchema {
                    name: "action",
                    kind: "string",
                    required: true,
                    description: "list or preview.",
                },
                ParamSchema {
                    name: "top",
                    kind: "integer",
                    required: false,
                    description: "list: rows answered, 1-5000 (default 200); count has them all.",
                },
                ParamSchema {
                    name: "label",
                    kind: "string",
                    required: false,
                    description: "preview: the texture's label (from list).",
                },
                ParamSchema {
                    name: "mip",
                    kind: "integer",
                    required: false,
                    description: "preview: the mip (default 0).",
                },
                ParamSchema {
                    name: "layer",
                    kind: "integer",
                    required: false,
                    description: "preview: the layer or depth slice (default 0).",
                },
                ParamSchema {
                    name: "channel",
                    kind: "string",
                    required: false,
                    description: "preview: rgba (default), r, g, b or a (one channel as gray).",
                },
                ParamSchema {
                    name: "max_size",
                    kind: "integer",
                    required: false,
                    description: "preview: longest side of the answer, 1-256 (default 64).",
                },
                ParamSchema {
                    name: "pixels",
                    kind: "boolean",
                    required: false,
                    description: "preview: answer the RGBA8 pixels, row-major (default false).",
                },
            ],
            result: "list: { backend, total_bytes, count, resources: [{ label, kind, format, usage, width, height, layers, mips, bytes, count, owner, category, previewable }] }; preview: { label, mip, layer, channel, width, height, source_width, source_height, pixels: [[r, g, b, a]] | null }",
            mutating: false,
            permission: Permission::Read,
        },
        gpu_resources as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{MemoryCategory, ResourceKind};

    fn info(label: &str, usage: wgpu::TextureUsages, bytes: u64) -> (ResourceInfo, wgpu::TextureUsages) {
        let info = ResourceInfo {
            label: label.to_owned(),
            kind: ResourceKind::Texture2d,
            format: "Rgba8Unorm".to_owned(),
            width: 512,
            height: 512,
            layers: 1,
            mips: 1,
            samples: 1,
            bytes,
            category: MemoryCategory::Other,
            copyable: true,
            dump_format: Some(regia_render::DumpFormat::Png),
        };
        (info, usage)
    }

    #[test]
    fn rows_merge_the_registry_and_the_allocator_report_largest_first() {
        let textures = [
            info(
                "hdr color",
                wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                4 << 20,
            ),
            info(
                "ui glyph atlas",
                wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                1 << 20,
            ),
        ];
        let report = MemoryReport::from_allocations(
            "Vulkan",
            [
                ("hdr color", 4 << 20),
                ("mesh vertices", 2 << 20),
                ("mesh vertices", 1 << 20),
                ("frame uniforms", 256),
            ],
            16 << 20,
        );
        let rows = gpu_rows(&textures, Some(&report));
        let summary: Vec<(&str, RowKind, u64)> =
            rows.iter().map(|r| (r.label.as_str(), r.kind, r.bytes)).collect();
        assert_eq!(
            summary,
            [
                ("hdr color", RowKind::RenderTarget, 4 << 20),
                ("mesh vertices", RowKind::Allocation, 3 << 20),
                ("ui glyph atlas", RowKind::Texture, 1 << 20),
                ("frame uniforms", RowKind::Allocation, 256),
            ]
        );
        assert_eq!(rows[1].count, 2);
        assert_eq!(rows[1].category, "meshes");
        assert!(rows[0].previewable && !rows[1].previewable);
        assert_eq!(
            rows[0].usage.as_deref(),
            Some("COPY_SRC | TEXTURE_BINDING | RENDER_ATTACHMENT")
        );
        assert!(
            rows[0]
                .row_name()
                .starts_with("hdr color, render target, 512x512, 1 mips, Rgba8Unorm, 4.00 MiB"),
            "{}",
            rows[0].row_name()
        );
        assert_eq!(gpu_rows(&textures, None).len(), 2, "no report: the registry only");
    }

    #[test]
    fn previews_point_sample_and_pick_channels() {
        let texels = Texels {
            width: 4,
            height: 2,
            channels: 4,
            values: (0..32).map(|i| (i % 4) as f32 / 3.0).collect(),
        };
        let img = preview_image(&texels, Channel::Rgba, 2);
        assert_eq!(
            (img.width, img.height, img.source_width, img.source_height),
            (2, 1, 4, 2)
        );
        assert_eq!(&img.rgba[..4], &[0, 85, 170, 255]);
        let g = preview_image(&texels, Channel::G, 256);
        assert_eq!((g.width, g.height), (4, 2), "smaller than the bound: unscaled");
        assert_eq!(&g.rgba[..4], &[85, 85, 85, 255]);
        let one = Texels {
            width: 1,
            height: 1,
            channels: 1,
            values: vec![2.0],
        };
        assert_eq!(
            preview_image(&one, Channel::Rgba, 8).rgba,
            [255, 255, 255, 255],
            "gray, clamped"
        );
        assert_eq!(
            preview_image(&one, Channel::A, 8).rgba,
            [255, 255, 255, 255],
            "no alpha: opaque"
        );
        assert_eq!(preview_image(&one, Channel::B, 8).rgba, [0, 0, 0, 255]);
        assert_eq!(preview_size(1024, 256, 256), (256, 64));
        assert_eq!(preview_size(3, 4000, 256), (1, 256));
        for c in Channel::ALL {
            assert_eq!(Channel::parse(c.name()), Some(c));
        }
    }

    #[test]
    fn the_method_refuses_bad_params_and_a_headless_preview() {
        let mut world = World::new();
        let out = gpu_resources(&mut world, &json!({ "action": "list" })).expect("list");
        assert!(out["resources"].is_array(), "{out}");
        for (params, code) in [
            (json!({ "action": "nope" }), "gpu_bad_action"),
            (json!({ "action": "preview" }), "gpu_label_required"),
            (
                json!({ "action": "preview", "label": "x", "channel": "q" }),
                "gpu_bad_channel",
            ),
            (
                json!({ "action": "preview", "label": "x", "max_size": 0 }),
                "gpu_bad_max_size",
            ),
            (
                json!({ "action": "preview", "label": "x", "mip": -1 }),
                "gpu_bad_subresource",
            ),
            (json!({ "action": "preview", "label": "x" }), "gpu_no_device"),
        ] {
            let err = gpu_resources(&mut world, &params).expect_err("refused");
            assert!(format!("{err:?}").contains(code), "{params}: {err:?}");
        }
    }

    /// egui can run the panel twice in one frame; the Live toggle commits once, so a click turns it
    /// on and the second pass of that frame does not turn it off again.
    #[test]
    fn the_live_toggle_commits_once_per_frame() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let row = GpuRow {
            label: "gpu browser live toggle".to_owned(),
            kind: RowKind::Texture,
            format: Some("Rgba8Unorm".to_owned()),
            usage: Some("COPY_SRC".to_owned()),
            extent: Some((4, 4, 1, 1)),
            bytes: 64,
            count: 1,
            owner: "render".to_owned(),
            category: "other".to_owned(),
            previewable: true,
        };
        let rows = [row];
        let mut world = World::new();
        world.insert_resource(GpuBrowser::default());
        world.resource_mut::<GpuBrowser>().keys.focus(0);
        let ctx = egui::Context::default();
        let size = [900.0, 600.0];
        // Every frame asks egui to discard its first pass, so the panel runs twice on the same
        // input, as a layout change makes it do.
        let twice = |ui: &mut egui::Ui, world: &mut World| {
            rows_ui(ui, world, &rows, None);
            if ui.ctx().current_pass_index() == 0 {
                ui.ctx().request_discard("test: run the pass again");
            }
        };
        frame(&ctx, size, vec![], |ui| twice(ui, &mut world));
        tab_to(&ctx, size, "Live preview", false, 20, |ui| twice(ui, &mut world));
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Space, egui::Modifiers::NONE)],
            |ui| twice(ui, &mut world),
        );
        frame(&ctx, size, vec![], |ui| twice(ui, &mut world));
        assert!(
            world.resource::<GpuBrowser>().live,
            "one press turns Live on and it stays on"
        );
    }

    /// A device on lavapipe (or any adapter): `None` skips the GPU tests where none exists.
    fn device() -> Option<regia_render::RenderDevice> {
        match regia_render::RenderDevice::shared_headless() {
            Ok(device) => Some(device),
            Err(err) => {
                eprintln!("skip gpu_browser GPU test: {err}");
                None
            }
        }
    }

    /// Acceptance (P-313): a known 1 MiB texture (512 x 512 RGBA8) appears in the list with its
    /// size, format and usage, and its preview's pixels match its content.
    #[test]
    fn a_known_one_mib_texture_is_listed_and_its_preview_matches_its_content() {
        let Some(device) = device() else {
            return;
        };
        regia_render::set_debug_readback(true);
        let label = "gpu browser test pattern";
        let (w, h) = (512u32, 512u32);
        let texel = |x: u32, y: u32| {
            [
                (x % 256) as u8,
                (y % 256) as u8,
                ((x / 8 + y / 8) % 2 * 255) as u8,
                255,
            ]
        };
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&texel(x, y));
            }
        }
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        device.queue.write_texture(
            texture.as_image_copy(),
            &data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            texture.size(),
        );
        regia_render::resource_inspector::track(label, &texture);
        let mut world = World::new();
        world.insert_resource(device);
        let out = gpu_resources(&mut world, &json!({ "action": "list", "top": 5000 })).expect("list");
        let row = out["resources"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["label"] == label))
            .unwrap_or_else(|| panic!("{label} is listed: {out}"));
        assert_eq!(row["bytes"], json!(1 << 20), "{row}");
        assert_eq!(row["kind"], "texture");
        assert_eq!(row["format"], "Rgba8Unorm");
        assert_eq!(
            (row["width"].clone(), row["height"].clone()),
            (json!(512), json!(512))
        );
        assert!(
            row["usage"].as_str().is_some_and(|u| u.contains("COPY_SRC")),
            "{row}"
        );
        assert_eq!(row["previewable"], true);
        // The panel's bounded preview: 256 x 256, every second texel.
        let img = read_preview(&world, label, 0, 0, Channel::Rgba, PREVIEW_MAX).expect("preview");
        assert_eq!(
            (img.width, img.height, img.source_width, img.source_height),
            (256, 256, 512, 512)
        );
        for (x, y) in [(0u32, 0u32), (1, 0), (37, 101), (255, 255), (128, 7)] {
            let i = ((y * img.width + x) * 4) as usize;
            assert_eq!(
                &img.rgba[i..i + 4],
                &texel(x * 2, y * 2),
                "preview pixel ({x}, {y})"
            );
        }
        // The agent preview at full size reads every texel back unchanged.
        let out = gpu_resources(
            &mut world,
            &json!({ "action": "preview", "label": label, "max_size": 256, "channel": "g", "pixels": true }),
        )
        .expect("agent preview");
        assert_eq!(out["width"], json!(256));
        assert_eq!(
            out["pixels"][3 * 256 + 5],
            json!([6, 6, 6, 255]),
            "green of texel (10, 6)"
        );
        let err = gpu_resources(
            &mut world,
            &json!({ "action": "preview", "label": label, "mip": 1 }),
        )
        .expect_err("one mip");
        assert!(format!("{err:?}").contains("subresource_out_of_range"), "{err:?}");
    }

    /// The browser is reached and operated from the keyboard: Tab to the first row, Down steps to
    /// the next and the preview follows the selection (every widget named).
    #[test]
    fn the_resource_list_steps_with_the_arrow_keys() {
        use crate::canvas_keys::test_input::{focused_name, frame, key, tab_to};
        let Some(device) = device() else {
            return;
        };
        regia_render::set_debug_readback(true);
        let make = |label: &str, side: u32| {
            let texture = device.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: side,
                    height: side,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            regia_render::resource_inspector::track(label, &texture);
            texture
        };
        let _big = make("gpu browser keys big", 600);
        let _small = make("gpu browser keys small", 300);
        // A fixed list: other tests register textures in the same process-wide registry.
        let rows: Vec<GpuRow> = current_rows(&World::new())
            .0
            .into_iter()
            .filter(|r| r.label.starts_with("gpu browser keys"))
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "gpu browser keys big", "sorted by size");
        let (big, small) = (rows[0].row_name(), rows[1].row_name());
        let mut world = World::new();
        world.insert_resource(device);
        let ctx = egui::Context::default();
        let size = [900.0, 900.0];
        let draw = |ui: &mut egui::Ui, world: &mut World| {
            egui::CentralPanel::default().show(ui, |ui| rows_ui(ui, world, &rows, Some("test")));
        };
        frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
        let update = tab_to(&ctx, size, &big, false, 10, |ui| draw(ui, &mut world));
        assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
        assert_eq!(
            world.resource::<GpuBrowser>().selected.as_deref(),
            Some("gpu browser keys big")
        );
        frame(
            &ctx,
            size,
            vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)],
            |ui| draw(ui, &mut world),
        );
        let down = frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
        assert_eq!(focused_name(&down).as_deref(), Some(small.as_str()));
        let b = world.resource::<GpuBrowser>();
        assert_eq!(b.selected.as_deref(), Some("gpu browser keys small"));
        let shown = b.shown.as_ref().expect("a preview was read");
        assert_eq!(
            shown.key.0, "gpu browser keys small",
            "the preview follows the selection"
        );
        assert_eq!(shown.image.as_ref().map(|i| (i.width, i.height)), Ok((256, 256)));
    }
}
