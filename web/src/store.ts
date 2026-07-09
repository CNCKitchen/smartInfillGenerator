// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { create } from "zustand";
import {
  engine,
  type OptimizeOutput,
  type OptRegion,
  type OptSummary,
  type SlicerFlavor,
} from "./engine/EngineClient";
import { EngineSession } from "./engine/EngineSession";
import type { SectionVolume } from "./engine/EngineProtocol";
import { ENVELOPE_STEP, FieldServer, isEnvelope } from "./engine/FieldServer";
import type {
  Bc,
  BcKind,
  ForceMode,
  CheckReport,
  LoadedModel,
  LoadStep,
  LoadStepOverride,
  Material,
  PatternCurve,
  PatternKey,
  ResolutionKey,
  SolveStats,
  VoxelInfo,
} from "./types";
import { DEFAULT_CURVES, DEFAULT_MATERIALS, RESOLUTIONS, RESULT_FIELDS } from "./types";
import { shrinkFromPhysics, ROOM_TEMP_C } from "./materials";
import { fitCylinderFromSelection } from "./cylinderFit";
import { CONTOUR_BANDS, CONTOUR_BANDS_MIN, CONTOUR_BANDS_MAX, jet, bandHexColors } from "./viewer/colormaps";
import {
  type QuantityKind,
  type UnitPrefs,
  PRESETS,
  DEFAULT_PRESET,
  QUANTITY_KINDS,
  QUANTITIES,
  setActiveUnits,
  convertUnitToCanonical,
  format as formatQ,
} from "./units";

/** Color 3MF export bands map 1:1 to slicer filament slots — Bambu/Orca cap. */
export const COLOR_STEPS_MIN = 2;
export const COLOR_STEPS_MAX = 16;

export type Tool = "orbit" | "select" | "brush" | "place" | "pickdir";
export type ViewMode = "setup" | "mesh" | "deformed" | "density" | "infill";

// ---- persisted user settings (materials + infill stiffness curves) ----

const SETTINGS_KEY = "sig.settings.v1";

/** Density-level configuration (⚙ Settings, persisted per browser). */
export interface LevelSettings {
  /** Printability floor in % — graded mode's pinned bottom level. */
  floorPct: number;
  /** Densest allowed graded level in %. */
  capPct: number;
  /** Auto = place levels from the optimized field; manual = fixed list. */
  mode: "auto" | "manual";
  /** Manual levels in % (used when mode === "manual"). */
  manual: number[];
  /** Printability floor for the binary (hollow/solid) mode in %. */
  binaryFloorPct: number;
}

const DEFAULT_LEVELS: LevelSettings = {
  floorPct: 10,
  capPct: 70,
  mode: "auto",
  manual: [10, 40, 70],
  binaryFloorPct: 5,
};

interface PersistedSettings {
  materials: Material[];
  curves: Record<PatternKey, PatternCurve>;
  levels: LevelSettings;
}

function loadSettings(): PersistedSettings {
  const fallback: PersistedSettings = {
    materials: DEFAULT_MATERIALS.map((m) => ({ ...m })),
    curves: {
      gyroid: { ...DEFAULT_CURVES.gyroid },
      cubic: { ...DEFAULT_CURVES.cubic },
      grid: { ...DEFAULT_CURVES.grid },
    },
    levels: { ...DEFAULT_LEVELS, manual: [...DEFAULT_LEVELS.manual] },
  };
  try {
    const raw = localStorage.getItem(SETTINGS_KEY);
    if (!raw) return fallback;
    const p = JSON.parse(raw) as Partial<PersistedSettings>;
    if (Array.isArray(p.materials) && p.materials.length) {
      fallback.materials = p.materials
        .filter((m) => m && typeof m.e0 === "number" && m.e0 > 0)
        .map((m) => {
          // Pre-strength saves: default to the PLA-ish 50 MPa.
          const strength = typeof m.strength === "number" && m.strength > 0 ? m.strength : 50;
          return {
            name: String(m.name),
            e0: m.e0,
            nu: m.nu,
            density: m.density,
            strength,
            // Pre-anisotropy saves: layer adhesion ≈ 70% of σₜ.
            strengthZ:
              typeof m.strengthZ === "number" && m.strengthZ > 0
                ? m.strengthZ
                : Math.round(0.7 * strength),
            // Interlayer shear (DESIGN §15): optional measured value; absent
            // = the engine derives 0.6·strengthZ.
            shearStrengthZ:
              typeof m.shearStrengthZ === "number" && m.shearStrengthZ > 0
                ? m.shearStrengthZ
                : undefined,
            // Pre-build-sim saves: default to a ~0.5% process shrink.
            shrink: typeof m.shrink === "number" && m.shrink > 0 ? m.shrink : 0.005,
            // Pre-anisotropy saves: Z shrink ≈ half the in-plane value.
            shrinkZ:
              typeof m.shrinkZ === "number" && m.shrinkZ > 0
                ? m.shrinkZ
                : 0.5 * (typeof m.shrink === "number" && m.shrink > 0 ? m.shrink : 0.005),
            // Pre-plasticity saves: yield ≈ 90% of σₜ (rough printed value).
            yieldStrength:
              typeof m.yieldStrength === "number" && m.yieldStrength > 0
                ? m.yieldStrength
                : Math.round(0.9 * strength),
            // Optional thermal data (physics-derived build-sim shrink). Old
            // saves simply lack it — the raw-shrink legacy path applies.
            tLock: typeof m.tLock === "number" ? m.tLock : undefined,
            cte: typeof m.cte === "number" && m.cte > 0 ? m.cte : undefined,
            cteZ: typeof m.cteZ === "number" && m.cteZ > 0 ? m.cteZ : undefined,
          };
        });
      if (!fallback.materials.length) fallback.materials = DEFAULT_MATERIALS.map((m) => ({ ...m }));
    }
    for (const k of ["gyroid", "cubic", "grid"] as PatternKey[]) {
      const c = p.curves?.[k];
      if (c && typeof c.coeff === "number" && typeof c.exponent === "number") {
        fallback.curves[k] = { coeff: c.coeff, exponent: c.exponent };
      }
    }
    const l = p.levels;
    if (l && typeof l === "object") {
      if (typeof l.floorPct === "number") fallback.levels.floorPct = clampPct(l.floorPct, 5, 30);
      if (typeof l.capPct === "number") fallback.levels.capPct = clampPct(l.capPct, 40, 100);
      if (l.mode === "manual") fallback.levels.mode = "manual";
      if (Array.isArray(l.manual)) {
        const m = l.manual.filter((v) => typeof v === "number" && v >= 1 && v <= 100);
        if (m.length >= 2) fallback.levels.manual = m;
      }
      if (typeof l.binaryFloorPct === "number") {
        fallback.levels.binaryFloorPct = clampPct(l.binaryFloorPct, 3, 15);
      }
    }
  } catch {
    // corrupted storage: keep defaults
  }
  return fallback;
}

function clampPct(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, Math.round(v)));
}

/** Budget slider band of the active optimization mode/goal. In match mode
 *  the slider is the REFERENCE uniform infill %, which lives in the graded
 *  printable band regardless of fill mode. */
export function budgetBounds(s: {
  optMode: "graded" | "binary" | "solid";
  goal: "budget" | "match";
  levelSettings: LevelSettings;
}): [number, number] {
  // Solid topology: the budget is the RETAINED VOLUME fraction of the design
  // domain, not an infill density — its own band.
  if (s.optMode === "solid") return [5, 80];
  if (s.goal === "match") return [s.levelSettings.floorPct, s.levelSettings.capPct];
  return s.optMode === "binary"
    ? [s.levelSettings.binaryFloorPct, 90]
    : [s.levelSettings.floorPct, s.levelSettings.capPct];
}

// ---- result set (Results-view switcher + staleness) ----

/** The TYPE of a retained result. A result's full identity is (kind, load
 *  step) — see `ResultEntry.id` / `resultStashId`. */
export type ResultKind = "optimized" | "uniform" | "solid" | "asprinted" | "modal";

/** Monotonic input epochs. A result is stale when an epoch it depends on has
 *  advanced past the value it was built at. Grid/geometry changes are NOT
 *  epochs — they DROP the result set (the stashed node grid no longer matches). */
export interface ResultEpochs {
  /** Loads & supports (BCs). */
  loads: number;
  /** Material properties. */
  material: number;
  /** As-printed knobs: print infill, pattern, shells, snap-off skin. */
  print: number;
  /** Optimization knobs: budget, goal, mode, levels, symmetry, skin… */
  opt: number;
}

const ZERO_EPOCHS: ResultEpochs = { loads: 0, material: 0, print: 0, opt: 0 };

/** A provenance-card cell: a plain string, or a CANONICAL value tagged with its
 *  quantity kind so the card formats it in the live display unit (the rows are
 *  built once at solve time, so unit-bearing ones must stay reformattable). */
export type ProvVal =
  | string
  | { v: number; kind: QuantityKind; prefix?: string; suffix?: string };

/** One retained result: metadata for the dropdown + the provenance card.
 *  The displacement/stress data itself lives in the engine stash (keyed by id)
 *  and in the worker's per-result solution. */
export interface ResultEntry {
  /** Engine stash key = the result's full identity. Single load step: the bare
   *  `kind` (byte-identical to the pre-load-step model + old project files).
   *  Multiple steps: `${kind}::${loadStepId}` (see `resultStashId`). */
  id: string;
  kind: ResultKind;
  /** Which load step this result was solved for. */
  loadStepId: string;
  /** Step name at build time — labels the viewer's load-step selector. */
  loadStepName: string;
  /** Dropdown label, e.g. "Optimized · graded 24%". */
  label: string;
  /** Headline outputs for the provenance card. */
  maxDisplacement: number;
  massGrams: number | null;
  /** Min safety factor (as-printed only; null = not computed). */
  minSf: number | null;
  converged: boolean;
  /** Provenance card title + rows (the settings the result was built with). */
  provTitle: string;
  provRows: [string, ProvVal][];
  /** Epoch stamps at build time — diffed against the live epochs for staleness. */
  epochs: ResultEpochs;
}

/** Fixed display order in the dropdown. */
const RESULT_ORDER: ResultKind[] = ["optimized", "uniform", "asprinted", "solid", "modal"];

/** Engine stash key for a (kind, load step). With a single load step we keep
 *  the BARE kind so the stash key, the result roster, and saved `.filasim`
 *  files are byte-identical to the pre-load-step model. Only once a project has
 *  more than one step do results become per-step (`kind::stepId`). */
function resultStashId(kind: ResultKind, stepId: string, singleStep: boolean): string {
  return singleStep ? kind : `${kind}::${stepId}`;
}

// The envelope pseudo-step sentinel (`ENVELOPE_STEP`) and `isEnvelope` live in
// engine/FieldServer.ts with the client-side envelope reduction they gate.

/** Append an "Envelope · worst case" pseudo-step to every kind that has ≥2 real
 *  load-step results (a single step has nothing to envelope). The entry carries
 *  worst-case headline numbers (max |u|, min SF) for the provenance card. */
function withEnvelope(results: ResultEntry[], steps: LoadStep[]): ResultEntry[] {
  const out = results.filter((r) => !isEnvelope(r)); // rebuild envelopes fresh
  const kinds: ResultKind[] = [];
  for (const r of out) if (!kinds.includes(r.kind)) kinds.push(r.kind);
  for (const kind of kinds) {
    // Modal "steps" are independent mode shapes — a worst-case envelope across
    // different modes is meaningless, so modal never gets one.
    if (kind === "modal") continue;
    const group = out.filter((r) => r.kind === kind);
    if (group.length < 2) continue;
    const maxDisplacement = Math.max(...group.map((r) => r.maxDisplacement));
    const sfs = group.map((r) => r.minSf).filter((v): v is number => v != null);
    const minSf = sfs.length ? Math.min(...sfs) : null;
    const label = group[0].label;
    out.push({
      id: `${kind}::${ENVELOPE_STEP}`,
      kind,
      loadStepId: ENVELOPE_STEP,
      loadStepName: "Envelope · worst case",
      label,
      maxDisplacement,
      massGrams: group[0].massGrams,
      minSf,
      converged: group.every((r) => r.converged),
      provTitle: `${group[0].provTitle.split(" · ")[0]} · envelope`,
      provRows: [
        ["Load steps", `${group.length} combined`],
        ["Worst max |u|", fmtMm(maxDisplacement)],
        ...(minSf != null ? ([["Min safety factor", `${minSf.toFixed(2)}×`]] as [string, ProvVal][]) : []),
        ["Reduction", "max field · min SF, per point"],
      ],
      epochs: { ...group[0].epochs },
    });
  }
  return out;
}

/** Order results by kind, then by their load step's position (so the per-step
 *  entries of one kind stay in step order in the selector). The envelope's
 *  sentinel step id isn't in `steps`, so it sorts AFTER the real steps. */
function sortResults(results: ResultEntry[], steps: LoadStep[]): ResultEntry[] {
  const stepIdx = (id: string) => {
    const i = steps.findIndex((s) => s.id === id);
    return i < 0 ? steps.length : i;
  };
  return [...results].sort(
    (a, b) =>
      RESULT_ORDER.indexOf(a.kind) - RESULT_ORDER.indexOf(b.kind) ||
      stepIdx(a.loadStepId) - stepIdx(b.loadStepId)
  );
}

/** A result is stale when a dependency epoch has advanced. Displacement stays
 *  correct (the stash is self-contained); stress may be inconsistent — which is
 *  exactly why it's badged "re-run". */
export function resultStale(e: ResultEntry, ep: ResultEpochs): boolean {
  if (ep.loads !== e.epochs.loads || ep.material !== e.epochs.material) return true;
  if (e.kind === "asprinted") return ep.print !== e.epochs.print;
  if (e.kind === "solid") return false; // depends on grid+loads+material only
  // Modal modes depend on grid+loads+material (checked above) and, for the
  // as-printed model, the print settings — never on the optimized design.
  if (e.kind === "modal") return ep.print !== e.epochs.print;
  return ep.opt !== e.epochs.opt; // optimized + uniform
}

/** Cheap fingerprint of the loads/supports — changes whenever a BC is added,
 *  removed, re-parameterized, or re-painted. */
function bcFingerprint(bcs: Bc[]): string {
  return bcs
    .map((b) => {
      let triSum = 0;
      for (let i = 0; i < b.tris.length; i++) triSum = (triSum + b.tris[i]) >>> 0;
      return [b.kind, b.tris.length, triSum, b.axes?.join(""), b.force?.join(","), b.pressure, b.stiffness].join(",");
    })
    .join("|");
}

function saveSettings(
  materials: Material[],
  curves: Record<PatternKey, PatternCurve>,
  levels: LevelSettings
) {
  try {
    localStorage.setItem(SETTINGS_KEY, JSON.stringify({ materials, curves, levels }));
  } catch {
    // storage full/blocked: settings just won't persist
  }
}

const initialSettings = loadSettings();

// ---- display-unit preference (per-user UI pref, NOT in the project file) ----
// See docs/units-design.md §5: lives in localStorage, identical across projects;
// later moves to the user account when accounts land.

const UNIT_KEY = "sig.units.v1";

/** Load the saved unit selection, dropping any stale unit ids (renamed/removed
 *  in a later registry) back to the default preset's choice. */
function loadUnitPrefs(): UnitPrefs {
  const base: UnitPrefs = { ...PRESETS[DEFAULT_PRESET].units };
  try {
    const raw = localStorage.getItem(UNIT_KEY);
    if (!raw) return base;
    const p = JSON.parse(raw) as Partial<UnitPrefs>;
    for (const k of QUANTITY_KINDS) {
      const id = p[k];
      if (id && QUANTITIES[k].units.some((u) => u.id === id)) base[k] = id;
    }
  } catch {
    // corrupted storage: keep the default preset
  }
  return base;
}

function saveUnitPrefs(prefs: UnitPrefs) {
  try {
    localStorage.setItem(UNIT_KEY, JSON.stringify(prefs));
  } catch {
    // storage full/blocked: selection just won't persist
  }
}

const initialUnitPrefs = loadUnitPrefs();
// Prime the format chokepoint's module mirror before the first render.
setActiveUnits(initialUnitPrefs);

// ---- STL import unit (one-time bake to canonical mm; see units-design §8) ----
// STL is unitless — a 1×1×1 file is either 1 mm or 1 inch and the file can't
// say which. The IMPORT unit (a one-time, irreversible bake) is decoupled from
// the DISPLAY unit (reversible). Persisted separately from the display prefs.

const IMPORT_KEY = "sig.import.v1";

interface ImportPrefs {
  /** Last chosen import unit id (a length-unit id). */
  unit: string;
  /** Show the picker on each STL import (false = "don't ask again"). */
  ask: boolean;
}

function loadImportPrefs(): ImportPrefs {
  const base: ImportPrefs = { unit: "mm", ask: true };
  try {
    const raw = localStorage.getItem(IMPORT_KEY);
    if (!raw) return base;
    const p = JSON.parse(raw) as Partial<ImportPrefs>;
    if (p.unit && QUANTITIES.length.units.some((u) => u.id === p.unit)) base.unit = p.unit;
    if (typeof p.ask === "boolean") base.ask = p.ask;
  } catch {
    // corrupted: keep defaults
  }
  return base;
}

function saveImportPrefs(unit: string, ask: boolean) {
  try {
    localStorage.setItem(IMPORT_KEY, JSON.stringify({ unit, ask }));
  } catch {
    // storage blocked: just won't persist
  }
}

const initialImportPrefs = loadImportPrefs();

/** Commit a new unit selection: update the format chokepoint's module mirror,
 *  persist it, and bump `unitRev` so pure-format components re-render even though
 *  no canonical data changed. */
function applyUnitPrefs(set: SetState, get: () => AppState, prefs: UnitPrefs) {
  setActiveUnits(prefs);
  saveUnitPrefs(prefs);
  set({ unitPrefs: prefs, unitRev: get().unitRev + 1 });
  // The 3D overlays are imperative (not React) — refresh them so they re-render
  // in the new unit immediately, not only on the next camera move: the min/max
  // marker labels and the pinned value callouts.
  if (get().showExtremes) sceneEvents.onShowExtremes?.(true, fieldUnit(get().resultField));
  sceneEvents.onUnitsChanged?.();
}

interface AppState {
  // workflow navigation (step rail): 1 Model … 6 View & export
  activeStep: number;
  // model
  fileName: string | null;
  model: LoadedModel | null;
  segAngle: number;
  /** Where surface patches come from: dihedral crease angle, or (STEP only)
   *  exact BREP faces. Defaults to "cad" when the import provides faces. */
  segSource: "angle" | "cad";
  // interaction
  tool: Tool;
  brushRadius: number;
  brushErase: boolean;
  // bcs
  bcs: Bc[];
  activeBcId: string | null;
  /** FEA load steps (load cases) — see DESIGN §13. A fresh model has exactly
   *  one step with empty overrides, so the single-case setup is unchanged; a
   *  table appears in the UI only once a 2nd step is added. The shared `bcs`
   *  hold geometry/baseline; each step overrides per-BC active/force/pressure. */
  loadSteps: LoadStep[];
  /** Which load step is being edited / shown. Always a valid `loadSteps` id. */
  activeLoadStepId: string;
  // physics
  material: Material;
  materials: Material[];
  curves: Record<PatternKey, PatternCurve>;
  resolution: ResolutionKey | "custom";
  /** Cell size h in mm when resolution = "custom" (0 = not yet chosen —
   *  initialized from the current grid on first selection). */
  customH: number;
  // print properties (step 3 · Properties) — shared by the as-printed
  // verify solve, the optimizer's skin model, and the 3MF export
  pattern: PatternKey;
  perimeters: number;
  lineWidth: number; // mm
  /** Solid top/bottom shells: count × layerHeight = shell thickness.
   *  0 = none (open-top showpieces — the infill shows). */
  topBottomLayers: number;
  layerHeight: number; // mm
  /** Uniform interior infill % "as printed" — the slicer setting. */
  printInfill: number;
  /** Snap the voxel size to wall/k so the skin is k exact cell layers. */
  snapVoxel: boolean;
  /** Composite skin: blend part-wall surface cells (skin fraction) instead
   *  of rounding the skin to whole voxel layers — thin walls stay
   *  representable on coarse grids. Off = legacy whole-layer skin. */
  compositeSkin: boolean;
  /** What "Solve once" analyzes: the print, the CAD-ideal solid, or the FDM
   *  build simulation (inherent-strain warp + bed peel). */
  analyzeMode: "printed" | "solid" | "buildsim";
  /** Verify-tab analysis type: a static linear solve, or a modal (natural-
   *  frequency) analysis. Orthogonal to `analyzeMode` (printed vs solid stiffness
   *  applies to both). */
  analysisType: "static" | "modal";
  /** Number of modes to compute in a modal run (1–20). */
  modalModeCount: number;
  /** Free-free modal: run an UNCONSTRAINED part (soft-anchored), discarding the
   *  6 rigid-body modes. For parts with no supports. */
  freeFree: boolean;
  /** Active top-level workspace: structural Simulate & Optimize, or Build Sim. */
  appMode: "optimize" | "buildsim";
  /** Build-sim: which state to deform by (off-bed sprung shape or on the bed). */
  buildState: "released" | "bonded";
  /** Build-sim bed temperature (°C) — sent as tBed when the active material
   *  carries thermal data (tLock + cte). Process setting, not material. */
  buildBedTemp: number;
  /** Build-sim chamber temperature (°C) — sent as tChamber when the active
   *  material carries thermal data. */
  buildChamberTemp: number;
  /** Build-sim live progress (activated layers); null when not running. */
  buildProgress: { done: number; total: number } | null;
  /** Both states of the last completed build sim, kept so the Show-state toggle
   *  can flip on bed ⇄ released with no re-solve. null until a build finishes
   *  (and after a workspace switch / new geometry). peakLift/peakShear are the
   *  bed-peel traction maxima (MPa, mesh-independent, uncalibrated). */
  buildResult: {
    bondedMax: number;
    releasedMax: number;
    densityAware: boolean;
    peakLift: number;
    peakShear: number;
  } | null;
  /** Extras of the last as-printed solve (results dock); null = solid run. */
  printedStats: PrintedSummary | null;
  /** Mesh view: color each cell by its element density (0–1: skin = 1,
   *  interior = infill ratio / optimized density, composite cells blended). */
  meshDensity: boolean;
  /** Mesh view (Build Sim): inherent-strain layer view — color cells by the
   *  per-element strain SOURCE and scrub the build height with `strainLayer`. */
  strainView: boolean;
  /** Current build layer shown in the inherent-strain view (1..strainLayerMax). */
  strainLayer: number;
  /** Total voxel layers (Z) of the analysis grid — the strain scrubber range. */
  strainLayerMax: number;
  /** Peak inherent-strain source in the current view (MPa, for the label). */
  strainPeakMPa: number;
  /** Smoothed stress display: fields nodal-averaged and evaluated on the
   *  true surface instead of flat per-cell (post-processing only). */
  smoothStress: boolean;
  /** Material (occupancy-decoupled) stress display: report the true material
   *  stress at finite-cell cut cells (eps ÷ occupancy) instead of the
   *  occupancy-scaled value — removes the curved-skin staircase stripes.
   *  Display-side only; the safety factor is unchanged. */
  materialStress: boolean;
  /** Include the interlayer SHEAR term in the layer-adhesion criterion
   *  (DESIGN §15 dec. 1). Off = pure tension across the layers. Affects the
   *  sfz/sf fields, the orientation sweep and its preview — display-side
   *  derived, the solution stays valid. */
  layerShear: boolean;
  /** Discrete ("contour banded") result display: the color scale is quantized
   *  into fixed steps with hard edges (classic FEA contour bands) instead of a
   *  smooth gradient. Toggled by clicking the result legend bar. */
  bandedContour: boolean;
  /** Number of discrete steps when `bandedContour` is on — adjusted by
   *  scrolling the legend bar (clamped to CONTOUR_BANDS_MIN..MAX). */
  bandCount: number;
  // optimization inputs
  budget: number; // infill budget: target mean interior density in %
  smoothIters: number; // Taubin passes on modifier regions
  nBins: number;
  /** Minimum member size in mm (printability length scale); null = auto
   *  (2× line width). Drives the optimizer's density-filter radius so thin,
   *  unprintable members are smoothed away — mesh-independent. */
  minMemberMm: number | null;
  /** Optimization goal: stiffest at a mass budget, or lightest at a
   *  target stiffness ("as stiff as uniform X%"). */
  goal: "budget" | "match";
  /** Optimization mode: graded infill densities, binary (hollow/solid core),
   *  or solid topology (material removal — a new optimized shape). */
  optMode: "graded" | "binary" | "solid";
  /** Part Topo: keep the cells under loads/supports solid (default on). Off =
   *  pure topology optimization that may carve those regions too. */
  retainBc: boolean;
  /** Solid-mode self-supporting overhang filter (prints without supports). */
  selfSupport: boolean;
  /** Overhang angle from horizontal (degrees) for the self-supporting filter. */
  overhangDeg: number;
  /** Planar symmetry constraint for the optimizer. Plane n·p = c (n unit,
   *  world mm) — the gizmo's rings can tilt it off-axis. */
  symOn: boolean;
  symNormal: [number, number, number];
  symC: number;
  /** Solid-fill pattern written to the 3MF in binary mode. */
  solidPattern: "rectilinear" | "concentric";
  /** Density-level configuration (persisted with materials/curves). */
  levelSettings: LevelSettings;
  // run state
  busy: string | null;
  error: string | null;
  notice: string | null;
  check: CheckReport | null;
  stats: SolveStats | null;
  hasResult: boolean;
  optProgress: { iteration: number; maxIter: number; pass?: number; passes?: number } | null;
  optSummary: OptSummary | null;
  /** Validate Orientation (DESIGN §15): the finished sweep — two n×n grids of
   *  min layer-adhesion SF (scored = constraint ring excluded, all = nothing
   *  hidden), the stash ids it folded, and the orientation-independent
   *  material-SF floor for the readout. */
  orientSweep: {
    n: number;
    stepDeg: number;
    scored: Float32Array;
    all: Float32Array;
    ids: string[];
    materialSfMin: number;
  } | null;
  orientProgress: { done: number; total: number } | null;
  /** Selected heatmap pixel; non-null = display-only preview is active. */
  orientSel: { ip: number; ir: number } | null;
  /** Retained, selectable results for the Results view's switcher. */
  results: ResultEntry[];
  /** Which retained result the Results (deformed) view is showing — a
   *  `ResultEntry.id` (bare kind when single-step, else `kind::stepId`). */
  activeResultId: string | null;
  /** Live input epochs — diffed against each result's stamps for staleness. */
  resultEpochs: ResultEpochs;
  viewMode: ViewMode;
  /** Overlay the model's triangle mesh as a wireframe (Setup/Mesh views) so
   *  the input mesh quality can be inspected. */
  wireframe: boolean;
  deformScale: number;
  animateDeformed: boolean;
  /** Display autoscale chosen by the viewer (deformation exaggeration base). */
  autoScale: number;
  voxelInfo: VoxelInfo | null;
  voxelMeshReady: boolean;
  settingsOpen: boolean;
  /** Load-steps manager modal (step on/off matrix + naming) — DESIGN §13. */
  loadStepsOpen: boolean;
  /** Imprint & privacy modal (German Impressumspflicht). */
  imprintOpen: boolean;
  /** Support/donation popup — shown when a run starts (unless suppressed). */
  supportOpen: boolean;
  /** Startup disclaimer (legal): shown every load unless skipped below. */
  disclaimerOpen: boolean;
  /** Dev/testing escape hatch (persisted in this browser). */
  disclaimerSkipped: boolean;
  // ---- display units (presentation only; see docs/units-design.md) ----
  /** Per-quantity display-unit selection. The engine/store stay canonical;
   *  this only drives rendering + input conversion. */
  unitPrefs: UnitPrefs;
  /** Unit settings popover (clicked from the status-strip units chip). */
  unitsOpen: boolean;
  /** Bumped on every unit change so pure-format components re-render even when
   *  their underlying (canonical) data is unchanged. */
  unitRev: number;
  // ---- STL import unit (decoupled from display units; see units-design §8) ----
  /** Last chosen STL import unit (length-unit id); also the default for silent
   *  ("don't ask again") imports. */
  importUnit: string;
  /** Whether to show the import-unit picker on each STL open. */
  askImportUnit: boolean;
  /** STL awaiting a unit choice (picker open); null otherwise. */
  pendingImport: { name: string; bytes: ArrayBuffer } | null;
  /** Densities of the extracted modifier regions (for the region list). */
  regionInfos: { density: number }[];
  regionVisible: boolean[];
  /** Isosurface density in % (0 = off for graded). For Part Topo / binary this
   *  is the export level too — the level the exported geometry is cut from the
   *  CONTINUOUS optimized field (separate from the budget). For graded it's a
   *  display-only cutaway. */
  densityThreshold: number;
  /** Target slicer for the project 3MF export. */
  exportSlicer: SlicerFlavor;
  /** Result field shown in the Deformed view ("u" or a stress/strain kind). */
  resultField: string;
  /** Surface the results are mapped on: smooth STL or the analysis voxels. */
  resultSurface: "stl" | "voxel";
  /** Min/max of the active stress/strain field, for the legend. */
  fieldRange: { min: number; max: number } | null;
  /** User override of the color scale (null = auto). */
  legendMin: number | null;
  legendMax: number | null;
  /** Mark the locations of the min/max values in the plot. */
  showExtremes: boolean;
  // Section plane.
  sectionOn: boolean;
  // "Log for nerds": solver/optimizer telemetry + convergence series.
  logOpen: boolean;
  logLines: LogLine[];
  /** One sample per optimizer iteration of the LAST/running optimization. */
  optSeries: OptIterSample[];
  /** MGCG residual history of the last plain solve (log-scale plot). */
  solveResiduals: number[];
  /** Relative-residual convergence target of the last solve — the plot's
   *  limit line. 0 until the first solve reports it. */
  solveTol: number;

