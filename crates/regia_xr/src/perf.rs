// SPDX-License-Identifier: MPL-2.0
//! Counters the XR performance tests read.
//!
//! A frame records the copies, texture views, bind groups, uniform writes,
//! draw-list clones and cached meshes it built. The tests fail when a later
//! frame does that work again. Each counter is a relaxed atomic.

use std::sync::atomic::{AtomicU64, Ordering};

fn load(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

static DISPLAY_COPIES: AtomicU64 = AtomicU64::new(0);
static DISPLAY_SUBMITS: AtomicU64 = AtomicU64::new(0);
static TEXTURE_VIEWS: AtomicU64 = AtomicU64::new(0);
static QUAD_BIND_GROUPS: AtomicU64 = AtomicU64::new(0);
static QUAD_UNIFORM_WRITES: AtomicU64 = AtomicU64::new(0);
static OBJECT_UNIFORM_WRITES: AtomicU64 = AtomicU64::new(0);
static OBJECT_UNIFORM_BYTES: AtomicU64 = AtomicU64::new(0);
static SCENE_CLONES: AtomicU64 = AtomicU64::new(0);
static MESH_ENTRIES: AtomicU64 = AtomicU64::new(0);

/// Zeroes every counter.
pub fn reset() {
    for counter in [
        &DISPLAY_COPIES,
        &DISPLAY_SUBMITS,
        &TEXTURE_VIEWS,
        &QUAD_BIND_GROUPS,
        &QUAD_UNIFORM_WRITES,
        &OBJECT_UNIFORM_WRITES,
        &OBJECT_UNIFORM_BYTES,
        &SCENE_CLONES,
        &MESH_ENTRIES,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

/// Eye-to-display copies since [`reset`].
#[must_use]
pub fn display_copies() -> u64 {
    load(&DISPLAY_COPIES)
}

/// Display-path command submissions since [`reset`].
#[must_use]
pub fn display_submits() -> u64 {
    load(&DISPLAY_SUBMITS)
}

/// Eye, display and mirror `create_view` calls since [`reset`].
///
/// A clone of a view that was already created is not counted.
#[must_use]
pub fn texture_views() -> u64 {
    load(&TEXTURE_VIEWS)
}

/// Quad bind groups created since [`reset`].
#[must_use]
pub fn quad_bind_groups() -> u64 {
    load(&QUAD_BIND_GROUPS)
}

/// Quad uniform `write_buffer` calls since [`reset`].
#[must_use]
pub fn quad_uniform_writes() -> u64 {
    load(&QUAD_UNIFORM_WRITES)
}

/// Multiview object-uniform `write_buffer` calls since [`reset`].
#[must_use]
pub fn object_uniform_writes() -> u64 {
    load(&OBJECT_UNIFORM_WRITES)
}

/// Bytes of those object-uniform writes since [`reset`].
#[must_use]
pub fn object_uniform_bytes() -> u64 {
    load(&OBJECT_UNIFORM_BYTES)
}

/// `FrameScene` clones handed to an eye since [`reset`].
#[must_use]
pub fn scene_clones() -> u64 {
    load(&SCENE_CLONES)
}

/// Cached multiview meshes after the last `StereoForward::render`.
#[must_use]
pub fn mesh_entries() -> u64 {
    load(&MESH_ENTRIES)
}

pub(crate) fn display_copy() {
    add(&DISPLAY_COPIES, 1);
}

pub(crate) fn display_submit() {
    add(&DISPLAY_SUBMITS, 1);
}

pub(crate) fn texture_view() {
    add(&TEXTURE_VIEWS, 1);
}

pub(crate) fn quad_bind_group() {
    add(&QUAD_BIND_GROUPS, 1);
}

pub(crate) fn quad_uniform_write() {
    add(&QUAD_UNIFORM_WRITES, 1);
}

pub(crate) fn object_uniform_write(bytes: u64) {
    add(&OBJECT_UNIFORM_WRITES, 1);
    add(&OBJECT_UNIFORM_BYTES, bytes);
}

pub(crate) fn scene_cloned() {
    add(&SCENE_CLONES, 1);
}

pub(crate) fn set_mesh_entries(n: u64) {
    MESH_ENTRIES.store(n, Ordering::Relaxed);
}
