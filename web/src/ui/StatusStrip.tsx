// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Machine status strip: ready-lamp, grid / solver / optimizer telemetry,
// units, and the "Log for nerds" drawer toggle.

import { useShallow } from "zustand/shallow";
import { useStore } from "../store";
import { fmtLen, lenUnit } from "./fmt";
import { format, unitLabel } from "../units";

export function StatusStrip() {
  const s = useStore(
    useShallow((s) => ({
      error: s.error,
      busy: s.busy,
      model: s.model,
      fileName: s.fileName,
      voxelInfo: s.voxelInfo,
      optProgress: s.optProgress,
      buildProgress: s.buildProgress,
      stats: s.stats,
      optSummary: s.optSummary,
      openImprint: s.openImprint,
      logOpen: s.logOpen,
      setLogOpen: s.setLogOpen,
      openUnits: s.openUnits,
      // re-render the chip + dimension readouts whenever the unit selection changes
      unitRev: s.unitRev,
    }))
  );
  const lamp = s.error ? "lamp err" : s.busy ? "lamp busy" : "lamp";
  // The busy chip narrates details ("… — iteration 12"); the strip's state
  // lamp keeps just the stage name before the "—" so it doesn't tick.
  const state = s.busy
    ? s.busy.replace(/…$/, "").split(" — ")[0].toUpperCase()
    : s.error
      ? "ERROR"
      : "READY";
  const v = s.voxelInfo;
  const p = s.optProgress;
  const bp = s.buildProgress;
  const m = s.model;
  return (
    <footer className="strip">
      <div>
        <span className={lamp} /> {state}
      </div>
      {m && (
        <div className="partinfo" title={s.fileName ?? undefined}>
          <b>{s.fileName}</b> · {fmtLen(m.bbox[3] - m.bbox[0])} × {fmtLen(m.bbox[4] - m.bbox[1])} ×{" "}
          {fmtLen(m.bbox[5] - m.bbox[2])} {lenUnit()} · {m.triCount.toLocaleString()} tris
        </div>
      )}
      {v && (
        <div>
          GRID{" "}
          <b>
            {v.nx}×{v.ny}×{v.nz}
          </b>{" "}
          · <b>{Math.round(v.solid / 1000)}k</b> voxels · h <b>{format(v.h, "length")}</b>
        </div>
      )}
      {bp && (
        <div>
          SIM layer{" "}
          <b>
            {bp.done}
            {bp.total > 0 ? ` of ${bp.total}` : ""}
          </b>
          {bp.total > 0 && (
            <>
              {" "}
              · <b>{Math.round((bp.done / bp.total) * 100)}%</b>
            </>
          )}
        </div>
      )}
      {p ? (
        <div>
          OPT{" "}
          {(p.passes ?? 1) > 1 && (
            <>
              pass{" "}
              <b>
                {p.pass}/{p.passes}
              </b>{" "}
              ·{" "}
            </>
          )}
          it{" "}
          <b>
            {p.iteration} of ≤{p.maxIter}
          </b>
        </div>
      ) : (
        <>
          {s.stats && (
            <div>
              SOLVE <b>{s.stats.iterations} it</b>
              {s.stats.relResidual > 0 && (
                <>
                  {" "}
                  · res <b>{s.stats.relResidual.toExponential(1)}</b>
                </>
              )}{" "}
              · <b>{s.stats.seconds.toFixed(1)} s</b>
              {!s.stats.converged && <span className="warn"> · ⚠ NOT CONVERGED</span>}
            </div>
          )}
          {s.optSummary && (
            <div>
              OPT <b>{s.optSummary.iterations} it</b> ·{" "}
              {s.optSummary.converged ? "converged" : <span className="warn">⚠ at cap</span>}
            </div>
          )}
        </>
      )}
      <div className="grow" />
      <button
        onClick={() => s.openUnits(true)}
        title="Display units (length · stress · mass) — click to change"
      >
        {unitLabel("length")} · {unitLabel("stress")} · {unitLabel("mass")}
      </button>
      <button onClick={() => s.openImprint(true)} title="Impressum & Datenschutzerklärung">
        § IMPRINT
      </button>
      <button
        className={s.logOpen ? "on" : ""}
        onClick={() => s.setLogOpen(!s.logOpen)}
        title="Solver & optimizer telemetry with convergence charts"
      >
        ▤ LOG FOR NERDS
      </button>
    </footer>
  );
}
