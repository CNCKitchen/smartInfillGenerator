// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { useEffect, useRef } from "react";
import { budgetBounds, useStore, type ViewMode } from "../store";
import { NumInput } from "./NumInput";
import { RESULT_FIELDS } from "../types";
import type { Bc, BcKind, PatternKey } from "../types";

const KIND_LABEL: Record<BcKind, string> = {
  fixed: "Fixed support",
  frictionless: "Frictionless support",
  elastic: "Elastic support",
  force: "Force",
  pressure: "Pressure",
};

const KIND_DOT: Record<BcKind, string> = {
  fixed: "#3b82f6",
  frictionless: "#22d3ee",
  elastic: "#34d399",
  force: "#ef4444",
  pressure: "#f59e0b",
};

export function Sidebar() {
  const s = useStore();
  const fileRef = useRef<HTMLInputElement>(null);

  const pickFile = () => fileRef.current?.click();
  const onFile = async (f: File | undefined) => {
    if (!f) return;
    await s.loadFile(f.name, await f.arrayBuffer());
  };

  // Leaving the boundary-conditions workspace (clicking another step) or
  // pressing Esc snaps the tool back to plain orbiting so a stray click in
  // the viewport can't silently edit a selection. Disarm on CLICK (deferred),
  // not pointerdown: disarming re-renders and shifts the layout between
  // press and release, which eats the first click on Check/Solve.
  useEffect(() => {
    const onClick = (e: MouseEvent) => {
      const st = useStore.getState();
      if (st.tool === "orbit") return;
      const el = e.target as HTMLElement | null;
      if (!el || el.closest("[data-bcsection]") || el.closest(".viewer")) return;
      setTimeout(() => useStore.getState().setTool("orbit"), 0);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      const st = useStore.getState();
      if (st.tool !== "orbit") st.setTool("orbit");
    };
    document.addEventListener("click", onClick, true);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("click", onClick, true);
      document.removeEventListener("keydown", onKey);
    };
  }, []);

  return (
    <div className="sidebar">
      <h1>
        <span className="apptitle">
          Smart Infill <span>Generator</span>
        </span>
        <button className="gear" title="Settings — materials & infill curves" onClick={() => s.openSettings(true)}>
          ⚙
        </button>
      </h1>

      {/* ---- import ---- */}
      <section>
        <header>1 · Model</header>
        <input
          ref={fileRef}
          type="file"
          accept=".stl,.3mf"
          hidden
          onChange={(e) => void onFile(e.target.files?.[0])}
        />
        <button className="primary" onClick={pickFile}>
          {s.fileName ? "Replace model…" : "Open STL / 3MF…"}
        </button>
        {s.fileName ? (
          <div className="fileinfo">
            <div>{s.fileName}</div>
            <div className="dim">
              {s.model!.triCount.toLocaleString()} triangles · {s.model!.patchCount} surfaces
            </div>
            <div className="dim">
              {fmt(s.model!.bbox[3] - s.model!.bbox[0])} × {fmt(s.model!.bbox[4] - s.model!.bbox[1])}{" "}
              × {fmt(s.model!.bbox[5] - s.model!.bbox[2])} mm
            </div>
          </div>
        ) : (
          <div className="dim drophint">…or drop a file into the viewport. Units: mm.</div>
        )}
        {s.model && (
          <label className="row">
            <span>Surface detection {s.segAngle}°</span>
            <input
              type="range"
              min={5}
              max={80}
              value={s.segAngle}
              onChange={(e) => void s.setSegAngle(Number(e.target.value))}
            />
          </label>
        )}
      </section>

      {/* ---- supports & loads ---- */}
      {s.model && (
        <section data-bcsection>
          <header>2 · Boundary conditions</header>
          {s.bcs.map((bc) => (
            <BcRow key={bc.id} bc={bc} />
          ))}

          <div className="addrow">
            <button onClick={() => s.addBc("fixed")}>+ Fixed</button>
            <button onClick={() => s.addBc("elastic")}>+ Elastic</button>
            <button onClick={() => s.addBc("frictionless")}>+ Slide</button>
            <button onClick={() => s.addBc("force")}>+ Force</button>
            <button onClick={() => s.addBc("pressure")}>+ Pressure</button>
          </div>

          {s.bcs.length > 0 && (
            <div className="toolrow">
              <button
                className={s.tool === "select" ? "on" : ""}
                onClick={() => s.setTool(s.tool === "select" ? "orbit" : "select")}
              >
                Pick surface
              </button>
              <button
                className={s.tool === "brush" ? "on" : ""}
                onClick={() => s.setTool(s.tool === "brush" ? "orbit" : "brush")}
              >
                Brush
              </button>
            </div>
          )}
          {s.tool === "brush" && (
            <>
              <label className="row">
                <span>Radius {s.brushRadius.toFixed(1)} mm</span>
                <input
                  type="range"
                  min={0.5}
                  max={25}
                  step={0.5}
                  value={s.brushRadius}
                  onChange={(e) => s.setBrushRadius(Number(e.target.value))}
                />
              </label>
              <label className="rowcheck">
                <input
                  type="checkbox"
                  checked={s.brushErase}
                  onChange={(e) => s.setBrushErase(e.target.checked)}
                />
                <span>Erase mode</span>
              </label>
            </>
          )}
          {s.activeBcId && s.tool !== "orbit" && (
            <div className="hint">
              Click surfaces to add to the highlighted condition (click again to remove,
              shift-click always removes). Esc or clicking another step returns to orbiting.
            </div>
          )}
          {s.activeBcId && s.tool === "orbit" && (
            <div className="hint">
              Choose <b>Pick surface</b> or <b>Brush</b> to assign surfaces to the highlighted
              condition. Orbiting is always active.
            </div>
          )}
        </section>
      )}

      {/* ---- physics ---- */}
      {s.model && (
        <section>
          <header>3 · Material & analysis</header>
          <label className="row">
            <span>Material</span>
            <select
              value={s.material.name}
              onChange={(e) => {
                const m = s.materials.find((m) => m.name === e.target.value);
                if (m) s.setMaterial(m);
              }}
            >
              {s.materials.map((m) => (
                <option key={m.name}>{m.name}</option>
              ))}
            </select>
          </label>
          <div className="dim small">
            E = {s.material.e0} MPa · ν = {s.material.nu} · ρ = {s.material.density} g/cm³ —{" "}
            <a className="link" onClick={() => s.openSettings(true)}>
              edit
            </a>
          </div>
          <label className="row">
            <span>Resolution</span>
            <select
              value={s.resolution}
              onChange={(e) => s.setResolution(e.target.value as "preview" | "normal" | "fine")}
            >
              <option value="preview">Preview (fast)</option>
              <option value="normal">Normal</option>
              <option value="fine">Fine</option>
            </select>
          </label>
        </section>
      )}

      {/* ---- check / solve ---- */}
      {s.model && (
        <section>
          <header>4 · Verify setup</header>
          <div className="toolrow">
            <button onClick={() => void s.runCheck()} disabled={!!s.busy}>
              Check setup
            </button>
            <button onClick={() => void s.runSolve()} disabled={!!s.busy}>
              Solve once
            </button>
          </div>
          {s.check && (
            <div className={s.check.ok ? "status ok" : "status bad"}>
              {s.check.ok
                ? `Setup OK — ${s.check.islandCount} body, fully constrained.`
                : s.check.islandCount > 1
                  ? `${s.check.islandCount} disconnected bodies; at least one can still move (animated).`
                  : "Under-constrained — the part can still move (animated). Add supports."}
            </div>
          )}
          {s.stats && s.hasResult && !s.optSummary && (
            <div className="status ok">
              Max deflection <b>{fmtDisp(s.stats.maxDisplacement)}</b> · {s.stats.iterations} iters
              · {s.stats.seconds.toFixed(1)} s
            </div>
          )}
        </section>
      )}

      {/* ---- optimize ---- */}
      {s.model && (
        <section>
          <header>5 · Optimize infill</header>
          <div className="toolrow">
            <button
              className={s.goal === "budget" ? "on" : ""}
              onClick={() => s.setGoal("budget")}
              title="Maximize stiffness at a given material budget"
            >
              Stiffest at budget
            </button>
            <button
              className={s.goal === "match" ? "on" : ""}
              onClick={() => s.setGoal("match")}
              title="Find the lightest design that is as stiff as a uniform print at X%"
            >
              Match uniform stiffness
            </button>
          </div>
          <div className="toolrow">
            <button
              className={s.optMode === "graded" ? "on" : ""}
              onClick={() => s.setOptMode("graded")}
              title="Several discrete infill densities, placed from the optimized field"
            >
              Graded
            </button>
            <button
              className={s.optMode === "binary" ? "on" : ""}
              onClick={() => s.setOptMode("binary")}
              title="Hollow or solid: interior is either the printability floor or 100% dense"
            >
              Binary (hollow/solid)
            </button>
          </div>
          <label className="row">
            <span>
              {s.goal === "match" ? `As stiff as uniform ${s.budget}%` : `Infill budget ${s.budget}%`}
            </span>
            <input
              type="range"
              min={budgetBounds(s)[0]}
              max={budgetBounds(s)[1]}
              step={1}
              value={s.budget}
              onChange={(e) => s.setBudget(Number(e.target.value))}
            />
          </label>
          <div className="dim small">
            {s.goal === "match"
              ? `Finds the LIGHTEST layout with the stiffness of a uniform ${s.budget}% print (a few warm-started passes search the needed budget).`
              : s.optMode === "binary"
                ? `Mean interior density: cells are either ${s.levelSettings.binaryFloorPct}% (so it prints) or 100% solid. The optimizer runs SIMP-penalized so the design goes black/white.`
                : "Mean infill of the interior — same scale as your slicer's uniform infill %. Walls and shells come on top."}
          </div>
          <label className="row">
            <span>Infill pattern</span>
            <select value={s.pattern} onChange={(e) => s.setPattern(e.target.value as PatternKey)}>
              <option value="gyroid">Gyroid</option>
              <option value="cubic">Cubic</option>
              <option value="grid">Grid</option>
            </select>
          </label>
          {s.optMode === "binary" && (
            <label className="row">
              <span>Solid fill</span>
              <select
                value={s.solidPattern}
                onChange={(e) =>
                  s.setSolidPattern(e.target.value as "default" | "rectilinear" | "concentric")
                }
              >
                <option value="default">Profile default</option>
                <option value="rectilinear">Rectilinear</option>
                <option value="concentric">Concentric</option>
              </select>
            </label>
          )}
          <div className="row">
            <label className="row" style={{ flex: 1 }}>
              <span>Perimeters</span>
              <NumInput
                value={s.perimeters}
                step={1}
                min={1}
                max={8}
                onCommit={(v) => s.setPerimeters(v)}
              />
            </label>
            <label className="row">
              <span>Line width</span>
              <NumInput
                value={s.lineWidth}
                step={0.05}
                min={0.1}
                max={1.5}
                onCommit={(v) => s.setLineWidth(v)}
              />
              <span className="dim">mm</span>
            </label>
          </div>
          <div className="row">
            <div className="dim small" style={{ flex: 1 }}>
              ≈ {(s.perimeters * s.lineWidth).toFixed(2)} mm solid skin — perimeters go into the
              3MF; match the line width to your profile
            </div>
            {s.optMode === "binary" ? (
              <span className="dim small">2 levels (hollow/solid)</span>
            ) : s.levelSettings.mode === "manual" ? (
              <span className="dim small" title="Manual levels — change in ⚙ Settings">
                levels {s.levelSettings.manual.join("/")}%
              </span>
            ) : (
              <label className="row">
                <span>Levels</span>
                <select value={s.nBins} onChange={(e) => s.setNBins(Number(e.target.value))}>
                  <option value={2}>2</option>
                  <option value={3}>3</option>
                  <option value={4}>4</option>
                </select>
              </label>
            )}
          </div>
          <label className="row">
            <span>Region smoothing {s.smoothIters === 0 ? "off" : `${s.smoothIters}×`}</span>
            <input
              type="range"
              min={0}
              max={40}
              step={1}
              value={s.smoothIters}
              onChange={(e) => s.setSmoothIters(Number(e.target.value))}
            />
          </label>
          {s.optSummary && (
            <div className="dim small">
              Smoothing updates the regions live — check the Regions view; exports use what you
              see.
            </div>
          )}
          <button className="primary" onClick={() => void s.runOptimize()} disabled={!!s.busy}>
            Optimize infill
          </button>
          {s.optProgress && (
            <div className="progress">
              <div
                className="bar"
                style={{ width: `${(100 * s.optProgress.iteration) / s.optProgress.maxIter}%` }}
              />
              <span>
                {(s.optProgress.passes ?? 1) > 1
                  ? `pass ${s.optProgress.pass}/${s.optProgress.passes} · `
                  : ""}
                iteration {s.optProgress.iteration}/{s.optProgress.maxIter}
              </span>
            </div>
          )}
          {s.optSummary && <SummaryCard />}
        </section>
      )}

      {/* ---- view + export ---- */}
      {s.model && (
        <section>
          <header>6 · View & export</header>
          <div className="toolrow">
            <ViewBtn mode="setup" label="Setup" />
            <ViewBtn mode="mesh" label="Mesh" />
            {s.hasResult && <ViewBtn mode="deformed" label="Deformed" />}
            {s.optSummary && <ViewBtn mode="density" label="Density" />}
            {s.optSummary && <ViewBtn mode="infill" label="Regions" />}
          </div>
          {s.viewMode === "mesh" && (
            <div className="dim small">
              The hex mesh the solver actually runs on (winding-number voxelization at the chosen
              resolution).
            </div>
          )}
          {s.viewMode === "density" && s.optSummary && (
            <>
              <label className="row">
                <span>
                  {s.densityThreshold >= 10 ? `Cutaway ρ ≥ ${s.densityThreshold}%` : "Cutaway off"}
                </span>
                <input
                  type="range"
                  min={0}
                  max={70}
                  step={1}
                  value={s.densityThreshold}
                  onChange={(e) => s.setDensityThreshold(Number(e.target.value))}
                />
              </label>
              <div className="dim small">
                Shows only material denser than the threshold — look inside the part instead of
                just its painted surface.
              </div>
            </>
          )}
          {s.viewMode === "infill" && s.regionInfos.length > 0 && (
            <div className="regionlist">
              {s.regionInfos.map((r, i) => (
                <label key={i} className="regionrow">
                  <input
                    type="checkbox"
                    checked={s.regionVisible[i] !== false}
                    onChange={(e) => s.setRegionVisible(i, e.target.checked)}
                  />
                  <span className="dot" style={{ background: rampCss(r.density / 0.8) }} />
                  <span>
                    Modifier {i + 1} — infill {Math.round(r.density * 100)}%
                  </span>
                </label>
              ))}
              <div className="dim small">
                Regions nest (denser inside sparser) — toggle to inspect one at a time.
              </div>
            </div>
          )}
          {s.viewMode === "deformed" && (
            <>
              <label className="row">
                <span>Result field</span>
                <select
                  value={s.resultField}
                  onChange={(e) => void s.setResultField(e.target.value)}
                >
                  <option value="u">Displacement |u|</option>
                  <option value="sf">Safety factor σₜ/σᵥᴹ</option>
                  <optgroup label="Stress (MPa)">
                    {RESULT_FIELDS.filter((f) => f.unit === "MPa").map((f) => (
                      <option key={f.value} value={f.value}>
                        {f.label}
                      </option>
                    ))}
                  </optgroup>
                  <optgroup label="Strain">
                    {RESULT_FIELDS.filter((f) => f.unit === "" && f.value !== "sf").map((f) => (
                      <option key={f.value} value={f.value}>
                        {f.label}
                      </option>
                    ))}
                  </optgroup>
                </select>
              </label>
              {s.resultField === "sf" && (
                <div className="dim small">
                  σₜ from the material table; the allowable of graded infill scales with the same
                  E(ρ) law as its stiffness (first-order). Red marks the lowest factor — enable
                  "Mark min/max" to pin it.
                </div>
              )}
              <label className="row">
                <span>Exaggeration ×{s.deformScale.toFixed(1)}</span>
                <input
                  type="range"
                  min={0}
                  max={3}
                  step={0.1}
                  value={s.deformScale}
                  onChange={(e) => s.setDeformScale(Number(e.target.value))}
                />
              </label>
              <label className="rowcheck">
                <input
                  type="checkbox"
                  checked={s.animateDeformed}
                  onChange={(e) => s.setAnimateDeformed(e.target.checked)}
                />
                <span>Animate deflection (loop 0 → max)</span>
              </label>
              <label className="rowcheck">
                <input
                  type="checkbox"
                  checked={s.showExtremes}
                  onChange={(e) => s.setShowExtremes(e.target.checked)}
                />
                <span>Mark min / max locations</span>
              </label>
              {s.resultField !== "u" && (
                <div className="dim small">
                  Cell-center values mapped to the surface — stair-step concentrations at the
                  voxel boundary are approximate. Click the legend numbers to set a custom scale.
                </div>
              )}
            </>
          )}
          <div className="toolrow">
            <button className={s.sectionOn ? "on" : ""} onClick={() => s.toggleSection()}>
              Section plane
            </button>
            {s.sectionOn && (
              <>
                <button onClick={() => s.flipSection()}>Flip</button>
                <button onClick={() => s.setSectionAxis("x")}>X</button>
                <button onClick={() => s.setSectionAxis("y")}>Y</button>
                <button onClick={() => s.setSectionAxis("z")}>Z</button>
              </>
            )}
          </div>
          {s.sectionOn && (
            <div className="dim small">
              Drag the arrow to slide the plane along its normal, the rings to tilt it. X/Y/Z
              aligns the normal; Flip keeps the other half.
            </div>
          )}
          {s.optSummary && (
            <>
              <button className="primary" onClick={() => void s.downloadThreeMf()}>
                Download OrcaSlicer project (.3mf)
              </button>
              <button onClick={() => void s.downloadStls()}>
                Download modifier STLs (.zip)
              </button>
              <div className="hint">
                The 3MF opens in OrcaSlicer/Bambu Studio with the part, the modifier volumes, and
                their infill densities already set (base infill{" "}
                {Math.round(s.optSummary.baseDensity * 100)}% on the object). Only densities are
                overridden — walls, shells, and everything else come from your own profiles.
              </div>
            </>
          )}
        </section>
      )}

      <footer className="dim small">
        Static linear analysis on a voxel grid · all computation stays in your browser.
      </footer>
    </div>
  );
}

