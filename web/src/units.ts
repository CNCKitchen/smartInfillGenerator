// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// ─────────────────────────────────────────────────────────────────────────────
// Display-unit registry (see docs/units-design.md).
//
// THE ONE INVARIANT: the engine, store, solver and every stored number are
// CANONICAL forever — mm, N, and everything derived from them (MPa = N/mm², …).
// Units are a pure PRESENTATION layer: a value is converted to the user's unit
// only at render (`format`/`convertFromCanonical`) and back to canonical only at
// commit (`convertToCanonical`). Nothing downstream of an input widget ever sees
// a non-canonical number.
//
// Selection is PER-QUANTITY, grouped by conceptual quantity-kind (NOT by physical
// dimension): moment and energy share a dimension but never a unit; stress,
// pressure and modulus share a dimension but are three independent pickers.
// ─────────────────────────────────────────────────────────────────────────────

/** Every physical quantity the UI ever shows. The grouping key for unit
 *  selection — conceptual, not dimensional. */
export type QuantityKind =
  | "length"
  | "area"
  | "volume"
  | "force"
  | "moment"
  | "pressure"
  | "stress"
  | "modulus"
  | "mass"
  | "acceleration"
  | "density"
  | "strain"
  | "angle"
  | "energy";

// ─── compile-time quantity branding (opt-in; docs/units-design.md §3.2) ──────
// Phantom-tag a canonical number with its quantity so the wrong converter is a
// compile error: `formatStress(aLength)` won't type-check. Zero runtime cost
// (the tag is erased). This is the additive hardening pass — annotate engine
// result fields / store fields / constants as `Canonical<"stress">` etc. and the
// brand propagates to every reader for free. Bulk arrays use the Field wrapper.
declare const QBRAND: unique symbol;
/** A canonical (mm/N/MPa…) scalar tagged with its quantity kind. */
export type Canonical<K extends QuantityKind> = number & { readonly [QBRAND]: K };
/** Strip the brand (when handing a value to plain-number math/APIs). */
export const raw = (v: Canonical<QuantityKind>): number => v;
/** Tag a known-canonical number with a quantity kind (use at value birth). */
export const canonical = <K extends QuantityKind>(v: number, _kind: K): Canonical<K> =>
  v as Canonical<K>;
/** Bulk per-element field carrying its quantity kind — the chokepoint for
 *  legend/colorbar/3D-paint, which can't render without reading `kind`. */
export interface Field {
  kind: QuantityKind;
  data: Float32Array;
}

export interface UnitDef {
  /** Stable key persisted in localStorage — never change once shipped. */
  id: string;
  /** Display label, e.g. "mm", "N·m", "g/cm³". */
  label: string;
  /** Multiply a value in THIS unit by `toCanonical` to get the canonical value
   *  (canonical → display is therefore `value / toCanonical`). */
  toCanonical: number;
  /** Display precision for read-only readouts and as the input's snap. */
  decimals: number;
  /** Use exponential notation instead of fixed decimals (raw strain). */
  exp?: boolean;
  /** Magnitude auto-scaling for read-only surfaces only (never editable ones):
   *  when |display value| is below `threshold` (in display units), show it in a
   *  smaller companion unit instead — e.g. mm→µm, in→mil. */
  small?: { label: string; factor: number; decimals: number; threshold: number };
}

export interface QuantitySpec {
  kind: QuantityKind;
  /** Human label for the picker, e.g. "Length", "Stress". */
  label: string;
  /** Canonical unit id — the unit the engine/store actually hold. */
  canonical: string;
  units: UnitDef[];
}

// Conversion constants (exact where defined; rounded otherwise).
const IN = 25.4; // mm per inch
const LBF = 4.4482216152605; // N per lbf
const KGF = 9.80665; // N per kgf
const PSI = 0.0068947572931684; // MPa per psi
const G_PER_LB = 453.59237; // g per pound-mass
const G_PER_OZ = 28.349523125;
// Mass units that make the consistent systems' F=ma compose without a factor:
//   SI-mm: N = tonne·mm/s²  →  mass unit = tonne (1e6 g)
//   US-in: lbf = slinch·in/s² → mass unit = slinch (lbf·s²/in = 175.126…  kg)
const G_PER_TONNE = 1e6;
const G_PER_SLINCH = 175126.835; // g per slinch (lbf·s²/in)

