// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { useShallow } from "zustand/shallow";
import { useStore } from "../store";
import { NumInput } from "./NumInput";
import { format } from "../units";
import { isIsotropic } from "../types";
export function SettingsModal() {
  const s = useStore(
    useShallow((s) => ({
      settingsOpen: s.settingsOpen,
      openSettings: s.openSettings,
      materials: s.materials,
      material: s.material,
      openMaterialsManager: s.openMaterialsManager,
      curves: s.curves,
      propertySets: s.propertySets,
      activeSetId: s.activeSetId,
      openPropsManager: s.openPropsManager,
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
          FDM (printed) and isotropic (machined, cast, resin prints) materials live in one
          library — every value, the process switch and the stress–strain charts are in the
          material manager. Editing the material in use invalidates current results. Saved in
          this browser.
        </div>
        <div className="row">
          <button onClick={() => s.openMaterialsManager(true)}>Manage materials…</button>
          <span className="dim small">
            {`${s.materials.length} materials — in use: ${s.material.name} (E = ${format(s.material.e0, "modulus")}, ${
              isIsotropic(s.material)
                ? `σy = ${format(s.material.yieldStrength, "stress")}`
                : `σₜ = ${format(s.material.strength, "stress")}`
            }).`}
          </span>
        </div>

        <h3>Infill properties</h3>
        <div className="dim small">
          The infill is chosen on the Properties step; its property set (E(ρ) law + anisotropy
          ratios) is managed here.
        </div>
        <div className="row">
          <button onClick={() => s.openPropsManager(true)}>Manage infill properties…</button>
          <span className="dim small">
            {(() => {
              const act = s.propertySets.find((p) => p.id === s.activeSetId);
              const c = act?.curve ?? s.curves.cubic;
              const rel = (rho: number) =>
                `${(100 * Math.min(1, c.coeff * Math.pow(rho, c.exponent))).toFixed(1)}%`;
              return `In use: ${act ? act.name : "project values"} — E(20%) ${rel(0.2)} · E(50%) ${rel(0.5)}.`;
            })()}
          </span>
        </div>

        <h3>Density levels</h3>
        <div className="dim small">
          The printable band the discrete levels are chosen from. Floor = "just so it prints"
          (also the budget slider's minimum). The levels themselves — auto placement or a pinned
          comma-separated list — are set on the Optimize step next to the level count.
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
