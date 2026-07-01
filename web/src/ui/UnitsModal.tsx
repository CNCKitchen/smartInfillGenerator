// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Display-unit settings (opened from the status-strip units chip). Presets-first
// (docs/units-design.md §4/§10a): the preset buttons are the primary control;
// the per-quantity grid sits behind an "advanced" disclosure and is greyed out
// while a CONSISTENT preset (SI-mm / US-in) is active, because a consistent
// readout can't be overridden without ceasing to be consistent (§6).

import { useState } from "react";
import { useShallow } from "zustand/shallow";
import { useStore } from "../store";
import {
  PRESETS,
  QUANTITIES,
  QUANTITY_KINDS,
  presetOf,
  type QuantityKind,
} from "../units";

const PRESET_ORDER = ["metric", "imperial", "simm", "usin"];

export function UnitsModal() {
  const s = useStore(
    useShallow((s) => ({
      unitsOpen: s.unitsOpen,
      unitPrefs: s.unitPrefs,
      openUnits: s.openUnits,
      setUnitPreset: s.setUnitPreset,
      setUnit: s.setUnit,
    }))
  );
  const [showAdvanced, setShowAdvanced] = useState(false);
  if (!s.unitsOpen) return null;

  const activePreset = presetOf(s.unitPrefs);
  const consistent = activePreset ? PRESETS[activePreset].consistent : false;

  return (
    <div className="modalback" onClick={() => s.openUnits(false)}>
      <div className="modal" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 560 }}>
        <div className="modalhead">
          <h2>Units</h2>
          <button className="x" onClick={() => s.openUnits(false)}>
            ×
          </button>
        </div>

        <div className="dim small">
          Display only — the engine always computes in mm/N. Pick a preset, or open Custom to set each
          quantity. The <b>consistent</b> systems (SI-mm, US-in) make every readout multiply out
          (σ = F/A) at the cost of idiomatic units (no GPa, no N·m), and lock the per-quantity grid.
        </div>

        <h3>Preset</h3>
        <div className="toolrow">
          {PRESET_ORDER.map((id) => {
            const p = PRESETS[id];
            return (
              <button
                key={id}
                className={activePreset === id ? "on" : ""}
                onClick={() => s.setUnitPreset(id)}
                title={p.consistent ? "Consistent system — readouts compose" : undefined}
              >
                {p.label}
              </button>
            );
          })}
        </div>

        <div className="toolrow">
          <button onClick={() => setShowAdvanced((v) => !v)}>
            {showAdvanced ? "▾ Customize per quantity" : "▸ Customize per quantity"}
          </button>
        </div>

        {showAdvanced && (
          <table className="settingstable">
            <thead>
              <tr>
                <th>Quantity</th>
                <th>Unit</th>
              </tr>
            </thead>
            <tbody>
              {QUANTITY_KINDS.map((kind: QuantityKind) => {
                const spec = QUANTITIES[kind];
                return (
                  <tr key={kind}>
                    <td>{spec.label}</td>
                    <td>
                      <select
                        value={s.unitPrefs[kind]}
                        disabled={consistent}
                        onChange={(e) => s.setUnit(kind, e.target.value)}
                      >
                        {spec.units.map((u) => (
                          <option key={u.id} value={u.id}>
                            {u.label || "(raw)"}
                          </option>
                        ))}
                      </select>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}

        {showAdvanced && consistent && (
          <div className="hint">
            Per-quantity overrides are locked while a consistent preset is active. Switch to Metric or
            Imperial to mix units.
          </div>
        )}
      </div>
    </div>
  );
}
