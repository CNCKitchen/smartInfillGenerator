// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Shared BC presentation metadata — used by the Loads panel and the Load-steps
// manager modal so their rows always match each other and the 3D glyphs.

import type { Bc, BcKind } from "../types";

export const KIND_LABEL: Record<BcKind, string> = {
  fixed: "Fixed support",
  frictionless: "Frictionless support",
  displacement: "Displacement support",
  elastic: "Elastic support",
  force: "Force",
  pressure: "Pressure",
};

// Mirrors BC_COLORS in SceneManager — the dots must match the 3D glyphs.
export const KIND_DOT: Record<BcKind, string> = {
  fixed: "#2563eb",
  frictionless: "#0e9cbf",
  displacement: "#7c3aed",
  elastic: "#1f9d6b",
  force: "#d93025",
  pressure: "#c97b10",
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
};

/** A force / pressure's per-step unit. */
export const KIND_UNIT: Partial<Record<BcKind, string>> = {
  force: "N",
  pressure: "MPa",
};

/** Display name: the user's custom name, or the auto kind label. */
export function bcLabel(bc: Pick<Bc, "kind" | "name">): string {
  return bc.name && bc.name.trim() ? bc.name : KIND_LABEL[bc.kind];
}
