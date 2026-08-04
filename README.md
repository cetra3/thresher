# Thresher

Thresher is a wrapper for a memory allocator. It calls your callback when the
memory in use goes above a threshold.

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

Thresher has two levels. Use them together.

* `set_callback_threshold` is advisory. Thresher calls your callback. The
  allocation continues.
* `set_hard_limit` refuses. Thresher fails each allocation that goes above the
  limit.

```rust
THRESHER.set_callback_threshold(3 * GIB);      // profile here
THRESHER.set_hard_limit(3 * GIB + 512 * MIB);  // refuse here
```

## Changes in 0.2

`set_threshold` and `get_threshold` are now `set_callback_threshold` and
`get_callback_threshold`. The hard limit is `set_hard_limit` and
`get_hard_limit`. The two settings are both a number of bytes, thus the names
must tell you which one refuses.

`get_allocated()` is now approximate. It can lag by a maximum of 64 KiB for each
thread. The signature is the same. See [Accounting](#accounting).

## Motivation

Other crates limit the memory in use. They do not tell you what occurred before
the process stops. A diagnostic must allocate more memory. Thus you must keep
some memory free to run it.

Thresher gives you a threshold. When the memory in use goes above the threshold,
your callback can decrease the memory or record the cause.

Examples of use:

* Write a heap profile before an OOM kill. Set the threshold to 90% of the
  memory available.
* Apply back pressure. Slow down or refuse new requests.
* Release large buffers. `reqwest` and `hyper` keep write buffers at their
  maximum size ([hyper#1790](https://github.com/hyperium/hyper/issues/1790)).

## Examples

* [`examples/basic.rs`](examples/basic.rs) is the minimum version.
* [`examples/jemalloc.rs`](examples/jemalloc.rs) writes a heap profile.

## The hard limit

If an allocation goes above the hard limit, `alloc` returns null. Rust then
calls `handle_alloc_error`, which stops the process. This is better than an OOM
kill, because the limit is yours and the machine stays serviceable. But the
process stops. Keep memory free between the threshold and the limit for the
callback. The message is the standard Rust message,
`memory allocation of N bytes failed`.

To continue after a refusal, use `Vec::try_reserve` or an equivalent function.
These functions return an `Err`. They do not call `handle_alloc_error`.

`Enforcement::MarkedThreads` applies the limit only to the threads that call
`mark_current_thread(true)`. Use it for the threads that do user work. Thresher
counts the memory of all threads in all conditions.

The hard limit is not exact. Thresher compares the limit with the shared total.
The shared total lags by a maximum of 64 KiB for each other thread. Thresher
does not lock between the comparison and the allocation. Thus two threads can go
above the limit together. Set the limit a minimum of a few hundred kilobytes
below the maximum that the process must not exceed.

## Accounting

Each allocation in the process goes through Thresher. Thus the accounting must
be fast.

Each thread keeps its own balance. The thread adds its balance to the shared
total only when the balance goes above 64 KiB. One shared counter makes all
threads write to one cache line, which is slow. When a thread stops, it adds its
balance to the shared total.

There are two results:

* `get_allocated()` is approximate. It can lag by a maximum of 64 KiB for each
  thread. Call `flush()` to add the balance of the current thread. Thresher
  records each allocation of more than 64 KiB immediately.

* Thresher sees only the requests through `GlobalAlloc`. It does not see the
  free pages that the allocator keeps, or an arena. For the true resident
  memory, read the value from the operating system and give it to
  `set_allocated()`:

  ```rust
  std::thread::spawn(|| loop {
      std::thread::sleep(Duration::from_millis(50));
      THRESHER.set_allocated(resident_bytes());
  });
  ```

The callback runs in the allocator, on the thread that went above the threshold.
The callback can allocate memory, and Thresher does not call the callback again
for those allocations. The hard limit does not apply to the callback, thus you
must keep its allocations small. The callback must not panic, because a panic in
an allocation is undefined behavior.

## Benchmarks

```bash
cargo bench
```

There are two targets, because a program can have only one
`#[global_allocator]`. The `system` target uses the system allocator. The
`thresher` target uses the wrapper. Compare the two by benchmark id, for example
`system/contention/8` against `thresher/contention/8`. Use `--save-baseline` and
`--baseline` to compare a change with an earlier result.

The `contention` benchmark is the important one. It runs N threads that allocate
memory at the same time. The overhead must stay constant as N increases. One
shared counter does not stay constant.

Overhead for each alloc/free pair, above the system allocator:

| threads | 1 | 2 | 4 | 8 |
| --- | --- | --- | --- | --- |
| 0.2, one balance for each thread | 1.6 ns | 1.6 ns | 1.6 ns | 1.5 ns |
| 0.1, one shared counter | 5.6 ns | 35.2 ns | 75.1 ns | 158.1 ns |

Measured on a 16-core AMD Ryzen 9 9950X3D with glibc 2.44. The overhead of 0.2
is constant. The overhead of 0.1 increases with each thread, because all threads
write to one cache line.

The `alloc_free` benchmark has no loop to amortize the accounting. Its overhead
is 2.0 ns for 16 B and 1 KiB, and 2.8 ns for 64 KiB.

Two limits on these results:

* Do not compare times from different machines. The same code measured 1.6 ns of
  overhead here and approximately 2 ns on a 4-core Xeon. The ratio to the system
  allocator changes more. A fast allocator makes the same overhead a larger part
  of the total.
* More threads than cores measures oversubscription, not contention.

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
