// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Imperative three.js layer: mesh display, patch hover/select, brush,
// BC coloring + support glyphs, axis gizmo, rigid-body-mode animation,
// deformed-shape overlay (with looping animation), density/region/voxel views.

import * as THREE from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";
import { TransformControls } from "three/addons/controls/TransformControls.js";
import type { Bc, LoadedModel } from "../types";
import type { Tool, ViewMode } from "../store";
import { CONTOUR_BANDS, jet, ramp, type RGB } from "./colormaps";
import type { OptRegion } from "../engine/EngineClient";

/** Named orthographic camera presets (keyboard Ctrl + 0–6). Axes follow the
 *  Z-up / Blender convention: "front" is the −Y face, matching the default
 *  isometric corner the part is framed from on load. */
export type CameraView = "default" | "top" | "bottom" | "front" | "behind" | "left" | "right";

/** Digit (KeyboardEvent.key) → camera preset for the Ctrl + 0–6 shortcuts. */
const VIEW_KEYS: Record<string, CameraView> = {
  "0": "default",
  "1": "top",
  "2": "bottom",
  "3": "front",
  "4": "behind",
  "5": "left",
  "6": "right",
};

const BASE_COLOR = new THREE.Color(0x9aa3ad);
// Hover highlight: saturated amber, unmistakable against the gray part and
// every BC color (a light gray tint was too close to the base material).
const HOVER_TINT = new THREE.Color(0xffb224);

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

