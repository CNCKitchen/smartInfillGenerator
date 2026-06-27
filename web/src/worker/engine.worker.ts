// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

/// <reference lib="webworker" />
// The wasm Model lives here; the main thread talks via EngineClient.

import type { Model } from "../wasm/sig_wasm.js";

let model: Model | null = null;
let ModelCtor: typeof Model;
/** wasm hook installing the cancellation flag (thread-local checker). */
let setCancelFlagFn: ((flag: Int32Array) => void) | null = null;
/** Shared flag with the main thread: [0] != 0 = stop the running solve. */
let cancelArr: Int32Array | null = null;
/** wasm hook installing the live residual-progress buffer (thread-local). */
let setProgressBufferFn: ((count: Int32Array, data: Float32Array) => void) | null = null;
/** Project (.infeall) unzip helpers from the wasm module. */
let projectManifestFn: ((bytes: Uint8Array) => string) | null = null;
let projectModelFn: ((bytes: Uint8Array) => Uint8Array) | null = null;
/** Original imported model bytes (for project save) + name. */
let lastModel: { bytes: Uint8Array; name: string } | null = null;
/** Project bytes staged between openProjectModel and openProjectRestore. */
let pendingProject: Uint8Array | null = null;

// Pick the threaded module when the page is cross-origin isolated
// (SharedArrayBuffer available); otherwise the single-threaded fallback.
// Both expose the identical Model API.
const ready = (async () => {
  if (self.crossOriginIsolated) {
    // Static asset (web/public/wasm-mt), deliberately NOT bundled — the
    // rayon pool workers re-import the glue by plain relative URL.
    const mt = (await import(
      /* @vite-ignore */ new URL(import.meta.env.BASE_URL + "wasm-mt/sig_wasm.js", self.location.origin).href
    )) as typeof import("../wasm/sig_wasm.js") & {
      initThreadPool(threads: number): Promise<unknown>;
    };
    await mt.default();
    const threads = Math.max(1, navigator.hardwareConcurrency || 4);
    await mt.initThreadPool(threads);
    ModelCtor = mt.Model;
    setCancelFlagFn = mt.set_cancel_flag;
    setProgressBufferFn = mt.set_progress_buffer;
    projectManifestFn = mt.project_manifest;
    projectModelFn = mt.project_model;
    console.info(`engine: threaded wasm (${threads} threads)`);
  } else {
    const st = await import("../wasm/sig_wasm.js");
    await st.default();
    ModelCtor = st.Model;
    setCancelFlagFn = st.set_cancel_flag;
    setProgressBufferFn = st.set_progress_buffer;
    projectManifestFn = st.project_manifest;
    projectModelFn = st.project_model;
    console.info("engine: single-threaded wasm (page not cross-origin isolated)");
  }
})();

