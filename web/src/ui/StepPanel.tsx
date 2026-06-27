// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// One panel, one step: the active station's controls. Everything the old
// all-at-once sidebar offered is still here, just shown one step at a time.

import { useEffect, useRef, useState } from "react";
import { useShallow } from "zustand/shallow";
import {
  budgetBounds,
  resultStale,
  symLabel,
  useStore,
  COLOR_STEPS_MIN,
  COLOR_STEPS_MAX,
} from "../store";
import { NumInput } from "./NumInput";
import { RESULT_FIELDS, type Bc, type ForceMode, type LoadStep, type PatternKey } from "../types";
import { fmtDisp, fmtLen, rampCss } from "./fmt";
import { bcLabel, KIND_DOT, KIND_LABEL, SUPPORT_KINDS } from "./bcmeta";

const SLICER_NAMES = {
  orca: "OrcaSlicer",
  bambu: "Bambu Studio",
  prusa: "PrusaSlicer",
} as const;

/** Round a unit-direction component for display (avoids 0.00003757… noise from
 *  an auto-tracked surface normal; the value re-normalizes on commit anyway). */
const r4 = (x: number) => Math.round(x * 1e4) / 1e4;

const HEAD: Record<number, { title: string; sub: string }> = {
  1: { title: "Model", sub: "Drop an STL or 3MF — units are mm." },
  2: { title: "Boundary conditions", sub: "Where the part is held, how it is loaded." },
  3: { title: "Properties", sub: "Material, print settings, analysis grid." },
  4: { title: "Verify setup", sub: "Check constraints, then analyze the print or the solid." },
  5: { title: "Optimize infill", sub: "Distribute density where the loads need it." },
  6: { title: "View & export", sub: "Inspect the result, hand off to the slicer." },
};

/** Surface-patch source: dihedral crease angle (the slider) or, for STEP
 *  imports, the exact BREP faces. Shared by the Model and BC steps so the
 *  "Pick surface" tool selects whole CAD faces when chosen. */
function SurfacePatchControl() {
  const s = useStore(
    useShallow((s) => ({
      model: s.model,
      segSource: s.segSource,
      segAngle: s.segAngle,
      setSegSource: s.setSegSource,
      setSegAngle: s.setSegAngle,
    }))
  );
  if (!s.model) return null;
  const cad = s.model.hasCadFaces;
  return (
    <>
      <div className="g-label">
        <span>Surface detection</span>
        {s.segSource === "angle" && <b>{s.segAngle}°</b>}
      </div>
      {cad && (
        <div className="toolrow">
          <button
            className={s.segSource === "cad" ? "on" : ""}
            onClick={() => void s.setSegSource("cad")}
            title="One pickable surface per CAD face from the STEP file"
          >
            CAD faces
          </button>
          <button
            className={s.segSource === "angle" ? "on" : ""}
            onClick={() => void s.setSegSource("angle")}
            title="Group triangles into surfaces by crease angle"
          >
            Crease angle
          </button>
        </div>
      )}
      {s.segSource === "angle" ? (
        <>
          <input
            type="range"
            min={5}
            max={80}
            value={s.segAngle}
            onChange={(e) => void s.setSegAngle(Number(e.target.value))}
          />
          <div className="dim small">
            Splits the skin into pickable surfaces — lower the angle if patches merge, raise it if
            they shatter.
          </div>
        </>
      ) : (
        <div className="dim small">
          Picking whole CAD faces from the STEP file. Switch to crease angle for a custom grouping
          (e.g. to merge a filleted region).
        </div>
      )}
    </>
  );
}

export function StepPanel() {
  const s = useStore(
    useShallow((s) => ({ model: s.model, activeStep: s.activeStep }))
  );
  const step = s.model ? s.activeStep : 1;

  // Leaving the boundary-conditions workspace (clicking another step) or
  // pressing Esc snaps the tool back to plain orbiting so a stray click in
  // the viewport can't silently edit a selection. Disarm on CLICK (deferred),
  // not pointerdown: disarming re-renders and shifts the layout between
  // press and release, which eats the first click on Check/Solve.
  useEffect(() => {
    const onClick = (e: MouseEvent) => {
      const st = useStore.getState();
      if (st.tool === "orbit") return;
      const el = e.target as HTMLElement | null;
      if (!el || el.closest("[data-bcsection]") || el.closest(".viewer")) return;
      setTimeout(() => useStore.getState().setTool("orbit"), 0);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      const st = useStore.getState();
      if (st.tool !== "orbit") st.setTool("orbit");
    };
    document.addEventListener("click", onClick, true);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("click", onClick, true);
      document.removeEventListener("keydown", onKey);
    };
  }, []);

  const head = HEAD[step];
  return (
    <section className="panel" data-bcsection={step === 2 ? true : undefined}>
      <div className="p-head">
        <b>
          {step} · {head.title}
        </b>
        <span>{head.sub}</span>
      </div>
      {step === 1 && <StepModel />}
      {step === 2 && <StepBcs />}
      {step === 3 && <StepProperties />}
      {step === 4 && <StepVerify />}
      {step === 5 && <StepOptimize />}
      {step === 6 && <StepExport />}
    </section>
  );
}

// ---------------- 1 · Model ----------------

function StepModel() {
  const s = useStore(
    useShallow((s) => ({
      fileName: s.fileName,
      model: s.model,
      loadFile: s.loadFile,
      tool: s.tool,
      setTool: s.setTool,
      rotateModel: s.rotateModel,
    }))
  );
  const fileRef = useRef<HTMLInputElement>(null);
  const onFile = async (f: File | undefined) => {
    if (!f) return;
    await s.loadFile(f.name, await f.arrayBuffer());
  };
  return (
    <>
      <input
        ref={fileRef}
        type="file"
        accept=".stl,.3mf"
        hidden
        onChange={(e) => void onFile(e.target.files?.[0])}
      />
      <button className="primary" onClick={() => fileRef.current?.click()}>
        {s.fileName ? "Replace model…" : "Open STL / 3MF…"}
      </button>
      {s.fileName ? (
        <div className="fileinfo">
          <div>{s.fileName}</div>
          <div className="dim">
            {s.model!.triCount.toLocaleString()} triangles · {s.model!.patchCount} surfaces
          </div>
          <div className="dim">
            {fmtLen(s.model!.bbox[3] - s.model!.bbox[0])} ×{" "}
            {fmtLen(s.model!.bbox[4] - s.model!.bbox[1])} ×{" "}
            {fmtLen(s.model!.bbox[5] - s.model!.bbox[2])} mm
          </div>
        </div>
      ) : (
        <div className="dim drophint">…or drop a file into the viewport. Units: mm.</div>
      )}
      {s.model && (
        <>
          <div className="group">
            <div className="g-label">
              <span>Print orientation</span>
              <b>Z = build direction</b>
            </div>
            <div className="toolrow">
              <button
                className={s.tool === "place" ? "on" : ""}
                onClick={() => s.setTool(s.tool === "place" ? "orbit" : "place")}
                title="Click the surface the part prints on — it becomes the bottom"
              >
                ⤓ Place on face
              </button>
              <button onClick={() => void s.rotateModel("x")} title="Rotate +90° about X">
                ⟳X
              </button>
              <button onClick={() => void s.rotateModel("y")} title="Rotate +90° about Y">
                ⟳Y
              </button>
              <button onClick={() => void s.rotateModel("z")} title="Rotate +90° about Z">
                ⟳Z
              </button>
            </div>
            <div className="dim small">
              {s.tool === "place"
                ? "Click the face the part prints ON — it turns to the build plate (Z−)."
                : "Layer-adhesion safety treats Z as the layer direction. Loads keep their world directions; results reset on reorientation."}
            </div>
          </div>
          <div className="group">
            <SurfacePatchControl />
          </div>
        </>
      )}
      <div className="hint">
        Static linear analysis on a voxel grid — all computation stays in your browser.
      </div>
    </>
  );
}

