// SPDX-License-Identifier: MPL-2.0
//! `regia_web`: the product viewer's web player (`docs/specs/42-product-viewer.md` § 3, ADR-2536,
//! ADR-2541).
//!
//! On `wasm32-unknown-unknown` this crate is the browser build: a `RegiaViewer` class for page scripts
//! (`open`, `options`, `setPick`, `build`, `setView`, events `ready`, `changed`, `error`), a lazy
//! `requestAnimationFrame` loop, pointer input, resize and visibility handling, a canvas surface on
//! WebGPU or WebGL2, and HTTP fetch of packages.
//!
//! Everything that is not the browser itself lives in the platform-free modules ([`lazy`], [`player`],
//! [`options`], [`urls`], [`package`], [`product`], [`view`], [`scene`]), which build everywhere and are tested
//! natively. The player works through the traits in [`seams`]; [`product`] implements them over
//! `regia_configurator` and `regia_viewer_pack`, [`view`] over `regia_viewer_view`, and [`scene`] over
//! `regia_viewer`.
//!
//! Page: `book/agents/web-player.md`. Scripted capture ([`capture`], the player's deterministic mode, and a
//! headless rig the render-out drives): `book/agents/capture-hooks.md`.

#[cfg(target_arch = "wasm32")]
mod browser;
pub mod capture;
pub mod error;
pub mod lazy;
pub mod options;
pub mod package;
pub mod player;
pub mod product;
pub mod scene;
pub mod seams;
pub mod urls;
pub mod view;

pub use error::{ERROR_CODES, WebError};
pub use lazy::{LazyLoop, Schedule};
pub use options::{BackendChoice, PlayerOptions, drawing_size};
pub use player::{PickResult, Player, PlayerEvent};
