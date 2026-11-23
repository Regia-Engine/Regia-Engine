// SPDX-License-Identifier: MPL-2.0
//! Interior counters for the solver's scratch growth and visit counts.
//!
//! A [`Tally`] compares equal to every other tally, so it can sit on a `PartialEq` geometry type
//! without the count becoming part of the geometry. It is `Send + Sync` (an atomic), so a
//! [`crate::FluidWorld`] that owns one stays a Bevy resource.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// A relaxed counter. `PartialEq` is always true: the count is a measurement, not state.
#[derive(Debug, Default)]
pub(crate) struct Tally(AtomicU64);

impl Clone for Tally {
    fn clone(&self) -> Self {
        Self(AtomicU64::new(self.get()))
    }
}

impl PartialEq for Tally {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Tally {
    /// The current count.
    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Adds `n`.
    pub(crate) fn add(&self, n: u64) {
        if n != 0 {
            self.0.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Replaces the count.
    pub(crate) fn set(&self, n: u64) {
        self.0.store(n, Ordering::Relaxed);
    }
}

/// A `Mutex` scratch buffer that clones by locking. `FluidSim` stays `Clone` and `Sync`.
#[derive(Debug)]
pub(crate) struct Scratch<T>(Mutex<T>);

impl<T> Scratch<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<T: Default> Default for Scratch<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: Clone> Clone for Scratch<T> {
    fn clone(&self) -> Self {
        Self::new(self.lock().clone())
    }
}

/// Grows `v` to `len` when its capacity is short, and counts that growth.
pub(crate) fn fit<T: Clone>(v: &mut Vec<T>, len: usize, fill: T, grows: &Tally) {
    if v.capacity() < len {
        grows.add(1);
    }
    v.resize(len, fill);
}
