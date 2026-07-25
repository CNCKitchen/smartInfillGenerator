// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Optimize Orientation (DESIGN §15): section for station 5 · Optimize.
//! One button sweeps the ±90° rotation-X/rotation-Y hemisphere for the worst
//! layer-adhesion safety factor per orientation (single solve — the stress
//! field is orientation-independent; only the layer criterion moves).
//! The heatmap colors by the SCORED value (constraint ring excluded); the
//! readout shows both scored and hide-nothing minima plus the
//! orientation-independent material floor, so nothing is silently hidden.
//! Click or DRAG on the map (or step the rotation inputs) for a display-only
//! preview: the part rotates to that build direction, undeformed, colored by
//! its per-vertex layer SF.

import { useEffect, useRef, useState } from "react";
import { useShallow } from "zustand/shallow";
import { useStore } from "../store";
import { jet } from "../viewer/colormaps";
import { OPT_HELP } from "./helptext";
import { NumInput } from "./NumInput";
import { Section } from "./Section";

/** Canvas display size (px) — the n×n grid upscales with pixelated sampling. */
const MAP_SIZE = 222;

export function ValidateOrientation() {
  const s = useStore(
    useShallow((s) => ({
      orientSweep: s.orientSweep,
      orientProgress: s.orientProgress,
      orientSel: s.orientSel,
      results: s.results,
      layerShear: s.layerShear,
      setLayerShear: s.setLayerShear,
      runOrientationSweep: s.runOrientationSweep,
      selectOrientation: s.selectOrientation,
      clearOrientationPreview: s.clearOrientationPreview,
    }))
  );
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const dragging = useRef(false);
  const sw = s.orientSweep;

  // Paint the heatmap at native n×n; CSS upscales with hard pixel edges.
  useEffect(() => {
    const cv = canvasRef.current;
    if (!cv || !sw) return;
    const n = sw.n;
    cv.width = n;
    cv.height = n;
    const ctx = cv.getContext("2d");
    if (!ctx) return;
    let lo = Infinity;
    let hi = -Infinity;
    for (const v of sw.scored) {
      if (v < lo) lo = v;
      if (v > hi) hi = v;
    }
    if (!(hi > lo)) hi = lo + 1;
    const img = ctx.createImageData(n, n);
    for (let ip = 0; ip < n; ip++) {
      for (let ir = 0; ir < n; ir++) {
        const t = (sw.scored[ip * n + ir] - lo) / (hi - lo);
        const [r, g, b] = jet(1 - t); // SF convention: red = critical LOW
        // Canvas y grows downward; draw rotation X +90° at the top.
        const px = 4 * ((n - 1 - ip) * n + ir);
        img.data[px] = Math.round(255 * r);
        img.data[px + 1] = Math.round(255 * g);
        img.data[px + 2] = Math.round(255 * b);
        img.data[px + 3] = 255;
      }
    }
    ctx.putImageData(img, 0, 0);
  }, [sw]);

  const canRun = s.results.some((r) => r.kind !== "modal") && !s.orientProgress;

  // Best pixel + readout for the selected (or center = current) pixel.
  let best: { ip: number; ir: number; sf: number } | null = null;
  let readout: { rotX: number; rotY: number; scored: number; all: number; isCurrent: boolean } | null =
    null;
  if (sw) {
    const n = sw.n;
    // Best = the orientation whose WORST cell is safest (max of the minima).
    let bi = 0;
    for (let i = 1; i < sw.scored.length; i++) if (sw.scored[i] > sw.scored[bi]) bi = i;
    best = { ip: Math.floor(bi / n), ir: bi % n, sf: sw.scored[bi] };
    const center = (n - 1) / 2;
    const sel = s.orientSel ?? { ip: center, ir: center };
    readout = {
      rotX: -90 + sel.ip * sw.stepDeg,
      rotY: -90 + sel.ir * sw.stepDeg,
      scored: sw.scored[sel.ip * n + sel.ir],
      all: sw.all[sel.ip * n + sel.ir],
      isCurrent: sel.ip === center && sel.ir === center,
    };
  }

  /** Map a pointer event on the (upscaled) canvas to grid indices. */
  const pixelAt = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (!sw) return null;
    const rect = e.currentTarget.getBoundingClientRect();
    const n = sw.n;
    const ir = Math.min(n - 1, Math.max(0, Math.floor(((e.clientX - rect.left) / rect.width) * n)));
    const row = Math.min(n - 1, Math.max(0, Math.floor(((e.clientY - rect.top) / rect.height) * n)));
    return { ip: n - 1 - row, ir }; // top row = rotation X +90°
  };

  const pick = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const p = pixelAt(e);
    if (!p) return;
    const cur = useStore.getState().orientSel;
    if (cur && cur.ip === p.ip && cur.ir === p.ir) return;
    void s.selectOrientation(p.ip, p.ir);
  };

  /** Grid indices → CSS position (center of the pixel) on the upscaled map. */
  const markerPos = (ip: number, ir: number) => {
    const n = sw!.n;
    return {
      left: `${(100 * (ir + 0.5)) / n}%`,
      top: `${(100 * (n - 1 - ip + 0.5)) / n}%`,
    };
  };

  /** Rotation input commit: snap the angle to the grid and select the pixel. */
  const commitAngle = (axis: "x" | "y") => (v: number) => {
    if (!sw) return;
    const n = sw.n;
    const idx = Math.min(n - 1, Math.max(0, Math.round((v + 90) / sw.stepDeg)));
    const center = (n - 1) / 2;
    const sel = s.orientSel ?? { ip: center, ir: center };
    void s.selectOrientation(axis === "x" ? idx : sel.ip, axis === "y" ? idx : sel.ir);
  };

  return (
    <Section
      title="Optimize orientation"
      help={OPT_HELP.orientation}
      badge={sw && best ? `best ${best.sf.toFixed(2)}×` : undefined}
    >
      <label
        className="rowcheck"
        title="Layers also fail by sliding along the layer plane (τ vs τᶻ, the interaction criterion). Off = pure tension across the layers — affects the sfz/sf results too, and deletes the current map."
      >
        <input
          type="checkbox"
          checked={s.layerShear}
          onChange={(e) => s.setLayerShear(e.target.checked)}
        />
        <span>Include interlayer shear (τᶻ)</span>
      </label>
      <div className="toolrow">
        <button disabled={!canRun} onClick={() => void s.runOrientationSweep()}>
          {sw ? "Re-sweep" : "Sweep orientations"}
        </button>
        {s.orientSel && <button onClick={() => s.clearOrientationPreview()}>Exit preview</button>}
      </div>
      {s.orientProgress && (
        <div className="progress">
          <div
            className="bar"
            style={{
              width: `${Math.round((100 * s.orientProgress.done) / Math.max(1, s.orientProgress.total))}%`,
            }}
          />
          <span>
            sweeping {s.orientProgress.done}/{s.orientProgress.total} orientations
          </span>
        </div>
      )}
      {sw && readout && best && (
        <>
          <div
            style={{
              position: "relative",
              width: MAP_SIZE,
              height: MAP_SIZE,
              margin: "6px auto 2px",
            }}
          >
            <canvas
              ref={canvasRef}
              style={{
                width: "100%",
                height: "100%",
                imageRendering: "pixelated",
                border: "1px solid rgba(0,0,0,0.35)",
                cursor: "crosshair",
                display: "block",
                touchAction: "none",
              }}
              onPointerDown={(e) => {
                dragging.current = true;
                e.currentTarget.setPointerCapture(e.pointerId);
                pick(e);
              }}
              onPointerMove={(e) => {
                if (dragging.current) pick(e);
              }}
              onPointerUp={(e) => {
                dragging.current = false;
                e.currentTarget.releasePointerCapture(e.pointerId);
              }}
            />
            <MapMarker pos={markerPos((sw.n - 1) / 2, (sw.n - 1) / 2)} kind="current" />
            <MapMarker pos={markerPos(best.ip, best.ir)} kind="best" />
            {s.orientSel && <MapMarker pos={markerPos(s.orientSel.ip, s.orientSel.ir)} kind="sel" />}
          </div>
          <div className="toolrow" style={{ justifyContent: "center" }}>
            <button onClick={() => void s.selectOrientation(best.ip, best.ir)}>Best</button>
            <button onClick={() => void s.selectOrientation((sw.n - 1) / 2, (sw.n - 1) / 2)}>
              Current
            </button>
          </div>
          <div className="duo">
            <label style={{ display: "flex", alignItems: "center", gap: 10 }}>
              <span className="dim small" style={{ whiteSpace: "nowrap" }}>
                Rotation X (°)
              </span>
              <NumInput
                value={readout.rotX}
                min={-90}
                max={90}
                step={sw.stepDeg}
                onCommit={commitAngle("x")}
              />
            </label>
            <label style={{ display: "flex", alignItems: "center", gap: 10 }}>
              <span className="dim small" style={{ whiteSpace: "nowrap" }}>
                Rotation Y (°)
              </span>
              <NumInput
                value={readout.rotY}
                min={-90}
                max={90}
                step={sw.stepDeg}
                onCommit={commitAngle("y")}
              />
            </label>
          </div>
          <div className="kv">
            <span>{readout.isCurrent ? "As oriented" : "Previewing"}</span>
            <b>
              rot X {readout.rotX}° / rot Y {readout.rotY}°
            </b>
          </div>
          <div className="kv">
            <span>Layer SF (scored)</span>
            <b>{readout.scored.toFixed(2)}×</b>
          </div>
          <div className="kv">
            <span>Layer SF (all cells)</span>
            <b>{readout.all.toFixed(2)}×</b>
          </div>
          <div className="kv">
            <span>Material SF (any orientation)</span>
            <b>{sw.materialSfMin.toFixed(2)}×</b>
          </div>
          <div className="kv">
            <span>Best orientation</span>
            <b>
              {-90 + best.ip * sw.stepDeg}° / {-90 + best.ir * sw.stepDeg}° · {best.sf.toFixed(2)}×
            </b>
          </div>
          {sw.materialSfMin <= best.sf && (
            <div className="dim small">
              The material limit governs everywhere — orientation cannot raise the overall safety
              factor above {sw.materialSfMin.toFixed(2)}×.
            </div>
          )}
        </>
      )}
    </Section>
  );
}

/** Small positioned marker on the heatmap (crosshair / best / selection). */
function MapMarker({
  pos,
  kind,
}: {
  pos: { left: string; top: string };
  kind: "current" | "best" | "sel";
}) {
  const style: React.CSSProperties = {
    position: "absolute",
    ...pos,
    transform: "translate(-50%, -50%)",
    pointerEvents: "none",
    fontSize: kind === "sel" ? 16 : 13,
    lineHeight: 1,
    color: "#111",
    textShadow: "0 0 2px #fff, 0 0 3px #fff",
  };
  return <span style={style}>{kind === "current" ? "＋" : kind === "best" ? "◎" : "◉"}</span>;
}
