// Only reached with the `alloc-error-hook` cargo feature, which is off by
// default: the crate builds on stable Rust unless you ask for the hook.
#![cfg_attr(feature = "alloc-error-hook", feature(alloc_error_hook))]

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
//!
//! # Accounting
//!
//! Every allocation and deallocation in the process passes through here, so the
//! bookkeeping has to be close to free or it becomes the bottleneck it is
//! supposed to be watching.
//!
//! Rather than touch a shared counter on every call, each thread keeps its own
//! running balance and only reconciles with the shared total once it has drifted
//! by [`RECONCILE_BATCH`] bytes. Allocators keep their arenas thread local for
//! the same reason: a single atomic that every thread has to write to serialises
//! all of them on one cache line.
//!
//! The consequence is that [`Thresher::get_allocated`] is *approximate*. It can
//! lag by up to [`RECONCILE_BATCH`] bytes per running thread, and a thread that
//! exits while holding a balance never reconciles it, so the total drifts a
//! little over the life of a process that churns through threads. For picking a
//! threshold in the hundreds of megabytes this does not matter; if you need the
//! total to be exact at a point in time, call [`Thresher::flush`] from the thread
//! you care about.
//!
//! Note also that this only ever sees what is requested through
//! [`GlobalAlloc`] — an allocator holding on to freed pages, or mapping a large
//! arena up front, is invisible here. If you need the real resident figure you
//! can poll it out of band (`/proc/self/statm`, or your allocator's own stats)
//! and feed it back in with [`Thresher::set_allocated`].
//!
//! # Two limits
//!
//! There are two independent levels, and they are meant to be used together:
//!
//! * The **threshold** ([`Thresher::set_threshold`]) is advisory. Crossing it
//!   runs your callback and nothing else — dump a heap profile, shed load, drop
//!   caches. Allocation carries on.
//!
//! * The **limit** ([`Thresher::set_limit`]) is a hard cap. Allocations that
//!   would take the process past it fail: [`GlobalAlloc::alloc`] returns null,
//!   and Rust turns that into `handle_alloc_error`. With a
//!   [`std::alloc::set_alloc_error_hook`] installed you get to turn it into a
//!   panic, which unwinds the offending task and leaves the process running.
//!
//! Set the threshold below the limit and you get a warning shot before anything
//! starts failing:
//!
//! ```rust
//! # use std::alloc;
//! # use thresher::Thresher;
//! # #[global_allocator]
//! # static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);
//! THRESHER.set_threshold(3 * 1024 * 1024 * 1024); // profile at 3 GiB
//! THRESHER.set_limit(3 * 1024 * 1024 * 1024 + 512 * 1024 * 1024); // refuse at 3.5 GiB
//! ```
//!
//! See [`Thresher::set_limit`] for what a process needs to do to make the hard
//! cap survivable rather than merely a tidier way to die.

use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// How far a thread's own balance may drift from the shared total before it
/// reconciles.
///
/// Allocations larger than this always reconcile immediately, so a single big
/// allocation is never hidden by batching.
pub const RECONCILE_BATCH: usize = 64 * 1024;

/// Everything the allocator tracks per thread.
///
/// Deliberately one thread local rather than several. Resolving a thread local
/// is not free — on macOS it is a call into `_tlv_get_addr` — and the allocation
/// path reads more than one of these fields, so they share a single lookup and
/// the fields are reached through the resulting reference.
///
/// `Cell` of `Copy` types with a `const` initialiser, so access needs no lazy
/// initialisation and registers no destructor. Neither would be safe to do from
/// inside an allocator, and both would recurse.
struct ThreadState {
    /// Allocated bytes the shared counter has not seen yet.
    balance: Cell<isize>,

    /// The shared total was at or over the hard limit at the last reconcile.
    ///
    /// Checked on every allocation, so it has to be local: reading the shared
    /// total instead would put that cache line back in the hot path, which is
    /// the thing the batching exists to avoid.
    over_limit: Cell<bool>,

    /// Set while this thread is inside the threshold callback.
    in_callback: Cell<bool>,

    /// An allocation was refused, and this was the running total at the time.
    /// See [`Thresher::take_refusal`].
    refusal: Cell<Option<usize>>,

    /// This thread has been refused and is being let through until it recovers.
    ///
    /// Kept separate from `refusal` on purpose. The alloc error hook takes the
    /// refusal and then panics, and panicking allocates — so if taking the
    /// report also re-armed enforcement, the panic would be refused in turn and
    /// abort mid-unwind, which is the outcome the limit exists to avoid.
    grace: Cell<bool>,

