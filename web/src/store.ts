import { create } from "zustand";
import { engine, type OptRegion, type OptSummary } from "./engine/EngineClient";
import type {
  Bc,
  BcKind,
  CheckReport,
  LoadedModel,
  Material,
  ResolutionKey,
  SolveStats,
} from "./types";
import { MATERIALS, RESOLUTIONS } from "./types";

export type Tool = "orbit" | "select" | "brush";
export type ViewMode = "setup" | "deformed" | "density" | "infill";

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
  gravity: boolean;
  resolution: ResolutionKey;
  // optimization inputs
  budget: number; // % of solid mass
  pattern: "gyroid" | "cubic" | "grid";
  wallMm: number;
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

  loadFile(name: string, bytes: ArrayBuffer): Promise<void>;
  setSegAngle(angle: number): Promise<void>;
  setTool(tool: Tool): void;
  setBrushRadius(r: number): void;
  setBrushErase(on: boolean): void;
  addBc(kind: BcKind): void;
  removeBc(id: string): void;
  setActiveBc(id: string | null): void;
  updateBcTris(id: string, tris: Uint32Array): void;
  updateBcParams(id: string, params: Partial<Pick<Bc, "force" | "pressure">>): void;
  setMaterial(m: Material): void;
  setGravity(on: boolean): void;
  setResolution(r: ResolutionKey): void;
  setBudget(v: number): void;
  setPattern(p: "gyroid" | "cubic" | "grid"): void;
  setWallMm(v: number): void;
  setNBins(v: number): void;
  runCheck(): Promise<void>;
  runSolve(): Promise<void>;
  runOptimize(): Promise<void>;
  downloadThreeMf(): Promise<void>;
  downloadStls(): Promise<void>;
  setViewMode(mode: ViewMode): void;
  setDeformScale(s: number): void;
  clearError(): void;
}

let bcCounter = 0;

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
}

export const sceneEvents: SceneEvents = {};

async function pushBcs(get: () => AppState) {
  await engine.setBcs(get().bcs);
}

