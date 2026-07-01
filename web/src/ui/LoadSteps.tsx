// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Load-steps manager (DESIGN §13) — a Settings-style modal holding the per-step
// editing GRID, transposed: rows = load steps, columns = conditions. Supports
// are an on/off checkbox; forces expand to editable X/Y/Z columns and pressures
// to an editable value, so you can change loads across all steps at a glance.
// The Loads panel keeps the compact selector + the detailed active-step editor.

import { useShallow } from "zustand/shallow";
import { useStore } from "../store";
import { NumInput } from "./NumInput";
import { UnitInput } from "./UnitInput";
import { bcLabel, KIND_DOT, BC_QUANTITY } from "./bcmeta";
import { unitLabel } from "../units";

export function LoadStepsModal() {
  const s = useStore(
    useShallow((s) => ({
      loadStepsOpen: s.loadStepsOpen,
      openLoadSteps: s.openLoadSteps,
      bcs: s.bcs,
      loadSteps: s.loadSteps,
      activeLoadStepId: s.activeLoadStepId,
      addLoadStep: s.addLoadStep,
      removeLoadStep: s.removeLoadStep,
      renameLoadStep: s.renameLoadStep,
      setActiveLoadStep: s.setActiveLoadStep,
      setStepBcActive: s.setStepBcActive,
      setStepForce: s.setStepForce,
      setStepPressure: s.setStepPressure,
      setStepMoment: s.setStepMoment,
      setStepIncludeOptimize: s.setStepIncludeOptimize,
      setStepWeight: s.setStepWeight,
      unitRev: s.unitRev,
    }))
  );
  if (!s.loadStepsOpen) return null;
  const single = s.loadSteps.length <= 1;
  return (
    <div className="modalback" onClick={() => s.openLoadSteps(false)}>
      <div className="modal wide" onClick={(e) => e.stopPropagation()}>
        <div className="modalhead">
          <h2>Load steps</h2>
          <button className="x" onClick={() => s.openLoadSteps(false)}>
            ×
          </button>
        </div>

        <div className="dim small">
          Each row is a load case the part is analyzed under. Supports toggle on/off per step;
          forces and pressures take their own value per step right here. Click a step's name to make
          it the active step (highlighted) — that's what the Loads panel edits and Solve runs. Define
          and name the conditions in the Loads panel.
        </div>

        {s.bcs.length === 0 ? (
          <div className="dim" style={{ padding: "14px 0" }}>
            Add supports &amp; loads in the Loads panel first — they'll appear here as columns.
          </div>
        ) : (
          <div className="lsgridwrap">
            <table className="lsgrid">
              <thead>
                <tr>
                  <th className="lscorner" rowSpan={2}>
                    Load step
                  </th>
                  {s.bcs.map((bc) => {
                    const span =
                      bc.kind === "force" || bc.kind === "bearing" || bc.kind === "moment"
                        ? 4
                        : bc.kind === "pressure"
                          ? 2
                          : 1;
                    return (
                      <th key={bc.id} className="lsgrouphead" colSpan={span}>
                        <span className="dot" style={{ background: KIND_DOT[bc.kind] }} />
                        {bcLabel(bc)}
                        {BC_QUANTITY[bc.kind] && (
                          <span className="lscolunit">{unitLabel(BC_QUANTITY[bc.kind]!)}</span>
                        )}
                      </th>
                    );
                  })}
                </tr>
                <tr>
                  {s.bcs.flatMap((bc) => {
                    if (bc.kind === "force" || bc.kind === "bearing" || bc.kind === "moment")
                      return [
                        <th key={`${bc.id}-on`} className="lssub">
                          on
                        </th>,
                        ...(["X", "Y", "Z"] as const).map((a) => (
                          <th key={`${bc.id}-${a}`} className="lssub">
                            {a}
                          </th>
                        )),
                      ];
                    if (bc.kind === "pressure")
                      return [
                        <th key={`${bc.id}-on`} className="lssub">
                          on
                        </th>,
                        <th key={`${bc.id}-p`} className="lssub">
                          p
                        </th>,
                      ];
                    return [
                      <th key={`${bc.id}-on`} className="lssub">
                        on
                      </th>,
                    ];
                  })}
                </tr>
              </thead>
              <tbody>
                {s.loadSteps.map((ls) => (
                  <tr key={ls.id} className={ls.id === s.activeLoadStepId ? "on" : ""}>
                    <td className="lssteprow">
                      <input
                        className="lsname"
                        value={ls.name}
                        onFocus={() => s.setActiveLoadStep(ls.id)}
                        onChange={(e) => s.renameLoadStep(ls.id, e.target.value)}
                      />
                      <button
                        className="x"
                        disabled={single}
                        onClick={() => s.removeLoadStep(ls.id)}
                        title="Remove this load step"
                      >
                        ×
                      </button>
                    </td>
                    {s.bcs.flatMap((bc) => {
                      const on = ls.overrides[bc.id]?.active !== false;
                      const onCell = (
                        <td key={`${bc.id}-on`}>
                          <input
                            type="checkbox"
                            checked={on}
                            onChange={(e) => s.setStepBcActive(ls.id, bc.id, e.target.checked)}
                            title={`${bcLabel(bc)} — ${on ? "active" : "off"} in ${ls.name}`}
                          />
                        </td>
                      );
                      if (bc.kind === "force" || bc.kind === "bearing") {
                        const f = ls.overrides[bc.id]?.force ?? bc.force ?? [0, 0, 0];
                        return [
                          onCell,
                          ...[0, 1, 2].map((c) => (
                            <td key={`${bc.id}-${c}`}>
                              <UnitInput
                                className="gridnum"
                                value={f[c]}
                                kind="force"
                                step={1}
                                disabled={!on}
                                onCommit={(v) => {
                                  const nf = [...f] as [number, number, number];
                                  nf[c] = v;
                                  s.setStepForce(ls.id, bc.id, nf);
                                }}
                              />
                            </td>
                          )),
                        ];
                      }
                      if (bc.kind === "moment") {
                        const mm = ls.overrides[bc.id]?.moment ?? bc.moment ?? [0, 0, 0];
                        return [
                          onCell,
                          ...[0, 1, 2].map((c) => (
                            <td key={`${bc.id}-${c}`}>
                              <UnitInput
                                className="gridnum"
                                value={mm[c]}
                                kind="moment"
                                step={10}
                                disabled={!on}
                                onCommit={(v) => {
                                  const nv = [...mm] as [number, number, number];
                                  nv[c] = v;
                                  s.setStepMoment(ls.id, bc.id, nv);
                                }}
                              />
                            </td>
                          )),
                        ];
                      }
                      if (bc.kind === "pressure") {
                        const p = ls.overrides[bc.id]?.pressure ?? bc.pressure ?? 0;
                        return [
                          onCell,
                          <td key={`${bc.id}-p`}>
                            <UnitInput
                              className="gridnum"
                              value={p}
                              kind="pressure"
                              step={0.01}
                              disabled={!on}
                              onCommit={(v) => s.setStepPressure(ls.id, bc.id, v)}
                            />
                          </td>,
                        ];
                      }
                      return [onCell];
                    })}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}

        {!single && (
          <div className="lsopt">
            <div className="lsopthead">Optimization · weighted-sum compliance</div>
            <div className="dim small">
              Which load cases the optimized infill must resist, and how much each one matters. Solve
              always runs every step — this only shapes Optimize. Weights are normalized; an
              unchecked step is ignored.
            </div>
            <div className="lsoptgrid">
              {(() => {
                const wsum =
                  s.loadSteps.filter((x) => x.includeInOptimize).reduce((a, x) => a + x.weight, 0) ||
                  1;
                return s.loadSteps.map((ls) => (
                  <div key={ls.id} className={`lsoptrow${ls.includeInOptimize ? "" : " off"}`}>
                    <label className="lsoptinc">
                      <input
                        type="checkbox"
                        checked={ls.includeInOptimize}
                        onChange={(e) => s.setStepIncludeOptimize(ls.id, e.target.checked)}
                      />
                      <span>{ls.name}</span>
                    </label>
                    <NumInput
                      className="gridnum"
                      value={ls.weight}
                      step={0.1}
                      disabled={!ls.includeInOptimize}
                      onCommit={(v) => s.setStepWeight(ls.id, v)}
                    />
                    <span className="lsoptpct">
                      {ls.includeInOptimize ? `${Math.round((100 * ls.weight) / wsum)}%` : "—"}
                    </span>
                  </div>
                ));
              })()}
            </div>
          </div>
        )}

        <div className="modalfoot">
          <button onClick={() => s.addLoadStep()}>+ Add load step</button>
          <button className="primary" onClick={() => s.openLoadSteps(false)}>
            Done
          </button>
        </div>
      </div>
    </div>
  );
}