    /// Whether this thread has opted in under [`Enforcement::MarkedThreads`].
    marked: Cell<bool>,
}

impl ThreadState {
    const fn new() -> Self {
        Self {
            balance: Cell::new(0),
            over_limit: Cell::new(false),
            in_callback: Cell::new(false),
            refusal: Cell::new(None),
            grace: Cell::new(false),
            marked: Cell::new(false),
        }
    }
}

thread_local! {
    static STATE: ThreadState = const { ThreadState::new() };
}

/// Which threads the hard limit is allowed to fail allocations on.
///
/// See [`Thresher::set_enforcement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Enforcement {
    /// Any thread that allocates past the limit is refused.
    #[default]
    AllThreads,
    /// Only threads that have called [`Thresher::mark_current_thread`].
    MarkedThreads,
}

/// Install a [`std::alloc::set_alloc_error_hook`] that turns a refusal by the
/// hard limit into a panic.
///
/// Requires the `alloc-error-hook` cargo feature, and so a nightly compiler —
/// `set_alloc_error_hook` is still unstable. The point of it living here rather
/// than in your code is that `#![feature(..)]` gates apply to the crate that
/// uses them: with this, the gate is in *thresher's* crate root and yours needs
/// no `#![feature]` attribute of its own.
///
/// Call it once at startup, alongside [`Thresher::set_limit`]:
///
/// ```rust,no_run
/// # use std::alloc;
/// # use thresher::Thresher;
/// # #[global_allocator]
/// # static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);
/// THRESHER.set_limit(4 * 1024 * 1024 * 1024);
/// thresher::install_alloc_error_hook();
/// ```
///
/// Allocation failures that were *not* this limit are left alone: the message
/// goes to stderr and the process aborts, which is the right answer when the
/// machine really has run out of memory. Write the hook yourself with
/// [`Thresher::take_refusal`] if you want different wording, structured logging,
/// or a payload your `catch_unwind` can downcast.
#[cfg(feature = "alloc-error-hook")]
pub fn install_alloc_error_hook() {
    std::alloc::set_alloc_error_hook(|layout| {
        // Reads the same thread local as `Thresher::take_refusal`; the record
        // sits on the thread, not the allocator, so this needs no captures and
        // coerces to the plain `fn(Layout)` the hook wants.
        let Some(allocated) = STATE.with(|state| state.refusal.take()) else {
            // Not us. Returning lets the default abort stand.
            eprintln!("memory allocation of {} bytes failed", layout.size());
            return;
        };

        panic!(
            "thresher refused an allocation of {} bytes ({allocated} bytes allocated)",
            layout.size()
        );
    });
}

