// SPDX-License-Identifier: MPL-2.0
//! Sound, VFX and decals for destruction (ADR-2734): [`DestructionFxPlugin`] reads every
//! `regia_physics` `DestructionFxMessage` and plays what a `.destructionfx.ron` table
//! ([`DestructionFxTable`]) names for its `fx_tag`.
//!
//! Each step the planner ([`DestructionFxPlayer`]) turns the step's messages into plays, in
//! message order: a row's `kinds` filter, its `min_magnitude`, its per-tag cap and the table's
//! per-step cap apply; the rest are counted as dropped. A play's variation (decal size and spin,
//! the cue's pick and pitch) comes from the break event's seed, so a server and its clients pick
//! the same variation. Plans go into buffers the player keeps between steps (pooled playback): after
//! the first busy step the planner allocates nothing, which [`DestructionFxStats::buffer_growths`]
//! counts.
//!
//! Planning and the counters run in every build (a headless server reports what it would play).
//! Playing them needs the `destruction-fx` feature (on through `regia/destruction-fx`, which also
//! turns on `client`): sound cues through `regia_audio::Audio::play_cue_seeded`, VFX systems
//! through `regia_vfx::VfxScene::spawn`, decals through `regia_vfx::DecalPool::spawn`. A missing
//! resource skips that half. Guide: `book/agents/destruction-fx.md`; format:
//! `docs/formats/destructionfx-ron.md`.

use std::collections::HashMap;

use regia_app::prelude::*;
use regia_physics::destruction::{DestructionFx, DestructionFxMessage, FxKind};
use serde::{Deserialize, Serialize};

/// Version of the `.destructionfx.ron` shape ([`DestructionFxTable`]).
pub const DESTRUCTION_FX_FORMAT_VERSION: u32 = 1;

/// Whether this build plays the plans (the `destruction-fx` feature); without it the plugin plans
/// and counts only.
pub const PLAYBACK: bool = cfg!(feature = "destruction-fx");

/// Most rows one table may hold.
pub const MAX_FX_ROWS: usize = 1024;

/// One `.destructionfx.ron`: what each `fx_tag` plays.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DestructionFxTable {
    /// Must be [`DESTRUCTION_FX_FORMAT_VERSION`].
    pub format_version: u32,
    /// Display name.
    pub name: String,
    /// Most plays per step over every tag (0: no cap).
    #[serde(default = "default_step_cap")]
    pub max_per_step: u32,
    /// The row a tag with no row of its own uses (`None`: such tags play nothing).
    #[serde(default)]
    pub fallback: Option<String>,
    /// One row per tag, no tag twice.
    pub rows: Vec<DestructionFxRow>,
}

fn default_step_cap() -> u32 {
    32
}

/// What one `fx_tag` plays.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DestructionFxRow {
    /// The `fx_tag` (`"concrete"`, `"glass"`).
    pub tag: String,
    /// Moments that play (empty: all three).
    #[serde(default)]
    pub kinds: Vec<FxKind>,
    /// A registered sound cue.
    #[serde(default)]
    pub sound: Option<String>,
    /// A VFX system in the `VfxScene` library.
    #[serde(default)]
    pub vfx: Option<String>,
    /// A decal left where it happened.
    #[serde(default)]
    pub decal: Option<FxDecal>,
    /// Most plays of this tag per step (0: no cap of its own).
    #[serde(default = "default_tag_cap")]
    pub max_per_step: u32,
    /// Moments weaker than this (the message's magnitude: break impulse or chunk mass) play nothing.
    #[serde(default)]
    pub min_magnitude: f32,
    /// Seeded size variation, 0 to 1: a play's scale is `1 ± scale_jitter`.
    #[serde(default)]
    pub scale_jitter: f32,
}

fn default_tag_cap() -> u32 {
    8
}

/// A decal of a [`DestructionFxRow`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FxDecal {
    /// Half extents `(u, v, depth)`, metres (each > 0).
    pub half_size: [f32; 3],
    /// Linear tint.
    pub color: [f32; 4],
    /// Seconds it stays (> 0).
    pub lifetime_secs: f32,
}

