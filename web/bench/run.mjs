// SPDX-License-Identifier: AGPL-3.0-only
// Headless solver benchmark driver: serves web/ statically with COOP/COEP
// (cross-origin isolation -> SharedArrayBuffer -> threads) and drives a
// local Chrome/Edge via puppeteer-core.
//
// Usage: node bench/run.mjs

import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import puppeteer from "puppeteer-core";

const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

const MIME = {
  ".html": "text/html",
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".wasm": "application/wasm",
  ".json": "application/json",
};

const server = http.createServer((req, res) => {
  const urlPath = decodeURIComponent(new URL(req.url, "http://x").pathname);
  // Mirror vite: /public content is served at the site root.
  const mapped = urlPath.startsWith("/wasm-mt/") ? "/public" + urlPath : urlPath;
  let file = path.join(webRoot, mapped);
  // The benchmark STL lives at the repo root.
  if (urlPath === "/3dbenchy.stl") file = path.join(webRoot, "..", "3dbenchy.stl");
  if (urlPath.endsWith("/")) file = path.join(file, "index.html");
  const allowed = file.startsWith(webRoot) || urlPath === "/3dbenchy.stl";
  if (!allowed || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
    console.log(`[404] ${urlPath}`);
    res.writeHead(404);
    res.end("not found");
    return;
  }
  res.writeHead(200, {
    "Content-Type": MIME[path.extname(file)] ?? "application/octet-stream",
    "Cross-Origin-Opener-Policy": "same-origin",
    "Cross-Origin-Embedder-Policy": "require-corp",
    "Cache-Control": "no-store",
  });
  fs.createReadStream(file).pipe(res);
});

const browserPath = [
  "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
  "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
  "/usr/bin/google-chrome",
  "/usr/bin/chromium",
].find((p) => fs.existsSync(p));
if (!browserPath) {
  console.error("no Chrome/Edge found");
  process.exit(1);
}

await new Promise((ok) => server.listen(0, "127.0.0.1", ok));
const port = server.address().port;
const url = `http://127.0.0.1:${port}/bench/`;
console.log(`serving ${webRoot} at ${url}`);
console.log(`browser: ${browserPath}`);

const browser = await puppeteer.launch({
  executablePath: browserPath,
  headless: "new",
  protocolTimeout: 900_000,
});
try {
  const page = await browser.newPage();
  page.on("console", (m) => console.log(`[page] ${m.text()}`));
  await page.goto(url, { waitUntil: "load" });
  await page.waitForFunction("window.__benchResults !== undefined", {
    timeout: 600_000,
  });
  const results = await page.evaluate("window.__benchResults");
  console.log("\n=== results ===");
  for (const r of Array.isArray(results) ? results : [results]) {
    if (r.seconds !== undefined) {
      const extra =
        (r.iterations !== undefined ? `, ${r.iterations} iters` : "") +
        (r.voxelize !== undefined ? `, voxelize ${r.voxelize.toFixed(2)} s` : "");
      console.log(
        `${r.label.padEnd(16)} ${r.name}: ${r.seconds.toFixed(2)} s (ratio ${Number(r.ratio).toFixed(4)}${extra})`
      );
    } else {
      console.log(`${r.label}: ${r.name ?? r.error}`);
    }
  }
} finally {
  await browser.close();
  server.close();
}