/// The main allocation wrapper. [`Thresher::new()`] to wrap an existing allocator
pub struct Thresher<A> {
    allocator: A,
    allocated: AtomicUsize,
    threshold: AtomicUsize,
    limit: AtomicUsize,
    marked_threads_only: AtomicBool,
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
            limit: AtomicUsize::new(usize::MAX),
            marked_threads_only: AtomicBool::new(false),
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
        self.threshold.store(threshold, Ordering::Relaxed);
    }

    /// Set the callback to execute when the threshold is reached.
    /// This callback may be called multiple times if the allocation threshold is reached and then reduced.
    ///
    /// As this callback happens when allocating, you need to ensure that it happens rather quickly, as to not block running code.
    ///
    /// The callback runs inside the allocator, on whichever thread happened to
    /// cross the threshold, so there are two things it must not do:
    ///
    /// * **Panic.** Unwinding out of an allocation is undefined behaviour. Keep
    ///   the callback infallible; anything that might fail belongs on the other
    ///   side of a channel, as in the `jemalloc` example.
    /// * **Block on anything that allocates.** Taking a lock that another thread
    ///   holds while it is itself allocating will deadlock.
    ///
    /// The callback is free to allocate — that is what the headroom below the
    /// threshold is for — and allocations it makes will not re-enter it.
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

    /// Returns the approximate number of bytes allocated.
    ///
    /// This can lag by up to [`RECONCILE_BATCH`] bytes per running thread; see
    /// the module docs. Call [`Thresher::flush`] first to fold in the calling
    /// thread's own outstanding balance.
    pub fn get_allocated(&self) -> usize {
        self.allocated.load(Ordering::Relaxed)
    }

    /// Returns the current threshold.
    pub fn get_threshold(&self) -> usize {
        self.threshold.load(Ordering::Relaxed)
    }

    /// Set the hard `limit` in bytes, above which allocations start failing.
    ///
    /// Where [`set_threshold`](Thresher::set_threshold) asks nicely, this one
    /// says no: an allocation that would take the process past the limit gets a
    /// null pointer back. Set it to `usize::MAX` (the default) to disable.
    ///
    /// On its own, a null from the allocator means `handle_alloc_error`, which
    /// aborts. That is already an improvement on being OOM killed — you get a
    /// message, on your own terms, at a threshold you chose — but it is still a
    /// dead process. To survive it, a process needs three things:
    ///
    /// 1. **An alloc error hook that panics.** Panicking from inside the
    ///    allocator is undefined behaviour, but by the time the hook runs the
    ///    allocator frame has returned and you are in ordinary safe code, so a
    ///    panic there is fine. [`take_refusal`](Thresher::take_refusal) tells the
    ///    hook whether the failure was this limit or a genuine out of memory.
    ///    (`set_alloc_error_hook` is still unstable; see `examples/hard_limit.rs`.)
    ///
    /// 2. **Something to catch the unwind.** A `catch_unwind` around each unit of
    ///    work — a request, a query — turns the panic into an error for that one
    ///    caller. Without it the panic just takes the thread down instead.
    ///
    /// 3. **Room to breathe.** Set the limit below whatever will actually kill
    ///    you (the cgroup limit, the machine), because unwinding and reporting
    ///    both need to allocate. The first refusal on a thread is one-shot for
    ///    exactly this reason: once refused, that thread is allowed to allocate
    ///    again so it can unwind and tell you what happened, and it re-arms when
    ///    the total drops back under the limit.
    ///
    /// Consider [`set_enforcement`](Thresher::set_enforcement) as well, so the
    /// limit fails the work you can afford to lose rather than your logging.
    ///
    /// ```rust
    /// # use std::alloc;
    /// # use thresher::Thresher;
    /// # #[global_allocator]
    /// # static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);
    /// THRESHER.set_limit(4 * 1024 * 1024 * 1024);
    /// ```
    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    /// Returns the current hard limit.
    pub fn get_limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    /// Choose which threads the hard limit may fail allocations on.
    ///
    /// Defaults to [`Enforcement::AllThreads`], which is the honest reading of
    /// "hard limit" but will happily refuse your logging, your metrics and your
    /// health check along with the work that caused the problem.
    ///
    /// A server that wants to stay up should usually run in
    /// [`Enforcement::MarkedThreads`] and mark only the pool it runs user work
    /// on. Every thread is still *accounted* for — the total is the whole
    /// process either way — but only marked threads can be refused:
    ///
    /// ```rust,no_run
    /// # use std::alloc;
    /// # use thresher::{Enforcement, Thresher};
    /// # #[global_allocator]
    /// # static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);
    /// # fn main() {
    /// THRESHER.set_enforcement(Enforcement::MarkedThreads);
    ///
    /// tokio::runtime::Builder::new_multi_thread()
    ///     .on_thread_start(|| THRESHER.mark_current_thread(true))
    ///     .build()
    ///     .unwrap();
    /// # }
    /// ```
    pub fn set_enforcement(&self, enforcement: Enforcement) {
        self.marked_threads_only.store(
            matches!(enforcement, Enforcement::MarkedThreads),
            Ordering::Relaxed,
        );
    }

    /// Returns the current enforcement scope.
    pub fn get_enforcement(&self) -> Enforcement {
        if self.marked_threads_only.load(Ordering::Relaxed) {
            Enforcement::MarkedThreads
        } else {
            Enforcement::AllThreads
        }
    }

    /// Opt the calling thread in to (or out of) the hard limit.
    ///
    /// Only has any effect under [`Enforcement::MarkedThreads`]. Call it from
    /// the thread itself, typically in whatever start hook your runtime offers.
    pub fn mark_current_thread(&self, marked: bool) {
        STATE.with(|state| state.marked.set(marked));
    }

    /// Take the reason the calling thread's last allocation failed, if it was
    /// this limit that failed it.
    ///
    /// Returns the running total at the point of refusal, in bytes, and clears
    /// the record. Meant for an alloc error hook, which runs on the same thread
    /// as the failed allocation and otherwise has no way to tell a limit from a
    /// real out of memory:
    ///
    /// ```rust,ignore
    /// std::alloc::set_alloc_error_hook(|layout| match THRESHER.take_refusal() {
    ///     Some(allocated) => panic!(
    ///         "thresher refused {} bytes at {allocated} allocated",
    ///         layout.size()
    ///     ),
    ///     // Not us: the machine really is out of memory.
    ///     None => eprintln!("allocation of {} bytes failed", layout.size()),
    /// });
    /// ```
    ///
    /// Taking the refusal does not re-arm the limit on this thread — the panic
    /// this hook is about to raise has to be able to allocate. See
    /// [`rearm_current_thread`](Thresher::rearm_current_thread).
    pub fn take_refusal(&self) -> Option<usize> {
        STATE.with(|state| state.refusal.take())
    }

    /// Re-arm the hard limit on the calling thread after a refusal.
    ///
    /// A refused thread is let through until the process recovers, so that it
    /// can unwind and report rather than aborting halfway. Enforcement comes
    /// back on its own once the running total drops below the limit, which is
    /// the usual outcome — the work being unwound releases what it was holding.
    ///
    /// Call this at the point you catch the unwind, if you would rather not wait
    /// for that. On a busy server the total may stay above the limit because of
    /// *other* threads, and until it comes down the refused thread is running
    /// unenforced.
    ///
    /// ```rust,no_run
    /// # use std::alloc;
    /// # use std::panic::{self, AssertUnwindSafe};
    /// # use thresher::Thresher;
    /// # #[global_allocator]
    /// # static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);
    /// # fn run_query() {}
    /// let result = panic::catch_unwind(AssertUnwindSafe(run_query));
    /// THRESHER.rearm_current_thread();
    /// ```
    pub fn rearm_current_thread(&self) {
        STATE.with(|state| {
            state.grace.set(false);
            state.refusal.set(None);
        });
    }

    /// Reconcile the calling thread's outstanding balance into the shared total.
    ///
    /// Only affects the thread it is called from. Can trip the threshold, and so
    /// run the callback, if this thread was holding an unreconciled balance that
    /// takes the total over.
    pub fn flush(&self) {
        STATE.with(|state| {
            let balance = state.balance.replace(0);

            if balance > 0 {
                self.reconcile_alloc(state, balance as usize);
            } else if balance < 0 {
                self.reconcile_dealloc(state, balance.unsigned_abs());
            }
        });
    }

    /// Overwrite the running total with a known-good figure.
    ///
    /// What passes through [`GlobalAlloc`] and what the process is actually
    /// holding are not the same number: allocators round up, keep freed pages,
    /// and map arenas the wrapper never sees. If you have a better source — the
    /// resident set size, or your allocator's own statistics — you can poll it on
    /// a timer and correct the total here.
    ///
    /// Crossing the threshold this way runs the callback, same as an allocation
    /// would, so a poller is enough on its own to catch growth that never shows
    /// up as an allocation request.
    ///
    /// ```rust
    /// # use std::alloc;
    /// # use thresher::Thresher;
    /// # #[global_allocator]
    /// # static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);
    /// # fn resident_bytes() -> usize { 0 }
    /// std::thread::spawn(|| {
    ///     loop {
    ///         std::thread::sleep(std::time::Duration::from_millis(50));
    ///         THRESHER.set_allocated(resident_bytes());
    ///     }
    /// });
    /// ```
    pub fn set_allocated(&self, allocated: usize) {
        let threshold = self.threshold.load(Ordering::Relaxed);
        let previous = self.allocated.swap(allocated, Ordering::Relaxed);

        STATE.with(|state| {
            self.update_limit_state(state, allocated);

            if allocated >= threshold && previous < threshold {
                self.run_callback(state, allocated);
            }
        });
    }

    /// Whether an allocation of `size` must be refused.
    ///
    /// The overwhelmingly common case is no hard limit configured at all, so the
    /// inline part is one thread local read and a comparison against a constant,
    /// both of which predict perfectly. Everything else is out of line.
    ///
    /// The size check is not redundant with `over_limit`: that flag only says
    /// where the total stood at this thread's last reconcile, so on its own a
    /// single enormous allocation would be served first and noticed afterwards,
    /// which is too late to be a limit.
    #[inline]
    fn should_refuse(&self, state: &ThreadState, size: usize) -> bool {
        if !state.over_limit.get() && size < RECONCILE_BATCH {
            return false;
        }

        self.refusal_check(state, size)
    }

    #[cold]
    #[inline(never)]
    fn refusal_check(&self, state: &ThreadState, size: usize) -> bool {
        let limit = self.limit.load(Ordering::Relaxed);

        if limit == usize::MAX {
            return false;
        }

        // The threshold callback exists to run when memory is tight; refusing
        // its allocations would defeat the point of leaving headroom for it.
        if state.in_callback.get() {
            return false;
        }

        // One refusal per thread. The panic that follows a null allocates — the
        // payload, the message, the backtrace — and refusing those too would
        // turn an unwind we can catch into an abort we cannot.
        if state.grace.get() {
            return false;
        }

        if self.marked_threads_only.load(Ordering::Relaxed) && !state.marked.get() {
            return false;
        }

        // Count this thread's unreconciled balance, which the shared total has
        // not seen yet, along with the allocation being asked for.
        let projected = self
            .allocated
            .load(Ordering::Relaxed)
            .saturating_add(state.balance.get().max(0) as usize)
            .saturating_add(size);

        if projected < limit {
            state.over_limit.set(false);
            return false;
        }

        state.over_limit.set(true);
        state.refusal.set(Some(projected));
        state.grace.set(true);

        true
    }

    /// Note where the shared total stands relative to the hard limit, so the
    /// next allocation on this thread can decide without reading shared state.
    #[inline]
    fn update_limit_state(&self, state: &ThreadState, allocated: usize) {
        let over = allocated >= self.limit.load(Ordering::Relaxed);

        state.over_limit.set(over);

        // Recovered: re-arm, so a thread that was refused once can be refused
        // again if the process climbs back up.
        if !over {
            state.grace.set(false);
            state.refusal.set(None);
        }
    }

    /// Debit this thread's balance, reconciling if it has drifted far enough.
    #[inline]
    fn record_alloc(&self, state: &ThreadState, size: usize) {
        // `Layout` guarantees a size that fits in an `isize`.
        let balance = state.balance.get().saturating_add(size as isize);

        if balance < RECONCILE_BATCH as isize {
            state.balance.set(balance);
            return;
        }

        state.balance.set(0);
        self.reconcile_alloc(state, balance as usize);
    }

    /// Credit this thread's balance, reconciling if it has drifted far enough.
    #[inline]
    fn record_dealloc(&self, state: &ThreadState, size: usize) {
        let balance = state.balance.get().saturating_sub(size as isize);

        if balance > -(RECONCILE_BATCH as isize) {
            state.balance.set(balance);
            return;
        }

        state.balance.set(0);
        self.reconcile_dealloc(state, balance.unsigned_abs());
    }

    #[cold]
    #[inline(never)]
    fn reconcile_alloc(&self, state: &ThreadState, size: usize) {
        let threshold = self.threshold.load(Ordering::Relaxed);
        let old_allocated = self.allocated.fetch_add(size, Ordering::Relaxed);
        let new_allocated = old_allocated.saturating_add(size);

        self.update_limit_state(state, new_allocated);

        // only execute call back when we've passed the threshold
        if new_allocated >= threshold && old_allocated < threshold {
            self.run_callback(state, new_allocated);
        }
    }

    #[cold]
    #[inline(never)]
    fn reconcile_dealloc(&self, state: &ThreadState, size: usize) {
        // Saturating rather than a plain `fetch_sub`: a `set_allocated` resync,
        // or threads that exited holding a positive balance, can leave the total
        // lower than what is being credited back, and wrapping past zero would
        // leave the callback permanently tripped.
        let allocated = self
            .allocated
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |allocated| {
                Some(allocated.saturating_sub(size))
            })
            .unwrap_or_else(|allocated| allocated)
            .saturating_sub(size);

        self.update_limit_state(state, allocated);
    }

    #[cold]
    #[inline(never)]
    fn run_callback(&self, state: &ThreadState, allocated: usize) {
        let Some(callback) = self.callback.get() else {
            return;
        };

        // A callback worth writing allocates — dumping a heap profile, waking a
        // task — and those allocations come straight back through here.
        if state.in_callback.replace(true) {
            return;
        }

        let _reset = CallbackGuard;
        callback(allocated);
    }
}

