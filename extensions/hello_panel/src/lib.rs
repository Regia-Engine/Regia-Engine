// SPDX-License-Identifier: MPL-2.0
//! The engine's example editor extension (ADR-9850 (5)): see `hello.rs`.

mod hello;

pub use hello::{HelloCount, HelloPanel};

regia_framework::extension!(HelloPanel);
