// SPDX-License-Identifier: MPL-2.0
//! Pipelines shared by every user of one [`crate::RenderDevice`] (ADR-195 "Warm coverage").
//!
//! Subsystems that own their GPU state per instance (a terrain, an effects context) used to
//! compile their pipelines per instance: a map's second terrain, a terrain rebuilt for a new
//! map, or an effects context created after the warm compiled everything again, on whatever
//! thread made the instance (usually the frame thread). A [`DevicePipelines`] keeps each
//! pipeline (or any cheap-to-clone GPU object: a shader module, a set of pipelines) under a
//! caller-chosen key for the device's lifetime, so a warm job that builds an instance on a
//! [`crate::PipelineWarmQueue`] worker leaves every later instance with nothing to compile.
//!
//! Two callers asking for the same key at once share one build: the second waits for the
//! first. The registry lives in the [`crate::RenderDevice`] (all clones share it) and is
//! dropped with the last clone, so it never keeps a device alive.
//!
//! Keys are the caller's: include everything the object depends on (shader source hash,
//! target formats, entry points, the device features that change a layout). Bind group layouts
//! with equal entries are the same layout on a device (wgpu deduplicates them), so a pipeline
//! built for one instance's layouts draws with another instance's bind groups.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

type Slot = Arc<dyn Any + Send + Sync>;

/// GPU objects shared by every user of one device ([`crate::RenderDevice::shared_pipelines`]).
#[derive(Default)]
pub struct DevicePipelines {
    slots: Mutex<HashMap<(TypeId, u64), Slot>>,
}

impl std::fmt::Debug for DevicePipelines {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevicePipelines")
            .field("entries", &self.len())
            .finish()
    }
}

impl DevicePipelines {
    /// The object stored under `key`, built by `build` (on this thread) if nothing is stored
    /// yet. A build of the same key running on another thread is waited for, never repeated.
    pub fn get_or_create<T: Clone + Send + Sync + 'static>(&self, key: u64, build: impl FnOnce() -> T) -> T {
        self.cell::<T>(key).get_or_init(build).clone()
    }

    /// The object stored under `key`, if it has been built.
    #[must_use]
    pub fn get<T: Clone + Send + Sync + 'static>(&self, key: u64) -> Option<T> {
        let slot = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(TypeId::of::<T>(), key))
            .cloned()?;
        slot.downcast_ref::<OnceLock<T>>()?.get().cloned()
    }

    /// Entries stored (built or being built).
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether nothing is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn cell<T: Send + Sync + 'static>(&self, key: u64) -> Arc<OnceLock<T>> {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = slots
            .entry((TypeId::of::<T>(), key))
            .or_insert_with(|| Arc::new(OnceLock::<T>::new()) as Slot)
            .clone();
        drop(slots);
        // The entry was made for `T` (the type id is part of the key).
        slot.downcast::<OnceLock<T>>()
            .unwrap_or_else(|_| Arc::new(OnceLock::new()))
    }
}

/// A stable 64-bit key of `value` for [`DevicePipelines`] (FNV-1a over its `Hash` output:
/// the same in every process, unlike `std`'s randomised hasher).
#[must_use]
pub fn pipeline_key(value: &impl Hash) -> u64 {
    struct Fnv(u64);
    impl Hasher for Fnv {
        fn finish(&self) -> u64 {
            self.0
        }
        fn write(&mut self, bytes: &[u8]) {
            for &b in bytes {
                self.0 ^= u64::from(b);
                self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    value.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn builds_once_per_key_and_type() {
        let reg = DevicePipelines::default();
        let builds = AtomicU32::new(0);
        let make = |v: u32| {
            builds.fetch_add(1, Ordering::Relaxed);
            v
        };
        assert_eq!(reg.get_or_create(1, || make(10)), 10);
        assert_eq!(reg.get_or_create(1, || make(11)), 10);
        assert_eq!(reg.get_or_create(2, || make(20)), 20);
        // Same key, another type: its own entry.
        assert_eq!(reg.get_or_create(1, || "s".to_string()), "s");
        assert_eq!(builds.load(Ordering::Relaxed), 2);
        assert_eq!(reg.get::<u32>(2), Some(20));
        assert_eq!(reg.get::<u32>(3), None);
        assert_eq!(reg.len(), 3);
    }

    #[test]
    fn concurrent_callers_share_one_build() {
        let reg = Arc::new(DevicePipelines::default());
        let builds = Arc::new(AtomicU32::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (reg, builds) = (Arc::clone(&reg), Arc::clone(&builds));
                std::thread::spawn(move || {
                    reg.get_or_create(7, || {
                        builds.fetch_add(1, Ordering::Relaxed);
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        Arc::new(5u8)
                    })
                })
            })
            .collect();
        for t in threads {
            assert_eq!(*t.join().expect("thread"), 5);
        }
        assert_eq!(builds.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn keys_are_stable() {
        assert_eq!(pipeline_key(&("terrain", 3u32)), pipeline_key(&("terrain", 3u32)));
        assert_ne!(pipeline_key(&("terrain", 3u32)), pipeline_key(&("terrain", 4u32)));
    }
}