// ---------------- 2 · Boundary conditions ----------------

function StepBcs() {
  const s = useStore(
    useShallow((s) => ({
      bcs: s.bcs,
      addBc: s.addBc,
      tool: s.tool,
      setTool: s.setTool,
      brushRadius: s.brushRadius,
      setBrushRadius: s.setBrushRadius,
      brushErase: s.brushErase,
      setBrushErase: s.setBrushErase,
      activeBcId: s.activeBcId,
    }))
  );
  const supports = s.bcs.filter((bc) => SUPPORT_KINDS.includes(bc.kind));
  const loads = s.bcs.filter((bc) => !SUPPORT_KINDS.includes(bc.kind));
  return (
    <>
      <LoadStepPanel />
      <div className="group">
        <div className="g-label">
          <span>Supports</span>
        </div>
        {supports.map((bc) => (
          <BcRow key={bc.id} bc={bc} />
        ))}
        <div className="addrow">
          <button onClick={() => s.addBc("fixed")}>+ Fixed</button>
          <button onClick={() => s.addBc("elastic")}>+ Elastic</button>
          <button onClick={() => s.addBc("frictionless")}>+ Frictionless</button>
          <button onClick={() => s.addBc("displacement")}>+ Displacement</button>
        </div>
      </div>

      <div className="group">
        <div className="g-label">
          <span>Loads</span>
        </div>
        {loads.map((bc) => (
          <BcRow key={bc.id} bc={bc} />
        ))}
        <div className="addrow">
          <button onClick={() => s.addBc("force")}>+ Force</button>
          <button onClick={() => s.addBc("pressure")}>+ Pressure</button>
        </div>
      </div>

      {s.bcs.length > 0 && (
        <div className="group">
          <div className="g-label">
            <span>Assign surfaces</span>
          </div>
          <div className="toolrow">
            <button
              className={s.tool === "select" ? "on" : ""}
              onClick={() => s.setTool(s.tool === "select" ? "orbit" : "select")}
            >
              Pick surface
            </button>
            <button
              className={s.tool === "brush" ? "on" : ""}
              onClick={() => s.setTool(s.tool === "brush" ? "orbit" : "brush")}
            >
              Brush
            </button>
          </div>
          <div style={{ marginTop: 4 }}>
            <SurfacePatchControl />
          </div>
        </div>
      )}
      {s.tool === "brush" && (
        <>
          <div className="group">
            <div className="g-label">
              <span>Brush diameter</span>
              <b>{(s.brushRadius * 2).toFixed(1)} mm</b>
            </div>
            <input
              type="range"
              min={1}
              max={50}
              step={1}
              value={s.brushRadius * 2}
              onChange={(e) => s.setBrushRadius(Number(e.target.value) / 2)}
            />
          </div>
          <label className="rowcheck">
            <input
              type="checkbox"
              checked={s.brushErase}
              onChange={(e) => s.setBrushErase(e.target.checked)}
            />
            <span>Erase mode</span>
          </label>
        </>
      )}
      {s.activeBcId && (s.tool === "select" || s.tool === "brush") && (
        <div className="hint">
          Click surfaces to add to the highlighted condition (click again to remove, shift-click
          always removes). Esc or clicking another step returns to orbiting.
        </div>
      )}
      {s.activeBcId && s.tool === "pickdir" && (
        <div className="hint">
          Pick-direction is armed — click a triangle to aim the force along its normal. Esc returns
          to orbiting.
        </div>
      )}
      {s.activeBcId && s.tool === "orbit" && (
        <div className="hint">
          Choose <b>Pick surface</b> or <b>Brush</b> to assign surfaces to the highlighted
          condition. Orbiting is always active.
        </div>
      )}
    </>
  );
}

// Load steps (FEA load cases) — DESIGN §13. The single-case setup is unchanged:
// with one step this is just a subtle "+ Add load step". Adding a 2nd step
// reveals a compact step SELECTOR; the BC list below then edits whichever step
// is active (and its loads show in the viewport). Naming + the per-step on/off
// matrix live in the ⚙ Manage modal (LoadStepsModal), like Settings.
function LoadStepPanel() {
  const s = useStore(
    useShallow((s) => ({
      loadSteps: s.loadSteps,
      activeLoadStepId: s.activeLoadStepId,
      addLoadStep: s.addLoadStep,
      setActiveLoadStep: s.setActiveLoadStep,
      openLoadSteps: s.openLoadSteps,
    }))
  );
  if (s.loadSteps.length <= 1) {
    return (
      <div className="addrow lsadd">
        <button onClick={() => s.addLoadStep()} title="Analyze the part under several load cases">
          + Add load step
        </button>
      </div>
    );
  }
  const active = s.loadSteps.find((ls) => ls.id === s.activeLoadStepId);
  return (
    <div className="group lsbar">
      <div className="g-label">
        <span>Load step</span>
        <button
          className="lsmanage"
          onClick={() => s.openLoadSteps(true)}
          title="Rename steps & toggle which supports / loads are active in each"
        >
          ⚙ Manage load steps
        </button>
      </div>
      <div className="lspills">
        {s.loadSteps.map((ls, i) => (
          <button
            key={ls.id}
            className={ls.id === s.activeLoadStepId ? "lspill on" : "lspill"}
            onClick={() => s.setActiveLoadStep(ls.id)}
            title={ls.name}
          >
            {i + 1}
          </button>
        ))}
        <button className="lspill add" onClick={() => s.addLoadStep()} title="Add a load step">
          +
        </button>
      </div>
      <div className="dim lsactive">
        Editing <b>{active?.name}</b> — its loads show below and in the viewport.
      </div>
    </div>
  );
}

