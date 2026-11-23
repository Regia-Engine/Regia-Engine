// SPDX-License-Identifier: MPL-2.0
//! Dynamic glyph atlas (`specs/18` § 2): rasterized glyphs packed into square pages — an R8
//! (alpha mask) atlas for monochrome glyphs and an RGBA8 sRGB atlas for colour emoji. Pages are
//! shelf-packed; when all pages of a kind are full, the least-recently-used page is wiped and
//! reused (its glyphs re-rasterize on demand, and stale placements are dropped).
//!
//! Each page belongs to one font size bucket ([`size_bucket`], ADR-7208): a glyph is packed only
//! into pages of its bucket, so shelves hold glyphs of similar height and a burst of large text
//! recycles large-text pages rather than the body text's. The buckets share the kind's texture
//! array (one GPU binding per kind); a recycled page takes the bucket of the glyph that needed it.
//!
//! Between [`GlyphAtlases::begin_frame`] and [`GlyphAtlases::end_frame`] (the UI extract, ADR-8832)
//! a page that served a glyph this frame is pinned: quads already emitted point into it, so it is
//! never the page recycled. When every page is pinned the glyph is refused (not drawn this frame)
//! and counted in [`GlyphAtlases::thrash`], instead of overwriting a glyph the frame already drew.

use std::collections::{HashMap, VecDeque};

use cosmic_text as ct;
use regia_app::prelude::*;

/// Page side in pixels (square pages).
pub const PAGE_SIZE: u32 = 512;
/// How many pages of each kind exist at once (the GPU binds them as texture-array layers).
pub const MAX_PAGES: u32 = 8;
/// Upper font sizes (physical px, inclusive) of the size buckets but the last: up to 16 px,
/// up to 32 px, up to 64 px, and larger (ADR-7208).
pub const SIZE_BUCKET_LIMITS: [f32; 3] = [16.0, 32.0, 64.0];

/// The size bucket of a glyph rasterized at `font_px` physical px: 0 to
/// `SIZE_BUCKET_LIMITS.len()` (see [`SIZE_BUCKET_LIMITS`]).
pub fn size_bucket(font_px: f32) -> u8 {
    let n = SIZE_BUCKET_LIMITS
        .iter()
        .take_while(|limit| font_px > **limit)
        .count();
    u8::try_from(n).unwrap_or(u8::MAX)
}

/// Where a glyph lives: which atlas kind, which page, and its rect in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphPlace {
    /// Atlas kind: `0` = mono (R8), `1` = colour (RGBA8).
    pub kind: u8,
    /// Page index within the kind (the GPU texture-array layer).
    pub page: u32,
    /// Position and size within the page, in pixels: x, y, w, h.
    pub rect: [u32; 4],
}

/// One packed page.
struct Page {
    data: Vec<u8>,
    /// Shelf packing cursor: next free x, next shelf y, current shelf height.
    next: [u32; 3],
    /// The font size bucket whose glyphs this page holds ([`size_bucket`]).
    bucket: u8,
    /// The frame epoch in which this page last served a glyph (`0`: never inside a frame).
    used: u64,
}

/// One atlas kind (mono or colour).
struct AtlasKind {
    pages: Vec<Page>,
    lru: VecDeque<u32>,
    bytes_per_pixel: usize,
}

/// What [`AtlasKind::allocate`] did.
enum Alloc {
    /// Packed at `(page, rect)`; `true` when an old page was wiped for it.
    Placed(u32, [u32; 4], bool),
    /// Every page is full and pinned by this frame: nothing may be recycled.
    Pinned,
    /// The rect cannot fit a page at all.
    Refused,
}

impl AtlasKind {
    fn new_page(&mut self, bucket: u8) -> u32 {
        let bytes = (PAGE_SIZE * PAGE_SIZE) as usize * self.bytes_per_pixel;
        self.pages.push(Page {
            data: vec![0; bytes],
            next: [0, 0, 0],
            bucket,
            used: 0,
        });
        let idx = u32::try_from(self.pages.len() - 1).unwrap_or(u32::MAX);
        self.lru.push_back(idx);
        idx
    }