/// Why a table failed to load. [`DestructionFxError::code`] is the agent-protocol error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum DestructionFxError {
    /// RON syntax or shape error.
    #[error("destruction fx parse: {0}")]
    Parse(String),
    /// A `format_version` this engine cannot read.
    #[error("destruction fx format_version {found} is not supported (this engine reads {expected})")]
    Version {
        /// Version found.
        found: u32,
        /// Version supported.
        expected: u32,
    },
    /// A rule of the format failed.
    #[error("destruction fx table `{name}`: {reason}")]
    Invalid {
        /// Table name.
        name: String,
        /// Which rule.
        reason: String,
    },
}

impl DestructionFxError {
    /// The agent-protocol error code: lowercase ASCII, digits and `_`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "destruction_fx_parse",
            Self::Version { .. } => "destruction_fx_version",
            Self::Invalid { .. } => "destruction_fx_invalid",
        }
    }
}

impl DestructionFxTable {
    /// Parses, migrates (none exist at v1) and validates a `.destructionfx.ron`.
    ///
    /// # Errors
    /// [`DestructionFxError`] naming the first problem.
    pub fn from_ron(text: &str) -> Result<Self, DestructionFxError> {
        let text = migrate_destruction_fx(text)?;
        let table: Self = ron::from_str(&text).map_err(|e| DestructionFxError::Parse(e.to_string()))?;
        table.validate()?;
        Ok(table)
    }

    /// Pretty RON.
    ///
    /// # Errors
    /// [`DestructionFxError::Parse`] when serialization fails.
    pub fn to_ron(&self) -> Result<String, DestructionFxError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| DestructionFxError::Parse(e.to_string()))
    }

    /// Checks every rule of the format.
    ///
    /// # Errors
    /// [`DestructionFxError::Version`] or [`DestructionFxError::Invalid`].
    pub fn validate(&self) -> Result<(), DestructionFxError> {
        let fail = |reason: String| DestructionFxError::Invalid {
            name: self.name.clone(),
            reason,
        };
        if self.format_version != DESTRUCTION_FX_FORMAT_VERSION {
            return Err(DestructionFxError::Version {
                found: self.format_version,
                expected: DESTRUCTION_FX_FORMAT_VERSION,
            });
        }
        if self.rows.len() > MAX_FX_ROWS {
            return Err(fail(format!("{} rows (at most {MAX_FX_ROWS})", self.rows.len())));
        }
        let mut tags = std::collections::HashSet::with_capacity(self.rows.len());
        for (i, row) in self.rows.iter().enumerate() {
            if row.tag.is_empty() {
                return Err(fail(format!("row {i} has an empty tag")));
            }
            if !tags.insert(row.tag.as_str()) {
                return Err(fail(format!("row {i} repeats the tag `{}`", row.tag)));
            }
            if !(row.min_magnitude.is_finite() && row.min_magnitude >= 0.0) {
                return Err(fail(format!("row {i} min_magnitude must be finite and >= 0")));
            }
            if !(row.scale_jitter.is_finite() && (0.0..=1.0).contains(&row.scale_jitter)) {
                return Err(fail(format!("row {i} scale_jitter must be 0..=1")));
            }
            if let Some(d) = &row.decal
                && !(d.half_size.iter().all(|h| h.is_finite() && *h > 0.0)
                    && d.color.iter().all(|c| c.is_finite())
                    && d.lifetime_secs.is_finite()
                    && d.lifetime_secs > 0.0)
            {
                return Err(fail(format!(
                    "row {i} decal needs finite half sizes and lifetime > 0"
                )));
            }
        }
        if let Some(f) = &self.fallback
            && !tags.contains(f.as_str())
        {
            return Err(fail(format!("fallback `{f}` names no row")));
        }
        Ok(())
    }
}

/// Brings an older `.destructionfx.ron` to the current version (none exist at v1).
///
/// # Errors
/// [`DestructionFxError::Parse`] when the version cannot be read, [`DestructionFxError::Version`]
/// for any other version.
pub fn migrate_destruction_fx(text: &str) -> Result<String, DestructionFxError> {
    let version = regia_core::version::peek_ron_version(text).map_err(DestructionFxError::Parse)?;
    if version == DESTRUCTION_FX_FORMAT_VERSION {
        return Ok(text.to_string());
    }
    Err(DestructionFxError::Version {
        found: version,
        expected: DESTRUCTION_FX_FORMAT_VERSION,
    })
}

