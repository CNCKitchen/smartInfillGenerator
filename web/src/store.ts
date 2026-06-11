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

interface PersistedSettings {
  materials: Material[];
  curves: Record<PatternKey, PatternCurve>;
}

function loadSettings(): PersistedSettings {
  const fallback: PersistedSettings = {
    materials: DEFAULT_MATERIALS.map((m) => ({ ...m })),
    curves: {
      gyroid: { ...DEFAULT_CURVES.gyroid },
      cubic: { ...DEFAULT_CURVES.cubic },
      grid: { ...DEFAULT_CURVES.grid },
    },
  };
  try {
    const raw = localStorage.getItem(SETTINGS_KEY);
    if (!raw) return fallback;
    const p = JSON.parse(raw) as Partial<PersistedSettings>;
    if (Array.isArray(p.materials) && p.materials.length) {
      fallback.materials = p.materials
        .filter((m) => m && typeof m.e0 === "number" && m.e0 > 0)
        .map((m) => ({ name: String(m.name), e0: m.e0, nu: m.nu, density: m.density }));
      if (!fallback.materials.length) fallback.materials = DEFAULT_MATERIALS.map((m) => ({ ...m }));
    }
    for (const k of ["gyroid", "cubic", "grid"] as PatternKey[]) {
      const c = p.curves?.[k];
      if (c && typeof c.coeff === "number" && typeof c.exponent === "number") {
        fallback.curves[k] = { coeff: c.coeff, exponent: c.exponent };
      }
    }
  } catch {
    // corrupted storage: keep defaults
  }
  return fallback;
}

function saveSettings(materials: Material[], curves: Record<PatternKey, PatternCurve>) {
  try {
    localStorage.setItem(SETTINGS_KEY, JSON.stringify({ materials, curves }));
  } catch {
    // storage full/blocked: settings just won't persist
  }
}

const initialSettings = loadSettings();

