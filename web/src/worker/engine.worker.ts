// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

/// <reference lib="webworker" />
// The wasm Model lives here; the main thread talks via EngineClient.

import type { Model } from "../wasm/filasim_wasm.js";
import type {
  BuildSimProgressMessage,
  EngineResponses,
  EngineWorkerRequest,
  LoadedModelData,
  ModalProgressMessage,
  Op,
  OptimizeProgressMessage,
  WorkerErrorMessage,
  WorkerRequest,
} from "../engine/EngineProtocol";

let model: Model | null = null;
let ModelCtor: typeof Model;
/** wasm hook installing the cancellation flag (thread-local checker). */
let setCancelFlagFn: ((flag: Int32Array) => void) | null = null;
/** Shared flag with the main thread: [0] != 0 = stop the running solve. */
let cancelArr: Int32Array | null = null;
/** wasm hook installing the live residual-progress buffer (thread-local). */
let setProgressBufferFn: ((count: Int32Array, data: Float32Array) => void) | null = null;
/** Project (.filasim) unzip helpers from the wasm module. */
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
      /* @vite-ignore */ new URL(import.meta.env.BASE_URL + "wasm-mt/filasim_wasm.js", self.location.origin).href
    )) as typeof import("../wasm/filasim_wasm.js") & {
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
    const st = await import("../wasm/filasim_wasm.js");
    await st.default();
    ModelCtor = st.Model;
    setCancelFlagFn = st.set_cancel_flag;
    setProgressBufferFn = st.set_progress_buffer;
    projectManifestFn = st.project_manifest;
    projectModelFn = st.project_model;
    console.info("engine: single-threaded wasm (page not cross-origin isolated)");
  }
})();

/** Typed success reply: `data` is checked against the op's entry in
 *  `EngineResponses`, so a switch case can't post the wrong shape. */
function reply<O extends Op>(
  msg: WorkerRequest<O>,
  data: EngineResponses[O],
  transfer: Transferable[] = []
): void {
  (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data }, transfer);
}

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

