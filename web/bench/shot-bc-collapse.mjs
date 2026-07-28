// SPDX-License-Identifier: AGPL-3.0-only
// One-off visual check for collapsible BC rows: load a cube, add several
// supports + loads, screenshot the list (all but the newest collapsed), then
// click a collapsed row and confirm its editor expands.
// Usage: node bench/shot-bc-collapse.mjs
import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import puppeteer from "puppeteer-core";

const dist = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../dist");
const MIME = {
  ".html": "text/html",
  ".js": "text/javascript",
  ".css": "text/css",
  ".wasm": "application/wasm",
  ".woff2": "font/woff2",
};
const server = http.createServer((req, res) => {
  let file = path.join(dist, decodeURIComponent(new URL(req.url, "http://x").pathname));
  if (file.endsWith(path.sep)) file = path.join(file, "index.html");
  if (!fs.existsSync(file) || fs.statSync(file).isDirectory()) {
    res.writeHead(404);
    res.end();
    return;
  }
  res.writeHead(200, { "Content-Type": MIME[path.extname(file)] ?? "application/octet-stream" });
  fs.createReadStream(file).pipe(res);
});
await new Promise((ok) => server.listen(0, "127.0.0.1", ok));
const url = `http://127.0.0.1:${server.address().port}/`;

// 20 mm ASCII cube STL (12 triangles).
const V = [
  // [normal, a, b, c] per facet — two per cube face
  [[0,0,-1],[0,0,0],[20,20,0],[20,0,0]], [[0,0,-1],[0,0,0],[0,20,0],[20,20,0]],
  [[0,0,1],[0,0,20],[20,0,20],[20,20,20]], [[0,0,1],[0,0,20],[20,20,20],[0,20,20]],
  [[0,-1,0],[0,0,0],[20,0,0],[20,0,20]], [[0,-1,0],[0,0,0],[20,0,20],[0,0,20]],
  [[0,1,0],[0,20,0],[20,20,20],[20,20,0]], [[0,1,0],[0,20,0],[0,20,20],[20,20,20]],
  [[-1,0,0],[0,0,0],[0,0,20],[0,20,20]], [[-1,0,0],[0,0,0],[0,20,20],[0,20,0]],
  [[1,0,0],[20,0,0],[20,20,20],[20,0,20]], [[1,0,0],[20,0,0],[20,20,0],[20,20,20]],
];
const stl =
  "solid cube\n" +
  V.map(
    ([n, a, b, c]) =>
      `facet normal ${n.join(" ")}\nouter loop\n` +
      [a, b, c].map((v) => `vertex ${v.join(" ")}`).join("\n") +
      "\nendloop\nendfacet"
  ).join("\n") +
  "\nendsolid cube\n";
const stlPath = path.join(os.tmpdir(), "filasim-cube.stl");
fs.writeFileSync(stlPath, stl);

const browserPath = [
  "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
  "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
].find((p) => fs.existsSync(p));
const browser = await puppeteer.launch({ executablePath: browserPath, headless: "new" });
const page = await browser.newPage();
await page.setViewport({ width: 1500, height: 950 });
await page.goto(url, { waitUntil: "networkidle0" });
await page.click(".modal.disclaimer .consent");

const input = await page.$('input[type="file"]');
await input.uploadFile(stlPath);
// STL import asks for the file's unit — confirm the default (mm).
await page.waitForSelector(".modalfoot .primary", { timeout: 15000 });
await page.click(".modalfoot .primary");
await page.waitForSelector(".fileinfo", { timeout: 30000 });
await new Promise((r) => setTimeout(r, 1500));

// step 2 · Boundary conditions
await page.evaluate(() => {
  for (const b of document.querySelectorAll(".rail .station"))
    if (b.querySelector(".st-no")?.textContent === "2") b.click();
});
await page.waitForSelector(".addrow");

// Add 2 supports + 3 loads by button label. A BC with no surfaces assigned is
// dropped on the next add (dropUnassignedBc), so after each add click the cube
// — the select tool is armed and assigns the picked surface to the new BC.
const add = async (label, pick) => {
  await page.evaluate((l) => {
    for (const b of document.querySelectorAll(".addrow button"))
      if (b.textContent.trim() === l) return b.click();
  }, label);
  await new Promise((r) => setTimeout(r, 200));
  if (pick) {
    await page.mouse.click(pick[0], pick[1]);
    await new Promise((r) => setTimeout(r, 300));
  }
};
await add("+ Fixed", [820, 550]);
await add("+ Elastic", [1100, 550]);
await add("+ Force", [950, 260]);
await add("+ Pressure", [820, 550]);
await add("+ Acceleration"); // selection-less, never dropped
await add("+ Point mass", [950, 260]);
await add("+ Force", [1100, 550]);

const state = () =>
  page.$$eval(".bc", (rows) =>
    rows.map((r) => ({
      name: r.querySelector(".bcnameinput")?.value,
      active: r.classList.contains("active"),
      caret: r.querySelector(".bccaret")?.textContent,
      dim: r.querySelector(".bchead .dim")?.textContent,
      hasEditor: !!r.querySelector(".forceedit, .bcparams, .forcegrid"),
    }))
  );
console.log("after adding 5 BCs:", JSON.stringify(await state(), null, 1));
await page.screenshot({ path: "bench/bc-collapsed.png" });

// click the FIRST (collapsed) row's header dot area → expands, others collapse
await page.evaluate(() => document.querySelectorAll(".bc .bchead")[0].click());
await new Promise((r) => setTimeout(r, 200));
console.log("after clicking row 1:", JSON.stringify(await state(), null, 1));
await page.screenshot({ path: "bench/bc-row1-expanded.png" });

// click it again → everything collapsed
await page.evaluate(() => document.querySelectorAll(".bc .bchead")[0].click());
await new Promise((r) => setTimeout(r, 200));
console.log("after clicking row 1 again:", JSON.stringify(await state(), null, 1));
await page.screenshot({ path: "bench/bc-all-collapsed.png" });

await browser.close();
server.close();
