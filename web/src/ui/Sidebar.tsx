import { useRef } from "react";
import { useStore } from "../store";
import type { Bc, BcKind } from "../types";
import { MATERIALS } from "../types";

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

  return (
    <div className="sidebar">
      <h1>
        Smart Infill <span>Generator</span>
      </h1>

      {/* ---- import ---- */}
      <section>
        <header>1 · Model</header>
        <input
          ref={fileRef}
          type="file"
          accept=".stl"
          hidden
          onChange={(e) => void onFile(e.target.files?.[0])}
        />
        <button className="primary" onClick={pickFile}>
          {s.fileName ? "Replace model…" : "Open STL…"}
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
        <section>
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
              brush). Shift-click always removes.
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
                const m = MATERIALS.find((m) => m.name === e.target.value);
                if (m) s.setMaterial(m);
              }}
            >
              {MATERIALS.map((m) => (
                <option key={m.name}>{m.name}</option>
              ))}
            </select>
          </label>
          <div className="dim small">
            E = {s.material.e0} MPa · ν = {s.material.nu} · ρ = {s.material.density} g/cm³
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

      {/* ---- run ---- */}
      {s.model && (
        <section>
          <header>4 · Run</header>
          <div className="toolrow">
            <button onClick={() => void s.runCheck()} disabled={!!s.busy}>
              Check setup
            </button>
            <button className="primary" onClick={() => void s.runSolve()} disabled={!!s.busy}>
              Solve
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
          {s.stats && s.hasResult && (
            <>
              <div className="status ok">
                Max deflection <b>{s.stats.maxDisplacement.toExponential(2)} mm</b> ·{" "}
                {s.stats.iterations} iters · {s.stats.seconds.toFixed(1)} s
              </div>
              <label className="rowcheck">
                <input
                  type="checkbox"
                  checked={s.showDeformed}
                  onChange={(e) => s.setShowDeformed(e.target.checked)}
                />
                <span>Show deformed shape (color = |u|)</span>
              </label>
              {s.showDeformed && (
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
              )}
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
