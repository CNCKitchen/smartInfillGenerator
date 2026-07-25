// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// The caliper rail: six workflow stations on a measuring scale, the active
// one carried by the orange carriage. Done-states are derived from results,
// not from "visited" flags, so they stay honest when inputs change.

import { useShallow } from "zustand/shallow";
import { useStore } from "../store";
import { SUPPORT_KINDS } from "./bcmeta";

const OPTIMIZE_STEPS: { n: number; label: string; title: string }[] = [
  { n: 1, label: "Model", title: "1 · Model" },
  { n: 2, label: "Loads", title: "2 · Boundary conditions" },
  { n: 3, label: "Properties", title: "3 · Properties — material, print settings, analysis grid" },
  { n: 4, label: "Verify", title: "4 · Verify setup" },
  { n: 5, label: "Optimize", title: "5 · Optimization — infill density & print orientation" },
  { n: 6, label: "Export", title: "6 · View & export" },
];

// Build Sim ignores structural loads/verify/optimize — only the part, its
// material/grid, the simulation, and export.
const BUILDSIM_STEPS: { n: number; label: string; title: string }[] = [
  { n: 1, label: "Model", title: "1 · Model" },
  { n: 2, label: "Properties", title: "2 · Material & analysis grid" },
  { n: 3, label: "Simulate", title: "3 · Build simulation — warping & bed peel" },
  { n: 4, label: "Export", title: "4 · View & export" },
];

export function StepRail() {
  const s = useStore(
    useShallow((s) => ({
      bcs: s.bcs,
      model: s.model,
      check: s.check,
      hasResult: s.hasResult,
      optSummary: s.optSummary,
      appMode: s.appMode,
      activeStep: s.activeStep,
      setActiveStep: s.setActiveStep,
    }))
  );
  const buildsim = s.appMode === "buildsim";
  const STEPS = buildsim ? BUILDSIM_STEPS : OPTIMIZE_STEPS;
  const hasSupport = s.bcs.some(
    (b) =>
      (b.kind === "fixed" ||
        b.kind === "elastic" ||
        b.kind === "frictionless" ||
        b.kind === "displacement") &&
      b.tris.length > 0
  );
  const hasLoad = s.bcs.some((b) => {
    if (SUPPORT_KINDS.includes(b.kind)) return false;
    // Acceleration is selection-less (DESIGN §16) — it's a load with no surface.
    if (b.kind === "accel") return true;
    return b.tris.length > 0;
  });
  const done: Record<number, boolean> = buildsim
    ? {
        1: !!s.model,
        2: !!s.model, // material & grid always carry valid defaults
        3: s.hasResult, // a build sim has been run
        4: false,
      }
    : {
        1: !!s.model,
        2: hasSupport && hasLoad,
        3: !!s.model, // material & resolution always carry valid defaults
        4: !!s.check?.ok || s.hasResult,
        5: !!s.optSummary,
        6: false,
      };
  const active = s.model ? s.activeStep : 1;
  return (
    <nav className="rail" aria-label="Workflow">
      {STEPS.map((st) => (
        <button
          key={st.n}
          className={`station${active === st.n ? " active" : ""}${done[st.n] ? " done" : ""}`}
          disabled={st.n > 1 && !s.model}
          title={st.title}
          onClick={() => s.setActiveStep(st.n)}
        >
          <span className="st-no">{st.n}</span>
          <span className="st-name">{st.label}</span>
        </button>
      ))}
    </nav>
  );
}
