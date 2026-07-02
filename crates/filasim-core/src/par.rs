// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Thin parallelism layer: rayon when the `parallel` feature is on, sequential
//! fallback otherwise (wasm32 without threads). All hot loops in the solver go
//! through these helpers so the two builds share one code path.

#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// Chunk size for vector ops: large enough to amortize task overhead.
/// (Only referenced by the threaded helpers below.)
#[cfg(feature = "parallel")]
const CHUNK: usize = 1 << 14;

// The native-vs-wasm threading split lives in exactly these two drivers; every
// element-wise vector op routes through them instead of repeating the
// `#[cfg(parallel)]` pair. Each op's closure is element-wise within its chunk,
// so splitting into parallel chunks and running over the whole slice once
// (sequential) produce identical results — no reductions go through here.

/// Apply `f` to each chunk of `a` (parallel chunks when threaded; the whole
/// slice as one chunk otherwise).
#[inline]
fn each_chunk_mut<A: Send, F: Fn(&mut [A]) + Sync>(a: &mut [A], f: F) {
    #[cfg(feature = "parallel")]
    a.par_chunks_mut(CHUNK).for_each(|c| f(c));
    #[cfg(not(feature = "parallel"))]
    f(a);
}

/// Like `each_chunk_mut` but zips a second, read-only slice in lockstep.
#[inline]
fn zip_chunks_mut<A: Send, B: Sync, F: Fn(&mut [A], &[B]) + Sync>(a: &mut [A], b: &[B], f: F) {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(feature = "parallel")]
    a.par_chunks_mut(CHUNK).zip(b.par_chunks(CHUNK)).for_each(|(ac, bc)| f(ac, bc));
    #[cfg(not(feature = "parallel"))]
    f(a, b);
}

/// y[i] += a * x[i]
pub fn axpy(y: &mut [f32], a: f32, x: &[f32]) {
    zip_chunks_mut(y, x, |yc, xc| {
        for (yi, xi) in yc.iter_mut().zip(xc) {
            *yi += a * xi;
        }
    });
}

/// y[i] = x[i] + b * y[i]  (xpby, used for CG direction update)
pub fn xpby(y: &mut [f32], x: &[f32], b: f32) {
    zip_chunks_mut(y, x, |yc, xc| {
        for (yi, xi) in yc.iter_mut().zip(xc) {
            *yi = xi + b * *yi;
        }
    });
}

/// Dot product with f64 accumulation (keeps CG orthogonality honest in f32).
pub fn dot(a: &[f32], b: &[f32]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(feature = "parallel")]
    {
        a.par_chunks(CHUNK)
            .zip(b.par_chunks(CHUNK))
            .map(|(ac, bc)| ac.iter().zip(bc).map(|(x, y)| *x as f64 * *y as f64).sum::<f64>())
            .sum()
    }
    #[cfg(not(feature = "parallel"))]
    {
        a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum()
    }
}

pub fn norm2(a: &[f32]) -> f64 {
    dot(a, a).sqrt()
}

/// Parallel "for each index" over a range, writing into disjoint slots of `out`
/// computed from the index (out[i] = f(i)).
pub fn map_indexed<T: Send + Sync, F: Fn(usize) -> T + Sync>(out: &mut [T], f: F) {
    #[cfg(feature = "parallel")]
    out.par_iter_mut().enumerate().for_each(|(i, o)| *o = f(i));
    #[cfg(not(feature = "parallel"))]
    for (i, o) in out.iter_mut().enumerate() {
        *o = f(i);
    }
}

/// Run `f(item)` for every item in parallel. Caller guarantees any shared
/// writes are disjoint (see UnsafeSlice).
pub fn for_each<T: Sync, F: Fn(&T) + Sync + Send>(items: &[T], f: F) {
    #[cfg(feature = "parallel")]
    items.par_iter().for_each(|it| f(it));
    #[cfg(not(feature = "parallel"))]
    for it in items {
        f(it);
    }
}

/// out[i] = a[i] - b[i]
pub fn sub(out: &mut [f32], a: &[f32], b: &[f32]) {
    debug_assert!(out.len() == a.len() && out.len() == b.len());
    #[cfg(feature = "parallel")]
    out.par_chunks_mut(CHUNK).zip(a.par_chunks(CHUNK)).zip(b.par_chunks(CHUNK)).for_each(
        |((oc, ac), bc)| {
            for ((o, x), y) in oc.iter_mut().zip(ac).zip(bc) {
                *o = x - y;
            }
        },
    );
    #[cfg(not(feature = "parallel"))]
    for ((o, x), y) in out.iter_mut().zip(a).zip(b) {
        *o = x - y;
    }
}

