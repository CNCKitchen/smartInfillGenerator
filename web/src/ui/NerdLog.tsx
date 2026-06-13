// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// "Log for nerds": a bottom drawer streaming solver/optimizer telemetry with
// small inline convergence charts. No charting dependency on purpose — a
// ~60-line SVG polyline does everything needed here (license policy: keep the
// core free of new third-party code).

import { useEffect, useRef } from "react";
import { useStore } from "../store";

interface Series {
  ys: number[];
  color: string;
  label?: string;
}

function MiniChart({
  title,
  series,
  xs,
  logY,
  threshold,
  yFmt = (v) => v.toPrecision(2),
}: {
  title: string;
  series: Series[];
  /** X value per sample (defaults to the index). */
  xs?: number[];
  logY?: boolean;
  /** Horizontal dashed marker (e.g. a convergence threshold). */
  threshold?: number;
  yFmt?: (v: number) => string;
}) {
  const W = 252;
  const H = 116;
  const PAD = { l: 6, r: 6, t: 17, b: 6 };

  const n = Math.max(...series.map((s) => s.ys.length), 0);
  const tf = (v: number) => (logY ? Math.log10(Math.max(v, 1e-30)) : v);
  const vals: number[] = [];
  for (const s of series) for (const y of s.ys) if (Number.isFinite(y) && (!logY || y > 0)) vals.push(tf(y));
  if (threshold !== undefined) vals.push(tf(threshold));

  let body: React.ReactNode = (
    <text x={W / 2} y={H / 2 + 8} textAnchor="middle" className="nl-empty">
      —
    </text>
  );
  if (n >= 2 && vals.length >= 2) {
    let lo = Math.min(...vals);
    let hi = Math.max(...vals);
    if (hi - lo < 1e-12) {
      hi += 1;
      lo -= 1;
    }
    const x0 = xs?.[0] ?? 0;
    const x1 = xs?.[n - 1] ?? n - 1;
    const sx = (i: number) => {
      const x = xs?.[i] ?? i;
      return PAD.l + ((x - x0) / Math.max(x1 - x0, 1e-9)) * (W - PAD.l - PAD.r);
    };
    const sy = (v: number) => PAD.t + (1 - (tf(v) - lo) / (hi - lo)) * (H - PAD.t - PAD.b);
    const last = series[0].ys[series[0].ys.length - 1];
    body = (
      <>
        {threshold !== undefined && (
          <line
            x1={PAD.l}
            x2={W - PAD.r}
            y1={sy(threshold)}
            y2={sy(threshold)}
            className="nl-threshold"
          />
        )}
        {series.map((s, k) => (
          <polyline
            key={k}
            fill="none"
            stroke={s.color}
            strokeWidth={k === 0 ? 1.6 : 1}
            opacity={k === 0 ? 1 : 0.55}
            points={s.ys
              .map((y, i) => (Number.isFinite(y) && (!logY || y > 0) ? `${sx(i)},${sy(y)}` : ""))
              .filter(Boolean)
              .join(" ")}
          />
        ))}
        <text x={W - PAD.r} y={12} textAnchor="end" fill={series[0].color} className="nl-val">
          {Number.isFinite(last) ? yFmt(last) : "—"}
        </text>
      </>
    );
  }
  return (
    <svg className="nl-chart" width={W} height={H} viewBox={`0 0 ${W} ${H}`}>
      <rect x={0.5} y={0.5} width={W - 1} height={H - 1} rx={6} className="nl-frame" />
      <text x={PAD.l + 2} y={12} className="nl-title">
        {title}
        {logY ? " (log)" : ""}
      </text>
      {body}
    </svg>
  );
}

export function NerdLog() {
  const open = useStore((s) => s.logOpen);
  const lines = useStore((s) => s.logLines);
  const optSeries = useStore((s) => s.optSeries);
  const solveResiduals = useStore((s) => s.solveResiduals);
  const solveTol = useStore((s) => s.solveTol);
  const setLogOpen = useStore((s) => s.setLogOpen);
  const clearLog = useStore((s) => s.clearLog);
  const disclaimerSkipped = useStore((s) => s.disclaimerSkipped);
  const setDisclaimerSkipped = useStore((s) => s.setDisclaimerSkipped);
  const listRef = useRef<HTMLDivElement>(null);

  // Follow the tail like a terminal.
  useEffect(() => {
    const el = listRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [lines.length, open]);

  // The toggle lives in the status strip; closed means invisible.
  if (!open) return null;

  const its = optSeries.map((p) => p.it);
  return (
    <div className="nerdlog">
      <div className="nl-head">
        <b>Log for nerds</b>
        <span className="dim small">
          MGCG solver & SIMP optimizer telemetry — convergence when mean |Δρ| &lt; 0.005 twice
        </span>
        <span style={{ flex: 1 }} />
        <button onClick={clearLog}>Clear</button>
        <button onClick={() => setLogOpen(false)}>Close</button>
      </div>
      <div className="nl-charts">
        <MiniChart
          title="Compliance C [N·mm]"
          xs={its}
          logY
          series={[{ ys: optSeries.map((p) => p.compliance), color: "#e06a13" }]}
          yFmt={(v) => v.toExponential(2)}
        />
        <MiniChart
          title="Design change Δρ — mean & max"
          xs={its}
          logY
          threshold={0.005}
          series={[
            { ys: optSeries.map((p) => p.meanChange), color: "#e06a13", label: "mean" },
            { ys: optSeries.map((p) => p.change), color: "#aba8a0", label: "max" },
          ]}
        />
        <MiniChart
          title="Inner CG iterations / step"
          xs={its}
          series={[{ ys: optSeries.map((p) => p.innerIters), color: "#e06a13" }]}
          yFmt={(v) => v.toFixed(0)}
        />
        <MiniChart
          title="MGCG residual"
          logY
          threshold={solveTol || undefined}
          series={[{ ys: solveResiduals, color: "#e06a13" }]}
          yFmt={(v) => v.toExponential(0)}
        />
      </div>
      <div className="nl-log" ref={listRef}>
        {lines.length === 0 ? (
          <div className="dim small">Run Check, Solve or Optimize — telemetry appears here.</div>
        ) : (
          lines.map((l, i) => (
            <div key={i} className="nl-line">
              <span className="nl-time">{l.t}</span>
              <span className="nl-msg">{l.msg}</span>
            </div>
          ))
        )}
      </div>
      <label className="nl-skip dim small">
        <input
          type="checkbox"
          checked={disclaimerSkipped}
          onChange={(e) => setDisclaimerSkipped(e.target.checked)}
        />
        Skip the startup disclaimer in this browser (dev/testing)
      </label>
    </div>
  );
}
