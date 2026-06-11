/// <reference lib="webworker" />
// The wasm Model lives here; the main thread talks via EngineClient.

import init, { Model } from "../wasm/sig_wasm.js";

let model: Model | null = null;
const ready = init();

type Req =
  | { id: number; op: "load"; bytes: ArrayBuffer; name: string }
  | { id: number; op: "resegment"; angle: number }
  | { id: number; op: "setMaterial"; e0: number; nu: number; density: number }
  | { id: number; op: "setGravity"; on: boolean }
  | { id: number; op: "setResolution"; cells: number }
  | {
      id: number;
      op: "setBcs";
      bcs: { kind: string; tris: Uint32Array; force?: number[]; pressure?: number }[];
    }
  | { id: number; op: "voxelInfo" }
  | { id: number; op: "voxelMesh" }
  | { id: number; op: "check" }
  | { id: number; op: "solve" }
  | {
      id: number;
      op: "optimize";
      budgetPct: number;
      exponent: number;
      coeff: number;
      perimeters: number;
      lineWidth: number;
      smoothIters: number;
      nBins: number;
    }
  | { id: number; op: "densityShape"; threshold: number }
  | { id: number; op: "resmooth"; iters: number }
  | { id: number; op: "resultField"; kind: string }
  | { id: number; op: "exportThreeMf" }
  | { id: number; op: "exportStls" };

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
        model = new Model(new Uint8Array(msg.bytes), msg.name);
        const positions = model.positions();
        const patchIds = model.patch_ids();
        const data = {
          positions,
          patchIds,
          patchCount: model.patch_count(),
          triCount: model.triangle_count(),
          bbox: Array.from(model.bbox()),
          meshObjects: model.mesh_object_count(),
        };
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data }, [
          positions.buffer,
          patchIds.buffer,
        ]);
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
      case "setMaterial":
        requireModel().set_material(msg.e0, msg.nu, msg.density);
        break;
      case "setGravity":
        requireModel().set_gravity(msg.on);
        break;
      case "setResolution":
        requireModel().set_resolution(msg.cells);
        break;
      case "setBcs": {
        const m = requireModel();
        m.clear_bcs();
        for (const bc of msg.bcs) {
          if (bc.kind === "fixed") m.add_fixed(bc.tris);
          else if (bc.kind === "frictionless") m.add_frictionless(bc.tris);
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
      case "check": {
        const report = JSON.parse(requireModel().check());
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: report });
        return;
      }
      case "solve": {
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
      case "optimize": {
        const m = requireModel();
        const t0 = performance.now();
        const summary = JSON.parse(
          m.optimize(
            msg.budgetPct,
            msg.exponent,
            msg.coeff,
            msg.perimeters,
            msg.lineWidth,
            msg.smoothIters,
            msg.nBins,
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
      case "resultField": {
        const values = requireModel().result_field(msg.kind);
        (self as unknown as Worker).postMessage({ id: msg.id, ok: true, data: values }, [
          values.buffer,
        ]);
        return;
      }
      case "exportThreeMf": {
        const bytes = requireModel().export_3mf();
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
