// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Material charts — the visualization band of the Material Manager, plain
// SVG (no chart dependency, license policy §2.14), same idioms and validated
// palette as PropertyCharts:
//   (a) stress–strain curves: bilinear elastic–hardening model from E, σy,
//       Eₜ, σᵤ, εᵣ (chart-only fields; FDM falls back to σₜ as ultimate),
//       multi-material overlay, yield dot, rupture ×, dashed layer-adhesion
//       level σₜᶻ for FDM;
//   (b) property bars: E, σ, ρ and the specific values, normalized per row;
//   (c) FDM anisotropy bars: σₜ vs σₜᶻ vs τᶻ (the Z-weakness at a glance);
//   (d) numeric readout table.
// Identity is carried by a fixed per-material color (library order — the
// compare filter never repaints survivors).

import { useMemo, useRef, useState } from "react";
import { isIsotropic, type Material } from "../types";
import { convertFromCanonical, unitLabel } from "../units";

/** Validated categorical palette (identical to PropertyCharts — one system).
 *  Light surface #fcfcfa: CVD ΔE 9.1, normal-vision ΔE 19.6; the sub-3:1
 *  contrast slots are relieved by legend/value labels and the readout table.
 *  Fixed order, never cycled. */
const SERIES = ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4"] as const;
const SERIES_OVERFLOW = "#52514e";
const GRID = "#e1e0d9";
const AXIS = "#c3c2b7";

/** Color bound to the LIBRARY index, never to the drawn rank. */
export function materialColor(index: number): string {
  return index >= 0 && index < SERIES.length ? SERIES[index] : SERIES_OVERFLOW;
}

/** One drawn series: the material plus its library index (identity/color). */
export interface MatEntry {
  m: Material;
  index: number;
  color: string;
}

interface Tip {
  x: number;
  y: number;
  lines: { color?: string; text: string }[];
  strain?: number;
}

const fmtV = (v: number): string => {
  if (!Number.isFinite(v)) return "—";
  const a = Math.abs(v);
  if (a >= 10000) return Math.round(v).toLocaleString("en-US");
  if (a >= 100) return v.toFixed(0);
  if (a >= 10) return v.toFixed(1);
  return v.toFixed(2);
};

const stress = (mpa: number) => convertFromCanonical(mpa, "stress");
const modulus = (mpa: number) => convertFromCanonical(mpa, "modulus");
const densityD = (g: number) => convertFromCanonical(g, "density");

// ---- stress–strain model ----------------------------------------------------

interface SSCurve {
  /** Engineering strain (fraction) / stress (MPa) polyline, origin first. */
  pts: { e: number; s: number }[];
  yieldPt?: { e: number; s: number };
  rupture?: { e: number; s: number };
  /** FDM layer-adhesion level σₜᶻ in MPa — drawn as a dashed horizontal. */
  adhesion?: number;
}

/** Bilinear elastic–hardening curve from the scalar material fields. Chart
 *  model only — solves do not read it (plasticity is future work). The pure
 *  bilinear idealization: elastic slope E to σy, then slope Eₜ straight to
 *  εᵣ — σᵤ does NOT cap the line (it is informational, readout only).
 *  Missing Eₜ ⇒ perfectly-plastic plateau; missing εᵣ ⇒ the post-yield
 *  branch extends to 3·εy, unmarked. */
export function stressStrainCurve(m: Material): SSCurve {
  const E = Math.max(1, m.e0);
  const fdm = !isIsotropic(m);
  const sy = m.yieldStrength > 0 ? m.yieldStrength : undefined;
  const er = m.strainAtRupture;
  const Et = m.tangentModulus;

  const pts: { e: number; s: number }[] = [{ e: 0, s: 0 }];
  let yieldPt: SSCurve["yieldPt"];
  let rupture: SSCurve["rupture"];

  if (sy != null) {
    const ey = sy / E;
    pts.push({ e: ey, s: sy });
    yieldPt = { e: ey, s: sy };
    const eEnd = er ?? ey * 3;
    if (eEnd > ey) pts.push({ e: eEnd, s: sy + (Et ?? 0) * (eEnd - ey) });
    if (er != null) rupture = pts[pts.length - 1];
  } else {
    // No yield data (FDM with the build-sim yield disabled): elastic straight
    // to the tensile strength — the only strength scalar left to anchor on.
    const su = m.ultimateStrength ?? (fdm ? m.strength : undefined);
    if (su != null) {
      const eu = su / E;
      pts.push({ e: eu, s: su });
      if (er != null && er > eu) pts.push({ e: er, s: su });
      if (er != null) rupture = pts[pts.length - 1];
    } else {
      pts.push({ e: 0.02, s: 0.02 * E });
    }
  }

  return { pts, yieldPt, rupture, adhesion: fdm ? m.strengthZ : undefined };
}

