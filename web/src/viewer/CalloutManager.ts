// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Fixed value callouts (modifier-gated gestures on contour views) and the
// min/max extreme markers: DOM overlay dots/chips, the SVG leader-line layer,
// the rubber-band box drags, and the per-tick screen projection that keeps
// everything pinned to the (possibly deformed) surface. Extracted verbatim
// from SceneManager.

import * as THREE from "three";
import type { ViewMode } from "../store";
import { format } from "../units";

/** A fixed value callout pinned to a surface point in a contour view. Its 3D
 *  anchor is recomputed each frame from the (possibly deformed) source mesh:
 *  `point` callouts from a face + barycentric coords, `max`/`min` from a soup
 *  vertex index. The value itself is captured once (it doesn't move). */
interface Callout {
  kind: "point" | "max" | "min";
  mesh: THREE.Mesh;
  faceIndex: number; // point: hit face; max/min: -1
  bary: THREE.Vector3; // point only
  vertexIndex: number; // max/min: soup vertex; point: -1
  value: number;
  dot: HTMLDivElement;
  chip: HTMLDivElement;
  line: SVGLineElement;
}

const SVG_NS = "http://www.w3.org/2000/svg";

/** Narrow view of the scene the callout/extreme overlays read. All accessors
 *  so the manager never holds stale references. */
export interface CalloutHost {
  camera(): THREE.Camera;
  /** Canvas rect for client → NDC conversion. */
  canvasRect(): DOMRect;
  /** Viewport size in px (projection → screen placement). */
  viewSize(): { w: number; h: number };
  viewMode(): ViewMode;
  /** A model is loaded (gates the capture-phase gesture). */
  hasMesh(): boolean;
  /** Formatter for the cursor value readout; null disables the gestures. */
  probeFormat(): ((v: number) => string) | null;
  /** The surface currently carrying probeable values, with a per-vertex
   *  value accessor (vertex index into the non-indexed soup). */
  probeSource(): { mesh: THREE.Mesh; valueAt: (i: number) => number } | null;
  /** Geometry + displacement buffer of the surface carrying the extremes
   *  (voxel-result hull when active, else the part mesh). */
  resultGeometry(): { geom: THREE.BufferGeometry | null; disp: Float32Array | null };
  /** DISPLAYED world position of an interior rest-space point (rest +
   *  exaggeration·u, from the volumetric section payload). False = no volume
   *  loaded → the interior marker hides. */
  interiorDisplayedPos(rest: [number, number, number], out: THREE.Vector3): boolean;
  /** A line for the nerd log (e.g. a placed value callout). */
  onLog(msg: string): void;
}

/** Interior (solid-cell) field extremes from the volumetric section payload —
 *  candidate for the third marker. `flip` = inverted colormap (safety factor:
 *  the critical extreme is the MIN). */
export interface InteriorExtreme {
  flip: boolean;
  min: number;
  max: number;
  minAt: [number, number, number];
  maxAt: [number, number, number];
}

/** A load's value label, drawn like a result callout (a dot pinned to the load
 *  anchor + an offset value chip + a thin leader line) instead of an opaque 3D
 *  sprite sitting on the picked spot. Projected each frame in `projectBcCallouts`. */
export interface BcCalloutItem {
  id: string;
  world: THREE.Vector3;
  text: string;
  /** Roster colour (CSS hex) for the dot fill, chip text, and leader line. */
  color: string;
  /** Deactivated in the shown step → drawn dim. */
  ghost: boolean;
}

export class CalloutManager {
  // ---- fixed value callouts (contour views) ----
  private callouts: Callout[] = [];
  /** In-progress modifier gesture: ctrl = point, shift = max-in-box, alt = min. */
  private annoDrag: { mode: "point" | "max" | "min"; x0: number; y0: number; x1: number; y1: number } | null =
    null;
  private annoRectEl: HTMLDivElement | null = null; // rubber-band for box drags
  private annoSvg: SVGSVGElement | null = null; // leader-line layer
  private parent: HTMLElement | null = null;
  private _calloutWorld = new THREE.Vector3();
  private raycaster = new THREE.Raycaster();
  private pointer = new THREE.Vector2();

