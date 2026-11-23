// SPDX-License-Identifier: MPL-2.0
//! Stacking (`docs/specs/53-abilities-attributes-effects.md` § 2.5): which applications merge,
//! the cap, what a merge does to time and pulses, what happens at the cap and at lapse.

use std::sync::Arc;

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use super::effects::{
    AppliedEffect, AppliedEffects, ApplyEffect, ApplyOutcome, EffectAsset, EndReason, Outcome, StacksChanged,
    apply_effect_at_depth, end_effect, evaluate_amounts, length_ticks,
};
use super::{AbilityCounters, AbilityError, cues};

/// Which applications merge into one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StackKey {
    /// One entry per `(asset, source)`.
    BySource,
    /// One entry per asset on the target.
    ByTarget,
}

/// What a merge does to the remaining time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeOnAdd {
    /// Back to the full length.
    Reset,
    /// Unchanged.
    Keep,
    /// Plus the new length.
    Add,
}

/// What a merge does to the pulse timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PulseOnAdd {
    /// Next pulse one full period from now.
    Restart,
    /// Unchanged.
    Keep,
}

/// What a successful merge does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnAdd {
    /// Remaining time.
    pub time: TimeOnAdd,
    /// Pulse timer.
    pub pulse: PulseOnAdd,
}

impl Default for OnAdd {
    fn default() -> Self {
        Self {
            time: TimeOnAdd::Reset,
            pulse: PulseOnAdd::Keep,
        }
    }
}

/// What an application does at the cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WhenFull {
    /// Refuse (`rejected_full`).
    Reject,
    /// Refuse but renew the time per `on_add.time`.
    RejectRenew,
    /// End the entry (`restarted`) and apply fresh at one stack.
    Restart,
    /// Refuse and apply another effect to the target instead (`spilled`).
    Spill(String),
}

/// What happens when the time runs out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OnLapse {
    /// End the entry.
    EndAll,
    /// Drop one stack and renew the time; end at zero.
    PopOne,
    /// Renew the time with the count unchanged (only removal ends it).
    Renew,
}

fn default_cap() -> u16 {
    1
}

/// The stacking rule of an effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackRule {
    /// Which applications merge.
    pub key: StackKey,
    /// Highest stack count, at least 1.
    #[serde(default = "default_cap")]
    pub cap: u16,
    /// What a merge does.
    #[serde(default)]
    pub on_add: OnAdd,
    /// What happens at the cap.
    pub when_full: WhenFull,
    /// What happens at lapse.
    pub on_lapse: OnLapse,
    /// Whether every added stack writes a burst cue.
    #[serde(default)]
    pub cue_each_stack: bool,
}

/// The result of a merge attempt.
pub(crate) enum Merge {
    /// No entry to merge with: apply as new.
    New,
    /// The attempt is finished.
    Done(ApplyOutcome),
    /// The old entry ended (`restarted`): apply fresh.
    Fresh,
}

fn renew(entry: &mut AppliedEffect, time: TimeOnAdd, length: u32) {
    match time {
        TimeOnAdd::Reset => {
            entry.remaining_ticks = length;
            entry.duration_ticks = length;
        }
        TimeOnAdd::Keep => {}
        TimeOnAdd::Add => entry.remaining_ticks = entry.remaining_ticks.saturating_add(length),
    }
}

