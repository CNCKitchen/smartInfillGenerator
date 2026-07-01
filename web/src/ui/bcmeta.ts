// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Shared BC presentation metadata — used by the Loads panel and the Load-steps
// manager modal so their rows always match each other and the 3D glyphs.

import type { Bc, BcKind } from "../types";
import type { QuantityKind } from "../units";

export const KIND_LABEL: Record<BcKind, string> = {
  fixed: "Fixed support",
  frictionless: "Frictionless support",
  displacement: "Displacement support",
  elastic: "Elastic support",
  force: "Force",
  pressure: "Pressure",
  bearing: "Bearing load",
  moment: "Moment",
};

// Mirrors BC_COLORS in SceneManager — the dots must match the 3D glyphs.
export const KIND_DOT: Record<BcKind, string> = {
  fixed: "#2563eb",
  frictionless: "#0e9cbf",
  displacement: "#7c3aed",
  elastic: "#1f9d6b",
  force: "#d93025",
  pressure: "#c97b10",
  bearing: "#b5179e",
  moment: "#e8590c",
};

export const SUPPORT_KINDS: BcKind[] = ["fixed", "elastic", "frictionless", "displacement"];

/** Short kind label for auto-generated names + compact table columns. */
export const KIND_SHORT: Record<BcKind, string> = {
  fixed: "Fixed",
  frictionless: "Frictionless",
  displacement: "Displacement",
  elastic: "Elastic",
  force: "Force",
  pressure: "Pressure",
  bearing: "Bearing",
  moment: "Moment",
};

/** A load's per-step unit. Canonical labels — for live display use
 *  `unitLabel(BC_QUANTITY[kind])` so it follows the unit selection. */
export const KIND_UNIT: Partial<Record<BcKind, string>> = {
  force: "N",
  pressure: "MPa",
  bearing: "N",
  moment: "N·mm",
};

/** The display-unit quantity kind of a load's value (force/pressure/moment).
 *  Drives unit-aware inputs + headers in the loads UI. */
export const BC_QUANTITY: Partial<Record<BcKind, QuantityKind>> = {
  force: "force",
  bearing: "force",
  pressure: "pressure",
  moment: "moment",
};

/** Display name: the user's custom name, or the auto kind label. */
export function bcLabel(bc: Pick<Bc, "kind" | "name">): string {
  return bc.name && bc.name.trim() ? bc.name : KIND_LABEL[bc.kind];
}
