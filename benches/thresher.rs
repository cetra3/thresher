//! The same workloads running through [`Thresher`], so the difference against
//! the `system` target is the wrapper's accounting overhead.
//!
//! The threshold is armed but set out of reach, which is what a deployed
//! process looks like right up until the moment it doesn't: the per-allocation
//! bookkeeping is paid in full, and the threshold comparison is paid once per
//! reconcile, without the callback ever firing.
//!
//! No hard limit is set, so what this measures on that side is the disabled
//! path — a load and a compare against `usize::MAX`. The cost of an *armed*
//! limit is not isolated here.

use criterion::{Criterion, criterion_group, criterion_main};
use thresher::Thresher;

#[global_allocator]
static ALLOCATOR: Thresher<std::alloc::System> = Thresher::new(std::alloc::System);

mod common;

fn benches(c: &mut Criterion) {
    ALLOCATOR.set_callback_threshold(usize::MAX - 1);
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