/// Versions [`migrate_destruction_fx`] accepts below the current one (empty at v1).
#[must_use]
pub fn destruction_fx_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Serde field-list fingerprint of `.destructionfx.ron` (for `cargo xtask check-formats`).
#[must_use]
pub fn destruction_fx_schema_fingerprint() -> String {
    use regia_core::schema::serde_field_names;
    regia_core::schema::join_field_lists(&[
        serde_field_names::<DestructionFxTable>(),
        serde_field_names::<DestructionFxRow>(),
        serde_field_names::<FxDecal>(),
    ])
}

/// One planned play.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FxPlay {
    /// Index of the row in the table.
    pub row: u32,
    /// The moment.
    pub kind: FxKind,
    /// World position, metres.
    pub position: [f32; 3],
    /// The break event's seed.
    pub seed: u64,
    /// Seeded scale (`1 ± scale_jitter`).
    pub scale: f32,
    /// Seeded spin about the decal's axis, radians in `[0, 2π)`.
    pub spin: f32,
}

/// What the planner and the playback did since the plugin was added.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestructionFxStats {
    /// Messages read.
    pub messages: u64,
    /// Plays planned.
    pub planned: u64,
    /// Messages whose tag had no row (and no fallback).
    pub unmatched: u64,
    /// Messages a row's `kinds` or `min_magnitude` skipped.
    pub filtered: u64,
    /// Plays the per-tag cap dropped.
    pub dropped_tag_cap: u64,
    /// Plays the per-step cap dropped.
    pub dropped_step_cap: u64,
    /// Sound cues started.
    pub sounds: u64,
    /// Sound cues that did not start (unknown cue, cooldown, voice limit).
    pub sounds_refused: u64,
    /// VFX systems spawned.
    pub vfx: u64,
    /// VFX systems the scene did not know.
    pub vfx_unknown: u64,
    /// Decals placed.
    pub decals: u64,
    /// Times a pooled buffer had to grow (0 after warm-up).
    pub buffer_growths: u64,
    /// Steps with at least one message.
    pub busy_steps: u64,
}

/// The planner: table, tag index and pooled buffers.
#[derive(Debug, Default)]
pub struct DestructionFxPlayer {
    table: Option<DestructionFxTable>,
    index: HashMap<String, u32>,
    fallback: Option<u32>,
    per_tag: Vec<u32>,
    plays: Vec<FxPlay>,
    stats: DestructionFxStats,
}

impl DestructionFxPlayer {
    /// A player for `table` (validated).
    ///
    /// # Errors
    /// As [`DestructionFxTable::validate`].
    pub fn new(table: DestructionFxTable) -> Result<Self, DestructionFxError> {
        let mut player = Self::default();
        player.set_table(table)?;
        Ok(player)
    }

