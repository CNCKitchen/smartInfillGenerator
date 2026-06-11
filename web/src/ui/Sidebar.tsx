import { useEffect, useRef } from "react";
import { useStore, type ViewMode } from "../store";
import type { Bc, BcKind, PatternKey } from "../types";

const KIND_LABEL: Record<BcKind, string> = {
  fixed: "Fixed support",
  frictionless: "Frictionless support",
  force: "Force",
  pressure: "Pressure",
};

const KIND_DOT: Record<BcKind, string> = {
  fixed: "#3b82f6",
  frictionless: "#22d3ee",
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

  // Leaving the supports & loads workspace snaps the tool back to Orbit so a
  // stray click in the viewport can't silently edit a selection.
  useEffect(() => {
    const onDown = (e: PointerEvent) => {
      const st = useStore.getState();
      if (st.tool === "orbit") return;
      const el = e.target as HTMLElement | null;
      if (!el || el.closest("[data-bcsection]") || el.closest(".viewer")) return;
      st.setTool("orbit");
    };
    document.addEventListener("pointerdown", onDown, true);
    return () => document.removeEventListener("pointerdown", onDown, true);
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
          <header>2 · Supports & loads</header>
          <div className="toolrow">
            <button className={s.tool === "orbit" ? "on" : ""} onClick={() => s.setTool("orbit")}>
              Orbit
            </button>
            <button className={s.tool === "select" ? "on" : ""} onClick={() => s.setTool("select")}>
              Pick surface
            </button>
            <button className={s.tool === "brush" ? "on" : ""} onClick={() => s.setTool("brush")}>
              Brush
            </button>
          </div>
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

          {s.bcs.map((bc) => (
            <BcRow key={bc.id} bc={bc} />
          ))}

          <div className="addrow">
            <button onClick={() => s.addBc("fixed")}>+ Fixed</button>
            <button onClick={() => s.addBc("frictionless")}>+ Slide</button>
            <button onClick={() => s.addBc("force")}>+ Force</button>
            <button onClick={() => s.addBc("pressure")}>+ Pressure</button>
          </div>
          {s.activeBcId && (
            <div className="hint">
              Click surfaces to add to the highlighted condition (click again to remove, or use the
              brush). Shift-click always removes. Clicking outside this section returns to Orbit.
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
          <label className="rowcheck">
            <input type="checkbox" checked={s.gravity} onChange={(e) => s.setGravity(e.target.checked)} />
            <span>Include self-weight (gravity −Z)</span>
          </label>
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
          <label className="row">
            <span>Material budget {s.budget}%</span>
            <input
              type="range"
              min={20}
              max={90}
              step={1}
              value={s.budget}
              onChange={(e) => s.setBudget(Number(e.target.value))}
            />
          </label>
          <label className="row">
            <span>Infill pattern</span>
            <select value={s.pattern} onChange={(e) => s.setPattern(e.target.value as PatternKey)}>
              <option value="gyroid">Gyroid</option>
              <option value="cubic">Cubic</option>
              <option value="grid">Grid</option>
            </select>
          </label>
          <div className="row">
            <label className="row" style={{ flex: 1 }}>
              <span>Perimeters</span>
              <input
                type="number"
                value={s.perimeters}
                step={1}
                min={1}
                max={8}
                onChange={(e) => s.setPerimeters(Number(e.target.value))}
              />
            </label>
            <label className="row">
              <span>Line width</span>
              <input
                type="number"
                value={s.lineWidth}
                step={0.05}
                min={0.1}
                max={1.5}
                onChange={(e) => s.setLineWidth(Number(e.target.value))}
              />
              <span className="dim">mm</span>
            </label>
          </div>
          <div className="row">
            <div className="dim small" style={{ flex: 1 }}>
              ≈ {(s.perimeters * s.lineWidth).toFixed(2)} mm solid skin — match your print profile
            </div>
            <label className="row">
              <span>Levels</span>
              <select value={s.nBins} onChange={(e) => s.setNBins(Number(e.target.value))}>
                <option value={2}>2</option>
                <option value={3}>3</option>
                <option value={4}>4</option>
              </select>
            </label>
          </div>
          <label className="row">
            <span>Region smoothing {s.smoothIters === 0 ? "off" : `${s.smoothIters}×`}</span>
            <input
              type="range"
              min={0}
              max={20}
              step={1}
              value={s.smoothIters}
              onChange={(e) => s.setSmoothIters(Number(e.target.value))}
            />
          </label>
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
            </>
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
  return (
    <div className="card">
      <div className="cardrow big">
        <span>{o.massGrams.toFixed(1)} g</span>
        <span className="dim">of {o.massSolidGrams.toFixed(1)} g solid ({Math.round(o.massFrac * 100)}%)</span>
      </div>
      <div className="cardrow">
        <span>Stiffness vs solid</span>
        <b>{stiff}%</b>
      </div>
      <div className="cardrow">
        <span>vs uniform infill, same mass</span>
        <b>+{gain}% stiffer</b>
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
            : `stopped at the ${o.iterations}-iteration cap`}{" "}
          · {o.seconds.toFixed(1)} s
          {o.effectiveBudget * 100 > s.budget + 1
            ? ` · budget raised to ${Math.round(o.effectiveBudget * 100)}% (walls + minimum infill)`
            : ""}
        </span>
      </div>
      {o.regionCount === 0 && (
        <div className="cardrow small" style={{ color: "#f0c674" }}>
          <span>
            No separate regions: walls + minimum infill already use the whole budget, so the
            interior stays at the base density. Raise the budget to get differentiated zones.
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
              <input
                type="number"
                value={bc.force![i]}
                step={1}
                onChange={(e) => {
                  const f = [...bc.force!] as [number, number, number];
                  f[i] = Number(e.target.value);
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
            <input
              type="number"
              value={bc.pressure}
              step={0.01}
              onChange={(e) => s.updateBcParams(bc.id, { pressure: Number(e.target.value) })}
            />
          </label>
          <span className="dim">MPa</span>
        </div>
      )}
    </div>
  );
}

function fmt(x: number): string {
  return x >= 100 ? x.toFixed(0) : x.toFixed(1);
}
