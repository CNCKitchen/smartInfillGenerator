// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

export type BcKind =
  | "fixed"
  | "frictionless"
  | "displacement"
  | "elastic"
  | "force"
  | "pressure"
  | "bearing"
  | "moment";

/** Fitted cylinder for a bearing load: axis + radius recovered from the
 *  selected surface, plus the cylindricity check. `ok=false` means the
 *  selection isn't cylindrical and the bearing load can't be applied. */
export interface CylFit {
  ok: boolean;
  axis: [number, number, number];
  point: [number, number, number];
  radius: number;
  residual: number;
}

/** How a force load is defined in the UI. "components" edits Fx/Fy/Fz
 *  directly; "direction" edits a unit direction + a scalar magnitude. Either
 *  way the resolved vector is stored in `force` for the solver. */
export type ForceMode = "components" | "direction";

export interface Bc {
  id: string;
  kind: BcKind;
  /** User-facing name (e.g. "Motor mount", "Tip load"). Defaults to an
   *  auto-numbered kind label ("Force 1") so steps/tables read clearly. */
  name?: string;
  /** Selected triangle indices. */
  tris: Uint32Array;
  /** Force vector in N (force only) — the resolved load the solver uses. */
  force?: [number, number, number];
  /** Pressure in MPa (pressure only). */
  pressure?: number;
  /** Foundation bedding modulus in N/mm³, σ = k·u (elastic only). */
  stiffness?: number;
  /** Which global axes are enforced (displacement support only). */
  axes?: [boolean, boolean, boolean];
  /** Prescribed displacement per global axis in mm (displacement support only);
   *  0 = pin to zero. Only axes enabled in `axes` are enforced. */
  disp?: [number, number, number];
  /** Force definition mode (force only); defaults to "direction". */
  forceMode?: ForceMode;
  /** Unit direction for "direction" mode (force only). */
  forceDir?: [number, number, number];
  /** Magnitude in N for "direction" mode (force only). */
  forceMag?: number;
  /** True while the direction auto-tracks the selection's area-weighted
   *  average normal; cleared once the user picks/edits a direction. */
  forceDirAuto?: boolean;
  // --- Bearing load (kind "bearing"): reuses force / forceMode / forceDir /
  //     forceMag as the bearing force vector (total N). The loaded half of the
  //     bore is the half this vector points into. `cyl` caches the fit of the
  //     selected cylindrical surface. ---
  /** Fitted cylinder for the bearing selection (bearing only); null until a
   *  valid cylindrical surface is selected. */
  cyl?: CylFit | null;
  /** Transient validation message when a bearing selection isn't cylindrical. */
  cylError?: string;
  // --- Moment (kind "moment"): a deformable distributed couple (N·mm). Dual
  //     mode like force, but its own fields so the two never collide. ---
  /** Resolved moment vector in N·mm (moment only) — what the solver uses. */
  moment?: [number, number, number];
  /** Moment definition mode (moment only); defaults to "components". */
  momentMode?: ForceMode;
  /** Unit axis for "direction" mode (moment only). */
  momentDir?: [number, number, number];
  /** Magnitude in N·mm for "direction" mode (moment only). */
  momentMag?: number;
}

/** Per-load-step override of a single BC (see DESIGN §13). Absent fields
 *  inherit the base `Bc`; `active` defaults true. For SUPPORTS only `active`
 *  is meaningful (selection/params stay shared). LOADS additionally carry a
 *  full per-step value — a resolved force vector or a pressure. */
export interface LoadStepOverride {
  /** Whether this BC participates in the step. Absent = true. */
  active?: boolean;
  /** Full per-step force vector in N (force AND bearing BCs — bearing stores its
   *  push force in `force`); absent = inherit base. */
  force?: [number, number, number];
  /** Per-step pressure in MPa (pressure BCs); absent = inherit base. */
  pressure?: number;
  /** Full per-step moment vector in N·mm (moment BCs); absent = inherit base. */
  moment?: [number, number, number];
}

/** One FEA load case. The shared `bcs` array defines geometry/selection ONCE;
 *  each load step layers a thin override map on top. The default single-step
 *  model keeps empty overrides — every BC active at its base value — so the
 *  single-case setup is identical to having no load steps at all. */
