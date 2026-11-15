mod dense;
mod full;
mod sparse;

pub use dense::{DENSE_BLOCK_NUM_BYTES, DenseBlock, DenseBlockCodec};
pub use full::{FULL_BLOCK_NUM_BYTES, FullBlock, FullBlockCodec};
pub use sparse::{SparseBlock, SparseBlockCodec};

#[cfg(test)]
mod tests;
