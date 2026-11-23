// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3),
// src/experimental/coroutines/generational_storage.rs. Copyright (c) 2019-2021 Fedor Logachev
// <not.fl3@gmail.com>, dual-licensed MIT OR Apache-2.0; used here under the MIT licence
// (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//! Slot storage addressed by generational ids: a freed slot is reused with a bumped generation, so
//! a stale [`GenerationalId`] (a handle to a coroutine that already finished) never reaches the
//! coroutine that now lives in its slot.
//!
//! Changes from the original: out-of-range ids are rejected (`>=` instead of `>`), freeing a
//! vacant or stale slot is a no-op instead of pushing it on the free list twice, and `clear`
//! frees slots instead of truncating, so ids from before the clear stay stale.

/// Handle to one coroutine slot: the slot index plus its reuse generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GenerationalId {
    pub(crate) id: usize,
    pub(crate) generation: usize,
}

impl GenerationalId {
    /// The slot index (diagnostics: `coroutines.list`).
    pub fn index(self) -> usize {
        self.id
    }

    /// How many times the slot was reused before this id was handed out.
    pub fn generation(self) -> usize {
        self.generation
    }
}

struct GenerationalCell<T> {
    generation: usize,
    state: T,
}

/// A `Vec` of optional cells plus a free list.
pub(crate) struct GenerationalStorage<T> {
    vec: Vec<Option<GenerationalCell<T>>>,
    /// Vacant slot indices with the generation they last held.
    free_indices: Vec<(usize, usize)>,
}

impl<T> Default for GenerationalStorage<T> {
    fn default() -> Self {
        Self {
            vec: Vec::new(),
            free_indices: Vec::new(),
        }
    }
}

impl<T> GenerationalStorage<T> {
    /// Stores `data` in a free slot (bumping its generation) or a new one.
    pub fn push(&mut self, data: T) -> GenerationalId {
        if let Some((free_id, old_generation)) = self.free_indices.pop()
            && let Some(cell) = self.vec.get_mut(free_id)
            && cell.is_none()
        {
            let generation = old_generation + 1;
            *cell = Some(GenerationalCell {
                state: data,
                generation,
            });
            return GenerationalId {
                id: free_id,
                generation,
            };
        }
        self.vec.push(Some(GenerationalCell {
            state: data,
            generation: 0,
        }));
        GenerationalId {
            id: self.vec.len() - 1,
            generation: 0,
        }
    }

    /// The value behind `id`, unless the slot was freed or reused since.
    pub fn get(&self, id: GenerationalId) -> Option<&T> {
        let cell = self.vec.get(id.id)?.as_ref()?;
        (cell.generation == id.generation).then_some(&cell.state)
    }

    /// Mutable access to the value behind `id`, unless the slot was freed or reused since.
    pub fn get_mut(&mut self, id: GenerationalId) -> Option<&mut T> {
        let cell = self.vec.get_mut(id.id)?.as_mut()?;
        (cell.generation == id.generation).then_some(&mut cell.state)
    }

    /// Number of occupied slots.
    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.vec.iter().filter(|c| c.is_some()).count()
    }

    /// Drops every value. Every slot goes on the free list with its generation, so ids handed out
    /// before stay invalid after the slots are reused.
    pub fn clear(&mut self) {
        for (index, slot) in self.vec.iter_mut().enumerate() {
            if let Some(cell) = slot.take() {
                self.free_indices.push((index, cell.generation));
            }
        }
    }

    /// Frees the slot of `id` and returns its value. An outdated id (the slot was freed or reused)
    /// is a legitimate request and returns `None` without touching the slot's current value.
    pub fn free(&mut self, id: GenerationalId) -> Option<T> {
        let slot = self.vec.get_mut(id.id)?;
        if slot.as_ref()?.generation != id.generation {
            return None;
        }
        let cell = slot.take()?;
        self.free_indices.push((id.id, cell.generation));
        Some(cell.state)
    }

    /// Bytes reserved for slots (diagnostics).
    pub fn allocated_memory(&self) -> usize {
        self.vec.capacity() * size_of::<Option<GenerationalCell<T>>>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freed_slots_are_reused_with_a_new_generation() {
        let mut storage = GenerationalStorage::default();
        let a = storage.push("a");
        let b = storage.push("b");
        assert_eq!(storage.free(a), Some("a"));
        let c = storage.push("c");
        assert_eq!(c.index(), a.index(), "slot reused");
        assert_eq!(c.generation(), a.generation() + 1);
        assert_eq!(storage.get(a), None, "the stale id misses the new value");
        assert_eq!(storage.get(c), Some(&"c"));
        assert_eq!(storage.get(b), Some(&"b"));
        assert_eq!(storage.count(), 2);
    }

    #[test]
    fn stale_and_out_of_range_ids_are_harmless() {
        let mut storage = GenerationalStorage::default();
        let a = storage.push(1);
        assert_eq!(storage.free(a), Some(1));
        assert_eq!(storage.free(a), None, "double free is a no-op");
        let b = storage.push(2);
        assert_eq!(storage.free(a), None, "a stale id does not free the reused slot");
        assert_eq!(storage.get(b), Some(&2));
        let beyond = GenerationalId { id: 7, generation: 0 };
        assert_eq!(storage.get(beyond), None);
        assert_eq!(storage.get_mut(beyond), None);
        assert_eq!(storage.free(beyond), None);
        // One free list entry per freed slot: two pushes need one reuse and one new slot.
        storage.free(b);
        let c = storage.push(3);
        let d = storage.push(4);
        assert_eq!(c.index(), 0);
        assert_eq!(d.index(), 1);
    }

    #[test]
    fn clear_invalidates_everything() {
        let mut storage = GenerationalStorage::default();
        let a = storage.push(1);
        storage.clear();
        assert_eq!(storage.get(a), None);
        assert_eq!(storage.count(), 0);
        let b = storage.push(2);
        assert_eq!(b.index(), a.index());
        assert_eq!(storage.get(a), None, "an id from before the clear stays stale");
        assert!(storage.allocated_memory() < usize::MAX);
    }
}