/** Piecewise-linear stress at a strain, or null past the curve's end. */
function stressAt(c: SSCurve, e: number): number | null {
  const last = c.pts[c.pts.length - 1];
  if (e > last.e + 1e-9) return null;
  for (let i = 1; i < c.pts.length; i++) {
    const a = c.pts[i - 1];
    const b = c.pts[i];
    if (e <= b.e) {
      const t = b.e === a.e ? 0 : (e - a.e) / (b.e - a.e);
      return a.s + t * (b.s - a.s);
    }
  }
  return last.s;
}

function niceTicks(max: number, n = 4): number[] {
  if (!(max > 0)) return [];
  const raw = max / n;
  const mag = Math.pow(10, Math.floor(Math.log10(raw)));
  const norm = raw / mag;
  const step = (norm <= 1 ? 1 : norm <= 2 ? 2 : norm <= 5 ? 5 : 10) * mag;
  const out: number[] = [];
  for (let v = step; v <= max * 1.001; v += step) out.push(+v.toPrecision(10));
  return out;
}

// ---- the band ---------------------------------------------------------------

export function MaterialCharts(props: {
  materials: Material[];
  selIdx: number;
  onSelect: (index: number) => void;
}) {
  const [cmp, setCmp] = useState<number[]>([]);
  const sel = props.materials[props.selIdx] ?? props.materials[0];
  // Compare set survives deletes/selection changes by dropping invalid slots.
  const cmpValid = cmp.filter((i) => i !== props.selIdx && i >= 0 && i < props.materials.length);

  const entries: MatEntry[] = [
    { m: sel, index: props.selIdx, color: materialColor(props.selIdx) },
    ...cmpValid.map((i) => ({ m: props.materials[i], index: i, color: materialColor(i) })),
  ];
  // Two entities may share the overflow color — keep the selection readable.
  for (let i = 1; i < entries.length; i++) {
    if (entries[i].color === entries[0].color) entries[i].color = SERIES_OVERFLOW;
  }

  const toggleCmp = (i: number) =>
    setCmp((c) => (c.includes(i) ? c.filter((x) => x !== i) : [...c, i]));

  return (
    <div className="propscharts">
      <div className="chartcontrols">
        <span className="dim small">Compare with</span>
        {cmpValid.map((i) => (
          <button
            key={i}
            className="chip sel"
            title="Remove from comparison"
            onClick={() => toggleCmp(i)}
          >
            <span className="chipdot" style={{ background: materialColor(i) }} />{" "}
            {props.materials[i].name} ×
          </button>
        ))}
        {/* A picker instead of one chip per material: the row stays compact
            however long the library grows — only CHOSEN comparisons take
            space. Palette bound: selection + 4 comparisons keeps every drawn
            series distinguishable (5 fixed slots). */}
        {cmpValid.length < 4 ? (
          <select
            value=""
            onChange={(e) => {
              const i = Number(e.target.value);
              if (Number.isFinite(i) && e.target.value !== "") toggleCmp(i);
            }}
          >
            <option value="">{cmpValid.length ? "add…" : "none"}</option>
            {props.materials.map((m, i) =>
              i === props.selIdx || cmpValid.includes(i) ? null : (
                <option key={i} value={i}>
                  {m.name}
                </option>
              )
            )}
          </select>
        ) : (
          <span className="dim small">· up to 4 at once</span>
        )}
      </div>

      <div className="chartrow">
        <StressStrainChart entries={entries} onSelect={props.onSelect} />
        <PropertyBars entries={entries} />
      </div>
      <div className="chartrow">
        <AnisotropyBars entries={entries} />
        <MatReadout entries={entries} />
      </div>
    </div>
  );
}

// ---- (a) stress–strain ------------------------------------------------------

