// SPDX-License-Identifier: MPL-2.0
//! The render graph's transient texture pool (ADR-2018).
//!
//! A [`TransientPool`] hands out textures keyed by [`TextureDesc`]. Within one pool frame
//! ([`TransientPool::begin_frame`]) every [`TransientPool::acquire`] returns a texture no earlier
//! acquire of that frame holds, and the same sequence of acquires gets the same textures on
//! every frame: entries are searched in creation order and the first free one with an equal
//! description wins. So a frame whose requests do not change creates nothing and allocates
//! nothing on the heap (the handles are reference counted), and what a texture held at the end
//! of one frame is still there at the start of the next, exactly as with a texture the frame
//! kept in a field of its own.
//!
//! An entry not acquired in the last [`TransientPool::idle_frames`] pool frames is dropped at
//! the next [`TransientPool::begin_frame`] (a resize, a format change). Pool frames count the
//! frames the pool was used in, not the caller's clock, so a render-to-texture view drawn every
//! tenth frame keeps its textures. Each created texture gets a new
//! [`TransientTexture::generation`], so a bind group cached against a pooled view can key on
//! it and never outlive the texture it names.
//!
//! An acquire may carry a slot key ([`TransientPool::acquire_keyed`], ADR-7877): it is served only
//! by an entry of the same key, so two images with one description that must not share a texture
//! (the two HDR scratch copies) stay distinct. Unkeyed acquires ([`TransientPool::acquire`], key 0)
//! never get a keyed entry. [`TransientPool::keep_latest`] holds the latest texture of a key
//! through pool frames that do not acquire it (an image a cache prewarms ahead of the frame that
//! draws it).

use std::sync::atomic::{AtomicU64, Ordering};

use super::TextureDesc;
use crate::TrackedDevice;

/// Process-wide generation counter: two pools never hand out the same generation.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// One texture handed out by [`TransientPool::acquire`] (cheap to clone: the handles are
/// reference counted).
#[derive(Debug, Clone)]
pub struct TransientTexture {
    /// The texture.
    pub texture: wgpu::Texture,
    /// Its default view (every layer, every mip).
    pub view: wgpu::TextureView,
    /// Unique per created texture (process wide, never 0): a cache keyed by it is rebuilt when
    /// the pool replaces the texture.
    pub generation: u64,
}

struct PoolEntry {
    desc: TextureDesc,
    /// The slot key it was created under (0: unkeyed).
    key: u32,
    /// The label it was created with.
    label: String,
    texture: TransientTexture,
    last_used: u64,
    in_use: bool,
    /// Acquired in this pool frame and then released.
    released: bool,
}

/// [`TransientPool::stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransientPoolStats {
    /// Textures the pool holds now.
    pub textures: usize,
    /// Their bytes (every mip, layer and sample).
    pub bytes: u64,
    /// Textures acquired in the current pool frame.
    pub in_use: usize,
    /// Textures created since the pool was made.
    pub created: u64,
    /// Acquires served by an existing texture since the pool was made.
    pub reused: u64,
    /// Textures dropped as idle since the pool was made.
    pub evicted: u64,
    /// Acquires served by a texture another acquire of the same pool frame had released: the
    /// creates transient aliasing saved ([`TransientPool::set_aliasing`], ADR-2074).
    pub aliased: u64,
}

/// One texture of [`TransientPool::entry_info`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransientEntryInfo {
    /// Its description.
    pub desc: TextureDesc,
    /// The slot key it was created under (0: an unkeyed acquire, [`TransientPool::acquire_keyed`]).
    pub key: u32,
    /// The label it was created with.
    pub label: String,
    /// [`TransientTexture::generation`].
    pub generation: u64,
    /// Bytes (every mip, layer and sample).
    pub bytes: u64,
    /// Acquired, or held by [`TransientPool::keep_latest`], in the current pool frame.
    pub used_this_frame: bool,
}

/// Textures keyed by [`TextureDesc`], reused frame after frame (see the module docs).
pub struct TransientPool {
    entries: Vec<PoolEntry>,
    /// The caller's frame of the current pool frame (`None` before the first).
    frame: Option<u64>,
    /// Pool frames started so far.
    tick: u64,
    idle_frames: u64,
    created: u64,
    reused: u64,
    evicted: u64,
    aliased: u64,
    aliasing: bool,
}

impl Default for TransientPool {
    fn default() -> Self {
        Self::new(Self::DEFAULT_IDLE_FRAMES)
    }
}

impl TransientPool {
    /// Frames an unused texture stays in a [`Self::default`] pool (the frame's size-keyed
    /// targets use the same number).
    pub const DEFAULT_IDLE_FRAMES: u64 = 4;

    /// An empty pool that drops a texture no frame acquired for `idle_frames` frames.
    #[must_use]
    pub fn new(idle_frames: u64) -> Self {
        Self {
            entries: Vec::new(),
            frame: None,
            tick: 0,
            idle_frames,
            created: 0,
            reused: 0,
            evicted: 0,
            aliased: 0,
            aliasing: false,
        }
    }

    /// Whether a graph run releases each transient after its last reader, so a later transient
    /// of the same description reuses its texture ([`crate::RenderGraph::alias_plan`], ADR-2074).
    /// Off by default.
    pub fn set_aliasing(&mut self, on: bool) {
        self.aliasing = on;
    }

    /// See [`Self::set_aliasing`].
    #[must_use]
    pub fn aliasing(&self) -> bool {
        self.aliasing
    }

    /// Pool frames an unused texture stays.
    #[must_use]
    pub fn idle_frames(&self) -> u64 {
        self.idle_frames
    }

