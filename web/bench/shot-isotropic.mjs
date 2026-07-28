// SPDX-License-Identifier: AGPL-3.0-only
// One-off check for isotropic materials (machined/cast/resin): load a cube,
// switch the material to steel and assert the print stack disappears (walls /
// infill / anisotropic / stiffness toggle / build-sim workspace), solve, check
// the SF field list collapses to the von Mises criterion, run Part Topo and
// assert the export offers the optimized-shape STL. Then switch back to PLA
// and assert the FDM controls return.
// Usage: node bench/shot-isotropic.mjs
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
const browser = await puppeteer.launch({
  executablePath: browserPath,
  headless: "new",
  protocolTimeout: 600000,
});
const page = await browser.newPage();
page.on("console", (m) => {
  if (m.type() === "error") console.log("[page]", m.text());
});
await page.setViewport({ width: 1500, height: 950 });
await page.goto(url, { waitUntil: "networkidle0" });
await page.click(".modal.disclaimer .consent");

const input = await page.$('input[type="file"]');
await input.uploadFile(stlPath);
await page.waitForSelector(".modalfoot .primary", { timeout: 15000 });
await page.click(".modalfoot .primary");
await page.waitForSelector(".fileinfo", { timeout: 30000 });
await new Promise((r) => setTimeout(r, 1500));

const station = async (no) => {
  await page.evaluate((n) => {
    for (const b of document.querySelectorAll(".rail .station"))
      if (b.querySelector(".st-no")?.textContent === n) b.click();
  }, no);
  await new Promise((r) => setTimeout(r, 400));
};
const panelText = () => page.$eval(".panel", (e) => e.textContent);
const assert = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

// ---- step 3 · Properties: FDM baseline, then switch to steel ----
await station("3");
let txt = await panelText();
assert(txt.includes("Perimeters"), "FDM baseline: Perimeters control missing");
assert(txt.includes("Infill pattern"), "FDM baseline: Infill pattern control missing");

const matSelect = await page.evaluate(() => {
  const sel = [...document.querySelectorAll(".panel select")].find((s) =>
    [...s.options].some((o) => o.textContent.includes("Steel"))
  );
  if (!sel) return null;
  const groups = [...sel.querySelectorAll("optgroup")].map((g) => g.label);
  const opts = [...sel.options].map((o) => o.textContent);
  sel.value = "Steel (mild, S235)";
  sel.dispatchEvent(new Event("change", { bubbles: true }));
  return { groups, opts };
});
assert(matSelect, "material select with a Steel option not found");
console.log("material optgroups:", JSON.stringify(matSelect.groups));
console.log("materials:", JSON.stringify(matSelect.opts));
assert(matSelect.opts.some((o) => o.includes("Aluminum 6061")), "Aluminum built-in missing");
assert(matSelect.opts.some((o) => o.includes("Resin (SLA")), "Resin built-in missing");
await new Promise((r) => setTimeout(r, 500));

txt = await panelText();
assert(!txt.includes("Perimeters"), "steel: Perimeters still shown");
assert(!txt.includes("Infill pattern"), "steel: Infill pattern still shown");
assert(!txt.includes("Anisotropic infill"), "steel: anisotropic checkbox still shown");
assert(!txt.includes("Snap voxel"), "steel: snap-to-wall checkbox still shown");
assert(txt.includes("Isotropic material"), "steel: isotropic note missing");
assert(txt.includes("σy"), "steel: yield readout missing");
console.log("step 3 OK: print stack hidden for steel");

// Build-sim workspace must be gone from the top bar.
const workspaceGone = await page.evaluate(
  () => ![...document.querySelectorAll("select option")].some((o) => o.value === "buildsim")
);
assert(workspaceGone, "steel: Build Simulation workspace still offered");
await page.screenshot({ path: "bench/isotropic-props.png" });

// ---- step 2 · BCs, then step 4 · Analyze: no stiffness toggle, solve ----
await station("2");
await page.waitForSelector(".addrow");
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
await add("+ Force", [950, 260]);

await station("4");
txt = await panelText();
assert(!txt.includes("As printed"), "steel: 'As printed' stiffness toggle still shown");
assert(txt.includes("solved fully dense"), "steel: solid-only analyze note missing");
await page.evaluate(() => {
  for (const b of document.querySelectorAll("button"))
    if (b.textContent.trim() === "Solve once") return b.click();
});
await page.waitForSelector(".legend", { timeout: 120000 });
await new Promise((r) => setTimeout(r, 1200));
await page.evaluate(() => {
  for (const b of document.querySelectorAll("button"))
    if (b.textContent.includes("Close & Continue")) return b.click();
});
console.log("step 4 OK: steel solve landed");

