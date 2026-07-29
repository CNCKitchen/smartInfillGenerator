// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! The bundled sample model: the CNC Kitchen Hook, loaded on startup so
//! an empty filaSim greets people with something to poke at instead of a bare
//! drop zone. The assets live in public/sample/ (original STEP bytes + the
//! pre-tessellated mesh cache built by `npm run sample:mesh`), and the canned
//! loads below bind to CAD-FACE ENTITY IDS — the identity that is stable for
//! these exact file bytes (DESIGN §18 dec. 5), so they survive meshStep
//! upgrades (a live re-import re-derives the triangles from the same ids).
//!
//! The setup mirrors how the hook is actually used: a strap through the big
//! slot bears on its far inner wall (elastic foundation — a strap, not a
//! bolted joint), and 250 N pulls at the small hook's inner eye, straight
//! away from the slot. Directions are in the IMPORT frame; the fresh-import
//! seating only translates, so they hold in world space too.

import type { Bc } from "./types";

export const SAMPLE_FILE = "CNCKTestHook.step";
export const SAMPLE_MESH = "CNCKTestHook.mesh";

/** Where the sample assets are served from (BASE_URL always ends in "/"). */
export const sampleUrl = (file: string) => `${import.meta.env.BASE_URL}sample/${file}`;

export const SAMPLE_NOTICE =
  "Sample part loaded — the CNC Kitchen Hook with its loads already set up. " +
  "Try Analyze, or drop your own STL/3MF/STEP to replace it.";

/** One canned BC: everything but the session id and the derived triangles. */
export interface SampleBcSpec {
  /** Whole-CAD-face selection, STEP entity record numbers. */
  faceIds: number[];
  bc: Omit<Bc, "id" | "tris">;
}

/** Face 1787 = the slot's far inner wall (plane at y=62.5, normal −Y, facing
 *  the small hook); face 1826 = the slim flat at the bottom of the small
 *  hook's eye (plane, normal +Y, between the big eye cylinder and the
 *  bspline sector) — the purpose-built bearing surface the −Y pull presses
 *  square onto. Entity ids from `npm run sample:mesh -- --faces`. */
export const SAMPLE_BCS: SampleBcSpec[] = [
  {
    faceIds: [1787],
    bc: {
      kind: "elastic",
      name: "Elastic 1",
      // Webbing/strap bedding — the addBc printed-plastic default.
      stiffness: 100,
    },
  },
  {
    faceIds: [1826],
    bc: {
      kind: "force",
      name: "Force 1",
      force: [0, -250, 0],
      forceMode: "direction",
      forceDir: [0, -1, 0],
      forceMag: 250,
      forceDirAuto: false,
    },
  },
];