    /// Shelf-packs a `w×h` rect into a page of `bucket`, wiping the least recently used page (of
    /// any bucket) when no page of the bucket has room and no page is left to add. `frame` is the
    /// open frame's epoch: a page used in it is pinned and never wiped.
    fn allocate(&mut self, w: u32, h: u32, bucket: u8, frame: Option<u64>) -> Alloc {
        if w == 0 || h == 0 || w > PAGE_SIZE || h > PAGE_SIZE {
            return Alloc::Refused;
        }
        // Grow while we can.
        let room = |p: &Page| p.bucket == bucket && fits(p.next, w, h);
        if self.pages.len() < MAX_PAGES as usize && !self.pages.iter().any(room) {
            let idx = self.new_page(bucket);
            self.pages[idx as usize].next = [w + 1, 0, h];
            return Alloc::Placed(idx, [0, 0, w, h], false);
        }
        for p in 0..self.pages.len() {
            if !room(&self.pages[p]) {
                continue;
            }
            let Ok(index) = u32::try_from(p) else {
                return Alloc::Refused;
            };
            let page = &mut self.pages[p];
            let (x, y, shelf) = (page.next[0], page.next[1], page.next[2]);
            if x + w <= PAGE_SIZE {
                page.next[0] = x + w + 1;
                page.next[2] = shelf.max(h);
                return Alloc::Placed(index, [x, y, w, h], false);
            }
            let new_y = y + shelf + 1;
            page.next = [w + 1, new_y, h];
            return Alloc::Placed(index, [0, new_y, w, h], false);
        }
        // All pages full: recycle the least recently used one this frame has not drawn from.
        let pages = &self.pages;
        let pinned = |idx: u32| frame.is_some_and(|f| pages.get(idx as usize).is_some_and(|p| p.used == f));
        let Some(pos) = self.lru.iter().position(|&idx| !pinned(idx)) else {
            return Alloc::Pinned;
        };
        let Some(old) = self.lru.remove(pos) else {
            return Alloc::Pinned;
        };
        let page = &mut self.pages[old as usize];
        page.data.fill(0);
        page.next = [w + 1, 0, h];
        page.bucket = bucket;
        self.lru.push_back(old);
        Alloc::Placed(old, [0, 0, w, h], true)
    }

    /// A cache hit on `page`: it becomes the most recently used, and is pinned for the open frame.
    fn touch(&mut self, page: u32, frame: Option<u64>) {
        if let Some(pos) = self.lru.iter().position(|&p| p == page) {
            self.lru.remove(pos);
            self.lru.push_back(page);
        }
        self.pin(page, frame);
    }

    /// Pins `page` for the open frame (if any): a quad of this frame now samples it.
    fn pin(&mut self, page: u32, frame: Option<u64>) {
        if let (Some(frame), Some(p)) = (frame, self.pages.get_mut(page as usize)) {
            p.used = frame;
        }
    }
}

/// Premultiplies straight-alpha sRGB RGBA8 pixels in place (in linear light).
fn premultiply_srgb8(pixels: &mut [u8]) {
    use crate::color::{linear_to_srgb8, srgb_to_linear};
    for p in pixels.as_chunks_mut::<4>().0 {
        if p[3] == 255 {
            continue;
        }
        let a = f32::from(p[3]) / 255.0;
        for c in &mut p[..3] {
            *c = linear_to_srgb8(srgb_to_linear(*c) * a);
        }
    }
}

/// Whether a rect of `w×h` fits given a shelf cursor.
fn fits(next: [u32; 3], w: u32, h: u32) -> bool {
    let (x, y, shelf) = (next[0], next[1], next[2]);
    (x + w <= PAGE_SIZE && y + h <= PAGE_SIZE) || (y + shelf + 1 + h <= PAGE_SIZE)
}

/// The resource owning both atlases' CPU state and the glyph → placement maps.
#[derive(Resource, Default)]
pub struct GlyphAtlases {
    mono: AtlasKind,
    color: AtlasKind,
    placements: HashMap<ct::CacheKey, GlyphPlace>,
    offsets: HashMap<ct::CacheKey, [i32; 2]>,
    dirty_pages: Vec<(u8, u32)>,
    /// Pages recycled so far (cached glyph quads on a recycled page are stale).
    wipes: u64,
    /// The open frame's epoch ([`Self::begin_frame`]); `None` outside a frame.
    frame: Option<u64>,
    /// The last epoch handed out.
    epoch: u64,
    /// Glyphs refused because every page was pinned by its frame (ADR-8832).
    thrash: u64,
    /// Of those, the ones refused in the open frame.
    frame_thrash: u64,
    /// Frames that refused at least one glyph.
    thrash_frames: u64,
}

impl Default for AtlasKind {
    fn default() -> Self {
        Self {
            pages: Vec::new(),
            lru: VecDeque::new(),
            bytes_per_pixel: 1,
        }
    }
}

