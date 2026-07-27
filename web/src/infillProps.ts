// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Infill property sets (DESIGN §24) — the unit of infill calibration data.
//
// A set carries FITS ONLY: the Gibson–Ashby magnitude law {coeff, exponent}
// plus the five independent transverse-isotropic ratios. Measured per-density
// points stay out of the product on purpose (§24.1 dec. 1). `Gxy` and `ν_zp`
// are DERIVED, never stored — the same anti-inconsistency rule as the Rust
// `TiRatios` (a stored sixth constant would let a data-entry slip produce a
// tensor that is not actually TI while the kernel accepted it).

import type { PatternCurve } from "./types";
import { DEFAULT_CURVES } from "./types";

/** Model classes the FORMAT knows. Only classes the kernel can actually solve
 *  are selectable for solving (today: both of these — isotropic is the
 *  degenerate TI case). Tetragonal (6th constant) is a future additive class;
 *  unknown classes import as view-only, never solvable (§24.1 dec. 2). */
export type InfillModelClass = "isotropic" | "transverse_isotropic";

const MODEL_CLASSES: InfillModelClass[] = ["isotropic", "transverse_isotropic"];

/** The five independent TI constants, normalized to Ep = 1 (mirror of the
 *  Rust `TiRatios`; "p" = layer plane, "z" = build axis). */
export interface TiRatiosData {
  /** Ez/Ep — build-axis over in-plane modulus. */
  ezEp: number;
  /** Gz/Ep — through-layer shear over in-plane modulus. */
  gzEp: number;
  /** ν_p — in-plane Poisson ratio. */
  nuP: number;
  /** ν_pz — in-plane stress → build-axis contraction (major ratio). */
  nuPz: number;
}

/** Free-text provenance — who measured this, on what, under which terms. */
export interface SetProvenance {
  author?: string;
  /** Material + process the calibration ran on (free text). */
  calibratedOn?: string;
  /** ISO date of the calibration / publication. */
  date?: string;
  /** License / terms text. */
  license?: string;
}

export interface InfillPropertySet {
  /** Stable identity across imports/updates (uuid; built-in uses a well-known id). */
  id: string;
  /** Publisher version — same id + higher version offers a replace on import. */
  version: number;
  name: string;
  /** Pattern display name (free-form; the kernel doesn't switch on it). */
  pattern: string;
  modelClass: InfillModelClass;
  /** Magnitude law E/E₀ = coeff·ρ^exponent — the ρ dependence. */
  curve: PatternCurve;
  /** Anisotropy shape, constant across the band (§22 dec. 2). */
  ratios: TiRatiosData;
  /** Density band the fit is calibrated over, as fractions. Curves render
   *  dashed outside it; never clamps anything (§24.1 dec. 7). */
  calibratedBand: [number, number];
  /** false = paid pack: projects reference {id, version, name} instead of
   *  embedding the values (§24.1 dec. 4). */
  embedInProject: boolean;
  provenance: SetProvenance;
  /** Where this entry came from — decides read-only (builtin/imported) vs
   *  editable (user). Library-local, never serialized to .filaprops. */
  origin: "builtin" | "user" | "imported";
}

/** Derived in-plane shear ratio — TI fixes it, it is not free. */
export function gxyEp(r: TiRatiosData): number {
  return 1 / (2 * (1 + r.nuP));
}

/** Derived ν_zp by Maxwell reciprocity. */
export function nuZp(r: TiRatiosData): number {
  return r.nuPz * r.ezEp;
}

/** Physical admissibility — closed-form mirror of `TiRatios::is_physical`
 *  (Sylvester on the normal compliance block + positive shear). The manager
 *  clamps inputs, the wasm layer re-checks independently; this is the UI-side
 *  gate so a bad import is refused with a reason instead of failing a solve. */
export function isPhysicalRatios(r: TiRatiosData): boolean {
  if (!(r.ezEp > 0 && r.gzEp > 0 && r.nuP > -1)) return false;
  if (![r.ezEp, r.gzEp, r.nuP, r.nuPz].every(Number.isFinite)) return false;
  const minor2 = 1 - r.nuP * r.nuP;
  const det = (1 - r.nuP * r.nuP) / r.ezEp - 2 * r.nuPz * r.nuPz * (1 + r.nuP);
  return minor2 > 0 && det > 0;
}

