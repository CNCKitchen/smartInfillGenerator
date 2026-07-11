// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Shared BC presentation metadata — used by the Loads panel and the Load-steps
// manager modal so their rows always match each other and the 3D glyphs.

import type { Bc, BcKind } from "../types";
import type { QuantityKind } from "../units";
import type { HelpContent } from "./HelpTip";

export const KIND_LABEL: Record<BcKind, string> = {
  fixed: "Fixed support",
  frictionless: "Frictionless support",
  displacement: "Displacement support",
  elastic: "Elastic support",
  force: "Force",
  pressure: "Pressure",
  bearing: "Bearing load",
  moment: "Moment",
  accel: "Acceleration",
  mass: "Point mass",
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
  accel: "#b08900",
  mass: "#0f766e", // teal — must match BC_COLORS.mass in ColorManager.ts
};

export const SUPPORT_KINDS: BcKind[] = ["fixed", "elastic", "frictionless", "displacement"];

/** Loads applied to a triangle SELECTION (as opposed to the selection-less
 *  "accel"). Drives the "needs a surface" attach validation + glyph anchoring. */
export const SELECTION_LESS_KINDS: BcKind[] = ["accel"];

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
  accel: "Accel",
  mass: "Mass",
};

/** A load's per-step unit. Canonical labels — for live display use
 *  `unitLabel(BC_QUANTITY[kind])` so it follows the unit selection. */
export const KIND_UNIT: Partial<Record<BcKind, string>> = {
  force: "N",
  pressure: "MPa",
  bearing: "N",
  moment: "N·mm",
  accel: "g",
};

/** The display-unit quantity kind of a load's value (force/pressure/moment).
 *  Drives unit-aware inputs + headers in the loads UI. */
export const BC_QUANTITY: Partial<Record<BcKind, QuantityKind>> = {
  force: "force",
  bearing: "force",
  pressure: "pressure",
  moment: "moment",
  accel: "acceleration",
};

/** Display name: the user's custom name, or the auto kind label. */
export function bcLabel(bc: Pick<Bc, "kind" | "name">): string {
  return bc.name && bc.name.trim() ? bc.name : KIND_LABEL[bc.kind];
}

/** Hover-card help for the add-BC buttons (see HelpTip). Text only for now —
 *  HelpContent has an `img` slot for detailed illustrated instructions. */
export const BC_HELP: Record<BcKind, HelpContent> = {
  fixed: {
    title: "Fixed support",
    text: [
      "Holds the selected surfaces completely still — no movement in any direction.",
      "The stiffest support there is. Use it where the part is bolted, clamped or glued down. Expect stress concentrations at the support edges — that stiffness is rarely physical.",
    ],
  },
  elastic: {
    title: "Elastic support",
    text: [
      "A springy foundation on the selected surfaces: every point gets a spring proportional to its area, set by the bedding modulus k (N/mm³).",
      "Use it for gaskets, rubber feet, soft mounts — anywhere a Fixed support would be unrealistically rigid and spike the stress at its edges.",
    ],
  },
  frictionless: {
    title: "Frictionless support",
    text: [
      "Blocks motion perpendicular to the selected surface but lets it slide freely in-plane — like resting on a perfectly slippery plate.",
      "Also the standard way to model a symmetry plane on a half model.",
    ],
  },
  displacement: {
    title: "Displacement support",
    text: [
      "Prescribes how far the selected surfaces move along the global axes you enable — 0 mm pins that axis, a non-zero value enforces a motion.",
      "Axes you leave unchecked stay completely free, so it doubles as a per-axis pin.",
    ],
  },
  force: {
    title: "Force",
    text: [
      "A total force applied to the selected surfaces, distributed over their area.",
      "Enter it as X/Y/Z components or as a direction plus magnitude — the direction follows the surface normal until you set your own.",
    ],
  },
  moment: {
    title: "Moment",
    text: [
      "A torque about an axis (right-hand rule), applied to the selected surface as a distributed couple — the surface stays deformable, nothing is rigidly clamped.",
      "Enter it as X/Y/Z components or as an axis plus magnitude.",
    ],
  },
  bearing: {
    title: "Bearing load",
    text: [
      "The load a pin, bolt or shaft presses onto the wall of a hole. Select a cylindrical surface — the fit is checked automatically.",
      "The force is spread over the loaded half of the bore with a cosine distribution, like a real pin contact. Any component along the bore axis is ignored.",
    ],
  },
  pressure: {
    title: "Pressure",
    text: [
      "A uniform pressure acting perpendicular to the selected surfaces — positive pushes onto the surface.",
      "On curved selections every spot is loaded along its own local normal, so a closed vessel under internal pressure comes out right.",
    ],
  },
  accel: {
    title: "Acceleration",
    text: [
      "An acceleration the whole part feels — gravity, a crash, a maneuver. Every bit of mass, plus any attached point masses, is loaded by F = m·a along this vector.",
      "Use the 1 g ↓ preset for standing weight, or enter a direction + magnitude (e.g. 6 g sideways). No surface to select — it acts on the entire part; part self-weight turns on automatically.",
    ],
  },
  mass: {
    title: "Point mass",
    text: [
      "A component bolted to the selected surface — a motor, battery or camera — whose weight loads the part. Enter its mass and the position of its centre of gravity.",
      "Under an acceleration the offset CG loads the mount in bending, not just compression (the lever arm a smeared surface load would miss). Add an Acceleration too, or the mass does nothing.",
    ],
  },
};
