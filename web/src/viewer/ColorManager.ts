// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Geometry coloring for the viewer: the base/BC color repaint (per-triangle
// vertex colors + hover tint), the scalar-field LUT paths (stress/strain jet,
// displacement jet, density ramp, flat envelope), banded-contour quantization
// of the shared jet LUT, legend range overrides, and the uv-channel math that
// feeds the 1D LUT textures. Extracted verbatim from SceneManager.

import * as THREE from "three";
import type { Bc } from "../types";
import type { ViewMode } from "../store";
import { CONTOUR_BANDS, jet, ramp, type RGB } from "./colormaps";

const BASE_COLOR = new THREE.Color(0x9aa3ad);
// Hover highlight: saturated amber, unmistakable against the gray part and
// every BC color (a light gray tint was too close to the base material).
const HOVER_TINT = new THREE.Color(0xffb224);

// Deepened for the light Werkbank stage; KIND_DOT in StepPanel.tsx must match.
export const BC_COLORS: Record<string, THREE.Color> = {
  fixed: new THREE.Color(0x2563eb),
  frictionless: new THREE.Color(0x0e9cbf),
  displacement: new THREE.Color(0x7c3aed),
  elastic: new THREE.Color(0x1f9d6b),
  force: new THREE.Color(0xd93025),
  pressure: new THREE.Color(0xc97b10),
  bearing: new THREE.Color(0xb5179e),
  moment: new THREE.Color(0xe8590c),
  accel: new THREE.Color(0xb08900),
  // Saturated teal — the former slate-gray (0x495057) barely contrasted against
  // the gray part, so a mass patch's surface tint + sphere read as a muddy blob.
  mass: new THREE.Color(0x0f766e),
};

/** Row-center y coordinates of the two-row LUT (see makeLut): row 0 is the
 *  colormap, row 1 a flat neutral grey for MASKED samples (NaN values —
 *  e.g. cells excluded from the orientation score, DESIGN §15). Sampling at
 *  the row centers never blends the rows, even with linear filtering. */
export const LUT_ROW_MAP = 0.25;
export const LUT_ROW_MASK = 0.75;

/** Normalize a scalar field into the uv.x channel for LUT sampling:
 *  t = clamp((v - lo) / (hi - lo)), optionally flipped (safety factor: red
 *  marks the LOW values). Non-finite values sample the grey MASK row. */
export function writeFieldUvs(
  uvs: Float32Array,
  values: ArrayLike<number>,
  lo: number,
  hi: number,
  flip: boolean
) {
  const inv = hi - lo > 1e-30 ? 1 / (hi - lo) : 0;
  for (let i = 0; i < values.length; i++) {
    const v = values[i];
    if (!Number.isFinite(v)) {
      uvs[2 * i] = 0.5;
      uvs[2 * i + 1] = LUT_ROW_MASK;
      continue;
    }
    const t = Math.min(1, Math.max(0, (v - lo) * inv));
    uvs[2 * i] = flip ? 1 - t : t;
    uvs[2 * i + 1] = LUT_ROW_MAP;
  }
}

/** Density (0–1, legend tops out at 80%) → ramp LUT coordinate. */
export function writeDensityUvs(uvs: Float32Array, density: ArrayLike<number>) {
  for (let i = 0; i < density.length; i++) {
    uvs[2 * i] = Math.min(1, density[i] / 0.8);
    uvs[2 * i + 1] = LUT_ROW_MAP;
  }
}

/** Bake a colormap (see ./colormaps) into a 256×2 texture: row 0 the
 *  colormap (sampled via uv.x at LUT_ROW_MAP), row 1 flat grey for masked
 *  samples (LUT_ROW_MASK). The same `jet`/`ramp` feed the legend bars. */
function makeLut(fn: (t: number) => RGB): THREE.DataTexture {
  const n = 256;
  const data = new Uint8Array(n * 2 * 4);
  for (let i = 0; i < n; i++) {
    const [r, g, b] = fn(i / (n - 1));
    data[4 * i] = Math.round(255 * r);
    data[4 * i + 1] = Math.round(255 * g);
    data[4 * i + 2] = Math.round(255 * b);
    data[4 * i + 3] = 255;
    // Mask row: neutral grey (matches the werkbank chassis tones).
    data[4 * (n + i)] = 138;
    data[4 * (n + i) + 1] = 143;
    data[4 * (n + i) + 2] = 152;
    data[4 * (n + i) + 3] = 255;
  }
  const tex = new THREE.DataTexture(data, n, 2, THREE.RGBAFormat);
  tex.colorSpace = THREE.SRGBColorSpace;
  tex.minFilter = THREE.LinearFilter;
  tex.magFilter = THREE.LinearFilter;
  tex.needsUpdate = true;
  return tex;
}

