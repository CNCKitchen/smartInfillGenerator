// Promise wrapper around the engine worker.

import type { Bc, CheckReport, LoadedModel, SolveStats, VoxelInfo } from "../types";

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
}

export class EngineClient {
  private worker: Worker;
  private nextId = 1;
  private pending = new Map<number, Pending>();

  constructor() {
    this.worker = new Worker(new URL("../worker/engine.worker.ts", import.meta.url), {
      type: "module",
    });
    this.worker.onmessage = (ev) => {
      const { id, ok, data, error } = ev.data;
      const p = this.pending.get(id);
      if (!p) return;
      this.pending.delete(id);
      if (ok) p.resolve(data);
      else p.reject(new Error(error));
    };
  }

  private call<T>(msg: Record<string, unknown>, transfer: Transferable[] = []): Promise<T> {
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject });
      this.worker.postMessage({ id, ...msg }, transfer);
    });
  }

  load(bytes: ArrayBuffer): Promise<LoadedModel> {
    return this.call<LoadedModel>({ op: "load", bytes }, [bytes]);
  }

  resegment(angle: number): Promise<{ patchIds: Uint32Array; patchCount: number }> {
    return this.call({ op: "resegment", angle });
  }

  setMaterial(e0: number, nu: number, density: number): Promise<void> {
    return this.call({ op: "setMaterial", e0, nu, density });
  }

  setGravity(on: boolean): Promise<void> {
    return this.call({ op: "setGravity", on });
  }

  setResolution(cells: number): Promise<void> {
    return this.call({ op: "setResolution", cells });
  }

  setBcs(bcs: Bc[]): Promise<void> {
    // Copy tri arrays: the originals stay with the UI.
    const payload = bcs.map((bc) => ({
      kind: bc.kind,
      tris: new Uint32Array(bc.tris),
      force: bc.force,
      pressure: bc.pressure,
    }));
    return this.call(
      { op: "setBcs", bcs: payload },
      payload.map((b) => b.tris.buffer)
    );
  }

  voxelInfo(): Promise<VoxelInfo> {
    return this.call({ op: "voxelInfo" });
  }

  check(): Promise<CheckReport> {
    return this.call({ op: "check" });
  }

  solve(): Promise<{ stats: SolveStats; displacements: Float32Array }> {
    return this.call({ op: "solve" });
  }
}

export const engine = new EngineClient();
