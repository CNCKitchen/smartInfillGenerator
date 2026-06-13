// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { useEffect, useRef, useState } from "react";
import { SceneManager } from "./SceneManager";
import { sceneEvents, useStore } from "../store";
import { RESULT_FIELDS } from "../types";

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
      onPlaceFace: (normal) => {
        void useStore.getState().applyPlaceOnFace(normal);
      },
      onPickDir: (normal) => {
        useStore.getState().applyPickedDir(normal);
      },
      onAutoScale: (autoScale) => {
        useStore.setState({ autoScale });
      },
      onSectionMoved: (normal, constant) => {
        useStore.getState().onSectionPlaneMoved(normal, constant);
      },
      onSymmetryMoved: (normal, c) => {
        useStore.getState().onSymmetryPlaneMoved(normal, c);
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
    sceneEvents.onVoxelMesh = (hull, edges, density) => scene.setVoxelMesh(hull, edges, density);
    sceneEvents.onMeshDensity = (on) => scene.setMeshDensity(on);
    sceneEvents.onWireframe = (on) => scene.setWireframe(on);
    sceneEvents.onVoxelCutActive = (on) => scene.setVoxelCutActive(on);
    sceneEvents.onAnimateDeformed = (on) => scene.setDeformAnimate(on);
    sceneEvents.onOptShape = (p, i, d) => scene.setOptShape(p, i, d);
    sceneEvents.onRegionVisibility = (vis) => scene.setRegionVisibility(vis);
    sceneEvents.onScalarField = (v, flip) => scene.setScalarField(v, flip ?? false);
    sceneEvents.onVoxelResult = (p, d, e, ed) => scene.setVoxelResult(p, d, e, ed);
    sceneEvents.onResultSurface = (s) => scene.setResultSurface(s);
    sceneEvents.onLegendRange = (min, max) => scene.setLegendRange(min, max);
    sceneEvents.onShowExtremes = (on, unit) => scene.setShowExtremes(on, unit);
    sceneEvents.onSectionState = (on) => scene.setSection(on);
    sceneEvents.onSectionFlip = () => scene.flipSection();
    sceneEvents.onSectionAxis = (a) => scene.setSectionAxis(a);
    sceneEvents.onModelTransformed = (p, bbox) => scene.updateModelPositions(p, bbox);
    sceneEvents.onSymmetry = (enabled, normal, c) => scene.setSymmetry(enabled, normal, c);

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

  // Hover value probe: a formatter whenever a contour legend is on screen
  // (result fields, density views, mesh-view element density).
  const viewMode = useStore((s) => s.viewMode);
  const resultField = useStore((s) => s.resultField);
  const meshDensity = useStore((s) => s.meshDensity);
  useEffect(() => {
    const scene = sceneRef.current;
    if (!scene) return;
    let fmt: ((v: number) => string) | null = null;
    if (viewMode === "deformed") {
      if (resultField === "u") {
        fmt = (v) => fmtDisp(v);
      } else if (resultField.startsWith("sf")) {
        fmt = (v) => `${v.toFixed(2)}×`;
      } else {
        const unit = RESULT_FIELDS.find((f) => f.value === resultField)?.unit ?? "";
        fmt = (v) => fmtField(v, unit);
      }
    } else if (
      viewMode === "density" ||
      viewMode === "infill" ||
      (viewMode === "mesh" && meshDensity)
    ) {
      fmt = (v) => `${(v * 100).toFixed(0)}%`;
    }
    scene.setProbeFormatter(fmt);
  }, [viewMode, resultField, meshDensity]);

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

function jetCss(flip = false): string {
  const stops: string[] = [];
  for (let i = 0; i <= 8; i++) {
    const pos = i / 8;
    const t = flip ? 1 - pos : pos;
    const r = Math.round(255 * Math.min(1, Math.max(0, 1.5 - Math.abs(4 * t - 3))));
    const g = Math.round(255 * Math.min(1, Math.max(0, 1.5 - Math.abs(4 * t - 2))));
    const b = Math.round(255 * Math.min(1, Math.max(0, 1.5 - Math.abs(4 * t - 1))));
    stops.push(`rgb(${r},${g},${b}) ${(100 * pos).toFixed(1)}%`);
  }
  return `linear-gradient(to top, ${stops.join(", ")})`;
}

const JET_GRADIENT = jetCss();
// Safety factor: red marks the LOW (critical) end of the scale.
const JET_GRADIENT_FLIP = jetCss(true);
// Matches ramp() in SceneManager (density + region colors).
const RAMP_GRADIENT =
  "linear-gradient(to top, #264de6 0%, #26e4e6 33%, #f0e61c 66%, #f21519 100%)";

function fmtDisp(mm: number): string {
  if (mm >= 0.01) return `${mm.toFixed(2)} mm`;
  return `${(mm * 1000).toFixed(1)} µm`;
}

function fmtField(v: number, unit: string): string {
  if (unit === "MPa") {
    const a = Math.abs(v);
    if (a >= 0.01 || a === 0) return `${v.toPrecision(3)} MPa`;
    return `${v.toExponential(1)} MPa`;
  }
  // strain: dimensionless, engineering notation
  return v === 0 ? "0" : v.toExponential(2);
}

/** Click-to-edit legend bound: shows the formatted value, becomes an input. */
function EditableBound({
  value,
  display,
  hint,
  onCommit,
}: {
  value: number;
  display: string;
  hint: string;
  onCommit: (v: number) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState("");
  const commit = () => {
    setEditing(false);
    const v = parseFloat(text);
    if (Number.isFinite(v)) onCommit(v);
  };
  if (!editing) {
    return (
      <span
        className="legendedit"
        title={`Click to set (${hint})`}
        onClick={() => {
          setText(String(Number(value.toPrecision(4))));
          setEditing(true);
        }}
      >
        {display}
      </span>
    );
  }
  return (
    <input
      className="legendinput"
      autoFocus
      value={text}
      onChange={(e) => setText(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => {
        if (e.key === "Enter") commit();
        if (e.key === "Escape") setEditing(false);
      }}
    />
  );
}

function Legend() {
  const viewMode = useStore((s) => s.viewMode);
  const stats = useStore((s) => s.stats);
  const autoScale = useStore((s) => s.autoScale);
  const deformScale = useStore((s) => s.deformScale);
  const setDeformScale = useStore((s) => s.setDeformScale);
  const showExtremes = useStore((s) => s.showExtremes);
  const setShowExtremes = useStore((s) => s.setShowExtremes);
  const optSummary = useStore((s) => s.optSummary);
  const voxelInfo = useStore((s) => s.voxelInfo);
  const resultField = useStore((s) => s.resultField);
  const fieldRange = useStore((s) => s.fieldRange);
  const legendMin = useStore((s) => s.legendMin);
  const legendMax = useStore((s) => s.legendMax);
  const setLegendRange = useStore((s) => s.setLegendRange);
  const smoothStress = useStore((s) => s.smoothStress);
  const setSmoothStress = useStore((s) => s.setSmoothStress);

  if (viewMode === "deformed" && stats) {
    const total = autoScale * deformScale;
    const totalLabel = total >= 9.5 ? `×${Math.round(total)}` : `×${total.toFixed(1)}`;
    const def = RESULT_FIELDS.find((f) => f.value === resultField);
    const isField = resultField !== "u" && !!def && !!fieldRange;
    const unit = isField ? def!.unit : "mm";
    const autoMin = isField ? fieldRange!.min : 0;
    const autoMax = isField ? fieldRange!.max : stats.maxDisplacement;
    const effMin = legendMin ?? autoMin;
    const effMax = legendMax ?? autoMax;
    const overridden = legendMin !== null || legendMax !== null;
    const isSf = resultField.startsWith("sf");
    const fmt = (v: number) => (isSf ? v.toFixed(2) : isField ? fmtField(v, unit) : fmtDisp(v));
    const hint = unit === "MPa" ? "MPa" : unit === "mm" ? "mm" : isSf ? "factor" : "strain";
    return (
      <div className="legend">
        <div className="legendtitle">{isField ? def!.label : "Displacement |u|"}</div>
        <div className="legendbody">
          <div className="legendbar" style={{ background: isSf ? JET_GRADIENT_FLIP : JET_GRADIENT }} />
          <div className="legendlabels">
            <EditableBound
              value={effMax}
              display={fmt(effMax)}
              hint={hint}
              onCommit={(v) => setLegendRange(effMin, v)}
            />
            <span>{fmt((effMin + effMax) / 2)}</span>
            <EditableBound
              value={effMin}
              display={fmt(effMin)}
              hint={hint}
              onCommit={(v) => setLegendRange(v, effMax)}
            />
          </div>
        </div>
        {overridden && (
          <button className="legendreset" onClick={() => setLegendRange(null, null)}>
            ↺ auto scale
          </button>
        )}
        <label className="legendcheck">
          <input
            type="checkbox"
            checked={showExtremes}
            onChange={(e) => setShowExtremes(e.target.checked)}
          />
          <span>mark min / max</span>
        </label>
        {isField && (
          <label className="legendcheck">
            <input
              type="checkbox"
              checked={smoothStress}
              onChange={(e) => setSmoothStress(e.target.checked)}
            />
            <span>smoothed (nodal average)</span>
          </label>
        )}
        {isSf && (
          <div className="legendnote">allowable scales with E(ρ) — red marks the critical low</div>
        )}
        {isField && !isSf && (
          <div className="legendnote">
            {smoothStress
              ? "nodal-averaged, evaluated on the surface"
              : "cell-center values — voxel-edge peaks are approximate"}
          </div>
        )}
        <div className="legendnote">
          exaggerated{" "}
          <EditableBound
            value={total}
            display={totalLabel}
            hint="total ×, 0 = undeformed"
            onCommit={(v) =>
              setDeformScale(Math.min(10, Math.max(0, v / Math.max(autoScale, 1e-9))))
            }
          />
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
    return <MeshLegend />;
  }
  return null;
}

/** Mesh-view legend: grid stats + the element-density plot toggle. */
function MeshLegend() {
  const voxelInfo = useStore((s) => s.voxelInfo)!;
  const meshDensity = useStore((s) => s.meshDensity);
  const setMeshDensity = useStore((s) => s.setMeshDensity);
  const perimeters = useStore((s) => s.perimeters);
  const lineWidth = useStore((s) => s.lineWidth);
  const optSummary = useStore((s) => s.optSummary);
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
      <label className="legendcheck">
        <input
          type="checkbox"
          checked={meshDensity}
          onChange={(e) => setMeshDensity(e.target.checked)}
        />
        <span>element density</span>
      </label>
      {meshDensity && (
        <div className="legendbody">
          <div className="legendbar" style={{ background: RAMP_GRADIENT }} />
          <div className="legendlabels">
            <span>100%</span>
            <span>50%</span>
            <span>0%</span>
          </div>
        </div>
      )}
      <div className="legendnote">
        {meshDensity ? (
          <>
            skin ({(perimeters * lineWidth).toFixed(2)} mm wall) = 100%
            <br />
            interior = {optSummary ? "optimized density" : "infill setting"}
          </>
        ) : (
          <>skin = {(perimeters * lineWidth).toFixed(2)} mm wall</>
        )}
      </div>
    </div>
  );
}