/// Tries to merge `req` into an existing entry (§ 2.5).
pub(crate) fn try_merge(
    world: &mut World,
    req: &ApplyEffect,
    asset: &Arc<EffectAsset>,
    rule: &StackRule,
    depth: u8,
) -> Result<Merge, AbilityError> {
    let Some(effects) = world.get::<AppliedEffects>(req.target) else {
        return Ok(Merge::New);
    };
    let Some(index) = effects
        .list
        .iter()
        .position(|e| e.asset == req.asset && (rule.key == StackKey::ByTarget || e.source == req.source))
    else {
        return Ok(Merge::New);
    };
    let mut entry = effects.list[index].clone();
    let tick = world.get_resource::<AbilityCounters>().map_or(0, |c| c.tick);
    // The duration reads the stack count the entry will have after the merge.
    let merged_stacks = if entry.stacks < rule.cap.max(1) {
        entry.stacks + 1
    } else {
        entry.stacks
    };
    let length = length_ticks(world, asset, req, merged_stacks);
    if entry.stacks < rule.cap.max(1) {
        let old = entry.stacks;
        entry.stacks += 1;
        renew(&mut entry, rule.on_add.time, length);
        if rule.on_add.pulse == PulseOnAdd::Restart && entry.period_ticks > 0 {
            entry.next_pulse_tick = tick + u64::from(entry.period_ticks);
        }
        if req.level > entry.level {
            entry.level = req.level;
            let (edits, live, _) = evaluate_amounts(world, asset, &entry, req.target);
            entry.edit_amounts = edits;
            entry.live_amounts = live;
        }
        let id = entry.id;
        let new = entry.stacks;
        put_back(world, req.target, entry);
        world.write_message(StacksChanged {
            target: req.target,
            id,
            old,
            new,
        });
        if rule.cue_each_stack {
            cues::fire_cues(
                world,
                &asset.cues,
                cues::CueAt::Applied,
                None,
                cues::CueBase {
                    entity: req.target,
                    source: req.source,
                    amount: f32::from(new),
                    effect: Some(id),
                },
            );
        }
        return Ok(Merge::Done(ApplyOutcome {
            outcome: Outcome::Stacked,
            id: Some(id),
            stacks: new,
        }));
    }
    match &rule.when_full {
        WhenFull::Reject => Ok(Merge::Done(ApplyOutcome {
            outcome: Outcome::RejectedFull,
            id: Some(entry.id),
            stacks: entry.stacks,
        })),
        WhenFull::RejectRenew => {
            renew(&mut entry, rule.on_add.time, length);
            let out = ApplyOutcome {
                outcome: Outcome::RejectedFull,
                id: Some(entry.id),
                stacks: entry.stacks,
            };
            put_back(world, req.target, entry);
            Ok(Merge::Done(out))
        }
        WhenFull::Restart => {
            end_effect(world, req.target, entry.id, EndReason::Restarted);
            Ok(Merge::Fresh)
        }
        WhenFull::Spill(path) => {
            let spill = ApplyEffect {
                asset: path.clone(),
                ..req.clone()
            };
            let _ = apply_effect_at_depth(world, spill, depth + 1)?;
            Ok(Merge::Done(ApplyOutcome {
                outcome: Outcome::Spilled,
                id: Some(entry.id),
                stacks: entry.stacks,
            }))
        }
    }
}

fn put_back(world: &mut World, target: Entity, entry: AppliedEffect) {
    if let Some(mut effects) = world.get_mut::<AppliedEffects>(target)
        && let Some(slot) = effects.list.iter_mut().find(|e| e.id == entry.id)
    {
        *slot = entry;
    }
}

/// What a lapse did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lapse {
    /// The entry ends (`expired`).
    End,
    /// The last stack dropped (`PopOne`): the entry ends (`expired`).
    Emptied { old: u16 },
    /// One stack dropped, time renewed.
    Popped { old: u16, new: u16 },
    /// Time renewed.
    Renewed,
}

/// Applies the lapse rule to an entry whose time ran out.
pub(crate) fn lapse(entry: &mut AppliedEffect, rule: Option<&StackRule>) -> Lapse {
    match rule.map(|r| r.on_lapse) {
        None | Some(OnLapse::EndAll) => Lapse::End,
        Some(OnLapse::PopOne) => {
            let old = entry.stacks;
            if old <= 1 {
                entry.stacks = 0;
                return Lapse::Emptied { old };
            }
            entry.stacks -= 1;
            entry.remaining_ticks = entry.duration_ticks.max(1);
            Lapse::Popped {
                old,
                new: entry.stacks,
            }
        }
        Some(OnLapse::Renew) => {
            entry.remaining_ticks = entry.duration_ticks.max(1);
            Lapse::Renewed
        }
    }
}