export interface LoadStep {
  id: string;
  name: string;
  /** Keyed by BC id. Absent entry = that BC active at its base value. */
  overrides: Record<string, LoadStepOverride>;
  /** Whether this step feeds the multi-load optimizer (weighted-sum). */
  includeInOptimize: boolean;
  /** Relative weight in the weighted-sum optimizer objective. */
  weight: number;
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
  /** Relative residual per MGCG iteration (element 0 = initial residual). */
  residuals?: number[];
  /** MGCG relative-residual convergence target — the plot's limit line. */
  tol?: number;
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
  /** True when imported from STEP: exact BREP faces are available as a
   *  surface-patch source (see store `segSource`). */
  hasCadFaces: boolean;
}

export interface Material {
  name: string;
  e0: number; // MPa
  nu: number;
  density: number; // g/cm³
  /** Tensile strength in MPa (printed, conservative datasheet value) —
   *  drives the safety-factor plot. */
  strength: number;
  /** Layer-adhesion strength in MPa: tension PERPENDICULAR to the layers
   *  (σzz, Z-up build direction). Typically 50–80% of σₜ — drives the
   *  conservative "worst case" safety factor. */
  strengthZ: number;
  /** Build-sim: IN-PLANE (XY) process shrink as a fraction (0.004 = 0.4%) — the
   *  dominant warp driver (in-plane contraction fought by the bed). The
   *  inherent-strain warp scales linearly with it. Uncalibrated rough defaults;
   *  this is a MATERIAL property, not a per-run knob. */
  shrink: number;
  /** Build-sim: THROUGH-LAYER (Z) process shrink as a fraction. FDM is
   *  transversely isotropic — at voxel scale the bead-direction detail averages
   *  out and what survives is in-plane vs build-direction. Usually < in-plane
   *  (the interlayer accommodates Z). Equal to `shrink` ⇒ isotropic. */
  shrinkZ: number;
  /** Build-sim: yield stress in MPa. Drives the elastic–perfectly-plastic step
   *  that locks in the incompatible plastic strain near the bed — without it a
   *  uniform-eigenstrain release is the stress-free compatible shrink, which is
   *  density-blind (0 % and 100 % infill warp identically). Rough printed value;
   *  uncalibrated. 0 ⇒ pure-elastic release. */
  yieldStrength: number;
}

export const DEFAULT_MATERIALS: Material[] = [
  { name: "PLA", e0: 3500, nu: 0.35, density: 1.24, strength: 50, strengthZ: 35, shrink: 0.004, shrinkZ: 0.002, yieldStrength: 45 },
  { name: "PETG", e0: 2100, nu: 0.37, density: 1.27, strength: 45, strengthZ: 34, shrink: 0.004, shrinkZ: 0.002, yieldStrength: 40 },
  { name: "ABS", e0: 2250, nu: 0.37, density: 1.05, strength: 38, strengthZ: 25, shrink: 0.008, shrinkZ: 0.004, yieldStrength: 33 },
  { name: "ASA", e0: 2400, nu: 0.37, density: 1.07, strength: 43, strengthZ: 29, shrink: 0.006, shrinkZ: 0.003, yieldStrength: 38 },
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

/** Result fields selectable in the Deformed view. `u`/`ux`/`uy`/`uz` are
 *  computed client-side from the displacement buffer; the rest come from the
 *  engine. */
export interface ResultFieldDef {
  value: string;
  label: string;
  unit: "mm" | "MPa" | "";
}

export const RESULT_FIELDS: ResultFieldDef[] = [
  { value: "u", label: "Displacement |u|", unit: "mm" },
  { value: "ux", label: "Displacement ux", unit: "mm" },
  { value: "uy", label: "Displacement uy", unit: "mm" },
  { value: "uz", label: "Displacement uz", unit: "mm" },
  { value: "sf", label: "Safety factor — worst case", unit: "" },
  { value: "sfm", label: "Safety factor — material σₜ/σᵥᴹ", unit: "" },
  { value: "sfz", label: "Safety factor — layer adhesion", unit: "" },
  { value: "vm", label: "von Mises σ", unit: "MPa" },
  { value: "svm", label: "Signed von Mises σ", unit: "MPa" },
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
