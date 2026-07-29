// SPDX-License-Identifier: AGPL-3.0-only
// One-off end-to-end check of the startup sample model: serve dist/, open the
// app cold, and assert that (1) the bundled CNCKTestHook.step auto-loads via
// the PRE-TESSELLATED cache (nerd-log line), (2) both canned BCs (Elastic 1 +
// Force 1) bound to their CAD faces, and (3) the setup actually SOLVES (the
// legend appears — meaning the check gate passed on the elastic+force rig).
// Screenshots the loaded setup and the solved result.
// Usage: node bench/shot-sample.mjs   (after `npm run build`)
import http from "node:http";
import fs from "node:fs";
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
  ".step": "application/octet-stream",
  ".mesh": "application/octet-stream",
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
page.on("console", (m) => {
  if (m.type() === "error") console.log("[page]", m.text());
});
await page.setViewport({ width: 1500, height: 950 });
const t0 = Date.now();
await page.goto(url, { waitUntil: "networkidle0" });
await page.click(".modal.disclaimer .consent");

// 1. The sample auto-loads without any interaction and lands on station 1
//    (Model), where the file card names the hook.
await page.waitForSelector(".fileinfo", { timeout: 30000 });
console.log(`sample visible ${Date.now() - t0} ms after goto`);
const fileinfo = await page.$eval(".fileinfo", (el) => el.textContent);
if (!fileinfo.includes("CNCKTestHook")) throw new Error(`fileinfo says "${fileinfo}"`);
// The replace button pulses while the sample is loaded (isSample flag).
if (!(await page.$("button.primary.sample-pulse")))
  throw new Error("replace button is not pulsing on the sample model");
const notice = await page.evaluate(
  () => document.querySelector(".toast.notice")?.textContent ?? ""
);
if (!notice.includes("Sample part loaded")) throw new Error(`no sample notice (got "${notice}")`);
await new Promise((r) => setTimeout(r, 1200));

// No error toast on boot.
const toast = await page.evaluate(
  () => document.querySelector(".toast:not(.notice)")?.textContent ?? null
);
if (toast) throw new Error(`error toast on boot: ${toast}`);

// 2. The import came from the pre-tessellated cache, not a live tessellation.
await page.evaluate(() => {
  for (const b of document.querySelectorAll("footer button"))
    if (b.textContent.includes("LOG FOR NERDS")) return b.click();
});
await page.waitForSelector(".nerdlog", { timeout: 5000 });
const log = await page.$eval(".nl-log", (el) => el.textContent);
if (!log.includes("pre-tessellated cache"))
  throw new Error(`nerd log has no cache-hit line:\n${log}`);
console.log("cache hit: yes");
await page.evaluate(() => {
  for (const b of document.querySelectorAll(".nerdlog button"))
    if (b.textContent.trim() === "Close") return b.click();
});

// 3. Both canned BCs are present and bound (station 2 lists them).
await page.evaluate(() => {
  for (const b of document.querySelectorAll(".rail .station"))
    if (b.querySelector(".st-no")?.textContent === "2") b.click();
});
await page.waitForSelector(".bcnameinput", { timeout: 5000 });
const bcNames = await page.$$eval(".bcnameinput", (els) => els.map((e) => e.value));
console.log("BCs:", JSON.stringify(bcNames));
if (!(bcNames.includes("Elastic 1") && bcNames.includes("Force 1")))
  throw new Error(`expected Elastic 1 + Force 1, got ${JSON.stringify(bcNames)}`);
await page.screenshot({ path: "bench/sample-boot.png" });
// Zoomed look at the hook (visual check of the force-face placement).
await page.mouse.move(840, 575);
for (let i = 0; i < 6; i++) {
  await page.mouse.wheel({ deltaY: -240 });
  await new Promise((r) => setTimeout(r, 120));
}
await new Promise((r) => setTimeout(r, 500));
await page.screenshot({ path: "bench/sample-hook-zoom.png" });
for (let i = 0; i < 6; i++) {
  await page.mouse.wheel({ deltaY: 240 });
  await new Promise((r) => setTimeout(r, 120));
}
await new Promise((r) => setTimeout(r, 500));

// 4. The canned setup solves (proves the elastic+force rig passes the check
//    gate — no missing selection, no rigid-body mode).
await page.evaluate(() => {
  for (const b of document.querySelectorAll(".rail .station"))
    if (b.querySelector(".st-no")?.textContent === "4") b.click();
});
await new Promise((r) => setTimeout(r, 400));
await page.evaluate(() => {
  for (const b of document.querySelectorAll("button"))
    if (b.textContent.trim() === "Solve once") return b.click();
});
await page.waitForSelector(".legend", { timeout: 120000 });
await new Promise((r) => setTimeout(r, 1500));
await page.evaluate(() => {
  for (const b of document.querySelectorAll("button"))
    if (b.textContent.includes("Close & Continue")) return b.click();
});
await new Promise((r) => setTimeout(r, 400));
const legend = await page.$eval(".legend", (el) => el.textContent);
console.log("legend:", legend.replace(/\s+/g, " ").slice(0, 160));
const errToast = await page.evaluate(
  () => document.querySelector(".toast:not(.notice)")?.textContent ?? null
);
if (errToast) throw new Error(`error toast after solve: ${errToast}`);
await page.screenshot({ path: "bench/sample-solved.png" });

await browser.close();
server.close();
console.log("PASS");
