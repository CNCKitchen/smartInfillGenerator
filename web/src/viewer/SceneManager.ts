// Imperative three.js layer: mesh display, patch hover/select, brush,
// BC coloring + support glyphs, axis gizmo, rigid-body-mode animation,
// deformed-shape overlay (with looping animation), density/region/voxel views.

import * as THREE from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";
import type { Bc, LoadedModel } from "../types";
import type { Tool, ViewMode } from "../store";
import type { OptRegion } from "../engine/EngineClient";

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
  /** Viewer picked a new deformation autoscale (display exaggeration base). */
  onAutoScale?: (autoScale: number) => void;
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

  // Rigid-body-mode animation
  private rbmMode: { t: number[]; r: number[]; center: number[] } | null = null;
  private rbmAmp = 1;

  // Result views
  private displacements: Float32Array | null = null;
  private vertexDensity: Float32Array | null = null;
  private regionMeshes: THREE.Mesh[] = [];
  private regionVisible: boolean[] = [];
  private viewMode: ViewMode = "setup";
  private deformScale = 1;
  private autoScale = 1;
  private deformAnimate = false;

  // Live optimization skeleton / density-threshold cutaway.
  private optShapeMesh: THREE.Mesh | null = null;

  // Colormaps are sampled per-fragment from 1D LUT textures via the uv
  // channel — per-vertex colors interpolate straight through RGB and turn
  // jet into blue→purple→red on coarse meshes.
  private lutJet = makeLut(jet);
  private lutRamp = makeLut(ramp);
  private uvs: Float32Array | null = null;
  private scalarMode: "none" | "jet" | "ramp" = "none";

  private clock = new THREE.Clock();
  private callbacks: SceneCallbacks = {};
  private disposed = false;

  init(canvas: HTMLCanvasElement, callbacks: SceneCallbacks) {
    this.callbacks = callbacks;
    this.renderer = new THREE.WebGLRenderer({ canvas, antialias: true });
    this.renderer.setPixelRatio(window.devicePixelRatio);
    this.renderer.autoClear = false;
    this.scene.background = new THREE.Color(0x14181d);

    this.camera = new THREE.OrthographicCamera(-120, 120, 120, -120, 0.1, 10000);
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

    this.scene.add(this.bcMarkers);
    this.scene.add(this.voxelGroup);
    this.buildGizmo();

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

  setTool(tool: Tool, brushRadius: number, brushErase: boolean) {
    this.tool = tool;
    this.brushRadius = brushRadius;
    this.brushErase = brushErase;
    this.controls.enabled = tool !== "brush";
    if (this.brushCursor) this.brushCursor.visible = tool === "brush";
    if (tool !== "select") this.setHover(null);
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
      new THREE.MeshBasicMaterial({ color: 0x8fa0b3 })
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
    this.vertexDensity = null;
    this.rbmMode = null;
    this.viewMode = "setup";
    this.setRegions(null);
    this.setVoxelMesh(null, null);
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
    });
    this.mesh = new THREE.Mesh(this.geometry, material);
    this.scene.add(this.mesh);

    this.setPatchIds(model.patchIds);
    this.bcs = [];
    this.activeBcId = null;
    this.rebuildBcMarkers();
    this.repaint();

    // Fit camera (parallel projection: frustum half-height from the bbox).
    const [lx, ly, lz, hx, hy, hz] = model.bbox;
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
    this.rebuildBcMarkers();
  }

  /** Force arrows + classic support triangles (4-sided cones read as ▽). */
  private rebuildBcMarkers() {
    for (const d of this.markerDisposables) d.dispose();
    this.markerDisposables = [];
    this.bcMarkers.clear();
    if (!this.basePositions) return;
    for (const bc of this.bcs) {
      if (bc.tris.length === 0) continue;
      if (bc.kind === "force" && bc.force) {
        const f = new THREE.Vector3(...bc.force);
        if (f.lengthSq() === 0) continue;
        const centroid = this.selectionCentroid(bc.tris);
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
        this.markerDisposables.push(arrow);
        this.bcMarkers.add(arrow);
      } else if (bc.kind === "fixed" || bc.kind === "frictionless") {
        this.buildSupportGlyphs(bc);
      }
    }
    this.updateMarkerVisibility();
  }

  private buildSupportGlyphs(bc: Bc) {
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
    });
    this.markerDisposables.push(coneGeo, mat);
    const up = new THREE.Vector3(0, 1, 0);
    let plateGeo: THREE.CylinderGeometry | null = null;
    if (bc.kind === "frictionless") {
      plateGeo = new THREE.CylinderGeometry(rCone * 1.25, rCone * 1.25, rCone * 0.18, 16);
      this.markerDisposables.push(plateGeo);
    }
    for (const it of chosen) {
      // Tip touches the surface; body sticks outward along the normal.
      // Frictionless: small gap + plate = "support that can slide".
      const gap = bc.kind === "frictionless" ? 0.35 * hCone : 0;
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

  // ---------- result views ----------

  setDisplacements(disp: Float32Array | null, stats: { maxDisplacement: number } | null) {
    this.displacements = disp;
    if (disp && stats && stats.maxDisplacement > 0) {
      this.autoScale = (0.08 * this.bboxDiag) / stats.maxDisplacement;
    } else {
      this.autoScale = 1;
    }
    this.callbacks.onAutoScale?.(this.autoScale);
    this.refreshView();
  }

  setVertexDensity(density: Float32Array | null) {
    this.vertexDensity = density;
    this.refreshView();
  }

  setDeformAnimate(on: boolean) {
    this.deformAnimate = on;
    if (!on) this.applyPositions(); // restore full deflection
  }

  setVoxelMesh(hull: Float32Array | null, edges: Float32Array | null) {
    for (const d of this.voxelDisposables) d.dispose();
    this.voxelDisposables = [];
    this.voxelGroup.clear();
    if (hull && hull.length) {
      const geo = new THREE.BufferGeometry();
      geo.setAttribute("position", new THREE.BufferAttribute(hull, 3));
      geo.computeVertexNormals(); // soup → flat per-face normals
      const mat = new THREE.MeshStandardMaterial({
        color: 0x7e8b99,
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
    }
    if (edges && edges.length) {
      const geo = new THREE.BufferGeometry();
      geo.setAttribute("position", new THREE.BufferAttribute(edges, 3));
      const mat = new THREE.LineBasicMaterial({ color: 0x12161b, transparent: true, opacity: 0.6 });
      this.voxelDisposables.push(geo, mat);
      this.voxelGroup.add(new THREE.LineSegments(geo, mat));
    }
    this.refreshView();
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
        });
      } else {
        mat = new THREE.MeshStandardMaterial({
          color: 0xd9974f,
          roughness: 0.55,
          metalness: 0.05,
          side: THREE.DoubleSide,
        });
      }
      this.optShapeMesh = new THREE.Mesh(geo, mat);
      this.scene.add(this.optShapeMesh);
    }
    this.refreshView();
  }

  setRegionVisibility(vis: boolean[]) {
    this.regionVisible = vis;
    this.refreshView();
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
    for (const r of regions) {
      const geo = new THREE.BufferGeometry();
      geo.setAttribute("position", new THREE.BufferAttribute(r.positions, 3));
      geo.setIndex(new THREE.BufferAttribute(r.indices, 1));
      geo.computeVertexNormals();
      ramp(Math.min(1, r.density / 0.8), c);
      const mat = new THREE.MeshStandardMaterial({
        color: c.clone(),
        transparent: true,
        opacity: 0.62,
        roughness: 0.6,
        metalness: 0.0,
        depthWrite: false,
        side: THREE.DoubleSide,
      });
      const mesh = new THREE.Mesh(geo, mat);
      mesh.visible = false;
      this.scene.add(mesh);
      this.regionMeshes.push(mesh);
    }
    this.refreshView();
  }

  setViewState(mode: ViewMode, deformScale: number) {
    this.viewMode = mode;
    this.deformScale = deformScale;
    this.refreshView();
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
    const ghost = infill || showShape;
    mat.transparent = ghost;
    mat.opacity = ghost ? 0.15 : 1.0;
    mat.depthWrite = !ghost;
    mat.needsUpdate = true;
    this.mesh.visible = this.viewMode !== "mesh";
    this.voxelGroup.visible = this.viewMode === "mesh";
    this.regionMeshes.forEach((m, i) => {
      m.visible = infill && this.regionVisible[i] !== false;
    });
    this.updateMarkerVisibility();
    this.applyPositions();
    this.applyColors();
  }

  /** Switch the part material between BC vertex colors and a scalar LUT. */
  private setSurfaceMaterialMode(mode: "none" | "jet" | "ramp") {
    if (!this.mesh || mode === this.scalarMode) return;
    this.scalarMode = mode;
    const mat = this.mesh.material as THREE.MeshStandardMaterial;
    if (mode === "none") {
      mat.map = null;
      mat.vertexColors = true;
    } else {
      mat.map = mode === "jet" ? this.lutJet : this.lutRamp;
      mat.vertexColors = false;
    }
    mat.needsUpdate = true;
  }

  private applyColors() {
    if (!this.geometry || !this.colors || !this.uvs) return;
    const uvAttr = this.geometry.getAttribute("uv") as THREE.BufferAttribute;
    if (this.viewMode === "deformed" && this.displacements) {
      const d = this.displacements;
      let maxMag = 1e-12;
      const n = d.length / 3;
      const mags = new Float32Array(n);
      for (let i = 0; i < n; i++) {
        mags[i] = Math.hypot(d[3 * i], d[3 * i + 1], d[3 * i + 2]);
        maxMag = Math.max(maxMag, mags[i]);
      }
      for (let i = 0; i < n; i++) {
        this.uvs[2 * i] = mags[i] / maxMag;
        this.uvs[2 * i + 1] = 0.5;
      }
      uvAttr.needsUpdate = true;
      this.setSurfaceMaterialMode("jet");
      return;
    }
    if (this.viewMode === "density" && this.vertexDensity) {
      for (let i = 0; i < this.vertexDensity.length; i++) {
        this.uvs[2 * i] = Math.min(1, this.vertexDensity[i] / 0.8);
        this.uvs[2 * i + 1] = 0.5;
      }
      uvAttr.needsUpdate = true;
      this.setSurfaceMaterialMode("ramp");
      return;
    }
    this.setSurfaceMaterialMode("none");
    this.repaint();
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
  }

  private tick() {
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
  }
}

