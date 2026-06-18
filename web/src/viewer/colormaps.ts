// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Single source of truth for the viewer's colormaps. The GPU LUT (painted
//! surface), the legend gradient bars, and the region-list / hover-probe
//! swatches all derive from THESE functions, so the legend can never disagree
//! with the surface it labels — a credibility issue for an FEA result view.
//! Pure math (no THREE dependency): a colormap maps t∈[0,1] → normalized RGB.

export type RGB = [number, number, number];

const clamp01 = (x: number) => Math.min(1, Math.max(0, x));

/** Compact blue→cyan→yellow→red ramp (density + region colors). */
export function ramp(x: number): RGB {
  const t = clamp01(x);
  if (t < 0.33) return [0.15, 0.3 + 1.8 * t, 0.9];
  if (t < 0.66) return [0.15 + 2.4 * (t - 0.33), 0.9, 0.9 - 2.4 * (t - 0.33)];
  return [0.95, 0.9 - 2.4 * (t - 0.66), 0.1];
}

/** Classic jet colormap (displacement / stress / SF fields). */
export function jet(x: number): RGB {
  const t = clamp01(x);
  return [
    clamp01(1.5 - Math.abs(4 * t - 3)),
    clamp01(1.5 - Math.abs(4 * t - 2)),
    clamp01(1.5 - Math.abs(4 * t - 1)),
  ];
}

const byte = (v: number) => Math.round(255 * clamp01(v));

/** DEFAULT band count for the discrete ("contour banded") result display — the
 *  GPU LUT and the legend bar both quantize into the live count, so they stay in
 *  sync. The user scrolls the legend bar to change it, clamped to MIN..MAX. */
export const CONTOUR_BANDS = 10;
export const CONTOUR_BANDS_MIN = 2;
export const CONTOUR_BANDS_MAX = 20;

/** `rgb(r,g,b)` string for a colormap at parameter `t`. */
export function cssColor(fn: (t: number) => RGB, t: number): string {
  const [r, g, b] = fn(t);
  return `rgb(${byte(r)},${byte(g)},${byte(b)})`;
}

/**
 * CSS `linear-gradient(to top, …)` sampling a colormap at `steps`+1 stops.
 * `flip` reverses it (e.g. safety factor: red marks the LOW/critical end).
 */
export function cssGradient(fn: (t: number) => RGB, flip = false, steps = 8): string {
  const stops: string[] = [];
  for (let i = 0; i <= steps; i++) {
    const pos = i / steps;
    stops.push(`${cssColor(fn, flip ? 1 - pos : pos)} ${(100 * pos).toFixed(1)}%`);
  }
  return `linear-gradient(to top, ${stops.join(", ")})`;
}

/**
 * Discrete (banded) version of {@link cssGradient}: `n` flat color blocks with
 * hard edges, each painted at its band-center color — the legend twin of the
 * quantized GPU LUT. `flip` reverses it (safety factor).
 */
export function cssBands(fn: (t: number) => RGB, flip = false, n = CONTOUR_BANDS): string {
  const stops: string[] = [];
  for (let b = 0; b < n; b++) {
    const tc = (b + 0.5) / n; // band-center color
    const col = cssColor(fn, flip ? 1 - tc : tc);
    stops.push(`${col} ${((100 * b) / n).toFixed(2)}%`, `${col} ${((100 * (b + 1)) / n).toFixed(2)}%`);
  }
  return `linear-gradient(to top, ${stops.join(", ")})`;
}