/** Narrow view of the scene state the coloring paths read. All accessors so
 *  the manager never holds stale references (buffers are swapped per model). */
export interface ColorHost {
  mesh(): THREE.Mesh | null;
  geometry(): THREE.BufferGeometry | null;
  colors(): Float32Array | null;
  uvs(): Float32Array | null;
  triCount(): number;
  bcs(): Bc[];
  activeBcId(): string | null;
  patchToTris(): Map<number, number[]>;
  viewMode(): ViewMode;
  displacements(): Float32Array | null;
  vertexDensity(): Float32Array | null;
  hasOptShape(): boolean;
  voxResultActive(): boolean;
  voxRes(): { geo: THREE.BufferGeometry; disp: Float32Array; uvs: Float32Array } | null;
  /** Min/max tracking for the active result plot (extrema markers). */
  trackExtremes(values: Float32Array | ArrayLike<number>): void;
  clearExtremes(): void;
  /** Auto min/max of the active displacement COMPONENT (signed), for the
   *  legend. |u| magnitude reports nothing — the legend uses the solve stat. */
  onResultRange(min: number, max: number): void;
}

export class ColorManager {
  // Colormaps are sampled per-fragment from 1D LUT textures via the uv
  // channel — per-vertex colors interpolate straight through RGB and turn
  // jet into blue→purple→red on coarse meshes.
  readonly lutJet = makeLut(jet);
  readonly lutRamp = makeLut(ramp);
  private scalarMode: "none" | "jet" | "ramp" | "flat" = "none";
  /** Discrete contour bands: the jet LUT is quantized into `bandCount` flat
   *  steps (toggled by clicking the legend, count set by scrolling it). Result
   *  fields only — the density ramp stays smooth. */
  private banded = false;
  private bandCount = CONTOUR_BANDS;

  // Scalar result field (stress/strain) overriding displacement colors.
  // flip = inverted colormap (safety factor: red marks the LOW values).
  private scalarField: {
    values: Float32Array;
    min: number;
    max: number;
    flip: boolean;
  } | null = null;
  /** User override of the color-scale range (click-to-edit legend). */
  private legendRange: { min: number | null; max: number | null } = { min: null, max: null };
  /** Displacement coloring quantity: -1 = |u| magnitude, 0/1/2 = signed
   *  X/Y/Z component. Only consulted on the displacement fallback (a scalar
   *  stress/strain field, when present, always takes precedence). */
  private dispComponent = -1;
  /** Last auto range reported for a displacement component — de-dupes the
   *  store writes that feed the legend. */
  private lastDispRange: { min: number; max: number } | null = null;

  private triBcColor: (THREE.Color | null)[] = [];
  private hoverPatch: number | null = null;
  /** Set by repaint() (full color rewrite), cleared after the next render. While
   *  set, setHover uploads in full too, so its partial range can't shadow a
   *  pending full rewrite (three uploads partially whenever updateRanges is set). */
  private colorsDirtyFull = false;
  private _hoverCol = new THREE.Color();

  constructor(private readonly host: ColorHost) {}

  /** The active scalar field (probe/callout value source). */
  get scalarFieldData(): { values: Float32Array; min: number; max: number; flip: boolean } | null {
    return this.scalarField;
  }

  /** The color normalization the LAST applyColors actually used (legend
   *  overrides included) — the section cap samples the same LUT with the
   *  same range so cap and surface always agree. Null outside result views. */
  get appliedRange(): { lo: number; hi: number; flip: boolean } | null {
    return this.lastApplied;
  }

  /** Active displacement coloring component (-1 = |u|, 0/1/2 = X/Y/Z). */
  get dispComponentValue(): number {
    return this.dispComponent;
  }

  private lastApplied: { lo: number; hi: number; flip: boolean } | null = null;

  /** The color buffer (if any) was just uploaded with the render. */
  markColorsUploaded() {
    this.colorsDirtyFull = false;
  }

  /** New patch set: any hovered patch id is stale. */
  resetHover() {
    this.hoverPatch = null;
  }

  /** New model geometry: back to plain BC vertex colors. */
  resetScalarMode() {
    this.scalarMode = "none";
  }

  clearScalarField() {
    this.scalarField = null;
  }