  // Min/max value markers for the active result plot.
  private extremesOn = false;
  private extremesUnit = "";
  private extremeData: { minIdx: number; maxIdx: number; minVal: number; maxVal: number } | null =
    null;
  // Min/max value marks as DOM overlays projected each frame (see `tick`), so
  // they keep a constant screen size and stay subtle — matching the hover
  // probe — instead of world-scaled sprites that balloon as you zoom in.
  private extremeEls: {
    minDot: HTMLDivElement;
    minChip: HTMLDivElement;
    maxDot: HTMLDivElement;
    maxChip: HTMLDivElement;
    intDot: HTMLDivElement;
    intChip: HTMLDivElement;
  } | null = null;
  private extremeWorld = { min: new THREE.Vector3(), max: new THREE.Vector3(), int: new THREE.Vector3() };
  private extremeVisible = false;
  private interiorVisible = false;
  /** Interior extreme candidate (volumetric payload); marker shows only when
   *  it BEATS the surface extreme (higher max / lower SF min). */
  private interior: InteriorExtreme | null = null;
  private extremeScratch = new THREE.Vector3();

  // ---- load value labels (setup view) drawn callout-style ----
  private bcEls: { world: THREE.Vector3; dot: HTMLDivElement; chip: HTMLDivElement; line: SVGLineElement }[] = [];
  private bcVisible = true;

  constructor(private readonly host: CalloutHost) {}

  /** Create the DOM overlays under `parent` and hook the gesture listeners.
   *  `before` is the probe tooltip: the SVG leader-line layer slots in front
   *  of it in the DOM so it paints under the probe/chips/dots. */
  attach(parent: HTMLElement, before: HTMLElement) {
    this.parent = parent;
    // Min/max marks: a small colored dot at the extreme point + a probe-style
    // value chip beside it, both placed every frame from the 3D location.
    const mk = (kind: "min" | "max" | "interior") => {
      const dot = document.createElement("div");
      dot.className = `extreme-dot ${kind}`;
      const chip = document.createElement("div");
      chip.className = `probe extreme-chip ${kind}`;
      dot.style.display = chip.style.display = "none";
      parent.append(dot, chip);
      return { dot, chip };
    };
    const lo = mk("min");
    const hi = mk("max");
    const int = mk("interior");
    this.extremeEls = {
      minDot: lo.dot,
      minChip: lo.chip,
      maxDot: hi.dot,
      maxChip: hi.chip,
      intDot: int.dot,
      intChip: int.chip,
    };

    // Fixed value callouts: a leader-line SVG layer (behind the chips) and a
    // rubber-band rectangle for the shift/alt box drags.
    const svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("class", "callout-lines");
    svg.style.cssText =
      "position:absolute;inset:0;width:100%;height:100%;pointer-events:none;overflow:visible;";
    parent.insertBefore(svg, before); // under the probe/chips/dots
    this.annoSvg = svg;

    const rect = document.createElement("div");
    rect.className = "callout-rubber";
    rect.style.cssText =
      "position:absolute;display:none;border:1px dashed #2b2f36;background:rgba(255,178,36,.12);pointer-events:none;";
    parent.appendChild(rect);
    this.annoRectEl = rect;

    // Capture phase so it beats OrbitControls' own canvas pointerdown.
    parent.addEventListener("pointerdown", this.onAnnoDownCapture, true);
    // Move + release on document so a drag that leaves the canvas still tracks.
    document.addEventListener("pointermove", this.onAnnoMove);
    document.addEventListener("pointerup", this.onAnnoUp);
    document.addEventListener("keydown", this.onAnnoKey);
  }

