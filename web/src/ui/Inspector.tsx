// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Results dock: DRO-style readout windows for the SELECTED result — outcomes
// only (deflection first, then how the design compares). The settings a result
// was computed with live on the provenance card in the viewport, not here.
// An as-printed entry gets the printed dock even while an optimization exists;
// optimized / uniform / solid entries get the optimizer dock.

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
      results: s.results,
      activeResultId: s.activeResultId,
    }))
  );
  const entry = s.results.find((r) => r.id === s.activeResultId);
  if (entry?.kind === "asprinted" && s.printedStats) return <PrintedResults />;
  if (s.optSummary) return <OptResults />;
  if (s.printedStats && s.stats && s.hasResult) return <PrintedResults />;
  return null;
}

/** Whether the dock has anything to show. On narrow windows the inspector is
 *  an overlay drawer, and its tab must not offer to open an empty panel — so
 *  App gates the tab on this. Kept next to the render conditions above so the
 *  two cannot drift apart. */
export function useInspectorPopulated(): boolean {
  return useStore((s) => {
    const entry = s.results.find((r) => r.id === s.activeResultId);
    if (entry?.kind === "asprinted" && s.printedStats) return true;
    if (s.optSummary) return true;
    return !!(s.printedStats && s.stats && s.hasResult);
  });
}

