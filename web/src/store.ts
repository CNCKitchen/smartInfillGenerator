// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { create } from "zustand";
import { engine, type OptRegion, type OptSummary } from "./engine/EngineClient";
import type {
  Bc,
  BcKind,
  CheckReport,
  LoadedModel,
  Material,
  PatternCurve,
  PatternKey,
  ResolutionKey,
  SolveStats,
  VoxelInfo,
} from "./types";
import { DEFAULT_CURVES, DEFAULT_MATERIALS, RESOLUTIONS, RESULT_FIELDS } from "./types";

export type Tool = "orbit" | "select" | "brush";
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
  optMode: "graded" | "binary";
  goal: "budget" | "match";
  levelSettings: LevelSettings;
}): [number, number] {
  if (s.goal === "match") return [s.levelSettings.floorPct, s.levelSettings.capPct];
  return s.optMode === "binary"
    ? [s.levelSettings.binaryFloorPct, 90]
    : [s.levelSettings.floorPct, s.levelSettings.capPct];
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

interface AppState {
  // workflow navigation (step rail): 1 Model … 6 View & export
  activeStep: number;
  // model
  fileName: string | null;
  model: LoadedModel | null;
  segAngle: number;
  // interaction
  tool: Tool;
  brushRadius: number;
  brushErase: boolean;
  // bcs
  bcs: Bc[];
  activeBcId: string | null;
  // physics
  material: Material;
  materials: Material[];
  curves: Record<PatternKey, PatternCurve>;
  resolution: ResolutionKey;
  // print properties (step 3 · Properties) — shared by the as-printed
  // verify solve, the optimizer's skin model, and the 3MF export
  pattern: PatternKey;
  perimeters: number;
  lineWidth: number; // mm
  /** Uniform interior infill % "as printed" — the slicer setting. */
  printInfill: number;
  /** Snap the voxel size to wall/k so the skin is k exact cell layers. */
  snapVoxel: boolean;
  /** What "Solve once" analyzes: the print or the CAD-ideal solid. */
  analyzeMode: "printed" | "solid";
  /** Extras of the last as-printed solve (results dock); null = solid run. */
  printedStats: PrintedSummary | null;
  /** Mesh view: tint the skin cells (within wall thickness of the surface). */
  meshSkinTint: boolean;
  // optimization inputs
  budget: number; // infill budget: target mean interior density in %
  smoothIters: number; // Taubin passes on modifier regions
  nBins: number;
  /** Optimization goal: stiffest at a mass budget, or lightest at a
   *  target stiffness ("as stiff as uniform X%"). */
  goal: "budget" | "match";
  /** Graded densities vs binary (hollow/solid core). */
  optMode: "graded" | "binary";
  /** Solid-fill pattern written to the 3MF in binary mode. */
  solidPattern: "default" | "rectilinear" | "concentric";
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
  viewMode: ViewMode;
  deformScale: number;
  animateDeformed: boolean;
  /** Display autoscale chosen by the viewer (deformation exaggeration base). */
  autoScale: number;
  voxelInfo: VoxelInfo | null;
  voxelMeshReady: boolean;
  settingsOpen: boolean;
  /** Densities of the extracted modifier regions (for the region list). */
  regionInfos: { density: number }[];
  regionVisible: boolean[];
  /** Density cutaway threshold in %, 0 = off (surface paint only). */
  densityThreshold: number;
  /** Result field shown in the Deformed view ("u" or a stress/strain kind). */
  resultField: string;
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

  setActiveStep(n: number): void;
  loadFile(name: string, bytes: ArrayBuffer): Promise<void>;
  setSegAngle(angle: number): Promise<void>;
  setTool(tool: Tool): void;
  setBrushRadius(r: number): void;
  setBrushErase(on: boolean): void;
  addBc(kind: BcKind): void;
  removeBc(id: string): void;
  setActiveBc(id: string | null): void;
  updateBcTris(id: string, tris: Uint32Array): void;
  updateBcParams(id: string, params: Partial<Pick<Bc, "force" | "pressure" | "stiffness">>): void;
  setMaterial(m: Material): void;
  updateMaterial(index: number, m: Material): void;
  addMaterial(): void;
  removeMaterial(index: number): void;
  resetMaterials(): void;
  setCurve(pattern: PatternKey, c: PatternCurve): void;
  resetCurves(): void;
  openSettings(open: boolean): void;
  setResolution(r: ResolutionKey): void;
  setBudget(v: number): void;
  setPattern(p: PatternKey): void;
  setPerimeters(v: number): void;
  setLineWidth(v: number): void;
  setPrintInfill(v: number): void;
  setSnapVoxel(on: boolean): void;
  setAnalyzeMode(m: "printed" | "solid"): void;
  setMeshSkinTint(on: boolean): void;
  /** Scene → store: the section plane moved (three.js plane convention). */
  onSectionPlaneMoved(normal: [number, number, number], constant: number): void;
  setSmoothIters(v: number): void;
  setNBins(v: number): void;
  setGoal(g: "budget" | "match"): void;
  setOptMode(m: "graded" | "binary"): void;
  setSolidPattern(p: "default" | "rectilinear" | "concentric"): void;
  updateLevelSettings(p: Partial<LevelSettings>): void;
  setRegionVisible(index: number, on: boolean): void;
  setDensityThreshold(v: number): void;
  setResultField(kind: string): Promise<void>;
  setLegendRange(min: number | null, max: number | null): void;
  setShowExtremes(on: boolean): void;
  toggleSection(): void;
  flipSection(): void;
  setSectionAxis(axis: "x" | "y" | "z"): void;
  setLogOpen(open: boolean): void;
  clearLog(): void;
  runCheck(): Promise<void>;
  runSolve(): Promise<void>;
  runOptimize(): Promise<void>;
  downloadThreeMf(): Promise<void>;
  downloadStls(): Promise<void>;
  setViewMode(mode: ViewMode): Promise<void>;
  setDeformScale(s: number): void;
  setAnimateDeformed(on: boolean): void;
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
  /** Cell layers resolving the skin (k after voxel snapping). */
  skinLayers: number;
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
let isoTimer: ReturnType<typeof setTimeout> | null = null;
let smoothTimer: ReturnType<typeof setTimeout> | null = null;
let meshCutTimer: ReturnType<typeof setTimeout> | null = null;
/** Last section plane reported by the scene (three.js convention:
 *  kept side is normal·p + constant ≥ 0). */
let lastSectionPlane: { normal: [number, number, number]; constant: number } | null = null;
/** Per-kind cache of fetched stress/strain fields (cleared on invalidation). */
const fieldCache = new Map<string, Float32Array>();

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
          `${info.solid.toLocaleString()} solid of ${info.cells.toLocaleString()} cells`
      );
    }
    set({ voxelInfo: info });
  } catch {
    // grid not buildable yet — the caller surfaces the real error
  }
}