interface AppState {
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
  // optimization inputs
  budget: number; // infill budget: target mean interior density in % (10–70)
  pattern: PatternKey;
  perimeters: number;
  lineWidth: number; // mm
  smoothIters: number; // Taubin passes on modifier regions
  nBins: number;
  // run state
  busy: string | null;
  error: string | null;
  notice: string | null;
  check: CheckReport | null;
  stats: SolveStats | null;
  hasResult: boolean;
  optProgress: { iteration: number; maxIter: number } | null;
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
  setSmoothIters(v: number): void;
  setNBins(v: number): void;
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
  onVoxelMesh?: (hull: Float32Array | null, edges: Float32Array | null) => void;
  onAnimateDeformed?: (on: boolean) => void;
  /** Live optimization skeleton or density-threshold cutaway mesh,
   *  optionally colored by a per-vertex density scalar. */
  onOptShape?: (
    positions: Float32Array | null,
    indices: Uint32Array | null,
    density?: Float32Array | null
  ) => void;
  onRegionVisibility?: (visible: boolean[]) => void;
  /** Stress/strain scalars for the deformed view (null = |u| colors). */
  onScalarField?: (values: Float32Array | null) => void;
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

function invalidateResults(set: (p: Partial<AppState>) => void, get: () => AppState) {
  set({
    check: null,
    stats: null,
    hasResult: false,
    optSummary: null,
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
  fileName: null,
  model: null,
  segAngle: 30,
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
  smoothIters: 15,
  nBins: 3,
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

  async loadFile(name, bytes) {
    set({ busy: "Parsing & segmenting…", error: null, notice: null });
    try {
      const model = await engine.load(bytes, name.replace(/\.(stl|3mf)$/i, ""));
      const m = get().material;
      await engine.setMaterial(m.e0, m.nu, m.density);
      await engine.setResolution(RESOLUTIONS[get().resolution]);
      set({
        fileName: name,
        model,
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
    void engine.setMaterial(m.e0, m.nu, m.density);
  },

  updateMaterial(index, m) {
    const mats = get().materials.slice();
    const wasSelected = mats[index]?.name === get().material.name;
    mats[index] = m;
    set({ materials: mats });
    saveSettings(mats, get().curves);
    if (wasSelected) {
      set({ material: m });
      invalidateResults(set, get);
      void engine.setMaterial(m.e0, m.nu, m.density);
    }
  },

  addMaterial() {
    const mats = [...get().materials, { name: "Custom", e0: 2000, nu: 0.35, density: 1.2 }];
    set({ materials: mats });
    saveSettings(mats, get().curves);
  },

  removeMaterial(index) {
    const mats = get().materials.filter((_, i) => i !== index);
    if (!mats.length) return;
    const removedSelected = get().materials[index]?.name === get().material.name;
    set({ materials: mats });
    saveSettings(mats, get().curves);
    if (removedSelected) get().setMaterial(mats[0]);
  },

  resetMaterials() {
    const mats = DEFAULT_MATERIALS.map((m) => ({ ...m }));
    set({ materials: mats });
    saveSettings(mats, get().curves);
    const sel = mats.find((m) => m.name === get().material.name) ?? mats[0];
    get().setMaterial(sel);
  },

  setCurve(pattern, c) {
    const curves = { ...get().curves, [pattern]: c };
    set({ curves });
    saveSettings(get().materials, curves);
  },

  resetCurves() {
    const curves = {
      gyroid: { ...DEFAULT_CURVES.gyroid },
      cubic: { ...DEFAULT_CURVES.cubic },
      grid: { ...DEFAULT_CURVES.grid },
    };
    set({ curves });
    saveSettings(get().materials, curves);
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
    // Infill budget: mean interior density, bounded by the printable band.
    set({ budget: Math.min(70, Math.max(10, Math.round(v))) });
  },
  setPattern(p) {
    set({ pattern: p });
  },
  setPerimeters(v) {
    set({ perimeters: Math.min(8, Math.max(1, Math.round(v))) });
  },
  setLineWidth(v) {
    set({ lineWidth: Math.min(1.5, Math.max(0.1, v)) });
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
      const m = get().material;
      appendLog(set, `Solve: ${m.name} (E₀ ${m.e0} MPa, ν ${m.nu}) …`);
      const { stats, displacements } = await engine.solve();
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
        solveResiduals: stats.residuals ?? [],
        hasResult: true,
        viewMode: "deformed",
        busy: null,
        resultField: "u",
        fieldRange: null,
        legendMin: null,
        legendMax: null,
        notice: stats.converged
          ? null
          : `Solver stopped at the iteration cap (residual ${stats.relResidual.toExponential(1)}) — the shown result is a close approximation. Preview resolution converges faster.`,
      });
      sceneEvents.onScalarField?.(null);
      sceneEvents.onDisplacements?.(displacements, stats);
      sceneEvents.onViewState?.("deformed", get().deformScale);
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
      optSeries: [],
    });
    sceneEvents.onAnimateMode?.(null);
    try {
      await pushBcs(get);
      await logGridInfo(set);
      const curve = st.curves[st.pattern];
      appendLog(
        set,
        `Optimize: infill budget ${st.budget}% (${st.pattern}: E/E₀ = ${curve.coeff}·ρ^${curve.exponent}), ` +
          `skin ${st.perimeters}×${st.lineWidth} mm, ${st.nBins} levels — ` +
          `convergence when mean |Δρ| < 0.005 twice`
      );
      const out = await engine.optimize(
        st.budget,
        curve.exponent,
        curve.coeff,
        st.perimeters,
        st.lineWidth,
        st.smoothIters,
        st.nBins,
        (p, density, skelPositions, skelIndices, skelDensity) => {
          set((s) => ({
            optProgress: { iteration: p.iteration, maxIter: p.maxIter },
            optSeries: [
              ...s.optSeries,
              {
                it: p.iteration,
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
          appendLog(
            set,
            `  it ${String(p.iteration).padStart(2)}: C ${p.compliance.toExponential(3)} N·mm · ` +
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
    if (mode === "mesh" && !get().voxelMeshReady) {
      set({ busy: "Building analysis mesh…", error: null });
      try {
        const { hull, edges, info } = await engine.voxelMesh();
        set({ voxelInfo: info, voxelMeshReady: true, busy: null });
        sceneEvents.onVoxelMesh?.(hull, edges);
      } catch (e) {
        set({ busy: null, error: e instanceof Error ? e.message : String(e) });
        return;
      }
    }
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
      sceneEvents.onScalarField?.(values);
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
