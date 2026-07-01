# Modal Analysis (Verify tab) — Design Note

*Drafted 2026-06-30 via a "grill-me" design interview. Status: **feature design, pre-implementation**.
Constrained, undamped natural-frequency modal analysis added to the Verify tab. Validated against
an analytic cantilever in-repo; ANSYS cross-check deferred to the author. Unlike build-sim, modal
has clean analytic ground truth, so it is held to the existing analytic + CalculiX golden-test bar.*

## 0. One-paragraph summary

Add a **constrained, undamped modal analysis** to the Verify tab. The user picks a number of modes
(1–20, default 6); the solver returns the lowest N natural frequencies (Hz) + their mode shapes,
computed by **subspace inverse iteration (the robust LOBPCG cousin) reusing the existing
geometric-multigrid solve as the `K⁻¹` operator**, against a **lumped, density-scaled mass matrix**. Each mode is surfaced as an **output result-case** under a
new `kind: "modal"`, riding the existing `ResultEntry` / result-stash / viewer-switcher infra — so
mode switching, the deformed view, and animation come for free. Looking at a mode **auto-starts the
animation** (fixed visual rate, symmetric ± swing). The work reuses the static-FEA pipeline
end-to-end; the only genuinely new code is the eigensolver + lumped mass in `sig-core`.

## 1. Scope

| In | Out (deferred / rejected) |
|----|----|
| Constrained modal (uses Loads-step supports) | Exact mass-shift / inertia-relief free-free (see §11 — shipped the soft-anchor variant instead) |
| **Free-free modal** (unconstrained part, soft-anchored, rigid-body modes dropped — §11) | Damping (Rayleigh/ζ), forced/frequency response, transient |
| Undamped natural frequencies (Hz) + mode shapes | Modal stress in calibrated MPa (magnitude is arbitrary) |
| User-selected mode count (1–20, default 6) | Safety factor on a mode shape |
| Printed **or** solid, honoring the existing `analyzeMode` toggle | — |
| **Live progress + MGCG convergence trace** (§12) | — |
| Reuse static FEA pipeline + result/viewer infra | — |

## 2. Physics & method (decided)

- **Boundary conditions — constrained (Q1).** Solve `K v = λ M v` with the **existing fixed
  supports** applied (the part as mounted). Modal is force-free, so loads/forces are ignored; only
  supports/displacement constraints matter. Free-free is a clean follow-up (the inertia-relief /
  3-2-1 path from build-sim State-2 already exists) but is **out of scope**.
- **Undamped (Q2).** No damping model, no damping input. Light polymer damping (ζ≈1–5 %) barely
  moves the natural frequencies and changes no mode shape. Damped frequency-response is a separate
  feature if ever wanted.
- **Eigensolver — subspace inverse iteration + Rayleigh–Ritz (Q3).** The solver is matrix-free
  geometric-multigrid CG (`mg.rs`) — no assembled `K`, so a sparse eigensolver (ARPACK/LAPACK) is
  out; the method must be matrix-free too. **Implemented as subspace (block) inverse iteration with
  Rayleigh–Ritz** — the robust cousin of LOBPCG: each outer step solves `K X = M V` (one matrix-free
  MGCG solve per column, **reusing the public `MgSolver::solve_warm` as the multigrid inverse**),
  M-orthonormalizes `X`, and extracts Ritz pairs from the small `p×p` projected stiffness `Yᵀ K Y`
  (cyclic Jacobi). Inverse iteration maps the smallest eigenvalues to the dominant directions of
  `K⁻¹ M`, so the lowest frequencies converge first; `guard = clamp(N/2, 4, 8)` extra block columns
  absorb the slow tail. This reuses the multigrid even more directly than a hand-rolled LOBPCG
  preconditioner hook (no new V-cycle entry point needed) and is markedly easier to prove correct.
  Implemented in `crates/sig-core/src/modal.rs`. (LOBPCG and shift-invert Lanczos both considered;
  the inverse-subspace form won on reuse-of-the-public-API + robustness for the small `N` here.)
