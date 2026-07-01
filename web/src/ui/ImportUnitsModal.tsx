// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// STL import-unit picker (units-design §8). STL is unitless, so we ask what the
// file was authored in and bake it to canonical mm once. This is the IMPORT unit
// (a one-time, irreversible scale) — distinct from the reversible DISPLAY unit.
// The status-strip bounding-box readout is the backstop if the wrong unit is
// picked; the Model step also has a ×25.4 / ÷25.4 rescale escape hatch.

import { useState } from "react";
import { useShallow } from "zustand/shallow";
import { useStore } from "../store";
import { QUANTITIES } from "../units";

export function ImportUnitsModal() {
  const s = useStore(
    useShallow((s) => ({
      pendingImport: s.pendingImport,
      importUnit: s.importUnit,
      confirmImport: s.confirmImport,
      cancelImport: s.cancelImport,
    }))
  );
  const [unit, setUnit] = useState(s.importUnit);
  const [remember, setRemember] = useState(false);
  if (!s.pendingImport) return null;

  return (
    <div className="modalback" onClick={() => s.cancelImport()}>
      <div className="modal" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 420 }}>
        <div className="modalhead">
          <h2>Import units</h2>
          <button className="x" onClick={() => s.cancelImport()}>
            ×
          </button>
        </div>

        <div className="dim small">
          STL files carry no unit. Pick what <b>{s.pendingImport.name}</b> was authored in — the part
          is scaled to mm once on import. If it comes in the wrong size, the bounding box in the
          status bar will show it, and you can rescale from the Model step.
        </div>

        <label className="row" style={{ marginTop: 12 }}>
          <span>This file is in</span>
          <select value={unit} onChange={(e) => setUnit(e.target.value)}>
            {QUANTITIES.length.units.map((u) => (
              <option key={u.id} value={u.id}>
                {u.label}
              </option>
            ))}
          </select>
        </label>

        <label className="rowcheck" style={{ marginTop: 8 }}>
          <input
            type="checkbox"
            checked={remember}
            onChange={(e) => setRemember(e.target.checked)}
          />
          <span>Don't ask again — always use this unit (re-enable in Settings)</span>
        </label>

        <div className="modalfoot">
          <button onClick={() => s.cancelImport()}>Cancel</button>
          <button className="primary" onClick={() => s.confirmImport(unit, remember)}>
            Import
          </button>
        </div>
      </div>
    </div>
  );
}