impl GlyphAtlases {
    /// Looks up (or rasterizes and packs) a glyph. `rasterize` produces the swash image on demand.
    /// Returns the placement plus the bitmap offset to apply at draw time
    /// (`offset = [placement.left, -placement.top]`).
    pub fn get_or_insert(
        &mut self,
        key: ct::CacheKey,
        rasterize: impl FnOnce() -> Option<ct::SwashImage>,
    ) -> Option<(GlyphPlace, [i32; 2])> {
        let frame = self.frame;
        if let Some(place) = self.placements.get(&key).copied() {
            self.kind_mut(place.kind).touch(place.page, frame);
            let offset = self.offsets.get(&key).copied().unwrap_or([0, 0]);
            return Some((place, offset));
        }
        let image = rasterize()?;
        let (w, h) = (image.placement.width, image.placement.height);
        if w == 0 || h == 0 {
            return None;
        }
        let kind_idx: u8 = match image.content {
            ct::SwashContent::Color => 1,
            _ => 0,
        };
        // A bitmap shorter than its placement claims is refused before it can take space or
        // recycle a page.
        let src_len = (w as usize)
            .checked_mul(h as usize)?
            .checked_mul(if kind_idx == 1 { 4 } else { 1 })?;
        if image.data.len() < src_len {
            return None;
        }
        {
            let kind = &mut self.kind_mut(kind_idx);
            kind.bytes_per_pixel = if kind_idx == 1 { 4 } else { 1 };
        }
        let bucket = size_bucket(f32::from_bits(key.font_size_bits));
        let (page, rect, wiped) = match self.kind_mut(kind_idx).allocate(w, h, bucket, frame) {
            Alloc::Placed(page, rect, wiped) => (page, rect, wiped),
            Alloc::Pinned => {
                self.thrash += 1;
                self.frame_thrash += 1;
                return None;
            }
            Alloc::Refused => return None,
        };
        self.kind_mut(kind_idx).pin(page, frame);
        if wiped {
            self.wipes += 1;
            // Placements on the recycled page are stale.
            self.placements
                .retain(|_, p| !(p.kind == kind_idx && p.page == page));
            let placements = &self.placements;
            self.offsets.retain(|k, _| placements.contains_key(k));
        }
        // Copy the bitmap into the page.
        {
            let kind = self.kind_mut(kind_idx);
            let bpp = kind.bytes_per_pixel;
            let src_bpp = if kind_idx == 1 { 4 } else { 1 };
            let page_data = &mut kind.pages[page as usize].data;
            let src: &[u8] = image.data.as_slice();
            for row in 0..h as usize {
                let src_row = row * src_bpp * w as usize;
                let dst_row = ((rect[1] as usize + row) * PAGE_SIZE as usize + rect[0] as usize) * bpp;
                let copy_w = w as usize * src_bpp;
                let src_slice = src.get(src_row..src_row + copy_w)?;
                let dst_slice = page_data.get_mut(dst_row..dst_row + copy_w)?;
                dst_slice.copy_from_slice(src_slice);
                if kind_idx == 1 {
                    // Colour glyphs are stored premultiplied (in linear light, sRGB-encoded) so
                    // bilinear filtering never bleeds the colour of transparent texels.
                    premultiply_srgb8(dst_slice);
                }
            }
        }
        let place = GlyphPlace {
            kind: kind_idx,
            page,
            rect,
        };
        let offset = [image.placement.left, -image.placement.top];
        self.placements.insert(key, place);
        self.offsets.insert(key, offset);
        let dirty = (kind_idx, page);
        if !self.dirty_pages.contains(&dirty) {
            self.dirty_pages.push(dirty);
        }
        Some((place, offset))
    }

    /// Opens a frame: until [`Self::end_frame`], every page that serves a glyph is pinned and never
    /// recycled, so the quads this frame already emitted keep sampling their own glyphs. Calling it
    /// again while a frame is open starts a new frame (the old pins lapse).
    pub fn begin_frame(&mut self) {
        self.epoch = self.epoch.wrapping_add(1).max(1);
        self.frame = Some(self.epoch);
        self.frame_thrash = 0;
    }