/** Relative stiffness E/E₀ of one direction at density ρ. `ep` is the raw
 *  magnitude law (capped at 1 like the Settings readout always did); the
 *  other directions scale it by their frozen ratio. */
export function relStiffness(
  set: InfillPropertySet,
  rho: number,
  dir: "ep" | "ez" | "gz" | "gxy"
): number {
  const ep = Math.min(1, set.curve.coeff * Math.pow(rho, set.curve.exponent));
  switch (dir) {
    case "ep":
      return ep;
    case "ez":
      return ep * set.ratios.ezEp;
    case "gz":
      return ep * set.ratios.gzEp;
    case "gxy":
      return ep * gxyEp(set.ratios);
  }
}

// ---- built-in set ----

export const BUILTIN_CUBIC_ID = "builtin-cubic";

/** The measured cubic calibration (DESIGN §22.3/§22.5). The ratio values MUST
 *  equal the Rust `ti::CUBIC` constants — the smoke test pins the identity
 *  "explicit built-in ratios ≡ none sent" so a drift here is a test failure,
 *  not a silent physics change. */
export const BUILTIN_CUBIC: InfillPropertySet = {
  id: BUILTIN_CUBIC_ID,
  version: 1,
  name: "Cubic (built-in)",
  pattern: "Cubic",
  modelClass: "transverse_isotropic",
  curve: { ...DEFAULT_CURVES.cubic },
  ratios: { ezEp: 0.8029, gzEp: 0.4247, nuP: 0.2713, nuPz: 0.3651 },
  calibratedBand: [0.2, 0.7],
  embedInProject: true,
  provenance: {
    author: "CNC Kitchen",
    calibratedOn: "PLA, 0.45 mm line / 0.2 mm layer, flow-calibrated toolpath homogenization",
    date: "2026-07-26",
    license: "Bundled with InFEAll",
  },
  origin: "builtin",
};

// ---- input bounds (manager clamps; import validates) ----

export const CURVE_BOUNDS = {
  coeff: [0.05, 2] as const,
  exponent: [1, 3.5] as const,
};

export const RATIO_BOUNDS: Record<keyof TiRatiosData, readonly [number, number]> = {
  ezEp: [0.05, 2],
  gzEp: [0.01, 1.5],
  nuP: [0, 0.49],
  nuPz: [0, 0.6],
};

// ---- .filaprops interchange (§24.1 dec. 9) ----

export const FILAPROPS_VERSION = 1;
export const FILAPROPS_EXT = ".filaprops";

/** Serialize sets for export — `origin` is library-local and stripped. */
export function exportFilaprops(sets: InfillPropertySet[]): string {
  const out = sets.map(({ origin: _origin, ...rest }) => rest);
  return JSON.stringify({ formatVersion: FILAPROPS_VERSION, sets: out }, null, 2);
}

export interface ImportParse {
  sets: InfillPropertySet[];
  /** One human-readable reason per REJECTED entry (schema/physicality). */
  errors: string[];
}

function num(v: unknown): v is number {
  return typeof v === "number" && Number.isFinite(v);
}

/** Parse + validate a .filaprops payload. Invalid entries are rejected with a
 *  reason, never silently repaired (§24.1 dec. 10); valid ones come back with
 *  origin "imported". */
