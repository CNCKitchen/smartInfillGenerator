// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Floating viewport controls, CAD-style: display modes top center, the
// result-field picker right beneath them, deflection playback bottom center,
// the section plane bottom left. Result review happens ON the result.

import { useShallow } from "zustand/shallow";
import { useStore, resultStale, type ViewMode, type ResultKind } from "../store";
import { RESULT_FIELDS } from "../types";
import { format } from "../units";

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
      appMode: s.appMode,
      buildState: s.buildState,
      setBuildState: s.setBuildState,
      buildResult: s.buildResult,
      results: s.results,
      activeResultId: s.activeResultId,
      resultEpochs: s.resultEpochs,
      selectResult: s.selectResult,
      wireframe: s.wireframe,
      setWireframe: s.setWireframe,
      smoothShading: s.smoothShading,
      setSmoothShading: s.setSmoothShading,
      featureEdges: s.featureEdges,
      setFeatureEdges: s.setFeatureEdges,
      cadColors: s.cadColors,
      setCadColors: s.setCadColors,
      hasCadColors: !!s.stepInfo?.cadTriColors,
      strainView: s.strainView,
      setStrainView: s.setStrainView,
      strainLayer: s.strainLayer,
      strainLayerMax: s.strainLayerMax,
      strainPeakMPa: s.strainPeakMPa,
      setStrainLayer: s.setStrainLayer,
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
      unitRev: s.unitRev,
    }))
  );
  if (!s.model) return null;
  const resultsView = s.viewMode === "deformed" && s.hasResult;
  // Modal result active: stress/strain are RELATIVE (a mass-normalized mode has
  // arbitrary magnitude), and safety factor is meaningless — so the field picker
  // keeps displacement/stress/strain but drops SF and labels them "relative".
  const activeModal = s.results.find((r) => r.id === s.activeResultId)?.kind === "modal";
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
          <button
            className={s.featureEdges ? "on" : ""}
            onClick={() => s.setFeatureEdges(!s.featureEdges)}
            title="Outline the part's feature edges (CAD face borders on STEP, crease borders on STL)"
          >
            Edges
          </button>
          <button
            className={s.smoothShading ? "on" : ""}
            onClick={() => s.setSmoothShading(!s.smoothShading)}
            title="Smooth-shade curved surfaces (crease-aware) instead of flat facets"
          >
            Smooth
          </button>
          {s.hasCadColors && (
            <button
              className={s.cadColors ? "on" : ""}
              onClick={() => s.setCadColors(!s.cadColors)}
              title="Show the CAD file's face colors on the part"
            >
              Colors
            </button>
          )}
          {/* Build Sim: inherent-strain layer view — scrub the build height and
              color cells by their per-element strain source. */}
          {s.appMode === "buildsim" && s.viewMode === "mesh" && (
            <button
              className={s.strainView ? "on" : ""}
              onClick={() => void s.setStrainView(!s.strainView)}
              title="Color cells by the inherent-strain source the build sim applies (∝ density), and scrub the build layers"
            >
              Inherent strain
            </button>
          )}
        </div>
      )}

      {s.viewMode === "mesh" && s.strainView && s.strainLayerMax > 0 && (
        <div className="fieldchip strainscrub">
          <span title="Show the part built up to this voxel layer">
            Layer <b>{Math.min(s.strainLayer || s.strainLayerMax, s.strainLayerMax)}</b> /{" "}
            {s.strainLayerMax}
          </span>
          <input
            type="range"
            min={1}
            max={s.strainLayerMax}
            value={Math.min(s.strainLayer || s.strainLayerMax, s.strainLayerMax)}
            onChange={(e) => void s.setStrainLayer(Number(e.target.value))}
          />
          <span className="chipdiv" />
          <span title="Peak per-element inherent-strain source (uncalibrated)">
            peak <b>{format(s.strainPeakMPa, "stress")}</b>
          </span>
        </div>
      )}

      {resultsView && (
        <div className="fieldchip">
          {/* Build Sim: the on-bed ⇄ released switch lives where the load-case
              selector sits in the analysis workspace. Both states are cached,
              so this is an instant re-map (no re-solve). */}
          {s.appMode === "buildsim" && s.buildResult && (
            <>
              <div className="seg" title="Switch the warped shape between held on the build plate and released (sprung) — the predeform target">
                <button
                  className={s.buildState === "bonded" ? "on" : ""}
                  onClick={() => void s.setBuildState("bonded")}
                >
                  On bed
                </button>
                <button
                  className={s.buildState === "released" ? "on" : ""}
                  onClick={() => void s.setBuildState("released")}
                >
                  Released
                </button>
              </div>
              <span className="chipdiv" />
            </>
          )}
          {s.results.length > 0 && (() => {
            // Results are pre-sorted (kind order, then load-step order). Collapse
            // to DISTINCT kinds for the first dropdown; the second dropdown steps
            // through the active kind's load cases (hidden when there's one).
            const activeEntry = s.results.find((r) => r.id === s.activeResultId);
            const activeKind = activeEntry?.kind;
            const kinds: ResultKind[] = [];
            for (const r of s.results) if (!kinds.includes(r.kind)) kinds.push(r.kind);
            const stepsForKind = s.results.filter((r) => r.kind === activeKind);
            const kindLabel = (k: ResultKind) => s.results.find((r) => r.kind === k)?.label ?? k;
            const kindStale = (k: ResultKind) =>
              s.results.some((r) => r.kind === k && resultStale(r, s.resultEpochs));
            const pickKind = (k: ResultKind) => {
              const same = s.results.find(
                (r) => r.kind === k && r.loadStepId === activeEntry?.loadStepId
              );
              const target = same ?? s.results.find((r) => r.kind === k);
              if (target) void s.selectResult(target.id);
            };
            return (
              <>
                <select
                  className="resultsel"
                  value={activeKind ?? ""}
                  onChange={(e) => pickKind(e.target.value as ResultKind)}
                  title="Which result to view — optimized, the even-fill baselines, or the as-printed solve"
                >
                  {kinds.map((k) => (
                    <option key={k} value={k}>
                      {kindStale(k) ? "⚠ " : ""}
                      {kindLabel(k)}
                    </option>
                  ))}
                </select>
                {stepsForKind.length > 1 && (
                  <select
                    className="resultsel stepsel"
                    value={s.activeResultId ?? ""}
                    onChange={(e) => void s.selectResult(e.target.value)}
                    title="Step through the load cases — the color scale stays fixed so they compare directly (use Fit to rescale)"
                  >
                    {stepsForKind.map((r) => (
                      <option key={r.id} value={r.id}>
                        {resultStale(r, s.resultEpochs) ? "⚠ " : ""}
                        {r.loadStepName}
                      </option>
                    ))}
                  </select>
                )}
                <span className="chipdiv" />
              </>
            );
          })()}
          {/* Build sim runs on its own coarse grid (not the analysis voxels),
              so it only offers the smooth-STL result surface. */}
          {s.appMode !== "buildsim" && (
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
          )}
          {s.appMode === "buildsim" ? (
            // Build sim has no stress/SF solve — only the warp and the bed-peel
            // reaction fields make sense here.
            <select
              value={s.resultField}
              onChange={(e) => void s.setResultField(e.target.value)}
              title="Field plotted on the warped shape — displacement, or the bed-peel reaction (uncalibrated relative indicator)"
            >
              <option value="u">Displacement |u|</option>
              <optgroup label="Displacement (mm)">
                {RESULT_FIELDS.filter((f) => ["ux", "uy", "uz"].includes(f.value)).map((f) => (
                  <option key={f.value} value={f.value}>
                    {f.label}
                  </option>
                ))}
              </optgroup>
              <optgroup label="Residual print stress (MPa)">
                <option value="vm">von Mises</option>
                <option value="szz">σzz — interlayer tension (delamination)</option>
                <option value="sxx">σxx</option>
                <option value="syy">σyy</option>
              </optgroup>
              <optgroup label="Bed peel (MPa · relative)">
                <option value="peel">Peel traction (+Z)</option>
                <option value="peelshear">Bed shear traction</option>
              </optgroup>
            </select>
          ) : activeModal ? (
            <select
              value={s.resultField}
              onChange={(e) => void s.setResultField(e.target.value)}
              title="Field on the mode shape. Stress/strain show the relative PATTERN (where it concentrates) — a mode is mass-normalized, so the magnitudes are arbitrary, not calibrated."
            >
              <option value="u">Displacement |u| (mode shape)</option>
              <optgroup label="Displacement (mode shape)">
                {RESULT_FIELDS.filter((f) => ["ux", "uy", "uz"].includes(f.value)).map((f) => (
                  <option key={f.value} value={f.value}>
                    {f.label}
                  </option>
                ))}
              </optgroup>
              <optgroup label="Stress (relative — pattern only)">
                {RESULT_FIELDS.filter((f) => f.unit === "MPa").map((f) => (
                  <option key={f.value} value={f.value}>
                    {f.label}
                  </option>
                ))}
              </optgroup>
              <optgroup label="Strain (relative — pattern only)">
                {RESULT_FIELDS.filter((f) => f.unit === "" && !f.value.startsWith("sf")).map((f) => (
                  <option key={f.value} value={f.value}>
                    {f.label}
                  </option>
                ))}
              </optgroup>
            </select>
          ) : (
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
          )}
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
