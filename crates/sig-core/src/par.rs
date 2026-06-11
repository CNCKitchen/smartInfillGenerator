// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Thin parallelism layer: rayon when the `parallel` feature is on, sequential
//! fallback otherwise (wasm32 without threads). All hot loops in the solver go
//! through these helpers so the two builds share one code path.

#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// Chunk size for vector ops: large enough to amortize task overhead.
const CHUNK: usize = 1 << 14;

/// y[i] += a * x[i]
pub fn axpy(y: &mut [f32], a: f32, x: &[f32]) {
    debug_assert_eq!(y.len(), x.len());
    #[cfg(feature = "parallel")]
    y.par_chunks_mut(CHUNK).zip(x.par_chunks(CHUNK)).for_each(|(yc, xc)| {
        for (yi, xi) in yc.iter_mut().zip(xc) {
            *yi += a * xi;
        }
    });
    #[cfg(not(feature = "parallel"))]
    for (yi, xi) in y.iter_mut().zip(x) {
        *yi += a * xi;
    }
}

/// y[i] = x[i] + b * y[i]  (xpby, used for CG direction update)
pub fn xpby(y: &mut [f32], x: &[f32], b: f32) {
    debug_assert_eq!(y.len(), x.len());
    #[cfg(feature = "parallel")]
    y.par_chunks_mut(CHUNK).zip(x.par_chunks(CHUNK)).for_each(|(yc, xc)| {
        for (yi, xi) in yc.iter_mut().zip(xc) {
            *yi = xi + b * *yi;
        }
    });
    #[cfg(not(feature = "parallel"))]
    for (yi, xi) in y.iter_mut().zip(x) {
        *yi = xi + b * *yi;
    }
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

pub fn fill(y: &mut [f32], v: f32) {
    #[cfg(feature = "parallel")]
    y.par_chunks_mut(CHUNK).for_each(|c| c.fill(v));
    #[cfg(not(feature = "parallel"))]
    y.fill(v);
}

pub fn copy(dst: &mut [f32], src: &[f32]) {
    debug_assert_eq!(dst.len(), src.len());
    #[cfg(feature = "parallel")]
    dst.par_chunks_mut(CHUNK).zip(src.par_chunks(CHUNK)).for_each(|(d, s)| d.copy_from_slice(s));
    #[cfg(not(feature = "parallel"))]
    dst.copy_from_slice(src);
}

/// Zero entries where mask is true.
pub fn mask_zero(y: &mut [f32], mask: &[bool]) {
    debug_assert_eq!(y.len(), mask.len());
    #[cfg(feature = "parallel")]
    y.par_chunks_mut(CHUNK).zip(mask.par_chunks(CHUNK)).for_each(|(yc, mc)| {
        for (yi, m) in yc.iter_mut().zip(mc) {
            if *m {
                *yi = 0.0;
            }
        }
    });
    #[cfg(not(feature = "parallel"))]
    for (yi, m) in y.iter_mut().zip(mask) {
        if *m {
            *yi = 0.0;
        }
    }
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

// ---- f64 variants for the outer (mixed-precision) CG loop ----

pub fn axpy64(y: &mut [f64], a: f64, x: &[f64]) {
    debug_assert_eq!(y.len(), x.len());
    #[cfg(feature = "parallel")]
    y.par_chunks_mut(CHUNK).zip(x.par_chunks(CHUNK)).for_each(|(yc, xc)| {
        for (yi, xi) in yc.iter_mut().zip(xc) {
            *yi += a * xi;
        }
    });
    #[cfg(not(feature = "parallel"))]
    for (yi, xi) in y.iter_mut().zip(x) {
        *yi += a * xi;
    }
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
    debug_assert_eq!(p.len(), z.len());
    #[cfg(feature = "parallel")]
    p.par_chunks_mut(CHUNK).zip(z.par_chunks(CHUNK)).for_each(|(pc, zc)| {
        for (pi, zi) in pc.iter_mut().zip(zc) {
            *pi = *zi as f64 + beta * *pi;
        }
    });
    #[cfg(not(feature = "parallel"))]
    for (pi, zi) in p.iter_mut().zip(z) {
        *pi = *zi as f64 + beta * *pi;
    }
}

pub fn demote(dst: &mut [f32], src: &[f64]) {
    debug_assert_eq!(dst.len(), src.len());
    #[cfg(feature = "parallel")]
    dst.par_chunks_mut(CHUNK).zip(src.par_chunks(CHUNK)).for_each(|(dc, sc)| {
        for (d, s) in dc.iter_mut().zip(sc) {
            *d = *s as f32;
        }
    });
    #[cfg(not(feature = "parallel"))]
    for (d, s) in dst.iter_mut().zip(src) {
        *d = *s as f32;
    }
}

pub fn promote(dst: &mut [f64], src: &[f32]) {
    debug_assert_eq!(dst.len(), src.len());
    #[cfg(feature = "parallel")]
    dst.par_chunks_mut(CHUNK).zip(src.par_chunks(CHUNK)).for_each(|(dc, sc)| {
        for (d, s) in dc.iter_mut().zip(sc) {
            *d = *s as f64;
        }
    });
    #[cfg(not(feature = "parallel"))]
    for (d, s) in dst.iter_mut().zip(src) {
        *d = *s as f64;
    }
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
}