function makeTextSprite(text: string, color: number): THREE.Sprite {
  const canvas = document.createElement("canvas");
  canvas.width = 64;
  canvas.height = 64;
  const ctx = canvas.getContext("2d")!;
  ctx.font = "bold 44px 'Segoe UI', system-ui, sans-serif";
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

/** Compact blue→cyan→yellow→red ramp (density + region colors). */
function ramp(x: number, out: THREE.Color) {
  const t = Math.min(1, Math.max(0, x));
  if (t < 0.33) out.setRGB(0.15, 0.3 + 1.8 * t, 0.9);
  else if (t < 0.66) out.setRGB(0.15 + 2.4 * (t - 0.33), 0.9, 0.9 - 2.4 * (t - 0.33));
  else out.setRGB(0.95, 0.9 - 2.4 * (t - 0.66), 0.1);
}

/** Classic jet colormap (displacement view). */
function jet(x: number, out: THREE.Color) {
  const t = Math.min(1, Math.max(0, x));
  const r = Math.min(1, Math.max(0, 1.5 - Math.abs(4 * t - 3)));
  const g = Math.min(1, Math.max(0, 1.5 - Math.abs(4 * t - 2)));
  const b = Math.min(1, Math.max(0, 1.5 - Math.abs(4 * t - 1)));
  out.setRGB(r, g, b);
}

/** Bake a colormap into a 1D texture, sampled per-fragment via uv.x. */
function makeLut(fn: (t: number, out: THREE.Color) => void): THREE.DataTexture {
  const n = 256;
  const data = new Uint8Array(n * 4);
  const c = new THREE.Color();
  for (let i = 0; i < n; i++) {
    fn(i / (n - 1), c);
    data[4 * i] = Math.round(255 * c.r);
    data[4 * i + 1] = Math.round(255 * c.g);
    data[4 * i + 2] = Math.round(255 * c.b);
    data[4 * i + 3] = 255;
  }
  const tex = new THREE.DataTexture(data, n, 1, THREE.RGBAFormat);
  tex.colorSpace = THREE.SRGBColorSpace;
  tex.minFilter = THREE.LinearFilter;
  tex.magFilter = THREE.LinearFilter;
  tex.needsUpdate = true;
  return tex;
}
