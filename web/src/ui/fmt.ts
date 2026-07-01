// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { cssColor, ramp } from "../viewer/colormaps";
import { format, formatParts, convertFromCanonical, unitLabel } from "../units";

// These helpers take CANONICAL values (mm) and render them in the user's chosen
// display unit via the registry chokepoint (see ../units.ts). They are the
// length/displacement display path; stress/force/etc. call `format(v, kind)`
// directly. Re-rendering on a unit change is driven by the store's `unitRev`.

/** Displacement/length: "0.42 mm" below 10 mm, "1.3 µm" for the tiny stuff
 *  (auto-magnitude lives in the registry). */
export function fmtDisp(mm: number): string {
  return format(mm, "length");
}

/** Same as fmtDisp but split for the DRO windows: ["0.42", "mm"]. */
export function fmtDispParts(mm: number): [string, string] {
  return formatParts(mm, "length");
}

/** Length NUMBER ONLY (no unit), adaptive precision, in the active length unit.
 *  Pair with `lenUnit()` for the trailing unit label (bbox dims etc.). */
export function fmtLen(mmCanonical: number): string {
  const x = convertFromCanonical(mmCanonical, "length");
  return x >= 100 ? x.toFixed(0) : x.toFixed(1);
}

/** Active length unit label (e.g. "mm", "in") for inline dimension readouts. */
export function lenUnit(): string {
  return unitLabel("length");
}

/** The viewer's region color ramp (legend dots in the region list). Shares the
 * one `ramp` definition with the surface paint + legend bar, so they match. */
export function rampCss(x: number): string {
  return cssColor(ramp, x);
}
