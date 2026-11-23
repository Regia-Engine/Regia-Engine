// SPDX-License-Identifier: MPL-2.0
//! Cues (`docs/specs/53-abilities-attributes-effects.md` § 2.7): presentation-only messages. No
//! simulation system reads [`CueEvent`].

use std::collections::VecDeque;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use serde::{Deserialize, Serialize};

use super::{AbilityCounters, AbilitySettings, EffectId};
use crate::tags::Tag;

/// Which moment a cue binding follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CueAt {
    /// The effect applied (or the ability started): a burst.
    Applied,
    /// A pulse ran: a burst.
    Pulsed,
    /// The effect ended (or the ability ended): a burst.
    Removed,
    /// Begin when the effect starts, end when it ends.
    Sustained,
}

/// Free-form numbers a cue consumer may use.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CueParams {
    /// Intensity scale (consumer-defined).
    #[serde(default)]
    pub scale: Option<f32>,
}

/// Links a cue tag to an effect or ability moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CueBinding {
    /// The cue tag (by convention under `Cue.`).
    pub tag: Tag,
    /// The moment.
    pub at: CueAt,
    /// Parameters.
    #[serde(default)]
    pub params: CueParams,
}

/// The kind of a [`CueEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CueKind {
    /// A one-off.
    Burst,
    /// A sustained cue starts.
    Begin,
    /// A sustained cue ends.
    End,
}

impl CueKind {
    /// Lowercase name.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            CueKind::Burst => "burst",
            CueKind::Begin => "begin",
            CueKind::End => "end",
        }
    }
}

/// A presentation request. Consumers are ordinary `Update` systems.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct CueEvent {
    /// The entity the cue plays on.
    pub entity: Entity,
    /// The cue tag.
    pub tag: Tag,
    /// Burst, begin or end.
    pub kind: CueKind,
    /// Who caused it.
    pub source: Option<Entity>,
    /// A number (stack count for sustained cues, the edit amount for pulses).
    pub amount: f32,
    /// Where, when known.
    pub location: Option<Vec3>,
    /// Surface normal, when known.
    pub normal: Option<Vec3>,
    /// The effect, for effect cues.
    pub effect: Option<EffectId>,
}

/// The last 256 cues with their tick, for `abilities.cues`.
#[derive(Resource, Debug, Clone, Default)]
pub struct CueLog {
    ring: VecDeque<(u64, CueEvent)>,
}

/// Capacity of the [`CueLog`] ring.
pub const CUE_RING: usize = 256;

impl CueLog {
    /// Cues at or after `since_tick`, oldest first, at most `limit`.
    #[must_use]
    pub fn since(&self, since_tick: u64, limit: usize) -> Vec<(u64, CueEvent)> {
        self.ring
            .iter()
            .filter(|(tick, _)| *tick >= since_tick)
            .take(limit)
            .cloned()
            .collect()
    }

    fn push(&mut self, tick: u64, cue: CueEvent) {
        if self.ring.len() == CUE_RING {
            self.ring.pop_front();
        }
        self.ring.push_back((tick, cue));
    }
}

/// Writes `cue` unless cues are disabled or filtered for the entity.
pub(crate) fn write_cue(world: &mut World, cue: CueEvent) {
    if let Some(settings) = world.get_resource::<AbilitySettings>() {
        if !settings.cues_enabled {
            return;
        }
        if let Some(filter) = settings.cue_filter
            && !filter(cue.entity, world)
        {
            return;
        }
    }
    let tick = world.get_resource::<AbilityCounters>().map_or(0, |c| c.tick);
    if let Some(mut log) = world.get_resource_mut::<CueLog>() {
        log.push(tick, cue.clone());
    }
    world.write_message(cue);
}

/// The parts of a [`CueEvent`] shared by every binding of one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CueBase {
    pub entity: Entity,
    pub source: Option<Entity>,
    pub amount: f32,
    pub effect: Option<EffectId>,
}

/// Writes the cues of `bindings` for a moment. `at` is the moment; `Sustained` bindings write
/// `kind` (Begin or End) when `sustained` is given.
pub(crate) fn fire_cues(
    world: &mut World,
    bindings: &[CueBinding],
    at: CueAt,
    sustained: Option<CueKind>,
    base: CueBase,
) {
    for binding in bindings {
        let kind = if binding.at == at {
            CueKind::Burst
        } else if binding.at == CueAt::Sustained
            && let Some(kind) = sustained
        {
            kind
        } else {
            continue;
        };
        write_cue(
            world,
            CueEvent {
                entity: base.entity,
                tag: binding.tag,
                kind,
                source: base.source,
                amount: base.amount,
                location: None,
                normal: None,
                effect: base.effect,
            },
        );
    }
}
