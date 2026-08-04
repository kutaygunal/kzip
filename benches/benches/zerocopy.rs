//! Zero-copy read-path memory benchmark.
//!
//! Measures the bytes allocated while decoding a compressed entry directly from
//! a borrowed `&[u8]` slice (the zero-copy path) versus a path that first
//! copies the compressed input into an owned buffer (simulating a non-buffer
//! source that materializes a copy). A counting global allocator records
//! allocations so the memory footprint is reported alongside timing.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use zip_core::constant::CompressionMethod;
use zip_core::{compress_bytes, decode_slice_into};

/// Minimal counting allocator that tracks total bytes handed out.
struct CountingAlloc;
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

// SAFETY: `CountingAlloc` forwards to `System` and only adds an atomic counter;
// it is a well-behaved `GlobalAlloc` (no aliasing or layout mismatches beyond
// what `System` itself guarantees). This benchmark crate is *not* zip-core /
// zip-async, which both remain `unsafe`-free.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(l.size(), Ordering::Relaxed);
        System.alloc(l)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(l.size(), Ordering::Relaxed);
        System.alloc_zeroed(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCATED.fetch_add(n.saturating_sub(l.size()), Ordering::Relaxed);
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static A: CountingAlloc = CountingAlloc;

fn bench_zerocopy(c: &mut Criterion) {
    let data = b"zero copy decode benchmark payload with repeated text ".repeat(20_000);
    let comp = compress_bytes(&data, CompressionMethod::Deflate, 6).unwrap();
    let method = CompressionMethod::Deflate;
    let clen = comp.len() as u64;

    // 1. Zero-copy: decode directly from the borrowed `comp` slice.
    c.bench_function("decode_zero_copy_slice", |b| {
        b.iter_batched(
            || Vec::<u8>::new(),
            |mut out| {
                decode_slice_into(&comp, method, clen, &mut out).unwrap();
                out
            },
            BatchSize::SmallInput,
        );
    });
    let mut out = Vec::new();
    ALLOCATED.store(0, Ordering::Relaxed);
    decode_slice_into(&comp, method, clen, &mut out).unwrap();
    eprintln!(
        "zero-copy: decoded {} -> {} bytes allocating ~{} bytes (input slice borrowed, no copy)",
        comp.len(),
        out.len(),
        ALLOCATED.load(Ordering::Relaxed)
    );

    // 2. Copy path: materialize an owned copy of the compressed input first,
    //    as a non-buffer source must do before decoding.
    c.bench_function("decode_copied_input", |b| {
        b.iter_batched(
            || (comp.clone(), Vec::<u8>::new()),
            |(owned, mut out)| {
                decode_slice_into(&owned, method, clen, &mut out).unwrap();
                out
            },
            BatchSize::SmallInput,
        );
    });
    let mut out2 = Vec::new();
    ALLOCATED.store(0, Ordering::Relaxed);
    let owned = comp.clone(); // counted: a non-buffer source must copy input
    decode_slice_into(&owned, method, clen, &mut out2).unwrap();
    eprintln!(
        "copied-input: decoded {} -> {} bytes allocating ~{} bytes (input copy included)",
        owned.len(),
        out2.len(),
        ALLOCATED.load(Ordering::Relaxed)
    );

    // 3. BufferPool reuse: reuse a pre-sized output buffer across iterations to
    //    avoid repeated reallocation on the hot loop.
    let mut pool = zip_core::BufferPool::new(4);
    c.bench_function("decode_with_buffer_pool", |b| {
        b.iter_batched(
            || pool.acquire_len(data.len()),
            |mut buf| {
                decode_slice_into(&comp, method, clen, &mut buf).unwrap();
                buf
            },
            BatchSize::PerIteration,
        );
    });
}

criterion_group!(benches, bench_zerocopy);
criterion_main!(benches);
