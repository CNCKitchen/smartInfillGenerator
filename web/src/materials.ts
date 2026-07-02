// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Physics-derived Build Sim shrink.
//
// The warp eigenstrain is the thermal contraction accumulated from the
// temperature where the material stops relaxing ("locks") down to room
// temperature: shrink = CTE · (tLock − tFinal). Locking happens at the glass
// transition Tg for amorphous polymers (Hepler & Davids 2024; Lukhi 2025) and
// near the crystallization temperature Tc for semi-crystalline ones. The CTE
// values are EFFECTIVE printed-part values that fold raster/air-gap effects
// into one coefficient — ≈96e-6/°C for FDM PLA and ≈88e-6/°C for ABS per
// Materials 17:4668 (2024).
//
// The thermal data (tLock, cte, cteZ) lives on the ONE material library entry
// (`Material` in types.ts) — the same material the structural steps use.
// Materials without it fall back to the raw `shrink`/`shrinkZ` legacy path.

import type { Material } from "./types";

/** Room temperature (°C) the part cools to after removal — the ladder's tFinal. */
export const ROOM_TEMP_C = 20;

/** Physics-derived eigenstrain shrinks (negative = shrink) for lock → tFinal,
 *  or null when the material carries no thermal data (→ legacy raw-shrink path). */
export function shrinkFromPhysics(m: Material, tFinal: number): { shrink: number; shrinkZ: number } | null {
  if (m.tLock == null || m.cte == null) return null;
  return {
    shrink: -m.cte * (m.tLock - tFinal),
    shrinkZ: -(m.cteZ ?? m.cte) * (m.tLock - tFinal),
  };
}
