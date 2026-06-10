// Imperative three.js layer: mesh display, patch hover/select, brush,
// BC coloring, rigid-body-mode animation, deformed-shape overlay.

import * as THREE from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";
import type { Bc, LoadedModel, SolveStats } from "../types";
import type { Tool } from "../store";

const BASE_COLOR = new THREE.Color(0x9aa3ad);
const HOVER_TINT = new THREE.Color(0xc9d4e0);
const BC_COLORS: Record<string, THREE.Color> = {
  fixed: new THREE.Color(0x3b82f6),
  frictionless: new THREE.Color(0x22d3ee),
  force: new THREE.Color(0xef4444),
  pressure: new THREE.Color(0xf59e0b),
};

export interface SceneCallbacks {
  /** Patch clicked in select mode: toggle its triangles in the active BC. */
  onPickPatch?: (tris: Uint32Array, additive: boolean) => void;
  /** Brush stroke: triangles under the brush. */
  onBrush?: (tris: Uint32Array, erase: boolean) => void;
}

export class SceneManager {
  private renderer!: THREE.WebGLRenderer;
  private scene = new THREE.Scene();
  private camera!: THREE.PerspectiveCamera;
  private controls!: OrbitControls;
  private raycaster = new THREE.Raycaster();
  private pointer = new THREE.Vector2();

  private mesh: THREE.Mesh | null = null;
  private geometry: THREE.BufferGeometry | null = null;
  private basePositions: Float32Array | null = null;
  private colors: Float32Array | null = null;
  private patchIds: Uint32Array | null = null;
  private patchToTris = new Map<number, number[]>();
  private triCount = 0;
  private bboxDiag = 100;

  private bcs: Bc[] = [];
  private activeBcId: string | null = null;
  private triBcColor: (THREE.Color | null)[] = [];
  private hoverPatch: number | null = null;

  private tool: Tool = "orbit";
  private brushRadius = 3;
  private brushErase = false;
  private brushing = false;
  private brushCursor: THREE.Mesh | null = null;

  private arrows: THREE.Object3D[] = [];

  // Rigid-body-mode animation
  private rbmMode: { t: number[]; r: number[]; center: number[] } | null = null;
  private rbmAmp = 1;

  // Deformed view
  private displacements: Float32Array | null = null;
  private showDeformed = false;
  private deformScale = 1;
  private autoScale = 1;

  private clock = new THREE.Clock();
  private callbacks: SceneCallbacks = {};
  private disposed = false;

  init(canvas: HTMLCanvasElement, callbacks: SceneCallbacks) {
    this.callbacks = callbacks;
    this.renderer = new THREE.WebGLRenderer({ canvas, antialias: true });
    this.renderer.setPixelRatio(window.devicePixelRatio);
    this.scene.background = new THREE.Color(0x14181d);

    this.camera = new THREE.PerspectiveCamera(45, 1, 0.1, 10000);
    this.camera.position.set(120, -160, 110);
    this.camera.up.set(0, 0, 1); // printer convention: Z up

    this.controls = new OrbitControls(this.camera, canvas);
    this.controls.enableDamping = true;
    this.controls.dampingFactor = 0.12;

    const hemi = new THREE.HemisphereLight(0xffffff, 0x30363d, 1.0);
    this.scene.add(hemi);
    const key = new THREE.DirectionalLight(0xffffff, 1.6);
    key.position.set(1, -1.2, 1.8);
    this.scene.add(key);
    const fill = new THREE.DirectionalLight(0xa0b4ff, 0.5);
    fill.position.set(-1.5, 1, -0.5);
    this.scene.add(fill);

    const grid = new THREE.GridHelper(400, 40, 0x39414b, 0x232a32);
    grid.rotation.x = Math.PI / 2; // Z-up
    this.scene.add(grid);

    canvas.addEventListener("pointermove", this.onPointerMove);
    canvas.addEventListener("pointerdown", this.onPointerDown);
    canvas.addEventListener("pointerup", this.onPointerUp);
    canvas.addEventListener("pointerleave", () => this.setHover(null));

    const loop = () => {
      if (this.disposed) return;
      requestAnimationFrame(loop);
      this.tick();
    };
    loop();
  }