export function parseFilaprops(text: string): ImportParse {
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return { sets: [], errors: ["not a JSON file"] };
  }
  const doc = raw as { formatVersion?: unknown; sets?: unknown };
  if (!num(doc.formatVersion) || doc.formatVersion > FILAPROPS_VERSION) {
    return {
      sets: [],
      errors: [
        `unsupported formatVersion ${String(doc.formatVersion)} (this build reads ≤ ${FILAPROPS_VERSION})`,
      ],
    };
  }
  if (!Array.isArray(doc.sets)) return { sets: [], errors: ["missing sets array"] };

  const sets: InfillPropertySet[] = [];
  const errors: string[] = [];
  doc.sets.forEach((entry, i) => {
    const label = () => {
      const n = (entry as { name?: unknown })?.name;
      return typeof n === "string" && n ? `"${n}"` : `entry ${i + 1}`;
    };
    const e = entry as Partial<InfillPropertySet> & Record<string, unknown>;
    if (typeof e.id !== "string" || !e.id) return void errors.push(`${label()}: missing id`);
    if (typeof e.name !== "string" || !e.name) return void errors.push(`${label()}: missing name`);
    if (!MODEL_CLASSES.includes(e.modelClass as InfillModelClass))
      return void errors.push(
        `${label()}: unknown modelClass "${String(e.modelClass)}" — this build solves ${MODEL_CLASSES.join(", ")}`
      );
    const c = e.curve as PatternCurve | undefined;
    if (!c || !num(c.coeff) || !num(c.exponent))
      return void errors.push(`${label()}: missing curve {coeff, exponent}`);
    const r = e.ratios as TiRatiosData | undefined;
    if (!r || !num(r.ezEp) || !num(r.gzEp) || !num(r.nuP) || !num(r.nuPz))
      return void errors.push(`${label()}: missing ratios {ezEp, gzEp, nuP, nuPz}`);
    if (!isPhysicalRatios(r))
      return void errors.push(
        `${label()}: ratios are not a physically valid material (stiffness tensor not positive definite)`
      );
    const band = e.calibratedBand as [number, number] | undefined;
    const validBand =
      Array.isArray(band) && band.length === 2 && num(band[0]) && num(band[1]) &&
      band[0] > 0 && band[0] < band[1] && band[1] <= 1;
    if (!validBand) return void errors.push(`${label()}: calibratedBand must be [lo, hi] fractions in (0, 1]`);
    const p = (e.provenance ?? {}) as SetProvenance;
    sets.push({
      id: e.id,
      version: num(e.version) ? e.version : 1,
      name: e.name,
      pattern: typeof e.pattern === "string" && e.pattern ? e.pattern : e.name,
      modelClass: e.modelClass as InfillModelClass,
      curve: { coeff: c.coeff, exponent: c.exponent },
      ratios: { ezEp: r.ezEp, gzEp: r.gzEp, nuP: r.nuP, nuPz: r.nuPz },
      calibratedBand: [band![0], band![1]],
      embedInProject: e.embedInProject !== false,
      provenance: {
        author: typeof p.author === "string" ? p.author : undefined,
        calibratedOn: typeof p.calibratedOn === "string" ? p.calibratedOn : undefined,
        date: typeof p.date === "string" ? p.date : undefined,
        license: typeof p.license === "string" ? p.license : undefined,
      },
      origin: "imported",
    });
  });
  return { sets, errors };
}

export interface MergeReport {
  added: string[];
  replaced: string[];
  /** Renamed on import: same name, different id. */
  renamed: string[];
  /** Skipped: same id, same-or-older version already present. */
  skipped: string[];
}

/** First unused variant of `want`: the name itself, else "name (2)", … */
export function uniqueSetName(want: string, sets: InfillPropertySet[]): string {
  if (!sets.some((s) => s.name === want)) return want;
  for (let n = 2; ; n++) {
    const cand = `${want} (${n})`;
    if (!sets.some((s) => s.name === cand)) return cand;
  }
}

/** Merge parsed imports into the library (§24.1 dec. 9 collision rules):
 *  same id → replace only when strictly newer, else skip; the built-in id and
 *  name clashes import under a suffixed name. Pure — returns the new list. */
export function mergeImport(
  library: InfillPropertySet[],
  incoming: InfillPropertySet[]
): { library: InfillPropertySet[]; report: MergeReport } {
  const out = [...library];
  const report: MergeReport = { added: [], replaced: [], renamed: [], skipped: [] };
  const freshName = (want: string) => uniqueSetName(want, out);
  for (const s of incoming) {
    const at = out.findIndex((x) => x.id === s.id);
    if (at >= 0 && out[at].origin !== "builtin") {
      if (s.version > out[at].version) {
        out[at] = { ...s, name: out[at].name === s.name ? s.name : freshName(s.name) };
        report.replaced.push(out[at].name);
      } else {
        report.skipped.push(s.name);
      }
      continue;
    }
    // Colliding with the built-in id (or a name already taken) → new identity
    // under a suffixed name rather than shadowing.
    const entry = { ...s };
    if (at >= 0) entry.id = crypto.randomUUID();
    const name = freshName(entry.name);
    if (name !== entry.name) {
      entry.name = name;
      report.renamed.push(name);
    } else {
      report.added.push(name);
    }
    out.push(entry);
  }
  return { library: out, report };
}

