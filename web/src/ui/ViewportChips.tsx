// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Floating viewport controls, CAD-style: display modes top center, the
// section plane bottom left. They act on the view, so they live on the view.

import { useStore, type ViewMode } from "../store";

function ViewBtn({ mode, label }: { mode: ViewMode; label: string }) {
  const s = useStore();
  return (
    <button className={s.viewMode === mode ? "on" : ""} onClick={() => void s.setViewMode(mode)}>
      {label}
    </button>
  );
}

export function ViewportChips() {
  const s = useStore();
  if (!s.model) return null;
  return (
    <>
      <div className="viewmodes">
        <ViewBtn mode="setup" label="Setup" />
        <ViewBtn mode="mesh" label="Mesh" />
        {s.hasResult && <ViewBtn mode="deformed" label="Deformed" />}
        {s.optSummary && <ViewBtn mode="density" label="Density" />}
        {s.optSummary && <ViewBtn mode="infill" label="Regions" />}
      </div>
      <div className="sectionchip">
        <button
          className={s.sectionOn ? "on" : ""}
          onClick={() => s.toggleSection()}
          title="Cut through any view — drag the arrow to slide the plane, the rings to tilt it"
        >
          Section
        </button>
        {s.sectionOn && (
          <>
            <button onClick={() => s.flipSection()} title="Keep the other half">
              Flip
            </button>
            <button onClick={() => s.setSectionAxis("x")}>X</button>
            <button onClick={() => s.setSectionAxis("y")}>Y</button>
            <button onClick={() => s.setSectionAxis("z")}>Z</button>
          </>
        )}
      </div>
    </>
  );
}
