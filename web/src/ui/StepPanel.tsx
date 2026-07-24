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
  effectiveBcs,
  selectionCentroid,
  ONE_G_MMS2,
  COLOR_STEPS_MIN,
  COLOR_STEPS_MAX,
} from "../store";
import { NumInput } from "./NumInput";
import { UnitInput } from "./UnitInput";
import { RESULT_FIELDS, type Bc, type ForceMode, type LoadStep, type PatternKey } from "../types";
import { shrinkFromPhysics, ROOM_TEMP_C } from "../materials";
import { fmtDisp, fmtLen, lenUnit, rampCss } from "./fmt";
import { BC_HELP, bcLabel, KIND_DOT, KIND_LABEL, SUPPORT_KINDS } from "./bcmeta";
import { HelpTip } from "./HelpTip";
import { ValidateOrientation } from "./ValidateOrientation";
import {
  format,
  unitLabel,
  convertFromCanonical,
  convertToCanonical,
  unitDef,
  type QuantityKind,
} from "../units";

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

const BUILD_HEAD: Record<number, { title: string; sub: string }> = {
  1: { title: "Model", sub: "Drop an STL or 3MF — units are mm." },
  2: { title: "Material & grid", sub: "Material (incl. shrink) and the analysis grid." },
  3: { title: "Build simulation", sub: "Inherent-strain warping & bed peel, layer by layer." },
  4: { title: "View & export", sub: "Inspect the warp, hand off the predeformed mesh." },
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
  // Drag the slider live off a local draft; only resegment (expensive) once
  // the user releases, so mid-drag doesn't flash "Re-segmenting…" every pixel.
  const [draft, setDraft] = useState<number | null>(null);
  if (!s.model) return null;
  const cad = s.model.hasCadFaces;
  const shown = draft ?? s.segAngle;
  const commit = () => {
    if (draft !== null) {
      if (draft !== s.segAngle) void s.setSegAngle(draft);
      setDraft(null);
    }
  };
  return (
    <>
      <div className="g-label">
        <span>Surface detection</span>
        {s.segSource === "angle" && <b>{shown}°</b>}
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
            min={0}
            max={80}
            value={shown}
            onChange={(e) => setDraft(Number(e.target.value))}
            onPointerUp={commit}
            onKeyUp={commit}
            onBlur={commit}
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
    useShallow((s) => ({
      model: s.model,
      activeStep: s.activeStep,
      appMode: s.appMode,
      // re-render the whole panel subtree (all unit-bearing inputs/readouts) on
      // a unit change — saves wiring unitRev into every sub-editor.
      unitRev: s.unitRev,
    }))
  );
  const buildsim = s.appMode === "buildsim";
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
      if (
        !el ||
        el.closest("[data-bcsection]") ||
        el.closest("[data-keeptool]") ||
        el.closest(".viewer")
      )
        return;
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

  const head = (buildsim ? BUILD_HEAD : HEAD)[step];
  return (
    <section className="panel" data-bcsection={!buildsim && step === 2 ? true : undefined}>
      <div className="p-head">
        <b>
          {step} · {head.title}
        </b>
        <span>{head.sub}</span>
      </div>
      {buildsim ? (
        <>
          {step === 1 && <StepModel />}
          {step === 2 && <StepProperties />}
          {step === 3 && <StepBuildSim />}
          {step === 4 && <StepExport />}
        </>
      ) : (
        <>
          {step === 1 && <StepModel />}
          {step === 2 && <StepBcs />}
          {step === 3 && <StepProperties />}
          {step === 4 && <StepVerify />}
          {step === 5 && <StepOptimize />}
          {step === 6 && <StepExport />}
        </>
      )}
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
      rescaleModel: s.rescaleModel,
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
            {fmtLen(s.model!.bbox[5] - s.model!.bbox[2])} {lenUnit()}
          </div>
        </div>
      ) : (
        <div className="dim drophint">…or drop a file into the viewport. STL units set on import.</div>
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
              <button onClick={() => void s.rotateModel("x")} title="Rotate +5° about X">
                ⟳X
              </button>
              <button onClick={() => void s.rotateModel("y")} title="Rotate +5° about Y">
                ⟳Y
              </button>
              <button onClick={() => void s.rotateModel("z")} title="Rotate +5° about Z">
                ⟳Z
              </button>
            </div>
            <div className="dim small">
              {s.tool === "place"
                ? "Click the face the part prints ON — it turns to the build plate (Z−)."
                : "Layer-adhesion safety treats Z as the layer direction. Loads keep their world directions; results reset on reorientation."}
            </div>
          </div>
          <div className="group" data-keeptool>
            <SurfacePatchControl />
          </div>
          <div className="group">
            <div className="g-label">
              <span>Rescale</span>
              <b className="dim">wrong import unit?</b>
            </div>
            <div className="toolrow">
              <button onClick={() => void s.rescaleModel(1 / 25.4)} title="Scale ÷25.4 (mm → inch-sized)">
                ÷25.4
              </button>
              <button onClick={() => void s.rescaleModel(25.4)} title="Scale ×25.4 (inch → mm-sized)">
                ×25.4
              </button>
              <button onClick={() => void s.rescaleModel(0.1)} title="Scale ÷10">
                ÷10
              </button>
              <button onClick={() => void s.rescaleModel(10)} title="Scale ×10">
                ×10
              </button>
            </div>
            <div className="dim small">
              An STL imported in the wrong unit comes in 25.4× off. Rescale here without re-importing —
              the bounding box in the status bar confirms the size.
            </div>
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
          <HelpTip help={BC_HELP.fixed}>
            <button onClick={() => s.addBc("fixed")}>+ Fixed</button>
          </HelpTip>
          <HelpTip help={BC_HELP.elastic}>
            <button onClick={() => s.addBc("elastic")}>+ Elastic</button>
          </HelpTip>
          <HelpTip help={BC_HELP.frictionless}>
            <button onClick={() => s.addBc("frictionless")}>+ Frictionless</button>
          </HelpTip>
          <HelpTip help={BC_HELP.displacement}>
            <button onClick={() => s.addBc("displacement")}>+ Displacement</button>
          </HelpTip>
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
          <HelpTip help={BC_HELP.force}>
            <button onClick={() => s.addBc("force")}>+ Force</button>
          </HelpTip>
          <HelpTip help={BC_HELP.moment}>
            <button onClick={() => s.addBc("moment")}>+ Moment</button>
          </HelpTip>
          <HelpTip help={BC_HELP.bearing}>
            <button onClick={() => s.addBc("bearing")}>+ Bearing load</button>
          </HelpTip>
          <HelpTip help={BC_HELP.pressure}>
            <button onClick={() => s.addBc("pressure")}>+ Pressure</button>
          </HelpTip>
          <HelpTip help={BC_HELP.accel}>
            <button onClick={() => s.addBc("accel")}>+ Acceleration</button>
          </HelpTip>
          <HelpTip help={BC_HELP.mass}>
            <button onClick={() => s.addBc("mass")}>+ Point mass</button>
          </HelpTip>
        </div>
        <AttachedMassNote />
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
              <b>{format(s.brushRadius * 2, "length")}</b>
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
        {bc.kind === "accel" ? (
          <span className="dim">whole part</span>
        ) : (
          <span className="dim">{bc.tris.length ? `${bc.tris.length} tris` : "select…"}</span>
        )}
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
      {bc.kind === "bearing" && <BearingEditor bc={bc} step={step} />}
      {bc.kind === "moment" && <MomentEditor bc={bc} step={step} />}
      {bc.kind === "accel" && <AccelEditor bc={bc} step={step} />}
      {bc.kind === "mass" && <MassEditor bc={bc} step={step} />}
      {bc.kind === "displacement" && <DisplacementEditor bc={bc} />}
      {bc.kind === "pressure" && (
        <div className="bcparams" onClick={(e) => e.stopPropagation()}>
          <label>
            p
            <UnitInput
              value={step ? step.overrides[bc.id]?.pressure ?? bc.pressure ?? 0 : bc.pressure ?? 0}
              kind="pressure"
              step={0.01}
              onCommit={(v) =>
                step ? s.setStepPressure(step.id, bc.id, v) : s.updateBcParams(bc.id, { pressure: v })
              }
            />
          </label>
          <span className="dim">{unitLabel("pressure")}</span>
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
  kind,
  step = 1,
}: {
  /** CANONICAL component values. */
  values: [number, number, number];
  /** Receives CANONICAL components. */
  onChange: (v: [number, number, number]) => void;
  label?: string;
  /** Static unit label (dimensionless vectors, e.g. a direction). */
  unit?: string;
  /** Quantity kind — when set, each component is shown in the active display
   *  unit and converted back to canonical on commit (force / moment). */
  kind?: QuantityKind;
  step?: number;
}) {
  const lbl = kind ? unitLabel(kind) : unit;
  const show = (v: number) =>
    kind ? Number(convertFromCanonical(v, kind).toFixed(unitDef(kind).decimals)) : v;
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
            value={show(values[i])}
            step={step}
            onCommit={(v) => {
              const nv = [...values] as [number, number, number];
              nv[i] = kind ? convertToCanonical(v, kind) : v;
              onChange(nv);
            }}
          />
          {lbl && <span className="funit">{lbl}</span>}
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
          kind="force"
          step={1}
          onChange={(nf) => s.updateBcParams(bc.id, { force: nf })}
        />
      ) : (
        <>
          <div className="forcerow">
            <span className="flabel">|F|</span>
            <UnitInput
              className="fnum"
              value={bc.forceMag ?? 0}
              kind="force"
              step={1}
              onCommit={(v) => s.setForceMag(bc.id, v)}
            />
            <span className="funit">{unitLabel("force")}</span>
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
        <VectorInput values={f} label="F" kind="force" step={1} onChange={setVec} />
      ) : (
        <>
          <div className="forcerow">
            <span className="flabel">|F|</span>
            <UnitInput
              className="fnum"
              value={mag}
              kind="force"
              step={1}
              onCommit={(v) => setVec([dir[0] * v, dir[1] * v, dir[2] * v])}
            />
            <span className="funit">{unitLabel("force")}</span>
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

/** Unit vector, or `fallback` when the input is ~zero. */
function unitOr(
  d: [number, number, number],
  fallback: [number, number, number]
): [number, number, number] {
  const l = Math.hypot(d[0], d[1], d[2]);
  return l > 1e-9 ? [d[0] / l, d[1] / l, d[2] / l] : fallback;
}

/** Bearing-load editor: the push force (reusing the Force editor — components OR
 *  direction + magnitude, pick/flip/surface-normal, AND per-load-step values)
 *  plus a live cylinder readout. The force vector says which way the pin
 *  presses; the loaded half + cosine distribution + axial reject all happen in
 *  the solver once the selection fits a cylinder. */
function BearingEditor({ bc, step }: { bc: Bc; step?: LoadStep }) {
  return (
    <>
      <ForceEditor bc={bc} step={step} />
      <div className="forceedit" onClick={(e) => e.stopPropagation()} style={{ paddingTop: 0 }}>
        <BearingCylStatus bc={bc} />
      </div>
    </>
  );
}

/** Cylinder-fit feedback for a bearing load: the fitted ⌀/axis once valid, the
 *  hard-block message when a non-cylindrical face was picked, or a prompt. */
function BearingCylStatus({ bc }: { bc: Bc }) {
  if (bc.cylError) {
    return (
      <div className="dim small" style={{ color: "#c0392b" }}>
        {bc.cylError}
      </div>
    );
  }
  if (bc.tris.length === 0) {
    return (
      <div className="dim small">
        Pick a <b>cylindrical</b> surface (a bore or boss). The load presses the wall in the F
        direction, cosine-distributed over the contacted half; any component along the axis is
        ignored.
      </div>
    );
  }
  if (bc.cyl?.ok) {
    const a = bc.cyl.axis;
    return (
      <div className="dim small">
        ⌀ {format(bc.cyl.radius * 2, "length")} · axis ({r4(a[0])}, {r4(a[1])}, {r4(a[2])})
      </div>
    );
  }
  return <div className="dim small">Checking cylindricity…</div>;
}

/** Moment editor: edits the active load step's vector when multi-step, else the
 *  base BC (mirrors ForceEditor). */
function MomentEditor({ bc, step }: { bc: Bc; step?: LoadStep }) {
  return step ? <StepMomentEditor bc={bc} step={step} /> : <BaseMomentEditor bc={bc} />;
}

/** Per-step moment editor — edits the active load step's vector. Components, or
 *  axis + magnitude derived from it. Mode is a local view choice. */
function StepMomentEditor({ bc, step }: { bc: Bc; step: LoadStep }) {
  const s = useStore(
    useShallow((s) => ({
      activeBcId: s.activeBcId,
      tool: s.tool,
      setActiveBc: s.setActiveBc,
      setTool: s.setTool,
      setStepMoment: s.setStepMoment,
      aimStepMomentAlongNormal: s.aimStepMomentAlongNormal,
    }))
  );
  const [mode, setMode] = useState<ForceMode>(bc.momentMode ?? "components");
  const m = step.overrides[bc.id]?.moment ?? bc.moment ?? [0, 0, 0];
  const setVec = (v: [number, number, number]) => s.setStepMoment(step.id, bc.id, v);
  const mag = Math.hypot(m[0], m[1], m[2]);
  const dir: [number, number, number] =
    mag > 1e-9 ? [m[0] / mag, m[1] / mag, m[2] / mag] : bc.momentDir ?? [0, 0, 1];
  const picking = s.activeBcId === bc.id && s.tool === "pickdir";
  const round = (x: number) => Math.round(x * 1000) / 1000;
  return (
    <div className="forceedit" onClick={(e) => e.stopPropagation()}>
      <ForceModeToggle mode={mode} onMode={setMode} />
      {mode === "components" ? (
        <VectorInput values={m} label="M" kind="moment" step={10} onChange={setVec} />
      ) : (
        <>
          <div className="forcerow">
            <span className="flabel">|M|</span>
            <UnitInput
              className="fnum"
              value={mag}
              kind="moment"
              step={10}
              onCommit={(v) => setVec([dir[0] * v, dir[1] * v, dir[2] * v])}
            />
            <span className="funit">{unitLabel("moment")}</span>
          </div>
          <VectorInput
            values={[round(dir[0]), round(dir[1]), round(dir[2])]}
            label="a"
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
            onFlip={() => setVec([-m[0], -m[1], -m[2]])}
            onNormal={() => s.aimStepMomentAlongNormal(step.id, bc.id)}
          />
          <div className="dim small">This step's moment vector (right-hand rule about the axis).</div>
        </>
      )}
    </div>
  );
}

/** Single-step (base) moment editor: a moment vector (components OR axis +
 *  magnitude), N·mm. Applied as a deformable distributed couple about the
 *  selection centroid. */
function BaseMomentEditor({ bc }: { bc: Bc }) {
  const s = useStore(
    useShallow((s) => ({
      activeBcId: s.activeBcId,
      tool: s.tool,
      setActiveBc: s.setActiveBc,
      setTool: s.setTool,
      updateBcParams: s.updateBcParams,
      setMomentDir: s.setMomentDir,
      flipMomentDir: s.flipMomentDir,
      resetMomentDirToNormal: s.resetMomentDirToNormal,
    }))
  );
  const active = s.activeBcId === bc.id;
  const mode = bc.momentMode ?? "components";
  const m = bc.moment ?? [0, 0, 0];
  const dir = bc.momentDir ?? [0, 0, 1];
  const mag = bc.momentMag ?? 0;
  const picking = active && s.tool === "pickdir";
  const setMode = (mm: ForceMode) => {
    if (mm === mode) return;
    if (mm === "components") {
      // The resolved vector is already current; just switch the editor view.
      s.updateBcParams(bc.id, { momentMode: "components" });
    } else {
      const len = Math.hypot(m[0], m[1], m[2]);
      const nd = unitOr(m, dir);
      const nm = len > 1e-9 ? len : mag || 100;
      s.updateBcParams(bc.id, { momentMode: "direction", momentDir: nd, momentMag: nm });
    }
  };
  return (
    <div className="forceedit" onClick={(e) => e.stopPropagation()}>
      <ForceModeToggle mode={mode} onMode={setMode} />
      {mode === "components" ? (
        <VectorInput
          values={m}
          label="M"
          kind="moment"
          step={10}
          onChange={(nm) => s.updateBcParams(bc.id, { moment: nm })}
        />
      ) : (
        <>
          <div className="forcerow">
            <span className="flabel">|M|</span>
            <UnitInput
              className="fnum"
              value={mag}
              kind="moment"
              step={10}
              onCommit={(v) => {
                const nd = unitOr(dir, [0, 0, 1]);
                s.updateBcParams(bc.id, {
                  momentMag: v,
                  moment: [nd[0] * v, nd[1] * v, nd[2] * v],
                });
              }}
            />
            <span className="funit">{unitLabel("moment")}</span>
          </div>
          <VectorInput
            values={[r4(dir[0]), r4(dir[1]), r4(dir[2])]}
            label="a"
            step={0.1}
            onChange={(d) => s.setMomentDir(bc.id, d)}
          />
          <ForceDirTools
            picking={picking}
            disabledNormal={bc.tris.length === 0}
            onPick={() => {
              s.setActiveBc(bc.id);
              s.setTool(picking ? "orbit" : "pickdir");
            }}
            onFlip={() => s.flipMomentDir(bc.id)}
            onNormal={() => s.resetMomentDirToNormal(bc.id)}
          />
          <div className="dim small">
            {picking
              ? "Click a triangle — its normal becomes the moment axis (right-hand rule)."
              : "Axis of rotation (right-hand rule). ‘Surface normal’ aims it along the selection."}
          </div>
        </>
      )}
      <div className="dim small">
        Applied as a distributed force couple about the selection's centroid (the voxel mesh has no
        rotational DOFs).
      </div>
    </div>
  );
}

/** Informational line under the Loads group: total attached (dummy) mass —
 *  external components, EXCLUDED from the printed-part mass (DESIGN §16 dec. 10)
 *  — plus a soft, non-blocking advisory when a mass has no acceleration to feel
 *  (dec. 11: "added the motor, forgot gravity"). */
function AttachedMassNote() {
  const s = useStore(useShallow((s) => ({ bcs: s.bcs, loadSteps: s.loadSteps })));
  const masses = s.bcs.filter((b) => b.kind === "mass");
  if (masses.length === 0) return null;
  const totalG = masses.reduce((a, b) => a + (b.massGrams ?? 0), 0);
  const multi = s.loadSteps.length > 1;
  const anyActiveAccel = s.loadSteps.some((ls) =>
    effectiveBcs(s.bcs, multi ? ls : undefined).some((b) => b.kind === "accel")
  );
  return (
    <div className="dim small" style={{ marginTop: 6 }}>
      Attached masses: {format(totalG, "mass")} — external components, excluded from the printed
      part mass.
      {!anyActiveAccel && (
        <div style={{ color: "#c07a0a", marginTop: 2 }}>
          ⚠ No acceleration is active — add an Acceleration (e.g. gravity) or the masses load nothing.
        </div>
      )}
    </div>
  );
}

/** Acceleration editor (DESIGN §16): a selection-less world acceleration every
 *  mass feels as F = m·a. Dual-mode like force (components OR direction + |a|),
 *  shown in g by default, with a one-click "1 g ↓" preset. `step` set ⇒ edits
 *  that load step's vector; otherwise the base BC. */
function AccelEditor({ bc, step }: { bc: Bc; step?: LoadStep }) {
  return step ? <StepAccelEditor bc={bc} step={step} /> : <BaseAccelEditor bc={bc} />;
}

const ACCEL_CONVENTION = "Every mass feels F = m·a along this vector — gravity is 1 g down.";

function BaseAccelEditor({ bc }: { bc: Bc }) {
  const s = useStore(
    useShallow((s) => ({
      setAccelMode: s.setAccelMode,
      setAccelMag: s.setAccelMag,
      setAccelDir: s.setAccelDir,
      flipAccelDir: s.flipAccelDir,
      setAccelOneGDown: s.setAccelOneGDown,
      updateBcParams: s.updateBcParams,
    }))
  );
  const mode = bc.accelMode ?? "direction";
  const a = bc.accel ?? [0, 0, 0];
  const dir = bc.accelDir ?? [0, 0, -1];
  return (
    <div className="forceedit" onClick={(e) => e.stopPropagation()}>
      <ForceModeToggle mode={mode} onMode={(m) => s.setAccelMode(bc.id, m)} />
      {mode === "components" ? (
        <VectorInput
          values={a}
          label="a"
          kind="acceleration"
          step={1}
          onChange={(na) => s.updateBcParams(bc.id, { accel: na })}
        />
      ) : (
        <>
          <div className="forcerow">
            <span className="flabel">|a|</span>
            <UnitInput
              className="fnum"
              value={bc.accelMag ?? 0}
              kind="acceleration"
              step={1}
              onCommit={(v) => s.setAccelMag(bc.id, v)}
            />
            <span className="funit">{unitLabel("acceleration")}</span>
          </div>
          <VectorInput
            values={[r4(dir[0]), r4(dir[1]), r4(dir[2])]}
            label="d"
            step={0.1}
            onChange={(d) => s.setAccelDir(bc.id, d)}
          />
          <div className="toolrow">
            <button onClick={() => s.setAccelOneGDown(bc.id)} title="1 g straight down (−Z)">
              ↓ 1 g down
            </button>
            <button onClick={() => s.flipAccelDir(bc.id)} title="Reverse the acceleration direction">
              ⇄ Flip
            </button>
          </div>
        </>
      )}
      <div className="dim small">{ACCEL_CONVENTION}</div>
    </div>
  );
}

/** Per-step acceleration editor — edits the active load step's vector. */
function StepAccelEditor({ bc, step }: { bc: Bc; step: LoadStep }) {
  const s = useStore(useShallow((s) => ({ setStepAccel: s.setStepAccel })));
  const [mode, setMode] = useState<ForceMode>(bc.accelMode ?? "direction");
  const a = step.overrides[bc.id]?.accel ?? bc.accel ?? [0, 0, 0];
  const setVec = (v: [number, number, number]) => s.setStepAccel(step.id, bc.id, v);
  const mag = Math.hypot(a[0], a[1], a[2]);
  const dir: [number, number, number] =
    mag > 1e-9 ? [a[0] / mag, a[1] / mag, a[2] / mag] : bc.accelDir ?? [0, 0, -1];
  const round = (x: number) => Math.round(x * 1000) / 1000;
  return (
    <div className="forceedit" onClick={(e) => e.stopPropagation()}>
      <ForceModeToggle mode={mode} onMode={setMode} />
      {mode === "components" ? (
        <VectorInput values={a} label="a" kind="acceleration" step={1} onChange={setVec} />
      ) : (
        <>
          <div className="forcerow">
            <span className="flabel">|a|</span>
            <UnitInput
              className="fnum"
              value={mag}
              kind="acceleration"
              step={1}
              onCommit={(v) => setVec([dir[0] * v, dir[1] * v, dir[2] * v])}
            />
            <span className="funit">{unitLabel("acceleration")}</span>
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
          <div className="toolrow">
            <button onClick={() => setVec([0, 0, -ONE_G_MMS2])} title="1 g straight down (−Z)">
              ↓ 1 g down
            </button>
            <button onClick={() => setVec([-a[0], -a[1], -a[2]])} title="Reverse the acceleration direction">
              ⇄ Flip
            </button>
          </div>
        </>
      )}
      <div className="dim small">This step's acceleration. {ACCEL_CONVENTION}</div>
    </div>
  );
}

/** Point-mass editor (DESIGN §16): the component mass + its CG position (XYZ
 *  DRO, initialized at the patch centroid), with a live F = m·a and transported
 *  couple |M| readout for the shown step so the lever arm is visible pre-solve.
 *  The rigid-coupling toggle is present but disabled — a later milestone. */
function MassEditor({ bc, step }: { bc: Bc; step?: LoadStep }) {
  const s = useStore(
    useShallow((s) => ({
      setMassGrams: s.setMassGrams,
      setMassPoint: s.setMassPoint,
      resetMassPointToCentroid: s.resetMassPointToCentroid,
      bcs: s.bcs,
      positions: s.model?.positions,
    }))
  );
  const point = bc.point ?? [0, 0, 0];
  const massG = bc.massGrams ?? 0;
  // Summed active acceleration for the shown step (world mm/s²) — resolves the
  // per-step overrides + drops deactivated entities via effectiveBcs.
  const a: [number, number, number] = [0, 0, 0];
  for (const b of effectiveBcs(s.bcs, step)) {
    if (b.kind === "accel" && b.accel) {
      a[0] += b.accel[0];
      a[1] += b.accel[1];
      a[2] += b.accel[2];
    }
  }
  const aMag = Math.hypot(a[0], a[1], a[2]);
  // F = m·a (N): mass in tonne × accel in mm/s². Transported couple |M| = |(p−c)×F|.
  const mT = massG * 1e-6;
  const F: [number, number, number] = [mT * a[0], mT * a[1], mT * a[2]];
  const Fmag = mT * aMag;
  const c = selectionCentroid(s.positions, bc.tris) ?? point;
  const arm: [number, number, number] = [point[0] - c[0], point[1] - c[1], point[2] - c[2]];
  const Mmag = Math.hypot(
    arm[1] * F[2] - arm[2] * F[1],
    arm[2] * F[0] - arm[0] * F[2],
    arm[0] * F[1] - arm[1] * F[0]
  );
  const noSel = bc.tris.length === 0;
  return (
    <div className="forceedit" onClick={(e) => e.stopPropagation()}>
      <div className="forcerow">
        <span className="flabel">m</span>
        <UnitInput
          className="fnum"
          value={massG}
          kind="mass"
          step={1}
          onCommit={(v) => s.setMassGrams(bc.id, v)}
        />
        <span className="funit">{unitLabel("mass")}</span>
      </div>
      {noSel ? (
        <div className="dim small">
          Select the mounting surface — the CG starts at its centre, then offset it to the
          component's true centre of gravity below.
        </div>
      ) : (
        <>
          <div className="dim small" style={{ marginTop: 4 }}>
            Centre of gravity ({unitLabel("length")})
          </div>
          <VectorInput values={point} label="" kind="length" step={1} onChange={(p) => s.setMassPoint(bc.id, p)} />
          <div className="toolrow">
            <button onClick={() => s.resetMassPointToCentroid(bc.id)} title="Snap the CG back to the patch centre">
              ◎ CG at patch centre
            </button>
          </div>
          <div className="dim small">
            {aMag < 1e-9 ? (
              "Add an Acceleration (e.g. gravity) — a mass loads nothing without one."
            ) : (
              <>
                Under the shown load: |F| = {format(Fmag, "force")}, transported |M| ={" "}
                {format(Mmag, "moment")}.
              </>
            )}
          </div>
        </>
      )}
      {/* Coupling behavior (DESIGN §16): the RIGID mount (engine-complete, see
          crates/filasim-core/src/rigid.rs) is HIDDEN for now — its penalty term
          costs ~3× the MGCG iterations of a normal solve, not worth exposing yet.
          Masses stay deformable (load-only). Re-enable by rendering the toggle:
            <div className="seg" title="How the mass couples to the mounting patch">
              <button className={bc.behavior === "rigid" ? "" : "on"}
                onClick={() => s.updateBcParams(bc.id, { behavior: "deformable" })}>
                Deformable (load only)</button>
              <button className={bc.behavior === "rigid" ? "on" : ""}
                onClick={() => s.updateBcParams(bc.id, { behavior: "rigid" })}>
                Rigid (stiffens face)</button>
            </div> */}
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
            <UnitInput
              className="fnum"
              value={disp[i]}
              kind="length"
              step={0.1}
              disabled={!axes[i]}
              onCommit={(v) => {
                const d = [...disp] as [number, number, number];
                d[i] = v;
                s.updateBcParams(bc.id, { disp: d });
              }}
            />
            <span className="funit">{unitLabel("length")}</span>
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
          E = {format(s.material.e0, "modulus")} · ν = {s.material.nu} · ρ ={" "}
          {format(s.material.density, "density")} · σₜ = {format(s.material.strength, "stress")} —{" "}
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
            <b>{unitLabel("length")}</b>
          </div>
          <UnitInput
            value={s.lineWidth}
            kind="length"
            step={0.05}
            min={0.1}
            max={1.5}
            onCommit={(v) => s.setLineWidth(v)}
          />
        </div>
      </div>
      <div className="dim small">
        ≈ {format(wall, "length")} solid wall — what the analysis assumes and what the 3MF's
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
            <b>{unitLabel("length")}</b>
          </div>
          <UnitInput
            value={s.layerHeight}
            kind="length"
            step={0.05}
            min={0.04}
            max={0.6}
            onCommit={(v) => s.setLayerHeight(v)}
          />
        </div>
      </div>
      <div className="dim small">
        {s.topBottomLayers > 0
          ? `≈ ${format(s.topBottomLayers * s.layerHeight, "length")} solid shells on up/down-facing surfaces — exported as top/bottom shell layers.`
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
                  <span className="dim small">Cell size h ({unitLabel("length")})</span>
                  <UnitInput
                    value={s.customH}
                    kind="length"
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
                      Coarser than the {format(wall, "length")} wall — composite skin keeps the
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
              ? `Grid h = ${format(s.voxelInfo.h, "length")} — the ${format(wall, "length")} skin spans ${(wall / s.voxelInfo.h).toFixed(2)} cell layers; partially covered cells get a blended wall + infill stiffness.`
              : `Grid h = ${format(s.voxelInfo.h, "length")} — the ${format(wall, "length")} skin is ${k} cell layer${k === 1 ? "" : "s"} thick.`
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
      analysisType: s.analysisType,
      setAnalysisType: s.setAnalysisType,
      modalModeCount: s.modalModeCount,
      setModalModeCount: s.setModalModeCount,
      freeFree: s.freeFree,
      setFreeFree: s.setFreeFree,
      runModal: s.runModal,
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
  const modal = s.analysisType === "modal";
  return (
    <>
      <div className="group">
        <div className="g-label">
          <span>Analysis</span>
        </div>
        <div className="seg">
          <button
            className={!modal ? "on" : ""}
            onClick={() => s.setAnalysisType("static")}
            title="Linear static solve under the loads & supports — deflection, stress, safety factor"
          >
            Static
          </button>
          <button
            className={modal ? "on" : ""}
            onClick={() => s.setAnalysisType("modal")}
            title="Natural frequencies & mode shapes (constrained, undamped) — how the part resonates as supported"
          >
            Modal
          </button>
        </div>
        <div className="dim small">
          {modal
            ? "Constrained, undamped modal — the lowest natural frequencies + mode shapes of the part as supported by the first load case. Force-free; the selected stiffness (below) sets both stiffness and mass."
            : "Linear static solve under the current loads & supports."}
        </div>
      </div>
      <div className="group">
        <div className="g-label">
          <span>Stiffness</span>
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
        </div>
        <div className="dim small">
          {s.analyzeMode === "printed"
            ? `Skin ${s.perimeters} × ${format(s.lineWidth, "length")} at 100%, interior ${s.printInfill}% ${s.pattern} — accuracy is the accuracy of the calibrated E(ρ) curve.`
            : "Fully dense E₀ everywhere — answers \"how much stiffness does printing cost me?\" next to an as-printed run."}
        </div>
      </div>
      {modal && (
        <div className="group">
          <div className="g-label">
            <span>Modes</span>
          </div>
          <div className="numrow">
            <input
              type="number"
              min={1}
              max={20}
              step={1}
              value={s.modalModeCount}
              disabled={!!s.busy}
              onChange={(e) => s.setModalModeCount(Number(e.target.value))}
            />
            <span className="dim small">natural frequencies to compute (1–20; higher = slower).</span>
          </div>
          <label className="checkrow" title="Analyze a part with NO supports: the 6 rigid-body modes are soft-anchored and discarded, leaving the flexible modes. Indicative — validate against FEA.">
            <input
              type="checkbox"
              checked={s.freeFree}
              disabled={!!s.busy}
              onChange={(e) => s.setFreeFree(e.target.checked)}
            />
            <span>Unconstrained (free-free) — discard rigid-body modes</span>
          </label>
        </div>
      )}
      <div className="toolrow">
        <button onClick={() => void s.runCheck()} disabled={!!s.busy}>
          Check setup
        </button>
        {modal ? (
          <button onClick={() => void s.runModal()} disabled={!!s.busy}>
            Run modal
          </button>
        ) : (
          <button onClick={() => void s.runSolve()} disabled={!!s.busy}>
            Solve once
          </button>
        )}
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
      {!modal && s.stats && s.hasResult && !s.optSummary && (
        <div className="status ok">
          Max deflection <b>{fmtDisp(s.stats.maxDisplacement)}</b> ·{" "}
          {s.printedStats ? `as printed (${s.printedStats.infillPct}% ${s.printedStats.pattern})` : "solid"} ·{" "}
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

// ---------------- Build Sim · 3 · Simulate ----------------

function StepBuildSim() {
  const s = useStore(
    useShallow((s) => ({
      material: s.material,
      buildState: s.buildState,
      buildBedTemp: s.buildBedTemp,
      buildChamberTemp: s.buildChamberTemp,
      setBuildBedTemp: s.setBuildBedTemp,
      setBuildChamberTemp: s.setBuildChamberTemp,
      runSolve: s.runSolve,
      busy: s.busy,
      stats: s.stats,
      hasResult: s.hasResult,
      buildProgress: s.buildProgress,
      buildResult: s.buildResult,
      openSettings: s.openSettings,
    }))
  );
  const bp = s.buildProgress;
  const br = s.buildResult;
  const pct = bp && bp.total > 0 ? Math.round((bp.done / bp.total) * 100) : 0;
  // ONE material: the Properties selection drives the build sim too. Thermal
  // data (tLock + cte) derives the shrink from physics; without it the raw
  // material shrink applies (legacy path).
  const phys = shrinkFromPhysics(s.material, ROOM_TEMP_C);
  return (
    <>
      <div className="group">
        <div className="g-label">
          <span>Material</span>
          <b>{s.material.name}</b>
        </div>
        {phys ? (
          <div className="dim small">
            Inherent-strain warp via sequential layer activation. Shrink from physics — locks at{" "}
            {s.material.tLock} °C (Tg/Tc), CTE {((s.material.cte ?? 0) * 1e6).toFixed(0)} ppm/°C: XY{" "}
            {(Math.abs(phys.shrink) * 100).toFixed(2)}% · Z{" "}
            {(Math.abs(phys.shrinkZ) * 100).toFixed(2)}% (lock → {ROOM_TEMP_C} °C room) — from
            Tg/CTE, edit in{" "}
            <button className="linkbtn" onClick={() => s.openSettings(true)}>
              ⚙ Settings
            </button>
            . Uncalibrated: the warp shape is meaningful, the absolute magnitude is not.
          </div>
        ) : (
          <div className="dim small">
            Inherent-strain warp via sequential layer activation. Shrink is a material property (
            <b>{s.material.name}</b>: XY {(s.material.shrink * 100).toFixed(2)}% · Z{" "}
            {((s.material.shrinkZ ?? s.material.shrink) * 100).toFixed(2)}%) — edit it in{" "}
            <button className="linkbtn" onClick={() => s.openSettings(true)}>
              ⚙ Settings
            </button>
            . Uncalibrated: the warp shape is meaningful, the absolute magnitude is not.
          </div>
        )}
      </div>
      {phys && (
        <>
          <div className="duo">
            <div className="group">
              <div className="g-label">
                <span>Bed temp</span>
                <b>°C</b>
              </div>
              <NumInput
                value={s.buildBedTemp}
                step={5}
                min={0}
                max={200}
                onCommit={(v) => s.setBuildBedTemp(v)}
              />
            </div>
            <div className="group">
              <div className="g-label">
                <span>Chamber temp</span>
                <b>°C</b>
              </div>
              <NumInput
                value={s.buildChamberTemp}
                step={5}
                min={0}
                max={150}
                onCommit={(v) => s.setBuildChamberTemp(v)}
              />
            </div>
          </div>
          <div className="dim small">
            Bed & chamber set the temperature ladder — which layers are still warm while the part
            builds. The total shrink (lock → room) is unchanged.
          </div>
        </>
      )}
      <div className="toolrow">
        <button className="primary" onClick={() => void s.runSolve()} disabled={!!s.busy}>
          Run build simulation
        </button>
      </div>
      {bp && (
        <div className="group">
          <div className="g-label">
            <span>Simulating — layer {bp.done}{bp.total > 0 ? ` of ${bp.total}` : ""}</span>
            {bp.total > 0 && <b>{pct}%</b>}
          </div>
          <div
            style={{
              height: 6,
              borderRadius: 3,
              background: "rgba(0,0,0,0.12)",
              overflow: "hidden",
            }}
          >
            <div
              style={{
                height: "100%",
                width: `${pct}%`,
                background: "var(--accent, #e8722b)",
                transition: "width 0.1s linear",
              }}
            />
          </div>
        </div>
      )}
      {!bp && s.stats && s.hasResult && (
        <div className="status ok">
          Max warp <b>{fmtDisp(s.stats.maxDisplacement)}</b> ·{" "}
          {s.buildState === "released" ? "released (off-bed)" : "on bed"} ·{" "}
          {s.stats.seconds.toFixed(1)} s
        </div>
      )}
      {!bp && br && (
        <div className="dim small">
          Both states saved — switch On&nbsp;bed / Released on the <b>Results</b> bar (top). On bed{" "}
          <b>{fmtDisp(br.bondedMax)}</b> · released <b>{fmtDisp(br.releasedMax)}</b>. Stiffness &amp;
          strain field:{" "}
          <b>{br.densityAware ? "as-printed infill density" : "solid hull"}</b>
          {br.densityAware ? "" : " (optimize the part first to use the printed infill)"}.
          <br />
          Bed peel — peak traction <b>{format(br.peakLift, "stress")}</b> · shear{" "}
          <b>{format(br.peakShear, "stress")}</b>. Pick <b>Peel traction</b> on the Results bar's field
          menu to see where the part wants to lift (mesh-independent, uncalibrated indicator).
        </div>
      )}
      <div className="hint">
        Build sim ignores supports/loads — its only inputs are the part, the material shrink, the
        as-printed infill density (when optimized), and the build plate. It runs on a coarser grid
        than analysis for speed. Solve lands in the deformed <b>Results</b> view; switch On&nbsp;bed /
        Released there to compare with no re-solve.
      </div>
    </>
  );
}

// ---------------- 5 · Optimize infill ----------------

/** Inline density-level list next to the count selector. Empty = auto
 *  placement (levels picked from the optimized field); a comma-separated
 *  list (e.g. "10, 40, 70") pins them manually and syncs the count
 *  selector. Changing the count re-spreads a pinned list; clearing the
 *  box returns to auto. */
function LevelsInline() {
  const s = useStore(
    useShallow((s) => ({
      levelSettings: s.levelSettings,
      updateLevelSettings: s.updateLevelSettings,
      nBins: s.nBins,
      setNBins: s.setNBins,
      optSummary: s.optSummary,
    }))
  );
  const manual = s.levelSettings.mode === "manual";
  const shown = manual ? s.levelSettings.manual.join(", ") : "";
  // Auto mode: once a result exists, surface the levels auto placement chose —
  // ready to copy/tweak into a pinned list.
  const sum = s.optSummary;
  const placeholder =
    !manual && sum && !sum.solid && !sum.binary && sum.bins.length
      ? `auto: ${sum.bins.map((b) => Math.round(b.density * 100)).join(", ")}`
      : "auto";
  const [text, setText] = useState(shown);
  useEffect(() => {
    setText(shown);
  }, [shown]);
  const commit = () => {
    if (text.trim() === "") {
      if (manual) s.updateLevelSettings({ mode: "auto" });
      else setText(shown);
      return;
    }
    const vals = text
      .split(/[,;\s]+/)
      .map(Number)
      .filter((v) => Number.isFinite(v) && v >= 1 && v <= 100)
      .map(Math.round);
    const uniq = [...new Set(vals)].sort((a, b) => a - b);
    if (uniq.length >= 2 && uniq.length <= 8) {
      s.updateLevelSettings({ mode: "manual", manual: uniq });
      if (uniq.length !== s.nBins) s.setNBins(uniq.length);
    } else {
      setText(shown);
    }
  };
  return (
    <input
      type="text"
      value={text}
      size={11}
      placeholder={placeholder}
      title="Density levels in % — a comma-separated list (e.g. 10, 40, 70) pins them; empty = auto placement from the optimized field"
      onChange={(e) => setText(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => {
        if (e.key === "Enter") (e.target as HTMLInputElement).blur();
      }}
    />
  );
}

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
            Skin {s.perimeters} × {format(s.lineWidth, "length")} · {s.pattern} —{" "}
            <a className="link" onClick={() => s.setActiveStep(3)}>
              edit in Properties
            </a>
          </div>
          {s.optMode === "binary" ? (
            <span className="dim small">2 levels (hollow/solid)</span>
          ) : (
            <label className="row">
              <span className="dim small">Levels</span>
              <select value={s.nBins} onChange={(e) => s.setNBins(Number(e.target.value))}>
                <option value={2}>2</option>
                <option value={3}>3</option>
                <option value={4}>4</option>
                {s.nBins > 4 && <option value={s.nBins}>{s.nBins}</option>}
              </select>
              <LevelsInline />
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
          <b>
            {s.minMemberMm == null
              ? `auto · ${format(2 * s.lineWidth, "length")}`
              : unitLabel("length")}
          </b>
        </div>
        <div className="toolrow">
          <UnitInput
            value={s.minMemberMm ?? 2 * s.lineWidth}
            kind="length"
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
                ` At this resolution (h=${format(h, "length")}) the filter is capped — the` +
                  ` enforced size tops out near ${format(16 * h, "length")}; use a coarser mesh` +
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

      <ValidateOrientation />
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
            {!s.optSummary.binary &&
              " A level pinned at 100% also gets rectilinear infill on its region, so it slices truly solid."}
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