function fieldUnit(kind: string): string {
  if (kind.startsWith("sf")) return "×"; // marker labels show a plain factor
  return RESULT_FIELDS.find((f) => f.value === kind)?.unit ?? "";
}

/** Events the 3D scene listens to (kept out of React rendering). */
export interface SceneEvents {
  onModelLoaded?: (m: LoadedModel) => void;
  onPatchIdsChanged?: (patchIds: Uint32Array) => void;
  onBcsChanged?: (bcs: Bc[], activeBcId: string | null) => void;
  onAnimateMode?: (mode: { t: number[]; r: number[]; center: number[] } | null) => void;
  onDisplacements?: (disp: Float32Array | null, stats: { maxDisplacement: number } | null) => void;
  onVertexDensity?: (density: Float32Array | null) => void;
  onRegions?: (regions: OptRegion[] | null) => void;
  onViewState?: (mode: ViewMode, deformScale: number) => void;
  onVoxelMesh?: (
    hull: Float32Array | null,
    edges: Float32Array | null,
    skin?: Float32Array | null
  ) => void;
  /** Tint skin cells in the mesh view. */
  onMeshSkinTint?: (on: boolean) => void;
  /** Voxel-true section active: the scene must NOT plane-clip the voxel
   *  group (the cut already lives in the geometry) and hides its cap. */
  onVoxelCutActive?: (on: boolean) => void;
  onAnimateDeformed?: (on: boolean) => void;
  /** Live optimization skeleton or density-threshold cutaway mesh,
   *  optionally colored by a per-vertex density scalar. */
  onOptShape?: (
    positions: Float32Array | null,
    indices: Uint32Array | null,
    density?: Float32Array | null
  ) => void;
  onRegionVisibility?: (visible: boolean[]) => void;
  /** Stress/strain scalars for the deformed view (null = |u| colors).
   *  flip inverts the colormap (safety factor: red = critical LOW). */
  onScalarField?: (values: Float32Array | null, flip?: boolean) => void;
  /** User override of the color-scale range (nulls = auto). */
  onLegendRange?: (min: number | null, max: number | null) => void;
  /** Min/max location markers; unit drives the label formatting. */
  onShowExtremes?: (on: boolean, unit: string) => void;
  // Section plane controls.
  onSectionState?: (on: boolean) => void;
  onSectionFlip?: () => void;
  onSectionAxis?: (axis: "x" | "y" | "z") => void;
}