/// Clears the re-entrancy flag, including if the callback unwinds.
struct CallbackGuard;

impl Drop for CallbackGuard {
    fn drop(&mut self) {
        // Cannot borrow the caller's `&ThreadState` here, but this only runs
        // once per callback, not per allocation.
        STATE.with(|state| state.in_callback.set(false));
    }
}

// Each method resolves the thread local exactly once and passes the reference
// down. Reaching for it separately in the refusal check and again in the
// accounting measurably costs more than the work either of them does.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Thresher<A> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        STATE.with(|state| {
            if self.should_refuse(state, layout.size()) {
                return ptr::null_mut();
            }

            let ptr = unsafe { self.allocator.alloc(layout) };

            if !ptr.is_null() {
                self.record_alloc(state, layout.size());
            }

            ptr
        })
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.allocator.dealloc(ptr, layout) };
        STATE.with(|state| self.record_dealloc(state, layout.size()));
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        STATE.with(|state| {
            if self.should_refuse(state, layout.size()) {
                return ptr::null_mut();
            }

            let ptr = unsafe { self.allocator.alloc_zeroed(layout) };

            if !ptr.is_null() {
                self.record_alloc(state, layout.size());
            }

            ptr
        })
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, old_layout: Layout, size: usize) -> *mut u8 {
        let old_size = old_layout.size();

        STATE.with(|state| {
            // Only growth can be refused, and returning null here leaves the
            // block at `ptr` untouched and still valid, as `realloc` requires.
            if size > old_size && self.should_refuse(state, size - old_size) {
                return ptr::null_mut();
            }

            let new_ptr = unsafe { self.allocator.realloc(ptr, old_layout, size) };

            if !new_ptr.is_null() {
                if size > old_size {
                    self.record_alloc(state, size - old_size);
                } else {
                    self.record_dealloc(state, old_size - size);
                }
            }

            new_ptr
        })
    }
}