  /** A new solution resets the field picker to |u| (store side); keep the
   *  coloring component in step so it never colors by a stale X/Y/Z choice. */
  resetDispComponent() {
    this.dispComponent = -1;
    this.lastDispRange = null;
  }

  /** Choose what the deformed view colors by: -1 = |u| magnitude, 0/1/2 =
   *  signed X/Y/Z displacement component. Returns false when unchanged. */
  setDispComponent(comp: number): boolean {
    if (this.dispComponent === comp) return false;
    this.dispComponent = comp;
    this.lastDispRange = null; // force a fresh range report for the new field
    return true;
  }

  /** Stress/strain scalars per soup vertex; null reverts to |u| coloring.
   *  `flip` inverts the colormap (safety factor: red = the critical LOW).
   *  `signed` centers the color scale on 0 (signed von Mises: blue =
   *  compression, green ≈ unloaded, red = tension) — must match the store's
   *  symmetric `fieldRange` so the legend agrees with the surface.
   *  `range` overrides the auto (surface min/max) scale — the FieldServer
   *  widens it to the interior (volumetric) extremes so legend, surface and
   *  section cap share one honest scale. */
  setScalarField(
    values: Float32Array | null,
    flip = false,
    signed = false,
    range: { min: number; max: number } | null = null
  ) {
    if (values && values.length) {
      let min: number;
      let max: number;
      if (range) {
        ({ min, max } = range);
      } else {
        min = Infinity;
        max = -Infinity;
        for (let i = 0; i < values.length; i++) {
          min = Math.min(min, values[i]);
          max = Math.max(max, values[i]);
        }
        if (signed) {
          const m = Math.max(Math.abs(min), Math.abs(max), 1e-12);
          min = -m;
          max = m;
        }
      }
      this.scalarField = { values, min, max, flip };
    } else {
      this.scalarField = null;
    }
  }

  /** Clamp the color scale to a user range (null = auto). */
  setLegendRange(min: number | null, max: number | null) {
    this.legendRange = { min, max };
  }

  /** Recompute the full per-triangle color buffer. */
  repaint() {
    const colors = this.host.colors();
    const geometry = this.host.geometry();
    if (!colors || !geometry) return;
    const triCount = this.host.triCount();
    const triColor: (THREE.Color | null)[] = new Array(triCount).fill(null);
    for (const bc of this.host.bcs()) {
      const col = BC_COLORS[bc.kind] ?? new THREE.Color(0x888888);
      const isActive = bc.id === this.host.activeBcId();
      const c = isActive ? col.clone().lerp(new THREE.Color(0xffffff), 0.25) : col;
      for (const t of bc.tris) triColor[t] = c;
    }
    this.triBcColor = triColor;
    const hover = this.hoverPatch !== null ? this.host.patchToTris().get(this.hoverPatch) : undefined;
    const hoverSet = hover ? new Set(hover) : null;
    for (let t = 0; t < triCount; t++) {
      let c = triColor[t] ?? BASE_COLOR;
      if (hoverSet?.has(t)) {
        c = triColor[t] ? triColor[t]!.clone().lerp(HOVER_TINT, 0.65) : HOVER_TINT;
      }
      for (let v = 0; v < 3; v++) {
        colors[9 * t + 3 * v] = c.r;
        colors[9 * t + 3 * v + 1] = c.g;
        colors[9 * t + 3 * v + 2] = c.b;
      }
    }
    const attr = geometry.getAttribute("color") as THREE.BufferAttribute;
    attr.clearUpdateRanges(); // a full rewrite supersedes any pending hover range
    this.colorsDirtyFull = true;
    attr.needsUpdate = true;
  }

