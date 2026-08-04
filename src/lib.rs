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
//!     THRESHER.set_callback_threshold(100 * 1024 * 1024);
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
//! all of them on one cache line. A thread reconciles what it is still holding
//! when it exits, so threads coming and going do not make the total drift.
//!
//! The consequence is that [`Thresher::get_allocated`] is *approximate*: it can
//! lag by up to [`RECONCILE_BATCH`] bytes per running thread. For picking a
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
//! # Two levels
//!
//! There are two independent levels, and they are meant to be used together:
//!
//! * The **callback threshold** ([`Thresher::set_callback_threshold`]) is
//!   advisory. Crossing it runs your callback and nothing else — dump a heap
//!   profile, shed load, drop caches. Allocation carries on.
//!
//! * The **hard limit** ([`Thresher::set_hard_limit`]) refuses. Allocations that
//!   would take the process past it fail: [`GlobalAlloc::alloc`] returns null,
//!   and Rust turns that into `handle_alloc_error`.
//!
//! Set the threshold below the limit and you get a warning shot before anything
//! starts failing:
//!
//! ```rust
//! # use std::alloc;
//! # use thresher::Thresher;
//! # #[global_allocator]
//! # static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);
//! THRESHER.set_callback_threshold(3 * 1024 * 1024 * 1024); // profile at 3 GiB
//! THRESHER.set_hard_limit(3 * 1024 * 1024 * 1024 + 512 * 1024 * 1024); // refuse at 3.5 GiB
//! ```
//!
//! # Scope
//!
//! The running total is process wide, not per instance: it lives in a `static`,
//! because a thread that exits has to fold its outstanding balance back in from
//! a thread local destructor, which has no way back to the allocator it was
//! accounting for. Only one allocator can be the `#[global_allocator]`, so there
//! is only one total worth keeping.
//!
//! The practical consequence is that a `Thresher` which is *not* the global
//! allocator — one wrapping a sub-allocator to measure a single subsystem, say —
//! still reports and limits the whole process. [`Thresher::mark_current_thread`] is
//! likewise a property of the thread rather than of the instance.

use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    panic::{self, AssertUnwindSafe},
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

/// The running total, in bytes.
///
/// Process wide rather than a field on `Thresher`, because a thread that exits
/// has to fold its outstanding balance back in from its thread local's `Drop`,
/// which has no way back to the allocator it was accounting for. Only one
/// allocator can be the `#[global_allocator]`, so there is only one total worth
/// keeping.
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

/// What the allocator tracks per thread.
///
/// `Cell` of `Copy` types with a `const` initialiser, so access needs no lazy
/// initialisation — which would not be safe to do from inside an allocator, and
/// would recurse.
///
/// The `Drop` impl does mean a thread local destructor gets registered, and
/// registering one can allocate (`_tlv_atexit` on macOS calls `malloc`), which
/// would re-enter the allocator on a thread's first allocation. Measured on
/// glibc, it does not: the first allocation on a thread does not nest.
struct ThreadState {
    /// Allocated bytes the shared total has not seen yet.
    balance: Cell<isize>,

    /// Whether this thread has opted in under [`Enforcement::MarkedThreads`].
    marked: Cell<bool>,
}

impl ThreadState {
    const fn new() -> Self {
        Self {
            balance: Cell::new(0),
            marked: Cell::new(false),
        }
    }
}

impl Drop for ThreadState {
    fn drop(&mut self) {
        // Whatever this thread had not reconciled is still owned by somebody —
        // a buffer handed to another thread, a leak — so it belongs in the total
        // rather than disappearing along with the thread.
        //
        // No threshold check here: this runs from a thread local destructor,
        // which is no place to call back into user code, and the balance is
        // smaller than RECONCILE_BATCH by construction.
        match self.balance.replace(0) {
            balance if balance > 0 => {
                add_allocated(balance as usize);
            }
            balance if balance < 0 => {
                sub_allocated(balance.unsigned_abs());
            }
            _ => {}
        }
    }
}