export function describeMerge(r: MergeReport, errors: string[]): string {
  const bits: string[] = [];
  const list = (xs: string[]) => xs.join(", ");
  if (r.added.length) bits.push(`imported ${list(r.added)}`);
  if (r.renamed.length) bits.push(`imported as ${list(r.renamed)} (name taken)`);
  if (r.replaced.length) bits.push(`updated ${list(r.replaced)}`);
  if (r.skipped.length) bits.push(`skipped ${list(r.skipped)} (already present)`);
  if (errors.length) bits.push(`rejected ${errors.length}: ${errors.join("; ")}`);
  return bits.length ? bits[0].charAt(0).toUpperCase() + bits.join(" · ").slice(1) : "Nothing to import.";
}

// ---- per-browser library persistence ----

const LIBRARY_KEY = "sig.infillprops.v1";

export interface StoredLibrary {
  sets: InfillPropertySet[];
  activeSetId: string | null;
}

/** Load the library. The built-in set is INJECTED, never persisted — its
 *  values ride each release, not the user's storage. First run migrates a
 *  legacy user-calibrated curve (old ⚙ Settings c/n) into a user set so
 *  nobody's calibration silently reverts (§24.2). */
export function loadPropsLibrary(legacyCubicCurve: PatternCurve): StoredLibrary {
  let stored: InfillPropertySet[] = [];
  let activeSetId: string | null = BUILTIN_CUBIC_ID;
  let firstRun = true;
  try {
    const raw = localStorage.getItem(LIBRARY_KEY);
    if (raw) {
      firstRun = false;
      const p = JSON.parse(raw) as Partial<StoredLibrary>;
      // Re-validate through the import parser: storage is user-writable, and
      // one corrupt entry must not take the library down.
      if (Array.isArray(p.sets)) {
        const revalidated = parseFilaprops(
          JSON.stringify({ formatVersion: FILAPROPS_VERSION, sets: p.sets })
        );
        const originOf = new Map(
          (p.sets as InfillPropertySet[]).map((s) => [s.id, s.origin] as const)
        );
        stored = revalidated.sets.map((s) => ({
          ...s,
          origin: originOf.get(s.id) === "user" ? "user" : "imported",
        }));
      }
      if (typeof p.activeSetId === "string" || p.activeSetId === null) {
        activeSetId = p.activeSetId;
      }
    }
  } catch {
    // corrupted storage: fall through to defaults
  }
  const sets = [BUILTIN_CUBIC, ...stored.filter((s) => s.id !== BUILTIN_CUBIC_ID)];
  if (firstRun) {
    const d = DEFAULT_CURVES.cubic;
    const c = legacyCubicCurve;
    if (c.coeff !== d.coeff || c.exponent !== d.exponent) {
      const migrated: InfillPropertySet = {
        ...BUILTIN_CUBIC,
        id: crypto.randomUUID(),
        name: "Cubic (customized)",
        curve: { ...c },
        provenance: { author: "Migrated from the pre-§24 Settings curve" },
        origin: "user",
      };
      sets.push(migrated);
      activeSetId = migrated.id;
    }
  }
  if (activeSetId !== null && !sets.some((s) => s.id === activeSetId)) {
    activeSetId = BUILTIN_CUBIC_ID;
  }
  return { sets, activeSetId };
}

export function savePropsLibrary(sets: InfillPropertySet[], activeSetId: string | null): void {
  try {
    localStorage.setItem(
      LIBRARY_KEY,
      JSON.stringify({ sets: sets.filter((s) => s.origin !== "builtin"), activeSetId })
    );
  } catch {
    // storage full/blocked — the session keeps working, persistence degrades
  }
}