  dispose() {
    document.removeEventListener("pointermove", this.onAnnoMove);
    document.removeEventListener("pointerup", this.onAnnoUp);
    document.removeEventListener("keydown", this.onAnnoKey);
    this.parent?.removeEventListener("pointerdown", this.onAnnoDownCapture, true);
    this.clearCallouts();
    this.clearBcCallouts();
    this.annoSvg?.remove();
    this.annoRectEl?.remove();
    if (this.extremeEls) for (const el of Object.values(this.extremeEls)) el.remove();
  }

  /** Capture-phase pointerdown on the viewport parent — runs BEFORE OrbitControls'
   *  own canvas listener so we can claim a modifier gesture and stop it. (Orbit-
   *  Controls remaps shift/ctrl + left-drag to a PAN; returning from the bubble-
   *  phase handler is too late, so we block it here.) */
  private onAnnoDownCapture = (ev: PointerEvent) => {
    if (ev.button !== 0 || !this.host.hasMesh()) return;
    const mode = ev.ctrlKey ? "point" : ev.shiftKey ? "max" : ev.altKey ? "min" : null;
    if (!mode || !this.host.probeFormat() || !this.host.probeSource()) return;
    ev.stopImmediatePropagation(); // block OrbitControls pan + our own handlers
    ev.preventDefault();
    this.annoDrag = { mode, x0: ev.clientX, y0: ev.clientY, x1: ev.clientX, y1: ev.clientY };
  };

  private onAnnoMove = (ev: PointerEvent) => {
    if (!this.annoDrag) return;
    this.annoDrag.x1 = ev.clientX;
    this.annoDrag.y1 = ev.clientY;
    if (this.annoDrag.mode !== "point") this.drawAnnoRect();
  };

  private onAnnoUp = () => {
    const drag = this.annoDrag;
    if (!drag) return;
    this.annoDrag = null;
    if (this.annoRectEl) this.annoRectEl.style.display = "none";
    if (drag.mode === "point") {
      this.addPointCallout(drag.x1, drag.y1);
    } else if (Math.abs(drag.x1 - drag.x0) > 3 && Math.abs(drag.y1 - drag.y0) > 3) {
      this.addExtremeCallout(drag.mode, drag);
    }
  };

  private onAnnoKey = (ev: KeyboardEvent) => {
    if (ev.key === "Escape" && this.callouts.length) this.clearCallouts();
  };

  private drawAnnoRect() {
    const r = this.annoRectEl;
    const d = this.annoDrag;
    if (!r || !d) return;
    const rect = this.host.canvasRect();
    r.style.display = "block";
    r.style.left = `${Math.min(d.x0, d.x1) - rect.left}px`;
    r.style.top = `${Math.min(d.y0, d.y1) - rect.top}px`;
    r.style.width = `${Math.abs(d.x1 - d.x0)}px`;
    r.style.height = `${Math.abs(d.y1 - d.y0)}px`;
  }

  /** Ctrl-click: the field value interpolated at the clicked surface point. */
  private addPointCallout(clientX: number, clientY: number) {
    const src = this.host.probeSource();
    if (!src || !src.mesh.visible) return;
    const rect = this.host.canvasRect();
    this.pointer.x = ((clientX - rect.left) / rect.width) * 2 - 1;
    this.pointer.y = -((clientY - rect.top) / rect.height) * 2 + 1;
    this.raycaster.setFromCamera(this.pointer, this.host.camera());
    const hits = this.raycaster.intersectObject(src.mesh, false);
    const hit = hits.length ? hits[0] : null;
    if (!hit || hit.faceIndex == null) return;
    const pos = (src.mesh.geometry.getAttribute("position") as THREE.BufferAttribute)
      .array as Float32Array;
    const f = hit.faceIndex;
    const tri = new THREE.Triangle(
      new THREE.Vector3(pos[9 * f], pos[9 * f + 1], pos[9 * f + 2]),
      new THREE.Vector3(pos[9 * f + 3], pos[9 * f + 4], pos[9 * f + 5]),
      new THREE.Vector3(pos[9 * f + 6], pos[9 * f + 7], pos[9 * f + 8])
    );
    const bary = new THREE.Vector3();
    tri.getBarycoord(hit.point, bary);
    const value =
      bary.x * src.valueAt(3 * f) + bary.y * src.valueAt(3 * f + 1) + bary.z * src.valueAt(3 * f + 2);
    this.createCallout({ kind: "point", mesh: src.mesh, faceIndex: f, bary, vertexIndex: -1, value });
  }

