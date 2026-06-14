<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com> -->

# InFEAll — Technical Documentation

Engineering documentation for the InFEAll structural-analysis engine, written for
**structural/mechanical engineers** who need to know what the tool computes, how,
and where its limits are — the same ground a commercial code covers in its theory
and verification manuals.

## Documents

| Document | What's in it |
|---|---|
| **[Theory & Engineering Reference](theory-manual.md)** | Governing equations, material models (isotropic elasticity, Gibson–Ashby infill, skin/wall composite, cut-cell occupancy, strength & safety factors), the HEX8 element, meshing, boundary conditions, the MGCG solver, analysis types, post-processing, and the consolidated **assumptions & limitations**. |
| **[Verification & Validation Manual](verification-manual.md)** | Every analytic/textbook case the engine is tested against (with formulas and tolerances), the meshing-convention benchmarks, the printed-composite checks, format golden files, the planned cross-code & physical validation, the **Standard Validation Battery** to run regularly, and the measured accuracy envelope. |

## Start here

- **"Can I trust this number?"** → Theory Manual
  [§12 Assumptions & Limitations](theory-manual.md#12-assumptions--limitations)
  and the Verification Manual
  [§10 Accuracy Envelope](verification-manual.md#10-measured-accuracy-envelope).
- **"What exactly does it solve?"** → Theory Manual
  [§3 Governing equations](theory-manual.md#3-governing-equations) and
  [§10 Analysis types](theory-manual.md#10-analysis-types).
- **"How do I re-check it works?"** → Verification Manual
  [§9 Standard Validation Battery](verification-manual.md#9-the-standard-validation-battery).

## Scope in one paragraph

InFEAll is a browser-based, voxel-discretized **linear-elastic static** FEA for
FDM parts. It homogenizes sparse infill and the printed shell into per-cell
effective stiffnesses, solves with a matrix-free multigrid CG, and reports
displacement, stress/strain, and FDM-specific (bulk + inter-layer) safety
factors — for a solid part, an as-printed part, or an infill-optimized part. It
is **not** a certified tool: no nonlinearity, dynamics, buckling, or thermal
analysis, and safety factors are advisory. Read the limitations before relying on
a result.

## Related project documents

- [`../DESIGN.md`](../DESIGN.md) — product design record (the *why*).
- [`../PHASE1_RESULTS.md`](../PHASE1_RESULTS.md) — core-engine spike results & first benchmarks.
- [`../README.md`](../README.md) — project overview, build, and run.

---

*Why in-repo Markdown rather than a GitHub wiki: these docs are versioned with the
engine, reviewed in the same pull requests as code changes, and can't silently
drift from the implementation. They can still be published to a wiki or docs site
later if desired.*