/** The registry. One entry per quantity kind. */
export const QUANTITIES: Record<QuantityKind, QuantitySpec> = {
  length: {
    kind: "length",
    label: "Length",
    canonical: "mm",
    units: [
      { id: "mm", label: "mm", toCanonical: 1, decimals: 2, small: { label: "µm", factor: 1000, decimals: 1, threshold: 0.01 } },
      { id: "cm", label: "cm", toCanonical: 10, decimals: 3 },
      { id: "m", label: "m", toCanonical: 1000, decimals: 4 },
      { id: "in", label: "in", toCanonical: IN, decimals: 3, small: { label: "mil", factor: 1000, decimals: 1, threshold: 0.01 } },
    ],
  },
  area: {
    kind: "area",
    label: "Area",
    canonical: "mm2",
    units: [
      { id: "mm2", label: "mm²", toCanonical: 1, decimals: 1 },
      { id: "cm2", label: "cm²", toCanonical: 100, decimals: 2 },
      { id: "m2", label: "m²", toCanonical: 1e6, decimals: 4 },
      { id: "in2", label: "in²", toCanonical: IN * IN, decimals: 3 },
    ],
  },
  volume: {
    kind: "volume",
    label: "Volume",
    canonical: "mm3",
    units: [
      { id: "mm3", label: "mm³", toCanonical: 1, decimals: 0 },
      { id: "cm3", label: "cm³", toCanonical: 1000, decimals: 2 },
      { id: "L", label: "L", toCanonical: 1e6, decimals: 3 },
      { id: "in3", label: "in³", toCanonical: IN * IN * IN, decimals: 3 },
    ],
  },
  force: {
    kind: "force",
    label: "Force",
    canonical: "N",
    units: [
      { id: "N", label: "N", toCanonical: 1, decimals: 1 },
      { id: "kN", label: "kN", toCanonical: 1000, decimals: 3 },
      { id: "kgf", label: "kgf", toCanonical: KGF, decimals: 2 },
      { id: "lbf", label: "lbf", toCanonical: LBF, decimals: 2 },
    ],
  },
  moment: {
    kind: "moment",
    label: "Moment",
    canonical: "Nmm",
    units: [
      { id: "Nmm", label: "N·mm", toCanonical: 1, decimals: 0 },
      { id: "Nm", label: "N·m", toCanonical: 1000, decimals: 2 },
      { id: "kNm", label: "kN·m", toCanonical: 1e6, decimals: 3 },
      { id: "lbfin", label: "lbf·in", toCanonical: LBF * IN, decimals: 2 },
      { id: "lbfft", label: "lbf·ft", toCanonical: LBF * IN * 12, decimals: 2 },
    ],
  },
  pressure: {
    kind: "pressure",
    label: "Pressure",
    canonical: "MPa",
    units: [
      { id: "MPa", label: "MPa", toCanonical: 1, decimals: 3 },
      { id: "kPa", label: "kPa", toCanonical: 1e-3, decimals: 1 },
      { id: "bar", label: "bar", toCanonical: 0.1, decimals: 3 },
      { id: "psi", label: "psi", toCanonical: PSI, decimals: 1 },
    ],
  },
  stress: {
    kind: "stress",
    label: "Stress",
    canonical: "MPa",
    units: [
      { id: "MPa", label: "MPa", toCanonical: 1, decimals: 2 },
      { id: "kPa", label: "kPa", toCanonical: 1e-3, decimals: 0 },
      { id: "GPa", label: "GPa", toCanonical: 1000, decimals: 3 },
      { id: "psi", label: "psi", toCanonical: PSI, decimals: 0 },
      { id: "ksi", label: "ksi", toCanonical: PSI * 1000, decimals: 2 },
    ],
  },
  modulus: {
    kind: "modulus",
    label: "Modulus",
    canonical: "MPa",
    units: [
      { id: "MPa", label: "MPa", toCanonical: 1, decimals: 0 },
      { id: "GPa", label: "GPa", toCanonical: 1000, decimals: 2 },
      { id: "psi", label: "psi", toCanonical: PSI, decimals: 0 },
      { id: "ksi", label: "ksi", toCanonical: PSI * 1000, decimals: 1 },
      { id: "Mpsi", label: "Mpsi", toCanonical: PSI * 1e6, decimals: 3 },
    ],
  },
  mass: {
    kind: "mass",
    label: "Mass",
    canonical: "g",
    units: [
      { id: "g", label: "g", toCanonical: 1, decimals: 1 },
      { id: "kg", label: "kg", toCanonical: 1000, decimals: 3 },
      { id: "t", label: "t", toCanonical: G_PER_TONNE, decimals: 6 },
      { id: "oz", label: "oz", toCanonical: G_PER_OZ, decimals: 2 },
      { id: "lb", label: "lb", toCanonical: G_PER_LB, decimals: 3 },
      { id: "slinch", label: "slinch", toCanonical: G_PER_SLINCH, decimals: 6 },
    ],
  },
  acceleration: {
    kind: "acceleration",
    label: "Acceleration",
    // Canonical mm/s² — the SI-mm consistent accel (N = tonne·mm/s²). Default
    // display is g (DESIGN §16 dec. 2): 1 g₀ = 9810 mm/s² (the engine's rounded
    // convention, matching the regbench/self-weight anchors).
    canonical: "mms2",
    units: [
      { id: "g", label: "g", toCanonical: 9810, decimals: 3 },
      { id: "ms2", label: "m/s²", toCanonical: 1000, decimals: 2 },
      { id: "mms2", label: "mm/s²", toCanonical: 1, decimals: 0 },
      { id: "ins2", label: "in/s²", toCanonical: IN, decimals: 1 },
    ],
  },
  density: {
    kind: "density",
    label: "Density",
    canonical: "gcm3",
    units: [
      { id: "gcm3", label: "g/cm³", toCanonical: 1, decimals: 3 },
      { id: "kgm3", label: "kg/m³", toCanonical: 1e-3, decimals: 0 },
      { id: "lbin3", label: "lb/in³", toCanonical: G_PER_LB / (IN * IN * IN), decimals: 4 },
      // consistent-system densities (mass-unit / length-unit³):
      { id: "tmm3", label: "t/mm³", toCanonical: G_PER_TONNE / 1, decimals: 12 },
      { id: "slinchin3", label: "slinch/in³", toCanonical: G_PER_SLINCH / (IN * IN * IN), decimals: 6 },
    ],
  },
  strain: {
    kind: "strain",
    label: "Strain",
    canonical: "raw",
    units: [
      { id: "raw", label: "", toCanonical: 1, decimals: 4, exp: true },
      { id: "pct", label: "%", toCanonical: 0.01, decimals: 3 },
      { id: "ue", label: "µε", toCanonical: 1e-6, decimals: 0 },
    ],
  },
  angle: {
    kind: "angle",
    label: "Angle",
    canonical: "deg",
    units: [
      { id: "deg", label: "°", toCanonical: 1, decimals: 1 },
      { id: "rad", label: "rad", toCanonical: 180 / Math.PI, decimals: 4 },
    ],
  },
  energy: {
    kind: "energy",
    label: "Energy",
    canonical: "Nmm",
    units: [
      { id: "Nmm", label: "N·mm", toCanonical: 1, decimals: 1 },
      { id: "J", label: "J", toCanonical: 1000, decimals: 3 },
      { id: "inlbf", label: "in·lbf", toCanonical: LBF * IN, decimals: 3 },
    ],
  },
};

