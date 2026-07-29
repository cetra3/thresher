//! The same workloads running through [`Thresher`], so the difference against
//! the `system` target is the wrapper's accounting overhead.
//!
//! The threshold is armed but set out of reach: we want to pay for the
//! accounting and the threshold comparison on every allocation without the
//! callback ever firing, which is what a deployed process looks like right up
//! until the moment it doesn't.

use criterion::{Criterion, criterion_group, criterion_main};
use thresher::Thresher;

#[global_allocator]
static ALLOCATOR: Thresher<std::alloc::System> = Thresher::new(std::alloc::System);

#[path = "common.rs"]
mod common;

fn benches(c: &mut Criterion) {
    ALLOCATOR.set_threshold(usize::MAX - 1);
    ALLOCATOR.set_callback(|allocated| {
        // Never expected to run; if it does the numbers below are meaningless.
        panic!("threshold reached during benchmark: {allocated} bytes");
    });

    common::run(c, "thresher");
}

criterion_group! {
    name = benches_group;
    config = common::config();
    targets = benches
}
criterion_main!(benches_group);
