#![no_std]

use allocator::{AllocError, AllocResult, BaseAllocator, ByteAllocator, PageAllocator};
use core::{alloc::Layout, ptr::NonNull};

/// Early memory allocator
/// Use it before formal bytes-allocator and pages-allocator can work!
/// This is a double-end memory range:
/// - Alloc bytes forward
/// - Alloc pages backward
///
/// [ bytes-used | avail-area | pages-used ]
/// |            | -->    <-- |            |
/// start       b_pos        p_pos       end
///
/// For bytes area, 'count' records number of allocations.
/// When it goes down to ZERO, free bytes-used area.
/// For pages area, it will never be freed!
pub struct EarlyAllocator<const SIZE: usize> {
    start: usize,
    end: usize,
    b_pos: usize,
    p_pos: usize,
    count: usize,
}

impl<const SIZE: usize> EarlyAllocator<SIZE> {
    pub const fn new() -> Self {
        assert!(SIZE.is_power_of_two(), "invalid page size");
        Self {
            start: 0,
            end: 0,
            b_pos: 0,
            p_pos: 0,
            count: 0,
        }
    }

    fn align_up(pos: usize, align: usize) -> Option<usize> {
        pos.checked_add(align - 1).map(|p| p & !(align - 1))
    }

    /// Count complete frames without rounding the upper address up.
    fn frames(start: usize, end: usize) -> usize {
        let first = start / SIZE + usize::from(start % SIZE != 0);
        (end / SIZE).saturating_sub(first)
    }
}

impl<const SIZE: usize> BaseAllocator for EarlyAllocator<SIZE> {
    fn init(&mut self, start: usize, size: usize) {
        assert!(size != 0, "empty memory region");
        let end = start.checked_add(size).expect("memory region overflow");
        self.start = start;
        self.end = end;
        self.b_pos = start;
        self.p_pos = end;
        self.count = 0;
    }

    // Only an unused end may grow without exposing already allocated memory.
    fn add_memory(&mut self, start: usize, size: usize) -> AllocResult {
        if size == 0 {
            return Err(AllocError::InvalidParam);
        }
        let end = start.checked_add(size).ok_or(AllocError::InvalidParam)?;
        if self.start == self.end {
            self.init(start, size);
        } else if start < self.end && end > self.start {
            return Err(AllocError::MemoryOverlap);
        } else if start == self.end && self.p_pos == self.end {
            self.end = end;
            self.p_pos = end;
        } else if end == self.start && self.count == 0 {
            self.start = start;
            self.b_pos = start;
        } else {
            return Err(AllocError::InvalidParam);
        }
        Ok(())
    }
}

impl<const SIZE: usize> ByteAllocator for EarlyAllocator<SIZE> {
    fn alloc(&mut self, layout: Layout) -> AllocResult<NonNull<u8>> {
        if layout.size() == 0 {
            return Err(AllocError::InvalidParam);
        }
        // NonNull byte allocations must skip address zero.
        let pos = Self::align_up(self.b_pos.max(1), layout.align()).ok_or(AllocError::NoMemory)?;
        let end = pos.checked_add(layout.size()).ok_or(AllocError::NoMemory)?;
        if end > self.p_pos {
            return Err(AllocError::NoMemory);
        }
        let count = self.count.checked_add(1).ok_or(AllocError::NoMemory)?;
        let ptr = NonNull::new(pos as *mut u8).ok_or(AllocError::NoMemory)?;
        self.b_pos = end;
        self.count = count;
        Ok(ptr)
    }

    fn dealloc(&mut self, pos: NonNull<u8>, layout: Layout) {
        let pos = pos.as_ptr() as usize;
        assert!(self.count != 0 && layout.size() != 0, "invalid byte free");
        assert!(
            pos >= self.start && pos % layout.align() == 0,
            "invalid byte free"
        );
        assert!(
            pos.checked_add(layout.size())
                .map_or(false, |end| end <= self.b_pos),
            "invalid byte free"
        );
        self.count -= 1;
        if self.count == 0 {
            self.b_pos = self.start;
        }
    }

    fn total_bytes(&self) -> usize {
        self.end - self.start
    }

    fn used_bytes(&self) -> usize {
        self.total_bytes() - self.available_bytes()
    }

    fn available_bytes(&self) -> usize {
        self.p_pos - self.b_pos
    }
}

impl<const SIZE: usize> PageAllocator for EarlyAllocator<SIZE> {
    const PAGE_SIZE: usize = SIZE;

