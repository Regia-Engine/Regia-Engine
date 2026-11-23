// SPDX-License-Identifier: MPL-2.0
//! Tier markers (`tiles.*`, spec 66 § 3): pass-through nodes the tile planner reads.

use crate::data::Bundle;
use crate::exec::NodeCtx;
use crate::nodes::{BuiltinNode, NodeError, SettingType as T, Settings, pins_any_inout, set};
use crate::tiles::TierSize;

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "tiles.tier",
        "tiles",
        "Everything downstream runs at this tier: `Base`, `Whole` or `Meters(m)` (base × 2^k)",
        &[set(
            "cell_m",
            T::Any,
            "Base",
            "`Base`, `Whole` or `Meters(m)`; m = base cell × 2^k, 8 to 4096 m",
        )],
        pins_any_inout,
        eval_pass,
    )
    .checked(check_tier),
    BuiltinNode::new(
        "tiles.no_clip",
        "tiles",
        "Keeps coarse-tier data whole for its fine-tier consumers (no clip to the cell plus halo)",
        &[],
        pins_any_inout,
        eval_pass,
    ),
];

/// Data passes through unchanged.
fn eval_pass(_: &mut NodeCtx<'_>, inputs: &[Bundle], _: &Settings) -> Result<Vec<Bundle>, NodeError> {
    Ok(vec![crate::nodes::input(inputs, 0)])
}

fn check_tier(s: &Settings) -> Vec<(String, String)> {
    match s.get("cell_m").map(TierSize::from_setting) {
        Some(Some(_)) => Vec::new(),
        _ => vec![(
            "setting_type".to_string(),
            "`cell_m` must be `Base`, `Whole` or `Meters(m)` with m > 0".to_string(),
        )],
    }
}
