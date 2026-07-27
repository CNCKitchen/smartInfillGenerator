// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Infill property charts (DESIGN §24 M2) — the four dec. 8 visualizations,
// plain SVG (no chart dependency, license policy §2.14):
//   (a) stiffness-vs-density curves: direction selector, multi-set overlay,
//       log-log toggle, solid inside each set's calibrated band, dashed
//       (extrapolated) outside;
//   (b) directional stiffness polar section E(θ), layer plane → build axis,
//       at a chosen density, vs the dashed isotropic reference;
//   (c) anisotropy ratio bars across sets;
//   (d) reference-density readout table.
// Everything is relative to solid (E/E₀); identity is carried by a fixed
// per-set color (library order — a filtered view never repaints survivors).

import { useMemo, useRef, useState } from "react";
import {
  gxyEp,
  nuZp,
  relStiffness,
  type InfillPropertySet,
} from "../infillProps";

/** Validated categorical palette (dataviz reference order, light surface
 *  #fcfcfa: CVD ΔE 9.1, normal-vision ΔE 19.6 — sub-3:1 slots are relieved by
 *  legend/value labels and the readout table). Fixed order, never cycled. */
const SERIES = ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4"] as const;
/** Overflow color for a selected set beyond the palette (never cycled). */
const SERIES_OVERFLOW = "#52514e";
const GRID = "#e1e0d9";
const AXIS = "#c3c2b7";

export type Direction = "ep" | "ez" | "gz" | "gxy";

const DIR_LABEL: Record<Direction, string> = {
  ep: "Ep — in-plane",
  ez: "Ez — build axis",
  gz: "Gz — through-layer shear",
  gxy: "Gxy — in-plane shear",
};

const DIR_SHORT: Record<Direction, string> = { ep: "Ep", ez: "Ez", gz: "Gz", gxy: "Gxy" };

/** Sets drawn in the overlay: the first palette-many in library order, plus
 *  the selected one (overflow color) when it sits beyond them. */
function overlaySets(sets: InfillPropertySet[], selectedId: string) {
  const shown = sets.slice(0, SERIES.length).map((s, i) => ({ s, color: SERIES[i] as string }));
  if (!shown.some((e) => e.s.id === selectedId)) {
    const sel = sets.find((x) => x.id === selectedId);
    if (sel) shown.push({ s: sel, color: SERIES_OVERFLOW });
  }
  return shown;
}

function colorOf(sets: InfillPropertySet[], id: string): string {
  const i = sets.findIndex((s) => s.id === id);
  return i >= 0 && i < SERIES.length ? SERIES[i] : SERIES_OVERFLOW;
}

/** Directional Young's modulus of a TI solid, Ep = 1; θ from the BUILD AXIS.
 *  1/E(θ) = sin⁴θ·S11 + cos⁴θ·S33 + sin²θcos²θ·(2·S13 + S44). */
function eTheta(set: InfillPropertySet, theta: number): number {
  const r = set.ratios;
  const s = Math.sin(theta);
  const c = Math.cos(theta);
  const s2 = s * s;
  const c2 = c * c;
  const inv = s2 * s2 + (c2 * c2) / r.ezEp + s2 * c2 * (1 / r.gzEp - 2 * r.nuPz);
  return 1 / inv;
}

interface Tip {
  x: number;
  y: number;
  lines: { color?: string; text: string }[];
  /** Data-x of the crosshair (curve chart only). */
  rho?: number;
}

const pctFmt = (v: number) => `${(100 * v).toFixed(1)}%`;

