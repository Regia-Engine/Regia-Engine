// SPDX-License-Identifier: MPL-2.0
//! Material texture streaming: mip residency by screen-space need, with a fade-in.
//!
//! A streamed material ([`TextureStreaming::enabled`], standard surface only — graph
//! materials stay fully resident) first uploads only the low mips of its base colour,
//! normal, ORM and emissive maps (at most [`TextureStreaming::initial_max_px`] on the largest
//! edge). Every frame each visible draw measures how many texels its material needs along an
//! edge to keep one texel per output pixel (from the mesh's world-space size per UV unit
//! `uv_span`, the distance and the vertical pixel density); the material keeps the largest.
//! When the need passes what is resident, the next larger set (one mip level up) is built off
//! to the side and filled a few rows at a time inside a per-frame budget — a time budget
//! ([`TextureStreaming::upload_us_per_frame`]) at the measured upload rate, capped at
//! [`TextureStreaming::upload_kib_per_frame`] — so a 4 K level never lands in one frame.
//! Once complete it replaces the old set (one new bind group) and fades in: the material
//! uniform carries a minimum LOD (`detail.w`) that starts at the number of new levels and
//! falls to 0 over [`TextureStreaming::fade_frames`]; the shader stretches its UV gradients
//! so the sampler never goes below it (anisotropy kept). At 0 the shader takes the plain
//! `textureSample` path, so a fully resident material draws exactly as an unstreamed one.
//!
//! The tier cap ([`TextureStreaming::max_px`]) bounds the largest level ever resident (Deck:
//! 2048), so 4 K authoring costs a 2 K budget there.
//!
//! A cooked block-compressed slot ([`crate::compressed_texture::CompressedTexture`], the BC7 /
//! BC5 / ASTC chains a pak carries) streams the same way: its levels are read from the cooked
//! chain the material holds (no CPU copy), uploaded a few block rows at a time, and its lowest
//! resident set never drops below one block at level 0.
//!
//! **Budget** ([`TextureStreaming::budget_mib`], every game preset since lane tex-budget): the
//! streamed sets of all materials together stay under a byte budget per tier
//! ([`TextureStreaming::for_preset`]). An eager policy ([`TextureStreaming::eager`]) uploads a
//! new material up to its cap while the budget has room — so frames match the unstreamed ones
//! until memory runs short — and at its floor ([`TextureStreaming::initial_max_px`]) after
//! that. Over the budget, the least-recently-seen materials drop their top mips (the remaining
//! levels are copied on the GPU, nothing is read back or re-decoded); a material seen this frame
//! never drops below what its screen size needs. The CPU mip chain of an RGBA8 source exists
//! only while a larger set is being filled: once a material is at the residency it wants, the
//! renderer holds no CPU copy of its pixels (a later stream-in rebuilds the chain from the
//! material's level 0).
//!
//! **Out of memory** (ADR-223, v0.1.16): every streamed texture is created inside a wgpu
//! `OutOfMemory` error scope, so a failed allocation is caught (counted by
//! [`crate::gpu_health::captured_oom_count`], which also signals memory pressure) and never
//! reaches the uncaptured-error handler. A material's first set that fails is retried one mip
//! level smaller, down to its last level; a larger set that fails is dropped (the material keeps
//! what it has), and streaming then backs off: no uploads and no eager first use for
//! [`STREAM_OOM_BACKOFF`], doubling on each further failure up to [`STREAM_OOM_BACKOFF_MAX`].
//! While [`crate::gpu_health::texture_mips_held`] holds, need-driven growth stops one level short
//! and first use is not eager; each new hold makes every set give up a level once (never below
//! its floor). On integrated GPUs the budget is [`TextureStreaming::for_integrated_gpu`].
//!
//! Need is the isotropic estimate (Williams 1983, "Pyramidal Parametrics": LOD = log2 of
//! texels per pixel) with a margin ([`TextureStreaming::need_margin_percent`]) for the finer
//! mips anisotropic filtering reads at grazing angles.

/// A discrete card's texture budget is at least its device-local VRAM over this
/// ([`TextureStreaming::vram_scaled_budget_mib`]).
const VRAM_TEXTURE_SHARE_DIVISOR: u64 = 4;

/// How long streaming backs off after a streamed texture could not be allocated (doubling on
/// each further failure, up to [`STREAM_OOM_BACKOFF_MAX`]).
pub const STREAM_OOM_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

/// The longest streaming back-off after repeated allocation failures.
pub const STREAM_OOM_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(60);

/// The nominal frame rate the back-off durations convert to frame counts with. Wall time
/// never gates the back-off (audit G, render5 determinism): at 60 presented frames per
/// second a 5 s back-off is 300 frames, exactly as before.
const BACKOFF_FPS: f32 = 60.0;

/// Back-off duration in frames at [`BACKOFF_FPS`] (at least 1).
fn backoff_frames(duration: std::time::Duration) -> u32 {
    (duration.as_secs_f32() * BACKOFF_FPS).round().max(1.0) as u32
}

/// Streaming policy, part of the frame quality ([`crate::FrameQuality::texture_streaming`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureStreaming {
    /// Stream material textures. Off uploads every map at full size on first use.
    pub enabled: bool,
    /// Largest edge uploaded at first use, texels (the low mips).
    pub initial_max_px: u32,
    /// Largest edge ever resident, texels (`0` = the authored size).
    pub max_px: u32,
    /// Streaming upload cap per frame, KiB (a level larger than this spreads over frames).
    pub upload_kib_per_frame: u32,
    /// Streaming time budget per frame, µs: the byte budget is this times the measured upload
    /// rate (staging writes run at 1–3 GB/s on the farm's RTX 5090), capped by
    /// [`Self::upload_kib_per_frame`]. Keeps streaming under the frame's hitch line.
    pub upload_us_per_frame: u32,
    /// Frames a newly resident set fades in over.
    pub fade_frames: u32,
    /// Need margin, percent of the isotropic estimate (`200` = one mip finer).
    pub need_margin_percent: u32,
    /// Stream every material to its cap, nearest need first, instead of stopping at the need:
    /// the frame converges to exactly the unstreamed image (desktop; memory is not short).
    pub resident_all: bool,
    /// Resident budget of all streamed material maps together, MiB (`0` = unlimited). Over it,
    /// the least-recently-seen materials drop their top mips; a stream-in that would not fit
    /// evicts first or waits.
    pub budget_mib: u32,
    /// First use uploads as much as the budget has room for (up to [`Self::max_px`]) at once,
    /// so frames match the unstreamed ones while memory lasts; off uploads the low mips first.
    /// [`Self::initial_max_px`] is then the floor a material is uploaded at (and evicted down
    /// to) when the budget is full.
    pub eager: bool,
}

impl TextureStreaming {
    /// No streaming: every material map is resident at full size from its first frame.
    pub const OFF: Self = Self {
        enabled: false,
        initial_max_px: 0,
        max_px: 0,
        upload_kib_per_frame: 0,
        upload_us_per_frame: 0,
        fade_frames: 0,
        need_margin_percent: 100,
        resident_all: false,
        budget_mib: 0,
        eager: false,
    };
    /// Desktop: low mips first, then everything to the authored size by need, 1.5 ms of
    /// uploads a frame (at most 16 MiB), a 0.4 s fade at 60 fps. Settled frames are the
    /// unstreamed frames.
    pub const DESKTOP: Self = Self {
        enabled: true,
        initial_max_px: 256,
        max_px: 0,
        upload_kib_per_frame: 16 * 1024,
        upload_us_per_frame: 1500,
        fade_frames: 24,
        need_margin_percent: 400,
        resident_all: true,
        budget_mib: 0,
        eager: false,
    };
    /// Steam Deck: 2 K cap, 2 ms of uploads a frame (at most 8 MiB), one mip of margin.
    pub const DECK: Self = Self {
        enabled: true,
        initial_max_px: 256,
        max_px: 2048,
        upload_kib_per_frame: 8 * 1024,
        upload_us_per_frame: 2000,
        fade_frames: 16,
        need_margin_percent: 200,
        resident_all: false,
        budget_mib: 0,
        eager: false,
    };

    /// The budgeted tiers' shared shape: eager first use, need-driven growth past it with one
    /// mip of margin, 1.5 ms / 16 MiB of uploads a frame, a 0.4 s fade at 60 fps.
    const BUDGETED: Self = Self {
        enabled: true,
        initial_max_px: 256,
        max_px: 0,
        upload_kib_per_frame: 16 * 1024,
        upload_us_per_frame: 1500,
        fade_frames: 24,
        need_margin_percent: 200,
        resident_all: false,
        budget_mib: 0,
        eager: true,
    };
    /// Android, low: 1 K cap, 256 MiB (unified memory: the budget is also RAM), 128 px floor.
    pub const MOBILE_LOW: Self = Self {
        initial_max_px: 128,
        max_px: 1024,
        upload_kib_per_frame: 4 * 1024,
        upload_us_per_frame: 2000,
        fade_frames: 16,
        budget_mib: 256,
        ..Self::BUDGETED
    };
    /// Android, high: 2 K cap, 512 MiB.
    pub const MOBILE_HIGH: Self = Self {
        initial_max_px: 128,
        max_px: 2048,
        upload_kib_per_frame: 8 * 1024,
        upload_us_per_frame: 2000,
        fade_frames: 16,
        budget_mib: 512,
        ..Self::BUDGETED
    };
    /// Steam Deck: 2 K cap, 1 GiB of its 16 GB unified memory.
    pub const DECK_BUDGETED: Self = Self {
        max_px: 2048,
        upload_kib_per_frame: 8 * 1024,
        upload_us_per_frame: 2000,
        fade_frames: 16,
        budget_mib: 1024,
        ..Self::BUDGETED
    };
    /// Desktop Low (4–6 GB cards): 2 K cap, 1 GiB.
    pub const DESKTOP_LOW: Self = Self {
        max_px: 2048,
        budget_mib: 1024,
        ..Self::BUDGETED
    };
    /// Desktop High (8–12 GB cards): 4 K cap, 2 GiB. An 8 K authored map costs a 4 K one.
    pub const DESKTOP_HIGH: Self = Self {
        max_px: 4096,
        budget_mib: 2048,
        ..Self::BUDGETED
    };
    /// Desktop Ultra (16 GB+ cards): authored size, 4 GiB.
    pub const DESKTOP_ULTRA: Self = Self {
        max_px: 0,
        budget_mib: 4096,
        ..Self::BUDGETED
    };