  /** Hover is only ever a 2-patch delta, so don't rebuild the whole color buffer
   *  (a ~1.8M-float rewrite + full VBO upload on a big STL). Restore the tris of
   *  the previously-hovered patch to their base color, tint the newly-hovered
   *  patch, and upload only the touched span. Base colors live in `triBcColor`,
   *  kept current by repaint(). */
  setHover(patch: number | null) {
    if (patch === this.hoverPatch) return;
    const prev = this.hoverPatch;
    this.hoverPatch = patch;
    const colors = this.host.colors();
    const geometry = this.host.geometry();
    if (!colors || !geometry) return;
    let lo = Infinity;
    let hi = -Infinity;
    const paint = (t: number, c: THREE.Color) => {
      const o = 9 * t;
      for (let v = 0; v < 3; v++) {
        colors[o + 3 * v] = c.r;
        colors[o + 3 * v + 1] = c.g;
        colors[o + 3 * v + 2] = c.b;
      }
      if (t < lo) lo = t;
      if (t > hi) hi = t;
    };
    if (prev !== null) {
      const tris = this.host.patchToTris().get(prev);
      if (tris) for (const t of tris) paint(t, this.triBcColor[t] ?? BASE_COLOR);
    }
    if (patch !== null) {
      const tris = this.host.patchToTris().get(patch);
      if (tris)
        for (const t of tris) {
          const base = this.triBcColor[t];
          paint(t, base ? this._hoverCol.copy(base).lerp(HOVER_TINT, 0.65) : HOVER_TINT);
        }
    }
    if (hi < lo) return; // both patches empty / unknown
    const attr = geometry.getAttribute("color") as THREE.BufferAttribute;
    // A full rewrite is already queued this frame: don't add a partial range, or
    // three would upload only it and drop the rest of the rewrite.
    if (!this.colorsDirtyFull) attr.addUpdateRange(9 * lo, 9 * (hi - lo + 1));
    attr.needsUpdate = true;
  }

  /** Per-vertex scalar for the active displacement field: |u| magnitude or the
   *  signed component. `d` is the surface's 3-per-vertex displacement buffer. */
  dispValueAt(d: Float32Array, i: number): number {
    const c = this.dispComponent;
    return c < 0 ? Math.hypot(d[3 * i], d[3 * i + 1], d[3 * i + 2]) : d[3 * i + c];
  }

  /** Build the per-vertex displacement scalar array and the color-scale bounds
   *  for the active field, honoring any user legend override. Reports the auto
   *  range to the legend for signed components (|u| uses the solve stat). */
  private dispFieldValues(d: Float32Array): { values: Float32Array; lo: number; hi: number } {
    const comp = this.dispComponent;
    const n = d.length / 3;
    const values = new Float32Array(n);
    let dmin = Infinity;
    let dmax = -Infinity;
    for (let i = 0; i < n; i++) {
      const v = comp < 0 ? Math.hypot(d[3 * i], d[3 * i + 1], d[3 * i + 2]) : d[3 * i + comp];
      values[i] = v;
      if (v < dmin) dmin = v;
      if (v > dmax) dmax = v;
    }
    // |u| anchors the scale at 0; a signed component spans its own min/max.
    const autoLo = comp < 0 ? 0 : dmin;
    const autoHi = comp < 0 ? Math.max(dmax, 1e-12) : dmax;
    // Report the auto range for BOTH |u| and the signed components so the
    // legend bound follows the active result (de-duped in reportDispRange).
    this.reportDispRange(autoLo, autoHi);
    const lo = this.legendRange.min ?? autoLo;
    const hi = this.legendRange.max ?? autoHi;
    return { values, lo, hi };
  }

  private reportDispRange(min: number, max: number) {
    const last = this.lastDispRange;
    if (last && last.min === min && last.max === max) return;
    this.lastDispRange = { min, max };
    this.host.onResultRange(min, max);
  }

  /** Set discrete contour banding + the band count. Rewrites the SHARED jet LUT
   *  in place (both the smooth surface and the voxel-result surface sample it, so
   *  they update together) — quantized into `count` flat steps with nearest
   *  sampling for crisp band edges, or the smooth ramp when off. */
  setBanded(on: boolean, count = this.bandCount) {
    if (this.banded === on && this.bandCount === count) return;
    this.banded = on;
    this.bandCount = count;
    const tex = this.lutJet;
    const data = tex.image.data as Uint8Array;
    const n = tex.image.width; // quantize the colormap row ONLY (row 1 = mask grey)
    for (let i = 0; i < n; i++) {
      let t = i / (n - 1);
      if (on) {
        const b = Math.min(count - 1, Math.floor(t * count));
        t = (b + 0.5) / count; // band-center color
      }
      const [r, g, bl] = jet(t);
      data[4 * i] = Math.round(255 * r);
      data[4 * i + 1] = Math.round(255 * g);
      data[4 * i + 2] = Math.round(255 * bl);
      data[4 * i + 3] = 255;
    }
    tex.magFilter = on ? THREE.NearestFilter : THREE.LinearFilter;
    tex.minFilter = on ? THREE.NearestFilter : THREE.LinearFilter;
    tex.needsUpdate = true;
    this.repaint();
  }