#[cfg(test)]
mod tests {

    use std::{
        alloc,
        hint::black_box,
        sync::{
            Mutex, MutexGuard, Once,
            atomic::{AtomicBool, AtomicUsize},
        },
        thread,
    };

    use super::*;

    #[global_allocator]
    static ALLOCATOR: Thresher<alloc::System> = Thresher::new(alloc::System);

    /// Number of times the callback has run since the last [`setup`].
    static FIRES: AtomicUsize = AtomicUsize::new(0);

    /// Whether the callback's own allocation was refused by the hard limit.
    static CALLBACK_REFUSED: AtomicBool = AtomicBool::new(false);

    /// The callback can only be registered once for the whole test binary, and
    /// the threshold is process wide, so the tests take turns.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Ask the allocator for `size` bytes and give them straight back, reporting
    /// whether the request was refused.
    ///
    /// Going through the `GlobalAlloc` methods by hand rather than allocating
    /// normally is deliberate: a refused allocation in real code reaches
    /// `handle_alloc_error`, which would abort the whole test binary.
    fn probe(size: usize) -> bool {
        let layout = Layout::from_size_align(size, 16).expect("valid layout");

        // SAFETY: `layout` has a non-zero size, and anything handed back is
        // returned to the same allocator with the same layout immediately.
        unsafe {
            let ptr = ALLOCATOR.alloc(layout);

            if ptr.is_null() {
                return true;
            }

            ALLOCATOR.dealloc(ptr, layout);
            false
        }
    }

