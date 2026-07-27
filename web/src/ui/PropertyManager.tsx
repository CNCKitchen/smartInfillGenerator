// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Infill Property Manager (DESIGN §24) — the library surface for infill
// property sets: inspect, duplicate-to-edit, delete, import/export
// `.filaprops`. Entered from ⚙ Settings → Infill properties. Graphs land in
// §24 M2; this is the M1 list + detail surface.

import { useRef, useState } from "react";
import { useShallow } from "zustand/shallow";
import { useStore } from "../store";
import { NumInput } from "./NumInput";
import { PropertyCharts } from "./PropertyCharts";
import {
  CURVE_BOUNDS,
  exportFilaprops,
  FILAPROPS_EXT,
  gxyEp,
  nuZp,
  RATIO_BOUNDS,
  relStiffness,
  type InfillPropertySet,
  type SetProvenance,
  type TiRatiosData,
} from "../infillProps";

function downloadText(fileName: string, text: string) {
  const blob = new Blob([text], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = fileName;
  a.click();
  URL.revokeObjectURL(url);
}

function safeFileName(name: string): string {
  return name.replace(/[^\w\- ]+/g, "").trim().replace(/\s+/g, "-").toLowerCase() || "infill-set";
}

const ORIGIN_LABEL: Record<InfillPropertySet["origin"], string> = {
  builtin: "Built-in",
  user: "User",
  imported: "Imported",
};

const MODEL_LABEL = {
  isotropic: "Isotropic",
  transverse_isotropic: "Transverse isotropic",
} as const;

export function PropertyManagerModal() {
  const s = useStore(
    useShallow((s) => ({
      open: s.propsManagerOpen,
      openPropsManager: s.openPropsManager,
      propertySets: s.propertySets,
      activeSetId: s.activeSetId,
      setActivePropertySet: s.setActivePropertySet,
      duplicatePropertySet: s.duplicatePropertySet,
      updatePropertySet: s.updatePropertySet,
      deletePropertySet: s.deletePropertySet,
      importPropertySets: s.importPropertySets,
    }))
  );
  const [selId, setSelId] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const fileRef = useRef<HTMLInputElement>(null);
  if (!s.open) return null;

  const sel =
    s.propertySets.find((x) => x.id === (selId ?? s.activeSetId)) ?? s.propertySets[0];
  const editable = sel.origin === "user";
  const isActive = sel.id === s.activeSetId;
  const update = (patch: Parameters<typeof s.updatePropertySet>[1]) =>
    s.updatePropertySet(sel.id, patch);
  const setRatio = (k: keyof TiRatiosData, v: number) => {
    const [lo, hi] = RATIO_BOUNDS[k];
    update({ ratios: { ...sel.ratios, [k]: Math.min(hi, Math.max(lo, v)) } });
  };
  const setProv = (k: keyof SetProvenance, v: string) =>
    update({ provenance: { ...sel.provenance, [k]: v || undefined } });

  const onImportFile = async (f: File | undefined) => {
    if (!f) return;
    setNote(s.importPropertySets(await f.text()));
  };

  const pct = (v: number) => `${(100 * v).toFixed(1)}%`;

  return (
    <div className="modalback" onClick={() => s.openPropsManager(false)}>
      <div className="modal wide propsmodal" onClick={(e) => e.stopPropagation()}>
        <div className="modalhead">
          <h2>Infill properties</h2>
          <button className="x" onClick={() => s.openPropsManager(false)}>
            ×
          </button>
        </div>
        <div className="dim small">
          One set = one pattern calibration: the E(ρ) = c · E₀ · ρⁿ magnitude law plus the
          anisotropy ratios (normalized to the in-plane modulus Ep). Built-in and imported sets
          are read-only — duplicate one to calibrate your own. The active set drives the solves.
        </div>

        <div className="propsbody">
          <div className="propslist">
            {s.propertySets.map((p) => (
              <button
                key={p.id}
                className={`propsrow${p.id === sel.id ? " sel" : ""}`}
                onClick={() => setSelId(p.id)}
              >
                <span className="name">{p.name}</span>
                <span className="dim small">
                  {p.pattern} · {ORIGIN_LABEL[p.origin]}
                  {p.id === s.activeSetId ? " · ACTIVE" : ""}
                </span>
              </button>
            ))}
            <div className="toolrow">
              <button onClick={() => fileRef.current?.click()}>Import…</button>
              <button
                onClick={() =>
                  downloadText(`infill-properties${FILAPROPS_EXT}`, exportFilaprops(s.propertySets))
                }
              >
                Export all
              </button>
            </div>
            <input
              ref={fileRef}
              type="file"
              accept={`${FILAPROPS_EXT},.json,application/json`}
              style={{ display: "none" }}
              onChange={(e) => {
                void onImportFile(e.target.files?.[0]);
                e.target.value = "";
              }}
            />
          </div>

          <div className="propsdetail">
            {editable ? (
              <input
                type="text"
                className="propsname"
                value={sel.name}
                onChange={(e) => update({ name: e.target.value })}
              />
            ) : (
              <h3>{sel.name}</h3>
            )}

            <table className="settingstable">
              <tbody>
                <tr>
                  <td className="dim">Pattern</td>
                  <td>
                    {editable ? (
                      <input
                        type="text"
                        value={sel.pattern}
                        onChange={(e) => update({ pattern: e.target.value })}
                      />
                    ) : (
                      sel.pattern
                    )}
                  </td>
                  <td className="dim">Model</td>
                  <td>{MODEL_LABEL[sel.modelClass]}</td>
                </tr>
                <tr>
                  <td className="dim">Source</td>
                  <td>{ORIGIN_LABEL[sel.origin]}</td>
                  <td className="dim">Projects</td>
                  <td>{sel.embedInProject ? "embeds values" : "reference-only (licensed)"}</td>
                </tr>
                <tr>
                  <td className="dim" title="Density band the fit is calibrated over — outside it the law is extrapolated (never clamped)">
                    Calibrated band
                  </td>
                  <td colSpan={3}>
                    {editable ? (
                      <span className="row">
                        <NumInput
                          value={Math.round(100 * sel.calibratedBand[0])}
                          min={1}
                          max={99}
                          step={5}
                          onCommit={(v) =>
                            update({
                              calibratedBand: [
                                Math.min(sel.calibratedBand[1] - 0.01, Math.max(0.01, v / 100)),
                                sel.calibratedBand[1],
                              ],
                            })
                          }
                        />
                        –
                        <NumInput
                          value={Math.round(100 * sel.calibratedBand[1])}
                          min={2}
                          max={100}
                          step={5}
                          onCommit={(v) =>
                            update({
                              calibratedBand: [
                                sel.calibratedBand[0],
                                Math.max(sel.calibratedBand[0] + 0.01, Math.min(1, v / 100)),
                              ],
                            })
                          }
                        />
                        %
                      </span>
                    ) : (
                      `${Math.round(100 * sel.calibratedBand[0])}–${Math.round(100 * sel.calibratedBand[1])} %`
                    )}
                  </td>
                </tr>
              </tbody>
            </table>

            <h3>Magnitude law E(ρ) = c · E₀ · ρⁿ</h3>
            <table className="settingstable">
              <thead>
                <tr>
                  <th>c</th>
                  <th>n</th>
                  <th className="dim">Ep(20%)</th>
                  <th className="dim">Ep(50%)</th>
                </tr>
              </thead>
              <tbody>
                <tr>
                  <td>
                    {editable ? (
                      <NumInput
                        value={sel.curve.coeff}
                        min={CURVE_BOUNDS.coeff[0]}
                        max={CURVE_BOUNDS.coeff[1]}
                        step={0.05}
                        onCommit={(v) => update({ curve: { ...sel.curve, coeff: v } })}
                      />
                    ) : (
                      sel.curve.coeff.toFixed(4)
                    )}
                  </td>
                  <td>
                    {editable ? (
                      <NumInput
                        value={sel.curve.exponent}
                        min={CURVE_BOUNDS.exponent[0]}
                        max={CURVE_BOUNDS.exponent[1]}
                        step={0.05}
                        onCommit={(v) => update({ curve: { ...sel.curve, exponent: v } })}
                      />
                    ) : (
                      sel.curve.exponent.toFixed(4)
                    )}
                  </td>
                  <td className="dim">{pct(relStiffness(sel, 0.2, "ep"))}</td>
                  <td className="dim">{pct(relStiffness(sel, 0.5, "ep"))}</td>
                </tr>
              </tbody>
            </table>

            <h3>Anisotropy ratios (Ep = 1)</h3>
            <table className="settingstable">
              <thead>
                <tr>
                  <th title="Build-axis modulus over in-plane modulus">Ez/Ep</th>
                  <th title="Through-layer shear over in-plane modulus">Gz/Ep</th>
                  <th title="In-plane Poisson ratio">νp</th>
                  <th title="In-plane stress → build-axis contraction (major ratio)">νpz</th>
                  <th className="dim" title="Derived: TI fixes Gxy = Ep/(2(1+νp)) — not a free constant">
                    Gxy/Ep
                  </th>
                  <th className="dim" title="Derived by Maxwell reciprocity: νzp = νpz·Ez/Ep">νzp</th>
                </tr>
              </thead>
              <tbody>
                <tr>
                  {(["ezEp", "gzEp", "nuP", "nuPz"] as const).map((k) => (
                    <td key={k}>
                      {editable ? (
                        <NumInput
                          value={sel.ratios[k]}
                          min={RATIO_BOUNDS[k][0]}
                          max={RATIO_BOUNDS[k][1]}
                          step={0.01}
                          onCommit={(v) => setRatio(k, v)}
                        />
                      ) : (
                        sel.ratios[k].toFixed(4)
                      )}
                    </td>
                  ))}
                  <td className="dim">{gxyEp(sel.ratios).toFixed(4)}</td>
                  <td className="dim">{nuZp(sel.ratios).toFixed(4)}</td>
                </tr>
              </tbody>
            </table>
            <div className="hint">
              Stiffness relative to solid at 20% / 50% infill — in-plane Ep{" "}
              {pct(relStiffness(sel, 0.2, "ep"))} / {pct(relStiffness(sel, 0.5, "ep"))}, build-axis
              Ez {pct(relStiffness(sel, 0.2, "ez"))} / {pct(relStiffness(sel, 0.5, "ez"))},
              through-layer shear Gz {pct(relStiffness(sel, 0.2, "gz"))} /{" "}
              {pct(relStiffness(sel, 0.5, "gz"))}.
            </div>

            <h3>Provenance</h3>
            <table className="settingstable">
              <tbody>
                {(
                  [
                    ["author", "Author"],
                    ["calibratedOn", "Calibrated on"],
                    ["date", "Date"],
                    ["license", "License"],
                  ] as const
                ).map(([k, label]) => (
                  <tr key={k}>
                    <td className="dim">{label}</td>
                    <td>
                      {editable ? (
                        <input
                          type="text"
                          value={sel.provenance[k] ?? ""}
                          onChange={(e) => setProv(k, e.target.value)}
                        />
                      ) : (
                        (sel.provenance[k] ?? "—")
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>

            <div className="toolrow">
              <button
                disabled={isActive}
                title={isActive ? "Already the active set" : "Use this set for solves — existing results go stale"}
                onClick={() => s.setActivePropertySet(sel.id)}
              >
                Set active
              </button>
              <button onClick={() => s.duplicatePropertySet(sel.id)}>Duplicate</button>
              <button
                onClick={() =>
                  downloadText(`${safeFileName(sel.name)}${FILAPROPS_EXT}`, exportFilaprops([sel]))
                }
              >
                Export
              </button>
              <button
                disabled={sel.origin === "builtin" || isActive}
                title={
                  sel.origin === "builtin"
                    ? "The built-in set cannot be deleted"
                    : isActive
                      ? "Switch the active set first"
                      : undefined
                }
                onClick={() => {
                  s.deletePropertySet(sel.id);
                  setSelId(null);
                }}
              >
                Delete
              </button>
            </div>
          </div>
        </div>

        <PropertyCharts sets={s.propertySets} selectedId={sel.id} onSelect={setSelId} />

        {note && <div className="hint">{note}</div>}
      </div>
    </div>
  );
}
