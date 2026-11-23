// SPDX-License-Identifier: MPL-2.0
//! DOM listeners that remove themselves.

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{AddEventListenerOptions, Event, EventTarget};

use super::js::describe;

/// A listener on a DOM target; dropping it removes it.
pub struct Listener {
    target: EventTarget,
    name: &'static str,
    closure: Closure<dyn FnMut(Event)>,
}

impl Listener {
    /// Adds `handler` for `name` on `target`. `passive`: the handler never calls `preventDefault`, which
    /// keeps scrolling smooth where it applies.
    pub fn new(
        target: &EventTarget,
        name: &'static str,
        passive: bool,
        handler: impl FnMut(Event) + 'static,
    ) -> Self {
        let closure = Closure::<dyn FnMut(Event)>::new(handler);
        let options = AddEventListenerOptions::new();
        options.set_passive(passive);
        if let Err(e) = target.add_event_listener_with_callback_and_add_event_listener_options(
            name,
            closure.as_ref().unchecked_ref(),
            &options,
        ) {
            web_sys::console::error_1(&format!("addEventListener {name}: {}", describe(&e)).into());
        }
        Listener {
            target: target.clone(),
            name,
            closure,
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self
            .target
            .remove_event_listener_with_callback(self.name, self.closure.as_ref().unchecked_ref());
    }
}

/// The RGBA bytes of what `canvas` shows now, top row first: drawn onto a 2D canvas of its size and read there, which
/// works for a WebGL2 and a WebGPU canvas alike. For scripted capture.
pub(super) fn canvas_rgba(canvas: &web_sys::HtmlCanvasElement) -> Result<Vec<u8>, String> {
    let fail = |what: &str, e: wasm_bindgen::JsValue| format!("{what}: {e:?}");
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let copy = document
        .create_element("canvas")
        .map_err(|e| fail("create a canvas", e))?
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .map_err(|_| "the new element is not a canvas".to_owned())?;
    copy.set_width(canvas.width());
    copy.set_height(canvas.height());
    let context = copy
        .get_context("2d")
        .map_err(|e| fail("a 2d context", e))?
        .ok_or("no 2d context")?
        .dyn_into::<web_sys::CanvasRenderingContext2d>()
        .map_err(|_| "the context is not 2d".to_owned())?;
    context
        .draw_image_with_html_canvas_element(canvas, 0.0, 0.0)
        .map_err(|e| fail("draw the canvas", e))?;
    let image = context
        .get_image_data(0.0, 0.0, f64::from(canvas.width()), f64::from(canvas.height()))
        .map_err(|e| fail("read the canvas", e))?;
    Ok(image.data().0)
}