  setActiveStep(n: number): void;
  /** Open a model. For an ambiguous STL it shows the import-unit picker (unless
   *  "don't ask again"); `unitId` set = use it directly (the picker's confirm). */
  loadFile(name: string, bytes: ArrayBuffer, unitId?: string): Promise<void>;
  /** Picker confirmed: bake the pending STL at `unitId`; `remember` = stop
   *  asking on future imports. */
  confirmImport(unitId: string, remember: boolean): void;
  /** Dismiss the import-unit picker without loading. */
  cancelImport(): void;
  /** Re-enable the import-unit picker ("don't ask again" undo). */
  setAskImportUnit(on: boolean): void;
  /** Rescale the loaded geometry in place (the "wrong import unit" escape
   *  hatch, e.g. ×25.4 / ÷25.4) and re-seat it on the plate. */
  rescaleModel(factor: number): Promise<void>;
  setSegAngle(angle: number): Promise<void>;
  /** Switch the surface-patch source (crease angle vs CAD faces). */
  setSegSource(src: "angle" | "cad"): Promise<void>;
  setTool(tool: Tool): void;
  /** Print-orientation tools (Model step): rotate 90° about a world axis,
   *  or place the clicked face on the build plate (face normal → −Z). */
  rotateModel(axis: "x" | "y" | "z"): Promise<void>;
  applyPlaceOnFace(normal: [number, number, number]): Promise<void>;
  /** Symmetry plane (Optimize step). */
  toggleSymmetry(): void;
  setSymAxis(axis: "x" | "y" | "z"): void;
  centerSymmetry(): void;
  /** Scene → store: the symmetry gizmo was dragged/rotated. */
  onSymmetryPlaneMoved(normal: [number, number, number], c: number): void;
  setBrushRadius(r: number): void;
  setBrushErase(on: boolean): void;
  addBc(kind: BcKind): void;
  removeBc(id: string): void;
  setActiveBc(id: string | null): void;
  /** Rename a condition (display only — doesn't stale results). */
  setBcName(id: string, name: string): void;
  updateBcTris(id: string, tris: Uint32Array): void;
  updateBcParams(
    id: string,
    params: Partial<
      Pick<
        Bc,
        | "force"
        | "pressure"
        | "stiffness"
        | "axes"
        | "disp"
        | "forceMode"
        | "forceDir"
        | "forceMag"
        | "forceDirAuto"
        | "moment"
        | "momentMode"
        | "momentDir"
        | "momentMag"
        | "cyl"
        | "cylError"
      >
    >
  ): void;
  /** Toggle a single global axis of a displacement support. */
  toggleBcAxis(id: string, axis: 0 | 1 | 2): void;
  /** Switch a force load between component and direction definition. */
  setForceMode(id: string, mode: ForceMode): void;
  /** Set the magnitude (N) of a direction-mode force. */
  setForceMag(id: string, mag: number): void;
  /** Set the (un-normalized) direction of a force; clears auto-tracking. */
  setForceDir(id: string, dir: [number, number, number]): void;
  /** Reverse a force's direction (clears auto-tracking). */
  flipForceDir(id: string): void;
  /** Snap a force's direction back to the selection's average normal. */
  resetForceDirToNormal(id: string): void;
  /** Set the (un-normalized) axis of a moment; keeps |M|, switches to direction. */
  setMomentDir(id: string, dir: [number, number, number]): void;
  /** Reverse a moment's axis (right-hand-rule sense). */
  flipMomentDir(id: string): void;
  /** Snap a moment's axis to the selection's average surface normal. */
  resetMomentDirToNormal(id: string): void;
  /** Scene → store: the pick-direction tool clicked a triangle (its normal). */
  applyPickedDir(normal: [number, number, number]): void;
  // load steps (FEA load cases) — see DESIGN §13. The data layer; the table UI
  // and the multi-step solve/optimize wiring land in later milestones.
  /** Append a new load step (all BCs active at base) and make it active. */
  addLoadStep(): void;
  /** Remove a load step; no-op when only one remains. */
  removeLoadStep(id: string): void;
  /** Rename a load step. */
  renameLoadStep(id: string, name: string): void;
  /** Select the load step being edited / shown. */
  setActiveLoadStep(id: string): void;
  /** Activate/deactivate a BC within a step (constraints + load inclusion). */
  setStepBcActive(stepId: string, bcId: string, active: boolean): void;
  /** Set a step's per-BC force vector (force BCs). */
  setStepForce(stepId: string, bcId: string, force: [number, number, number]): void;
  /** Aim a step's force along the selection's average normal (magnitude kept). */
  aimStepForceAlongNormal(stepId: string, bcId: string): void;
  /** Set a step's per-BC pressure (pressure BCs). */
  setStepPressure(stepId: string, bcId: string, pressure: number): void;
  /** Set a step's per-BC moment vector (moment BCs). */
  setStepMoment(stepId: string, bcId: string, moment: [number, number, number]): void;
  /** Aim a step's moment axis along the selection's average normal (|M| kept). */
  aimStepMomentAlongNormal(stepId: string, bcId: string): void;
  /** Include/exclude a step from the multi-load optimizer. */
  setStepIncludeOptimize(stepId: string, include: boolean): void;
  /** Set a step's weight in the weighted-sum optimizer objective. */
  setStepWeight(stepId: string, weight: number): void;
  setMaterial(m: Material): void;
  updateMaterial(index: number, m: Material): void;
  addMaterial(): void;
  removeMaterial(index: number): void;
  resetMaterials(): void;
  setCurve(pattern: PatternKey, c: PatternCurve): void;
  resetCurves(): void;
  openSettings(open: boolean): void;
  openLoadSteps(open: boolean): void;
  openImprint(open: boolean): void;
  /** Show the support popup unless the user suppressed it (7-day window). */
  maybeShowSupport(): void;
  /** Close the support popup; when `dontShowAgain`, suppress it for 7 days. */
  closeSupport(dontShowAgain: boolean): void;
  setResolution(r: ResolutionKey | "custom"): void;
  setCustomH(v: number): void;
  setBudget(v: number): void;
  setPattern(p: PatternKey): void;
  setPerimeters(v: number): void;
  setLineWidth(v: number): void;
  setTopBottomLayers(v: number): void;
  setLayerHeight(v: number): void;
  setPrintInfill(v: number): void;
  setSnapVoxel(on: boolean): void;
  setCompositeSkin(on: boolean): void;
  setAnalyzeMode(m: "printed" | "solid" | "buildsim"): void;
  setAnalysisType(t: "static" | "modal"): void;
  setModalModeCount(n: number): void;
  setFreeFree(on: boolean): void;
  setAppMode(m: "optimize" | "buildsim"): void;
  setBuildState(s: "released" | "bonded"): void | Promise<void>;
  setBuildBedTemp(v: number): void;
  setBuildChamberTemp(v: number): void;
  setMeshDensity(on: boolean): void;
  setStrainView(on: boolean): Promise<void>;
  setStrainLayer(layer: number): Promise<void>;
  setSmoothStress(on: boolean): void;
  setLayerShear(on: boolean): void;
  setMaterialStress(on: boolean): void;
  /** Flip between smooth and discrete (banded) result contours. */
  toggleBandedContour(): void;
  /** Set the discrete band count (turns banding on); clamped to MIN..MAX. */
  setBandCount(n: number): void;
  /** Scene → store: the section plane moved (three.js plane convention). */
  onSectionPlaneMoved(normal: [number, number, number], constant: number): void;
  setSmoothIters(v: number): void;
  setNBins(v: number): void;
  /** Minimum member size in mm; null restores auto (2× line width). */
  setMinMemberMm(v: number | null): void;
  setGoal(g: "budget" | "match"): void;
  setOptMode(m: "graded" | "binary" | "solid"): void;
  setRetainBc(on: boolean): void;
  setSelfSupport(on: boolean): void;
  setOverhangDeg(deg: number): void;
  setSolidPattern(p: "rectilinear" | "concentric"): void;
  updateLevelSettings(p: Partial<LevelSettings>): void;
  setRegionVisible(index: number, on: boolean): void;
  /** Isosurface density: display cutaway in all modes, AND (Part Topo / binary)
   *  the export level — re-extracts the geometry and what later exports use. */
  setDensityThreshold(v: number): void;
  setExportSlicer(s: SlicerFlavor): void;
  setResultSurface(surface: "stl" | "voxel"): Promise<void>;
  consentDisclaimer(): void;
  setDisclaimerSkipped(on: boolean): void;
  /** Open/close the unit-settings popover. */
  openUnits(open: boolean): void;
  /** Apply a whole preset (Metric / SI-mm / US-in / …). */
  setUnitPreset(presetId: string): void;
  /** Override a single quantity's display unit (per-quantity selection). */
  setUnit(kind: QuantityKind, unitId: string): void;
  setResultField(kind: string): Promise<void>;
  setLegendRange(min: number | null, max: number | null): void;
  setShowExtremes(on: boolean): void;
  toggleSection(): void;
  flipSection(): void;
  setSectionAxis(axis: "x" | "y" | "z"): void;
  setLogOpen(open: boolean): void;
  clearLog(): void;
  /** Append a line to the nerd log (e.g. a placed value callout from the viewer). */
  logNote(msg: string): void;
  /** Switch the Results view to a retained result (instant — engine-stashed). */
  selectResult(id: string): Promise<void>;
  /** Pin the legend's color range to the CURRENT step's data (the "fit"
   *  button) so stepping through load cases stays comparable. */
  fitLegend(): void;
  runCheck(): Promise<void>;
  runSolve(): Promise<void>;
  /** Validate Orientation (DESIGN §15): sweep the ±90° pitch/roll hemisphere
   *  of the active result kind (all load steps folded worst-case) for the
   *  min layer-adhesion SF per orientation. */
  runOrientationSweep(): Promise<void>;
  /** Select a heatmap pixel: rotate the displayed part to that build
   *  direction and recolor with its per-vertex layer SF (display-only). */
  selectOrientation(ip: number, ir: number): Promise<void>;
  /** Drop the orientation preview: restore pose + the active result field. */
  clearOrientationPreview(): void;
  /** Constrained modal analysis (Verify tab): compute `modalModeCount` natural
   *  frequencies + mode shapes on the FIRST load case's supports, surfacing each
   *  mode as a switchable result-case. */
  runModal(): Promise<void>;
  runOptimize(): Promise<void>;
  downloadThreeMf(): Promise<void>;
  downloadStls(): Promise<void>;
  /** Solid topology mode: download the optimized shape as one STL. */
  downloadShape(): Promise<void>;
  /** Number of discrete color steps for the color-3MF export (= filament
   *  slots), clamped to COLOR_STEPS_MIN..MAX. */
  colorSteps: number;
  setColorSteps(n: number): void;
  /** Export the active result field as a standalone color 3MF: `colorSteps`
   *  discrete Bambu/Orca filament bands across the active contour min/max, on
   *  the original undeformed mesh. Only meaningful in the results view. */
  downloadColorThreeMf(): Promise<void>;
  /** Save the whole project as a `.filasim` file. `includeResults` embeds the
   *  FEA displacement buffers (instant reopen) — off keeps the file small and
   *  restores the optimized design only. */
  saveProject(includeResults: boolean): Promise<void>;
  /** Open a `.filasim` project: restore model, settings, design, and results. */
  openProject(file: File): Promise<void>;
  setViewMode(mode: ViewMode): Promise<void>;
  setWireframe(on: boolean): void;
  setDeformScale(s: number): void;
  setAnimateDeformed(on: boolean): void;
  /** Stop the running solve/optimization at its next solver checkpoint. */
  cancelRun(): void;
  clearError(): void;
}

export interface LogLine {
  t: string;
  msg: string;
}

/** Dock data of the last as-printed verify solve. */
export interface PrintedSummary {
  massGrams: number;
  massSolidGrams: number;
  /** Cell layers modeling the skin — fractional with composite skin on. */
  skinLayers: number;
  /** The solve used the composite (blended) skin model. */
  compositeSkin: boolean;
  /** Print settings the solve used (the dock labels them honestly). */
  infillPct: number;
  pattern: PatternKey;
  perimeters: number;
  lineWidth: number;
  /** Minimum safety factor over the part; null if the field fetch failed. */
  minSf: number | null;
  /** Which strength limit produced the minimum: in-layer material (σᵥᴹ)
   *  or layer adhesion (σzz tension). */
  sfGoverns: "material" | "layer" | null;
}

/** One optimizer iteration for the nerd-log convergence charts. */
export interface OptIterSample {
  it: number;
  compliance: number;
  massFrac: number;
  meanInfill: number;
  change: number;
  meanChange: number;
  innerIters: number;
  innerRes: number;
}

let bcCounter = 0;
let stepCounter = 0;

/** Short kind labels for auto-generated BC names ("Force 1", "Fixed 2", …). */
const BC_KIND_NAME: Record<BcKind, string> = {
  fixed: "Fixed",
  frictionless: "Frictionless",
  displacement: "Displacement",
  elastic: "Elastic",
  force: "Force",
  pressure: "Pressure",
  bearing: "Bearing",
  moment: "Moment",
};

/** Fresh load step with no overrides (every BC active at its base value). */
function makeLoadStep(name: string): LoadStep {
  return { id: `ls${++stepCounter}`, name, overrides: {}, includeInOptimize: true, weight: 1 };
}

/** Deep-copy a step's per-BC overrides (so a cloned step diverges freely). */
function cloneOverrides(ov: Record<string, LoadStepOverride>): Record<string, LoadStepOverride> {
  const out: Record<string, LoadStepOverride> = {};
  for (const [k, v] of Object.entries(ov)) {
    out[k] = {
      ...v,
      force: v.force ? ([...v.force] as [number, number, number]) : undefined,
      moment: v.moment ? ([...v.moment] as [number, number, number]) : undefined,
    };
  }
  return out;
}

/** Immutably patch (or, with `patch === null`, drop) one BC's override in one
 *  step. Used by the per-step edit actions. */
function patchStepOverride(
  steps: LoadStep[],
  stepId: string,
  bcId: string,
  patch: Partial<LoadStepOverride> | null
): LoadStep[] {
  return steps.map((s) => {
    if (s.id !== stepId) return s;
    const overrides = { ...s.overrides };
    if (patch === null) delete overrides[bcId];
    else overrides[bcId] = { ...overrides[bcId], ...patch };
    return { ...s, overrides };
  });
}

let isoTimer: ReturnType<typeof setTimeout> | null = null;
let smoothTimer: ReturnType<typeof setTimeout> | null = null;
let meshCutTimer: ReturnType<typeof setTimeout> | null = null;
/** Last section plane reported by the scene (three.js convention:
 *  kept side is normal·p + constant ≥ 0). */
let lastSectionPlane: { normal: [number, number, number]; constant: number } | null = null;
/** Dev/testing escape hatch for the startup disclaimer (this browser only). */
const SKIP_DISCLAIMER_KEY = "sig-skip-disclaimer";

/** Support popup (CTA shown when a run starts) — suppressed for 7 days when the
 *  user ticks "Don't show this again". We store an EXPIRY timestamp: once it
 *  passes, the decision is invalidated and the popup returns. */
const SUPPORT_SUPPRESS_KEY = "filasim-support-suppress-until";
const SUPPORT_SUPPRESS_MS = 7 * 24 * 60 * 60 * 1000;

function supportSuppressed(): boolean {
  try {
    const until = Number(localStorage.getItem(SUPPORT_SUPPRESS_KEY));
    return Number.isFinite(until) && until > 0 && Date.now() < until;
  } catch {
    return false;
  }
}

function disclaimerSkippedInit(): boolean {
  try {
    return localStorage.getItem(SKIP_DISCLAIMER_KEY) === "1";
  } catch {
    return false;
  }
}

/** Owns the per-solution engine-session state (field caches, voxel-result
 *  geometry, residual poll). The `onVoxelResult` sink forwards to the scene at
 *  call time (sceneEvents is populated by the viewer on mount). */
const session = new EngineSession((p, d, e, ed) =>
  sceneEvents.onVoxelResult?.(p, d, e, ed)
);

/** Owns the result-field display pipeline: the four fetch/compute paths
 *  (envelope reduction, peel maps, displacement components, engine scalar
 *  fields), their staleness discipline, and the reduced-envelope cache.
 *  The store adapts it via `pushScalarField` below. */
const fieldServer = new FieldServer(session);

/** Push the active result field, sized for the active result surface
 *  ("u" = displacement coloring straight from the displacement arrays).
 *  Thin adapter over the FieldServer: the store stays the only zustand
 *  writer (`fieldRange`) and the only scene-event caller; the server picks
 *  the fetch path and drops results the user has navigated away from. */
function pushScalarField(set: SetState, get: () => AppState): Promise<void> {
  return fieldServer.pushActiveField(
    () => {
      const s = get();
      return {
        activeResultId: s.activeResultId,
        resultField: s.resultField,
        resultSurface: s.resultSurface,
        results: s.results,
        sectionOn: s.sectionOn,
      };
    },
    {
      setFieldRange: (range) => set({ fieldRange: range }),
      scalarField: (values, flip, signed, range) =>
        sceneEvents.onScalarField?.(values, flip, signed, range),
      peelMap: (positions, values, max) => sceneEvents.onPeelMap?.(positions, values, max),
      dispComponent: (comp) => sceneEvents.onDispComponent?.(comp),
      sectionVolume: (data) => sceneEvents.onSectionVolume?.(data),
      log: (msg) => appendLog(set, msg),
    }
  );
}

/** Min safety factor of the CURRENT (live) printed solution from BOTH limits
 *  (material σᵥᴹ and layer-adhesion σzz), and which governs. Pure read. When
 *  `cache` is set the two fields are kept in the shared field cache (instant to
 *  view afterwards) — the multi-step loop passes false so a non-displayed
 *  step's fields never shadow the displayed one. Null if a fetch fails. */
async function computeMinSf(
  cache: boolean
): Promise<{ minSf: number; governs: "layer" | "material" } | null> {
  try {
    const [sfm, sfz] = await Promise.all([
      engine.resultField("sfm"),
      engine.resultField("sfz"),
    ]);
    if (cache) {
      session.setField("sfm", false, sfm);
      session.setField("sfz", false, sfz);
    }
    let minM = Infinity;
    let minZ = Infinity;
    for (let i = 0; i < sfm.length; i++) minM = Math.min(minM, sfm[i]);
    for (let i = 0; i < sfz.length; i++) minZ = Math.min(minZ, sfz[i]);
    const minSf = Math.min(minM, minZ);
    if (Number.isFinite(minSf)) {
      return { minSf, governs: minZ < minM ? "layer" : "material" };
    }
  } catch {
    // result vanished mid-fetch
  }
  return null;
}

/** Fetch + cache both safety-factor fields and write the min (and which
 *  limit governs) into printedStats. Returns the minima for logging, or
 *  null when there is no printed result / it vanished mid-fetch. */
async function refreshMinSf(
  set: SetState,
  get: () => AppState
): Promise<{ minSf: number; governs: "layer" | "material" } | null> {
  if (!get().printedStats) return null;
  const sf = await computeMinSf(true);
  if (sf && get().printedStats) {
    set({ printedStats: { ...get().printedStats!, minSf: sf.minSf, sfGoverns: sf.governs } });
    return sf;
  }
  // no printed result, or the field vanished mid-fetch: dock shows mass/deflection only
  return null;
}

/** Apply a rotation about the part's bbox center, then seat it on the build
 *  plate (z-min → 0) and re-center it over the plate origin in XY. Patches and
 *  BC selections survive (index-based); the grid and every result drop on both
 *  sides. */
async function transformModel(set: SetState, get: () => AppState, r: number[]) {
  const st = get();
  if (!st.model || st.busy) return;
  const b = st.model.bbox;
  const c = [(b[0] + b[3]) / 2, (b[1] + b[4]) / 2, (b[2] + b[5]) / 2];
  const t = [
    c[0] - (r[0] * c[0] + r[1] * c[1] + r[2] * c[2]),
    c[1] - (r[3] * c[0] + r[4] * c[1] + r[5] * c[2]),
    c[2] - (r[6] * c[0] + r[7] * c[1] + r[8] * c[2]),
  ];
  try {
    let out = await engine.transform([...r, ...t]);
    // Seat on the plate (z-min → 0) and re-center the XY footprint over the
    // plate origin, so a rotate / place-on-face never drifts the part off the
    // build grid.
    const cx = (out.bbox[0] + out.bbox[3]) / 2;
    const cy = (out.bbox[1] + out.bbox[4]) / 2;
    const dz = out.bbox[2];
    if (Math.abs(cx) > 1e-6 || Math.abs(cy) > 1e-6 || Math.abs(dz) > 1e-6) {
      out = await engine.transform([1, 0, 0, 0, 1, 0, 0, 0, 1, -cx, -cy, -dz]);
    }
    const bbox = out.bbox as LoadedModel["bbox"];
    set({ model: { ...get().model!, positions: out.positions, bbox } });
    invalidateResults(set, get);
    invalidateGrid(set, get);
    sceneEvents.onModelTransformed?.(out.positions, bbox);
    // The symmetry plane keeps its world position; re-center it on the
    // moved part so it doesn't strand outside.
    if (get().symOn) get().centerSymmetry();
  } catch (e) {
    set({ error: e instanceof Error ? e.message : String(e) });
  }
}

/** Push the symmetry plane to the viewport. Visible only while it's being
 *  edited: checkbox on, Optimize step active, nothing running (the scene
 *  additionally hides it in result views). */
function pushSymmetry(get: () => AppState) {
  const s = get();
  const enabled = s.symOn && s.activeStep === 5 && !s.busy;
  sceneEvents.onSymmetry?.(enabled, s.symNormal, s.symC);
}

/** Short human label of the symmetry plane for logs and the panel. */
export function symLabel(normal: [number, number, number], c: number): string {
  const axes: ["X" | "Y" | "Z", number][] = [
    ["X", normal[0]],
    ["Y", normal[1]],
    ["Z", normal[2]],
  ];
  const major = axes.find(([, v]) => Math.abs(v) > 0.9999);
  if (major) return `⊥${major[0]} @ ${formatQ(c * Math.sign(major[1]), "length")}`;
  return `tilted (n = ${normal.map((v) => v.toFixed(2)).join(", ")})`;
}

const MAX_LOG_LINES = 800;

function logTime(): string {
  return new Date().toLocaleTimeString([], { hour12: false });
}

type SetState = (p: Partial<AppState> | ((s: AppState) => Partial<AppState>)) => void;

function appendLog(set: SetState, msg: string) {
  set((s) => ({
    logLines: [...s.logLines.slice(-(MAX_LOG_LINES - 1)), { t: logTime(), msg }],
  }));
}

/** Log the analysis grid when it (re)builds — entry of check/solve/optimize. */
async function logGridInfo(set: SetState) {
  try {
    const info = await engine.voxelInfo();
    const prev = useStore.getState().voxelInfo;
    if (
      !prev ||
      prev.nx !== info.nx ||
      prev.ny !== info.ny ||
      prev.nz !== info.nz ||
      prev.solid !== info.solid
    ) {
      appendLog(
        set,
        `Voxel grid ${info.nx}×${info.ny}×${info.nz} @ h=${info.h.toFixed(2)} mm — ` +
          `${info.solid.toLocaleString()} voxels`
      );
    }
    set({ voxelInfo: info });
  } catch {
    // grid not buildable yet — the caller surfaces the real error
  }
}