    /// The budgeted policy of a profile preset: streaming is on in every game preset.
    #[must_use]
    pub fn for_preset(preset: crate::RenderProfilePreset) -> Self {
        use crate::RenderProfilePreset as P;
        match preset {
            P::MobileLow => Self::MOBILE_LOW,
            P::MobileHigh => Self::MOBILE_HIGH,
            P::DesktopLow => Self::DESKTOP_LOW,
            P::DesktopHigh => Self::DESKTOP_HIGH,
            // The offline profile streams like Ultra (ADR-1403).
            P::DesktopUltra | P::Cinematic => Self::DESKTOP_ULTRA,
            P::Deck => Self::DECK_BUDGETED,
        }
    }

    /// The resident budget in bytes; `u64::MAX` when streaming is off or unbudgeted.
    #[must_use]
    pub fn resident_budget_bytes(self) -> u64 {
        if self.enabled && self.budget_mib > 0 {
            u64::from(self.budget_mib) * 1024 * 1024
        } else {
            u64::MAX
        }
    }

    /// [`Self::budget_mib`] on a GPU with `vram_mib` of device-local memory: a discrete card's
    /// budget grows to a quarter of its VRAM, never below the preset's (each
    /// desktop preset's budget is that share of the smallest card its tier is for: Desktop
    /// High's 2 GiB a quarter of 8 GB, so a 16 GB card gets 4 GiB). Integrated, mobile and
    /// unknown memory, and an unbudgeted policy (`0`), keep the preset's. Applied by
    /// [`crate::FrameRenderer`] every frame unless
    /// [`crate::FrameRenderer::set_vram_scaled_budgets`] turned it off.
    #[must_use]
    pub fn vram_scaled_budget_mib(self, vram_mib: Option<u64>, discrete: bool) -> u32 {
        match vram_mib {
            Some(vram) if discrete && self.enabled && self.budget_mib > 0 => {
                let share = vram / VRAM_TEXTURE_SHARE_DIVISOR;
                self.budget_mib.max(u32::try_from(share).unwrap_or(u32::MAX))
            }
            _ => self.budget_mib,
        }
    }

    /// This policy on an integrated (shared-memory) GPU ([`crate::low_memory`], ADR-223): a
    /// budgeted policy's budget becomes at most
    /// [`crate::low_memory::INTEGRATED_STREAM_BUDGET_MIB`], at most the device-local heap
    /// (`heap_mib`) and at most a quarter of system RAM (`ram_mib`), never below
    /// [`crate::low_memory::INTEGRATED_BUDGET_FLOOR_MIB`] (unknown sizes are not limits), and
    /// first use uploads the low mips (`eager` off). Streaming off and unbudgeted policies
    /// (`budget_mib` 0: the editor, the lab) are returned unchanged. Applied by
    /// [`crate::FrameRenderer`] every frame when
    /// [`crate::low_memory::integrated_limits_apply`] says so.
    #[must_use]
    pub fn for_integrated_gpu(self, heap_mib: Option<u64>, ram_mib: Option<u64>) -> Self {
        use crate::low_memory::{
            INTEGRATED_BUDGET_FLOOR_MIB, INTEGRATED_RAM_SHARE_DIVISOR, INTEGRATED_STREAM_BUDGET_MIB,
        };
        if !self.enabled || self.budget_mib == 0 {
            return self;
        }
        let mib = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
        let cap = [
            Some(INTEGRATED_STREAM_BUDGET_MIB),
            heap_mib.map(mib),
            ram_mib.map(|ram| mib(ram / INTEGRATED_RAM_SHARE_DIVISOR)),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(INTEGRATED_STREAM_BUDGET_MIB)
        .max(INTEGRATED_BUDGET_FLOOR_MIB);
        Self {
            budget_mib: self.budget_mib.min(cap),
            eager: false,
            ..self
        }
    }

    /// The policy while [`crate::gpu_health::texture_mips_held`]: the need margin halved (a set
    /// wants one level less than its screen size asks for), no stream-everything-to-the-cap, no
    /// eager first use.
    pub(crate) fn under_pressure(self) -> Self {
        Self {
            need_margin_percent: (self.need_margin_percent / 2).max(1),
            resident_all: false,
            eager: false,
            ..self
        }
    }

    /// The policy of a complete frame (an offline capture, ADR-2582): every set the stream wants
    /// is uploaded in the frame that wants it, whatever the measured upload rate, and swaps in
    /// without a fade, so what a frame shows depends on the frames before it only through the
    /// need they measured, never on the CPU's or GPU's speed.
    #[must_use]
    pub fn complete(self) -> Self {
        Self {
            upload_kib_per_frame: u32::MAX,
            upload_us_per_frame: u32::MAX,
            fade_frames: 0,
            ..self
        }
    }

    fn budget_bytes(self) -> u64 {
        u64::from(self.upload_kib_per_frame.max(64)) * 1024
    }

    /// A complete frame's policy ([`Self::complete`]): no per-frame time budget. A growth step
    /// may jump straight to the wanted skip instead of one level a frame, so what a frame shows
    /// depends on the need measured, never on how many frames the climb took.
    fn is_complete(self) -> bool {
        self.upload_us_per_frame == u32::MAX
    }

    /// This frame's byte budget at `bytes_per_us` measured upload rate.
    fn frame_budget(self, bytes_per_us: f64) -> u64 {
        if self.upload_us_per_frame == u32::MAX {
            // No time budget ([`Self::complete`]): the measured rate is not read.
            return self.budget_bytes();
        }
        let by_time = (f64::from(self.upload_us_per_frame) * bytes_per_us) as u64;
        self.budget_bytes().min(by_time.max(64 * 1024))
    }
}

impl Default for TextureStreaming {
    fn default() -> Self {
        Self::OFF
    }
}

/// Streamed slots, in bind order: base colour, normal, ORM / occlusion, emissive.
pub(crate) const STREAMED_SLOTS: usize = 4;

/// One slot's level-0 image as the material holds it, or its cooked block-compressed chain.
#[derive(Clone, Copy)]
pub(crate) struct SlotSource<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba: &'a [u8],
    pub srgb: bool,
    /// A cooked chain the device samples: every level comes from it (`rgba` is unused).
    pub block: Option<&'a crate::compressed_texture::CompressedTexture>,
}

/// One streamed slot: its full size and the CPU levels 1.. (level 0 is the material's own
/// image, read again from the source when a stream-in reaches it).
struct Slot {
    width: u32,
    height: u32,
    /// Texture format: RGBA8 (sRGB or not) or the cooked block format.
    format: wgpu::TextureFormat,
    /// Format the slot is sampled through (the sRGB / linear variant of [`Self::format`]).
    view_format: wgpu::TextureFormat,
    /// Levels 1.. `(w, h, bytes)`; empty unless a larger set is being filled.
    chain: Vec<(u32, u32, Vec<u8>)>,
    /// Level count of the full chain (1 + levels below level 0).
    levels: u32,
}

impl Slot {
    fn level<'a>(&'a self, level: u32, source: Option<SlotSource<'a>>) -> Option<(u32, u32, &'a [u8])> {
        if let Some(block) = source.and_then(|s| s.block) {
            let bytes: &[u8] = block.levels.get(level as usize)?;
            return Some(((self.width >> level).max(1), (self.height >> level).max(1), bytes));
        }
        if level == 0 {
            let s = source?;
            let bytes = s.rgba.get(..crate::mips::rgba8_bytes(self.width, self.height)?)?;
            return Some((self.width, self.height, bytes));
        }
        // A freed level (empty) is missing, not empty: a set must never upload nothing for it.
        self.chain
            .get(level as usize - 1)
            .filter(|(_, _, b)| !b.is_empty())
            .map(|(w, h, b)| (*w, *h, b.as_slice()))
    }

    /// The top level a set that skips `skip` levels starts at: at most the last level, and for
    /// a block format never smaller than one block (level 0 of a texture spans whole blocks).
    fn base(&self, skip: u32) -> u32 {
        let (bw, bh) = self.format.block_dimensions();
        let mut base = skip.min(self.levels - 1);
        while base > 0 && ((self.width >> base) < bw || (self.height >> base) < bh) {
            base -= 1;
        }
        base
    }

    /// The texture of a set whose top level is `base`, created inside an `OutOfMemory` error
    /// scope: `None` (counted, pressure signalled: [`crate::gpu_health::note_captured_oom`]) when
    /// the device could not allocate it.
    fn try_create_texture(&self, d: &wgpu::Device, base: u32) -> Option<wgpu::Texture> {
        oom_scoped(d, "streamed texture", || {
            #[cfg(any(test, feature = "test-hooks"))]
            if hooks::take_texture_oom() {
                return hooks::oom_texture(d);
            }
            self.create_texture(d, base)
        })
    }

    /// The texture of a set whose top level is `base`.
    fn create_texture(&self, d: &wgpu::Device, base: u32) -> wgpu::Texture {
        let (bw, bh) = self.format.block_dimensions();
        let view_formats = [self.view_format];
        d.create_texture(&wgpu::TextureDescriptor {
            label: Some("material texture (streamed)"),
            // Block formats need whole blocks at level 0.
            size: wgpu::Extent3d {
                width: (self.width >> base).max(1).next_multiple_of(bw),
                height: (self.height >> base).max(1).next_multiple_of(bh),
                depth_or_array_layers: 1,
            },
            mip_level_count: (self.levels - base).max(1),
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            // COPY_SRC: an eviction copies the lower levels into a smaller texture.
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: if self.view_format == self.format {
                &[]
            } else {
                &view_formats
            },
        })
    }

    /// The view the material binds.
    fn view(&self, texture: &wgpu::Texture) -> wgpu::TextureView {
        texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(self.view_format),
            ..Default::default()
        })
    }
}

/// Runs `f` inside an `OutOfMemory` error scope: `None` (counted, pressure signalled:
/// [`crate::gpu_health::note_captured_oom`]) when the device failed an allocation in it. A
/// scope catches what the device refuses while `f` runs: texture creation, the staging buffer of
/// a `write_texture`, a bind group's descriptors. It cannot catch a failure at queue submit (the
/// copies an encoder only records), which wgpu reports as a device error.
pub(crate) fn oom_scoped<T>(d: &wgpu::Device, what: &str, f: impl FnOnce() -> T) -> Option<T> {
    let scope = d.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let out = f();
    // Native wgpu resolves the scope at once (nothing is awaited).
    match regia_core::tasks::block_on(scope.pop()) {
        None => Some(out),
        Some(error) => {
            crate::gpu_health::note_captured_oom(what, &error);
            None
        }
    }
}

