// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Results dock: DRO-style readout windows. Optimization results take
// precedence; an as-printed verify solve gets its own readouts (mass at the
// print settings, deflection, min safety factor). Empty instrument = hidden.

import { useShallow } from "zustand/shallow";
import { useStore } from "../store";
import { fmtDispParts } from "./fmt";
import { format, formatParts } from "../units";

export function Inspector() {
  const s = useStore(
    useShallow((s) => ({
      optSummary: s.optSummary,
      printedStats: s.printedStats,
      stats: s.stats,
      hasResult: s.hasResult,
    }))
  );
  if (s.optSummary) return <OptResults />;
  if (s.printedStats && s.stats && s.hasResult) return <PrintedResults />;
  return null;
}

/** Dock after "Solve once · As printed": the part at today's print settings. */
function PrintedResults() {
  const s = useStore(
    useShallow((s) => ({
      printedStats: s.printedStats,
      stats: s.stats,
      material: s.material,
      unitRev: s.unitRev,
    }))
  );
  const p = s.printedStats!;
  const stats = s.stats!;
  const [defl, deflUnit] = fmtDispParts(stats.maxDisplacement);
  const [mass, massUnit] = formatParts(p.massGrams, "mass");
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
            of {format(p.massSolidGrams, "mass")} solid ·{" "}
            {Math.round((100 * p.massGrams) / Math.max(p.massSolidGrams, 1e-9))} %
          </span>
        </div>
        <div className="dro-window">
          <b>{mass}</b>
          <span>{massUnit}</span>
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
              ? `layer adhesion governs · σₜᶻ ${format(s.material.strengthZ, "stress")} · τᶻ ${format(
                  s.material.shearStrengthZ ?? 0.6 * s.material.strengthZ,
                  "stress"
                )}`
              : p.sfGoverns === "material"
                ? `material governs · σₜ ${format(s.material.strength, "stress")}`
                : `σₜ ${format(s.material.strength, "stress")} / σₜᶻ ${format(s.material.strengthZ, "stress")}`}
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
          {p.perimeters} × {format(p.lineWidth, "length")} · {p.infillPct}% {p.pattern}
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
  const s = useStore(
    useShallow((s) => ({
      optSummary: s.optSummary,
      budget: s.budget,
      unitRev: s.unitRev,
      setResultField: s.setResultField,
      setLegendRange: s.setLegendRange,
      setBandCount: s.setBandCount,
    }))
  );
  const o = s.optSummary!;
  const solid = o.solid;
  const stiff = Math.round(o.stiffnessVsSolid * 100);
  const gain = (o.gainVsUniform * 100).toFixed(1);
  const uniformPct = Math.round(o.meanInfill * 100);
  // Solid topology: the comparison is against the SAME material spread evenly
  // (not a uniform infill %); the win is the optimized layout at equal mass.
  const uniformLabel = solid
    ? "vs material spread evenly, same weight"
    : `vs ${uniformPct} % uniform, same weight`;
  const isMatch = o.goal === "match" && o.massUniformRefGrams != null;
  const isStrength = o.goal === "strength";
  const saved = isMatch ? 1 - o.massGrams / o.massUniformRefGrams! : 0;
  const [defl, deflUnit] = fmtDispParts(o.maxDisplacement);
  // One-click binding view (DESIGN §17 dec. 6): the SF field of the run's
  // measure, banded in two colors with the boundary AT the target — every
  // red cell is below the required safety factor.
  const showSfView = () => {
    const field = o.sfMeasure === "material" ? "sfm" : o.sfMeasure === "layer" ? "sfz" : "sf";
    void s.setResultField(field);
    s.setLegendRange(0, 2 * (o.sfTarget ?? 2));
    s.setBandCount(2);
  };

  return (
    <aside className="inspector" aria-label="Results">
      <div className="i-head">
        <span>Results</span>
        <span>{isMatch ? "match goal" : isStrength ? "strength goal" : "budget goal"}</span>
      </div>

      {/* §17 dec. 6: infeasibility is routine product behaviour, not an error —
          deliver the all-at-cap design with the honest ceiling + a diagnosis of
          WHERE it binds (skin ⇒ infill can't fix it; interior ⇒ raise the cap). */}
      {isStrength && o.sfFeasible === false && (
        <div className="warnbanner">
          ⚠ <b>Target SF {o.sfTarget} is not reachable.</b> Even with the whole interior at the
          density cap the safety factor reaches <b>{(o.sfBest ?? 0).toFixed(2)}</b> — that
          all-at-cap design is what you see below.{" "}
          {(o.bindingSkinShare ?? 0) > 0.5
            ? "The critical region sits in the solid skin, so more infill cannot fix it: reorient the part, add perimeters/shells, or pick a stronger material."
            : "The critical region is interior material already at the cap — raising the density cap (Levels settings) may get you further."}{" "}
          <button className="linkbtn" onClick={showSfView}>
            Show the critical region
          </button>
        </div>
      )}

      {/* o.converged is the optimizer's DESIGN-stationarity signal (mean |Δρ|
          settled before the iteration cap). It does NOT yet reflect the
          binned VERIFICATION solve's MGCG convergence — that is hardcoded
          converged:true in the wasm layer (see crates/filasim-wasm/src/lib.rs,
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

      {isStrength ? (
        <div className="dro hero">
          <div className="dro-label">
            <span>
              {o.sfFeasible === false
                ? `best achievable · target ≥ ${o.sfTarget}`
                : `safety factor · target ≥ ${o.sfTarget}`}
            </span>
            <span>
              {o.sfMeasure === "material"
                ? "material (von Mises)"
                : o.sfMeasure === "layer"
                  ? "layer adhesion"
                  : "material & layer adhesion"}
            </span>
          </div>
          <div className="dro-window">
            <b>{(o.sfAchieved ?? 0).toFixed(2)}</b>
            <span>× SF</span>
          </div>
        </div>
      ) : isMatch ? (
        <div className="dro hero">
          <div className="dro-label">
            <span>vs {Math.round(o.refUniformPct!)} % uniform, same stiffness</span>
            <span>
              {format(o.massUniformRefGrams!, "mass", { unit: false })} →{" "}
              {format(o.massGrams, "mass")}
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
            <span>{uniformLabel}</span>
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
            of {format(o.massSolidGrams, "mass")} solid · {Math.round(o.massFrac * 100)} %
          </span>
        </div>
        <div className="dro-window">
          <b>{formatParts(o.massGrams, "mass")[0]}</b>
          <span>{formatParts(o.massGrams, "mass")[1]}</span>
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
      {isStrength && (
        <>
          <div className="kv">
            <span>SF at 100 % fill (cap)</span>
            <b>{(o.sfBest ?? 0).toFixed(2)} ×</b>
          </div>
          {(o.sfPerStep?.length ?? 0) > 1 && (
            <div className="kv">
              <span>SF per load step (worst binds)</span>
              <b>{o.sfPerStep!.map((v) => v.toFixed(2)).join(" · ")}</b>
            </div>
          )}
          <div className="kv">
            <span>Critical region</span>
            <b>
              <button className="linkbtn" onClick={showSfView}>
                show in SF view
              </button>
            </b>
          </div>
        </>
      )}
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
        <span>{solid ? "Retained volume" : "Infill levels"}</span>
        <b>
          {solid
            ? `${uniformPct} %`
            : `${o.bins.map((b) => `${Math.round(b.density * 100)}`).join(" · ")} %`}
        </b>
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
      {o.selfWeight && (
        <div className="dim small selfweightnote">
          Self-weight active: each design carries its own true weight, so the fully-solid baseline
          is heavier and deflects more than a weightless comparison would show.
        </div>
      )}
      {isStrength && (
        <div className="dim small selfweightnote">
          SF targets are a design aid: strengths come from the material presets (or your measured
          values) with Gibson–Ashby scaling — not a certified safety factor.
        </div>
      )}
    </aside>
  );
}
