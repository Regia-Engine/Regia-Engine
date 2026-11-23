// SPDX-License-Identifier: MPL-2.0
//! The tagging global allocator behind [`super`]'s per-subsystem ledger.
//!
//! Every block gets a header in front of the caller's bytes: `prefix = max(16, align)` bytes, a
//! multiple of the caller's alignment, so `raw + prefix` keeps it. The last header byte holds the
//! [`super::Subsystem`] tag of the thread that allocated the block, the byte before it the
//! thread's [`super::MemoryOwner`] row (0: none); a free reads them back, so the bytes leave the
//! ledger of the subsystem and owner that took them, whichever thread frees them. A reallocation
//! keeps the header (the block's contents move with it) and so stays with its owner.

#![allow(unsafe_code)] // a GlobalAlloc impl is unsafe by definition; see each SAFETY note

use std::alloc::{GlobalAlloc, Layout, System};

/// Smallest header: enough for the tag and owner bytes and keeps 16-byte alignment of the
/// caller's bytes.
const MIN_PREFIX: usize = 16;

/// A global allocator that forwards to `A` and counts each allocation against the allocating
/// thread's [`super::Subsystem`] ([`super::enter`]).
///
/// ```ignore
/// #[global_allocator]
/// static ALLOC: regia_core::memory::TaggingAllocator = regia_core::memory::TaggingAllocator::system();
/// ```
///
/// Costs a `max(16, align)`-byte header and a few relaxed atomic adds per allocation: meant for
/// dev builds, tests and soaks, not shipped games.
#[derive(Debug, Default, Clone, Copy)]
pub struct TaggingAllocator<A = System> {
    inner: A,
}

impl TaggingAllocator<System> {
    /// Wraps the system allocator.
    #[must_use]
    pub const fn system() -> Self {
        Self { inner: System }
    }
}

impl<A> TaggingAllocator<A> {
    /// Wraps `inner` (another global allocator).
    #[must_use]
    pub const fn new(inner: A) -> Self {
        Self { inner }
    }
}

/// Header size for a caller alignment.
fn prefix(align: usize) -> usize {
    align.max(MIN_PREFIX)
}

/// The layout actually requested from the inner allocator for `size` caller bytes; `None` when it
/// overflows.
fn outer_layout(size: usize, align: usize) -> Option<Layout> {
    let total = size.checked_add(prefix(align))?;
    Layout::from_size_align(total, align).ok()
}

