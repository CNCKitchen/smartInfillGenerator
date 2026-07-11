<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com> -->

# Implementation brief — Acceleration loads & remote point masses (DESIGN.md §16)

Spec interviewed and locked 2026-07-09. **DESIGN.md §16 is the source of truth**; this file
is the session hand-off: a paste-ready prompt for milestone 1+2 (the shippable deformable
feature), starter prompts for milestones 3 and 4, and the code anchors verified during the
interview so the implementing session doesn't re-discover them.

---

## Prompt — Milestones 1 + 2 (engine + UI, deformable, ships the feature)

Paste into a fresh session:

```
Implement DESIGN.md §16 (acceleration loads + remote point masses), build items (1) engine
and (2) data model + UI — the deformable path end-to-end. Read DESIGN.md §16 fully first;
it is the source of truth and every decision in its table is final (interviewed 2026-07-09).
Also skim §13 (the load-steps override rails you'll ride) and §14 (the regbench beam suite
you'll extend). docs/accel-mass-implementation-brief.md has verified code anchors — use
them instead of re-searching.

Scope guard — NOT in this session: rigid behavior (schema field `behavior` exists from day
one, but only "deformable" is implemented and the UI toggle is hidden/disabled), rotational
loads, the drag gizmo, optimizer inclusion of accel steps (force their includeInOptimize
off with an advisory note in the steps table — §16 build item 2 INTERIM).

Engine work (filasim-core / filasim-wasm):
- New BcKind::Mass { point: [f64;3], mass: f64 } (tonnes, mm). In assemble(), realize it
  per the current accel vector a as nodal forces: distributed F = m·a (force-style area
  weighting) + transported couple M = (p − c) × F via the existing moment_forces about the
  patch area-weighted centroid c. Zero active accel ⇒ zero contribution.
- Replace the uniform-density gravity body force with per-cell volume-fraction scaling:
  f_cell = ρ_mat · volfrac_e · a · h³ lumped to the cell's 8 nodes, correct in all three
  model states (uniform / optimized / printed — reuse the field the mass readouts already
  composite; find it, don't re-derive it). Self-weight is ALWAYS on when an accel is
  active (§16 dec. 3) — no flag.
- Per-step accel: the worker resolves each step's active accel entities to ONE summed
  vector and passes it through the existing assemble() gravity parameter; extend the wasm
  surface as needed (set_gravity's hardcoded vector goes away; keep the export
  backward-compatible or migrate its 3 call sites in web/).
- RBM/mechanism check: body forces and mass loads must count toward per-component
  hasLoads/load_nodes (§16 dec. 11) — an unconstrained island under acceleration must fail
  the check.
- regbench additions (§14 cantilever): (a) lever-arm analytic — tip mass m at offset r
  must match a hand-composed tip force mg + moment mgr to solver tolerance; (b)
  self-weight cantilever — 1g, no dummy mass, vs the q·L⁴/8EI band. All existing
  no-accel metrics must stay byte-identical (regbench --check against a fresh baseline).
- Theory manual: short section on inertial loads + the remote-mass couple transport.

Web work (types/store/worker/UI):
- Kinds "accel" (selection-less — tris becomes kind-dependent; sweep EVERY bc.tris-touching
  path with kind guards: attach validation, viewer highlight, steps table, signatures) and
  "mass" ({tris, massGrams, point, behavior}) per §16 dec. 5. Per-step overrides: accel =
  {active, accel vector}; mass = {active} only. Multiple accels sum per step.
- Units: new "acceleration" quantity kind in units.ts, display g (default) / m/s²; mass in
  g. Internal conversions: g₀ = 9810 mm/s²; g → tonne ×1e-6; g/cm³ → tonne/mm³ ×1e-9.
- Panel: accel entity = direction+magnitude dual mode (force pattern) + "1g ↓" preset +
  the one-line convention statement "Every mass feels F = m·a along this vector."; mass =
  XYZ DRO fields initialized at the selected surface's area-weighted CG, live |F| and |M|
  readout for the shown step. Mass point must transform WITH the part on Model-step
  rotations; accel vectors stay world-fixed like forces (§16 dec. 8).
- Viewport: mass = sphere at point + spider lines to patch + name/mass label; accel = one
  labeled arrow at the part bbox centroid in roster color, shown when active in the
  displayed step.
- Preflight (advisory, never blocking): mass with no step having any active accel.
- Persistence: PROJECT_SCHEMA stays 1, strictly additive; old projects load unchanged and
  accel-free projects round-trip byte-identical. Setup panel gets the informational
  "attached masses: N g" line; dummy masses stay OUT of every part-mass metric.

Gates before you call it done: cargo test; regbench --check (existing metrics byte-identical,
new cases green); npm run wasm THEN the wasm smoke test (it loads the built bundle — add
smoke coverage: mass lever-arm solve + accel-step solve + schema round-trip); node
scripts/preflight.mjs. UI follows the Werkbank theme (styles.css tokens, DRO readouts — no
generic styling). New files get SPDX AGPL-3.0-only headers; no new dependencies without
license vetting. Then a manual sanity pass: load hook5 v3.stl, fix the top, add a 100 g
mass on the hook face with the point offset 40 mm outward, add Gravity 1g↓, solve — the
stress pattern must show bending from the lever arm, and deleting the mass point offset
(point = patch CG) must reproduce a plain distributed-weight result.
```

