// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Floating viewport controls, CAD-style: display modes top center, the
// result-field picker right beneath them, deflection playback bottom center,
// the section plane bottom left. Result review happens ON the result.

import { useShallow } from "zustand/shallow";
import { useStore, resultStale, type ViewMode, type ResultKind } from "../store";
import { RESULT_FIELDS } from "../types";

function ViewBtn({ mode, label }: { mode: ViewMode; label: string }) {
  const s = useStore(
    useShallow((s) => ({ viewMode: s.viewMode, setViewMode: s.setViewMode }))
  );
  return (
    <button className={s.viewMode === mode ? "on" : ""} onClick={() => void s.setViewMode(mode)}>
      {label}
    </button>
  );
}

export function ViewportChips() {
  const s = useStore(
    useShallow((s) => ({
      model: s.model,
      viewMode: s.viewMode,
      hasResult: s.hasResult,
      optSummary: s.optSummary,
      results: s.results,
      activeResultId: s.activeResultId,
      resultEpochs: s.resultEpochs,
      selectResult: s.selectResult,
      wireframe: s.wireframe,
      setWireframe: s.setWireframe,
      resultSurface: s.resultSurface,
      setResultSurface: s.setResultSurface,
      resultField: s.resultField,
      setResultField: s.setResultField,
      animateDeformed: s.animateDeformed,
      setAnimateDeformed: s.setAnimateDeformed,
      sectionOn: s.sectionOn,
      toggleSection: s.toggleSection,
      flipSection: s.flipSection,
      setSectionAxis: s.setSectionAxis,
    }))
  );
  if (!s.model) return null;
  const resultsView = s.viewMode === "deformed" && s.hasResult;
  return (
    <>
      <div className="viewmodes">
        <ViewBtn mode="setup" label="Setup" />
        <ViewBtn mode="mesh" label="Mesh" />
        {s.hasResult && <ViewBtn mode="deformed" label="Results" />}
        {s.optSummary && <ViewBtn mode="density" label="Density" />}
        {s.optSummary && <ViewBtn mode="infill" label="Regions" />}
      </div>

      {(s.viewMode === "setup" || s.viewMode === "mesh") && (
        <div className="wirechip">
          <button
            className={s.wireframe ? "on" : ""}
            onClick={() => s.setWireframe(!s.wireframe)}
            title="Overlay the model's triangle mesh to inspect its quality"
          >
            Wireframe
          </button>
        </div>
      )}

      {resultsView && (
        <div className="fieldchip">
          {s.results.length > 0 && (
            <>
              <select
                className="resultsel"
                value={s.activeResultId ?? ""}
                onChange={(e) => void s.selectResult(e.target.value as ResultKind)}
                title="Which result to view — optimized, the even-fill baselines, or the as-printed solve"
              >
                {s.results.map((r) => (
                  <option key={r.id} value={r.id}>
                    {resultStale(r, s.resultEpochs) ? "⚠ " : ""}
                    {r.label}
                  </option>
                ))}
              </select>
              <span className="chipdiv" />
            </>
          )}
          <div className="seg">
            <button
              className={s.resultSurface === "stl" ? "on" : ""}
              onClick={() => void s.setResultSurface("stl")}
              title="Results sampled onto the smooth part surface"
            >
              STL
            </button>
            <button
              className={s.resultSurface === "voxel" ? "on" : ""}
              onClick={() => void s.setResultSurface("voxel")}
              title="Results on the analysis voxel mesh — exact nodal displacements, per-cell field values"
            >
              Voxels
            </button>
          </div>
          <select
            value={s.resultField}
            onChange={(e) => void s.setResultField(e.target.value)}
            title={
              s.resultField.startsWith("sf")
                ? "Allowables from the material table; graded infill scales with the same E(ρ) law as its stiffness. Worst case = min(material σᵥᴹ check, layer-adhesion σzz-tension check). Advisory."
                : "Scalar plotted on the deformed shape"
            }
          >
            <option value="u">Displacement |u|</option>
            <optgroup label="Displacement (mm)">
              {RESULT_FIELDS.filter((f) => ["ux", "uy", "uz"].includes(f.value)).map((f) => (
                <option key={f.value} value={f.value}>
                  {f.label}
                </option>
              ))}
            </optgroup>
            <optgroup label="Safety factor">
              <option value="sf">Safety factor — worst case</option>
              <option value="sfm">Safety factor — material σₜ/σᵥᴹ</option>
              <option value="sfz">Safety factor — layer adhesion</option>
            </optgroup>
            <optgroup label="Stress (MPa)">
              {RESULT_FIELDS.filter((f) => f.unit === "MPa").map((f) => (
                <option key={f.value} value={f.value}>
                  {f.label}
                </option>
              ))}
            </optgroup>
            <optgroup label="Strain">
              {RESULT_FIELDS.filter((f) => f.unit === "" && !f.value.startsWith("sf")).map((f) => (
                <option key={f.value} value={f.value}>
                  {f.label}
                </option>
              ))}
            </optgroup>
          </select>
        </div>
      )}

      {resultsView && (
        <button
          className={`playchip${s.animateDeformed ? " on" : ""}`}
          onClick={() => s.setAnimateDeformed(!s.animateDeformed)}
          title="Loop the deflection 0 → max"
        >
          {s.animateDeformed ? "■ Stop" : "▶ Animate"}
        </button>
      )}

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