/// y[i] = a * y[i] + b * x[i]  (Chebyshev direction update)
pub fn axpby(y: &mut [f32], a: f32, b: f32, x: &[f32]) {
    zip_chunks_mut(y, x, |yc, xc| {
        for (yi, xi) in yc.iter_mut().zip(xc) {
            *yi = a * *yi + b * xi;
        }
    });
}

pub fn fill(y: &mut [f32], v: f32) {
    each_chunk_mut(y, |c| c.fill(v));
}

pub fn copy(dst: &mut [f32], src: &[f32]) {
    zip_chunks_mut(dst, src, |d, s| d.copy_from_slice(s));
}

/// Zero entries where mask is true.
pub fn mask_zero(y: &mut [f32], mask: &[bool]) {
    zip_chunks_mut(y, mask, |yc, mc| {
        for (yi, m) in yc.iter_mut().zip(mc) {
            if *m {
                *yi = 0.0;
            }
        }
    });
}

/// Parallel iteration over equal-size chunks of `data`; the callback receives
/// the element offset of its chunk. Used for node-blocked solver loops.
pub fn chunks_mut_indexed<F: Fn(usize, &mut [f32]) + Sync>(data: &mut [f32], chunk: usize, f: F) {
    #[cfg(feature = "parallel")]
    data.par_chunks_mut(chunk).enumerate().for_each(|(i, c)| f(i * chunk, c));
    #[cfg(not(feature = "parallel"))]
    for (i, c) in data.chunks_mut(chunk).enumerate() {
        f(i * chunk, c);
    }
}

/// Like `chunks_mut_indexed` but over TWO mutable slices in lockstep (the
/// fused smoother updates z and d in one pass).
pub fn chunks2_mut_indexed<F: Fn(usize, &mut [f32], &mut [f32]) + Sync>(
    a: &mut [f32],
    b: &mut [f32],
    chunk: usize,
    f: F,
) {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(feature = "parallel")]
    a.par_chunks_mut(chunk)
        .zip(b.par_chunks_mut(chunk))
        .enumerate()
        .for_each(|(i, (ac, bc))| f(i * chunk, ac, bc));
    #[cfg(not(feature = "parallel"))]
    for (i, (ac, bc)) in a.chunks_mut(chunk).zip(b.chunks_mut(chunk)).enumerate() {
        f(i * chunk, ac, bc);
    }
}

/// Parallel iteration over chunk ranges [start, end) of a length-`len` index
/// space (sequential fallback: one whole-range call). The blocked driver for
/// kernels that stream several vectors in lockstep (modal Rayleigh–Ritz).
pub fn for_each_range<F: Fn(usize, usize) + Sync>(len: usize, chunk: usize, f: F) {
    #[cfg(feature = "parallel")]
    {
        let nch = len.div_ceil(chunk).max(1);
        (0..nch).into_par_iter().for_each(|i| {
            let s = i * chunk;
            f(s, (s + chunk).min(len));
        });
    }
    #[cfg(not(feature = "parallel"))]
    {
        let _ = chunk;
        f(0, len);
    }
}

/// Blocked parallel map-reduce over chunk ranges (associative `add`; chunked
/// summation order differs from a single pass — callers accept that).
pub fn map_reduce_ranges<T, F, A, Z>(len: usize, chunk: usize, f: F, add: A, zero: Z) -> T
where
    T: Send,
    F: Fn(usize, usize) -> T + Sync,
    A: Fn(T, T) -> T + Sync + Send,
    Z: Fn() -> T + Sync + Send,
{
    #[cfg(feature = "parallel")]
    {
        let nch = len.div_ceil(chunk).max(1);
        (0..nch)
            .into_par_iter()
            .map(|i| {
                let s = i * chunk;
                f(s, (s + chunk).min(len))
            })
            .reduce(zero, add)
    }
    #[cfg(not(feature = "parallel"))]
    {
        let _ = (add, zero, chunk);
        f(0, len)
    }
}

// ---- f64 variants for the outer (mixed-precision) CG loop ----

/// f64 twin of `chunks_mut_indexed`.
pub fn chunks_mut_indexed64<F: Fn(usize, &mut [f64]) + Sync>(
    data: &mut [f64],
    chunk: usize,
    f: F,
) {
    #[cfg(feature = "parallel")]
    data.par_chunks_mut(chunk).enumerate().for_each(|(i, c)| f(i * chunk, c));
    #[cfg(not(feature = "parallel"))]
    for (i, c) in data.chunks_mut(chunk).enumerate() {
        f(i * chunk, c);
    }
}

