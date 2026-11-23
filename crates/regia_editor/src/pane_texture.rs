// SPDX-License-Identifier: MPL-2.0
//! Viewport panes shown straight from GPU textures (ADR-1555).
//!
//! A pane used to be rendered, read back to the CPU as an `egui::ColorImage` and uploaded again
//! with `Context::load_texture` every frame. The GPU path keeps the picture on the GPU: the
//! pane's renderer leaves its `Rgba8Unorm` LDR target (the same sRGB-encoded bytes the readback
//! returns), the pane hands it to [`PaneTextures::submit`], and the windowed shell registers it
//! with `egui_wgpu::Renderer` as an `egui::TextureId::User` ([`PaneTextures::apply`]). Egui's own
//! textures are `Rgba8Unorm` holding gamma-space bytes too, so both paths sample identical data.
//!
//! Lifetime: a slot is keyed by pane. [`PaneTextures::submit`] marks it used; [`PaneTextures::apply`]
//! (once per frame, after the egui run) registers new slots, rebinds live ones to the current
//! target (a resize or a renderer rebuild swaps the texture; the `TextureId` stays), and frees the
//! slots no pane submitted since the last call (a closed pane or panel). A freshly created slot has
//! no id until the first `apply`, so a pane shows its background for that one frame.
//!
//! The CPU readback stays as the fallback ([`PanePath::Cpu`]): headless runs, the agent's
//! screenshots and captures never go through this module, and `REGIA_EDITOR_PANE_PATH=cpu` or
//! `editor.viewport_pane_path` switches the live panel back.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use regia_ecs::prelude::{Resource, bevy_ecs};

/// Environment variable that picks the live panel's pane path at startup (`cpu` or `gpu`).
pub const PANE_PATH_ENV: &str = "REGIA_EDITOR_PANE_PATH";

/// How a live pane's picture reaches egui.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanePath {
    /// The renderer's target is registered as a native egui texture (no readback).
    #[default]
    Gpu,
    /// The picture is read back and uploaded as an egui `ColorImage` every frame.
    Cpu,
}

impl PanePath {
    /// Lowercase protocol name (`"gpu"`, `"cpu"`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Gpu => "gpu",
            Self::Cpu => "cpu",
        }
    }

    /// Parses a protocol name (case-insensitive); `None` for anything else.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "gpu" => Some(Self::Gpu),
            "cpu" => Some(Self::Cpu),
            _ => None,
        }
    }

    /// The path [`PANE_PATH_ENV`] asks for; [`PanePath::Gpu`] when unset or unrecognized.
    #[must_use]
    pub fn from_env() -> Self {
        std::env::var(PANE_PATH_ENV)
            .ok()
            .and_then(|text| Self::parse(&text))
            .unwrap_or_default()
    }
}

/// Which pane a texture belongs to: an instance's pane, or a main-viewport pane (`instance` is
/// `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneKey {
    /// The viewport instance id; `None` for the main viewport.
    pub instance: Option<u64>,
    /// The pane index within its viewport.
    pub pane: usize,
}

/// What [`PaneTextures::apply`] does to an egui renderer. `egui_wgpu::Renderer` (with its
/// device) is the real one; tests count calls with a stand-in.
pub trait NativeTextures {
    /// Registers `view` and returns the new user texture id.
    fn register(&mut self, view: &wgpu::TextureView) -> egui::TextureId;
    /// Points the existing `id` at `view`.
    fn rebind(&mut self, id: egui::TextureId, view: &wgpu::TextureView);
    /// Releases `id`.
    fn free(&mut self, id: egui::TextureId);
}

/// [`NativeTextures`] over a real `egui_wgpu::Renderer` and its device.
pub struct EguiNative<'a> {
    /// The egui renderer that draws the UI.
    pub renderer: &'a mut egui_wgpu::Renderer,
    /// Its device (the one the pane renderers draw on).
    pub device: &'a wgpu::Device,
}

impl NativeTextures for EguiNative<'_> {
    fn register(&mut self, view: &wgpu::TextureView) -> egui::TextureId {
        self.renderer
            .register_native_texture(self.device, view, wgpu::FilterMode::Linear)
    }

    fn rebind(&mut self, id: egui::TextureId, view: &wgpu::TextureView) {
        self.renderer
            .update_egui_texture_from_wgpu_texture(self.device, view, wgpu::FilterMode::Linear, id);
    }

    fn free(&mut self, id: egui::TextureId) {
        self.renderer.free_texture(&id);
    }
}

#[derive(Default)]
struct Slot {
    id: Option<egui::TextureId>,
    /// The target to bind at the next `apply`.
    pending: Option<wgpu::TextureView>,
    touched: bool,
}