- **Mass matrix — lumped, density-scaled (Q3).** `m_node = ρ·(voxel volume)/8` summed per node,
  with `ρ` from the material `density` (g/cm³, already on every `Material` — `web/src/types.ts`)
  scaled by the **same per-cell `eps`/density field the stiffness uses**. Lumped → `M·v` is an
  element-wise multiply and `M⁻¹` is free, ideal for matrix-free LOBPCG. Consistent mass buys little
  for low modes and is deferred.
- **Printed vs solid (Q4).** Modal **honors the existing `analyzeMode` toggle** — no new control.
  Density scales **both** stiffness (`E(ρ)`, wired) **and** mass (lumped ρ-scaling), and the two
  partly cancel — a genuinely useful printed-vs-ideal comparison. Whatever the user selected is used.

## 3. Load-case interaction (decided — Q-followup)

- **Modal binds to the FIRST load case's supports.** Constrained modal needs exactly one support
  set, but a multi-LC project has several. Rule: modal always uses `effectiveBcs(bcs, loadSteps[0])`
  — the **first** load case's supports. Forces are ignored regardless (force-free eigenproblem).
- **Existing load-case results are never overwritten.** Modal lives under its own `kind: "modal"`
  with stash keys `modal::mode-{i}` — a separate namespace from `optimized::{stepId}` /
  `solid::{stepId}` / `asprinted::{stepId}`. A user who solved several load cases keeps every result;
  modal only *adds* mode-cases. The **`LoadStep` input matrix is never written.**
- **Constraint check reuse.** `runCheck()` for modal validates against the **same first-LC support
  set**. Constrained modal of an under-constrained part yields rigid-body (~0 Hz) modes — so modal
  reuses the **existing under-constraint guard** and refuses/warns with the same animated-RBM
  message, rather than returning six garbage ~0 Hz modes.

## 4. UI / placement (decided)

- **Analysis-type selector `Static | Modal` (Q5)** in the Verify panel, above the actions. Selecting
  *Modal* swaps the static "Run solve" for "Run modal" and reveals the mode-count input. The
  printed/solid `analyzeMode` toggle stays put and applies to both (orthogonal axis: *what analysis*
  vs *which stiffness*). `runCheck()` (voxelize + constraint validation) is shared.
- **Mode-count input (Q6).** Integer stepper, **range 1–20, default 6**. Hard cap at 20 to keep a
  verify click responsive (LOBPCG block cost grows ~linearly with N; highest-in-block modes converge
  worst). Hint that higher counts cost more time.

## 5. Results as output cases (decided — the reuse spine)

Each mode → one `ResultEntry` (`web/src/store.ts`) under `kind: "modal"`, with the mode as a
**synthetic result-step** (`loadStepName: "Mode 1 · 182 Hz"`). The existing per-step viewer selector
(`ViewportChips.tsx`) becomes the mode switcher; each mode's shape is stashed like a step solution;
the deformed view + animation apply unchanged. **This subsumes any bespoke Inspector table** — modes
ride the result switcher. Corrections, because a mode is an **output**, not an input:

1. **Not a real `LoadStep` (Q-followup).** `LoadStep[]` is the *input* matrix (forces/supports per
   case, `LoadSteps.tsx`). Modes must **not** appear there — they'd be fake force inputs. They live
   purely as `ResultEntry` outputs; the `LoadStep` input layer is bypassed.
2. **No envelope.** The "Envelope · worst case across steps" synthetic (`store.ts` `withEnvelope`)
   fires for any kind with ≥2 steps — meaningless across mode shapes. **Suppress envelope for
   `kind: "modal"`.**
3. **Units relabel.** `ResultEntry.maxDisplacement` is mm and the legend reads |u| in mm — but a mode
   shape is **mass-normalized, arbitrary scale**. For modal: the provenance **headline is frequency
   (Hz)**, not max-deflection-mm; the **displacement legend reads "mode shape (normalized)"**, not mm.

