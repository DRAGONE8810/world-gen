//! Range allocator for the shared vertex pool.
//!
//! Instead of one GPU buffer per chunk (thousands of buffers, one bind and
//! one draw call each), all meshes live in a few big buffers. This hands
//! out ranges of such a buffer: a first-fit free list kept sorted by
//! offset, merging neighbouring free ranges when one is released so the
//! pool doesn't fragment into slivers as chunks stream in and out.

use std::collections::BTreeMap;

pub struct RangeAllocator {
    capacity: u64,
    /// Free ranges: offset → length, never adjacent (they get merged).
    free: BTreeMap<u64, u64>,
    used: u64,
}

impl RangeAllocator {
    pub fn new(capacity: u64) -> Self {
        let mut free = BTreeMap::new();
        if capacity > 0 {
            free.insert(0, capacity);
        }
        Self { capacity, free, used: 0 }
    }

    pub fn used(&self) -> u64 {
        self.used
    }

    /// First free range of at least `len`, or `None` when full.
    pub fn alloc(&mut self, len: u64) -> Option<u64> {
        assert!(len > 0);
        let (&offset, &size) = self.free.iter().find(|(_, &size)| size >= len)?;
        self.free.remove(&offset);
        if size > len {
            self.free.insert(offset + len, size - len);
        }
        self.used += len;
        Some(offset)
    }

    pub fn free(&mut self, offset: u64, len: u64) {
        debug_assert!(offset + len <= self.capacity);
        self.used -= len;
        let (mut start, mut end) = (offset, offset + len);
        // Merge with the free range just before…
        if let Some((&prev, &prev_len)) = self.free.range(..offset).next_back() {
            debug_assert!(prev + prev_len <= offset, "double free at {offset}");
            if prev + prev_len == offset {
                self.free.remove(&prev);
                start = prev;
            }
        }
        // …and just after.
        if let Some(&next_len) = self.free.get(&end) {
            self.free.remove(&end);
            end += next_len;
        }
        self.free.insert(start, end - start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_frees_and_merges() {
        let mut a = RangeAllocator::new(100);
        let x = a.alloc(30).unwrap();
        let y = a.alloc(30).unwrap();
        let z = a.alloc(30).unwrap();
        assert_eq!((x, y, z), (0, 30, 60));
        assert!(a.alloc(20).is_none(), "only 10 left");
        a.free(y, 30);
        assert_eq!(a.alloc(20), Some(30), "first fit reuses the hole");
        a.free(30, 20);
        a.free(x, 30);
        a.free(z, 30);
        assert_eq!(a.used(), 0);
        // Everything merged back into one range.
        assert_eq!(a.alloc(100), Some(0));
    }

    #[test]
    fn random_churn_never_overlaps() {
        let mut a = RangeAllocator::new(10_000);
        let mut live: Vec<(u64, u64)> = Vec::new();
        let mut state = 12345u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            if live.len() > 50 || (next() % 3 == 0 && !live.is_empty()) {
                let i = (next() % live.len() as u64) as usize;
                let (o, l) = live.swap_remove(i);
                a.free(o, l);
            } else {
                let l = 1 + next() % 300;
                if let Some(o) = a.alloc(l) {
                    for &(p, m) in &live {
                        assert!(o + l <= p || p + m <= o, "[{o}, {}) overlaps [{p}, {})", o + l, p + m);
                    }
                    live.push((o, l));
                }
            }
        }
        assert_eq!(a.used(), live.iter().map(|x| x.1).sum::<u64>());
    }
}
