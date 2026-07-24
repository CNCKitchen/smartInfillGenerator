// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Imperative three.js layer: mesh display, patch hover/select, brush,
// BC coloring + support glyphs, axis gizmo, rigid-body-mode animation,
// deformed-shape overlay (with looping animation), density/region/voxel views.
// Orchestrates the extracted managers: ColorManager (surface coloring/LUTs),
// GizmoController (section + symmetry plane rigs), ResultSurfaceManager
// (result meshes + displacement application), CalloutManager (value callouts
// + min/max markers).

import * as THREE from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";
import type { Bc, LoadedModel } from "../types";
import type { Tool, ViewMode } from "../store";
import { ramp } from "./colormaps";
import type { OptRegion } from "../engine/EngineClient";
import { BC_COLORS, ColorManager } from "./ColorManager";
import { GizmoController } from "./GizmoController";
import { CalloutManager, type BcCalloutItem } from "./CalloutManager";
import { ResultSurfaceManager } from "./ResultSurfaceManager";
import { SectionFieldCap } from "./SectionFieldCap";
import type { SectionVolume } from "../engine/EngineProtocol";

/** Cut-face color of the capped section view — a matte clay clearly distinct
 *  from the part gray (CAD convention: the section face reads as "cut
 *  material", not as more surface). Result views replace it with the
 *  field-mapped cap (SectionFieldCap). */
const CUT_FACE_COLOR = 0xbe7b4d;

/** Named orthographic camera presets (keyboard 1–6 / Ctrl + 0–6). Axes follow
 *  the Z-up / Blender convention: "front" is the −Y face, matching the default
 *  isometric corner the part is framed from on load. */
export type CameraView = "default" | "top" | "bottom" | "front" | "behind" | "left" | "right";

/** Digit (KeyboardEvent.key) → camera preset, slicer (Bambu/Orca) layout.
 *  Plain 1–6 snap the views; Ctrl/⌘ + 0–6 also work (0 = default ISO). */
const VIEW_KEYS: Record<string, CameraView> = {
  "0": "default",
  "1": "top",
  "2": "bottom",
  "3": "front",
  "4": "behind",
  "5": "left",
  "6": "right",
};

