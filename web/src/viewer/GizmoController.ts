// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Combined plane gizmo shared by the section plane and the symmetry plane.
// Both use the SAME rig — a proxy Object3D carrying a translucent quad, a
// TransformControls that translates along the local normal only, and one that
// rotates about the two in-plane axes (spinning about the normal is a no-op
// and stays hidden). They differ only in handle sizes, quad color, and how the
// quad is sized: the section quad is a fixed part-diagonal square; the
// symmetry quad is a unit square refit live to the part's projected extents.
// Unified from the two near-identical systems that lived in SceneManager.

import * as THREE from "three";
import { TransformControls } from "three/addons/controls/TransformControls.js";
import type { LoadedModel } from "../types";

export type PlaneGizmoKind = "section" | "symmetry";

/** Per-kind cosmetics: handle sizes and the quad's color/opacities.
 *  Section: blue, slightly larger handles. Symmetry: orange (distinct). */
const KIND_CFG: Record<
  PlaneGizmoKind,
  { translateSize: number; rotateSize: number; color: number; quadOpacity: number; edgeOpacity: number }
> = {
  section: { translateSize: 0.75, rotateSize: 1.05, color: 0x2e6fd0, quadOpacity: 0.08, edgeOpacity: 0.7 },
  symmetry: { translateSize: 0.7, rotateSize: 1.0, color: 0xd97706, quadOpacity: 0.1, edgeOpacity: 0.8 },
};

/** What the gizmo needs from the scene, and how it reports back. */
export interface GizmoHost {
  scene: THREE.Scene;
  camera(): THREE.Camera;
  domElement(): HTMLElement;
  /** A handle drag started/ended: gate OrbitControls while dragging. */
  onDraggingChanged(dragging: boolean): void;
  /** The proxy moved and the plane/quad re-synced (drag or programmatic). */
  onChanged(): void;
  bboxDiag(): number;
  /** Part AABB in world mm — sizes the symmetry quad. Null before a model. */
  partBbox(): LoadedModel["bbox"] | null;
}

export class GizmoController {
  /** Pose carrier for the plane: local +Z is the plane normal. */
  readonly proxy = new THREE.Object3D();
  private translate: TransformControls | null = null;
  private rotate: TransformControls | null = null;
  private quad: THREE.Group | null = null;
  private quadDisposables: { dispose(): void }[] = [];

  constructor(
    private readonly kind: PlaneGizmoKind,
    private readonly host: GizmoHost,
    /** Section only: the clipping plane kept in sync with the proxy pose
     *  (three.js convention: kept side is normal·p + constant ≥ 0). */
    private readonly plane?: THREE.Plane
  ) {}

  /** The TransformControls have been created (first ensure() ran). */
  exists(): boolean {
    return this.translate !== null;
  }

  /** Lazily create the proxy + controls + quad. Re-ensuring a symmetry gizmo
   *  rebuilds its quad (refresh size to the current part); a section gizmo
   *  keeps its quad (setModel resizes it explicitly). */
  ensure() {
    if (this.translate) {
      if (this.kind === "symmetry") this.buildQuad();
      return;
    }
    this.host.scene.add(this.proxy);
    const cfg = KIND_CFG[this.kind];
    const make = (
      mode: "translate" | "rotate",
      size: number,
      tune: (tc: TransformControls) => void
    ) => {
      const tc = new TransformControls(this.host.camera(), this.host.domElement());
      tc.setMode(mode);
      tc.setSpace("local");
      tc.setSize(size);
      tune(tc);
      tc.addEventListener("dragging-changed", (e: { value?: unknown }) => {
        this.host.onDraggingChanged(!!e.value);
      });
      tc.addEventListener("objectChange", () => this.sync());
      tc.attach(this.proxy);
      this.host.scene.add(tc.getHelper());
      return tc;
    };
    // One combined gizmo: the plane cuts/mirrors everything, so tangential
    // motion is meaningless — only the normal arrow translates; two rings
    // rotate (spinning about the normal is a no-op and stays hidden).
    this.translate = make("translate", cfg.translateSize, (tc) => {
      tc.showX = false;
      tc.showY = false;
    });
    this.rotate = make("rotate", cfg.rotateSize, (tc) => {
      tc.showZ = false;
    });
    this.buildQuad();
  }