    fn alloc_pages(&mut self, num_pages: usize, align_pow2: usize) -> AllocResult<usize> {
        if num_pages == 0 || !align_pow2.is_power_of_two() {
            return Err(AllocError::InvalidParam);
        }
        let bytes = num_pages
            .checked_mul(SIZE)
            .ok_or(AllocError::InvalidParam)?;
        // align_pow2 is an alignment in bytes, not an exponent.
        let align = align_pow2.max(SIZE);
        let pos = self.p_pos.checked_sub(bytes).ok_or(AllocError::NoMemory)? & !(align - 1);
        if pos < self.b_pos {
            return Err(AllocError::NoMemory);
        }
        // The tail and any alignment gap stay reserved permanently.
        self.p_pos = pos;
        Ok(pos)
    }

    fn dealloc_pages(&mut self, _pos: usize, _num_pages: usize) {
        // Intentionally unreclaimable, including alignment padding.
    }

    fn total_pages(&self) -> usize {
        Self::frames(self.start, self.end)
    }

    fn used_pages(&self) -> usize {
        self.total_pages() - self.available_pages()
    }

    fn available_pages(&self) -> usize {
        Self::frames(self.b_pos, self.p_pos)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    type Alloc = EarlyAllocator<0x1000>;

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    fn state<const SIZE: usize>(a: &EarlyAllocator<SIZE>) -> [usize; 5] {
        [a.start, a.end, a.b_pos, a.p_pos, a.count]
    }

    fn stats(a: &Alloc, total: usize, used: usize, pages: usize, free_pages: usize) {
        assert_eq!(a.total_bytes(), total);
        assert_eq!(a.used_bytes(), used);
        assert_eq!(a.available_bytes(), total - used);
        assert_eq!(a.total_pages(), pages);
        assert_eq!(a.used_pages(), pages - free_pages);
        assert_eq!(a.available_pages(), free_pages);
    }

    #[test]
    fn empty_and_invalid_requests_leave_state_unchanged() {
        let mut a = Alloc::new();
        stats(&a, 0, 0, 0, 0);
        let before = state(&a);
        assert!(matches!(a.alloc(layout(1, 1)), Err(AllocError::NoMemory)));
        assert!(matches!(a.alloc_pages(1, 1), Err(AllocError::NoMemory)));
        assert_eq!(state(&a), before);
        a.init(0x1000, 0x8000);
        let before = state(&a);
        assert!(matches!(
            a.alloc(layout(0, 8)),
            Err(AllocError::InvalidParam)
        ));
        for (count, align) in [(0, 0x1000), (1, 0), (1, 3), (1, 0x1800), (usize::MAX, 1)] {
            assert!(matches!(
                a.alloc_pages(count, align),
                Err(AllocError::InvalidParam)
            ));
            assert_eq!(state(&a), before);
        }
        assert!(matches!(
            a.alloc_pages(usize::MAX / 0x1000, 1),
            Err(AllocError::NoMemory)
        ));
        assert!(matches!(
            a.alloc(layout(isize::MAX as usize, 1)),
            Err(AllocError::NoMemory)
        ));
        assert_eq!(state(&a), before);
    }

    #[test]
    fn unaligned_region_and_shared_statistics() {
        let mut a = Alloc::new();
        a.init(0x1003, 0x9002); // end = 0xa005
        stats(&a, 0x9002, 0, 8, 8);
        let first = a.alloc(layout(13, 64)).unwrap();
        assert_eq!(first.as_ptr() as usize, 0x1040);
        stats(&a, 0x9002, 0x4a, 8, 8);
        assert_eq!(a.alloc_pages(2, 0x4000).unwrap(), 0x8000);
        stats(&a, 0x9002, 0x204f, 8, 6);
        // An alignment below PAGE_SIZE is valid, but pages remain aligned.
        assert_eq!(a.alloc_pages(1, 8).unwrap(), 0x7000);
        stats(&a, 0x9002, 0x304f, 8, 5);
        a.dealloc(first, layout(13, 64));
        stats(&a, 0x9002, 0x3005, 8, 5);
    }

    #[test]
    fn byte_reclamation_waits_for_last_allocation() {
        let mut a = Alloc::new();
        a.init(0x1003, 0x8ffd);
        let first = a.alloc(layout(1, 1)).unwrap();
        let second = a.alloc(layout(100, 256)).unwrap();
        assert_eq!(second.as_ptr() as usize, 0x1100);
        let page = a.alloc_pages(1, 0x1000).unwrap();
        let used = a.used_bytes();
        a.dealloc(first, layout(1, 1));
        assert_eq!(a.used_bytes(), used);
        let third = a.alloc(layout(1, 1)).unwrap();
        assert_eq!(third.as_ptr() as usize, 0x1164);
        a.dealloc(third, layout(1, 1));
        a.dealloc(second, layout(100, 256));
        assert_eq!(a.b_pos, 0x1003);
        assert_eq!(a.p_pos, page);
        assert_eq!(a.used_bytes(), 0x1000);
        assert_eq!(a.alloc(layout(1, 1)).unwrap(), first);
    }

    #[test]
    fn both_ends_can_meet_but_never_overlap() {
        for pages_first in [false, true] {
            let mut a = Alloc::new();
            a.init(0x1000, 0x3000);
            let ptr;
            if pages_first {
                assert_eq!(a.alloc_pages(1, 0x1000).unwrap(), 0x3000);
                ptr = a.alloc(layout(0x2000, 1)).unwrap();
            } else {
                ptr = a.alloc(layout(0x2000, 1)).unwrap();
                assert_eq!(a.alloc_pages(1, 0x1000).unwrap(), 0x3000);
            }
            stats(&a, 0x3000, 0x3000, 3, 0);
            let before = state(&a);
            assert!(matches!(a.alloc(layout(1, 1)), Err(AllocError::NoMemory)));
            assert!(matches!(a.alloc_pages(1, 1), Err(AllocError::NoMemory)));
            assert_eq!(state(&a), before);
            a.dealloc(ptr, layout(0x2000, 1));
            stats(&a, 0x3000, 0x1000, 3, 2);
        }
    }

    #[test]
    fn alignment_gaps_can_cause_oom_without_consuming_memory() {
        let mut a = Alloc::new();
        a.init(0x1001, 0x3fff);
        let before = state(&a);
        assert!(matches!(
            a.alloc(layout(0x2001, 0x4000)),
            Err(AllocError::NoMemory)
        ));
        // Rounding 0x3000 down to a 0x4000 boundary crosses the byte cursor.
        assert!(matches!(
            a.alloc_pages(2, 0x4000),
            Err(AllocError::NoMemory)
        ));
        assert_eq!(state(&a), before);
        assert_eq!(a.alloc_pages(3, 1).unwrap(), 0x2000);
    }

    #[test]
    fn pages_and_their_alignment_padding_are_never_reclaimed() {
        let mut a = Alloc::new();
        a.init(0x1000, 0x8000);
        let page = a.alloc_pages(2, 0x4000).unwrap();
        assert_eq!(page, 0x4000);
        stats(&a, 0x8000, 0x5000, 8, 3);
        let before = state(&a);
        a.dealloc_pages(page, 2);
        assert_eq!(state(&a), before);
        assert_eq!(a.alloc_pages(1, 1).unwrap(), 0x3000);
        assert!(matches!(
            a.add_memory(0x9000, 0x1000),
            Err(AllocError::InvalidParam)
        ));
    }

    #[test]
    fn contiguous_extensions_preserve_existing_allocations() {
        let mut a = Alloc::new();
        a.add_memory(0x3000, 0x1000).unwrap();
        let ptr = a.alloc(layout(0x100, 16)).unwrap();
        a.add_memory(0x4000, 0x2000).unwrap();
        stats(&a, 0x3000, 0x100, 3, 2);
        let page = a.alloc_pages(1, 1).unwrap();
        assert_eq!(page, 0x5000);
        let before = state(&a);
        assert!(matches!(
            a.add_memory(0x2000, 0x1000),
            Err(AllocError::InvalidParam)
        ));
        assert!(matches!(
            a.add_memory(0x6000, 0x1000),
            Err(AllocError::InvalidParam)
        ));
        assert_eq!(state(&a), before);
        a.dealloc(ptr, layout(0x100, 16));
        a.add_memory(0x1000, 0x2000).unwrap();
        stats(&a, 0x5000, 0x1000, 5, 4);
        assert_eq!(a.alloc(layout(16, 16)).unwrap().as_ptr() as usize, 0x1000);
        assert_eq!(a.p_pos, page);
    }

    #[test]
    fn invalid_extensions_leave_state_unchanged() {
        let mut a = Alloc::new();
        a.init(0x2000, 0x2000);
        let before = state(&a);
        for (start, size) in [(0x4000, 0), (usize::MAX, 2), (0x5000, 1), (0x1000, 1)] {
            assert!(matches!(
                a.add_memory(start, size),
                Err(AllocError::InvalidParam)
            ));
            assert_eq!(state(&a), before);
        }
        for (start, size) in [(0x2000, 1), (0x1fff, 2), (0x3fff, 2), (0x1000, 0x4000)] {
            assert!(matches!(
                a.add_memory(start, size),
                Err(AllocError::MemoryOverlap)
            ));
            assert_eq!(state(&a), before);
        }
    }

    #[test]
    fn address_arithmetic_does_not_wrap() {
        let mut a = Alloc::new();
        a.init(usize::MAX - 15, 15);
        let before = state(&a);
        assert!(matches!(a.alloc(layout(1, 32)), Err(AllocError::NoMemory)));
        assert!(matches!(a.alloc(layout(16, 1)), Err(AllocError::NoMemory)));
        assert!(matches!(a.alloc_pages(1, 1), Err(AllocError::NoMemory)));
        assert_eq!(state(&a), before);
        stats(&a, 15, 0, 0, 0);
        let ptr = a.alloc(layout(15, 1)).unwrap();
        assert_eq!(a.available_bytes(), 0);
        a.dealloc(ptr, layout(15, 1));
        assert_eq!(state(&a), before);
        // The count is checked before committing either field too.
        a.count = usize::MAX;
        let before = state(&a);
        assert!(matches!(a.alloc(layout(1, 1)), Err(AllocError::NoMemory)));
        assert_eq!(state(&a), before);
    }

    #[test]
    fn invalid_initialization_panics_before_changing_state() {
        assert!(catch_unwind(EarlyAllocator::<0>::new).is_err());
        assert!(catch_unwind(EarlyAllocator::<3>::new).is_err());
        let mut a = Alloc::new();
        a.init(0x1000, 0x1000);
        let before = state(&a);
        assert!(catch_unwind(AssertUnwindSafe(|| a.init(1, 0))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| a.init(usize::MAX, 2))).is_err());
        assert_eq!(state(&a), before);
    }

