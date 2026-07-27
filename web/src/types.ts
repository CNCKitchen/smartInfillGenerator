// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

export type BcKind =
  | "fixed"
  | "frictionless"
  | "displacement"
  | "cylindrical"
  | "elastic"
  | "force"
  | "pressure"
  | "bearing"
  | "moment"
  // Inertial loads (DESIGN §16). "accel" is the first SELECTION-LESS BC (no
  // `tris`) — a world acceleration every mass feels; "mass" is a remote point
  // mass bolted to a selected patch.
  | "accel"
  | "mass";

/** Fitted cylinder for a bearing load or a cylindrical support: axis + radius
 *  recovered from the selected surface, plus the cylindricity check. `ok=false`
 *  means the selection isn't cylindrical and the condition can't be applied. */
export interface CylFit {
  ok: boolean;
  axis: [number, number, number];
  point: [number, number, number];
  radius: number;
  residual: number;
  /** True when axis/point/radius come from the STEP file's analytic cylinder
   *  (exact CAD identity) instead of a least-squares fit (DESIGN §18 M3). */
  exact?: boolean;
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
  /** Which LOCAL cylinder directions are fixed (cylindrical support only), in
   *  the order [radial, tangential, axial]; unchecked = free. Default
   *  [true, false, true] — a journal bearing: the bore wall carries the radial
   *  load and holds the part axially, but it can still turn about the axis.
   *  The fitted cylinder that defines the frame is cached in `cyl`. */
  cylDof?: [boolean, boolean, boolean];
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
  /** Fitted cylinder of the selection (bearing load AND cylindrical support —
   *  the support's local radial/tangential/axial frame comes from the same fit);
   *  null until a valid cylindrical surface is selected. */
  cyl?: CylFit | null;
  /** Transient validation message when the selection isn't cylindrical. */
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
  // --- Acceleration (kind "accel", DESIGN §16): a selection-less world
  //     acceleration. Every mass (self-weight + attached masses) feels F = m·a
  //     along this vector. Dual-mode like force; canonical unit mm/s². ---
  /** Resolved acceleration vector in mm/s² (accel only) — what the solver sums. */
  accel?: [number, number, number];
  /** Acceleration definition mode (accel only); defaults to "direction". */
  accelMode?: ForceMode;
  /** Unit direction for "direction" mode (accel only). */
  accelDir?: [number, number, number];
  /** Magnitude in mm/s² for "direction" mode (accel only). */
  accelMag?: number;
  // --- Remote point mass (kind "mass", DESIGN §16): a component of `massGrams`
  //     with its CG at `point`, attached to the selected patch. Its inertial
  //     force + transported couple load the patch under the active accel. ---
  /** Component mass in grams (mass only). Canonical mass unit (g). */
  massGrams?: number;
  /** CG world position in mm (mass only). Transforms WITH the part on
   *  reorientation; initialized at the selected patch's area-weighted centroid. */
  point?: [number, number, number];
  /** Mass coupling (mass only); default "deformable" (load-only). "rigid"
   *  (stiffens the mounting face) is a later milestone — schema-ready now. */
  behavior?: "deformable" | "rigid";
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
  /** Full per-step acceleration vector in mm/s² (accel BCs); absent = inherit
   *  base. Multiple active accel entities sum vectorially in a step. Mass BCs
   *  carry only `active` (a component's mass isn't a per-step quantity). */
  accel?: [number, number, number];
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
  /** Interlayer SHEAR strength in MPa: sliding ALONG the layer plane
   *  (τ = √(σyz²+σzx²), Z-up build direction) — the second axis of the
   *  layer-adhesion failure criterion (DESIGN §15). Unset = no measured
   *  value; the engine derives 0.6·strengthZ. */
  shearStrengthZ?: number;
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
  /** Build-sim: locking temperature in °C — where the material stops relaxing
   *  on cool-down: Tg (amorphous) / near Tc (semi-crystalline). With `cte` set,
   *  the build-sim shrink is DERIVED (CTE × lock→room) and the temperature
   *  ladder is enabled; unset ⇒ the raw `shrink`/`shrinkZ` legacy path. */
  tLock?: number;
  /** Build-sim: effective printed-part CTE in 1/°C (in-plane). Folds
   *  raster/air-gap effects into one coefficient. */
  cte?: number;
  /** Build-sim: through-layer (Z) CTE in 1/°C; unset = isotropic (= cte). */
  cteZ?: number;
}

