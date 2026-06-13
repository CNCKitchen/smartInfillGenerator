// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Cooperative cancellation for long-running solves/optimizations.
//!
//! The embedder installs a thread-local checker — in the web app a closure
//! reading a SharedArrayBuffer flag that the UI thread sets while the worker
//! is blocked inside wasm (a postMessage could never arrive mid-solve). The
//! MGCG iteration loop and the SIMP outer loop poll `requested()` and bail
//! out early; callers surface `Cancelled` errors. Threads without a checker
//! (rayon workers, native tests) always read `false`.

use std::cell::RefCell;

thread_local! {
    static CHECKER: RefCell<Option<Box<dyn Fn() -> bool>>> = const { RefCell::new(None) };
}

/// Install (or clear) this thread's cancellation checker.
pub fn set_checker(f: Option<Box<dyn Fn() -> bool>>) {
    CHECKER.with(|c| *c.borrow_mut() = f);
}

/// True when the embedder requested a stop. Cheap enough to poll per
/// CG iteration.
pub fn requested() -> bool {
    CHECKER.with(|c| c.borrow().as_ref().is_some_and(|f| f()))
}
