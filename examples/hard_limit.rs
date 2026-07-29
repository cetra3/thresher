//! The hard limit end to end: a worker that survives a query too big to run.
//!
//! ```bash
//! cargo run --example hard_limit --features alloc-error-hook
//! ```
//!
//! The seam has four parts, and all four are needed:
//!
//! ```text
//!   worker allocates past the limit
//!            |
//!            v
//!   Thresher::alloc returns null          <- still inside the allocator, no panic
//!            |
//!            v
//!   Rust calls the alloc error hook       <- ordinary safe code; panicking is fine here
//!            |
//!            v
//!   panic unwinds the worker's stack      <- the query's buffers are dropped on the way
//!            |
//!            v
//!   catch_unwind at the job boundary      <- one job fails, the process carries on
//! ```
//!
//! Note what is *not* here: no `#![feature(alloc_error_hook)]`. Feature gates
//! apply to the crate that uses them, and with the `alloc-error-hook` cargo
//! feature the gate lives in thresher's crate root instead of this one, so
//! downstream code stays plain Rust. The build still needs a nightly compiler,
//! because `set_alloc_error_hook` is unstable wherever it is called from.
//!
//! Without the hook at all, thresher is stable and a refused allocation aborts
//! with a message of your choosing at a limit you chose — worse than unwinding,
//! much better than being OOM killed silently.

use std::{
    alloc,
    panic::{self, AssertUnwindSafe},
    thread,
};

use thresher::{Enforcement, Thresher};

#[global_allocator]
static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);

const MIB: usize = 1024 * 1024;

/// Pretend units of work: one ordinary, one heavy enough to trip the warning but
/// still worth finishing, one that has to be stopped, then business as usual.
const JOBS: &[(&str, usize)] = &[
    ("small-scan", 8 * MIB),
    ("heavy-aggregate", 96 * MIB),
    ("runaway-join", 512 * MIB),
    ("small-scan-again", 8 * MIB),
];

fn main() {
    let baseline = THRESHER.get_allocated();

    // Warn at 64 MiB over where we started, refuse at 128 MiB over.
    THRESHER.set_threshold(baseline + 64 * MIB);
    THRESHER.set_limit(baseline + 128 * MIB);

    THRESHER.set_callback(|allocated| {
        // Runs inside the allocator: no panicking, no blocking, no I/O you care
        // about. A real one would nudge a task to dump a heap profile.
        eprintln!("[thresher] threshold reached at {} MiB", allocated / MIB);
    });

    // Only the worker pool may be refused. The main thread stays able to
    // allocate no matter how bad things get, so it can still report.
    THRESHER.set_enforcement(Enforcement::MarkedThreads);

    // Turns a refusal into a panic. We are back in safe code by the time this
    // runs, so panicking is fine — and the refused thread is allowed to
    // allocate again, which building the panic message needs.
    //
    // Write your own with `Thresher::take_refusal` if you want different
    // wording or a payload your `catch_unwind` can downcast.
    thresher::install_alloc_error_hook();

    // Quieten the default panic output; the catch below does the reporting.
    panic::set_hook(Box::new(|_| {}));

    let worker = thread::spawn(|| {
        THRESHER.mark_current_thread(true);

        for (name, size) in JOBS {
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| run_job(*size)));

            // The failed job has unwound and dropped what it was holding, so the
            // total is back under the limit and enforcement re-arms on its own.
            // Doing it explicitly means we do not depend on that: on a busy
            // server, other threads may be holding the total up.
            THRESHER.rearm_current_thread();

            match outcome {
                Ok(touched) => println!("{name}: ok, touched {} MiB", touched / MIB),
                Err(payload) => {
                    let reason = payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .unwrap_or("unknown");

                    println!("{name}: refused ({reason})");
                }
            }
        }
    });

    worker.join().expect("worker thread outlives its jobs");

    let _ = panic::take_hook();

    THRESHER.flush();
    println!(
        "\nstill running, {} KiB allocated",
        THRESHER.get_allocated() / 1024
    );
}

/// Allocate `size` bytes, touch them so they are really resident, and free them.
fn run_job(size: usize) -> usize {
    let mut buffer = vec![0u8; size];

    for page in buffer.chunks_mut(4096) {
        page[0] = 1;
    }

    buffer.len()
}
