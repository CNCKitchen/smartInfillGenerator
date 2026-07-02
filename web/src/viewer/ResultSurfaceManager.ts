// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Result-surface data + meshes: the analysis voxel hull (+ cell edges +
// element-density tint), the voxel-result surface (exact nodal displacements,
// alternate deformed view), displacement application / exaggeration on the
// STL soup, rigid-body-mode animation amplitudes, the build-sim ghost/active
// preview hulls, and the bed-peel heatmap. Extracted verbatim from
// SceneManager; the facade keeps orchestration (refreshView/clipping/caps).

import * as THREE from "three";
import type { ViewMode } from "../store";
import { jet, ramp } from "./colormaps";

/** Narrow view of the scene the result surfaces read, and how they report
 *  back. Accessors so the manager never holds stale references. */
export interface ResultHost {
  scene: THREE.Scene;
  /** The part mesh's geometry (displayed, possibly deformed vertices). */
  geometry(): THREE.BufferGeometry | null;
  /** Rest-pose soup positions of the part mesh. */
  basePositions(): Float32Array | null;
  viewMode(): ViewMode;
  bboxDiag(): number;
  /** Shared jet LUT (ColorManager's) for the voxel-result surface material. */
  lutJet(): THREE.DataTexture;
  /** Section clipping for a newly built peel map (section state at build time). */
  peelClippingPlanes(): THREE.Plane[] | null;
  /** The displayed vertices moved — extreme markers ride them. */
  onPositionsApplied(): void;
}

export class ResultSurfaceManager {
  // Analysis (voxel) mesh
  readonly voxelGroup = new THREE.Group();
  private voxelDisposables: { dispose(): void }[] = [];
  /** Per-vertex element density of the current voxel hull (0–1: skin = 1,
   *  interior = infill ratio / optimized density, composite cells blended). */
  voxelDensity: Float32Array | null = null;
  meshDensity = false;
  // Force the density ramp on mesh cells (inherent-strain layer view).
  meshFieldColor = false;

  // Build-sim live preview: faint full-hull ghost (deactivated voxels) + a
  // growing deformed active hull (already-printed voxels, exaggeration baked in).
  readonly buildGroup = new THREE.Group();
  buildGhost: THREE.Object3D | null = null;
  buildActive: THREE.Object3D | null = null;
  // Build-sim bed-peel heatmap: a flat jet-colored soup lying on the plate.
  peelMap: THREE.Object3D | null = null;

  // Rigid-body-mode animation
  rbmMode: { t: number[]; r: number[]; center: number[] } | null = null;
  private rbmAmp = 1;

