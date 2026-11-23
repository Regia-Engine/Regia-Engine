// SPDX-License-Identifier: MPL-2.0
//! In-memory asset storage and load state.

use std::collections::HashMap;

use crate::id::{AssetId, Handle};

/// Per-type asset slab with generational handles.
pub struct Assets<T> {
    slots: Vec<Option<Slot<T>>>,
    id_map: HashMap<AssetId, u32>,
    /// Recorded events, once [`Assets::enable_events`] turned them on (ADR-7202).
    events: Option<Vec<AssetEvent<T>>>,
    /// The id each removed slot held ([`Assets::id_of_slot`]); slots are never reused.
    removed: HashMap<u32, (u32, AssetId)>,
}

struct Slot<T> {
    generation: u32,
    /// The stable id the slot was inserted under ([`Assets::id_of`]).
    id: AssetId,
    value: T,
}

/// Load state for a handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadState {
    /// Not yet requested.
    NotLoaded,
    /// IO or decode in flight.
    Loading,
    /// Available in [`Assets`].
    Loaded,
    /// Failed with message.
    Failed(String),
}

/// Asset lifecycle events (spec § 3). An [`Assets`] store records them once
/// [`Assets::enable_events`] turned them on: [`Assets::insert`] of a new id is `Added`, of a loaded
/// id `Modified`, [`Assets::remove`] is `Removed` (with the handle that just stopped resolving).
/// The [`crate::AssetServer`] adds `LoadFailed`, and the [`crate::AssetPlugin`] forwards them as
/// ECS messages each frame (ADR-7202).
#[derive(Clone, Debug)]
pub enum AssetEvent<T> {
    /// New asset inserted.
    Added(Handle<T>),
    /// Asset replaced in place (hot reload).
    Modified(Handle<T>),
    /// Asset removed from memory.
    Removed(Handle<T>),
    /// Load failed.
    LoadFailed {
        /// Asset id.
        id: AssetId,
        /// Error text.
        error: String,
    },
}

// By hand: `T` is only the handle's marker, so equality asks nothing of it.
impl<T> PartialEq for AssetEvent<T> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Added(a), Self::Added(b))
            | (Self::Modified(a), Self::Modified(b))
            | (Self::Removed(a), Self::Removed(b)) => a.index == b.index && a.generation == b.generation,
            (Self::LoadFailed { id: a, error: e }, Self::LoadFailed { id: b, error: f }) => a == b && e == f,
            _ => false,
        }
    }
}

impl<T: Send + Sync + 'static> regia_ecs::prelude::Message for AssetEvent<T> {}

impl<T> Default for Assets<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            id_map: HashMap::new(),
            events: None,
            removed: HashMap::new(),
        }
    }
}

impl<T> Assets<T> {
    /// Empty storage.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `value` and return a strong handle.
    ///
    /// ```
    /// use regia_asset::{AssetId, Assets};
    ///
    /// let mut assets = Assets::new();
    /// let id = AssetId::new_random();
    /// let handle = assets.insert(id, 1u32);
    /// // Inserting the same id again (a hot reload) keeps the handle valid.
    /// assets.insert(id, 2u32);
    /// assert_eq!(assets.get(&handle), Some(&2));
    /// ```
    ///
    /// Inserting an id that is already loaded replaces the value in place (hot reload,
    /// `docs/specs/12-assets.md` § 8): the slot and its generation stay, so every handle to it
    /// stays valid and resolves to the new value.
    pub fn insert(&mut self, id: AssetId, value: T) -> Handle<T> {
        if let Some(&idx) = self.id_map.get(&id) {
            let slot = &mut self.slots[idx as usize];
            if let Some(s) = slot {
                s.value = value;
                let generation = s.generation;
                self.push_event(AssetEvent::Modified(Handle::from_raw(idx, generation)));
                return Handle::from_raw(idx, generation);
            }
        }
        // Removed slots are never reused (see `remove`): a new id always takes a new slot, so a
        // stale handle can never resolve to another asset at the same generation.
        let index = self.slots.len() as u32;
        self.slots.push(Some(Slot {
            generation: 1,
            id,
            value,
        }));
        self.id_map.insert(id, index);
        self.push_event(AssetEvent::Added(Handle::from_raw(index, 1)));
        Handle::from_raw(index, 1)
    }

    /// Resolve a handle to its value when generation matches.
    pub fn get(&self, handle: &Handle<T>) -> Option<&T> {
        let slot = self.slots.get(handle.index as usize)?.as_ref()?;
        if slot.generation != handle.generation {
            return None;
        }
        Some(&slot.value)
    }

    /// Drops the value stored for `id`; true when there was one. Every handle to it stops
    /// resolving. The slot is not reused, so a stale handle never reads another asset.
    pub fn remove(&mut self, id: AssetId) -> bool {
        let Some(idx) = self.id_map.remove(&id) else {
            return false;
        };
        let Some(slot) = self.slots[idx as usize].take() else {
            return false;
        };
        self.removed.insert(idx, (slot.generation, slot.id));
        self.push_event(AssetEvent::Removed(Handle::from_raw(idx, slot.generation)));
        true
    }