function ViewBtn({ mode, label }: { mode: ViewMode; label: string }) {
  const s = useStore();
  return (
    <button className={s.viewMode === mode ? "on" : ""} onClick={() => void s.setViewMode(mode)}>
      {label}
    </button>
  );
}

function SummaryCard() {
  const s = useStore();
  const o = s.optSummary!;
  const stiff = Math.round(o.stiffnessVsSolid * 100);
  const gain = (o.gainVsUniform * 100).toFixed(1);
  const uniformPct = Math.round(o.meanInfill * 100);
  const isMatch = o.goal === "match" && o.massUniformRefGrams != null;
  const saved = isMatch ? 1 - o.massGrams / o.massUniformRefGrams! : 0;
  return (
    <div className="card">
      <div className="cardrow big">
        <span>{o.massGrams.toFixed(1)} g</span>
        <span className="dim">of {o.massSolidGrams.toFixed(1)} g solid ({Math.round(o.massFrac * 100)}%)</span>
      </div>
      {isMatch && (
        <div className="cardrow">
          <span>vs {Math.round(o.refUniformPct!)}% uniform (same stiffness)</span>
          <b>
            −{(saved * 100).toFixed(0)}% weight ({o.massUniformRefGrams!.toFixed(1)} g → {o.massGrams.toFixed(1)} g)
          </b>
        </div>
      )}
      {isMatch && Math.abs(o.matchDeviation ?? 0) > 0.02 && (
        <div className="cardrow small" style={{ color: "#f0c674" }}>
          <span>
            stiffness {((o.matchDeviation ?? 0) * 100).toFixed(1)}% off the target (search hit its
            pass limit) — re-run or adjust the reference
          </span>
        </div>
      )}
      <div className="cardrow">
        <span>vs {uniformPct}% uniform infill (same weight)</span>
        <b>+{gain}% stiffer</b>
      </div>
      <div className="cardrow">
        <span>Stiffness vs 100% solid</span>
        <b>{stiff}%</b>
      </div>
      <div className="cardrow">
        <span>Max deflection</span>
        <b>{fmtDisp(o.maxDisplacement)}</b>
      </div>
      <div className="cardrow">
        <span>Infill levels</span>
        <span>
          {o.bins.map((b) => `${Math.round(b.density * 100)}%`).join(" · ")}
        </span>
      </div>
      <div className="cardrow dim small">
        <span>
          {o.converged
            ? `converged in ${o.iterations} iterations`
            : `stopped at the ${o.iterations}-iteration cap`}
          {o.passes > 1 ? ` over ${o.passes} passes` : ""} · {o.seconds.toFixed(1)} s
          {Math.abs(o.targetInfill * 100 - s.budget) > 0.5
            ? ` · target clamped to ${Math.round(o.targetInfill * 100)}% (printable range)`
            : ""}
        </span>
      </div>
      {o.regionCount === 0 && (
        <div className="cardrow small" style={{ color: "#f0c674" }}>
          <span>
            No separate regions: the whole interior ended at one density level. Raise the infill
            budget (or the number of levels) to get differentiated zones.
          </span>
        </div>
      )}
    </div>
  );
}

