// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Results dock: DRO-style readout windows for the optimization outcome.
// Rendered only when a result exists — an empty instrument shows nothing.

import { useStore } from "../store";
import { fmtDispParts } from "./fmt";

export function Inspector() {
  const s = useStore();
  const o = s.optSummary;
  if (!o) return null;

  const stiff = Math.round(o.stiffnessVsSolid * 100);
  const gain = (o.gainVsUniform * 100).toFixed(1);
  const uniformPct = Math.round(o.meanInfill * 100);
  const isMatch = o.goal === "match" && o.massUniformRefGrams != null;
  const saved = isMatch ? 1 - o.massGrams / o.massUniformRefGrams! : 0;
  const [defl, deflUnit] = fmtDispParts(o.maxDisplacement);

  return (
    <aside className="inspector" aria-label="Results">
      <div className="i-head">
        <span>Results</span>
        <span>{isMatch ? "match goal" : "budget goal"}</span>
      </div>

      {isMatch ? (
        <div className="dro hero">
          <div className="dro-label">
            <span>vs {Math.round(o.refUniformPct!)} % uniform, same stiffness</span>
            <span>
              {o.massUniformRefGrams!.toFixed(1)} → {o.massGrams.toFixed(1)} g
            </span>
          </div>
          <div className="dro-window">
            <b>−{(saved * 100).toFixed(0)}</b>
            <span>% WEIGHT</span>
          </div>
        </div>
      ) : (
        <div className="dro hero">
          <div className="dro-label">
            <span>vs {uniformPct} % uniform, same weight</span>
          </div>
          <div className="dro-window">
            <b>+{gain}</b>
            <span>% STIFFER</span>
          </div>
        </div>
      )}

      <div className="dro">
        <div className="dro-label">
          <span>Mass</span>
          <span>
            of {o.massSolidGrams.toFixed(1)} g solid · {Math.round(o.massFrac * 100)} %
          </span>
        </div>
        <div className="dro-window">
          <b>{o.massGrams.toFixed(1)}</b>
          <span>g</span>
        </div>
      </div>

      <div className="dro">
        <div className="dro-label">
          <span>Max deflection</span>
        </div>
        <div className="dro-window">
          <b>{defl}</b>
          <span>{deflUnit}</span>
        </div>
      </div>

      <div className="divider" />
      {isMatch && (
        <div className="kv">
          <span>vs {uniformPct} % uniform, same weight</span>
          <b>+{gain} %</b>
        </div>
      )}
      <div className="kv">
        <span>Stiffness vs 100 % solid</span>
        <b>{stiff} %</b>
      </div>
      <div className="kv">
        <span>Infill levels</span>
        <b>{o.bins.map((b) => `${Math.round(b.density * 100)}`).join(" · ")} %</b>
      </div>
      <div className="kv">
        <span>{o.converged ? "Converged" : "Stopped at cap"}</span>
        <b>
          {o.iterations} it{o.passes > 1 ? ` · ${o.passes} passes` : ""} · {o.seconds.toFixed(1)} s
        </b>
      </div>
      {Math.abs(o.targetInfill * 100 - s.budget) > 0.5 && (
        <div className="kv">
          <span>Target clamped (printable band)</span>
          <b>{Math.round(o.targetInfill * 100)} %</b>
        </div>
      )}

      {isMatch && Math.abs(o.matchDeviation ?? 0) > 0.02 && (
        <div className="warnrow">
          Stiffness {((o.matchDeviation ?? 0) * 100).toFixed(1)} % off the target (search hit its
          pass limit) — re-run or adjust the reference.
        </div>
      )}
      {o.regionCount === 0 && (
        <div className="warnrow">
          No separate regions: the whole interior ended at one density level. Raise the infill
          budget (or the number of levels) to get differentiated zones.
        </div>
      )}
    </aside>
  );
}