export const sceneEvents: SceneEvents = {};

async function pushBcs(get: () => AppState) {
  await engine.setBcs(get().bcs);
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
  const wall = st.perimeters * st.lineWidth;
  const cutting = st.sectionOn && lastSectionPlane !== null;
  try {
    const { hull, edges, skin, info } = await engine.voxelMeshCut(
      cutting ? lastSectionPlane : null,
      wall
    );
    if (get().viewMode !== "mesh") return true; // user moved on mid-fetch
    set({ voxelInfo: info, voxelMeshReady: true });
    sceneEvents.onVoxelCutActive?.(cutting);
    sceneEvents.onVoxelMesh?.(hull, edges, skin);
    return true;
  } catch (e) {
    set({ error: e instanceof Error ? e.message : String(e) });
    return false;
  }
}

function invalidateResults(set: (p: Partial<AppState>) => void, get: () => AppState) {
  set({
    check: null,
    stats: null,
    hasResult: false,
    optSummary: null,
    printedStats: null,
    regionInfos: [],
    regionVisible: [],
    densityThreshold: 0,
    resultField: "u",
    fieldRange: null,
    legendMin: null,
    legendMax: null,
  });
  fieldCache.clear();
  sceneEvents.onLegendRange?.(null, null);
  sceneEvents.onScalarField?.(null);
  sceneEvents.onRegions?.(null);
  sceneEvents.onVertexDensity?.(null);
  sceneEvents.onDisplacements?.(null, null);
  sceneEvents.onOptShape?.(null, null);
  if (get().viewMode !== "setup" && get().viewMode !== "mesh") {
    set({ viewMode: "setup" });
    sceneEvents.onViewState?.("setup", get().deformScale);
  }
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

export const useStore = create<AppState>((set, get) => ({
  activeStep: 1,
  fileName: null,
  model: null,
  segAngle: 10,
  tool: "orbit",
  brushRadius: 3,
  brushErase: false,
  bcs: [],
  activeBcId: null,
  material: initialSettings.materials[0],
  materials: initialSettings.materials,
  curves: initialSettings.curves,
  resolution: "preview",
  budget: 25,
  pattern: "gyroid",
  perimeters: 2,
  lineWidth: 0.45,
  printInfill: 25,
  snapVoxel: true,
  analyzeMode: "printed",
  printedStats: null,
  meshSkinTint: false,
  smoothIters: 15,
  nBins: 3,
  goal: "budget",
  optMode: "graded",
  solidPattern: "default",
  levelSettings: initialSettings.levels,
  busy: null,
  error: null,
  notice: null,
  check: null,
  stats: null,
  hasResult: false,
  optProgress: null,
  optSummary: null,
  viewMode: "setup",
  deformScale: 1,
  animateDeformed: false,
  autoScale: 1,
  voxelInfo: null,
  voxelMeshReady: false,
  settingsOpen: false,
  regionInfos: [],
  regionVisible: [],
  densityThreshold: 0,
  resultField: "u",
  fieldRange: null,
  legendMin: null,
  legendMax: null,
  showExtremes: false,
  sectionOn: false,
  logOpen: false,
  logLines: [],
  optSeries: [],
  solveResiduals: [],

  setActiveStep(n) {
    set({ activeStep: Math.min(6, Math.max(1, Math.round(n))) });
  },

  async loadFile(name, bytes) {
    set({ busy: "Parsing & segmenting…", error: null, notice: null });
    try {
      const model = await engine.load(bytes, name.replace(/\.(stl|3mf)$/i, ""));
      const m = get().material;
      await engine.setMaterial(m.e0, m.nu, m.density, m.strength, m.strengthZ);
      await engine.setResolution(RESOLUTIONS[get().resolution]);
      // A fresh wasm Model defaults to snap off; push the current setting.
      // (Inline, not pushSnap: the store's `model` isn't set yet.)
      await engine.setSnapWall(
        get().snapVoxel ? get().perimeters * get().lineWidth : 0
      );
      set({
        fileName: name,
        model,
        // a fresh model means fresh supports & loads — jump there
        activeStep: 2,
        bcs: [],
        activeBcId: null,
        tool: "orbit",
        check: null,
        stats: null,
        hasResult: false,
        optSummary: null,
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
          (model as LoadedModel & { meshObjects?: number }).meshObjects &&
          (model as LoadedModel & { meshObjects?: number }).meshObjects! > 1
            ? "3MF contained multiple meshes — analyzing the largest body only."
            : null,
      });
      fieldCache.clear();
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
      const [bx, by, bz] = [
        model.bbox[3] - model.bbox[0],
        model.bbox[4] - model.bbox[1],
        model.bbox[5] - model.bbox[2],
      ];
      appendLog(
        set,
        `Loaded "${name}" — ${model.triCount.toLocaleString()} display triangles, bbox ${bx.toFixed(1)}×${by.toFixed(1)}×${bz.toFixed(1)} mm`
      );
    } catch (e) {
      set({ busy: null, error: e instanceof Error ? e.message : String(e) });
    }
  },

  async setSegAngle(angle) {
    set({ segAngle: angle });
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

  setTool(tool) {
    set({ tool });
  },
  setBrushRadius(r) {
    set({ brushRadius: r });
  },
  setBrushErase(on) {
    set({ brushErase: on });
  },

  addBc(kind) {
    const bc: Bc = {
      id: `bc${++bcCounter}`,
      kind,
      tris: new Uint32Array(0),
      force: kind === "force" ? [0, 0, -10] : undefined,
      pressure: kind === "pressure" ? 0.1 : undefined,
      // ~printed-plastic mount; bolted-to-steel would be >= 5000 (≈ fixed).
      stiffness: kind === "elastic" ? 100 : undefined,
    };
    set({ bcs: [...get().bcs, bc], activeBcId: bc.id, tool: "select" });
    invalidateResults(set, get);
    sceneEvents.onBcsChanged?.(get().bcs, bc.id);
  },

  removeBc(id) {
    set({
      bcs: get().bcs.filter((b) => b.id !== id),
      activeBcId: get().activeBcId === id ? null : get().activeBcId,
    });
    if (get().bcs.length === 0) set({ tool: "orbit" });
    invalidateResults(set, get);
    sceneEvents.onBcsChanged?.(get().bcs, get().activeBcId);
    void pushBcs(get);
  },

  setActiveBc(id) {
    set({ activeBcId: id });
    if (id === null) set({ tool: "orbit" });
    sceneEvents.onBcsChanged?.(get().bcs, id);
  },

  updateBcTris(id, tris) {
    set({ bcs: get().bcs.map((b) => (b.id === id ? { ...b, tris } : b)) });
    invalidateResults(set, get);
    sceneEvents.onBcsChanged?.(get().bcs, get().activeBcId);
    void pushBcs(get);
  },

  updateBcParams(id, params) {
    set({ bcs: get().bcs.map((b) => (b.id === id ? { ...b, ...params } : b)) });
    invalidateResults(set, get);
    sceneEvents.onBcsChanged?.(get().bcs, get().activeBcId);
    void pushBcs(get);
  },

  setMaterial(m) {
    set({ material: m });
    invalidateResults(set, get);
    void engine.setMaterial(m.e0, m.nu, m.density, m.strength, m.strengthZ);
  },

  updateMaterial(index, m) {
    const mats = get().materials.slice();
    const wasSelected = mats[index]?.name === get().material.name;
    mats[index] = m;
    set({ materials: mats });
    saveSettings(mats, get().curves, get().levelSettings);
    if (wasSelected) {
      set({ material: m });
      invalidateResults(set, get);
      void engine.setMaterial(m.e0, m.nu, m.density, m.strength, m.strengthZ);
    }
  },

  addMaterial() {
    const mats = [
      ...get().materials,
      { name: "Custom", e0: 2000, nu: 0.35, density: 1.2, strength: 40, strengthZ: 28 },
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

  setResolution(r) {
    set({ resolution: r });
    invalidateResults(set, get);
    invalidateGrid(set, get);
    void engine.setResolution(RESOLUTIONS[r]);
  },

  setBudget(v) {
    // Infill budget: mean interior density, bounded by the printable band
    // of the active mode (graded: floor..cap; binary: binary floor..90).
    const [lo, hi] = budgetBounds(get());
    set({ budget: Math.min(hi, Math.max(lo, Math.round(v))) });
  },
  setPattern(p) {
    // The pattern law feeds the next solve/optimize; a shown printed result
    // would no longer match it.
    set({ pattern: p, printedStats: null });
  },
  setPerimeters(v) {
    set({ perimeters: Math.min(8, Math.max(1, Math.round(v))), printedStats: null });
    if (get().snapVoxel) {
      // The wall changed: with snapping on the engine rebuilds the grid.
      invalidateResults(set, get);
      invalidateGrid(set, get);
    }
    void pushSnap(get);
  },
  setLineWidth(v) {
    set({ lineWidth: Math.min(1.5, Math.max(0.1, v)), printedStats: null });
    if (get().snapVoxel) {
      invalidateResults(set, get);
      invalidateGrid(set, get);
    }
    void pushSnap(get);
  },
  setPrintInfill(v) {
    const pct = Math.min(100, Math.max(5, Math.round(v)));
    set({ printInfill: pct, printedStats: null });
    // "Here's your print today — now beat it": the optimizer's budget
    // follows the print setting (still clamped to its own band).
    get().setBudget(pct);
  },
  setSnapVoxel(on) {
    set({ snapVoxel: on });
    // The engine drops grid + results when the snap value actually changes.
    invalidateResults(set, get);
    invalidateGrid(set, get);
    void pushSnap(get);
  },
  setAnalyzeMode(m) {
    set({ analyzeMode: m });
  },

  setMeshSkinTint(on) {
    set({ meshSkinTint: on });
    sceneEvents.onMeshSkinTint?.(on);
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
  },

  setGoal(g) {
    set({ goal: g });
    get().setBudget(get().budget); // re-clamp to the goal's band
  },

  setOptMode(m) {
    set({ optMode: m });
    get().setBudget(get().budget); // re-clamp to the mode's printable band
  },

  setSolidPattern(p) {
    set({ solidPattern: p });
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
        const st = get();
        if (!st.optSummary) return;
        if (v < 10) {
          // Below the printable floor everything is "inside" — cutaway off.
          sceneEvents.onOptShape?.(null, null);
          return;
        }
        try {
          const { positions, indices, density } = await engine.densityShape(v / 100);
          if (get().densityThreshold === v) sceneEvents.onOptShape?.(positions, indices, density);
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

  async runCheck() {
    if (!get().model) return;
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
                .map((c, i) => `#${i + 1}: ${c.cells.toLocaleString()} cells, ${c.constrained ? "ok" : "free"}`)
                .join(", ")
      );
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      set({ busy: null, error: msg });
      appendLog(set, `Check failed: ${msg}`);
    }
  },

  async runSolve() {
    if (!get().model) return;
    set({ busy: "Solving…", error: null });
    sceneEvents.onAnimateMode?.(null);
    try {
      await pushBcs(get);
      await logGridInfo(set);
      const report = await engine.check();
      set({ check: report });
      if (!report.ok) {
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
      const st0 = get();
      const m = st0.material;
      const printed = st0.analyzeMode === "printed";
      const curve = st0.curves[st0.pattern];
      let printedSummary: PrintedSummary | null = null;
      let stats: SolveStats;
      let displacements: Float32Array;
      if (printed) {
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
        });
        stats = out.stats;
        displacements = out.displacements;
        printedSummary = {
          massGrams: out.stats.massGrams,
          massSolidGrams: out.stats.massSolidGrams,
          skinLayers: out.stats.skinLayers,
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
            `skin resolved by ${out.stats.skinLayers} cell layer${out.stats.skinLayers === 1 ? "" : "s"}`
        );
      } else {
        appendLog(set, `Solve solid: ${m.name} (E₀ ${m.e0} MPa, ν ${m.nu}) …`);
        const out = await engine.solve();
        stats = out.stats;
        displacements = out.displacements;
      }
      fieldCache.clear(); // stress fields belong to the previous solution
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
        hasResult: true,
        viewMode: "deformed",
        busy: null,
        resultField: "u",
        fieldRange: null,
        legendMin: null,
        legendMax: null,
        notice: stats.converged
          ? printedSummary && printedSummary.skinLayers === 1
            ? "The wall is only one voxel layer thick at this resolution — printed-mode results are coarse. Raise the resolution in Properties."
            : null
          : `Solver stopped at the iteration cap (residual ${stats.relResidual.toExponential(1)}) — the shown result is a close approximation. Preview resolution converges faster.`,
      });
      sceneEvents.onScalarField?.(null);
      sceneEvents.onDisplacements?.(displacements, stats);
      sceneEvents.onViewState?.("deformed", get().deformScale);
      if (printedSummary) {
        // Min safety factors for the dock — both limits, so the dock can say
        // WHICH one governs. Fields are cached: picking them in the viewer
        // afterwards is instant.
        try {
          const [sfm, sfz] = await Promise.all([
            engine.resultField("sfm"),
            engine.resultField("sfz"),
          ]);
          fieldCache.set("sfm", sfm);
          fieldCache.set("sfz", sfz);
          let minM = Infinity;
          let minZ = Infinity;
          for (let i = 0; i < sfm.length; i++) minM = Math.min(minM, sfm[i]);
          for (let i = 0; i < sfz.length; i++) minZ = Math.min(minZ, sfz[i]);
          const minSf = Math.min(minM, minZ);
          if (Number.isFinite(minSf) && get().printedStats) {
            const governs = minZ < minM ? "layer" : "material";
            set({ printedStats: { ...get().printedStats!, minSf, sfGoverns: governs } });
            appendLog(
              set,
              `  min safety factor ${minSf.toFixed(2)}× — ` +
                (governs === "layer"
                  ? `layer adhesion governs (σₜᶻ ${m.strengthZ} MPa vs σzz tension)`
                  : `material governs (σₜ ${m.strength} MPa vs σᵥᴹ)`)
            );
          }
        } catch {
          // result vanished mid-fetch: the dock shows mass/deflection only
        }
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      set({ busy: null, error: msg });
      appendLog(set, `Solve failed: ${msg}`);
    }
  },

  async runOptimize() {
    const st = get();
    if (!st.model) return;
    set({
      busy: "Optimizing infill…",
      error: null,
      optProgress: null,
      optSummary: null,
      printedStats: null,
      optSeries: [],
    });
    sceneEvents.onAnimateMode?.(null);
    try {
      await pushBcs(get);
      await logGridInfo(set);
      const curve = st.curves[st.pattern];
      const binary = st.optMode === "binary";
      const match = st.goal === "match";
      const ls = st.levelSettings;
      const manual = !binary && ls.mode === "manual" && ls.manual.length >= 2;
      appendLog(
        set,
        `Optimize (${binary ? `binary: ${ls.binaryFloorPct}% or solid` : manual ? `manual levels ${ls.manual.join("/")}%` : "graded, auto levels"}): ` +
          (match
            ? `match the stiffness of uniform ${st.budget}% — lightest design via budget secant`
            : `infill budget ${st.budget}%`) +
          ` (${st.pattern}: E/E₀ = ${curve.coeff}·ρ^${curve.exponent}), ` +
          `skin ${st.perimeters}×${st.lineWidth} mm — convergence when mean |Δρ| < 0.005 twice` +
          (binary ? " · optimizer SIMP-penalized p=3" : "")
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
          solidPattern: binary && st.solidPattern !== "default" ? st.solidPattern : null,
          goal: st.goal,
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
            `  ${p.passes > 1 ? `p${p.pass} ` : ""}it ${String(p.iteration).padStart(2)}: C ${p.compliance.toExponential(3)} N·mm · ` +
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
          `(${out.summary.seconds.toFixed(1)} s) · levels ${out.summary.bins.map((b) => `${Math.round(b.density * 100)}%`).join("/")} · ` +
          `mean infill ${(out.summary.meanInfill * 100).toFixed(1)}% · mass ${out.summary.massGrams.toFixed(1)} g (${Math.round(out.summary.massFrac * 100)}% of solid)`
      );
      appendLog(
        set,
        `  verification: stiffness ${Math.round(out.summary.stiffnessVsSolid * 100)}% of solid · ` +
          `+${(out.summary.gainVsUniform * 100).toFixed(1)}% stiffer than uniform ${Math.round(out.summary.meanInfill * 100)}% infill at equal weight`
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
      fieldCache.clear(); // stress fields belong to the previous solution
      sceneEvents.onLegendRange?.(null, null);
      set({
        resultField: "u",
        fieldRange: null,
        legendMin: null,
        legendMax: null,
        optSummary: out.summary,
        optProgress: null,
        busy: null,
        viewMode: "density",
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
      sceneEvents.onDisplacements?.(out.displacements, {
        maxDisplacement: out.summary.maxDisplacement,
      });
      sceneEvents.onRegions?.(out.regions);
      sceneEvents.onRegionVisibility?.(vis);
      // Land in the density view with a 25% cutaway by default — the
      // interior structure is the result, not the painted surface.
      sceneEvents.onViewState?.("density", get().deformScale);
      get().setDensityThreshold(25);
    } catch (e) {
      sceneEvents.onOptShape?.(null, null);
      const msg = e instanceof Error ? e.message : String(e);
      set({ busy: null, optProgress: null, error: msg });
      appendLog(set, `Optimize failed: ${msg}`);
    }
  },

  async downloadThreeMf() {
    try {
      const bytes = await engine.exportThreeMf();
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

  async setViewMode(mode) {
    if (mode === "mesh") {
      const first = !get().voxelMeshReady;
      if (first) set({ busy: "Building analysis mesh…", error: null });
      const prev = get().viewMode;
      set({ viewMode: "mesh" });
      sceneEvents.onViewState?.("mesh", get().deformScale);
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
    if (kind === "u") {
      set({ fieldRange: null });
      sceneEvents.onScalarField?.(null);
      return;
    }
    try {
      let values = fieldCache.get(kind);
      if (!values) {
        values = await engine.resultField(kind);
        fieldCache.set(kind, values);
      }
      if (get().resultField !== kind) return; // user moved on mid-fetch
      let min = Infinity;
      let max = -Infinity;
      for (let i = 0; i < values.length; i++) {
        min = Math.min(min, values[i]);
        max = Math.max(max, values[i]);
      }
      set({ fieldRange: { min, max } });
      // Safety factor: invert the colormap so red marks the critical LOW.
      sceneEvents.onScalarField?.(values, kind.startsWith("sf"));
    } catch (e) {
      set({ error: e instanceof Error ? e.message : String(e), resultField: "u", fieldRange: null });
      sceneEvents.onScalarField?.(null);
    }
  },

  setLegendRange(min, max) {
    if (min !== null && max !== null && !(max > min)) return; // ignore inverted
    set({ legendMin: min, legendMax: max });
    sceneEvents.onLegendRange?.(min, max);
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
  },

  flipSection() {
    sceneEvents.onSectionFlip?.();
  },

  setSectionAxis(axis) {
    sceneEvents.onSectionAxis?.(axis);
  },

  clearError() {
    set({ error: null, notice: null });
  },
}));