type Req =
  | { id: number; op: "load"; bytes: ArrayBuffer; name: string }
  | {
      id: number;
      op: "transform";
      /** Rigid transform: [r00..r22 row-major, tx, ty, tz] in mm. */
      matrix: number[];
    }
  | { id: number; op: "resegment"; angle: number }
  | { id: number; op: "useCadFaces" }
  | {
      id: number;
      op: "setMaterial";
      e0: number;
      nu: number;
      density: number;
      strength: number;
      strengthZ: number;
    }
  | { id: number; op: "setGravity"; on: boolean }
  | { id: number; op: "setResolution"; cells: number }
  | { id: number; op: "setVoxelSize"; h: number }
  | {
      id: number;
      op: "setBcs";
      bcs: {
        kind: string;
        tris: Uint32Array;
        force?: number[];
        pressure?: number;
        stiffness?: number;
        axes?: boolean[];
        disp?: number[];
      }[];
    }
  | { id: number; op: "voxelInfo" }
  | { id: number; op: "voxelMesh" }
  | {
      id: number;
      op: "voxelMeshCut";
      plane: { normal: [number, number, number]; constant: number } | null;
      wall: number;
      /** Top/bottom shell thickness in mm (layers × layer height). */
      topBottomMm: number;
      /** Uniform infill % for interior-cell density (optimized densities
       *  win when an optimization result exists). */
      infillPct: number;
    }
  | { id: number; op: "check" }
  | { id: number; op: "solve" }
  | { id: number; op: "solveOptimized" }
  | { id: number; op: "setSnapWall"; wall: number }
  | { id: number; op: "setCompositeSkin"; on: boolean }
  | { id: number; op: "setSmoothStress"; on: boolean }
  | { id: number; op: "setMaterialStress"; on: boolean }
  | { id: number; op: "setCancelBuffer"; buf: SharedArrayBuffer }
  | { id: number; op: "setProgressBuffer"; buf: SharedArrayBuffer }
  | {
      id: number;
      op: "solvePrinted";
      /** PrintedOpts object — serialized to JSON for the wasm API. */
      opts: Record<string, unknown>;
    }
  | {
      id: number;
      op: "buildSim";
      /** BuildSimOpts object — serialized to JSON for the wasm API. */
      opts: Record<string, unknown>;
    }
  | {
      id: number;
      op: "optimize";
      /** OptimizeOptions object — serialized to JSON for the wasm API. */
      opts: Record<string, unknown>;
    }
  | { id: number; op: "densityShape"; threshold: number }
  | { id: number; op: "resmooth"; iters: number }
  | { id: number; op: "setIsoThreshold"; threshold: number; smoothIters: number }
  | { id: number; op: "resultField"; kind: string }
  | { id: number; op: "voxelResults" }
  | { id: number; op: "voxelResultField"; kind: string }
  | { id: number; op: "stashResult"; resultId: string }
  | { id: number; op: "activateResult"; resultId: string }
  | { id: number; op: "clearResults" }
  | { id: number; op: "clearLoadCases" }
  | { id: number; op: "addLoadCase"; weight: number }
  | { id: number; op: "transformMatrix" }
  | { id: number; op: "exportProject"; manifest: string; modelEntry: string; includeResults: boolean }
  | { id: number; op: "openProjectModel"; bytes: ArrayBuffer }
  | { id: number; op: "openProjectRestore" }
  | { id: number; op: "vertexDensity" }
  | { id: number; op: "exportThreeMf"; slicer: string; thumbnail: Uint8Array | null }
  | {
      id: number;
      op: "exportColorThreeMf";
      kind: string;
      lo: number;
      hi: number;
      steps: number;
      colors: string[];
      thumbnail: Uint8Array | null;
    }
  | { id: number; op: "exportStls" }
  | { id: number; op: "exportSolidStl" };

/** Collect region meshes + transfer list (shared by optimize + resmooth). */
function collectRegions(m: Model): {
  regions: { density: number; positions: Float32Array; indices: Uint32Array }[];
  transfer: Transferable[];
} {
  const regions: { density: number; positions: Float32Array; indices: Uint32Array }[] = [];
  const transfer: Transferable[] = [];
  for (let i = 0; i < m.region_count(); i++) {
    const positions = m.region_positions(i);
    const indices = m.region_indices(i);
    regions.push({ density: m.region_density(i), positions, indices });
    transfer.push(positions.buffer, indices.buffer);
  }
  return { regions, transfer };
}

