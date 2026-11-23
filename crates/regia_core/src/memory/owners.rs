// SPDX-License-Identifier: MPL-2.0
//! Named heap owners inside a subsystem (ADR-3351).
//!
//! A [`Subsystem`] row says *which part of the engine* holds bytes; an owner says *what* inside it
//! does: `render.shader_cache`, `render.atmosphere_luts`, ... An owner is a `static`
//! [`MemoryOwner`]; [`enter_owner`] works like [`super::enter`] and also tags each allocation with
//! the owner, so the bytes count against both the subsystem (the totals do not change) and the
//! owner's row. A plain [`super::enter`] of another subsystem inside an owner scope clears the
//! owner (an asset load started from renderer code is the asset subsystem's, not the renderer
//! owner's); one of the owner's own subsystem keeps it.
//!
//! At most [`MAX_OWNERS`]` - 1` owners get a row (the first ones entered, process-wide); later
//! ones are counted in their subsystem only. Registration takes a lock once per owner; after that
//! entering an owner costs two thread-local stores.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use super::{Slot, Subsystem, SubsystemMemory};

/// Owner rows the ledger has room for, the unused id 0 included.
pub const MAX_OWNERS: usize = 64;

/// A named heap owner inside a [`Subsystem`]: declare it as a `static` and enter it with
/// [`enter_owner`].
///
/// ```
/// use regia_core::memory::{MemoryOwner, Subsystem, enter_owner};
/// static LUTS: MemoryOwner = MemoryOwner::new(Subsystem::Render, "render.example_luts");
/// let _mem = enter_owner(&LUTS);
/// ```
#[derive(Debug)]
pub struct MemoryOwner {
    subsystem: Subsystem,
    name: &'static str,
    /// The owner's row, 0 until the first [`enter_owner`] (or when the ledger is full).
    id: AtomicU8,
}

impl MemoryOwner {
    /// An owner called `name` (stable, lowercase, `<subsystem>.<holder>`) inside `subsystem`.
    #[must_use]
    pub const fn new(subsystem: Subsystem, name: &'static str) -> Self {
        Self {
            subsystem,
            name,
            id: AtomicU8::new(0),
        }
    }

    /// The owner's name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The subsystem the owner's bytes also count against.
    #[must_use]
    pub fn subsystem(&self) -> Subsystem {
        self.subsystem
    }

    /// The owner's row id, registering it on first use; 0 when the ledger is full.
    fn id(&self) -> u8 {
        match self.id.load(Ordering::Acquire) {
            0 => register(self),
            id => id,
        }
    }
}

static OWNER_LEDGER: [Slot; MAX_OWNERS] = [const { Slot::new() }; MAX_OWNERS];
static OWNER_INFO: [OnceLock<(Subsystem, &'static str)>; MAX_OWNERS] =
    [const { OnceLock::new() }; MAX_OWNERS];
/// The next free id (1-based; 0 means "no owner").
static NEXT_OWNER: AtomicU8 = AtomicU8::new(1);
static REGISTER: Mutex<()> = Mutex::new(());

/// Gives `owner` a row. Two statics with the same subsystem and name share one row.
fn register(owner: &MemoryOwner) -> u8 {
    let _guard = REGISTER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let id = owner.id.load(Ordering::Acquire);
    if id != 0 {
        return id;
    }
    let next = NEXT_OWNER.load(Ordering::Acquire);
    let same = (1..next).find(|i| OWNER_INFO[usize::from(*i)].get() == Some(&(owner.subsystem, owner.name)));
    let id = match same {
        Some(id) => id,
        None if usize::from(next) < MAX_OWNERS => {
            let _ = OWNER_INFO[usize::from(next)].set((owner.subsystem, owner.name));
            NEXT_OWNER.store(next + 1, Ordering::Release);
            next
        }
        // Full: the bytes still count against the subsystem.
        None => return 0,
    };
    owner.id.store(id, Ordering::Release);
    id
}

/// The owner slot of a header byte, or `None` for 0 and unknown ids.
pub(super) fn owner_slot(owner: u8) -> Option<&'static Slot> {
    if owner == 0 {
        return None;
    }
    OWNER_LEDGER.get(usize::from(owner))
}

/// The subsystem of owner row `owner`.
pub(super) fn owner_subsystem(owner: u8) -> Option<Subsystem> {
    OWNER_INFO
        .get(usize::from(owner))
        .and_then(OnceLock::get)
        .map(|(subsystem, _)| *subsystem)
}

/// Counts this thread's heap allocations against `owner` (and its subsystem) until the guard
/// drops. Nests like [`super::enter`].
pub fn enter_owner(owner: &'static MemoryOwner) -> super::MemoryScope {
    super::enter_tagged(owner.subsystem, owner.id())
}

/// Runs `f` inside an owner scope ([`enter_owner`]).
pub fn scoped_owner<R>(owner: &'static MemoryOwner, f: impl FnOnce() -> R) -> R {
    let _scope = enter_owner(owner);
    f()
}

/// The owner this thread is working for, by name (`None` outside every owner scope).
#[must_use]
pub fn current_owner() -> Option<&'static str> {
    let id = super::current_owner_tag();
    OWNER_INFO
        .get(usize::from(id))
        .and_then(OnceLock::get)
        .map(|(_, name)| *name)
}