  /** Shift/Alt box drag: the highest (max) / lowest (min) field value among the
   *  surface points whose projection falls inside the dragged box. */
  private addExtremeCallout(
    mode: "max" | "min",
    d: { x0: number; y0: number; x1: number; y1: number }
  ) {
    const src = this.host.probeSource();
    if (!src || !src.mesh.visible) return;
    const rect = this.host.canvasRect();
    const ndc = (cx: number, cy: number): [number, number] => [
      ((cx - rect.left) / rect.width) * 2 - 1,
      -((cy - rect.top) / rect.height) * 2 + 1,
    ];
    const [ax, ay] = ndc(d.x0, d.y0);
    const [bx, by] = ndc(d.x1, d.y1);
    const loX = Math.min(ax, bx);
    const hiX = Math.max(ax, bx);
    const loY = Math.min(ay, by);
    const hiY = Math.max(ay, by);
    const pos = (src.mesh.geometry.getAttribute("position") as THREE.BufferAttribute)
      .array as Float32Array;
    const mw = src.mesh.matrixWorld;
    const p = this._calloutWorld;
    const camera = this.host.camera();
    const n = (pos.length / 3) | 0;
    let bestI = -1;
    let bestV = mode === "max" ? -Infinity : Infinity;
    for (let i = 0; i < n; i++) {
      p.set(pos[3 * i], pos[3 * i + 1], pos[3 * i + 2]).applyMatrix4(mw).project(camera);
      if (p.z < -1 || p.z > 1 || p.x < loX || p.x > hiX || p.y < loY || p.y > hiY) continue;
      const v = src.valueAt(i);
      if (mode === "max" ? v > bestV : v < bestV) {
        bestV = v;
        bestI = i;
      }
    }
    if (bestI < 0) return;
    this.createCallout({
      kind: mode,
      mesh: src.mesh,
      faceIndex: -1,
      bary: new THREE.Vector3(),
      vertexIndex: bestI,
      value: bestV,
    });
  }

  private createCallout(c: Omit<Callout, "dot" | "chip" | "line">) {
    const parent = this.parent;
    if (!parent || !this.annoSvg) return;
    const fmt = this.host.probeFormat();
    const label = fmt ? fmt(c.value) : `${c.value}`;
    const dot = document.createElement("div");
    dot.style.cssText =
      "position:absolute;width:9px;height:9px;margin:-5px 0 0 -5px;border-radius:50%;" +
      "background:#ffb224;border:1.5px solid #2b2f36;box-shadow:0 0 0 1px rgba(255,255,255,.7);" +
      "pointer-events:none;z-index:5;";
    const chip = document.createElement("div");
    chip.className = "probe";
    chip.style.position = "absolute";
    chip.style.pointerEvents = "none";
    chip.style.zIndex = "5";
    chip.textContent = label;
    const line = document.createElementNS(SVG_NS, "line");
    line.setAttribute("stroke", "#2b2f36");
    line.setAttribute("stroke-width", "1.25");
    this.annoSvg.appendChild(line);
    parent.append(dot, chip);
    const callout: Callout = { ...c, dot, chip, line };
    this.callouts.push(callout);
    this.projectCallout(callout);
    const w = this.calloutWorld(callout);
    if (w) {
      this.host.onLog(
        `callout ${label} @ (${w.x.toFixed(1)}, ${w.y.toFixed(1)}, ${w.z.toFixed(1)}) mm`
      );
    }
  }

