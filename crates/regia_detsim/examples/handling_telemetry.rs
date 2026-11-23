// SPDX-License-Identifier: MPL-2.0
//! Sample feel-target dump for the engine default hull (ADR-8393).
//!
//! Prints one `regia-handling-rig` line. The column names are the dump contract in
//! `book/agents/det-hover-rig.md`.

fn main() {
    let params = regia_detsim::hover::HoverParams::DEFAULT_HULL;
    let report = regia_detsim::handling::measure(&params, regia_detsim::site_physics::EARTH_G);
    let dumped = regia_detsim::handling::format_report(&report);
    println!("regia-handling-rig {dumped}");
}