/// One owner's heap use ([`owner_snapshot`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerMemory {
    /// The owner's name ([`MemoryOwner::name`]).
    pub name: &'static str,
    /// The subsystem its bytes also count against.
    pub subsystem: Subsystem,
    /// Its counters, as a subsystem's.
    pub memory: SubsystemMemory,
}

impl Default for OwnerMemory {
    fn default() -> Self {
        Self {
            name: "",
            subsystem: Subsystem::Unscoped,
            memory: SubsystemMemory::default(),
        }
    }
}

/// Every registered owner's heap use at (nearly) one moment. Allocation-free to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerSnapshot {
    len: usize,
    rows: [OwnerMemory; MAX_OWNERS],
}

impl Default for OwnerSnapshot {
    fn default() -> Self {
        Self {
            len: 0,
            rows: [OwnerMemory::default(); MAX_OWNERS],
        }
    }
}

impl OwnerSnapshot {
    /// A snapshot holding `rows` (the first [`MAX_OWNERS`]), for reports built from recorded data
    /// and tests.
    #[must_use]
    pub fn from_rows(rows: &[OwnerMemory]) -> Self {
        let mut out = Self::default();
        for row in rows.iter().take(MAX_OWNERS) {
            out.rows[out.len] = *row;
            out.len += 1;
        }
        out
    }

    /// The owners, in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &OwnerMemory> {
        self.rows[..self.len].iter()
    }

    /// Registered owners.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no owner was ever entered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The owner called `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&OwnerMemory> {
        self.iter().find(|o| o.name == name)
    }

    /// Live bytes of `name` (0 when it never registered).
    #[must_use]
    pub fn live_bytes(&self, name: &str) -> u64 {
        self.get(name).map_or(0, |o| o.memory.live_bytes)
    }

    /// Keeps, per owner, the smaller live bytes and live allocations of `self` and `other` (the
    /// floor over a series of samples); owners `other` has and `self` lacks are added.
    pub fn floor_with(&mut self, other: &Self) {
        for row in other.iter() {
            match self.rows[..self.len].iter_mut().find(|o| o.name == row.name) {
                Some(mine) => {
                    mine.memory.live_bytes = mine.memory.live_bytes.min(row.memory.live_bytes);
                    mine.memory.live_allocations =
                        mine.memory.live_allocations.min(row.memory.live_allocations);
                }
                None if self.len < MAX_OWNERS => {
                    self.rows[self.len] = *row;
                    self.len += 1;
                }
                None => {}
            }
        }
    }
}

