// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

export type BcKind = "fixed" | "frictionless" | "force" | "pressure";

export interface Bc {
  id: string;
  kind: BcKind;
  /** Selected triangle indices. */
  tris: Uint32Array;
  /** Force vector in N (force only). */
  force?: [number, number, number];
  /** Pressure in MPa (pressure only). */
  pressure?: number;
}

export interface RbmMode {
  t: [number, number, number];
  r: [number, number, number];
  center: [number, number, number];
}

export interface ComponentReport {
  cells: number;
  constrained: boolean;
  lambdaRatio: number;
  hasLoads: boolean;
  mode: RbmMode | null;
}

export interface CheckReport {
  ok: boolean;
  islandCount: number;
  components: ComponentReport[];
}

export interface SolveStats {
  iterations: number;
  relResidual: number;
  /** False when the iteration cap hit first — result is approximate. */
  converged: boolean;
  maxDisplacement: number;
  /** Wall time measured in the worker, seconds. */
  seconds: number;
}

export interface VoxelInfo {
  nx: number;
  ny: number;
  nz: number;
  h: number;
  cells: number;
  solid: number;
}

export interface LoadedModel {
  positions: Float32Array;
  patchIds: Uint32Array;
  patchCount: number;
  triCount: number;
  bbox: [number, number, number, number, number, number];
}

export interface Material {
  name: string;
  e0: number; // MPa
  nu: number;
  density: number; // g/cm³
}

export const DEFAULT_MATERIALS: Material[] = [
  { name: "PLA", e0: 3500, nu: 0.35, density: 1.24 },
  { name: "PETG", e0: 2100, nu: 0.37, density: 1.27 },
  { name: "ABS", e0: 2250, nu: 0.37, density: 1.05 },
  { name: "ASA", e0: 2400, nu: 0.37, density: 1.07 },
];

export type PatternKey = "gyroid" | "cubic" | "grid";

/** Infill stiffness law E(ρ) = coeff · E₀ · ρ^exponent (Gibson–Ashby). */
export interface PatternCurve {
  coeff: number;
  exponent: number;
}

export const DEFAULT_CURVES: Record<PatternKey, PatternCurve> = {
  gyroid: { coeff: 1.0, exponent: 1.5 },
  cubic: { coeff: 1.0, exponent: 1.8 },
  grid: { coeff: 1.0, exponent: 2.0 },
};

export const RESOLUTIONS = {
  preview: 100_000,
  normal: 300_000,
  fine: 1_000_000,
} as const;

export type ResolutionKey = keyof typeof RESOLUTIONS;

/** Result fields selectable in the Deformed view. `u` is computed client-side
 *  from the displacement buffer; the rest come from the engine. */
export interface ResultFieldDef {
  value: string;
  label: string;
  unit: "mm" | "MPa" | "";
}

export const RESULT_FIELDS: ResultFieldDef[] = [
  { value: "u", label: "Displacement |u|", unit: "mm" },
  { value: "vm", label: "von Mises σ", unit: "MPa" },
  { value: "sxx", label: "Normal σxx", unit: "MPa" },
  { value: "syy", label: "Normal σyy", unit: "MPa" },
  { value: "szz", label: "Normal σzz", unit: "MPa" },
  { value: "sxy", label: "Shear τxy", unit: "MPa" },
  { value: "syz", label: "Shear τyz", unit: "MPa" },
  { value: "szx", label: "Shear τzx", unit: "MPa" },
  { value: "evm", label: "von Mises ε", unit: "" },
  { value: "exx", label: "Normal εxx", unit: "" },
  { value: "eyy", label: "Normal εyy", unit: "" },
  { value: "ezz", label: "Normal εzz", unit: "" },
  { value: "gxy", label: "Shear γxy", unit: "" },
  { value: "gyz", label: "Shear γyz", unit: "" },
  { value: "gzx", label: "Shear γzx", unit: "" },
];
