// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Results dock: DRO-style readout windows. Optimization results take
// precedence; an as-printed verify solve gets its own readouts (mass at the
// print settings, deflection, min safety factor). Empty instrument = hidden.

import { useStore } from "../store";
import { fmtDispParts } from "./fmt";

export function Inspector() {
  const s = useStore();
  if (s.optSummary) return <OptResults />;
  if (s.printedStats && s.stats && s.hasResult) return <PrintedResults />;
  return null;
}

/** Dock after "Solve once · As printed": the part at today's print settings. */
function PrintedResults() {
  const s = useStore();
  const p = s.printedStats!;
  const stats = s.stats!;
  const [defl, deflUnit] = fmtDispParts(stats.maxDisplacement);
  return (
    <aside className="inspector" aria-label="Results">
      <div className="i-head">
        <span>Results</span>
        <span>as printed</span>
      </div>

      {!stats.converged && (
        <div className="warnbanner">
          ⚠ <b>Solve did not converge.</b> It stopped at the {stats.iterations}-iteration cap with
          relative residual {stats.relResidual.toExponential(1)} (target{" "}
          {(stats.tol ?? 1e-5).toExponential(0)}). The deflection, stress and safety-factor numbers
          below are an <b>unconverged approximation</b> — treat them as indicative only. A coarser
          mesh (Preview / Normal) converges reliably and is usually just as accurate for homogenized
          infill.
        </div>
      )}

      <div className="dro">
        <div className="dro-label">
          <span>Mass</span>
          <span>
            of {p.massSolidGrams.toFixed(1)} g solid ·{" "}
            {Math.round((100 * p.massGrams) / Math.max(p.massSolidGrams, 1e-9))} %
          </span>
        </div>
        <div className="dro-window">
          <b>{p.massGrams.toFixed(1)}</b>
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

      <div className="dro hero">
        <div className="dro-label">
          <span>Min safety factor</span>
          <span>
            {p.sfGoverns === "layer"
              ? `layer adhesion governs · σₜᶻ ${s.material.strengthZ} MPa`
              : p.sfGoverns === "material"
                ? `material governs · σₜ ${s.material.strength} MPa`
                : `σₜ ${s.material.strength} / σₜᶻ ${s.material.strengthZ} MPa`}
          </span>
        </div>
        <div className="dro-window">
          <b>{p.minSf !== null ? p.minSf.toFixed(2) : "—"}</b>
          <span>×</span>
        </div>
      </div>

      <div className="divider" />
      <div className="kv">
        <span>Print settings</span>
        <b>
          {p.perimeters} × {p.lineWidth} mm · {p.infillPct}% {p.pattern}
        </b>
      </div>
      <div className="kv">
        <span>Skin resolution</span>
        <b>
          {p.compositeSkin
            ? `${p.skinLayers.toFixed(2)} layers · composite`
            : `${p.skinLayers} cell layer${p.skinLayers === 1 ? "" : "s"}`}
        </b>
      </div>
      <div className="kv">
        <span>{stats.converged ? "Solved" : "Stopped at cap"}</span>
        <b>
          {stats.iterations} it · {stats.seconds.toFixed(1)} s
        </b>
      </div>
      <div className="kv">
        <span>Advisory</span>
        <b>homogenized infill · static linear</b>
      </div>
      {!p.compositeSkin && p.skinLayers === 1 && (
        <div className="warnrow">
          The wall is one voxel layer at this resolution — coarse. Raise the resolution in
          Properties (or enable composite skin) for a trustworthy printed-mode result.
        </div>
      )}
    </aside>
  );
}

function OptResults() {
  const s = useStore();
  const o = s.optSummary!;
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

      {/* o.converged is the optimizer's DESIGN-stationarity signal (mean |Δρ|
          settled before the iteration cap). It does NOT yet reflect the
          binned VERIFICATION solve's MGCG convergence — that is hardcoded
          converged:true in the wasm layer (see crates/sig-wasm/src/lib.rs,
          the Solution built after the optimize loop). Surfacing the real
          verification-solve residual here is the deferred follow-up. */}
      {!o.converged && (
        <div className="warnbanner">
          ⚠ <b>Optimization did not converge.</b> The design was still changing when it hit the{" "}
          {o.iterations}-iteration cap, so the layout and the stiffness / mass figures below are{" "}
          <b>preliminary</b>. Re-run — a coarser analysis resolution converges more reliably — before
          trusting these numbers.
        </div>
      )}

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
