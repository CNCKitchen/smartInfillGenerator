// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Physics-derived Build Sim material presets.
//
// The warp eigenstrain is the thermal contraction accumulated from the
// temperature where the material stops relaxing ("locks") down to room
// temperature: shrink = CTE · (tLock − tFinal). Locking happens at the glass
// transition Tg for amorphous polymers (Hepler & Davids 2024; Lukhi 2025) and
// near the crystallization temperature Tc for semi-crystalline ones. The CTE
// values are EFFECTIVE printed-part values that fold raster/air-gap effects
// into one coefficient — ≈96e-6/°C for FDM PLA and ≈88e-6/°C for ABS per
// Materials 17:4668 (2024).

export interface BuildMaterial {
  /** Stable preset id — "custom" is the raw-shrink legacy sentinel. */
  id: string;
  label: string;
  /** Locking temperature in °C: Tg (amorphous) / Tc (semi-crystalline). */
  tLock: number;
  /** Effective printed-part CTE in 1/°C (in-plane). */
  cte: number;
  /** Through-layer (Z) CTE in 1/°C; omitted = isotropic (= cte). */
  cteZ?: number;
  /** Typical bed temperature in °C (filled in when the preset is picked). */
  defaultBed: number;
  /** Typical chamber/ambient temperature in °C. */
  defaultChamber: number;
  semiCrystalline?: boolean;
  note?: string;
}

export const BUILD_MATERIALS: BuildMaterial[] = [
  { id: "pla", label: "PLA", tLock: 60, cte: 96e-6, defaultBed: 60, defaultChamber: 25 },
  { id: "petg", label: "PETG", tLock: 80, cte: 68e-6, defaultBed: 70, defaultChamber: 25 },
  { id: "abs", label: "ABS", tLock: 100, cte: 88e-6, defaultBed: 100, defaultChamber: 35 },
  { id: "asa", label: "ASA", tLock: 100, cte: 90e-6, defaultBed: 100, defaultChamber: 35 },
  { id: "pc", label: "PC", tLock: 145, cte: 68e-6, defaultBed: 110, defaultChamber: 40 },
  {
    id: "custom",
    label: "Custom (raw shrink)",
    tLock: 0,
    cte: 0,
    defaultBed: 0,
    defaultChamber: 0,
    note: "Raw material shrink, no temperature ladder (legacy path).",
  },
];

/** Room temperature (°C) the part cools to after removal — the ladder's tFinal. */
export const ROOM_TEMP_C = 20;

/** Preset for `id`, or undefined for "custom"/unknown (→ legacy raw-shrink path). */
export function getBuildMaterial(id: string): BuildMaterial | undefined {
  return id === "custom" ? undefined : BUILD_MATERIALS.find((m) => m.id === id);
}

/** Physics-derived eigenstrain shrinks (negative = shrink) for lock → tFinal. */
export function shrinkFromPhysics(m: BuildMaterial, tFinal: number): { shrink: number; shrinkZ: number } {
  return {
    shrink: -m.cte * (m.tLock - tFinal),
    shrinkZ: -(m.cteZ ?? m.cte) * (m.tLock - tFinal),
  };
}