## 6. Animation (decided — Q7)

Reuse the existing `deformAnimate` loop (`SceneManager.ts` `tick()`):

1. **Fixed visual rate, not physical Hz.** A 180 Hz mode animated at 180 Hz is an invisible blur;
   standard FEA viewers animate every mode at a constant visual speed and *print* the frequency. The
   existing loop already runs on a fixed 2.4 s period — used as-is. Frequency shown as a number.
2. **Symmetric ± swing.** Static uses a one-sided `0 → max → 0` (`0.5−0.5·cos`) loop; a mode shape
   oscillates **+A → 0 → −A → 0 → +A**. Add a symmetric ±amplitude (`sin`) variant for modal.
3. **Auto-start on view.** Selecting a mode sets `animateDeformed = true` automatically (user can
   still ■ Stop). After a modal run completes, **auto-select Mode 1** → a shape is loaded and
   animating with zero clicks. This is the "when looking at modal results, start the animation"
   requirement.

## 7. Mode-shape scaling & colored fields (decided)

- **Per-mode normalization (Q10).** LOBPCG returns mass-normalized vectors (`vᵀMv = 1`, arbitrary
  magnitude). The existing deform path already auto-normalizes by field peak
  (`autoScale = 0.08·diag/maxDisp`, `Viewer.tsx`), so each mode shows at a consistent visual swing
  for free, with `deformScale` as the exaggeration knob. Each mode is normalized **independently**
  (absolute mode amplitude is meaningless). No new viewer normalization code; no physical mm assigned
  to a mode.
