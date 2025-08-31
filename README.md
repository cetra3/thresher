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

## Motivation

While there are crates to limit and cap memory usage, there are occasions where you want to know what's going on before ending the process.  However, running any sort of diagnostic may require you to allocate *more* memory, which means you do need a little bit of headroom in order to have this be useful. This is what this library is for: having a threshold of memory usage, after which actions can be taken to either reduce memory or provide enough information to know what's going on.

Here are a few uses:

* if you have processes that are being killed by OOM, then you may want to record a heap profile of what's happening.  I.e, set the threshold to 90% of available memory, and have it write a heap dump. This is essentially the main motivation for this library.

* Another situation may be to provide some back pressure or slow down requests to prevent an OOM in the first place.  I.e, if things are happening too quickly.

* You could also use this threshold as an opportunity to dump buffers/drop potential memory hogs.  I.e, `reqwest/hyper` have write buffers that are [never sized down](https://github.com/hyperium/hyper/issues/1790).

## Examples

* [`examples/basic.rs`](examples/basic.rs) example for a bare bones version of this.
* [`examples/jemalloc.rs`](examples/jemalloc.rs) for a way to wire up and have it dump a heap profile.

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
