// SPDX-License-Identifier: MPL-2.0
//! Overlay pass hook: lets an engine crate (the runtime UI, `specs/18` § 3) draw on top of every
//! window frame, after the scene draw / clear and before screenshot readback and present.
//!
//! The renderer itself stays UI-agnostic: it only knows "someone may want to append draw commands
//! to this frame". `regia_ui` installs its overlay renderer through [`WindowOverlay::set`] when
//! its plugin is built; other passes (the quick draw layer, `regia_quick`; the 2D toolkit's
//! sprite/tilemap pass, `regia_2d`) add ordered layers with [`WindowOverlay::insert`] or
//! [`WindowOverlay::insert_shared`]. Layers draw in ascending order; the UI is order
//! [`WindowOverlay::UI_ORDER`].

use std::sync::{Arc, Mutex};

use regia_app::prelude::*;

use crate::RenderDevice;

/// Draws over a window frame. `view` is the frame's texture view (`format`, `width`×`height`).
pub trait OverlayDraw: Send {
    /// Records draw commands into `encoder`.
    fn draw(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    );

    /// [`Self::draw`] with the texture behind `view`, for passes that read the frame drawn so
    /// far (the UI's backdrop blur copies from it). Only called when the target is known; the
    /// texture's usage may lack `COPY_SRC` (then such passes fall back). The default ignores
    /// the texture.
    #[allow(clippy::too_many_arguments)]
    fn draw_with_target(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        target: &wgpu::Texture,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) {
        let _ = target;
        self.draw(device, encoder, view, format, width, height);
    }

    /// Builds whatever [`Self::draw`] would compile for `format` (pipelines, bind groups), so the
    /// first frame that draws this layer creates nothing. Called on a warm worker by the boot
    /// tier ([`crate::PipelineWarmQueue`]); the default does nothing.
    fn warm(&mut self, device: &RenderDevice, format: wgpu::TextureFormat) {
        let _ = (device, format);
    }
}

/// A shared overlay pass.
pub type SharedOverlay = Arc<Mutex<Box<dyn OverlayDraw>>>;

/// One installed overlay layer.
struct OverlayLayer {
    order: i32,
    name: &'static str,
    draw: SharedOverlay,
}

/// The registered overlay passes, run by `render_windows` for every window, every frame, in
/// ascending order.
///
/// Shared (not `ResMut`-owned) so the owner crate can keep a handle to the same state.
#[derive(Resource, Default)]
pub struct WindowOverlay {
    layers: Vec<OverlayLayer>,
}

impl WindowOverlay {
    /// Order of the runtime UI layer ([`Self::set`]). Lower orders draw first (underneath).
    pub const UI_ORDER: i32 = 0;

    /// Installs `overlay` as the UI overlay pass (name `"ui"`, [`Self::UI_ORDER`]), replacing any
    /// previous one.
    pub fn set(&mut self, overlay: Box<dyn OverlayDraw>) {
        self.insert(Self::UI_ORDER, "ui", overlay);
    }

    /// Installs `overlay` as layer `name` drawn at `order` (replacing a layer of the same name).
    /// Returns the shared handle.
    pub fn insert(&mut self, order: i32, name: &'static str, overlay: Box<dyn OverlayDraw>) -> SharedOverlay {
        let draw: SharedOverlay = Arc::new(Mutex::new(overlay));
        self.insert_shared(order, name, draw.clone());
        draw
    }

    /// Installs an already shared pass as layer `name` at `order` (replacing a layer of the same
    /// name); the owner keeps its own handle to the same state (the 2D toolkit's sprite pass).
    pub fn insert_shared(&mut self, order: i32, name: &'static str, draw: SharedOverlay) {
        self.layers.retain(|layer| layer.name != name);
        let at = self.layers.partition_point(|layer| layer.order <= order);
        self.layers.insert(at, OverlayLayer { order, name, draw });
    }

    /// Removes layer `name`; true when it was installed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.layers.len();
        self.layers.retain(|layer| layer.name != name);
        self.layers.len() != before
    }

    /// True when any overlay pass is installed.
    pub fn is_set(&self) -> bool {
        !self.layers.is_empty()
    }

    /// The installed layers' shared handles in draw order (warm jobs hold them on a worker).
    pub fn shared_layers(&self) -> Vec<SharedOverlay> {
        self.layers.iter().map(|layer| layer.draw.clone()).collect()
    }

    /// The installed layer names in draw order.
    pub fn layer_names(&self) -> Vec<&'static str> {
        self.layers.iter().map(|layer| layer.name).collect()
    }

    /// The installed layers' names and shared handles in draw order, without collecting them.
    pub(crate) fn layers(&self) -> impl Iterator<Item = (&'static str, &SharedOverlay)> {
        self.layers.iter().map(|layer| (layer.name, &layer.draw))
    }
}

/// Records the installed overlay (if any) into `encoder`. Callers must submit the encoder after the
/// clear/scene pass in the **same** submission so the surface texture stays writable across passes
/// (a separate submit after clear was observed to leave screenshots showing only the clear colour
/// on some Vulkan drivers). `target` is the texture behind `view` when it is known: passes that read
/// the frame so far (the UI backdrop blur) copy from it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_overlay_on(
    overlay: &WindowOverlay,
    device: &RenderDevice,
    encoder: &mut wgpu::CommandEncoder,
    target: Option<&wgpu::Texture>,
    view: &wgpu::TextureView,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) {
    record_overlay_filtered(
        overlay,
        device,
        encoder,
        view,
        target,
        format,
        width,
        height,
        |_| true,
    );
}

/// [`record_overlay_on`] for the layers whose name `keep` accepts (render-out draws only the UI).
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_overlay_filtered(
    overlay: &WindowOverlay,
    device: &RenderDevice,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    target: Option<&wgpu::Texture>,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    keep: impl Fn(&str) -> bool,
) {
    for layer in overlay.layers.iter().filter(|layer| keep(layer.name)) {
        // Spec 54: the layer's name (`ui` for the runtime UI) is its breadcrumb pass name.
        let _crumb = crate::breadcrumbs::frame_pass(layer.name);
        let Ok(mut draw) = layer.draw.lock() else {
            tracing::warn!(target: "regia_render", "overlay {} lock poisoned; skipping it", layer.name);
            continue;
        };
        match target {
            Some(texture) => draw.draw_with_target(device, encoder, view, texture, format, width, height),
            None => draw.draw(device, encoder, view, format, width, height),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Noop;
    impl OverlayDraw for Noop {
        fn draw(
            &mut self,
            _: &RenderDevice,
            _: &mut wgpu::CommandEncoder,
            _: &wgpu::TextureView,
            _: wgpu::TextureFormat,
            _: u32,
            _: u32,
        ) {
        }
    }

    #[test]
    fn layers_draw_in_order_and_replace_by_name() {
        let mut overlay = WindowOverlay::default();
        assert!(!overlay.is_set());
        overlay.set(Box::new(Noop));
        overlay.insert(-10, "quick", Box::new(Noop));
        overlay.insert(10, "stats", Box::new(Noop));
        overlay.set(Box::new(Noop));
        assert_eq!(overlay.layer_names(), ["quick", "ui", "stats"]);
        assert!(overlay.remove("stats"));
        assert!(!overlay.remove("stats"));
        assert_eq!(overlay.layer_names(), ["quick", "ui"]);
    }
}
