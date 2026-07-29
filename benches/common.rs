//! Workloads shared by the `system` and `thresher` bench targets.
//!
//! A binary can only have one `#[global_allocator]`, so the two targets run the
//! *same* workloads under different allocators and the results are compared by
//! benchmark id: `system/alloc_free/16` vs `thresher/alloc_free/16`, etc.

use std::hint::black_box;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput};

/// Alloc/free pairs each contended thread performs per criterion iteration.
///
/// Large enough that spawning the threads is noise, small enough that a sample
/// stays in the millisecond range.
const OPS_PER_ITER: usize = 2_000;

/// Allocate a block of `size` bytes and immediately free it.
#[inline]
fn alloc_free(size: usize) {
    let v: Vec<u8> = Vec::with_capacity(size);
    black_box(v.as_ptr());
    drop(black_box(v));
}

/// Push `n` elements onto an empty vec, exercising the `realloc` path.
#[inline]
fn vec_grow(n: usize) {
    let mut v: Vec<u64> = Vec::new();
    for i in 0..n {
        v.push(i as u64);
    }
    black_box(&v);
}

/// Hold `count` live allocations of `size` bytes at once, then drop them all.
///
/// `vec![0u8; n]` goes through `alloc_zeroed`, and keeping the blocks live means
/// the allocated total ratchets up rather than oscillating around zero.
#[inline]
fn batch_retain(count: usize, size: usize) {
    let mut held: Vec<Vec<u8>> = Vec::with_capacity(count);
    for _ in 0..count {
        held.push(black_box(vec![0u8; size]));
    }
    black_box(&held);
}

/// Single threaded alloc/dealloc of a few representative sizes.
fn bench_alloc_free(c: &mut Criterion, prefix: &str) {
    let mut group = c.benchmark_group(format!("{prefix}/alloc_free"));
    for size in [16usize, 1024, 65536] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| alloc_free(size));
        });
    }
    group.finish();
}

fn bench_patterns(c: &mut Criterion, prefix: &str) {
    c.bench_function(&format!("{prefix}/vec_grow/4096"), |b| {
        b.iter(|| vec_grow(4096));
    });
    c.bench_function(&format!("{prefix}/batch_retain/1024x256"), |b| {
        b.iter(|| batch_retain(1024, 256));
    });
}

/// The one that matters: N threads allocating at once.
///
/// A global allocator that keeps its accounting in a single shared counter
/// serialises every thread on that cache line, so the per-op cost should climb
/// with the thread count even though the threads share no data of their own.
fn bench_contention(c: &mut Criterion, prefix: &str) {
    let mut group = c.benchmark_group(format!("{prefix}/contention"));

    for threads in [1usize, 2, 4, 8] {
        group.throughput(Throughput::Elements((threads * OPS_PER_ITER) as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    // +1 for this thread, which acts as the starter and the clock.
                    let start_line = Arc::new(Barrier::new(threads + 1));
                    let finish_line = Arc::new(Barrier::new(threads + 1));

                    let handles: Vec<_> = (0..threads)
                        .map(|_| {
                            let start_line = Arc::clone(&start_line);
                            let finish_line = Arc::clone(&finish_line);
                            thread::spawn(move || {
                                start_line.wait();
                                for _ in 0..iters {
                                    for _ in 0..OPS_PER_ITER {
                                        alloc_free(64);
                                    }
                                }
                                finish_line.wait();
                            })
                        })
                        .collect();

                    start_line.wait();
                    let started = Instant::now();
                    finish_line.wait();
                    let elapsed = started.elapsed();

                    for handle in handles {
                        handle.join().unwrap();
                    }

                    elapsed
                });
            },
        );
    }

    group.finish();
}

/// Run every workload, tagging each benchmark id with `prefix`.
pub fn run(c: &mut Criterion, prefix: &str) {
    bench_alloc_free(c, prefix);
    bench_patterns(c, prefix);
    bench_contention(c, prefix);
}

/// Shorter than criterion's defaults — these workloads are cheap and very
/// repeatable, and the contention benches spawn threads on every sample.
pub fn config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2))
        .sample_size(50)
}
