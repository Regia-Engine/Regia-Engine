//! Game code in a library crate. `patch_probe.sh <log> <rounds> lib` edits this file instead of
//! the binary crate.

use std::time::{SystemTime, UNIX_EPOCH};

use regia::prelude::*;

/// State that must survive a patch.
#[derive(Resource, Default)]
pub struct LibGame {
    /// Frames.
    pub frame: u64,
    /// Accumulator.
    pub value: i64,
}

/// System whose body the probe edits.
pub fn lib_system(mut g: ResMut<LibGame>) {
    const MARKER: &str = "v1";
    g.frame += 1;
    g.value += 1;
    if g.frame % 6 == 0 {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        println!(
            "PATCHPROBE {ms} lib frame={} value={} marker={}",
            g.frame, g.value, MARKER
        );
    }
}
