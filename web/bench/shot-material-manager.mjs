// SPDX-License-Identifier: AGPL-3.0-only
// One-off visual check: open the Material Manager (Settings → Manage
// materials…), screenshot FDM detail + charts, toggle a comparison, switch a
// material's process, and check the Properties-step edit link opens it.
// Usage: node bench/shot-material-manager.mjs
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

const browserPath = [
  "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
  "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
].find((p) => fs.existsSync(p));
const browser = await puppeteer.launch({ executablePath: browserPath, headless: "new" });
const page = await browser.newPage();
page.on("pageerror", (e) => console.log("PAGE ERROR:", e.message));
await page.setViewport({ width: 1500, height: 1100 });
await page.goto(url, { waitUntil: "networkidle0" });
await page.click(".modal.disclaimer .consent");
await new Promise((r) => setTimeout(r, 300));

const clickByText = async (sel, text) => {
  const ok = await page.evaluate(
    (sel, text) => {
      const el = [...document.querySelectorAll(sel)].find((b) => b.textContent.includes(text));
      if (el) el.click();
      return !!el;
    },
    sel,
    text
  );
  if (!ok) throw new Error(`not found: ${sel} "${text}"`);
};

// Settings → Manage materials…
await clickByText("button", "⚙");
await clickByText("button", "Manage materials");
await new Promise((r) => setTimeout(r, 300));
console.log(
  "manager head:",
  await page.$eval(".propsmodal .modalhead h2", (e) => e.textContent.trim())
);
console.log(
  "list rows:",
  await page.$$eval(".propslist .propsrow", (rows) => rows.map((r) => r.textContent.trim()))
);
await page.screenshot({ path: "bench/matmanager-fdm.png" });

// Compare PLA with PETG + Aluminum (via the add-comparison picker), then
// select an isotropic material.
const addCompare = async (text) => {
  const ok = await page.evaluate((text) => {
    const sel = document.querySelector(".chartcontrols select");
    const opt = sel && [...sel.options].find((o) => o.textContent.includes(text));
    if (!opt) return false;
    sel.value = opt.value;
    sel.dispatchEvent(new Event("change", { bubbles: true }));
    return true;
  }, text);
  if (!ok) throw new Error(`compare option not found: "${text}"`);
};
await addCompare("PETG");
await addCompare("Aluminum");
await new Promise((r) => setTimeout(r, 250));
await page.screenshot({ path: "bench/matmanager-compare.png" });

await clickByText(".propslist .propsrow", "Steel");
await new Promise((r) => setTimeout(r, 250));
console.log(
  "steel sections:",
  await page.$$eval(".propsdetail .sectiontitle", (es) => es.map((e) => e.textContent.trim()))
);
await page.screenshot({ path: "bench/matmanager-iso.png" });

// Duplicate steel, flip the copy to FDM, check the FDM sections appear.
await clickByText(".propsdetail .toolrow button", "Duplicate");
await new Promise((r) => setTimeout(r, 250));
await clickByText(".procswitch .chip", "FDM");
await new Promise((r) => setTimeout(r, 250));
console.log(
  "flipped-copy sections:",
  await page.$$eval(".propsdetail .sectiontitle", (es) => es.map((e) => e.textContent.trim()))
);
await page.screenshot({ path: "bench/matmanager-flip.png" });
// Reset defaults must demand a typed DELETE (and also cleans up the copy).
await clickByText(".propslist .toolrow button", "Reset defaults");
await new Promise((r) => setTimeout(r, 200));
const disabledBefore = await page.$eval(
  ".confirmreset .toolrow button",
  (b) => b.disabled
);
await page.type(".confirmreset input", "nope");
const disabledWrong = await page.$eval(".confirmreset .toolrow button", (b) => b.disabled);
await page.screenshot({ path: "bench/matmanager-reset-confirm.png" });
await page.$eval(".confirmreset input", (i) => i.select());
await page.type(".confirmreset input", "DELETE");
await new Promise((r) => setTimeout(r, 150));
console.log("typed:", await page.$eval(".confirmreset input", (i) => i.value));
const disabledRight = await page.$eval(".confirmreset .toolrow button", (b) => b.disabled);
console.log(
  "reset confirm disabled empty/wrong/DELETE:",
  disabledBefore,
  disabledWrong,
  disabledRight
);
await clickByText(".confirmreset .toolrow button", "Reset to defaults");
await new Promise((r) => setTimeout(r, 300));
console.log(
  "after reset: dialog gone:",
  !(await page.$(".confirmreset")),
  "· rows:",
  (await page.$$(".propslist .propsrow")).length
);

// Close the manager + settings; load a cube so the Properties step renders,
// then check its "edit" link reopens the manager.
await page.click(".propsmodal .modalhead .x");
await new Promise((r) => setTimeout(r, 150));
await page.click(".modal .modalhead .x");
await new Promise((r) => setTimeout(r, 150));

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
const input = await page.$('input[type="file"]');
await input.uploadFile(stlPath);
await page.waitForSelector(".modalfoot .primary", { timeout: 15000 });
await page.click(".modalfoot .primary");
await page.waitForSelector(".fileinfo", { timeout: 30000 });
await new Promise((r) => setTimeout(r, 1200));

await page.evaluate(() => {
  for (const b of document.querySelectorAll(".rail .station"))
    if (b.querySelector(".st-no")?.textContent === "3") b.click();
});
await new Promise((r) => setTimeout(r, 400));
await clickByText("a.link", "edit");
await new Promise((r) => setTimeout(r, 300));
console.log("edit link reopens manager:", !!(await page.$(".propsmodal")));
await page.screenshot({ path: "bench/matmanager-from-step.png" });

await browser.close();
server.close();