    /// Starts a pool frame for the caller's frame `frame`: every texture is free again, and the
    /// ones not acquired in the last [`Self::idle_frames`] pool frames are dropped. Returns
    /// `true` when it dropped any. A `frame` equal to the current one starts no new pool frame
    /// and only frees the textures (a second view of the same renderer frame).
    pub fn begin_frame(&mut self, frame: u64) -> bool {
        if self.frame != Some(frame) {
            self.frame = Some(frame);
            self.tick += 1;
        }
        let tick = self.tick;
        let idle = self.idle_frames;
        let before = self.entries.len();
        self.entries
            .retain(|entry| entry.last_used.saturating_add(idle) >= tick);
        let dropped = before - self.entries.len();
        self.evicted += dropped as u64;
        for entry in &mut self.entries {
            entry.in_use = false;
            entry.released = false;
        }
        dropped > 0
    }

    /// A texture matching `desc` that no other acquire of this pool frame holds: the first free
    /// one in creation order, or a new one labelled `label` (created through `device`, so the
    /// hitch counters see it). Never a keyed entry ([`Self::acquire_keyed`]).
    pub fn acquire(&mut self, device: &TrackedDevice, desc: &TextureDesc, label: &str) -> TransientTexture {
        self.acquire_keyed(device, desc, 0, label)
    }

    /// [`Self::acquire`] under the slot key `key`: only an entry created under the same key
    /// serves it (ADR-7877), so two images of one description get two textures every frame
    /// even when one of them is not acquired in some frames. Key 0 is [`Self::acquire`].
    pub fn acquire_keyed(
        &mut self,
        device: &TrackedDevice,
        desc: &TextureDesc,
        key: u32,
        label: &str,
    ) -> TransientTexture {
        let tick = self.tick;
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| !entry.in_use && entry.key == key && entry.desc == *desc)
        {
            entry.in_use = true;
            entry.last_used = tick;
            self.reused += 1;
            if entry.released {
                self.aliased += 1;
            }
            return entry.texture.clone();
        }
        let texture = device.create_texture(&desc.descriptor(Some(label)));
        crate::resource_inspector::track(label, &texture);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let texture = TransientTexture {
            texture,
            view,
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
        };
        self.created += 1;
        self.entries.push(PoolEntry {
            desc: desc.clone(),
            key,
            label: label.to_string(),
            texture: texture.clone(),
            last_used: tick,
            in_use: true,
            released: false,
        });
        texture
    }

    /// The entry of slot key `key` used most recently (the last created among equals): the
    /// texture its owner drew into last.
    fn latest_index(&self, key: u32) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.key == key && best.is_none_or(|b| entry.last_used >= self.entries[b].last_used) {
                best = Some(index);
            }
        }
        best
    }

    /// Holds the latest texture of slot key `key` ([`Self::latest`]) for this pool frame without
    /// handing it out: it is not dropped as idle while a caller keeps it, and a later acquire of
    /// the same key still gets it. Older textures of the key (another size or format) age out as
    /// usual. Key 0 holds nothing. Returns whether a texture was held.
    pub fn keep_latest(&mut self, key: u32) -> bool {
        if key == 0 {
            return false;
        }
        let tick = self.tick;
        match self.latest_index(key) {
            Some(index) => {
                self.entries[index].last_used = tick;
                true
            }
            None => false,
        }
    }

    /// The latest texture of slot key `key` (see [`Self::keep_latest`]), if the pool holds one.
    #[must_use]
    pub fn latest(&self, key: u32) -> Option<&TransientTexture> {
        if key == 0 {
            return None;
        }
        self.latest_index(key).map(|index| &self.entries[index].texture)
    }

    /// Frees the texture of `generation` for the rest of this pool frame: a later acquire of
    /// the same description may get it (for a pass whose last reader has recorded). Returns
    /// whether the pool held it.
    pub fn release(&mut self, generation: u64) -> bool {
        match self
            .entries
            .iter_mut()
            .find(|entry| entry.texture.generation == generation)
        {
            Some(entry) => {
                entry.in_use = false;
                entry.released = true;
                true
            }
            None => false,
        }
    }

    /// Drops every texture now (device loss, a renderer reset).
    pub fn clear(&mut self) {
        self.evicted += self.entries.len() as u64;
        self.entries.clear();
    }

    /// What the pool holds and has done.
    #[must_use]
    pub fn stats(&self) -> TransientPoolStats {
        TransientPoolStats {
            textures: self.entries.len(),
            bytes: self.entries.iter().map(|entry| entry.desc.bytes()).sum(),
            in_use: self.entries.iter().filter(|entry| entry.in_use).count(),
            created: self.created,
            reused: self.reused,
            evicted: self.evicted,
            aliased: self.aliased,
        }
    }

    /// The description and generation of every texture the pool holds, in creation order.
    #[must_use]
    pub fn entries(&self) -> Vec<(TextureDesc, u64)> {
        self.entries
            .iter()
            .map(|entry| (entry.desc.clone(), entry.texture.generation))
            .collect()
    }

    /// Every texture the pool holds with its slot key and label, in creation order.
    #[must_use]
    pub fn entry_info(&self) -> Vec<TransientEntryInfo> {
        let tick = self.tick;
        self.entries
            .iter()
            .map(|entry| TransientEntryInfo {
                desc: entry.desc.clone(),
                key: entry.key,
                label: entry.label.clone(),
                generation: entry.texture.generation,
                bytes: entry.desc.bytes(),
                used_this_frame: entry.in_use || (self.frame.is_some() && entry.last_used == tick),
            })
            .collect()
    }
}
