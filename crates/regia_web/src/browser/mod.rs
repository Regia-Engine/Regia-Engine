// SPDX-License-Identifier: MPL-2.0
//! The browser side: everything here needs `wasm32-unknown-unknown` and a page.

mod api;
mod dom;
mod fetch;
mod gpu;
mod js;

use wasm_bindgen::prelude::wasm_bindgen;

/// Runs when the module loads: a panic reaches the console with its message instead of a bare trap.
#[wasm_bindgen(start)]
pub fn start() {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&info.to_string().into());
    }));
}