export const DEFAULT_MATERIALS: Material[] = [
  { name: "PLA", e0: 3500, nu: 0.35, density: 1.24, strength: 50, strengthZ: 35, shrink: 0.004, shrinkZ: 0.002, yieldStrength: 45, tLock: 60, cte: 96e-6 },
  { name: "PETG", e0: 2100, nu: 0.37, density: 1.27, strength: 45, strengthZ: 34, shrink: 0.004, shrinkZ: 0.002, yieldStrength: 40, tLock: 80, cte: 68e-6 },
  { name: "ABS", e0: 2250, nu: 0.37, density: 1.05, strength: 38, strengthZ: 25, shrink: 0.008, shrinkZ: 0.004, yieldStrength: 33, tLock: 100, cte: 88e-6 },
  { name: "ASA", e0: 2400, nu: 0.37, density: 1.07, strength: 43, strengthZ: 29, shrink: 0.006, shrinkZ: 0.003, yieldStrength: 38, tLock: 100, cte: 90e-6 },
];

/** Sparse-infill patterns the solver can model (DESIGN §22).
 *
 *  Only `cubic` survived the M0 calibration gate. Gyroid and grid are not
 *  merely uncalibrated — the kernel's transverse-isotropic material model is
 *  the wrong model CLASS for grid (it is tetragonal: TI mispredicts its
 *  in-plane shear by 28×–86×), and gyroid's ratios swing ±32 % across the
 *  density band against a ±15 % gate. Shipping a knob whose physics we know
 *  is wrong is worse than not shipping it. */
export type PatternKey = "cubic";

/** Patterns removed by §22. A project saved with one of these opens on
 *  `cubic` — see `migratePattern`. Kept as data so the UI can explain the
 *  substitution rather than silently swapping the user's choice. */
export const RETIRED_PATTERNS = ["gyroid", "grid"] as const;

/** Sparse patterns are cubic-only; `rectilinear`/`concentric` remain for the
 *  SOLID regions (`solidPattern`), where the material is dense and the sparse
 *  infill law never applies. */
export function migratePattern(p: unknown): PatternKey {
  return p === "cubic" ? "cubic" : "cubic";
}

/** Infill stiffness law E(ρ) = coeff · E₀ · ρ^exponent (Gibson–Ashby). */
export interface PatternCurve {
  coeff: number;
  exponent: number;
}

/** Measured E(ρ) law (DESIGN §22.5), least-squares over the calibrated
 *  20–70 % band under flow calibration.
 *
 *  Accurate to ±6.4 % across that band — and the residual is a limit of THIS
 *  MODEL, not of the data. Cubic's Ep(ρ) is not a single power law: its
 *  log-log slope climbs monotonically from ~1.05 at 20 % to ~1.80 at 70 %, so
 *  one exponent cannot fit the whole band. A fit restricted to 20–40 %, where
 *  most prints live, is accurate to ±1.7 % (coeff 0.5808, exponent 1.2060) —
 *  see §22.5 for why the band-wide fit ships instead.
 *
 *  Two things this law is NOT valid for. It must not be extrapolated to ρ → 1
 *  (it gives 0.69, not 1.0 — it is a local fit on the band, and solid cells
 *  never use it: they take the solid tensor directly). And below 20 % it is
 *  extrapolation, not measurement. */
export const DEFAULT_CURVES: Record<PatternKey, PatternCurve> = {
  cubic: { coeff: 0.6933, exponent: 1.3401 },
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
  // DESIGN §20 dec. 7 — the criterion's view of the three fields above: the
  // SAME masked smoothing the SF-target goal and the settings optimizer
  // evaluate, with the BC singularity zone greyed out. Plotting one of these
  // shows the field the reported number comes from.
  { value: "sfx", label: "Criterion SF — worst case", unit: "" },
  { value: "sfmx", label: "Criterion SF — material", unit: "" },
  { value: "sfzx", label: "Criterion SF — layer adhesion", unit: "" },
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