function fieldUnit(kind: string): string {
  if (kind.startsWith("sf")) return "×"; // marker labels show a plain factor
  if (kind === "peel" || kind === "peelshear") return "MPa"; // build-sim bed traction
  return RESULT_FIELDS.find((f) => f.value === kind)?.unit ?? "";
}

/** Events the 3D scene listens to (kept out of React rendering). */
export interface SceneEvents {
  onModelLoaded?: (m: LoadedModel) => void;
  onPatchIdsChanged?: (patchIds: Uint32Array) => void;
  onBcsChanged?: (bcs: Bc[], activeBcId: string | null, inactive?: Set<string>) => void;
  onAnimateMode?: (mode: { t: number[]; r: number[]; center: number[] } | null) => void;
  onDisplacements?: (disp: Float32Array | null, stats: { maxDisplacement: number } | null) => void;
  onVertexDensity?: (density: Float32Array | null) => void;
  onRegions?: (regions: OptRegion[] | null) => void;
  /** Result is a Part Topo body (solid): hide the original envelope hull in the
   *  result views and render the body opaque. */
  onResultSolid?: (solid: boolean) => void;
  /** Snapshot the current view to a square PNG for the 3MF plate thumbnail. */
  captureThumbnail?: () => Uint8Array | null;
  onViewState?: (mode: ViewMode, deformScale: number) => void;
  onVoxelMesh?: (
    hull: Float32Array | null,
    edges: Float32Array | null,
    density?: Float32Array | null
  ) => void;
  /** Build-sim live preview: faint full-hull ghost (deactivated voxels). */
  onBuildGhost?: (positions: Float32Array | null) => void;
  /** Build-sim live preview: growing deformed active hull (printed voxels),
   *  jet-colored by normalised |u| (`mags`). */
  onBuildActive?: (positions: Float32Array | null, mags?: Float32Array | null) => void;
  /** Build-sim bed-peel heatmap on the plate (flat soup + per-vertex value,
   *  jet-normalised to `max`). null clears it. */
  onPeelMap?: (
    positions: Float32Array | null,
    values: Float32Array | null,
    max: number
  ) => void;
  /** Same part, new pose (orientation tools): swap positions in place. */
  onModelTransformed?: (positions: Float32Array, bbox: LoadedModel["bbox"]) => void;
  /** Symmetry plane state for the viewport: enabled + plane n·p = c. */
  onSymmetry?: (enabled: boolean, normal: [number, number, number], c: number) => void;
  /** Color mesh-view cells by element density. */
  onMeshDensity?: (on: boolean) => void;
  /** Force the density ramp on mesh cells (inherent-strain layer view). */
  onMeshFieldColor?: (on: boolean) => void;
  /** Toggle the triangle-mesh wireframe overlay on the model. */
  onWireframe?: (on: boolean) => void;
  /** Voxel-true section active: the scene must NOT plane-clip the voxel
   *  group (the cut already lives in the geometry) and hides its cap. */
  onVoxelCutActive?: (on: boolean) => void;
  onAnimateDeformed?: (on: boolean) => void;
  /** Modal result active: animate the mode shape as a symmetric ± swing
   *  (vibration) rather than the one-sided 0 → max deflection loop. */
  onModalAnim?: (on: boolean) => void;
  /** Live optimization skeleton or density-threshold cutaway mesh,
   *  optionally colored by a per-vertex density scalar. */
  onOptShape?: (
    positions: Float32Array | null,
    indices: Uint32Array | null,
    density?: Float32Array | null
  ) => void;
  onRegionVisibility?: (visible: boolean[]) => void;
  /** Stress/strain scalars for the deformed view (null = |u| colors).
   *  flip inverts the colormap (safety factor: red = critical LOW);
   *  signed centers the color scale on 0 (signed von Mises: ±tension);
   *  range widens the auto color scale (interior extremes — legend, surface
   *  and section cap share one honest scale). */
  onScalarField?: (
    values: Float32Array | null,
    flip?: boolean,
    signed?: boolean,
    range?: { min: number; max: number }
  ) => void;
  /** Volumetric section payload for the capped section view (null clears —
   *  the cap falls back to its plain cut color). */
  onSectionVolume?: (data: SectionVolume | null) => void;
  /** Color the deformed view by a displacement quantity: -1 = |u| magnitude,
   *  0/1/2 = signed X/Y/Z component (computed from the displacement buffer). */
  onDispComponent?: (comp: number) => void;
  /** Voxel-hull result geometry: hull soup + exact nodal displacements,
   *  cell-edge segments + their displacements (nulls clear it). */
  onVoxelResult?: (
    positions: Float32Array | null,
    displacements: Float32Array | null,
    edges: Float32Array | null,
    edgeDisplacements: Float32Array | null
  ) => void;
  /** Which surface the deformed view shows: smooth STL or analysis voxels. */
  onResultSurface?: (surface: "stl" | "voxel") => void;
  /** User override of the color-scale range (nulls = auto). */
  onLegendRange?: (min: number | null, max: number | null) => void;
  /** Min/max location markers; unit drives the label formatting. */
  onShowExtremes?: (on: boolean, unit: string) => void;
  /** Display unit changed — re-format imperative overlays (pinned callouts). */
  onUnitsChanged?: () => void;
  /** Smooth vs discrete (banded) result contours, and the band count. */
  onBandedContour?: (on: boolean, count: number) => void;
  // Section plane controls.
  onSectionState?: (on: boolean) => void;
  onSectionFlip?: () => void;
  onSectionAxis?: (axis: "x" | "y" | "z") => void;
  /** Display-only orientation preview (DESIGN §15): rotate the part so the
   *  given layer normal (part frame) points up; null restores the true pose. */
  onOrientationPreview?: (dir: [number, number, number] | null) => void;
}

export const sceneEvents: SceneEvents = {};

/** Resolve the BCs the solver should see for one load step (DESIGN §13):
 *  drop BCs deactivated in the step and apply its per-step force / pressure
 *  overrides. With an empty override map (the default single step) this returns
 *  the base BCs unchanged — so the single-case solve is byte-identical. */
export function effectiveBcs(bcs: Bc[], step: LoadStep | undefined): Bc[] {
  if (!step) return bcs;
  const out: Bc[] = [];
  for (const b of bcs) {
    const ov = step.overrides[b.id];
    if (!ov) {
      out.push(b);
      continue;
    }
    if (ov.active === false) continue; // deactivated in this step
    if (ov.force === undefined && ov.pressure === undefined && ov.moment === undefined) {
      out.push(b);
      continue;
    }
    out.push({
      ...b,
      force: ov.force ?? b.force,
      pressure: ov.pressure ?? b.pressure,
      moment: ov.moment ?? b.moment,
    });
  }
  return out;
}

/** The load step currently being edited / solved. */
function activeStep(s: AppState): LoadStep | undefined {
  return s.loadSteps.find((ls) => ls.id === s.activeLoadStepId);
}

/** BCs as the active step sees them, for the 3D glyphs + surface tint: per-step
 *  force / pressure values applied so the viewport reflects the case you're on.
 *  Deactivated BCs keep their values too — the viewport draws them TRANSLUCENT
 *  (see `inactiveBcIds`) rather than hiding them. Single step (empty overrides)
 *  → returns the base BCs unchanged. */
function sceneBcs(bcs: Bc[], step: LoadStep | undefined): Bc[] {
  if (!step) return bcs;
  return bcs.map((b) => {
    const ov = step.overrides[b.id];
    if (!ov) return b;
    return {
      ...b,
      force: ov.force ?? b.force,
      pressure: ov.pressure ?? b.pressure,
      moment: ov.moment ?? b.moment,
    };
  });
}

/** Ids of BCs deactivated in the active step — drawn translucent in the viewport. */
function inactiveBcIds(step: LoadStep | undefined): Set<string> {
  const set = new Set<string>();
  if (step) {
    for (const [id, ov] of Object.entries(step.overrides)) {
      if (ov.active === false) set.add(id);
    }
  }
  return set;
}

/** Push the active step's BC glyphs to the viewport. */
function pushBcGlyphs(get: () => AppState, activeBcId?: string | null) {
  const id = activeBcId === undefined ? get().activeBcId : activeBcId;
  const step = activeStep(get());
  sceneEvents.onBcsChanged?.(sceneBcs(get().bcs, step), id, inactiveBcIds(step));
}

async function pushBcs(get: () => AppState) {
  await engine.setBcs(effectiveBcs(get().bcs, activeStep(get())));
}


/** After a per-step override edit: if it touched the ACTIVE step, re-sync the
 *  engine (live RBM check + next solve) and stale any standing result. Edits to
 *  a non-active step are invisible to the solver until that step is selected. */
function syncIfActiveStep(
  set: (p: Partial<AppState>) => void,
  get: () => AppState,
  stepId: string
) {
  if (stepId !== get().activeLoadStepId) return;
  markResultsStale(set, get, "loads");
  void pushBcs(get);
  pushBcGlyphs(get);
}

/** Area-weighted average outward normal of a triangle selection (matches the
 *  Rust `average_normal`: sum of area vectors, normalized). `positions` is the
 *  triangle soup (9 floats/tri). Returns null for an empty/degenerate set. */
function selectionNormal(
  positions: Float32Array | undefined,
  tris: Uint32Array
): [number, number, number] | null {
  if (!positions || tris.length === 0) return null;
  let nx = 0;
  let ny = 0;
  let nz = 0;
  for (const t of tris) {
    const o = 9 * t;
    const e1x = positions[o + 3] - positions[o];
    const e1y = positions[o + 4] - positions[o + 1];
    const e1z = positions[o + 5] - positions[o + 2];
    const e2x = positions[o + 6] - positions[o];
    const e2y = positions[o + 7] - positions[o + 1];
    const e2z = positions[o + 8] - positions[o + 2];
    nx += 0.5 * (e1y * e2z - e1z * e2y);
    ny += 0.5 * (e1z * e2x - e1x * e2z);
    nz += 0.5 * (e1x * e2y - e1y * e2x);
  }
  const len = Math.hypot(nx, ny, nz);
  if (len < 1e-12) return null;
  return [nx / len, ny / len, nz / len];
}

/** Resolved load vector of a force BC for the solver: the direction × the
 *  magnitude in "direction" mode, the components verbatim otherwise. */
function resolveForce(bc: Bc): [number, number, number] {
  if (bc.forceMode === "direction" && bc.forceDir) {
    const m = bc.forceMag ?? 0;
    return [bc.forceDir[0] * m, bc.forceDir[1] * m, bc.forceDir[2] * m];
  }
  return bc.force ?? [0, 0, 0];
}

/** Magnitude a force should carry when switching into direction mode: the
 *  explicit forceMag, else the length of the current component vector, else a
 *  sensible 10 N default. */
function forceMagFor(bc: Bc): number {
  if (bc.forceMag != null) return bc.forceMag;
  const f = bc.force;
  const len = f ? Math.hypot(f[0], f[1], f[2]) : 0;
  return len || 10;
}

/** Bounding-box volume of the loaded model (mm³), 0 without a model. */
function bboxVolume(s: AppState): number {
  const b = s.model?.bbox;
  if (!b) return 0;
  return Math.max(0, (b[3] - b[0]) * (b[4] - b[1]) * (b[5] - b[2]));
}

/** Target cell count of the active resolution. Custom mode derives it from
 *  the user's cell SIZE and the part's bbox (engine-capped 10k–4M). */
function resolutionCells(s: AppState): number {
  if (s.resolution === "custom") {
    const vol = bboxVolume(s);
    if (vol <= 0 || s.customH <= 0) return RESOLUTIONS.normal;
    return Math.min(4_000_000, Math.max(10_000, Math.round(vol / s.customH ** 3)));
  }
  return RESOLUTIONS[s.resolution];
}

/** Push the analysis resolution to the engine: a preset targets a SOLID-cell
 *  count (the engine sizes the cell from the part volume); custom pins the exact
 *  cell size. */
async function pushResolution(get: () => AppState) {
  const s = get();
  if (s.resolution === "custom" && s.customH > 0) {
    await engine.setVoxelSize(s.customH);
  } else {
    await engine.setResolution(resolutionCells(get()));
  }
}

/** Push the voxel-snap wall to the engine from the current print settings. */
async function pushSnap(get: () => AppState) {
  const s = get();
  if (!s.model) return; // nothing loaded yet — loadFile pushes the snap
  await engine.setSnapWall(s.snapVoxel ? s.perimeters * s.lineWidth : 0);
}

/** (Re)build the Mesh-view voxel hull: full, or voxel-true cut by the
 *  section plane (whole cells dropped — the interior cells become visible,
 *  so the skin thickness can be inspected instead of a planar cut). */
async function refreshMeshView(set: SetState, get: () => AppState): Promise<boolean> {
  const st = get();
  if (!st.model || st.viewMode !== "mesh") return true;
  // Inherent-strain layer view: voxel hull up to the scrubbed build layer,
  // cells colored by their per-element strain source (∝ density). Uses the
  // material shrink (XY/Z) to scale the reported source magnitude.
  if (st.strainView) {
    try {
      const shrinkXy = Math.abs(st.material.shrink);
      const shrinkZ = Math.abs(st.material.shrinkZ ?? st.material.shrink);
      const layer = st.strainLayer > 0 ? st.strainLayer : 1_000_000;
      const { hull, values, edges, max, nz } = await engine.inherentStrainVoxels(
        layer,
        shrinkXy,
        shrinkZ
      );
      if (get().viewMode !== "mesh" || !get().strainView) return true;
      set({
        strainLayerMax: nz,
        strainLayer: get().strainLayer > 0 ? Math.min(get().strainLayer, nz) : nz,
        strainPeakMPa: max,
      });
      sceneEvents.onVoxelCutActive?.(false);
      sceneEvents.onMeshFieldColor?.(true);
      sceneEvents.onVoxelMesh?.(hull, edges, values);
      return true;
    } catch (e) {
      set({ error: e instanceof Error ? e.message : String(e) });
      return false;
    }
  }
  sceneEvents.onMeshFieldColor?.(false);
  const wall = st.perimeters * st.lineWidth;
  const cutting = st.sectionOn && lastSectionPlane !== null;
  try {
    const { hull, edges, density, info } = await engine.voxelMeshCut(
      cutting ? lastSectionPlane : null,
      wall,
      st.topBottomLayers * st.layerHeight,
      st.printInfill
    );
    if (get().viewMode !== "mesh") return true; // user moved on mid-fetch
    set({ voxelInfo: info, voxelMeshReady: true });
    sceneEvents.onVoxelCutActive?.(cutting);
    sceneEvents.onVoxelMesh?.(hull, edges, density);
    return true;
  } catch (e) {
    set({ error: e instanceof Error ? e.message : String(e) });
    return false;
  }
}

/** Eagerly build + cache the analysis voxel hull on the scene so the Mesh view
 *  is available INSTANTLY — including DURING a solve/optimization, when the
 *  single worker is blocked inside the WASM solver and can't serve a fresh
 *  request. Builds the full (uncut) hull; section re-cuts happen lazily once
 *  the worker is free again. Skips when a current hull is already cached. */
async function prebuildMeshView(set: SetState, get: () => AppState) {
  const st = get();
  if (!st.model || st.voxelMeshReady) return;
  const wall = st.perimeters * st.lineWidth;
  try {
    const { hull, edges, density, info } = await engine.voxelMeshCut(
      null,
      wall,
      st.topBottomLayers * st.layerHeight,
      st.printInfill
    );
    set({ voxelInfo: info, voxelMeshReady: true });
    sceneEvents.onVoxelCutActive?.(false);
    // The scene retains the hull and only shows it when the Mesh view is active
    // (refreshView gates voxelGroup visibility) — caching it now is invisible
    // until the user switches to Mesh, mid-run or after.
    sceneEvents.onVoxelMesh?.(hull, edges, density);
  } catch {
    // Non-fatal: the Mesh view just stays lazy (built on first entry).
  }
}

/** DESTRUCTIVE reset — geometry/grid changes only. The stashed results sit on
 *  the OLD node grid (or moved part), so they can't be shown correctly: drop
 *  the whole set (engine stash included) and snap out of any result view. */
function invalidateResults(set: (p: Partial<AppState>) => void, get: () => AppState) {
  clearEnvelopeCache();
  set({
    check: null,
    stats: null,
    hasResult: false,
    optSummary: null,
    printedStats: null,
    results: [],
    activeResultId: null,
    resultEpochs: { ...ZERO_EPOCHS },
    regionInfos: [],
    regionVisible: [],
    densityThreshold: 0,
    resultField: "u",
    fieldRange: null,
    legendMin: null,
    legendMax: null,
  });
  session.invalidateSolution();
  void engine.clearResults();
  sceneEvents.onLegendRange?.(null, null);
  sceneEvents.onScalarField?.(null);
  sceneEvents.onRegions?.(null);
  sceneEvents.onResultSolid?.(false);
  sceneEvents.onVertexDensity?.(null);
  sceneEvents.onDisplacements?.(null, null);
  sceneEvents.onOptShape?.(null, null);
  if (get().viewMode !== "setup" && get().viewMode !== "mesh") {
    set({ viewMode: "setup" });
    sceneEvents.onViewState?.("setup", get().deformScale);
  }
}

/** Clear the LIVE result + deformed view (stats, hasResult, the deformed scene)
 *  WITHOUT dropping the stashed roster — used when switching workspaces so one
 *  workspace's output can't masquerade as the other's (Build Sim warp leaking
 *  into Verify, etc.). Roster results stay re-selectable. */
function clearLiveResultView(set: (p: Partial<AppState>) => void, get: () => AppState) {
  set({
    check: null,
    stats: null,
    hasResult: false,
    optSummary: null,
    printedStats: null,
    activeResultId: null,
    resultField: "u",
    fieldRange: null,
    legendMin: null,
    legendMax: null,
    buildResult: null,
    // The exaggeration belonged to the result being cleared (Build Sim derives
    // a result-specific deformScale) — don't leak it into the next workspace.
    deformScale: 1,
  });
  session.invalidateSolution();
  sceneEvents.onLegendRange?.(null, null);
  sceneEvents.onScalarField?.(null);
  sceneEvents.onPeelMap?.(null, null, 0);
  sceneEvents.onRegions?.(null);
  sceneEvents.onResultSolid?.(false);
  sceneEvents.onVertexDensity?.(null);
  sceneEvents.onDisplacements?.(null, null);
  sceneEvents.onOptShape?.(null, null);
  if (get().viewMode !== "setup" && get().viewMode !== "mesh") {
    set({ viewMode: "setup" });
    sceneEvents.onViewState?.("setup", get().deformScale);
  }
}

/** NON-destructive — bump the given input epochs so the dependent retained
 *  results badge "re-run needed" while staying viewable on the same grid.
 *  No-op when nothing is retained. */
function markResultsStale(
  set: (p: Partial<AppState>) => void,
  get: () => AppState,
  ...keys: (keyof ResultEpochs)[]
) {
  if (get().results.length === 0) return;
  const ep = { ...get().resultEpochs };
  for (const k of keys) ep[k] += 1;
  set({ resultEpochs: ep });
  // The orientation map was swept from the now-stale inputs — delete it
  // outright (unlike results it has no staleness badge of its own).
  clearOrientationSweep();
}

/** Insert a freshly-computed (single-step) result, REPLACING every prior result
 *  of the same kind — including the per-step entries of an earlier multi-step
 *  solve, which a single-step re-run supersedes — and keep the set ordered. */
function upsertResult(set: (p: Partial<AppState>) => void, get: () => AppState, entry: ResultEntry) {
  const next = get().results.filter((r) => r.kind !== entry.kind);
  next.push(entry);
  set({ results: sortResults(next, get().loadSteps) });
}

/** Length provenance cell (canonical mm) — formatted live in the display unit. */
function fmtMm(v: number): ProvVal {
  return { v, kind: "length" };
}

/** Mass provenance cell (canonical g) — formatted live in the display unit. */
function fmtMass(g: number): ProvVal {
  return { v: g, kind: "mass" };
}

/** Signed percentage with an explicit + (negatives carry their own −). Both
 *  "vs uniform" (positive: stiffer than even fill) and "vs solid" (negative:
 *  softer than fully dense) read off the same compliance-ratio calculation. */
function signedPct(x: number): string {
  return `${x >= 0 ? "+" : ""}${(x * 100).toFixed(0)}%`;
}

/** Analysis-grid summary line for the provenance card. */
function meshLabel(s: AppState): ProvVal {
  const vi = s.voxelInfo;
  return vi
    ? {
        v: vi.h,
        kind: "length",
        prefix: "h ",
        // Just the solid voxel count — the bounding-box total is redundant with
        // the grid dims and only counts empty space the analysis skips.
        suffix: ` · ${Math.round(vi.solid / 1000)}k voxels`,
      }
    : "—";
}

/** Shared deformation reference = the LOWEST-deflection retained result (min
 *  max |u|). Feeding this same value to the viewer for EVERY result keeps the
 *  exaggeration factor equal across switches, so a stiffer result visibly
 *  deflects less than a softer one instead of each self-normalizing. `extra`
 *  folds in a result that's being created before it's in the set. */
function referenceMaxDisp(results: ResultEntry[], extra?: number): number {
  let m = Infinity;
  for (const r of results) if (r.maxDisplacement > 0) m = Math.min(m, r.maxDisplacement);
  if (extra && extra > 0) m = Math.min(m, extra);
  return Number.isFinite(m) && m > 0 ? m : 1;
}

/** The color range to PIN for the field currently on screen, used to hold the
 *  legend fixed while stepping through load cases (and as the "fit" target).
 *  |u| anchors at 0; signed components and stress/SF fields take the scene-
 *  reported data range. Null when there's nothing concrete to pin yet. */
function currentLegendRange(s: AppState): [number, number] | null {
  const fr = s.fieldRange;
  if (s.resultField === "u") {
    const max =
      fr?.max ?? s.results.find((r) => r.id === s.activeResultId)?.maxDisplacement ?? 0;
    return max > 0 ? [0, max] : null;
  }
  return fr ? [fr.min, fr.max] : null;
}

// ---- envelope (worst case across load steps) ----

/** The FieldServer holds the reduced-envelope fields (keyed `${kind}::${field}`).
 *  Cleared whenever the result set is rebuilt (new solve / grid drop) — the
 *  stashes it reduces over would no longer match. */
function clearEnvelopeCache() {
  fieldServer.clearEnvelopeCache();
  // The orientation sweep (DESIGN §15) reduces over the same stashes — every
  // invalidation that stales the envelope stales it too.
  clearOrientationSweep();
}

/** Drop the orientation sweep + preview (DESIGN §15): restores the true pose
 *  and orphans a sweep still in flight via the token (its resolve is
 *  discarded). Called on envelope invalidation AND whenever loads/inputs
 *  change (`markResultsStale`) — the map was computed from the old inputs. */
function clearOrientationSweep() {
  orientToken++;
  const s = useStore.getState();
  if (s.orientSel) sceneEvents.onOrientationPreview?.(null);
  if (s.orientSweep || s.orientSel || s.orientProgress) {
    useStore.setState({ orientSweep: null, orientSel: null, orientProgress: null });
  }
}

/** Monotone token orphaning in-flight orientation sweeps on invalidation. */
let orientToken = 0;

/** One in-flight preview-field fetch at a time; while the user drags across
 *  the heatmap only the LATEST selection is fetched when the current request
 *  resolves (trailing-edge coalescing — the worker never floods). */
let orientFetchBusy = false;

function orientFetchField(set: SetState, get: () => AppState) {
  if (orientFetchBusy) return;
  orientFetchBusy = true;
  void (async () => {
    try {
      for (;;) {
        const sel = get().orientSel;
        const sw = get().orientSweep;
        if (!sel || !sw) break;
        const dir = orientDir(-90 + sel.ip * sw.stepDeg, -90 + sel.ir * sw.stepDeg);
        const values = await engine.layerSfField(dir, sw.ids, get().resultSurface);
        const now = get().orientSel;
        if (!now) break;
        if (now.ip !== sel.ip || now.ir !== sel.ir) continue; // stale — refetch newest
        let lo = Infinity;
        let hi = -Infinity;
        for (const v of values) {
          if (!Number.isFinite(v)) continue; // NaN = grey-masked ring cells
          if (v < lo) lo = v;
          if (v > hi) hi = v;
        }
        if (!Number.isFinite(lo) || lo === hi) {
          lo = 0;
          hi = Math.max(hi, 1);
        }
        sceneEvents.onScalarField?.(values, true, false, { min: lo, max: hi });
        set({ fieldRange: { min: lo, max: hi } });
        const after = get().orientSel;
        if (!after || (after.ip === sel.ip && after.ir === sel.ir)) break;
        // selection moved during the push — go around once more
      }
    } catch {
      get().clearOrientationPreview(); // result vanished mid-fetch
    } finally {
      orientFetchBusy = false;
    }
  })();
}

/** Layer normal n = Rx(pitch)·Ry(roll)·ẑ — must match core orient.rs. */
function orientDir(pitchDeg: number, rollDeg: number): [number, number, number] {
  const p = (pitchDeg * Math.PI) / 180;
  const r = (rollDeg * Math.PI) / 180;
  return [Math.sin(r), -Math.sin(p) * Math.cos(r), Math.cos(p) * Math.cos(r)];
}

/** Display an envelope result: undeformed geometry (it has no single
 *  displacement state) colored by the worst-case scalar field. */
async function showEnvelope(set: SetState, get: () => AppState, e: ResultEntry) {
  const model = get().model;
  if (!model) return;
  // The reduction runs on the smooth-surface fields — force the STL surface.
  if (get().resultSurface === "voxel") {
    set({ resultSurface: "stl" });
    sceneEvents.onResultSurface?.("stl");
  }
  session.invalidateSolution();
  sceneEvents.onResultSolid?.(false);
  // Zero displacement buffer → the part sits undeformed; the worst-case contour
  // carries the meaning. Length matches the surface soup (9 floats per triangle).
  const zero = new Float32Array(model.triCount * 9);
  sceneEvents.onScalarField?.(null);
  sceneEvents.onDisplacements?.(zero, { maxDisplacement: referenceMaxDisp(get().results) });
  await pushScalarField(set, get); // envelope branch reduces + paints the field
}

/** Standard analysis across MULTIPLE load steps (DESIGN §13). Solves every step
 *  — steps that share fixtures reuse the cached multigrid hierarchy, so only the
 *  forces (cheap RHS) change between them — stashes each as a per-step result
 *  (`kind::stepId`), then displays the active step. Single-step projects never
 *  reach here: `runSolve` keeps its byte-identical path. THROWS on cancel /
 *  solver error (runSolve's catch handles it); returns early (no throw) when a
 *  step is under-constrained. */
