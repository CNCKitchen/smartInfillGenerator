// SPDX-License-Identifier: AGPL-3.0-only
// Runs bench_solve from both wasm builds inside a worker. The threaded
// module needs initThreadPool (spawns nested workers) and SharedArrayBuffer.

const CASES = [
  ["solve 128x32x32 (0.13M cells)", [128, 32, 32, 0.5]],
  ["solve 256x64x64 (1.05M cells)", [256, 64, 64, 0.25]],
];

function runCases(wasm, label, post) {
  for (const [name, [nx, ny, nz, h]] of CASES) {
    const t0 = performance.now();
    const ratio = wasm.bench_solve(nx, ny, nz, h);
    post({ label, name, seconds: (performance.now() - t0) / 1000, ratio });
  }
}

// Real-part case: 3DBenchy (thin shells, high contrast) through the actual
// Model API — fix the hull bottom, press the funnel down, default 300k cells.
async function runBenchy(mod, label, post, stlBytes) {
  const m = new mod.Model(new Uint8Array(stlBytes), "3dbenchy");
  const pos = m.positions(); // 9 floats per triangle
  let zmin = Infinity;
  let zmax = -Infinity;
  for (let i = 2; i < pos.length; i += 3) {
    if (pos[i] < zmin) zmin = pos[i];
    if (pos[i] > zmax) zmax = pos[i];
  }
  const fixed = [];
  const loaded = [];
  for (let t = 0; t * 9 < pos.length; t++) {
    const z0 = pos[9 * t + 2];
    const z1 = pos[9 * t + 5];
    const z2 = pos[9 * t + 8];
    if (z0 < zmin + 1 && z1 < zmin + 1 && z2 < zmin + 1) fixed.push(t);
    else if (z0 > zmax - 3 && z1 > zmax - 3 && z2 > zmax - 3) loaded.push(t);
  }
  m.add_fixed(new Uint32Array(fixed));
  m.add_force(new Uint32Array(loaded), 0, 0, -50);
  const tv0 = performance.now();
  const info = JSON.parse(m.voxel_info()); // voxelization, timed separately
  const tVox = (performance.now() - tv0) / 1000;
  const t0 = performance.now();
  const stats = JSON.parse(m.solve());
  const seconds = (performance.now() - t0) / 1000;
  post({
    label,
    name: `benchy solve (${(info.solid / 1e3).toFixed(0)}k solid cells)`,
    seconds,
    ratio: stats.maxDisplacement,
    iterations: stats.iterations,
    voxelize: tVox,
  });
  // Interactive re-solve: same supports, tweaked load — exercises the
  // solver-hierarchy cache + warm start (what a force-slider drag costs).
  m.clear_bcs();
  m.add_fixed(new Uint32Array(fixed));
  m.add_force(new Uint32Array(loaded), 5, 0, -55);
  const t1 = performance.now();
  const stats2 = JSON.parse(m.solve());
  post({
    label,
    name: "benchy re-solve (load tweak)",
    seconds: (performance.now() - t1) / 1000,
    ratio: stats2.maxDisplacement,
    iterations: stats2.iterations,
  });
  m.free();
}

self.onmessage = async (e) => {
  const post = (m) => self.postMessage(m);
  try {
    const stl = await (await fetch("/3dbenchy.stl")).arrayBuffer();

    const st = await import("../src/wasm/sig_wasm.js");
    const stWasm = await st.default();
    runCases(stWasm, "single-thread", post);
    await runBenchy(st, "single-thread", post, stl);

    if (self.crossOriginIsolated) {
      post({ label: "status", name: "loading mt module" });
      const mt = await import("/wasm-mt/sig_wasm.js");
      const mtWasm = await mt.default();
      const threads = Math.max(1, e.data.threads || 4);
      post({ label: "status", name: `mt loaded, initThreadPool(${threads})` });
      await mt.initThreadPool(threads);
      post({ label: "status", name: "pool ready" });
      runCases(mtWasm, `threaded x${threads}`, post);
      await runBenchy(mt, `threaded x${threads}`, post, stl);
    } else {
      post({ label: "threaded", name: "SKIPPED: not cross-origin isolated" });
    }
  } catch (err) {
    post({ label: "error", name: String(err && err.message ? err.message : err) });
  }
  post({ done: true });
};
