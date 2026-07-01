// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Live progress streaming out of a running solve.
//!
//! The companion to `cancel`: where cancellation lets the UI thread reach INTO
//! a blocked solve, this lets the solve push telemetry OUT while it runs. The
//! embedder (the web worker) installs a thread-local sink — a closure that
//! copies the residual trace into a SharedArrayBuffer the UI thread polls and
//! redraws — because a `postMessage` could never arrive while the worker is
//! blocked inside wasm. Threads without a sink (rayon workers, native tests
//! and benches unless they opt in) are a cheap no-op: one borrow + a `None`
//! check.
//!
//! The MGCG loop calls `publish` on a stride, NOT every iteration. The copy is
//! trivial, but repainting faster than the UI can (it polls at frame cadence)
//! is wasted work, and a stride keeps the hot loop's instrumentation to a
//! predictable handful of calls regardless of iteration count.

use std::cell::RefCell;

thread_local! {
    static SINK: RefCell<Option<Box<dyn Fn(&[f32])>>> = const { RefCell::new(None) };
}

/// Install (or clear) this thread's residual-progress sink. Must run on the
/// thread that drives the solve — the sink is thread-local.
pub fn set_sink(f: Option<Box<dyn Fn(&[f32])>>) {
    SINK.with(|c| *c.borrow_mut() = f);
}

/// Push the residual trace so far to the embedder. No-op when no sink is
/// installed; cheap enough to call from the solve loop.
pub fn publish(trace: &[f32]) {
    SINK.with(|c| {
        if let Some(f) = c.borrow().as_ref() {
            f(trace);
        }
    });
}