  // Result views
  displacements: Float32Array | null = null;
  vertexDensity: Float32Array | null = null;
  /** Results on the analysis voxel hull (exact nodal displacements) —
   *  alternate surface for the deformed view, toggled by resultSurface. */
  resultSurface: "stl" | "voxel" = "stl";
  voxRes: {
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
  deformScale = 1;
  autoScale = 1;
  deformAnimate = false;
  /** Modal mode-shape animation: symmetric ± swing instead of 0 → max. */
  modalAnim = false;

  constructor(private readonly host: ResultHost) {}

  // ---------- rigid-body-mode animation ----------

  setRbmMode(mode: { t: number[]; r: number[]; center: number[] } | null) {
    this.rbmMode = mode;
    const p = this.host.basePositions();
    if (mode && p) {
      // Normalize amplitude: peak surface motion = 6% of bbox diagonal.
      let maxU = 1e-12;
      for (let i = 0; i < p.length; i += 3) {
        const u = this.modeDisplacement(mode, p[i], p[i + 1], p[i + 2]);
        maxU = Math.max(maxU, Math.hypot(u[0], u[1], u[2]));
      }
      this.rbmAmp = (0.06 * this.host.bboxDiag()) / maxU;
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

  /** Store the displacement field + derive the display autoscale
   *  (8% of the bbox diagonal at peak deflection). */
  setDisplacements(disp: Float32Array | null, stats: { maxDisplacement: number } | null) {
    this.displacements = disp;
    if (disp && stats && stats.maxDisplacement > 0) {
      this.autoScale = (0.08 * this.host.bboxDiag()) / stats.maxDisplacement;
    } else {
      this.autoScale = 1;
    }
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
   *  null clears it. The facade refreshes visibility afterwards. */
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
  }

  /** Growing deformed active hull (already-printed voxels, exaggeration baked
   *  in), jet-colored by normalised |u| (`mags`, 0–1). Replaced each preview
   *  frame; null clears it. */
  setBuildActive(positions: Float32Array | null, mags?: Float32Array | null) {
    if (this.buildActive) {
      this.buildGroup.remove(this.buildActive);
      this.disposeMesh(this.buildActive);
      this.buildActive = null;
    }
    if (positions && positions.length) {
      const geo = new THREE.BufferGeometry();
      geo.setAttribute("position", new THREE.BufferAttribute(positions, 3));
      geo.computeVertexNormals();
      const n = positions.length / 3;
      const colors = new Float32Array(positions.length);
      for (let i = 0; i < n; i++) {
        const [r, g, b] = jet(mags && i < mags.length ? mags[i] : 0);
        colors[3 * i] = r;
        colors[3 * i + 1] = g;
        colors[3 * i + 2] = b;
      }
      geo.setAttribute("color", new THREE.BufferAttribute(colors, 3));
      const mat = new THREE.MeshStandardMaterial({
        vertexColors: true,
        roughness: 0.85,
        metalness: 0.05,
        flatShading: true,
        side: THREE.DoubleSide,
      });
      this.buildActive = new THREE.Mesh(geo, mat);
      this.buildGroup.add(this.buildActive);
    }
  }

  /** Build-sim bed-peel heatmap: a flat triangle soup at the plate, jet-colored
   *  by `values / max`. Sits in world space under the part so the peel reads
   *  from a top/iso view. null clears it. */
  setPeelMap(positions: Float32Array | null, values: Float32Array | null, max: number) {
    if (this.peelMap) {
      this.host.scene.remove(this.peelMap);
      this.disposeMesh(this.peelMap);
      this.peelMap = null;
    }
    if (positions && positions.length) {
      const geo = new THREE.BufferGeometry();
      geo.setAttribute("position", new THREE.BufferAttribute(positions, 3));
      const n = positions.length / 3;
      const colors = new Float32Array(positions.length);
      const inv = max > 0 ? 1 / max : 0;
      for (let i = 0; i < n; i++) {
        const [r, g, b] = jet(values && i < values.length ? values[i] * inv : 0);
        colors[3 * i] = r;
        colors[3 * i + 1] = g;
        colors[3 * i + 2] = b;
      }
      geo.setAttribute("color", new THREE.BufferAttribute(colors, 3));
      const mat = new THREE.MeshBasicMaterial({
        vertexColors: true,
        side: THREE.DoubleSide,
        clippingPlanes: this.host.peelClippingPlanes(),
      });
      this.peelMap = new THREE.Mesh(geo, mat);
      this.host.scene.add(this.peelMap);
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
      this.host.scene.remove(this.voxRes.group);
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
        map: this.host.lutJet(),
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
      this.host.scene.add(group);
      this.voxRes = { group, geo, base: positions, disp, uvs, lineGeo, lineBase, lineDisp };
    }
  }

  /** Switch the deformed view between the smooth STL and the voxel hull.
   *  Returns false when unchanged (the facade skips the refresh). */
  setResultSurface(surface: "stl" | "voxel"): boolean {
    if (this.resultSurface === surface) return false;
    this.resultSurface = surface;
    return true;
  }

  /** Voxel result surface currently driving the deformed view. */
  voxResultActive(): boolean {
    return this.host.viewMode() === "deformed" && this.resultSurface === "voxel" && !!this.voxRes;
  }

  /** Color the mesh-view cells by element density (0–1 ramp). */
  setMeshDensity(on: boolean) {
    this.meshDensity = on;
    this.applyMeshTint();
  }

  /** Force the 0–1 ramp on the mesh-view cells regardless of the density toggle
   *  — used by the inherent-strain layer view (the value channel carries the
   *  normalised source strength). */
  setMeshFieldColor(on: boolean) {
    this.meshFieldColor = on;
    this.applyMeshTint();
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
      if ((this.meshDensity || this.meshFieldColor) && density) {
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

  // ---------- displacement application / exaggeration ----------

  applyPositions(rbmOffset?: number, deformFactor = 1, animating = false) {
    const geometry = this.host.geometry();
    const base = this.host.basePositions();
    if (!geometry || !base) return;
    const attr = geometry.getAttribute("position") as THREE.BufferAttribute;
    const out = attr.array as Float32Array;
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
    } else if (this.displacements && this.host.viewMode() === "deformed") {
      const d = this.displacements;
      const s = this.autoScale * this.deformScale * deformFactor;
      for (let i = 0; i < base.length; i++) out[i] = base[i] + s * d[i];
    } else {
      out.set(base);
    }
    attr.needsUpdate = true;
    // Recomputing vertex normals over the whole surface soup is the dominant
    // per-frame cost of the mode-shape / deflection animation (a 300k-tri part
    // is ~900k verts). During playback we keep the rest-pose normals: on a fast
    // swing the stale shading is imperceptible next to the color field, and the
    // one-shot applyPositions() on stop refreshes them. This roughly triples
    // playback FPS on large meshes.
    if (!animating) geometry.computeVertexNormals();
    this.morphVoxelResult(deformFactor, animating);
    // Markers ride the displayed (deformed/animated) vertices.
    this.host.onPositionsApplied();
  }

  /** Deform the voxel-result hull (and its cell edges) like the part. */
  private morphVoxelResult(deformFactor: number, animating = false) {
    const vr = this.voxRes;
    if (!vr || !vr.group.visible) return;
    const s = this.autoScale * this.deformScale * deformFactor;
    const attr = vr.geo.getAttribute("position") as THREE.BufferAttribute;
    const out = attr.array as Float32Array;
    for (let i = 0; i < vr.base.length; i++) out[i] = vr.base[i] + s * vr.disp[i];
    attr.needsUpdate = true;
    if (!animating) vr.geo.computeVertexNormals();
    if (vr.lineGeo && vr.lineBase && vr.lineDisp) {
      const la = vr.lineGeo.getAttribute("position") as THREE.BufferAttribute;
      const lo = la.array as Float32Array;
      for (let i = 0; i < vr.lineBase.length; i++) {
        lo[i] = vr.lineBase[i] + s * vr.lineDisp[i];
      }
      la.needsUpdate = true;
    }
  }
}
