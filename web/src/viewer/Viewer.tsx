import { useEffect, useRef } from "react";
import { SceneManager } from "./SceneManager";
import { sceneEvents, useStore } from "../store";

function union(a: Uint32Array, b: Uint32Array): Uint32Array {
  const s = new Set<number>(a as unknown as number[]);
  for (const t of b) s.add(t);
  return Uint32Array.from(s);
}

function subtract(a: Uint32Array, b: Uint32Array): Uint32Array {
  const s = new Set<number>(a as unknown as number[]);
  for (const t of b) s.delete(t);
  return Uint32Array.from(s);
}

function containsAll(a: Uint32Array, b: Uint32Array): boolean {
  const s = new Set<number>(a as unknown as number[]);
  for (const t of b) if (!s.has(t)) return false;
  return true;
}

export function Viewer() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const sceneRef = useRef<SceneManager | null>(null);

  const tool = useStore((s) => s.tool);
  const brushRadius = useStore((s) => s.brushRadius);
  const brushErase = useStore((s) => s.brushErase);

  useEffect(() => {
    const scene = new SceneManager();
    sceneRef.current = scene;
    scene.init(canvasRef.current!, {
      onPickPatch: (tris, additive) => {
        const st = useStore.getState();
        const bc = st.bcs.find((b) => b.id === st.activeBcId);
        if (!bc) return;
        const next =
          !additive || containsAll(bc.tris, tris) ? subtract(bc.tris, tris) : union(bc.tris, tris);
        st.updateBcTris(bc.id, next);
      },
      onBrush: (tris, erase) => {
        const st = useStore.getState();
        const bc = st.bcs.find((b) => b.id === st.activeBcId);
        if (!bc) return;
        st.updateBcTris(bc.id, erase ? subtract(bc.tris, tris) : union(bc.tris, tris));
      },
      onAutoScale: (autoScale) => {
        useStore.setState({ autoScale });
      },
    });

    sceneEvents.onModelLoaded = (m) => scene.setModel(m);
    sceneEvents.onPatchIdsChanged = (ids) => scene.setPatchIds(ids);
    sceneEvents.onBcsChanged = (bcs, active) => scene.setBcs(bcs, active);
    sceneEvents.onAnimateMode = (mode) => scene.setRbmMode(mode);
    sceneEvents.onDisplacements = (d, stats) => scene.setDisplacements(d, stats);
    sceneEvents.onVertexDensity = (d) => scene.setVertexDensity(d);
    sceneEvents.onRegions = (r) => scene.setRegions(r);
    sceneEvents.onViewState = (mode, scale) => scene.setViewState(mode, scale);
    sceneEvents.onVoxelMesh = (hull, edges) => scene.setVoxelMesh(hull, edges);
    sceneEvents.onAnimateDeformed = (on) => scene.setDeformAnimate(on);

    const obs = new ResizeObserver(() => {
      const el = wrapRef.current;
      if (el) scene.resize(el.clientWidth, el.clientHeight);
    });
    obs.observe(wrapRef.current!);

    return () => {
      obs.disconnect();
      scene.dispose();
    };
  }, []);

  useEffect(() => {
    sceneRef.current?.setTool(tool, brushRadius, brushErase);
  }, [tool, brushRadius, brushErase]);

  // Drag & drop.
  useEffect(() => {
    const el = wrapRef.current!;
    const onDrop = async (ev: DragEvent) => {
      ev.preventDefault();
      const file = ev.dataTransfer?.files?.[0];
      if (!file) return;
      const bytes = await file.arrayBuffer();
      void useStore.getState().loadFile(file.name, bytes);
    };
    const onDrag = (ev: DragEvent) => ev.preventDefault();
    el.addEventListener("drop", onDrop);
    el.addEventListener("dragover", onDrag);
    return () => {
      el.removeEventListener("drop", onDrop);
      el.removeEventListener("dragover", onDrag);
    };
  }, []);

  return (
    <div className="viewer" ref={wrapRef}>
      <canvas ref={canvasRef} />
      <Legend />
    </div>
  );
}

// ---- color-scale legend overlay ----

function jetCss(): string {
  const stops: string[] = [];
  for (let i = 0; i <= 8; i++) {
    const t = i / 8;
    const r = Math.round(255 * Math.min(1, Math.max(0, 1.5 - Math.abs(4 * t - 3))));
    const g = Math.round(255 * Math.min(1, Math.max(0, 1.5 - Math.abs(4 * t - 2))));
    const b = Math.round(255 * Math.min(1, Math.max(0, 1.5 - Math.abs(4 * t - 1))));
    stops.push(`rgb(${r},${g},${b}) ${(100 * t).toFixed(1)}%`);
  }
  return `linear-gradient(to top, ${stops.join(", ")})`;
}

const JET_GRADIENT = jetCss();
// Matches ramp() in SceneManager (density + region colors).
const RAMP_GRADIENT =
  "linear-gradient(to top, #264de6 0%, #26e4e6 33%, #f0e61c 66%, #f21519 100%)";

function fmtDisp(mm: number): string {
  if (mm >= 0.01) return `${mm.toFixed(2)} mm`;
  return `${(mm * 1000).toFixed(1)} µm`;
}

function Legend() {
  const viewMode = useStore((s) => s.viewMode);
  const stats = useStore((s) => s.stats);
  const autoScale = useStore((s) => s.autoScale);
  const deformScale = useStore((s) => s.deformScale);
  const optSummary = useStore((s) => s.optSummary);
  const voxelInfo = useStore((s) => s.voxelInfo);

  if (viewMode === "deformed" && stats) {
    const max = stats.maxDisplacement;
    const total = autoScale * deformScale;
    const totalLabel = total >= 9.5 ? `×${Math.round(total)}` : `×${total.toFixed(1)}`;
    return (
      <div className="legend">
        <div className="legendtitle">Displacement |u|</div>
        <div className="legendbody">
          <div className="legendbar" style={{ background: JET_GRADIENT }} />
          <div className="legendlabels">
            <span>{fmtDisp(max)}</span>
            <span>{fmtDisp(max / 2)}</span>
            <span>0</span>
          </div>
        </div>
        <div className="legendnote">
          shape exaggerated {totalLabel}
          {deformScale === 0 ? " (undeformed)" : ""}
        </div>
      </div>
    );
  }
  if ((viewMode === "density" || viewMode === "infill") && optSummary) {
    return (
      <div className="legend">
        <div className="legendtitle">Infill density</div>
        <div className="legendbody">
          <div className="legendbar" style={{ background: RAMP_GRADIENT }} />
          <div className="legendlabels">
            <span>≥80%</span>
            <span>40%</span>
            <span>0%</span>
          </div>
        </div>
        {viewMode === "infill" && (
          <div className="legendnote">solid color = modifier region</div>
        )}
      </div>
    );
  }
  if (viewMode === "mesh" && voxelInfo) {
    return (
      <div className="legend">
        <div className="legendtitle">Analysis mesh</div>
        <div className="legendnote">
          {voxelInfo.solid.toLocaleString()} hex cells
          <br />
          h = {voxelInfo.h.toFixed(2)} mm
          <br />
          {voxelInfo.nx}×{voxelInfo.ny}×{voxelInfo.nz} grid
        </div>
      </div>
    );
  }
  return null;
}
