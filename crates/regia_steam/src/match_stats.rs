// SPDX-License-Identifier: MPL-2.0
//! Platform stats from finished-match records (`regia_framework::match_results`,
//! `book/agents/match-results.md`): [`PlatformStatsSink`] adds the local player's record values to
//! Steam stats. The framework has no platform dependency; this sink is where the two meet.

use regia_app::prelude::*;
use regia_framework::match_results::{MatchResult, MatchResultSink, ResultValue, clamp_i32};

use crate::Steam;

/// One stat the sink writes: `stat` gains the record's `value` for the local player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformStat {
    /// The Steam API stat name (configured on the partner site).
    pub stat: String,
    /// The record value added.
    pub value: ResultValue,
}

/// A [`MatchResultSink`] that adds record values of [`PlatformStatsSink::player`] to Steam stats
/// with [`Steam::stat_add`] (threshold achievements unlock as usual). A peer that did not play the
/// match (a dedicated host) writes nothing. A zero value is skipped, so an unused stat is never
/// touched.
#[derive(Debug, Clone, Default)]
pub struct PlatformStatsSink {
    /// The local player's key in the record (`steam:<id>` on Steam).
    pub player: String,
    /// The stats, in write order.
    pub stats: Vec<PlatformStat>,
}

impl MatchResultSink for PlatformStatsSink {
    fn name(&self) -> &str {
        "platform_stats"
    }

    fn send(&mut self, world: &mut World, result: &MatchResult) -> Result<(), String> {
        if result.player(&self.player).is_none() {
            return Ok(());
        }
        let steam = world
            .get_resource::<Steam>()
            .ok_or_else(|| "no Steam resource".to_owned())?;
        let mut first_error = None;
        for entry in &self.stats {
            let value = entry.value.read(result, &self.player).map_or(0, clamp_i32);
            if value == 0 {
                continue;
            }
            if let Err(e) = steam.stat_add(&entry.stat, value) {
                first_error.get_or_insert_with(|| format!("stat {}: {e}", entry.stat));
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}