    /// Serialise against the other tests, register the shared callback, and
    /// start from a disarmed threshold and limit with no recorded fires.
    fn setup() -> MutexGuard<'static, ()> {
        static REGISTER: Once = Once::new();

        let guard = TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());

        ALLOCATOR.set_threshold(usize::MAX);
        ALLOCATOR.set_limit(usize::MAX);
        ALLOCATOR.set_enforcement(Enforcement::AllThreads);
        ALLOCATOR.mark_current_thread(false);
        ALLOCATOR.take_refusal();

        REGISTER.call_once(|| {
            ALLOCATOR.set_callback(|_| {
                FIRES.fetch_add(1, Ordering::Relaxed);

                // Allocating from the callback is the whole point of leaving
                // headroom. It must not come back around into the callback, and
                // the hard limit must not refuse it.
                CALLBACK_REFUSED.store(probe(4 * RECONCILE_BATCH), Ordering::Relaxed);

                let held: Vec<u8> = Vec::with_capacity(4 * RECONCILE_BATCH);
                black_box(held.as_ptr());
            });
        });

        FIRES.store(0, Ordering::Relaxed);
        CALLBACK_REFUSED.store(false, Ordering::Relaxed);

        guard
    }

    fn fires() -> usize {
        FIRES.load(Ordering::Relaxed)
    }

    /// A limit the process is already past, so the next request has to be
    /// refused. Callers must disarm it before allocating normally again.
    fn arm_limit_behind_us() {
        ALLOCATOR.flush();
        ALLOCATOR.set_limit(ALLOCATOR.get_allocated().saturating_sub(1));
    }

    #[test]
    fn fires_once_when_threshold_crossed() {
        let _guard = setup();

        ALLOCATOR.flush();
        ALLOCATOR.set_threshold(ALLOCATOR.get_allocated() + 8 * 1024 * 1024);
        assert_eq!(fires(), 0);

        let held = vec![0u8; 16 * 1024 * 1024];

        // Once, not once per allocation: the callback is edge triggered, and its
        // own allocations must not re-enter it.
        assert_eq!(fires(), 1);

        drop(black_box(held));
    }

    #[test]
    fn does_not_fire_below_threshold() {
        let _guard = setup();

        ALLOCATOR.flush();
        ALLOCATOR.set_threshold(ALLOCATOR.get_allocated() + 64 * 1024 * 1024);

        let held = vec![0u8; 8 * 1024 * 1024];
        assert_eq!(fires(), 0);

        drop(black_box(held));
    }

    #[test]
    fn accounting_returns_to_baseline() {
        let _guard = setup();

        ALLOCATOR.flush();
        let baseline = ALLOCATOR.get_allocated();

        let held = vec![0u8; 32 * 1024 * 1024];
        ALLOCATOR.flush();
        assert!(
            ALLOCATOR.get_allocated() >= baseline + 32 * 1024 * 1024,
            "allocation was not accounted for"
        );

        drop(black_box(held));
        ALLOCATOR.flush();

        // Other threads in the test binary are allocating too, so this is a
        // "came back down" check rather than an exact one.
        let settled = ALLOCATOR.get_allocated();
        assert!(
            settled < baseline + 8 * 1024 * 1024,
            "deallocation was not accounted for: {settled} vs baseline {baseline}"
        );
    }

    /// Allocations smaller than the batch size stay on the thread until they add
    /// up, but they must not be lost.
    #[test]
    fn batched_allocations_reconcile() {
        let _guard = setup();

        ALLOCATOR.flush();
        let baseline = ALLOCATOR.get_allocated();

        // Individually well under RECONCILE_BATCH, four megabytes in total.
        let held: Vec<Vec<u8>> = (0..4096).map(|_| vec![0u8; 1024]).collect();

        ALLOCATOR.flush();
        assert!(
            ALLOCATOR.get_allocated() >= baseline + 4 * 1024 * 1024,
            "batched allocations went missing"
        );

        drop(black_box(held));
    }

    #[test]
    fn concurrent_allocations_do_not_wrap() {
        let _guard = setup();

        ALLOCATOR.flush();
        let baseline = ALLOCATOR.get_allocated();

        thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..10_000 {
                        let block: Vec<u8> = Vec::with_capacity(1024);
                        black_box(block.as_ptr());
                    }
                    ALLOCATOR.flush();
                });
            }
        });

        ALLOCATOR.flush();

        // Symmetric alloc/free across threads: the total must come back near
        // where it started rather than drifting or wrapping past zero.
        let settled = ALLOCATOR.get_allocated();
        assert!(
            settled < baseline + 8 * 1024 * 1024,
            "concurrent accounting drifted: {settled} vs baseline {baseline}"
        );
    }

    #[test]
    fn hard_limit_refuses_allocations_past_it() {
        let _guard = setup();

        arm_limit_behind_us();
        let refused = probe(4 * RECONCILE_BATCH);

        // Disarm before anything else on this thread allocates for real.
        ALLOCATOR.set_limit(usize::MAX);

        assert!(refused, "an allocation past the hard limit was served");
        assert!(
            ALLOCATOR.take_refusal().is_some(),
            "the refusal was not recorded for the alloc error hook"
        );
    }

    #[test]
    fn hard_limit_allows_allocations_under_it() {
        let _guard = setup();

        ALLOCATOR.flush();
        ALLOCATOR.set_limit(ALLOCATOR.get_allocated() + 64 * 1024 * 1024);

        let refused = probe(4 * RECONCILE_BATCH);

        ALLOCATOR.set_limit(usize::MAX);

        assert!(!refused, "an allocation under the hard limit was refused");
        assert!(ALLOCATOR.take_refusal().is_none());
    }

    /// The panic that follows a null pointer allocates. If the thread stayed
    /// refused, that allocation would fail too and abort instead of unwinding.
    #[test]
    fn refusal_is_one_shot_so_the_thread_can_unwind() {
        let _guard = setup();

        arm_limit_behind_us();
        let first = probe(4 * RECONCILE_BATCH);
        let second = probe(4 * RECONCILE_BATCH);

        ALLOCATOR.set_limit(usize::MAX);

        assert!(first, "the first allocation past the limit was served");
        assert!(!second, "a refused thread was left with no room to unwind");
        assert!(ALLOCATOR.take_refusal().is_some());
    }

    /// The exact sequence an alloc error hook performs: take the refusal, then
    /// allocate to build and raise the panic. If taking the refusal re-armed the
    /// limit, that allocation would fail and abort mid-unwind.
    #[test]
    fn taking_the_refusal_leaves_room_to_panic() {
        let _guard = setup();

        arm_limit_behind_us();
        let refused = probe(4 * RECONCILE_BATCH);
        let reported = ALLOCATOR.take_refusal();
        let panic_allocation = probe(4 * RECONCILE_BATCH);

        ALLOCATOR.set_limit(usize::MAX);

        assert!(refused);
        assert!(reported.is_some());
        assert!(
            !panic_allocation,
            "the panic that follows a refusal could not allocate"
        );
    }

    #[test]
    fn rearming_restores_enforcement() {
        let _guard = setup();

        arm_limit_behind_us();
        let refused = probe(4 * RECONCILE_BATCH);
        ALLOCATOR.take_refusal();
        ALLOCATOR.rearm_current_thread();
        let after_rearm = probe(4 * RECONCILE_BATCH);

        ALLOCATOR.set_limit(usize::MAX);
        ALLOCATOR.take_refusal();

        assert!(refused);
        assert!(after_rearm, "the limit was not re-armed");
    }

    /// Recovering below the limit re-arms without the application asking.
    #[test]
    fn recovery_restores_enforcement() {
        let _guard = setup();

        arm_limit_behind_us();
        let refused = probe(4 * RECONCILE_BATCH);

        // What unwinding does: release what the failed work was holding.
        ALLOCATOR.set_limit(ALLOCATOR.get_allocated() + 64 * 1024 * 1024);
        ALLOCATOR.flush();
        ALLOCATOR.set_allocated(ALLOCATOR.get_allocated());

        let recovered = ALLOCATOR.take_refusal();

        ALLOCATOR.set_limit(usize::MAX);

        assert!(refused);
        assert!(
            recovered.is_none(),
            "dropping back under the limit did not clear the refusal"
        );
    }

    #[test]
    fn only_marked_threads_are_refused_when_scoped() {
        let _guard = setup();

        ALLOCATOR.set_enforcement(Enforcement::MarkedThreads);
        arm_limit_behind_us();

        let unmarked = probe(4 * RECONCILE_BATCH);
        ALLOCATOR.mark_current_thread(true);
        let marked = probe(4 * RECONCILE_BATCH);

        ALLOCATOR.set_limit(usize::MAX);
        ALLOCATOR.set_enforcement(Enforcement::AllThreads);
        ALLOCATOR.mark_current_thread(false);

        assert!(!unmarked, "an unmarked thread was refused");
        assert!(marked, "a marked thread was not refused");
        assert!(ALLOCATOR.take_refusal().is_some());
    }

    /// The callback runs *because* memory is tight, so the limit that made it
    /// run must not stop it doing its job.
    #[test]
    fn the_callback_is_exempt_from_the_hard_limit() {
        let _guard = setup();

        ALLOCATOR.flush();
        let baseline = ALLOCATOR.get_allocated();
        ALLOCATOR.set_threshold(baseline + 1024 * 1024);
        ALLOCATOR.set_limit(baseline + 1024 * 1024);

        // Trips both at once, so the callback runs while over the limit.
        ALLOCATOR.set_allocated(baseline + 2 * 1024 * 1024);

        ALLOCATOR.set_limit(usize::MAX);
        ALLOCATOR.set_threshold(usize::MAX);
        ALLOCATOR.set_allocated(baseline);

        assert_eq!(fires(), 1);
        assert!(
            !CALLBACK_REFUSED.load(Ordering::Relaxed),
            "the threshold callback was refused by the hard limit"
        );
    }

    #[test]
    fn set_allocated_can_trip_the_threshold() {
        let _guard = setup();

        ALLOCATOR.flush();
        let baseline = ALLOCATOR.get_allocated();
        ALLOCATOR.set_threshold(baseline + 1024 * 1024);
        assert_eq!(fires(), 0);

        ALLOCATOR.set_allocated(baseline + 2 * 1024 * 1024);
        assert_eq!(fires(), 1);

        // Put the shared total back so the next test starts from something real.
        ALLOCATOR.set_threshold(usize::MAX);
        ALLOCATOR.set_allocated(baseline);
    }
}