// SAFETY: every method forwards to `inner` with a layout that is valid for it (the caller's layout
// grown by a header that is a multiple of the caller's alignment, see `outer_layout`) and returns a
// pointer `prefix` bytes into that block, which is aligned to the caller's alignment and has
// `layout.size()` bytes after it. `dealloc` and `realloc` recompute the same header from the
// caller's layout (the `GlobalAlloc` contract hands back the layout the block was allocated with),
// so they pass `inner` the same block and layout it returned.
unsafe impl<A: GlobalAlloc> GlobalAlloc for TaggingAllocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(outer) = outer_layout(layout.size(), layout.align()) else {
            return std::ptr::null_mut();
        };
        // SAFETY: `outer` has a non-zero size (it includes the header).
        let raw = unsafe { self.inner.alloc(outer) };
        if raw.is_null() {
            return raw;
        }
        let tag = super::current_tag();
        let owner = super::current_owner_tag();
        let pre = prefix(layout.align());
        // SAFETY: `raw` points to `outer.size() >= pre >= 16` writable bytes; `pre - 1` and
        // `pre - 2` are in the header.
        unsafe {
            raw.add(pre - 1).write(tag);
            raw.add(pre - 2).write(owner);
        }
        super::record_alloc(tag, owner, layout.size());
        // SAFETY: `pre <= outer.size()`, so the result stays inside the block.
        unsafe { raw.add(pre) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let Some(outer) = outer_layout(layout.size(), layout.align()) else {
            return std::ptr::null_mut();
        };
        // SAFETY: `outer` has a non-zero size (it includes the header).
        let raw = unsafe { self.inner.alloc_zeroed(outer) };
        if raw.is_null() {
            return raw;
        }
        let tag = super::current_tag();
        let owner = super::current_owner_tag();
        let pre = prefix(layout.align());
        // SAFETY: as in `alloc`.
        unsafe {
            raw.add(pre - 1).write(tag);
            raw.add(pre - 2).write(owner);
        }
        super::record_alloc(tag, owner, layout.size());
        // SAFETY: as in `alloc`.
        unsafe { raw.add(pre) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let pre = prefix(layout.align());
        // SAFETY: `ptr` came from `alloc`/`realloc` with this layout, so it is `pre` bytes into a
        // block of `inner`, and the tag byte sits just before it.
        let raw = unsafe { ptr.sub(pre) };
        // SAFETY: the tag and owner bytes were written when the block was allocated.
        let (tag, owner) = unsafe { (raw.add(pre - 1).read(), raw.add(pre - 2).read()) };
        super::record_free(tag, owner, layout.size());
        // SAFETY: the block was allocated with this outer layout, which was valid then (so
        // `from_size_align_unchecked`'s conditions hold).
        unsafe {
            let outer = Layout::from_size_align_unchecked(layout.size() + pre, layout.align());
            self.inner.dealloc(raw, outer);
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let pre = prefix(layout.align());
        // Refuse sizes whose outer layout would overflow; the old block stays valid.
        if outer_layout(new_size, layout.align()).is_none() {
            return std::ptr::null_mut();
        }
        // SAFETY: as in `dealloc`.
        let raw = unsafe { ptr.sub(pre) };
        // SAFETY: the tag and owner bytes were written when the block was allocated.
        let (tag, owner) = unsafe { (raw.add(pre - 1).read(), raw.add(pre - 2).read()) };
        // SAFETY: `raw` was allocated by `inner` with this outer layout (valid then), and the new
        // outer size `new_size + pre` was checked above to form a valid layout.
        let moved = unsafe {
            let outer = Layout::from_size_align_unchecked(layout.size() + pre, layout.align());
            self.inner.realloc(raw, outer, new_size + pre)
        };
        if moved.is_null() {
            return moved;
        }
        super::record_resize(tag, owner, layout.size(), new_size);
        // SAFETY: the new block has `new_size + pre` bytes and kept the header (realloc copies
        // the contents), so `pre` bytes in is the caller's data.
        unsafe { moved.add(pre) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Subsystem, enter, snapshot};

    #[test]
    fn header_keeps_alignment_and_tag_follows_the_block() {
        // Not installed as the global allocator here: driven directly. The Agent slot is used by
        // nothing else in this crate's tests.
        let alloc = TaggingAllocator::system();
        let before = snapshot().subsystem(Subsystem::Agent);
        for align in [1usize, 2, 8, 16, 32, 64, 4096] {
            let layout = Layout::from_size_align(24, align).expect("valid test layout");
            let ptr = {
                let _scope = enter(Subsystem::Agent);
                // SAFETY: non-zero size; freed below with the same layout.
                unsafe { alloc.alloc(layout) }
            };
            assert!(!ptr.is_null());
            assert!((ptr as usize).is_multiple_of(align), "align {align}");
            // SAFETY: 24 writable bytes at `ptr`.
            unsafe { ptr.write_bytes(0xAB, 24) };
            let mid = snapshot().subsystem(Subsystem::Agent);
            assert_eq!(mid.live_bytes, before.live_bytes + 24);
            // Grown from another scope: stays Agent's.
            let grown = {
                let _scope = enter(Subsystem::Render);
                // SAFETY: `ptr` is live with `layout`; 100 is a valid new size.
                unsafe { alloc.realloc(ptr, layout, 100) }
            };
            assert!(!grown.is_null());
            assert!((grown as usize).is_multiple_of(align));
            // SAFETY: the first 24 bytes moved with the block.
            assert_eq!(unsafe { grown.read() }, 0xAB);
            assert_eq!(
                snapshot().subsystem(Subsystem::Agent).live_bytes,
                before.live_bytes + 100
            );
            let grown_layout = Layout::from_size_align(100, align).expect("valid test layout");
            // SAFETY: `grown` is live with `grown_layout`.
            unsafe { alloc.dealloc(grown, grown_layout) };
            assert_eq!(
                snapshot().subsystem(Subsystem::Agent).live_bytes,
                before.live_bytes
            );
        }
        let zeroed_layout = Layout::from_size_align(64, 8).expect("valid test layout");
        let _scope = enter(Subsystem::Agent);
        // SAFETY: non-zero size; freed below with the same layout.
        let zeroed = unsafe { alloc.alloc_zeroed(zeroed_layout) };
        assert!(!zeroed.is_null());
        // SAFETY: 64 readable bytes, not written through `zeroed` while this borrow lives.
        let bytes = unsafe { std::slice::from_raw_parts(zeroed, 64) };
        assert!(bytes.iter().all(|b| *b == 0));
        // SAFETY: `zeroed` is live with `zeroed_layout`.
        unsafe { alloc.dealloc(zeroed, zeroed_layout) };
        assert_eq!(
            snapshot().subsystem(Subsystem::Agent).live_allocations,
            before.live_allocations
        );
    }

    #[test]
    fn the_owner_byte_follows_the_block_through_realloc_and_free() {
        use crate::memory::{MemoryOwner, enter_owner, owner_snapshot};
        static OWNER: MemoryOwner = MemoryOwner::new(Subsystem::Ui, "ui.tagging_test");
        let alloc = TaggingAllocator::system();
        for align in [1usize, 16, 64] {
            let before = owner_snapshot().live_bytes("ui.tagging_test");
            let layout = Layout::from_size_align(40, align).expect("valid test layout");
            let ptr = {
                let _owner = enter_owner(&OWNER);
                // SAFETY: non-zero size; freed below with the size it has by then.
                unsafe { alloc.alloc(layout) }
            };
            assert!(!ptr.is_null());
            assert_eq!(owner_snapshot().live_bytes("ui.tagging_test"), before + 40);
            // Grown and freed outside the owner scope: still the owner's.
            let grown = {
                let _scope = enter(Subsystem::Render);
                // SAFETY: `ptr` is live with `layout`; 400 is a valid new size.
                unsafe { alloc.realloc(ptr, layout, 400) }
            };
            assert!(!grown.is_null());
            assert_eq!(owner_snapshot().live_bytes("ui.tagging_test"), before + 400);
            let grown_layout = Layout::from_size_align(400, align).expect("valid test layout");
            // SAFETY: `grown` is live with `grown_layout`.
            unsafe { alloc.dealloc(grown, grown_layout) };
            assert_eq!(owner_snapshot().live_bytes("ui.tagging_test"), before);
        }
        // A block allocated outside every owner scope never reaches an owner row.
        let before = owner_snapshot();
        let layout = Layout::from_size_align(64, 8).expect("valid test layout");
        // SAFETY: non-zero size; freed right after with the same layout.
        let block = unsafe { alloc.alloc_zeroed(layout) };
        assert_eq!(
            owner_snapshot().live_bytes("ui.tagging_test"),
            before.live_bytes("ui.tagging_test")
        );
        // SAFETY: `block` is live with `layout`.
        unsafe { alloc.dealloc(block, layout) };
    }

    /// An inner allocator that checks what the wrapper hands it: every block comes back with the
    /// layout it went out with, and the pointer it gave out. `refuse` makes the next allocations
    /// fail, as an exhausted heap does.
    #[derive(Default)]
    struct Checking {
        live: std::sync::Mutex<Vec<(usize, Layout)>>,
        refuse: std::sync::atomic::AtomicBool,
    }

    impl Checking {
        fn take(&self, ptr: *mut u8, layout: Layout) {
            let mut live = self.live.lock().expect("no panic while held");
            let at = live
                .iter()
                .position(|(addr, _)| *addr == ptr.addr())
                .expect("the wrapper freed a pointer the inner allocator never gave out");
            assert_eq!(live.remove(at).1, layout, "a block came back with another layout");
        }
    }

    // SAFETY: forwards to `System` with the same arguments; the list only records them.
    unsafe impl GlobalAlloc for Checking {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if self.refuse.load(std::sync::atomic::Ordering::Relaxed) {
                return std::ptr::null_mut();
            }
            // SAFETY: the caller's contract, passed on.
            let ptr = unsafe { System.alloc(layout) };
            self.live
                .lock()
                .expect("no panic while held")
                .push((ptr.addr(), layout));
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            self.take(ptr, layout);
            // SAFETY: the caller's contract, passed on.
            unsafe { System.dealloc(ptr, layout) };
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if self.refuse.load(std::sync::atomic::Ordering::Relaxed) {
                return std::ptr::null_mut();
            }
            self.take(ptr, layout);
            // SAFETY: the caller's contract, passed on.
            let moved = unsafe { System.realloc(ptr, layout, new_size) };
            let grown = Layout::from_size_align(new_size, layout.align()).expect("a valid new layout");
            self.live
                .lock()
                .expect("no panic while held")
                .push((moved.addr(), grown));
            moved
        }
    }

    #[test]
    fn the_inner_allocator_gets_back_the_layouts_it_handed_out() {
        let alloc = TaggingAllocator::new(Checking::default());
        for align in [1usize, 8, 16, 64, 4096] {
            for size in [1usize, 15, 16, 17, 4096] {
                let layout = Layout::from_size_align(size, align).expect("valid test layout");
                // SAFETY: non-zero size; freed below with the layout it has by then.
                let ptr = unsafe { alloc.alloc(layout) };
                assert!((ptr as usize).is_multiple_of(align), "align {align}");
                // SAFETY: `size` writable bytes.
                unsafe { ptr.write_bytes(0x5A, size) };
                // Shrink and grow: the bytes that stay in range move with the block.
                let small = (size / 2).max(1);
                // SAFETY: `ptr` is live with `layout`; `small` is non-zero.
                let shrunk = unsafe { alloc.realloc(ptr, layout, small) };
                assert!((shrunk as usize).is_multiple_of(align));
                // SAFETY: `small` readable bytes.
                assert_eq!(unsafe { shrunk.add(small - 1).read() }, 0x5A);
                let small_layout = Layout::from_size_align(small, align).expect("valid test layout");
                // SAFETY: `shrunk` is live with `small_layout`; freed with it right after.
                unsafe { alloc.dealloc(shrunk, small_layout) };
            }
        }
        assert!(alloc.inner.live.lock().expect("no panic while held").is_empty());
    }

    /// A failed allocation is a null and no ledger change; a failed reallocation leaves the old
    /// block valid and its bytes counted, as `GlobalAlloc` promises its caller.
    #[test]
    fn a_refused_allocation_changes_nothing() {
        let alloc = TaggingAllocator::new(Checking::default());
        let layout = Layout::from_size_align(32, 8).expect("valid test layout");
        let _scope = enter(Subsystem::Audio);
        // SAFETY: non-zero size; freed below.
        let block = unsafe { alloc.alloc(layout) };
        // SAFETY: 32 writable bytes.
        unsafe { block.write_bytes(0x7E, 32) };
        let before = snapshot().subsystem(Subsystem::Audio);
        alloc
            .inner
            .refuse
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // SAFETY: non-zero sizes; both calls are refused, so nothing is allocated.
        unsafe {
            assert!(alloc.alloc(layout).is_null());
            assert!(alloc.alloc_zeroed(layout).is_null());
        }
        // SAFETY: `block` is live with `layout`, and stays so when the call fails.
        assert!(unsafe { alloc.realloc(block, layout, 64) }.is_null());
        alloc
            .inner
            .refuse
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            snapshot().subsystem(Subsystem::Audio).live_bytes,
            before.live_bytes
        );
        // SAFETY: the old block is still live, 32 readable bytes.
        assert_eq!(unsafe { block.add(31).read() }, 0x7E);
        // SAFETY: `block` is live with `layout`.
        unsafe { alloc.dealloc(block, layout) };
    }

    /// A size whose outer layout would overflow is refused before the inner allocator sees it, and
    /// the old block stays valid.
    #[test]
    fn a_reallocation_too_large_for_the_header_is_refused() {
        let alloc = TaggingAllocator::new(Checking::default());
        let layout = Layout::from_size_align(8, 8).expect("valid test layout");
        // SAFETY: non-zero size; freed below.
        let block = unsafe { alloc.alloc(layout) };
        // SAFETY: 8 writable bytes.
        unsafe { block.write_bytes(0x11, 8) };
        let too_large = isize::MAX as usize - 3;
        // SAFETY: `block` is live with `layout`. `too_large` is a valid size for `layout`'s align by
        // `GlobalAlloc::realloc`'s rule (it does not overflow `isize` when rounded up), and the
        // header is what pushes it over.
        assert!(unsafe { alloc.realloc(block, layout, too_large - 8) }.is_null());
        // SAFETY: the old block is still live.
        assert_eq!(unsafe { block.read() }, 0x11);
        // SAFETY: `block` is live with `layout`.
        unsafe { alloc.dealloc(block, layout) };
    }

    #[test]
    fn overflowing_layouts_fail_cleanly() {
        assert!(outer_layout(usize::MAX - 4, 8).is_none());
        assert!(outer_layout(isize::MAX as usize, 8).is_none());
        assert_eq!(outer_layout(10, 4).map(|l| l.size()), Some(26));
        assert_eq!(
            outer_layout(10, 64).map(|l| (l.size(), l.align())),
            Some((74, 64))
        );
    }
}
