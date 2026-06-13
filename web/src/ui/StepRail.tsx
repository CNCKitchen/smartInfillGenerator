// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// The caliper rail: six workflow stations on a measuring scale, the active
// one carried by the orange carriage. Done-states are derived from results,
// not from "visited" flags, so they stay honest when inputs change.

import { useStore } from "../store";

const STEPS: { n: number; label: string; title: string }[] = [
  { n: 1, label: "Model", title: "1 · Model" },
  { n: 2, label: "Loads", title: "2 · Boundary conditions" },
  { n: 3, label: "Properties", title: "3 · Properties — material, print settings, analysis grid" },
  { n: 4, label: "Verify", title: "4 · Verify setup" },
  { n: 5, label: "Optimize", title: "5 · Optimize infill" },
  { n: 6, label: "Export", title: "6 · View & export" },
];

export function StepRail() {
  const s = useStore();
  const hasSupport = s.bcs.some(
    (b) =>
      (b.kind === "fixed" ||
        b.kind === "elastic" ||
        b.kind === "frictionless" ||
        b.kind === "displacement") &&
      b.tris.length > 0
  );
  const hasLoad = s.bcs.some(
    (b) => (b.kind === "force" || b.kind === "pressure") && b.tris.length > 0
  );
  const done: Record<number, boolean> = {
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