export const QUANTITY_KINDS = Object.keys(QUANTITIES) as QuantityKind[];

/** A full unit selection: one unit id per quantity kind. */
export type UnitPrefs = Record<QuantityKind, string>;

export interface PresetDef {
  id: string;
  label: string;
  /** Consistent systems guarantee every readout composes (σ = F/A multiplies
   *  out) — at the cost of idiomatic prefixes (no GPa, no N·m). The popover
   *  greys out per-quantity overrides while a consistent preset is active. */
  consistent: boolean;
  units: UnitPrefs;
}

export const PRESETS: Record<string, PresetDef> = {
  metric: {
    id: "metric",
    label: "Metric",
    consistent: false,
    units: {
      length: "mm",
      area: "mm2",
      volume: "mm3",
      force: "N",
      moment: "Nm",
      pressure: "MPa",
      stress: "MPa",
      modulus: "GPa",
      mass: "g",
      acceleration: "g",
      density: "gcm3",
      strain: "pct",
      angle: "deg",
      energy: "J",
    },
  },
  imperial: {
    id: "imperial",
    label: "Imperial",
    consistent: false,
    units: {
      length: "in",
      area: "in2",
      volume: "in3",
      force: "lbf",
      moment: "lbfin",
      pressure: "psi",
      stress: "psi",
      modulus: "ksi",
      mass: "lb",
      acceleration: "g",
      density: "lbin3",
      strain: "pct",
      angle: "deg",
      energy: "inlbf",
    },
  },
  simm: {
    id: "simm",
    label: "SI-mm",
    consistent: true,
    units: {
      length: "mm",
      area: "mm2",
      volume: "mm3",
      force: "N",
      moment: "Nmm",
      pressure: "MPa",
      stress: "MPa",
      modulus: "MPa",
      mass: "t",
      acceleration: "mms2",
      density: "tmm3",
      strain: "raw",
      angle: "rad",
      energy: "Nmm",
    },
  },
  usin: {
    id: "usin",
    label: "US-in",
    consistent: true,
    units: {
      length: "in",
      area: "in2",
      volume: "in3",
      force: "lbf",
      moment: "lbfin",
      pressure: "psi",
      stress: "psi",
      modulus: "psi",
      mass: "slinch",
      acceleration: "ins2",
      density: "slinchin3",
      strain: "raw",
      angle: "rad",
      energy: "inlbf",
    },
  },
};