  /** Show/hide + enable/disable the whole rig (proxy carries the quad). */
  setVisible(show: boolean) {
    this.proxy.visible = show;
    for (const tc of [this.translate, this.rotate]) {
      if (tc) {
        tc.enabled = show;
        tc.getHelper().visible = show;
      }
    }
  }

  /** (Re)build the translucent plane rectangle, child of the proxy so it is
   *  ALWAYS centered on the gizmo (PlaneHelper centers on the world origin's
   *  foot point instead, which strands the gizmo off to one side). */
  buildQuad() {
    if (this.quad) {
      this.proxy.remove(this.quad);
      for (const d of this.quadDisposables) d.dispose();
      this.quadDisposables = [];
    }
    const cfg = KIND_CFG[this.kind];
    // Section: fixed part-sized square. Symmetry: unit quad, scaled to the
    // part by updateQuadSize.
    const s = this.kind === "section" ? this.host.bboxDiag() * 1.15 : 1;
    const group = new THREE.Group();
    const quadGeo = new THREE.PlaneGeometry(s, s);
    const quadMat = new THREE.MeshBasicMaterial({
      color: cfg.color,
      transparent: true,
      opacity: cfg.quadOpacity,
      side: THREE.DoubleSide,
      depthWrite: false,
    });
    const edgeGeo = new THREE.EdgesGeometry(quadGeo);
    const edgeMat = new THREE.LineBasicMaterial({
      color: cfg.color,
      transparent: true,
      opacity: cfg.edgeOpacity,
    });
    this.quadDisposables.push(quadGeo, quadMat, edgeGeo, edgeMat);
    group.add(new THREE.Mesh(quadGeo, quadMat));
    group.add(new THREE.LineSegments(edgeGeo, edgeMat));
    this.quad = group;
    this.proxy.add(group);
    if (this.kind === "symmetry") this.updateQuadSize();
  }

  /** Symmetry only: scale the unit quad so it spans the part: its two in-plane
   *  axes get the extent of the part's AABB projected onto them (so an
   *  axis-aligned plane is exactly the perpendicular part dimensions, and a
   *  tilted one stays the silhouette size). Falls back to the bbox diagonal
   *  before a model loads. */
  updateQuadSize() {
    if (this.kind !== "symmetry" || !this.quad) return;
    const b = this.host.partBbox();
    if (!b) {
      const d = this.host.bboxDiag() * 1.1;
      this.quad.scale.set(d, d, 1);
      return;
    }
    const dim = new THREE.Vector3(b[3] - b[0], b[4] - b[1], b[5] - b[2]);
    const q = this.proxy.quaternion;
    const ex = new THREE.Vector3(1, 0, 0).applyQuaternion(q);
    const ey = new THREE.Vector3(0, 1, 0).applyQuaternion(q);
    // Projected AABB extent along a world axis e: Σ |e·axis|·dim_axis.
    const span = (e: THREE.Vector3) =>
      Math.abs(e.x) * dim.x + Math.abs(e.y) * dim.y + Math.abs(e.z) * dim.z;
    this.quad.scale.set(span(ex) || 1, span(ey) || 1, 1);
  }

  /** Re-derive the plane (section) / quad fit (symmetry) from the proxy pose,
   *  then notify the host. Fired on every handle drag (objectChange) and by
   *  the programmatic moves (flip, axis snap, new model). */
  sync() {
    if (this.kind === "section") {
      const n = new THREE.Vector3(0, 0, 1).applyQuaternion(this.proxy.quaternion);
      this.plane!.setFromNormalAndCoplanarPoint(n, this.proxy.position);
    } else {
      // Tilting the plane (rotate ring) changes which part dimensions it
      // spans — refit the quad live so it always reads as "the size of the
      // part".
      this.updateQuadSize();
    }
    this.host.onChanged();
  }
}