/** PNG bytes of a 2D canvas (decode the data URL to a Uint8Array). */
function pngBytesFromCanvas(c: HTMLCanvasElement): Uint8Array | null {
  const url = c.toDataURL("image/png");
  const b64 = url.slice(url.indexOf(",") + 1);
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
// Deepened for the light Werkbank stage; KIND_DOT in StepPanel.tsx must match.
const BC_COLORS: Record<string, THREE.Color> = {
  fixed: new THREE.Color(0x2563eb),
  frictionless: new THREE.Color(0x0e9cbf),
  displacement: new THREE.Color(0x7c3aed),
  elastic: new THREE.Color(0x1f9d6b),
  force: new THREE.Color(0xd93025),
  pressure: new THREE.Color(0xc97b10),
};

export interface SceneCallbacks {
  /** Patch clicked in select mode: toggle its triangles in the active BC. */
  onPickPatch?: (tris: Uint32Array, additive: boolean) => void;
  /** Brush stroke: triangles under the brush. */
  onBrush?: (tris: Uint32Array, erase: boolean) => void;
  /** Place-on-face: the clicked triangle's outward normal (world). */
  onPlaceFace?: (normal: [number, number, number]) => void;
  /** Pick-direction: the clicked triangle's outward normal (world) — used to
   *  aim a direction-mode force load. */
  onPickDir?: (normal: [number, number, number]) => void;
  /** Viewer picked a new deformation autoscale (display exaggeration base). */
  onAutoScale?: (autoScale: number) => void;
  /** Auto min/max of the active displacement COMPONENT (signed), for the
   *  legend. |u| magnitude reports nothing — the legend uses the solve stat. */
  onResultRange?: (min: number, max: number) => void;
  /** Section plane changed (three.js convention: kept side is
   *  normal·p + constant ≥ 0) — the mesh view recuts its voxels from this. */
  onSectionMoved?: (normal: [number, number, number], constant: number) => void;
  /** Symmetry plane moved/rotated: plane n·p = c (n unit, world mm). */
  onSymmetryMoved?: (normal: [number, number, number], c: number) => void;
  /** WebGL context was lost (true) or restored (false) — for a user notice
   *  while the GPU resets and the viewport is briefly blank. */
  onContextLost?: (lost: boolean) => void;
  /** A line for the nerd log (e.g. a placed value callout). */
  onLog?: (msg: string) => void;
}

export class SceneManager {
  private renderer!: THREE.WebGLRenderer;
  private scene = new THREE.Scene();
  // Parallel projection (engineering convention) — lengths stay comparable.
  private camera!: THREE.OrthographicCamera;
  private orthoHalf = 120;
  private controls!: OrbitControls;
  private raycaster = new THREE.Raycaster();
  private pointer = new THREE.Vector2();

  // bumpMesh-style navigation: left-drag orbits around the surface point
  // under the cursor (free over the poles), wheel zooms toward the cursor.
  // OrbitControls keeps only damping + right-drag pan; rotation and zoom are
  // handled manually below.
  private canvas!: HTMLCanvasElement;
  private pivotMarker: THREE.Mesh | null = null;
  private orbitPivot: THREE.Vector3 | null = null; // active drag pivot
  private lastOrbitPivot: THREE.Vector3 | null = null; // fallback between drags
  private orbitStart: { x: number; y: number } | null = null;
  private orbitLast: { x: number; y: number } | null = null;
  private orbiting = false;
  private _oq1 = new THREE.Quaternion();
  private _oq2 = new THREE.Quaternion();
  private _oRight = new THREE.Vector3();
  private _oTmp = new THREE.Vector3();
  private _oTmp2 = new THREE.Vector3();
  private _oDir = new THREE.Vector3();
  /** Clearance (rad, ~2.3°) kept between the view axis and the ±Z pole, where
   *  OrbitControls' up = +Z `lookAt` is degenerate and the azimuth snaps 180°. */
  private readonly poleEps = 0.04;

  private mesh: THREE.Mesh | null = null;
  private geometry: THREE.BufferGeometry | null = null;
  private basePositions: Float32Array | null = null;
  private colors: Float32Array | null = null;
  private patchIds: Uint32Array | null = null;
  private patchToTris = new Map<number, number[]>();
  private triCount = 0;
  private bboxDiag = 100;
  /** Part AABB in world mm [lx,ly,lz,hx,hy,hz] — sizes/centers the symmetry
   *  plane on the actual part. Null until a model is loaded. */
  private partBbox: LoadedModel["bbox"] | null = null;

  /** Triangle-mesh wireframe overlay (inspect the input mesh) + its toggle. */
  private wireframeOn = false;
  private wireframeLines: THREE.LineSegments | null = null;

  private bcs: Bc[] = [];
  private activeBcId: string | null = null;
  /** BCs deactivated in the active load step — drawn translucent. */
  private inactiveBcs: Set<string> = new Set();
  private triBcColor: (THREE.Color | null)[] = [];
  private hoverPatch: number | null = null;
  /** Set by repaint() (full color rewrite), cleared after the next render. While
   *  set, setHover uploads in full too, so its partial range can't shadow a
   *  pending full rewrite (three uploads partially whenever updateRanges is set). */
  private colorsDirtyFull = false;
  private _hoverCol = new THREE.Color();

  // ---- fixed value callouts (contour views) ----
  private callouts: Callout[] = [];
  /** In-progress modifier gesture: ctrl = point, shift = max-in-box, alt = min. */
  private annoDrag: { mode: "point" | "max" | "min"; x0: number; y0: number; x1: number; y1: number } | null =
    null;
  private annoRectEl: HTMLDivElement | null = null; // rubber-band for box drags
  private annoSvg: SVGSVGElement | null = null; // leader-line layer
  private _calloutWorld = new THREE.Vector3();

  private tool: Tool = "orbit";
  private brushRadius = 3;
  private brushErase = false;
  private brushing = false;
  private brushCursor: THREE.Mesh | null = null;
  /** Crosshair shown at the hovered surface point in the "pick direction" tool,
   *  to signal that a click here sets the force direction. */
  private pickCursor: THREE.LineSegments | null = null;
  /** Live arrow shown at the hovered point in "pick direction", previewing the
   *  load direction (the hovered face's outward normal) a click would set. */
  private pickArrow: THREE.Group | null = null;
  private pickArrowDisposables: (THREE.BufferGeometry | THREE.Material)[] = [];

  /** Force arrows + support glyphs (classic FEA triangles), setup view only. */
  private bcMarkers = new THREE.Group();
  private markerDisposables: { dispose(): void }[] = [];

  // Axis gizmo (inset, bottom-right)
  private gizmoScene = new THREE.Scene();
  private gizmoCam = new THREE.OrthographicCamera(-1.9, 1.9, 1.9, -1.9, 0.1, 20);
  private viewW = 0;
  private viewH = 0;

  // Analysis (voxel) mesh
  private voxelGroup = new THREE.Group();
  private voxelDisposables: { dispose(): void }[] = [];

  // Build-sim live preview: faint full-hull ghost (deactivated voxels) + a
  // growing deformed active hull (already-printed voxels, exaggeration baked in).
  private buildGroup = new THREE.Group();
  private buildGhost: THREE.Object3D | null = null;
  private buildActive: THREE.Object3D | null = null;

  // Rigid-body-mode animation
  private rbmMode: { t: number[]; r: number[]; center: number[] } | null = null;
  private rbmAmp = 1;

  // Result views
  private displacements: Float32Array | null = null;
  private vertexDensity: Float32Array | null = null;
  /** Results on the analysis voxel hull (exact nodal displacements) —
   *  alternate surface for the deformed view, toggled by resultSurface. */
  private resultSurface: "stl" | "voxel" = "stl";
  private voxRes: {
    group: THREE.Group;
    geo: THREE.BufferGeometry;
    base: Float32Array;
    disp: Float32Array;
    uvs: Float32Array;
    lineGeo: THREE.BufferGeometry | null;
    lineBase: Float32Array | null;
    lineDisp: Float32Array | null;
  } | null = null;
  private voxResDisposables: { dispose(): void }[] = [];
  private regionMeshes: THREE.Mesh[] = [];
  private regionVisible: boolean[] = [];
  private viewMode: ViewMode = "setup";
  private deformScale = 1;
  private autoScale = 1;
  private deformAnimate = false;

  // Live optimization skeleton / density-threshold cutaway.
  private optShapeMesh: THREE.Mesh | null = null;
  // Result is a Part Topo body: hide the original envelope hull in result views
  // and render the body opaque (no moiré against the coincident envelope).
  private resultSolid = false;

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
  } | null = null;
  private extremeWorld = { min: new THREE.Vector3(), max: new THREE.Vector3() };
  private extremeVisible = false;
  private extremeScratch = new THREE.Vector3();

  // Section plane: clipping + stencil caps + combined transform gizmo
  // (translate along the normal only + two rotation rings).
  private sectionOn = false;
  private sectionPlane = new THREE.Plane(new THREE.Vector3(-1, 0, 0), 0);
  private sectionProxy = new THREE.Object3D();
  private sectionTranslate: TransformControls | null = null;
  private sectionRotate: TransformControls | null = null;
  private sectionQuad: THREE.Group | null = null;
  private sectionQuadDisposables: { dispose(): void }[] = [];
  private capPart: THREE.Object3D[] = [];
  private capVoxel: THREE.Object3D[] = [];
  /** Per-vertex element density of the current voxel hull (0–1: skin = 1,
   *  interior = infill ratio / optimized density, composite cells blended). */
  private voxelDensity: Float32Array | null = null;
  private meshDensity = false;
  /** The voxel hull already carries the section cut in its geometry. */
  private voxelCutActive = false;
  private capDisposables: { dispose(): void }[] = [];

  // Symmetry plane (optimizer constraint): section-style combined gizmo
  // (translate along the normal + two rotation rings). Visible only while
  // it's being edited — the store gates on step/busy, the scene additionally
  // hides it in result views.
  private symEnabled = false;
  private symProxy = new THREE.Object3D();
  private symTranslate: TransformControls | null = null;
  private symRotate: TransformControls | null = null;
  private symQuad: THREE.Group | null = null;
  private symQuadDisposables: { dispose(): void }[] = [];

  // Hover value probe: contour value next to the cursor on result/density
  // surfaces. The formatter doubles as the on/off switch (null = off).
  private probeEl: HTMLDivElement | null = null;
  private probeFormat: ((v: number) => string) | null = null;

  // Colormaps are sampled per-fragment from 1D LUT textures via the uv
  // channel — per-vertex colors interpolate straight through RGB and turn
  // jet into blue→purple→red on coarse meshes.
  private lutJet = makeLut(jet);
  private lutRamp = makeLut(ramp);
  private uvs: Float32Array | null = null;
  private scalarMode: "none" | "jet" | "ramp" | "flat" = "none";
  /** Discrete contour bands: the jet LUT is quantized into `bandCount` flat
   *  steps (toggled by clicking the legend, count set by scrolling it). Result
   *  fields only — the density ramp stays smooth. */
  private banded = false;
  private bandCount = CONTOUR_BANDS;

  private clock = new THREE.Clock();
  private callbacks: SceneCallbacks = {};
  private disposed = false;

  init(canvas: HTMLCanvasElement, callbacks: SceneCallbacks) {
    this.callbacks = callbacks;
    // stencil: required for the filled section caps (default off since r163).
    this.renderer = new THREE.WebGLRenderer({ canvas, antialias: true, stencil: true });
    this.renderer.setPixelRatio(window.devicePixelRatio);
    this.renderer.autoClear = false;
    this.renderer.localClippingEnabled = true;
    // Werkbank stage: light studio gray (matches .stage in styles.css).
    this.scene.background = new THREE.Color(0xdedcd6);

    this.camera = new THREE.OrthographicCamera(-120, 120, 120, -120, 0.1, 10000);
    this.camera.position.set(120, -160, 110);
    this.camera.up.set(0, 0, 1); // printer convention: Z up

    this.canvas = canvas;
    this.controls = new OrbitControls(this.camera, canvas);
    this.controls.enableDamping = true;
    this.controls.dampingFactor = 0.08;
    // "Move" pans in the screen plane (not along the ground).
    this.controls.screenSpacePanning = true;
    // Rotation + zoom are manual (pivot-on-cursor orbit, cursor-centric
    // zoom — see installNavigation). OrbitControls keeps damping + R-drag pan.
    this.controls.enableRotate = false;
    this.controls.enableZoom = false;
    // Full polar range: the manual orbit (onOrbitMove) already clamps its own
    // pitch to poleEps, so OrbitControls' per-frame reconstruction never lands
    // on the ±Z pole *during a drag* (where up = +Z lookAt is degenerate and
    // the azimuth snaps). The only on-pole placements are the top/bottom camera
    // presets (setCameraView), which set up = +Y so lookAt stays well-defined —
    // a poleEps clamp here would knock those ~2.3° off a true axis view.
    this.controls.minPolarAngle = 0;
    this.controls.maxPolarAngle = Math.PI;

    const hemi = new THREE.HemisphereLight(0xffffff, 0xb9b6ae, 1.0);
    this.scene.add(hemi);
    const key = new THREE.DirectionalLight(0xffffff, 1.6);
    key.position.set(1, -1.2, 1.8);
    this.scene.add(key);
    const fill = new THREE.DirectionalLight(0xc8d2e0, 0.4);
    fill.position.set(-1.5, 1, -0.5);
    this.scene.add(fill);

    const grid = new THREE.GridHelper(400, 40, 0xafada6, 0xc8c6bf);
    grid.rotation.x = Math.PI / 2; // Z-up
    this.scene.add(grid);

    this.scene.add(this.bcMarkers);
    this.scene.add(this.voxelGroup);
    this.scene.add(this.buildGroup);
    this.buildGizmo();

    canvas.addEventListener("pointermove", this.onPointerMove);
    canvas.addEventListener("pointerdown", this.onPointerDown);
    canvas.addEventListener("pointerup", this.onPointerUp);
    canvas.addEventListener("pointerleave", () => {
      this.setHover(null);
      if (this.probeEl) this.probeEl.style.display = "none";
      if (this.brushCursor) this.brushCursor.visible = false;
      if (this.pickCursor) this.pickCursor.visible = false;
      if (this.pickArrow) this.pickArrow.visible = false;
    });
    canvas.addEventListener("webglcontextlost", this.onGlLost);
    canvas.addEventListener("webglcontextrestored", this.onGlRestored);
    this.installNavigation(canvas);

    // Hover value probe tooltip (sibling of the canvas, .viewer is relative).
    if (canvas.parentElement) {
      this.probeEl = document.createElement("div");
      this.probeEl.className = "probe";
      this.probeEl.style.display = "none";
      canvas.parentElement.appendChild(this.probeEl);

      // Min/max marks: a small colored dot at the extreme point + a probe-style
      // value chip beside it, both placed every frame from the 3D location.
      const parent = canvas.parentElement;
      const mk = (kind: "min" | "max") => {
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
      this.extremeEls = {
        minDot: lo.dot,
        minChip: lo.chip,
        maxDot: hi.dot,
        maxChip: hi.chip,
      };

      // Fixed value callouts: a leader-line SVG layer (behind the chips) and a
      // rubber-band rectangle for the shift/alt box drags.
      const svg = document.createElementNS(SVG_NS, "svg");
      svg.setAttribute("class", "callout-lines");
      svg.style.cssText =
        "position:absolute;inset:0;width:100%;height:100%;pointer-events:none;overflow:visible;";
      parent.insertBefore(svg, this.probeEl); // under the probe/chips/dots
      this.annoSvg = svg;

      const rect = document.createElement("div");
      rect.className = "callout-rubber";
      rect.style.cssText =
        "position:absolute;display:none;border:1px dashed #2b2f36;background:rgba(255,178,36,.12);pointer-events:none;";
      parent.appendChild(rect);
      this.annoRectEl = rect;

      // Capture phase so it beats OrbitControls' own canvas pointerdown.
      parent.addEventListener("pointerdown", this.onAnnoDownCapture, true);
    }

    const loop = () => {
      if (this.disposed) return;
      requestAnimationFrame(loop);
      this.tick();
    };
    loop();
  }

  dispose() {
    this.disposed = true;
    document.removeEventListener("pointermove", this.onOrbitMove);
    document.removeEventListener("pointerup", this.onOrbitUp);
    document.removeEventListener("pointermove", this.onAnnoMove);
    document.removeEventListener("pointerup", this.onAnnoUp);
    document.removeEventListener("keydown", this.onAnnoKey);
    document.removeEventListener("keydown", this.onViewKey);
    this.canvas?.parentElement?.removeEventListener("pointerdown", this.onAnnoDownCapture, true);
    this.clearCallouts();
    this.canvas?.removeEventListener("wheel", this.onWheel);
    this.canvas?.removeEventListener("webglcontextlost", this.onGlLost);
    this.canvas?.removeEventListener("webglcontextrestored", this.onGlRestored);
    this.probeEl?.remove();
    this.annoSvg?.remove();
    this.annoRectEl?.remove();
    if (this.extremeEls) for (const el of Object.values(this.extremeEls)) el.remove();
    if (this.wireframeLines) {
      this.wireframeLines.geometry.dispose();
      (this.wireframeLines.material as THREE.Material).dispose();
    }
    for (const d of this.pickArrowDisposables) d.dispose();
    this.renderer?.dispose();
  }

  resize(width: number, height: number) {
    if (!this.renderer) return;
    this.viewW = width;
    this.viewH = height;
    this.renderer.setSize(width, height, false);
    this.updateFrustum();
  }

  private updateFrustum() {
    const aspect = this.viewH > 0 ? this.viewW / this.viewH : 1;
    this.camera.left = -this.orthoHalf * aspect;
    this.camera.right = this.orthoHalf * aspect;
    this.camera.top = this.orthoHalf;
    this.camera.bottom = -this.orthoHalf;
    this.camera.updateProjectionMatrix();
  }

  /** Snap to a named orthographic preset, re-framing the part. The default
   *  reproduces the on-load isometric (see setModel); the six axis views look
   *  straight down a world axis. Top/bottom take up = +Y (Z is the view axis);
   *  the rest keep the printer's Z-up. */
  setCameraView(view: CameraView) {
    if (!this.camera || !this.controls) return;

    // Re-centre on the part; fall back to the live orbit target with no model.
    const center = new THREE.Vector3();
    const b = this.partBbox;
    if (b) center.set((b[0] + b[3]) / 2, (b[1] + b[4]) / 2, (b[2] + b[5]) / 2);
    else center.copy(this.controls.target);

    // dir: offset from the part toward the camera (parallel projection, so its
    // length only sets clipping, not framing). up: which way is up on screen.
    let dir: THREE.Vector3;
    let up: THREE.Vector3;
    switch (view) {
      case "top":
        dir = new THREE.Vector3(0, 0, 1);
        up = new THREE.Vector3(0, 1, 0);
        break;
      case "bottom":
        dir = new THREE.Vector3(0, 0, -1);
        up = new THREE.Vector3(0, 1, 0);
        break;
      case "front":
        dir = new THREE.Vector3(0, -1, 0);
        up = new THREE.Vector3(0, 0, 1);
        break;
      case "behind":
        dir = new THREE.Vector3(0, 1, 0);
        up = new THREE.Vector3(0, 0, 1);
        break;
      case "left":
        dir = new THREE.Vector3(-1, 0, 0);
        up = new THREE.Vector3(0, 0, 1);
        break;
      case "right":
        dir = new THREE.Vector3(1, 0, 0);
        up = new THREE.Vector3(0, 0, 1);
        break;
      default: // isometric, identical to the on-load fit in setModel
        dir = new THREE.Vector3(0.7, -0.8, 0.55);
        up = new THREE.Vector3(0, 0, 1);
        break;
    }

    const dist = this.bboxDiag * 2.2;
    this.camera.up.copy(up);
    this.camera.position.copy(center).addScaledVector(dir, dist);
    this.controls.target.copy(center);
    this.camera.near = this.bboxDiag / 100;
    this.camera.far = this.bboxDiag * 50;
    this.camera.zoom = 1;
    this.orthoHalf = this.bboxDiag * 0.62;
    this.updateFrustum();
    this.camera.lookAt(center);
    this.controls.update();
    // Re-pivot the next orbit drag on the part centre, not a stale surface hit.
    this.lastOrbitPivot = center.clone();
  }

  setTool(tool: Tool, brushRadius: number, brushErase: boolean) {
    this.tool = tool;
    this.brushRadius = brushRadius;
    this.brushErase = brushErase;
    this.controls.enabled = tool !== "brush";
    if (this.brushCursor) this.brushCursor.visible = tool === "brush";
    // The pick-direction preview arrow follows the pointer (shown on hover in
    // onPointerMove); just clear it when leaving the tool.
    if (this.pickArrow && tool !== "pickdir") this.pickArrow.visible = false;
    if (tool !== "select" && tool !== "place") this.setHover(null);
  }

  // ---------- axis gizmo ----------

  private buildGizmo() {
    const axes: [string, number, THREE.Vector3][] = [
      ["X", 0xe5534b, new THREE.Vector3(1, 0, 0)],
      ["Y", 0x57ab5a, new THREE.Vector3(0, 1, 0)],
      ["Z", 0x539bf5, new THREE.Vector3(0, 0, 1)],
    ];
    for (const [label, color, dir] of axes) {
      const arrow = new THREE.ArrowHelper(dir, new THREE.Vector3(), 1.05, color, 0.34, 0.16);
      this.gizmoScene.add(arrow);
      const sprite = makeTextSprite(label, color);
      sprite.position.copy(dir).multiplyScalar(1.45);
      this.gizmoScene.add(sprite);
    }
    const origin = new THREE.Mesh(
      new THREE.SphereGeometry(0.09, 12, 8),
      new THREE.MeshBasicMaterial({ color: 0x6e7173 })
    );
    this.gizmoScene.add(origin);
  }

  // ---------- model ----------

  setModel(model: LoadedModel) {
    if (this.mesh) {
      this.scene.remove(this.mesh);
      this.geometry?.dispose();
      (this.mesh.material as THREE.Material).dispose();
      // Null immediately: cleanup below triggers refreshView, which must
      // not touch the old geometry with new-sized buffers (set() with a
      // longer source throws "offset is out of bounds").
      this.mesh = null;
      this.geometry = null;
    }
    this.triCount = model.triCount;
    this.basePositions = new Float32Array(model.positions);
    this.colors = new Float32Array(this.triCount * 9);
    this.displacements = null;
    this.dispComponent = -1;
    this.lastDispRange = null;
    this.vertexDensity = null;
    this.rbmMode = null;
    this.viewMode = "setup";
    this.setRegions(null);
    this.setVoxelMesh(null, null);
    this.setBuildGhost(null);
    this.setBuildActive(null);
    this.setOptShape(null, null);

    this.geometry = new THREE.BufferGeometry();
    this.geometry.setAttribute("position", new THREE.BufferAttribute(model.positions, 3));
    this.geometry.setAttribute("color", new THREE.BufferAttribute(this.colors, 3));
    this.uvs = new Float32Array(this.triCount * 3 * 2);
    this.geometry.setAttribute("uv", new THREE.BufferAttribute(this.uvs, 2));
    this.geometry.computeVertexNormals();
    this.scalarMode = "none";

    const material = new THREE.MeshStandardMaterial({
      vertexColors: true,
      metalness: 0.05,
      roughness: 0.72,
      side: THREE.DoubleSide,
      // Push the fill back a hair so the wireframe overlay never z-fights it.
      polygonOffset: true,
      polygonOffsetFactor: 1,
      polygonOffsetUnits: 1,
    });
    this.mesh = new THREE.Mesh(this.geometry, material);
    this.scene.add(this.mesh);
    this.buildWireframe();

    this.setPatchIds(model.patchIds);
    this.bcs = [];
    this.activeBcId = null;
    this.scalarField = null;
    this.rebuildBcMarkers();
    this.repaint();

    // Fit camera (parallel projection: frustum half-height from the bbox).
    const [lx, ly, lz, hx, hy, hz] = model.bbox;
    this.partBbox = model.bbox;
    const center = new THREE.Vector3((lx + hx) / 2, (ly + hy) / 2, (lz + hz) / 2);
    this.bboxDiag = Math.hypot(hx - lx, hy - ly, hz - lz) || 100;
    const dist = this.bboxDiag * 2.2;
    this.camera.position.set(center.x + dist * 0.7, center.y - dist * 0.8, center.z + dist * 0.55);
    this.controls.target.copy(center);
    this.camera.near = this.bboxDiag / 100;
    this.camera.far = this.bboxDiag * 50;
    this.camera.zoom = 1;
    this.orthoHalf = this.bboxDiag * 0.62;
    this.updateFrustum();
    this.controls.update();

    if (!this.brushCursor) {
      const geo = new THREE.SphereGeometry(1, 24, 16);
      const mat = new THREE.MeshBasicMaterial({
        color: 0xff6b6b,
        transparent: true,
        opacity: 0.3,
        depthWrite: false,
      });
      this.brushCursor = new THREE.Mesh(geo, mat);
      this.brushCursor.visible = false;
      this.scene.add(this.brushCursor);
    }

    if (!this.pickCursor) {
      // A small 3-axis crosshair, drawn on top (depthTest off) so it reads as a
      // "click here" marker regardless of viewing angle. Scaled per-frame to the
      // part size when shown.
      const pts = new Float32Array([
        -1, 0, 0, 1, 0, 0, 0, -1, 0, 0, 1, 0, 0, 0, -1, 0, 0, 1,
      ]);
      const geo = new THREE.BufferGeometry();
      geo.setAttribute("position", new THREE.BufferAttribute(pts, 3));
      const mat = new THREE.LineBasicMaterial({
        color: 0xf76707, // CNC-orange accent — "actionable"
        depthTest: false,
        transparent: true,
      });
      this.pickCursor = new THREE.LineSegments(geo, mat);
      this.pickCursor.renderOrder = 999;
      this.pickCursor.visible = false;
      this.scene.add(this.pickCursor);
    }

    // Section plane follows the new part.
    if (this.sectionTranslate) {
      this.sectionProxy.position.copy(this.controls.target);
      this.buildSectionQuad(); // resize to the new part
      this.syncSectionFromProxy();
      this.rebuildCapGroups();
    }
    if (this.symTranslate) this.buildSymQuad(); // symmetry plane too
    this.refreshClipping();
  }

  /** Same part, new pose (orientation tools): swap the positions in place —
   *  triangle count, patches, and BC selections are pose-invariant. */
  updateModelPositions(positions: Float32Array, bbox: LoadedModel["bbox"]) {
    if (!this.mesh || !this.geometry || !this.basePositions) return;
    if (positions.length !== this.basePositions.length) return;
    const attr = this.geometry.getAttribute("position") as THREE.BufferAttribute;
    (attr.array as Float32Array).set(positions);
    attr.needsUpdate = true;
    this.geometry.computeVertexNormals();
    this.geometry.computeBoundingBox();
    this.geometry.computeBoundingSphere();
    this.basePositions = new Float32Array(positions);
    this.partBbox = bbox;
    const [lx, ly, lz, hx, hy, hz] = bbox;
    this.bboxDiag = Math.hypot(hx - lx, hy - ly, hz - lz) || this.bboxDiag;
    this.controls.target.set((lx + hx) / 2, (ly + hy) / 2, (lz + hz) / 2);
    this.controls.update();
    this.buildWireframe(); // re-derive from the moved geometry
    this.rebuildBcMarkers();
    this.repaint();
  }

  setPatchIds(patchIds: Uint32Array) {
    this.patchIds = patchIds;
    this.patchToTris.clear();
    for (let t = 0; t < patchIds.length; t++) {
      const p = patchIds[t];
      let list = this.patchToTris.get(p);
      if (!list) {
        list = [];
        this.patchToTris.set(p, list);
      }
      list.push(t);
    }
    this.hoverPatch = null;
    this.repaint();
  }

  // ---------- BC display ----------

  setBcs(bcs: Bc[], activeBcId: string | null, inactive?: Set<string>) {
    this.bcs = bcs;
    this.activeBcId = activeBcId;
    this.inactiveBcs = inactive ?? new Set();
    this.repaint();
    this.rebuildBcMarkers();
  }

  /** Toggle the triangle-mesh wireframe overlay (mesh inspection). */
  setWireframe(on: boolean) {
    this.wireframeOn = on;
    if (on && !this.wireframeLines) this.buildWireframe();
    this.refreshView();
  }

  /** (Re)build the wireframe overlay from the current model geometry. Every
   *  triangle edge is drawn (THREE.WireframeGeometry, not EdgesGeometry) so
   *  the actual triangulation shows, not just the silhouette creases. */
  private buildWireframe() {
    if (this.wireframeLines) {
      this.scene.remove(this.wireframeLines);
      this.wireframeLines.geometry.dispose();
      (this.wireframeLines.material as THREE.Material).dispose();
      this.wireframeLines = null;
    }
    if (!this.geometry) return;
    const wf = new THREE.WireframeGeometry(this.geometry);
    const mat = new THREE.LineBasicMaterial({
      color: 0x334155,
      transparent: true,
      opacity: 0.55,
    });
    this.wireframeLines = new THREE.LineSegments(wf, mat);
    // Match the current toggle + view so a model loaded with wireframe already
    // on shows immediately; refreshView keeps it in sync afterwards.
    this.wireframeLines.visible =
      this.wireframeOn && (this.viewMode === "setup" || this.viewMode === "mesh");
    this.scene.add(this.wireframeLines);
  }

  /** Force arrows + classic support triangles (4-sided cones read as ▽). */
  private rebuildBcMarkers() {
    for (const d of this.markerDisposables) d.dispose();
    this.markerDisposables = [];
    this.bcMarkers.clear();
    if (!this.basePositions) return;
    for (const bc of this.bcs) {
      if (bc.tris.length === 0) continue;
      const inactive = this.inactiveBcs.has(bc.id);
      if (bc.kind === "force" && bc.force) {
        const f = new THREE.Vector3(...bc.force);
        if (f.lengthSq() === 0) continue;
        const centroid = this.selectionCentroid(bc.tris);
        const dir = f.clone().normalize();
        const len = this.bboxDiag * 0.18;
        // Solid shaft + cone (NOT ArrowHelper): its line shaft disappears
        // when the arrow is viewed end-on — e.g. a -Z force from a top-down
        // camera — leaving a context-free floating dot. A shaded cylinder
        // stays readable from every angle. Deactivated in this step → ghosted.
        const mat = new THREE.MeshStandardMaterial({
          color: 0xff5252,
          roughness: 0.45,
          metalness: 0.05,
          transparent: inactive,
          opacity: inactive ? 0.25 : 1,
        });
        const shaftLen = len * 0.72;
        const shaftGeo = new THREE.CylinderGeometry(len * 0.025, len * 0.025, shaftLen, 10);
        const headGeo = new THREE.ConeGeometry(len * 0.07, len * 0.28, 14);
        this.markerDisposables.push(mat, shaftGeo, headGeo);
        const g = new THREE.Group();
        const shaft = new THREE.Mesh(shaftGeo, mat);
        shaft.position.y = shaftLen / 2;
        const head = new THREE.Mesh(headGeo, mat);
        head.position.y = shaftLen + len * 0.14;
        g.add(shaft, head);
        // Value label at the tail: even with the arrow viewed dead-on (a
        // -Z force from a top view collapses it to a disc) the annotation
        // says what the dot is.
        const mag = f.length();
        const label = this.makeLabelSprite(
          `${mag >= 9.95 ? mag.toFixed(0) : mag.toFixed(1)} N`,
          0xc2330e
        );
        if (inactive) (label.material as THREE.SpriteMaterial).opacity = 0.3;
        label.position.set(0, -len * 0.02, 0);
        g.add(label);
        // Local +Y becomes the force direction (the head is the +Y end).
        g.quaternion.setFromUnitVectors(new THREE.Vector3(0, 1, 0), dir);
        // Keep the WHOLE arrow outside the part. A pushing load (into the
        // surface) keeps its head at the surface with the shaft trailing
        // outward; a pulling load (away from the surface) sits tail-on-surface
        // so the head is at the far, outer end — never driven into the part.
        const n = this.selectionNormal(bc.tris);
        const pulling = n ? f.dot(n) >= 0 : false;
        g.position.copy(pulling ? centroid : centroid.clone().sub(dir.clone().multiplyScalar(len)));
        this.bcMarkers.add(g);
      } else if (
        bc.kind === "fixed" ||
        bc.kind === "frictionless" ||
        bc.kind === "displacement" ||
        bc.kind === "elastic"
      ) {
        this.buildSupportGlyphs(bc, inactive);
      }
    }
    this.updateMarkerVisibility();
  }

  private buildSupportGlyphs(bc: Bc, inactive = false) {
    const p = this.basePositions!;
    // Triangle centroids + outward normals + areas of the selection.
    const items: { c: THREE.Vector3; n: THREE.Vector3; a: number }[] = [];
    const e1 = new THREE.Vector3();
    const e2 = new THREE.Vector3();
    for (const t of bc.tris) {
      const o = 9 * t;
      const a = new THREE.Vector3(p[o], p[o + 1], p[o + 2]);
      const b = new THREE.Vector3(p[o + 3], p[o + 4], p[o + 5]);
      const c = new THREE.Vector3(p[o + 6], p[o + 7], p[o + 8]);
      e1.subVectors(b, a);
      e2.subVectors(c, a);
      const n = new THREE.Vector3().crossVectors(e1, e2);
      const len = n.length();
      if (len < 1e-12) continue;
      n.divideScalar(len);
      items.push({ c: a.add(b).add(c).divideScalar(3), n, a: len });
    }
    if (!items.length) return;
    // Greedy farthest-point sampling, seeded at the largest triangle.
    items.sort((u, v) => v.a - u.a);
    const chosen = [items[0]];
    const minD2 = items.map((it) => it.c.distanceToSquared(items[0].c));
    const spacing2 = (0.06 * this.bboxDiag) ** 2;
    while (chosen.length < 12) {
      let best = -1;
      let bd = spacing2;
      for (let i = 0; i < items.length; i++) {
        if (minD2[i] > bd) {
          bd = minD2[i];
          best = i;
        }
      }
      if (best < 0) break;
      chosen.push(items[best]);
      for (let i = 0; i < items.length; i++) {
        minD2[i] = Math.min(minD2[i], items[i].c.distanceToSquared(items[best].c));
      }
    }
    const hCone = 0.034 * this.bboxDiag;
    const rCone = 0.017 * this.bboxDiag;
    // 4 radial segments: from any side the cone reads as the textbook ▽.
    const coneGeo = new THREE.ConeGeometry(rCone, hCone, 4);
    const mat = new THREE.MeshStandardMaterial({
      color: BC_COLORS[bc.kind],
      roughness: 0.5,
      metalness: 0.05,
      flatShading: true,
      transparent: inactive,
      opacity: inactive ? 0.25 : 1,
    });
    this.markerDisposables.push(coneGeo, mat);
    const up = new THREE.Vector3(0, 1, 0);
    let plateGeo: THREE.CylinderGeometry | null = null;
    if (bc.kind === "frictionless") {
      plateGeo = new THREE.CylinderGeometry(rCone * 1.25, rCone * 1.25, rCone * 0.18, 16);
      this.markerDisposables.push(plateGeo);
    }
    // Elastic: the textbook spring symbol — a coil between surface and cone.
    let coilGeo: THREE.TubeGeometry | null = null;
    const coilH = 0.55 * hCone;
    if (bc.kind === "elastic") {
      const turns = 3.5;
      const coilR = rCone * 0.45;
      const pts: THREE.Vector3[] = [];
      for (let i = 0; i <= 40; i++) {
        const t = i / 40;
        const ang = t * turns * Math.PI * 2;
        pts.push(new THREE.Vector3(Math.cos(ang) * coilR, t * coilH, Math.sin(ang) * coilR));
      }
      coilGeo = new THREE.TubeGeometry(new THREE.CatmullRomCurve3(pts), 60, coilR * 0.22, 5, false);
      this.markerDisposables.push(coilGeo);
    }
    for (const it of chosen) {
      // Tip touches the surface; body sticks outward along the normal.
      // Frictionless: small gap + plate = "support that can slide".
      // Elastic: coil at the surface, cone behind it = "support on a spring".
      const gap =
        bc.kind === "frictionless" ? 0.35 * hCone : bc.kind === "elastic" ? coilH : 0;
      const cone = new THREE.Mesh(coneGeo, mat);
      cone.quaternion.setFromUnitVectors(up, it.n.clone().negate());
      cone.position.copy(it.c).addScaledVector(it.n, hCone / 2 + gap);
      this.bcMarkers.add(cone);
      if (plateGeo) {
        const plate = new THREE.Mesh(plateGeo, mat);
        plate.quaternion.setFromUnitVectors(up, it.n);
        plate.position.copy(it.c).addScaledVector(it.n, rCone * 0.12);
        this.bcMarkers.add(plate);
      }
      if (coilGeo) {
        const coil = new THREE.Mesh(coilGeo, mat);
        coil.quaternion.setFromUnitVectors(up, it.n);
        coil.position.copy(it.c);
        this.bcMarkers.add(coil);
      }
    }
  }

  private updateMarkerVisibility() {
    this.bcMarkers.visible = this.viewMode === "setup";
  }

  private selectionCentroid(tris: Uint32Array): THREE.Vector3 {
    const p = this.basePositions!;
    const c = new THREE.Vector3();
    let n = 0;
    for (const t of tris) {
      for (let v = 0; v < 3; v++) {
        c.x += p[9 * t + 3 * v];
        c.y += p[9 * t + 3 * v + 1];
        c.z += p[9 * t + 3 * v + 2];
        n++;
      }
    }
    return n ? c.multiplyScalar(1 / n) : c;
  }

  /** Area-weighted average outward normal of a triangle selection (null when
   *  degenerate) — mirrors the engine's `average_normal`. */
  private selectionNormal(tris: Uint32Array): THREE.Vector3 | null {
    const p = this.basePositions;
    if (!p) return null;
    const acc = new THREE.Vector3();
    const e1 = new THREE.Vector3();
    const e2 = new THREE.Vector3();
    const cr = new THREE.Vector3();
    for (const t of tris) {
      const o = 9 * t;
      e1.set(p[o + 3] - p[o], p[o + 4] - p[o + 1], p[o + 5] - p[o + 2]);
      e2.set(p[o + 6] - p[o], p[o + 7] - p[o + 1], p[o + 8] - p[o + 2]);
      cr.crossVectors(e1, e2); // 2 × area × unit normal
      acc.add(cr);
    }
    return acc.lengthSq() > 1e-20 ? acc.normalize() : null;
  }

  /** Recompute the full per-triangle color buffer. */
  private repaint() {
    if (!this.colors || !this.geometry) return;
    const triColor: (THREE.Color | null)[] = new Array(this.triCount).fill(null);
    for (const bc of this.bcs) {
      const col = BC_COLORS[bc.kind];
      const isActive = bc.id === this.activeBcId;
      const c = isActive ? col.clone().lerp(new THREE.Color(0xffffff), 0.25) : col;
      for (const t of bc.tris) triColor[t] = c;
    }
    this.triBcColor = triColor;
    const hover = this.hoverPatch !== null ? this.patchToTris.get(this.hoverPatch) : undefined;
    const hoverSet = hover ? new Set(hover) : null;
    for (let t = 0; t < this.triCount; t++) {
      let c = triColor[t] ?? BASE_COLOR;
      if (hoverSet?.has(t)) {
        c = triColor[t] ? triColor[t]!.clone().lerp(HOVER_TINT, 0.65) : HOVER_TINT;
      }
      for (let v = 0; v < 3; v++) {
        this.colors[9 * t + 3 * v] = c.r;
        this.colors[9 * t + 3 * v + 1] = c.g;
        this.colors[9 * t + 3 * v + 2] = c.b;
      }
    }
    const attr = this.geometry.getAttribute("color") as THREE.BufferAttribute;
    attr.clearUpdateRanges(); // a full rewrite supersedes any pending hover range
    this.colorsDirtyFull = true;
    attr.needsUpdate = true;
  }

  /** Hover is only ever a 2-patch delta, so don't rebuild the whole color buffer
   *  (a ~1.8M-float rewrite + full VBO upload on a big STL). Restore the tris of
   *  the previously-hovered patch to their base color, tint the newly-hovered
   *  patch, and upload only the touched span. Base colors live in `triBcColor`,
   *  kept current by repaint(). */
  private setHover(patch: number | null) {
    if (patch === this.hoverPatch) return;
    const prev = this.hoverPatch;
    this.hoverPatch = patch;
    if (!this.colors || !this.geometry) return;
    let lo = Infinity;
    let hi = -Infinity;
    const paint = (t: number, c: THREE.Color) => {
      const o = 9 * t;
      for (let v = 0; v < 3; v++) {
        this.colors![o + 3 * v] = c.r;
        this.colors![o + 3 * v + 1] = c.g;
        this.colors![o + 3 * v + 2] = c.b;
      }
      if (t < lo) lo = t;
      if (t > hi) hi = t;
    };
    if (prev !== null) {
      const tris = this.patchToTris.get(prev);
      if (tris) for (const t of tris) paint(t, this.triBcColor[t] ?? BASE_COLOR);
    }
    if (patch !== null) {
      const tris = this.patchToTris.get(patch);
      if (tris)
        for (const t of tris) {
          const base = this.triBcColor[t];
          paint(t, base ? this._hoverCol.copy(base).lerp(HOVER_TINT, 0.65) : HOVER_TINT);
        }
    }
    if (hi < lo) return; // both patches empty / unknown
    const attr = this.geometry.getAttribute("color") as THREE.BufferAttribute;
    // A full rewrite is already queued this frame: don't add a partial range, or
    // three would upload only it and drop the rest of the rewrite.
    if (!this.colorsDirtyFull) attr.addUpdateRange(9 * lo, 9 * (hi - lo + 1));
    attr.needsUpdate = true;
  }

  // ---------- picking ----------

  private rayTri(ev: PointerEvent): THREE.Intersection | null {
    if (!this.mesh) return null;
    const rect = this.renderer.domElement.getBoundingClientRect();
    this.pointer.x = ((ev.clientX - rect.left) / rect.width) * 2 - 1;
    this.pointer.y = -((ev.clientY - rect.top) / rect.height) * 2 + 1;
    this.raycaster.setFromCamera(this.pointer, this.camera);
    const hits = this.raycaster.intersectObject(this.mesh, false);
    return hits.length ? hits[0] : null;
  }

  /** Outward geometric normal of a soup triangle (winding order). */
  private triNormalOf(faceIndex: number): THREE.Vector3 | null {
    const p = this.basePositions;
    if (!p) return null;
    const o = 9 * faceIndex;
    const e1 = new THREE.Vector3(p[o + 3] - p[o], p[o + 4] - p[o + 1], p[o + 5] - p[o + 2]);
    const e2 = new THREE.Vector3(p[o + 6] - p[o], p[o + 7] - p[o + 1], p[o + 8] - p[o + 2]);
    const n = e1.cross(e2);
    return n.lengthSq() > 1e-20 ? n.normalize() : null;
  }

  /** Lazily build the reusable pick-direction preview arrow (unit length along
   *  +Y; scaled + oriented per hover). */
  private ensurePickArrow() {
    if (this.pickArrow) return;
    // Drawn on top (depthTest off, high renderOrder) like the old crosshair, so
    // the preview is always visible even when the normal aims into the part.
    const mat = new THREE.MeshBasicMaterial({ color: 0xf76707, depthTest: false, transparent: true });
    const shaftGeo = new THREE.CylinderGeometry(0.04, 0.04, 0.72, 12);
    const headGeo = new THREE.ConeGeometry(0.11, 0.3, 16);
    this.pickArrowDisposables.push(mat, shaftGeo, headGeo);
    const shaft = new THREE.Mesh(shaftGeo, mat);
    shaft.position.y = 0.36;
    shaft.renderOrder = 999;
    const head = new THREE.Mesh(headGeo, mat);
    head.position.y = 0.86;
    head.renderOrder = 999;
    const g = new THREE.Group();
    g.add(shaft, head);
    g.renderOrder = 999;
    g.visible = false;
    this.pickArrow = g;
    this.scene.add(g);
  }

  private onPointerMove = (ev: PointerEvent) => {
    if (this.orbiting) return; // camera drag in progress — skip hover/brush
    if (!this.mesh) return;
    this.updateProbe(ev);
    if (this.tool === "select" || this.tool === "place") {
      const hit = this.rayTri(ev);
      const patch =
        hit && hit.faceIndex != null && this.patchIds ? this.patchIds[hit.faceIndex] : null;
      this.setHover(patch);
    } else if (this.tool === "brush") {
      const hit = this.rayTri(ev);
      if (hit && this.brushCursor) {
        this.brushCursor.visible = true;
        this.brushCursor.position.copy(hit.point);
        this.brushCursor.scale.setScalar(this.brushRadius);
      } else if (this.brushCursor) {
        this.brushCursor.visible = false;
      }
      if (this.brushing && hit) this.applyBrush(hit.point);
    } else if (this.tool === "pickdir") {
      // Live preview: an arrow at the hovered point along that face's outward
      // normal — the force direction a click would set.
      const hit = this.rayTri(ev);
      const n = hit && hit.faceIndex != null ? this.triNormalOf(hit.faceIndex) : null;
      if (hit && n) {
        this.ensurePickArrow();
        const a = this.pickArrow!;
        a.visible = true;
        a.position.copy(hit.point);
        a.quaternion.setFromUnitVectors(new THREE.Vector3(0, 1, 0), n);
        a.scale.setScalar(this.bboxDiag * 0.18);
      } else if (this.pickArrow) {
        this.pickArrow.visible = false;
      }
    }
  };

  private onPointerDown = (ev: PointerEvent) => {
    if (ev.button !== 0 || !this.mesh) return;
    // (Modifier-gated callout gestures are claimed earlier, in the capture-phase
    // onAnnoDownCapture, so they never reach OrbitControls or this handler.)
    // Arm a pivot orbit on every left-press in a navigable tool. The camera
    // only moves once the drag passes a threshold, so a plain click still
    // selects/places without disturbing the view.
    if (this.controls.enabled) this.beginOrbit(ev);
    if (this.tool === "select") {
      const hit = this.rayTri(ev);
      if (hit && hit.faceIndex != null && this.patchIds) {
        const patch = this.patchIds[hit.faceIndex];
        const tris = this.patchToTris.get(patch);
        if (tris) this.callbacks.onPickPatch?.(new Uint32Array(tris), !ev.shiftKey);
      }
    } else if (this.tool === "place" || this.tool === "pickdir") {
      const hit = this.rayTri(ev);
      const n = hit && hit.faceIndex != null ? this.triNormalOf(hit.faceIndex) : null;
      if (n) {
        if (this.tool === "place") this.callbacks.onPlaceFace?.([n.x, n.y, n.z]);
        else this.callbacks.onPickDir?.([n.x, n.y, n.z]);
      }
    } else if (this.tool === "brush") {
      this.brushing = true;
      const hit = this.rayTri(ev);
      if (hit) this.applyBrush(hit.point);
    }
  };

  private onPointerUp = () => {
    this.brushing = false;
  };

  private applyBrush(point: THREE.Vector3) {
    if (!this.basePositions) return;
    const r2 = this.brushRadius * this.brushRadius;
    const p = this.basePositions;
    const hit: number[] = [];
    for (let t = 0; t < this.triCount; t++) {
      const cx = (p[9 * t] + p[9 * t + 3] + p[9 * t + 6]) / 3;
      const cy = (p[9 * t + 1] + p[9 * t + 4] + p[9 * t + 7]) / 3;
      const cz = (p[9 * t + 2] + p[9 * t + 5] + p[9 * t + 8]) / 3;
      const dx = cx - point.x;
      const dy = cy - point.y;
      const dz = cz - point.z;
      if (dx * dx + dy * dy + dz * dz <= r2) hit.push(t);
    }
    if (hit.length) this.callbacks.onBrush?.(new Uint32Array(hit), this.brushErase);
  }

  // ---------- navigation (orbit / move / zoom) ----------
  // Faithful port of the bumpMesh (stlTexturizer) camera routine: left-drag
  // orbits around the surface point under the cursor with no polar clamping,
  // the wheel zooms toward the cursor, and right-drag pans in screen space.

  private installNavigation(canvas: HTMLCanvasElement) {
    // Small red sphere marking the live orbit centre (drawn over the part).
    const marker = new THREE.Mesh(
      new THREE.SphereGeometry(1, 16, 10),
      new THREE.MeshBasicMaterial({ color: 0xff2222, depthTest: false })
    );
    marker.renderOrder = 10;
    marker.visible = false;
    this.pivotMarker = marker;
    this.scene.add(marker);
    // Move + release on document so a drag that leaves the canvas still tracks.
    document.addEventListener("pointermove", this.onOrbitMove);
    document.addEventListener("pointerup", this.onOrbitUp);
    document.addEventListener("pointermove", this.onAnnoMove);
    document.addEventListener("pointerup", this.onAnnoUp);
    document.addEventListener("keydown", this.onAnnoKey);
    document.addEventListener("keydown", this.onViewKey);
    canvas.addEventListener("wheel", this.onWheel, { passive: false });
  }

  /** Visible meshes the orbit ray can land on (setup STL + every result
   *  surface), so the pivot follows whatever the user is actually looking at. */
  private orbitTargets(): THREE.Object3D[] {
    const list: THREE.Object3D[] = [];
    const add = (o: THREE.Object3D | null | undefined) => {
      if (o && (o as THREE.Mesh).isMesh && o.visible) list.push(o);
    };
    if (this.mesh?.visible) list.push(this.mesh);
    if (this.voxelGroup.visible) this.voxelGroup.children.forEach(add);
    if (this.voxRes?.group.visible) this.voxRes.group.children.forEach(add);
    add(this.optShapeMesh);
    for (const m of this.regionMeshes) add(m);
    return list;
  }

  /** Nearest surface point under the cursor, or null if the ray misses. */
  private pickPoint(ev: PointerEvent): THREE.Vector3 | null {
    const targets = this.orbitTargets();
    if (!targets.length) return null;
    const rect = this.renderer.domElement.getBoundingClientRect();
    this.pointer.x = ((ev.clientX - rect.left) / rect.width) * 2 - 1;
    this.pointer.y = -((ev.clientY - rect.top) / rect.height) * 2 + 1;
    this.raycaster.setFromCamera(this.pointer, this.camera);
    const hits = this.raycaster.intersectObjects(targets, false);
    return hits.length ? hits[0].point.clone() : null;
  }

  private beginOrbit(ev: PointerEvent) {
    // Orbit about the surface under the cursor; fall back to the last pivot,
    // then to the controls target, so a drag off the part still rotates.
    const pivot = this.pickPoint(ev) ?? this.lastOrbitPivot ?? this.controls.target.clone();
    this.orbitPivot = pivot.clone();
    this.lastOrbitPivot = pivot.clone();
    this.orbitStart = { x: ev.clientX, y: ev.clientY };
    this.orbitLast = { x: ev.clientX, y: ev.clientY };
    this.orbiting = false; // promoted once the drag passes the threshold
  }

  private showPivotMarker() {
    const m = this.pivotMarker;
    if (!m || !this.orbitPivot) return;
    m.position.copy(this.orbitPivot);
    // ~1.5% of the visible frustum height: same apparent size at any zoom.
    m.scale.setScalar((this.camera.top / this.camera.zoom) * 0.015);
    m.visible = true;
  }

  private onOrbitMove = (ev: PointerEvent) => {
    if (!this.orbitPivot || !this.orbitLast || !this.controls.enabled) return;
    if (!this.orbiting) {
      const moved = Math.hypot(ev.clientX - this.orbitStart!.x, ev.clientY - this.orbitStart!.y);
      if (moved < 3) return; // tolerate a click without flashing the marker
      this.orbiting = true;
      this.showPivotMarker();
      if (this.pickArrow) this.pickArrow.visible = false; // don't freeze it mid-orbit
    }
    const dx = ev.clientX - this.orbitLast.x;
    const dy = ev.clientY - this.orbitLast.y;
    this.orbitLast = { x: ev.clientX, y: ev.clientY };
    if (dx === 0 && dy === 0) return;

    const pivot = this.orbitPivot;
    const rotSpeed = 0.005;
    // Pure quaternion rotation: yaw about world Z, pitch about the camera's
    // right axis.
    this.camera.updateMatrixWorld();
    this._oRight.setFromMatrixColumn(this.camera.matrixWorld, 0).normalize(); // camera right

    // Pitch clamp: keep the view direction clear of the ±Z pole. Yaw about Z
    // doesn't change the tilt; only pitch can drive the look direction onto the
    // Z axis. There, OrbitControls' per-frame `lookAt(target)` with up = +Z is
    // degenerate and the azimuth snaps 180° — and a fast drag can shove the
    // camera *past* the pole in one frame, so the snap flip-flops. Limiting the
    // pitch to land just shy of vertical turns that jump into a clean stop.
    const eps = this.poleEps;
    this._oDir.copy(this.controls.target).sub(this.camera.position).normalize();
    let pitch = -dy * rotSpeed;
    const ang0 = Math.acos(Math.max(-1, Math.min(1, this._oDir.z))); // tilt from +Z, [0,π]
    this._oq2.setFromAxisAngle(this._oRight, pitch);
    const angP = Math.acos(
      Math.max(-1, Math.min(1, this._oTmp.copy(this._oDir).applyQuaternion(this._oq2).z))
    );
    if (angP < eps || angP > Math.PI - eps) {
      const clamped = Math.max(eps, Math.min(Math.PI - eps, angP));
      const denom = angP - ang0;
      pitch = Math.abs(denom) > 1e-9 ? pitch * ((clamped - ang0) / denom) : 0;
    }

    this._oq1.setFromAxisAngle(this._oTmp.set(0, 0, 1), -dx * rotSpeed);
    this._oq2.setFromAxisAngle(this._oRight, pitch);
    this._oq1.premultiply(this._oq2);

    // Swing both the camera and the orbit target around the pivot so
    // OrbitControls (which owns damping + pan) stays consistent.
    this._oTmp.copy(this.camera.position).sub(pivot).applyQuaternion(this._oq1);
    this.camera.position.copy(pivot).add(this._oTmp);
    this._oTmp2.copy(this.controls.target).sub(pivot).applyQuaternion(this._oq1);
    this.controls.target.copy(pivot).add(this._oTmp2);
    this.camera.quaternion.premultiply(this._oq1);
    this.camera.updateMatrixWorld();
  };

  private onOrbitUp = () => {
    if (!this.orbitPivot) return;
    this.orbitPivot = null;
    this.orbitStart = null;
    this.orbitLast = null;
    if (this.orbiting) {
      this.orbiting = false;
      // Re-level: hand the up vector back to OrbitControls upright.
      this.camera.up.set(0, 0, 1);
      this.camera.lookAt(this.controls.target);
    }
    if (this.pivotMarker) this.pivotMarker.visible = false;
  };

  /** Cursor-centric zoom: keep the world point under the cursor pinned while
   *  the orthographic frustum scales. */
  private onWheel = (ev: WheelEvent) => {
    if (!this.controls.enabled) return;
    ev.preventDefault();
    const rect = this.renderer.domElement.getBoundingClientRect();
    const ndcX = ((ev.clientX - rect.left) / rect.width) * 2 - 1;
    const ndcY = -((ev.clientY - rect.top) / rect.height) * 2 + 1;
    this._oTmp.set(ndcX, ndcY, 0).unproject(this.camera);
    const factor = ev.deltaY > 0 ? 1 / 1.1 : 1.1;
    this.camera.zoom = Math.max(0.05, Math.min(200, this.camera.zoom * factor));
    this.camera.updateProjectionMatrix();
    this._oTmp2.set(ndcX, ndcY, 0).unproject(this.camera);
    this._oTmp.sub(this._oTmp2);
    this.camera.position.add(this._oTmp);
    this.controls.target.add(this._oTmp);
    this.controls.update();
  };

  // ---------- WebGL context loss ----------

  /** True between `webglcontextlost` and `webglcontextrestored`. three.js
   *  already preventDefaults, no-ops its own render(), and rebuilds the GL
   *  backend on restore (resources re-upload on the next render). We add the
   *  app layer: skip our own GL calls during the gap (so nothing touches the
   *  dead context) and surface a notice; the rAF loop keeps running and repaints
   *  itself once the context is back. */
  private contextLost = false;

  private onGlLost = (e: Event) => {
    e.preventDefault(); // ensure the browser will fire webglcontextrestored
    this.contextLost = true;
    this.callbacks.onContextLost?.(true);
  };

  private onGlRestored = () => {
    this.contextLost = false;
    this.callbacks.onContextLost?.(false);
    // three re-initialized the GL backend (its listener ran first); the loop's
    // next tick re-uploads geometry/materials and repaints. Nothing to rebuild.
  };

  // ---------- hover value probe ----------

  /** Formatter for the cursor value readout; null disables the probe. */
  setProbeFormatter(fmt: ((v: number) => string) | null) {
    this.probeFormat = fmt;
    if (!fmt && this.probeEl) this.probeEl.style.display = "none";
  }

  /** The surface currently carrying probeable values, with a per-vertex
   *  value accessor (vertex index into the non-indexed soup). */
  private probeSource(): { mesh: THREE.Mesh; valueAt: (i: number) => number } | null {
    if (this.voxResultActive()) {
      const vr = this.voxRes!;
      const m = vr.group.children.find((c): c is THREE.Mesh => c instanceof THREE.Mesh);
      if (!m) return null;
      const sf = this.scalarField;
      if (sf && sf.values.length * 2 === vr.uvs.length) {
        return { mesh: m, valueAt: (i) => sf.values[i] };
      }
      const d = vr.disp;
      return { mesh: m, valueAt: (i) => this.dispValueAt(d, i) };
    }
    if (this.viewMode === "deformed" && this.mesh && this.displacements) {
      const sf = this.scalarField;
      if (sf && this.uvs && sf.values.length * 2 === this.uvs.length) {
        return { mesh: this.mesh, valueAt: (i) => sf.values[i] };
      }
      const d = this.displacements;
      return { mesh: this.mesh, valueAt: (i) => this.dispValueAt(d, i) };
    }
    if (
      (this.viewMode === "density" || this.viewMode === "infill") &&
      this.mesh &&
      this.vertexDensity
    ) {
      const v = this.vertexDensity;
      return { mesh: this.mesh, valueAt: (i) => v[i] };
    }
    if (this.viewMode === "mesh" && this.meshDensity && this.voxelDensity) {
      const hull = this.voxelGroup.children.find(
        (c): c is THREE.Mesh => c instanceof THREE.Mesh
      );
      if (!hull) return null;
      const v = this.voxelDensity;
      return { mesh: hull, valueAt: (i) => v[i] };
    }
    return null;
  }

  private updateProbe(ev: PointerEvent) {
    const el = this.probeEl;
    if (!el) return;
    const fmt = this.probeFormat;
    const src = fmt ? this.probeSource() : null;
    if (!src || !src.mesh.visible) {
      el.style.display = "none";
      return;
    }
    const rect = this.renderer.domElement.getBoundingClientRect();
    this.pointer.x = ((ev.clientX - rect.left) / rect.width) * 2 - 1;
    this.pointer.y = -((ev.clientY - rect.top) / rect.height) * 2 + 1;
    this.raycaster.setFromCamera(this.pointer, this.camera);
    const hits = this.raycaster.intersectObject(src.mesh, false);
    const hit = hits.length ? hits[0] : null;
    if (!hit || hit.faceIndex == null) {
      el.style.display = "none";
      return;
    }
    // Barycentric interpolation on the CURRENTLY DISPLAYED (possibly
    // deformed) triangle — the ray hit that geometry.
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
    const v =
      bary.x * src.valueAt(3 * f) +
      bary.y * src.valueAt(3 * f + 1) +
      bary.z * src.valueAt(3 * f + 2);
    el.textContent = fmt!(v);
    el.style.display = "block";
    el.style.left = `${ev.clientX - rect.left + 14}px`;
    el.style.top = `${ev.clientY - rect.top + 14}px`;
  }

  // ---------- fixed value callouts (contour views) ----------

  /** Capture-phase pointerdown on the viewport parent — runs BEFORE OrbitControls'
   *  own canvas listener so we can claim a modifier gesture and stop it. (Orbit-
   *  Controls remaps shift/ctrl + left-drag to a PAN; returning from the bubble-
   *  phase handler is too late, so we block it here.) */
  private onAnnoDownCapture = (ev: PointerEvent) => {
    if (ev.button !== 0 || !this.mesh) return;
    const mode = ev.ctrlKey ? "point" : ev.shiftKey ? "max" : ev.altKey ? "min" : null;
    if (!mode || !this.probeFormat || !this.probeSource()) return;
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

  /** Ctrl/⌘ + 0–6 snap the camera to a named orthographic view (slicer
   *  convention): 0 default ISO, 1 top, 2 bottom, 3 front, 4 behind, 5 left,
   *  6 right. */
  private onViewKey = (ev: KeyboardEvent) => {
    if ((!ev.ctrlKey && !ev.metaKey) || ev.altKey || ev.shiftKey) return;
    // Don't hijack the digit while typing in a field (e.g. legend bound editor).
    const t = ev.target as HTMLElement | null;
    if (t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName))) return;
    const view = VIEW_KEYS[ev.key];
    if (!view) return;
    ev.preventDefault();
    this.setCameraView(view);
  };

  private drawAnnoRect() {
    const r = this.annoRectEl;
    const d = this.annoDrag;
    if (!r || !d) return;
    const rect = this.renderer.domElement.getBoundingClientRect();
    r.style.display = "block";
    r.style.left = `${Math.min(d.x0, d.x1) - rect.left}px`;
    r.style.top = `${Math.min(d.y0, d.y1) - rect.top}px`;
    r.style.width = `${Math.abs(d.x1 - d.x0)}px`;
    r.style.height = `${Math.abs(d.y1 - d.y0)}px`;
  }

  /** Ctrl-click: the field value interpolated at the clicked surface point. */
  private addPointCallout(clientX: number, clientY: number) {
    const src = this.probeSource();
    if (!src || !src.mesh.visible) return;
    const rect = this.renderer.domElement.getBoundingClientRect();
    this.pointer.x = ((clientX - rect.left) / rect.width) * 2 - 1;
    this.pointer.y = -((clientY - rect.top) / rect.height) * 2 + 1;
    this.raycaster.setFromCamera(this.pointer, this.camera);
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
    const src = this.probeSource();
    if (!src || !src.mesh.visible) return;
    const rect = this.renderer.domElement.getBoundingClientRect();
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
    const n = (pos.length / 3) | 0;
    let bestI = -1;
    let bestV = mode === "max" ? -Infinity : Infinity;
    for (let i = 0; i < n; i++) {
      p.set(pos[3 * i], pos[3 * i + 1], pos[3 * i + 2]).applyMatrix4(mw).project(this.camera);
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
    const parent = this.canvas?.parentElement;
    if (!parent || !this.annoSvg) return;
    const label = this.probeFormat ? this.probeFormat(c.value) : `${c.value}`;
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
      this.callbacks.onLog?.(
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
    const v = w.project(this.camera); // mutates the shared scratch in place
    if (v.z < -1 || v.z > 1) return hide();
    const x = (v.x * 0.5 + 0.5) * this.viewW;
    const y = (-v.y * 0.5 + 0.5) * this.viewH;
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

  private updateCallouts() {
    for (const c of this.callouts) this.projectCallout(c);
  }

  /** Drop all callouts — a new result field / view / surface invalidates them. */
  private clearCallouts() {
    for (const c of this.callouts) {
      c.dot.remove();
      c.chip.remove();
      c.line.remove();
    }
    this.callouts.length = 0;
  }

  // ---------- rigid-body-mode animation ----------

  setRbmMode(mode: { t: number[]; r: number[]; center: number[] } | null) {
    this.rbmMode = mode;
    if (mode && this.basePositions) {
      // Normalize amplitude: peak surface motion = 6% of bbox diagonal.
      let maxU = 1e-12;
      const p = this.basePositions;
      for (let i = 0; i < p.length; i += 3) {
        const u = this.modeDisplacement(mode, p[i], p[i + 1], p[i + 2]);
        maxU = Math.max(maxU, Math.hypot(u[0], u[1], u[2]));
      }
      this.rbmAmp = (0.06 * this.bboxDiag) / maxU;
    }
    if (!mode) this.applyPositions(); // restore
  }

  private modeDisplacement(
    mode: { t: number[]; r: number[]; center: number[] },
    x: number,
    y: number,
    z: number
  ): [number, number, number] {
    const dx = x - mode.center[0];
    const dy = y - mode.center[1];
    const dz = z - mode.center[2];
    return [
      mode.t[0] + mode.r[1] * dz - mode.r[2] * dy,
      mode.t[1] + mode.r[2] * dx - mode.r[0] * dz,
      mode.t[2] + mode.r[0] * dy - mode.r[1] * dx,
    ];
  }

  // ---------- result views ----------

  setDisplacements(disp: Float32Array | null, stats: { maxDisplacement: number } | null) {
    this.displacements = disp;
    // A new solution resets the field picker to |u| (store side); keep the
    // coloring component in step so it never colors by a stale X/Y/Z choice.
    this.dispComponent = -1;
    this.lastDispRange = null;
    if (disp && stats && stats.maxDisplacement > 0) {
      this.autoScale = (0.08 * this.bboxDiag) / stats.maxDisplacement;
    } else {
      this.autoScale = 1;
    }
    this.callbacks.onAutoScale?.(this.autoScale);
    this.refreshView();
  }

  /** Choose what the deformed view colors by: -1 = |u| magnitude, 0/1/2 =
   *  signed X/Y/Z displacement component. */
  setDispComponent(comp: number) {
    if (this.dispComponent === comp) return;
    this.clearCallouts(); // values belong to the previous field
    this.dispComponent = comp;
    this.lastDispRange = null; // force a fresh range report for the new field
    this.refreshView();
  }

  /** Per-vertex scalar for the active displacement field: |u| magnitude or the
   *  signed component. `d` is the surface's 3-per-vertex displacement buffer. */
  private dispValueAt(d: Float32Array, i: number): number {
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
    this.callbacks.onResultRange?.(min, max);
  }

  setVertexDensity(density: Float32Array | null) {
    this.vertexDensity = density;
    this.refreshView();
  }

  setDeformAnimate(on: boolean) {
    this.deformAnimate = on;
    if (!on) this.applyPositions(); // restore full deflection
  }

  /** Flat-shaded soup mesh for the live build preview. */
  private buildHullMesh(positions: Float32Array, ghost: boolean): THREE.Mesh {
    const geo = new THREE.BufferGeometry();
    geo.setAttribute("position", new THREE.BufferAttribute(positions, 3));
    geo.computeVertexNormals();
    const mat = new THREE.MeshStandardMaterial({
      color: ghost ? 0x9aa0a6 : 0xe8722b,
      roughness: 0.85,
      metalness: 0.05,
      flatShading: true,
      side: THREE.DoubleSide,
      transparent: ghost,
      opacity: ghost ? 0.12 : 1,
      depthWrite: !ghost,
    });
    return new THREE.Mesh(geo, mat);
  }

  private disposeMesh(obj: THREE.Object3D | null) {
    if (!obj) return;
    const m = obj as THREE.Mesh;
    m.geometry?.dispose();
    (m.material as THREE.Material | undefined)?.dispose();
  }

  /** Faint full-hull ghost (the deactivated voxels) for the build preview.
   *  null clears it. */
  setBuildGhost(positions: Float32Array | null) {
    if (this.buildGhost) {
      this.buildGroup.remove(this.buildGhost);
      this.disposeMesh(this.buildGhost);
      this.buildGhost = null;
    }
    if (positions && positions.length) {
      this.buildGhost = this.buildHullMesh(positions, true);
      this.buildGroup.add(this.buildGhost);
    }
    this.updateBuildVisibility();
  }

  /** Growing deformed active hull (already-printed voxels, exaggeration baked
   *  in). Replaced each preview frame; null clears it. */
  setBuildActive(positions: Float32Array | null) {
    if (this.buildActive) {
      this.buildGroup.remove(this.buildActive);
      this.disposeMesh(this.buildActive);
      this.buildActive = null;
    }
    if (positions && positions.length) {
      this.buildActive = this.buildHullMesh(positions, false);
      this.buildGroup.add(this.buildActive);
    }
    this.updateBuildVisibility();
  }

  /** While the preview is up, hide the normal model/voxel/BC views; on clear,
   *  restore them via refreshView. */
  private updateBuildVisibility() {
    const on = !!(this.buildGhost || this.buildActive);
    this.buildGroup.visible = on;
    if (on) {
      if (this.mesh) this.mesh.visible = false;
      this.voxelGroup.visible = false;
      this.bcMarkers.visible = false;
    } else {
      this.refreshView();
    }
  }

  setVoxelMesh(
    hull: Float32Array | null,
    edges: Float32Array | null,
    density?: Float32Array | null
  ) {
    for (const d of this.voxelDisposables) d.dispose();
    this.voxelDisposables = [];
    this.voxelGroup.clear();
    this.voxelDensity = density ?? null;
    if (hull && hull.length) {
      const geo = new THREE.BufferGeometry();
      geo.setAttribute("position", new THREE.BufferAttribute(hull, 3));
      geo.setAttribute("color", new THREE.BufferAttribute(new Float32Array(hull.length), 3));
      geo.computeVertexNormals(); // soup → flat per-face normals
      const mat = new THREE.MeshStandardMaterial({
        color: 0xffffff, // actual color lives in the vertex attribute
        vertexColors: true,
        roughness: 0.85,
        metalness: 0.05,
        flatShading: true,
        side: THREE.DoubleSide,
        polygonOffset: true,
        polygonOffsetFactor: 1,
        polygonOffsetUnits: 1,
      });
      this.voxelDisposables.push(geo, mat);
      this.voxelGroup.add(new THREE.Mesh(geo, mat));
      this.applyMeshTint();
    }
    if (edges && edges.length) {
      const geo = new THREE.BufferGeometry();
      geo.setAttribute("position", new THREE.BufferAttribute(edges, 3));
      const mat = new THREE.LineBasicMaterial({ color: 0x2a2d30, transparent: true, opacity: 0.45 });
      this.voxelDisposables.push(geo, mat);
      this.voxelGroup.add(new THREE.LineSegments(geo, mat));
    }
    if (this.sectionTranslate) this.rebuildCapGroups();
    this.refreshClipping();
    this.refreshView();
  }

  /** Voxel hull + exact nodal displacements for the results view
   *  (alternate result surface; nulls clear it). */
  setVoxelResult(
    positions: Float32Array | null,
    disp: Float32Array | null,
    edges: Float32Array | null,
    edgeDisp: Float32Array | null
  ) {
    if (this.voxRes) {
      this.scene.remove(this.voxRes.group);
      for (const d of this.voxResDisposables) d.dispose();
    }
    this.voxRes = null;
    this.voxResDisposables = [];
    if (positions && disp && positions.length) {
      const group = new THREE.Group();
      const geo = new THREE.BufferGeometry();
      // The attribute gets a copy: the original stays as the morph base.
      geo.setAttribute("position", new THREE.BufferAttribute(positions.slice(), 3));
      const uvs = new Float32Array((positions.length / 3) * 2);
      geo.setAttribute("uv", new THREE.BufferAttribute(uvs, 2));
      geo.computeVertexNormals(); // soup → flat per-face normals
      const mat = new THREE.MeshStandardMaterial({
        map: this.lutJet,
        roughness: 0.85,
        metalness: 0.05,
        flatShading: true,
        side: THREE.DoubleSide,
        polygonOffset: true,
        polygonOffsetFactor: 1,
        polygonOffsetUnits: 1,
      });
      this.voxResDisposables.push(geo, mat);
      group.add(new THREE.Mesh(geo, mat));
      let lineGeo: THREE.BufferGeometry | null = null;
      let lineBase: Float32Array | null = null;
      let lineDisp: Float32Array | null = null;
      if (edges && edgeDisp && edges.length) {
        lineGeo = new THREE.BufferGeometry();
        lineGeo.setAttribute("position", new THREE.BufferAttribute(edges.slice(), 3));
        const lmat = new THREE.LineBasicMaterial({
          color: 0x2a2d30,
          transparent: true,
          opacity: 0.35,
        });
        this.voxResDisposables.push(lineGeo, lmat);
        group.add(new THREE.LineSegments(lineGeo, lmat));
        lineBase = edges;
        lineDisp = edgeDisp;
      }
      group.visible = false;
      this.scene.add(group);
      this.voxRes = { group, geo, base: positions, disp, uvs, lineGeo, lineBase, lineDisp };
    }
    this.refreshClipping();
    this.refreshView();
  }

  /** Switch the deformed view between the smooth STL and the voxel hull. */
  setResultSurface(surface: "stl" | "voxel") {
    if (this.resultSurface === surface) return;
    this.clearCallouts(); // pinned to the previous surface's vertices
    this.resultSurface = surface;
    this.refreshView();
  }

  /** Voxel result surface currently driving the deformed view. */
  private voxResultActive(): boolean {
    return this.viewMode === "deformed" && this.resultSurface === "voxel" && !!this.voxRes;
  }

  /** Color the mesh-view cells by element density (0–1 ramp). */
  setMeshDensity(on: boolean) {
    this.meshDensity = on;
    this.applyMeshTint();
  }

  /** Voxel-true section active: the cut lives in the geometry, so the voxel
   *  group must NOT also be plane-clipped (and its stencil cap hides). */
  setVoxelCutActive(on: boolean) {
    if (this.voxelCutActive === on) return;
    this.voxelCutActive = on;
    this.refreshClipping();
    this.updateSectionVisibility();
  }

  private applyMeshTint() {
    const hullMesh = this.voxelGroup.children.find(
      (c): c is THREE.Mesh => c instanceof THREE.Mesh
    );
    if (!hullMesh) return;
    const colors = hullMesh.geometry.getAttribute("color") as THREE.BufferAttribute | undefined;
    if (!colors) return;
    const arr = colors.array as Float32Array;
    const density = this.voxelDensity;
    // Element-density plot: the same blue→cyan→yellow→red ramp as the
    // infill-density legend, plain 0–1 scale (1 = solid skin). Off: the
    // flat chassis gray-blue.
    const c = new THREE.Color();
    for (let v = 0; v < arr.length / 3; v++) {
      if (this.meshDensity && density) {
        c.setRGB(...ramp(Math.min(1, Math.max(0, density[v]))));
        arr[3 * v] = c.r;
        arr[3 * v + 1] = c.g;
        arr[3 * v + 2] = c.b;
      } else {
        arr[3 * v] = 0.494;
        arr[3 * v + 1] = 0.545;
        arr[3 * v + 2] = 0.6;
      }
    }
    colors.needsUpdate = true;
  }

  /** Live optimization skeleton or density-threshold cutaway mesh. When a
   *  per-vertex density scalar is provided, it is colored through the same
   *  ramp LUT as the density legend. */
  setOptShape(
    positions: Float32Array | null,
    indices: Uint32Array | null,
    density?: Float32Array | null
  ) {
    if (this.optShapeMesh) {
      this.scene.remove(this.optShapeMesh);
      this.optShapeMesh.geometry.dispose();
      (this.optShapeMesh.material as THREE.Material).dispose();
      this.optShapeMesh = null;
    }
    if (positions && indices && indices.length) {
      const geo = new THREE.BufferGeometry();
      geo.setAttribute("position", new THREE.BufferAttribute(positions, 3));
      geo.setIndex(new THREE.BufferAttribute(indices, 1));
      geo.computeVertexNormals();
      let mat: THREE.MeshStandardMaterial;
      if (density && density.length * 3 === positions.length) {
        const uv = new Float32Array(density.length * 2);
        for (let i = 0; i < density.length; i++) {
          uv[2 * i] = Math.min(1, density[i] / 0.8);
          uv[2 * i + 1] = 0.5;
        }
        geo.setAttribute("uv", new THREE.BufferAttribute(uv, 2));
        mat = new THREE.MeshStandardMaterial({
          map: this.lutRamp,
          roughness: 0.55,
          metalness: 0.05,
          side: THREE.DoubleSide,
          // Bias the body slightly toward the camera so a coincident ghost hull
          // loses the depth test cleanly (no z-fight moiré) where they overlap.
          polygonOffset: true,
          polygonOffsetFactor: -1,
          polygonOffsetUnits: -1,
        });
      } else {
        mat = new THREE.MeshStandardMaterial({
          color: 0xd9974f,
          roughness: 0.55,
          metalness: 0.05,
          side: THREE.DoubleSide,
          polygonOffset: true,
          polygonOffsetFactor: -1,
          polygonOffsetUnits: -1,
        });
      }
      this.optShapeMesh = new THREE.Mesh(geo, mat);
      this.scene.add(this.optShapeMesh);
    }
    this.refreshClipping();
    this.refreshView();
  }

  setRegionVisibility(vis: boolean[]) {
    this.regionVisible = vis;
    this.refreshView();
  }

  /** Part Topo result: the body replaces the part, so hide the envelope hull in
   *  the density/regions views and render the body opaque. */
  setResultSolid(solid: boolean) {
    this.resultSolid = solid;
    this.refreshView();
  }

  /** Render the CURRENT view (just the main scene — no axis gizmo) to a square
   *  PNG for the 3MF plate thumbnail. The render must run synchronously right
   *  before the readback (the WebGL context has no preserveDrawingBuffer).
   *  Returns the PNG bytes, or null on any failure (export falls back to a
   *  placeholder). */
  /** Thumbnail for the 3MF: frame the optimized result alone, from the app's
   *  default isometric direction, filling a square — independent of wherever
   *  the user's live camera happens to be. Renders OFF-SCREEN (its own square
   *  target + temp camera, only the result meshes + lights visible) so the
   *  on-screen view never flickers and nothing has to be saved/restored on the
   *  live camera. Falls back to a square grab of the live view if there's no
   *  result to frame. */
  captureThumbnail(size = 512): Uint8Array | null {
    const r = this.renderer;
    if (!r) return null;
    // Graded/binary show the original part as a translucent envelope (like the
    // viewport); Part Topo's body IS the result, so no envelope there.
    const showGhost = !this.resultSolid && !!this.mesh;
    // Frame the WHOLE part (envelope for graded/binary, else the result body) so
    // nothing is cropped and it stays centered.
    const box = new THREE.Box3();
    const tmp = new THREE.Box3();
    const addBox = (m: THREE.Mesh) => {
      m.geometry.computeBoundingBox();
      if (m.geometry.boundingBox) {
        tmp.copy(m.geometry.boundingBox).applyMatrix4(m.matrixWorld);
        box.union(tmp);
      }
    };
    this.regionMeshes.forEach((m, i) => {
      if (this.regionVisible[i] !== false) addBox(m);
    });
    if (showGhost && this.mesh) addBox(this.mesh);
    if (box.isEmpty()) return this.captureViewportSquare(size);

    const center = box.getCenter(new THREE.Vector3());
    const sphere = box.getBoundingSphere(new THREE.Sphere());
    const dir = new THREE.Vector3(120, -160, 110).normalize(); // app default view
    const cam = new THREE.OrthographicCamera(-1, 1, 1, -1, 0.01, sphere.radius * 8 + 10);
    cam.up.set(0, 0, 1);
    cam.position.copy(center).addScaledVector(dir, sphere.radius * 4 + 1);
    cam.lookAt(center);
    cam.updateMatrixWorld();
    // Fit a SQUARE frustum to the projected bbox so the part fills the frame.
    const right = new THREE.Vector3().setFromMatrixColumn(cam.matrixWorld, 0);
    const up = new THREE.Vector3().setFromMatrixColumn(cam.matrixWorld, 1);
    const corner = new THREE.Vector3();
    let half = 1e-6;
    for (let xi = 0; xi < 2; xi++)
      for (let yi = 0; yi < 2; yi++)
        for (let zi = 0; zi < 2; zi++) {
          corner
            .set(xi ? box.max.x : box.min.x, yi ? box.max.y : box.min.y, zi ? box.max.z : box.min.z)
            .sub(center);
          half = Math.max(half, Math.abs(corner.dot(right)), Math.abs(corner.dot(up)));
        }
    half *= 1.08; // small margin
    cam.left = -half;
    cam.right = half;
    cam.top = half;
    cam.bottom = -half;
    cam.updateProjectionMatrix();

    // Show only the result meshes + lights (+ the ghost envelope); the scene
    // background still paints.
    const ghost = showGhost && this.mesh ? this.mesh : null;
    const snap = this.scene.children.map((o) => o.visible);
    for (const o of this.scene.children) {
      o.visible =
        o instanceof THREE.Light || o === ghost || this.regionMeshes.includes(o as THREE.Mesh);
    }
    this.regionMeshes.forEach((m, i) => {
      if (this.regionVisible[i] === false) m.visible = false;
    });
    // The part's live material may be opaque / stress-colored; swap a flat
    // translucent gray so the envelope reads like the viewport ghost. Render it
    // behind the regions.
    let savedMat: THREE.Material | THREE.Material[] | null = null;
    let savedOrder = 0;
    let ghostMat: THREE.MeshStandardMaterial | null = null;
    if (ghost) {
      savedMat = ghost.material;
      savedOrder = ghost.renderOrder;
      ghostMat = new THREE.MeshStandardMaterial({
        color: 0xc9c6bf,
        transparent: true,
        opacity: 0.16,
        depthWrite: false,
        side: THREE.DoubleSide,
        roughness: 0.95,
      });
      ghost.material = ghostMat;
      ghost.renderOrder = -1; // under the (renderOrder ≥ 0) region meshes
    }

    const rt = new THREE.WebGLRenderTarget(size, size);
    let bytes: Uint8Array | null = null;
    try {
      const prev = r.getRenderTarget();
      r.setScissorTest(false);
      r.setRenderTarget(rt);
      r.setViewport(0, 0, size, size);
      r.clear();
      r.render(this.scene, cam);
      const rgba = new Uint8Array(size * size * 4);
      r.readRenderTargetPixels(rt, 0, 0, size, size, rgba);
      r.setRenderTarget(prev);
      bytes = this.rgbaToPng(rgba, size);
    } catch {
      bytes = null;
    } finally {
      rt.dispose();
      if (ghost && savedMat) {
        ghost.material = savedMat;
        ghost.renderOrder = savedOrder;
      }
      ghostMat?.dispose();
      this.scene.children.forEach((o, i) => (o.visible = snap[i]));
    }
    return bytes;
  }

  /** Encode a bottom-up RGBA buffer (WebGL read) into PNG bytes, flipping rows
   *  to top-down and forcing opaque. */
  private rgbaToPng(rgba: Uint8Array, size: number): Uint8Array | null {
    const c = document.createElement("canvas");
    c.width = size;
    c.height = size;
    const ctx = c.getContext("2d");
    if (!ctx) return null;
    const img = ctx.createImageData(size, size);
    const row = size * 4;
    for (let y = 0; y < size; y++) {
      const s = (size - 1 - y) * row;
      const d = y * row;
      for (let x = 0; x < row; x += 4) {
        img.data[d + x] = rgba[s + x];
        img.data[d + x + 1] = rgba[s + x + 1];
        img.data[d + x + 2] = rgba[s + x + 2];
        img.data[d + x + 3] = 255;
      }
    }
    ctx.putImageData(img, 0, 0);
    return pngBytesFromCanvas(c);
  }

  /** Fallback: center-square crop of the live on-screen render. */
  private captureViewportSquare(size: number): Uint8Array | null {
    try {
      const r = this.renderer;
      if (!r || this.viewW <= 0 || this.viewH <= 0) return null;
      r.setScissorTest(false);
      r.setViewport(0, 0, this.viewW, this.viewH);
      r.clear();
      r.render(this.scene, this.camera);
      const src = r.domElement;
      const side = Math.min(src.width, src.height);
      const sx = (src.width - side) / 2;
      const sy = (src.height - side) / 2;
      const c = document.createElement("canvas");
      c.width = size;
      c.height = size;
      const ctx = c.getContext("2d");
      if (!ctx) return null;
      ctx.fillStyle = "#dedcd6"; // match the scene background
      ctx.fillRect(0, 0, size, size);
      ctx.drawImage(src, sx, sy, side, side, 0, 0, size, size);
      return pngBytesFromCanvas(c);
    } catch {
      return null;
    }
  }

  setRegions(regions: OptRegion[] | null) {
    for (const m of this.regionMeshes) {
      this.scene.remove(m);
      m.geometry.dispose();
      (m.material as THREE.Material).dispose();
    }
    this.regionMeshes = [];
    this.regionVisible = [];
    if (!regions) {
      this.refreshView();
      return;
    }
    const c = new THREE.Color();
    regions.forEach((r, i) => {
      const geo = new THREE.BufferGeometry();
      geo.setAttribute("position", new THREE.BufferAttribute(r.positions, 3));
      geo.setIndex(new THREE.BufferAttribute(r.indices, 1));
      geo.computeVertexNormals();
      c.setRGB(...ramp(Math.min(1, r.density / 0.8)));
      // Part Topo body: opaque single surface (no nested modifiers to see
      // through). Infill modifiers stay translucent so the nesting is visible.
      const mat = new THREE.MeshStandardMaterial({
        color: c.clone(),
        transparent: !this.resultSolid,
        opacity: this.resultSolid ? 1.0 : 0.62,
        roughness: 0.6,
        metalness: 0.0,
        depthWrite: this.resultSolid,
        side: THREE.DoubleSide,
      });
      const mesh = new THREE.Mesh(geo, mat);
      // Regions are strictly nested (each denser region sits INSIDE the sparser
      // one) so they share a centroid; three.js' distance-based transparency
      // sort then flips as the camera orbits, and the outer hull overdraws the
      // inner ones — the "only one region visible" bug. Pin the painter's order
      // by density rank: regions arrive outer→inner (ascending density), so draw
      // outer first and the dense core last (on top). Stable from every angle.
      mesh.renderOrder = i;
      mesh.visible = false;
      this.scene.add(mesh);
      this.regionMeshes.push(mesh);
    });
    this.refreshClipping();
    this.refreshView();
  }

  setViewState(mode: ViewMode, deformScale: number) {
    if (this.viewMode !== mode) this.clearCallouts(); // callouts are per-view
    this.viewMode = mode;
    this.deformScale = deformScale;
    this.refreshView();
  }

  /** Stress/strain scalars per soup vertex; null reverts to |u| coloring.
   *  `flip` inverts the colormap (safety factor: red = the critical LOW).
   *  `signed` centers the color scale on 0 (signed von Mises: blue =
   *  compression, green ≈ unloaded, red = tension) — must match the store's
   *  symmetric `fieldRange` so the legend agrees with the surface. */
  setScalarField(values: Float32Array | null, flip = false, signed = false) {
    this.clearCallouts(); // values belong to the previous field
    if (values && values.length) {
      let min = Infinity;
      let max = -Infinity;
      for (let i = 0; i < values.length; i++) {
        min = Math.min(min, values[i]);
        max = Math.max(max, values[i]);
      }
      if (signed) {
        const m = Math.max(Math.abs(min), Math.abs(max), 1e-12);
        min = -m;
        max = m;
      }
      this.scalarField = { values, min, max, flip };
    } else {
      this.scalarField = null;
    }
    this.refreshView();
  }

  /** Clamp the color scale to a user range (null = auto). */
  setLegendRange(min: number | null, max: number | null) {
    this.legendRange = { min, max };
    this.refreshView();
  }

  /** Toggle the min/max location markers; unit drives label formatting. */
  setShowExtremes(on: boolean, unit: string) {
    this.extremesOn = on;
    this.extremesUnit = unit;
    this.refreshView();
  }

  // ---------- section plane ----------

  setSection(on: boolean) {
    this.sectionOn = on;
    if (on) this.ensureSectionObjects();
    this.refreshClipping();
    this.refreshView();
    if (on) this.emitSectionMoved(); // mesh view recuts from the plane
  }

  flipSection() {
    this.sectionProxy.rotateX(Math.PI); // local +Z (= plane normal) flips
    this.syncSectionFromProxy();
  }

  setSectionAxis(axis: "x" | "y" | "z") {
    const n =
      axis === "x"
        ? new THREE.Vector3(1, 0, 0)
        : axis === "y"
          ? new THREE.Vector3(0, 1, 0)
          : new THREE.Vector3(0, 0, 1);
    this.sectionProxy.quaternion.setFromUnitVectors(new THREE.Vector3(0, 0, 1), n);
    this.syncSectionFromProxy();
  }

  // ---------- symmetry plane (optimizer constraint) ----------

  /** Show/update the symmetry plane n·p = c. `enabled` is the store-side
   *  gate (checkbox on + Optimize step active + not running); the scene
   *  additionally hides the plane in result views. */
  setSymmetry(enabled: boolean, normal: [number, number, number], c: number) {
    this.symEnabled = enabled;
    if (enabled) this.ensureSymObjects();
    if (!this.symTranslate) return;
    const n = new THREE.Vector3(...normal);
    if (n.lengthSq() < 1e-12) n.set(1, 0, 0);
    n.normalize();
    this.symProxy.quaternion.setFromUnitVectors(new THREE.Vector3(0, 0, 1), n);
    // Any point on the plane works; pick the one closest to the part center
    // so the gizmo and quad sit centered on the part (robust to camera pans).
    const ctr = this.partCenter();
    const d = n.dot(ctr) - c;
    this.symProxy.position.copy(ctr).addScaledVector(n, -d);
    this.updateSymQuadSize(); // refit the quad to the part at this orientation
    this.updateSymVisibility();
  }

  /** Part bbox center in world mm, falling back to the orbit target before a
   *  model is loaded. */
  private partCenter(): THREE.Vector3 {
    const b = this.partBbox;
    return b
      ? new THREE.Vector3((b[0] + b[3]) / 2, (b[1] + b[4]) / 2, (b[2] + b[5]) / 2)
      : this.controls.target.clone();
  }

  private emitSymmetryMoved() {
    const n = new THREE.Vector3(0, 0, 1).applyQuaternion(this.symProxy.quaternion);
    // Tilting the plane (rotate ring) changes which part dimensions it spans —
    // refit the quad live so it always reads as "the size of the part".
    this.updateSymQuadSize();
    this.callbacks.onSymmetryMoved?.([n.x, n.y, n.z], n.dot(this.symProxy.position));
  }

  /** Hide the plane outside editing contexts (result views). */
  private updateSymVisibility() {
    const show =
      this.symEnabled &&
      this.viewMode !== "deformed" &&
      this.viewMode !== "density" &&
      this.viewMode !== "infill";
    this.symProxy.visible = show;
    for (const tc of [this.symTranslate, this.symRotate]) {
      if (tc) {
        tc.enabled = show;
        tc.getHelper().visible = show;
      }
    }
  }

  private ensureSymObjects() {
    if (this.symTranslate) {
      this.buildSymQuad(); // refresh size to the current part
      return;
    }
    this.scene.add(this.symProxy);
    const make = (
      mode: "translate" | "rotate",
      size: number,
      cfg: (tc: TransformControls) => void
    ) => {
      const tc = new TransformControls(this.camera, this.renderer.domElement);
      tc.setMode(mode);
      tc.setSpace("local");
      tc.setSize(size);
      cfg(tc);
      tc.addEventListener("dragging-changed", (e: { value?: unknown }) => {
        this.controls.enabled = !e.value && this.tool !== "brush";
      });
      tc.addEventListener("objectChange", () => this.emitSymmetryMoved());
      tc.attach(this.symProxy);
      this.scene.add(tc.getHelper());
      return tc;
    };
    // Same combined gizmo as the section plane: translate along the normal
    // only, two rotation rings (spinning about the normal is a no-op).
    this.symTranslate = make("translate", 0.7, (tc) => {
      tc.showX = false;
      tc.showY = false;
    });
    this.symRotate = make("rotate", 1.0, (tc) => {
      tc.showZ = false;
    });
    this.buildSymQuad();
  }

  /** Translucent orange rectangle marking the symmetry plane (child of the
   *  proxy — distinct from the blue section plane). Built as a unit quad and
   *  scaled to the part by {@link updateSymQuadSize}. */
  private buildSymQuad() {
    if (this.symQuad) {
      this.symProxy.remove(this.symQuad);
      for (const d of this.symQuadDisposables) d.dispose();
      this.symQuadDisposables = [];
    }
    const group = new THREE.Group();
    const quadGeo = new THREE.PlaneGeometry(1, 1);
    const quadMat = new THREE.MeshBasicMaterial({
      color: 0xd97706,
      transparent: true,
      opacity: 0.1,
      side: THREE.DoubleSide,
      depthWrite: false,
    });
    const edgeGeo = new THREE.EdgesGeometry(quadGeo);
    const edgeMat = new THREE.LineBasicMaterial({
      color: 0xd97706,
      transparent: true,
      opacity: 0.8,
    });
    this.symQuadDisposables.push(quadGeo, quadMat, edgeGeo, edgeMat);
    group.add(new THREE.Mesh(quadGeo, quadMat));
    group.add(new THREE.LineSegments(edgeGeo, edgeMat));
    this.symQuad = group;
    this.symProxy.add(group);
    this.updateSymQuadSize();
  }

  /** Scale the unit quad so it spans the part: its two in-plane axes get the
   *  extent of the part's AABB projected onto them (so an axis-aligned plane
   *  is exactly the perpendicular part dimensions, and a tilted one stays the
   *  silhouette size). Falls back to the bbox diagonal before a model loads. */
  private updateSymQuadSize() {
    if (!this.symQuad) return;
    const b = this.partBbox;
    if (!b) {
      const d = this.bboxDiag * 1.1;
      this.symQuad.scale.set(d, d, 1);
      return;
    }
    const dim = new THREE.Vector3(b[3] - b[0], b[4] - b[1], b[5] - b[2]);
    const q = this.symProxy.quaternion;
    const ex = new THREE.Vector3(1, 0, 0).applyQuaternion(q);
    const ey = new THREE.Vector3(0, 1, 0).applyQuaternion(q);
    // Projected AABB extent along a world axis e: Σ |e·axis|·dim_axis.
    const span = (e: THREE.Vector3) =>
      Math.abs(e.x) * dim.x + Math.abs(e.y) * dim.y + Math.abs(e.z) * dim.z;
    this.symQuad.scale.set(span(ex) || 1, span(ey) || 1, 1);
  }

  // ---------- section plane objects ----------

  private ensureSectionObjects() {
    if (!this.sectionTranslate) {
      this.sectionProxy.position.copy(this.controls.target);
      this.sectionProxy.quaternion.setFromUnitVectors(
        new THREE.Vector3(0, 0, 1),
        new THREE.Vector3(1, 0, 0)
      );
      this.scene.add(this.sectionProxy);
      const make = (mode: "translate" | "rotate", size: number, cfg: (tc: TransformControls) => void) => {
        const tc = new TransformControls(this.camera, this.renderer.domElement);
        tc.setMode(mode);
        tc.setSpace("local");
        tc.setSize(size);
        cfg(tc);
        tc.addEventListener("dragging-changed", (e: { value?: unknown }) => {
          this.controls.enabled = !e.value && this.tool !== "brush";
        });
        tc.addEventListener("objectChange", () => this.syncSectionFromProxy());
        tc.attach(this.sectionProxy);
        this.scene.add(tc.getHelper());
        return tc;
      };
      // One combined gizmo: the plane cuts everything, so tangential motion
      // is meaningless — only the normal arrow translates; two rings rotate
      // (spinning about the normal is a no-op and stays hidden).
      this.sectionTranslate = make("translate", 0.75, (tc) => {
        tc.showX = false;
        tc.showY = false;
      });
      this.sectionRotate = make("rotate", 1.05, (tc) => {
        tc.showZ = false;
      });
      this.buildSectionQuad();
      this.syncSectionFromProxy();
    }
    this.rebuildCapGroups();
  }

  /** Translucent plane rectangle, child of the proxy so it is ALWAYS
   *  centered on the gizmo (PlaneHelper centers on the world origin's
   *  foot point instead, which strands the gizmo off to one side). */
  private buildSectionQuad() {
    if (this.sectionQuad) {
      this.sectionProxy.remove(this.sectionQuad);
      for (const d of this.sectionQuadDisposables) d.dispose();
      this.sectionQuadDisposables = [];
    }
    const d = this.bboxDiag * 1.15;
    const group = new THREE.Group();
    const quadGeo = new THREE.PlaneGeometry(d, d);
    const quadMat = new THREE.MeshBasicMaterial({
      color: 0x2e6fd0,
      transparent: true,
      opacity: 0.08,
      side: THREE.DoubleSide,
      depthWrite: false,
    });
    const edgeGeo = new THREE.EdgesGeometry(quadGeo);
    const edgeMat = new THREE.LineBasicMaterial({ color: 0x2e6fd0, transparent: true, opacity: 0.7 });
    this.sectionQuadDisposables.push(quadGeo, quadMat, edgeGeo, edgeMat);
    group.add(new THREE.Mesh(quadGeo, quadMat));
    group.add(new THREE.LineSegments(edgeGeo, edgeMat));
    this.sectionQuad = group;
    this.sectionProxy.add(group);
  }

  private syncSectionFromProxy() {
    const n = new THREE.Vector3(0, 0, 1).applyQuaternion(this.sectionProxy.quaternion);
    this.sectionPlane.setFromNormalAndCoplanarPoint(n, this.sectionProxy.position);
    // Caps lie exactly on the plane.
    for (const group of [this.capPart, this.capVoxel]) {
      const cap = group[2] as THREE.Mesh | undefined;
      if (cap) {
        cap.position.copy(this.sectionProxy.position);
        cap.quaternion.copy(this.sectionProxy.quaternion);
      }
    }
    this.emitSectionMoved();
  }

  private emitSectionMoved() {
    const p = this.sectionPlane;
    this.callbacks.onSectionMoved?.([p.normal.x, p.normal.y, p.normal.z], p.constant);
  }

  /** Stencil-buffer cap (three.js clipping_stencil technique): back faces of
   *  the clipped solid increment, front faces decrement; a plane quad drawn
   *  where stencil != 0 fills the cut so the part reads as solid. */
  private makeCapGroup(
    geometry: THREE.BufferGeometry,
    color: number,
    order: number
  ): THREE.Object3D[] {
    const stencilBase = () => {
      const m = new THREE.MeshBasicMaterial();
      m.depthWrite = false;
      m.depthTest = false;
      m.colorWrite = false;
      m.stencilWrite = true;
      m.stencilFunc = THREE.AlwaysStencilFunc;
      m.clippingPlanes = [this.sectionPlane];
      this.capDisposables.push(m);
      return m;
    };
    const backMat = stencilBase();
    backMat.side = THREE.BackSide;
    backMat.stencilFail = THREE.IncrementWrapStencilOp;
    backMat.stencilZFail = THREE.IncrementWrapStencilOp;
    backMat.stencilZPass = THREE.IncrementWrapStencilOp;
    const frontMat = stencilBase();
    frontMat.side = THREE.FrontSide;
    frontMat.stencilFail = THREE.DecrementWrapStencilOp;
    frontMat.stencilZFail = THREE.DecrementWrapStencilOp;
    frontMat.stencilZPass = THREE.DecrementWrapStencilOp;
    const back = new THREE.Mesh(geometry, backMat);
    const front = new THREE.Mesh(geometry, frontMat);
    back.renderOrder = order;
    front.renderOrder = order;

    const capGeo = new THREE.PlaneGeometry(this.bboxDiag * 4, this.bboxDiag * 4);
    const capMat = new THREE.MeshStandardMaterial({
      color,
      metalness: 0.05,
      roughness: 0.8,
      stencilWrite: true,
      stencilRef: 0,
      stencilFunc: THREE.NotEqualStencilFunc,
      stencilFail: THREE.ReplaceStencilOp,
      stencilZFail: THREE.ReplaceStencilOp,
      stencilZPass: THREE.ReplaceStencilOp,
    });
    this.capDisposables.push(capGeo, capMat);
    const cap = new THREE.Mesh(capGeo, capMat);
    cap.renderOrder = order + 0.1;
    cap.onAfterRender = (renderer) => renderer.clearStencil();
    cap.position.copy(this.sectionProxy.position);
    cap.quaternion.copy(this.sectionProxy.quaternion);
    const group = [back, front, cap];
    for (const o of group) this.scene.add(o);
    return group;
  }

  /** (Re)create cap groups for the part mesh and the voxel hull. */
  private rebuildCapGroups() {
    if (!this.sectionTranslate) return; // section never enabled yet
    for (const o of [...this.capPart, ...this.capVoxel]) this.scene.remove(o);
    for (const d of this.capDisposables) d.dispose();
    this.capPart = [];
    this.capVoxel = [];
    this.capDisposables = [];
    if (this.geometry) {
      this.capPart = this.makeCapGroup(this.geometry, 0x76808c, 1);
    }
    const hull = this.voxelGroup.children.find((c): c is THREE.Mesh => c instanceof THREE.Mesh);
    if (hull) {
      this.capVoxel = this.makeCapGroup(hull.geometry as THREE.BufferGeometry, 0x5f6c7b, 3);
    }
    this.updateSectionVisibility();
  }

  /** Push/remove the clipping plane on every content material. */
  private refreshClipping() {
    const planes = this.sectionOn ? [this.sectionPlane] : null;
    // Mesh view: the ghosted STL overlay stays WHOLE while the voxel hull is
    // cut — the full part silhouette is the reference the cut is judged
    // against.
    const partPlanes = this.viewMode === "mesh" ? null : planes;
    // The voxel hull never plane-clips while it carries a voxel-true cut.
    const voxelPlanes = this.sectionOn && !this.voxelCutActive ? [this.sectionPlane] : null;
    const apply = (
      mat: THREE.Material | THREE.Material[] | undefined,
      p: THREE.Plane[] | null
    ) => {
      if (!mat) return;
      for (const m of Array.isArray(mat) ? mat : [mat]) {
        const had = (m.clippingPlanes?.length ?? 0) > 0;
        const want = !!p;
        if (had !== want) {
          m.clippingPlanes = p;
          m.needsUpdate = true;
        }
      }
    };
    apply(this.mesh?.material, partPlanes);
    apply(this.wireframeLines?.material, partPlanes);
    for (const c of this.voxelGroup.children) apply((c as THREE.Mesh).material, voxelPlanes);
    for (const c of this.voxRes?.group.children ?? []) {
      apply((c as THREE.Mesh).material, planes);
    }
    for (const m of this.regionMeshes) apply(m.material, planes);
    apply(this.optShapeMesh?.material ?? undefined, planes);
  }

  private updateSectionVisibility() {
    const gizmoVisible = this.sectionOn;
    for (const tc of [this.sectionTranslate, this.sectionRotate]) {
      if (tc) {
        tc.getHelper().visible = gizmoVisible;
        tc.enabled = gizmoVisible;
      }
    }
    this.sectionProxy.visible = gizmoVisible; // carries the plane quad
    // Caps only where an OPAQUE solid is being cut (ghosted part: see inside).
    const mat = this.mesh?.material as THREE.MeshStandardMaterial | undefined;
    const partCap = this.sectionOn && !!this.mesh?.visible && !!mat && !mat.transparent;
    for (const o of this.capPart) o.visible = partCap;
    const voxCap = this.sectionOn && this.voxelGroup.visible && !this.voxelCutActive;
    for (const o of this.capVoxel) o.visible = voxCap;
  }

  /** Re-derive positions, colors, part opacity, and overlay visibility. */
  private refreshView() {
    if (!this.mesh) return;
    const mat = this.mesh.material as THREE.MeshStandardMaterial;
    const infill = this.viewMode === "infill";
    // Density view with an opt shape (live skeleton / cutaway): ghost the
    // part so the interior structure is what you actually see.
    const showShape = this.viewMode === "density" && !!this.optShapeMesh;
    if (this.optShapeMesh) this.optShapeMesh.visible = showShape;
    // Mesh view: the STL stays as a transparent overlay on the voxel hull,
    // so the approximation quality is visible at a glance.
    const meshView = this.viewMode === "mesh";
    const ghost = infill || showShape || meshView;
    mat.transparent = ghost;
    mat.opacity = ghost ? 0.15 : 1.0;
    mat.depthWrite = !ghost;
    mat.needsUpdate = true;
    const voxResult = this.voxResultActive();
    // Part Topo: the optimized body IS the result — drop the original envelope
    // hull in the density/regions views so it doesn't moiré against the
    // coincident body surface (the carved regions sit inside it; the retained
    // faces sit exactly on it).
    const hideHull = this.resultSolid && (this.viewMode === "density" || infill);
    this.mesh.visible = !voxResult && !hideHull;
    this.voxelGroup.visible = this.viewMode === "mesh";
    if (this.voxRes) this.voxRes.group.visible = voxResult;
    // Wireframe overlay: undeformed model views only (its lines are built from
    // the rest shape, so it would not track a deformed result).
    if (this.wireframeLines) {
      this.wireframeLines.visible =
        this.wireframeOn &&
        this.mesh.visible &&
        (this.viewMode === "setup" || this.viewMode === "mesh");
    }
    this.regionMeshes.forEach((m, i) => {
      m.visible = infill && this.regionVisible[i] !== false;
    });
    this.updateMarkerVisibility();
    this.updateSectionVisibility();
    this.updateSymVisibility();
    this.refreshClipping(); // mesh view exempts the ghost STL from the cut
    this.applyPositions();
    this.applyColors();
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
    const n = data.length / 4;
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
    if (!this.mesh || mode === this.scalarMode) return;
    this.scalarMode = mode;
    const mat = this.mesh.material as THREE.MeshStandardMaterial;
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
    const vr = this.voxRes!;
    const uvAttr = vr.geo.getAttribute("uv") as THREE.BufferAttribute;
    const sf = this.scalarField;
    if (sf && sf.values.length * 2 === vr.uvs.length) {
      const lo = this.legendRange.min ?? sf.min;
      const hi = this.legendRange.max ?? sf.max;
      const inv = hi - lo > 1e-30 ? 1 / (hi - lo) : 0;
      for (let i = 0; i < sf.values.length; i++) {
        const t = Math.min(1, Math.max(0, (sf.values[i] - lo) * inv));
        vr.uvs[2 * i] = sf.flip ? 1 - t : t;
        vr.uvs[2 * i + 1] = 0.5;
      }
      uvAttr.array.set(vr.uvs);
      uvAttr.needsUpdate = true;
      this.trackExtremes(sf.values, 1);
      return;
    }
    const { values, lo, hi } = this.dispFieldValues(vr.disp);
    const inv = hi - lo > 1e-30 ? 1 / (hi - lo) : 0;
    for (let i = 0; i < values.length; i++) {
      vr.uvs[2 * i] = Math.min(1, Math.max(0, (values[i] - lo) * inv));
      vr.uvs[2 * i + 1] = 0.5;
    }
    uvAttr.array.set(vr.uvs);
    uvAttr.needsUpdate = true;
    this.trackExtremes(values, 1);
  }

  private applyColors() {
    if (!this.geometry || !this.colors || !this.uvs) return;
    const uvAttr = this.geometry.getAttribute("uv") as THREE.BufferAttribute;
    if (this.voxResultActive()) {
      this.colorVoxelResult();
      this.repaint();
      return;
    }
    if (this.viewMode === "deformed" && this.displacements) {
      const sf = this.scalarField;
      if (sf && sf.values.length * 2 === this.uvs.length) {
        // Stress/strain field coloring (user range override clamps).
        const lo = this.legendRange.min ?? sf.min;
        const hi = this.legendRange.max ?? sf.max;
        const inv = hi - lo > 1e-30 ? 1 / (hi - lo) : 0;
        for (let i = 0; i < sf.values.length; i++) {
          const t = Math.min(1, Math.max(0, (sf.values[i] - lo) * inv));
          this.uvs[2 * i] = sf.flip ? 1 - t : t;
          this.uvs[2 * i + 1] = 0.5;
        }
        uvAttr.needsUpdate = true;
        this.setSurfaceMaterialMode("jet");
        this.trackExtremes(sf.values, 1);
        return;
      }
      const { values, lo, hi } = this.dispFieldValues(this.displacements);
      const inv = hi - lo > 1e-30 ? 1 / (hi - lo) : 0;
      for (let i = 0; i < values.length; i++) {
        this.uvs[2 * i] = Math.min(1, Math.max(0, (values[i] - lo) * inv));
        this.uvs[2 * i + 1] = 0.5;
      }
      uvAttr.needsUpdate = true;
      this.setSurfaceMaterialMode("jet");
      this.trackExtremes(values, 1);
      return;
    }
    if (this.viewMode === "density" && this.vertexDensity) {
      // With a cutaway/skeleton present, the dense interior is shown there
      // (color-coded); the part is just a flat translucent envelope so the
      // density isn't also smeared onto its mostly-skin outer surface.
      if (this.optShapeMesh) {
        this.setSurfaceMaterialMode("flat");
        this.extremeData = null;
        this.updateExtremeMarkers();
        this.repaint();
        return;
      }
      // No cutaway: paint the density straight onto the surface.
      for (let i = 0; i < this.vertexDensity.length; i++) {
        this.uvs[2 * i] = Math.min(1, this.vertexDensity[i] / 0.8);
        this.uvs[2 * i + 1] = 0.5;
      }
      uvAttr.needsUpdate = true;
      this.setSurfaceMaterialMode("ramp");
      return;
    }
    this.setSurfaceMaterialMode("none");
    this.extremeData = null;
    this.updateExtremeMarkers();
    this.repaint();
  }

  // ---------- min/max markers ----------

  private trackExtremes(values: Float32Array | ArrayLike<number>, _stride: number) {
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

  private fmtExtreme(v: number): string {
    if (this.extremesUnit === "mm") {
      const a = Math.abs(v);
      return a >= 0.01 || a === 0 ? `${v.toFixed(3)} mm` : `${(v * 1000).toFixed(1)} µm`;
    }
    if (this.extremesUnit === "MPa") {
      return `${Math.abs(v) >= 0.01 || v === 0 ? v.toPrecision(3) : v.toExponential(1)} MPa`;
    }
    if (this.extremesUnit === "×") {
      return `${v.toFixed(2)}×`; // safety factor
    }
    return v === 0 ? "0" : v.toExponential(2);
  }

  /** Small screen-aligned value chip (canvas-rendered text on a light pill),
   *  world-scaled to the part. Disposal goes through markerDisposables. */
  private makeLabelSprite(text: string, color: number): THREE.Sprite {
    const canvas = document.createElement("canvas");
    const ctx = canvas.getContext("2d")!;
    const font = "bold 28px 'B612 Mono', 'Barlow', system-ui, sans-serif";
    ctx.font = font;
    const w = Math.ceil(ctx.measureText(text).width) + 18;
    canvas.width = w;
    canvas.height = 40;
    ctx.font = font;
    ctx.fillStyle = "#fcfcfae8";
    ctx.fillRect(0, 0, w, 40);
    ctx.fillStyle = `#${color.toString(16).padStart(6, "0")}`;
    ctx.textBaseline = "middle";
    ctx.fillText(text, 9, 21);
    const tex = new THREE.CanvasTexture(canvas);
    const mat = new THREE.SpriteMaterial({ map: tex, depthTest: false, transparent: true });
    this.markerDisposables.push(tex, mat);
    const sprite = new THREE.Sprite(mat);
    const hWorld = 0.035 * this.bboxDiag;
    sprite.scale.set((hWorld * w) / 40, hWorld, 1);
    sprite.renderOrder = 9;
    return sprite;
  }

  /** Refresh the min/max marks: store the DISPLAYED extreme world positions
   *  (projected to the screen each frame in `tick`) and update their value
   *  chips. Visibility/placement of the DOM overlays happens in
   *  `projectExtremes`. */
  private updateExtremeMarkers(positionsOnly = false) {
    const vox = this.voxResultActive();
    const geom = vox ? this.voxRes!.geo : this.geometry;
    const disp = vox ? this.voxRes!.disp : this.displacements;
    this.extremeVisible =
      this.extremesOn &&
      this.viewMode === "deformed" &&
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
    if (!positionsOnly) {
      els.minChip.textContent = `min ${this.fmtExtreme(d.minVal)}`;
      els.maxChip.textContent = `max ${this.fmtExtreme(d.maxVal)}`;
    }
  }

  /** Project the stored extreme positions to screen pixels and place the DOM
   *  marks. Called every frame from `tick` (the camera may have moved). */
  private projectExtremes() {
    const els = this.extremeEls;
    if (!els) return;
    if (!this.extremeVisible) {
      for (const el of [els.minDot, els.minChip, els.maxDot, els.maxChip]) {
        el.style.display = "none";
      }
      return;
    }
    this.placeExtreme(els.minDot, els.minChip, this.extremeWorld.min);
    this.placeExtreme(els.maxDot, els.maxChip, this.extremeWorld.max);
  }

  private placeExtreme(dot: HTMLDivElement, chip: HTMLDivElement, world: THREE.Vector3) {
    const v = this.extremeScratch.copy(world).project(this.camera);
    if (v.z < -1 || v.z > 1) {
      dot.style.display = chip.style.display = "none"; // behind the camera
      return;
    }
    const x = (v.x * 0.5 + 0.5) * this.viewW;
    const y = (-v.y * 0.5 + 0.5) * this.viewH;
    dot.style.display = chip.style.display = "block";
    dot.style.left = `${x}px`;
    dot.style.top = `${y}px`;
    chip.style.left = `${x + 9}px`;
    chip.style.top = `${y + 9}px`;
  }

  private applyPositions(rbmOffset?: number, deformFactor = 1) {
    if (!this.geometry || !this.basePositions) return;
    const attr = this.geometry.getAttribute("position") as THREE.BufferAttribute;
    const out = attr.array as Float32Array;
    const base = this.basePositions;
    if (out.length !== base.length) return; // mid-model-swap: sizes disagree
    if (this.rbmMode && rbmOffset !== undefined) {
      const m = this.rbmMode;
      const s = rbmOffset * this.rbmAmp;
      for (let i = 0; i < base.length; i += 3) {
        const u = this.modeDisplacement(m, base[i], base[i + 1], base[i + 2]);
        out[i] = base[i] + s * u[0];
        out[i + 1] = base[i + 1] + s * u[1];
        out[i + 2] = base[i + 2] + s * u[2];
      }
    } else if (this.displacements && this.viewMode === "deformed") {
      const d = this.displacements;
      const s = this.autoScale * this.deformScale * deformFactor;
      for (let i = 0; i < base.length; i++) out[i] = base[i] + s * d[i];
    } else {
      out.set(base);
    }
    attr.needsUpdate = true;
    this.geometry.computeVertexNormals();
    this.morphVoxelResult(deformFactor);
    // Markers ride the displayed (deformed/animated) vertices.
    this.updateExtremeMarkers(true);
  }

  /** Deform the voxel-result hull (and its cell edges) like the part. */
  private morphVoxelResult(deformFactor: number) {
    const vr = this.voxRes;
    if (!vr || !vr.group.visible) return;
    const s = this.autoScale * this.deformScale * deformFactor;
    const attr = vr.geo.getAttribute("position") as THREE.BufferAttribute;
    const out = attr.array as Float32Array;
    for (let i = 0; i < vr.base.length; i++) out[i] = vr.base[i] + s * vr.disp[i];
    attr.needsUpdate = true;
    vr.geo.computeVertexNormals();
    if (vr.lineGeo && vr.lineBase && vr.lineDisp) {
      const la = vr.lineGeo.getAttribute("position") as THREE.BufferAttribute;
      const lo = la.array as Float32Array;
      for (let i = 0; i < vr.lineBase.length; i++) {
        lo[i] = vr.lineBase[i] + s * vr.lineDisp[i];
      }
      la.needsUpdate = true;
    }
  }

  private tick() {
    if (this.contextLost) return; // GPU is mid-reset — don't touch the dead context
    if (this.rbmMode) {
      const t = this.clock.getElapsedTime();
      this.applyPositions(Math.sin(t * 2.0 * Math.PI * 0.66));
    } else if (this.deformAnimate && this.viewMode === "deformed" && this.displacements) {
      const t = this.clock.getElapsedTime();
      // Smooth 0 → max → 0 loop, 2.4 s period.
      this.applyPositions(undefined, 0.5 - 0.5 * Math.cos((2 * Math.PI * t) / 2.4));
    }
    this.controls.update();
    const r = this.renderer;
    if (this.viewW <= 0 || this.viewH <= 0) return;
    r.setScissorTest(false);
    r.setViewport(0, 0, this.viewW, this.viewH);
    r.clear();
    r.render(this.scene, this.camera);
    this.colorsDirtyFull = false; // the color buffer (if any) was just uploaded
    // Axis gizmo inset, bottom-right.
    const s = 104;
    const m = 10;
    this.gizmoCam.position
      .copy(this.camera.position)
      .sub(this.controls.target)
      .normalize()
      .multiplyScalar(6);
    this.gizmoCam.up.copy(this.camera.up);
    this.gizmoCam.lookAt(0, 0, 0);
    r.clearDepth();
    r.setScissorTest(true);
    r.setScissor(this.viewW - s - m, m, s, s);
    r.setViewport(this.viewW - s - m, m, s, s);
    r.render(this.gizmoScene, this.gizmoCam);
    r.setScissorTest(false);
    r.setViewport(0, 0, this.viewW, this.viewH);
    // Reproject the min/max marks + fixed callouts now the camera is settled.
    this.projectExtremes();
    this.updateCallouts();
  }
}

function makeTextSprite(text: string, color: number): THREE.Sprite {
  const canvas = document.createElement("canvas");
  canvas.width = 64;
  canvas.height = 64;
  const ctx = canvas.getContext("2d")!;
  ctx.font = "bold 44px 'Barlow', 'Segoe UI', system-ui, sans-serif";
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  ctx.fillStyle = `#${color.toString(16).padStart(6, "0")}`;
  ctx.fillText(text, 32, 34);
  const tex = new THREE.CanvasTexture(canvas);
  const mat = new THREE.SpriteMaterial({ map: tex, depthTest: false, transparent: true });
  const sprite = new THREE.Sprite(mat);
  sprite.scale.setScalar(0.62);
  return sprite;
}

/** Bake a colormap (see ./colormaps) into a 1D texture, sampled per-fragment
 * via uv.x. The same `jet`/`ramp` feed the legend bars, so they stay in sync. */
function makeLut(fn: (t: number) => RGB): THREE.DataTexture {
  const n = 256;
  const data = new Uint8Array(n * 4);
  for (let i = 0; i < n; i++) {
    const [r, g, b] = fn(i / (n - 1));
    data[4 * i] = Math.round(255 * r);
    data[4 * i + 1] = Math.round(255 * g);
    data[4 * i + 2] = Math.round(255 * b);
    data[4 * i + 3] = 255;
  }
  const tex = new THREE.DataTexture(data, n, 1, THREE.RGBAFormat);
  tex.colorSpace = THREE.SRGBColorSpace;
  tex.minFilter = THREE.LinearFilter;
  tex.magFilter = THREE.LinearFilter;
  tex.needsUpdate = true;
  return tex;
}
