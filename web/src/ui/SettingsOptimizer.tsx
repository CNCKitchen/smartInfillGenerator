// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Settings Optimizer (DESIGN §20): section for station 5 · Optimize.
//! Answers the question users actually ask their slicer — "how many walls and
//! how much infill do I need so this holds?" — by searching UNIFORM print
//! settings for the lightest print whose safety factor still clears a target.
//!
//! The walls × infill landscape appears as soon as the search starts and fills
//! in one cell per solve (the part recolors with each candidate's safety factor
//! at the same time), so the run is legible while it happens rather than a
//! spinner followed by a verdict. Solved cells carry their weight and safety
//! factor; the rest stay dim until "Solve full map". The winner is retained as
//! a selectable result, and Apply writes the settings into the print fields and
//! re-runs the standard As-Printed solve to verify them on the real mesh.

import { useShallow } from "zustand/shallow";
import { useStore } from "../store";
import { format } from "../units";
import { HelpTip, InfoTip } from "./HelpTip";
import { OPT_HELP } from "./helptext";
import { NumInput } from "./NumInput";
import { Section } from "./Section";

/** Above this, the binding cell is a notch tip rather than a weak region, and
 *  SF_crit will keep falling as the mesh is refined (§17 dec. 4, 2026-07-25).
 *  The old volume trim hid exactly this case; the panel now says it out loud. */
const RISER_WARN = 1.6;

/** One landscape cell as the panel needs it — from the finished sweep or from
 *  the run in flight. */
interface Cell {
  sf: number;
  massGrams: number;
}

/** Landscape cell fill: dim when unsolved, red below target, green above,
 *  each shaded by how far from the target it sits. */
function cellColor(sf: number | undefined, target: number): string {
  if (sf === undefined) return "var(--well-deep)";
  const r = sf / Math.max(target, 1e-9);
  if (r < 1) {
    // 0 → deep red, 1 → pale red.
    const t = Math.max(0, Math.min(1, r));
    return `rgb(${Math.round(150 + 80 * t)}, ${Math.round(54 + 110 * t)}, ${Math.round(28 + 100 * t)})`;
  }
  // 1 → pale green, ≥2× target → deep green.
  const t = Math.max(0, Math.min(1, r - 1));
  return `rgb(${Math.round(200 - 130 * t)}, ${Math.round(224 - 90 * t)}, ${Math.round(203 - 130 * t)})`;
}