self.onmessage = async (ev: MessageEvent<Req>) => {
  const msg = ev.data;
  try {
    await ready;
    switch (msg.op) {
      case "load": {
        model?.free();
        const loadBytes = new Uint8Array(msg.bytes);
        model = new ModelCtor(loadBytes, msg.name);
        lastModel = { bytes: loadBytes, name: msg.name };
        const positions = model.positions();
        const patchIds = model.patch_ids();
        const data = {
          positions,
          patchIds,
          patchCount: model.patch_count(),
          triCount: model.triangle_count(),
          bbox: Array.from(model.bbox()),
          meshObjects: model.mesh_object_count(),
          hasCadFaces: model.has_cad_faces(),
        };
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data }, [
          positions.buffer,
          patchIds.buffer,
        ]);
        return;
      }
      case "transform": {
        const m = requireModel();
        m.transform(new Float64Array(msg.matrix));
        const positions = m.positions();
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { positions, bbox: Array.from(m.bbox()) } },
          [positions.buffer]
        );
        return;
      }
      case "resegment": {
        requireModel().resegment(msg.angle);
        const patchIds = requireModel().patch_ids();
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { patchIds, patchCount: requireModel().patch_count() } },
          [patchIds.buffer]
        );
        return;
      }
      case "useCadFaces": {
        requireModel().use_cad_faces();
        const patchIds = requireModel().patch_ids();
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { patchIds, patchCount: requireModel().patch_count() } },
          [patchIds.buffer]
        );
        return;
      }
      case "setMaterial":
        requireModel().set_material(msg.e0, msg.nu, msg.density, msg.strength, msg.strengthZ);
        break;
      case "setGravity":
        requireModel().set_gravity(msg.on);
        break;
      case "setResolution":
        requireModel().set_resolution(msg.cells);
        break;
      case "setVoxelSize":
        requireModel().set_voxel_size(msg.h);
        break;
      case "setSnapWall":
        requireModel().set_snap_wall(msg.wall);
        break;
      case "setCompositeSkin":
        requireModel().set_composite_skin(msg.on);
        break;
      case "setSmoothStress":
        requireModel().set_smooth_stress(msg.on);
        break;
      case "setMaterialStress":
        requireModel().set_material_stress(msg.on);
        break;
      case "setCancelBuffer":
        cancelArr = new Int32Array(msg.buf);
        setCancelFlagFn?.(cancelArr);
        break;
      case "setProgressBuffer": {
        // Layout: count (one i32) then the residual trace (f32). The solve
        // loop fills it via the wasm sink; the main thread polls it to draw
        // the live convergence plot.
        const count = new Int32Array(msg.buf, 0, 1);
        const data = new Float32Array(msg.buf, 4, (msg.buf.byteLength - 4) >> 2);
        setProgressBufferFn?.(count, data);
        break;
      }
      case "setBcs": {
        const m = requireModel();
        m.clear_bcs();
        for (const bc of msg.bcs) {
          if (bc.kind === "fixed") m.add_fixed(bc.tris);
          else if (bc.kind === "frictionless") m.add_frictionless(bc.tris);
          else if (bc.kind === "displacement") {
            const a = bc.axes ?? [false, false, true];
            const v = bc.disp ?? [0, 0, 0];
            m.add_displacement(bc.tris, !!a[0], !!a[1], !!a[2], v[0] ?? 0, v[1] ?? 0, v[2] ?? 0);
          } else if (bc.kind === "elastic") m.add_elastic(bc.tris, bc.stiffness ?? 100);
          else if (bc.kind === "force") {
            const f = bc.force ?? [0, 0, 0];
            m.add_force(bc.tris, f[0], f[1], f[2]);
          } else if (bc.kind === "pressure") m.add_pressure(bc.tris, bc.pressure ?? 0);
        }
        break;
      }
      case "voxelInfo": {
        const info = JSON.parse(requireModel().voxel_info());
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: info });
        return;
      }
      case "voxelMesh": {
        const m = requireModel();
        const hull = m.voxel_hull();
        const edges = m.voxel_edges();
        const info = JSON.parse(m.voxel_info());
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { hull, edges, info } },
          [hull.buffer, edges.buffer]
        );
        return;
      }
      case "voxelMeshCut": {
        const m = requireModel();
        const p = msg.plane;
        const arr = m.voxel_mesh_cut(
          p !== null,
          p?.normal[0] ?? 0,
          p?.normal[1] ?? 0,
          p?.normal[2] ?? 0,
          p?.constant ?? 0,
          msg.wall,
          msg.topBottomMm,
          msg.infillPct
        );
        const hull = arr[0] as Float32Array;
        const density = arr[1] as Float32Array;
        const edges = arr[2] as Float32Array;
        const info = JSON.parse(m.voxel_info());
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { hull, density, edges, info } },
          [hull.buffer, density.buffer, edges.buffer]
        );
        return;
      }
      case "check": {
        const report = JSON.parse(requireModel().check());
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: report });
        return;
      }
      case "solve": {
        if (cancelArr) Atomics.store(cancelArr, 0, 0); // arm fresh
        const t0 = performance.now();
        const stats = JSON.parse(requireModel().solve());
        const displacements = requireModel().vertex_displacements();
        stats.seconds = (performance.now() - t0) / 1000;
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { stats, displacements } },
          [displacements.buffer]
        );
        return;
      }
      case "solveOptimized": {
        if (cancelArr) Atomics.store(cancelArr, 0, 0); // arm fresh
        const t0 = performance.now();
        const stats = JSON.parse(requireModel().solve_optimized());
        const displacements = requireModel().vertex_displacements();
        stats.seconds = (performance.now() - t0) / 1000;
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { stats, displacements } },
          [displacements.buffer]
        );
        return;
      }
      case "solvePrinted": {
        if (cancelArr) Atomics.store(cancelArr, 0, 0); // arm fresh
        const t0 = performance.now();
        const stats = JSON.parse(requireModel().solve_printed(JSON.stringify(msg.opts)));
        const displacements = requireModel().vertex_displacements();
        stats.seconds = (performance.now() - t0) / 1000;
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { stats, displacements } },
          [displacements.buffer]
        );
        return;
      }
      case "buildSim": {
        if (cancelArr) Atomics.store(cancelArr, 0, 0); // arm fresh
        const t0 = performance.now();
        const stats = JSON.parse(
          requireModel().solve_build_sim(
            JSON.stringify(msg.opts),
            // Per-layer: progress (done/total) + throttled warp preview (empty
            // displacements on non-preview frames). Reuses the `density` slot.
            (done: number, total: number, disp: Float32Array) => {
              (self as unknown as Worker).postMessage(
                { id: msg.id, progress: true, data: { done, total }, density: disp },
                disp.length > 0 ? [disp.buffer] : []
              );
            }
          )
        );
        const displacements = requireModel().vertex_displacements();
        stats.seconds = (performance.now() - t0) / 1000;
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { stats, displacements } },
          [displacements.buffer]
        );
        return;
      }
      case "optimize": {
        if (cancelArr) Atomics.store(cancelArr, 0, 0); // arm fresh
        const m = requireModel();
        const t0 = performance.now();
        const summary = JSON.parse(
          m.optimize(
            JSON.stringify(msg.opts),
            (
              json: string,
              density: Float32Array,
              skelPositions: Float32Array,
              skelIndices: Uint32Array,
              skelDensity: Float32Array
            ) => {
              (self as unknown as Worker).postMessage(
                {
                  id: msg.id,
                  progress: true,
                  data: JSON.parse(json),
                  density,
                  skelPositions,
                  skelIndices,
                  skelDensity,
                },
                [density.buffer, skelPositions.buffer, skelIndices.buffer, skelDensity.buffer]
              );
            }
          )
        );
        summary.seconds = (performance.now() - t0) / 1000;
        // Collect region meshes + final fields in one payload.
        const { regions, transfer } = collectRegions(m);
        const vertexDensity = m.vertex_density();
        const displacements = m.vertex_displacements();
        transfer.push(vertexDensity.buffer, displacements.buffer);
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { summary, regions, vertexDensity, displacements } },
          transfer
        );
        return;
      }
      case "densityShape": {
        const arr = requireModel().density_isosurface(msg.threshold);
        const positions = arr[0] as Float32Array;
        const indices = arr[1] as Uint32Array;
        const density = arr[2] as Float32Array;
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { positions, indices, density } },
          [positions.buffer, indices.buffer, density.buffer]
        );
        return;
      }
      case "resmooth": {
        const m = requireModel();
        m.resmooth_regions(msg.iters);
        const { regions, transfer } = collectRegions(m);
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { regions } },
          transfer
        );
        return;
      }
      case "setIsoThreshold": {
        const m = requireModel();
        m.set_iso_threshold(msg.threshold, msg.smoothIters);
        const { regions, transfer } = collectRegions(m);
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { regions } },
          transfer
        );
        return;
      }
      case "resultField": {
        const values = requireModel().result_field(msg.kind);
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: values }, [
          values.buffer,
        ]);
        return;
      }
      case "voxelResults": {
        const arr = requireModel().voxel_results();
        const positions = arr[0] as Float32Array;
        const displacements = arr[1] as Float32Array;
        const edges = arr[2] as Float32Array;
        const edgeDisplacements = arr[3] as Float32Array;
        (self as unknown as Worker).postMessage(
          { id: msg.id, ok: true, data: { positions, displacements, edges, edgeDisplacements } },
          [positions.buffer, displacements.buffer, edges.buffer, edgeDisplacements.buffer]
        );
        return;
      }
      case "voxelResultField": {
        const values = requireModel().voxel_result_field(msg.kind);
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: values }, [
          values.buffer,
        ]);
        return;
      }
      case "stashResult":
        requireModel().stash_result(msg.resultId);
        break;
      case "activateResult": {
        const displacements = requireModel().activate_result(msg.resultId);
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: displacements }, [
          displacements.buffer,
        ]);
        return;
      }
      case "clearResults":
        requireModel().clear_results();
        break;
      case "clearLoadCases":
        requireModel().clear_load_cases();
        break;
      case "addLoadCase":
        requireModel().add_load_case(msg.weight);
        break;
      case "transformMatrix": {
        const mtx = Array.from(requireModel().transform_matrix());
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: mtx });
        return;
      }
      case "exportProject": {
        if (!lastModel) throw new Error("no model loaded to save");
        const bytes = requireModel().export_project(
          lastModel.bytes,
          msg.modelEntry,
          msg.manifest,
          msg.includeResults
        );
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: bytes }, [bytes.buffer]);
        return;
      }
      case "openProjectModel": {
        if (!projectManifestFn || !projectModelFn) throw new Error("engine not ready");
        const projBytes = new Uint8Array(msg.bytes);
        const manifest = projectManifestFn(projBytes);
        const modelBytes = projectModelFn(projBytes);
        let name = "project";
        try {
          const mf = JSON.parse(manifest);
          if (mf.fileName) name = String(mf.fileName).replace(/\.(stl|3mf|infeall)$/i, "");
        } catch {
          // manifest name is cosmetic — fall back to "project"
        }
        model?.free();
        model = new ModelCtor(modelBytes, name);
        lastModel = { bytes: modelBytes, name };
        pendingProject = projBytes;
        const positions = model.positions();
        const patchIds = model.patch_ids();
        const data = {
          manifest,
          model: {
            positions,
            patchIds,
            patchCount: model.patch_count(),
            triCount: model.triangle_count(),
            bbox: Array.from(model.bbox()),
            meshObjects: model.mesh_object_count(),
            hasCadFaces: model.has_cad_faces(),
          },
        };
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data }, [
          positions.buffer,
          patchIds.buffer,
        ]);
        return;
      }
      case "openProjectRestore": {
        if (!pendingProject) throw new Error("no project staged to restore");
        const summary = JSON.parse(requireModel().restore_project(pendingProject));
        pendingProject = null;
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: summary });
        return;
      }
      case "vertexDensity": {
        const vd = requireModel().vertex_density();
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: vd }, [vd.buffer]);
        return;
      }
      case "exportThreeMf": {
        const thumb = msg.thumbnail ?? new Uint8Array(0);
        const bytes = requireModel().export_3mf(msg.slicer, thumb);
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: bytes }, [
          bytes.buffer,
        ]);
        return;
      }
      case "exportColorThreeMf": {
        const thumb = msg.thumbnail ?? new Uint8Array(0);
        const bytes = requireModel().export_color_3mf(
          msg.kind,
          msg.lo,
          msg.hi,
          msg.steps,
          JSON.stringify(msg.colors),
          thumb
        );
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: bytes }, [
          bytes.buffer,
        ]);
        return;
      }
      case "exportStls": {
        const bytes = requireModel().export_stls();
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: bytes }, [
          bytes.buffer,
        ]);
        return;
      }
      case "exportSolidStl": {
        const bytes = requireModel().export_solid_stl();
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: bytes }, [
          bytes.buffer,
        ]);
        return;
      }
    }
    (self as unknown as Worker).postMessage({ id: msg.id, ok: true });
  } catch (e) {
    (self as unknown as Worker).postMessage({
      id: msg.id,
      ok: false,
      error: e instanceof Error ? e.message : String(e),
    });
  }
};

function requireModel(): Model {
  if (!model) throw new Error("no model loaded");
  return model;
}