  /** Switch the part material between BC vertex colors, a scalar LUT, or a flat
   *  uni-color (the translucent envelope used when the readout lives elsewhere
   *  — e.g. the density cutaway carries the colors, the part is just a shell). */
  private setSurfaceMaterialMode(mode: "none" | "jet" | "ramp" | "flat") {
    const mesh = this.host.mesh();
    if (!mesh || mode === this.scalarMode) return;
    this.scalarMode = mode;
    const mat = mesh.material as THREE.MeshStandardMaterial;
    if (mode === "none") {
      mat.map = null;
      mat.vertexColors = true;
      mat.color.setHex(0xffffff); // vertex colors carry the actual color
    } else if (mode === "flat") {
      mat.map = null;
      mat.vertexColors = false;
      mat.color.setHex(0xc9c6bf); // neutral Werkbank-chassis envelope tone
    } else {
      mat.map = mode === "jet" ? this.lutJet : this.lutRamp;
      mat.vertexColors = false;
      mat.color.setHex(0xffffff); // LUT map supplies the color
    }
    mat.needsUpdate = true;
  }

  /** Write a scalar (or |u|) into the voxel hull's uv channel (jet LUT). */
  private colorVoxelResult() {
    const vr = this.host.voxRes()!;
    const uvAttr = vr.geo.getAttribute("uv") as THREE.BufferAttribute;
    const sf = this.scalarField;
    if (sf && sf.values.length * 2 === vr.uvs.length) {
      const lo = this.legendRange.min ?? sf.min;
      const hi = this.legendRange.max ?? sf.max;
      writeFieldUvs(vr.uvs, sf.values, lo, hi, sf.flip);
      this.lastApplied = { lo, hi, flip: sf.flip };
      uvAttr.array.set(vr.uvs);
      uvAttr.needsUpdate = true;
      this.host.trackExtremes(sf.values);
      return;
    }
    const { values, lo, hi } = this.dispFieldValues(vr.disp);
    writeFieldUvs(vr.uvs, values, lo, hi, false);
    this.lastApplied = { lo, hi, flip: false };
    uvAttr.array.set(vr.uvs);
    uvAttr.needsUpdate = true;
    this.host.trackExtremes(values);
  }

  /** Re-derive the surface colors for the active view: one of the four LUT/
   *  colormap paths (voxel-result jet, stress/strain jet, displacement jet,
   *  density ramp) or the plain BC vertex-color repaint. */
  applyColors() {
    const geometry = this.host.geometry();
    const colors = this.host.colors();
    const uvs = this.host.uvs();
    if (!geometry || !colors || !uvs) return;
    const uvAttr = geometry.getAttribute("uv") as THREE.BufferAttribute;
    this.lastApplied = null; // set by the result-coloring branches below
    if (this.host.voxResultActive()) {
      this.colorVoxelResult();
      this.repaint();
      return;
    }
    const displacements = this.host.displacements();
    if (this.host.viewMode() === "deformed" && displacements) {
      const sf = this.scalarField;
      if (sf && sf.values.length * 2 === uvs.length) {
        // Stress/strain field coloring (user range override clamps).
        const lo = this.legendRange.min ?? sf.min;
        const hi = this.legendRange.max ?? sf.max;
        writeFieldUvs(uvs, sf.values, lo, hi, sf.flip);
        this.lastApplied = { lo, hi, flip: sf.flip };
        uvAttr.needsUpdate = true;
        this.setSurfaceMaterialMode("jet");
        this.host.trackExtremes(sf.values);
        return;
      }
      const { values, lo, hi } = this.dispFieldValues(displacements);
      writeFieldUvs(uvs, values, lo, hi, false);
      this.lastApplied = { lo, hi, flip: false };
      uvAttr.needsUpdate = true;
      this.setSurfaceMaterialMode("jet");
      this.host.trackExtremes(values);
      return;
    }
    const vertexDensity = this.host.vertexDensity();
    if (this.host.viewMode() === "density" && vertexDensity) {
      // With a cutaway/skeleton present, the dense interior is shown there
      // (color-coded); the part is just a flat translucent envelope so the
      // density isn't also smeared onto its mostly-skin outer surface.
      if (this.host.hasOptShape()) {
        this.setSurfaceMaterialMode("flat");
        this.host.clearExtremes();
        this.repaint();
        return;
      }
      // No cutaway: paint the density straight onto the surface.
      writeDensityUvs(uvs, vertexDensity);
      uvAttr.needsUpdate = true;
      this.setSurfaceMaterialMode("ramp");
      return;
    }
    this.setSurfaceMaterialMode("none");
    this.host.clearExtremes();
    this.repaint();
  }
}