  /** Current world anchor of a callout from the (possibly deformed) mesh. */
  private calloutWorld(c: Callout): THREE.Vector3 | null {
    const attr = c.mesh.geometry.getAttribute("position") as THREE.BufferAttribute | undefined;
    const pos = attr?.array as Float32Array | undefined;
    if (!pos) return null;
    const out = this._calloutWorld;
    if (c.kind === "point") {
      const f = c.faceIndex;
      if (9 * f + 8 >= pos.length) return null;
      out.set(
        c.bary.x * pos[9 * f] + c.bary.y * pos[9 * f + 3] + c.bary.z * pos[9 * f + 6],
        c.bary.x * pos[9 * f + 1] + c.bary.y * pos[9 * f + 4] + c.bary.z * pos[9 * f + 7],
        c.bary.x * pos[9 * f + 2] + c.bary.y * pos[9 * f + 5] + c.bary.z * pos[9 * f + 8]
      );
    } else {
      const i = c.vertexIndex;
      if (3 * i + 2 >= pos.length) return null;
      out.set(pos[3 * i], pos[3 * i + 1], pos[3 * i + 2]);
    }
    return out.applyMatrix4(c.mesh.matrixWorld);
  }

  private projectCallout(c: Callout) {
    const w = this.calloutWorld(c);
    const hide = () => {
      c.dot.style.display = c.chip.style.display = "none";
      c.line.style.display = "none";
    };
    if (!w || !c.mesh.visible) return hide();
    const v = w.project(this.host.camera()); // mutates the shared scratch in place
    if (v.z < -1 || v.z > 1) return hide();
    const { w: vw, h: vh } = this.host.viewSize();
    const x = (v.x * 0.5 + 0.5) * vw;
    const y = (-v.y * 0.5 + 0.5) * vh;
    c.dot.style.display = c.chip.style.display = c.line.style.display = "block";
    c.dot.style.left = `${x}px`;
    c.dot.style.top = `${y}px`;
    c.chip.style.left = `${x + 14}px`;
    c.chip.style.top = `${y + 12}px`;
    c.line.setAttribute("x1", `${x}`);
    c.line.setAttribute("y1", `${y}`);
    c.line.setAttribute("x2", `${x + 14}`);
    c.line.setAttribute("y2", `${y + 12}`);
  }

  updateCallouts() {
    for (const c of this.callouts) this.projectCallout(c);
  }

  /** Drop all callouts — a new result field / view / surface invalidates them. */
  clearCallouts() {
    for (const c of this.callouts) {
      c.dot.remove();
      c.chip.remove();
      c.line.remove();
    }
    this.callouts.length = 0;
  }

  /** Re-format the pinned value callouts after a display-unit change (their chip
   *  text is captured once at creation; `probeFormat` reads the live unit). */
  relabelCallouts() {
    const fmt = this.host.probeFormat();
    if (!fmt) return;
    for (const c of this.callouts) c.chip.textContent = fmt(c.value);
  }

  // ---------- load value labels (callout-style) ----------

  /** Replace the load value labels. Each becomes a small roster-coloured dot on
   *  the load anchor + an offset value chip + a leader line — so the value never
   *  covers the picked spot (unlike the old opaque 3D sprite). Rebuilt only when
   *  the BC set changes; `projectBcCallouts` places them every frame. */
  setBcCallouts(items: BcCalloutItem[]) {
    this.clearBcCallouts();
    if (!this.parent || !this.annoSvg) return;
    for (const it of items) {
      const dot = document.createElement("div");
      dot.className = "bc-callout-dot";
      dot.style.background = it.color;
      const chip = document.createElement("div");
      chip.className = "probe bc-callout-chip";
      chip.textContent = it.text;
      chip.style.color = it.color;
      const line = document.createElementNS(SVG_NS, "line");
      line.setAttribute("class", "bc-callout-line");
      line.setAttribute("stroke", it.color);
      if (it.ghost) {
        dot.style.opacity = chip.style.opacity = "0.45";
        line.style.opacity = "0.3";
      }
      this.annoSvg.appendChild(line);
      this.parent.append(dot, chip);
      this.bcEls.push({ world: it.world, dot, chip, line });
    }
    this.projectBcCallouts();
  }