## Starter prompt — Milestone 3 (optimizer)

```
Implement DESIGN.md §16 build item (3): acceleration steps in the optimizer per dec. 4 —
recompute the body force from the current density field every SIMP iteration, standard
compliance sensitivity, NO load-derivative term. Remove the milestone-2 interim exclusion
of accel steps from includeInOptimize. Comparison card: each design carries its own true
weight + fine-print note (dec. 10). Add the optimizer self-weight regbench case; single-load
and no-accel optimize paths must stay regbench-byte-identical.
```

## Starter prompt — Milestone 4 (rigid behavior)

```
Implement DESIGN.md §16 build item (4): rigid remote-mass behavior — penalty spider from
patch nodes to a 6-DOF virtual master at the mass point, master statically condensed (free
6×6 block) so the operator gains per-node 3×3 diagonal blocks + one rank-6 coupling term
per rigid mass. Thread through the SIMD/threaded matvec, MG preconditioner pass-through,
RBM check, and optimizer solves. Retire the convergence risk: penalty scaling vs the
Chebyshev smoother, validated at 1M cells. Enable the per-mass behavior toggle in the UI
("Deformable (load only) / Rigid (stiffens the mounting face)").
```

## Verified code anchors (2026-07-09)

Engine (`crates/filasim-core`, `crates/filasim-wasm` — note: crates renamed from `sig-*`):

- `filasim-core/src/attach.rs` — `BcKind` enum + `BcSpec` (~line 50), `assemble()`
  signature with the `gravity: Option<([f64;3], f64)>` parameter (~102), per-kind force
  realization incl. `BcKind::Moment` (~262), the uniform-density gravity block to replace
  (~275–297: note it checks `grid.scale > 0` for occupancy but does NOT scale the force —
  the bug §16 dec. 3 fixes), `moment_forces` deformable-couple math (~565),
  `sample_selection` area-weighted routing (~304). Gravity forces are NOT pushed to
  `load_nodes` (dec. 11 work item).
- `filasim-core/src/bin/regbench.rs` — the harness; §14 beam suite lives here. Run:
  `cargo run --release -p filasim-core --bin regbench -- --check <baseline>`.
- `filasim-core/tests/phase2.rs` — existing `gravity_self_weight_cantilever` test (~287).
- `filasim-wasm/src/lib.rs` — `GRAVITY_MM_S2` hardcoded const (35), `gravity_on` field
  (~656), `set_gravity` (~1156), `gravity_arg()` (~1414), and ~8 `assemble(...)` call
  sites that all take `self.gravity_arg()`.

Web (`web/src`):

- `types.ts` — `BcKind` union (line 4), `Bc` interface (30), `LoadStepOverride` (83),
  `LoadStep` (99), `Material.density` g/cm³ (168), `DEFAULT_MATERIALS` (209).
- `store.ts` — BC kind union duplicate (~734), BC signature hash (~335), `addBc` defaults
  (~2856), force auto-direction pattern (~2946), schema load + re-id (~2610, ~2623).
- `worker/engine.worker.ts` — BC translation to wasm (~187), `setGravity` op (~142).
- `engine/EngineProtocol.ts` (~55, ~182) + `engine/EngineClient.ts` (~158) — `setGravity`
  plumbing to repurpose.
- `units.ts` — quantity kinds (~25); add "acceleration" (g default, m/s²) and mass units.
- `ui/bcmeta.ts` — per-kind display metadata (~57).
- `ui/LoadSteps.tsx` — the steps table (per-step active/value overrides, include/weight).
- `ui/StepPanel.tsx` — station panels (BC editing lives here); `viewer/SceneManager.ts` —
  BC glyph drawing (force arrows ~809).
- Convention note "Loads keep their world directions" — Model-step panel (DESIGN.md line
  ~149); §15 build item 4 records the Apply-flow exception.