/** Dock for "Solve once · As printed": the part at today's print settings. */
function PrintedResults() {
  const s = useStore(
    useShallow((s) => ({
      printedStats: s.printedStats,
      stats: s.stats,
      material: s.material,
      results: s.results,
      activeResultId: s.activeResultId,
      unitRev: s.unitRev,
    }))
  );
  const p = s.printedStats!;
  // Prefer the selected as-printed entry (a load step or the worst-case
  // envelope) over the last solve's live stats, so switching results in the
  // viewer switches these readouts with it.
  const active = s.results.find((r) => r.id === s.activeResultId);
  const entry = active?.kind === "asprinted" ? active : undefined;
  const maxDisp = entry?.maxDisplacement ?? s.stats?.maxDisplacement;
  if (maxDisp == null) return null;
  const converged = entry ? entry.converged : s.stats?.converged ?? true;
  const minSf = entry ? entry.minSf : p.minSf;
  const massGrams = entry?.massGrams ?? p.massGrams;
  const multiStep = s.results.filter((r) => r.kind === "asprinted").length > 1;
  const [defl, deflUnit] = fmtDispParts(maxDisp);
  const [mass, massUnit] = formatParts(massGrams, "mass");
  return (
    <aside className="inspector" aria-label="Results">
      <div className="i-head">
        <span>Results</span>
        <span>as printed</span>
      </div>

      {!converged && (
        <div className="warnbanner">
          ⚠ <b>Solve did not converge.</b>{" "}
          {s.stats && !s.stats.converged ? (
            <>
              It stopped at the {s.stats.iterations}-iteration cap with relative residual{" "}
              {s.stats.relResidual.toExponential(1)} (target {(s.stats.tol ?? 1e-5).toExponential(0)}
              ).{" "}
            </>
          ) : (
            <>It stopped at the iteration cap. </>
          )}
          The numbers below are an <b>unconverged approximation</b> — treat them as indicative only.
          A coarser mesh (Preview / Normal) converges reliably and is usually just as accurate for
          homogenized infill.
        </div>
      )}

      <div className="dro hero">
        <div className="dro-label">
          <span>Max deflection</span>
          {multiStep && entry && <span>{entry.loadStepName}</span>}
        </div>
        <div className="dro-window">
          <b>{defl}</b>
          <span>{deflUnit}</span>
        </div>
      </div>

      <div className="dro">
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
        {/* Below 1 the part does not hold the load — red, not a plain digit. */}
        <div className={minSf !== null && minSf < 1 ? "dro-window alarm" : "dro-window"}>
          <b>{minSf !== null ? minSf.toFixed(2) : "—"}</b>
          <span>×</span>
        </div>
        {/* §17 dec. 4 (2026-07-25): this IS the optimizers' number — the
            criterion's lowest scored cell, not the raw surface minimum — so
            the dock can confirm or refute a goal instead of contradicting it.
            When it rides a notch tip, say so where it is read. */}
        {p.sfRiser != null && p.sfRiser > 1.6 && (
          <div className="dro-note">
            on a sharp stress riser ({p.sfRiser.toFixed(1)}× safer one cell away) — a finer mesh
            will report less
          </div>
        )}
      </div>

      <div className="dro">
        <div className="dro-label">
          <span>Mass</span>
          <span>
            of {format(p.massSolidGrams, "mass")} solid ·{" "}
            {Math.round((100 * (massGrams ?? p.massGrams)) / Math.max(p.massSolidGrams, 1e-9))} %
          </span>
        </div>
        <div className="dro-window">
          <b>{mass}</b>
          <span>{massUnit}</span>
        </div>
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
      optMinSf: s.optMinSf,
      results: s.results,
      activeResultId: s.activeResultId,
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
  const isMatch = o.goal === "match" && o.massUniformRefGrams != null;
  const isStrength = o.goal === "strength";
  const saved = isMatch ? 1 - o.massGrams / o.massUniformRefGrams! : 0;
  // The deflection readout follows the SELECTED result (an optimized load step,
  // the envelope, or one of the baselines); the comparison card below always
  // holds the optimizer's primary-load trio.
  const active = s.results.find((r) => r.id === s.activeResultId);
  const entry =
    active && (active.kind === "optimized" || active.kind === "uniform" || active.kind === "solid")
      ? active
      : undefined;
  const multiStep = s.results.filter((r) => r.kind === "optimized").length > 1;
  const deflSub =
    entry && entry.kind !== "optimized"
      ? entry.label
      : entry && multiStep
        ? entry.loadStepName
        : undefined;
  const [defl, deflUnit] = fmtDispParts(entry?.maxDisplacement ?? o.maxDisplacement);
  const hasBars = !!o.hasBaselines && o.uniformMaxDisp != null && o.solidMaxDisp != null;
  // One-click binding view (DESIGN §17 dec. 6): the SF field of the run's
  // measure, banded in two colors with the boundary AT the target — every
  // red cell is below the required safety factor.
  const showSfView = () => {
    const base = o.sfMeasure === "material" ? "sfm" : o.sfMeasure === "layer" ? "sfz" : "sf";
    // DESIGN §20 dec. 7: when the run excluded a support's singularity zone,
    // show the CRITERION's view — that zone greyed — so the red cells are
    // exactly the ones the goal was unable to fix. Never hidden silently: the
    // plain field is one pick away in the field chip.
    const field = (o.bcExcludedCells ?? 0) > 0 ? `${base}x` : base;
    void s.setResultField(field);
    s.setLegendRange(0, 2 * (o.sfTarget ?? 2));
    s.setBandCount(2);
  };

  const bars = hasBars
    ? [
        {
          key: "uniform",
          name: `Uniform ${uniformPct} %`,
          mass: o.massGrams,
          d: o.uniformMaxDisp!,
          me: false,
        },
        { key: "opt", name: "Optimized", mass: o.massGrams, d: o.maxDisplacement, me: true },
        { key: "solid", name: "Solid 100 %", mass: o.massSolidGrams, d: o.solidMaxDisp!, me: false },
      ]
    : [];
  const barMax = Math.max(...bars.map((b) => b.d), 1e-12);

  // Efficiency = stiffness per unit weight, solid = 100 %. Derived from the
  // summary's compliance ratios: optimized is stiffnessVsSolid at massFrac of
  // the weight; the equal-mass uniform fill is (1 + gainVsUniform)× softer at
  // that same weight. No extra solves involved.
  const effOpt = o.stiffnessVsSolid / Math.max(o.massFrac, 1e-9);
  const effUni = effOpt / (1 + o.gainVsUniform);
  const effRows = [
    {
      key: "uniform",
      name: solid ? "Even spread" : `Uniform ${uniformPct} %`,
      e: effUni,
      me: false,
    },
    { key: "opt", name: "Optimized", e: effOpt, me: true },
    { key: "solid", name: "Solid 100 %", e: 1, me: false },
  ];
  const effMax = Math.max(...effRows.map((r) => r.e), 1e-12);

  // Min safety factor per design: min over the part, worst load step for the
  // optimized design; the baselines exist under the primary load only. The SF
  // fields are capped at 10 in the engine, so at-cap reads "≥ 10".
  const sfRows = [
    { key: "uniform", name: `Uniform ${uniformPct} %`, v: s.optMinSf?.uniform, me: false },
    { key: "opt", name: "Optimized", v: s.optMinSf?.optimized, me: true },
    { key: "solid", name: "Solid 100 %", v: s.optMinSf?.solid, me: false },
  ].filter((r) => r.v != null);
  const sfLabel = (sf: number) => (sf >= 10 ? "≥ 10" : `${sf.toFixed(2)} ×`);

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

      {isStrength && (
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
          <div className={(o.sfAchieved ?? 0) < 1 ? "dro-window alarm" : "dro-window"}>
            <b>{(o.sfAchieved ?? 0).toFixed(2)}</b>
            <span>× SF</span>
          </div>
        </div>
      )}
      {isMatch && (
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
      )}

      <div className={isStrength || isMatch ? "dro" : "dro hero"}>
        <div className="dro-label">
          <span>Max deflection</span>
          {deflSub && <span>{deflSub}</span>}
        </div>
        <div className="dro-window">
          <b>{defl}</b>
          <span>{deflUnit}</span>
        </div>
      </div>

      {hasBars ? (
        <div className="cmpcard">
          <div className="cmp-head">
            <span>How it compares</span>
            <span>max deflection{multiStep ? " · primary load" : ""}</span>
          </div>
          {bars.map((b) => (
            <div key={b.key} className={b.me ? "cmprow me" : "cmprow"}>
              <div className="cmp-line">
                <span>
                  {b.name} · {format(b.mass, "mass")}
                </span>
                <b>{fmtDispParts(b.d).join(" ")}</b>
              </div>
              <div className="cmp-bar">
                <i style={{ width: `${Math.max(2, (100 * b.d) / barMax)}%` }} />
              </div>
            </div>
          ))}
          <div className="cmp-notes">
            <div>
              <b>+{gain} %</b> stiffer than {uniformPct} % uniform at the same weight
            </div>
            <div>
              <b>{stiff} %</b> of solid stiffness at {Math.round(o.massFrac * 100)} % of the weight
            </div>
          </div>
        </div>
      ) : (
        // Part Topo (solid-topology) runs have no baseline solves to plot —
        // the comparison collapses to its two takeaway lines.
        <div className="cmpcard">
          <div className="cmp-head">
            <span>How it compares</span>
          </div>
          <div className="cmp-notes noline">
            <div>
              <b>+{gain} %</b> stiffer than{" "}
              {solid ? "the same material spread evenly" : `${uniformPct} % uniform`} at the same
              weight
            </div>
            <div>
              <b>{stiff} %</b> of solid stiffness at {Math.round(o.massFrac * 100)} % of the weight
            </div>
          </div>
        </div>
      )}

      <div className="cmpcard">
        <div className="cmp-head">
          <span>Efficiency</span>
          <span>stiffness ÷ weight · solid = 100 %</span>
        </div>
        {effRows.map((r) => (
          <div key={r.key} className={r.me ? "cmprow me" : "cmprow"}>
            <div className="cmp-line">
              <span>{r.name}</span>
              <b>{Math.round(100 * r.e)} %</b>
            </div>
            <div className="cmp-bar">
              <i style={{ width: `${Math.max(2, (100 * r.e) / effMax)}%` }} />
            </div>
          </div>
        ))}
      </div>

      {sfRows.length > 0 && (
        <div
          className="cmpcard"
          title="Min over the part; the optimized design takes its worst load step, the baselines were solved under the primary load. Advisory — preset/measured strengths with Gibson–Ashby scaling."
        >
          <div className="cmp-head">
            <span>Safety factor</span>
            <span>worst case · min SF</span>
          </div>
          {sfRows.map((r) => (
            <div key={r.key} className={r.me ? "cmprow me" : "cmprow"}>
              <div className="cmp-line">
                <span>
                  {r.name} · {r.v!.governs === "layer" ? "layer adhesion" : "material"}
                </span>
                <b className={r.v!.minSf < 1 ? "neg" : ""}>{sfLabel(r.v!.minSf)}</b>
              </div>
            </div>
          ))}
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

      {isStrength && (
        <>
          <div className="divider" />
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