    /// Closes the frame opened by [`Self::begin_frame`]: pins lapse, and a frame that had to refuse
    /// glyphs is counted and reported (`tracing::warn` on the first such frame and then at every
    /// power of two, so sustained thrash does not flood the log). Returns the glyphs refused in it.
    pub fn end_frame(&mut self) -> u64 {
        let refused = std::mem::take(&mut self.frame_thrash);
        if self.frame.take().is_some() && refused > 0 {
            self.thrash_frames += 1;
            if self.thrash_frames.is_power_of_two() {
                tracing::warn!(
                    target: "regia_ui",
                    refused,
                    thrash_frames = self.thrash_frames,
                    thrash = self.thrash,
                    "glyph atlas full: {refused} glyphs not drawn this frame (every page holds glyphs \
                     the frame already drew; fewer sizes or sub-pixel bins need fewer pages)"
                );
            }
        }
        refused
    }

    /// A quad of the open frame samples `page` of `kind` without a lookup (a caller replays quads
    /// it cached, as the quick layer's shape cache does): the page becomes the most recently
    /// used and, inside a frame, is pinned like a page that served a glyph (ADR-8930).
    pub fn use_page(&mut self, kind: u8, page: u32) {
        let frame = self.frame;
        self.kind_mut(kind).touch(page, frame);
    }

    /// Glyphs refused so far because every page of their kind was pinned by the open frame (the
    /// frame needed more distinct glyphs than [`MAX_PAGES`] pages hold).
    pub fn thrash(&self) -> u64 {
        self.thrash
    }

    /// Frames that refused at least one glyph ([`Self::thrash`]).
    pub fn thrash_frames(&self) -> u64 {
        self.thrash_frames
    }

    /// Drops every placement on `page` of `kind` (after a wipe) so those glyphs re-rasterize.
    pub fn invalidate_page(&mut self, kind: u8, page: u32) {
        self.placements.retain(|_, p| !(p.kind == kind && p.page == page));
        let placements = &self.placements;
        self.offsets.retain(|k, _| placements.contains_key(k));
        self.dirty_pages.retain(|d| *d != (kind, page));
        self.dirty_pages.push((kind, page));
    }

    fn kind_mut(&mut self, kind: u8) -> &mut AtlasKind {
        if kind == 1 {
            &mut self.color
        } else {
            &mut self.mono
        }
    }

    /// How many atlas pages have been recycled (LRU) so far.
    pub fn wipes(&self) -> u64 {
        self.wipes
    }

    /// The font size bucket ([`size_bucket`]) page `page` of `kind` holds now.
    pub fn page_bucket(&self, kind: u8, page: u32) -> Option<u8> {
        let k = if kind == 1 { &self.color } else { &self.mono };
        k.pages.get(page as usize).map(|p| p.bucket)
    }

    /// Number of live pages of `kind`.
    pub fn page_count(&self, kind: u8) -> u32 {
        let k = if kind == 1 { &self.color } else { &self.mono };
        u32::try_from(k.pages.len()).unwrap_or(0)
    }

    /// Takes the pages whose pixels changed since the last upload.
    pub fn take_dirty_pages(&mut self) -> Vec<(u8, u32)> {
        std::mem::take(&mut self.dirty_pages)
    }

    /// Pixels of one page for GPU upload: `(data, bytes_per_pixel)`.
    pub fn page_data(&self, kind: u8, page: u32) -> Option<(&[u8], usize)> {
        let k = if kind == 1 { &self.color } else { &self.mono };
        k.pages
            .get(page as usize)
            .map(|p| (p.data.as_slice(), k.bytes_per_pixel))
    }

    /// Cached glyph count.
    pub fn len(&self) -> usize {
        self.placements.len()
    }