export function PropertyCharts(props: {
  sets: InfillPropertySet[];
  selectedId: string;
  onSelect: (id: string) => void;
}) {
  const [dir, setDir] = useState<Direction>("ep");
  const [logLog, setLogLog] = useState(false);
  const [polarPct, setPolarPct] = useState(30);
  const sel = props.sets.find((s) => s.id === props.selectedId) ?? props.sets[0];

  return (
    <div className="propscharts">
      <div className="chartcontrols">
        <span className="dim small">Direction</span>
        {(Object.keys(DIR_LABEL) as Direction[]).map((d) => (
          <button
            key={d}
            className={`chip${dir === d ? " sel" : ""}`}
            title={DIR_LABEL[d]}
            onClick={() => setDir(d)}
          >
            {DIR_SHORT[d]}
          </button>
        ))}
        <label className="row small" style={{ marginLeft: 10 }}>
          <input type="checkbox" checked={logLog} onChange={(e) => setLogLog(e.target.checked)} />
          log–log
        </label>
        <span className="dim small" style={{ marginLeft: "auto" }}>
          Polar at
        </span>
        <input
          type="range"
          min={10}
          max={100}
          step={5}
          value={polarPct}
          onChange={(e) => setPolarPct(Number(e.target.value))}
        />
        <span className="small mono">{polarPct}%</span>
      </div>

      <div className="chartrow">
        <CurveChart
          sets={props.sets}
          selectedId={props.selectedId}
          dir={dir}
          logLog={logLog}
          onSelect={props.onSelect}
        />
        <PolarChart set={sel} color={colorOf(props.sets, sel.id)} rho={polarPct / 100} />
      </div>
      <div className="chartrow">
        <RatioBars sets={props.sets} selectedId={props.selectedId} />
        <ReadoutTable sets={props.sets} selectedId={props.selectedId} dir={dir} />
      </div>
    </div>
  );
}

// ---- (a) stiffness vs density ----