thread_local! {
    static STATE: ThreadState = const { ThreadState::new() };

    /// Set while this thread is inside the threshold callback.
    ///
    /// Kept out of [`ThreadState`] so that the allocation path never has to look
    /// at it: it is read on the cold paths only — running the callback, and
    /// deciding whether the hard limit applies to an allocation the callback
    /// itself made.
    static IN_CALLBACK: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` against this thread's state, which is `None` once the thread local
/// has been destroyed.
///
/// Not the plain `with`, which panics on a destroyed thread local — and a panic
/// unwinding out of [`GlobalAlloc`] is undefined behaviour.
///
/// The `None` arm is insurance rather than a path anything is known to take:
/// `STATE` registers its destructor on a thread's first allocation, so where
/// destructors run in reverse registration order it is the last thing dropped
/// and nothing that allocates runs after it. That is the platform's decision to
/// change, not ours, so the accounting handles the case rather than assuming it
/// away.
#[inline]
fn with_state<R>(f: impl Fn(Option<&ThreadState>) -> R) -> R {
    let result = STATE.try_with(|state| f(Some(state)));

    match result {
        Ok(result) => result,
        Err(_) => f(None),
    }
}

/// Add to the shared total, returning what it was before.
#[inline]
fn add_allocated(size: usize) -> usize {
    // Relaxed throughout: the total is a number, not a handle on data published
    // to another thread, so there is nothing for an acquire to acquire. Every
    // allocation in the process passes through here.
    ALLOCATED.fetch_add(size, Ordering::Relaxed)
}

/// Subtract from the shared total.
#[inline]
fn sub_allocated(size: usize) {
    // Saturating rather than a plain `fetch_sub`: a `set_allocated` resync can
    // leave the total lower than what is being credited back, and wrapping past
    // zero would leave the callback permanently tripped.
    //
    // The closure always returns `Some`, so this never fails.
    let _ = ALLOCATED.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |allocated| {
        Some(allocated.saturating_sub(size))
    });
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

/// The main allocation wrapper. [`Thresher::new()`] to wrap an existing allocator
pub struct Thresher<A> {
    allocator: A,
    callback_threshold: AtomicUsize,
    hard_limit: AtomicUsize,
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
            callback_threshold: AtomicUsize::new(usize::MAX),
            hard_limit: AtomicUsize::new(usize::MAX),
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
    ///     THRESHER.set_callback_threshold(100 * 1024 * 1024);
    /// }
    /// ```
    ///
    pub fn set_callback_threshold(&self, threshold: usize) {
        self.callback_threshold.store(threshold, Ordering::Release);
    }

    /// Set the callback to execute when the threshold is reached.
    /// This callback may be called multiple times if the allocation threshold is reached and then reduced.
    ///
    /// As this callback happens when allocating, you need to ensure that it happens rather quickly, as to not block running code.
    ///
    /// The callback runs inside the allocator, on whichever thread happened to
    /// cross the threshold, so there are two things it must not do:
    ///
    /// * **Panic.** Unwinding out of an allocation is undefined behaviour, so
    ///   the panic is caught here and swallowed: the callback stops where it
    ///   panicked, and nothing is reported. Keep the callback infallible;
    ///   anything that might fail belongs on the other side of a channel, as in
    ///   the `jemalloc` example.
    /// * **Block on anything that allocates.** Taking a lock that another thread
    ///   holds while it is itself allocating will deadlock.
    ///
    /// The callback is free to allocate — that is what the headroom below the
    /// threshold is for — and allocations it makes will not re-enter it, nor are
    /// they refused by [`set_hard_limit`](Thresher::set_hard_limit): the callback
    /// runs precisely because memory is tight, so refusing it would defeat the
    /// point. That exemption is unbounded, so keep what the callback allocates
    /// bounded.
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
        ALLOCATED.load(Ordering::Acquire)
    }

    /// Returns the current threshold.
    pub fn get_callback_threshold(&self) -> usize {
        self.callback_threshold.load(Ordering::Acquire)
    }

    /// Set the hard limit in bytes, above which allocations start failing.
    ///
    /// Where [`set_callback_threshold`](Thresher::set_callback_threshold) asks
    /// nicely, this one says no: an allocation that would take the process past
    /// the limit gets a null pointer back. Set it to `usize::MAX` (the default) to
    /// disable.
    ///
    /// A null from the allocator means `handle_alloc_error`, which aborts. That
    /// is a better death than being OOM killed — it happens at a figure you
    /// chose, while the rest of the machine is still healthy — but it is still
    /// the end of the process, and the abort message is the standard
    /// `memory allocation of N bytes failed` rather than anything of ours. Leave
    /// room between the threshold and the limit so the callback has somewhere to
    /// do its work, and consider
    /// [`set_enforcement`](Thresher::set_enforcement) so the limit fails the work
    /// you can afford to lose rather than your logging.
    ///
    /// To handle a refusal rather than die of it, allocate fallibly:
    /// [`Vec::try_reserve`] and friends turn the null into an `Err` instead of
    /// reaching `handle_alloc_error` at all, which is what makes
    /// [`Enforcement::MarkedThreads`] worth pairing this with.
    ///
    /// # The limit is not exact
    ///
    /// The comparison is against the shared total, which lags by up to
    /// [`RECONCILE_BATCH`] bytes per *other* running thread (this thread's own
    /// outstanding balance is counted). Nothing locks between the check and the
    /// allocation either, so concurrent threads can each pass a check that only
    /// one of them would have passed in sequence. Both errors are in the same
    /// direction — the process can sit somewhat above the limit — so set it far
    /// enough below the ceiling you actually cannot cross that a few hundred
    /// kilobytes of slack does not matter.
    ///
    /// ```rust
    /// # use std::alloc;
    /// # use thresher::Thresher;
    /// # #[global_allocator]
    /// # static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);
    /// THRESHER.set_hard_limit(4 * 1024 * 1024 * 1024);
    /// ```
    pub fn set_hard_limit(&self, hard_limit: usize) {
        self.hard_limit.store(hard_limit, Ordering::Release);
    }

    /// Returns the current hard limit.
    pub fn get_hard_limit(&self) -> usize {
        self.hard_limit.load(Ordering::Acquire)
    }

    /// Choose which threads the hard limit may fail allocations on.
    ///
    /// Defaults to [`Enforcement::AllThreads`], which is the honest reading of
    /// "hard limit" but will happily refuse your logging, your metrics and your
    /// health check along with the work that caused the problem.
    ///
    /// A server that wants a say in what dies should usually run in
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
            Ordering::Release,
        );
    }

    /// Returns the current enforcement scope.
    pub fn get_enforcement(&self) -> Enforcement {
        if self.marked_threads_only.load(Ordering::Acquire) {
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
        with_state(|state| {
            if let Some(state) = state {
                state.marked.set(marked);
            }
        });
    }

    /// Reconcile the calling thread's outstanding balance into the shared total.
    ///
    /// Only affects the thread it is called from. Can trip the threshold, and so
    /// run the callback, if this thread was holding an unreconciled balance that
    /// takes the total over.
    pub fn flush(&self) {
        with_state(|state| {
            let Some(state) = state else {
                return;
            };

            match state.balance.replace(0) {
                balance if balance > 0 => self.reconcile_alloc(balance as usize),
                balance if balance < 0 => self.reconcile_dealloc(balance.unsigned_abs()),
                _ => {}
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
        let threshold = self.callback_threshold.load(Ordering::Relaxed);
        let previous = ALLOCATED.swap(allocated, Ordering::Relaxed);

        if allocated >= threshold && previous < threshold {
            self.run_callback(allocated);
        }
    }

    /// Whether an allocation of `size` must be refused.
    ///
    /// The overwhelmingly common case is no hard limit configured at all, so the
    /// inline part is a load of a word nothing ever writes to — every core holds
    /// a clean copy of that line — and a comparison against a constant.
    #[inline]
    fn should_refuse(&self, state: Option<&ThreadState>, size: usize) -> bool {
        let hard_limit = self.hard_limit.load(Ordering::Relaxed);

        if hard_limit == usize::MAX {
            return false;
        }

        self.refusal_check(state, size, hard_limit)
    }

    #[inline(never)]
    fn refusal_check(&self, state: Option<&ThreadState>, size: usize, hard_limit: usize) -> bool {
        // The threshold callback exists to run when memory is tight; refusing
        // its allocations would defeat the point of leaving headroom for it.
        if IN_CALLBACK.with(|in_callback| in_callback.get()) {
            return false;
        }

        if self.marked_threads_only.load(Ordering::Relaxed)
            && !state.is_some_and(|state| state.marked.get())
        {
            return false;
        }

        // Count this thread's unreconciled balance, which the shared total has
        // not seen yet, along with the allocation being asked for.
        let projected = ALLOCATED
            .load(Ordering::Relaxed)
            .saturating_add(state.map_or(0, |state| state.balance.get().max(0) as usize))
            .saturating_add(size);

        projected >= hard_limit
    }

    /// Debit this thread's balance, reconciling if it has drifted far enough.
    #[inline]
    fn record_alloc(&self, state: Option<&ThreadState>, size: usize) {
        let Some(state) = state else {
            return self.reconcile_alloc(size);
        };

        // `Layout` guarantees a size that fits in an `isize`.
        let balance = state.balance.get().saturating_add(size as isize);

        if balance < RECONCILE_BATCH as isize {
            state.balance.set(balance);
            return;
        }

        state.balance.set(0);
        self.reconcile_alloc(balance as usize);
    }

    /// Credit this thread's balance, reconciling if it has drifted far enough.
    #[inline]
    fn record_dealloc(&self, state: Option<&ThreadState>, size: usize) {
        let Some(state) = state else {
            return self.reconcile_dealloc(size);
        };

        let balance = state.balance.get().saturating_sub(size as isize);

        if balance > -(RECONCILE_BATCH as isize) {
            state.balance.set(balance);
            return;
        }

        state.balance.set(0);
        self.reconcile_dealloc(balance.unsigned_abs());
    }

    #[cold]
    #[inline(never)]
    fn reconcile_alloc(&self, size: usize) {
        // Relaxed: nothing is published through the threshold, and this is read
        // once per reconcile on every thread that allocates.
        let threshold = self.callback_threshold.load(Ordering::Relaxed);
        let old_allocated = add_allocated(size);
        let new_allocated = old_allocated.saturating_add(size);

        // only execute call back when we've passed the threshold
        if new_allocated >= threshold && old_allocated < threshold {
            self.run_callback(new_allocated);
        }
    }

    #[cold]
    #[inline(never)]
    fn reconcile_dealloc(&self, size: usize) {
        sub_allocated(size);
    }

    #[cold]
    #[inline(never)]
    fn run_callback(&self, allocated: usize) {
        let Some(callback) = self.callback.get() else {
            return;
        };

        // A callback worth writing allocates — dumping a heap profile, waking a
        // task — and those allocations come straight back through here.
        if IN_CALLBACK.with(|in_callback| in_callback.replace(true)) {
            return;
        }

        // Created before the catch, not inside it: a panic allocates its
        // payload, its message, and a backtrace if one is asked for, and those
        // allocations come back through here. With the flag still set across the
        // unwind they cannot re-enter the callback, and the hard limit does not
        // refuse them — which matters, because the callback only ever runs when
        // memory is already tight.
        let _reset = CallbackGuard;

        // `GlobalAlloc` implementations must not unwind, and the callback is
        // user code. Catching here is what makes that impossible rather than
        // merely documented against.
        //
        // `AssertUnwindSafe` because the only state a panicking callback can
        // leave torn is its own: nothing on this side of the boundary is handed
        // to it, and nothing but the callback itself sees that state again.
        let _ = panic::catch_unwind(AssertUnwindSafe(|| callback(allocated)));
    }
}

/// Clears the re-entrancy flag, including if the callback unwinds.
struct CallbackGuard;

impl Drop for CallbackGuard {
    fn drop(&mut self) {
        IN_CALLBACK.with(|in_callback| in_callback.set(false));
    }
}

// Each method resolves the thread local exactly once and passes the reference
// down. Reaching for it separately in the refusal check and again in the
// accounting measurably costs more than the work either of them does.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Thresher<A> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        with_state(|state| {
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
        with_state(|state| self.record_dealloc(state, layout.size()));
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        with_state(|state| {
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

        with_state(|state| {
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

    /// Makes the next callback run panic. Cleared as it is read, so one crossing
    /// panics and the next does not.
    static PANIC_NEXT: AtomicBool = AtomicBool::new(false);

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

    /// Resize a block from `from` to `to` through `realloc` with the limit armed
    /// behind us, reporting whether the resize was refused.
    ///
    /// The seed block is allocated *before* the limit goes up, so what is under
    /// test is the resize rather than the original allocation, and the block is
    /// freed after it comes down. Nothing between the two allocates: a panic in
    /// that window would allocate its payload, be refused, and abort the test
    /// binary rather than fail a test.
    fn probe_realloc(from: usize, to: usize) -> bool {
        let layout = Layout::from_size_align(from, 16).expect("valid layout");
        let resized_layout = Layout::from_size_align(to, 16).expect("valid layout");

        // SAFETY: allocated here, resized and freed with the layout it currently
        // has, and never read from.
        unsafe {
            let ptr = ALLOCATOR.alloc(layout);
            assert!(!ptr.is_null(), "the seed allocation was refused");

            arm_limit_behind_us();
            let resized = ALLOCATOR.realloc(ptr, layout, to);
            let refused = resized.is_null();
            disarm_limit();

            // `realloc` returning null has to leave the original block valid, so
            // on refusal it is still ours to free under its original layout.
            if refused {
                ALLOCATOR.dealloc(ptr, layout);
            } else {
                ALLOCATOR.dealloc(resized, resized_layout);
            }

            refused
        }
    }

    /// Serialise against the other tests, register the shared callback, and
    /// start from a disarmed threshold and limit with no recorded fires.
    fn setup() -> MutexGuard<'static, ()> {
        static REGISTER: Once = Once::new();

        let guard = TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());

        ALLOCATOR.set_callback_threshold(usize::MAX);
        ALLOCATOR.set_hard_limit(usize::MAX);
        ALLOCATOR.set_enforcement(Enforcement::AllThreads);
        ALLOCATOR.mark_current_thread(false);

        REGISTER.call_once(|| {
            ALLOCATOR.set_callback(|_| {
                FIRES.fetch_add(1, Ordering::Relaxed);

                // Allocating from the callback is the whole point of leaving
                // headroom. It must not come back around into the callback, and
                // the hard limit must not refuse it.
                CALLBACK_REFUSED.store(probe(4 * RECONCILE_BATCH), Ordering::Relaxed);

                let held: Vec<u8> = Vec::with_capacity(4 * RECONCILE_BATCH);
                black_box(held.as_ptr());

                // Last, so everything above still happened when it panics.
                if PANIC_NEXT.swap(false, Ordering::Relaxed) {
                    panic!("callback panicking on purpose");
                }
            });
        });

        FIRES.store(0, Ordering::Relaxed);
        CALLBACK_REFUSED.store(false, Ordering::Relaxed);
        PANIC_NEXT.store(false, Ordering::Relaxed);

        guard
    }

    fn fires() -> usize {
        FIRES.load(Ordering::Relaxed)
    }

    /// A limit the process is already past, so the next request has to be
    /// refused.
    ///
    /// Scoped to this thread: with the limit armed, *any* allocation that is not
    /// a [`probe`] reaches `handle_alloc_error` and takes the test binary with
    /// it, and the test harness has threads of its own. Callers must still
    /// disarm before allocating normally again.
    fn arm_limit_behind_us() {
        ALLOCATOR.set_enforcement(Enforcement::MarkedThreads);
        ALLOCATOR.mark_current_thread(true);
        ALLOCATOR.flush();
        ALLOCATOR.set_hard_limit(ALLOCATOR.get_allocated().saturating_sub(1));
    }

    /// Undo [`arm_limit_behind_us`]. Nothing on this thread may allocate between
    /// the two.
    fn disarm_limit() {
        ALLOCATOR.set_hard_limit(usize::MAX);
        ALLOCATOR.set_enforcement(Enforcement::AllThreads);
        ALLOCATOR.mark_current_thread(false);
    }

    #[test]
    fn fires_once_when_threshold_crossed() {
        let _guard = setup();

        ALLOCATOR.flush();
        ALLOCATOR.set_callback_threshold(ALLOCATOR.get_allocated() + 8 * 1024 * 1024);
        assert_eq!(fires(), 0);

        let held = vec![0u8; 16 * 1024 * 1024];

        // Once, not once per allocation: the callback is edge triggered, and its
        // own allocations must not re-enter it.
        assert_eq!(fires(), 1);

        drop(black_box(held));
    }

    /// Unwinding out of `GlobalAlloc` is undefined behaviour, so a panicking
    /// callback must not escape the allocator. It is caught and swallowed, the
    /// process carries on, and the next crossing still runs the callback — the
    /// re-entrancy flag has to come back down on the way out.
    #[test]
    fn a_panicking_callback_is_swallowed() {
        let _guard = setup();

        // The panic is expected, so keep its message out of the test output. It
        // still allocates its payload, which is the part under test.
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));

        PANIC_NEXT.store(true, Ordering::Relaxed);

        ALLOCATOR.flush();
        ALLOCATOR.set_callback_threshold(ALLOCATOR.get_allocated() + 8 * 1024 * 1024);
        let held = vec![0u8; 16 * 1024 * 1024];
        assert_eq!(fires(), 1, "the callback did not run");
        drop(black_box(held));

        // Arm it again from wherever the total has settled.
        ALLOCATOR.set_callback_threshold(usize::MAX);
        ALLOCATOR.flush();
        ALLOCATOR.set_callback_threshold(ALLOCATOR.get_allocated() + 4 * 1024 * 1024);
        let held = vec![0u8; 16 * 1024 * 1024];
        drop(black_box(held));

        panic::set_hook(previous_hook);

        assert_eq!(
            fires(),
            2,
            "the callback stopped running after it panicked once"
        );
    }

    #[test]
    fn does_not_fire_below_threshold() {
        let _guard = setup();

        ALLOCATOR.flush();
        ALLOCATOR.set_callback_threshold(ALLOCATOR.get_allocated() + 64 * 1024 * 1024);

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

        // Not `>= baseline + 32 MiB`: other threads in the test binary are
        // finishing while this runs, and a thread that exits reconciles its
        // balance from `ThreadState::drop`, which can be negative and pulls the
        // shared total down. The point is that 32 MiB was counted, so leave room
        // for that churn rather than assert to the byte.
        let allocated = ALLOCATOR.get_allocated();
        assert!(
            allocated >= baseline + 24 * 1024 * 1024,
            "allocation was not accounted for: {allocated} vs baseline {baseline}"
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

        // Tolerant for the same reason as `accounting_returns_to_baseline`:
        // threads finishing elsewhere reconcile balances into the total while
        // this test runs.
        let allocated = ALLOCATOR.get_allocated();
        assert!(
            allocated >= baseline + 3 * 1024 * 1024,
            "batched allocations went missing: {allocated} vs baseline {baseline}"
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

    /// A thread that exits still holding an unreconciled balance hands it back,
    /// rather than taking it out of the total along with itself.
    #[test]
    fn an_exiting_thread_reconciles_its_balance() {
        let _guard = setup();

        // Under RECONCILE_BATCH, so it stays on the thread's own balance and
        // only the drop can account for it.
        let size = RECONCILE_BATCH / 2;
        let layout = Layout::from_size_align(size, 16).expect("valid layout");

        ALLOCATOR.flush();
        let baseline = ALLOCATOR.get_allocated();

        // Allocated on a thread that then exits, and handed back as an address
        // so the block outlives it.
        let handle = thread::spawn(move || unsafe { ALLOCATOR.alloc(layout) } as usize);
        let ptr = handle.join().expect("thread allocates and exits") as *mut u8;

        assert!(!ptr.is_null());

        let after = ALLOCATOR.get_allocated();

        // SAFETY: allocated just above from this allocator, with this layout.
        unsafe { ALLOCATOR.dealloc(ptr, layout) };

        // Tearing a thread down frees more than it allocates, so the balance the
        // drop hands back is the leaked block minus that: check for most of it
        // rather than all of it.
        assert!(
            after >= baseline + size / 2,
            "a thread exited holding {size} bytes and they went missing: \
             {after} vs baseline {baseline}"
        );
    }

    /// What happens once this thread's state has been destroyed and something
    /// later in the teardown allocates anyway: no batching left to do, so it
    /// goes straight to the total rather than being lost.
    ///
    /// Reached through the accounting directly because the condition cannot be
    /// staged from a test: `ThreadState` registers its destructor on a thread's
    /// very first allocation, so it is the last thread local to be dropped.
    #[test]
    fn accounting_survives_the_thread_state_being_gone() {
        let _guard = setup();

        let size = 8 * RECONCILE_BATCH;

        ALLOCATOR.flush();
        let before = ALLOCATOR.get_allocated();
        ALLOCATOR.record_alloc(None, size);
        let allocated = ALLOCATOR.get_allocated();

        ALLOCATOR.record_dealloc(None, size);
        let settled = ALLOCATOR.get_allocated();

        assert!(
            allocated >= before + size,
            "an allocation after the thread state was gone went missing"
        );
        assert!(
            settled < allocated,
            "a deallocation after the thread state was gone went missing"
        );
    }

    #[test]
    fn hard_limit_refuses_allocations_past_it() {
        let _guard = setup();

        arm_limit_behind_us();
        let refused = probe(4 * RECONCILE_BATCH);
        disarm_limit();

        assert!(refused, "an allocation past the hard limit was served");
    }

    #[test]
    fn hard_limit_allows_allocations_under_it() {
        let _guard = setup();

        ALLOCATOR.flush();
        ALLOCATOR.set_hard_limit(ALLOCATOR.get_allocated() + 64 * 1024 * 1024);

        let refused = probe(4 * RECONCILE_BATCH);

        ALLOCATOR.set_hard_limit(usize::MAX);

        assert!(!refused, "an allocation under the hard limit was refused");
    }

    /// A single allocation big enough to blow the limit on its own is refused
    /// before it is served, not noticed afterwards.
    #[test]
    fn hard_limit_catches_a_single_oversized_allocation() {
        let _guard = setup();

        ALLOCATOR.set_enforcement(Enforcement::MarkedThreads);
        ALLOCATOR.mark_current_thread(true);
        ALLOCATOR.flush();
        ALLOCATOR.set_hard_limit(ALLOCATOR.get_allocated() + 1024 * 1024);

        let refused = probe(64 * 1024 * 1024);
        disarm_limit();

        assert!(refused, "an allocation straight past the limit was served");
    }

    /// Growing through `realloc` is refused the same as a fresh allocation, and
    /// the block being grown survives the refusal — `realloc` is required to
    /// leave it valid when it returns null.
    #[test]
    fn hard_limit_refuses_growth_through_realloc() {
        let _guard = setup();

        let refused = probe_realloc(16, 4 * RECONCILE_BATCH);

        assert!(refused, "a realloc past the hard limit was served");
    }

    /// Shrinking cannot take the process further over, so it is never refused —
    /// even from behind an armed limit.
    #[test]
    fn hard_limit_allows_shrinking_through_realloc() {
        let _guard = setup();

        let refused = probe_realloc(4 * RECONCILE_BATCH, 16);

        assert!(!refused, "shrinking was refused by the hard limit");
    }

    /// Coming back down under the limit lifts it again, with nothing to call.
    #[test]
    fn dropping_back_under_the_limit_restores_service() {
        let _guard = setup();

        arm_limit_behind_us();
        let refused = probe(4 * RECONCILE_BATCH);

        // What unwinding does: release what the failed work was holding.
        ALLOCATOR.set_hard_limit(ALLOCATOR.get_allocated() + 64 * 1024 * 1024);
        let recovered = probe(4 * RECONCILE_BATCH);

        disarm_limit();

        assert!(refused, "an allocation past the hard limit was served");
        assert!(
            !recovered,
            "the limit kept refusing after dropping under it"
        );
    }

    #[test]
    fn only_marked_threads_are_refused_when_scoped() {
        let _guard = setup();

        ALLOCATOR.set_enforcement(Enforcement::MarkedThreads);
        ALLOCATOR.flush();
        ALLOCATOR.set_hard_limit(ALLOCATOR.get_allocated().saturating_sub(1));

        let unmarked = probe(4 * RECONCILE_BATCH);
        ALLOCATOR.mark_current_thread(true);
        let marked = probe(4 * RECONCILE_BATCH);

        disarm_limit();

        assert!(!unmarked, "an unmarked thread was refused");
        assert!(marked, "a marked thread was not refused");
    }

    /// The callback runs *because* memory is tight, so the limit that made it
    /// run must not stop it doing its job.
    #[test]
    fn the_callback_is_exempt_from_the_hard_limit() {
        let _guard = setup();

        // Scoped to this thread, because the callback runs here and everything
        // else in the binary has to stay able to allocate.
        ALLOCATOR.set_enforcement(Enforcement::MarkedThreads);
        ALLOCATOR.mark_current_thread(true);

        ALLOCATOR.flush();
        let baseline = ALLOCATOR.get_allocated();
        ALLOCATOR.set_callback_threshold(baseline + 1024 * 1024);
        ALLOCATOR.set_hard_limit(baseline + 1024 * 1024);

        // Trips both at once, so the callback runs while over the limit.
        ALLOCATOR.set_allocated(baseline + 2 * 1024 * 1024);

        disarm_limit();
        ALLOCATOR.set_callback_threshold(usize::MAX);
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
        ALLOCATOR.set_callback_threshold(baseline + 1024 * 1024);
        assert_eq!(fires(), 0);

        ALLOCATOR.set_allocated(baseline + 2 * 1024 * 1024);
        assert_eq!(fires(), 1);

        // Put the shared total back so the next test starts from something real.
        ALLOCATOR.set_callback_threshold(usize::MAX);
        ALLOCATOR.set_allocated(baseline);
    }
}