    /// True when no glyph is cached.
    pub fn is_empty(&self) -> bool {
        self.placements.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_image(w: u32, h: u32, color: bool) -> ct::SwashImage {
        let data = if color {
            vec![255u8; (w * h * 4) as usize]
        } else {
            vec![200u8; (w * h) as usize]
        };
        let mut image = ct::SwashImage::new();
        image.placement.width = w;
        image.placement.height = h;
        image.content = if color {
            ct::SwashContent::Color
        } else {
            ct::SwashContent::Mask
        };
        image.data = data;
        image
    }

    fn key(i: u32) -> ct::CacheKey {
        ct::CacheKey {
            font_id: ct::fontdb::ID::dummy(),
            glyph_id: (i % u32::from(u16::MAX)) as u16,
            font_size_bits: 18.0f32.to_bits(),
            x_bin: ct::SubpixelBin::Zero,
            y_bin: ct::SubpixelBin::Zero,
            font_weight: ct::fontdb::Weight::NORMAL,
            flags: ct::CacheKeyFlags::empty(),
        }
    }

    #[test]
    fn invalidate_page_drops_the_offsets_too() {
        let mut atlas = GlyphAtlases::default();
        let k = key(7);
        let (place, _) = atlas.get_or_insert(k, || Some(fake_image(8, 8, false))).unwrap();
        assert!(atlas.offsets.contains_key(&k));
        atlas.invalidate_page(place.kind, place.page);
        assert!(!atlas.offsets.contains_key(&k), "stale offset survives the wipe");
    }

    #[test]
    fn packs_many_glyphs_without_overlap() {
        let mut atlas = GlyphAtlases::default();
        let mut rects = Vec::new();
        for i in 0..600u32 {
            let (place, _) = atlas
                .get_or_insert(key(i), || Some(fake_image(40, 40, false)))
                .unwrap();
            rects.push(place);
        }
        // Same page rects must not overlap.
        for p in 0..atlas.page_count(0) {
            let on_page: Vec<[u32; 4]> = rects
                .iter()
                .filter(|r| r.kind == 0 && r.page == p)
                .map(|r| r.rect)
                .collect();
            for i in 0..on_page.len() {
                for j in i + 1..on_page.len() {
                    let (a, b) = (on_page[i], on_page[j]);
                    let sep = a[0] + a[2] <= b[0]
                        || b[0] + b[2] <= a[0]
                        || a[1] + a[3] <= b[1]
                        || b[1] + b[3] <= a[1];
                    assert!(sep, "overlap on page {p}: {a:?} vs {b:?}");
                }
            }
        }
        assert!(atlas.page_count(0) <= MAX_PAGES, "page budget respected");
    }

    #[test]
    fn cached_lookup_is_stable() {
        let mut atlas = GlyphAtlases::default();
        let k = key(999);
        let (p1, o1) = atlas
            .get_or_insert(k, || Some(fake_image(10, 12, false)))
            .unwrap();
        let (p2, o2) = atlas.get_or_insert(k, || None).unwrap();
        assert_eq!((p1, o1), (p2, o2));
        assert_eq!(atlas.len(), 1);
    }

    fn sized_key(i: u32, px: f32) -> ct::CacheKey {
        ct::CacheKey {
            font_size_bits: px.to_bits(),
            ..key(i)
        }
    }

    /// `specs/18` § 2 (ADR-7208): glyphs of different font size buckets never share an atlas
    /// page, and glyphs of one bucket do.
    #[test]
    fn two_font_sizes_land_in_different_atlas_buckets() {
        let mut atlas = GlyphAtlases::default();
        let (small, _) = atlas
            .get_or_insert(sized_key(1, 14.0), || Some(fake_image(8, 10, false)))
            .unwrap();
        let (large, _) = atlas
            .get_or_insert(sized_key(2, 48.0), || Some(fake_image(30, 36, false)))
            .unwrap();
        let (small_too, _) = atlas
            .get_or_insert(sized_key(3, 15.0), || Some(fake_image(8, 10, false)))
            .unwrap();
        assert_eq!(small.kind, large.kind, "both mono");
        assert_ne!(small.page, large.page, "14 px and 48 px glyphs share a page");
        assert_eq!(small.page, small_too.page, "one bucket packs into one page");
    }

    /// Glyph `i`'s bitmap byte: every glyph's pixels carry its own tag.
    fn tag(i: u32) -> u8 {
        1 + (i % 251) as u8
    }

    /// 200×200 glyphs: four fill a 512×512 page (2×2 with the 1 px gutter), 32 fill the atlas.
    const BIG: u32 = 200;
    const FIT: u32 = 4 * MAX_PAGES;

    /// True when `place` still holds glyph `i`'s bitmap.
    fn holds(atlas: &GlyphAtlases, place: GlyphPlace, i: u32) -> bool {
        let Some((data, bpp)) = atlas.page_data(place.kind, place.page) else {
            return false;
        };
        let [x, y, w, h] = place.rect;
        (y..y + h).all(|row| {
            let start = ((row * PAGE_SIZE + x) as usize) * bpp;
            data[start..start + w as usize * bpp].iter().all(|&b| b == tag(i))
        })
    }

    /// deep-zai-ui-text finding 2 (ADR-8832): a frame that needs more glyphs than the atlas holds
    /// never recycles a page its own quads already point into. Every quad emitted this frame still
    /// samples its own glyph at draw time; the glyphs that do not fit are refused and counted.
    #[test]
    fn a_frame_that_overflows_the_atlas_keeps_every_emitted_quad_intact() {
        let mut atlas = GlyphAtlases::default();
        // An earlier frame fills every page.
        atlas.begin_frame();
        for i in 0..FIT {
            let (place, _) = atlas.get_or_insert(key(i), || Some(mask_tagged(BIG, i))).unwrap();
            assert!(holds(&atlas, place, i));
        }
        assert_eq!(atlas.end_frame(), 0);
        assert_eq!(atlas.page_count(0), MAX_PAGES);
        assert_eq!(atlas.thrash(), 0);
        // This frame's text needs 40 new glyphs: more than the 32 that fit.
        atlas.begin_frame();
        let mut emitted = Vec::new();
        for i in 1000..1000 + FIT + 8 {
            if let Some((place, _)) = atlas.get_or_insert(key(i), || Some(mask_tagged(BIG, i))) {
                emitted.push((i, place));
            }
        }
        // Draw time: every quad of the frame samples its own bitmap.
        for &(i, place) in &emitted {
            assert!(
                holds(&atlas, place, i),
                "glyph {i}'s quad at {place:?} samples another glyph"
            );
        }
        assert_eq!(emitted.len() as u32, FIT, "every glyph that fits is drawn");
        assert_eq!(atlas.end_frame(), 8, "the eight that do not fit are refused");
        assert_eq!(
            (atlas.thrash(), atlas.thrash_frames()),
            (8, 1),
            "the thrash counter rises"
        );
        assert_eq!(atlas.wipes(), u64::from(MAX_PAGES), "each old page recycled once");
    }

    /// A frame whose glyphs are all cached allocates nothing: no rasterization, no new page, no
    /// wipe, nothing to upload, and no thrash.
    #[test]
    fn a_steady_frame_allocates_nothing_new() {
        let mut atlas = GlyphAtlases::default();
        atlas.begin_frame();
        for i in 0..FIT {
            assert!(
                atlas
                    .get_or_insert(key(i), || Some(mask_tagged(BIG, i)))
                    .is_some()
            );
        }
        atlas.end_frame();
        let _ = atlas.take_dirty_pages();
        let (pages, wipes) = (atlas.page_count(0), atlas.wipes());
        for _ in 0..3 {
            atlas.begin_frame();
            for i in (0..FIT).rev() {
                let (place, _) = atlas
                    .get_or_insert(key(i), || panic!("glyph {i} rasterized again"))
                    .unwrap();
                assert!(holds(&atlas, place, i));
            }
            assert_eq!(atlas.end_frame(), 0);
            assert!(atlas.take_dirty_pages().is_empty(), "nothing to upload");
        }
        assert_eq!(
            (atlas.page_count(0), atlas.wipes(), atlas.thrash()),
            (pages, wipes, 0)
        );
    }

    /// The next frame after a thrash frame recycles normally: pins lapse at `end_frame`, so the
    /// glyphs refused last frame get pages this frame (those of glyphs the frame does not draw).
    #[test]
    fn pins_lapse_at_the_end_of_the_frame() {
        let mut atlas = GlyphAtlases::default();
        atlas.begin_frame();
        for i in 0..FIT + 4 {
            let _ = atlas.get_or_insert(key(i), || Some(mask_tagged(BIG, i)));
        }
        assert_eq!(atlas.end_frame(), 4);
        atlas.begin_frame();
        for i in FIT..FIT + 4 {
            let (place, _) = atlas
                .get_or_insert(key(i), || Some(mask_tagged(BIG, i)))
                .expect("a page is free to recycle in the new frame");
            assert!(holds(&atlas, place, i));
        }
        assert_eq!(atlas.end_frame(), 0);
        // Outside a frame (immediate callers, tests) nothing is pinned: the LRU page recycles.
        assert!(
            atlas
                .get_or_insert(key(5000), || Some(mask_tagged(BIG, 5000)))
                .is_some()
        );
        assert_eq!(atlas.thrash(), 4);
    }

    fn mask_tagged(side: u32, i: u32) -> ct::SwashImage {
        let mut image = fake_image(side, side, false);
        image.data.fill(tag(i));
        image
    }

    #[test]
    fn color_glyphs_go_to_color_atlas() {
        let mut atlas = GlyphAtlases::default();
        let (place, _) = atlas
            .get_or_insert(key(7), || Some(fake_image(32, 32, true)))
            .unwrap();
        assert_eq!(place.kind, 1);
        assert_eq!(atlas.page_count(1), 1);
        assert_eq!(atlas.page_count(0), 0);
    }
}
