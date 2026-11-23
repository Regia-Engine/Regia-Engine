// SPDX-License-Identifier: MPL-2.0
//! The `regia` command as a library (`docs/specs/21-tooling-and-packaging.md` § 1).
//!
//! The binary in `src/main.rs` parses arguments and dispatches. Only [`new_project`] lives
//! here, with [`distribution`] (spec 94), [`session`] (the multi-process session harness),
//! [`dcc_feed`] (the reference client, which its conformance tests read), [`key_track`]
//! (the key-track format), [`play_install`] (a fresh-home install and a physical-key driver)
//! and [`contact_sheet`] (labeled JPEG sheets, ADR-8036); the other commands stay in the binary.
//! The editor keeps its own copy of this scaffolding (`regia_editor::scaffold`): both crates are
//! the Tools layer, so neither may depend on the other.

pub mod contact_sheet;
pub mod dcc_feed;
pub mod distribution;
pub mod key_track;
pub mod lobby_hub_args;
pub mod new_project;
pub mod play_install;
pub mod session;

// Shipping package audit (ADR-8062).
pub mod package_hygiene;
