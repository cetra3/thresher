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

You can also set a hard limit, after which Thresher will refuse allocations above that size.

```rust
ALLOCATOR.set_hard_limit(100 * 1024 * 1024);
```

Note that setting a hard limit can _easily_ cause a panic in your code and by using it, you should probably use the `try_` methods of collections such as `try_reserve` etc..

## Motivation

Other crates limit the memory in use. They do not tell you what occurred before
the process stops. A diagnostic must allocate more memory. Thus you must keep
some memory free to run it.

Thresher gives you a threshold. When the memory in use goes above the threshold,
your callback can decrease the memory or record the cause.

Examples of use:

- Write a heap profile before an OOM kill. Set the threshold to 90% of the
  memory available.
- Apply back pressure. Slow down or refuse new requests.
- Release large buffers. `reqwest` and `hyper` keep write buffers at their
  maximum size ([hyper#1790](https://github.com/hyperium/hyper/issues/1790)).

## Examples

- [`examples/basic.rs`](examples/basic.rs) is the minimum version.
- [`examples/jemalloc.rs`](examples/jemalloc.rs) writes a heap profile.

## Memory Accounting

In the previous thresher version we used a single atomic usize for the memory threshold counter.  With a lot of small allocations this could actually cause a pretty big degradation in performance as each thread needed to read the value and invalidate cache lines etc...

So in this version, to increase the performance, we use a per-thread local counter that gets reconciled every 64KiB to the "global" max.  This does mean that the threshold can lag behind the global allocated by up to 64KiB * the number of threads.  This is a classic accuracy/performance tradeoff: If you feel like this may cause problems, please raise an issue.

## License

Licensed under either of

- Apache License, Version 2.0
  ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license
  ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
