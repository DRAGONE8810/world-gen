//! Palette-compressed block storage for one 16×16×16 section.
//!
//! A section holds 4096 blocks. Stored naively as `u16` that is 8 KiB, and a
//! full-height chunk (24 sections) would be 192 KiB. But a section rarely
//! contains more than a handful of distinct block types, so we store:
//!
//! * a small **palette**: the list of distinct block ids in this section, and
//! * for each block, an **index into the palette**, bit-packed using only as
//!   many bits as the palette size needs.
//!
//! | distinct blocks | bits/block | size       |
//! |-----------------|-----------:|-----------:|
//! | 1 (all air)     | 0          | ~0 bytes   |
//! | 2               | 1          | 512 bytes  |
//! | 3–4             | 2          | 1 KiB      |
//! | 5–16            | 4          | 2 KiB      |
//!
//! Indices never straddle two `u64` words, so reads are one shift and one
//! mask (this is the layout Minecraft adopted in 1.16 for the same reason).

use crate::block::BlockId;

pub const SECTION_SIZE: usize = 16;
pub const SECTION_VOLUME: usize = SECTION_SIZE * SECTION_SIZE * SECTION_SIZE;

/// Index of a block inside a section. Y-major order (`y`, then `z`, then `x`)
/// keeps horizontal layers contiguous, which matches how terrain is filled.
#[inline]
pub const fn section_index(x: usize, y: usize, z: usize) -> usize {
    (y << 8) | (z << 4) | x
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PalettedSection {
    palette: Vec<BlockId>,
    bits: u32,
    data: Vec<u64>,
}

impl Default for PalettedSection {
    fn default() -> Self {
        Self::filled(BlockId::default())
    }
}

#[inline]
fn bits_for(len: usize) -> u32 {
    if len <= 1 {
        0
    } else {
        usize::BITS - (len - 1).leading_zeros()
    }
}

#[inline]
fn words_for(bits: u32) -> usize {
    if bits == 0 {
        0
    } else {
        let per_word = 64 / bits as usize;
        SECTION_VOLUME.div_ceil(per_word)
    }
}

impl PalettedSection {
    /// A section where every block is `block`. Costs one palette entry.
    pub fn filled(block: BlockId) -> Self {
        Self { palette: vec![block], bits: 0, data: Vec::new() }
    }

    /// Compresses a dense array of 4096 blocks. This is how the generator
    /// builds sections: fill a plain array (fast, cache friendly), then pack.
    pub fn from_dense(blocks: &[BlockId; SECTION_VOLUME]) -> Self {
        // Distinct values, in order of first appearance. A 64-entry lookup
        // table for low ids avoids hashing in the common case.
        let mut palette: Vec<BlockId> = Vec::new();
        let mut lut = [u16::MAX; 64];
        let mut indices = [0u16; SECTION_VOLUME];
        for (i, &b) in blocks.iter().enumerate() {
            let idx = if (b.0 as usize) < lut.len() && lut[b.0 as usize] != u16::MAX {
                lut[b.0 as usize]
            } else if let Some(p) = palette.iter().position(|&x| x == b) {
                p as u16
            } else {
                palette.push(b);
                let p = (palette.len() - 1) as u16;
                if (b.0 as usize) < lut.len() {
                    lut[b.0 as usize] = p;
                }
                p
            };
            indices[i] = idx;
        }
        let bits = bits_for(palette.len());
        let mut s = Self { data: vec![0; words_for(bits)], palette, bits };
        if bits > 0 {
            for (i, &idx) in indices.iter().enumerate() {
                s.write_index(i, idx as u64);
            }
        }
        s
    }

    #[inline]
    fn read_index(&self, i: usize) -> usize {
        let per_word = 64 / self.bits as usize;
        let word = self.data[i / per_word];
        let shift = (i % per_word) as u32 * self.bits;
        ((word >> shift) & ((1u64 << self.bits) - 1)) as usize
    }

    #[inline]
    fn write_index(&mut self, i: usize, value: u64) {
        let per_word = 64 / self.bits as usize;
        let shift = (i % per_word) as u32 * self.bits;
        let mask = ((1u64 << self.bits) - 1) << shift;
        let w = &mut self.data[i / per_word];
        *w = (*w & !mask) | (value << shift);
    }

    #[inline]
    pub fn get(&self, x: usize, y: usize, z: usize) -> BlockId {
        self.get_index(section_index(x, y, z))
    }

    #[inline]
    pub fn get_index(&self, i: usize) -> BlockId {
        if self.bits == 0 {
            self.palette[0]
        } else {
            self.palette[self.read_index(i)]
        }
    }

    pub fn set(&mut self, x: usize, y: usize, z: usize, block: BlockId) {
        self.set_index(section_index(x, y, z), block);
    }

    pub fn set_index(&mut self, i: usize, block: BlockId) {
        let idx = match self.palette.iter().position(|&b| b == block) {
            Some(p) => p,
            None => {
                self.palette.push(block);
                let needed = bits_for(self.palette.len());
                if needed > self.bits {
                    self.repack(needed);
                }
                self.palette.len() - 1
            }
        };
        if self.bits > 0 {
            self.write_index(i, idx as u64);
        }
    }

    /// Grows the bit width, keeping every stored index.
    fn repack(&mut self, new_bits: u32) {
        let old =
            std::mem::replace(self, Self { palette: Vec::new(), bits: new_bits, data: vec![0; words_for(new_bits)] });
        for i in 0..SECTION_VOLUME {
            let idx = if old.bits == 0 { 0 } else { old.read_index(i) };
            self.write_index(i, idx as u64);
        }
        self.palette = old.palette;
    }

    /// `Some(block)` when the whole section is a single block type. Lets the
    /// mesher and generator skip empty or solid sections in O(1).
    #[inline]
    pub fn uniform(&self) -> Option<BlockId> {
        (self.bits == 0).then(|| self.palette[0])
    }

    pub fn palette(&self) -> &[BlockId] {
        &self.palette
    }

    pub fn bits_per_block(&self) -> u32 {
        self.bits
    }

    /// Expands back into a dense array (used by the mesher).
    pub fn to_dense(&self, out: &mut [BlockId; SECTION_VOLUME]) {
        if self.bits == 0 {
            out.fill(self.palette[0]);
            return;
        }
        let per_word = 64 / self.bits as usize;
        let mask = (1u64 << self.bits) - 1;
        let mut i = 0;
        'outer: for &word in &self.data {
            let mut w = word;
            for _ in 0..per_word {
                out[i] = self.palette[(w & mask) as usize];
                w >>= self.bits;
                i += 1;
                if i == SECTION_VOLUME {
                    break 'outer;
                }
            }
        }
    }

    /// Approximate heap memory used, in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.palette.capacity() * std::mem::size_of::<BlockId>() + self.data.capacity() * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Xoroshiro128PlusPlus;

    #[test]
    fn single_value_costs_nothing() {
        let s = PalettedSection::filled(BlockId(3));
        assert_eq!(s.uniform(), Some(BlockId(3)));
        assert_eq!(s.get(5, 6, 7), BlockId(3));
        assert_eq!(s.bits_per_block(), 0);
    }

    #[test]
    fn set_get_matches_dense_reference() {
        let mut rng = Xoroshiro128PlusPlus::from_seed(11);
        let mut reference = [BlockId(0); SECTION_VOLUME];
        let mut s = PalettedSection::default();
        // Grow through 1, 2, 3, 4, 5 and 6 bits.
        for step in 0..20_000 {
            let i = rng.next_bounded(SECTION_VOLUME as u32) as usize;
            let max_id = 1 + (step / 400).min(40) as u32;
            let b = BlockId(rng.next_bounded(max_id) as u16);
            s.set_index(i, b);
            reference[i] = b;
        }
        for (i, expected) in reference.iter().enumerate() {
            assert_eq!(s.get_index(i), *expected, "index {i}");
        }
        let mut dense = [BlockId(0); SECTION_VOLUME];
        s.to_dense(&mut dense);
        assert_eq!(dense, reference);
    }

    #[test]
    fn from_dense_roundtrip_and_sizes() {
        let mut dense = [BlockId(0); SECTION_VOLUME];
        for (i, b) in dense.iter_mut().enumerate() {
            *b = BlockId((i % 5) as u16 * 7);
        }
        let s = PalettedSection::from_dense(&dense);
        assert_eq!(s.palette().len(), 5);
        assert_eq!(s.bits_per_block(), 3);
        let mut back = [BlockId(0); SECTION_VOLUME];
        s.to_dense(&mut back);
        assert_eq!(back, dense);
        // 3 bits -> 21 per word -> 196 words.
        assert!(s.memory_bytes() < 1700);
    }

    #[test]
    fn bits_for_sizes() {
        assert_eq!(bits_for(1), 0);
        assert_eq!(bits_for(2), 1);
        assert_eq!(bits_for(3), 2);
        assert_eq!(bits_for(4), 2);
        assert_eq!(bits_for(5), 3);
        assert_eq!(bits_for(16), 4);
        assert_eq!(bits_for(17), 5);
    }
}
