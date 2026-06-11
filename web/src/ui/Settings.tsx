// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { useStore } from "../store";
import type { PatternKey } from "../types";

const PATTERN_LABEL: Record<PatternKey, string> = {
  gyroid: "Gyroid",
  cubic: "Cubic",
  grid: "Grid",
};

export function SettingsModal() {
  const s = useStore();
  if (!s.settingsOpen) return null;
  return (
    <div className="modalback" onClick={() => s.openSettings(false)}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modalhead">
          <h2>Settings</h2>
          <button className="x" onClick={() => s.openSettings(false)}>
            ×
          </button>
        </div>

        <h3>Materials</h3>
        <div className="dim small">
          E in MPa, ρ in g/cm³. Editing the material in use invalidates current results. Saved in
          this browser.
        </div>
        <table className="settingstable">
          <thead>
            <tr>
              <th>Name</th>
              <th>E (MPa)</th>
              <th>ν</th>
              <th>ρ (g/cm³)</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {s.materials.map((m, i) => (
              <tr key={i}>
                <td>
                  <input
                    type="text"
                    value={m.name}
                    onChange={(e) => s.updateMaterial(i, { ...m, name: e.target.value })}
                  />
                </td>
                <td>
                  <input
                    type="number"
                    value={m.e0}
                    min={10}
                    step={50}
                    onChange={(e) =>
                      s.updateMaterial(i, { ...m, e0: Math.max(10, Number(e.target.value)) })
                    }
                  />
                </td>
                <td>
                  <input
                    type="number"
                    value={m.nu}
                    min={0}
                    max={0.49}
                    step={0.01}
                    onChange={(e) =>
                      s.updateMaterial(i, {
                        ...m,
                        nu: Math.min(0.49, Math.max(0, Number(e.target.value))),
                      })
                    }
                  />
                </td>
                <td>
                  <input
                    type="number"
                    value={m.density}
                    min={0.1}
                    step={0.01}
                    onChange={(e) =>
                      s.updateMaterial(i, { ...m, density: Math.max(0.1, Number(e.target.value)) })
                    }
                  />
                </td>
                <td>
                  <button
                    className="x"
                    disabled={s.materials.length <= 1}
                    onClick={() => s.removeMaterial(i)}
                  >
                    ×
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        <div className="toolrow">
          <button onClick={() => s.addMaterial()}>+ Add material</button>
          <button onClick={() => s.resetMaterials()}>Reset defaults</button>
        </div>

        <h3>Infill stiffness curves</h3>
        <div className="dim small">
          E(ρ) = c · E₀ · ρⁿ per pattern (Gibson–Ashby). Used by the optimizer and the
          verification solves — calibrate c and n against measured stiffness-vs-density data.
        </div>
        <table className="settingstable">
          <thead>
            <tr>
              <th>Pattern</th>
              <th>c</th>
              <th>n</th>
              <th className="dim">E(20%)</th>
              <th className="dim">E(50%)</th>
            </tr>
          </thead>
          <tbody>
            {(Object.keys(PATTERN_LABEL) as PatternKey[]).map((p) => {
              const c = s.curves[p];
              const rel = (rho: number) =>
                `${(100 * Math.min(1, c.coeff * Math.pow(rho, c.exponent))).toFixed(1)}%`;
              return (
                <tr key={p}>
                  <td>{PATTERN_LABEL[p]}</td>
                  <td>
                    <input
                      type="number"
                      value={c.coeff}
                      min={0.05}
                      max={2}
                      step={0.05}
                      onChange={(e) =>
                        s.setCurve(p, {
                          ...c,
                          coeff: Math.min(2, Math.max(0.05, Number(e.target.value))),
                        })
                      }
                    />
                  </td>
                  <td>
                    <input
                      type="number"
                      value={c.exponent}
                      min={1}
                      max={3.5}
                      step={0.05}
                      onChange={(e) =>
                        s.setCurve(p, {
                          ...c,
                          exponent: Math.min(3.5, Math.max(1, Number(e.target.value))),
                        })
                      }
                    />
                  </td>
                  <td className="dim">{rel(0.2)}</td>
                  <td className="dim">{rel(0.5)}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
        <div className="toolrow">
          <button onClick={() => s.resetCurves()}>Reset curves</button>
        </div>
        <div className="hint">
          Changed curves apply to the next optimization run. E(ρ) values are relative to the solid
          material; the table shows the resulting stiffness at 20% and 50% infill.
        </div>
      </div>
    </div>
  );
}
