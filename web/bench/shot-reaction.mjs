// SPDX-License-Identifier: AGPL-3.0-only
// One-off visual + numeric check for the reaction-forces result view: load a
// cube, fix one face, load another (default 10 N), solve, switch the result
// field to "reaction" and assert the legend table balances the applied load
// (|F| ≈ 10 N) with the view undeformed (exaggeration 0). Screenshots the
// arrows + table.
// Usage: node bench/shot-reaction.mjs
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
const browser = await puppeteer.launch({ executablePath: browserPath, headless: "new" });
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

// step 2 · Boundary conditions: fixed on a lower face, force on the top.
const station = async (no) => {
  await page.evaluate((n) => {
    for (const b of document.querySelectorAll(".rail .station"))
      if (b.querySelector(".st-no")?.textContent === n) b.click();
  }, no);
  await new Promise((r) => setTimeout(r, 400));
};
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
await add("+ Force", [950, 260]); // default 10 N along the picked face normal

// step 4 · Analyze: solve once.
await station("4");
await page.evaluate(() => {
  for (const b of document.querySelectorAll("button"))
    if (b.textContent.trim() === "Solve once") return b.click();
});
// Wait for the deformed result view (legend shows up when the solve lands).
await page.waitForSelector(".legend", { timeout: 120000 });
await new Promise((r) => setTimeout(r, 1500));
// Dismiss the support/tip popup so it doesn't cover the viewport.
await page.evaluate(() => {
  for (const b of document.querySelectorAll("button"))
    if (b.textContent.includes("Close & Continue")) return b.click();
});
await new Promise((r) => setTimeout(r, 400));

// Switch the result field to "reaction".
await page.evaluate(() => {
  for (const sel of document.querySelectorAll("select")) {
    const opt = [...sel.options].find((o) => o.value === "reaction");
    if (opt) {
      sel.value = "reaction";
      sel.dispatchEvent(new Event("change", { bubbles: true }));
      return;
    }
  }
  throw new Error("no select with a 'reaction' option");
});
await page.waitForSelector(".legendreaction", { timeout: 30000 });
await new Promise((r) => setTimeout(r, 1200));

const legend = await page.evaluate(() => {
  const el = document.querySelector(".legendreaction");
  const rows = [...el.querySelectorAll(".reactionrow")].map((r) => ({
    name: r.querySelector(".reactionname")?.textContent.trim(),
    cells: [...r.querySelectorAll("td")].map((c) => c.textContent.trim()),
  }));
  return { rows, text: el.textContent };
});
console.log("reaction table:", JSON.stringify(legend.rows, null, 1));

// Assertions: one support row; |F| ≈ 10 N; view is undeformed.
if (legend.rows.length !== 1) throw new Error(`expected 1 support row, got ${legend.rows.length}`);
const fTotal = parseFloat(legend.rows[0].cells[1]);
if (!(Math.abs(fTotal - 10) < 0.5))
  throw new Error(`fixed-support |F| = ${fTotal}, expected ≈ 10 N (balance of the 10 N load)`);
if (!legend.text.includes("undeformed"))
  throw new Error("reaction view did not drop the exaggeration to 0 (no 'undeformed' note)");
console.log(`OK: |F| = ${fTotal} N balances the 10 N load; view undeformed.`);

await page.screenshot({ path: "bench/reaction-view.png" });

// Switch back to |u| and confirm the exaggeration snapshot is restored
// (legend note no longer says undeformed) and the table is gone.
await page.evaluate(() => {
  for (const sel of document.querySelectorAll("select")) {
    const opt = [...sel.options].find((o) => o.value === "u");
    if (opt && [...sel.options].some((o) => o.value === "reaction")) {
      sel.value = "u";
      sel.dispatchEvent(new Event("change", { bubbles: true }));
      return;
    }
  }
});
await new Promise((r) => setTimeout(r, 1200));
const after = await page.evaluate(() => ({
  reactionGone: !document.querySelector(".legendreaction"),
  undeformed: document.querySelector(".legend")?.textContent.includes("undeformed") ?? false,
}));
console.log("after switching back to |u|:", JSON.stringify(after));
if (!after.reactionGone) throw new Error("reaction table did not clear on field switch");
if (after.undeformed) throw new Error("exaggeration was not restored on leaving the reaction view");
await page.screenshot({ path: "bench/reaction-restored.png" });

await browser.close();
server.close();
console.log("PASS");