async function solveAllSteps(set: SetState, get: () => AppState) {
  const st0 = get();
  const steps = st0.loadSteps;
  const m = st0.material;
  const printed = st0.analyzeMode === "printed";
  const curve = st0.curves[st0.pattern];
  const kind: ResultKind = printed ? "asprinted" : "solid";
  const activeId = st0.activeLoadStepId;

  appendLog(set, `Solve ${steps.length} load steps — ${printed ? "as printed" : "solid"}, ${m.name}`);
  await logGridInfo(set);
  clearEnvelopeCache(); // the previous solve's reduced fields no longer apply

  // 1) Validate every step up front (each may toggle different supports), so a
  //    bad step aborts cleanly instead of leaving a half-built roster.
  for (let i = 0; i < steps.length; i++) {
    set({ busy: `Checking load step ${i + 1}/${steps.length}: ${steps[i].name}…` });
    await engine.setBcs(effectiveBcs(st0.bcs, steps[i]));
    const report = await engine.check();
    if (steps[i].id === activeId) set({ check: report });
    if (!report.ok) {
      const bad = report.components.find((c) => !c.constrained && c.mode);
      set({ activeLoadStepId: steps[i].id, check: report });
      pushBcGlyphs(get);
      sceneEvents.onAnimateMode?.(bad?.mode ?? null);
      appendLog(set, `Solve aborted: load step "${steps[i].name}" is under-constrained`);
      set({
        busy: null,
        error:
          report.islandCount > 1
            ? `Load step "${steps[i].name}" has ${report.islandCount} disconnected parts and is under-constrained — see the animated motion.`
            : `Load step "${steps[i].name}" is under-constrained — the animation shows the free motion. Add or extend supports.`,
      });
      return;
    }
  }

  // 2) Cache the voxel hull once (BC-independent) so the Mesh view is viewable
  //    during the blocking solves.
  await prebuildMeshView(set, get);

  // 3) Solve each step, stash it, build its result entry.
  const entries: ResultEntry[] = [];
  let displayStats: SolveStats | null = null;
  let displayPrinted: PrintedSummary | null = null;
  let displayMinSf: { minSf: number; governs: "layer" | "material" } | null = null;
  for (let i = 0; i < steps.length; i++) {
    const step = steps[i];
    set({ busy: `Solving load step ${i + 1}/${steps.length}: ${step.name}…`, solveResiduals: [] });
    await engine.setBcs(effectiveBcs(st0.bcs, step));
    const stop = session.startResidualPoll((r) => set({ solveResiduals: r }));
    let stats: SolveStats;
    let printedSummary: PrintedSummary | null = null;
    try {
      if (printed) {
        const out = await engine.solvePrinted({
          infillPct: st0.printInfill,
          exponent: curve.exponent,
          coeff: curve.coeff,
          perimeters: st0.perimeters,
          lineWidth: st0.lineWidth,
          topBottomLayers: st0.topBottomLayers,
          layerHeight: st0.layerHeight,
        });
        stats = out.stats;
        printedSummary = {
          massGrams: out.stats.massGrams,
          massSolidGrams: out.stats.massSolidGrams,
          skinLayers: out.stats.skinLayers,
          compositeSkin: out.stats.compositeSkin,
          infillPct: st0.printInfill,
          pattern: st0.pattern,
          perimeters: st0.perimeters,
          lineWidth: st0.lineWidth,
          minSf: null,
          sfGoverns: null,
        };
      } else {
        const out = await engine.solve();
        stats = out.stats;
      }
    } finally {
      stop();
    }
    // Per-step min safety factor (printed only). Don't cache the fields — a
    // non-displayed step's sf would otherwise shadow the displayed one.
    let minSf: number | null = null;
    if (printed) {
      const sf = await computeMinSf(false);
      if (sf) minSf = sf.minSf;
      if (step.id === activeId) displayMinSf = sf;
    }
    const rid = resultStashId(kind, step.id, false);
    await engine.stashResult(rid);
    const cur = get();
    const rows: [string, ProvVal][] = printedSummary
      ? [
          ["Load step", step.name],
          ["Infill", `${printedSummary.infillPct}% ${printedSummary.pattern}`],
          ["Skin", { v: printedSummary.lineWidth, kind: "length", prefix: `${printedSummary.perimeters} × ` }],
          ["Material", m.name],
          ["Mesh", meshLabel(cur)],
          ["Mass", fmtMass(printedSummary.massGrams)],
          ["Max |u|", fmtMm(stats.maxDisplacement)],
        ]
      : [
          ["Load step", step.name],
          ["Model", "fully dense E₀"],
          ["Material", m.name],
          ["Mesh", meshLabel(cur)],
          ["Max |u|", fmtMm(stats.maxDisplacement)],
        ];
    entries.push({
      id: rid,
      kind,
      loadStepId: step.id,
      loadStepName: step.name,
      label: printed ? `As printed · ${st0.printInfill}% ${st0.pattern}` : "Solid material",
      maxDisplacement: stats.maxDisplacement,
      massGrams: printedSummary ? printedSummary.massGrams : null,
      minSf,
      converged: stats.converged,
      provTitle: `${printed ? "As printed" : "Solid material"} · ${step.name}`,
      provRows: rows,
      epochs: { ...cur.resultEpochs },
    });
    appendLog(
      set,
      `  ${step.name}: max |u| ${stats.maxDisplacement.toExponential(2)} mm` +
        (printed && minSf != null ? `, min SF ${minSf.toFixed(2)}×` : "") +
        (stats.converged ? "" : " — UNCONVERGED")
    );
    if (step.id === activeId) {
      displayStats = stats;
      displayPrinted = printedSummary;
    }
  }

  // 4) Display the active step: make its stash the live solution and paint it.
  //    (Every step was solved; the active one is what the user was editing.)
  const finalStats = displayStats!; // the active step is always one of `steps`
  const activeRid = resultStashId(kind, activeId, false);
  const roster = withEnvelope(
    sortResults([...st0.results.filter((r) => r.kind !== kind), ...entries], steps),
    steps
  );
  session.invalidateSolution();
  const disp = await engine.activateResult(activeRid);
  sceneEvents.onLegendRange?.(null, null);
  const anyUnconverged = entries.some((e) => !e.converged);
  set({
    results: roster,
    activeResultId: activeRid,
    stats: finalStats,
    printedStats: displayPrinted
      ? {
          ...displayPrinted,
          minSf: displayMinSf?.minSf ?? null,
          sfGoverns: displayMinSf?.governs ?? null,
        }
      : null,
    solveResiduals: finalStats.residuals ?? [],
    solveTol: finalStats.tol ?? get().solveTol,
    hasResult: true,
    viewMode: "deformed",
    busy: null,
    resultField: "u",
    fieldRange: null,
    legendMin: null,
    legendMax: null,
    notice: anyUnconverged
      ? "Some load steps did not converge (stopped at the iteration cap) — those results are only indicative. A coarser resolution converges reliably."
      : displayPrinted && !displayPrinted.compositeSkin && displayPrinted.skinLayers === 1
        ? "The wall is only one voxel layer thick at this resolution — printed-mode results are coarse. Raise the resolution in Properties, or enable composite skin."
        : null,
  });
  sceneEvents.onResultSolid?.(false); // a standard solve is never the optimized solid body
  sceneEvents.onScalarField?.(null);
  sceneEvents.onDisplacements?.(disp, { maxDisplacement: referenceMaxDisp(roster) });
  sceneEvents.onViewState?.("deformed", get().deformScale);
  if (get().resultSurface === "voxel") {
    try {
      await session.loadVoxelResult();
    } catch {
      set({ resultSurface: "stl" });
      sceneEvents.onResultSurface?.("stl");
    }
  }
  appendLog(
    set,
    `Solve complete: ${steps.length} load steps. Showing "${activeStep(get())?.name ?? ""}".`
  );
}

/** After a multi-step optimization, evaluate the SINGLE optimized design under
 *  every load step so the Results roster offers each one — not just the
 *  optimizer's primary case (DESIGN §13). The optimized stress eps is
 *  BC-independent, so each step is a fresh loads/supports solve (shared-support
 *  steps reuse the cached multigrid hierarchy) via engine.solveOptimized().
 *  Returns the per-step optimized entries (`optimized::stepId`); the caller
 *  folds in the baselines + envelope and chooses which to display. A step that's
 *  under-constrained on its own is skipped, never fatal — the optimize result is
 *  already on screen. The engine ends on the LAST stashed step's solution. */
async function stashOptimizedSteps(
  set: SetState,
  get: () => AppState,
  out: OptimizeOutput
): Promise<ResultEntry[]> {
  const st = get();
  const steps = st.loadSteps;
  const sm = out.summary;
  const meanPct = Math.round(sm.meanInfill * 100);
  const modeLabel = sm.solid ? "Part Topo" : sm.binary ? "binary" : "graded";
  const goalNote = sm.goal === "match" ? " · match" : "";
  const ep = { ...st.resultEpochs };
  const entries: ResultEntry[] = [];
  for (let i = 0; i < steps.length; i++) {
    const step = steps[i];
    set({ busy: `Evaluating optimized design · ${step.name} (${i + 1}/${steps.length})…` });
    await engine.setBcs(effectiveBcs(st.bcs, step));
    // A step under-constrained in isolation can't be evaluated — skip it rather
    // than discarding the whole optimized roster.
    const report = await engine.check();
    if (!report.ok) {
      appendLog(set, `  ${step.name}: under-constrained on its own — left out of the optimized roster`);
      continue;
    }
    let stats: SolveStats;
    try {
      const r = await engine.solveOptimized();
      stats = r.stats;
    } catch (e) {
      appendLog(set, `  ${step.name}: ${e instanceof Error ? e.message : String(e)} — skipped`);
      continue;
    }
    const rid = resultStashId("optimized", step.id, false);
    await engine.stashResult(rid);
    const cur = get();
    const rows: [string, ProvVal][] = [
      ["Load step", step.name],
      ["Mode", modeLabel + goalNote],
      [sm.solid ? "Retained vol" : "Mean infill", `${meanPct}%`],
    ];
    if (!sm.solid) rows.push(["Pattern", st.pattern]);
    rows.push(
      ["Material", cur.material.name],
      ["Mesh", meshLabel(cur)],
      ["Mass", fmtMass(sm.massGrams)],
      ["Max |u|", fmtMm(stats.maxDisplacement)]
    );
    entries.push({
      id: rid,
      kind: "optimized",
      loadStepId: step.id,
      loadStepName: step.name,
      label: `Optimized · ${modeLabel} ${meanPct}%`,
      maxDisplacement: stats.maxDisplacement,
      massGrams: sm.massGrams,
      minSf: null,
      converged: stats.converged,
      provTitle: `Optimized ${sm.solid ? "shape" : "infill"} · ${step.name}`,
      provRows: rows,
      epochs: ep,
    });
    appendLog(
      set,
      `  ${step.name}: max |u| ${stats.maxDisplacement.toExponential(2)} mm` +
        (stats.converged ? "" : " — UNCONVERGED")
    );
  }
  return entries;
}

/** Single-step (or single-load) optimize result roster — byte-identical to the
 *  pre-load-step model and old `.filasim` files: one bare `optimized` stash plus
 *  the equal-mass uniform + solid baselines (infill modes), tagged with the sole
 *  (active) load step. */
async function stashOptimizedSingle(set: SetState, get: () => AppState, out: OptimizeOutput) {
  await engine.stashResult("optimized");
  const cur = get();
  const sm = out.summary;
  const meanPct = Math.round(sm.meanInfill * 100);
  const modeLabel = sm.solid ? "Part Topo" : sm.binary ? "binary" : "graded";
  const goalNote = sm.goal === "match" ? " · match" : "";
  const ep = { ...cur.resultEpochs };
  const optStepId = cur.activeLoadStepId;
  const optStepName = activeStep(cur)?.name ?? "Load step";
  const optRows: [string, ProvVal][] = [
    ["Mode", modeLabel + goalNote],
    [sm.solid ? "Retained vol" : "Mean infill", `${meanPct}%`],
  ];
  if (!sm.solid) optRows.push(["Pattern", cur.pattern]);
  optRows.push(
    ["Material", cur.material.name],
    ["Mesh", meshLabel(cur)],
    ["Mass", fmtMass(sm.massGrams)],
    ["Max |u|", fmtMm(sm.maxDisplacement)],
    // Same compliance-ratio calc for both; vs solid comes out negative
    // (the optimized design is softer than fully dense material).
    ["vs solid", signedPct(sm.stiffnessVsSolid - 1)],
    ["vs uniform", signedPct(sm.gainVsUniform)]
  );
  const next: ResultEntry[] = get().results.filter((r) => r.kind === "asprinted");
  next.push({
    id: "optimized",
    kind: "optimized",
    loadStepId: optStepId,
    loadStepName: optStepName,
    label: `Optimized · ${modeLabel} ${meanPct}%`,
    maxDisplacement: sm.maxDisplacement,
    massGrams: sm.massGrams,
    minSf: null,
    converged: sm.converged,
    provTitle: sm.solid ? "Optimized shape" : "Optimized infill",
    provRows: optRows,
    epochs: ep,
  });
  if (sm.hasBaselines) {
    next.push({
      id: "uniform",
      kind: "uniform",
      loadStepId: optStepId,
      loadStepName: optStepName,
      label: `Uniform · equal mass ${meanPct}%`,
      maxDisplacement: sm.uniformMaxDisp ?? sm.maxDisplacement,
      massGrams: sm.massGrams,
      minSf: null,
      converged: true,
      provTitle: "Uniform · equal mass",
      provRows: [
        ["Infill", `${meanPct}% (even)`],
        ["Pattern", cur.pattern],
        ["Material", cur.material.name],
        ["Mesh", meshLabel(cur)],
        ["Mass", fmtMass(sm.massGrams)],
        ["Max |u|", fmtMm(sm.uniformMaxDisp ?? sm.maxDisplacement)],
      ],
      epochs: ep,
    });
    next.push({
      id: "solid",
      kind: "solid",
      loadStepId: optStepId,
      loadStepName: optStepName,
      label: "Solid material",
      maxDisplacement: sm.solidMaxDisp ?? 0,
      massGrams: sm.massSolidGrams,
      minSf: null,
      converged: true,
      provTitle: "Solid material",
      provRows: [
        ["Model", "fully dense E₀"],
        ["Material", cur.material.name],
        ["Mesh", meshLabel(cur)],
        ["Mass", fmtMass(sm.massSolidGrams)],
        ["Max |u|", fmtMm(sm.solidMaxDisp ?? 0)],
      ],
      epochs: ep,
    });
  }
  set({ results: sortResults(next, cur.loadSteps), activeResultId: "optimized" });
}

/** Multi-step optimize result roster (DESIGN §13): the one optimized design
 *  evaluated under EVERY load step (`optimized::stepId`) + a worst-case
 *  envelope, so the viewer's step selector offers them all. The equal-mass
 *  uniform + solid baselines were solved by the optimizer under the PRIMARY
 *  (first included) load only — they're tagged with that step (single-step
 *  kinds, no envelope). Displays the active step's optimized result. */
async function stashOptimizedMultiStep(set: SetState, get: () => AppState, out: OptimizeOutput) {
  const optEntries = await stashOptimizedSteps(set, get, out);
  const cur = get();
  const sm = out.summary;
  const meanPct = Math.round(sm.meanInfill * 100);
  const ep = optEntries[0]?.epochs ?? { ...cur.resultEpochs };
  // The optimizer's primary case = the first INCLUDED step (its baselines belong
  // to that load). Fall back defensively if includes were cleared post-run.
  const included = cur.loadSteps.filter((s) => s.includeInOptimize);
  const primary = included[0] ?? activeStep(cur) ?? cur.loadSteps[0];
  const baseEntries: ResultEntry[] = [];
  if (sm.hasBaselines) {
    baseEntries.push({
      id: "uniform",
      kind: "uniform",
      loadStepId: primary.id,
      loadStepName: primary.name,
      label: `Uniform · equal mass ${meanPct}%`,
      maxDisplacement: sm.uniformMaxDisp ?? sm.maxDisplacement,
      massGrams: sm.massGrams,
      minSf: null,
      converged: true,
      provTitle: "Uniform · equal mass",
      provRows: [
        ["Infill", `${meanPct}% (even)`],
        ["Pattern", cur.pattern],
        ["Material", cur.material.name],
        ["Mesh", meshLabel(cur)],
        ["Mass", fmtMass(sm.massGrams)],
        ["Max |u|", fmtMm(sm.uniformMaxDisp ?? sm.maxDisplacement)],
        ["Load case", primary.name],
      ],
      epochs: ep,
    });
    baseEntries.push({
      id: "solid",
      kind: "solid",
      loadStepId: primary.id,
      loadStepName: primary.name,
      label: "Solid material",
      maxDisplacement: sm.solidMaxDisp ?? 0,
      massGrams: sm.massSolidGrams,
      minSf: null,
      converged: true,
      provTitle: "Solid material",
      provRows: [
        ["Model", "fully dense E₀"],
        ["Material", cur.material.name],
        ["Mesh", meshLabel(cur)],
        ["Mass", fmtMass(sm.massSolidGrams)],
        ["Max |u|", fmtMm(sm.solidMaxDisp ?? 0)],
        ["Load case", primary.name],
      ],
      epochs: ep,
    });
  }
  const kept = cur.results.filter((r) => r.kind === "asprinted");
  const roster = withEnvelope(
    sortResults([...kept, ...optEntries, ...baseEntries], cur.loadSteps),
    cur.loadSteps
  );
  // Show the ACTIVE step's optimized result (fall back to the first stashed).
  // Activating it makes its displacements the live deform buffer; the view is
  // still on "infill", so this just primes the result/deformed views.
  const activeRid = resultStashId("optimized", cur.activeLoadStepId, false);
  const showId = optEntries.some((e) => e.id === activeRid)
    ? activeRid
    : optEntries[0]?.id ?? "optimized";
  let disp = out.displacements;
  if (optEntries.length) {
    try {
      disp = await engine.activateResult(showId);
    } catch {
      // keep the optimizer's primary displacements
    }
  }
  session.invalidateSolution();
  set({ results: roster, activeResultId: showId, busy: null });
  sceneEvents.onDisplacements?.(disp, { maxDisplacement: referenceMaxDisp(roster) });
}

// ---- project (.filasim) save / load ----

const PROJECT_SCHEMA = 1;
const APP_VERSION = "0.1.0";

/** The session settings a project round-trips. The per-browser library stays
 *  separate; the project embeds the VALUES it used so it opens identically
 *  on any machine. */
function collectSettings(s: AppState) {
  return {
    segAngle: s.segAngle,
    segSource: s.segSource,
    material: s.material,
    materials: s.materials,
    curves: s.curves,
    levelSettings: s.levelSettings,
    resolution: s.resolution,
    customH: s.customH,
    pattern: s.pattern,
    perimeters: s.perimeters,
    lineWidth: s.lineWidth,
    topBottomLayers: s.topBottomLayers,
    layerHeight: s.layerHeight,
    printInfill: s.printInfill,
    snapVoxel: s.snapVoxel,
    compositeSkin: s.compositeSkin,
    smoothStress: s.smoothStress,
    materialStress: s.materialStress,
    analyzeMode: s.analyzeMode,
    buildBedTemp: s.buildBedTemp,
    buildChamberTemp: s.buildChamberTemp,
    budget: s.budget,
    smoothIters: s.smoothIters,
    nBins: s.nBins,
    minMemberMm: s.minMemberMm,
    goal: s.goal,
    optMode: s.optMode,
    retainBc: s.retainBc,
    selfSupport: s.selfSupport,
    overhangDeg: s.overhangDeg,
    symOn: s.symOn,
    symNormal: s.symNormal,
    symC: s.symC,
    solidPattern: s.solidPattern,
    exportSlicer: s.exportSlicer,
    densityThreshold: s.densityThreshold,
    resultSurface: s.resultSurface,
  };
}
type ProjectSettings = ReturnType<typeof collectSettings>;

/** On-disk load step. Overrides are keyed by the BC's INDEX in `bcs` (ids are
 *  reassigned on load), so the keys survive the re-id. See DESIGN §13. */
interface SerializedLoadStep {
  /** Stable step id (additive — absent in pre-feature files). Persisted so a
   *  saved per-step result's `kind::stepId` key still resolves after reload. */
  id?: string;
  name: string;
  includeInOptimize: boolean;
  weight: number;
  overrides: Record<number, LoadStepOverride>;
}

interface ProjectManifest {
  app: string;
  schemaVersion: number;
  appVersion: string;
  fileName: string;
  /** Cumulative orientation transform (12 numbers) to replay on the re-import. */
  transform: number[];
  settings: ProjectSettings;
  bcs: (Omit<Bc, "tris"> & { tris: number[] })[];
  /** FEA load steps (DESIGN §13). OPTIONAL & additive: absent in v1 files and
   *  files saved before this feature → the loader synthesizes a single step,
   *  so the schema version does NOT bump and old/new builds interoperate. */
  loadSteps?: SerializedLoadStep[];
  optSummary: OptSummary | null;
  regionInfos: { density: number }[];
  /** Result roster (metadata only; the buffers live in results/*.f32). Null
   *  when results were excluded from the save. */
  results: ResultEntry[] | null;
  activeResultId: string | null;
}

/** Load steps → on-disk form: re-key each override by the BC's index (ids are
 *  reassigned on load) and drop overrides for BCs that no longer exist. */
function serializeLoadSteps(bcs: Bc[], steps: LoadStep[]): SerializedLoadStep[] {
  const index = new Map(bcs.map((b, i) => [b.id, i]));
  return steps.map((s) => ({
    id: s.id,
    name: s.name,
    includeInOptimize: s.includeInOptimize,
    weight: s.weight,
    overrides: Object.fromEntries(
      Object.entries(s.overrides).flatMap(([bcId, ov]) => {
        const i = index.get(bcId);
        return i === undefined ? [] : [[i, ov] as [number, LoadStepOverride]];
      })
    ),
  }));
}

/** On-disk load steps → runtime, remapping override keys from BC index back to
 *  the freshly-assigned BC ids. Absent/empty `serialized` (v1 or pre-feature
 *  files) synthesizes a single default step over `bcs`. */
function deserializeLoadSteps(bcs: Bc[], serialized: SerializedLoadStep[] | undefined): LoadStep[] {
  if (!serialized || serialized.length === 0) return [makeLoadStep("Load step 1")];
  // Keep saved step ids verbatim (so per-step result keys still resolve), and
  // advance the counter past them so later new steps can't collide.
  for (const s of serialized) {
    const n = s.id && /^ls(\d+)$/.exec(s.id);
    if (n) stepCounter = Math.max(stepCounter, Number(n[1]));
  }
  return serialized.map((s) => ({
    id: s.id ?? `ls${++stepCounter}`,
    name: s.name,
    includeInOptimize: s.includeInOptimize ?? true,
    weight: s.weight ?? 1,
    overrides: Object.fromEntries(
      Object.entries(s.overrides ?? {}).flatMap(([idx, ov]) => {
        const id = bcs[Number(idx)]?.id;
        return id === undefined ? [] : [[id, ov] as [string, LoadStepOverride]];
      })
    ),
  }));
}

/** Model or resolution changed: the voxel grid (and its display mesh) is stale. */
function invalidateGrid(set: (p: Partial<AppState>) => void, get: () => AppState) {
  set({ voxelInfo: null, voxelMeshReady: false });
  sceneEvents.onVoxelMesh?.(null, null);
  if (get().viewMode === "mesh") {
    set({ viewMode: "setup" });
    sceneEvents.onViewState?.("setup", get().deformScale);
  }
}

function download(bytes: Uint8Array, filename: string, mime: string) {
  const blob = new Blob([bytes.slice()], { type: mime });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 5000);
}

const initialStep = makeLoadStep("Load step 1");

