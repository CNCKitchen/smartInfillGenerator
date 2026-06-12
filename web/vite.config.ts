import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Cross-origin isolation unlocks SharedArrayBuffer -> threaded wasm solver.
// Production hosting must send the same two headers (or ship
// coi-serviceworker); the worker falls back to the single-threaded module
// when isolation is missing, so nothing breaks without them.
const coiHeaders = {
  "Cross-Origin-Opener-Policy": "same-origin",
  "Cross-Origin-Embedder-Policy": "require-corp",
};

export default defineConfig({
  // VITE_BASE is set by the GitHub Actions deploy workflow to /smartInfillGenerator/.
  // Unset in local dev → defaults to '/'.
  base: process.env.VITE_BASE,
  plugins: [react()],
  build: { target: "esnext" },
  worker: { format: "es" },
  esbuild: { target: "esnext" },
  server: { headers: coiHeaders },
  preview: { headers: coiHeaders },
});
