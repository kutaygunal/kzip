//! A reusable buffer pool for the read/decode path.
//!
//! The zero-copy read path tries to decode directly from caller-provided
//! slices, but codecs (and the CRC layer) occasionally need a staging buffer.
//! Allocating a fresh `Vec` on every call is wasteful for hot loops that
//! decode many entries. [`BufferPool`] recycles buffers to avoid that
//! per-call allocation while keeping the pool size bounded.

use std::collections::VecDeque;

/// A bounded pool of reusable byte buffers.
///
/// Acquire a `Vec<u8>` with [`BufferPool::acquire`], use it, then return it
/// with [`BufferPool::release`]. Buffers are recycled until `capacity` are in
/// the pool, at which point excess buffers are dropped (the pool never grows
/// without bound).
#[derive(Debug)]
pub struct BufferPool {
    pool: VecDeque<Vec<u8>>,
    capacity: usize,
}

impl BufferPool {
    /// Create an empty pool that will hold at most `capacity` recycled buffers.
    pub fn new(capacity: usize) -> Self {
        BufferPool {
            pool: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Number of buffers currently recycled in the pool.
    pub fn len(&self) -> usize {
        self.pool.len()
    }

    /// Whether the pool currently holds no recycled buffers.
    pub fn is_empty(&self) -> bool {
        self.pool.is_empty()
    }

    /// Acquire a reusable buffer (possibly empty). Reuses a recycled buffer if
    /// one is available, otherwise returns a fresh (empty) allocation.
    pub fn acquire(&mut self) -> Vec<u8> {
        self.pool.pop_back().unwrap_or_default()
    }

    /// Acquire a buffer pre-sized to at least `len` bytes, reusing capacity
    /// where possible. This is the common call on the read path.
    pub fn acquire_len(&mut self, len: usize) -> Vec<u8> {
        let mut buf = self.acquire();
        buf.resize(len, 0);
        buf
    }

    /// Return a buffer to the pool for reuse. Buffers beyond `capacity` are
    /// dropped so the pool stays bounded.
    pub fn release(&mut self, buf: Vec<u8>) {
        if self.pool.len() < self.capacity {
            self.pool.push_back(buf);
        }
    }

    /// Drop all recycled buffers, freeing their memory.
    pub fn clear(&mut self) {
        self.pool.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_reuses_buffers() {
        let mut pool = BufferPool::new(2);
        assert!(pool.is_empty());

        let mut b = pool.acquire_len(1024);
        b[0] = 7;
        assert_eq!(b.len(), 1024);
        assert_eq!(pool.len(), 0);

        pool.release(b);
        assert_eq!(pool.len(), 1);

        // Acquiring again reuses the recycled buffer (same allocation).
        let b2 = pool.acquire();
        assert_eq!(b2.capacity(), 1024);
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn pool_is_bounded() {
        let mut pool = BufferPool::new(3);
        for _ in 0..10 {
            pool.release(vec![0u8; 64]);
        }
        assert_eq!(pool.len(), 3);
        pool.clear();
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn acquire_len_resizes() {
        let mut pool = BufferPool::new(1);
        let b = pool.acquire_len(4096);
        assert_eq!(b.len(), 4096);
        pool.release(b);

        // Reuse then re-size.
        let mut b2 = pool.acquire_len(8192);
        assert_eq!(b2.capacity(), 8192);
        assert_eq!(b2.len(), 8192);
    }

    #[test]
    fn capacity_zero_never_retains_buffers() {
        let mut pool = BufferPool::new(0);
        pool.release(vec![0u8; 16]);
        assert_eq!(
            pool.len(),
            0,
            "a zero-capacity pool must not retain buffers"
        );
        // Acquiring still yields a usable (empty) buffer.
        let b = pool.acquire();
        assert!(b.is_empty());
        // And releasing that acquisition cannot grow the pool.
        pool.release(b);
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn pool_stays_at_capacity_when_overreleased() {
        let mut pool = BufferPool::new(2);
        for i in 0..10u8 {
            pool.release(vec![i; 8]);
        }
        assert_eq!(pool.len(), 2, "over-release must not grow the pool");
        pool.clear();
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn acquire_len_shrinks_reused_buffer() {
        // Reusing a larger buffer with a smaller `acquire_len` must resize down
        // to the requested length while keeping the allocation reusable.
        let mut pool = BufferPool::new(1);
        let big = pool.acquire_len(1000);
        pool.release(big);
        let small = pool.acquire_len(4);
        assert_eq!(small.len(), 4);
        assert!(
            small.capacity() >= 4,
            "capacity must be at least the requested len"
        );
        pool.release(small);
        assert_eq!(pool.len(), 1);
    }
}
