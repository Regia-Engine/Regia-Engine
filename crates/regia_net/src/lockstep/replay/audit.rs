// SPDX-License-Identifier: MPL-2.0
//! The bit-exact audit of a `.lsreplay` (ADR-8513): `regia net replay-audit`.
//!
//! The audit steps the game's [`DesyncReplay`] hook through every recorded frame, in file order.
//! It starts from the first Full (or from the match start when the first record is the frame of
//! tick 0), compares the world and every subsystem hash at each Hashes record bit for bit, and at
//! each later Full compares the replayed state's world hash with the recorded one and then loads
//! the recorded state, so a divergence in one stretch does not hide the next. The first tick that
//! differs is named with the subsystems that differ there (in the recorded order) and the newest
//! checkpoint before it that agreed.

use serde::{Deserialize, Serialize};

use super::{
    FullRecord, HashesRecord, LsRecord, LsReplayError, LsReplayHeader, LsReplayReader, ReplayAbortInfo,
};
use crate::desync::{BundleSnapshot, DesyncReplay, ReplayRegistry};

/// The verdict of [`audit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayAudit {
    /// The file's header.
    pub header: LsReplayHeader,
    /// The first and last tick stepped (`None`: no frame was stepped).
    pub ticks: Option<(u64, u64)>,
    /// Frames stepped.
    pub frames: u64,
    /// Fulls in the file.
    pub fulls: u64,
    /// Fulls whose recorded world hash was compared with the replayed state.
    pub fulls_compared: u64,
    /// Hashes records compared.
    pub hashes_compared: u64,
    /// Records the audit could not use (frames before the first Full, hashes of a tick it did
    /// not step to).
    pub skipped: u64,
    /// Comparisons that differed (every one, not only the first).
    pub mismatches: u64,
    /// The first tick whose recorded hashes differ from the replay.
    pub first_diverging_tick: Option<u64>,
    /// The newest compared tick before it at which everything agreed.
    pub last_agreed_tick: Option<u64>,
    /// The first subsystem that differs there, in the recorded order (`None`: only the world hash
    /// differs, or a Full found it).
    pub subsystem: Option<String>,
    /// Every subsystem that differs there.
    pub subsystems: Vec<String>,
    /// What found it: `hashes` or `full`.
    pub found_by: Option<String>,
    /// Ticks of the Fulls the audit loaded after the replay had left the recorded states.
    pub resumed_at: Vec<u64>,
    /// The abort record, when the match aborted.
    pub abort: Option<ReplayAbortInfo>,
}

impl ReplayAudit {
    /// Every recorded hash matched the replay bit for bit.
    pub fn bit_exact(&self) -> bool {
        self.mismatches == 0
    }

    /// The verdict as lines for a person (what `regia net replay-audit` prints).
    pub fn summary(&self) -> Vec<String> {
        let h = &self.header;
        let mut out = vec![format!(
            "game {} (protocol version {}, {} seats, {} Hz, seed {:#x})",
            h.game_id,
            h.protocol_version,
            h.seats.len(),
            h.tick_rate,
            h.seed
        )];
        let ticks = self
            .ticks
            .map_or_else(|| "no frame".to_owned(), |(a, b)| format!("ticks {a}..={b}"));
        out.push(format!(
            "replayed {ticks}: {} frames, {} hash checkpoints and {} of {} fulls compared, {} records skipped",
            self.frames, self.hashes_compared, self.fulls_compared, self.fulls, self.skipped
        ));
        match self.first_diverging_tick {
            Some(tick) => {
                out.push(format!(
                    "first diverging tick {tick}: subsystem {}{}{}",
                    self.subsystem.as_deref().unwrap_or("(world hash only)"),
                    self.last_agreed_tick
                        .map_or_else(String::new, |t| format!(", last agreed tick {t}")),
                    self.found_by
                        .as_deref()
                        .map_or_else(String::new, |f| format!(" (found by the {f})"))
                ));
                if self.subsystems.len() > 1 {
                    out.push(format!(
                        "  every subsystem differing there: {}",
                        self.subsystems.join(", ")
                    ));
                }
                out.push(format!(
                    "NOT bit-exact: {} comparisons differ; resumed from the fulls at {:?}",
                    self.mismatches, self.resumed_at
                ));
            }
            None if self.hashes_compared + self.fulls_compared == 0 => {
                out.push(
                    "bit-exact, but nothing was compared (the file holds no hashes or fulls)".to_owned(),
                );
            }
            None => out.push("bit-exact: every recorded hash matches the replay".to_owned()),
        }
        if let Some(abort) = &self.abort {
            out.push(format!(
                "the match aborted at tick {}: {} ({})",
                abort.tick, abort.code, abort.reason
            ));
        }
        out
    }