function StressStrainChart(props: { entries: MatEntry[]; onSelect: (index: number) => void }) {
  const W = 460;
  const H = 240;
  const M = { l: 48, r: 14, t: 10, b: 28 };
  const [tip, setTip] = useState<Tip | null>(null);
  const svgRef = useRef<SVGSVGElement>(null);

  const curves = useMemo(
    () => props.entries.map((en) => ({ en, c: stressStrainCurve(en.m) })),
    [props.entries]
  );
  const anyFdm = curves.some(({ c }) => c.adhesion != null);

  // X spans exactly to the largest rupture/end strain — εᵣ IS the axis end.
  const eMax = Math.max(1e-6, ...curves.map(({ c }) => c.pts[c.pts.length - 1].e));
  const sMaxMpa = Math.max(
    ...curves.flatMap(({ c }) => [...c.pts.map((p) => p.s), c.adhesion ?? 0])
  );
  const sMax = stress(sMaxMpa) * 1.08; // axis in the DISPLAY unit

  const x = (e: number) => M.l + ((W - M.l - M.r) * e) / eMax;
  const y = (sDisp: number) => H - M.b - ((H - M.t - M.b) * sDisp) / sMax;

  const xTicks = niceTicks(eMax * 100, 5); // strain in %
  const yTicks = niceTicks(sMax, 4);

  const paths = curves.map(({ en, c }) => ({
    en,
    c,
    d: c.pts.map((p, i) => `${i ? "L" : "M"}${x(p.e).toFixed(1)},${y(stress(p.s)).toFixed(1)}`).join(""),
  }));

  const onMove = (ev: React.MouseEvent) => {
    const rect = svgRef.current?.getBoundingClientRect();
    if (!rect) return;
    const px = ev.clientX - rect.left;
    const e = Math.min(eMax, Math.max(0, ((px - M.l) / (W - M.l - M.r)) * eMax));
    setTip({
      x: Math.min(px, W - 160),
      y: 12,
      strain: e,
      lines: [
        { text: `ε = ${(100 * e).toFixed(2)} %` },
        ...curves
          .map(({ en, c }) => ({ en, v: stressAt(c, e) }))
          .filter((r): r is { en: MatEntry; v: number } => r.v != null)
          .sort((a, b) => b.v - a.v)
          .map(({ en, v }) => ({
            color: en.color,
            text: `${en.m.name}: ${fmtV(stress(v))} ${unitLabel("stress")}`,
          })),
      ],
    });
  };

  return (
    <div className="chartcard">
      <div className="charttitle">
        Stress–strain{" "}
        <span className="dim">
          · bilinear model, ● yield, × rupture{anyFdm ? ", dashed = layer adhesion σₜᶻ" : ""}
        </span>
      </div>
      <div className="chartplot">
        <svg ref={svgRef} width={W} height={H} onMouseMove={onMove} onMouseLeave={() => setTip(null)}>
          {yTicks.map((t) => (
            <g key={t}>
              <line x1={M.l} x2={W - M.r} y1={y(t)} y2={y(t)} stroke={GRID} />
              <text x={M.l - 5} y={y(t) + 3} textAnchor="end" className="ticktext">
                {fmtV(t)}
              </text>
            </g>
          ))}
          {xTicks.map((t) => (
            <text key={t} x={x(t / 100)} y={H - M.b + 14} textAnchor="middle" className="ticktext">
              {fmtV(t)}%
            </text>
          ))}
          <line x1={M.l} x2={W - M.r} y1={H - M.b} y2={H - M.b} stroke={AXIS} />
          <line x1={M.l} x2={M.l} y1={M.t} y2={H - M.b} stroke={AXIS} />
          <text
            x={M.l - 34}
            y={M.t + (H - M.t - M.b) / 2}
            className="ticktext"
            textAnchor="middle"
            transform={`rotate(-90 ${M.l - 34} ${M.t + (H - M.t - M.b) / 2})`}
          >
            σ ({unitLabel("stress")})
          </text>
          {paths.map(({ en, c, d }, i) => (
            <g key={en.index} stroke={en.color} fill="none">
              {c.adhesion != null && (
                <line
                  x1={M.l}
                  x2={W - M.r}
                  y1={y(stress(c.adhesion))}
                  y2={y(stress(c.adhesion))}
                  strokeDasharray="4 4"
                  opacity={0.55}
                  strokeWidth={1.2}
                />
              )}
              <path d={d} strokeWidth={i === 0 ? 2.5 : 2} opacity={i === 0 ? 1 : 0.85} />
              {c.yieldPt && (
                <circle
                  cx={x(c.yieldPt.e)}
                  cy={y(stress(c.yieldPt.s))}
                  r={3.2}
                  fill={en.color}
                  stroke="#fcfcfa"
                  strokeWidth={1}
                />
              )}
              {c.rupture && (
                <path
                  d={`M${x(c.rupture.e) - 4},${y(stress(c.rupture.s)) - 4}L${x(c.rupture.e) + 4},${y(stress(c.rupture.s)) + 4}M${x(c.rupture.e) - 4},${y(stress(c.rupture.s)) + 4}L${x(c.rupture.e) + 4},${y(stress(c.rupture.s)) - 4}`}
                  strokeWidth={1.8}
                />
              )}
            </g>
          ))}
          {tip?.strain !== undefined && (
            <line
              x1={x(tip.strain)}
              x2={x(tip.strain)}
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
        {props.entries.map((en, i) => (
          <button
            key={en.index}
            className={`legenditem${i === 0 ? " sel" : ""}`}
            title={i === 0 ? undefined : "Click to inspect this material"}
            onClick={() => props.onSelect(en.index)}
          >
            <span className="chipdot" style={{ background: en.color }} />
            {en.m.name}
          </button>
        ))}
      </div>
    </div>
  );
}

// ---- (b) property bars ------------------------------------------------------

function PropertyBars(props: { entries: MatEntry[] }) {
  const shown = props.entries;
  const sLimit = (m: Material) => (isIsotropic(m) ? m.yieldStrength : m.strength);
  const ROWS = [
    { label: `E (${unitLabel("modulus")})`, get: (m: Material) => modulus(m.e0) },
    { label: `σₜ/σy (${unitLabel("stress")})`, get: (m: Material) => stress(sLimit(m)) },
    { label: `ρ (${unitLabel("density")})`, get: (m: Material) => densityD(m.density) },
    // Specific values stay canonical: a per-row NORMALIZED comparison — the
    // absolute number matters less than the ranking it makes visible.
    { label: "E/ρ (MPa·cm³/g)", get: (m: Material) => m.e0 / m.density },
    { label: "σ/ρ (MPa·cm³/g)", get: (m: Material) => sLimit(m) / m.density },
  ];

  const W = 460;
  const LAB = 108;
  const BAR = 9;
  const GAP = 2;
  const groupGap = 8;
  const groupH = shown.length * (BAR + GAP) + groupGap;
  const H = ROWS.length * groupH + 6;

  return (
    <div className="chartcard">
      <div className="charttitle">
        Property comparison <span className="dim">· bars normalized per row</span>
      </div>
      <svg width={W} height={H}>
        {ROWS.map((row, ri) => {
          const maxV = Math.max(1e-12, ...shown.map(({ m }) => row.get(m)));
          const w = (v: number) => ((W - LAB - 64) * v) / maxV;
          return (
            <g key={row.label} transform={`translate(0,${ri * groupH + 4})`}>
              <text
                x={LAB - 6}
                y={(shown.length * (BAR + GAP)) / 2 + 4}
                textAnchor="end"
                className="ticktext"
              >
                {row.label}
              </text>
              {shown.map((en, si) => {
                const v = row.get(en.m);
                return (
                  <g key={en.index} transform={`translate(${LAB},${si * (BAR + GAP)})`}>
                    <rect
                      width={Math.max(1, w(v))}
                      height={BAR}
                      rx={2}
                      fill={en.color}
                      opacity={si === 0 ? 1 : 0.85}
                    >
                      <title>{`${en.m.name} — ${row.label} = ${fmtV(v)}`}</title>
                    </rect>
                    <text x={w(v) + 4} y={BAR - 1} className="ticktext">
                      {fmtV(v)}
                    </text>
                  </g>
                );
              })}
            </g>
          );
        })}
      </svg>
    </div>
  );
}

// ---- (c) FDM anisotropy -----------------------------------------------------

function AnisotropyBars(props: { entries: MatEntry[] }) {
  const fdm = props.entries.filter(({ m }) => !isIsotropic(m));
  const ROWS = [
    { label: "σₜ in-layer", get: (m: Material) => m.strength, auto: () => false },
    { label: "σₜᶻ adhesion", get: (m: Material) => m.strengthZ, auto: () => false },
    {
      label: "τᶻ shear",
      get: (m: Material) => m.shearStrengthZ ?? 0.6 * m.strengthZ,
      auto: (m: Material) => m.shearStrengthZ == null,
    },
  ];

  const W = 460;
  const LAB = 92;
  const BAR = 9;
  const GAP = 2;
  const groupGap = 8;
  const groupH = Math.max(1, fdm.length) * (BAR + GAP) + groupGap;
  const H = ROWS.length * groupH + 6;
  const maxV = Math.max(1e-12, ...fdm.flatMap(({ m }) => ROWS.map((r) => r.get(m))));
  const w = (v: number) => ((W - LAB - 76) * v) / maxV;

  return (
    <div className="chartcard">
      <div className="charttitle">
        Layer anisotropy <span className="dim">· strengths in {unitLabel("stress")}, (auto) = 0.6·σₜᶻ</span>
      </div>
      {fdm.length === 0 ? (
        <div className="dim small">
          Isotropic materials have no build direction — nothing to compare here.
        </div>
      ) : (
        <svg width={W} height={H}>
          {ROWS.map((row, ri) => (
            <g key={row.label} transform={`translate(0,${ri * groupH + 4})`}>
              <text
                x={LAB - 6}
                y={(fdm.length * (BAR + GAP)) / 2 + 4}
                textAnchor="end"
                className="ticktext"
              >
                {row.label}
              </text>
              {fdm.map((en, si) => {
                const v = row.get(en.m);
                return (
                  <g key={en.index} transform={`translate(${LAB},${si * (BAR + GAP)})`}>
                    <rect
                      width={Math.max(1, w(v))}
                      height={BAR}
                      rx={2}
                      fill={en.color}
                      opacity={si === 0 ? 1 : 0.85}
                    >
                      <title>{`${en.m.name} — ${row.label} = ${fmtV(stress(v))} ${unitLabel("stress")}${row.auto(en.m) ? " (auto)" : ""}`}</title>
                    </rect>
                    <text x={w(v) + 4} y={BAR - 1} className="ticktext">
                      {fmtV(stress(v))}
                      {row.auto(en.m) ? " (auto)" : ""}
                    </text>
                  </g>
                );
              })}
            </g>
          ))}
        </svg>
      )}
    </div>
  );
}

// ---- (d) readout table ------------------------------------------------------

function MatReadout(props: { entries: MatEntry[] }) {
  return (
    <div className="chartcard">
      <div className="charttitle">
        Values <span className="dim">· — = not applicable / unset</span>
      </div>
      <table className="settingstable readouttable">
        <thead>
          <tr>
            <th>Material</th>
            <th className="dim">E ({unitLabel("modulus")})</th>
            <th className="dim">ν</th>
            <th className="dim">ρ ({unitLabel("density")})</th>
            <th className="dim">σₜ/σy</th>
            <th className="dim">σₜᶻ</th>
            <th className="dim">σᵤ</th>
            <th className="dim">εᵣ</th>
          </tr>
        </thead>
        <tbody>
          {props.entries.map((en, i) => {
            const m = en.m;
            const iso = isIsotropic(m);
            return (
              <tr key={en.index} className={i === 0 ? "sel" : undefined}>
                <td>
                  <span className="chipdot" style={{ background: en.color }} /> {m.name}
                </td>
                <td className="num">{fmtV(modulus(m.e0))}</td>
                <td className="num">{m.nu.toFixed(2)}</td>
                <td className="num">{fmtV(densityD(m.density))}</td>
                <td className="num">{fmtV(stress(iso ? m.yieldStrength : m.strength))}</td>
                <td className="num">{iso ? "—" : fmtV(stress(m.strengthZ))}</td>
                <td className="num">
                  {m.ultimateStrength != null ? fmtV(stress(m.ultimateStrength)) : "—"}
                </td>
                <td className="num">
                  {m.strainAtRupture != null ? `${(100 * m.strainAtRupture).toFixed(1)}%` : "—"}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

// ---- shared tooltip ----------------------------------------------------------

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