/// Weighted dot `Σ w[i]·a[i]·b[i]` (the modal M-inner-product; lumped mass is
/// diagonal). Chunk-parallel with f64 accumulation per chunk.
pub fn dot_w64(w: &[f64], a: &[f64], b: &[f64]) -> f64 {
    debug_assert!(w.len() == a.len() && a.len() == b.len());
    map_reduce_ranges(
        a.len(),
        CHUNK64,
        |s, e| {
            let (wc, ac, bc) = (&w[s..e], &a[s..e], &b[s..e]);
            let mut acc = 0.0;
            for i in 0..ac.len() {
                acc += wc[i] * ac[i] * bc[i];
            }
            acc
        },
        |x, y| x + y,
        || 0.0,
    )
}

/// v *= s
pub fn scale64(v: &mut [f64], s: f64) {
    each_chunk_mut(v, |c| {
        for x in c.iter_mut() {
            *x *= s;
        }
    });
}

/// Chunk size for the f64 range helpers (bytes-comparable to `CHUNK` for f32).
pub const CHUNK64: usize = 1 << 13;

pub fn axpy64(y: &mut [f64], a: f64, x: &[f64]) {
    zip_chunks_mut(y, x, |yc, xc| {
        for (yi, xi) in yc.iter_mut().zip(xc) {
            *yi += a * xi;
        }
    });
}

pub fn dot64(a: &[f64], b: &[f64]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(feature = "parallel")]
    {
        a.par_chunks(CHUNK)
            .zip(b.par_chunks(CHUNK))
            .map(|(ac, bc)| ac.iter().zip(bc).map(|(x, y)| x * y).sum::<f64>())
            .sum()
    }
    #[cfg(not(feature = "parallel"))]
    {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }
}

pub fn norm2_64(a: &[f64]) -> f64 {
    dot64(a, a).sqrt()
}

/// dot(a_f64, b_f32) promoting on the fly.
pub fn dot_mixed(a: &[f64], b: &[f32]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(feature = "parallel")]
    {
        a.par_chunks(CHUNK)
            .zip(b.par_chunks(CHUNK))
            .map(|(ac, bc)| ac.iter().zip(bc).map(|(x, y)| x * *y as f64).sum::<f64>())
            .sum()
    }
    #[cfg(not(feature = "parallel"))]
    {
        a.iter().zip(b).map(|(x, y)| x * *y as f64).sum()
    }
}

/// p = z + beta * p with f32 z promoted into the f64 direction vector.
pub fn xpby_mixed(p: &mut [f64], z: &[f32], beta: f64) {
    zip_chunks_mut(p, z, |pc, zc| {
        for (pi, zi) in pc.iter_mut().zip(zc) {
            *pi = *zi as f64 + beta * *pi;
        }
    });
}

pub fn demote(dst: &mut [f32], src: &[f64]) {
    zip_chunks_mut(dst, src, |dc, sc| {
        for (d, s) in dc.iter_mut().zip(sc) {
            *d = *s as f32;
        }
    });
}

pub fn promote(dst: &mut [f64], src: &[f32]) {
    zip_chunks_mut(dst, src, |dc, sc| {
        for (d, s) in dc.iter_mut().zip(sc) {
            *d = *s as f64;
        }
    });
}

/// Shared mutable slice for scatter writes that are disjoint BY CONSTRUCTION
/// (e.g. cells of one color in an 8-colored hex grid never share nodes).
/// Safety rests on that invariant; callers must uphold it.
pub struct UnsafeSlice<'a, T> {
    ptr: *mut T,
    len: usize,
    _marker: std::marker::PhantomData<&'a mut T>,
}

unsafe impl<'a, T: Send + Sync> Sync for UnsafeSlice<'a, T> {}
unsafe impl<'a, T: Send + Sync> Send for UnsafeSlice<'a, T> {}

impl<'a, T> UnsafeSlice<'a, T> {
    pub fn new(slice: &'a mut [T]) -> Self {
        Self { ptr: slice.as_mut_ptr(), len: slice.len(), _marker: std::marker::PhantomData }
    }

    /// # Safety
    /// No two concurrent calls may target the same index.
    #[inline(always)]
    pub unsafe fn get_mut(&self, i: usize) -> &mut T {
        debug_assert!(i < self.len);
        &mut *self.ptr.add(i)
    }

    /// Reborrow a sub-range as a plain mutable slice (vectorizes better than
    /// per-element `get_mut` in blocked kernels).
    /// # Safety
    /// No two concurrent calls may overlap ranges.
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn slice_mut(&self, start: usize, len: usize) -> &mut [T] {
        debug_assert!(start + len <= self.len);
        std::slice::from_raw_parts_mut(self.ptr.add(start), len)
    }
}