function BcRow({ bc }: { bc: Bc }) {
  const s = useStore(
    useShallow((s) => ({
      activeBcId: s.activeBcId,
      setActiveBc: s.setActiveBc,
      removeBc: s.removeBc,
      setBcName: s.setBcName,
      updateBcParams: s.updateBcParams,
      setStepPressure: s.setStepPressure,
      loadSteps: s.loadSteps,
      activeLoadStepId: s.activeLoadStepId,
    }))
  );
  const active = s.activeBcId === bc.id;
  // Multi-step: the editors bind to the ACTIVE step (undefined = single-step,
  // edit the base BC as before). A BC switched off in the active step is dimmed
  // — its on/off lives in ⚙ Manage.
  const step =
    s.loadSteps.length > 1 ? s.loadSteps.find((ls) => ls.id === s.activeLoadStepId) : undefined;
  const off = step ? step.overrides[bc.id]?.active === false : false;
  return (
    <div
      className={`bc${active ? " active" : ""}${off ? " off" : ""}`}
      onClick={() => s.setActiveBc(active ? null : bc.id)}
    >
      <div className="bchead">
        <span className="dot" style={{ background: KIND_DOT[bc.kind] }} />
        <input
          className="bcnameinput"
          value={bcLabel(bc)}
          spellCheck={false}
          onClick={(e) => e.stopPropagation()}
          onChange={(e) => s.setBcName(bc.id, e.target.value)}
          title="Rename this condition"
        />
        {off && <span className="bcoff">off</span>}
        <span className="dim">{bc.tris.length ? `${bc.tris.length} tris` : "select…"}</span>
        <button
          className="x"
          onClick={(e) => {
            e.stopPropagation();
            s.removeBc(bc.id);
          }}
        >
          ×
        </button>
      </div>
      {bc.kind === "force" && <ForceEditor bc={bc} step={step} />}
      {bc.kind === "displacement" && <DisplacementEditor bc={bc} />}
      {bc.kind === "pressure" && (
        <div className="bcparams" onClick={(e) => e.stopPropagation()}>
          <label>
            p
            <NumInput
              value={step ? step.overrides[bc.id]?.pressure ?? bc.pressure ?? 0 : bc.pressure ?? 0}
              step={0.01}
              onCommit={(v) =>
                step ? s.setStepPressure(step.id, bc.id, v) : s.updateBcParams(bc.id, { pressure: v })
              }
            />
          </label>
          <span className="dim">MPa</span>
        </div>
      )}
      {bc.kind === "elastic" && (
        <div onClick={(e) => e.stopPropagation()}>
          <div className="bcparams">
            <label>
              k
              <NumInput
                value={bc.stiffness ?? 100}
                step={10}
                min={0.01}
                onCommit={(v) => s.updateBcParams(bc.id, { stiffness: Math.max(0.01, v) })}
              />
            </label>
            <span className="dim">N/mm³ foundation stiffness</span>
          </div>
          <div className="dim small">
            σ = k·u at the surface (k ≈ E/t of what's underneath): foam ~0.1, 3 mm rubber pad ~2,
            printed-plastic mount ~50–500, bolted to steel ≥ 5000 (≈ fixed).
          </div>
        </div>
      )}
    </div>
  );
}

/** Stacked X/Y/Z inputs — full-width rows so the numbers are fully visible (the
 *  old 3-across layout clipped them). Reused for force components AND the
 *  direction vector, so both read the same. Unit sits behind each field. */
function VectorInput({
  values,
  onChange,
  label = "F",
  unit = "",
  step = 1,
}: {
  values: [number, number, number];
  onChange: (v: [number, number, number]) => void;
  label?: string;
  unit?: string;
  step?: number;
}) {
  return (
    <div className="forcegrid">
      {(["X", "Y", "Z"] as const).map((axis, i) => (
        <label key={axis} className="forcerow">
          <span className="flabel">
            {label}
            {axis}
          </span>
          <NumInput
            className="fnum"
            value={values[i]}
            step={step}
            onCommit={(v) => {
              const nv = [...values] as [number, number, number];
              nv[i] = v;
              onChange(nv);
            }}
          />
          {unit && <span className="funit">{unit}</span>}
        </label>
      ))}
    </div>
  );
}

function ForceModeToggle({ mode, onMode }: { mode: ForceMode; onMode: (m: ForceMode) => void }) {
  return (
    <div className="seg">
      <button
        className={mode === "components" ? "on" : ""}
        onClick={() => onMode("components")}
        title="Define the load by its X/Y/Z components"
      >
        Components
      </button>
      <button
        className={mode === "direction" ? "on" : ""}
        onClick={() => onMode("direction")}
        title="Define the load by a direction and a magnitude"
      >
        Direction
      </button>
    </div>
  );
}

/** Force editor: components OR direction + magnitude. `step` set ⇒ edits that
 *  load step's vector; otherwise the base BC. */
function ForceEditor({ bc, step }: { bc: Bc; step?: LoadStep }) {
  return step ? <StepForceEditor bc={bc} step={step} /> : <BaseForceEditor bc={bc} />;
}

/** Single-step (base) force editor — owns the persisted mode + the pick/flip/
 *  normal direction tools. */
function BaseForceEditor({ bc }: { bc: Bc }) {
  const s = useStore(
    useShallow((s) => ({
      activeBcId: s.activeBcId,
      tool: s.tool,
      setForceMode: s.setForceMode,
      updateBcParams: s.updateBcParams,
      setForceMag: s.setForceMag,
      setForceDir: s.setForceDir,
      setActiveBc: s.setActiveBc,
      setTool: s.setTool,
      flipForceDir: s.flipForceDir,
      resetForceDirToNormal: s.resetForceDirToNormal,
    }))
  );
  const active = s.activeBcId === bc.id;
  const mode = bc.forceMode ?? "components";
  const force = bc.force ?? [0, 0, 0];
  const dir = bc.forceDir ?? [0, 0, -1];
  const picking = active && s.tool === "pickdir";
  return (
    <div className="forceedit" onClick={(e) => e.stopPropagation()}>
      <ForceModeToggle mode={mode} onMode={(m) => s.setForceMode(bc.id, m)} />
      {mode === "components" ? (
        <VectorInput
          values={force}
          label="F"
          unit="N"
          step={1}
          onChange={(nf) => s.updateBcParams(bc.id, { force: nf })}
        />
      ) : (
        <>
          <div className="forcerow">
            <span className="flabel">|F|</span>
            <NumInput
              className="fnum"
              value={bc.forceMag ?? 0}
              step={1}
              onCommit={(v) => s.setForceMag(bc.id, v)}
            />
            <span className="funit">N</span>
          </div>
          <VectorInput
            values={[r4(dir[0]), r4(dir[1]), r4(dir[2])]}
            label="d"
            step={0.1}
            onChange={(d) => s.setForceDir(bc.id, d)}
          />
          <ForceDirTools
            picking={picking}
            disabledNormal={bc.tris.length === 0}
            onPick={() => {
              s.setActiveBc(bc.id);
              s.setTool(picking ? "orbit" : "pickdir");
            }}
            onFlip={() => s.flipForceDir(bc.id)}
            onNormal={() => s.resetForceDirToNormal(bc.id)}
          />
          <div className="dim small">
            {picking
              ? "Click a triangle on the model — its normal becomes the force direction."
              : bc.forceDirAuto !== false
                ? "Direction follows the selection's average surface normal. Pick a face or edit d to override."
                : "Custom direction — ‘Surface normal’ snaps it back to the selection."}
          </div>
        </>
      )}
    </div>
  );
}

/** Per-step force editor — edits the active load step's vector. Components, or
 *  direction + magnitude derived from that vector. Mode is a local view choice. */
function StepForceEditor({ bc, step }: { bc: Bc; step: LoadStep }) {
  const s = useStore(
    useShallow((s) => ({
      activeBcId: s.activeBcId,
      tool: s.tool,
      setActiveBc: s.setActiveBc,
      setTool: s.setTool,
      setStepForce: s.setStepForce,
      aimStepForceAlongNormal: s.aimStepForceAlongNormal,
    }))
  );
  const [mode, setMode] = useState<ForceMode>(bc.forceMode ?? "components");
  const f = step.overrides[bc.id]?.force ?? bc.force ?? [0, 0, 0];
  const setVec = (v: [number, number, number]) => s.setStepForce(step.id, bc.id, v);
  const mag = Math.hypot(f[0], f[1], f[2]);
  const dir: [number, number, number] =
    mag > 1e-9 ? [f[0] / mag, f[1] / mag, f[2] / mag] : bc.forceDir ?? [0, 0, -1];
  const picking = s.activeBcId === bc.id && s.tool === "pickdir";
  const round = (x: number) => Math.round(x * 1000) / 1000;
  return (
    <div className="forceedit" onClick={(e) => e.stopPropagation()}>
      <ForceModeToggle mode={mode} onMode={setMode} />
      {mode === "components" ? (
        <VectorInput values={f} label="F" unit="N" step={1} onChange={setVec} />
      ) : (
        <>
          <div className="forcerow">
            <span className="flabel">|F|</span>
            <NumInput
              className="fnum"
              value={round(mag)}
              step={1}
              onCommit={(v) => setVec([dir[0] * v, dir[1] * v, dir[2] * v])}
            />
            <span className="funit">N</span>
          </div>
          <VectorInput
            values={[round(dir[0]), round(dir[1]), round(dir[2])]}
            label="d"
            step={0.1}
            onChange={(d) => {
              const len = Math.hypot(d[0], d[1], d[2]) || 1;
              setVec([(d[0] / len) * mag, (d[1] / len) * mag, (d[2] / len) * mag]);
            }}
          />
          <ForceDirTools
            picking={picking}
            disabledNormal={bc.tris.length === 0}
            onPick={() => {
              s.setActiveBc(bc.id);
              s.setTool(picking ? "orbit" : "pickdir");
            }}
            onFlip={() => setVec([-f[0], -f[1], -f[2]])}
            onNormal={() => s.aimStepForceAlongNormal(step.id, bc.id)}
          />
          <div className="dim small">
            This step's force vector. Magnitude × direction; pick a face, flip, or snap to the
            selection's normal.
          </div>
        </>
      )}
    </div>
  );
}

function ForceDirTools({
  picking,
  disabledNormal,
  onPick,
  onFlip,
  onNormal,
}: {
  picking: boolean;
  disabledNormal: boolean;
  onPick: () => void;
  onFlip: () => void;
  onNormal: () => void;
}) {
  return (
    <div className="toolrow">
      <button
        className={picking ? "on" : ""}
        onClick={onPick}
        title="Click a triangle on the model to aim the force along its normal"
      >
        ⊹ Pick direction
      </button>
      <button onClick={onFlip} title="Reverse the force direction">
        ⇄ Flip
      </button>
      <button
        onClick={onNormal}
        disabled={disabledNormal}
        title="Aim along the selection's area-weighted average normal"
      >
        ↻ Surface normal
      </button>
    </div>
  );
}

/** Displacement support editor: per global axis, an enforce checkbox + the
 *  prescribed displacement value (mm). Enforced + 0 = pin to zero; enforced + v
 *  = an imposed motion; unchecked = free (roller). */
function DisplacementEditor({ bc }: { bc: Bc }) {
  const s = useStore(
    useShallow((s) => ({ toggleBcAxis: s.toggleBcAxis, updateBcParams: s.updateBcParams }))
  );
  const axes = bc.axes ?? [false, false, true];
  const disp = bc.disp ?? [0, 0, 0];
  return (
    <div onClick={(e) => e.stopPropagation()}>
      <div className="forcegrid">
        {(["X", "Y", "Z"] as const).map((axis, i) => (
          <label key={axis} className="forcerow dispaxis">
            <input
              type="checkbox"
              checked={axes[i]}
              onChange={() => s.toggleBcAxis(bc.id, i as 0 | 1 | 2)}
              title={`Enforce the global ${axis} displacement`}
            />
            <span className="flabel">{axis}</span>
            <NumInput
              className="fnum"
              value={disp[i]}
              step={0.1}
              disabled={!axes[i]}
              onCommit={(v) => {
                const d = [...disp] as [number, number, number];
                d[i] = v;
                s.updateBcParams(bc.id, { disp: d });
              }}
            />
            <span className="funit">mm</span>
          </label>
        ))}
      </div>
      <div className="dim small">
        Check an axis to enforce it; the value is its prescribed displacement (0 = pinned).
        Unchecked axes slide free (a roller). All three checked ≈ a fixed support.
      </div>
    </div>
  );
}

// ---------------- 3 · Properties ----------------

function StepProperties() {
  const s = useStore(
    useShallow((s) => ({
      perimeters: s.perimeters,
      lineWidth: s.lineWidth,
      voxelInfo: s.voxelInfo,
      material: s.material,
      materials: s.materials,
      setMaterial: s.setMaterial,
      openSettings: s.openSettings,
      setPerimeters: s.setPerimeters,
      setLineWidth: s.setLineWidth,
      topBottomLayers: s.topBottomLayers,
      setTopBottomLayers: s.setTopBottomLayers,
      layerHeight: s.layerHeight,
      setLayerHeight: s.setLayerHeight,
      pattern: s.pattern,
      setPattern: s.setPattern,
      printInfill: s.printInfill,
      setPrintInfill: s.setPrintInfill,
      resolution: s.resolution,
      setResolution: s.setResolution,
      model: s.model,
      customH: s.customH,
      setCustomH: s.setCustomH,
      snapVoxel: s.snapVoxel,
      setSnapVoxel: s.setSnapVoxel,
      compositeSkin: s.compositeSkin,
      setCompositeSkin: s.setCompositeSkin,
    }))
  );
  const wall = s.perimeters * s.lineWidth;
  const k = s.voxelInfo ? Math.max(1, Math.round(wall / s.voxelInfo.h)) : null;
  return (
    <>
      <div className="group">
        <div className="g-label">
          <span>Material</span>
        </div>
        <select
          value={s.material.name}
          onChange={(e) => {
            const m = s.materials.find((m) => m.name === e.target.value);
            if (m) s.setMaterial(m);
          }}
        >
          {s.materials.map((m) => (
            <option key={m.name}>{m.name}</option>
          ))}
        </select>
        <div className="dim small">
          E = {s.material.e0} MPa · ν = {s.material.nu} · ρ = {s.material.density} g/cm³ · σₜ ={" "}
          {s.material.strength} MPa —{" "}
          <a className="link" onClick={() => s.openSettings(true)}>
            edit
          </a>
        </div>
      </div>

      <div className="duo">
        <div className="group">
          <div className="g-label">
            <span>Perimeters</span>
          </div>
          <NumInput value={s.perimeters} step={1} min={1} max={8} onCommit={(v) => s.setPerimeters(v)} />
        </div>
        <div className="group">
          <div className="g-label">
            <span>Line width</span>
            <b>mm</b>
          </div>
          <NumInput
            value={s.lineWidth}
            step={0.05}
            min={0.1}
            max={1.5}
            onCommit={(v) => s.setLineWidth(v)}
          />
        </div>
      </div>
      <div className="dim small">
        ≈ {wall.toFixed(2)} mm solid wall — what the analysis assumes and what the 3MF's
        wall_loops will print. Match the line width to your profile.
      </div>

      <div className="duo">
        <div className="group">
          <div className="g-label">
            <span>Top/bottom layers</span>
          </div>
          <NumInput
            value={s.topBottomLayers}
            step={1}
            min={0}
            max={20}
            onCommit={(v) => s.setTopBottomLayers(v)}
          />
        </div>
        <div className="group">
          <div className="g-label">
            <span>Layer height</span>
            <b>mm</b>
          </div>
          <NumInput
            value={s.layerHeight}
            step={0.05}
            min={0.04}
            max={0.6}
            onCommit={(v) => s.setLayerHeight(v)}
          />
        </div>
      </div>
      <div className="dim small">
        {s.topBottomLayers > 0
          ? `≈ ${(s.topBottomLayers * s.layerHeight).toFixed(2)} mm solid shells on up/down-facing surfaces — exported as top/bottom shell layers.`
          : "0 layers: no top/bottom shells — the infill shows through the surface (showpieces). Exported as 0 shell layers."}
      </div>

      <div className="duo">
        <div className="group">
          <div className="g-label">
            <span>Infill pattern</span>
          </div>
          <select value={s.pattern} onChange={(e) => s.setPattern(e.target.value as PatternKey)}>
            <option value="gyroid">Gyroid</option>
            <option value="cubic">Cubic</option>
            <option value="grid">Grid</option>
          </select>
        </div>
        <div className="group">
          <div className="g-label">
            <span>Infill</span>
            <b>{s.printInfill} %</b>
          </div>
          <input
            type="range"
            min={5}
            max={100}
            step={1}
            value={s.printInfill}
            onChange={(e) => s.setPrintInfill(Number(e.target.value))}
          />
        </div>
      </div>
      <div className="dim small">
        The uniform ratio "Solve as printed" analyzes (the optimizer's budget follows it as a
        starting point). The pattern's E(ρ) curve is editable in ⚙ Settings.
      </div>

      <div className="group">
        <div className="g-label">
          <span>Analysis resolution</span>
        </div>
        <select
          value={s.resolution}
          onChange={(e) =>
            s.setResolution(e.target.value as "preview" | "normal" | "fine" | "custom")
          }
        >
          <option value="preview">Preview (fast, ~100k cells)</option>
          <option value="normal">Normal (~300k cells)</option>
          <option value="fine">Fine (~1M cells)</option>
          <option value="custom">Custom…</option>
        </select>
        {s.resolution === "custom" &&
          (() => {
            const b = s.model?.bbox;
            const vol = b ? (b[3] - b[0]) * (b[4] - b[1]) * (b[5] - b[2]) : 0;
            const cells = vol > 0 && s.customH > 0 ? vol / s.customH ** 3 : 0;
            const tooFine = cells > 4_000_000;
            const tooCoarse = cells > 0 && cells < 10_000;
            return (
              <>
                <label className="row">
                  <span className="dim small">Cell size h (mm)</span>
                  <NumInput
                    value={s.customH}
                    step={0.1}
                    min={0.05}
                    max={20}
                    onCommit={(v) => s.setCustomH(v)}
                  />
                </label>
                <div className="dim small">
                  {cells > 0
                    ? `≈ ${Math.round(cells / 1000).toLocaleString()}k cells at this size.`
                    : "Load a model to size the grid."}
                  {tooFine &&
                    " Too fine — past the 4M-cell cap; the engine will coarsen to fit."}
                  {tooCoarse && " Very coarse — expect blocky geometry and rough numbers."}
                  {!tooFine && !tooCoarse && cells > 0 && s.customH > wall + 1e-9 && (
                    <>
                      {" "}
                      Coarser than the {wall.toFixed(2)} mm wall — composite skin keeps the
                      stiffness honest, but the geometry stays blocky.
                    </>
                  )}
                  {s.snapVoxel && " Voxel snap may still adjust h to wall/k."}
                </div>
              </>
            );
          })()}
        <label className="rowcheck">
          <input
            type="checkbox"
            checked={s.snapVoxel}
            onChange={(e) => s.setSnapVoxel(e.target.checked)}
          />
          <span>Snap voxel size to the wall (h = wall/k)</span>
        </label>
        <label className="rowcheck">
          <input
            type="checkbox"
            checked={s.compositeSkin}
            onChange={(e) => s.setCompositeSkin(e.target.checked)}
          />
          <span>Composite skin (blend part-wall cells)</span>
        </label>
        <div className="dim small">
          {s.voxelInfo
            ? s.compositeSkin
              ? `Grid h = ${s.voxelInfo.h.toFixed(2)} mm — the ${wall.toFixed(2)} mm skin spans ${(wall / s.voxelInfo.h).toFixed(2)} cell layers; partially covered cells get a blended wall + infill stiffness.`
              : `Grid h = ${s.voxelInfo.h.toFixed(2)} mm — the ${wall.toFixed(2)} mm skin is ${k} cell layer${k === 1 ? "" : "s"} thick.`
            : "Grid size is computed at the next check/solve/optimize."}
          {!s.compositeSkin && s.snapVoxel && k === 1 && (
            <> Single-layer skin is coarse — raise the resolution for printed-mode accuracy.</>
          )}
        </div>
      </div>
    </>
  );
}

// ---------------- 4 · Verify setup ----------------

function StepVerify() {
  const s = useStore(
    useShallow((s) => ({
      analyzeMode: s.analyzeMode,
      setAnalyzeMode: s.setAnalyzeMode,
      buildShrink: s.buildShrink,
      setBuildShrink: s.setBuildShrink,
      buildState: s.buildState,
      setBuildState: s.setBuildState,
      perimeters: s.perimeters,
      lineWidth: s.lineWidth,
      printInfill: s.printInfill,
      pattern: s.pattern,
      runCheck: s.runCheck,
      busy: s.busy,
      runSolve: s.runSolve,
      check: s.check,
      stats: s.stats,
      hasResult: s.hasResult,
      optSummary: s.optSummary,
      printedStats: s.printedStats,
    }))
  );
  return (
    <>
      <div className="group">
        <div className="g-label">
          <span>Analyze</span>
        </div>
        <div className="seg">
          <button
            className={s.analyzeMode === "printed" ? "on" : ""}
            onClick={() => s.setAnalyzeMode("printed")}
            title="Skin solid, interior at the uniform infill from Properties — through your calibrated E(ρ) curve"
          >
            As printed
          </button>
          <button
            className={s.analyzeMode === "solid" ? "on" : ""}
            onClick={() => s.setAnalyzeMode("solid")}
            title="Fully dense E₀ everywhere — the CAD-ideal reference"
          >
            Solid material
          </button>
          <button
            className={s.analyzeMode === "buildsim" ? "on" : ""}
            onClick={() => s.setAnalyzeMode("buildsim")}
            title="FDM build simulation: inherent-strain warping + bed peel (sequential layer activation)"
          >
            Build sim
          </button>
        </div>
        <div className="dim small">
          {s.analyzeMode === "printed"
            ? `Skin ${s.perimeters} × ${s.lineWidth} mm at 100%, interior ${s.printInfill}% ${s.pattern} — accuracy is the accuracy of the calibrated E(ρ) curve.`
            : s.analyzeMode === "solid"
              ? "Fully dense E₀ everywhere — answers \"how much stiffness does printing cost me?\" next to an as-printed run."
              : "Inherent-strain build simulation: predicts warping (Solve lands in the deformed view) and bed peel. Ignores supports/loads. Uncalibrated — shape is meaningful, absolute magnitude is not."}
        </div>
        {s.analyzeMode === "buildsim" && (
          <div className="toolrow">
            <label className="dim small" style={{ display: "flex", alignItems: "center", gap: 4 }}>
              Shrink %
              <input
                type="number"
                step={0.1}
                value={(-s.buildShrink * 100).toFixed(1)}
                onChange={(e) => s.setBuildShrink(-Math.abs(parseFloat(e.target.value) || 0) / 100)}
                style={{ width: 64 }}
              />
            </label>
            <div className="seg">
              <button
                className={s.buildState === "released" ? "on" : ""}
                onClick={() => s.setBuildState("released")}
                title="Off-bed sprung shape (the predeform target)"
              >
                Released
              </button>
              <button
                className={s.buildState === "bonded" ? "on" : ""}
                onClick={() => s.setBuildState("bonded")}
                title="Distortion while still held on the bed"
              >
                On bed
              </button>
            </div>
          </div>
        )}
      </div>
      <div className="toolrow">
        <button onClick={() => void s.runCheck()} disabled={!!s.busy}>
          Check setup
        </button>
        <button onClick={() => void s.runSolve()} disabled={!!s.busy}>
          Solve once
        </button>
      </div>
      {s.check && (
        <div className={s.check.ok ? "status ok" : "status bad"}>
          {s.check.ok
            ? `Setup OK — ${s.check.islandCount} body, fully constrained.`
            : s.check.islandCount > 1
              ? `${s.check.islandCount} disconnected bodies; at least one can still move (animated).`
              : "Under-constrained — the part can still move (animated). Add supports."}
        </div>
      )}
      {s.stats && s.hasResult && !s.optSummary && (
        <div className="status ok">
          Max deflection <b>{fmtDisp(s.stats.maxDisplacement)}</b> ·{" "}
          {s.analyzeMode === "buildsim"
            ? "build sim (warp)"
            : s.printedStats
              ? `as printed (${s.printedStats.infillPct}% ${s.printedStats.pattern})`
              : "solid"}{" "}
          ·{" "}
          {s.stats.iterations} iters · {s.stats.seconds.toFixed(1)} s
        </div>
      )}
      <div className="hint">
        Check animates any remaining rigid-body freedom. Solve lands in the <b>Results</b> view —
        the field picker sits under the view tabs, playback at the bottom, min/max markers and
        click-to-edit scale & exaggeration in the legend. As-printed results land in the dock on
        the right (mass, deflection, min safety factor).
      </div>
    </>
  );
}

// ---------------- 5 · Optimize infill ----------------

function StepOptimize() {
  const s = useStore(
    useShallow((s) => ({
      optMode: s.optMode,
      goal: s.goal,
      setGoal: s.setGoal,
      budget: s.budget,
      setBudget: s.setBudget,
      levelSettings: s.levelSettings,
      setOptMode: s.setOptMode,
      solidPattern: s.solidPattern,
      setSolidPattern: s.setSolidPattern,
      selfSupport: s.selfSupport,
      overhangDeg: s.overhangDeg,
      setSelfSupport: s.setSelfSupport,
      setOverhangDeg: s.setOverhangDeg,
      retainBc: s.retainBc,
      setRetainBc: s.setRetainBc,
      perimeters: s.perimeters,
      lineWidth: s.lineWidth,
      pattern: s.pattern,
      setActiveStep: s.setActiveStep,
      nBins: s.nBins,
      setNBins: s.setNBins,
      symOn: s.symOn,
      symNormal: s.symNormal,
      symC: s.symC,
      toggleSymmetry: s.toggleSymmetry,
      setSymAxis: s.setSymAxis,
      centerSymmetry: s.centerSymmetry,
      minMemberMm: s.minMemberMm,
      setMinMemberMm: s.setMinMemberMm,
      voxelInfo: s.voxelInfo,
      runOptimize: s.runOptimize,
      busy: s.busy,
      optProgress: s.optProgress,
      optSummary: s.optSummary,
    }))
  );
  return (
    <>
      {s.optMode !== "solid" && (
        <div className="group">
          <div className="g-label">
            <span>Goal</span>
          </div>
          <div className="seg">
            <button
              className={s.goal === "budget" ? "on" : ""}
              onClick={() => s.setGoal("budget")}
              title="Maximize stiffness at a given material budget"
            >
              Stiffest at budget
            </button>
            <button
              className={s.goal === "match" ? "on" : ""}
              onClick={() => s.setGoal("match")}
              title="Find the lightest design that is as stiff as a uniform print at X%"
            >
              Match uniform stiffness
            </button>
          </div>
        </div>
      )}

      <div className="group">
        <div className="g-label">
          <span>
            {s.optMode === "solid"
              ? "Retained volume"
              : s.goal === "match"
                ? "As stiff as uniform"
                : "Infill budget"}
          </span>
          <b>{s.budget} %</b>
        </div>
        <input
          type="range"
          min={budgetBounds(s)[0]}
          max={budgetBounds(s)[1]}
          step={1}
          value={s.budget}
          onChange={(e) => s.setBudget(Number(e.target.value))}
        />
        <div className="dim small">
          {s.optMode === "solid"
            ? `Keeps ${s.budget}% of the design volume as solid material and removes the rest — the stiffest shape at that mass. Load/support regions are kept regardless.`
            : s.goal === "match"
              ? `Finds the LIGHTEST layout with the stiffness of a uniform ${s.budget}% print (a few warm-started passes search the needed budget).`
              : s.optMode === "binary"
                ? `Mean interior density: cells are either ${s.levelSettings.binaryFloorPct}% (so it prints) or 100% solid. The optimizer runs SIMP-penalized so the design goes black/white.`
                : "Mean infill of the interior — same scale as your slicer's uniform infill %. Walls and shells come on top."}
        </div>
      </div>

      <div className="group">
        <div className="g-label">
          <span>Mode</span>
        </div>
        <div className="seg">
          <button
            className={s.optMode === "graded" ? "on" : ""}
            onClick={() => s.setOptMode("graded")}
            title="Several discrete infill densities, placed from the optimized field"
          >
            Graded
          </button>
          <button
            className={s.optMode === "binary" ? "on" : ""}
            onClick={() => s.setOptMode("binary")}
            title="Hollow or solid: interior is either the printability floor or 100% dense"
          >
            Binary
          </button>
          <button
            className={s.optMode === "solid" ? "on" : ""}
            onClick={() => s.setOptMode("solid")}
            title="Topology optimization: REMOVE material to make a new lightweight shape (no infill, no walls — the kept material prints solid)"
          >
            Part Topo
          </button>
        </div>
        {s.optMode === "solid" && (
          <div className="dim small">
            Removes material to make a new shape — not infill. The kept material prints
            solid; regions under loads &amp; supports are kept automatically.
          </div>
        )}
      </div>

      {s.optMode === "binary" && (
        <div className="group">
          <div className="g-label">
            <span>Solid fill</span>
          </div>
          <select
            value={s.solidPattern}
            onChange={(e) => s.setSolidPattern(e.target.value as "rectilinear" | "concentric")}
          >
            <option value="rectilinear">Rectilinear</option>
            <option value="concentric">Concentric</option>
          </select>
        </div>
      )}

      <div className="group">
        <div className="g-label">
          <span>Self-supporting</span>
          <b>{s.selfSupport ? `${s.overhangDeg}°` : "off"}</b>
        </div>
        <label className="rowcheck">
          <input
            type="checkbox"
            checked={s.selfSupport}
            onChange={(e) => s.setSelfSupport(e.target.checked)}
          />
          <span>Print without supports (overhang constraint)</span>
        </label>
        {s.selfSupport && (
          <>
            <div className="g-label">
              <span className="dim small">Angle from horizontal — 0° off, 90° vertical</span>
            </div>
            <input
              type="range"
              min={10}
              max={80}
              step={5}
              value={s.overhangDeg}
              onChange={(e) => s.setOverhangDeg(Number(e.target.value))}
            />
          </>
        )}
        <div className="dim small">
          Build direction is Z (the print orientation).{" "}
          {s.optMode === "solid"
            ? "The shape is constrained so downward faces stay steeper than this angle — it prints without supports."
            : "Constrains the dense regions to the overhang angle; unsupported material falls to the floor density."}{" "}
          {s.selfSupport
            ? `0° allows flat overhangs (no constraint), 90° allows only vertical walls.`
            : ""}{" "}
          Advisory: the voxel staircase can still nick the angle locally.
        </div>
      </div>

      {s.optMode === "solid" ? (
        <div className="group">
          <label className="rowcheck">
            <input
              type="checkbox"
              checked={s.retainBc}
              onChange={(e) => s.setRetainBc(e.target.checked)}
            />
            <span>Keep load &amp; support regions solid</span>
          </label>
          <div className="dim small">
            Outer shape is optimized — no walls/infill.{" "}
            {s.retainBc
              ? "Material under every load and support is forced to stay (recommended)."
              : "Load/support regions can also be removed — pure topology optimization; the result may carve under a load."}
          </div>
        </div>
      ) : (
        <div className="row">
          <div className="dim small" style={{ flex: 1 }}>
            Skin {s.perimeters} × {s.lineWidth} mm · {s.pattern} —{" "}
            <a className="link" onClick={() => s.setActiveStep(3)}>
              edit in Properties
            </a>
          </div>
          {s.optMode === "binary" ? (
            <span className="dim small">2 levels (hollow/solid)</span>
          ) : s.levelSettings.mode === "manual" ? (
            <span className="dim small" title="Manual levels — change in ⚙ Settings">
              levels {s.levelSettings.manual.join("/")}%
            </span>
          ) : (
            <label className="row">
              <span className="dim small">Levels</span>
              <select value={s.nBins} onChange={(e) => s.setNBins(Number(e.target.value))}>
                <option value={2}>2</option>
                <option value={3}>3</option>
                <option value={4}>4</option>
              </select>
            </label>
          )}
        </div>
      )}

      <div className="group">
        <div className="g-label">
          <span>Symmetry</span>
          {s.symOn && <b>{symLabel(s.symNormal, s.symC)}</b>}
        </div>
        <label className="rowcheck">
          <input type="checkbox" checked={s.symOn} onChange={() => s.toggleSymmetry()} />
          <span>Planar symmetry constraint</span>
        </label>
        {s.symOn && (
          <>
            <div className="toolrow">
              {(["x", "y", "z"] as const).map((a) => {
                const aligned =
                  Math.abs(s.symNormal[a === "x" ? 0 : a === "y" ? 1 : 2]) > 0.9999;
                return (
                  <button
                    key={a}
                    className={aligned ? "on" : ""}
                    onClick={() => s.setSymAxis(a)}
                    title={`Align the plane normal with ${a.toUpperCase()}`}
                  >
                    ⊥{a.toUpperCase()}
                  </button>
                );
              })}
              <button
                onClick={() => s.centerSymmetry()}
                title="Center the plane in the part's bounding box"
              >
                ⌖ Center
              </button>
            </div>
            <div className="dim small">
              Mirror-paired cells share one density. Drag the orange plane's arrow to move it,
              the rings to tilt it (shown while editing on this step). Cells whose mirror lands
              outside the part stay free.
            </div>
          </>
        )}
      </div>

      <div className="group">
        <div className="g-label">
          <span>Minimum member size</span>
          <b>{s.minMemberMm == null ? `auto · ${(2 * s.lineWidth).toFixed(2)} mm` : "mm"}</b>
        </div>
        <div className="toolrow">
          <NumInput
            value={s.minMemberMm ?? 2 * s.lineWidth}
            step={0.1}
            min={0}
            max={10}
            onCommit={(v) => s.setMinMemberMm(v)}
          />
          <button
            className={s.minMemberMm == null ? "on" : ""}
            onClick={() => s.setMinMemberMm(null)}
            title="Back to auto (2× line width)"
          >
            auto
          </button>
        </div>
        {(() => {
          const eff = s.minMemberMm ?? 2 * s.lineWidth;
          const h = s.voxelInfo?.h ?? 0;
          const capped = h > 0 && eff / (2 * h) > 8;
          return (
            <div className="dim small">
              Thicker members print more reliably; thinner ones blur away during
              optimization (≈ the filter diameter). Defaults to 2× your line width.
              {eff <= 1e-9 && " Off — only the numerical anti-checkerboard floor applies."}
              {capped &&
                ` At this resolution (h=${h.toFixed(2)} mm) the filter is capped — the` +
                  ` enforced size tops out near ${(16 * h).toFixed(2)} mm; use a coarser mesh` +
                  ` for larger members.`}
            </div>
          );
        })()}
      </div>

      <button className="primary" onClick={() => void s.runOptimize()} disabled={!!s.busy}>
        {s.optMode === "solid" ? "Optimize shape" : "Optimize infill"}
      </button>
      {s.optProgress && (
        <div className="progress">
          <div
            className="bar"
            style={{ width: `${(100 * s.optProgress.iteration) / s.optProgress.maxIter}%` }}
          />
          <span>
            {(s.optProgress.passes ?? 1) > 1
              ? `pass ${s.optProgress.pass}/${s.optProgress.passes} · `
              : ""}
            iteration {s.optProgress.iteration} of max {s.optProgress.maxIter}
          </span>
        </div>
      )}
      {s.optSummary && (
        <div className="dim small">Results land in the panel on the right — export from step 6.</div>
      )}
    </>
  );
}

// ---------------- 6 · View & export ----------------

function StepExport() {
  const s = useStore(
    useShallow((s) => ({
      hasResult: s.hasResult,
      optSummary: s.optSummary,
      results: s.results,
      resultEpochs: s.resultEpochs,
      viewMode: s.viewMode,
      densityThreshold: s.densityThreshold,
      setDensityThreshold: s.setDensityThreshold,
      regionInfos: s.regionInfos,
      regionVisible: s.regionVisible,
      setRegionVisible: s.setRegionVisible,
      smoothIters: s.smoothIters,
      setSmoothIters: s.setSmoothIters,
      downloadShape: s.downloadShape,
      exportSlicer: s.exportSlicer,
      setExportSlicer: s.setExportSlicer,
      downloadThreeMf: s.downloadThreeMf,
      downloadStls: s.downloadStls,
      activeResultId: s.activeResultId,
      resultField: s.resultField,
      colorSteps: s.colorSteps,
      setColorSteps: s.setColorSteps,
      downloadColorThreeMf: s.downloadColorThreeMf,
    }))
  );
  return (
    <>
      {!s.hasResult && !s.optSummary && (
        <div className="hint">
          Nothing to show yet — run <b>Solve once</b> (step 4) for the Results view or{" "}
          <b>Optimize infill</b> (step 5) for density regions. View modes sit at the top of the
          viewport, the section plane at its bottom left.
        </div>
      )}
      {s.viewMode === "mesh" && (
        <div className="dim small">
          The hex mesh the solver actually runs on (winding-number voxelization at the chosen
          resolution).
        </div>
      )}
      {/* Part Topo / binary: ONE isosurface-density control — previews AND sets
          the level the exported geometry is cut from the optimized field. */}
      {s.optSummary && (s.optSummary.solid || s.optSummary.binary) && (
        <div className="group">
          <div className="g-label">
            <span>Fine-tune surface</span>
          </div>
          <input
            type="range"
            min={20}
            max={80}
            step={1}
            value={100 - s.densityThreshold}
            onChange={(e) => s.setDensityThreshold(100 - Number(e.target.value))}
          />
          <div className="row" style={{ justifyContent: "space-between" }}>
            <span className="dim small">retain less</span>
            <span className="dim small">retain more</span>
          </div>
          <div className="dim small">
            Moves the exported {s.optSummary.solid ? "surface" : "dense region"} in or out by
            re-cutting the optimized field — <b>not</b> the budget. Updates live; exports use what
            you see.
          </div>
        </div>
      )}
      {/* Graded: display-only cutaway (no single export level). */}
      {s.viewMode === "density" &&
        s.optSummary &&
        !s.optSummary.solid &&
        !s.optSummary.binary && (
          <div className="group">
            <div className="g-label">
              <span>Density cutaway</span>
              <b>{s.densityThreshold >= 10 ? `ρ ≥ ${s.densityThreshold}%` : "off"}</b>
            </div>
            <input
              type="range"
              min={0}
              max={70}
              step={1}
              value={s.densityThreshold}
              onChange={(e) => s.setDensityThreshold(Number(e.target.value))}
            />
            <div className="dim small">
              Shows only material denser than the threshold — look inside the part instead of
              just its painted surface.
            </div>
          </div>
        )}
      {s.viewMode === "infill" && s.regionInfos.length > 0 && (
        <div className="group">
          <div className="g-label">
            <span>{s.optSummary?.solid ? "Optimized body" : "Modifier regions"}</span>
          </div>
          <div className="regionlist">
            {s.regionInfos.map((r, i) => (
              <label key={i} className="regionrow">
                <input
                  type="checkbox"
                  checked={s.regionVisible[i] !== false}
                  onChange={(e) => s.setRegionVisible(i, e.target.checked)}
                />
                <span className="dot" style={{ background: rampCss(r.density / 0.8) }} />
                <span>
                  {s.optSummary?.solid
                    ? "Optimized body (kept material)"
                    : `Modifier ${i + 1} — infill ${Math.round(r.density * 100)}%`}
                </span>
              </label>
            ))}
            {!s.optSummary?.solid && (
              <div className="dim small">
                Regions nest (denser inside sparser) — toggle to inspect one at a time.
              </div>
            )}
          </div>
        </div>
      )}
      {s.viewMode === "deformed" && (
        <div className="dim small">
          Result review lives on the viewport: field picker under the view tabs, playback at the
          bottom, min/max markers and click-to-edit scale & exaggeration in the legend.
        </div>
      )}
      {(() => {
        const opt = s.results.find((r) => r.kind === "optimized");
        return opt && resultStale(opt, s.resultEpochs) ? (
          <div className="warnbanner">
            ⚠ <b>Settings changed since this optimization.</b> The mesh, loads, material, or an
            optimization input was edited — this result and export are out of date. Re-run{" "}
            <b>Optimize infill</b> (step 5) before exporting.
          </div>
        ) : null;
      })()}
      {s.optSummary && !s.optSummary.converged && (
        <div className="warnbanner">
          ⚠ <b>Exporting an unconverged result.</b> The optimization stopped at its iteration cap
          before the design settled, so these densities are preliminary. Re-run to convergence before
          printing anything load-bearing.
        </div>
      )}
      {s.optSummary && (
        <div className="group">
          <div className="g-label">
            <span>Region smoothing</span>
            <b>{s.smoothIters === 0 ? "off" : `${s.smoothIters}×`}</b>
          </div>
          <input
            type="range"
            min={0}
            max={40}
            step={1}
            value={s.smoothIters}
            onChange={(e) => s.setSmoothIters(Number(e.target.value))}
          />
          <div className="dim small">
            Melts the voxel staircase off the exported surface — updates live, exports use what
            you see. Crank it up for a fully smooth part.
          </div>
        </div>
      )}
      {s.optSummary && s.optSummary.solid && (
        <div className="group">
          <div className="g-label">
            <span>Optimized shape</span>
          </div>
          <button className="primary" onClick={() => void s.downloadShape()}>
            Download optimized shape (.stl)
          </button>
          <div className="hint">
            A single watertight body of the kept material — re-slice it (print it solid /
            100% infill) or re-import it into CAD. Material under loads &amp; supports was
            kept automatically; floating islands were dropped. A single-object project 3MF is
            a planned follow-up.
          </div>
        </div>
      )}
      {s.optSummary && !s.optSummary.solid && (
        <>
          <div className="group">
            <div className="g-label">
              <span>Hand off</span>
            </div>
            <div className="seg">
              <button
                className={s.exportSlicer === "orca" ? "on" : ""}
                onClick={() => s.setExportSlicer("orca")}
                title="OrcaSlicer project flavor"
              >
                Orca
              </button>
              <button
                className={s.exportSlicer === "bambu" ? "on" : ""}
                onClick={() => s.setExportSlicer("bambu")}
                title="Bambu Studio flavor (its renamed pattern values — no 'values replaced' dialog)"
              >
                Bambu
              </button>
              <button
                className={s.exportSlicer === "prusa" ? "on" : ""}
                onClick={() => s.setExportSlicer("prusa")}
                title="PrusaSlicer flavor (modifier volumes + per-volume infill config)"
              >
                Prusa
              </button>
            </div>
            <button className="primary" onClick={() => void s.downloadThreeMf()}>
              Download {SLICER_NAMES[s.exportSlicer]} project (.3mf)
            </button>
            <button onClick={() => void s.downloadStls()}>Download modifier STLs (.zip)</button>
          </div>
          <div className="hint">
            The 3MF opens in {SLICER_NAMES[s.exportSlicer]} with the part, the modifier volumes,
            and their infill densities already set (base infill{" "}
            {Math.round(s.optSummary.baseDensity * 100)}% on the object). Only densities are
            overridden — walls, shells, and everything else come from your own profiles.
          </div>
        </>
      )}
      {s.hasResult && (
        <div className="group">
          <div className="g-label">
            <span>Color 3MF</span>
          </div>
          <label className="row" style={{ justifyContent: "space-between", alignItems: "center" }}>
            <span className="dim small">Color steps</span>
            <NumInput
              value={s.colorSteps}
              min={COLOR_STEPS_MIN}
              max={COLOR_STEPS_MAX}
              step={1}
              onCommit={(v) => s.setColorSteps(v)}
            />
          </label>
          <button
            className="primary"
            disabled={!s.activeResultId}
            onClick={() => void s.downloadColorThreeMf()}
          >
            Download color 3MF (.3mf)
          </button>
          <div className="hint">
            The active result —{" "}
            <b>{RESULT_FIELDS.find((f) => f.value === s.resultField)?.label ?? s.resultField}</b> —
            painted into {s.colorSteps} discrete filament bands across the current contour min/max.
            Triangles are cut along the band iso-lines for sharp, watertight transitions. Opens
            painted in Bambu Studio / OrcaSlicer.
          </div>
        </div>
      )}
    </>
  );
}
