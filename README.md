# Thresher

A memory allocation wrapper that hits a callback when a threshold is reached:

```rust
#[global_allocator]
static ALLOCATOR: Thresher<alloc::System> = Thresher::new(alloc::System);

fn main() {
    ALLOCATOR.set_callback_threshold(100 * 1024 * 1024);
    ALLOCATOR.set_callback(|allocation| {
        println!("Threshold reached! Allocated: {} bytes", allocation);
    });
}
```

There are two levels, meant to be used together:

* **`set_callback_threshold`** is advisory. Crossing it runs your callback and nothing
  else — dump a heap profile, shed load, drop buffers. Allocation carries on.

* **`set_hard_limit`** refuses. Allocations that would take the process past it
  fail, which ends the process on your terms at a figure you chose rather than
  the kernel's.

```rust
THRESHER.set_callback_threshold(3 * GIB);  // profile here
THRESHER.set_hard_limit(3 * GIB + 512 * MIB);  // refuse here
```

## Upgrading from 0.1

`set_threshold` / `get_threshold` are now `set_callback_threshold` /
`get_callback_threshold`, and the hard limit added alongside them is
`set_hard_limit` / `get_hard_limit`. Two settings both spelled "a number of
bytes you care about" needed names that said which one refuses.

`get_allocated()` also changed behaviour without changing signature: it is now
approximate, lagging by up to 64 KiB per running thread, because the accounting
is batched per thread rather than hitting one shared atomic on every allocation.
See [Accounting & overhead](#accounting--overhead). If you were alerting on that
number, `flush()` folds in the calling thread's balance.

## Motivation

While there are crates to limit and cap memory usage, there are occasions where you want to know what's going on before ending the process.  However, running any sort of diagnostic may require you to allocate *more* memory, which means you do need a little bit of headroom in order to have this be useful. This is what this library is for: having a threshold of memory usage, after which actions can be taken to either reduce memory or provide enough information to know what's going on.

Here are a few uses:

* if you have processes that are being killed by OOM, then you may want to record a heap profile of what's happening.  I.e, set the threshold to 90% of available memory, and have it write a heap dump. This is essentially the main motivation for this library.

* Another situation may be to provide some back pressure or slow down requests to prevent an OOM in the first place.  I.e, if things are happening too quickly.

* You could also use this threshold as an opportunity to dump buffers/drop potential memory hogs.  I.e, `reqwest/hyper` have write buffers that are [never sized down](https://github.com/hyperium/hyper/issues/1790).

## Examples

* [`examples/basic.rs`](examples/basic.rs) example for a bare bones version of this.
* [`examples/jemalloc.rs`](examples/jemalloc.rs) for a way to wire up and have it dump a heap profile.

## The hard limit

Past the limit, `alloc` returns null, which means `handle_alloc_error` and an
abort. That is better than an OOM kill — you find out at a figure you chose, on
your own terms, while the machine is still healthy — but it is still the end of
the process, and the abort message is the allocator's standard
`memory allocation of N bytes failed` rather than anything of ours. Leave
headroom between the threshold and the limit for the callback to do its work.

To survive a refusal rather than die of it, allocate fallibly: `Vec::try_reserve`
and friends turn the null into an `Err` instead of reaching `handle_alloc_error`
at all, so the request that overran the limit fails while the process carries on.

`Enforcement::MarkedThreads` narrows the limit to threads you opt in with
`mark_current_thread(true)` — usually the pool running user work, so the limit
fails on a query rather than on your logging. Every thread is still accounted for
either way.

The limit is not exact. It is compared against the shared total, which lags by up
to 64 KiB per *other* running thread, and nothing locks between the check and the
allocation, so concurrent threads can each pass a check only one of them would
have passed in sequence. Both errors run the same way — the process can sit
somewhat above the limit — so set it far enough below the ceiling you genuinely
cannot cross that a few hundred kilobytes of slack does not matter.

## Accounting & overhead

Every allocation in the process goes through this wrapper, so the accounting has
to be close to free or it becomes the bottleneck it is supposed to be watching.

Each thread keeps its own balance and only reconciles with the shared total once
it has drifted by 64 KiB, the same trick allocators use to keep their arenas
thread local. A single shared counter that every thread has to write to
serialises all of them on one cache line, which shows up as soon as more than
one thread allocates at a time. A thread hands back whatever it is still holding
when it exits, so threads coming and going do not make the total drift.

Two consequences worth knowing about:

* `get_allocated()` is approximate — it can lag by up to 64 KiB per running
  thread. Call `flush()` if you need the calling thread's balance folded in.
  Allocations larger than 64 KiB always reconcile immediately, so a single big
  allocation is never hidden.

* Only what is requested through `GlobalAlloc` is visible. An allocator holding
  freed pages, or mapping an arena up front, is not. If you need the real
  resident figure, poll it out of band and feed it back with `set_allocated()`,
  which will trip the threshold in its own right:

  ```rust
  std::thread::spawn(|| loop {
      std::thread::sleep(Duration::from_millis(50));
      THRESHER.set_allocated(resident_bytes());
  });
  ```

The callback runs inside the allocator, on whichever thread crossed the
threshold. It may allocate — that is what the headroom is for, and its
allocations will not re-enter it — but it must not panic, because unwinding out
of an allocation is undefined behaviour.

## Benchmarks

```bash
cargo bench
```

Two targets run the same workloads, since a binary can only have one
`#[global_allocator]`: `system` on the bare system allocator and `thresher`
through the wrapper. Compare by benchmark id (`system/contention/8` against
`thresher/contention/8`), and use `--save-baseline` / `--baseline` to compare a
change against itself.

The contention benchmark is the one to watch — N threads allocating at once,
sharing no data of their own. What matters there is not the size of the overhead
but whether it *grows* with the thread count, because that is what a single
shared counter would do. It doesn't:

Cost per alloc/free pair relative to the bare system allocator:

| threads | 1 | 2 | 4 | 8 |
| --- | --- | --- | --- | --- |
| per-thread batching | 1.14× | 1.11× | 1.09× | 1.14× |
| one shared counter | 2.27× | 9.49× | 20.8× | 20.8× |

Roughly a tenth on top of each allocation, flat across thread counts. The second
row is the same crate before the accounting was made thread local — a single
`AtomicUsize` taking a `fetch_add` on every allocation, which is what the batching
exists to avoid. It costs 2.3× with one thread just for the atomic, and an order
of magnitude more once threads start fighting over the cache line: 396 ns per
alloc/free pair at 8 threads against the bare allocator's 19 ns.

The isolated per-allocation path (`alloc_free`) costs around 1.4×, more than the
contention figure, because there the bookkeeping cannot amortise across a loop.
In absolute terms it is ~4 ns per alloc/free pair at 16 B and 1 KiB and ~6 ns at
64 KiB — near enough flat in nanoseconds, though not as a ratio, since the
underlying allocation gets dearer with size.

Two caveats on reading these. Thread counts above the machine's core count
measure oversubscription rather than contention — throughput flattens once the
cores are saturated no matter what the allocator does. And absolute nanosecond
figures are not portable between machines: the same binaries measured 10 ns and
17 ns per `malloc` on two different cloud hosts, so compare ratios from a single
interleaved run rather than numbers from different sittings. Run-to-run spread on
the ratio is a few percent; if you need tighter, raise `--measurement-time` and
`--sample-size`.

## License

Licensed under either of

 * Apache License, Version 2.0
   ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
 * MIT license
   ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