    /// Replaces the table (a reload); the counters stay.
    ///
    /// # Errors
    /// As [`DestructionFxTable::validate`]; the old table stays on an error.
    pub fn set_table(&mut self, table: DestructionFxTable) -> Result<(), DestructionFxError> {
        table.validate()?;
        self.index = table
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.tag.clone(), i as u32))
            .collect();
        self.fallback = table.fallback.as_ref().and_then(|f| self.index.get(f).copied());
        self.per_tag = vec![0; table.rows.len()];
        self.table = Some(table);
        Ok(())
    }

    /// The table, if one is set.
    #[must_use]
    pub fn table(&self) -> Option<&DestructionFxTable> {
        self.table.as_ref()
    }

    /// Counters since creation.
    #[must_use]
    pub fn stats(&self) -> DestructionFxStats {
        self.stats
    }

    /// The plays of the last [`Self::plan`].
    #[must_use]
    pub fn plays(&self) -> &[FxPlay] {
        &self.plays
    }

    /// Plans one step's moments, in order, into the pooled buffer, and returns the plays.
    pub fn plan<'a>(&mut self, moments: impl IntoIterator<Item = &'a DestructionFx>) -> &[FxPlay] {
        self.plays.clear();
        self.per_tag.fill(0);
        let Some(table) = self.table.as_ref() else {
            for _ in moments {
                self.stats.messages += 1;
                self.stats.unmatched += 1;
            }
            return &self.plays;
        };
        let step_cap = match table.max_per_step {
            0 => usize::MAX,
            n => n as usize,
        };
        let mut any = false;
        for fx in moments {
            any = true;
            self.stats.messages += 1;
            let Some(row_index) = self.index.get(fx.fx_tag.as_str()).copied().or(self.fallback) else {
                self.stats.unmatched += 1;
                continue;
            };
            let row = &table.rows[row_index as usize];
            if (!row.kinds.is_empty() && !row.kinds.contains(&fx.kind)) || fx.magnitude < row.min_magnitude {
                self.stats.filtered += 1;
                continue;
            }
            let count = &mut self.per_tag[row_index as usize];
            if row.max_per_step > 0 && *count >= row.max_per_step {
                self.stats.dropped_tag_cap += 1;
                continue;
            }
            if self.plays.len() >= step_cap {
                self.stats.dropped_step_cap += 1;
                continue;
            }
            *count += 1;
            let (u, v) = seeded_pair(fx.seed);
            if self.plays.len() == self.plays.capacity() {
                self.stats.buffer_growths += 1;
            }
            self.plays.push(FxPlay {
                row: row_index,
                kind: fx.kind,
                position: fx.position,
                seed: fx.seed,
                scale: 1.0 + row.scale_jitter * (2.0 * u - 1.0),
                spin: v * std::f32::consts::TAU,
            });
            self.stats.planned += 1;
        }
        if any {
            self.stats.busy_steps += 1;
        }
        &self.plays
    }

    #[cfg(feature = "destruction-fx")]
    fn stats_mut(&mut self) -> &mut DestructionFxStats {
        &mut self.stats
    }
}

