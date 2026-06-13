// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// One-off repro: drive the app, trigger a long busy (fine-resolution solve),
// screenshot during the spinner, and inventory orange DOM elements.
import { chromium } from "playwright";
import { writeFileSync } from "node:fs";

function boxStl(lo, hi) {
  const v = (x, y, z) => [x ? hi[0] : lo[0], y ? hi[1] : lo[1], z ? hi[2] : lo[2]];
  const faces = [
    [[0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0]],
    [[1, 0, 0], [1, 1, 0], [1, 1, 1], [1, 0, 1]],
    [[0, 0, 0], [1, 0, 0], [1, 0, 1], [0, 0, 1]],
    [[0, 1, 0], [0, 1, 1], [1, 1, 1], [1, 1, 0]],
    [[0, 0, 0], [0, 1, 0], [1, 1, 0], [1, 0, 0]],
    [[0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]],
  ];
  const tris = [];
  for (const f of faces) {
    const c = f.map(([x, y, z]) => v(x, y, z));
    tris.push([c[0], c[1], c[2]], [c[0], c[2], c[3]]);
  }
  const buf = new ArrayBuffer(84 + 50 * tris.length);
  const dv = new DataView(buf);
  dv.setUint32(80, tris.length, true);
  let off = 84;
  for (const t of tris) {
    off += 12;
    for (const p of t) for (const c of p) {
      dv.setFloat32(off, c, true);
      off += 4;
    }
    off += 2;
  }
  return Buffer.from(buf);
}

writeFileSync("repro-box.stl", boxStl([0, 0, 0], [60, 30, 15]));

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1600, height: 1000 } });
const sprites = [];
page.on("console", (m) => {
  const t = m.text();
  if (/sprite|shader|context/i.test(t)) sprites.push(t.slice(0, 120));
});
await page.goto("http://localhost:5173/");
await page.getByRole("button", { name: /I understand and consent/i }).click();
await page.locator('input[type="file"]').first().setInputFiles("repro-box.stl");
await page.waitForSelector("text=/\\d+ tris/i", { timeout: 30000 });

// Fine resolution: voxelizing ~1M cells keeps the busy chip up for seconds.
await page.locator("text=/Properties/i").first().click();
await page.locator("select").filter({ hasText: "Normal" }).selectOption("fine");
await page.locator(".rail >> text=/^Verify$/").first().click();
await page.getByRole("button", { name: "Solve once" }).click();
await page.waitForSelector("div.busychip", { timeout: 20000 });
await page.waitForTimeout(1200);

const busyVisible = await page.locator("div.busychip").isVisible();
await page.screenshot({ path: "repro-busy.png" });

const report = await page.evaluate(() => {
  const out = [];
  for (const el of document.querySelectorAll("*")) {
    const s = getComputedStyle(el);
    const paint = `${s.backgroundColor} ${s.borderTopColor} ${s.color} ${s.boxShadow}`;
    if (/232,\s*163|224,\s*106|217,\s*119|201,\s*123/.test(paint)) {
      const r = el.getBoundingClientRect();
      if (r.width > 0)
        out.push({
          cls: String(el.className).slice(0, 50),
          tag: el.tagName,
          x: Math.round(r.x),
          y: Math.round(r.y),
          w: Math.round(r.width),
          h: Math.round(r.height),
          anim: s.animationName,
        });
    }
  }
  return out;
});
console.log("busy visible:", busyVisible);
console.log("orange DOM elements:", JSON.stringify(report, null, 1));
console.log("sprite/shader console lines:", JSON.stringify(sprites.slice(0, 6), null, 1));
await browser.close();