self.onmessage = async (ev: MessageEvent<EngineWorkerRequest>) => {
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
          bbox: Array.from(model.bbox()) as LoadedModelData["bbox"],
          meshObjects: model.mesh_object_count(),
          bodyCount: model.body_count(),
          hasCadFaces: model.has_cad_faces(),
        };
        reply(msg, data, [positions.buffer, patchIds.buffer]);
        return;
      }
      case "loadMesh": {
        model?.free();
        model = ModelCtor.from_mesh(
          msg.positions,
          msg.indices,
          msg.faceOfTri,
          msg.solidOfTri,
          msg.name
        );
        // Original STEP bytes, verbatim — project save embeds these.
        lastModel = { bytes: new Uint8Array(msg.bytes), name: msg.name };
        const positions = model.positions();
        const patchIds = model.patch_ids();
        const data = {
          positions,
          patchIds,
          patchCount: model.patch_count(),
          triCount: model.triangle_count(),
          bbox: Array.from(model.bbox()) as LoadedModelData["bbox"],
          meshObjects: model.mesh_object_count(),
          bodyCount: model.body_count(),
          hasCadFaces: model.has_cad_faces(),
        };
        reply(msg, data, [positions.buffer, patchIds.buffer]);
        return;
      }
      case "transform": {
        const m = requireModel();
        m.transform(new Float64Array(msg.matrix));
        const positions = m.positions();
        reply(msg, { positions, bbox: Array.from(m.bbox()) }, [positions.buffer]);
        return;
      }
      case "resegment": {
        requireModel().resegment(msg.angle);
        const patchIds = requireModel().patch_ids();
        reply(msg, { patchIds, patchCount: requireModel().patch_count() }, [patchIds.buffer]);
        return;
      }
      case "originalPositions": {
        const op = requireModel().original_positions();
        reply(msg, op, [op.buffer]);
        return;
      }
      case "refinementParents": {
        const rp = requireModel().refinement_parents();
        reply(msg, rp, [rp.buffer]);
        return;
      }
      case "useCadFaces": {
        requireModel().use_cad_faces();
        const patchIds = requireModel().patch_ids();
        reply(msg, { patchIds, patchCount: requireModel().patch_count() }, [patchIds.buffer]);
        return;
      }
      case "setMaterial":
        requireModel().set_material(msg.e0, msg.nu, msg.density, msg.strength, msg.strengthZ, msg.shearStrengthZ);
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
        // Every active acceleration entity sums into ONE world vector for the
        // engine's body-load parameter (DESIGN §16 dec. 5); masses become
        // remote-mass BCs (grams → tonne). Both are always reset — an accel or
        // mass that vanished from this step must not linger from the last.
        const accel: [number, number, number] = [0, 0, 0];
        for (const bc of msg.bcs) {
          if (bc.kind === "fixed") m.add_fixed(bc.tris);
          else if (bc.kind === "frictionless") m.add_frictionless(bc.tris);
          else if (bc.kind === "displacement") {
            const a = bc.axes ?? [false, false, true];
            const v = bc.disp ?? [0, 0, 0];
            m.add_displacement(bc.tris, !!a[0], !!a[1], !!a[2], v[0] ?? 0, v[1] ?? 0, v[2] ?? 0);
          } else if (bc.kind === "cylindrical") {
            // Local cylinder DOFs [radial, tangential, axial]; the engine
            // re-fits the cylinder itself to build the frame.
            const d = bc.cylDof ?? [true, false, true];
            m.add_cylindrical(bc.tris, !!d[0], !!d[1], !!d[2]);
          } else if (bc.kind === "elastic") m.add_elastic(bc.tris, bc.stiffness ?? 100);
          else if (bc.kind === "force") {
            const f = bc.force ?? [0, 0, 0];
            m.add_force(bc.tris, f[0], f[1], f[2]);
          } else if (bc.kind === "pressure") m.add_pressure(bc.tris, bc.pressure ?? 0);
          else if (bc.kind === "bearing") {
            const f = bc.force ?? [0, 0, 0];
            m.add_bearing(bc.tris, f[0], f[1], f[2]);
          } else if (bc.kind === "moment") {
            const mm = bc.moment ?? [0, 0, 0];
            m.add_moment(bc.tris, mm[0], mm[1], mm[2]);
          } else if (bc.kind === "mass") {
            const p = bc.point ?? [0, 0, 0];
            // grams → tonne (consistent mm–N–MPa system: N = tonne·mm/s²).
            // DESIGN §16 milestone 4: a rigid mount also stiffens the patch.
            m.add_mass(
              bc.tris,
              p[0] ?? 0,
              p[1] ?? 0,
              p[2] ?? 0,
              (bc.massGrams ?? 0) * 1e-6,
              bc.behavior === "rigid"
            );
          } else if (bc.kind === "accel") {
            const a = bc.accel ?? [0, 0, 0];
            accel[0] += a[0] ?? 0;
            accel[1] += a[1] ?? 0;
            accel[2] += a[2] ?? 0;
          }
        }
        m.set_accel(accel[0], accel[1], accel[2]);
        break;
      }
      case "fitCylinder": {
        const json = requireModel().fit_cylinder(msg.tris);
        reply(msg, JSON.parse(json));
        return;
      }
      case "voxelInfo": {
        const info = JSON.parse(requireModel().voxel_info());
        reply(msg, info);
        return;
      }
      case "voxelMesh": {
        const m = requireModel();
        const hull = m.voxel_hull();
        const edges = m.voxel_edges();
        const info = JSON.parse(m.voxel_info());
        reply(msg, { hull, edges, info }, [hull.buffer, edges.buffer]);
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
        reply(msg, { hull, density, edges, info }, [hull.buffer, density.buffer, edges.buffer]);
        return;
      }
      case "check": {
        const report = JSON.parse(requireModel().check());
        reply(msg, report);
        return;
      }
      case "solve": {
        if (cancelArr) Atomics.store(cancelArr, 0, 0); // arm fresh
        const t0 = performance.now();
        const stats = JSON.parse(requireModel().solve());
        const displacements = requireModel().vertex_displacements();
        stats.seconds = (performance.now() - t0) / 1000;
        reply(msg, { stats, displacements }, [displacements.buffer]);
        return;
      }
      case "solveOptimized": {
        if (cancelArr) Atomics.store(cancelArr, 0, 0); // arm fresh
        const t0 = performance.now();
        const stats = JSON.parse(requireModel().solve_optimized());
        const displacements = requireModel().vertex_displacements();
        stats.seconds = (performance.now() - t0) / 1000;
        reply(msg, { stats, displacements }, [displacements.buffer]);
        return;
      }
      case "solvePrinted": {
        if (cancelArr) Atomics.store(cancelArr, 0, 0); // arm fresh
        const t0 = performance.now();
        const stats = JSON.parse(requireModel().solve_printed(JSON.stringify(msg.opts)));
        const displacements = requireModel().vertex_displacements();
        stats.seconds = (performance.now() - t0) / 1000;
        reply(msg, { stats, displacements }, [displacements.buffer]);
        return;
      }
      case "buildSim": {
        if (cancelArr) Atomics.store(cancelArr, 0, 0); // arm fresh
        const t0 = performance.now();
        const stats = JSON.parse(
          requireModel().solve_build_sim(
            JSON.stringify(msg.opts),
            // Per-layer: progress + (on throttled frames) the deformed activated
            // voxel hull (positions in `density`, normalised |u| in
            // `skelPositions`, max |u| in `data.maxU`).
            (done: number, total: number, pos: Float32Array, mags: Float32Array, maxU: number) => {
              (self as unknown as Worker).postMessage(
                {
                  id: msg.id,
                  progress: true,
                  data: { done, total, maxU },
                  density: pos,
                  skelPositions: mags,
                } satisfies BuildSimProgressMessage,
                pos.length > 0 ? [pos.buffer, mags.buffer] : []
              );
            }
          )
        );
        const displacements = requireModel().vertex_displacements();
        stats.seconds = (performance.now() - t0) / 1000;
        reply(msg, { stats, displacements }, [displacements.buffer]);
        return;
      }
      case "modalAnalysis": {
        if (cancelArr) Atomics.store(cancelArr, 0, 0); // arm fresh
        const t0 = performance.now();
        // Computes the modes, stashes each as `modal::mode-i`, and leaves
        // mode 0 live — so `vertex_displacements` returns its mesh deformation.
        // The callback streams per-outer-iteration progress + current frequencies.
        const result = JSON.parse(
          requireModel().modal_analysis(
            JSON.stringify(msg.opts),
            (outer: number, maxOuter: number, freqs: Float64Array) => {
              (self as unknown as Worker).postMessage({
                id: msg.id,
                progress: true,
                data: { outer, maxOuter, freqs: Array.from(freqs) },
              } satisfies ModalProgressMessage);
            }
          )
        );
        const displacements = requireModel().vertex_displacements();
        result.seconds = (performance.now() - t0) / 1000;
        reply(msg, { result, displacements }, [displacements.buffer]);
        return;
      }
      case "setBuildState": {
        // Flip on bed ⇄ released without re-solving; re-map the chosen field
        // onto the mesh for the deformed view.
        const stats = JSON.parse(requireModel().set_build_state(msg.state));
        const displacements = requireModel().vertex_displacements();
        reply(msg, { stats, displacements }, [displacements.buffer]);
        return;
      }
      case "settingsSweep": {
        // DESIGN §20: one blocking call in the worker (each candidate is a
        // full solve of every included load step); the wasm side pushes a
        // progress JSON per solved candidate, forwarded here so the panel can
        // narrate the landscape filling in.
        if (cancelArr) Atomics.store(cancelArr, 0, 0); // arm fresh
        const result = JSON.parse(
          requireModel().settings_sweep(
            JSON.stringify(msg.opts),
            // The axes push arrives with no field; every candidate push carries
            // its own per-soup-vertex SF field (the live preview), forwarded in
            // the progress message's `density` slot like the optimizer's.
            (json: string, field?: Float32Array) => {
              (self as unknown as Worker).postMessage(
                {
                  id: msg.id,
                  progress: true,
                  data: JSON.parse(json),
                  density: field,
                },
                field && field.length > 0 ? [field.buffer] : []
              );
            }
          )
        );
        reply(msg, result);
        return;
      }
      case "criterionSf": {
        reply(msg, JSON.parse(requireModel().criterion_sf(msg.measure)));
        return;
      }
      case "orientationSweep": {
        // DESIGN §15: one blocking begin (tensor extraction + mask), then the
        // pixel grid in row chunks so a progress push lands between calls.
        const m = requireModel();
        const meta = JSON.parse(m.orientation_sweep_begin(msg.ids, msg.stepDeg));
        const total: number = meta.pixels;
        const scored = new Float32Array(total);
        const all = new Float32Array(total);
        try {
          const CHUNK = Math.max(meta.n * 2, 64); // ~2 pitch rows per push
          for (let s = 0; s < total; s += CHUNK) {
            const [sc, al] = m.orientation_sweep_rows(s, CHUNK);
            scored.set(sc, s);
            all.set(al, s);
            (self as unknown as Worker).postMessage({
              id: msg.id,
              progress: true,
              data: { done: Math.min(s + CHUNK, total), total },
            });
          }
        } finally {
          m.orientation_sweep_end();
        }
        reply(
          msg,
          {
            n: meta.n,
            stepDeg: meta.stepDeg,
            scored,
            all,
            cellsSeen: meta.cellsSeen,
            cellsKept: meta.cellsKept,
            scoredCells: meta.scoredCells,
            materialSfMin: meta.materialSfMin,
          },
          [scored.buffer, all.buffer]
        );
        return;
      }
      case "setLayerShear":
        requireModel().set_layer_shear(msg.on);
        break;
      case "layerSfField": {
        const m = requireModel();
        const values =
          msg.surface === "voxel"
            ? m.layer_sf_voxel_field(msg.dir[0], msg.dir[1], msg.dir[2], msg.ids)
            : m.layer_sf_field(msg.dir[0], msg.dir[1], msg.dir[2], msg.ids);
        reply(msg, values, [values.buffer]);
        return;
      }
      case "optimize": {
        if (cancelArr) Atomics.store(cancelArr, 0, 0); // arm fresh
        const m = requireModel();
        const t0 = performance.now();
        const summary = JSON.parse(
          m.optimize(
            JSON.stringify(msg.opts),
            // Two message shapes ride this one callback: per-iteration progress
            // (JSON + the four preview buffers) and buffer-less `{phase: …}`
            // status pushes narrating the silent pipeline stages.
            (
              json: string,
              density?: Float32Array,
              skelPositions?: Float32Array,
              skelIndices?: Uint32Array,
              skelDensity?: Float32Array
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
                } satisfies OptimizeProgressMessage,
                density
                  ? [density.buffer, skelPositions!.buffer, skelIndices!.buffer, skelDensity!.buffer]
                  : []
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
        reply(msg, { summary, regions, vertexDensity, displacements }, transfer);
        return;
      }
      case "densityShape": {
        const arr = requireModel().density_isosurface(msg.threshold);
        const positions = arr[0] as Float32Array;
        const indices = arr[1] as Uint32Array;
        const density = arr[2] as Float32Array;
        reply(msg, { positions, indices, density }, [
          positions.buffer,
          indices.buffer,
          density.buffer,
        ]);
        return;
      }
      case "resmooth": {
        const m = requireModel();
        m.resmooth_regions(msg.iters);
        const { regions, transfer } = collectRegions(m);
        reply(msg, { regions }, transfer);
        return;
      }
      case "setIsoThreshold": {
        const m = requireModel();
        m.set_iso_threshold(msg.threshold, msg.smoothIters);
        const { regions, transfer } = collectRegions(m);
        reply(msg, { regions }, transfer);
        return;
      }
      case "resultField": {
        const values = requireModel().result_field(msg.kind);
        reply(msg, values, [values.buffer]);
        return;
      }
      case "reactionForces": {
        reply(msg, JSON.parse(requireModel().reaction_forces()));
        return;
      }
      case "peelField": {
        const values = requireModel().peel_field(msg.kind);
        reply(msg, values, [values.buffer]);
        return;
      }
      case "peelMap": {
        const arr = requireModel().peel_map(msg.kind);
        const positions = arr[0] as Float32Array;
        const values = arr[1] as Float32Array;
        reply(msg, { positions, values }, [positions.buffer, values.buffer]);
        return;
      }
      case "inherentStrainVoxels": {
        const arr = requireModel().inherent_strain_voxels(msg.layerMax, msg.shrinkXy, msg.shrinkZ);
        const hull = arr[0] as Float32Array;
        const values = arr[1] as Float32Array;
        const edges = arr[2] as Float32Array;
        const max = arr[3] as number;
        const nz = arr[4] as number;
        reply(msg, { hull, values, edges, max, nz }, [hull.buffer, values.buffer, edges.buffer]);
        return;
      }
      case "voxelResults": {
        const arr = requireModel().voxel_results(msg.solidBody);
        const positions = arr[0] as Float32Array;
        const displacements = arr[1] as Float32Array;
        const edges = arr[2] as Float32Array;
        const edgeDisplacements = arr[3] as Float32Array;
        reply(msg, { positions, displacements, edges, edgeDisplacements }, [
          positions.buffer,
          displacements.buffer,
          edges.buffer,
          edgeDisplacements.buffer,
        ]);
        return;
      }
      case "voxelResultField": {
        const values = requireModel().voxel_result_field(msg.kind, msg.solidBody);
        reply(msg, values, [values.buffer]);
        return;
      }
      case "sectionVolume": {
        const arr = requireModel().section_volume(msg.kind);
        const values = arr[0] as Float32Array;
        const disp = arr[1] as Float32Array;
        const meta = arr[2] as Float64Array;
        const range = Number.isNaN(meta[7])
          ? null
          : {
              min: meta[7],
              max: meta[8],
              minAt: [meta[9], meta[10], meta[11]] as [number, number, number],
              maxAt: [meta[12], meta[13], meta[14]] as [number, number, number],
            };
        reply(
          msg,
          {
            values,
            disp,
            dims: [meta[0], meta[1], meta[2]] as [number, number, number],
            origin: [meta[3], meta[4], meta[5]] as [number, number, number],
            h: meta[6],
            range,
          },
          [values.buffer, disp.buffer]
        );
        return;
      }
      case "stashResult":
        requireModel().stash_result(msg.resultId);
        break;
      case "activateResult": {
        const displacements = requireModel().activate_result(msg.resultId);
        reply(msg, displacements, [displacements.buffer]);
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
        reply(msg, mtx);
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
        reply(msg, bytes, [bytes.buffer]);
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
          if (mf.fileName) name = String(mf.fileName).replace(/\.(stl|3mf|step|stp|filasim)$/i, "");
        } catch {
          // manifest name is cosmetic — fall back to "project"
        }
        // STEP-model projects: the engine can't tessellate STEP (DESIGN §18)
        // — hand the embedded bytes back so the main thread runs meshStep and
        // follows up with `loadMesh`. The project stays staged for restore.
        const head = new TextDecoder("iso-8859-1").decode(
          modelBytes.subarray(0, Math.min(256, modelBytes.length))
        );
        if (head.includes("ISO-10303-21")) {
          pendingProject = projBytes;
          // Fresh copy: a plain ArrayBuffer view (transferable, exact size).
          const stepBytes = new Uint8Array(modelBytes).buffer;
          reply(msg, { manifest, stepModel: { bytes: stepBytes, name } }, [stepBytes]);
          return;
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
            bbox: Array.from(model.bbox()) as LoadedModelData["bbox"],
            meshObjects: model.mesh_object_count(),
            bodyCount: model.body_count(),
            hasCadFaces: model.has_cad_faces(),
          },
        };
        reply(msg, data, [positions.buffer, patchIds.buffer]);
        return;
      }
      case "openProjectRestore": {
        if (!pendingProject) throw new Error("no project staged to restore");
        const summary = JSON.parse(requireModel().restore_project(pendingProject));
        pendingProject = null;
        reply(msg, summary);
        return;
      }
      case "vertexDensity": {
        const vd = requireModel().vertex_density();
        reply(msg, vd, [vd.buffer]);
        return;
      }
      case "exportThreeMf": {
        const thumb = msg.thumbnail ?? new Uint8Array(0);
        const bytes = requireModel().export_3mf(msg.slicer, thumb);
        reply(msg, bytes, [bytes.buffer]);
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
        reply(msg, bytes, [bytes.buffer]);
        return;
      }
      case "exportStls": {
        const bytes = requireModel().export_stls();
        reply(msg, bytes, [bytes.buffer]);
        return;
      }
      case "exportSolidStl": {
        const bytes = requireModel().export_solid_stl();
        reply(msg, bytes, [bytes.buffer]);
        return;
      }
      default: {
        // Compile-time exhaustiveness: an op added to EngineRequests without
        // a case here fails this assignment. Unreachable at runtime.
        const unhandled: never = msg;
        return unhandled;
      }
    }
    (self as unknown as Worker).postMessage({ id: msg.id, ok: true });
  } catch (e) {
    (self as unknown as Worker).postMessage({
      id: msg.id,
      ok: false,
      error: e instanceof Error ? e.message : String(e),
    } satisfies WorkerErrorMessage);
  }
};

function requireModel(): Model {
  if (!model) throw new Error("no model loaded");
  return model;
}
