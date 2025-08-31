//! A memory allocation wrapper that hits a callback when a threshold is reached:
//!
//! ```rust
//! use std::alloc;
//! use thresher::Thresher;
//!
//! #[global_allocator]
//! // Wrap the standard system allocator
//! static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);
//!
//! fn main() {
//!
//!     // Set the threshold we care about reaching
//!     THRESHER.set_threshold(100 * 1024 * 1024);
//!
//!     // Set the callback when the threshold is reached (note: may be called multiple times)
//!     THRESHER.set_callback(|allocation| {
//!         println!("Threshold reached! Allocated: {} bytes", allocation);
//!     });
//!
//! }
//! ```

use std::{
    alloc::{GlobalAlloc, Layout},
    sync::{
        OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

/// The main allocation wrapper. [`Thresher::new()`] to wrap an existing allocator
pub struct Thresher<A> {
    allocator: A,
    allocated: AtomicUsize,
    threshold: AtomicUsize,
    callback: OnceLock<Box<dyn Fn(usize) + Send + Sync>>,
}

impl<A> Thresher<A> {
    /// Create a Thresher allocator that wraps an existing allocator
    ///
    /// This is meant to be used with the `#[global_allocator]` attribute
    ///
    /// If you don't use a custom allocator you can import [`std::alloc::System`]
    ///
    ///
    /// ```rust
    /// use std::alloc;
    /// use thresher::Thresher;
    ///
    /// #[global_allocator]
    /// static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);
    ///
    /// ```
    ///
    pub const fn new(allocator: A) -> Self {
        Self {
            allocator,
            allocated: AtomicUsize::new(0),
            threshold: AtomicUsize::new(usize::MAX),
            callback: OnceLock::new(),
        }
    }

    /// Set or update the memory `threshold` in bytes.
    /// When an allocation goes above this value, then
    /// The callback, if set, will be executed
    ///
    /// If threshold is not set, or set to `usize::MAX` this disables the callback.
    /// ```rust
    /// # use std::alloc;
    /// # use thresher::Thresher;
    /// # #[global_allocator]
    /// # static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);
    /// fn main() {
    ///     THRESHER.set_threshold(100 * 1024 * 1024);
    /// }
    /// ```
    ///
    pub fn set_threshold(&self, threshold: usize) {
        self.threshold.store(threshold, Ordering::Release);
    }

    /// Set the callback to execute when the threshold is reached.
    /// This callback may be called multiple times if the allocation threshold is reached and then reduced.
    ///
    /// As this callback happens when allocating, you need to ensure that it happens rather quickly, as to not block running code.
    ///
    /// Panics if set more than once.
    /// ```rust
    /// # use std::alloc;
    /// # use thresher::Thresher;
    /// # #[global_allocator]
    /// # static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);
    /// fn main() {
    ///     THRESHER.set_callback(|allocation| {
    ///         println!("Threshold reached! Allocated: {} bytes", allocation);
    ///     });
    /// }
    /// ```
    pub fn set_callback<F>(&self, callback: F)
    where
        F: Fn(usize) + Send + Sync + 'static,
    {
        self.callback
            .set(Box::new(callback))
            .map_err(drop)
            .expect("Callback is already registered");
    }

    fn maybe_callback(&self, allocation_size: usize) {
        let threshold = self.threshold.load(Ordering::Acquire);
        let old_allocated = self.allocated.fetch_add(allocation_size, Ordering::Release);
        let new_allocated = old_allocated + allocation_size;

        // only execute call back when we've passed the threshold
        if new_allocated >= threshold
            && old_allocated < threshold
            && let Some(cb) = self.callback.get()
        {
            cb(new_allocated);
        }
    }
}

unsafe impl<A: GlobalAlloc> GlobalAlloc for Thresher<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.allocator.alloc(layout) };

        if !ptr.is_null() {
            self.maybe_callback(layout.size());
        }

        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.allocator.dealloc(ptr, layout) };
        let size = layout.size();
        self.allocated.fetch_sub(size, Ordering::Release);
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.allocator.alloc_zeroed(layout) };

        if !ptr.is_null() {
            self.maybe_callback(layout.size());
        }

        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, old_layout: Layout, size: usize) -> *mut u8 {
        let new_ptr = unsafe { self.allocator.realloc(ptr, old_layout, size) };

        if !new_ptr.is_null() {
            let old_size = old_layout.size();
            let new_size =
                unsafe { Layout::from_size_align_unchecked(size, old_layout.align()) }.size();

            if new_size > old_size {
                let allocation_size = new_size - old_size;
                self.maybe_callback(allocation_size);
            } else {
                self.allocated
                    .fetch_sub(old_size - new_size, Ordering::Release);
            }
        }

        new_ptr
    }
}

#[cfg(test)]
mod tests {

    use std::{
        alloc,
        sync::{Arc, atomic::AtomicBool},
    };

    use super::*;

    #[global_allocator]
    static ALLOCATOR: Thresher<alloc::System> = Thresher::new(alloc::System);

    #[test]
    fn simple() {
        let flag = Arc::new(AtomicBool::new(false));
        let cb_flag = flag.clone();

        ALLOCATOR.set_threshold(1024 * 1024);
        ALLOCATOR.set_callback(move |_| {
            cb_flag.store(true, Ordering::Release);
        });

        assert!(!flag.load(Ordering::Acquire));
        let _bytes = vec![0u8; 1024 * 1024];
        assert!(flag.load(Ordering::Acquire));
    }
}
