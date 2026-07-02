// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { useEffect, useRef, useState } from "react";
import { useShallow } from "zustand/shallow";
import { useStore } from "../store";
import { NumInput } from "./NumInput";
import { UnitInput } from "./UnitInput";
import { unitLabel } from "../units";
import type { PatternKey } from "../types";

const PATTERN_LABEL: Record<PatternKey, string> = {
  gyroid: "Gyroid",
  cubic: "Cubic",
  grid: "Grid",
};

export function SettingsModal() {
  const s = useStore(
    useShallow((s) => ({
      settingsOpen: s.settingsOpen,
      openSettings: s.openSettings,
      materials: s.materials,
      updateMaterial: s.updateMaterial,
      removeMaterial: s.removeMaterial,
      addMaterial: s.addMaterial,
      resetMaterials: s.resetMaterials,
      curves: s.curves,
      setCurve: s.setCurve,
      resetCurves: s.resetCurves,
      levelSettings: s.levelSettings,
      updateLevelSettings: s.updateLevelSettings,
      unitRev: s.unitRev,
      askImportUnit: s.askImportUnit,
      setAskImportUnit: s.setAskImportUnit,
    }))
  );
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
          E, σₜ (in-layer tensile strength) and σₜᶻ (layer adhesion — tension across the layers,
          typically 50–80% of σₜ) in MPa, ρ in g/cm³. The strengths drive the safety-factor
          plots; the worst-case factor uses whichever limit governs. Tg and CTE (optional) derive
          the build-sim shrink from physics and enable its temperature ladder; blank = the raw
          Shrink % path. Editing the material in use invalidates current results. Saved in this
          browser.
        </div>
        <table className="settingstable">
          <thead>
            <tr>
              <th>Name</th>
              <th>E ({unitLabel("modulus")})</th>
              <th>ν</th>
              <th>ρ ({unitLabel("density")})</th>
              <th>σₜ ({unitLabel("stress")})</th>
              <th>σₜᶻ ({unitLabel("stress")})</th>
              <th title="Build-sim yield stress — enables the elastic–perfectly-plastic step so the released warp responds to infill density (0 = pure-elastic, density-blind)">σy ({unitLabel("stress")})</th>
              <th title="Build-sim IN-PLANE (XY) process shrink — the dominant warp driver (inherent strain). Used only when Tg/CTE are blank">Shrink % (XY)</th>
              <th title="Build-sim THROUGH-LAYER (Z) process shrink — transverse isotropy; usually less than in-plane. Used only when Tg/CTE are blank">Shrink % (Z)</th>
              <th title="Build-sim locking temperature in °C: Tg (amorphous) / ~Tc (semi-crystalline). With a CTE, the shrink derives from physics (CTE × lock→room) and the temperature ladder turns on; blank = raw shrink (legacy)">Tg (°C)</th>
              <th title="Build-sim effective printed-part CTE, in-plane (XY), in ppm/°C; blank = raw shrink (legacy)">CTE (ppm/°C)</th>
              <th title="Build-sim through-layer (Z) CTE in ppm/°C; blank = isotropic (= XY)">CTE Z (ppm/°C)</th>
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
                  <UnitInput
                    value={m.e0}
                    kind="modulus"
                    min={10}
                    step={50}
                    onCommit={(v) =>
                      s.updateMaterial(i, { ...m, e0: Math.max(10, v) })
                    }
                  />
                </td>
                <td>
                  <NumInput
                    value={m.nu}
                    min={0}
                    max={0.49}
                    step={0.01}
                    onCommit={(v) =>
                      s.updateMaterial(i, {
                        ...m,
                        nu: Math.min(0.49, Math.max(0, v)),
                      })
                    }
                  />
                </td>
                <td>
                  <UnitInput
                    value={m.density}
                    kind="density"
                    min={0.1}
                    step={0.01}
                    onCommit={(v) =>
                      s.updateMaterial(i, { ...m, density: Math.max(0.1, v) })
                    }
                  />
                </td>
                <td>
                  <UnitInput
                    value={m.strength}
                    kind="stress"
                    min={1}
                    step={1}
                    onCommit={(v) =>
                      s.updateMaterial(i, { ...m, strength: Math.max(1, v) })
                    }
                  />
                </td>
                <td>
                  <UnitInput
                    value={m.strengthZ}
                    kind="stress"
                    min={1}
                    step={1}
                    onCommit={(v) =>
                      s.updateMaterial(i, { ...m, strengthZ: Math.max(1, v) })
                    }
                  />
                </td>
                <td>
                  <UnitInput
                    value={m.yieldStrength ?? 0}
                    kind="stress"
                    min={0}
                    step={1}
                    onCommit={(v) =>
                      s.updateMaterial(i, { ...m, yieldStrength: Math.max(0, v) })
                    }
                  />
                </td>
                <td>
                  <NumInput
                    value={m.shrink * 100}
                    min={0}
                    step={0.1}
                    onCommit={(v) =>
                      s.updateMaterial(i, { ...m, shrink: Math.max(0, v) / 100 })
                    }
                  />
                </td>
                <td>
                  <NumInput
                    value={(m.shrinkZ ?? m.shrink) * 100}
                    min={0}
                    step={0.1}
                    onCommit={(v) =>
                      s.updateMaterial(i, { ...m, shrinkZ: Math.max(0, v) / 100 })
                    }
                  />
                </td>
                <td>
                  <OptNumInput
                    value={m.tLock}
                    min={0}
                    step={5}
                    onCommit={(v) =>
                      s.updateMaterial(i, { ...m, tLock: v == null ? undefined : Math.max(0, v) })
                    }
                  />
                </td>
                <td>
                  <OptNumInput
                    value={ppm(m.cte)}
                    min={0}
                    step={1}
                    onCommit={(v) =>
                      s.updateMaterial(i, { ...m, cte: v == null ? undefined : Math.max(0, v) / 1e6 })
                    }
                  />
                </td>
                <td>
                  <OptNumInput
                    value={ppm(m.cteZ)}
                    min={0}
                    step={1}
                    onCommit={(v) =>
                      s.updateMaterial(i, { ...m, cteZ: v == null ? undefined : Math.max(0, v) / 1e6 })
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
                    <NumInput
                      value={c.coeff}
                      min={0.05}
                      max={2}
                      step={0.05}
                      onCommit={(v) =>
                        s.setCurve(p, {
                          ...c,
                          coeff: Math.min(2, Math.max(0.05, v)),
                        })
                      }
                    />
                  </td>
                  <td>
                    <NumInput
                      value={c.exponent}
                      min={1}
                      max={3.5}
                      step={0.05}
                      onCommit={(v) =>
                        s.setCurve(p, {
                          ...c,
                          exponent: Math.min(3.5, Math.max(1, v)),
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

        <h3>Density levels</h3>
        <div className="dim small">
          The printable band and how the discrete levels are chosen. Floor = "just so it prints"
          (also the budget slider's minimum); pin the levels manually to match densities you have
          calibration data for.
        </div>
        <div className="row">
          <label className="row">
            <span>Floor</span>
            <NumInput
              value={s.levelSettings.floorPct}
              min={5}
              max={30}
              step={1}
              onCommit={(v) => s.updateLevelSettings({ floorPct: Math.min(30, Math.max(5, Math.round(v))) })}
            />
            <span className="dim">%</span>
          </label>
          <label className="row">
            <span>Cap</span>
            <NumInput
              value={s.levelSettings.capPct}
              min={40}
              max={100}
              step={5}
              onCommit={(v) => s.updateLevelSettings({ capPct: Math.min(100, Math.max(40, Math.round(v))) })}
            />
            <span className="dim">%</span>
          </label>
          <label className="row">
            <span>Binary floor</span>
            <NumInput
              value={s.levelSettings.binaryFloorPct}
              min={3}
              max={15}
              step={1}
              onCommit={(v) =>
                s.updateLevelSettings({ binaryFloorPct: Math.min(15, Math.max(3, Math.round(v))) })
              }
            />
            <span className="dim">%</span>
          </label>
        </div>
        <div className="row">
          <label className="row">
            <span>Placement</span>
            <select
              value={s.levelSettings.mode}
              onChange={(e) =>
                s.updateLevelSettings({ mode: e.target.value as "auto" | "manual" })
              }
            >
              <option value="auto">Auto (from the optimized field)</option>
              <option value="manual">Manual list</option>
            </select>
          </label>
          {s.levelSettings.mode === "manual" && <ManualLevelsInput />}
        </div>
        <div className="hint">
          Auto pins the bottom level at the floor and places the load-bearing levels high (dense
          infill is stiffer per gram). Manual levels still get the mass-true assignment, so the
          budget is met either way.
        </div>

        <h3>Import</h3>
        <label className="rowcheck">
          <input
            type="checkbox"
            checked={s.askImportUnit}
            onChange={(e) => s.setAskImportUnit(e.target.checked)}
          />
          <span>Ask for the unit on every STL import</span>
        </label>
        <div className="dim small">
          STL files carry no unit. When off, imports silently use your last choice — the status-bar
          bounding box still catches a wrong guess, and the Model step can rescale.
        </div>
      </div>
    </div>
  );
}

/** 1/°C → ppm/°C for display, rounded so 96e-6 shows as 96, not 96.00000000000001. */
function ppm(v: number | undefined): number | undefined {
  return v == null ? undefined : +(v * 1e6).toFixed(3);
}

/** NumInput variant for OPTIONAL fields: an empty box means "unset" and
 *  commits `undefined` (on blur, so mid-edit clearing doesn't unset). */
function OptNumInput({
  value,
  onCommit,
  ...rest
}: {
  value: number | undefined;
  onCommit: (v: number | undefined) => void;
} & Omit<React.InputHTMLAttributes<HTMLInputElement>, "value" | "onChange" | "type">) {
  const [text, setText] = useState<string>(value == null ? "" : String(value));
  const focused = useRef(false);
  useEffect(() => {
    if (!focused.current) setText(value == null ? "" : String(value));
  }, [value]);
  return (
    <input
      type="number"
      value={text}
      placeholder="—"
      onFocus={() => {
        focused.current = true;
      }}
      onChange={(e) => {
        setText(e.target.value);
        const n = Number(e.target.value);
        if (e.target.value !== "" && Number.isFinite(n)) onCommit(n);
      }}
      onBlur={(e) => {
        focused.current = false;
        const raw = e.target.value.trim();
        if (raw === "") onCommit(undefined);
        else {
          const n = Number(raw);
          if (Number.isFinite(n)) onCommit(n);
        }
        setText(value == null ? "" : String(value));
      }}
      {...rest}
    />
  );
}

/** Comma-separated manual level list, parsed/validated on commit. */
function ManualLevelsInput() {
  const s = useStore(
    useShallow((s) => ({
      levelSettings: s.levelSettings,
      updateLevelSettings: s.updateLevelSettings,
    }))
  );
  const [text, setText] = useState(s.levelSettings.manual.join(", "));
  useEffect(() => {
    setText(s.levelSettings.manual.join(", "));
  }, [s.levelSettings.manual]);
  const commit = () => {
    const vals = text
      .split(/[,;\s]+/)
      .map(Number)
      .filter((v) => Number.isFinite(v) && v >= 1 && v <= 100)
      .map(Math.round);
    const uniq = [...new Set(vals)].sort((a, b) => a - b);
    if (uniq.length >= 2) s.updateLevelSettings({ manual: uniq });
    else setText(s.levelSettings.manual.join(", "));
  };
  return (
    <label className="row" style={{ flex: 1 }}>
      <span>Levels %</span>
      <input
        type="text"
        value={text}
        placeholder="10, 40, 70"
        onChange={(e) => setText(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") (e.target as HTMLInputElement).blur();
        }}
      />
    </label>
  );
}
