// SPDX-License-Identifier: MPL-2.0
//! Process-wide counters for the animation hot-path allocation tests.
//!
//! Each counter increments when that allocating path runs. A reused buffer does not
//! increment it. The counters are relaxed and shared by every test in one process, so a
//! measurement holds [`lock_for_test`] for the whole delta.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());

static BIND_POSE_BUILDS: AtomicUsize = AtomicUsize::new(0);
static GLOBALS_FILLS: AtomicUsize = AtomicUsize::new(0);
static JOINT_INDEX_CALLS: AtomicUsize = AtomicUsize::new(0);
static MASK_RESOLVES: AtomicUsize = AtomicUsize::new(0);
static CURVE_NAME_CLONES: AtomicUsize = AtomicUsize::new(0);
static SET_FLOAT_ALLOCS: AtomicUsize = AtomicUsize::new(0);
static SYNC_CLIP_STRINGS: AtomicUsize = AtomicUsize::new(0);
static SPRING_BUFFER_ALLOCS: AtomicUsize = AtomicUsize::new(0);
static NODE_MORPH_DENSE_ALLOCS: AtomicUsize = AtomicUsize::new(0);
static ROOT_MOTION_REST_WALKS: AtomicUsize = AtomicUsize::new(0);
static PHYSICS_SCRATCH_ALLOCS: AtomicUsize = AtomicUsize::new(0);

fn load(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::Relaxed)
}

pub(crate) fn add(counter: &AtomicUsize) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Serialize hot-path measurements that share the process counters.
#[doc(hidden)]
pub fn lock_for_test() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Times [`crate::Skeleton::bind_pose_pose`] built a fresh pose.
#[doc(hidden)]
#[must_use]
pub fn bind_pose_builds() -> usize {
    load(&BIND_POSE_BUILDS)
}

pub(crate) fn count_bind_pose_build() {
    add(&BIND_POSE_BUILDS);
}

/// Times [`crate::Skeleton::globals`] (and the in-place fill) built a full joint-matrix buffer.
#[doc(hidden)]
#[must_use]
pub fn globals_fills() -> usize {
    load(&GLOBALS_FILLS)
}

pub(crate) fn count_globals_fill() {
    add(&GLOBALS_FILLS);
}

/// Times [`crate::Skeleton::joint_index`] walked the joint-name list.
#[doc(hidden)]
#[must_use]
pub fn joint_index_calls() -> usize {
    load(&JOINT_INDEX_CALLS)
}

pub(crate) fn count_joint_index() {
    add(&JOINT_INDEX_CALLS);
}

/// Times a layer mask was resolved from joint names into a weight vector.
#[doc(hidden)]
#[must_use]
pub fn mask_resolves() -> usize {
    load(&MASK_RESOLVES)
}

pub(crate) fn count_mask_resolve() {
    add(&MASK_RESOLVES);
}

/// Times a curve name was copied onto the heap during sampling or recording.
#[doc(hidden)]
#[must_use]
pub fn curve_name_clones() -> usize {
    load(&CURVE_NAME_CLONES)
}

pub(crate) fn count_curve_name_clone() {
    add(&CURVE_NAME_CLONES);
}

/// Times [`crate::params::ParameterSet::set_float`] allocated a new map key.
#[doc(hidden)]
#[must_use]
pub fn set_float_allocs() -> usize {
    load(&SET_FLOAT_ALLOCS)
}

pub(crate) fn count_set_float_alloc() {
    add(&SET_FLOAT_ALLOCS);
}

/// Times a sync group allocated a clip-name `String` for a candidate or a leader.
#[doc(hidden)]
#[must_use]
pub fn sync_clip_strings() -> usize {
    load(&SYNC_CLIP_STRINGS)
}

pub(crate) fn count_sync_clip_string() {
    add(&SYNC_CLIP_STRINGS);
}

/// Times the spring update allocated a skeleton-sized or per-substep buffer.
#[doc(hidden)]
#[must_use]
pub fn spring_buffer_allocs() -> usize {
    load(&SPRING_BUFFER_ALLOCS)
}

pub(crate) fn count_spring_buffer() {
    add(&SPRING_BUFFER_ALLOCS);
}

/// Times per-node morph apply allocated a dense weight vector.
#[doc(hidden)]
#[must_use]
pub fn node_morph_dense_allocs() -> usize {
    load(&NODE_MORPH_DENSE_ALLOCS)
}

pub(crate) fn count_node_morph_dense() {
    add(&NODE_MORPH_DENSE_ALLOCS);
}

/// Times a warp recomputed the root motion still to come in its window.
#[doc(hidden)]
#[must_use]
pub fn root_motion_rest_walks() -> usize {
    load(&ROOT_MOTION_REST_WALKS)
}

pub(crate) fn count_root_motion_rest() {
    add(&ROOT_MOTION_REST_WALKS);
}

/// Times a ragdoll blend allocated a full joint-matrix buffer.
#[doc(hidden)]
#[must_use]
pub fn physics_scratch_allocs() -> usize {
    load(&PHYSICS_SCRATCH_ALLOCS)
}

pub(crate) fn count_physics_scratch() {
    add(&PHYSICS_SCRATCH_ALLOCS);
}
