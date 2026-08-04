//! Baseline: the workloads running on the bare system allocator.
//!
//! Compare against the `thresher` target to get the wrapper's overhead.

use criterion::{Criterion, criterion_group, criterion_main};

#[global_allocator]
static ALLOCATOR: std::alloc::System = std::alloc::System;

#[path = "common.rs"]
mod common;

fn benches(c: &mut Criterion) {
    common::run(c, "system");
}

criterion_group! {
    name = benches_group;
    config = common::config();
    targets = benches
}
criterion_main!(benches_group);