    #[test]
    fn null_address_is_not_returned_for_bytes() {
        let mut a = EarlyAllocator::<1>::new();
        a.init(0, 3);
        let ptr = a.alloc(layout(1, 1)).unwrap();
        assert_eq!(ptr.as_ptr() as usize, 1);
        assert_eq!(a.alloc_pages(1, 1).unwrap(), 2);
        a.dealloc(ptr, layout(1, 1));
        assert_eq!(a.available_bytes(), 2);
        assert_eq!(a.available_pages(), 2);
        // Unlike NonNull byte allocations, page addresses can be zero.
        assert_eq!(a.alloc_pages(2, 1).unwrap(), 0);
    }

    #[test]
    fn complete_frame_counts_exclude_partial_boundary_pages() {
        let mut a = Alloc::new();
        a.init(0x1fff, 0x1002);
        stats(&a, 0x1002, 0, 1, 1);
        let ptr = a.alloc(layout(2, 1)).unwrap();
        stats(&a, 0x1002, 2, 1, 0);
        let before = state(&a);
        assert!(matches!(a.alloc_pages(1, 1), Err(AllocError::NoMemory)));
        assert_eq!(state(&a), before);
        a.dealloc(ptr, layout(2, 1));
        assert_eq!(a.alloc_pages(1, 1).unwrap(), 0x2000);
        stats(&a, 0x1002, 0x1001, 1, 0);
    }

