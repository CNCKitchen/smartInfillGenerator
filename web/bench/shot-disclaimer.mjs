// SPDX-License-Identifier: AGPL-3.0-only
// One-off visual check: serve dist/, screenshot the startup disclaimer,
// click consent, screenshot again. Usage: node bench/shot-disclaimer.mjs
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
await page.setViewport({ width: 1500, height: 950 });
await page.goto(url, { waitUntil: "networkidle0" });
await page.screenshot({ path: "bench/disclaimer-open.png" });
const headline = await page.$eval(".modal.disclaimer h2", (e) => e.textContent);
console.log("headline:", JSON.stringify(headline));
await page.click(".modal.disclaimer .consent");
await new Promise((r) => setTimeout(r, 300));
const gone = await page.$(".modal.disclaimer");
console.log("after consent, modal present:", !!gone);
await page.screenshot({ path: "bench/disclaimer-closed.png" });
await browser.close();
server.close();
