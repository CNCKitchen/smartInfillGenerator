// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { cssColor, ramp } from "../viewer/colormaps";

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

/** The viewer's region color ramp (legend dots in the region list). Shares the
 * one `ramp` definition with the surface paint + legend bar, so they match. */
export function rampCss(x: number): string {
  return cssColor(ramp, x);
}