export const DEFAULT_PRESET = "metric";

/** Identify which preset (if any) a selection matches — drives the popover's
 *  active-preset highlight and the "Custom" badge. */
export function presetOf(prefs: UnitPrefs): string | null {
  for (const p of Object.values(PRESETS)) {
    if (QUANTITY_KINDS.every((k) => p.units[k] === prefs[k])) return p.id;
  }
  return null;
}

// ─── active selection (module mirror of the store, for the format chokepoint) ──
// The store is the source of truth; it calls `setActiveUnits` on every change so
// the synchronous `format`/`convert*` helpers (used in render) always read the
// live selection. Display components re-render on the store's `unitRev` bump.

let active: UnitPrefs = { ...PRESETS[DEFAULT_PRESET].units };

export function setActiveUnits(prefs: UnitPrefs): void {
  active = { ...prefs };
}

export function activeUnits(): UnitPrefs {
  return active;
}

/** The chosen UnitDef for a kind (falls back to canonical if a stale id slips
 *  through). */
export function unitDef(kind: QuantityKind): UnitDef {
  const spec = QUANTITIES[kind];
  const id = active[kind];
  return spec.units.find((u) => u.id === id) ?? spec.units.find((u) => u.id === spec.canonical) ?? spec.units[0];
}

/** Display label of the active unit for a kind (e.g. "mm", "MPa"). */
export function unitLabel(kind: QuantityKind): string {
  return unitDef(kind).label;
}

/** Canonical → display number (no formatting, no unit string). */
export function convertFromCanonical(canonical: number, kind: QuantityKind): number {
  return canonical / unitDef(kind).toCanonical;
}

/** Display → canonical number. Use at commit time on the user's typed value. */
export function convertToCanonical(display: number, kind: QuantityKind): number {
  return display * unitDef(kind).toCanonical;
}

/** Convert a value given an explicit unit id (not the active one) → canonical.
 *  Used for one-time conversions like STL import. */
export function convertUnitToCanonical(display: number, kind: QuantityKind, unitId: string): number {
  const u = QUANTITIES[kind].units.find((x) => x.id === unitId);
  return display * (u?.toCanonical ?? 1);
}

interface FmtOpts {
  /** Append the unit label (default true). */
  unit?: boolean;
  /** Disable magnitude auto-scaling (e.g. mm↔µm) — use on editable surfaces. */
  noScale?: boolean;
  /** Override decimals. */
  decimals?: number;
}

/** Format a canonical value for display: "0.42 mm", "1.3 µm", "120 MPa".
 *  THE display chokepoint — every read-only readout routes through here. */
export function format(canonical: number, kind: QuantityKind, opts: FmtOpts = {}): string {
  const [num, lbl] = formatParts(canonical, kind, opts);
  return opts.unit === false || lbl === "" ? num : `${num} ${lbl}`;
}

/** Like `format` but split into [number, unitLabel] for DRO-style windows. */
export function formatParts(
  canonical: number,
  kind: QuantityKind,
  opts: FmtOpts = {}
): [string, string] {
  const u = unitDef(kind);
  const disp = canonical / u.toCanonical;
  const a = Math.abs(disp);
  if (!opts.noScale && u.small && a > 0 && a < u.small.threshold) {
    return [(disp * u.small.factor).toFixed(u.small.decimals), u.small.label];
  }
  const dec = opts.decimals ?? u.decimals;
  if (u.exp) return [disp === 0 ? "0" : disp.toExponential(Math.max(2, dec)), u.label];
  return [disp.toFixed(dec), u.label];
}