  /** Show/hide the load labels (they belong to the setup view, like the load
   *  glyphs — hidden in mesh/result views). */
  setBcCalloutsVisible(on: boolean) {
    this.bcVisible = on;
    this.projectBcCallouts();
  }

  /** Project the load labels to screen pixels + place their DOM. Called every
   *  frame from the render tick (the camera may have moved). Chip offset
   *  below-right of the dot (matching the result extremes), leader line between. */
  projectBcCallouts() {
    if (!this.bcEls.length) return;
    if (!this.bcVisible) {
      for (const e of this.bcEls) e.dot.style.display = e.chip.style.display = e.line.style.display = "none";
      return;
    }
    const { w, h } = this.host.viewSize();
    const cam = this.host.camera();
    for (const e of this.bcEls) {
      const v = this.extremeScratch.copy(e.world).project(cam);
      if (v.z < -1 || v.z > 1) {
        e.dot.style.display = e.chip.style.display = e.line.style.display = "none";
        continue;
      }
      const x = (v.x * 0.5 + 0.5) * w;
      const y = (-v.y * 0.5 + 0.5) * h;
      const ox = 15;
      const oy = 11;
      e.dot.style.display = e.chip.style.display = e.line.style.display = "block";
      e.dot.style.left = `${x}px`;
      e.dot.style.top = `${y}px`;
      e.chip.style.left = `${x + ox}px`;
      e.chip.style.top = `${y + oy}px`;
      e.line.setAttribute("x1", `${x}`);
      e.line.setAttribute("y1", `${y}`);
      e.line.setAttribute("x2", `${x + ox}`);
      e.line.setAttribute("y2", `${y + oy}`);
    }
  }

  clearBcCallouts() {
    for (const e of this.bcEls) {
      e.dot.remove();
      e.chip.remove();
      e.line.remove();
    }
    this.bcEls.length = 0;
  }

  // ---------- min/max markers ----------

  /** Toggle the min/max location markers; unit drives label formatting. */
  setShowExtremes(on: boolean, unit: string) {
    this.extremesOn = on;
    this.extremesUnit = unit;
  }

  trackExtremes(values: Float32Array | ArrayLike<number>) {
    let minIdx = 0;
    let maxIdx = 0;
    let minVal = Infinity;
    let maxVal = -Infinity;
    for (let i = 0; i < values.length; i++) {
      const v = values[i];
      if (v < minVal) {
        minVal = v;
        minIdx = i;
      }
      if (v > maxVal) {
        maxVal = v;
        maxIdx = i;
      }
    }
    this.extremeData = { minIdx, maxIdx, minVal, maxVal };
    this.updateExtremeMarkers();
  }

  /** No extreme-bearing field on screen: drop the data + hide the marks. */
  clearExtremes() {
    this.extremeData = null;
    this.updateExtremeMarkers();
  }

  /** Interior extreme candidate (null = no volumetric payload). The marker
   *  itself only shows when the interior value beats the surface extreme. */
  setInteriorExtreme(interior: InteriorExtreme | null) {
    this.interior = interior;
    this.updateExtremeMarkers();
  }

  private fmtExtreme(v: number): string {
    // `extremesUnit` is the field's canonical unit tag ("mm" | "MPa" | "×" | "")
    // from the store; route through the display-unit registry so markers match
    // the legend. Values are canonical.
    if (this.extremesUnit === "×") return `${v.toFixed(2)}×`; // safety factor
    if (this.extremesUnit === "mm") return format(v, "length");
    if (this.extremesUnit === "MPa") return format(v, "stress");
    return v === 0 ? "0" : format(v, "strain"); // dimensionless strain
  }

