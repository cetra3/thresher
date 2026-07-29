# Thresher

A memory allocation wrapper that hits a callback when a threshold is reached:

```rust
#[global_allocator]
static ALLOCATOR: Thresher<alloc::System> = Thresher::new(alloc::System);

fn main() {
    ALLOCATOR.set_threshold(100 * 1024 * 1024);
    ALLOCATOR.set_callback(|allocation| {
        println!("Threshold reached! Allocated: {} bytes", allocation);
    });
}
```

There are two levels, meant to be used together:

* **`set_threshold`** is advisory. Crossing it runs your callback and nothing
  else — dump a heap profile, shed load, drop buffers. Allocation carries on.

* **`set_limit`** is a hard cap. Allocations that would take the process past it
  fail, which — with an alloc error hook — becomes a panic you can catch at a
  job boundary instead of an OOM kill you can't.

```rust
THRESHER.set_threshold(3 * GIB);  // profile here
THRESHER.set_limit(3 * GIB + 512 * MIB);  // refuse here
```

## Motivation

While there are crates to limit and cap memory usage, there are occasions where you want to know what's going on before ending the process.  However, running any sort of diagnostic may require you to allocate *more* memory, which means you do need a little bit of headroom in order to have this be useful. This is what this library is for: having a threshold of memory usage, after which actions can be taken to either reduce memory or provide enough information to know what's going on.

Here are a few uses:

* if you have processes that are being killed by OOM, then you may want to record a heap profile of what's happening.  I.e, set the threshold to 90% of available memory, and have it write a heap dump. This is essentially the main motivation for this library.

* Another situation may be to provide some back pressure or slow down requests to prevent an OOM in the first place.  I.e, if things are happening too quickly.

* You could also use this threshold as an opportunity to dump buffers/drop potential memory hogs.  I.e, `reqwest/hyper` have write buffers that are [never sized down](https://github.com/hyperium/hyper/issues/1790).

## Examples

* [`examples/basic.rs`](examples/basic.rs) example for a bare bones version of this.
* [`examples/jemalloc.rs`](examples/jemalloc.rs) for a way to wire up and have it dump a heap profile.
* [`examples/hard_limit.rs`](examples/hard_limit.rs) for the hard cap end to end: a
  worker that survives a job too big to run.

## The hard limit

Past the limit, `alloc` returns null. On its own that means `handle_alloc_error`,
which aborts — already better than an OOM kill, because you get a message on your
own terms at a limit you chose, but still a dead process.

To survive it, a process needs three things:

1. **An alloc error hook that panics.** Panicking from inside the allocator is
   undefined behaviour, but by the time the hook runs the allocator frame has
   returned and you are in ordinary safe code. `take_refusal()` tells the hook
   whether it was this limit or a genuine out of memory.

2. **A `catch_unwind` at the job boundary**, so the panic fails one request
   rather than the thread.

3. **Room to breathe** — unwinding and reporting both allocate, so the limit
   belongs below whatever will actually kill you. The first refusal on a thread
   is one-shot for this reason: once refused, the thread is let through again so
   it can unwind and report, and enforcement re-arms when the total drops back
   under (or when you call `rearm_current_thread()`).

`Enforcement::MarkedThreads` narrows the limit to threads you opt in with
`mark_current_thread(true)` — usually the pool running user work, so the cap
fails a query rather than your logging. Every thread is still accounted for
either way.

```bash
cargo run --example hard_limit --features alloc-error-hook
```

```text
small-scan: ok, touched 8 MiB
[thresher] threshold reached at 96 MiB
heavy-aggregate: ok, touched 96 MiB
runaway-join: refused (thresher refused an allocation of 536870912 bytes (536872064 bytes allocated))
small-scan-again: ok, touched 8 MiB

still running, 1 KiB allocated
```

### Stable or nightly?

**Thresher is stable Rust, and the default build has no nightly anything in it.**
`set_alloc_error_hook` is the one unstable piece, and `#![feature(..)]` gates
apply to the crate that uses them — so the requirement lands on whoever calls it,
not on everybody.

| | needs | you get |
| --- | --- | --- |
| default | stable | threshold callback, accounting, and a hard limit that **aborts** — with your message, at your limit |
| write your own hook | `#![feature(alloc_error_hook)]` in *your* crate root, so nightly | the limit becomes a catchable panic, with full control over the message |
| `features = ["alloc-error-hook"]` | nightly to build | `thresher::install_alloc_error_hook()` does it for you — **your crate needs no `#![feature]`** |

The refusal is recorded in a thread local rather than on the allocator, so the
hook needs to capture nothing and thresher can install it on your behalf. That
moves the feature gate into thresher's crate root instead of yours.

Aborting is not nothing, if you would rather stay on stable: you find out at a
limit you chose, on your own terms, with a message you wrote, instead of the
kernel removing the process silently. You just do not get to keep serving.

`RUSTC_BOOTSTRAP=1` will make a stable compiler accept the feature gate. It is
the compiler's internal bootstrap escape hatch rather than a supported route, and
it can break without notice — fine for a spike, worth deciding deliberately
before it reaches production.

## Accounting & overhead

Every allocation in the process goes through this wrapper, so the accounting has
to be close to free or it becomes the bottleneck it is supposed to be watching.

Each thread keeps its own balance and only reconciles with the shared total once
it has drifted by 64 KiB, the same trick allocators use to keep their arenas
thread local. A single shared counter that every thread has to write to
serialises all of them on one cache line, which shows up as soon as more than
one thread allocates at a time.

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
sharing no data of their own. Thresher currently tracks the bare allocator
within noise up to 8 threads.

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