- **Fields (Q11).** Modal field picker = **{|u|, ux, uy, uz, σ_vm, σxx…, strains}** — keep
  displacement, stress, **and** strain (the *location/pattern* of stress-strain concentration in a
  mode is useful regardless of magnitude — where it'll fail when driven). **Hide safety factor**
  (inherently calibrated/absolute, meaningless on a normalized mode). **Stress/strain legend reads
  "relative"** (no calibrated MPa axis) so colors/locations are trusted but nobody misreads a number.

## 8. Validation (decided — Q9)

- **Analytic golden test.** Voxelized cantilever beam; assert the first 3–4 LOBPCG frequencies match
  Euler-Bernoulli/Timoshenko theory within a band. Most-likely-bug catcher: a wrong mass scaling
  makes every frequency off by a constant `√` factor.
- **Cross-resolution gate.** Coarse vs fine voxels must give the same frequencies within band — same
  voxel-independence release gate build-sim §4.3 insists on.
- **External.** Author will cross-check against **ANSYS**. (CalculiX also does modal if an automated
  second opinion is ever wanted.)

## 9. Persistence (decided — Q12)

**Option B — frequencies saved, shapes recomputed lazily.** Saving N full mode-shape fields
(`numModes × 3 × nNodes`) into every `.infeall` is bloat. Save the **frequencies** (tiny) + the
**mode-count config**; recompute mode shapes on demand when a modal result is reopened. Mode shapes
are fully determined by mesh+BCs — nothing is lost by recomputing. Wire the existing staleness-epoch
system so changing the mode count or the first-LC supports marks modal results stale (same as static).

## 10. Code touch points (reuse-first)

- **`crates/sig-core/`** — NEW `modal.rs`: assemble lumped density-scaled mass; LOBPCG with the
  `mg.rs` V-cycle as preconditioner; return frequencies (rad/s → Hz at the UI) + mode-shape vectors.
  Add a lumped element-mass helper in `fem.rs`. Reuse `mg.rs`, `solve.rs`, `attach.rs` (BCs),
  `stress.rs` (relative stress/strain recovery on a mode shape) wholesale. Add `pub mod modal;` to
  `lib.rs`.
- **`crates/sig-wasm/src/lib.rs`** — new `modal_analysis(opts_json)` entry (mode count, printed flag,
  perimeters/line-width like the others); returns frequencies + transferred mode-shape buffers.
- **`web/src/worker/engine.worker.ts`** — `"modalAnalysis"` request variant + handler; stash each
  mode shape keyed `modal::mode-{i}`.
- **`web/src/engine/EngineClient.ts`** — `modalAnalysis(opts)` method.
- **`web/src/store.ts`** — `analysisType: "static" | "modal"`, `modalModeCount`, modal `ResultKind`,
  first-LC binding, envelope suppression for modal, staleness epochs, Hz-headline provenance.
- **`web/src/ui/StepPanel.tsx`** — `Static | Modal` selector + mode-count stepper in `StepVerify()`.
- **`web/src/viewer/SceneManager.ts`** — symmetric ± swing variant in `tick()`; auto-animate on
  modal-mode select.
- **`web/src/types.ts`** — restrict modal field set; "relative" legend for modal stress/strain.

## 11. Free-free modal (unconstrained part) — added post-Q1

Q1 picked constrained modal and deferred free-free. Added on request: a **"Unconstrained (free-free)"**
checkbox (Verify panel, modal only). An unsupported part has a **singular `K`** (6 rigid-body modes
at λ = 0), which inverse iteration cannot invert.

- **Method — soft anchor springs (`sig_core::modal::rigid_body_anchor_springs`).** Weak isotropic
  ground springs (`k ≈ 1e-4·E·h`) at the ± extreme active node of each axis lift the 6 rigid-body
  modes to low-but-nonzero frequencies so `K` becomes SPD. **Reuses the existing spring machinery**,
  which already coarsens correctly through the multigrid hierarchy — *zero* `mg.rs` surgery. (An exact
  mass-shift `K+σM` or deflation would be more accurate but needs invasive solver changes; deferred.)
- **Filtering.** Request `num_modes + 6`, drop the 6 lowest (the lifted rigid-body modes) in the wasm
  layer, return the flexible ones. The under-constraint gate is skipped in this mode.
- **Status: indicative.** The soft anchors slightly perturb the flexible frequencies (weak `k` keeps
  it small); labeled in the UI as indicative, validate against FEA. Golden test
  `free_free_beam_filters_rigid_body` asserts the first flexible mode separates cleanly from the 6
  rigid-body modes and matches the free-free Euler–Bernoulli bending frequency (βL = 4.730) in band.

## 12a. Performance — LOBPCG (the 30-minute → seconds rewrite)

The first cut (subspace **inverse iteration**) ran a `K X = M V` solve per column. Even made
inexact, on a clustered/slender part (a pipe's degenerate bending pair) the subspace step is weak,
so it took ~50 outer iterations × `p` columns × several V-cycles = **~3800 V-cycles → ~10 min** on a
258k-cell pipe (and the first, fully-converged, version was ~30 min).

Rewrote the eigensolver as **LOBPCG** (`crates/sig-core/src/modal.rs`) — block, preconditioned by a
**single multigrid V-cycle** per mode per iteration (`MgSolver::precondition`, a public one-V-cycle
entry added to `mg.rs`; `apply_k` exposes the matrix-free `K`). Its conjugate search-direction block
`P` converges clustered modes fast, and there is no inner solve at all. Two further keys:

- **Rayleigh–Ritz over `[X | W | P]`** each iteration (`W` = preconditioned residual), with the
  basis M-orthonormalized and **rank-deficient columns dropped** (`m_orthonormalize_drop`).
- **Eigenvalue-stabilization convergence.** The eigenVALUES (the frequencies the user wants)
  converge well before the eigenVECTOR residual — especially when the multigrid preconditioner is
  weak (slender/thin parts, where the residual can plateau above tol). So the loop stops when the
  requested frequencies stop moving (`EIG_TOL = 1e-5`), not only on a tiny residual. Without this,
  the fine cantilever ran the full 100-iteration cap; with it, **5 iterations**.

Measured (golden tests): every case — 640 / 5120 cells, constrained and free-free —
**converges in ~5 iterations / ~25 V-cycles**; the suite went 75 s → **4.6 s**. Expected on the
258k-cell pipe: ~10-20 iterations → **~100-200 V-cycles (tens of seconds, ~20-40× faster).**
`ModalResult.total_inner_iters` (the V-cycle count) is surfaced to the nerd log.

*Caveat:* free-free's soft-anchored `K` is near-singular, so each V-cycle's coarse solve is
expensive; the low iteration count keeps it usable, but a large free-free part is still the slowest
path. Stiffer anchors (less accurate) or a mass-shift (more solver work) are the levers if needed.

## 12b. Profiling (pipe.stl, lower face fixed, 6 modes) — the memory-bound wall

Even after LOBPCG, a 258k-cell / 50k-solid pipe took minutes. A native profiling harness
(`pipe_modal_profile`, `#[ignore]`) broke it down and found the cost was **NOT the multigrid**:

- **Ops are memory-bandwidth-bound, not compute-bound.** A K-apply is ~1.9ms whether run 1-thread
  or 8 (`--features parallel` barely moved it). So threading and micro-optimizing don't help; only
  cutting *memory traffic* and *iteration count* do.
- **The scalar Rayleigh–Ritz projection dominated** — building `SᵀKS`, the new `X/KX/P` blocks, and
  Gram–Schmidt over the **full 813k-length** vectors was ~160s of a ~185s run (the ~680 V-cycles
  were only ~20s).
- **The vectors are ~75% zeros** (constrained/void DOFs), streamed for nothing.

Three fixes, in order of impact:

1. **Compact to free DOFs.** The LOBPCG block vectors now live on the `nf` free DOFs (`free_idx`,
   `scatter`/`gather`); the full node grid is materialized only for `K·x` and the preconditioner
   (which need the multigrid's layout). ~3-4× less traffic on the dominant scalar work.
2. **Minimize *total* V-cycles, not iterations.** In the browser a V-cycle is ~5× a native one
   (~170ms), so total V-cycles = `iters × p × precond_cycles` is the metric that matters. A *stronger*
   preconditioner (more V-cycles/iter) cut iterations but raised the V-cycle count — worse in the
   browser. So the preconditioner is a **single V-cycle** (`PRECOND_CYCLES = 1`), which uses the
   fewest total V-cycles; compaction makes the resulting extra iterations cheap. (`solve_warm(_, 1)`
   as the preconditioner *stalled* — use the raw `precondition` V-cycle.)
3. **Eigenvalue-stabilization stop** (§12a) — the pipe's frequencies are stable long before the
   residual, so it exits at ~68 iterations instead of the 100-cap.

Result on the pipe: **~185s → ~57s native, 612 V-cycles (6× fewer than the old inverse iteration's
3765), correct frequencies.** Remaining headroom (not done): carry `KX`/`KP` instead of recomputing
`K·basis`; parallelize the compact projection; soft-lock converged modes to shrink the block.

## 12. Progress + convergence readout — added post-ship

Modal is many MGCG solves (`p × outer`), so it is slow. Two indicators, both reusing existing infra:

- **Live MGCG convergence trace** — `runModal` starts the same residual poll the static solve uses,
  so the "nerd" convergence plot animates during the inner solves.
- **Per-outer-iteration progress** — `modal::analyze` takes an `on_progress(outer, max_outer,
  freqs_hz)` callback (plumbed wasm → worker → `EngineClient` like the build-sim layer callback); the
  busy line shows `Modal — iteration k/N · f1 ≈ … Hz` with the live Ritz estimates.

---

*Build the constrained undamped LOBPCG solver first and gate it on the cantilever analytic + the
cross-resolution test before trusting a number. Reuse the `ResultEntry`/viewer-switcher spine — do
not invent a parallel modal-results UI, and do not write modes into the `LoadStep` input matrix.*