  dispose() {
    this.disposed = true;
    this.renderer?.dispose();
  }

  resize(width: number, height: number) {
    if (!this.renderer) return;
    this.renderer.setSize(width, height, false);
    this.camera.aspect = width / height;
    this.camera.updateProjectionMatrix();
  }

  setTool(tool: Tool, brushRadius: number, brushErase: boolean) {
    this.tool = tool;
    this.brushRadius = brushRadius;
    this.brushErase = brushErase;
    this.controls.enabled = tool !== "brush";
    if (this.brushCursor) this.brushCursor.visible = tool === "brush";
    if (tool !== "select") this.setHover(null);
  }

  // ---------- model ----------

  setModel(model: LoadedModel) {
    if (this.mesh) {
      this.scene.remove(this.mesh);
      this.geometry?.dispose();
      (this.mesh.material as THREE.Material).dispose();
    }
    this.triCount = model.triCount;
    this.basePositions = new Float32Array(model.positions);
    this.colors = new Float32Array(this.triCount * 9);
    this.displacements = null;
    this.rbmMode = null;
    this.showDeformed = false;

    this.geometry = new THREE.BufferGeometry();
    this.geometry.setAttribute("position", new THREE.BufferAttribute(model.positions, 3));
    this.geometry.setAttribute("color", new THREE.BufferAttribute(this.colors, 3));
    this.geometry.computeVertexNormals();

    const material = new THREE.MeshStandardMaterial({
      vertexColors: true,
      metalness: 0.05,
      roughness: 0.72,
      side: THREE.DoubleSide,
    });
    this.mesh = new THREE.Mesh(this.geometry, material);
    this.scene.add(this.mesh);

    this.setPatchIds(model.patchIds);
    this.bcs = [];
    this.activeBcId = null;
    this.repaint();

    // Fit camera.
    const [lx, ly, lz, hx, hy, hz] = model.bbox;
    const center = new THREE.Vector3((lx + hx) / 2, (ly + hy) / 2, (lz + hz) / 2);
    this.bboxDiag = Math.hypot(hx - lx, hy - ly, hz - lz) || 100;
    const dist = this.bboxDiag * 1.8;
    this.camera.position.set(center.x + dist * 0.7, center.y - dist * 0.8, center.z + dist * 0.55);
    this.controls.target.copy(center);
    this.camera.near = this.bboxDiag / 1000;
    this.camera.far = this.bboxDiag * 50;
    this.camera.updateProjectionMatrix();
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

  setBcs(bcs: Bc[], activeBcId: string | null) {
    this.bcs = bcs;
    this.activeBcId = activeBcId;
    this.repaint();
    this.rebuildArrows();
  }

  private rebuildArrows() {
    for (const a of this.arrows) this.scene.remove(a);
    this.arrows = [];
    if (!this.basePositions) return;
    for (const bc of this.bcs) {
      if (bc.kind !== "force" || bc.tris.length === 0 || !bc.force) continue;
      const centroid = this.selectionCentroid(bc.tris);
      const f = new THREE.Vector3(...bc.force);
      if (f.lengthSq() === 0) continue;
      const dir = f.clone().normalize();
      const len = this.bboxDiag * 0.18;
      const arrow = new THREE.ArrowHelper(
        dir,
        centroid.clone().sub(dir.clone().multiplyScalar(len)),
        len,
        0xff5252,
        len * 0.25,
        len * 0.12
      );
      this.scene.add(arrow);
      this.arrows.push(arrow);
    }
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
        c = triColor[t] ? triColor[t]!.clone().lerp(HOVER_TINT, 0.45) : HOVER_TINT;
      }
      for (let v = 0; v < 3; v++) {
        this.colors[9 * t + 3 * v] = c.r;
        this.colors[9 * t + 3 * v + 1] = c.g;
        this.colors[9 * t + 3 * v + 2] = c.b;
      }
    }
    (this.geometry.getAttribute("color") as THREE.BufferAttribute).needsUpdate = true;
  }

  private setHover(patch: number | null) {
    if (patch === this.hoverPatch) return;
    this.hoverPatch = patch;
    this.repaint();
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

  private onPointerMove = (ev: PointerEvent) => {
    if (!this.mesh) return;
    if (this.tool === "select") {
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
    }
  };

  private onPointerDown = (ev: PointerEvent) => {
    if (ev.button !== 0 || !this.mesh) return;
    if (this.tool === "select") {
      const hit = this.rayTri(ev);
      if (hit && hit.faceIndex != null && this.patchIds) {
        const patch = this.patchIds[hit.faceIndex];
        const tris = this.patchToTris.get(patch);
        if (tris) this.callbacks.onPickPatch?.(new Uint32Array(tris), !ev.shiftKey);
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

  // ---------- deformed view ----------

  setDisplacements(disp: Float32Array | null, stats: SolveStats | null) {
    this.displacements = disp;
    if (disp && stats && stats.maxDisplacement > 0) {
      this.autoScale = (0.08 * this.bboxDiag) / stats.maxDisplacement;
    } else {
      this.autoScale = 1;
    }
    this.applyPositions();
    this.applyDispColors();
  }

  setDeformedView(show: boolean, scale: number) {
    this.showDeformed = show;
    this.deformScale = scale;
    this.applyPositions();
    this.applyDispColors();
  }

  private applyDispColors() {
    if (!this.geometry || !this.colors) return;
    if (!this.displacements || !this.showDeformed) {
      this.repaint();
      return;
    }
    // Color by displacement magnitude, viridis-ish ramp.
    const d = this.displacements;
    let maxMag = 1e-12;
    const mags = new Float32Array(d.length / 3);
    for (let i = 0; i < mags.length; i++) {
      mags[i] = Math.hypot(d[3 * i], d[3 * i + 1], d[3 * i + 2]);
      maxMag = Math.max(maxMag, mags[i]);
    }
    const c = new THREE.Color();
    for (let i = 0; i < mags.length; i++) {
      ramp(mags[i] / maxMag, c);
      this.colors[3 * i] = c.r;
      this.colors[3 * i + 1] = c.g;
      this.colors[3 * i + 2] = c.b;
    }
    (this.geometry.getAttribute("color") as THREE.BufferAttribute).needsUpdate = true;
  }

  private applyPositions(rbmOffset?: number) {
    if (!this.geometry || !this.basePositions) return;
    const attr = this.geometry.getAttribute("position") as THREE.BufferAttribute;
    const out = attr.array as Float32Array;
    const base = this.basePositions;
    if (this.rbmMode && rbmOffset !== undefined) {
      const m = this.rbmMode;
      const s = rbmOffset * this.rbmAmp;
      for (let i = 0; i < base.length; i += 3) {
        const u = this.modeDisplacement(m, base[i], base[i + 1], base[i + 2]);
        out[i] = base[i] + s * u[0];
        out[i + 1] = base[i + 1] + s * u[1];
        out[i + 2] = base[i + 2] + s * u[2];
      }
    } else if (this.displacements && this.showDeformed) {
      const d = this.displacements;
      const s = this.autoScale * this.deformScale;
      for (let i = 0; i < base.length; i++) out[i] = base[i] + s * d[i];
    } else {
      out.set(base);
    }
    attr.needsUpdate = true;
    this.geometry.computeVertexNormals();
  }

  private tick() {
    if (this.rbmMode) {
      const t = this.clock.getElapsedTime();
      this.applyPositions(Math.sin(t * 2.0 * Math.PI * 0.66));
    }
    this.controls.update();
    this.renderer.render(this.scene, this.camera);
  }
}

/** Compact blue→cyan→yellow→red ramp. */
function ramp(x: number, out: THREE.Color) {
  const t = Math.min(1, Math.max(0, x));
  if (t < 0.33) out.setRGB(0.15, 0.3 + 1.8 * t, 0.9);
  else if (t < 0.66) out.setRGB(0.15 + 2.4 * (t - 0.33), 0.9, 0.9 - 2.4 * (t - 0.33));
  else out.setRGB(0.95, 0.9 - 2.4 * (t - 0.66), 0.1);
}
