// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Material Manager — the library surface for materials: one list for FDM and
// isotropic together (a per-material process selector switches the kind),
// grouped detail fields instead of the old 14-column Settings tables, and a
// chart band (stress–strain, property comparison, layer anisotropy, readout).
// Entered from ⚙ Settings → Materials or the Properties step's "edit" link.

import { useEffect, useRef, useState } from "react";
import { useShallow } from "zustand/shallow";
import { useStore } from "../store";
import { NumInput } from "./NumInput";
import { UnitInput } from "./UnitInput";
import { MaterialCharts, materialColor } from "./MaterialCharts";
import {
  convertFromCanonical,
  convertToCanonical,
  unitDef,
  unitLabel,
  type QuantityKind,
} from "../units";
import { isIsotropic, type Material, type MaterialProcess } from "../types";

const PROCESS_LABEL: Record<MaterialProcess, string> = {
  fdm: "FDM (printed)",
  isotropic: "Isotropic",
};

export function MaterialManagerModal() {
  const s = useStore(
    useShallow((s) => ({
      open: s.materialsManagerOpen,
      openMaterialsManager: s.openMaterialsManager,
      materials: s.materials,
      material: s.material,
      setMaterial: s.setMaterial,
      updateMaterial: s.updateMaterial,
      addMaterial: s.addMaterial,
      removeMaterial: s.removeMaterial,
      duplicateMaterial: s.duplicateMaterial,
      resetMaterials: s.resetMaterials,
      unitRev: s.unitRev,
    }))
  );
  const [selIdx, setSelIdx] = useState(0);
  // Reset-to-defaults wipes every custom material — guarded by an explicit
  // type-DELETE confirmation, not a single misclick.
  const [confirmReset, setConfirmReset] = useState(false);
  const [confirmText, setConfirmText] = useState("");
  // Opening lands on the material in use (the list keeps the user's last
  // selection only while the modal stays open).
  const wasOpen = useRef(false);
  useEffect(() => {
    if (s.open && !wasOpen.current) {
      const i = s.materials.findIndex((m) => m.name === s.material.name);
      if (i >= 0) setSelIdx(i);
    }
    wasOpen.current = s.open;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [s.open]);
  if (!s.open) return null;

  const idx = Math.min(Math.max(0, selIdx), s.materials.length - 1);
  const m = s.materials[idx];
  const iso = isIsotropic(m);
  const inUse = m.name === s.material.name;
  const update = (patch: Partial<Material>) => s.updateMaterial(idx, { ...m, ...patch });

  const setProcess = (p: MaterialProcess) => {
    if ((m.process ?? "fdm") === p) return;
    if (p === "isotropic") {
      // FDM → isotropic: build-sim yield may be 0 (disabled); the isotropic
      // SF runs against yield, so seed it from σₜ.
      update({
        process: "isotropic",
        yieldStrength: m.yieldStrength > 0 ? m.yieldStrength : Math.max(1, Math.round(0.9 * m.strength)),
      });
    } else {
      // Isotropic → FDM: the normalized internals (σₜ = σₜᶻ = σy, τᶻ = σy/√3)
      // are DERIVED values — reseed printed-part defaults instead of letting
      // them masquerade as measured FDM data.
      update({
        process: "fdm",
        strength: Math.max(1, m.strength),
        strengthZ: Math.max(1, Math.round(0.7 * m.strength)),
        shearStrengthZ: undefined,
        shrink: 0.004,
        shrinkZ: 0.002,
      });
    }
  };

  return (
    <div className="modalback" onClick={() => s.openMaterialsManager(false)}>
      <div className="modal wide propsmodal" onClick={(e) => e.stopPropagation()}>
        <div className="modalhead">
          <h2 title="One library for every material — FDM (printed, with layer adhesion and build-sim data) and isotropic (machined, cast, resin prints) together; the process selector switches what a material is. The material in use is chosen on the Properties step.">
            Materials
          </h2>
          <button className="x" onClick={() => s.openMaterialsManager(false)}>
            ×
          </button>
        </div>

        <div className="propsbody">
          <div className="propslist">
            {s.materials.map((mat, i) => (
              <button
                key={i}
                className={`propsrow${i === idx ? " sel" : ""}`}
                onClick={() => setSelIdx(i)}
              >
                <span className="name">
                  <span className="chipdot" style={{ background: materialColor(i) }} /> {mat.name}
                </span>
                <span className="dim small">
                  {isIsotropic(mat) ? "isotropic" : "FDM"}
                  {mat.name === s.material.name ? " · in use" : ""}
                </span>
              </button>
            ))}
            <div className="toolrow">
              <button
                onClick={() => {
                  s.addMaterial();
                  setSelIdx(s.materials.length);
                }}
              >
                + Add
              </button>
              <button
                onClick={() => {
                  setConfirmText("");
                  setConfirmReset(true);
                }}
                title="Restore the built-in material list (removes custom materials — asks for confirmation)"
              >
                Reset defaults
              </button>
            </div>
          </div>

          <div className="propsdetail">
            <input
              type="text"
              className="propsname"
              value={m.name}
              onChange={(e) => update({ name: e.target.value })}
            />

            <div className="procswitch">
              <span className="dim small">Process</span>
              {(Object.keys(PROCESS_LABEL) as MaterialProcess[]).map((p) => (
                <button
                  key={p}
                  className={`chip${(m.process ?? "fdm") === p ? " sel" : ""}`}
                  title={
                    p === "fdm"
                      ? "Layered extrusion print: walls, infill, layer adhesion and build direction all apply"
                      : "Bulk material with no build direction (machined, cast, SLA): analyzed fully dense, safety factor against yield"
                  }
                  onClick={() => setProcess(p)}
                >
                  {PROCESS_LABEL[p]}
                </button>
              ))}
              {inUse && <span className="dim small">· in use — edits update results</span>}
            </div>

            <div className="sectiontitle">Elastic & physical</div>
            <div className="matfields">
              <Field label={`E (${unitLabel("modulus")})`} title="Young's modulus">
                <UnitInput
                  value={m.e0}
                  kind="modulus"
                  min={10}
                  step={iso ? 1000 : 50}
                  onCommit={(v) => update({ e0: Math.max(10, v) })}
                />
              </Field>
              <Field label="ν" title="Poisson ratio">
                <NumInput
                  value={m.nu}
                  min={0}
                  max={0.49}
                  step={0.01}
                  onCommit={(v) => update({ nu: Math.min(0.49, Math.max(0, v)) })}
                />
              </Field>
              <Field label={`ρ (${unitLabel("density")})`} title="Mass density">
                <UnitInput
                  value={m.density}
                  kind="density"
                  min={0.1}
                  step={0.01}
                  onCommit={(v) => update({ density: Math.max(0.1, v) })}
                />
              </Field>
            </div>

            {!iso && (
              <>
                <div className="sectiontitle">Strength — printed anisotropy</div>
                <div className="matfields">
                  <Field
                    label={`σₜ in-layer (${unitLabel("stress")})`}
                    title="Tensile strength in the layer plane (printed, conservative datasheet value) — drives the safety-factor plot"
                  >
                    <UnitInput
                      value={m.strength}
                      kind="stress"
                      min={1}
                      step={1}
                      onCommit={(v) => update({ strength: Math.max(1, v) })}
                    />
                  </Field>
                  <Field
                    label={`σₜᶻ layer adhesion (${unitLabel("stress")})`}
                    title="Tension across the layers (σzz) — typically 50–80% of σₜ; drives the worst-case safety factor"
                  >
                    <UnitInput
                      value={m.strengthZ}
                      kind="stress"
                      min={1}
                      step={1}
                      onCommit={(v) => update({ strengthZ: Math.max(1, v) })}
                    />
                  </Field>
                  <Field
                    label={`τᶻ interlayer shear (${unitLabel("stress")})`}
                    title="Sliding along the layer plane — the second axis of the layer-adhesion failure criterion. Blank = 0.6·σₜᶻ until a measured value is entered"
                  >
                    <OptUnitInput
                      value={m.shearStrengthZ}
                      kind="stress"
                      min={1}
                      step={1}
                      placeholder={autoShearLabel(m.strengthZ)}
                      onCommit={(v) =>
                        update({ shearStrengthZ: v == null ? undefined : Math.max(1, v) })
                      }
                    />
                  </Field>
                </div>
              </>
            )}

            <div className="sectiontitle">Stress–strain</div>
            <div className="matfields">
              <Field
                label={`σy yield (${unitLabel("stress")})`}
                title={
                  iso
                    ? "Yield strength — the stress at which the part counts as failed. Drives the von Mises safety factor"
                    : "Yield stress — enables the build-sim's elastic–plastic step (0 = pure-elastic) and anchors the stress–strain curve"
                }
              >
                {iso ? (
                  <UnitInput
                    value={m.yieldStrength}
                    kind="stress"
                    min={1}
                    step={1}
                    onCommit={(v) => update({ yieldStrength: Math.max(1, v) })}
                  />
                ) : (
                  <UnitInput
                    value={m.yieldStrength ?? 0}
                    kind="stress"
                    min={0}
                    step={1}
                    onCommit={(v) => update({ yieldStrength: Math.max(0, v) })}
                  />
                )}
              </Field>
              <Field
                label={`σᵤ ultimate (${unitLabel("stress")})`}
                title="Ultimate tensile strength — informational (shown in the readout; the chart line is the pure bilinear E → σy → Eₜ model). Dormant for solves"
              >
                <OptUnitInput
                  value={m.ultimateStrength}
                  kind="stress"
                  min={1}
                  step={1}
                  onCommit={(v) => update({ ultimateStrength: v == null ? undefined : Math.max(1, v) })}
                />
              </Field>
              <Field
                label={`Eₜ tangent (${unitLabel("modulus")})`}
                title="Bilinear-hardening tangent modulus (post-yield slope) — shapes the chart; dormant for solves"
              >
                <OptUnitInput
                  value={m.tangentModulus}
                  kind="modulus"
                  min={1}
                  step={10}
                  onCommit={(v) => update({ tangentModulus: v == null ? undefined : Math.max(1, v) })}
                />
              </Field>
              <Field
                label="εᵣ rupture (%)"
                title="Engineering strain at rupture — the × on the chart; dormant for solves"
              >
                <OptNumInput
                  value={m.strainAtRupture == null ? undefined : +(m.strainAtRupture * 100).toFixed(2)}
                  min={0}
                  step={1}
                  onCommit={(v) =>
                    update({ strainAtRupture: v == null ? undefined : Math.max(0, v) / 100 })
                  }
                />
              </Field>
            </div>

            {!iso ? (
              <>
                <div className="sectiontitle">Build sim & thermal</div>
                <div className="matfields">
                  <Field
                    label="Shrink % (XY)"
                    title="In-plane process shrink — the dominant warp driver. Used only when Tg/CTE are blank"
                  >
                    <NumInput
                      value={m.shrink * 100}
                      min={0}
                      step={0.1}
                      onCommit={(v) => update({ shrink: Math.max(0, v) / 100 })}
                    />
                  </Field>
                  <Field
                    label="Shrink % (Z)"
                    title="Through-layer process shrink — usually less than in-plane. Used only when Tg/CTE are blank"
                  >
                    <NumInput
                      value={(m.shrinkZ ?? m.shrink) * 100}
                      min={0}
                      step={0.1}
                      onCommit={(v) => update({ shrinkZ: Math.max(0, v) / 100 })}
                    />
                  </Field>
                  <Field
                    label="Tg (°C)"
                    title="Locking temperature: Tg (amorphous) / ~Tc (semi-crystalline). With a CTE, the build-sim shrink derives from physics; blank = raw shrink"
                  >
                    <OptNumInput
                      value={m.tLock}
                      min={0}
                      step={5}
                      onCommit={(v) => update({ tLock: v == null ? undefined : Math.max(0, v) })}
                    />
                  </Field>
                  <Field label="CTE (ppm/°C)" title="Effective printed-part CTE, in-plane (XY); blank = raw shrink">
                    <OptNumInput
                      value={ppm(m.cte)}
                      min={0}
                      step={1}
                      onCommit={(v) => update({ cte: v == null ? undefined : Math.max(0, v) / 1e6 })}
                    />
                  </Field>
                  <Field label="CTE Z (ppm/°C)" title="Through-layer CTE; blank = isotropic (= XY)">
                    <OptNumInput
                      value={ppm(m.cteZ)}
                      min={0}
                      step={1}
                      onCommit={(v) => update({ cteZ: v == null ? undefined : Math.max(0, v) / 1e6 })}
                    />
                  </Field>
                </div>
              </>
            ) : (
              <>
                <div className="sectiontitle">Thermal</div>
                <div className="matfields">
                  <Field label="CTE (ppm/°C)" title="Coefficient of thermal expansion">
                    <OptNumInput
                      value={ppm(m.cte)}
                      min={0}
                      step={1}
                      onCommit={(v) => update({ cte: v == null ? undefined : Math.max(0, v) / 1e6 })}
                    />
                  </Field>
                </div>
              </>
            )}

            <div className="toolrow">
              <button
                disabled={inUse}
                title={inUse ? "Already the material in use" : "Analyze with this material"}
                onClick={() => s.setMaterial(m)}
              >
                Use this material
              </button>
              <button
                onClick={() => {
                  s.duplicateMaterial(idx);
                  setSelIdx(s.materials.length);
                }}
              >
                Duplicate
              </button>
              <button
                disabled={s.materials.length <= 1}
                title={s.materials.length <= 1 ? "The last material cannot be deleted" : undefined}
                onClick={() => {
                  s.removeMaterial(idx);
                  setSelIdx(Math.max(0, idx - 1));
                }}
              >
                Delete
              </button>
            </div>
          </div>
        </div>

        <MaterialCharts materials={s.materials} selIdx={idx} onSelect={setSelIdx} />

        {confirmReset && (
          <div className="modalback" onClick={() => setConfirmReset(false)}>
            <div
              className="modal confirmreset"
              style={{ width: 430 }}
              onClick={(e) => e.stopPropagation()}
            >
              <div className="modalhead">
                <h2>Reset materials?</h2>
                <button className="x" onClick={() => setConfirmReset(false)}>
                  ×
                </button>
              </div>
              <div className="dim small">
                This restores the built-in material list and permanently removes every custom
                material and every edit to the built-ins — there is no undo. Type{" "}
                <b>DELETE</b> to confirm.
              </div>
              <input
                type="text"
                placeholder="DELETE"
                value={confirmText}
                autoFocus
                onChange={(e) => setConfirmText(e.target.value)}
              />
              <div className="toolrow">
                <button
                  disabled={confirmText.trim() !== "DELETE"}
                  title={confirmText.trim() !== "DELETE" ? 'Type "DELETE" to enable' : undefined}
                  onClick={() => {
                    s.resetMaterials();
                    setSelIdx(0);
                    setConfirmReset(false);
                    setConfirmText("");
                  }}
                >
                  Reset to defaults
                </button>
                <button onClick={() => setConfirmReset(false)}>Cancel</button>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function Field(props: { label: string; title?: string; children: React.ReactNode }) {
  return (
    <label className="matfield" title={props.title}>
      <span className="flabel">{props.label}</span>
      {props.children}
    </label>
  );
}

/** 1/°C → ppm/°C for display, rounded so 96e-6 shows as 96, not 96.00000000000001. */
function ppm(v: number | undefined): number | undefined {
  return v == null ? undefined : +(v * 1e6).toFixed(3);
}

/** A canonical value formatted in the active display unit. */
function fmtUnit(v: number, kind: QuantityKind): string {
  const u = unitDef(kind);
  return convertFromCanonical(v, kind).toFixed(u.decimals);
}

/** Placeholder for a blank τᶻ field: the derived 0.6·σₜᶻ default, in the
 *  active display unit, marked "auto" so a blank is visibly not zero. */
function autoShearLabel(strengthZ: number): string {
  return `${fmtUnit(0.6 * strengthZ, "stress")} auto`;
}

/** OptNumInput with unit conversion on the boundary (see UnitInput): edits in
 *  the display unit, commits canonical, blank commits undefined. */
function OptUnitInput({
  value,
  kind,
  onCommit,
  min,
  max,
  step,
  ...rest
}: {
  value: number | undefined;
  kind: QuantityKind;
  onCommit: (canonical: number | undefined) => void;
  min?: number;
  max?: number;
  step?: number;
} & Omit<
  React.InputHTMLAttributes<HTMLInputElement>,
  "value" | "onChange" | "type" | "min" | "max" | "step"
>) {
  const u = unitDef(kind);
  const toDisp = (v: number | undefined) =>
    v == null ? undefined : convertFromCanonical(v, kind);
  const disp = value == null ? undefined : Number(convertFromCanonical(value, kind).toFixed(u.decimals));
  return (
    <OptNumInput
      value={disp}
      min={toDisp(min)}
      max={toDisp(max)}
      step={step != null ? convertFromCanonical(step, kind) : undefined}
      onCommit={(v) => onCommit(v == null ? undefined : convertToCanonical(v, kind))}
      {...rest}
    />
  );
}

/** NumInput variant for OPTIONAL fields: an empty box means "unset" and
 *  commits `undefined` (on blur, so mid-edit clearing doesn't unset). */
function OptNumInput({
  value,
  onCommit,
  ...rest
}: {
  value: number | undefined;
  onCommit: (v: number | undefined) => void;
} & Omit<React.InputHTMLAttributes<HTMLInputElement>, "value" | "onChange" | "type">) {
  const [text, setText] = useState<string>(value == null ? "" : String(value));
  const focused = useRef(false);
  useEffect(() => {
    if (!focused.current) setText(value == null ? "" : String(value));
  }, [value]);
  return (
    <input
      type="number"
      value={text}
      placeholder="—"
      onFocus={() => {
        focused.current = true;
      }}
      onChange={(e) => {
        setText(e.target.value);
        const n = Number(e.target.value);
        if (e.target.value !== "" && Number.isFinite(n)) onCommit(n);
      }}
      onBlur={(e) => {
        focused.current = false;
        const raw = e.target.value.trim();
        if (raw === "") onCommit(undefined);
        else {
          const n = Number(raw);
          if (Number.isFinite(n)) onCommit(n);
        }
        setText(value == null ? "" : String(value));
      }}
      {...rest}
    />
  );
}