  /** Refresh the min/max marks: store the DISPLAYED extreme world positions
   *  (projected to the screen each frame in `tick`) and update their value
   *  chips. Visibility/placement of the DOM overlays happens in
   *  `projectExtremes`. */
  updateExtremeMarkers(positionsOnly = false) {
    const { geom, disp } = this.host.resultGeometry();
    this.extremeVisible =
      this.extremesOn &&
      this.host.viewMode() === "deformed" &&
      !!this.extremeData &&
      !!geom &&
      !!disp;
    const els = this.extremeEls;
    if (!this.extremeVisible || !this.extremeData || !els) {
      this.projectExtremes(); // hide the overlays
      return;
    }
    const pos = (geom!.getAttribute("position") as THREE.BufferAttribute).array as Float32Array;
    const d = this.extremeData;
    this.extremeWorld.min.set(pos[3 * d.minIdx], pos[3 * d.minIdx + 1], pos[3 * d.minIdx + 2]);
    this.extremeWorld.max.set(pos[3 * d.maxIdx], pos[3 * d.maxIdx + 1], pos[3 * d.maxIdx + 2]);
    // Third marker: the volumetric (interior) extreme, only when it BEATS the
    // surface by a margin (same 2% as the log advisory — a near-tie would just
    // duplicate the surface marker) — the true critical value then sits inside
    // the part (typically at the perimeter/infill interface). SF fields flip:
    // their critical extreme is the MIN.
    const int = this.interior;
    // Additive margin from the field's magnitude (multiplicative would invert
    // for signed fields whose extreme is negative).
    const eps = 0.02 * Math.max(Math.abs(d.minVal), Math.abs(d.maxVal));
    const beats = int && (int.flip ? int.min < d.minVal - eps : int.max > d.maxVal + eps);
    this.interiorVisible = !!(
      beats && this.host.interiorDisplayedPos(int!.flip ? int!.minAt : int!.maxAt, this.extremeWorld.int)
    );
    if (!positionsOnly) {
      els.minChip.textContent = `min ${this.fmtExtreme(d.minVal)}`;
      els.maxChip.textContent = `max ${this.fmtExtreme(d.maxVal)}`;
      if (int) {
        els.intChip.textContent = int.flip
          ? `min (interior) ${this.fmtExtreme(int.min)}`
          : `max (interior) ${this.fmtExtreme(int.max)}`;
      }
    }
  }

  /** Project the stored extreme positions to screen pixels and place the DOM
   *  marks. Called every frame from `tick` (the camera may have moved). */
  projectExtremes() {
    const els = this.extremeEls;
    if (!els) return;
    if (!this.extremeVisible) {
      for (const el of [els.minDot, els.minChip, els.maxDot, els.maxChip, els.intDot, els.intChip]) {
        el.style.display = "none";
      }
      return;
    }
    this.placeExtreme(els.minDot, els.minChip, this.extremeWorld.min);
    this.placeExtreme(els.maxDot, els.maxChip, this.extremeWorld.max);
    if (this.interiorVisible) {
      this.placeExtreme(els.intDot, els.intChip, this.extremeWorld.int);
    } else {
      els.intDot.style.display = els.intChip.style.display = "none";
    }
  }

  private placeExtreme(dot: HTMLDivElement, chip: HTMLDivElement, world: THREE.Vector3) {
    const v = this.extremeScratch.copy(world).project(this.host.camera());
    if (v.z < -1 || v.z > 1) {
      dot.style.display = chip.style.display = "none"; // behind the camera
      return;
    }
    const { w, h } = this.host.viewSize();
    const x = (v.x * 0.5 + 0.5) * w;
    const y = (-v.y * 0.5 + 0.5) * h;
    dot.style.display = chip.style.display = "block";
    dot.style.left = `${x}px`;
    dot.style.top = `${y}px`;
    chip.style.left = `${x + 9}px`;
    chip.style.top = `${y + 9}px`;
  }
}
