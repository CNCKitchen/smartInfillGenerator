// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

/** "0.42 mm" below 10 mm precision, "1.3 µm" for the tiny stuff. */
export function fmtDisp(mm: number): string {
  if (mm >= 0.01) return `${mm.toFixed(2)} mm`;
  return `${(mm * 1000).toFixed(1)} µm`;
}

/** Same as fmtDisp but split for the DRO windows: ["0.42", "mm"]. */
export function fmtDispParts(mm: number): [string, string] {
  if (mm >= 0.01) return [mm.toFixed(2), "mm"];
  return [(mm * 1000).toFixed(1), "µm"];
}

/** Length in mm: integer above 100, one decimal below. */
export function fmtLen(x: number): string {
  return x >= 100 ? x.toFixed(0) : x.toFixed(1);
}

/** Mirrors the viewer's region color ramp (legend dots in the region list). */
export function rampCss(x: number): string {
  const t = Math.min(1, Math.max(0, x));
  let r: number, g: number, b: number;
  if (t < 0.33) {
    r = 0.15; g = 0.3 + 1.8 * t; b = 0.9;
  } else if (t < 0.66) {
    r = 0.15 + 2.4 * (t - 0.33); g = 0.9; b = 0.9 - 2.4 * (t - 0.33);
  } else {
    r = 0.95; g = 0.9 - 2.4 * (t - 0.66); b = 0.1;
  }
  const c = (v: number) => Math.round(255 * Math.min(1, Math.max(0, v)));
  return `rgb(${c(r)},${c(g)},${c(b)})`;
}