/// [`oom_scoped`] around texture uploads (`queue.write_texture` stages the data in a buffer the
/// device may refuse).
fn oom_scoped_upload<T>(d: &wgpu::Device, f: impl FnOnce() -> T) -> Option<T> {
    oom_scoped(d, "streamed texture upload", || {
        #[cfg(any(test, feature = "test-hooks"))]
        if hooks::take_upload_oom() {
            drop(hooks::oom_texture(d));
        }
        f()
    })
}

/// Test hooks (cargo feature `test-hooks`, on in this crate's own tests; never in a shipped
/// game): make the next streamed allocations fail with the device's real out-of-memory error.
#[cfg(any(test, feature = "test-hooks"))]
mod hooks {
    use std::sync::atomic::{AtomicU32, Ordering};

    pub(super) static TEXTURES: AtomicU32 = AtomicU32::new(0);
    pub(super) static UPLOADS: AtomicU32 = AtomicU32::new(0);

    pub(super) fn take(counter: &AtomicU32) -> bool {
        counter
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
    }

    pub(super) fn take_texture_oom() -> bool {
        take(&TEXTURES)
    }

    pub(super) fn take_upload_oom() -> bool {
        take(&UPLOADS)
    }

    /// The allocation the hooks ask for: legal under the device's limits, far larger than any
    /// device's memory.
    pub(super) fn oom_texture(d: &wgpu::Device) -> wgpu::Texture {
        let limits = d.limits();
        let edge = limits.max_texture_dimension_2d.min(16384);
        d.create_texture(&wgpu::TextureDescriptor {
            label: Some("material texture (streamed, injected out-of-memory test)"),
            size: wgpu::Extent3d {
                width: edge,
                height: edge,
                depth_or_array_layers: limits.max_texture_array_layers.clamp(1, 2048),
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }
}

/// Test hook (memory-pressure tests): the next `count` streamed texture creations ask the device
/// for a legal but huge texture instead (16384² RGBA32F, as many layers as the device allows: a
/// TiB-class allocation), so the device's own out-of-memory path runs, caught by the stream's
/// error scope. Process-wide; `0` disarms it.
#[cfg(any(test, feature = "test-hooks"))]
pub fn inject_stream_texture_oom(count: u32) {
    hooks::TEXTURES.store(count, std::sync::atomic::Ordering::Relaxed);
}

/// Injected texture failures not used yet ([`inject_stream_texture_oom`]).
#[cfg(any(test, feature = "test-hooks"))]
#[must_use]
pub fn injected_stream_texture_ooms_left() -> u32 {
    hooks::TEXTURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Test hook: the next `count` streamed texture uploads (a set's first use, a growth step's
/// rows, a shrink's re-uploads) run next to the same huge allocation, so the upload's error
/// scope sees a real out-of-memory error. Process-wide; `0` disarms it.
#[cfg(any(test, feature = "test-hooks"))]
pub fn inject_stream_upload_oom(count: u32) {
    hooks::UPLOADS.store(count, std::sync::atomic::Ordering::Relaxed);
}

/// Injected upload failures not used yet ([`inject_stream_upload_oom`]).
#[cfg(any(test, feature = "test-hooks"))]
#[must_use]
pub fn injected_stream_upload_ooms_left() -> u32 {
    hooks::UPLOADS.load(std::sync::atomic::Ordering::Relaxed)
}

/// A larger texture set being filled under the frame budget.
struct Pending {
    skip: u32,
    textures: [Option<wgpu::Texture>; STREAMED_SLOTS],
    /// Next upload: slot, level (absolute, from the full size) and first row.
    slot: usize,
    level: u32,
    row: u32,
}

/// What a frame's [`MaterialStream::step`] changed.
pub(crate) struct StepOutcome {
    /// New views for the streamed slots (the bind group must be rebuilt).
    pub views: Option<[Option<wgpu::TextureView>; STREAMED_SLOTS]>,
}

/// Residency state of one material's streamed maps.
pub(crate) struct MaterialStream {
    slots: [Option<Slot>; STREAMED_SLOTS],
    /// The resident set's textures (kept to copy their levels when the set shrinks).
    resident: [Option<wgpu::Texture>; STREAMED_SLOTS],
    /// Levels dropped from the top of every slot in the resident set.
    skip: u32,
    /// The tier cap as a skip (levels never made resident).
    min_skip: u32,
    /// The floor as a skip: a set never drops below it ([`TextureStreaming::initial_max_px`]).
    floor: u32,
    /// Largest edge (any slot) at full size.
    full_px: u32,
    /// GPU bytes of the set that skips `i` levels (the slots never change; the last entry
    /// stands for every deeper skip).
    bytes_by_skip: Vec<u64>,
    need_px: f32,
    need_frame: u64,
    /// The last frame a visible draw used this material (eviction order).
    last_seen: u64,
    pending: Option<Pending>,
    /// Fade: frames left, frames in all, and the starting minimum LOD.
    fade: Option<(u32, u32, f32)>,
    stepped: u64,
}

/// Levels to drop from a `full_px` edge so it is at most `max_px` (`0` = none).
pub(crate) fn skip_for(full_px: u32, max_px: u32) -> u32 {
    if max_px == 0 {
        return 0;
    }
    let mut skip = 0;
    while (full_px >> skip) > max_px.max(1) {
        skip += 1;
    }
    skip
}

/// The tier's `(min_skip, floor)` with what the cook knows about the material's slots
/// ([`regia_asset::MipStreamMeta`]; one `(largest edge, metadata)` per present slot). All slots of
/// a material skip the same number of levels, each from its own top, so only levels that no slot
/// needs stay out: one slot without metadata, or one that adds detail at its top, keeps them. A
/// slot that must not stream holds the whole material at the cap, and the slot that keeps the
/// most levels for its `min_resident_px` raises the floor. `deepest` is the longest chain.
pub(crate) fn cooked_skips(
    min_skip: u32,
    floor: u32,
    cooked: &[(u32, Option<regia_asset::MipStreamMeta>)],
    deepest: u32,
) -> (u32, u32) {
    let unneeded = cooked
        .iter()
        .map(|(px, meta)| meta.map_or(0, |m| skip_for(*px, m.detail_px)))
        .min()
        .unwrap_or(0);
    let min_skip = min_skip.max(unneeded.min(deepest.saturating_sub(1)));
    let metas = || cooked.iter().filter_map(|(px, meta)| Some((*px, (*meta)?)));
    let floor = if metas().any(|(_, m)| !m.stream) {
        min_skip
    } else {
        metas()
            .filter(|(_, m)| m.min_resident_px != 0)
            .map(|(px, m)| skip_for(px, m.min_resident_px))
            .min()
            .map_or(floor, |resident| floor.min(resident))
    };
    (min_skip, floor.max(min_skip))
}

/// Object-space world length per UV unit of a mesh: `sqrt(Σ area_world / Σ area_uv)` over its
/// triangles (0 when the UVs are degenerate).
pub(crate) fn uv_span(vertices: &[crate::LabVertex], indices: &[u32]) -> f32 {
    let (mut world, mut uv) = (0.0f64, 0.0f64);
    for tri in indices.as_chunks::<3>().0 {
        let (Some(a), Some(b), Some(c)) = (
            vertices.get(tri[0] as usize),
            vertices.get(tri[1] as usize),
            vertices.get(tri[2] as usize),
        ) else {
            continue;
        };
        let p = |v: &crate::LabVertex| regia_core::glam::Vec3::from_array(v.position);
        world += f64::from((p(b) - p(a)).cross(p(c) - p(a)).length() * 0.5);
        let (u0, u1, u2) = (a.uv, b.uv, c.uv);
        let cross = (u1[0] - u0[0]) * (u2[1] - u0[1]) - (u2[0] - u0[0]) * (u1[1] - u0[1]);
        uv += f64::from(cross.abs() * 0.5);
    }
    if uv > 1e-12 && world > 0.0 {
        (world / uv).sqrt() as f32
    } else {
        0.0
    }
}

/// Texels along an edge a surface `uv_span` metres per UV unit needs at `distance` metres, with
/// `pixels_per_radian` = output height / (2 tan(fov_y / 2)): one texel per pixel.
#[must_use]
pub(crate) fn needed_px(uv_span: f32, distance: f32, pixels_per_radian: f32) -> f32 {
    if uv_span <= 0.0 {
        return 0.0;
    }
    uv_span * pixels_per_radian / distance.max(1e-3)
}

/// Bytes of one row of blocks (one texel row for RGBA8) of a `w`-wide level, and the level's
/// row count, for `h` texel rows.
fn row_layout(format: wgpu::TextureFormat, w: u32, h: u32) -> (u32, u32) {
    let (bw, bh) = format.block_dimensions();
    let block_bytes = format.block_copy_size(None).unwrap_or(4);
    (w.div_ceil(bw) * block_bytes, h.div_ceil(bh))
}

/// Writes block rows `row..row + rows` (texel rows for RGBA8) of a `w`-wide level.
#[allow(clippy::too_many_arguments)] // one write_texture call
fn write_rows(
    q: &crate::device::TrackedQueue,
    texture: &wgpu::Texture,
    format: wgpu::TextureFormat,
    mip: u32,
    w: u32,
    row: u32,
    rows: u32,
    bytes: &[u8],
) {
    let (bw, bh) = format.block_dimensions();
    let (row_bytes, _) = row_layout(format, w, 1);
    let start = (row * row_bytes) as usize;
    let end = ((row + rows) * row_bytes) as usize;
    let Some(data) = bytes.get(start..end) else {
        return;
    };
    q.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: mip,
            origin: wgpu::Origin3d {
                x: 0,
                y: row * bh,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(row_bytes),
            rows_per_image: Some(rows),
        },
        // The copy extent is the physical (whole-block) size of the rows.
        wgpu::Extent3d {
            width: w.div_ceil(bw) * bw,
            height: rows * bh,
            depth_or_array_layers: 1,
        },
    );
}

/// Bytes of the levels `base..levels` of a slot (its texture at that base).
fn slot_bytes(slot: &Slot, base: u32) -> u64 {
    (base..slot.levels)
        .map(|level| {
            let (row_bytes, rows) = row_layout(
                slot.format,
                (slot.width >> level).max(1),
                (slot.height >> level).max(1),
            );
            u64::from(row_bytes) * u64::from(rows)
        })
        .sum()
}

/// Bytes of the set that skips `skip` levels of every slot.
fn set_bytes(slots: &[Option<Slot>; STREAMED_SLOTS], skip: u32) -> u64 {
    slots.iter().flatten().map(|s| slot_bytes(s, s.base(skip))).sum()
}

/// The most levels any slot has.
fn stream_levels(slots: &[Option<Slot>; STREAMED_SLOTS]) -> u32 {
    slots.iter().flatten().map(|s| s.levels).max().unwrap_or(1)
}

/// One streamed texture as [`MaterialStream::textures`] reports it.
pub(crate) struct StreamedTexture {
    /// Slot index ([`STREAMED_SLOTS`] order).
    pub slot: usize,
    /// Full (authored) size.
    pub full: (u32, u32),
    /// Resident top level's size.
    pub resident: (u32, u32),
    /// Texture format.
    pub format: wgpu::TextureFormat,
    /// GPU bytes of the resident set.
    pub gpu_bytes: u64,
    /// CPU bytes the stream holds for it (the mip chain while a set is being filled).
    pub cpu_bytes: u64,
}

impl MaterialStream {
    /// Builds the stream and its first texture set, uploaded now in full: the low mips, or —
    /// under an eager policy — the largest set up to the cap that fits in `headroom` bytes.
    pub(crate) fn new(
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        policy: TextureStreaming,
        sources: [Option<SlotSource<'_>>; STREAMED_SLOTS],
        headroom: u64,
        now: u64,
    ) -> (Self, [Option<wgpu::TextureView>; STREAMED_SLOTS]) {
        let _span = tracing::info_span!("material_stream_new").entered();
        let slots: [Option<Slot>; STREAMED_SLOTS] = std::array::from_fn(|i| {
            sources[i].and_then(|s| {
                if let Some(block) = s.block {
                    let view_format = if s.srgb {
                        block.format.add_srgb_suffix()
                    } else {
                        block.format.remove_srgb_suffix()
                    };
                    return (!block.levels.is_empty()).then(|| Slot {
                        width: block.width.max(1),
                        height: block.height.max(1),
                        format: block.format,
                        view_format,
                        chain: Vec::new(),
                        levels: block.levels.len() as u32,
                    });
                }
                let (width, height) = (s.width.max(1), s.height.max(1));
                if crate::mips::rgba8_bytes(width, height).is_none_or(|n| s.rgba.len() < n) {
                    return None;
                }
                let chain = crate::mips::mip_chain_rgba8(width, height, s.rgba, s.srgb);
                let format = if s.srgb {
                    wgpu::TextureFormat::Rgba8UnormSrgb
                } else {
                    wgpu::TextureFormat::Rgba8Unorm
                };
                Some(Slot {
                    width,
                    height,
                    format,
                    view_format: format,
                    levels: chain.len() as u32 + 1,
                    chain,
                })
            })
        });
        let full_px = slots
            .iter()
            .flatten()
            .map(|s| s.width.max(s.height))
            .max()
            .unwrap_or(1);
        let min_skip = skip_for(full_px, policy.max_px);
        let floor = skip_for(full_px, policy.initial_max_px.max(1)).max(min_skip);
        let deepest = stream_levels(&slots);
        let cooked: Vec<(u32, Option<regia_asset::MipStreamMeta>)> = slots
            .iter()
            .zip(&sources)
            .filter_map(|(slot, source)| {
                let slot = slot.as_ref()?;
                let meta = source.as_ref()?.block.and_then(|block| block.mip_stream);
                Some((slot.width.max(slot.height), meta))
            })
            .collect();
        let (min_skip, floor) = cooked_skips(min_skip, floor, &cooked, deepest);
        let skip = if policy.eager {
            (min_skip..=floor)
                .find(|&s| set_bytes(&slots, s) <= headroom)
                .unwrap_or(floor)
        } else {
            floor
        };
        let bytes_by_skip = (0..=deepest).map(|s| set_bytes(&slots, s)).collect();
        let mut stream = Self {
            bytes_by_skip,
            slots,
            resident: [None, None, None, None],
            skip,
            min_skip,
            floor,
            full_px,
            need_px: 0.0,
            need_frame: 0,
            last_seen: now,
            pending: None,
            fade: None,
            stepped: now,
        };
        // Every slot's texture, uploaded at once (one set skips the same levels in every slot): a
        // slot the device cannot allocate, or refuses the upload of, retries the whole set one
        // level smaller, down to its last level.
        let mut resident: [Option<wgpu::Texture>; STREAMED_SLOTS] = [None, None, None, None];
        let mut views: [Option<wgpu::TextureView>; STREAMED_SLOTS] = [None, None, None, None];
        let mut skip = skip;
        loop {
            let last_try = skip + 1 >= deepest;
            let mut failed = false;
            for (i, slot) in stream.slots.iter().enumerate() {
                let Some(slot) = slot.as_ref() else {
                    continue;
                };
                let base = slot.base(skip);
                let ready = slot.try_create_texture(d, base).and_then(|texture| {
                    oom_scoped_upload(d, || {
                        for level in base..slot.levels {
                            if let Some((lw, lh, bytes)) = slot.level(level, sources[i]) {
                                let (_, rows) = row_layout(slot.format, lw, lh);
                                write_rows(q, &texture, slot.format, level - base, lw, 0, rows, bytes);
                            }
                        }
                    })
                    .map(|()| texture)
                });
                views[i] = ready.as_ref().map(|texture| slot.view(texture));
                resident[i] = ready;
                failed |= resident[i].is_none();
                // The last try keeps every slot it can get (one that got nothing, not even its
                // last level, then uploads whole like an unstreamed slot; the set holds no
                // memory for it: [`Self::resident_bytes`]).
                if failed && !last_try {
                    break;
                }
            }
            if !failed || last_try {
                break;
            }
            resident = [None, None, None, None];
            views = [None, None, None, None];
            skip += 1;
        }
        stream.skip = skip;
        stream.resident = resident;
        // Levels above the tier cap are never resident: free their CPU copies now.
        for slot in stream.slots.iter_mut().flatten() {
            for level in slot.chain.iter_mut().take(min_skip.saturating_sub(1) as usize) {
                level.2 = Vec::new();
            }
        }
        stream.release_if_idle(policy, now);
        (stream, views)
    }

    /// A visible draw of this material needs `px` texels along an edge this frame.
    pub(crate) fn note_need(&mut self, px: f32, now: u64) {
        if self.need_frame != now {
            self.need_frame = now;
            self.need_px = 0.0;
        }
        self.need_px = self.need_px.max(px);
        self.last_seen = now;
    }

    /// Scheduling priority this frame: a pending set first, then the need over what is
    /// resident (visible materials before unseen ones).
    pub(crate) fn priority(&self, now: u64) -> f32 {
        if self.pending.is_some() {
            return f32::MAX;
        }
        let need = if self.need_frame == now { self.need_px } else { 0.0 };
        need / (self.full_px >> self.skip).max(1) as f32
    }

    /// Minimum LOD the shader samples (`0` = plain sampling).
    pub(crate) fn min_lod(&self) -> f32 {
        self.fade.map_or(0.0, |(left, total, start)| {
            start * left as f32 / total.max(1) as f32
        })
    }

    /// `true` once nothing is pending or fading and the set is at its cap.
    pub(crate) fn settled(&self) -> bool {
        self.pending.is_none() && self.fade.is_none() && self.skip == self.min_skip
    }

    /// The skip the last measured need asks for (with the policy's margin), at most the cap.
    fn need_skip(&self, policy: TextureStreaming) -> u32 {
        let want = self.need_px * policy.need_margin_percent as f32 / 100.0;
        let mut skip = 0;
        while skip < 31 && (self.full_px >> (skip + 1)) as f32 >= want && (self.full_px >> (skip + 1)) > 0 {
            skip += 1;
        }
        skip.max(self.min_skip)
    }

    /// The skip the last frame's need asks for.
    fn wanted_skip(&self, policy: TextureStreaming, now: u64) -> u32 {
        if policy.resident_all {
            return self.min_skip;
        }
        // Need measured this frame (the draw loop runs before the stream pass).
        if self.need_frame != now {
            return self.skip;
        }
        self.need_skip(policy)
    }

    /// Drops the CPU chain once the resident set is what the stream wants (nothing pending, no
    /// growth asked for): no CPU copy of uploaded pixels outlives the upload.
    fn release_if_idle(&mut self, policy: TextureStreaming, now: u64) {
        if self.pending.is_none() && self.wanted_skip(policy, now) >= self.skip {
            for slot in self.slots.iter_mut().flatten() {
                slot.chain = Vec::new();
            }
        }
    }

    /// Rebuilds the CPU chain of every RGBA8 slot from its level 0 (dropped after the last
    /// upload) when a level the set skipping `skip` reads is missing. The rebuilt chain keeps
    /// every level down to the tier cap: growth moves one level a step and each step's set
    /// reads all its levels again, so freeing the levels above `skip` left the next step
    /// nothing to upload (hero-regress: uninitialised, black mips swapped in).
    fn ensure_chain(&mut self, sources: [Option<SlotSource<'_>>; STREAMED_SLOTS], skip: u32) {
        let min_skip = self.min_skip;
        for (slot, source) in self.slots.iter_mut().zip(sources) {
            let (Some(slot), Some(source)) = (slot.as_mut(), source) else {
                continue;
            };
            if source.block.is_some() || slot.levels <= 1 {
                continue;
            }
            // Chain entry `i` is level `i + 1`; level 0 is read from the source.
            let first = slot.base(skip).max(1) as usize - 1;
            if !slot.chain.is_empty() && slot.chain.iter().skip(first).all(|level| !level.2.is_empty()) {
                continue;
            }
            if crate::mips::rgba8_bytes(slot.width, slot.height).is_none_or(|n| source.rgba.len() < n) {
                continue;
            }
            let _span = tracing::info_span!("material_stream_rebuild_chain").entered();
            slot.chain = crate::mips::mip_chain_rgba8(slot.width, slot.height, source.rgba, source.srgb);
            for level in slot.chain.iter_mut().take(min_skip.saturating_sub(1) as usize) {
                level.2 = Vec::new();
            }
        }
    }

    /// GPU bytes of the resident set.
    pub(crate) fn resident_bytes(&self) -> u64 {
        // A slot without a texture (the device refused its last level or its upload) holds no
        // memory, whatever the set's skip says.
        if self
            .slots
            .iter()
            .zip(&self.resident)
            .any(|(s, r)| s.is_some() && r.is_none())
        {
            return self
                .slots
                .iter()
                .zip(&self.resident)
                .filter_map(|(s, r)| s.as_ref().filter(|_| r.is_some()))
                .map(|s| slot_bytes(s, s.base(self.skip)))
                .sum();
        }
        self.bytes_at(self.skip)
    }

    /// GPU bytes of the set that skips `skip` levels.
    fn bytes_at(&self, skip: u32) -> u64 {
        let i = (skip as usize).min(self.bytes_by_skip.len().saturating_sub(1));
        self.bytes_by_skip.get(i).copied().unwrap_or(0)
    }

    /// GPU bytes a pending set reserves (allocated as its slots start uploading).
    pub(crate) fn pending_bytes(&self) -> u64 {
        self.pending.as_ref().map_or(0, |p| self.bytes_at(p.skip))
    }

    /// GPU bytes giving up the resident set's top level would free (`0` when a set is pending or
    /// the set is at its floor): what a persistent-pressure drop asks of this material.
    pub(crate) fn one_level_gain(&self) -> u64 {
        if self.pending.is_some() || self.skip >= self.floor {
            return 0;
        }
        self.bytes_at(self.skip)
            .saturating_sub(self.bytes_at(self.skip + 1))
    }

    /// CPU bytes the stream holds (the RGBA8 mip chains while a set is being filled).
    pub(crate) fn cpu_bytes(&self) -> u64 {
        self.slots
            .iter()
            .flatten()
            .flat_map(|s| s.chain.iter())
            .map(|level| level.2.len() as u64)
            .sum()
    }

    /// The streamed textures with their full and resident sizes and bytes.
    pub(crate) fn textures(&self) -> impl Iterator<Item = StreamedTexture> + '_ {
        self.slots.iter().enumerate().filter_map(|(i, slot)| {
            let slot = slot.as_ref()?;
            let base = slot.base(self.skip);
            Some(StreamedTexture {
                slot: i,
                full: (slot.width, slot.height),
                resident: ((slot.width >> base).max(1), (slot.height >> base).max(1)),
                format: slot.format,
                gpu_bytes: slot_bytes(slot, base),
                cpu_bytes: slot.chain.iter().map(|level| level.2.len() as u64).sum(),
            })
        })
    }

    /// The last frame a visible draw used this material.
    pub(crate) fn last_seen(&self) -> u64 {
        self.last_seen
    }

    /// The need measured for frame `now` (texels along an edge), `0` when no draw measured it
    /// (residency feedback reports it beside the GPU's, ADR-1110).
    pub(crate) fn need_px_at(&self, now: u64) -> f32 {
        if self.need_frame == now { self.need_px } else { 0.0 }
    }

    /// The resident set's largest edge, texels.
    pub(crate) fn resident_px(&self) -> u32 {
        (self.full_px >> self.skip).max(1)
    }

    /// The skip the next [`Self::step`] aims for: the wanted skip, but at most one level above
    /// the resident set on an interactive policy (a 4 K set never lands in one allocation
    /// burst); a complete policy ([`TextureStreaming::complete`]) jumps the whole gap in its
    /// step.
    fn next_want(&self, policy: TextureStreaming, now: u64) -> u32 {
        let want = self.wanted_skip(policy, now);
        if policy.is_complete() {
            return want;
        }
        want.max(self.skip.saturating_sub(1))
    }

    /// What the next [`Self::step`] would allocate if allowed to grow: the resident bytes of
    /// the wanted set (`None` when it would not start one).
    pub(crate) fn growth(&self, policy: TextureStreaming, now: u64) -> Option<u64> {
        if self.pending.is_some() {
            return None;
        }
        let want = self.next_want(policy, now);
        (want < self.skip).then(|| self.bytes_at(want))
    }

    /// This stream as an eviction candidate: how far its set may shrink (a material seen this
    /// frame keeps what its screen size needs; others go down to the floor) and the bytes at
    /// each step. `None` when it cannot shrink (a set pending, or already at its limit).
    pub(crate) fn evict_candidate(
        &self,
        key: u64,
        policy: TextureStreaming,
        now: u64,
    ) -> Option<crate::texture_budget::EvictCandidate> {
        if self.pending.is_some() {
            return None;
        }
        let limit = if self.last_seen == now && self.need_frame == now {
            self.need_skip(policy).min(self.floor)
        } else {
            self.floor
        };
        if limit <= self.skip {
            return None;
        }
        let bytes_by_skip: Vec<u64> = (self.skip..=limit).map(|s| self.bytes_at(s)).collect();
        // Skips that change nothing (a short chain) are not offered.
        if bytes_by_skip.last() >= bytes_by_skip.first() {
            return None;
        }
        let resident_px = (self.full_px >> self.skip).max(1) as f32;
        let need = if self.need_frame == now { self.need_px } else { 0.0 };
        Some(crate::texture_budget::EvictCandidate {
            key,
            last_seen: self.last_seen,
            excess: if need > 0.0 {
                resident_px / need
            } else {
                f32::INFINITY
            },
            skip: self.skip,
            bytes_by_skip,
        })
    }

    /// Drops the resident set's top levels so it skips `new_skip`: each slot gets a smaller
    /// texture whose levels are copied from the current one on the GPU (`encoder`); a level whose
    /// block-padded size differs is written from `sources` instead. Returns the new views (the
    /// bind group must be rebuilt), or `None` when nothing changed (a set pending, `new_skip`
    /// not smaller, or a level with neither a copy nor a source).
    pub(crate) fn shrink(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        new_skip: u32,
        sources: Option<[Option<SlotSource<'_>>; STREAMED_SLOTS]>,
    ) -> Option<[Option<wgpu::TextureView>; STREAMED_SLOTS]> {
        let new_skip = new_skip.min(self.floor);
        if self.pending.is_some() || new_skip <= self.skip {
            return None;
        }
        let _span = tracing::info_span!("material_stream_shrink").entered();
        let level_size = |texture: &wgpu::Texture, mip: u32, format: wgpu::TextureFormat| {
            texture
                .size()
                .mip_level_size(mip, wgpu::TextureDimension::D2)
                .physical_size(format)
        };
        // Check first, so a set never ends half shrunk.
        let mut plans: [Option<(u32, u32)>; STREAMED_SLOTS] = [None; STREAMED_SLOTS];
        for (i, slot) in self.slots.iter().enumerate() {
            let (Some(slot), Some(old)) = (slot.as_ref(), self.resident[i].as_ref()) else {
                continue;
            };
            let (old_base, new_base) = (slot.base(self.skip), slot.base(new_skip));
            if new_base == old_base {
                continue;
            }
            let new_size = wgpu::Extent3d {
                width: (slot.width >> new_base)
                    .max(1)
                    .next_multiple_of(slot.format.block_dimensions().0),
                height: (slot.height >> new_base)
                    .max(1)
                    .next_multiple_of(slot.format.block_dimensions().1),
                depth_or_array_layers: 1,
            };
            for level in new_base..slot.levels {
                let src = level_size(old, level - old_base, slot.format);
                let dst = new_size
                    .mip_level_size(level - new_base, wgpu::TextureDimension::D2)
                    .physical_size(slot.format);
                if src != dst && slot.level(level, sources.and_then(|s| s[i])).is_none() {
                    return None;
                }
            }
            plans[i] = Some((old_base, new_base));
        }
        // Every smaller texture first: one the device cannot allocate leaves the set as it was.
        let mut created: [Option<wgpu::Texture>; STREAMED_SLOTS] = [None, None, None, None];
        for (i, plan) in plans.iter().enumerate() {
            let (Some((_, new_base)), Some(slot)) = (plan, self.slots[i].as_ref()) else {
                continue;
            };
            created[i] = Some(slot.try_create_texture(d, *new_base)?);
        }
        // Fill every smaller texture before any replaces a resident one: a refused re-upload
        // leaves the set as it was. (The copies only record into `encoder`; the device can
        // refuse nothing there.)
        let mut staged: [Option<(wgpu::Texture, wgpu::TextureView)>; STREAMED_SLOTS] =
            [None, None, None, None];
        for (i, (plan, texture)) in plans.into_iter().zip(created).enumerate() {
            let (Some((old_base, new_base)), Some(texture), Some(slot), Some(old)) =
                (plan, texture, self.slots[i].as_ref(), self.resident[i].as_ref())
            else {
                continue;
            };
            oom_scoped_upload(d, || {
                for level in new_base..slot.levels {
                    let (src_mip, dst_mip) = (level - old_base, level - new_base);
                    let src = level_size(old, src_mip, slot.format);
                    let dst = level_size(&texture, dst_mip, slot.format);
                    if src == dst {
                        encoder.copy_texture_to_texture(
                            wgpu::TexelCopyTextureInfo {
                                texture: old,
                                mip_level: src_mip,
                                origin: wgpu::Origin3d::ZERO,
                                aspect: wgpu::TextureAspect::All,
                            },
                            wgpu::TexelCopyTextureInfo {
                                texture: &texture,
                                mip_level: dst_mip,
                                origin: wgpu::Origin3d::ZERO,
                                aspect: wgpu::TextureAspect::All,
                            },
                            dst,
                        );
                    } else if let Some((w, h, bytes)) = slot.level(level, sources.and_then(|s| s[i])) {
                        let (_, rows) = row_layout(slot.format, w, h);
                        write_rows(q, &texture, slot.format, dst_mip, w, 0, rows, bytes);
                    }
                }
            })?;
            let view = slot.view(&texture);
            staged[i] = Some((texture, view));
        }
        let mut views: [Option<wgpu::TextureView>; STREAMED_SLOTS] = [None, None, None, None];
        for (i, staged) in staged.into_iter().enumerate() {
            if let Some((texture, view)) = staged {
                views[i] = Some(view);
                self.resident[i] = Some(texture);
            }
        }
        self.skip = new_skip;
        self.fade = None;
        Some(views)
    }

    /// One frame of streaming: advance the fade, continue a pending set inside `budget`
    /// bytes, or — when `may_grow` (the resident budget has room) — start one when the need
    /// grew. Runs once per frame per material.
    #[allow(clippy::too_many_arguments)] // the frame's streaming context
    pub(crate) fn step(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        policy: TextureStreaming,
        sources: [Option<SlotSource<'_>>; STREAMED_SLOTS],
        budget: &mut u64,
        may_grow: bool,
        now: u64,
    ) -> StepOutcome {
        let mut outcome = StepOutcome { views: None };
        if self.stepped == now {
            return outcome;
        }
        self.stepped = now;
        if let Some((left, total, start)) = self.fade {
            // A policy without a fade (complete frames, ADR-2582) also ends one in progress.
            self.fade = (left > 1 && policy.fade_frames > 0).then_some((left - 1, total, start));
        }
        if self.pending.is_none() {
            let want = self.next_want(policy, now);
            if want < self.skip && *budget > 0 && may_grow {
                self.ensure_chain(sources, want);
                self.pending = Some(Pending {
                    skip: want,
                    textures: [None, None, None, None],
                    slot: 0,
                    level: want,
                    row: 0,
                });
            } else {
                self.release_if_idle(policy, now);
            }
        }
        if self.pending.is_some() && self.fill(d, q, sources, budget) {
            let Some(pending) = self.pending.take() else {
                return outcome;
            };
            let gained = self.skip - pending.skip;
            self.skip = pending.skip;
            self.fade = (policy.fade_frames > 0 && gained > 0).then_some((
                policy.fade_frames,
                policy.fade_frames,
                gained as f32,
            ));
            // A slot the set has no texture for keeps its current view.
            let mut textures = pending.textures.into_iter();
            let mut resident = std::mem::take(&mut self.resident).into_iter();
            let mut new_resident: [Option<wgpu::Texture>; STREAMED_SLOTS] = [None, None, None, None];
            outcome.views = Some(std::array::from_fn(|i| {
                let old = resident.next().flatten();
                let Some(texture) = textures.next().flatten() else {
                    new_resident[i] = old;
                    return None;
                };
                let view = self.slots[i].as_ref().map(|s| s.view(&texture));
                new_resident[i] = Some(texture);
                view
            }));
            self.resident = new_resident;
            self.release_if_idle(policy, now);
        }
        outcome
    }

    /// Uploads rows of the pending set until `budget` runs out; `true` when it is complete.
    fn fill(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        sources: [Option<SlotSource<'_>>; STREAMED_SLOTS],
        budget: &mut u64,
    ) -> bool {
        let Some(pending) = self.pending.as_mut() else {
            return true;
        };
        let _span = tracing::info_span!("material_stream_fill").entered();
        while pending.slot < STREAMED_SLOTS {
            let Some(slot) = self.slots[pending.slot].as_ref() else {
                pending.slot += 1;
                pending.level = pending.skip;
                pending.row = 0;
                continue;
            };
            let base = slot.base(pending.skip);
            if *budget == 0 {
                return false;
            }
            // Each slot's texture is created when its upload starts (allocations spread too).
            if pending.textures[pending.slot].is_none() {
                let Some(texture) = slot.try_create_texture(d, base) else {
                    // Out of memory: drop the larger set (the resident one stays, one level
                    // down from it). The stream pass streams nothing more this frame
                    // ([`StreamFrame::halt_after_failure`]) and the next frames back off.
                    self.pending = None;
                    return false;
                };
                pending.textures[pending.slot] = Some(texture);
            }
            let Some(texture) = pending.textures[pending.slot].as_ref() else {
                return false;
            };
            pending.level = pending.level.max(base);
            if pending.level >= slot.levels {
                pending.slot += 1;
                pending.level = pending.skip;
                pending.row = 0;
                continue;
            }
            let Some((w, h, bytes)) = slot.level(pending.level, sources[pending.slot]) else {
                // The source went away (material replaced): give up on this set.
                self.pending = None;
                return false;
            };
            let (row_bytes, level_rows) = row_layout(slot.format, w, h);
            let row_bytes = u64::from(row_bytes);
            if *budget < row_bytes {
                return false;
            }
            let rows = ((*budget / row_bytes) as u32)
                .min(level_rows - pending.row)
                .max(1);
            let (level, row) = (pending.level, pending.row);
            let uploaded = oom_scoped_upload(d, || {
                write_rows(q, texture, slot.format, level - base, w, row, rows, bytes);
            });
            if uploaded.is_none() {
                // The device refused the staging memory: drop the larger set like a refused
                // texture (the resident set stays).
                self.pending = None;
                return false;
            }
            *budget = budget.saturating_sub(row_bytes * u64::from(rows));
            pending.row += rows;
            if pending.row >= level_rows {
                pending.row = 0;
                pending.level += 1;
            }
        }
        true
    }
}

/// Per-frame streaming state of the frame renderer.
#[derive(Default)]
pub(crate) struct StreamFrame {
    /// The policy of the frame being prepared.
    pub policy: TextureStreaming,
    /// Upload bytes left this frame.
    pub budget: u64,
    /// The frame's whole budget.
    pub start_budget: u64,
    /// Measured upload rate, bytes per µs (moving average; 0 = not measured yet).
    pub bytes_per_us: f64,
    /// Sets that completed and swapped in this frame.
    pub swapped: u32,
    /// Time the last stream pass took, µs.
    pub pass_us: u32,
    /// GPU bytes of every streamed set (resident and pending), re-totalled by each stream pass
    /// and raised as new materials upload: what the resident budget is checked against.
    pub resident: u64,
    /// [`crate::gpu_health::captured_oom_count`] at the start of this frame.
    ooms_seen: u64,
    /// Streaming backs off (no uploads, no eager first use) for this many more frames,
    /// after a failed allocation. Counted down once per frame at the nominal
    /// [`BACKOFF_FPS`], never on the wall clock: a replay that presents the same sim
    /// frames at any speed sees the same residency (audit G, render5 determinism).
    backoff_frames: u32,
    /// Frames since the back-off last ended; past [`STREAM_OOM_BACKOFF_MAX`] at
    /// [`BACKOFF_FPS`] the next failure starts short again.
    quiet_frames: u32,
    /// The current back-off length (doubles on each failure while backing off).
    backoff: std::time::Duration,
    /// [`crate::gpu_health::texture_mip_drops`] the last frame saw (`None` before the first).
    mip_drops_seen: Option<u64>,
    /// A new persistent-pressure drop is due ([`Self::take_pressure_drop`]); stays set until taken.
    drop_due: bool,
    /// This frame's policy is the held one ([`TextureStreaming::under_pressure`]).
    pub held: bool,
}

impl StreamFrame {
    /// Starts a frame under `policy`: held a mip lower while
    /// [`crate::gpu_health::texture_mips_held`], with no uploads and no eager first use while
    /// backing off after an allocation failure.
    /// `main_view`: the frame is the host's main view. A texture-view pass inside the host's frame
    /// takes the back-off as it stands but does not count down its frames.
    pub(crate) fn begin(&mut self, policy: TextureStreaming, main_view: bool) {
        self.begin_view_at(
            policy,
            crate::gpu_health::captured_oom_count(),
            crate::gpu_health::texture_mips_held(),
            crate::gpu_health::texture_mip_drops(),
            main_view,
        );
    }

    /// [`Self::begin`] of a main view with the process-wide health state given (testable):
    /// `ooms` caught allocation failures so far, `held` the mip hold, `drops` the
    /// persistent-pressure drops so far. The back-off counts frames, so there is no clock to
    /// inject.
    #[cfg(test)]
    fn begin_at(&mut self, policy: TextureStreaming, ooms: u64, held: bool, drops: u64) {
        self.begin_view_at(policy, ooms, held, drops, true);
    }

    /// [`Self::begin`] with the health state given. Only a main view steps the back-off's frame
    /// counts: a texture-view pass (a mirror, a capture) rendered in the same host frame used to
    /// count too, so a frame with several views ended the 300-frame back-off early
    /// (corr-render-rest LOW 24).
    fn begin_view_at(
        &mut self,
        policy: TextureStreaming,
        ooms: u64,
        held: bool,
        drops: u64,
        main_view: bool,
    ) {
        if self.mip_drops_seen.is_none() {
            // The counter is process-wide: failures caught before this renderer's first frame
            // (an earlier renderer, the one an engine-shaders rebuild replaced) are not its own.
            self.ooms_seen = ooms;
        }
        if ooms != self.ooms_seen {
            // Any renderer's failure counts: the memory is the device's.
            self.ooms_seen = ooms;
            self.backoff = (self.backoff * 2).clamp(STREAM_OOM_BACKOFF, STREAM_OOM_BACKOFF_MAX);
            self.backoff_frames = backoff_frames(self.backoff);
            self.quiet_frames = 0;
            tracing::info!(
                target: "regia_render",
                backoff_ms = self.backoff.as_millis() as u64,
                backoff_frames = self.backoff_frames,
                "texture streaming backs off after a failed allocation"
            );
        }
        let backing_off = self.backoff_frames > 0;
        if !main_view {
            // Inside the host's frame: no step of the frame counts.
        } else if backing_off {
            self.backoff_frames -= 1;
        } else if self.backoff > std::time::Duration::ZERO {
            // A quiet minute (in frames at BACKOFF_FPS) after the last back-off: the next
            // failure starts short again.
            self.quiet_frames += 1;
            if self.quiet_frames > backoff_frames(STREAM_OOM_BACKOFF_MAX) {
                self.backoff = std::time::Duration::ZERO;
                self.quiet_frames = 0;
            }
        }
        self.held = held;
        let mut policy = if self.held {
            policy.under_pressure()
        } else {
            policy
        };
        if backing_off {
            policy.eager = false;
        }
        // Kept until a stream pass takes it: a texture-view pass beginning first, or a frame
        // that does not stream, must not lose it.
        self.drop_due |= self.mip_drops_seen.is_some_and(|seen| seen != drops);
        self.mip_drops_seen = Some(drops);
        self.policy = policy;
        if self.bytes_per_us <= 0.0 {
            // 1 GB/s until the first upload is timed.
            self.bytes_per_us = 1000.0;
        }
        self.budget = if policy.enabled && !backing_off {
            policy.frame_budget(self.bytes_per_us)
        } else {
            0
        };
        self.start_budget = self.budget;
        self.swapped = 0;
    }

    /// Whether a persistent-pressure drop ([`crate::gpu_health::texture_mip_drops`]) is due:
    /// every streamed set gives up its top level once. Clears it.
    pub(crate) fn take_pressure_drop(&mut self) -> bool {
        std::mem::take(&mut self.drop_due)
    }

    /// Whether streaming is backing off after an allocation failure (or a failure this frame
    /// makes the next frame back off).
    pub(crate) fn backing_off(&self) -> bool {
        crate::gpu_health::captured_oom_count() != self.ooms_seen || self.backoff_frames > 0
    }

    /// The stream pass calls this before each material: once an allocation failed this frame
    /// (a first set in the frame's uploads, or a growth step), nothing more streams this frame
    /// (the rest of the frame's budget is dropped, what was uploaded stays counted). Returns
    /// whether streaming is halted.
    pub(crate) fn halt_after_failure(&mut self) -> bool {
        if crate::gpu_health::captured_oom_count() == self.ooms_seen {
            return false;
        }
        self.start_budget -= self.budget;
        self.budget = 0;
        true
    }

    /// Folds this frame's streaming (bytes uploaded in `elapsed`) into the rate estimate.
    pub(crate) fn measure(&mut self, elapsed: std::time::Duration) {
        let bytes = self.uploaded();
        let us = elapsed.as_secs_f64() * 1e6;
        self.pass_us = us as u32;
        // A failed allocation this frame cut the pass short: no rate.
        let failed = crate::gpu_health::captured_oom_count() != self.ooms_seen;
        if bytes >= 64 * 1024 && us > 1.0 && !failed {
            // Conservative: a slow frame lowers the rate at once, a fast one raises it slowly
            // (staging writes vary several-fold frame to frame; overshooting is the hitch).
            let rate = bytes as f64 / us;
            self.bytes_per_us = if rate < self.bytes_per_us {
                rate
            } else {
                0.9 * self.bytes_per_us + 0.1 * rate
            };
        }
    }

    /// Bytes streamed so far this frame.
    pub(crate) fn uploaded(&self) -> u64 {
        self.start_budget - self.budget
    }
}

/// Streaming totals for tools and the agent protocol ([`crate::FrameRenderer::stream_report`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamReport {
    /// Streamed materials.
    pub materials: u32,
    /// Of which settled (at their cap, nothing pending or fading).
    pub settled: u32,
    /// Of which with a set being filled.
    pub pending: u32,
    /// Of which fading in.
    pub fading: u32,
    /// Sum over streamed materials of the levels still dropped below their cap.
    pub levels_below_cap: u32,
    /// Bytes the last frame streamed.
    pub uploaded_bytes: u64,
    /// Sets the last frame completed and swapped in (one bind group each).
    pub swapped: u32,
    /// CPU time of the last frame's stream pass, µs (uploads are staged here; the GPU copies
    /// run inside the frame's submit).
    pub pass_us: u32,
    /// GPU bytes of the streamed sets (resident plus pending).
    pub resident_bytes: u64,
    /// CPU bytes the streams hold (RGBA8 mip chains while a set is being filled).
    pub cpu_bytes: u64,
}

impl MaterialStream {
    /// Adds this material to `report`.
    pub(crate) fn report(&self, report: &mut StreamReport) {
        report.materials += 1;
        report.settled += u32::from(self.settled());
        report.pending += u32::from(self.pending.is_some());
        report.fading += u32::from(self.fade.is_some());
        report.levels_below_cap += self.skip - self.min_skip;
        report.resident_bytes += self.resident_bytes() + self.pending_bytes();
        report.cpu_bytes += self.cpu_bytes();
    }
}

#[cfg(test)]
mod tests {
    use regia_asset::MipStreamMeta;

    #[test]
    fn cooked_metadata_moves_the_skips() {
        let meta = |detail_px: u32| Some(MipStreamMeta::new(detail_px));
        // 2048 maps under a tier that caps at 2048 (skip 0) with a 256 floor (skip 3).
        let tier = |cooked: &[Option<MipStreamMeta>]| {
            let slots: Vec<_> = cooked.iter().map(|meta| (2048, *meta)).collect();
            super::cooked_skips(0, 3, &slots, 12)
        };
        assert_eq!(tier(&[]), (0, 3), "no metadata: the tier alone");
        assert_eq!(tier(&[None, None]), (0, 3));
        assert_eq!(tier(&[meta(2048)]), (0, 3), "every level adds detail");
        assert_eq!(tier(&[meta(0)]), (0, 3), "0 means the same");
        assert_eq!(tier(&[meta(512)]), (2, 3), "levels above 512 add nothing");
        assert_eq!(
            tier(&[meta(16)]),
            (7, 7),
            "the cap passes the floor and takes it along"
        );
        assert_eq!(
            tier(&[meta(512), meta(1024)]),
            (1, 3),
            "the slot that needs most decides"
        );
        assert_eq!(
            tier(&[meta(512), None]),
            (0, 3),
            "a slot without metadata keeps every level"
        );
        assert_eq!(
            tier(&[
                meta(2048),
                Some(MipStreamMeta {
                    stream: false,
                    ..MipStreamMeta::new(2048)
                })
            ]),
            (0, 0),
            "one slot that must not stream holds the material at the cap"
        );
        assert_eq!(
            tier(&[Some(MipStreamMeta {
                min_resident_px: 1024,
                ..MipStreamMeta::new(2048)
            })]),
            (0, 1),
            "a higher floor"
        );
        assert_eq!(
            tier(&[Some(MipStreamMeta {
                min_resident_px: 64,
                ..MipStreamMeta::new(2048)
            })]),
            (0, 3),
            "a floor below the tier's changes nothing"
        );
    }

    #[test]
    fn a_short_chain_keeps_its_last_level() {
        let meta = Some(MipStreamMeta::new(16));
        assert_eq!(super::cooked_skips(0, 2, &[(2048, meta)], 3), (2, 2));
    }

    #[test]
    fn the_tier_cap_stays_when_the_cook_caps_less() {
        let meta = Some(MipStreamMeta::new(1024));
        assert_eq!(super::cooked_skips(2, 4, &[(4096, meta)], 13), (2, 4));
    }

    #[test]
    fn each_slot_counts_from_its_own_top() {
        // One skip takes a level off every slot, so a 1024 map that needs its 1024 level keeps
        // the set whole, however little the 2048 map next to it needs.
        let smooth = Some(MipStreamMeta::new(512));
        let detailed = Some(MipStreamMeta::new(1024));
        assert_eq!(
            super::cooked_skips(0, 3, &[(2048, smooth), (1024, detailed)], 12),
            (0, 3)
        );
        // A small map that is smooth too lets the set drop what both can spare.
        let small_smooth = Some(MipStreamMeta::new(512));
        assert_eq!(
            super::cooked_skips(0, 3, &[(2048, smooth), (1024, small_smooth)], 12),
            (1, 3)
        );
        // The same for the floor: 512 of a 1024 map is one level, not two.
        let keep = Some(MipStreamMeta {
            min_resident_px: 512,
            ..MipStreamMeta::new(1024)
        });
        assert_eq!(
            super::cooked_skips(0, 3, &[(2048, Some(MipStreamMeta::new(2048))), (1024, keep)], 12),
            (0, 1)
        );
    }

    use super::*;

    #[test]
    fn skip_caps_the_top_edge() {
        assert_eq!(skip_for(4096, 0), 0);
        assert_eq!(skip_for(4096, 2048), 1);
        assert_eq!(skip_for(4096, 256), 4);
        assert_eq!(skip_for(1000, 256), 2);
        assert_eq!(skip_for(128, 256), 0);
    }

    #[test]
    fn uv_span_of_a_unit_quad_mapped_once() {
        let v = |p: [f32; 3], uv: [f32; 2]| crate::LabVertex {
            position: p,
            uv,
            ..bytemuck::Zeroable::zeroed()
        };
        // A 4 m square mapped to the unit UV square: 4 m per UV unit.
        let verts = [
            v([0.0, 0.0, 0.0], [0.0, 0.0]),
            v([4.0, 0.0, 0.0], [1.0, 0.0]),
            v([4.0, 0.0, 4.0], [1.0, 1.0]),
            v([0.0, 0.0, 4.0], [0.0, 1.0]),
        ];
        let span = uv_span(&verts, &[0, 1, 2, 0, 2, 3]);
        assert!((span - 4.0).abs() < 1e-4, "{span}");
        assert_eq!(uv_span(&verts[..1], &[0, 0, 0]), 0.0);
    }

    #[test]
    fn need_falls_with_distance() {
        // 4 m per UV, 1080 px over a 90° field: ~540 px per radian.
        let ppr = 1080.0 / (2.0 * (45f32.to_radians()).tan());
        let near = needed_px(4.0, 2.0, ppr);
        let far = needed_px(4.0, 64.0, ppr);
        assert!((near / far - 32.0).abs() < 1e-3);
        assert!(near > 1000.0 && far < 40.0);
        assert_eq!(needed_px(0.0, 1.0, ppr), 0.0);
    }

    /// The DO27 tester's Radeon 610M (3153 MiB heap, 5877 MiB RAM) at Desktop Low: 384 MiB, not
    /// eager. Smaller machines get less; unbudgeted and off policies are untouched.
    #[test]
    fn integrated_gpus_get_a_small_non_eager_budget() {
        let low = TextureStreaming::DESKTOP_LOW.for_integrated_gpu(Some(3153), Some(5877));
        assert_eq!((low.budget_mib, low.eager), (384, false));
        assert_eq!(
            low.max_px,
            TextureStreaming::DESKTOP_LOW.max_px,
            "only the budget and eager change"
        );
        let high = TextureStreaming::DESKTOP_HIGH.for_integrated_gpu(Some(16_000), Some(32_000));
        assert_eq!(high.budget_mib, 384, "never above 384 MiB");
        // A quarter of 1 GiB of RAM; a 256 MiB carve-out; a tiny heap stops at the floor.
        assert_eq!(
            TextureStreaming::DESKTOP_LOW
                .for_integrated_gpu(None, Some(1024))
                .budget_mib,
            256
        );
        assert_eq!(
            TextureStreaming::DESKTOP_LOW
                .for_integrated_gpu(Some(256), Some(5877))
                .budget_mib,
            256
        );
        assert_eq!(
            TextureStreaming::DESKTOP_LOW
                .for_integrated_gpu(Some(32), None)
                .budget_mib,
            64
        );
        assert_eq!(
            TextureStreaming::DESKTOP_LOW
                .for_integrated_gpu(None, None)
                .budget_mib,
            384
        );
        // A preset budget under the cap stays.
        assert_eq!(
            TextureStreaming::MOBILE_LOW
                .for_integrated_gpu(Some(3153), Some(5877))
                .budget_mib,
            256
        );
        assert_eq!(
            TextureStreaming::DESKTOP.for_integrated_gpu(Some(3153), Some(5877)),
            TextureStreaming::DESKTOP
        );
        assert_eq!(
            TextureStreaming::OFF.for_integrated_gpu(Some(3153), Some(5877)),
            TextureStreaming::OFF
        );
    }

    #[test]
    fn under_pressure_wants_one_level_less_and_is_never_eager() {
        let held = TextureStreaming::DESKTOP.under_pressure();
        assert_eq!(held.need_margin_percent, 200);
        assert!(!held.resident_all && !held.eager);
        assert_eq!(
            TextureStreaming::DESKTOP_LOW.under_pressure().need_margin_percent,
            100
        );
        assert_eq!(
            held.budget_mib,
            TextureStreaming::DESKTOP.budget_mib,
            "the budget is the gate's"
        );
    }

    #[test]
    fn a_caught_failure_backs_streaming_off_then_it_resumes() {
        let policy = TextureStreaming::DESKTOP_LOW;
        let mut frame = StreamFrame::default();
        frame.begin_at(policy, 0, false, 0);
        assert!(frame.budget > 0 && frame.policy.eager);
        // A failure: no uploads and no eager first use for 5 s (300 frames at BACKOFF_FPS).
        frame.begin_at(policy, 1, false, 0);
        assert_eq!(frame.budget, 0);
        assert!(!frame.policy.eager);
        for _ in 0..299 {
            frame.begin_at(policy, 1, false, 0);
            assert_eq!(frame.budget, 0, "still backing off");
        }
        frame.begin_at(policy, 1, false, 0);
        assert!(frame.budget > 0 && frame.policy.eager, "resumed");
        // A second failure doubles the back-off (10 s = 600 frames).
        frame.begin_at(policy, 2, false, 0);
        for _ in 0..599 {
            frame.begin_at(policy, 2, false, 0);
            assert_eq!(frame.budget, 0, "10 s the second time");
        }
        frame.begin_at(policy, 2, false, 0);
        assert!(frame.budget > 0);
        // A quiet minute (3600 frames) later the next failure starts at 5 s again.
        for _ in 0..3600 {
            frame.begin_at(policy, 2, false, 0);
        }
        frame.begin_at(policy, 3, false, 0);
        for _ in 0..299 {
            frame.begin_at(policy, 3, false, 0);
            assert_eq!(frame.budget, 0, "5 s again");
        }
        frame.begin_at(policy, 3, false, 0);
        assert!(frame.budget > 0, "back to 5 s after a quiet minute");
    }

    /// A texture-view pass inside the host's frame backs off with it but does not count down the
    /// back-off (corr-render-rest LOW 24): a host frame with a mirror view still backs off for
    /// 300 host frames, not 150.
    #[test]
    fn view_passes_do_not_count_down_the_back_off() {
        let policy = TextureStreaming::DESKTOP_LOW;
        let mut frame = StreamFrame::default();
        frame.begin_at(policy, 0, false, 0);
        frame.begin_at(policy, 1, false, 0);
        assert_eq!(frame.budget, 0, "backing off");
        for _ in 0..299 {
            frame.begin_view_at(policy, 1, false, 0, false);
            assert_eq!(frame.budget, 0, "a view pass backs off too");
            assert!(!frame.policy.eager);
            frame.begin_at(policy, 1, false, 0);
            assert_eq!(frame.budget, 0, "still backing off");
        }
        frame.begin_at(policy, 1, false, 0);
        assert!(
            frame.budget > 0 && frame.policy.eager,
            "resumed after 300 host frames"
        );
    }

    /// Audit G finding 6 (render5 determinism): the back-off counts frames, never wall
    /// time. A replay that presents the same sim frames as fast as the CPU allows (zero
    /// wall time between frames) must see the same budget sequence.
    #[test]
    fn backoff_counts_frames_not_wall_time() {
        let policy = TextureStreaming::DESKTOP_LOW;
        let mut frame = StreamFrame::default();
        frame.begin_at(policy, 0, false, 0);
        assert!(frame.budget > 0);
        frame.begin_at(policy, 1, false, 0);
        assert_eq!(frame.budget, 0, "the failure frame backs off");
        // Present 300 frames back-to-back (no sleeps, no clock): exactly the 5 s
        // nominal back-off in frames.
        for _ in 0..299 {
            frame.begin_at(policy, 1, false, 0);
            assert_eq!(frame.budget, 0, "still backing off");
        }
        frame.begin_at(policy, 1, false, 0);
        assert!(
            frame.budget > 0 && frame.policy.eager,
            "uploads resume after exactly backoff_frames(STREAM_OOM_BACKOFF) frames"
        );
    }

    #[test]
    fn the_hold_lowers_the_policy_and_each_new_drop_is_due_once() {
        let mut frame = StreamFrame::default();
        frame.begin_at(TextureStreaming::DESKTOP, 0, false, 4);
        assert!(
            !frame.take_pressure_drop(),
            "drops before this renderer's first frame are not due"
        );
        frame.begin_at(TextureStreaming::DESKTOP, 0, true, 5);
        assert!(frame.held);
        assert_eq!(frame.policy, TextureStreaming::DESKTOP.under_pressure());
        assert!(frame.take_pressure_drop());
        assert!(!frame.take_pressure_drop(), "taken once");
        frame.begin_at(TextureStreaming::DESKTOP, 0, true, 5);
        assert!(!frame.take_pressure_drop(), "the same drop is not due twice");
        frame.begin_at(TextureStreaming::DESKTOP, 0, false, 5);
        assert!(!frame.held);
        assert_eq!(
            frame.policy,
            TextureStreaming::DESKTOP,
            "the hold ended: the policy is restored"
        );
    }

    /// The caught-failure count is process-wide: a renderer created after an earlier one's
    /// failure (or the one an engine-shaders rebuild replaces) must not back off for it.
    #[test]
    fn a_new_renderer_ignores_failures_caught_before_its_first_frame() {
        let policy = TextureStreaming::DESKTOP_LOW;
        let mut frame = StreamFrame::default();
        frame.begin_at(policy, 7, false, 3);
        assert!(
            frame.budget > 0 && frame.policy.eager,
            "no back-off for an old failure"
        );
        assert!(!frame.take_pressure_drop());
        // Its own failures still count.
        frame.begin_at(policy, 8, false, 3);
        assert_eq!(frame.budget, 0);
        assert!(!frame.policy.eager);
    }

    /// `begin` runs for every render pass (texture views too) and a frame may not stream at all:
    /// a drop stays due until a stream pass takes it.
    #[test]
    fn a_pressure_drop_waits_for_the_stream_pass_that_takes_it() {
        let mut frame = StreamFrame::default();
        frame.begin_at(TextureStreaming::DESKTOP, 0, false, 4);
        frame.begin_at(TextureStreaming::DESKTOP, 0, true, 5);
        // A second pass begins before any stream pass ran.
        frame.begin_at(TextureStreaming::DESKTOP, 0, true, 5);
        frame.begin_at(TextureStreaming::DESKTOP, 0, true, 5);
        assert!(frame.take_pressure_drop(), "still due");
        assert!(!frame.take_pressure_drop());
    }

    #[test]
    fn presets_order() {
        let presets = [
            TextureStreaming::OFF,
            TextureStreaming::DESKTOP,
            TextureStreaming::DECK,
        ];
        assert_eq!(presets.map(|p| p.enabled), [false, true, true]);
        assert_eq!(TextureStreaming::default(), TextureStreaming::OFF);
        assert_eq!(presets.map(|p| p.max_px), [0, 0, 2048]);
        assert_eq!(presets.map(|p| p.resident_all), [false, true, false]);
        assert!(presets[2].budget_bytes() < presets[1].budget_bytes());
        // 1.5 ms at 1 GB/s is 1.5 MB; the cap and the 64 KiB floor bound it.
        assert_eq!(presets[1].frame_budget(1000.0), 1_500_000);
        assert_eq!(presets[1].frame_budget(1e9), 16 * 1024 * 1024);
        assert_eq!(presets[1].frame_budget(0.0), 64 * 1024);
    }

    /// Determinism audit (render1, finding 3): a complete policy aims a growth step
    /// at the wanted skip itself — the step jumps the whole gap, so what a frame
    /// shows depends on the need measured, never on how many frames the climb took.
    /// An interactive policy still moves one level a step.
    #[test]
    fn a_complete_policy_aims_at_the_wanted_skip_not_one_level() {
        // 1024 px full, 256 px needed at a 200% margin: the wanted skip is 1 (a 512
        // edge); the resident set sits four levels coarser.
        let stream = MaterialStream {
            slots: [None, None, None, None],
            resident: [None, None, None, None],
            skip: 4,
            min_skip: 0,
            floor: 4,
            full_px: 1024,
            bytes_by_skip: (0..13).map(|s| 1u64 << (20 - s)).collect(),
            need_px: 256.0,
            need_frame: 0,
            last_seen: 0,
            pending: None,
            fade: None,
            stepped: u64::MAX,
        };
        assert_eq!(stream.wanted_skip(TextureStreaming::DESKTOP_HIGH, 0), 1);
        let interactive = stream
            .growth(TextureStreaming::DESKTOP_HIGH, 0)
            .expect("the interactive policy grows");
        assert_eq!(
            interactive,
            stream.bytes_at(3),
            "one level a step on an interactive policy"
        );
        let complete = stream
            .growth(TextureStreaming::DESKTOP_HIGH.complete(), 0)
            .expect("the complete policy grows");
        assert_eq!(
            complete,
            stream.bytes_at(1),
            "a complete policy jumps to the wanted skip in its step"
        );
    }

    /// ADR-2582: a complete frame's budget does not read the measured upload rate, and it fades
    /// nothing in.
    #[test]
    fn a_complete_policy_ignores_the_measured_rate_and_fades_nothing() {
        for policy in [TextureStreaming::DESKTOP, TextureStreaming::DECK] {
            let complete = policy.complete();
            assert_eq!(complete.fade_frames, 0);
            assert_eq!(complete.enabled, policy.enabled);
            assert_eq!(complete.resident_all, policy.resident_all);
            assert_eq!(complete.max_px, policy.max_px);
            let budgets = [0.0, 0.001, 1000.0, 1e9].map(|rate| complete.frame_budget(rate));
            assert!(budgets.iter().all(|&b| b == budgets[0]), "{budgets:?}");
            assert!(budgets[0] >= 1 << 40, "unbounded in practice: {}", budgets[0]);
        }
    }
}
