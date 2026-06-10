import { create } from "zustand";
import { engine } from "./engine/EngineClient";
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
  // run state
  busy: string | null;
  error: string | null;
  check: CheckReport | null;
  stats: SolveStats | null;
  hasResult: boolean;
  showDeformed: boolean;
  deformScale: number; // multiplier on auto scale, 0..3

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
  runCheck(): Promise<void>;
  runSolve(): Promise<void>;
  setShowDeformed(on: boolean): void;
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
  onDisplacements?: (disp: Float32Array | null, stats: SolveStats | null) => void;
  onDeformedView?: (show: boolean, scale: number) => void;
}

export const sceneEvents: SceneEvents = {};

async function pushBcs(get: () => AppState) {
  await engine.setBcs(get().bcs);
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
  resolution: "normal",
  busy: null,
  error: null,
  check: null,
  stats: null,
  hasResult: false,
  showDeformed: false,
  deformScale: 1,

  async loadFile(name, bytes) {
    set({ busy: "Parsing & segmenting…", error: null });
    try {
      const model = await engine.load(bytes);
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
        showDeformed: false,
        busy: null,
      });
      sceneEvents.onModelLoaded?.(model);
      sceneEvents.onBcsChanged?.([], null);
      sceneEvents.onDisplacements?.(null, null);
      sceneEvents.onAnimateMode?.(null);
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
      const next = { ...model, patchIds, patchCount };
      set({ model: next, busy: null });
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
    set({ bcs: [...get().bcs, bc], activeBcId: bc.id, tool: "select", hasResult: false, check: null });
    sceneEvents.onBcsChanged?.(get().bcs, bc.id);
  },

  removeBc(id) {
    set({
      bcs: get().bcs.filter((b) => b.id !== id),
      activeBcId: get().activeBcId === id ? null : get().activeBcId,
      check: null,
      hasResult: false,
    });
    sceneEvents.onBcsChanged?.(get().bcs, get().activeBcId);
    void pushBcs(get);
  },

  setActiveBc(id) {
    set({ activeBcId: id });
    sceneEvents.onBcsChanged?.(get().bcs, id);
  },

  updateBcTris(id, tris) {
    set({
      bcs: get().bcs.map((b) => (b.id === id ? { ...b, tris } : b)),
      check: null,
      hasResult: false,
    });
    sceneEvents.onBcsChanged?.(get().bcs, get().activeBcId);
    void pushBcs(get);
  },

  updateBcParams(id, params) {
    set({ bcs: get().bcs.map((b) => (b.id === id ? { ...b, ...params } : b)), hasResult: false });
    sceneEvents.onBcsChanged?.(get().bcs, get().activeBcId);
    void pushBcs(get);
  },

  setMaterial(m) {
    set({ material: m, hasResult: false });
    void engine.setMaterial(m.e0, m.nu, m.density);
  },

  setGravity(on) {
    set({ gravity: on, check: null, hasResult: false });
    void engine.setGravity(on);
  },

  setResolution(r) {
    set({ resolution: r, check: null, hasResult: false });
    void engine.setResolution(RESOLUTIONS[r]);
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
      // Run the check first so under-constraint gets the animated treatment.
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
      set({ stats, hasResult: true, showDeformed: true, busy: null });
      sceneEvents.onDisplacements?.(displacements, stats);
      sceneEvents.onDeformedView?.(true, get().deformScale);
    } catch (e) {
      set({ busy: null, error: e instanceof Error ? e.message : String(e) });
    }
  },

  setShowDeformed(on) {
    set({ showDeformed: on });
    sceneEvents.onDeformedView?.(on, get().deformScale);
  },

  setDeformScale(s) {
    set({ deformScale: s });
    sceneEvents.onDeformedView?.(get().showDeformed, s);
  },

  clearError() {
    set({ error: null });
  },
}));