    fn diverged(&mut self, tick: u64, subsystems: Vec<String>, by: &str) {
        self.mismatches += 1;
        if self.first_diverging_tick.is_none() {
            self.first_diverging_tick = Some(tick);
            self.subsystem = subsystems.first().cloned();
            self.subsystems = subsystems;
            self.found_by = Some(by.to_owned());
        }
    }

    fn agreed(&mut self, tick: u64) {
        if self.first_diverging_tick.is_none() {
            self.last_agreed_tick = Some(tick);
        }
    }
}

/// The game and version a file names, for a message.
fn named(game_id: &str, version: u32, id: u64) -> String {
    format!("{game_id} protocol version {version} (protocol id {id:#018x})")
}

/// Audits `bytes` through the hook registered for its game in `registry`.
pub fn audit_with(bytes: &[u8], registry: &ReplayRegistry) -> Result<ReplayAudit, LsReplayError> {
    let reader = LsReplayReader::new(bytes)?;
    let game = &reader.header().game_id;
    let mut hook = registry.hook(game).ok_or_else(|| LsReplayError::NoHook {
        game: game.clone(),
        registered: if registry.games().is_empty() {
            "none".to_owned()
        } else {
            registry.games().join(", ")
        },
    })?;
    audit(bytes, hook.as_mut())
}

/// Audits `bytes` through `hook` (module docs).
pub fn audit(bytes: &[u8], hook: &mut dyn DesyncReplay) -> Result<ReplayAudit, LsReplayError> {
    let _span = tracing::info_span!("regia_net::lsreplay_audit", bytes = bytes.len()).entered();
    let mut reader = LsReplayReader::new(bytes)?;
    let header = reader.header().clone();
    let id = hook.identity();
    if id.game_id != header.game_id
        || id.protocol_version != header.protocol_version
        || id.protocol_id != header.protocol_id
    {
        return Err(LsReplayError::GameMismatch {
            file: named(&header.game_id, header.protocol_version, header.protocol_id),
            hook: named(&id.game_id, id.protocol_version, id.protocol_id),
        });
    }
    let mut out = ReplayAudit {
        header,
        ticks: None,
        frames: 0,
        fulls: 0,
        fulls_compared: 0,
        hashes_compared: 0,
        skipped: 0,
        mismatches: 0,
        first_diverging_tick: None,
        last_agreed_tick: None,
        subsystem: None,
        subsystems: Vec::new(),
        found_by: None,
        resumed_at: Vec::new(),
        abort: None,
    };
    // `Some(t)`: the hook holds the state after tick `t` (`t + 1` is the next frame), or before
    // the first frame when `start` (then `t` is that frame's tick).
    let mut at: Option<(u64, bool)> = None;
    let mut rows = Vec::new();
    let mut first_record = true;
    while let Some(record) = reader.next_record()? {
        let first = std::mem::take(&mut first_record);
        match record {
            LsRecord::Full(full) => {
                out.fulls += 1;
                compare_full(hook, &full, at, &mut out, &mut rows)?;
                let snapshot = if full.start {
                    BundleSnapshot {
                        tick: None,
                        data: crate::desync::HexBytes(full.state),
                    }
                } else {
                    BundleSnapshot::at(full.tick, full.state)
                };
                hook.load(&snapshot).map_err(|reason| LsReplayError::HookFailed {
                    tick: full.tick,
                    reason,
                })?;
                let loaded = hook.world_hash(full.tick);
                if loaded != full.world {
                    return Err(LsReplayError::HookFailed {
                        tick: full.tick,
                        reason: format!(
                            "the loaded full hashes to {loaded:#018x}; the file recorded {:#018x} (a hook whose load and hash disagree, or a damaged full)",
                            full.world
                        ),
                    });
                }
                at = Some((full.tick, full.start));
            }
            LsRecord::Inputs(inputs) => {
                let tick = inputs.frame.tick;
                let expected = match at {
                    None if first && tick == 0 => {
                        hook.load(&BundleSnapshot::start())
                            .map_err(|reason| LsReplayError::HookFailed { tick, reason })?;
                        tick
                    }
                    None => {
                        out.skipped += 1;
                        continue;
                    }
                    Some((t, true)) => t,
                    Some((t, false)) => t + 1,
                };
                if tick != expected {
                    return Err(LsReplayError::Invalid(format!(
                        "the frames jump from tick {expected} to tick {tick}: a frame is missing"
                    )));
                }
                hook.step(&inputs.frame)
                    .map_err(|reason| LsReplayError::HookFailed { tick, reason })?;
                out.frames += 1;
                out.ticks = Some((out.ticks.map_or(tick, |(a, _)| a), tick));
                at = Some((tick, false));
            }
            LsRecord::Hashes(hashes) => {
                if at != Some((hashes.tick, false)) {
                    out.skipped += 1;
                    continue;
                }
                compare_hashes(hook, &hashes, &mut out, &mut rows);
            }
            LsRecord::PeerCmds(_) => {}
            LsRecord::Abort(abort) => out.abort = Some(abort),
        }
    }
    Ok(out)
}