    /// Live assets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.id_map.len()
    }

    /// True when nothing is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.id_map.is_empty()
    }

    /// Starts recording [`AssetEvent`]s (off by default, so a store nobody drains does not grow).
    pub fn enable_events(&mut self) {
        if self.events.is_none() {
            self.events = Some(Vec::new());
        }
    }

    /// Whether events are recorded.
    #[must_use]
    pub fn events_enabled(&self) -> bool {
        self.events.is_some()
    }

    /// Records `event` when events are on (the server adds `LoadFailed` this way).
    pub fn push_event(&mut self, event: AssetEvent<T>) {
        if let Some(events) = &mut self.events {
            events.push(event);
        }
    }

    /// The recorded events since the last call, in order; recording stays on.
    pub fn drain_events(&mut self) -> Vec<AssetEvent<T>> {
        self.events.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// Resolve id to handle if loaded.
    pub fn handle_for_id(&self, id: AssetId) -> Option<Handle<T>> {
        let idx = self.id_map.get(&id)?;
        let slot = self.slots[*idx as usize].as_ref()?;
        Some(Handle::from_raw(*idx, slot.generation))
    }

    /// The stable id a live handle was loaded under: the inverse of [`Self::handle_for_id`]. `None`
    /// for an invalid, stale or removed handle. Read only; nothing about loading changes.
    pub fn id_of(&self, handle: &Handle<T>) -> Option<AssetId> {
        let slot = self.slots.get(handle.index as usize)?.as_ref()?;
        (slot.generation == handle.generation).then_some(slot.id)
    }

    /// The id a handle was issued for, live or removed: the way to name the asset of an
    /// [`AssetEvent::Removed`]. `None` for a handle this store never issued.
    pub fn id_of_slot(&self, handle: &Handle<T>) -> Option<AssetId> {
        self.id_of(handle).or_else(|| {
            self.removed
                .get(&handle.index)
                .and_then(|&(generation, id)| (generation == handle.generation).then_some(id))
        })
    }

    /// Slots handed out so far, live or removed. Slots are never reused, so a caller that
    /// remembers this number reads only the newer ones with [`Self::entries_from`].
    pub fn slot_count(&self) -> u32 {
        self.slots.len() as u32
    }

    /// The live assets in slot order from slot `first` on: `(stable id, handle)`. With
    /// [`Self::slot_count`] a caller visits each newly loaded asset once.
    pub fn entries_from(&self, first: u32) -> impl Iterator<Item = (AssetId, Handle<T>)> + '_ {
        self.slots
            .iter()
            .enumerate()
            .skip(first as usize)
            .filter_map(|(i, s)| {
                s.as_ref()
                    .map(|s| (s.id, Handle::from_raw(i as u32, s.generation)))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reinserting_an_id_keeps_existing_handles_valid() {
        let mut assets = Assets::new();
        let id = AssetId::new_random();
        let first = assets.insert(id, 1u32);
        let second = assets.insert(id, 2u32);
        assert_eq!(first, second);
        assert_eq!(assets.get(&first), Some(&2));
    }

    #[test]
    fn a_removed_slot_still_names_its_id_and_events_follow_insert_and_remove() {
        let mut assets = Assets::new();
        let id = AssetId::new_random();
        assert!(assets.drain_events().is_empty(), "off by default");
        let silent = assets.insert(AssetId::new_random(), 0u32);
        assets.enable_events();
        let h = assets.insert(id, 1u32);
        assets.insert(id, 2u32);
        assert!(assets.remove(id));
        assert_eq!(assets.id_of(&h), None);
        assert_eq!(assets.id_of_slot(&h), Some(id));
        assert_eq!(assets.id_of_slot(&Handle::INVALID), None);
        assert_eq!(
            assets.drain_events(),
            vec![
                AssetEvent::Added(h),
                AssetEvent::Modified(h),
                AssetEvent::Removed(h)
            ]
        );
        assert_eq!(assets.len(), 1);
        assert!(assets.id_of_slot(&silent).is_some());
    }

    #[test]
    fn removing_an_id_invalidates_its_handles() {
        let mut assets = Assets::new();
        let id = AssetId::new_random();
        let first = assets.insert(id, 1u32);
        assert!(assets.remove(id));
        assert!(!assets.remove(id));
        assert_eq!(assets.get(&first), None);
        assert!(assets.handle_for_id(id).is_none());
        let again = assets.insert(id, 5u32);
        assert_eq!(assets.get(&again), Some(&5));
        assert_eq!(assets.get(&first), None, "the old handle stays dead");
    }

    #[test]
    fn handles_map_back_to_their_ids_and_new_slots_are_listed_once() {
        let mut assets = Assets::new();
        let (a, b) = (AssetId::new_random(), AssetId::new_random());
        let ha = assets.insert(a, 1u32);
        assert_eq!(assets.id_of(&ha), Some(a));
        assert_eq!(assets.handle_for_id(a), Some(ha));
        assert_eq!(assets.id_of(&Handle::INVALID), None);
        let seen = assets.slot_count();
        let hb = assets.insert(b, 2u32);
        assert_eq!(assets.entries_from(seen).collect::<Vec<_>>(), vec![(b, hb)]);
        assert_eq!(assets.entries_from(0).count(), 2);
        // A hot reload keeps the slot: nothing new to list.
        let seen = assets.slot_count();
        assert_eq!(assets.insert(a, 3u32), ha);
        assert_eq!(assets.entries_from(seen).count(), 0);
        // Removed: the handle no longer maps back, and the slot is skipped.
        assert!(assets.remove(a));
        assert_eq!(assets.id_of(&ha), None);
        assert_eq!(assets.entries_from(0).collect::<Vec<_>>(), vec![(b, hb)]);
    }
}