export function SettingsOptimizer() {
  const s = useStore(
    useShallow((s) => ({
      settingsSweep: s.settingsSweep,
      settingsLive: s.settingsLive,
      settingsProgress: s.settingsProgress,
      settingsSfTarget: s.settingsSfTarget,
      settingsApplied: s.settingsApplied,
      setSettingsSfTarget: s.setSettingsSfTarget,
      runSettingsSweep: s.runSettingsSweep,
      applySettingsWinner: s.applySettingsWinner,
      sfMeasure: s.sfMeasure,
      setSfMeasure: s.setSfMeasure,
      perimeters: s.perimeters,
      lineWidth: s.lineWidth,
      layerHeight: s.layerHeight,
      pattern: s.pattern,
      printInfill: s.printInfill,
      topBottomLayers: s.topBottomLayers,
      model: s.model,
      bcs: s.bcs,
      busy: s.busy,
    }))
  );
  const sw = s.settingsSweep;
  const live = s.settingsLive;
  const target = s.settingsSfTarget;
  const w = sw?.winner ?? null;
  const canRun = !!s.model && s.bcs.length > 0 && !s.busy;

  // The landscape draws from the finished sweep, or from the run in flight.
  const axes = sw
    ? { walls: sw.walls, densities: sw.densities }
    : live
      ? { walls: live.walls, densities: live.densities }
      : null;
  const byCell = new Map<string, Cell>();
  const source = sw?.candidates ?? live?.cells ?? [];
  for (const c of source) {
    byCell.set(`${c.wallIndex},${c.densityIndex}`, { sf: c.sf, massGrams: c.massGrams });
  }
  // Per wall count: the lightest FEASIBLE density found (the dec. 10 curve).
  const lightest = new Map<number, number>();
  for (const c of source) {
    if (c.sf < target) continue;
    const cur = lightest.get(c.wallIndex);
    if (cur === undefined || c.densityIndex < cur) lightest.set(c.wallIndex, c.densityIndex);
  }
  const fullyMapped =
    !!sw && !!axes && sw.candidates.length >= axes.walls.length * axes.densities.length;
  const saving =
    w && sw?.massSolidGrams ? 1 - w.massGrams / Math.max(sw.massSolidGrams, 1e-9) : null;
  const applied =
    !!w &&
    s.perimeters === w.wall &&
    s.topBottomLayers === w.topBottomLayers &&
    s.printInfill === Math.round(w.density * 100);

  return (
    <Section
      title="Optimize print settings"
      help={OPT_HELP.settings}
      badge={w ? `${w.wall} × ${Math.round(w.density * 100)} %` : `SF ≥ ${target}`}
    >
      <div className="duo">
        <div className="group">
          <div className="g-label">
            <span>Minimum SF</span>
            <InfoTip help={OPT_HELP.settingsSf} />
          </div>
          <NumInput
            value={target}
            step={0.1}
            min={1}
            max={9.5}
            onCommit={(v) => s.setSettingsSfTarget(v)}
          />
        </div>
        <div className="group">
          <div className="g-label">
            <span>Measured against</span>
            <InfoTip help={OPT_HELP.sfMeasure} />
          </div>
          <select
            value={s.sfMeasure}
            onChange={(e) => s.setSfMeasure(e.target.value as "material" | "layer" | "both")}
          >
            <option value="both">Material &amp; layer</option>
            <option value="material">Material only</option>
            <option value="layer">Layer adhesion only</option>
          </select>
        </div>
      </div>

      <div className="toolrow">
        <button disabled={!canRun} onClick={() => void s.runSettingsSweep()}>
          {sw ? "Search again" : "Find settings"}
        </button>
        {sw && !fullyMapped && (
          <button
            disabled={!canRun}
            title="Solve every remaining walls × infill combination — slow, but fills the whole landscape"
            onClick={() => void s.runSettingsSweep(true)}
          >
            Solve full map
          </button>
        )}
      </div>

      {s.settingsProgress && (
        <div className="progress">
          <div
            className="bar"
            style={{
              width: `${Math.round((100 * s.settingsProgress.done) / Math.max(1, s.settingsProgress.total))}%`,
            }}
          />
          <span>
            solved {s.settingsProgress.done} of ~{s.settingsProgress.total} candidates
          </span>
        </div>
      )}

      {!sw && !!live && (
        <div className="dim small" style={{ textAlign: "center" }}>
          The part is showing each candidate's safety factor as it is solved.
        </div>
      )}

      {sw && w && (
        <>
          {!sw.feasible && (
            <div className="warnbanner">
              ⚠ <b>Safety factor {target} is out of reach with uniform settings.</b> The strongest
              print these settings can make — <b>{w.wall}</b> perimeters at{" "}
              <b>{Math.round(w.density * 100)} %</b> — only reaches <b>{sw.bestSf.toFixed(2)}</b>,
              {sw.ceilingStop
                ? " so nothing lighter can hold the target either and the search stopped there."
                : " and it is the strongest thing the search found."}{" "}
              Reorient the part (layer adhesion is usually what binds), pick a stronger material, or
              let the graded optimizer put material only where it is needed.
            </div>
          )}
          <div className={sw.feasible ? "status ok" : "status bad"}>
            {sw.feasible ? "Lightest print that holds" : "Best achievable"}: <b>{w.wall}</b>{" "}
            perimeter{w.wall === 1 ? "" : "s"} · <b>{Math.round(w.density * 100)} %</b> {s.pattern}{" "}
            · <b>{w.topBottomLayers}</b> top/bottom layer{w.topBottomLayers === 1 ? "" : "s"}
          </div>
          <div className="kv">
            <span>Mass</span>
            <b>
              {w.massGrams.toFixed(1)} g
              {saving !== null && saving > 0 ? ` · ${Math.round(saving * 100)} % under solid` : ""}
            </b>
          </div>
          <div className="kv">
            <span>Safety factor</span>
            <b className={w.sf < 1 ? "neg" : ""}>
              {w.sf >= 10 ? "≥ 10" : w.sf.toFixed(2)} × (target {target})
            </b>
          </div>
          {sw.riserRatio !== null && sw.riserRatio > RISER_WARN && (
            <div className="dim small">
              That number sits on a <b>sharp stress riser</b> — the material around the binding
              cell is {sw.riserRatio.toFixed(1)}× safer than the cell itself. Peak stress at a
              notch tip is partly a property of the mesh: refine it and this safety factor will
              drop again. Round the corner, or treat the value as a lower bound rather than a
              converged one.
            </div>
          )}
          {sw.loadSteps > 1 && (
            <div className="kv">
              <span>Worst of {sw.loadSteps} load steps</span>
              <b>{w.sfPerStep.map((v) => (v >= 10 ? "≥10" : v.toFixed(2))).join(" · ")}</b>
            </div>
          )}
          <div className="kv">
            <span>Max deflection</span>
            <b>{format(w.maxDisplacement, "length")}</b>
          </div>
          <div className="kv">
            <span>Search cost</span>
            <b>
              {sw.solves} solve{sw.solves === 1 ? "" : "s"} · {sw.candidates.length} settings
            </b>
          </div>
          {sw.excludedCells > 0 && (
            <div className="kv">
              <span>Excluded near rigid supports</span>
              <b>
                {sw.excludedCells.toLocaleString()} of{" "}
                {(sw.excludedCells + sw.scoredCells).toLocaleString()} cells
              </b>
            </div>
          )}
          {sw.prunedWalls.length > 0 && (
            <div className="dim small">
              Skipped {sw.prunedWalls.join(", ")} perimeter
              {sw.prunedWalls.length === 1 ? "" : "s"} without solving — even at 10 % infill they
              already weigh more than the winner.
            </div>
          )}
        </>
      )}

      {axes && (
        <Landscape
          walls={axes.walls}
          densities={axes.densities}
          byCell={byCell}
          lightest={lightest}
          target={target}
          winner={w ? { wi: w.wallIndex, di: w.densityIndex } : null}
        />
      )}

      {sw && w && (
        <>
          <div className="toolrow">
            <HelpTip help={OPT_HELP.settingsApply}>
              <button disabled={!!s.busy} onClick={() => void s.applySettingsWinner()}>
                {applied ? "Re-verify settings" : "Apply settings & verify"}
              </button>
            </HelpTip>
          </div>
          {s.settingsApplied && (
            <div className={s.settingsApplied.ok ? "status ok" : "status bad"}>
              {s.settingsApplied.ok
                ? `Verified as printed on the analysis mesh — SF ${s.settingsApplied.sf.toFixed(2)} ≥ ${target}.`
                : `Applied, but the verification solve lands at SF ${s.settingsApplied.sf.toFixed(
                    2
                  )} — below the ${target} target. Snapping moved the mesh; add a perimeter or one infill step.`}
            </div>
          )}
        </>
      )}
      {sw && !w && (
        <div className="status bad">
          No candidate solved — the part may be too thin for any interior at these wall settings.
        </div>
      )}
      {sw && w && (
        <div className="dim small">
          The winner is kept as a <b>Settings</b> result in the Results view, with its critical
          point marked.
        </div>
      )}
    </Section>
  );
}