/// Two uniform `[0, 1)` draws from a seed (SplitMix64, Steele, Lea and Flood, 2014; 24 bits each).
fn seeded_pair(seed: u64) -> (f32, f32) {
    let next = |s: u64| {
        let mut z = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let a = next(seed);
    let b = next(a);
    ((a >> 40) as f32 / 16_777_216.0, (b >> 40) as f32 / 16_777_216.0)
}

/// Plays destruction sound, VFX and decals from a `.destructionfx.ron` table. Add it next to
/// `regia_physics`'s `DestructionPlugin`.
#[derive(Debug, Clone, Default)]
pub struct DestructionFxPlugin {
    /// The table; `None` starts empty (set one later with
    /// [`DestructionFxPlayer::set_table`] on [`DestructionFxState`]).
    pub table: Option<DestructionFxTable>,
}

/// The ECS resource [`DestructionFxPlugin`] adds: the planner and its counters.
#[derive(Resource, Debug, Default)]
pub struct DestructionFxState {
    /// The planner.
    pub player: DestructionFxPlayer,
    /// The table did not validate when the plugin was built: its error code.
    pub load_error: Option<&'static str>,
}

impl Plugin for DestructionFxPlugin {
    fn build(&self, app: &mut App) {
        let mut state = DestructionFxState::default();
        if let Some(table) = self.table.clone()
            && let Err(e) = state.player.set_table(table)
        {
            tracing::warn!(target: "regia_framework", "destruction fx table refused: {e}");
            state.load_error = Some(e.code());
        }
        // Registered here too, so the plugin stands without `DestructionPlugin` (it then reads nothing).
        app.add_message::<DestructionFxMessage>();
        app.insert_resource(state).add_systems(
            FixedUpdate,
            play_destruction_fx.after(regia_physics::destruction::step_destruction),
        );
    }
}

/// Plans this step's `DestructionFxMessage`s and, with the `destruction-fx` feature, plays them
/// (span `play_destruction_fx`).
pub fn play_destruction_fx(
    mut messages: MessageReader<DestructionFxMessage>,
    mut state: ResMut<DestructionFxState>,
    #[cfg(feature = "destruction-fx")] mut sinks: playback::Sinks<'_>,
) {
    if messages.is_empty() {
        return;
    }
    let _span = tracing::debug_span!("play_destruction_fx").entered();
    let state = &mut *state;
    state.player.plan(messages.read().map(|m| &m.0));
    #[cfg(feature = "destruction-fx")]
    playback::play(&mut state.player, &mut sinks);
}

#[cfg(feature = "destruction-fx")]
mod playback {
    //! The client half: sound cues, VFX systems and decals.

    use regia_app::prelude::*;
    use regia_audio::Audio;
    use regia_core::math::{Quat, Vec3};
    use regia_ecs::ecs::system::SystemParam;
    use regia_vfx::{Decal, DecalPool, VfxScene};

    use super::DestructionFxPlayer;

    /// Where plays go; each is optional.
    #[derive(SystemParam)]
    pub struct Sinks<'w> {
        audio: Option<ResMut<'w, Audio>>,
        vfx: Option<ResMut<'w, VfxScene>>,
        decals: Option<ResMut<'w, DecalPool>>,
    }

    pub(super) fn play(player: &mut DestructionFxPlayer, sinks: &mut Sinks<'_>) {
        let Some(table) = player.table.as_ref() else {
            return;
        };
        let mut sounds = 0;
        let mut refused = 0;
        let mut vfx = 0;
        let mut unknown = 0;
        let mut decals = 0;
        for play in &player.plays {
            let row = &table.rows[play.row as usize];
            let at = Vec3::from_array(play.position);
            if let (Some(cue), Some(audio)) = (row.sound.as_deref(), sinks.audio.as_deref_mut()) {
                match audio.play_cue_seeded(cue, play.seed) {
                    Ok(Some(_)) => sounds += 1,
                    _ => refused += 1,
                }
            }
            if let (Some(system), Some(scene)) = (row.vfx.as_deref(), sinks.vfx.as_deref_mut()) {
                if scene.spawn(system, at, Vec3::Y, play.seed).is_some() {
                    vfx += 1;
                } else {
                    unknown += 1;
                }
            }
            if let (Some(d), Some(pool)) = (row.decal.as_ref(), sinks.decals.as_deref_mut()) {
                pool.spawn(Decal {
                    position: at,
                    rotation: Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)
                        * Quat::from_rotation_z(play.spin),
                    half_size: Vec3::from_array(d.half_size) * play.scale,
                    color: d.color,
                    age: 0.0,
                    lifetime: Some(d.lifetime_secs),
                });
                decals += 1;
            }
        }
        let stats = player.stats_mut();
        stats.sounds += sounds;
        stats.sounds_refused += refused;
        stats.vfx += vfx;
        stats.vfx_unknown += unknown;
        stats.decals += decals;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_physics::destruction::DestructibleId;

    fn table() -> DestructionFxTable {
        DestructionFxTable {
            format_version: DESTRUCTION_FX_FORMAT_VERSION,
            name: "lab".into(),
            max_per_step: 6,
            fallback: Some("default".into()),
            rows: vec![
                DestructionFxRow {
                    tag: "concrete".into(),
                    kinds: vec![],
                    sound: Some("concrete_crack".into()),
                    vfx: Some("dust_puff".into()),
                    decal: Some(FxDecal {
                        half_size: [0.3, 0.3, 0.2],
                        color: [0.4, 0.4, 0.4, 1.0],
                        lifetime_secs: 20.0,
                    }),
                    max_per_step: 4,
                    min_magnitude: 0.0,
                    scale_jitter: 0.25,
                },
                DestructionFxRow {
                    tag: "default".into(),
                    kinds: vec![FxKind::Collapse],
                    sound: Some("rumble".into()),
                    vfx: None,
                    decal: None,
                    max_per_step: 0,
                    min_magnitude: 10.0,
                    scale_jitter: 0.0,
                },
            ],
        }
    }

    fn fx(tag: &str, kind: FxKind, seed: u64, magnitude: f32) -> DestructionFx {
        DestructionFx {
            destructible: DestructibleId(0),
            kind,
            position: [1.0, 2.0, 3.0],
            magnitude,
            fx_tag: tag.into(),
            seed,
        }
    }

    #[test]
    fn caps_filters_and_fallback() {
        let mut p = DestructionFxPlayer::new(table()).expect("table");
        let mut step: Vec<DestructionFx> = (0..10)
            .map(|i| fx("concrete", FxKind::BondBreak, i, 50.0))
            .collect();
        step.push(fx("wood", FxKind::Collapse, 99, 100.0)); // fallback row, collapse: plays
        step.push(fx("wood", FxKind::BondBreak, 98, 100.0)); // fallback row, wrong kind
        step.push(fx("wood", FxKind::Collapse, 97, 1.0)); // too weak
        let plays = p.plan(&step).to_vec();
        assert_eq!(plays.len(), 5, "4 concrete (tag cap) + 1 collapse");
        assert!(plays[..4].iter().all(|p| p.row == 0));
        assert_eq!(plays[4].row, 1);
        let s = p.stats();
        assert_eq!(s.dropped_tag_cap, 6);
        assert_eq!(s.filtered, 2);
        assert_eq!(s.planned, 5);
        // The step cap: 6 over everything.
        let many: Vec<DestructionFx> = (0..10).map(|i| fx("wood", FxKind::Collapse, i, 50.0)).collect();
        assert_eq!(p.plan(&many).len(), 6);
        assert_eq!(p.stats().dropped_step_cap, 4);
    }

    #[test]
    fn variation_follows_the_seed() {
        let mut a = DestructionFxPlayer::new(table()).expect("table");
        let mut b = DestructionFxPlayer::new(table()).expect("table");
        let step = vec![fx("concrete", FxKind::ChunkDetach, 12345, 5.0)];
        let pa = a.plan(&step).to_vec();
        assert_eq!(pa, b.plan(&step).to_vec());
        assert!((0.75..=1.25).contains(&pa[0].scale));
        let other = vec![fx("concrete", FxKind::ChunkDetach, 54321, 5.0)];
        assert_ne!(a.plan(&other)[0].scale, pa[0].scale);
    }

    #[test]
    fn pooled_buffers_stop_growing_after_warm_up() {
        let mut p = DestructionFxPlayer::new(table()).expect("table");
        let busy: Vec<DestructionFx> = (0..10)
            .map(|i| fx("concrete", FxKind::BondBreak, i, 50.0))
            .collect();
        p.plan(&busy);
        let warm = p.stats().buffer_growths;
        assert!(warm > 0);
        for round in 0..100u64 {
            let step: Vec<DestructionFx> = (0..10)
                .map(|i| fx("concrete", FxKind::BondBreak, round * 10 + i, 50.0))
                .collect();
            p.plan(&step);
        }
        assert_eq!(p.stats().buffer_growths, warm, "no growth after warm-up");
        assert_eq!(p.stats().busy_steps, 101);
    }

    #[test]
    fn format_round_trip_and_codes() {
        let t = table();
        let text = t.to_ron().expect("ron");
        assert_eq!(DestructionFxTable::from_ron(&text).expect("parse"), t);
        assert_eq!(
            DestructionFxTable::from_ron("(nope").expect_err("bad").code(),
            "destruction_fx_parse"
        );
        let newer = text.replace("format_version: 1", "format_version: 2");
        assert_eq!(
            DestructionFxTable::from_ron(&newer).expect_err("newer").code(),
            "destruction_fx_version"
        );
        for bad in [
            |t: &mut DestructionFxTable| t.rows[1].tag = "concrete".into(),
            |t: &mut DestructionFxTable| t.rows[0].tag = String::new(),
            |t: &mut DestructionFxTable| t.fallback = Some("glass".into()),
            |t: &mut DestructionFxTable| t.rows[0].scale_jitter = 2.0,
            |t: &mut DestructionFxTable| t.rows[0].min_magnitude = f32::NAN,
            |t: &mut DestructionFxTable| {
                if let Some(d) = t.rows[0].decal.as_mut() {
                    d.lifetime_secs = 0.0;
                }
            },
        ] {
            let mut t = table();
            bad(&mut t);
            assert_eq!(t.validate().expect_err("bad").code(), "destruction_fx_invalid");
        }
        assert!(destruction_fx_migration_sources().is_empty());
        assert!(destruction_fx_schema_fingerprint().contains("scale_jitter"));
    }

    #[test]
    fn no_table_counts_every_message_as_unmatched() {
        let mut p = DestructionFxPlayer::default();
        assert!(p.plan(&[fx("concrete", FxKind::BondBreak, 1, 1.0)]).is_empty());
        assert_eq!(p.stats().unmatched, 1);
    }
}