function fmtDisp(mm: number): string {
  if (mm >= 0.01) return `${mm.toFixed(2)} mm`;
  return `${(mm * 1000).toFixed(1)} µm`;
}

/** Mirrors the viewer's region color ramp for the legend dots. */
function rampCss(x: number): string {
  const t = Math.min(1, Math.max(0, x));
  let r: number, g: number, b: number;
  if (t < 0.33) {
    r = 0.15; g = 0.3 + 1.8 * t; b = 0.9;
  } else if (t < 0.66) {
    r = 0.15 + 2.4 * (t - 0.33); g = 0.9; b = 0.9 - 2.4 * (t - 0.33);
  } else {
    r = 0.95; g = 0.9 - 2.4 * (t - 0.66); b = 0.1;
  }
  const c = (v: number) => Math.round(255 * Math.min(1, Math.max(0, v)));
  return `rgb(${c(r)},${c(g)},${c(b)})`;
}

function BcRow({ bc }: { bc: Bc }) {
  const s = useStore();
  const active = s.activeBcId === bc.id;
  return (
    <div className={active ? "bc active" : "bc"} onClick={() => s.setActiveBc(active ? null : bc.id)}>
      <div className="bchead">
        <span className="dot" style={{ background: KIND_DOT[bc.kind] }} />
        <span className="bcname">{KIND_LABEL[bc.kind]}</span>
        <span className="dim">{bc.tris.length ? `${bc.tris.length} tris` : "select surfaces…"}</span>
        <button
          className="x"
          onClick={(e) => {
            e.stopPropagation();
            s.removeBc(bc.id);
          }}
        >
          ×
        </button>
      </div>
      {bc.kind === "force" && bc.force && (
        <div className="bcparams" onClick={(e) => e.stopPropagation()}>
          {(["X", "Y", "Z"] as const).map((axis, i) => (
            <label key={axis}>
              F{axis}
              <NumInput
                value={bc.force![i]}
                step={1}
                onCommit={(v) => {
                  const f = [...bc.force!] as [number, number, number];
                  f[i] = v;
                  s.updateBcParams(bc.id, { force: f });
                }}
              />
            </label>
          ))}
          <span className="dim">N total</span>
        </div>
      )}
      {bc.kind === "pressure" && (
        <div className="bcparams" onClick={(e) => e.stopPropagation()}>
          <label>
            p
            <NumInput
              value={bc.pressure ?? 0}
              step={0.01}
              onCommit={(v) => s.updateBcParams(bc.id, { pressure: v })}
            />
          </label>
          <span className="dim">MPa</span>
        </div>
      )}
      {bc.kind === "elastic" && (
        <div onClick={(e) => e.stopPropagation()}>
          <div className="bcparams">
            <label>
              k
              <NumInput
                value={bc.stiffness ?? 100}
                step={10}
                min={0.01}
                onCommit={(v) => s.updateBcParams(bc.id, { stiffness: Math.max(0.01, v) })}
              />
            </label>
            <span className="dim">N/mm³ foundation stiffness</span>
          </div>
          <div className="dim small">
            σ = k·u at the surface (k ≈ E/t of what's underneath): foam ~0.1, 3 mm rubber pad
            ~2, printed-plastic mount ~50–500, bolted to steel ≥ 5000 (≈ fixed).
          </div>
        </div>
      )}
    </div>
  );
}

function fmt(x: number): string {
  return x >= 100 ? x.toFixed(0) : x.toFixed(1);
}