/** Walls × density landscape (DESIGN §20 dec. 10): solved cells colored by
 *  safety factor with the boundary AT the target, unsolved cells dim, the
 *  winner ringed, and each wall count's lightest feasible print called out with
 *  its weight — the number the whole search is minimizing. */
function Landscape({
  walls,
  densities,
  byCell,
  lightest,
  target,
  winner,
}: {
  walls: number[];
  densities: number[];
  byCell: Map<string, Cell>;
  lightest: Map<number, number>;
  target: number;
  winner: { wi: number; di: number } | null;
}) {
  return (
    <div className="landscape">
      <div className="lmap" style={{ gridTemplateColumns: `auto repeat(${densities.length}, 1fr)` }}>
        {walls.map((wall, wi) => (
          <div key={`row${wi}`} style={{ display: "contents" }}>
            <span className="laxis">{wall}</span>
            {densities.map((d, di) => {
              const cell = byCell.get(`${wi},${di}`);
              const isWinner = winner?.wi === wi && winner?.di === di;
              const isLightest = lightest.get(wi) === di;
              return (
                <span
                  key={di}
                  className={`lcell${isWinner ? " win" : isLightest ? " light" : ""}`}
                  style={{ background: cellColor(cell?.sf, target) }}
                  title={
                    cell === undefined
                      ? `${wall} perimeters · ${Math.round(d * 100)} % — not solved`
                      : `${wall} perimeters · ${Math.round(d * 100)} % — ` +
                        `${cell.massGrams.toFixed(1)} g at SF ${cell.sf.toFixed(2)}` +
                        (cell.sf >= target ? "" : " (below target)")
                  }
                />
              );
            })}
          </div>
        ))}
        <span className="laxis" />
        {densities.map((d, di) => (
          <span key={`c${di}`} className="laxis bottom">
            {di % 2 === 0 ? Math.round(d * 100) : ""}
          </span>
        ))}
      </div>
      <div className="dim small" style={{ textAlign: "center" }}>
        perimeters (rows) × infill % (columns) — green holds SF ≥ {target}, red does not, dim was
        never solved. Hover a cell for its weight and SF; ◎ is the winner.
      </div>
    </div>
  );
}