function CurveChart(props: {
  sets: InfillPropertySet[];
  selectedId: string;
  dir: Direction;
  logLog: boolean;
  onSelect: (id: string) => void;
}) {
  const W = 460;
  const H = 250;
  const M = { l: 44, r: 18, t: 8, b: 26 };
  const [tip, setTip] = useState<Tip | null>(null);
  const svgRef = useRef<SVGSVGElement>(null);
  const shown = overlaySets(props.sets, props.selectedId);

  const RHO_MIN = 0.05;
  const E_MIN = 1e-3; // log floor: 0.1% of solid
  const x = (rho: number) =>
    M.l +
    (W - M.l - M.r) *
      (props.logLog
        ? (Math.log10(rho) - Math.log10(RHO_MIN)) / -Math.log10(RHO_MIN)
        : (rho - RHO_MIN) / (1 - RHO_MIN));
  const y = (e: number) =>
    H -
    M.b -
    (H - M.t - M.b) *
      (props.logLog
        ? (Math.log10(Math.max(e, E_MIN)) - Math.log10(E_MIN)) / -Math.log10(E_MIN)
        : e);

  const paths = useMemo(() => {
    return shown.map(({ s, color }) => {
      // Three segments per set: below / inside / above the calibrated band —
      // extrapolation renders dashed, measurement solid (§24.1 dec. 7).
      const seg = (lo: number, hi: number) => {
        if (hi <= lo) return "";
        const n = 48;
        let d = "";
        for (let i = 0; i <= n; i++) {
          const rho = lo + ((hi - lo) * i) / n;
          d += `${i ? "L" : "M"}${x(rho).toFixed(1)},${y(relStiffness(s, rho, props.dir)).toFixed(1)}`;
        }
        return d;
      };
      const [blo, bhi] = s.calibratedBand;
      return {
        s,
        color,
        below: seg(RHO_MIN, Math.min(Math.max(blo, RHO_MIN), 1)),
        inside: seg(Math.max(blo, RHO_MIN), Math.min(bhi, 1)),
        above: seg(Math.min(bhi, 1), 1),
      };
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.sets, props.selectedId, props.dir, props.logLog]);

  const yTicks = props.logLog ? [0.001, 0.01, 0.1, 1] : [0.25, 0.5, 0.75, 1];
  const xTicks = props.logLog ? [0.05, 0.1, 0.2, 0.5, 1] : [0.2, 0.4, 0.6, 0.8, 1];

  const onMove = (ev: React.MouseEvent) => {
    const rect = svgRef.current?.getBoundingClientRect();
    if (!rect) return;
    const px = ev.clientX - rect.left;
    const frac = Math.min(1, Math.max(0, (px - M.l) / (W - M.l - M.r)));
    const rho = props.logLog
      ? Math.pow(10, Math.log10(RHO_MIN) * (1 - frac))
      : RHO_MIN + (1 - RHO_MIN) * frac;
    setTip({
      x: Math.min(px, W - 150),
      y: 12,
      rho,
      lines: [
        { text: `ρ = ${(100 * rho).toFixed(0)}% · ${DIR_SHORT[props.dir]}/E₀` },
        ...shown
          .map(({ s, color }) => ({
            color,
            v: relStiffness(s, rho, props.dir),
            name: s.name,
          }))
          .sort((a, b) => b.v - a.v)
          .map((e) => ({ color: e.color, text: `${e.name}: ${pctFmt(e.v)}` })),
      ],
    });
  };

  return (
    <div className="chartcard">
      <div className="charttitle">
        {DIR_LABEL[props.dir]} vs density <span className="dim">· E/E₀, dashed = extrapolated</span>
      </div>
      <div className="chartplot">
        <svg
          ref={svgRef}
          width={W}
          height={H}
          onMouseMove={onMove}
          onMouseLeave={() => setTip(null)}
        >
          {yTicks.map((t) => (
            <g key={t}>
              <line x1={M.l} x2={W - M.r} y1={y(t)} y2={y(t)} stroke={GRID} />
              <text x={M.l - 5} y={y(t) + 3} textAnchor="end" className="ticktext">
                {props.logLog && t < 0.01 ? `${(100 * t).toFixed(1)}%` : `${Math.round(100 * t)}%`}
              </text>
            </g>
          ))}
          {xTicks.map((t) => (
            <text key={t} x={x(t)} y={H - M.b + 14} textAnchor="middle" className="ticktext">
              {Math.round(100 * t)}%
            </text>
          ))}
          <line x1={M.l} x2={W - M.r} y1={H - M.b} y2={H - M.b} stroke={AXIS} />
          <line x1={M.l} x2={M.l} y1={M.t} y2={H - M.b} stroke={AXIS} />
          {paths.map((p) => {
            const em = p.s.id === props.selectedId;
            const w = em ? 2.5 : 2;
            const op = em ? 1 : 0.75;
            return (
              <g key={p.s.id} strokeWidth={w} opacity={op} fill="none" stroke={p.color}>
                {p.below && <path d={p.below} strokeDasharray="4 4" />}
                {p.inside && <path d={p.inside} />}
                {p.above && <path d={p.above} strokeDasharray="4 4" />}
              </g>
            );
          })}
          {tip?.rho !== undefined && (
            <line
              x1={x(tip.rho)}
              x2={x(tip.rho)}
              y1={M.t}
              y2={H - M.b}
              stroke={AXIS}
              strokeDasharray="2 3"
            />
          )}
        </svg>
        {tip && <ChartTip tip={tip} />}
      </div>
      <div className="chartlegend">
        {shown.map(({ s, color }) => (
          <button
            key={s.id}
            className={`legenditem${s.id === props.selectedId ? " sel" : ""}`}
            onClick={() => props.onSelect(s.id)}
          >
            <span className="chipdot" style={{ background: color }} />
            {s.name}
          </button>
        ))}
        {props.sets.length > SERIES.length && (
          <span className="dim small">first {SERIES.length} + selection shown</span>
        )}
      </div>
    </div>
  );
}

// ---- (b) directional polar section ----

function PolarChart(props: { set: InfillPropertySet; color: string; rho: number }) {
  const SZ = 250;
  const cx = SZ / 2;
  const cy = SZ / 2 + 4;
  const R = SZ / 2 - 30;
  const [tip, setTip] = useState<Tip | null>(null);
  const svgRef = useRef<SVGSVGElement>(null);

  const ep = relStiffness(props.set, props.rho, "ep"); // isotropic reference = magnitude law
  // Radius scale: the largest directional modulus fills the plot.
  const maxE = useMemo(() => {
    let m = 0;
    for (let i = 0; i <= 90; i++) m = Math.max(m, eTheta(props.set, (i * Math.PI) / 180));
    return m * ep;
  }, [props.set, ep]);
  const r = (e: number) => (R * e) / Math.max(maxE, 1e-12);

  const path = useMemo(() => {
    let d = "";
    for (let i = 0; i <= 360; i += 2) {
      const th = (i * Math.PI) / 180;
      // θ measured from the build axis (screen vertical): x = sin, y = -cos.
      const rr = r(ep * eTheta(props.set, th));
      const px = cx + rr * Math.sin(th);
      const py = cy - rr * Math.cos(th);
      d += `${i ? "L" : "M"}${px.toFixed(1)},${py.toFixed(1)}`;
    }
    return d + "Z";
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.set, props.rho, ep, maxE]);

  const ez = ep * props.set.ratios.ezEp;

  const onMove = (ev: React.MouseEvent) => {
    const rect = svgRef.current?.getBoundingClientRect();
    if (!rect) return;
    const dx = ev.clientX - rect.left - cx;
    const dy = ev.clientY - rect.top - cy;
    const th = Math.atan2(Math.abs(dx), -dy); // 0 = build axis, symmetric
    const deg = (th * 180) / Math.PI;
    const e = ep * eTheta(props.set, th);
    setTip({
      x: Math.min(ev.clientX - rect.left + 10, SZ - 130),
      y: 10,
      lines: [
        { text: `${deg.toFixed(0)}° from build axis` },
        { color: props.color, text: `E/E₀ = ${pctFmt(e)}` },
      ],
    });
  };

  return (
    <div className="chartcard">
      <div className="charttitle">
        Directional stiffness at {Math.round(100 * props.rho)}%{" "}
        <span className="dim">· dashed = isotropic model</span>
      </div>
      <div className="chartplot">
        <svg
          ref={svgRef}
          width={SZ}
          height={SZ}
          onMouseMove={onMove}
          onMouseLeave={() => setTip(null)}
        >
          <line x1={cx - R - 6} x2={cx + R + 6} y1={cy} y2={cy} stroke={GRID} />
          <line x1={cx} x2={cx} y1={cy - R - 6} y2={cy + R + 6} stroke={GRID} />
          <circle cx={cx} cy={cy} r={r(ep)} fill="none" stroke={AXIS} strokeDasharray="4 4" />
          <path d={path} fill={props.color} fillOpacity={0.12} stroke={props.color} strokeWidth={2} />
          <text x={cx + R + 6} y={cy + 14} textAnchor="end" className="ticktext">
            layer plane · Ep {pctFmt(ep)}
          </text>
          <text x={cx} y={cy - R - 10} textAnchor="middle" className="ticktext">
            build axis · Ez {pctFmt(ez)}
          </text>
        </svg>
        {tip && <ChartTip tip={tip} />}
      </div>
      <div className="dim small">
        θ sweeps loading direction from the build axis into the layer plane; the dashed circle is
        what the isotropic model assumes at this density.
      </div>
    </div>
  );
}

// ---- (c) anisotropy ratio bars ----

const RATIO_ROWS = [
  { key: "ezEp", label: "Ez/Ep", get: (s: InfillPropertySet) => s.ratios.ezEp },
  { key: "gzEp", label: "Gz/Ep", get: (s: InfillPropertySet) => s.ratios.gzEp },
  { key: "gxyEp", label: "Gxy/Ep", get: (s: InfillPropertySet) => gxyEp(s.ratios) },
  { key: "nuP", label: "νp", get: (s: InfillPropertySet) => s.ratios.nuP },
  { key: "nuPz", label: "νpz", get: (s: InfillPropertySet) => s.ratios.nuPz },
  { key: "nuZp", label: "νzp", get: (s: InfillPropertySet) => nuZp(s.ratios) },
] as const;

function RatioBars(props: { sets: InfillPropertySet[]; selectedId: string }) {
  const shown = overlaySets(props.sets, props.selectedId);
  const W = 460;
  const LAB = 58;
  const BAR = 9;
  const GAP = 2;
  const groupGap = 8;
  const groupH = shown.length * (BAR + GAP) + groupGap;
  const H = RATIO_ROWS.length * groupH + 6;
  const maxV = Math.max(1, ...shown.flatMap(({ s }) => RATIO_ROWS.map((r) => r.get(s))));
  const w = (v: number) => ((W - LAB - 60) * v) / maxV;

  return (
    <div className="chartcard">
      <div className="charttitle">
        Anisotropy ratios <span className="dim">· Ep = 1 · Gxy, νzp derived</span>
      </div>
      <svg width={W} height={H}>
        {RATIO_ROWS.map((row, ri) => (
          <g key={row.key} transform={`translate(0,${ri * groupH + 4})`}>
            <text x={LAB - 6} y={(shown.length * (BAR + GAP)) / 2 + 4} textAnchor="end" className="ticktext">
              {row.label}
            </text>
            {shown.map(({ s, color }, si) => {
              const v = row.get(s);
              return (
                <g key={s.id} transform={`translate(${LAB},${si * (BAR + GAP)})`}>
                  <rect
                    width={Math.max(1, w(v))}
                    height={BAR}
                    rx={2}
                    fill={color}
                    opacity={s.id === props.selectedId ? 1 : 0.75}
                  >
                    <title>{`${s.name} — ${row.label} = ${v.toFixed(4)}`}</title>
                  </rect>
                  <text x={w(v) + 4} y={BAR - 1} className="ticktext">
                    {v.toFixed(3)}
                  </text>
                </g>
              );
            })}
          </g>
        ))}
      </svg>
    </div>
  );
}

// ---- (d) reference-density readout ----

function ReadoutTable(props: { sets: InfillPropertySet[]; selectedId: string; dir: Direction }) {
  const shown = overlaySets(props.sets, props.selectedId);
  const RHOS = [0.2, 0.5];
  const DIRS: Direction[] = ["ep", "ez", "gz"];
  return (
    <div className="chartcard">
      <div className="charttitle">
        Stiffness relative to solid <span className="dim">· at 20% and 50% infill</span>
      </div>
      <table className="settingstable readouttable">
        <thead>
          <tr>
            <th>Set</th>
            {RHOS.map((rho) =>
              DIRS.map((d) => (
                <th key={`${rho}${d}`} className="dim">
                  {DIR_SHORT[d]}({Math.round(rho * 100)}%)
                </th>
              ))
            )}
          </tr>
        </thead>
        <tbody>
          {shown.map(({ s, color }) => (
            <tr key={s.id} className={s.id === props.selectedId ? "sel" : undefined}>
              <td>
                <span className="chipdot" style={{ background: color }} /> {s.name}
              </td>
              {RHOS.map((rho) =>
                DIRS.map((d) => (
                  <td key={`${rho}${d}`} className="mono">
                    {pctFmt(relStiffness(s, rho, d))}
                  </td>
                ))
              )}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

// ---- shared tooltip ----

function ChartTip({ tip }: { tip: Tip }) {
  return (
    <div className="charttip" style={{ left: tip.x, top: tip.y }}>
      {tip.lines.map((l, i) => (
        <div key={i} className="row">
          {l.color && <span className="chipdot" style={{ background: l.color }} />}
          <span>{l.text}</span>
        </div>
      ))}
    </div>
  );
}