function invalidateResults(set: (p: Partial<AppState>) => void, get: () => AppState) {
  set({ check: null, stats: null, hasResult: false, optSummary: null });
  sceneEvents.onRegions?.(null);
  sceneEvents.onVertexDensity?.(null);
  sceneEvents.onDisplacements?.(null, null);
  if (get().viewMode !== "setup") {
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
  material: MATERIALS[0],
  gravity: false,
  resolution: "preview",
  budget: 50,
  pattern: "gyroid",
  wallMm: 0.9,
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

  async loadFile(name, bytes) {
    set({ busy: "Parsing & segmenting…", error: null, notice: null });
    try {
      const model = await engine.load(bytes, name.replace(/\.(stl|3mf)$/i, ""));
      const m = get().material;
      await engine.setMaterial(m.e0, m.nu, m.density);
      await engine.setGravity(get().gravity);
      await engine.setResolution(RESOLUTIONS[get().resolution]);
      set({
        fileName: name,
        model,
        bcs: [],
        activeBcId: null,
        check: null,
        stats: null,
        hasResult: false,
        optSummary: null,
        optProgress: null,
        viewMode: "setup",
        busy: null,
        notice:
          (model as LoadedModel & { meshObjects?: number }).meshObjects &&
          (model as LoadedModel & { meshObjects?: number }).meshObjects! > 1
            ? "3MF contained multiple meshes — analyzing the largest body only."
            : null,
      });
      sceneEvents.onModelLoaded?.(model);
      sceneEvents.onBcsChanged?.([], null);
      sceneEvents.onDisplacements?.(null, null);
      sceneEvents.onVertexDensity?.(null);
      sceneEvents.onRegions?.(null);
      sceneEvents.onAnimateMode?.(null);
      sceneEvents.onViewState?.("setup", get().deformScale);
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
    invalidateResults(set, get);
    sceneEvents.onBcsChanged?.(get().bcs, get().activeBcId);
    void pushBcs(get);
  },

  setActiveBc(id) {
    set({ activeBcId: id });
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

  setGravity(on) {
    set({ gravity: on });
    invalidateResults(set, get);
    void engine.setGravity(on);
  },

  setResolution(r) {
    set({ resolution: r });
    invalidateResults(set, get);
    void engine.setResolution(RESOLUTIONS[r]);
  },

  setBudget(v) {
    set({ budget: v });
  },
  setPattern(p) {
    set({ pattern: p });
  },
  setWallMm(v) {
    set({ wallMm: v });
  },
  setNBins(v) {
    set({ nBins: v });
  },

  async runCheck() {
    if (!get().model) return;
    set({ busy: "Voxelizing & checking constraints…", error: null });
    try {
      await pushBcs(get);
      const report = await engine.check();
      set({ check: report, busy: null });
      const bad = report.components.find((c) => !c.constrained && c.mode);
      sceneEvents.onAnimateMode?.(bad?.mode ?? null);
    } catch (e) {
      set({ busy: null, error: e instanceof Error ? e.message : String(e) });
    }
  },

  async runSolve() {
    if (!get().model) return;
    set({ busy: "Solving…", error: null });
    sceneEvents.onAnimateMode?.(null);
    try {
      await pushBcs(get);
      const report = await engine.check();
      set({ check: report });
      if (!report.ok) {
        const bad = report.components.find((c) => !c.constrained && c.mode);
        sceneEvents.onAnimateMode?.(bad?.mode ?? null);
        set({
          busy: null,
          error:
            report.islandCount > 1
              ? `Model has ${report.islandCount} disconnected parts and is under-constrained — see the animated motion.`
              : "Model is under-constrained — the animation shows the free motion. Add or extend supports.",
        });
        return;
      }
      const { stats, displacements } = await engine.solve();
      set({ stats, hasResult: true, viewMode: "deformed", busy: null });
      sceneEvents.onDisplacements?.(displacements, stats);
      sceneEvents.onViewState?.("deformed", get().deformScale);
    } catch (e) {
      set({ busy: null, error: e instanceof Error ? e.message : String(e) });
    }
  },

  async runOptimize() {
    const st = get();
    if (!st.model) return;
    set({ busy: "Optimizing infill…", error: null, optProgress: null, optSummary: null });
    sceneEvents.onAnimateMode?.(null);
    try {
      await pushBcs(get);
      const out = await engine.optimize(
        st.budget,
        st.pattern,
        st.wallMm,
        st.nBins,
        (p, density) => {
          set({ optProgress: { iteration: p.iteration, maxIter: p.maxIter } });
          if (get().viewMode !== "density") {
            set({ viewMode: "density" });
            sceneEvents.onViewState?.("density", get().deformScale);
          }
          sceneEvents.onVertexDensity?.(density);
        }
      );
      set({
        optSummary: out.summary,
        optProgress: null,
        busy: null,
        viewMode: "infill",
        stats: {
          iterations: out.summary.iterations,
          relResidual: 0,
          maxDisplacement: out.summary.maxDisplacement,
          seconds: out.summary.seconds,
        },
        hasResult: true,
      });
      sceneEvents.onVertexDensity?.(out.vertexDensity);
      sceneEvents.onDisplacements?.(out.displacements, {
        maxDisplacement: out.summary.maxDisplacement,
      });
      sceneEvents.onRegions?.(out.regions);
      sceneEvents.onViewState?.("infill", get().deformScale);
    } catch (e) {
      set({ busy: null, optProgress: null, error: e instanceof Error ? e.message : String(e) });
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

  setViewMode(mode) {
    set({ viewMode: mode });
    sceneEvents.onViewState?.(mode, get().deformScale);
  },

  setDeformScale(s) {
    set({ deformScale: s });
    sceneEvents.onViewState?.(get().viewMode, s);
  },

  clearError() {
    set({ error: null, notice: null });
  },
}));