export const useStore = create<AppState>((set, get) => ({
  activeStep: 1,
  fileName: null,
  model: null,
  segAngle: 10,
  segSource: "angle",
  tool: "orbit",
  brushRadius: 3,
  brushErase: false,
  bcs: [],
  activeBcId: null,
  loadSteps: [initialStep],
  activeLoadStepId: initialStep.id,
  material: initialSettings.materials[0],
  materials: initialSettings.materials,
  curves: initialSettings.curves,
  resolution: "preview",
  customH: 0,
  budget: 25,
  pattern: "gyroid",
  perimeters: 2,
  lineWidth: 0.45,
  topBottomLayers: 5,
  layerHeight: 0.2,
  printInfill: 25,
  snapVoxel: true,
  compositeSkin: true,
  analyzeMode: "printed",
  analysisType: "static",
  modalModeCount: 6,
  freeFree: false,
  appMode: "optimize",
  buildState: "released",
  buildBedTemp: 60,
  buildChamberTemp: 25,
  buildProgress: null,
  buildResult: null,
  printedStats: null,
  meshDensity: false,
  strainView: false,
  strainLayer: 0,
  strainLayerMax: 0,
  strainPeakMPa: 0,
  smoothStress: true,
  layerShear: true,
  materialStress: true,
  bandedContour: false,
  bandCount: CONTOUR_BANDS,
  colorSteps: CONTOUR_BANDS,
  smoothIters: 15,
  nBins: 3,
  minMemberMm: null, // auto = 2× line width
  goal: "budget",
  symOn: false,
  symNormal: [1, 0, 0],
  symC: 0,
  optMode: "graded",
  retainBc: true,
  selfSupport: false,
  overhangDeg: 45,
  solidPattern: "rectilinear",
  levelSettings: initialSettings.levels,
  busy: null,
  error: null,
  notice: null,
  check: null,
  stats: null,
  hasResult: false,
  optProgress: null,
  optSummary: null,
  orientSweep: null,
  orientProgress: null,
  orientSel: null,
  results: [],
  activeResultId: null,
  resultEpochs: { ...ZERO_EPOCHS },
  viewMode: "setup",
  wireframe: false,
  deformScale: 1,
  animateDeformed: false,
  autoScale: 1,
  voxelInfo: null,
  voxelMeshReady: false,
  settingsOpen: false,
  loadStepsOpen: false,
  imprintOpen: false,
  supportOpen: false,
  disclaimerOpen: !disclaimerSkippedInit(),
  disclaimerSkipped: disclaimerSkippedInit(),
  unitPrefs: initialUnitPrefs,
  unitsOpen: false,
  unitRev: 0,
  importUnit: initialImportPrefs.unit,
  askImportUnit: initialImportPrefs.ask,
  pendingImport: null,
  regionInfos: [],
  regionVisible: [],
  densityThreshold: 0,
  exportSlicer: "orca",
  resultField: "u",
  resultSurface: "stl",
  fieldRange: null,
  legendMin: null,
  legendMax: null,
  showExtremes: false,
  sectionOn: false,
  logOpen: false,
  logLines: [],
  optSeries: [],
  solveResiduals: [],
  solveTol: 0,

  setActiveStep(n) {
    const maxStep = get().appMode === "buildsim" ? 4 : 6;
    set({ activeStep: Math.min(maxStep, Math.max(1, Math.round(n))) });
    // The symmetry plane is an Optimize-step editing aid — hide it elsewhere.
    pushSymmetry(get);
  },

  async loadFile(name, bytes, unitId) {
    const isStl = /\.stl$/i.test(name);
    // STL is unitless → prompt for the import unit (unless "don't ask again").
    // 3MF/STEP carry their own units, so never prompt and never rescale here.
    if (isStl && unitId === undefined && get().askImportUnit) {
      set({ pendingImport: { name, bytes } });
      return;
    }
    const useUnit = unitId ?? (isStl ? get().importUnit : "mm");
    set({ busy: "Parsing & segmenting…", error: null, notice: null });
    try {
      let model = await engine.load(bytes, name.replace(/\.(stl|3mf|step|stp)$/i, ""));
      // One-time bake to canonical mm: scale the geometry by mm-per-import-unit
      // (e.g. inch → ×25.4). After this the model is canonical; the display unit
      // never reinterprets it (units-design §8).
      const f = convertUnitToCanonical(1, "length", useUnit);
      if (Math.abs(f - 1) > 1e-9) {
        const out = await engine.transform([f, 0, 0, 0, f, 0, 0, 0, f, 0, 0, 0]);
        model = { ...model, positions: out.positions, bbox: out.bbox as LoadedModel["bbox"] };
      }
      // Land every fresh import centered on the build grid: center the XY
      // footprint over the plate origin and seat the part on the plate
      // (z-min → 0). Mirrors the auto-seat that rotate / place-on-face do.
      {
        const cx = (model.bbox[0] + model.bbox[3]) / 2;
        const cy = (model.bbox[1] + model.bbox[4]) / 2;
        const dz = model.bbox[2];
        if (Math.abs(cx) > 1e-6 || Math.abs(cy) > 1e-6 || Math.abs(dz) > 1e-6) {
          const out = await engine.transform([1, 0, 0, 0, 1, 0, 0, 0, 1, -cx, -cy, -dz]);
          model = { ...model, positions: out.positions, bbox: out.bbox as LoadedModel["bbox"] };
        }
      }
      const m = get().material;
      await engine.setMaterial(m.e0, m.nu, m.density, m.strength, m.strengthZ, m.shearStrengthZ);
      await pushResolution(get);
      // A fresh wasm Model defaults to snap off; push the current setting.
      // (Inline, not pushSnap: the store's `model` isn't set yet.)
      await engine.setSnapWall(
        get().snapVoxel ? get().perimeters * get().lineWidth : 0
      );
      await engine.setCompositeSkin(get().compositeSkin);
      await engine.setSmoothStress(get().smoothStress);
      await engine.setMaterialStress(get().materialStress);
      await engine.setLayerShear(get().layerShear);
      const freshStep = makeLoadStep("Load step 1");
      set({
        fileName: name,
        model,
        // STEP arrives already segmented by its BREP faces (Model::new default),
        // so reflect that; STL/3MF use the crease-angle slider.
        segSource: model.hasCadFaces ? "cad" : "angle",
        // Land on the Model step so the print orientation can be set first;
        // supports & loads (fresh for every model) come next.
        activeStep: 1,
        bcs: [],
        activeBcId: null,
        loadSteps: [freshStep],
        activeLoadStepId: freshStep.id,
        tool: "orbit",
        symOn: false,
        check: null,
        stats: null,
        hasResult: false,
        optSummary: null,
        results: [],
        activeResultId: null,
        resultEpochs: { ...ZERO_EPOCHS },
        optProgress: null,
        viewMode: "setup",
        voxelInfo: null,
        voxelMeshReady: false,
        autoScale: 1,
        regionInfos: [],
        regionVisible: [],
        densityThreshold: 0,
        resultField: "u",
        fieldRange: null,
        legendMin: null,
        legendMax: null,
        busy: null,
        notice:
          [
            // 3MF only: import keeps the largest mesh object (STEP keeps all
            // shells, so its shell count must not trigger this message).
            /\.3mf$/i.test(name) && model.meshObjects > 1
              ? "3MF contained multiple meshes — analyzing the largest body only."
              : null,
            model.bodyCount > 1
              ? `Part contains ${model.bodyCount} separate bodies. filaSim cannot connect separate bodies — they only fuse where gaps are smaller than the voxel size. Merge them in CAD if they should act as one part.`
              : null,
          ]
            .filter(Boolean)
            .join(" ") || null,
      });
      session.invalidateSolution();
      // Clear stale overlays BEFORE the model swap so nothing survives even
      // if a later step fails.
      sceneEvents.onScalarField?.(null);
      sceneEvents.onBcsChanged?.([], null);
      sceneEvents.onDisplacements?.(null, null);
      sceneEvents.onVertexDensity?.(null);
      sceneEvents.onRegions?.(null);
      sceneEvents.onAnimateMode?.(null);
      sceneEvents.onVoxelMesh?.(null, null);
      sceneEvents.onOptShape?.(null, null);
      sceneEvents.onModelLoaded?.(model);
      sceneEvents.onViewState?.("setup", get().deformScale);
      pushSymmetry(get); // hide a previous model's symmetry plane
      const [bx, by, bz] = [
        model.bbox[3] - model.bbox[0],
        model.bbox[4] - model.bbox[1],
        model.bbox[5] - model.bbox[2],
      ];
      appendLog(
        set,
        `Loaded "${name}" — ${model.triCount.toLocaleString()} display triangles, bbox ${bx.toFixed(1)}×${by.toFixed(1)}×${bz.toFixed(1)} mm`
      );
      if (model.bodyCount > 1) {
        appendLog(
          set,
          `  ${model.bodyCount} separate bodies — not connected in the simulation (bodies only fuse across sub-voxel gaps)`
        );
      }
    } catch (e) {
      set({ busy: null, error: e instanceof Error ? e.message : String(e) });
    }
  },

  confirmImport(unitId, remember) {
    const p = get().pendingImport;
    const ask = remember ? false : get().askImportUnit;
    set({ pendingImport: null, importUnit: unitId, askImportUnit: ask });
    saveImportPrefs(unitId, ask);
    if (p) void get().loadFile(p.name, p.bytes, unitId);
  },

  cancelImport() {
    set({ pendingImport: null });
  },

  setAskImportUnit(on) {
    set({ askImportUnit: on });
    saveImportPrefs(get().importUnit, on);
  },

  async rescaleModel(factor) {
    if (!get().model || get().busy || !(factor > 0) || Math.abs(factor - 1) < 1e-9) return;
    // Pure uniform scale about the bbox center, then re-seat on the plate.
    await transformModel(set, get, [factor, 0, 0, 0, factor, 0, 0, 0, factor]);
  },

  async setSegAngle(angle) {
    // Adjusting the crease angle implies the dihedral source.
    set({ segAngle: angle, segSource: "angle" });
    if (!get().model) return;
    set({ busy: "Re-segmenting…" });
    try {
      const { patchIds, patchCount } = await engine.resegment(angle);
      const model = get().model!;
      set({ model: { ...model, patchIds, patchCount }, busy: null });
      sceneEvents.onPatchIdsChanged?.(patchIds);
    } catch (e) {
      set({ busy: null, error: e instanceof Error ? e.message : String(e) });
    }
  },

  async setSegSource(src) {
    set({ segSource: src });
    if (!get().model) return;
    set({ busy: src === "cad" ? "Loading CAD faces…" : "Re-segmenting…" });
    try {
      const { patchIds, patchCount } =
        src === "cad" ? await engine.useCadFaces() : await engine.resegment(get().segAngle);
      const model = get().model!;
      set({ model: { ...model, patchIds, patchCount }, busy: null });
      sceneEvents.onPatchIdsChanged?.(patchIds);
    } catch (e) {
      set({ busy: null, error: e instanceof Error ? e.message : String(e) });
    }
  },

  setTool(tool) {
    set({ tool });
  },

  async rotateModel(axis) {
    // +90° about the world axis, applied about the part's bbox center.
    const R: Record<"x" | "y" | "z", number[]> = {
      x: [1, 0, 0, 0, 0, -1, 0, 1, 0],
      y: [0, 0, 1, 0, 1, 0, -1, 0, 0],
      z: [0, -1, 0, 1, 0, 0, 0, 0, 1],
    };
    appendLog(set, `Rotate +90° about ${axis.toUpperCase()} — results reset`);
    await transformModel(set, get, R[axis]);
  },

  async applyPlaceOnFace(normal) {
    set({ tool: "orbit" }); // disarm: one click places
    // Rotation taking the clicked face's outward normal to −Z (build plate).
    const [ax, ay, az] = normal;
    const dot = -az; // n · (0,0,−1)
    let R: number[];
    if (dot > 1 - 1e-9) {
      R = [1, 0, 0, 0, 1, 0, 0, 0, 1]; // already facing down — just seat
    } else if (dot < -1 + 1e-9) {
      R = [1, 0, 0, 0, -1, 0, 0, 0, -1]; // 180° about X
    } else {
      // Rodrigues about k = n × (0,0,−1) (normalized), angle = acos(dot).
      let kx = -ay;
      let ky = ax;
      const kz = 0;
      const kl = Math.hypot(kx, ky) || 1;
      kx /= kl;
      ky /= kl;
      const c = dot;
      const s = Math.sqrt(Math.max(0, 1 - c * c));
      const t = 1 - c;
      R = [
        c + kx * kx * t, kx * ky * t - kz * s, kx * kz * t + ky * s,
        ky * kx * t + kz * s, c + ky * ky * t, ky * kz * t - kx * s,
        kz * kx * t - ky * s, kz * ky * t + kx * s, c + kz * kz * t,
      ];
    }
    appendLog(set, "Place on face: clicked surface becomes the build plate (Z−) — results reset");
    await transformModel(set, get, R);
  },

  toggleSymmetry() {
    const on = !get().symOn;
    set({ symOn: on });
    markResultsStale(set, get, "opt");
    if (on) get().centerSymmetry();
    else pushSymmetry(get);
  },
  setSymAxis(axis) {
    set({
      symNormal: [axis === "x" ? 1 : 0, axis === "y" ? 1 : 0, axis === "z" ? 1 : 0],
    });
    markResultsStale(set, get, "opt");
    get().centerSymmetry(); // re-anchor: c along the old normal is meaningless
  },
  centerSymmetry() {
    const m = get().model;
    if (m) {
      const b = m.bbox;
      const n = get().symNormal;
      const c =
        n[0] * ((b[0] + b[3]) / 2) + n[1] * ((b[1] + b[4]) / 2) + n[2] * ((b[2] + b[5]) / 2);
      set({ symC: Math.round(c * 100) / 100 });
    }
    // Editing the plane from a result/mesh view: jump back to the setup
    // view, where the plane is actually shown and editable.
    if (get().symOn && get().viewMode !== "setup") void get().setViewMode("setup");
    pushSymmetry(get);
  },
  onSymmetryPlaneMoved(normal, c) {
    // The scene already moved the plane — just mirror the values.
    set({ symNormal: normal, symC: Math.round(c * 100) / 100 });
    markResultsStale(set, get, "opt");
  },

  setBrushRadius(r) {
    set({ brushRadius: r });
  },
  setBrushErase(on) {
    set({ brushErase: on });
  },

  addBc(kind) {
    // Auto-name "Force 1", "Force 2", … per kind so steps/tables read clearly.
    const nOfKind = get().bcs.filter((b) => b.kind === kind).length;
    const bc: Bc = {
      id: `bc${++bcCounter}`,
      kind,
      name: `${BC_KIND_NAME[kind]} ${nOfKind + 1}`,
      tris: new Uint32Array(0),
      // Bearing reuses the force fields (its push vector); a force load and a
      // bearing load both default to a 10 N downward load.
      force: kind === "force" || kind === "bearing" ? [0, 0, -10] : undefined,
      pressure: kind === "pressure" ? 0.1 : undefined,
      // ~printed-plastic mount; bolted-to-steel would be >= 5000 (≈ fixed).
      stiffness: kind === "elastic" ? 100 : undefined,
      // Displacement support: roller locking the vertical (Z) axis by default,
      // prescribed value 0 (a classic pin-to-zero until the user sets a value).
      axes: kind === "displacement" ? [false, false, true] : undefined,
      disp: kind === "displacement" ? [0, 0, 0] : undefined,
      // Force: default to DIRECTION mode — the direction auto-tracks the
      // selection's average normal (forceDirAuto) and the magnitude is 10 N,
      // which is what most users want (push/pull on a face). Switch to
      // components to edit Fx/Fy/Fz directly.
      forceMode: kind === "force" ? "direction" : kind === "bearing" ? "direction" : undefined,
      forceDir: kind === "force" || kind === "bearing" ? [0, 0, -1] : undefined,
      forceMag: kind === "force" || kind === "bearing" ? 10 : undefined,
      // Force auto-tracks the surface normal; a bearing push direction is set by
      // the user (which way the pin presses), so it does NOT auto-track.
      forceDirAuto: kind === "force" ? true : kind === "bearing" ? false : undefined,
      cyl: kind === "bearing" ? null : undefined,
      // Moment: default to a 100 N·mm couple about +Z, components mode.
      moment: kind === "moment" ? [0, 0, 100] : undefined,
      momentMode: kind === "moment" ? "components" : undefined,
      momentDir: kind === "moment" ? [0, 0, 1] : undefined,
      momentMag: kind === "moment" ? 100 : undefined,
    };
    set({ bcs: [...get().bcs, bc], activeBcId: bc.id, tool: "select" });
    markResultsStale(set, get, "loads");
    pushBcGlyphs(get, bc.id);
  },

  removeBc(id) {
    set({
      bcs: get().bcs.filter((b) => b.id !== id),
      activeBcId: get().activeBcId === id ? null : get().activeBcId,
      // Drop any per-step overrides that referenced the removed BC.
      loadSteps: get().loadSteps.map((s) =>
        id in s.overrides
          ? { ...s, overrides: Object.fromEntries(Object.entries(s.overrides).filter(([k]) => k !== id)) }
          : s
      ),
    });
    if (get().bcs.length === 0) set({ tool: "orbit" });
    markResultsStale(set, get, "loads");
    pushBcGlyphs(get);
    void pushBcs(get);
  },

  setActiveBc(id) {
    set({ activeBcId: id });
    if (id === null) set({ tool: "orbit" });
    pushBcGlyphs(get, id);
  },

  setBcName(id, name) {
    set({ bcs: get().bcs.map((b) => (b.id === id ? { ...b, name } : b)) });
  },

  updateBcTris(id, tris) {
    const positions = get().model?.positions;
    const target = get().bcs.find((b) => b.id === id);
    const prevTris = target?.tris ?? new Uint32Array(0);
    // Bearing loads must sit on a cylindrical surface. Validate synchronously
    // (instant feedback, no worker round-trip) and HARD-BLOCK a non-cylindrical
    // pick by reverting to the previous selection.
    let bearingPatch: Partial<Bc> | null = null;
    if (target?.kind === "bearing") {
      if (tris.length === 0) {
        bearingPatch = { tris, cyl: null, cylError: undefined };
      } else {
        const fit = positions ? fitCylinderFromSelection(positions, tris) : null;
        if (fit?.ok) {
          bearingPatch = { tris, cyl: fit, cylError: undefined };
        } else {
          const pct =
            fit && isFinite(fit.residual) ? ` (${(fit.residual * 100).toFixed(0)}% off-round)` : "";
          bearingPatch = {
            tris: prevTris,
            cyl: null,
            cylError: `Selection isn’t a cylinder${pct} — bearing load needs a cylindrical surface.`,
          };
        }
      }
    }
    set({
      bcs: get().bcs.map((b) => {
        if (b.id !== id) return b;
        if (bearingPatch) return { ...b, ...bearingPatch };
        const next: Bc = { ...b, tris };
        // A direction-mode force that still auto-tracks re-aims along the new
        // selection's average normal (magnitude preserved).
        if (b.kind === "force" && b.forceMode === "direction" && b.forceDirAuto !== false) {
          const n = selectionNormal(positions, tris);
          if (n) {
            next.forceDir = n;
            next.force = resolveForce(next);
          }
        }
        return next;
      }),
    });
    markResultsStale(set, get, "loads");
    pushBcGlyphs(get);
    void pushBcs(get);
  },

  updateBcParams(id, params) {
    set({ bcs: get().bcs.map((b) => (b.id === id ? { ...b, ...params } : b)) });
    markResultsStale(set, get, "loads");
    pushBcGlyphs(get);
    void pushBcs(get);
  },

  toggleBcAxis(id, axis) {
    const bc = get().bcs.find((b) => b.id === id);
    if (!bc) return;
    const axes: [boolean, boolean, boolean] = [...(bc.axes ?? [false, false, false])];
    axes[axis] = !axes[axis];
    get().updateBcParams(id, { axes });
  },

  setForceMode(id, mode) {
    const bc = get().bcs.find((b) => b.id === id);
    if (!bc) return;
    if (mode === "components") {
      // Keep the resolved vector — the components editor picks it straight up.
      get().updateBcParams(id, { forceMode: "components", force: resolveForce(bc) });
      return;
    }
    // Direction mode: default the direction to the selection's average normal
    // while it still auto-tracks; magnitude carries over from the components.
    const auto = bc.forceDirAuto !== false;
    let dir: [number, number, number] = bc.forceDir ?? [0, 0, -1];
    if (auto) {
      const n = selectionNormal(get().model?.positions, bc.tris);
      if (n) dir = n;
    }
    const mag = forceMagFor(bc);
    const next: Bc = { ...bc, forceMode: "direction", forceDir: dir, forceMag: mag, forceDirAuto: auto };
    get().updateBcParams(id, {
      forceMode: "direction",
      forceDir: dir,
      forceMag: mag,
      forceDirAuto: auto,
      force: resolveForce(next),
    });
  },

  setForceMag(id, mag) {
    const bc = get().bcs.find((b) => b.id === id);
    if (!bc) return;
    const next: Bc = { ...bc, forceMag: mag };
    get().updateBcParams(id, { forceMag: mag, force: resolveForce(next) });
  },

  setForceDir(id, dir) {
    const bc = get().bcs.find((b) => b.id === id);
    if (!bc) return;
    const len = Math.hypot(dir[0], dir[1], dir[2]);
    if (len < 1e-12) return;
    const unit: [number, number, number] = [dir[0] / len, dir[1] / len, dir[2] / len];
    const mag = forceMagFor(bc);
    const next: Bc = { ...bc, forceDir: unit, forceMag: mag };
    get().updateBcParams(id, {
      forceDir: unit,
      forceMag: mag,
      forceDirAuto: false,
      force: resolveForce(next),
    });
  },

  flipForceDir(id) {
    const bc = get().bcs.find((b) => b.id === id);
    if (!bc) return;
    // Direction mode: reverse the unit direction. Components mode: negate the
    // whole vector (so the button works either way).
    if (bc.forceMode === "direction") {
      const d = bc.forceDir ?? [0, 0, -1];
      const flipped: [number, number, number] = [-d[0], -d[1], -d[2]];
      const next: Bc = { ...bc, forceDir: flipped };
      get().updateBcParams(id, { forceDir: flipped, forceDirAuto: false, force: resolveForce(next) });
    } else {
      const f = bc.force ?? [0, 0, 0];
      get().updateBcParams(id, { force: [-f[0], -f[1], -f[2]] });
    }
  },

  resetForceDirToNormal(id) {
    const bc = get().bcs.find((b) => b.id === id);
    if (!bc) return;
    const n = selectionNormal(get().model?.positions, bc.tris);
    if (!n) return;
    const next: Bc = { ...bc, forceDir: n };
    get().updateBcParams(id, { forceDir: n, forceDirAuto: true, force: resolveForce(next) });
  },

  setMomentDir(id, dir) {
    const bc = get().bcs.find((b) => b.id === id);
    if (!bc) return;
    const l = Math.hypot(dir[0], dir[1], dir[2]);
    if (l < 1e-12) return;
    const unit: [number, number, number] = [dir[0] / l, dir[1] / l, dir[2] / l];
    const mag = bc.momentMag ?? Math.hypot(...(bc.moment ?? [0, 0, 0])) ?? 0;
    get().updateBcParams(id, {
      momentMode: "direction",
      momentDir: unit,
      momentMag: mag,
      moment: [unit[0] * mag, unit[1] * mag, unit[2] * mag],
    });
  },

  flipMomentDir(id) {
    const bc = get().bcs.find((b) => b.id === id);
    if (!bc) return;
    const m = bc.moment ?? [0, 0, 0];
    const d = bc.momentDir ?? [0, 0, 1];
    get().updateBcParams(id, {
      moment: [-m[0], -m[1], -m[2]],
      momentDir: [-d[0], -d[1], -d[2]],
    });
  },

  resetMomentDirToNormal(id) {
    const bc = get().bcs.find((b) => b.id === id);
    if (!bc) return;
    const n = selectionNormal(get().model?.positions, bc.tris);
    if (!n) return;
    get().setMomentDir(id, n);
  },

  applyPickedDir(normal) {
    const id = get().activeBcId;
    if (!id) return;
    const bc = get().bcs.find((b) => b.id === id);
    if (!bc) return;
    if (bc.kind === "moment") {
      // Multi-step: aim THIS step's moment axis; single-step edits the base BC.
      const step = get().loadSteps.length > 1 ? activeStep(get()) : undefined;
      if (step) {
        const cur = step.overrides[id]?.moment ?? bc.moment ?? [0, 0, 0];
        const mag = Math.hypot(cur[0], cur[1], cur[2]) || bc.momentMag || 100;
        get().setStepMoment(step.id, id, [normal[0] * mag, normal[1] * mag, normal[2] * mag]);
      } else {
        get().setMomentDir(id, normal);
      }
      return;
    }
    if (bc.kind !== "force" && bc.kind !== "bearing") return;
    // Multi-step: aim THIS step's force vector along the clicked face (keep its
    // magnitude); single-step edits the base BC's direction.
    const step = get().loadSteps.length > 1 ? activeStep(get()) : undefined;
    if (step) {
      const cur = step.overrides[id]?.force ?? bc.force ?? [0, 0, 0];
      const mag = Math.hypot(cur[0], cur[1], cur[2]) || forceMagFor(bc);
      get().setStepForce(step.id, id, [normal[0] * mag, normal[1] * mag, normal[2] * mag]);
      return;
    }
    if (bc.forceMode !== "direction") get().setForceMode(id, "direction");
    get().setForceDir(id, normal);
  },

  // ---- load steps (FEA load cases) — see DESIGN §13 ----
  // The active step's effective BCs drive the solve (pushBcs reads it), so
  // selecting a step or editing its overrides re-syncs the engine + the live
  // RBM check and stales any standing result. Edits to a non-active step only
  // mutate state. The batch "solve every step" pass + per-step result roster
  // land in the next milestone; here Solve runs whichever step is active.
  addLoadStep() {
    const steps = get().loadSteps;
    const step = makeLoadStep(`Load step ${steps.length + 1}`);
    // Seed the new step from the one you were on (values + on/off), so it starts
    // like its predecessor instead of resetting to base — tweak from there.
    const src = steps.find((s) => s.id === get().activeLoadStepId) ?? steps[steps.length - 1];
    if (src) {
      step.overrides = cloneOverrides(src.overrides);
      step.includeInOptimize = src.includeInOptimize;
      step.weight = src.weight;
    }
    set({ loadSteps: [...steps, step], activeLoadStepId: step.id });
    void pushBcs(get);
    pushBcGlyphs(get);
  },

  removeLoadStep(id) {
    const steps = get().loadSteps;
    if (steps.length <= 1) return; // always keep at least one step
    const next = steps.filter((s) => s.id !== id);
    const switched = get().activeLoadStepId === id;
    const active = switched ? next[0].id : get().activeLoadStepId;
    set({ loadSteps: next, activeLoadStepId: active });
    if (switched) {
      markResultsStale(set, get, "loads");
      void pushBcs(get);
      pushBcGlyphs(get);
    }
  },

  renameLoadStep(id, name) {
    set({ loadSteps: get().loadSteps.map((s) => (s.id === id ? { ...s, name } : s)) });
  },

  setActiveLoadStep(id) {
    if (!get().loadSteps.some((s) => s.id === id) || id === get().activeLoadStepId) return;
    set({ activeLoadStepId: id });
    markResultsStale(set, get, "loads");
    void pushBcs(get);
    pushBcGlyphs(get);
  },

  setStepBcActive(stepId, bcId, active) {
    set({ loadSteps: patchStepOverride(get().loadSteps, stepId, bcId, { active }) });
    syncIfActiveStep(set, get, stepId);
  },

  setStepForce(stepId, bcId, force) {
    set({ loadSteps: patchStepOverride(get().loadSteps, stepId, bcId, { force }) });
    syncIfActiveStep(set, get, stepId);
  },

  aimStepForceAlongNormal(stepId, bcId) {
    const bc = get().bcs.find((b) => b.id === bcId);
    if (!bc) return;
    const n = selectionNormal(get().model?.positions, bc.tris);
    if (!n) return;
    const cur = get().loadSteps.find((s) => s.id === stepId)?.overrides[bcId]?.force ?? bc.force ?? [0, 0, 0];
    const mag = Math.hypot(cur[0], cur[1], cur[2]) || forceMagFor(bc);
    get().setStepForce(stepId, bcId, [n[0] * mag, n[1] * mag, n[2] * mag]);
  },

  setStepPressure(stepId, bcId, pressure) {
    set({ loadSteps: patchStepOverride(get().loadSteps, stepId, bcId, { pressure }) });
    syncIfActiveStep(set, get, stepId);
  },

  setStepMoment(stepId, bcId, moment) {
    set({ loadSteps: patchStepOverride(get().loadSteps, stepId, bcId, { moment }) });
    syncIfActiveStep(set, get, stepId);
  },

  aimStepMomentAlongNormal(stepId, bcId) {
    const bc = get().bcs.find((b) => b.id === bcId);
    if (!bc) return;
    const n = selectionNormal(get().model?.positions, bc.tris);
    if (!n) return;
    const cur =
      get().loadSteps.find((s) => s.id === stepId)?.overrides[bcId]?.moment ?? bc.moment ?? [0, 0, 0];
    const mag = Math.hypot(cur[0], cur[1], cur[2]) || bc.momentMag || 100;
    get().setStepMoment(stepId, bcId, [n[0] * mag, n[1] * mag, n[2] * mag]);
  },

  setStepIncludeOptimize(stepId, include) {
    set({
      loadSteps: get().loadSteps.map((s) => (s.id === stepId ? { ...s, includeInOptimize: include } : s)),
    });
  },

  setStepWeight(stepId, weight) {
    set({ loadSteps: get().loadSteps.map((s) => (s.id === stepId ? { ...s, weight } : s)) });
  },

  setMaterial(m) {
    set({ material: m });
    markResultsStale(set, get, "material");
    void engine.setMaterial(m.e0, m.nu, m.density, m.strength, m.strengthZ, m.shearStrengthZ);
  },

  updateMaterial(index, m) {
    const mats = get().materials.slice();
    const wasSelected = mats[index]?.name === get().material.name;
    mats[index] = m;
    set({ materials: mats });
    saveSettings(mats, get().curves, get().levelSettings);
    if (wasSelected) {
      set({ material: m });
      markResultsStale(set, get, "material");
      void engine.setMaterial(m.e0, m.nu, m.density, m.strength, m.strengthZ, m.shearStrengthZ);
    }
  },

  addMaterial() {
    const mats = [
      ...get().materials,
      { name: "Custom", e0: 2000, nu: 0.35, density: 1.2, strength: 40, strengthZ: 28, shrink: 0.005, shrinkZ: 0.0025, yieldStrength: 36 },
    ];
    set({ materials: mats });
    saveSettings(mats, get().curves, get().levelSettings);
  },

  removeMaterial(index) {
    const mats = get().materials.filter((_, i) => i !== index);
    if (!mats.length) return;
    const removedSelected = get().materials[index]?.name === get().material.name;
    set({ materials: mats });
    saveSettings(mats, get().curves, get().levelSettings);
    if (removedSelected) get().setMaterial(mats[0]);
  },

  resetMaterials() {
    const mats = DEFAULT_MATERIALS.map((m) => ({ ...m }));
    set({ materials: mats });
    saveSettings(mats, get().curves, get().levelSettings);
    const sel = mats.find((m) => m.name === get().material.name) ?? mats[0];
    get().setMaterial(sel);
  },

  setCurve(pattern, c) {
    const curves = { ...get().curves, [pattern]: c };
    set({ curves });
    saveSettings(get().materials, curves, get().levelSettings);
  },

  resetCurves() {
    const curves = {
      gyroid: { ...DEFAULT_CURVES.gyroid },
      cubic: { ...DEFAULT_CURVES.cubic },
      grid: { ...DEFAULT_CURVES.grid },
    };
    set({ curves });
    saveSettings(get().materials, curves, get().levelSettings);
  },

  openSettings(open) {
    set({ settingsOpen: open });
  },

  openLoadSteps(open) {
    set({ loadStepsOpen: open });
  },

  openImprint(open) {
    set({ imprintOpen: open });
  },

  maybeShowSupport() {
    if (!supportSuppressed()) set({ supportOpen: true });
  },

  closeSupport(dontShowAgain) {
    set({ supportOpen: false });
    if (dontShowAgain) {
      try {
        localStorage.setItem(SUPPORT_SUPPRESS_KEY, String(Date.now() + SUPPORT_SUPPRESS_MS));
      } catch {
        // storage blocked: the decision just won't persist
      }
    }
  },

  setResolution(r) {
    set({ resolution: r });
    if (r === "custom" && get().customH <= 0) {
      // Seed the cell size from the current grid (or the Normal preset).
      const s = get();
      const h =
        s.voxelInfo?.h ??
        (bboxVolume(s) > 0 ? Math.cbrt(bboxVolume(s) / RESOLUTIONS.normal) : 1);
      set({ customH: Math.round(h * 100) / 100 });
    }
    invalidateResults(set, get);
    invalidateGrid(set, get);
    void pushResolution(get);
  },

  setCustomH(v) {
    set({ customH: Math.min(20, Math.max(0.05, v)) });
    if (get().resolution === "custom") {
      invalidateResults(set, get);
      invalidateGrid(set, get);
      void pushResolution(get);
    }
  },

  setBudget(v) {
    // Infill budget: mean interior density, bounded by the printable band
    // of the active mode (graded: floor..cap; binary: binary floor..90).
    const [lo, hi] = budgetBounds(get());
    set({ budget: Math.min(hi, Math.max(lo, Math.round(v))) });
    markResultsStale(set, get, "opt");
  },
  setPattern(p) {
    // The pattern law feeds the next solve/optimize; both the as-printed and
    // the optimized results were built with the old curve.
    set({ pattern: p, printedStats: null });
    markResultsStale(set, get, "print", "opt");
  },
  setPerimeters(v) {
    set({ perimeters: Math.min(8, Math.max(1, Math.round(v))), printedStats: null });
    if (get().snapVoxel) {
      // The wall changed: with snapping on the engine rebuilds the grid.
      invalidateResults(set, get);
      invalidateGrid(set, get);
    } else {
      // No snap: the grid survives, but the skin band differs — the as-printed
      // and optimized results no longer match the wall.
      markResultsStale(set, get, "print", "opt");
    }
    void pushSnap(get);
  },
  setLineWidth(v) {
    set({ lineWidth: Math.min(1.5, Math.max(0.1, v)), printedStats: null });
    if (get().snapVoxel) {
      invalidateResults(set, get);
      invalidateGrid(set, get);
    } else {
      markResultsStale(set, get, "print", "opt");
    }
    void pushSnap(get);
  },
  setTopBottomLayers(v) {
    set({ topBottomLayers: Math.min(20, Math.max(0, Math.round(v))), printedStats: null });
    markResultsStale(set, get, "print", "opt");
    if (get().viewMode === "mesh") void refreshMeshView(set, get);
  },
  setLayerHeight(v) {
    set({ layerHeight: Math.min(0.6, Math.max(0.04, v)), printedStats: null });
    markResultsStale(set, get, "print", "opt");
    if (get().viewMode === "mesh") void refreshMeshView(set, get);
  },
  setPrintInfill(v) {
    const pct = Math.min(100, Math.max(5, Math.round(v)));
    set({ printInfill: pct, printedStats: null });
    // Print infill is an AS-PRINTED knob — stale only that baseline.
    markResultsStale(set, get, "print");
    // "Here's your print today — now beat it": the optimizer's budget follows
    // the print setting (clamped to its band). Update it inline, NOT via
    // setBudget, so the budget-follow doesn't also stale the optimized result.
    const [blo, bhi] = budgetBounds(get());
    set({ budget: Math.min(bhi, Math.max(blo, pct)) });
    // The mesh view's element-density colors follow the infill setting —
    // debounce the rebuild while the slider drags.
    if (get().viewMode === "mesh") {
      if (meshCutTimer) clearTimeout(meshCutTimer);
      meshCutTimer = setTimeout(() => {
        void refreshMeshView(set, get);
      }, 200);
    }
  },
  setSnapVoxel(on) {
    set({ snapVoxel: on });
    // The engine drops grid + results when the snap value actually changes.
    invalidateResults(set, get);
    invalidateGrid(set, get);
    void pushSnap(get);
  },
  setCompositeSkin(on) {
    set({ compositeSkin: on, printedStats: null });
    // The grid survives (classification is per-solve); results and the
    // mesh-view skin tint don't.
    invalidateResults(set, get);
    invalidateGrid(set, get);
    if (get().model) void engine.setCompositeSkin(on);
  },
  setAppMode(m) {
    if (m === get().appMode) return;
    // Results are workspace-specific: clear the live result + deformed view so
    // Build Sim's warp can't show (or mark Verify "done") in the other workspace.
    clearLiveResultView(set, get);
    // Build Sim has a 4-station rail; clamp the carriage when switching in.
    const maxStep = m === "buildsim" ? 4 : 6;
    // The inherent-strain mesh view is Build-Sim only.
    if (m !== "buildsim" && get().strainView) {
      set({ strainView: false });
      sceneEvents.onMeshFieldColor?.(false);
    }
    set({ appMode: m, activeStep: Math.min(get().activeStep, maxStep) });
  },
  async setBuildState(s) {
    const prev = get().buildState;
    set({ buildState: s });
    // Before a run (or in the other workspace) the toggle just picks which state
    // the NEXT run shows. When a build result is on screen, flip it live: both
    // states are cached in the engine, so this is a re-map, not a re-solve.
    if (prev === s || get().busy || get().appMode !== "buildsim" || !get().buildResult) return;
    try {
      const out = await engine.setBuildState(s);
      const maxDisp = out.stats.maxDisplacement;
      // Hold the CURRENTLY SHOWN geometric exaggeration across the flip (the
      // two states have different max |u|, so the scene re-anchors autoScale =
      // 0.08·diag/maxDisp below): shown = autoScale·deformScale, so set
      // deformScale = shown/autoScale(new). This also preserves a factor the
      // user typed into the legend.
      const shown = get().autoScale * get().deformScale;
      const bb = get().model?.bbox;
      let deformScale = get().deformScale;
      if (bb && Number.isFinite(shown)) {
        const diag = Math.hypot(bb[3] - bb[0], bb[4] - bb[1], bb[5] - bb[2]);
        const autoS = (0.08 * diag) / Math.max(maxDisp, 1e-9);
        deformScale = shown / autoS;
      }
      const cur = get().stats;
      set({
        stats: cur ? { ...cur, maxDisplacement: maxDisp } : cur,
        deformScale,
      });
      sceneEvents.onDisplacements?.(out.displacements, {
        // This state's OWN anchor — the roster reference would silently change
        // the shown factor between on-bed and released.
        maxDisplacement: maxDisp,
      });
      sceneEvents.onViewState?.("deformed", deformScale);
      // Stress fields are state-dependent (residual stress differs on bed vs
      // released) and cached per solution — drop the cache and repaint the
      // active field for the new state. (|u| repaints from the new disp buffer.)
      session.clearFields();
      await pushScalarField(set, get);
      appendLog(
        set,
        `Build state → ${s === "released" ? "released (off bed)" : "on bed"} · max |u| ${maxDisp.toExponential(2)} mm ×${Number.isFinite(shown) ? +shown.toFixed(1) : 1}`
      );
    } catch {
      // The cached states no longer match the current grid — force a re-run.
      set({ buildResult: null });
    }
  },
  setBuildBedTemp(v) {
    set({ buildBedTemp: Math.min(200, Math.max(0, v)) });
  },
  setBuildChamberTemp(v) {
    set({ buildChamberTemp: Math.min(150, Math.max(0, v)) });
  },
  setAnalyzeMode(m) {
    set({ analyzeMode: m });
  },
  setAnalysisType(t) {
    set({ analysisType: t });
  },
  setModalModeCount(n) {
    set({ modalModeCount: Math.max(1, Math.min(20, Math.round(n))) });
  },
  setFreeFree(on) {
    set({ freeFree: on });
  },

  setMeshDensity(on) {
    set({ meshDensity: on });
    sceneEvents.onMeshDensity?.(on);
  },

  async setStrainView(on) {
    // Turning it on resets the scrubber to "all layers" (0 → resolved to nz).
    set({ strainView: on, strainLayer: on ? 0 : get().strainLayer });
    if (!on) sceneEvents.onMeshFieldColor?.(false);
    if (get().viewMode === "mesh") await refreshMeshView(set, get);
  },

  async setStrainLayer(layer) {
    set({ strainLayer: Math.max(1, Math.round(layer)) });
    if (get().viewMode === "mesh" && get().strainView) await refreshMeshView(set, get);
  },

  toggleBandedContour() {
    const on = !get().bandedContour;
    set({ bandedContour: on });
    sceneEvents.onBandedContour?.(on, get().bandCount);
  },

  setBandCount(n) {
    // Adjusting the band count (scrolling the legend) implies banded display.
    const count = Math.max(CONTOUR_BANDS_MIN, Math.min(CONTOUR_BANDS_MAX, Math.round(n)));
    if (count === get().bandCount && get().bandedContour) return;
    set({ bandCount: count, bandedContour: true });
    sceneEvents.onBandedContour?.(true, count);
  },

  setSmoothStress(on) {
    set({ smoothStress: on });
    if (!get().model) return;
    void (async () => {
      await engine.setSmoothStress(on);
      // Pure post-processing: the solution stays valid — just re-fetch the
      // active field and the dock's min-SF under the new sampling.
      session.clearAllFields();
      clearEnvelopeCache(); // reduced fields were sampled at the old setting
      if (get().hasResult) {
        await pushScalarField(set, get);
        await refreshMinSf(set, get);
      }
    })();
  },

  setLayerShear(on) {
    set({ layerShear: on });
    if (!get().model) return;
    void (async () => {
      await engine.setLayerShear(on);
      // Display-side derived: sfz/sf re-derive under the toggled criterion;
      // the sweep/preview were computed with the other one — deleted via
      // clearEnvelopeCache → clearOrientationSweep.
      session.clearAllFields();
      clearEnvelopeCache();
      if (get().hasResult) {
        await pushScalarField(set, get);
        await refreshMinSf(set, get);
      }
    })();
  },

  setMaterialStress(on) {
    set({ materialStress: on });
    if (!get().model) return;
    void (async () => {
      await engine.setMaterialStress(on);
      // Pure post-processing: re-fetch the active field under the new modulus.
      // (SF is unaffected — the same factor cancels — but refresh it anyway so
      // the dock stays consistent if the user toggles mid-session.)
      session.clearAllFields();
      clearEnvelopeCache(); // reduced fields were sampled at the old setting
      if (get().hasResult) {
        await pushScalarField(set, get);
        await refreshMinSf(set, get);
      }
    })();
  },

  onSectionPlaneMoved(normal, constant) {
    lastSectionPlane = { normal, constant };
    if (get().viewMode !== "mesh" || !get().sectionOn) return;
    if (meshCutTimer) clearTimeout(meshCutTimer);
    meshCutTimer = setTimeout(() => {
      void refreshMeshView(set, get);
    }, 140);
  },
  setSmoothIters(v) {
    const iters = Math.min(40, Math.max(0, Math.round(v)));
    set({ smoothIters: iters });
    // Live re-smooth of an existing result (also affects later exports).
    if (!get().optSummary) return;
    if (smoothTimer) clearTimeout(smoothTimer);
    smoothTimer = setTimeout(() => {
      void (async () => {
        try {
          const { regions } = await engine.resmoothRegions(iters);
          if (get().smoothIters !== iters || !get().optSummary) return;
          sceneEvents.onRegions?.(regions);
          sceneEvents.onRegionVisibility?.(get().regionVisible);
        } catch {
          // result vanished mid-drag: ignore
        }
      })();
    }, 160);
  },
  setNBins(v) {
    set({ nBins: v });
    markResultsStale(set, get, "opt");
  },
  setMinMemberMm(v) {
    // null = auto (2× line width); otherwise clamp to a sane printable range.
    set({ minMemberMm: v == null ? null : Math.min(10, Math.max(0, v)) });
    markResultsStale(set, get, "opt");
  },

  setGoal(g) {
    set({ goal: g });
    get().setBudget(get().budget); // re-clamp to the goal's band
  },

  setOptMode(m) {
    set({ optMode: m });
    // Solid topology has no "match uniform stiffness" goal — force budget goal.
    if (m === "solid" && get().goal === "match") set({ goal: "budget" });
    get().setBudget(get().budget); // re-clamp to the mode's band
  },

  setRetainBc(on) {
    set({ retainBc: on });
    markResultsStale(set, get, "opt");
  },
  setSelfSupport(on) {
    set({ selfSupport: on });
    markResultsStale(set, get, "opt");
  },
  setOverhangDeg(deg) {
    // 0° = horizontal (no constraint) … 90° = vertical only.
    set({ overhangDeg: Math.min(90, Math.max(0, Math.round(deg))) });
    markResultsStale(set, get, "opt");
  },

  setSolidPattern(p) {
    set({ solidPattern: p });
    markResultsStale(set, get, "opt");
  },

  updateLevelSettings(p) {
    const levels = { ...get().levelSettings, ...p };
    if (levels.capPct < levels.floorPct + 10) levels.capPct = levels.floorPct + 10;
    set({ levelSettings: levels });
    saveSettings(get().materials, get().curves, levels);
    get().setBudget(get().budget); // floor/cap moved: re-clamp
  },

  setRegionVisible(index, on) {
    const vis = get().regionVisible.slice();
    vis[index] = on;
    set({ regionVisible: vis });
    sceneEvents.onRegionVisibility?.(vis);
  },

  setDensityThreshold(v) {
    set({ densityThreshold: v });
    if (isoTimer) clearTimeout(isoTimer);
    isoTimer = setTimeout(() => {
      void (async () => {
        const o = get().optSummary;
        if (!o) return;
        // Part Topo / binary: this ONE slider IS the export isosurface density —
        // it both previews (cutaway) and re-extracts the exported geometry.
        // Graded has no single export level, so it's a display-only cutaway.
        const drivesExport = o.solid || o.binary;
        if (v < 10 && !drivesExport) {
          // Below the printable floor everything is "inside" — cutaway off.
          sceneEvents.onOptShape?.(null, null);
          return;
        }
        try {
          const { positions, indices, density } = await engine.densityShape(v / 100);
          if (get().densityThreshold === v) sceneEvents.onOptShape?.(positions, indices, density);
          if (drivesExport) {
            const { regions } = await engine.setIsoThreshold(v / 100, get().smoothIters);
            if (get().densityThreshold !== v || !get().optSummary) return;
            set({
              regionInfos: regions.map((r) => ({ density: r.density })),
              regionVisible: regions.map(() => true),
            });
            sceneEvents.onRegions?.(regions);
            sceneEvents.onRegionVisibility?.(get().regionVisible);
          }
        } catch {
          // grid/result vanished mid-drag: ignore
        }
      })();
    }, 140);
  },

  setLogOpen(open) {
    set({ logOpen: open });
  },

  clearLog() {
    set({ logLines: [] });
  },

  logNote(msg) {
    appendLog(set, msg);
  },

  async selectResult(id) {
    const cur = get();
    const e = cur.results.find((r) => r.id === id);
    if (!e || cur.busy) return;
    // Envelope (worst case across steps): no stashed solution — render the
    // UNDEFORMED part colored by the reduced field. Always auto-fits (its range
    // differs from any single step).
    if (isEnvelope(e)) {
      set({ activeResultId: id, fieldRange: null, legendMin: null, legendMax: null });
      sceneEvents.onLegendRange?.(null, null);
      try {
        await showEnvelope(set, get, e);
      } catch (err) {
        set({ error: err instanceof Error ? err.message : String(err) });
      }
      return;
    }
    // Stepping through load cases of the SAME kind holds the color scale FIXED
    // so the steps stay visually comparable (a redder step really is more
    // stressed): pin the current range across the switch, pinning to what's on
    // screen now if it wasn't already. A KIND switch (or leaving the envelope)
    // auto-fits as before.
    const prev = cur.results.find((r) => r.id === cur.activeResultId);
    const stepSwitch = !!prev && !isEnvelope(prev) && prev.id !== e.id && prev.kind === e.kind;
    let pinMin: number | null = null;
    let pinMax: number | null = null;
    if (stepSwitch) {
      if (cur.legendMin !== null || cur.legendMax !== null) {
        pinMin = cur.legendMin;
        pinMax = cur.legendMax;
      } else {
        const r = currentLegendRange(cur);
        if (r) [pinMin, pinMax] = r;
      }
    }
    // Keep whatever plot the user is on (|u|, a component, or a stress/SF field)
    // across the switch.
    set({ activeResultId: id, fieldRange: null, legendMin: pinMin, legendMax: pinMax });
    sceneEvents.onLegendRange?.(pinMin, pinMax);
    try {
      // Swap the stashed solution in (instant) and re-deform the viewport.
      const displacements = await engine.activateResult(id);
      if (get().activeResultId !== id) return; // user switched again mid-fetch
      // The field + voxel caches belonged to the previously active result.
      session.invalidateSolution();
      // Only the optimized result in Part Topo mode is a solid body; every
      // other result renders on the part hull.
      sceneEvents.onResultSolid?.(e.kind === "optimized" && !!get().optSummary?.solid);
      sceneEvents.onScalarField?.(null);
      // Modal mode shapes are mass-normalized (unit peak) and animate as a
      // symmetric ± swing; auto-start the animation when a mode is viewed (Q7).
      const modal = e.kind === "modal";
      sceneEvents.onModalAnim?.(modal);
      if (modal && !get().animateDeformed) {
        set({ animateDeformed: true });
        sceneEvents.onAnimateDeformed?.(true);
      }
      // Anchor the exaggeration on the lowest-deflection result so the scale
      // stays equal across switches (this result still uses its own buffer).
      // Modal modes are all unit-peak, so anchor them to 1 (independent of any
      // static results in the roster, which have real mm magnitudes).
      sceneEvents.onDisplacements?.(displacements, {
        maxDisplacement: modal ? 1 : referenceMaxDisp(get().results),
      });
      if (get().resultSurface === "voxel") {
        try {
          await session.loadVoxelResult();
        } catch {
          set({ resultSurface: "stl" });
          sceneEvents.onResultSurface?.("stl");
        }
      }
      try {
        // Re-fetch the SAME field for the newly-active solution (recomputes the
        // contour and the legend range from this result's data).
        await pushScalarField(set, get);
      } catch {
        // The retained field doesn't apply to this result — fall back to |u|.
        if (get().activeResultId === id) {
          set({ resultField: "u", fieldRange: null });
          sceneEvents.onScalarField?.(null);
          sceneEvents.onDispComponent?.(-1);
        }
      }
    } catch (err) {
      set({ error: err instanceof Error ? err.message : String(err) });
    }
  },

  async runCheck() {
    if (!get().model || !session.beginRun()) return;
    set({ busy: "Voxelizing & checking constraints…", error: null });
    try {
      await pushBcs(get);
      await logGridInfo(set);
      const report = await engine.check();
      set({ check: report, busy: null });
      const bad = report.components.find((c) => !c.constrained && c.mode);
      sceneEvents.onAnimateMode?.(bad?.mode ?? null);
      appendLog(
        set,
        report.ok
          ? `Check: OK — ${report.islandCount} ${report.islandCount === 1 ? "body" : "bodies"}, fully constrained` +
              (report.components[0] ? ` (λ ratio ${report.components[0].lambdaRatio.toExponential(1)})` : "")
          : `Check: UNDER-CONSTRAINED — ${report.islandCount} ${report.islandCount === 1 ? "body" : "bodies"}; ` +
              report.components
                .map((c, i) => `#${i + 1}: ${c.cells.toLocaleString()} voxels, ${c.constrained ? "ok" : "free"}`)
                .join(", ")
      );
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      set({ busy: null, error: msg });
      appendLog(set, `Check failed: ${msg}`);
    } finally {
      session.endRun();
    }
  },

  async runSolve() {
    if (!get().model || !session.beginRun()) return;
    get().maybeShowSupport();
    set({ busy: "Solving…", error: null });
    sceneEvents.onAnimateMode?.(null);
    let stopResidualPoll = () => {};
    // Build sim ignores structural BCs (its only "loads" are the per-layer
    // eigenstrain + the build plate), so it skips the multi-step path and the
    // under-constraint gate, and isn't retained as a switchable structural result.
    const buildsim = get().appMode === "buildsim";
    try {
      // Multiple load steps: solve them all (each manages its own residual poll
      // and result stash). Single step falls through to the byte-identical path.
      if (!buildsim && get().loadSteps.length > 1) {
        await solveAllSteps(set, get);
        return;
      }
      // Build sim ignores BCs and runs on its OWN coarse grid (built inside the
      // engine), so skip pushing BCs and the analysis-grid info/build here —
      // building the fine analysis grid is exactly the cost we're avoiding.
      if (!buildsim) {
        await pushBcs(get);
        await logGridInfo(set);
      }
      const report = buildsim ? null : await engine.check();
      if (report) set({ check: report });
      if (report && !report.ok) {
        const bad = report.components.find((c) => !c.constrained && c.mode);
        sceneEvents.onAnimateMode?.(bad?.mode ?? null);
        appendLog(set, "Solve aborted: model is under-constrained");
        set({
          busy: null,
          error:
            report.islandCount > 1
              ? `Model has ${report.islandCount} disconnected parts and is under-constrained — see the animated motion.`
              : "Model is under-constrained — the animation shows the free motion. Add or extend supports.",
        });
        return;
      }
      // Cache the voxel hull NOW (worker still free) so the Mesh view is
      // viewable during the blocking solve that follows.
      await prebuildMeshView(set, get);
      const st0 = get();
      const m = st0.material;
      const printed = st0.analyzeMode === "printed";
      const curve = st0.curves[st0.pattern];
      let printedSummary: PrintedSummary | null = null;
      let stats: SolveStats;
      let displacements: Float32Array;
      // Clear the old curve and stream the new one as the MGCG loop runs (the
      // engine reset its shared buffer when the solve call below was issued).
      set({ solveResiduals: [] });
      stopResidualPoll = session.startResidualPoll((r) => set({ solveResiduals: r }));
      if (!buildsim && printed) {
        appendLog(
          set,
          `Solve as printed: ${m.name}, skin ${st0.perimeters}×${st0.lineWidth} mm solid, ` +
            `interior ${st0.printInfill}% ${st0.pattern} (E/E₀ = ${curve.coeff}·ρ^${curve.exponent}) …`
        );
        const out = await engine.solvePrinted({
          infillPct: st0.printInfill,
          exponent: curve.exponent,
          coeff: curve.coeff,
          perimeters: st0.perimeters,
          lineWidth: st0.lineWidth,
          topBottomLayers: st0.topBottomLayers,
          layerHeight: st0.layerHeight,
        });
        stats = out.stats;
        displacements = out.displacements;
        printedSummary = {
          massGrams: out.stats.massGrams,
          massSolidGrams: out.stats.massSolidGrams,
          skinLayers: out.stats.skinLayers,
          compositeSkin: out.stats.compositeSkin,
          infillPct: st0.printInfill,
          pattern: st0.pattern,
          perimeters: st0.perimeters,
          lineWidth: st0.lineWidth,
          minSf: null,
          sfGoverns: null,
        };
        appendLog(
          set,
          `  as printed: mass ${out.stats.massGrams.toFixed(1)} g of ${out.stats.massSolidGrams.toFixed(1)} g solid · ` +
            (out.stats.compositeSkin
              ? `skin spans ${out.stats.skinLayers.toFixed(2)} cell layers (composite blend)`
              : `skin resolved by ${out.stats.skinLayers} cell layer${out.stats.skinLayers === 1 ? "" : "s"}`)
        );
      } else if (buildsim) {
        // Eigenstrain shrinks from the ONE active Properties material. With
        // thermal data (tLock + cte) they derive from physics (CTE × lock→room
        // cooling) and the temperature ladder is enabled; without it the raw
        // material shrink is sent with no temperatures (legacy path).
        // Transverse isotropy: in-plane (XY) vs through-layer (Z).
        const mat = st0.material;
        const phys = shrinkFromPhysics(mat, ROOM_TEMP_C);
        const shrink = phys ? phys.shrink : -Math.abs(mat.shrink);
        const shrinkZ = phys ? phys.shrinkZ : -Math.abs(mat.shrinkZ ?? mat.shrink);
        // Yield enables the plastic step that makes the released warp depend on
        // infill density (without it a uniform shrink releases density-blind).
        const yieldStrength = Math.max(0, mat.yieldStrength ?? 0);
        appendLog(
          set,
          `Build sim — ${mat.name}, shrink XY ${(Math.abs(shrink) * 100).toFixed(2)}% · Z ${(Math.abs(shrinkZ) * 100).toFixed(2)}% (${
            phys
              ? `physics: lock ${mat.tLock} °C → ${ROOM_TEMP_C} °C, bed ${st0.buildBedTemp} °C, chamber ${st0.buildChamberTemp} °C`
              : "material"
          }), ` +
            `${yieldStrength > 0 ? `yield ${yieldStrength} MPa (plastic)` : "elastic"}, ` +
            `${st0.buildState === "released" ? "released (off-bed)" : "on-bed"} state` +
            " — sequential inherent-strain warp + bed peel (coarse grid) …"
        );
        // Live preview on the VOXEL hull: only the printed (activated) cells,
        // jet-colored by |u|, with the displacement legend — so it reads like
        // the final result as it builds. True scale (×1) by default: warp is a
        // real mm quantity users compare against their printer/tolerances; the
        // legend's "exaggerated ×" control raises it on demand.
        const exag = 1;
        const previewStats = (maxU: number) =>
          ({
            iterations: 0,
            relResidual: 0,
            converged: true,
            maxDisplacement: maxU,
            seconds: 0,
            residuals: [],
            tol: get().solveTol,
          }) as SolveStats;
        // The live preview hull bakes the geometric exaggeration. Pin autoScale=1
        // so the legend reads exactly ×exag (autoScale·deformScale) and `stats` so
        // the legend even shows (it gates on viewMode==="deformed" && stats).
        set({
          busy: "Build sim…",
          buildProgress: { done: 0, total: 0 },
          deformScale: exag,
          autoScale: 1,
          hasResult: true,
          viewMode: "deformed",
          resultField: "u",
          // Build sim is STL-only (its coarse grid ≠ the analysis voxels).
          resultSurface: "stl",
          stats: previewStats(0),
        });
        sceneEvents.onViewState?.("deformed", exag);
        sceneEvents.onPeelMap?.(null, null, 0); // clear any prior run's bed map
        const out = await engine.buildSim(
          {
            shrink,
            shrinkZ,
            state: st0.buildState,
            exaggeration: exag,
            yieldStrength,
            // All four temperatures together enable the ladder; a material
            // without thermal data sends none (legacy behavior).
            ...(phys && mat.tLock != null
              ? {
                  tLock: mat.tLock,
                  tBed: st0.buildBedTemp,
                  tChamber: st0.buildChamberTemp,
                  tFinal: ROOM_TEMP_C,
                }
              : {}),
          },
          (p, positions, mags) => {
            set({ buildProgress: { done: p.done, total: p.total } });
            // Throttled frames carry the deformed active voxel hull + |u| — paint
            // it and keep the legend in sync with the running max.
            if (positions && positions.length > 0) {
              sceneEvents.onBuildActive?.(positions, mags);
              set({
                stats: previewStats(p.maxU),
                legendMin: 0,
                legendMax: p.maxU,
                fieldRange: { min: 0, max: p.maxU },
              });
              sceneEvents.onLegendRange?.(0, p.maxU);
            }
          }
        );
        // Tear down the preview; the final result renders via the deformed view.
        sceneEvents.onBuildActive?.(null);
        set({ buildProgress: null });
        displacements = out.displacements;
        // Final exaggeration: match the preview's GEOMETRIC ×exag. The scene's
        // autoScale = 0.08·diag/maxDisp, and the shown factor is autoScale·
        // deformScale, so deformScale = exag/autoScale. The displacement anchor
        // below MUST be this run's own max |u| (not referenceMaxDisp) or the
        // factor silently multiplies by buildMax/staticMin.
        const bb = get().model?.bbox;
        if (bb) {
          const diag = Math.hypot(bb[3] - bb[0], bb[4] - bb[1], bb[5] - bb[2]);
          const autoS = (0.08 * diag) / Math.max(out.stats.maxDisplacement, 1e-9);
          set({ deformScale: exag / autoS });
        }
        // Synthesize a SolveStats so the deformed view + dock render uniformly.
        stats = {
          iterations: out.stats.layers,
          relResidual: 0,
          converged: true,
          maxDisplacement: out.stats.maxDisplacement,
          seconds: out.stats.seconds,
          residuals: [],
          tol: get().solveTol,
        } as SolveStats;
        appendLog(
          set,
          `  coarse build grid ${out.stats.nx}×${out.stats.ny}×${out.stats.nz} (${Math.round(out.stats.cells / 1000)}k cells, h ${out.stats.h.toFixed(2)} mm) · ${out.stats.layers} layers · MGCG ${Math.round(out.stats.itersMean)} mean / ${out.stats.itersMax} max iters/layer · ${out.stats.seconds.toFixed(1)} s`
        );
        appendLog(
          set,
          `  stiffness/strain field: ${out.stats.densityAware ? "as-printed infill density (optimized)" : "solid hull — run the optimizer first to use the printed infill"}`
        );
        appendLog(
          set,
          `  warp |u|: bonded (on bed) ${out.stats.bondedMax.toExponential(2)} mm, released (off bed) ${out.stats.releasedMax.toExponential(2)} mm — showing ${st0.buildState} ×${exag}`
        );
        appendLog(
          set,
          `  bed peel: peak traction ${out.stats.peakLift.toFixed(3)} MPa (+Z), peak shear ${out.stats.peakShear.toFixed(3)} MPa — mesh-independent, uncalibrated indicator`
        );
        // Both states are now cached in the engine → enable instant switching.
        set({
          buildResult: {
            bondedMax: out.stats.bondedMax,
            releasedMax: out.stats.releasedMax,
            densityAware: out.stats.densityAware,
            peakLift: out.stats.peakLift,
            peakShear: out.stats.peakShear,
          },
        });
      } else {
        appendLog(set, `Solve solid: ${m.name} (E₀ ${m.e0} MPa, ν ${m.nu}) …`);
        const out = await engine.solve();
        stats = out.stats;
        displacements = out.displacements;
      }
      // Solve done — stop polling before publishing the exact final trace so a
      // late poll can't overwrite it with the (possibly capped) live snapshot.
      stopResidualPoll();
      session.invalidateSolution(); // stress fields + voxel result belong to the previous solution
      sceneEvents.onLegendRange?.(null, null);
      appendLog(
        set,
        `Solve ${stats.converged ? "converged" : "stopped at the iteration cap"}: ` +
          `${stats.iterations} MGCG iterations → rel. residual ${stats.relResidual.toExponential(1)} ` +
          `in ${stats.seconds.toFixed(1)} s · max |u| ${stats.maxDisplacement.toExponential(2)} mm`
      );
      set({
        stats,
        printedStats: printedSummary,
        solveResiduals: stats.residuals ?? [],
        solveTol: stats.tol ?? get().solveTol,
        hasResult: true,
        viewMode: "deformed",
        busy: null,
        resultField: "u",
        fieldRange: null,
        legendMin: null,
        legendMax: null,
        notice: stats.converged
          ? printedSummary && !printedSummary.compositeSkin && printedSummary.skinLayers === 1
            ? "The wall is only one voxel layer thick at this resolution — printed-mode results are coarse. Raise the resolution in Properties, or enable composite skin."
            : null
          : `Solver did NOT converge (stopped at the iteration cap, residual ${stats.relResidual.toExponential(1)}) — the results are unconverged and only indicative. See the caution in the results panel; a coarser resolution converges reliably.`,
      });
      sceneEvents.onScalarField?.(null);
      sceneEvents.onDisplacements?.(displacements, {
        // Build sim anchors on its OWN max |u|: its deformScale was derived
        // from that (shown = autoScale·deformScale = ×exag exactly). Anchoring
        // on the roster would multiply the factor by buildMax/staticMin.
        maxDisplacement: buildsim
          ? stats.maxDisplacement
          : referenceMaxDisp(get().results, stats.maxDisplacement),
      });
      sceneEvents.onViewState?.("deformed", get().deformScale);
      // Active section plane: fetch the volumetric payload for the capped
      // section (|u| colors are client-side, so nothing else re-pushes here).
      if (get().sectionOn) void pushScalarField(set, get);
      // Voxel result surface active: reload its hull for the new solution.
      if (get().resultSurface === "voxel") {
        try {
          await session.loadVoxelResult();
        } catch {
          set({ resultSurface: "stl" });
          sceneEvents.onResultSurface?.("stl");
        }
      }
      if (printedSummary) {
        // Min safety factors for the dock — both limits, so the dock can say
        // WHICH one governs. Fields are cached: picking them in the viewer
        // afterwards is instant.
        const sf = await refreshMinSf(set, get);
        if (sf) {
          appendLog(
            set,
            `  min safety factor ${sf.minSf.toFixed(2)}× — ` +
              (sf.governs === "layer"
                ? `layer adhesion governs (σₜᶻ ${m.strengthZ} / τᶻ ${
                    m.shearStrengthZ ?? Math.round(0.6 * m.strengthZ * 10) / 10
                  } MPa vs layer tension+shear)`
                : `material governs (σₜ ${m.strength} MPa vs σᵥᴹ)`)
          );
        }
      }
      // Retain this solve as a switchable result (as-printed or solid baseline)
      // so the Results view can compare it against the optimized design. Build
      // sim shows live but isn't a switchable structural result.
      if (!buildsim) {
        const rid: ResultKind = printedSummary ? "asprinted" : "solid";
        try {
          await engine.stashResult(rid);
          const cur = get();
          const rows: [string, ProvVal][] = printedSummary
            ? [
                ["Infill", `${printedSummary.infillPct}% ${printedSummary.pattern}`],
                ["Skin", { v: printedSummary.lineWidth, kind: "length", prefix: `${printedSummary.perimeters} × ` }],
                ["Material", cur.material.name],
                ["Mesh", meshLabel(cur)],
                ["Mass", fmtMass(printedSummary.massGrams)],
                ["Max |u|", fmtMm(stats.maxDisplacement)],
              ]
            : [
                ["Model", "fully dense E₀"],
                ["Material", cur.material.name],
                ["Mesh", meshLabel(cur)],
                ["Max |u|", fmtMm(stats.maxDisplacement)],
              ];
          upsertResult(set, get, {
            id: rid,
            kind: rid,
            loadStepId: cur.activeLoadStepId,
            loadStepName: activeStep(cur)?.name ?? "Load step",
            label: printedSummary
              ? `As printed · ${printedSummary.infillPct}% ${printedSummary.pattern}`
              : "Solid material",
            maxDisplacement: stats.maxDisplacement,
            massGrams: printedSummary ? printedSummary.massGrams : null,
            minSf: cur.printedStats?.minSf ?? null,
            converged: stats.converged,
            provTitle: printedSummary ? "As printed" : "Solid material",
            provRows: rows,
            epochs: { ...cur.resultEpochs },
          });
          set({ activeResultId: rid });
        } catch {
          // stash failed — the solve still shows, it just isn't switchable
        }
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      if (/cancelled/i.test(msg)) {
        set({ busy: null, notice: "Solve stopped." });
        appendLog(set, "Solve cancelled by user");
      } else {
        set({ busy: null, error: msg });
        appendLog(set, `Solve failed: ${msg}`);
      }
    } finally {
      stopResidualPoll(); // also covers the error/cancel paths
      if (get().buildProgress) {
        // Build-sim cancel/error: drop the live preview + progress.
        sceneEvents.onBuildGhost?.(null);
        sceneEvents.onBuildActive?.(null);
        set({ buildProgress: null });
      }
      session.endRun();
    }
  },

  async runModal() {
    if (!get().model || !session.beginRun()) return;
    get().maybeShowSupport();
    set({ busy: "Modal analysis…", error: null });
    sceneEvents.onAnimateMode?.(null);
    let stopResidualPoll = () => {};
    try {
      const st0 = get();
      const m = st0.material;
      const printed = st0.analyzeMode === "printed";
      const free = st0.freeFree;
      const curve = st0.curves[st0.pattern];
      // Constrained modal binds to the FIRST load case's supports (one support
      // set; forces are ignored — the eigenproblem is force-free). Existing
      // per-load-case results stay untouched (modal lives in its own kind).
      const steps = st0.loadSteps;
      const firstStep = steps[0];
      await engine.setBcs(effectiveBcs(st0.bcs, firstStep));
      await logGridInfo(set);
      // Constrained modal needs a constrained part — reuse the under-constraint
      // gate (an under-constrained part has rigid-body ~0 Hz modes). Free-free
      // deliberately runs WITHOUT supports, so it skips this gate.
      if (!free) {
        const report = await engine.check();
        set({ check: report });
        if (!report.ok) {
          const bad = report.components.find((c) => !c.constrained && c.mode);
          sceneEvents.onAnimateMode?.(bad?.mode ?? null);
          appendLog(set, "Modal aborted: model is under-constrained");
          set({
            busy: null,
            error:
              "Model is under-constrained — add supports, or tick “Unconstrained (free-free)” to analyze the free part.",
          });
          return;
        }
      }
      await prebuildMeshView(set, get);
      appendLog(
        set,
        `Modal analysis — ${st0.modalModeCount} mode${st0.modalModeCount === 1 ? "" : "s"}, ` +
          `${printed ? "as printed" : "solid"}, ${m.name}` +
          (free
            ? " — free-free (unconstrained, rigid-body modes dropped), undamped"
            : (steps.length > 1 ? ` (supports from "${firstStep.name}")` : "") +
              " — constrained, undamped") +
          " …"
      );
      // Live MGCG convergence trace (the nerd convergence plot) while the many
      // inner solves run.
      set({ solveResiduals: [] });
      stopResidualPoll = session.startResidualPoll((r) => set({ solveResiduals: r }));
      const out = await engine.modalAnalysis(
        {
          numModes: st0.modalModeCount,
          solid: !printed,
          free,
          infillPct: st0.printInfill,
          exponent: curve.exponent,
          coeff: curve.coeff,
          perimeters: st0.perimeters,
          lineWidth: st0.lineWidth,
          topBottomLayers: st0.topBottomLayers,
          layerHeight: st0.layerHeight,
        },
        (p) => {
          // Per-outer-iteration progress: show how far along + the current f1.
          const f1 = p.freqs[0] != null ? ` · f1 ≈ ${p.freqs[0].toFixed(0)} Hz` : "";
          set({ busy: `Modal — iteration ${p.outer}/${p.maxOuter}${f1}` });
        }
      );
      stopResidualPoll();
      const { result, displacements } = out;
      session.invalidateSolution();
      appendLog(
        set,
        `Modal ${result.converged ? "converged" : "stopped at the iteration cap"} ` +
          `in ${result.seconds.toFixed(1)} s ` +
          `(${result.outerIters} outer iters, ${result.totalInnerIters} MGCG V-cycles): ` +
          result.modes.map((md, i) => `f${i + 1} ${md.freqHz.toFixed(1)} Hz`).join(", ")
      );
      // One ResultEntry per mode — each a synthetic "step" under kind "modal".
      // The mode shapes are already stashed in the engine (`modal::mode-i`); the
      // viewer's per-step selector switches them, reusing the deformed view.
      const cur = get();
      const kept = cur.results.filter((r) => r.kind !== "modal");
      const modalEntries: ResultEntry[] = result.modes.map((md, i) => ({
        id: md.id,
        kind: "modal",
        loadStepId: md.id,
        loadStepName: `Mode ${i + 1} · ${md.freqHz.toFixed(1)} Hz`,
        label: "Modal",
        maxDisplacement: 1, // mode shapes are mass-normalized to unit peak
        massGrams: null,
        minSf: null,
        converged: result.converged,
        provTitle: `Mode ${i + 1}`,
        provRows: [
          ["Frequency", `${md.freqHz.toFixed(1)} Hz`],
          ["Analysis", printed ? "modal · as printed" : "modal · solid"],
          ["Supports", free ? "free-free (unconstrained)" : steps.length > 1 ? firstStep.name : "load case"],
          ["Material", cur.material.name],
          ["Mesh", meshLabel(cur)],
          ["Shape", "mass-normalized (relative)"],
        ],
        epochs: { ...cur.resultEpochs },
      }));
      const roster = sortResults(
        withEnvelope([...kept, ...modalEntries], cur.loadSteps),
        cur.loadSteps
      );
      set({
        results: roster,
        activeResultId: modalEntries[0]?.id ?? null,
        hasResult: true,
        viewMode: "deformed",
        busy: null,
        resultField: "u",
        fieldRange: null,
        legendMin: null,
        legendMax: null,
        // Auto-start the animation when modal results come up (Q7).
        animateDeformed: true,
        notice: result.converged
          ? null
          : "Modal solve did not fully converge — the highest modes may be inaccurate. Lower the mode count or raise the resolution.",
      });
      // Mode 0 is live in the engine — show it deformed, animating as a ± swing.
      sceneEvents.onScalarField?.(null);
      sceneEvents.onModalAnim?.(true);
      sceneEvents.onAnimateDeformed?.(true);
      sceneEvents.onDisplacements?.(displacements, { maxDisplacement: 1 });
      sceneEvents.onViewState?.("deformed", get().deformScale);
      if (get().sectionOn) void pushScalarField(set, get); // section cap volume
      if (get().resultSurface === "voxel") {
        try {
          await session.loadVoxelResult();
        } catch {
          set({ resultSurface: "stl" });
          sceneEvents.onResultSurface?.("stl");
        }
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      if (/cancelled/i.test(msg)) {
        set({ busy: null, notice: "Modal analysis stopped." });
        appendLog(set, "Modal cancelled by user");
      } else {
        set({ busy: null, error: msg });
        appendLog(set, `Modal failed: ${msg}`);
      }
    } finally {
      stopResidualPoll();
      session.endRun();
    }
  },

  async runOrientationSweep() {
    const s = get();
    if (s.busy || s.orientProgress) return;
    const active = s.results.find((r) => r.id === s.activeResultId) ?? s.results[0];
    if (!active || active.kind === "modal") {
      set({ notice: "Run a solve first — Optimize Orientation reads its stress field." });
      return;
    }
    // Fold ALL real load steps of the active kind (§15 dec. 5 — a step's
    // delamination is fatal regardless of optimizer weight). Roster ids ARE
    // engine stash ids.
    const ids = s.results.filter((r) => r.kind === active.kind && !isEnvelope(r)).map((r) => r.id);
    get().clearOrientationPreview();
    const token = ++orientToken;
    set({ orientProgress: { done: 0, total: 1 }, orientSweep: null, orientSel: null });
    appendLog(
      set,
      `Optimize orientation — ±90° rotation X/Y sweep on ${active.kind}` +
        (ids.length > 1 ? ` (${ids.length} load steps, worst case)` : "")
    );
    try {
      const out = await engine.orientationSweep(ids, 5, (p) => {
        if (token === orientToken) set({ orientProgress: p });
      });
      if (token !== orientToken) return; // invalidated mid-sweep
      // Best orientation = the HIGHEST worst-case layer SF (each pixel is
      // already the min over cells; we want the max of those minima).
      let best = -Infinity;
      let bi = 0;
      out.scored.forEach((v, i) => {
        if (v > best) {
          best = v;
          bi = i;
        }
      });
      const center = out.scored[(out.scored.length - 1) / 2];
      set({
        orientSweep: {
          n: out.n,
          stepDeg: out.stepDeg,
          scored: out.scored,
          all: out.all,
          ids,
          materialSfMin: out.materialSfMin,
        },
        orientProgress: null,
      });
      appendLog(
        set,
        `  best rot X ${-90 + Math.floor(bi / out.n) * out.stepDeg}° / rot Y ` +
          `${-90 + (bi % out.n) * out.stepDeg}° — min layer SF ${best.toFixed(2)}× ` +
          `(as oriented ${center.toFixed(2)}×, material floor ${out.materialSfMin.toFixed(2)}×)`
      );
      // Land directly in the preview at the CURRENT orientation: undeformed
      // part, layer-SF coloring, legend labeled accordingly.
      void get().selectOrientation((out.n - 1) / 2, (out.n - 1) / 2);
    } catch (e) {
      if (token !== orientToken) return;
      set({ orientProgress: null, error: (e as Error).message ?? String(e) });
    }
  },

  async selectOrientation(ip, ir) {
    const sw = get().orientSweep;
    if (!sw) return;
    const entering = !get().orientSel;
    // The preview paints the LAYER SF — say so in the legend/kind selector
    // instead of leaving the previous field's label on a SF coloring.
    set({ orientSel: { ip, ir }, resultField: "sfz" });
    sceneEvents.onOrientationPreview?.(orientDir(-90 + ip * sw.stepDeg, -90 + ir * sw.stepDeg));
    if (entering) {
      // The recolor rides the deformed-view scalar path, but an orientation
      // preview shows the part UNDEFORMED (like the envelope): zero
      // displacements, and NO volumetric section field — the cap and its
      // interior extreme would keep showing the previous result's stale
      // volume (in its own units) under the layer-SF legend.
      if (get().viewMode !== "deformed") await get().setViewMode("deformed");
      sceneEvents.onSectionVolume?.(null);
      const model = get().model;
      if (model && get().resultSurface === "stl") {
        sceneEvents.onScalarField?.(null);
        sceneEvents.onDisplacements?.(new Float32Array(model.triCount * 9), {
          maxDisplacement: referenceMaxDisp(get().results),
        });
      }
    }
    orientFetchField(set, get);
  },

  clearOrientationPreview() {
    if (!get().orientSel) return;
    set({ orientSel: null });
    sceneEvents.onOrientationPreview?.(null);
    // Full restore (pose, displacements, field, legend) via re-activation.
    const id = get().activeResultId;
    if (id) void get().selectResult(id);
    else void pushScalarField(set, get);
  },

  async runOptimize() {
    const st = get();
    if (!st.model || !session.beginRun()) return;
    get().maybeShowSupport();
    set({
      busy: st.optMode === "solid" ? "Optimizing shape…" : "Optimizing infill…",
      error: null,
      optProgress: null,
      optSummary: null,
      printedStats: null,
      optSeries: [],
    });
    sceneEvents.onAnimateMode?.(null);
    pushSymmetry(get); // editing aid — hide while the optimization runs
    try {
      // MULTI-LOAD (DESIGN §13): register every INCLUDED load step as a weighted
      // optimization case (the optimizer minimizes the weighted-sum compliance).
      // A single-step project — or one included step — keeps the single-load
      // path (active / sole step's BCs), byte-identical to before.
      await engine.clearLoadCases();
      const included = st.loadSteps.filter((s) => s.includeInOptimize);
      const multiLoad = st.loadSteps.length > 1 && included.length >= 2;
      if (multiLoad) {
        for (const step of included) {
          await engine.setBcs(effectiveBcs(st.bcs, step));
          await engine.addLoadCase(step.weight);
        }
        const wsum = included.reduce((a, s) => a + s.weight, 0) || 1;
        appendLog(
          set,
          `Multi-load optimize — weighted sum of ${included.length} load steps: ` +
            included.map((s) => `${s.name} ${Math.round((100 * s.weight) / wsum)}%`).join(" · ")
        );
      } else if (st.loadSteps.length > 1 && included.length === 1) {
        await engine.setBcs(effectiveBcs(st.bcs, included[0]));
      } else if (included.length === 0) {
        set({
          busy: null,
          error:
            "No load steps are included in the optimization — enable at least one in “Manage load steps”.",
        });
        return;
      } else {
        await pushBcs(get); // single step: the active (sole) step's BCs
      }
      await logGridInfo(set);
      // Cache the voxel hull NOW (worker still free) so the Mesh view is
      // viewable during the blocking optimization that follows.
      await prebuildMeshView(set, get);
      const curve = st.curves[st.pattern];
      const solid = st.optMode === "solid";
      const binary = st.optMode === "binary";
      const match = st.goal === "match" && !solid;
      const ls = st.levelSettings;
      const manual = !binary && !solid && ls.mode === "manual" && ls.manual.length >= 2;
      appendLog(
        set,
        solid
          ? `Optimize SOLID topology: keep ${st.budget}% of the material, ` +
              `material removed elsewhere — convergence when mean |Δρ| < 0.005 twice · ` +
              `optimizer SIMP-penalized p=3` +
              (st.selfSupport ? ` · self-supporting ≥ ${st.overhangDeg}° overhang` : "") +
              (st.symOn ? ` · symmetry plane ${symLabel(st.symNormal, st.symC)}` : "")
          : `Optimize (${binary ? `binary: ${ls.binaryFloorPct}% or solid` : manual ? `manual levels ${ls.manual.join("/")}%` : "graded, auto levels"}): ` +
              (match
                ? `match the stiffness of uniform ${st.budget}% — lightest design via budget secant`
                : `infill budget ${st.budget}%`) +
              ` (${st.pattern}: E/E₀ = ${curve.coeff}·ρ^${curve.exponent}), ` +
              `skin ${st.perimeters}×${st.lineWidth} mm — convergence when mean |Δρ| < 0.005 twice` +
              (binary ? " · optimizer SIMP-penalized p=3" : "") +
              (st.selfSupport ? ` · self-supporting ${st.overhangDeg}°` : "") +
              (st.symOn ? ` · symmetry plane ${symLabel(st.symNormal, st.symC)}` : "")
      );
      let lastPass = 0;
      const out = await engine.optimize(
        {
          budgetPct: st.budget,
          exponent: curve.exponent,
          coeff: curve.coeff,
          perimeters: st.perimeters,
          lineWidth: st.lineWidth,
          smoothIters: st.smoothIters,
          nBins: st.nBins,
          floorPct: binary ? ls.binaryFloorPct : ls.floorPct,
          capPct: binary ? 100 : ls.capPct,
          levelsPct: binary ? [ls.binaryFloorPct, 100] : manual ? ls.manual : null,
          binary,
          solid,
          retainBc: st.retainBc,
          selfSupport: st.selfSupport,
          overhangDeg: st.overhangDeg,
          // Binary mode always pins the pattern (rectilinear/concentric) on the
          // modifiers AND the object-level general infill.
          solidPattern: binary ? st.solidPattern : null,
          goal: solid ? "budget" : st.goal,
          symmetry: st.symOn ? [...st.symNormal, st.symC] : null,
          topBottomLayers: st.topBottomLayers,
          layerHeight: st.layerHeight,
          // Auto = 2× line width (a true smallest printable rib); 0 disables.
          minMemberMm: st.minMemberMm ?? 2 * st.lineWidth,
        },
        (p, density, skelPositions, skelIndices, skelDensity) => {
          set((s) => ({
            optProgress: {
              iteration: p.iteration,
              maxIter: p.maxIter,
              pass: p.pass,
              passes: p.passes,
            },
            optSeries: [
              ...s.optSeries,
              {
                // Global sample index: match mode runs several passes whose
                // iteration counters restart — charts want one x axis.
                it: s.optSeries.length + 1,
                compliance: p.compliance,
                massFrac: p.massFrac,
                meanInfill: p.meanInfill,
                change: p.change,
                meanChange: p.meanChange,
                innerIters: p.innerIters,
                innerRes: p.innerRes,
              },
            ],
          }));
          if (p.pass !== lastPass) {
            lastPass = p.pass;
            if (p.passes > 1) {
              appendLog(
                set,
                `pass ${p.pass}/${p.passes}: optimizing at budget ${(p.budgetNow * 100).toFixed(1)}%`
              );
            }
          }
          appendLog(
            set,
            `  ${p.passes > 1 ? `p${p.pass} ` : ""}it ${String(p.iteration).padStart(2)}: bᵀu ${p.compliance.toExponential(3)} N·mm · ` +
              `infill ${(p.meanInfill * 100).toFixed(1)}% · Δmax ${p.change.toFixed(3)} · ` +
              `Δmean ${p.meanChange.toFixed(4)} · CG ${p.innerIters}@${p.innerRes.toExponential(1)}`
          );
          if (get().viewMode !== "density") {
            set({ viewMode: "density" });
            sceneEvents.onViewState?.("density", get().deformScale);
          }
          sceneEvents.onVertexDensity?.(density);
          // Watch the optimized shape gain detail iteration by iteration.
          sceneEvents.onOptShape?.(skelPositions ?? null, skelIndices ?? null, skelDensity ?? null);
        }
      );
      appendLog(
        set,
        `Optimize ${out.summary.converged ? `converged in ${out.summary.iterations} iterations` : `stopped at the ${out.summary.iterations}-iteration cap`} ` +
          `(${out.summary.seconds.toFixed(1)} s) · ` +
          (out.summary.solid
            ? `retained volume ${(out.summary.meanInfill * 100).toFixed(1)}%`
            : `levels ${out.summary.bins.map((b) => `${Math.round(b.density * 100)}%`).join("/")} · mean infill ${(out.summary.meanInfill * 100).toFixed(1)}%`) +
          ` · mass ${out.summary.massGrams.toFixed(1)} g (${Math.round(out.summary.massFrac * 100)}% of solid)`
      );
      appendLog(
        set,
        `  verification: stiffness ${Math.round(out.summary.stiffnessVsSolid * 100)}% of solid · ` +
          (out.summary.solid
            ? `+${(out.summary.gainVsUniform * 100).toFixed(1)}% stiffer than the same material spread uniformly`
            : `+${(out.summary.gainVsUniform * 100).toFixed(1)}% stiffer than uniform ${Math.round(out.summary.meanInfill * 100)}% infill at equal weight`)
      );
      if (out.summary.goal === "match" && out.summary.massUniformRefGrams) {
        const saved = 1 - out.summary.massGrams / out.summary.massUniformRefGrams;
        appendLog(
          set,
          `  match: stiffness of uniform ${Math.round(out.summary.refUniformPct ?? 0)}% hit within ` +
            `${((out.summary.matchDeviation ?? 0) * 100).toFixed(1)}% in ${out.summary.passes} passes · ` +
            `${out.summary.massGrams.toFixed(1)} g vs ${out.summary.massUniformRefGrams.toFixed(1)} g uniform (−${(saved * 100).toFixed(0)}%)`
        );
      }
      const vis = out.regions.map(() => true);
      session.invalidateSolution(); // stress fields + voxel result belong to the previous solution
      sceneEvents.onLegendRange?.(null, null);
      set({
        resultField: "u",
        fieldRange: null,
        legendMin: null,
        legendMax: null,
        optSummary: out.summary,
        optProgress: null,
        busy: null,
        // Jump straight to the Export step with the Regions view active.
        activeStep: 6,
        viewMode: "infill",
        // converged:true here is the OPTIMIZER's design-stationarity, not the
        // binned verification solve's MGCG convergence (the engine hardcodes
        // that — see crates/filasim-wasm/src/lib.rs Solution after the opt loop).
        // The dock keys its non-convergence banner off optSummary.converged;
        // surfacing the verification residual needs the deferred wasm change.
        stats: {
          iterations: out.summary.iterations,
          relResidual: 0,
          converged: true,
          maxDisplacement: out.summary.maxDisplacement,
          seconds: out.summary.seconds,
        },
        hasResult: true,
        regionInfos: out.regions.map((r) => ({ density: r.density })),
        regionVisible: vis,
      });
      sceneEvents.onOptShape?.(null, null);
      sceneEvents.onVertexDensity?.(out.vertexDensity);
      // The result set after this run is: any surviving as-printed result plus
      // optimized + (infill modes) the uniform/solid baselines. Anchor the
      // exaggeration on the stiffest of those so switching keeps it equal.
      sceneEvents.onDisplacements?.(out.displacements, {
        maxDisplacement: referenceMaxDisp(
          get().results.filter((r) => r.kind === "asprinted"),
          Math.min(
            out.summary.maxDisplacement,
            out.summary.uniformMaxDisp ?? Infinity,
            out.summary.solidMaxDisp ?? Infinity
          )
        ),
      });
      // Part Topo: the body IS the result — drop the original envelope hull in
      // the result views so it doesn't moiré against the coincident body.
      sceneEvents.onResultSolid?.(out.summary.solid);
      sceneEvents.onRegions?.(out.regions);
      sceneEvents.onRegionVisibility?.(vis);
      // Land in the Regions view. Part Topo / binary seed the isosurface density
      // at 50% (the export level); graded seeds a 25% cutaway for the density view.
      sceneEvents.onViewState?.("infill", get().deformScale);
      get().setDensityThreshold(out.summary.solid || out.summary.binary ? 50 : 25);
      // Re-arm the plane state (still hidden in result views; it shows again
      // when the user returns to a setup view on this step).
      pushSymmetry(get);
      // ---- retain the optimized design + its baseline solves as switchable
      // results. The equal-mass uniform and solid baselines were already solved
      // AND stashed by the optimizer (infill modes); the optimized solution is
      // stashed here. The set is rebuilt: keep any as-printed result, replace
      // the optimize-owned ones (drops baselines that solid mode doesn't keep).
      try {
        // MULTI-STEP projects evaluate the one optimized design under EVERY load
        // step so the Results roster carries them all (+ a worst-case envelope),
        // not just the optimizer's primary case (DESIGN §13). A single load step
        // keeps the byte-identical pre-load-step path.
        if (get().loadSteps.length > 1) await stashOptimizedMultiStep(set, get, out);
        else await stashOptimizedSingle(set, get, out);
      } catch {
        set({ busy: null });
        // stash failed — the optimized result still shows, it just isn't switchable
      }
    } catch (e) {
      sceneEvents.onOptShape?.(null, null);
      const msg = e instanceof Error ? e.message : String(e);
      if (/cancelled/i.test(msg)) {
        set({ busy: null, optProgress: null, notice: "Optimization stopped." });
        appendLog(set, "Optimization cancelled by user");
      } else {
        set({ busy: null, optProgress: null, error: msg });
        appendLog(set, `Optimize failed: ${msg}`);
      }
      pushSymmetry(get);
    } finally {
      session.endRun();
    }
  },

  setExportSlicer(slicer) {
    set({ exportSlicer: slicer });
  },

  consentDisclaimer() {
    set({ disclaimerOpen: false });
  },

  setDisclaimerSkipped(on) {
    set({ disclaimerSkipped: on });
    try {
      if (on) localStorage.setItem(SKIP_DISCLAIMER_KEY, "1");
      else localStorage.removeItem(SKIP_DISCLAIMER_KEY);
    } catch {
      // private mode: the checkbox still works for this session
    }
  },

  openUnits(open) {
    set({ unitsOpen: open });
  },

  setUnitPreset(presetId) {
    const preset = PRESETS[presetId];
    if (!preset) return;
    applyUnitPrefs(set, get, { ...preset.units });
  },

  setUnit(kind, unitId) {
    if (!QUANTITIES[kind].units.some((u) => u.id === unitId)) return;
    applyUnitPrefs(set, get, { ...get().unitPrefs, [kind]: unitId });
  },

  async downloadThreeMf() {
    try {
      // Snapshot the current view as the plate thumbnail (null → placeholder).
      const thumbnail = sceneEvents.captureThumbnail?.() ?? null;
      const bytes = await engine.exportThreeMf(get().exportSlicer, thumbnail);
      const base = (get().fileName ?? "part").replace(/\.(stl|3mf)$/i, "");
      download(bytes, `${base}_smart_infill.3mf`, "model/3mf");
    } catch (e) {
      set({ error: e instanceof Error ? e.message : String(e) });
    }
  },

  async downloadStls() {
    try {
      const bytes = await engine.exportStls();
      const base = (get().fileName ?? "part").replace(/\.(stl|3mf)$/i, "");
      download(bytes, `${base}_modifiers.zip`, "application/zip");
    } catch (e) {
      set({ error: e instanceof Error ? e.message : String(e) });
    }
  },

  async downloadShape() {
    try {
      const bytes = await engine.exportSolidStl();
      const base = (get().fileName ?? "part").replace(/\.(stl|3mf)$/i, "");
      download(bytes, `${base}_optimized.stl`, "model/stl");
    } catch (e) {
      set({ error: e instanceof Error ? e.message : String(e) });
    }
  },

  setColorSteps(n) {
    const v = Math.max(COLOR_STEPS_MIN, Math.min(COLOR_STEPS_MAX, Math.round(n)));
    set({ colorSteps: v });
  },

  async downloadColorThreeMf() {
    const s = get();
    if (!s.activeResultId) return;
    const field = s.resultField;
    // The active contour range = user override (legendMin/Max) else the field's
    // auto range (fieldRange, |u| anchored at 0). Same numbers the legend shows.
    const fr = s.fieldRange;
    const lo = s.legendMin ?? fr?.min ?? null;
    const hi = s.legendMax ?? fr?.max ?? null;
    if (lo === null || hi === null || !(hi > lo)) {
      set({ error: "No contour range to band — open the result in the Results view first." });
      return;
    }
    const steps = Math.max(COLOR_STEPS_MIN, Math.min(COLOR_STEPS_MAX, Math.round(s.colorSteps)));
    // Bake the band colors from the SAME ramp the viewer paints: all result
    // fields use jet; safety factors flip it (red = critical low). The engine
    // cuts each triangle along the band iso-lines for sharp, watertight bands.
    const colors = bandHexColors(jet, field.startsWith("sf"), steps);
    try {
      const thumbnail = sceneEvents.captureThumbnail?.() ?? null;
      const bytes = await engine.exportColorThreeMf(field, lo, hi, steps, colors, thumbnail);
      const fname = (s.fileName ?? "part").replace(/\.(stl|3mf)$/i, "");
      download(bytes, `${fname}_${field}_color.3mf`, "model/3mf");
    } catch (e) {
      set({ error: e instanceof Error ? e.message : String(e) });
    }
  },

  async saveProject(includeResults) {
    const s = get();
    if (!s.model || !s.fileName) return;
    set({ busy: "Saving project…", error: null, notice: null });
    try {
      const transform = await engine.transformMatrix();
      const ext = /\.3mf$/i.test(s.fileName) ? "3mf" : "stl";
      const manifest: ProjectManifest = {
        app: "filaSim",
        schemaVersion: PROJECT_SCHEMA,
        appVersion: APP_VERSION,
        fileName: s.fileName,
        transform,
        settings: collectSettings(s),
        bcs: s.bcs.map((b) => ({ ...b, tris: Array.from(b.tris) })),
        loadSteps: serializeLoadSteps(s.bcs, s.loadSteps),
        optSummary: s.optSummary,
        regionInfos: s.regionInfos,
        // Envelopes have no stashed buffer — they're re-derived on load.
        results: includeResults ? s.results.filter((r) => !isEnvelope(r)) : null,
        activeResultId: includeResults ? s.activeResultId : null,
      };
      const bytes = await engine.exportProject(JSON.stringify(manifest), `model.${ext}`, includeResults);
      const base = s.fileName.replace(/\.(stl|3mf)$/i, "");
      download(bytes, `${base}.filasim`, "application/octet-stream");
      set({
        busy: null,
        notice: includeResults
          ? "Project saved — model, settings, and results embedded."
          : "Project saved — model, settings, and the optimized design (no FEA results).",
      });
    } catch (e) {
      set({ busy: null, error: e instanceof Error ? e.message : String(e) });
    }
  },

  async openProject(file) {
    set({ busy: "Opening project…", error: null, notice: null });
    try {
      const bytes = await file.arrayBuffer();
      const { manifest, model: mi } = await engine.openProjectModel(bytes);
      const mf = JSON.parse(manifest) as ProjectManifest;
      if (mf.app !== "filaSim" || typeof mf.schemaVersion !== "number") {
        throw new Error("Not a filaSim project file.");
      }
      if (mf.schemaVersion > PROJECT_SCHEMA) {
        throw new Error("This project was saved by a newer version of filaSim — please update to open it.");
      }
      const st = mf.settings;
      const model: LoadedModel = {
        positions: mi.positions,
        patchIds: mi.patchIds,
        patchCount: mi.patchCount,
        triCount: mi.triCount,
        bbox: mi.bbox as LoadedModel["bbox"],
        hasCadFaces: mi.hasCadFaces,
      };
      // Re-id the loads/supports so they can't collide with this session's counter.
      const bcs: Bc[] = mf.bcs.map((b) => ({ ...b, id: `bc${++bcCounter}`, tris: Uint32Array.from(b.tris) }));
      // Load steps remap their override keys onto the re-id'd BCs (above);
      // pre-feature files synthesize a single default step.
      const loadSteps = deserializeLoadSteps(bcs, mf.loadSteps);
      set({
        fileName: mf.fileName,
        model,
        segAngle: st.segAngle,
        segSource: st.segSource,
        material: st.material,
        materials: st.materials,
        curves: st.curves,
        levelSettings: st.levelSettings,
        resolution: st.resolution,
        customH: st.customH,
        pattern: st.pattern,
        perimeters: st.perimeters,
        lineWidth: st.lineWidth,
        topBottomLayers: st.topBottomLayers,
        layerHeight: st.layerHeight,
        printInfill: st.printInfill,
        snapVoxel: st.snapVoxel,
        compositeSkin: st.compositeSkin,
        smoothStress: st.smoothStress,
        materialStress: st.materialStress,
        analyzeMode: st.analyzeMode,
        // Pre-temperature-ladder projects lack the temps (and older files may
        // carry a now-removed `buildMaterial` preset id — silently ignored;
        // the ONE Properties material drives the build sim).
        buildBedTemp: st.buildBedTemp ?? 60,
        buildChamberTemp: st.buildChamberTemp ?? 25,
        budget: st.budget,
        smoothIters: st.smoothIters,
        nBins: st.nBins,
        minMemberMm: st.minMemberMm,
        goal: st.goal,
        optMode: st.optMode,
        retainBc: st.retainBc,
        selfSupport: st.selfSupport,
        overhangDeg: st.overhangDeg,
        symOn: st.symOn,
        symNormal: st.symNormal,
        symC: st.symC,
        solidPattern: st.solidPattern,
        exportSlicer: st.exportSlicer,
        resultSurface: st.resultSurface,
        bcs,
        activeBcId: null,
        loadSteps,
        activeLoadStepId: loadSteps[0].id,
        tool: "orbit",
        activeStep: 6,
        check: null,
        stats: null,
        hasResult: false,
        optSummary: null,
        results: [],
        activeResultId: null,
        resultEpochs: { ...ZERO_EPOCHS },
        regionInfos: [],
        regionVisible: [],
        densityThreshold: 0,
        resultField: "u",
        fieldRange: null,
        legendMin: null,
        legendMax: null,
        autoScale: 1,
        voxelInfo: null,
        voxelMeshReady: false,
      });
      session.invalidateSolution();
      // Push the saved physics + grid settings to the engine.
      const m = get().material;
      await engine.setMaterial(m.e0, m.nu, m.density, m.strength, m.strengthZ, m.shearStrengthZ);
      await pushResolution(get);
      await engine.setSnapWall(st.snapVoxel ? st.perimeters * st.lineWidth : 0);
      await engine.setCompositeSkin(st.compositeSkin);
      await engine.setSmoothStress(st.smoothStress);
      // Session display setting, not part of the project manifest.
      await engine.setLayerShear(get().layerShear);
      await engine.setMaterialStress(st.materialStress);
      // Replay the saved orientation (clears the engine grid + opt; restore
      // rebuilds them), then push the loads/supports.
      if (Array.isArray(mf.transform) && mf.transform.length === 12) {
        const out = await engine.transform(mf.transform);
        set({ model: { ...get().model!, positions: out.positions, bbox: out.bbox as LoadedModel["bbox"] } });
      }
      // Push the active (first) load step's effective BCs — identical to `bcs`
      // for a single-step / pre-feature project.
      await engine.setBcs(effectiveBcs(bcs, loadSteps[0]));
      // Clear any stale overlays, then push the restored model + BC glyphs.
      sceneEvents.onScalarField?.(null);
      sceneEvents.onDisplacements?.(null, null);
      sceneEvents.onVertexDensity?.(null);
      sceneEvents.onRegions?.(null);
      sceneEvents.onOptShape?.(null, null);
      sceneEvents.onModelLoaded?.(get().model!);
      pushBcGlyphs(get, null);
      // Phase 2: restore the design + result buffers into the engine.
      const restore = await engine.openProjectRestore();
      // Optimized design → store + scene (Density/Regions/export).
      if (restore.hasDesign && mf.optSummary) {
        set({
          optSummary: mf.optSummary,
          regionInfos: mf.regionInfos,
          regionVisible: mf.regionInfos.map(() => true),
        });
        const { regions } = await engine.resmoothRegions(get().smoothIters);
        const vd = await engine.vertexDensity();
        sceneEvents.onResultSolid?.(!!mf.optSummary.solid);
        sceneEvents.onVertexDensity?.(vd);
        sceneEvents.onRegions?.(regions);
        sceneEvents.onRegionVisibility?.(get().regionVisible);
      }
      // Embedded FEA results → store + scene (deflection views + switcher).
      const haveResults = !!(mf.results && mf.results.length && restore.restoredResults.length);
      if (haveResults) {
        // Backfill the load-step fields for results saved before this feature
        // (always single-step → the sole step) so the roster type holds.
        const step0 = get().loadSteps[0];
        clearEnvelopeCache();
        const roster: ResultEntry[] = withEnvelope(
          sortResults(
            mf
              .results!.filter((r) => restore.restoredResults.includes(r.id))
              .map((r) => ({
                ...r,
                loadStepId: r.loadStepId ?? step0?.id ?? "",
                loadStepName: r.loadStepName ?? step0?.name ?? "Load step",
                epochs: { ...ZERO_EPOCHS },
              })),
            get().loadSteps
          ),
          get().loadSteps
        );
        const activeId =
          mf.activeResultId && roster.some((r) => r.id === mf.activeResultId)
            ? mf.activeResultId
            : (roster[0]?.id ?? null);
        set({ results: roster, activeResultId: activeId, hasResult: true });
        if (activeId) {
          const disp = await engine.activateResult(activeId);
          session.invalidateSolution();
          const activeKind = roster.find((r) => r.id === activeId)?.kind;
          sceneEvents.onResultSolid?.(activeKind === "optimized" && !!mf.optSummary?.solid);
          sceneEvents.onScalarField?.(null);
          sceneEvents.onDisplacements?.(disp, { maxDisplacement: referenceMaxDisp(roster) });
          if (get().resultSurface === "voxel") {
            try {
              await session.loadVoxelResult();
            } catch {
              set({ resultSurface: "stl" });
              sceneEvents.onResultSurface?.("stl");
            }
          }
        }
      }
      const landing: ViewMode = haveResults ? "deformed" : restore.hasDesign ? "density" : "setup";
      set({ busy: null, viewMode: landing });
      sceneEvents.onViewState?.(landing, get().deformScale);
      if (restore.hasDesign) get().setDensityThreshold(st.densityThreshold);
      if (landing === "deformed") await pushScalarField(set, get);
      appendLog(
        set,
        `Opened project "${mf.fileName}"` +
          (haveResults
            ? ` — ${restore.restoredResults.length} result${restore.restoredResults.length === 1 ? "" : "s"} restored`
            : restore.hasDesign
              ? " — optimized design restored (re-run Optimize to recompute deflection)"
              : "")
      );
    } catch (e) {
      set({ busy: null, error: e instanceof Error ? e.message : String(e) });
    }
  },

  async setViewMode(mode) {
    if (mode === "mesh") {
      const prev = get().viewMode;
      set({ viewMode: "mesh" });
      sceneEvents.onViewState?.("mesh", get().deformScale);
      // A run is in flight: the worker is blocked inside the solver and can't
      // build a fresh hull. Show the one cached at run start (prebuildMeshView)
      // — refreshView already made the voxel group visible — and don't queue a
      // request that would only resolve after the run (and overwrite its
      // status with a misleading "building mesh" message).
      if (get().busy) return;
      const first = !get().voxelMeshReady;
      if (first) set({ busy: "Building analysis mesh…", error: null });
      const ok = await refreshMeshView(set, get);
      if (first) set({ busy: null });
      if (!ok) {
        set({ viewMode: prev });
        sceneEvents.onViewState?.(prev, get().deformScale);
      }
      return;
    }
    // Leaving the mesh view: plane clipping owns sectioning again.
    sceneEvents.onVoxelCutActive?.(false);
    set({ viewMode: mode });
    sceneEvents.onViewState?.(mode, get().deformScale);
    // The bed-peel heatmap belongs only to the deformed result view.
    if (mode !== "deformed") {
      sceneEvents.onPeelMap?.(null, null, 0);
    } else if (get().resultSurface !== "voxel" && get().hasResult) {
      // STL deformed view: re-assert the active field (repaints peel/stress).
      void pushScalarField(set, get);
    }
    // Entering results with the voxel surface chosen: (re)load lazily —
    // an optimize lands on the density view, so the hull may be stale.
    if (mode === "deformed" && get().resultSurface === "voxel" && !session.isVoxelLoaded) {
      session
        .loadVoxelResult()
        .then(() => pushScalarField(set, get))
        .catch(() => {
          set({ resultSurface: "stl" });
          sceneEvents.onResultSurface?.("stl");
        });
    }
  },

  setWireframe(on) {
    set({ wireframe: on });
    sceneEvents.onWireframe?.(on);
  },

  setDeformScale(s) {
    set({ deformScale: s });
    sceneEvents.onViewState?.(get().viewMode, s);
  },

  setAnimateDeformed(on) {
    set({ animateDeformed: on });
    sceneEvents.onAnimateDeformed?.(on);
  },

  async setResultField(kind) {
    // The custom scale belongs to the previous field.
    set({ resultField: kind, legendMin: null, legendMax: null });
    sceneEvents.onLegendRange?.(null, null);
    sceneEvents.onShowExtremes?.(get().showExtremes, fieldUnit(kind));
    try {
      await pushScalarField(set, get);
    } catch (e) {
      set({ error: e instanceof Error ? e.message : String(e), resultField: "u", fieldRange: null });
      sceneEvents.onScalarField?.(null);
    }
  },

  async setResultSurface(surface) {
    if (get().resultSurface === surface) return;
    set({ resultSurface: surface });
    try {
      if (surface === "voxel") {
        await session.loadVoxelResult();
      }
      sceneEvents.onResultSurface?.(surface);
      // The scalar field is sized per surface — re-push for the active one.
      await pushScalarField(set, get);
    } catch (e) {
      set({ error: e instanceof Error ? e.message : String(e), resultSurface: "stl" });
      sceneEvents.onResultSurface?.("stl");
    }
  },

  setLegendRange(min, max) {
    if (min !== null && max !== null && !(max > min)) return; // ignore inverted
    set({ legendMin: min, legendMax: max });
    sceneEvents.onLegendRange?.(min, max);
  },

  fitLegend() {
    // Rescale (and pin) the legend to the CURRENT step's data — the shared-scale
    // escape hatch when one load case dwarfs the others.
    const r = currentLegendRange(get());
    if (r) {
      set({ legendMin: r[0], legendMax: r[1] });
      sceneEvents.onLegendRange?.(r[0], r[1]);
    } else {
      set({ legendMin: null, legendMax: null });
      sceneEvents.onLegendRange?.(null, null);
    }
  },

  setShowExtremes(on) {
    set({ showExtremes: on });
    sceneEvents.onShowExtremes?.(on, fieldUnit(get().resultField));
  },

  toggleSection() {
    const on = !get().sectionOn;
    set({ sectionOn: on });
    sceneEvents.onSectionState?.(on);
    // Mesh view sections by dropping whole cells, not by plane-clipping.
    if (get().viewMode === "mesh") void refreshMeshView(set, get);
    // Result views: (re)push the field so the volumetric section payload is
    // fetched for the cap (displacement fields skip it while the plane is off).
    if (on && get().viewMode === "deformed" && get().hasResult) {
      void pushScalarField(set, get);
    }
  },

  flipSection() {
    sceneEvents.onSectionFlip?.();
  },

  setSectionAxis(axis) {
    sceneEvents.onSectionAxis?.(axis);
  },

  cancelRun() {
    if (!engine.canCancel) return;
    engine.cancel();
    appendLog(set, "Stop requested — halting at the next solver checkpoint…");
  },

  clearError() {
    set({ error: null, notice: null });
    // The under-constrained toast drives the rigid-body-motion animation;
    // dismissing the toast must stop the looping drift, not leave it running.
    sceneEvents.onAnimateMode?.(null);
  },
}));