// SF options collapse to the von Mises criterion.
const sfOpts = await page.evaluate(() => {
  for (const sel of document.querySelectorAll("select")) {
    const vals = [...sel.options].map((o) => o.value);
    if (vals.includes("sfm")) return vals;
  }
  return null;
});
assert(sfOpts, "result-field select not found");
assert(!sfOpts.includes("sf") && !sfOpts.includes("sfz"), `steel: worst-case/layer SF still offered: ${sfOpts}`);
assert(sfOpts.includes("sfm") && sfOpts.includes("sfmx"), `steel: sfm/sfmx missing: ${sfOpts}`);
console.log("SF field list OK:", JSON.stringify(sfOpts.filter((v) => v.startsWith("sf"))));

// ---- step 5 · Part Topo only, then run it ----
await station("5");
txt = await panelText();
assert(!txt.includes("Graded"), "steel: Graded mode still offered");
assert(!txt.includes("Binary"), "steel: Binary mode still offered");
assert(txt.includes("Part topology optimization"), "steel: part-topo note missing");
assert(txt.includes("Optimize shape"), "steel: run button is not 'Optimize shape'");
assert(!txt.includes("Print settings"), "steel: settings optimizer still shown");
assert(!txt.includes("orientation"), "steel: orientation sweep still shown");
// The fold-section header carries the same "Optimize shape" title — click the
// PRIMARY run button only.
await page.evaluate(() => {
  for (const b of document.querySelectorAll(".panel button.primary"))
    if (b.textContent.trim() === "Optimize shape") return b.click();
});
// The busy chip / progress bar confirms the run actually started.
await page.waitForFunction(
  () => {
    const t = document.querySelector(".panel")?.textContent ?? "";
    return t.includes("iteration") || t.includes("Results land in the panel");
  },
  { timeout: 60000, polling: 1000 }
);
console.log("Part Topo started…");
// Part Topo on the preview grid: allow a few minutes. Poll from Node so a
// solver error / stall is visible instead of a blind timeout.
{
  const t0 = Date.now();
  let done = false;
  let last = "";
  while (Date.now() - t0 < 480000) {
    const st = await page.evaluate(() => ({
      panel: document.querySelector(".panel")?.textContent ?? "",
      err: document.querySelector(".errorbar, .error")?.textContent ?? "",
      busy: document.querySelector(".busy")?.textContent ?? "",
    }));
    if (st.err) throw new Error(`optimize errored: ${st.err}`);
    // Completion either leaves us on step 5 ("Results land…") or auto-advances
    // to the export step.
    if (
      st.panel.includes("Results land in the panel") ||
      st.panel.includes("Download optimized shape")
    ) {
      done = true;
      break;
    }
    const prog = st.panel.match(/iteration \d+ of max \d+/)?.[0] ?? st.busy ?? "";
    if (prog && prog !== last) {
      console.log("  …", prog);
      last = prog;
    }
    await new Promise((r) => setTimeout(r, 5000));
  }
  if (!done) {
    await page.screenshot({ path: "bench/isotropic-stall.png" });
    const dump = await page.evaluate(() => document.querySelector(".panel")?.textContent);
    throw new Error(`Part Topo did not finish; panel: ${dump?.slice(0, 400)}`);
  }
}
console.log("step 5 OK: Part Topo converged");
await page.screenshot({ path: "bench/isotropic-topo.png" });

// ---- step 6 · Export: shape STL only, no slicer 3MF ----
await station("6");
txt = await panelText();
assert(txt.includes("Download optimized shape"), "steel: shape STL export missing");
assert(!txt.includes("project (.3mf)"), "steel: slicer 3MF export still offered");
console.log("step 6 OK: shape STL export only");
await page.screenshot({ path: "bench/isotropic-export.png" });

// ---- switching back to PLA restores the print stack ----
await station("3");
await page.evaluate(() => {
  const sel = [...document.querySelectorAll(".panel select")].find((s) =>
    [...s.options].some((o) => o.textContent.includes("Steel"))
  );
  sel.value = "PLA";
  sel.dispatchEvent(new Event("change", { bubbles: true }));
});
await new Promise((r) => setTimeout(r, 500));
txt = await panelText();
assert(txt.includes("Perimeters"), "back to PLA: Perimeters not restored");
assert(txt.includes("Infill pattern"), "back to PLA: Infill pattern not restored");
const workspaceBack = await page.evaluate(
  () => [...document.querySelectorAll("select option")].some((o) => o.value === "buildsim")
);
assert(workspaceBack, "back to PLA: Build Simulation workspace not restored");
console.log("PLA restore OK");

await browser.close();
server.close();
console.log("PASS");