    #[test]
    fn allocations_back_real_disjoint_writable_memory() {
        #[repr(align(4096))]
        struct Memory([u8; 0x8000]);
        let mut memory = Memory([0; 0x8000]);
        let start = memory.0.as_mut_ptr() as usize;
        let mut a = Alloc::new();
        a.init(start, memory.0.len());
        let first = a.alloc(layout(37, 64)).unwrap();
        let second = a.alloc(layout(99, 256)).unwrap();
        let pages = a.alloc_pages(2, 0x2000).unwrap();
        assert_eq!(first.as_ptr() as usize % 64, 0);
        assert_eq!(second.as_ptr() as usize % 256, 0);
        assert_eq!(pages % 0x2000, 0);
        let offsets = [
            first.as_ptr() as usize - start,
            second.as_ptr() as usize - start,
            pages - start,
        ];
        assert!(offsets[0] + 37 <= offsets[1]);
        assert!(offsets[1] + 99 <= offsets[2]);
        assert!(offsets[2] + 0x2000 <= memory.0.len());
        // SAFETY: The returned, disjoint regions lie in the live Memory above.
        unsafe {
            first.as_ptr().write_bytes(0x11, 37);
            second.as_ptr().write_bytes(0x22, 99);
            (pages as *mut u8).write_bytes(0x33, 0x2000);
        }
        assert!(memory.0[offsets[0]..offsets[0] + 37]
            .iter()
            .all(|&b| b == 0x11));
        assert!(memory.0[offsets[1]..offsets[1] + 99]
            .iter()
            .all(|&b| b == 0x22));
        assert!(memory.0[offsets[2]..offsets[2] + 0x2000]
            .iter()
            .all(|&b| b == 0x33));
        a.dealloc(first, layout(37, 64));
        a.dealloc(second, layout(99, 256));
        a.dealloc_pages(pages, 2);
        let replacement = a.alloc(layout(37, 64)).unwrap();
        assert_eq!(replacement, first);
        assert!(replacement.as_ptr() as usize + 37 <= pages);
    }
}
