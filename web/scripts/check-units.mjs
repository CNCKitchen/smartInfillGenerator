// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Units lint backstop (docs/units-design.md §3.2). UI components (.tsx) must
// render values through the registry chokepoint (`format(v, kind)` / UnitInput),
// never by appending a hardcoded display-unit string to a number. This catches
// the regression: a stray `value.toFixed(2)} mm` or `} MPa` slipping back in.
//
// Scope: .tsx only. The engine-facing store.ts deliberately keeps canonical
// units in the "Log for nerds" lines, so .ts is not scanned.
//
// Run: node scripts/check-units.mjs   (exits 1 on a violation)

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const SRC = join(fileURLToPath(new URL(".", import.meta.url)), "..", "src");

// The bug pattern: a number formatting (or a `}` JSX expression close) followed
// by a hardcoded physical display unit. `%`, `×`, factors and axis labels are
// fine — only selectable physical units are forbidden.
const UNITS = ["mm", "cm", "µm", "MPa", "GPa", "kPa", "psi", "ksi", "N·mm", "N·m", "kN", "lbf", "g/cm³"];
const UNIT_ALT = UNITS.map((u) => u.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|");
const BAD = [
  new RegExp(`toFixed\\([^)]*\\)[^,;<\\n]{0,6}(?:${UNIT_ALT})\\b`),
  new RegExp(`\\}\\s+(?:${UNIT_ALT})\\b`),
];

function walk(dir) {
  const out = [];
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) out.push(...walk(p));
    else if (name.endsWith(".tsx")) out.push(p);
  }
  return out;
}

let violations = 0;
for (const file of walk(SRC)) {
  const lines = readFileSync(file, "utf8").split("\n");
  lines.forEach((line, i) => {
    if (BAD.some((re) => re.test(line))) {
      console.error(`${file}:${i + 1}: hardcoded display unit — route through format(v, kind)/UnitInput`);
      console.error(`   ${line.trim()}`);
      violations++;
    }
  });
}

if (violations) {
  console.error(`\n✗ ${violations} hardcoded display-unit string(s). See docs/units-design.md §3.2.`);
  process.exit(1);
}
console.log("✓ no hardcoded display units in .tsx");
