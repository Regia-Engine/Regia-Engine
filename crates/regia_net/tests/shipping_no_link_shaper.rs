// SPDX-License-Identifier: MPL-2.0
//! Spec 77 § 10.1 and acceptance 11 (ADR-7212): the network conditioner's `LinkShaper` is compiled
//! out of shipping builds (every build without `debug_assertions`: `release`, `shipping`).
//!
//! The check is the compile itself. This file glob-imports `regia_net::session` and its
//! `conditioner` module next to a local stand-in named `LinkShaper`; a use of the name in the type
//! namespace (where the engine's struct lives) is ambiguous (E0659) when the crate exports one of its
//! own, so the test crate only builds when the shaper is absent. Run it in a build without debug assertions:
//! `cargo test -p regia_net --release --test shipping_no_link_shaper`. Debug builds keep the shaper
//! (the loopback hub and the agent's `net.conditioner` use it) and compile this file to nothing.
#![cfg(not(debug_assertions))]

mod stand_in {
    /// Takes the name the engine must not export in this build.
    pub struct LinkShaper;
}

#[allow(unused_imports)]
use regia_net::session::conditioner::*;
#[allow(unused_imports)]
use regia_net::session::*;
use stand_in::*;

#[test]
fn link_shaper_is_compiled_out_of_builds_without_debug_assertions() {
    // Resolves to the stand-in only: the engine exports no `LinkShaper` here. The type annotation
    // matters: the engine's struct has fields, so it lives in the type namespace only, and a bare
    // value use would never meet it.
    let _stand_in: LinkShaper = LinkShaper;
    assert!(!regia_net::session::conditioner::conditioner_available());
    assert!(!regia_net::session::conditioner::LINK_SHAPER_COMPILED);
}