/// Reads every registered owner's counters. Allocation-free.
#[must_use]
pub fn owner_snapshot() -> OwnerSnapshot {
    let mut out = OwnerSnapshot::default();
    let next = usize::from(NEXT_OWNER.load(Ordering::Acquire)).min(MAX_OWNERS);
    for id in 1..next {
        let Some((subsystem, name)) = OWNER_INFO[id].get().copied() else {
            continue;
        };
        let s = &OWNER_LEDGER[id];
        let live_bytes = s.live_bytes.load(Ordering::Relaxed);
        out.rows[out.len] = OwnerMemory {
            name,
            subsystem,
            memory: SubsystemMemory {
                live_bytes,
                peak_bytes: s.peak_bytes.load(Ordering::Relaxed).max(live_bytes),
                live_allocations: s.live_allocations.load(Ordering::Relaxed),
                total_allocations: s.total_allocations.load(Ordering::Relaxed),
                total_bytes: s.total_bytes.load(Ordering::Relaxed),
            },
        };
        out.len += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{current, enter, record_alloc, record_free, record_resize, snapshot};

    static TEST_OWNER: MemoryOwner = MemoryOwner::new(Subsystem::Navigation, "navigation.test_owner");
    static TEST_OWNER_TWIN: MemoryOwner = MemoryOwner::new(Subsystem::Navigation, "navigation.test_owner");
    static NESTED: MemoryOwner = MemoryOwner::new(Subsystem::Navigation, "navigation.test_nested");

    #[test]
    fn owner_scopes_set_the_subsystem_and_clear_on_a_plain_scope() {
        assert_eq!(current_owner(), None);
        {
            let _o = enter_owner(&TEST_OWNER);
            assert_eq!(current(), Subsystem::Navigation);
            assert_eq!(current_owner(), Some("navigation.test_owner"));
            {
                let _n = enter_owner(&NESTED);
                assert_eq!(current_owner(), Some("navigation.test_nested"));
            }
            assert_eq!(current_owner(), Some("navigation.test_owner"));
            {
                let _plain = enter(Subsystem::Assets);
                assert_eq!(
                    current_owner(),
                    None,
                    "another subsystem's scope is not the owner's"
                );
            }
            {
                let _same = enter(Subsystem::Navigation);
                assert_eq!(
                    current_owner(),
                    Some("navigation.test_owner"),
                    "the owner's own subsystem keeps it"
                );
            }
            assert_eq!(current_owner(), Some("navigation.test_owner"));
        }
        assert_eq!(current_owner(), None);
        assert_eq!(current(), Subsystem::Unscoped);
        // Two statics with one name share the row.
        assert_eq!(TEST_OWNER.id(), TEST_OWNER_TWIN.id());
        assert_ne!(TEST_OWNER.id(), NESTED.id());
    }

    #[test]
    fn owner_bytes_count_against_the_owner_and_its_subsystem() {
        // Driven through the ledger functions (no global allocator in this binary). The
        // Navigation slot is used by no other test of this crate.
        let id = TEST_OWNER.id();
        assert_ne!(id, 0);
        let tag = Subsystem::Navigation as u8;
        let before_owner = owner_snapshot().live_bytes("navigation.test_owner");
        let before_sub = snapshot().subsystem(Subsystem::Navigation).live_bytes;
        record_alloc(tag, id, 300);
        record_resize(tag, id, 300, 1000);
        let mid = owner_snapshot();
        assert_eq!(mid.live_bytes("navigation.test_owner"), before_owner + 1000);
        let row = mid.get("navigation.test_owner").expect("registered");
        assert_eq!(row.subsystem, Subsystem::Navigation);
        assert!(row.memory.peak_bytes >= row.memory.live_bytes);
        assert_eq!(
            snapshot().subsystem(Subsystem::Navigation).live_bytes,
            before_sub + 1000
        );
        record_free(tag, id, 1000);
        assert_eq!(owner_snapshot().live_bytes("navigation.test_owner"), before_owner);
        assert_eq!(snapshot().subsystem(Subsystem::Navigation).live_bytes, before_sub);
    }

    #[test]
    fn the_floor_keeps_the_smaller_sample_and_adds_new_owners() {
        let row = |name, live| OwnerMemory {
            name,
            subsystem: Subsystem::Render,
            memory: SubsystemMemory {
                live_bytes: live,
                live_allocations: live / 10,
                ..SubsystemMemory::default()
            },
        };
        let mut a = OwnerSnapshot::default();
        a.rows[0] = row("render.a", 500);
        a.len = 1;
        let mut b = OwnerSnapshot::default();
        b.rows[0] = row("render.a", 300);
        b.rows[1] = row("render.b", 70);
        b.len = 2;
        a.floor_with(&b);
        assert_eq!(a.live_bytes("render.a"), 300);
        assert_eq!(a.live_bytes("render.b"), 70);
        assert_eq!(a.len(), 2);
        assert_eq!(a.live_bytes("render.none"), 0);
    }
}
