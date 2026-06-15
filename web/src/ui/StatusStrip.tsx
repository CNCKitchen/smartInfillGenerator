// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Machine status strip: ready-lamp, grid / solver / optimizer telemetry,
// units, and the "Log for nerds" drawer toggle.

import { useStore } from "../store";

export function StatusStrip() {
  const s = useStore();
  const lamp = s.error ? "lamp err" : s.busy ? "lamp busy" : "lamp";
  const state = s.busy ? s.busy.replace(/…$/, "").toUpperCase() : s.error ? "ERROR" : "READY";
  const v = s.voxelInfo;
  const p = s.optProgress;
  return (
    <footer className="strip">
      <div>
        <span className={lamp} /> {state}
      </div>
      {v && (
        <div>
          GRID{" "}
          <b>
            {v.nx}×{v.ny}×{v.nz}
          </b>{" "}
          · <b>{Math.round(v.solid / 1000)}k</b> cells · h <b>{v.h.toFixed(2)} mm</b>
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
      <div>mm · MPa</div>
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
