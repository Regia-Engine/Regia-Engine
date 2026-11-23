// SPDX-License-Identifier: MPL-2.0
//! [`ProjectilePool`]: a fixed number of projectile slots, stepped in slot order (ADR-8509).

use crate::ids::DetIdAllocator;
use crate::site_physics::WorldPhysics;

use super::{Impact, ProjectileParams, ProjectileState, SegmentHits, step};

/// One occupied slot: the projectile's kind and its state.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PoolEntry {
    /// Its numbers.
    pub params: ProjectileParams,
    /// Its state.
    pub state: ProjectileState,
}

/// Up to `capacity` projectiles in slots `1..=capacity` (ADR-8509).
///
/// Slots come from a [`DetIdAllocator`]: [`ProjectilePool::spawn`] takes the oldest freed slot, else
/// the lowest never-used one, so the slot a shot gets depends only on the order of earlier spawns
/// and ends. [`ProjectilePool::step_all`] steps the live slots in ascending order and appends their
/// impacts in that order, so two peers that spawn the same shots into the same slots get the same
/// impact list whatever order they spawned them in. An ended projectile's slot is freed in the same
/// tick, after its impact is appended.
///
/// No heap allocation after [`ProjectilePool::new`]: the slots and the allocator's lists are sized
/// there. Give `step_all` an impacts `Vec` with room for [`ProjectilePool::capacity`] entries (at
/// most one impact per projectile per tick) and clear it each tick, and the tick allocates nothing.
#[derive(Debug, Clone)]
pub struct ProjectilePool {
    ids: DetIdAllocator<u16>,
    /// `slots[id - 1]`.
    slots: Vec<Option<PoolEntry>>,
}

impl ProjectilePool {
    /// A pool of `capacity` slots (the host's setting; at least 1). Sizes everything up front.
    pub fn new(capacity: u16) -> Self {
        let capacity = capacity.max(1);
        let mut ids = DetIdAllocator::<u16>::new(capacity);
        // Touch every id once so the allocator's bitset and free list reach their full size now,
        // not on a later tick. Freeing in ascending order leaves the FIFO free list in the order a
        // fresh allocator would issue, so the slots handed out are the same.
        for _ in 0..capacity {
            let _ = ids.alloc();
        }
        for id in 1..=capacity {
            ids.free(id);
        }
        ProjectilePool {
            ids,
            slots: vec![None; usize::from(capacity)],
        }
    }

    /// The number of slots.
    pub fn capacity(&self) -> u16 {
        self.ids.cap()
    }

    /// Live projectiles.
    pub fn len(&self) -> usize {
        self.ids.live_count()
    }

    /// True when no projectile is live.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Put a projectile in the next free slot. `None` when the pool is full (the shot is dropped;
    /// the host decides whether that is an error).
    pub fn spawn(&mut self, params: ProjectileParams, state: ProjectileState) -> Option<u16> {
        let slot = self.ids.alloc()?;
        self.slots[usize::from(slot) - 1] = Some(PoolEntry { params, state });
        Some(slot)
    }

    /// Put a projectile in slot `slot` (a replayed or host-assigned slot). `false` when the slot is
    /// 0, past the capacity or live.
    pub fn spawn_in(&mut self, slot: u16, params: ProjectileParams, state: ProjectileState) -> bool {
        if !self.ids.reserve(slot) {
            return false;
        }
        self.slots[usize::from(slot) - 1] = Some(PoolEntry { params, state });
        true
    }

    /// Remove the projectile in `slot` without an impact. `false` when the slot is not live.
    pub fn despawn(&mut self, slot: u16) -> bool {
        if !self.ids.is_live(slot) {
            return false;
        }
        self.slots[usize::from(slot) - 1] = None;
        self.ids.free(slot);
        true
    }

    /// The projectile in `slot`.
    pub fn get(&self, slot: u16) -> Option<&PoolEntry> {
        let index = usize::from(slot).checked_sub(1)?;
        self.slots.get(index)?.as_ref()
    }

    /// The projectile in `slot`, to change (a gameplay effect on a live shot). Changing it is part
    /// of the simulation: every peer must make the same change on the same tick.
    pub fn get_mut(&mut self, slot: u16) -> Option<&mut PoolEntry> {
        let index = usize::from(slot).checked_sub(1)?;
        self.slots.get_mut(index)?.as_mut()
    }

    /// Live projectiles in ascending slot order (for drawing them, or hashing a snapshot).
    pub fn iter(&self) -> impl Iterator<Item = (u16, &PoolEntry)> + '_ {
        self.slots
            .iter()
            .zip(1_u16..)
            .filter_map(|(entry, slot)| entry.as_ref().map(|e| (slot, e)))
    }

    /// Step every live projectile one tick, in ascending slot order, and append each impact to
    /// `impacts` with its slot and its position in this tick's list (`order`, counted from the
    /// list's length on entry). The ended projectiles' slots are freed, in the same order.
    pub fn step_all(
        &mut self,
        world: &WorldPhysics,
        wind: [f32; 3],
        dt: f32,
        hits: &mut impl SegmentHits,
        impacts: &mut Vec<Impact>,
    ) {
        let start = impacts.len();
        for (slot, entry) in (1_u16..).zip(self.slots.iter_mut()) {
            let Some(live) = entry.as_mut() else { continue };
            let impact = step(&live.params, world, wind, &mut live.state, dt, hits);
            let alive = live.state.alive;
            if let Some(mut impact) = impact {
                impact.slot = slot;
                impact.order = u16::try_from(impacts.len() - start).unwrap_or(u16::MAX);
                impacts.push(impact);
            }
            if !alive {
                // Ended this tick (or spawned already ended): free the slot.
                *entry = None;
                self.ids.free(slot);
            }
        }
    }
}