#[derive(Default)]
struct Inner {
    path: PanePath,
    slots: HashMap<PaneKey, Slot>,
    registered: u64,
    freed: u64,
    rebound: u64,
    frames: u64,
    last_size: [u32; 2],
}

/// Counters and state of the pane textures, for `editor.viewport_stats` and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PaneTextureStats {
    /// The active path.
    pub path: PanePath,
    /// Registered native textures right now.
    pub live: usize,
    /// Textures registered since startup.
    pub registered: u64,
    /// Textures freed since startup.
    pub freed: u64,
    /// Rebinds (a new target behind an existing id) since startup.
    pub rebound: u64,
    /// Pane pictures submitted on the GPU path since startup.
    pub frames: u64,
    /// Size of the last submitted picture, pixels.
    pub last_size: [u32; 2],
}

/// The live panel's pane textures (see the module docs). Cloning shares the state, so the
/// viewport panels and the shell's present loop see the same slots.
#[derive(Resource, Clone, Default)]
pub struct PaneTextures {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for PaneTextures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaneTextures")
            .field("stats", &self.stats())
            .finish()
    }
}

impl PaneTextures {
    /// A registry on `path`.
    #[must_use]
    pub fn new(path: PanePath) -> Self {
        let textures = Self::default();
        textures.lock().path = path;
        textures
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The active path.
    #[must_use]
    pub fn path(&self) -> PanePath {
        self.lock().path
    }

    /// Switches the path. Leaving the GPU path frees every slot at the next [`Self::apply`].
    pub fn set_path(&self, path: PanePath) {
        self.lock().path = path;
    }

    /// Whether panes should render to GPU textures.
    #[must_use]
    pub fn gpu_enabled(&self) -> bool {
        self.path() == PanePath::Gpu
    }

    /// Counters and state.
    #[must_use]
    pub fn stats(&self) -> PaneTextureStats {
        let inner = self.lock();
        PaneTextureStats {
            path: inner.path,
            live: inner.slots.values().filter(|slot| slot.id.is_some()).count(),
            registered: inner.registered,
            freed: inner.freed,
            rebound: inner.rebound,
            frames: inner.frames,
            last_size: inner.last_size,
        }
    }

    /// Hands pane `key`'s finished picture to egui. Returns the texture id to paint with, or
    /// `None` until the first [`Self::apply`] after the slot was created.
    pub fn submit(&self, key: PaneKey, target: &regia_render::OffscreenTarget) -> Option<egui::TextureId> {
        let mut inner = self.lock();
        inner.frames += 1;
        inner.last_size = [target.width, target.height];
        let slot = inner.slots.entry(key).or_default();
        slot.pending = Some(target.view.clone());
        slot.touched = true;
        slot.id
    }

    /// The per-frame bookkeeping, called after the egui run and before the egui renderer draws:
    /// registers new slots, rebinds submitted ones, frees slots no pane submitted since the last
    /// call (and every slot when the CPU path is active).
    #[tracing::instrument(target = "regia_editor", skip_all, name = "pane_textures_apply")]
    pub fn apply(&self, natives: &mut dyn NativeTextures) {
        let mut inner = self.lock();
        let gpu = inner.path == PanePath::Gpu;
        let Inner {
            slots,
            registered,
            freed,
            rebound,
            ..
        } = &mut *inner;
        slots.retain(|_, slot| {
            if gpu && slot.touched {
                if let Some(view) = slot.pending.take() {
                    match slot.id {
                        Some(id) => {
                            natives.rebind(id, &view);
                            *rebound += 1;
                        }
                        None => {
                            slot.id = Some(natives.register(&view));
                            *registered += 1;
                        }
                    }
                }
                slot.touched = false;
                return true;
            }
            if let Some(id) = slot.id.take() {
                natives.free(id);
                *freed += 1;
            }
            false
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_names_round_trip() {
        for path in [PanePath::Gpu, PanePath::Cpu] {
            assert_eq!(PanePath::parse(path.name()), Some(path));
        }
        assert_eq!(PanePath::parse(" CPU "), Some(PanePath::Cpu));
        assert_eq!(PanePath::parse("vulkan"), None);
        assert_eq!(PanePath::default(), PanePath::Gpu);
    }

    #[test]
    fn a_registry_starts_empty_on_the_requested_path() {
        let textures = PaneTextures::new(PanePath::Cpu);
        assert!(!textures.gpu_enabled());
        assert_eq!(
            textures.stats(),
            PaneTextureStats {
                path: PanePath::Cpu,
                ..PaneTextureStats::default()
            }
        );
        textures.set_path(PanePath::Gpu);
        assert!(textures.gpu_enabled());
    }
}
