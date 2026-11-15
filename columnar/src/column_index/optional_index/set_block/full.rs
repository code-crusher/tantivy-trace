use std::io;

use crate::column_index::optional_index::{SelectCursor, Set, SetCodec};

/// Number of bytes in a full block.
///
/// A full block is a set containing every element of `0..ELEMENTS_PER_BLOCK`, so it does not
/// need to store anything: both `rank` and `select` are the identity, and `contains` always
/// returns `true`.
pub const FULL_BLOCK_NUM_BYTES: u32 = 0;

pub struct FullBlockCodec;

impl SetCodec for FullBlockCodec {
    type Item = u16;
    type Reader<'a> = FullBlock;

    fn serialize(_els: impl Iterator<Item = u16>, _wrt: impl io::Write) -> io::Result<()> {
        Ok(())
    }

    fn open(data: &[u8]) -> Self::Reader<'_> {
        debug_assert!(data.is_empty());
        FullBlock
    }
}

/// A block in which every element of `0..ELEMENTS_PER_BLOCK` is present.
///
/// The block carries no data. `contains` always returns `true`, `rank(el) == el`,
/// `rank_if_exists(el) == Some(el)` and `select(rank) == rank`.
#[derive(Copy, Clone)]
pub struct FullBlock;

impl SelectCursor<u16> for FullBlock {
    #[inline]
    fn select(&mut self, rank: u16) -> u16 {
        rank
    }
}

impl Set<u16> for FullBlock {
    type SelectCursor<'b>
        = Self
    where Self: 'b;

    #[inline(always)]
    fn contains(&self, _el: u16) -> bool {
        true
    }

    #[inline(always)]
    fn rank(&self, el: u16) -> u16 {
        el
    }

    #[inline(always)]
    fn rank_if_exists(&self, el: u16) -> Option<u16> {
        Some(el)
    }

    #[inline(always)]
    fn select(&self, rank: u16) -> u16 {
        rank
    }

    #[inline(always)]
    fn select_cursor(&self) -> Self::SelectCursor<'_> {
        *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_full_block() {
        assert_eq!(FULL_BLOCK_NUM_BYTES, 0);
        let block = FullBlockCodec::open(&[]);
        for el in [0u16, 1, 63, 64, 65_000, u16::MAX] {
            assert!(block.contains(el));
            assert_eq!(block.rank(el), el);
            assert_eq!(block.rank_if_exists(el), Some(el));
            assert_eq!(block.select(el), el);
        }
        let select_cursor = block.select_cursor();
        for rank in [0u16, 1, 63, 64, 65_000, u16::MAX] {
            assert_eq!(select_cursor.select(rank), rank);
        }
    }

    #[test]
    fn test_serialize_empty() {
        let mut buffer = Vec::new();
        FullBlockCodec::serialize(0u16..u16::MAX, &mut buffer).unwrap();
        assert!(buffer.is_empty());
    }
}