/** PNG bytes of a 2D canvas (decode the data URL to a Uint8Array). */
function pngBytesFromCanvas(c: HTMLCanvasElement): Uint8Array | null {
  const url = c.toDataURL("image/png");
  const b64 = url.slice(url.indexOf(",") + 1);
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export interface SceneCallbacks {
  /** Patch clicked in select mode: toggle its triangles in the active BC. */
  onPickPatch?: (tris: Uint32Array, additive: boolean) => void;
  /** Brush stroke: triangles under the brush. */
  onBrush?: (tris: Uint32Array, erase: boolean) => void;
  /** Wheel over the part in "brush" resized the brush — sync the panel slider. */
  onBrushRadius?: (radius: number) => void;
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
  // ---- meshstep-viewer parity (DESIGN §18 M4 / viewport) ----
  /** Camera-following key light — orbiting never leaves the part unlit. */
  private headlight: THREE.DirectionalLight | null = null;
  private readonly _hlRight = new THREE.Vector3();
  private readonly _hlUp = new THREE.Vector3();
  /** Corner → welded-vertex id (positions quantized to 1e-4 mm). Built per
   *  model; RIGID pose changes preserve coincidence, so it survives
   *  orientation edits and only rebuilds on a new model. */
  private weldIds: Uint32Array | null = null;
  /** CSR triangle list per welded vertex (smooth-shading adjacency). */
  private vertTriOffsets: Uint32Array | null = null;
  private vertTriList: Uint32Array | null = null;
  /** Optional smooth shading: within CAD faces (STEP) or crease-aware by the
   *  edge angle (STL/3MF) — hard edges match the feature-edge overlay. */
  private smoothShadingOn = false;
  /** Feature-edge overlay. STEP models push EXACT CAD border segments (from
   *  meshStep's conforming mesh — the display refinement is non-conforming,
   *  so deriving edges here would hallucinate T-junction edges); STL/3MF
   *  derive dihedral edges > `edgeAngleDeg` across properly-shared edges. */
  private featureEdgesOn = true;
  private edgeAngleDeg = 30;
  /** STEP only: world-space CAD border segments + per-triangle CAD face ids
   *  (shading groups). Null ⇒ STL path (dihedral derivation). */
  private explicitEdgeSegments: Float32Array | null = null;
  private cadFaceIds: Uint32Array | null = null;
  /** STL/3MF edge source: the ORIGINAL (pre-refinement, CONFORMING) soup,
   *  fetched from the engine per pose by the store. The working mesh must
   *  never be used here — its T-junctions have no exact edge partner, which
   *  both paints phantom "open" edges and hides real creases. `stlEdgePairs`
   *  caches the corner pairs (pose-invariant); positions refresh per pose. */
  private stlOrigPositions: Float32Array | null = null;
  private stlEdgePairs: Uint32Array | null = null;
  private featureEdgeLines: THREE.LineSegments | null = null;

  private bcs: Bc[] = [];
  private activeBcId: string | null = null;
  /** BCs deactivated in the active load step — drawn translucent. */
  private inactiveBcs: Set<string> = new Set();

  private tool: Tool = "orbit";
  private brushRadius = 3;
  private brushing = false;
  /** Whether the current brush stroke erases (RMB) or adds (LMB). */
  private strokeErase = false;
  /** RMB press position in "select" — a sub-threshold click on release removes
   *  the patch under the cursor (an RMB drag is still the OrbitControls pan). */
  private rmbDown: { x: number; y: number } | null = null;
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
  // Load VALUE labels are drawn as result-style callouts (dot + offset chip +
  // leader) via CalloutManager, not opaque 3D sprites — collected here during
  // rebuildBcMarkers, handed off after. See pushBcCallout.
  private pendingBcCallouts: BcCalloutItem[] = [];
  /** Loads/fixtures belong to the structural workspace — the Build Sim
   *  workspace disables them wholesale (its physics has no applied loads). */
  private bcMarkersEnabled = true;

  // Axis gizmo (inset, bottom-right)
  private gizmoScene = new THREE.Scene();
  private gizmoCam = new THREE.OrthographicCamera(-1.9, 1.9, 1.9, -1.9, 0.1, 20);
  private viewW = 0;
  private viewH = 0;

  private regionMeshes: THREE.Mesh[] = [];
  private regionVisible: boolean[] = [];
  private viewMode: ViewMode = "setup";

  /** FPS readout (sampled ~2×/s) — set by the Viewer to display a counter. */
  onFps?: (fps: number) => void;
  private fpsFrames = 0;
  private fpsLast = 0;

  // Live optimization skeleton / density-threshold cutaway.
  private optShapeMesh: THREE.Mesh | null = null;
  // Result is a Part Topo body: hide the original envelope hull in result views
  // and render the body opaque (no moiré against the coincident envelope).
  private resultSolid = false;

  // Section plane: clipping + stencil caps + combined transform gizmo
  // (translate along the normal only + two rotation rings).
  private sectionOn = false;
  private sectionPlane = new THREE.Plane(new THREE.Vector3(-1, 0, 0), 0);
  private sectionGizmo = new GizmoController(
    "section",
    {
      scene: this.scene,
      camera: () => this.camera,
      domElement: () => this.renderer.domElement,
      onDraggingChanged: (dragging) => {
        this.controls.enabled = !dragging;
      },
      onChanged: () => this.onSectionChanged(),
      bboxDiag: () => this.bboxDiag,
      partBbox: () => this.partBbox,
    },
    this.sectionPlane
  );
  private capPart: THREE.Object3D[] = [];
  private capVoxel: THREE.Object3D[] = [];
  /** Cap group for the voxel-RESULT surface (deformed results on the hull). */
  private capVoxRes: THREE.Object3D[] = [];
  /** The voxel hull already carries the section cut in its geometry. */
  private voxelCutActive = false;
  private capDisposables: { dispose(): void }[] = [];

  // Symmetry plane (optimizer constraint): section-style combined gizmo
  // (translate along the normal + two rotation rings). Visible only while
  // it's being edited — the store gates on step/busy, the scene additionally
  // hides it in result views.
  private symEnabled = false;
  private symGizmo = new GizmoController("symmetry", {
    scene: this.scene,
    camera: () => this.camera,
    domElement: () => this.renderer.domElement,
    onDraggingChanged: (dragging) => {
      this.controls.enabled = !dragging;
    },
    onChanged: () => this.onSymmetryChanged(),
    bboxDiag: () => this.bboxDiag,
    partBbox: () => this.partBbox,
  });

  // Hover value probe: contour value next to the cursor on result/density
  // surfaces. The formatter doubles as the on/off switch (null = off).
  private probeEl: HTMLDivElement | null = null;
  private probeFormat: ((v: number) => string) | null = null;

  private uvs: Float32Array | null = null;

  // Result-surface data + meshes (voxel hull, voxel result, build preview,
  // displacement application). Shared state flows through accessors.
  private results: ResultSurfaceManager = new ResultSurfaceManager({
    scene: this.scene,
    geometry: () => this.geometry,
    basePositions: () => this.basePositions,
    viewMode: () => this.viewMode,
    bboxDiag: () => this.bboxDiag,
    lutJet: () => this.colorMgr.lutJet,
    peelClippingPlanes: () => (this.sectionOn ? [this.sectionPlane] : null),
    onPositionsApplied: () => this.callouts.updateExtremeMarkers(true),
    onDeformScale: (s) => this.sectionField.setDefScale(s),
  });

  // Surface coloring: BC repaint/hover tint, scalar-field LUT paths, banding.
  private colorMgr: ColorManager = new ColorManager({
    mesh: () => this.mesh,
    geometry: () => this.geometry,
    colors: () => this.colors,
    uvs: () => this.uvs,
    triCount: () => this.triCount,
    // The Build Sim workspace hides loads/fixtures entirely (arrows AND face
    // tint) — an empty list here erases the tint on the next repaint.
    bcs: () => (this.bcMarkersEnabled ? this.bcs : []),
    activeBcId: () => this.activeBcId,
    patchToTris: () => this.patchToTris,
    viewMode: () => this.viewMode,
    displacements: () => this.results.displacements,
    vertexDensity: () => this.results.vertexDensity,
    hasOptShape: () => !!this.optShapeMesh,
    voxResultActive: () => this.results.voxResultActive(),
    voxRes: () => this.results.voxRes,
    trackExtremes: (values) => this.callouts.trackExtremes(values),
    clearExtremes: () => this.callouts.clearExtremes(),
    onResultRange: (min, max) => this.callbacks.onResultRange?.(min, max),
  });

  // Field-mapped section cap: colors the cut face by the volumetric result
  // field (shares the ColorManager's jet LUT, so banding follows along).
  private sectionField = new SectionFieldCap(this.colorMgr.lutJet);

  // Fixed value callouts + min/max extreme markers (DOM overlays).
  private callouts: CalloutManager = new CalloutManager({
    camera: () => this.camera,
    canvasRect: () => this.renderer.domElement.getBoundingClientRect(),
    viewSize: () => ({ w: this.viewW, h: this.viewH }),
    viewMode: () => this.viewMode,
    hasMesh: () => !!this.mesh,
    probeFormat: () => this.probeFormat,
    probeSource: () => this.probeSource(),
    resultGeometry: () => {
      const vox = this.results.voxResultActive();
      return {
        geom: vox ? this.results.voxRes!.geo : this.geometry,
        disp: vox ? this.results.voxRes!.disp : this.results.displacements,
      };
    },
    interiorDisplayedPos: (rest, out) => this.sectionField.displacedPoint(rest, out),
    onLog: (msg) => this.callbacks.onLog?.(msg),
  });

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
    // Key light FOLLOWS the camera (position updated per frame in tick(), a
    // little up-right of the view axis so faces keep gradient) — orbiting can
    // never turn the part's far side pitch-black. A weak static cool fill
    // keeps a fixed world anchor so the shading still shifts as you orbit.
    this.headlight = new THREE.DirectionalLight(0xffffff, 1.6);
    this.scene.add(this.headlight, this.headlight.target);
    const fill = new THREE.DirectionalLight(0xc8d2e0, 0.35);
    fill.position.set(-1.5, 1, -0.5);
    this.scene.add(fill);

    const grid = new THREE.GridHelper(400, 40, 0xafada6, 0xc8c6bf);
    grid.rotation.x = Math.PI / 2; // Z-up
    this.scene.add(grid);

    this.scene.add(this.bcMarkers);
    this.scene.add(this.results.voxelGroup);
    this.scene.add(this.results.buildGroup);
    this.buildGizmo();

    canvas.addEventListener("pointermove", this.onPointerMove);
    canvas.addEventListener("pointerdown", this.onPointerDown);
    canvas.addEventListener("pointerup", this.onPointerUp);
    // RMB is a selection tool (erase) — never the browser context menu.
    // OrbitControls only suppresses it while enabled; brush mode disables them.
    canvas.addEventListener("contextmenu", (e) => e.preventDefault());
    canvas.addEventListener("pointerleave", () => {
      this.colorMgr.setHover(null);
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

      // Min/max marks + fixed value callouts (DOM overlays, leader lines, and
      // the modifier-gated gestures) live in the callout manager.
      this.callouts.attach(canvas.parentElement, this.probeEl);
    }

    const loop = () => {
      if (this.disposed) return;
      requestAnimationFrame(loop);
      this.tick();
      // Sample the frame rate ~twice a second for the on-screen counter.
      this.fpsFrames++;
      const now = performance.now();
      if (this.fpsLast === 0) this.fpsLast = now;
      const dt = now - this.fpsLast;
      if (dt >= 500) {
        this.onFps?.((this.fpsFrames * 1000) / dt);
        this.fpsFrames = 0;
        this.fpsLast = now;
      }
    };
    loop();
  }

  dispose() {
    this.disposed = true;
    document.removeEventListener("pointermove", this.onOrbitMove);
    document.removeEventListener("pointerup", this.onOrbitUp);
    document.removeEventListener("keydown", this.onViewKey);
    this.callouts.dispose();
    this.canvas?.removeEventListener("wheel", this.onWheel);
    this.canvas?.removeEventListener("webglcontextlost", this.onGlLost);
    this.canvas?.removeEventListener("webglcontextrestored", this.onGlRestored);
    this.probeEl?.remove();
    if (this.wireframeLines) {
      this.wireframeLines.geometry.dispose();
      (this.wireframeLines.material as THREE.Material).dispose();
    }
    if (this.featureEdgeLines) {
      this.featureEdgeLines.geometry.dispose();
      (this.featureEdgeLines.material as THREE.Material).dispose();
    }
    for (const d of this.pickArrowDisposables) d.dispose();
    this.sectionField.dispose();
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

  /** Fit the part into the current viewport WITHOUT changing the view
   *  direction (keyboard F): re-center the orbit target on the part and size
   *  the frustum to the bbox's projected extent plus a small margin. */
  fitView() {
    if (!this.camera || !this.controls) return;
    const b = this.partBbox;
    if (!b) return;
    const center = this.partCenter();
    const dir = this.camera.position.clone().sub(this.controls.target);
    if (dir.lengthSq() < 1e-12) dir.set(0.7, -0.8, 0.55);
    dir.normalize();
    this.camera.position.copy(center).addScaledVector(dir, this.bboxDiag * 2.2);
    this.controls.target.copy(center);
    this.camera.near = this.bboxDiag / 100;
    this.camera.far = this.bboxDiag * 50;
    this.camera.lookAt(center); // same axis + up → the view only re-frames
    this.camera.updateMatrixWorld();
    // Projected half-extents of the bbox corners on the camera's screen axes.
    const right = new THREE.Vector3().setFromMatrixColumn(this.camera.matrixWorld, 0);
    const up = new THREE.Vector3().setFromMatrixColumn(this.camera.matrixWorld, 1);
    let ex = 0;
    let ey = 0;
    const p = new THREE.Vector3();
    for (let i = 0; i < 8; i++) {
      p.set(b[i & 1 ? 3 : 0], b[i & 2 ? 4 : 1], b[i & 4 ? 5 : 2]).sub(center);
      ex = Math.max(ex, Math.abs(p.dot(right)));
      ey = Math.max(ey, Math.abs(p.dot(up)));
    }
    const aspect = this.viewH > 0 ? this.viewW / this.viewH : 1;
    this.camera.zoom = 1; // wheel zoom is baked into the fitted frustum
    this.orthoHalf = Math.max(ey, ex / aspect, 1e-6) * 1.08;
    this.updateFrustum();
    this.controls.update();
    // Re-pivot the next orbit drag on the part centre, not a stale surface hit.
    this.lastOrbitPivot = center.clone();
  }

  setTool(tool: Tool, brushRadius: number) {
    this.tool = tool;
    this.brushRadius = brushRadius;
    // Navigation stays live in every tool; in "brush" the hover gate in
    // onPointerMove claims the pointer only while it is over the part.
    this.controls.enabled = true;
    if (this.brushCursor) this.brushCursor.visible = tool === "brush";
    // The pick-direction preview arrow follows the pointer (shown on hover in
    // onPointerMove); just clear it when leaving the tool.
    if (this.pickArrow && tool !== "pickdir") this.pickArrow.visible = false;
    if (tool !== "select" && tool !== "place") this.colorMgr.setHover(null);
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

  /** DESIGN §15 display-only orientation preview: rotate the part (mesh,
   *  wireframe, BC markers, result groups) about its bbox center so the given
   *  layer normal (part frame) points up (+Z). Null restores the true pose.
   *  Purely visual — engine state, results and picking stay untouched. */
  setOrientationPreview(dir: [number, number, number] | null) {
    const objs: THREE.Object3D[] = [this.bcMarkers, this.results.voxelGroup, this.results.buildGroup];
    if (this.mesh) objs.push(this.mesh);
    if (this.wireframeLines) objs.push(this.wireframeLines);
    if (this.featureEdgeLines) objs.push(this.featureEdgeLines);
    if (!dir || !this.partBbox) {
      for (const o of objs) {
        o.quaternion.identity();
        o.position.set(0, 0, 0);
      }
      return;
    }
    const n = new THREE.Vector3(dir[0], dir[1], dir[2]).normalize();
    const q = new THREE.Quaternion().setFromUnitVectors(n, new THREE.Vector3(0, 0, 1));
    const b = this.partBbox;
    const c = new THREE.Vector3((b[0] + b[3]) / 2, (b[1] + b[4]) / 2, (b[2] + b[5]) / 2);
    // Rotate about the part center: p' = q·p + (c − q·c).
    const pos = c.clone().sub(c.clone().applyQuaternion(q));
    for (const o of objs) {
      o.quaternion.copy(q);
      o.position.copy(pos);
    }
  }

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
    this.results.displacements = null;
    this.colorMgr.resetDispComponent();
    this.results.vertexDensity = null;
    this.results.rbmMode = null;
    this.sectionField.setVolume(null); // volumetric field belongs to the old part
    this.viewMode = "setup";
    this.setRegions(null);
    this.setVoxelMesh(null, null);
    this.setBuildGhost(null);
    this.setBuildActive(null);
    this.setPeelMap(null, null, 0);
    this.setOptShape(null, null);

    this.geometry = new THREE.BufferGeometry();
    this.geometry.setAttribute("position", new THREE.BufferAttribute(model.positions, 3));
    this.geometry.setAttribute("color", new THREE.BufferAttribute(this.colors, 3));
    this.uvs = new Float32Array(this.triCount * 3 * 2);
    this.geometry.setAttribute("uv", new THREE.BufferAttribute(this.uvs, 2));
    this.geometry.computeVertexNormals();
    this.colorMgr.resetScalarMode();

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
    this.buildShadingTopology();
    // Edge/shading topology (STEP CAD edges + shading groups + colors, STL
    // original soup) rides separately — the store pushes it after load; a
    // fresh model must not inherit the previous one's.
    this.explicitEdgeSegments = null;
    this.cadFaceIds = null;
    this.stlOrigPositions = null;
    this.stlEdgePairs = null;
    this.applyShading();
    this.colorMgr.setBaseColors(null);

    this.setPatchIds(model.patchIds);
    this.bcs = [];
    this.activeBcId = null;
    this.colorMgr.clearScalarField();
    this.rebuildBcMarkers();
    this.colorMgr.repaint();

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

    // Section plane follows the new part — through its CENTER (the orbit
    // target may have been panned anywhere).
    if (this.sectionGizmo.exists()) {
      this.sectionGizmo.proxy.position.copy(this.partCenter());
      this.sectionGizmo.buildQuad(); // resize to the new part
      this.sectionGizmo.sync();
      this.rebuildCapGroups();
    }
    if (this.symGizmo.exists()) this.symGizmo.buildQuad(); // symmetry plane too
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
    this.applyShading(); // flat OR crease-aware smooth, from the new pose
    this.geometry.computeBoundingBox();
    this.geometry.computeBoundingSphere();
    this.basePositions = new Float32Array(positions);
    this.partBbox = bbox;
    const [lx, ly, lz, hx, hy, hz] = bbox;
    this.bboxDiag = Math.hypot(hx - lx, hy - ly, hz - lz) || this.bboxDiag;
    this.controls.target.set((lx + hx) / 2, (ly + hy) / 2, (lz + hz) / 2);
    this.controls.update();
    // The part moved under the section plane — re-center the plane on the new
    // bbox center so the cut stays through the part.
    if (this.sectionGizmo.exists()) {
      this.sectionGizmo.proxy.position.copy(this.partCenter());
      this.sectionGizmo.buildQuad();
      this.sectionGizmo.sync();
      this.rebuildCapGroups();
    }
    this.buildWireframe(); // re-derive from the moved geometry
    // Feature edges: the store re-pushes STEP segments / the STL orig soup
    // in the new pose right after this call — nothing to do here.
    this.rebuildBcMarkers();
    this.colorMgr.repaint();
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
    this.buildFeatureEdges(); // no-op for STEP (exact segments), dihedral for STL
    this.colorMgr.resetHover();
    this.colorMgr.repaint();
  }

  // ---------- BC display ----------

  setBcs(bcs: Bc[], activeBcId: string | null, inactive?: Set<string>) {
    this.bcs = bcs;
    this.activeBcId = activeBcId;
    this.inactiveBcs = inactive ?? new Set();
    this.colorMgr.repaint();
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

  // ---------- shading & feature edges (meshstep-viewer parity) ----------

  /** Weld the soup corners by quantized position and build the per-vertex
   *  triangle adjacency (CSR). One pass per MODEL — rigid pose changes keep
   *  coincidence, so orientation edits reuse it. */
  private buildShadingTopology() {
    const pos = this.geometry?.getAttribute("position")?.array as Float32Array | undefined;
    if (!pos) {
      this.weldIds = null;
      this.vertTriOffsets = null;
      this.vertTriList = null;
      return;
    }
    const nCorners = this.triCount * 3;
    const weld = new Uint32Array(nCorners);
    const map = new Map<string, number>();
    let nVerts = 0;
    for (let c = 0; c < nCorners; c++) {
      const key = `${Math.round(pos[3 * c] * 1e4)},${Math.round(pos[3 * c + 1] * 1e4)},${Math.round(pos[3 * c + 2] * 1e4)}`;
      let id = map.get(key);
      if (id === undefined) {
        id = nVerts++;
        map.set(key, id);
      }
      weld[c] = id;
    }
    // CSR: triangles per welded vertex (a triangle appears once per distinct
    // corner vertex — degenerate repeats are harmless for averaging).
    const counts = new Uint32Array(nVerts + 1);
    for (let c = 0; c < nCorners; c++) counts[weld[c] + 1]++;
    for (let v = 0; v < nVerts; v++) counts[v + 1] += counts[v];
    const list = new Uint32Array(nCorners);
    const cursor = counts.slice(0, nVerts);
    for (let c = 0; c < nCorners; c++) list[cursor[weld[c]]++] = c / 3;
    this.weldIds = weld;
    this.vertTriOffsets = counts;
    this.vertTriList = list;
  }

  /** Per-triangle scaled face normals (cross product ≈ 2·area·n̂) + unit
   *  copies, from the CURRENT position buffer. */
  private faceNormals(): { scaled: Float32Array; unit: Float32Array } | null {
    const pos = this.geometry?.getAttribute("position")?.array as Float32Array | undefined;
    if (!pos) return null;
    const n = this.triCount;
    const scaled = new Float32Array(n * 3);
    const unit = new Float32Array(n * 3);
    for (let t = 0; t < n; t++) {
      const o = 9 * t;
      const ax = pos[o + 3] - pos[o];
      const ay = pos[o + 4] - pos[o + 1];
      const az = pos[o + 5] - pos[o + 2];
      const bx = pos[o + 6] - pos[o];
      const by = pos[o + 7] - pos[o + 1];
      const bz = pos[o + 8] - pos[o + 2];
      const cx = ay * bz - az * by;
      const cy = az * bx - ax * bz;
      const cz = ax * by - ay * bx;
      scaled[3 * t] = cx;
      scaled[3 * t + 1] = cy;
      scaled[3 * t + 2] = cz;
      const l = Math.hypot(cx, cy, cz) || 1;
      unit[3 * t] = cx / l;
      unit[3 * t + 1] = cy / l;
      unit[3 * t + 2] = cz / l;
    }
    return { scaled, unit };
  }

  /** Write the normal attribute for the active shading mode. Flat = three's
   *  face normals; smooth = averaging within a SHADING GROUP, so hard edges
   *  match the feature-edge overlay exactly: STEP corners blend only across
   *  triangles of the SAME CAD face (tangent neighbors converge to the same
   *  normal at the shared border, so fillets stay seamless while true edges
   *  stay hard), STL corners blend across dihedral angles below the edge
   *  angle. Runs on the CURRENT positions — re-applied after pose changes. */
  private applyShading() {
    if (!this.geometry) return;
    if (!this.smoothShadingOn || !this.weldIds || !this.vertTriOffsets || !this.vertTriList) {
      this.geometry.computeVertexNormals();
      return;
    }
    const fn = this.faceNormals();
    if (!fn) return;
    const cosCrease = Math.cos((this.edgeAngleDeg * Math.PI) / 180);
    this.geometry.computeVertexNormals(); // ensures the attribute exists/sized
    const normals = this.geometry.getAttribute("normal")!.array as Float32Array;
    const { scaled, unit } = fn;
    const weld = this.weldIds;
    const offs = this.vertTriOffsets;
    const list = this.vertTriList;
    const byFace = this.cadFaceIds && this.cadFaceIds.length === this.triCount ? this.cadFaceIds : null;
    const nCorners = this.triCount * 3;
    for (let c = 0; c < nCorners; c++) {
      const t = (c / 3) | 0;
      const nx = unit[3 * t];
      const ny = unit[3 * t + 1];
      const nz = unit[3 * t + 2];
      let sx = 0;
      let sy = 0;
      let sz = 0;
      const v = weld[c];
      for (let i = offs[v]; i < offs[v + 1]; i++) {
        const u = list[i];
        const same = byFace
          ? byFace[u] === byFace[t]
          : nx * unit[3 * u] + ny * unit[3 * u + 1] + nz * unit[3 * u + 2] > cosCrease;
        if (same) {
          sx += scaled[3 * u];
          sy += scaled[3 * u + 1];
          sz += scaled[3 * u + 2];
        }
      }
      const l = Math.hypot(sx, sy, sz) || 1;
      normals[3 * c] = sx / l;
      normals[3 * c + 1] = sy / l;
      normals[3 * c + 2] = sz / l;
    }
    this.geometry.getAttribute("normal")!.needsUpdate = true;
  }

  /** Toggle smooth shading (see applyShading for the group rules). */
  setSmoothShading(on: boolean) {
    if (this.smoothShadingOn === on) return;
    this.smoothShadingOn = on;
    this.applyShading();
  }

  /** Feature-edge angle for STL/3MF models (dihedral threshold, degrees).
   *  Drives BOTH the edge overlay and the smooth-shading creases so they
   *  always agree. No-op for the overlay on STEP (exact CAD edges). */
  setEdgeAngle(deg: number) {
    const d = Math.min(89, Math.max(1, deg));
    if (this.edgeAngleDeg === d) return;
    this.edgeAngleDeg = d;
    this.stlEdgePairs = null; // angle changed → re-derive on the orig soup
    if (!this.explicitEdgeSegments) this.buildFeatureEdges();
    if (this.smoothShadingOn && !this.cadFaceIds) this.applyShading();
  }

  /** STEP models: install the EXACT world-space CAD border segments (from
   *  meshStep's conforming mesh, transformed by the store per pose) + the
   *  per-working-triangle CAD face ids that group the smooth shading. Null
   *  arguments revert to the STL dihedral derivation. */
  setFeatureEdgeSegments(segments: Float32Array | null, faceOfTri: Uint32Array | null) {
    this.explicitEdgeSegments = segments;
    this.cadFaceIds = faceOfTri;
    this.buildFeatureEdges();
    if (this.smoothShadingOn) this.applyShading();
  }

  /** STL/3MF models: install the ORIGINAL (conforming) soup in its current
   *  pose — the store fetches it from the engine on load and after every
   *  transform. The cached corner pairs survive pose changes (rigid), so a
   *  re-push only re-reads coordinates. Null clears (STEP models). */
  setOriginalMesh(positions: Float32Array | null) {
    if (
      !positions ||
      !this.stlOrigPositions ||
      positions.length !== this.stlOrigPositions.length
    ) {
      this.stlEdgePairs = null; // different mesh → topology is stale
    }
    this.stlOrigPositions = positions;
    this.buildFeatureEdges();
  }

  /** Rebuild the feature-edge overlay. STEP: the pushed exact segments.
   *  STL/3MF: derived on the ORIGINAL soup — edges shared by exactly TWO
   *  triangles with a dihedral angle above the threshold. (Never derived on
   *  the working mesh: its T-junction refinement both hides real creases and
   *  invents open edges.) */
  private buildFeatureEdges() {
    let segments = this.explicitEdgeSegments;
    if (!segments && this.stlOrigPositions) {
      if (!this.stlEdgePairs) {
        this.stlEdgePairs = deriveEdgePairs(this.stlOrigPositions, this.edgeAngleDeg);
      }
      const pos = this.stlOrigPositions;
      const pairs = this.stlEdgePairs;
      segments = new Float32Array(pairs.length * 3);
      for (let i = 0; i < pairs.length; i++) {
        const c = pairs[i];
        segments[3 * i] = pos[3 * c];
        segments[3 * i + 1] = pos[3 * c + 1];
        segments[3 * i + 2] = pos[3 * c + 2];
      }
    }
    this.rebuildFeatureEdgeLines(segments);
  }

  /** (Re)create the feature-edge line object from coordinate segments. */
  private rebuildFeatureEdgeLines(segments: Float32Array | null) {
    if (this.featureEdgeLines) {
      this.scene.remove(this.featureEdgeLines);
      this.featureEdgeLines.geometry.dispose();
      (this.featureEdgeLines.material as THREE.Material).dispose();
      this.featureEdgeLines = null;
    }
    if (!segments || segments.length === 0) return;
    const geo = new THREE.BufferGeometry();
    geo.setAttribute("position", new THREE.BufferAttribute(segments, 3));
    geo.computeBoundingSphere();
    const mat = new THREE.LineBasicMaterial({
      color: 0x2b3440, // ink-dark, crisper than the wireframe overlay
      transparent: true,
      opacity: 0.85,
    });
    this.featureEdgeLines = new THREE.LineSegments(geo, mat);
    this.featureEdgeLines.visible =
      this.featureEdgesOn && (this.viewMode === "setup" || this.viewMode === "mesh");
    this.scene.add(this.featureEdgeLines);
    this.refreshClipping();
  }

  /** Toggle the feature-edge overlay. */
  setFeatureEdges(on: boolean) {
    this.featureEdgesOn = on;
    this.refreshView();
  }

  /** Per-triangle CAD base colors (linear RGB, 3 floats/tri) or null. */
  setCadColors(triColors: Float32Array | null) {
    this.colorMgr.setBaseColors(triColors);
    this.colorMgr.repaint();
  }

  /** Force arrows + classic support triangles (4-sided cones read as ▽). */
  private rebuildBcMarkers() {
    for (const d of this.markerDisposables) d.dispose();
    this.markerDisposables = [];
    this.bcMarkers.clear();
    this.pendingBcCallouts = [];
    if (!this.basePositions) {
      this.callouts.setBcCallouts(this.pendingBcCallouts); // clear any stale labels
      return;
    }
    for (const bc of this.bcs) {
      const inactive = this.inactiveBcs.has(bc.id);
      // Acceleration is SELECTION-LESS (DESIGN §16 dec. 12): one labeled arrow
      // at the part's bbox centroid, in the roster colour, shown when active in
      // the displayed step. Handled before the tris guard.
      if (bc.kind === "accel") {
        if (bc.accel) this.buildAccelGlyph(bc, inactive);
        continue;
      }
      if (bc.tris.length === 0) continue;
      if (bc.kind === "mass") {
        this.buildMassGlyph(bc, inactive);
        continue;
      }
      if (bc.kind === "bearing") {
        this.buildBearingGlyphs(bc, inactive);
        continue;
      }
      if (bc.kind === "moment" && bc.moment) {
        this.buildMomentGlyph(bc, inactive);
        continue;
      }
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
        // Value label: a result-style callout pinned to the loaded patch (dot +
        // offset chip + leader) instead of a sprite on the arrow — readable from
        // any angle and never covering the picked spot.
        const mag = f.length();
        this.pushBcCallout(bc, centroid, `${mag >= 9.95 ? mag.toFixed(0) : mag.toFixed(1)} N`, inactive);
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
    this.callouts.setBcCallouts(this.pendingBcCallouts);
    this.updateMarkerVisibility();
  }

  /** Acceleration glyph: a solid-shaft arrow through the part's bbox centre,
   *  along the acceleration vector, labelled with |a| in g. Selection-less
   *  (DESIGN §16 dec. 12); the shaded shaft+cone stays readable end-on. */
  private buildAccelGlyph(bc: Bc, inactive = false) {
    if (!bc.accel || !this.partBbox) return;
    const a = new THREE.Vector3(bc.accel[0], bc.accel[1], bc.accel[2]);
    if (a.lengthSq() === 0) return;
    const dir = a.clone().normalize();
    const b = this.partBbox;
    const center = new THREE.Vector3((b[0] + b[3]) / 2, (b[1] + b[4]) / 2, (b[2] + b[5]) / 2);
    const col = BC_COLORS.accel;
    const len = this.bboxDiag * 0.3; // body-wide — a touch longer than a surface load
    const mat = new THREE.MeshStandardMaterial({
      color: col.clone(),
      roughness: 0.5,
      metalness: 0.05,
      transparent: inactive,
      opacity: inactive ? 0.25 : 1,
    });
    const shaftLen = len * 0.72;
    const shaftGeo = new THREE.CylinderGeometry(len * 0.02, len * 0.02, shaftLen, 10);
    const headGeo = new THREE.ConeGeometry(len * 0.06, len * 0.26, 14);
    this.markerDisposables.push(mat, shaftGeo, headGeo);
    const g = new THREE.Group();
    const shaft = new THREE.Mesh(shaftGeo, mat);
    shaft.position.y = shaftLen / 2;
    const head = new THREE.Mesh(headGeo, mat);
    head.position.y = shaftLen + len * 0.13;
    g.add(shaft, head);
    const gMag = a.length() / 9810; // canonical mm/s² → g (DESIGN §16 convention)
    // Local +Y → acceleration direction; centre the shaft on the part centroid.
    g.quaternion.setFromUnitVectors(new THREE.Vector3(0, 1, 0), dir);
    g.position.copy(center.clone().sub(dir.clone().multiplyScalar(shaftLen / 2)));
    this.bcMarkers.add(g);
    // Value label as a callout at the arrow's head end (out past the part).
    const headWorld = center.clone().add(dir.clone().multiplyScalar(shaftLen / 2));
    this.pushBcCallout(bc, headWorld, `${gMag >= 9.95 ? gMag.toFixed(0) : gMag.toFixed(1)} g`, inactive);
  }

  /** Point-mass glyph: a filled sphere at the CG + spider lines to the mounting
   *  patch + a name/mass label, so the lever arm is always visible (DESIGN §16
   *  dec. 7). */
  private buildMassGlyph(bc: Bc, inactive = false) {
    const p = this.basePositions;
    if (!p) return;
    const point = new THREE.Vector3(...(bc.point ?? [0, 0, 0]));
    const col = BC_COLORS.mass;
    const r = this.bboxDiag * 0.02;
    const mat = new THREE.MeshStandardMaterial({
      color: col.clone(),
      roughness: 0.5,
      metalness: 0.1,
      transparent: inactive,
      opacity: inactive ? 0.3 : 1,
    });
    const sphGeo = new THREE.SphereGeometry(r, 20, 14);
    this.markerDisposables.push(mat, sphGeo);
    const g = new THREE.Group();
    const sphere = new THREE.Mesh(sphGeo, mat);
    sphere.position.copy(point);
    g.add(sphere);
    // Spider lines: CG → patch centroid + a few points spread across the patch.
    const centroid = this.selectionCentroid(bc.tris);
    const targets = [centroid];
    const n = bc.tris.length;
    const k = Math.min(5, n);
    for (let i = 0; i < k; i++) {
      const t = bc.tris[Math.floor(((i + 0.5) * n) / k)];
      const o = 9 * t;
      targets.push(
        new THREE.Vector3(
          (p[o] + p[o + 3] + p[o + 6]) / 3,
          (p[o + 1] + p[o + 4] + p[o + 7]) / 3,
          (p[o + 2] + p[o + 5] + p[o + 8]) / 3
        )
      );
    }
    const linePts: number[] = [];
    for (const t of targets) linePts.push(point.x, point.y, point.z, t.x, t.y, t.z);
    if (linePts.length) {
      const lgeo = new THREE.BufferGeometry();
      lgeo.setAttribute("position", new THREE.Float32BufferAttribute(linePts, 3));
      const lmat = new THREE.LineBasicMaterial({
        color: col.clone(),
        transparent: true,
        opacity: inactive ? 0.2 : 0.6,
      });
      this.markerDisposables.push(lgeo, lmat);
      g.add(new THREE.LineSegments(lgeo, lmat));
    }
    this.bcMarkers.add(g);
    // Value label as a callout anchored at the CG sphere — the chip is offset so
    // it no longer covers the picked patch (the old sprite sat right on it).
    const grams = bc.massGrams ?? 0;
    this.pushBcCallout(
      bc,
      point,
      `${bc.name ?? "Mass"} · ${grams >= 99.5 ? grams.toFixed(0) : grams.toFixed(1)} g`,
      inactive
    );
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
    // Name callout on the support patch — supports carry no magnitude, so the
    // chip just identifies which support it is (in its roster colour).
    this.pushBcCallout(bc, this.selectionCentroid(bc.tris), bc.name ?? "Support", inactive);
  }

  /** Bearing load: a fan of arrows over the loaded half of the fitted cylinder,
   *  each pointing in the push direction with length ∝ cos θ (the projected-area
   *  contact law). Nothing is drawn until the selection fits a cylinder. */
  private buildBearingGlyphs(bc: Bc, inactive: boolean) {
    const cyl = bc.cyl;
    const fa = bc.force ?? [0, 0, 0];
    const f = new THREE.Vector3(fa[0], fa[1], fa[2]);
    if (!cyl || !cyl.ok || f.lengthSq() === 0 || !this.basePositions) return;
    const axis = new THREE.Vector3(cyl.axis[0], cyl.axis[1], cyl.axis[2]).normalize();
    const frad = f.clone().sub(axis.clone().multiplyScalar(f.dot(axis)));
    if (frad.lengthSq() < 1e-12) return;
    const loadDir = frad.clone().normalize();
    const center = new THREE.Vector3(cyl.point[0], cyl.point[1], cyl.point[2]);
    const radius = cyl.radius;
    // Axial extent of the selection (its vertices projected on the axis).
    const p = this.basePositions;
    let amin = Infinity;
    let amax = -Infinity;
    for (const t of bc.tris) {
      for (let v = 0; v < 3; v++) {
        const o = 9 * t + 3 * v;
        const a =
          (p[o] - center.x) * axis.x +
          (p[o + 1] - center.y) * axis.y +
          (p[o + 2] - center.z) * axis.z;
        if (a < amin) amin = a;
        if (a > amax) amax = a;
      }
    }
    if (!isFinite(amin)) {
      amin = 0;
      amax = 0;
    }
    const w = new THREE.Vector3().crossVectors(axis, loadDir).normalize();
    const baseLen = Math.min(radius * 0.9, this.bboxDiag * 0.12);
    const mat = new THREE.MeshStandardMaterial({
      color: 0xb5179e,
      roughness: 0.45,
      metalness: 0.05,
      transparent: inactive,
      opacity: inactive ? 0.25 : 1,
    });
    const shaftGeo = new THREE.CylinderGeometry(baseLen * 0.03, baseLen * 0.03, 1, 8);
    const headGeo = new THREE.ConeGeometry(baseLen * 0.09, baseLen * 0.3, 12);
    this.markerDisposables.push(mat, shaftGeo, headGeo);
    const up = new THREE.Vector3(0, 1, 0);
    const nAx = amax - amin > radius * 0.5 ? 3 : 1;
    const nAng = 9;
    for (let i = 0; i < nAx; i++) {
      const aOff = nAx === 1 ? (amin + amax) / 2 : amin + ((amax - amin) * (i + 0.5)) / nAx;
      for (let j = 0; j < nAng; j++) {
        const phi = (-1 + (2 * j) / (nAng - 1)) * ((80 * Math.PI) / 180); // −80°…80°
        const cosT = Math.cos(phi);
        if (cosT <= 0.02) continue;
        const rhat = loadDir
          .clone()
          .multiplyScalar(Math.cos(phi))
          .add(w.clone().multiplyScalar(Math.sin(phi)));
        const surf = center.clone().addScaledVector(axis, aOff).addScaledVector(rhat, radius);
        const arrowLen = baseLen * cosT;
        const g = new THREE.Group();
        const shaft = new THREE.Mesh(shaftGeo, mat);
        shaft.scale.y = arrowLen * 0.7;
        shaft.position.y = arrowLen * 0.35;
        const head = new THREE.Mesh(headGeo, mat);
        head.position.y = arrowLen * 0.7 + baseLen * 0.15;
        g.add(shaft, head);
        g.quaternion.setFromUnitVectors(up, loadDir);
        // Head tip lands on the surface; shaft trails outward (pin pushing in).
        g.position.copy(surf).addScaledVector(loadDir, -(arrowLen * 0.7 + baseLen * 0.3));
        this.bcMarkers.add(g);
      }
    }
    const mag = frad.length();
    this.pushBcCallout(
      bc,
      this.selectionCentroid(bc.tris),
      `${mag >= 9.95 ? mag.toFixed(0) : mag.toFixed(1)} N`,
      inactive
    );
  }

  /** Moment: a curved arrow encircling the moment axis at the selection
   *  centroid; circulation sense follows the right-hand rule about the axis. */
  private buildMomentGlyph(bc: Bc, inactive: boolean) {
    const ma = bc.moment ?? [0, 0, 0];
    const mvec = new THREE.Vector3(ma[0], ma[1], ma[2]);
    if (mvec.lengthSq() === 0) return;
    const axis = mvec.clone().normalize();
    const center = this.selectionCentroid(bc.tris);
    const R = this.bboxDiag * 0.13;
    const ax = Math.abs(axis.x);
    const ay = Math.abs(axis.y);
    const az = Math.abs(axis.z);
    const seed =
      ax <= ay && ax <= az
        ? new THREE.Vector3(1, 0, 0)
        : ay <= az
          ? new THREE.Vector3(0, 1, 0)
          : new THREE.Vector3(0, 0, 1);
    const u = new THREE.Vector3().crossVectors(axis, seed).normalize();
    const wv = new THREE.Vector3().crossVectors(axis, u); // right-handed about axis
    const mat = new THREE.MeshStandardMaterial({
      color: 0xe8590c,
      roughness: 0.45,
      metalness: 0.05,
      transparent: inactive,
      opacity: inactive ? 0.25 : 1,
    });
    const sweep = Math.PI * 1.6; // ~288° open ring
    const pts: THREE.Vector3[] = [];
    const N = 48;
    for (let i = 0; i <= N; i++) {
      const ang = sweep * (i / N);
      pts.push(
        center
          .clone()
          .addScaledVector(u, R * Math.cos(ang))
          .addScaledVector(wv, R * Math.sin(ang))
      );
    }
    const tubeGeo = new THREE.TubeGeometry(new THREE.CatmullRomCurve3(pts), 64, R * 0.05, 8, false);
    const headGeo = new THREE.ConeGeometry(R * 0.16, R * 0.42, 14);
    this.markerDisposables.push(mat, tubeGeo, headGeo);
    this.bcMarkers.add(new THREE.Mesh(tubeGeo, mat));
    // Arrowhead at the open end, along the tangent (−sin, cos) at `sweep`.
    const tangent = u
      .clone()
      .multiplyScalar(-Math.sin(sweep))
      .add(wv.clone().multiplyScalar(Math.cos(sweep)))
      .normalize();
    const head = new THREE.Mesh(headGeo, mat);
    head.quaternion.setFromUnitVectors(new THREE.Vector3(0, 1, 0), tangent);
    head.position.copy(pts[pts.length - 1]).addScaledVector(tangent, R * 0.18);
    this.bcMarkers.add(head);
    const mag = mvec.length();
    this.pushBcCallout(
      bc,
      this.selectionCentroid(bc.tris),
      `${mag >= 9.95 ? mag.toFixed(0) : mag.toFixed(1)} N·mm`,
      inactive
    );
  }

  /** Workspace gate for the load/fixture display (false in Build Sim): hides
   *  the marker glyphs and repaints the surface without the BC face tint. */
  setBcMarkersEnabled(on: boolean) {
    if (on === this.bcMarkersEnabled) return;
    this.bcMarkersEnabled = on;
    this.colorMgr.repaint();
    this.updateMarkerVisibility();
  }

  private updateMarkerVisibility() {
    this.bcMarkers.visible = this.bcMarkersEnabled && this.viewMode === "setup";
    // Load value labels ride with the glyphs (setup view only).
    this.callouts.setBcCalloutsVisible(this.bcMarkers.visible);
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

  // ---------- picking ----------

  private rayTri(ev: { clientX: number; clientY: number }): THREE.Intersection | null {
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
      this.colorMgr.setHover(patch);
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
      // Hover gate: over the part the pointer belongs to the brush (press
      // paints, wheel sizes); beside it orbit/pan/zoom stay live. Only toggle
      // while no button is held so an in-flight stroke, orbit or pan is never
      // frozen mid-drag.
      if (!this.brushing && ev.buttons === 0) this.controls.enabled = !hit;
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
    if (!this.mesh) return;
    // RMB removes from the active selection: paint-erase in "brush", and in
    // "select" a sub-threshold click removes the patch on release (pointerup),
    // so the OrbitControls right-drag pan keeps working.
    if (ev.button === 2) {
      if (this.tool === "brush") {
        // Erase stroke only when the press lands on the part — beside it the
        // right button stays the OrbitControls pan.
        const hit = this.rayTri(ev);
        if (hit) {
          this.brushing = true;
          this.strokeErase = true;
          this.controls.enabled = false; // freeze a pan OrbitControls may have armed
          this.applyBrush(hit.point);
        }
      } else if (this.tool === "select") {
        this.rmbDown = { x: ev.clientX, y: ev.clientY };
      }
      return;
    }
    if (ev.button !== 0) return;
    // (Modifier-gated callout gestures are claimed earlier, in the capture-phase
    // onAnnoDownCapture, so they never reach OrbitControls or this handler.)
    // Arm a pivot orbit on every left-press in a navigable tool. The camera
    // only moves once the drag passes a threshold, so a plain click still
    // selects/places without disturbing the view. In "brush" a press ON the
    // part starts a paint stroke instead; beside the part it orbits.
    const brushHit = this.tool === "brush" ? this.rayTri(ev) : null;
    if (this.controls.enabled && !brushHit) this.beginOrbit(ev);
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
    } else if (this.tool === "brush" && brushHit) {
      this.brushing = true;
      this.strokeErase = false;
      this.controls.enabled = false;
      this.applyBrush(brushHit.point);
    }
  };

  private onPointerUp = (ev: PointerEvent) => {
    this.brushing = false;
    if (ev.button === 2 && this.rmbDown && this.tool === "select") {
      const moved = Math.hypot(ev.clientX - this.rmbDown.x, ev.clientY - this.rmbDown.y);
      if (moved < 4) {
        const hit = this.rayTri(ev);
        if (hit && hit.faceIndex != null && this.patchIds) {
          const patch = this.patchIds[hit.faceIndex];
          const tris = this.patchToTris.get(patch);
          if (tris) this.callbacks.onPickPatch?.(new Uint32Array(tris), false);
        }
      }
    }
    this.rmbDown = null;
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
    if (hit.length) this.callbacks.onBrush?.(new Uint32Array(hit), this.strokeErase);
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
    if (this.results.voxelGroup.visible) this.results.voxelGroup.children.forEach(add);
    if (this.results.voxRes?.group.visible) this.results.voxRes.group.children.forEach(add);
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
    // In "brush", the wheel over the part sizes the brush sphere (zoom keeps
    // working beside the part). Bounds match the panel slider (Ø 1–50 mm).
    if (this.tool === "brush" && this.rayTri(ev)) {
      ev.preventDefault();
      const factor = ev.deltaY > 0 ? 1 / 1.1 : 1.1;
      this.brushRadius = Math.max(0.5, Math.min(25, this.brushRadius * factor));
      if (this.brushCursor) this.brushCursor.scale.setScalar(this.brushRadius);
      this.callbacks.onBrushRadius?.(this.brushRadius);
      return;
    }
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
    if (this.results.voxResultActive()) {
      const vr = this.results.voxRes!;
      const m = vr.group.children.find((c): c is THREE.Mesh => c instanceof THREE.Mesh);
      if (!m) return null;
      const sf = this.colorMgr.scalarFieldData;
      if (sf && sf.values.length * 2 === vr.uvs.length) {
        return { mesh: m, valueAt: (i) => sf.values[i] };
      }
      const d = vr.disp;
      return { mesh: m, valueAt: (i) => this.colorMgr.dispValueAt(d, i) };
    }
    if (this.viewMode === "deformed" && this.mesh && this.results.displacements) {
      const sf = this.colorMgr.scalarFieldData;
      if (sf && this.uvs && sf.values.length * 2 === this.uvs.length) {
        return { mesh: this.mesh, valueAt: (i) => sf.values[i] };
      }
      const d = this.results.displacements;
      return { mesh: this.mesh, valueAt: (i) => this.colorMgr.dispValueAt(d, i) };
    }
    if (
      (this.viewMode === "density" || this.viewMode === "infill") &&
      this.mesh &&
      this.results.vertexDensity
    ) {
      const v = this.results.vertexDensity;
      return { mesh: this.mesh, valueAt: (i) => v[i] };
    }
    if (this.viewMode === "mesh" && this.results.meshDensity && this.results.voxelDensity) {
      const hull = this.results.voxelGroup.children.find(
        (c): c is THREE.Mesh => c instanceof THREE.Mesh
      );
      if (!hull) return null;
      const v = this.results.voxelDensity;
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

  // ---------- camera view keys ----------

  /** Camera keys (slicer convention): plain 1–6 snap to top / bottom / front /
   *  behind / left / right, plain F fits the part into the current viewport;
   *  Ctrl/⌘ + 0–6 also work (0 = default ISO — kept off the plain layer so a
   *  stray 0 doesn't yank the camera). */
  private onViewKey = (ev: KeyboardEvent) => {
    if (ev.altKey || ev.shiftKey) return;
    // Don't hijack keys while typing in a field (e.g. legend bound editor).
    const t = ev.target as HTMLElement | null;
    if (t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName))) return;
    const mod = ev.ctrlKey || ev.metaKey;
    if (!mod && (ev.key === "f" || ev.key === "F")) {
      ev.preventDefault();
      this.fitView();
      return;
    }
    const view = VIEW_KEYS[ev.key];
    if (!view || (!mod && ev.key === "0")) return;
    ev.preventDefault();
    this.setCameraView(view);
  };

  // ---------- rigid-body-mode animation ----------

  setRbmMode(mode: { t: number[]; r: number[]; center: number[] } | null) {
    this.results.setRbmMode(mode);
  }

  // ---------- result views ----------

  setDisplacements(disp: Float32Array | null, stats: { maxDisplacement: number } | null) {
    this.results.setDisplacements(disp, stats);
    // A new solution resets the field picker to |u| (store side); keep the
    // coloring component in step so it never colors by a stale X/Y/Z choice.
    this.colorMgr.resetDispComponent();
    this.callbacks.onAutoScale?.(this.results.autoScale);
    this.refreshView();
  }

  /** Choose what the deformed view colors by: -1 = |u| magnitude, 0/1/2 =
   *  signed X/Y/Z displacement component. */
  setDispComponent(comp: number) {
    if (!this.colorMgr.setDispComponent(comp)) return;
    this.callouts.clearCallouts(); // values belong to the previous field
    this.refreshView();
  }

  setVertexDensity(density: Float32Array | null) {
    this.results.vertexDensity = density;
    this.refreshView();
  }

  setDeformAnimate(on: boolean) {
    this.results.deformAnimate = on;
    if (!on) this.results.applyPositions(); // restore full deflection
  }

  /** Modal result active: animate as a symmetric ± swing (a vibrating mode
   *  passes through the undeformed shape) rather than the 0 → max loop. */
  setModalAnim(on: boolean) {
    this.results.modalAnim = on;
  }

  /** Faint full-hull ghost (the deactivated voxels) for the build preview.
   *  null clears it. */
  setBuildGhost(positions: Float32Array | null) {
    this.results.setBuildGhost(positions);
    // refreshView handles preview vs normal visibility (see its early branch).
    this.refreshView();
  }

  /** Growing deformed active hull (already-printed voxels, exaggeration baked
   *  in), jet-colored by normalised |u| (`mags`, 0–1). Replaced each preview
   *  frame; null clears it. */
  setBuildActive(positions: Float32Array | null, mags?: Float32Array | null) {
    this.results.setBuildActive(positions, mags);
    // refreshView handles preview vs normal visibility (see its early branch).
    this.refreshView();
  }

  /** Build-sim bed-peel heatmap: a flat triangle soup at the plate, jet-colored
   *  by `values / max`. Sits in world space under the part so the peel reads
   *  from a top/iso view. null clears it. */
  setPeelMap(positions: Float32Array | null, values: Float32Array | null, max: number) {
    this.results.setPeelMap(positions, values, max);
    // Part visibility depends on whether the peel map is shown.
    this.refreshView();
  }

  setVoxelMesh(
    hull: Float32Array | null,
    edges: Float32Array | null,
    density?: Float32Array | null
  ) {
    this.results.setVoxelMesh(hull, edges, density);
    if (this.sectionGizmo.exists()) this.rebuildCapGroups();
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
    this.results.setVoxelResult(positions, disp, edges, edgeDisp);
    if (this.sectionGizmo.exists()) this.rebuildCapGroups(); // new voxRes geometry
    this.refreshClipping();
    this.refreshView();
  }

  /** Switch the deformed view between the smooth STL and the voxel hull. */
  setResultSurface(surface: "stl" | "voxel") {
    if (!this.results.setResultSurface(surface)) return;
    this.callouts.clearCallouts(); // pinned to the previous surface's vertices
    this.refreshView();
  }

  /** Color the mesh-view cells by element density (0–1 ramp). */
  setMeshDensity(on: boolean) {
    this.results.setMeshDensity(on);
  }

  /** Force the 0–1 ramp on the mesh-view cells regardless of the density toggle
   *  — used by the inherent-strain layer view (the value channel carries the
   *  normalised source strength). */
  setMeshFieldColor(on: boolean) {
    this.results.setMeshFieldColor(on);
  }

  /** Voxel-true section active: the cut lives in the geometry, so the voxel
   *  group must NOT also be plane-clipped (and its stencil cap hides). */
  setVoxelCutActive(on: boolean) {
    if (this.voxelCutActive === on) return;
    this.voxelCutActive = on;
    this.refreshClipping();
    this.updateSectionVisibility();
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
          uv[2 * i + 1] = 0.25; // colormap row of the 2-row LUT (see makeLut)
        }
        geo.setAttribute("uv", new THREE.BufferAttribute(uv, 2));
        mat = new THREE.MeshStandardMaterial({
          map: this.colorMgr.lutRamp,
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
    if (this.viewMode !== mode) this.callouts.clearCallouts(); // callouts are per-view
    this.viewMode = mode;
    this.results.deformScale = deformScale;
    this.refreshView();
  }

  /** Stress/strain scalars per soup vertex; null reverts to |u| coloring.
   *  `flip` inverts the colormap (safety factor: red = the critical LOW).
   *  `signed` centers the color scale on 0 (signed von Mises: blue =
   *  compression, green ≈ unloaded, red = tension) — must match the store's
   *  symmetric `fieldRange` so the legend agrees with the surface. */
  setScalarField(
    values: Float32Array | null,
    flip = false,
    signed = false,
    range: { min: number; max: number } | null = null
  ) {
    this.callouts.clearCallouts(); // values belong to the previous field
    this.colorMgr.setScalarField(values, flip, signed, range);
    this.refreshView();
  }

  /** Volumetric payload for the field-mapped section cap (null clears — the
   *  cap falls back to its plain cut color). */
  setSectionVolume(data: SectionVolume | null) {
    this.sectionField.setVolume(data);
    this.syncCapField();
  }

  /** Clamp the color scale to a user range (null = auto). */
  setLegendRange(min: number | null, max: number | null) {
    this.colorMgr.setLegendRange(min, max);
    this.refreshView();
  }

  /** Toggle the min/max location markers; unit drives label formatting. */
  setShowExtremes(on: boolean, unit: string) {
    this.callouts.setShowExtremes(on, unit);
    this.refreshView();
  }

  /** Re-format the pinned value callouts after a display-unit change (their chip
   *  text is captured once at creation; `probeFormat` reads the live unit). */
  relabelCallouts() {
    this.callouts.relabelCallouts();
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
    this.sectionGizmo.proxy.rotateX(Math.PI); // local +Z (= plane normal) flips
    this.sectionGizmo.sync();
  }

  setSectionAxis(axis: "x" | "y" | "z") {
    this.sectionGizmo.proxy.quaternion.setFromUnitVectors(
      new THREE.Vector3(0, 0, 1),
      this.sectionNormalTowardCut(axis)
    );
    this.sectionGizmo.sync();
  }

  /** Axis-aligned section normal SIGNED so the camera sits on the clipped
   *  side — the cut always opens toward the viewer instead of hiding on the
   *  part's far side. No `axis`: the dominant axis of the view direction
   *  (initial plane on activation). Clipping keeps n·p + c ≥ 0, so "camera
   *  clipped" means the normal points WITH the view direction. */
  private sectionNormalTowardCut(axis?: "x" | "y" | "z"): THREE.Vector3 {
    const dir = this.partCenter().sub(this.camera.position); // view direction
    const a =
      axis ??
      (Math.abs(dir.x) >= Math.abs(dir.y) && Math.abs(dir.x) >= Math.abs(dir.z)
        ? "x"
        : Math.abs(dir.y) >= Math.abs(dir.z)
          ? "y"
          : "z");
    const n = new THREE.Vector3(a === "x" ? 1 : 0, a === "y" ? 1 : 0, a === "z" ? 1 : 0);
    if (n.dot(dir) < 0) n.negate();
    return n;
  }

  // ---------- symmetry plane (optimizer constraint) ----------

  /** Show/update the symmetry plane n·p = c. `enabled` is the store-side
   *  gate (checkbox on + Optimize step active + not running); the scene
   *  additionally hides the plane in result views. */
  setSymmetry(enabled: boolean, normal: [number, number, number], c: number) {
    this.symEnabled = enabled;
    if (enabled) this.symGizmo.ensure(); // re-ensuring refreshes the quad size
    if (!this.symGizmo.exists()) return;
    const n = new THREE.Vector3(...normal);
    if (n.lengthSq() < 1e-12) n.set(1, 0, 0);
    n.normalize();
    this.symGizmo.proxy.quaternion.setFromUnitVectors(new THREE.Vector3(0, 0, 1), n);
    // Any point on the plane works; pick the one closest to the part center
    // so the gizmo and quad sit centered on the part (robust to camera pans).
    const ctr = this.partCenter();
    const d = n.dot(ctr) - c;
    this.symGizmo.proxy.position.copy(ctr).addScaledVector(n, -d);
    this.symGizmo.updateQuadSize(); // refit the quad to the part at this orientation
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

  /** Symmetry gizmo drag (objectChange → sync): the quad was already refit;
   *  report the plane n·p = c to the store. */
  private onSymmetryChanged() {
    const n = new THREE.Vector3(0, 0, 1).applyQuaternion(this.symGizmo.proxy.quaternion);
    this.callbacks.onSymmetryMoved?.([n.x, n.y, n.z], n.dot(this.symGizmo.proxy.position));
  }

  /** Hide the plane outside editing contexts (result views). */
  private updateSymVisibility() {
    const show =
      this.symEnabled &&
      this.viewMode !== "deformed" &&
      this.viewMode !== "density" &&
      this.viewMode !== "infill";
    this.symGizmo.setVisible(show);
  }

  // ---------- section plane objects ----------

  private ensureSectionObjects() {
    if (!this.sectionGizmo.exists()) {
      // Through the PART's center — the orbit target may be panned anywhere,
      // and the part re-centers/rotates on import and orientation changes.
      // Normal along the dominant view axis, opening the cut TOWARD the
      // camera (the near half is the clipped one).
      this.sectionGizmo.proxy.position.copy(this.partCenter());
      this.sectionGizmo.proxy.quaternion.setFromUnitVectors(
        new THREE.Vector3(0, 0, 1),
        this.sectionNormalTowardCut()
      );
      this.sectionGizmo.ensure();
      this.sectionGizmo.sync();
    }
    this.rebuildCapGroups();
  }

  /** Section gizmo moved (drag or programmatic): the plane was already
   *  re-derived from the proxy; keep the stencil caps on it and notify. */
  private onSectionChanged() {
    // Caps lie exactly on the plane.
    for (const group of [this.capPart, this.capVoxel, this.capVoxRes]) {
      const cap = group[2] as THREE.Mesh | undefined;
      if (cap) {
        cap.position.copy(this.sectionGizmo.proxy.position);
        cap.quaternion.copy(this.sectionGizmo.proxy.quaternion);
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
      // The cut is looked at from the REMOVED side — the quad backfaces the
      // viewer there, so it must render double-sided or it culls away.
      side: THREE.DoubleSide,
      stencilWrite: true,
      stencilRef: 0,
      stencilFunc: THREE.NotEqualStencilFunc,
      stencilFail: THREE.ReplaceStencilOp,
      stencilZFail: THREE.ReplaceStencilOp,
      stencilZPass: THREE.ReplaceStencilOp,
    });
    this.capDisposables.push(capGeo, capMat);
    const cap = new THREE.Mesh(capGeo, capMat);
    // Result views swap in the field-mapped cap material (syncCapField);
    // keep the plain one around to swap back.
    cap.userData.plainMat = capMat;
    cap.renderOrder = order + 0.1;
    cap.onAfterRender = (renderer) => renderer.clearStencil();
    cap.position.copy(this.sectionGizmo.proxy.position);
    cap.quaternion.copy(this.sectionGizmo.proxy.quaternion);
    const group = [back, front, cap];
    for (const o of group) this.scene.add(o);
    return group;
  }

  /** (Re)create cap groups for the part mesh, the voxel hull, and the
   *  voxel-result surface. */
  private rebuildCapGroups() {
    if (!this.sectionGizmo.exists()) return; // section never enabled yet
    for (const o of [...this.capPart, ...this.capVoxel, ...this.capVoxRes]) this.scene.remove(o);
    for (const d of this.capDisposables) d.dispose();
    this.capPart = [];
    this.capVoxel = [];
    this.capVoxRes = [];
    this.capDisposables = [];
    if (this.geometry) {
      this.capPart = this.makeCapGroup(this.geometry, CUT_FACE_COLOR, 1);
    }
    const hull = this.results.voxelGroup.children.find(
      (c): c is THREE.Mesh => c instanceof THREE.Mesh
    );
    if (hull) {
      this.capVoxel = this.makeCapGroup(hull.geometry as THREE.BufferGeometry, CUT_FACE_COLOR, 3);
    }
    // The voxel-result geometry is displaced in place, so its stencil meshes
    // track the deformed shape automatically.
    if (this.results.voxRes) {
      this.capVoxRes = this.makeCapGroup(this.results.voxRes.geo, CUT_FACE_COLOR, 5);
    }
    this.updateSectionVisibility();
    this.syncCapField();
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
    apply(this.featureEdgeLines?.material, partPlanes);
    for (const c of this.results.voxelGroup.children) apply((c as THREE.Mesh).material, voxelPlanes);
    for (const c of this.results.voxRes?.group.children ?? []) {
      apply((c as THREE.Mesh).material, planes);
    }
    for (const m of this.regionMeshes) apply(m.material, planes);
    apply(this.optShapeMesh?.material ?? undefined, planes);
  }

  private updateSectionVisibility() {
    // Gizmo helpers + the proxy (which carries the plane quad).
    this.sectionGizmo.setVisible(this.sectionOn);
    // Caps only where an OPAQUE solid is being cut (ghosted part: see inside).
    const mat = this.mesh?.material as THREE.MeshStandardMaterial | undefined;
    const partCap = this.sectionOn && !!this.mesh?.visible && !!mat && !mat.transparent;
    for (const o of this.capPart) o.visible = partCap;
    const voxCap = this.sectionOn && this.results.voxelGroup.visible && !this.voxelCutActive;
    for (const o of this.capVoxel) o.visible = voxCap;
    const voxResCap = this.sectionOn && !!this.results.voxRes?.group.visible;
    for (const o of this.capVoxRes) o.visible = voxResCap;
  }

  /** Swap the cap quads between the plain cut color and the field-mapped
   *  material (result views with a loaded volume), and sync the shader's
   *  color scale to the EXACT normalization the surface coloring used. */
  private syncCapField() {
    const useField = this.sectionField.active && this.viewMode === "deformed";
    for (const group of [this.capPart, this.capVoxRes]) {
      const cap = group[2] as THREE.Mesh | undefined;
      if (!cap) continue;
      const want = useField
        ? this.sectionField.material
        : (cap.userData.plainMat as THREE.Material);
      if (cap.material !== want) cap.material = want;
    }
    const r = this.colorMgr.appliedRange;
    if (useField && r) {
      this.sectionField.setRange(r.lo, r.hi, r.flip, this.colorMgr.dispComponentValue);
    }
    // Third extreme marker: the volumetric (interior) extreme rides the same
    // payload — shown by the CalloutManager only when it beats the surface.
    const vol = this.sectionField.range;
    const sf = this.colorMgr.scalarFieldData;
    this.callouts.setInteriorExtreme(
      vol && sf
        ? { flip: sf.flip, min: vol.min, max: vol.max, minAt: vol.minAt, maxAt: vol.maxAt }
        : null
    );
  }

  /** Re-derive positions, colors, part opacity, and overlay visibility. */
  private refreshView() {
    if (!this.mesh) return;
    // Build-sim live preview overrides everything: only the growing active hull
    // is shown; the normal model/voxel/result surfaces are hidden.
    if (this.results.buildActive || this.results.buildGhost) {
      this.results.buildGroup.visible = true;
      this.mesh.visible = false;
      this.results.voxelGroup.visible = false;
      if (this.results.voxRes) this.results.voxRes.group.visible = false;
      if (this.wireframeLines) this.wireframeLines.visible = false;
      if (this.featureEdgeLines) this.featureEdgeLines.visible = false;
      this.bcMarkers.visible = false;
      this.callouts.setBcCalloutsVisible(false);
      return;
    }
    this.results.buildGroup.visible = false;
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
    const voxResult = this.results.voxResultActive();
    // Part Topo: the optimized body IS the result — drop the original envelope
    // hull in the density/regions views so it doesn't moiré against the
    // coincident body surface (the carved regions sit inside it; the retained
    // faces sit exactly on it).
    const hideHull = this.resultSolid && (this.viewMode === "density" || infill);
    // Bed-peel heatmap on screen: hide the part so the plate map reads cleanly
    // (no need to look under or through the model).
    this.mesh.visible = !voxResult && !hideHull && !this.results.peelMap;
    this.results.voxelGroup.visible = this.viewMode === "mesh";
    if (this.results.voxRes) this.results.voxRes.group.visible = voxResult;
    // Wireframe overlay: undeformed model views only (its lines are built from
    // the rest shape, so it would not track a deformed result).
    if (this.wireframeLines) {
      this.wireframeLines.visible =
        this.wireframeOn &&
        this.mesh.visible &&
        (this.viewMode === "setup" || this.viewMode === "mesh");
    }
    // Feature edges share the wireframe's rest-shape rule (they are built
    // from the undeformed pose) but stay on the OPAQUE setup surface only —
    // in the ghosted mesh view they would read as a fake wireframe.
    if (this.featureEdgeLines) {
      this.featureEdgeLines.visible =
        this.featureEdgesOn && this.mesh.visible && this.viewMode === "setup";
    }
    this.regionMeshes.forEach((m, i) => {
      m.visible = infill && this.regionVisible[i] !== false;
    });
    this.updateMarkerVisibility();
    this.updateSectionVisibility();
    this.updateSymVisibility();
    this.refreshClipping(); // mesh view exempts the ghost STL from the cut
    this.results.applyPositions();
    this.colorMgr.applyColors();
    this.syncCapField(); // cap follows the range applyColors just used
  }

  /** Set discrete contour banding + the band count. Rewrites the SHARED jet LUT
   *  in place (both the smooth surface and the voxel-result surface sample it, so
   *  they update together) — quantized into `count` flat steps with nearest
   *  sampling for crisp band edges, or the smooth ramp when off. */
  setBanded(on: boolean, count?: number) {
    this.colorMgr.setBanded(on, count);
  }

  /** Small screen-aligned value chip (canvas-rendered text on a light pill),
   *  world-scaled to the part. Disposal goes through markerDisposables. */
  /** Queue a load's value label as a result-style callout anchored at `world`
   *  (the picked patch / CG / bbox centre) in the load's roster colour — a small
   *  dot + an offset value chip + a leader line, so the value never covers the
   *  picked spot. Flushed to the CalloutManager after rebuildBcMarkers. */
  private pushBcCallout(bc: Bc, world: THREE.Vector3, text: string, ghost: boolean) {
    const c = BC_COLORS[bc.kind] ?? new THREE.Color(0x888888);
    this.pendingBcCallouts.push({ id: bc.id, world: world.clone(), text, color: `#${c.getHexString()}`, ghost });
  }

  private tick() {
    if (this.contextLost) return; // GPU is mid-reset — don't touch the dead context
    if (this.results.rbmMode) {
      const t = this.clock.getElapsedTime();
      this.results.applyPositions(Math.sin(t * 2.0 * Math.PI * 0.66), 1, true);
    } else if (
      this.results.deformAnimate &&
      this.viewMode === "deformed" &&
      this.results.displacements
    ) {
      const t = this.clock.getElapsedTime();
      // Modal: symmetric ± swing (+A → 0 → −A → 0), a vibrating mode shape.
      // Static deflection: one-sided 0 → max → 0 loop. Both at a 2.4 s period
      // (fixed VISUAL rate — the real frequency is shown as a number, not speed).
      const frac = this.results.modalAnim
        ? Math.sin((2 * Math.PI * t) / 2.4)
        : 0.5 - 0.5 * Math.cos((2 * Math.PI * t) / 2.4);
      this.results.applyPositions(undefined, frac, true);
    }
    this.controls.update();
    if (this.headlight) {
      const cam = this.camera;
      const d = cam.position.distanceTo(this.controls.target) || 100;
      this.headlight.position
        .copy(cam.position)
        .addScaledVector(this._hlRight.set(1, 0, 0).applyQuaternion(cam.quaternion), d * 0.5)
        .addScaledVector(this._hlUp.set(0, 1, 0).applyQuaternion(cam.quaternion), d * 0.6);
      this.headlight.target.position.copy(this.controls.target);
    }
    const r = this.renderer;
    if (this.viewW <= 0 || this.viewH <= 0) return;
    r.setScissorTest(false);
    r.setViewport(0, 0, this.viewW, this.viewH);
    r.clear();
    r.render(this.scene, this.camera);
    this.colorMgr.markColorsUploaded(); // the color buffer (if any) was just uploaded
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
    this.callouts.projectExtremes();
    this.callouts.updateCallouts();
    this.callouts.projectBcCallouts();
  }
}

/** Feature-edge corner pairs of a CONFORMING triangle soup: edges shared by
 *  exactly two triangles whose dihedral angle exceeds the threshold. Corner
 *  indices are pose-invariant — re-read coordinates after a rigid move. */
function deriveEdgePairs(pos: Float32Array, angleDeg: number): Uint32Array {
  const nTri = (pos.length / 9) | 0;
  const nCorners = nTri * 3;
  // Weld corners by quantized position.
  const weld = new Uint32Array(nCorners);
  const map = new Map<string, number>();
  let nv = 0;
  for (let c = 0; c < nCorners; c++) {
    const key = `${Math.round(pos[3 * c] * 1e4)},${Math.round(pos[3 * c + 1] * 1e4)},${Math.round(pos[3 * c + 2] * 1e4)}`;
    let id = map.get(key);
    if (id === undefined) {
      id = nv++;
      map.set(key, id);
    }
    weld[c] = id;
  }
  // Unit face normals.
  const unit = new Float32Array(nTri * 3);
  for (let t = 0; t < nTri; t++) {
    const o = 9 * t;
    const ax = pos[o + 3] - pos[o];
    const ay = pos[o + 4] - pos[o + 1];
    const az = pos[o + 5] - pos[o + 2];
    const bx = pos[o + 6] - pos[o];
    const by = pos[o + 7] - pos[o + 1];
    const bz = pos[o + 8] - pos[o + 2];
    const cx = ay * bz - az * by;
    const cy = az * bx - ax * bz;
    const cz = ax * by - ay * bx;
    const l = Math.hypot(cx, cy, cz) || 1;
    unit[3 * t] = cx / l;
    unit[3 * t + 1] = cy / l;
    unit[3 * t + 2] = cz / l;
  }
  const cosCrease = Math.cos((angleDeg * Math.PI) / 180);
  const edges = new Map<number, { ca: number; cb: number; tri: number; n: number; tri2: number }>();
  for (let t = 0; t < nTri; t++) {
    for (let e = 0; e < 3; e++) {
      const ca = 3 * t + e;
      const cb = 3 * t + ((e + 1) % 3);
      const va = weld[ca];
      const vb = weld[cb];
      if (va === vb) continue; // degenerate
      const key = (va < vb ? va : vb) * 4194304 + (va < vb ? vb : va);
      const ent = edges.get(key);
      if (!ent) edges.set(key, { ca, cb, tri: t, n: 1, tri2: -1 });
      else {
        ent.n++;
        ent.tri2 = t;
      }
    }
  }
  const pairs: number[] = [];
  for (const ent of edges.values()) {
    if (ent.n !== 2) continue;
    const a = ent.tri;
    const b = ent.tri2;
    const dot =
      unit[3 * a] * unit[3 * b] + unit[3 * a + 1] * unit[3 * b + 1] + unit[3 * a + 2] * unit[3 * b + 2];
    if (dot < cosCrease) pairs.push(ent.ca, ent.cb);
  }
  return Uint32Array.from(pairs);
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