fn compare_hashes(
    hook: &dyn DesyncReplay,
    hashes: &HashesRecord,
    out: &mut ReplayAudit,
    rows: &mut Vec<(&'static str, u64)>,
) {
    out.hashes_compared += 1;
    rows.clear();
    hook.subsystem_hashes(hashes.tick, rows);
    let differ: Vec<String> = hashes
        .subsystems
        .iter()
        .filter(|(name, recorded)| {
            rows.iter()
                .find(|(n, _)| crate::desync::clip_name(n) == name.as_str())
                .is_none_or(|(_, ours)| ours != recorded)
        })
        .map(|(name, _)| name.clone())
        .collect();
    let world = hook.world_hash(hashes.tick);
    if world != hashes.world || !differ.is_empty() {
        out.diverged(hashes.tick, differ, "hashes");
    } else {
        out.agreed(hashes.tick);
    }
}

/// Compares the replayed state with a Full the replay has stepped to: the world hash, and when it
/// differs, the subsystems (the replayed state's against the recorded state's, loaded after).
fn compare_full(
    hook: &mut dyn DesyncReplay,
    full: &FullRecord,
    at: Option<(u64, bool)>,
    out: &mut ReplayAudit,
    rows: &mut Vec<(&'static str, u64)>,
) -> Result<(), LsReplayError> {
    if full.start || at != Some((full.tick, false)) {
        return Ok(());
    }
    out.fulls_compared += 1;
    let ours = hook.world_hash(full.tick);
    if ours == full.world {
        out.agreed(full.tick);
        return Ok(());
    }
    rows.clear();
    hook.subsystem_hashes(full.tick, rows);
    let replayed: Vec<(&'static str, u64)> = rows.clone();
    hook.load(&BundleSnapshot::at(full.tick, full.state.clone()))
        .map_err(|reason| LsReplayError::HookFailed {
            tick: full.tick,
            reason,
        })?;
    rows.clear();
    hook.subsystem_hashes(full.tick, rows);
    let differ = rows
        .iter()
        .filter(|(name, recorded)| {
            replayed
                .iter()
                .find(|(n, _)| n == name)
                .is_none_or(|(_, h)| h != recorded)
        })
        .map(|(name, _)| crate::desync::clip_name(name).to_owned())
        .collect();
    out.diverged(full.tick, differ, "full");
    out.resumed_at.push(full.tick);
    Ok(())
}
