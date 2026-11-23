// SPDX-License-Identifier: MIT-0
//! {{name}} gameplay.

use regia::prelude::*;

/// The blank template's game plugin.
///
/// It adds no systems. Play-in-editor still simulates the open level with the engine's
/// built-in systems. Scripts registered at `REGIA:SCRIPTS` are project modules.
pub struct Game;

impl Plugin for Game {
    fn build(&self, _app: &mut App) {}
}

// REGIA:SCRIPTS
