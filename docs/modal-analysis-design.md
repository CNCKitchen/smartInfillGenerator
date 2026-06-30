# Modal Analysis (Verify tab) — Design Note

*Drafted 2026-06-30 via a "grill-me" design interview. Status: **feature design, pre-implementation**.
Constrained, undamped natural-frequency modal analysis added to the Verify tab. Validated against
an analytic cantilever in-repo; ANSYS cross-check deferred to the author. Unlike build-sim, modal
has clean analytic ground truth, so it is held to the existing analytic + CalculiX golden-test bar.*

## 0. One-paragraph summary

Add a **constrained, undamped modal analysis** to the Verify tab. The user picks a number of modes
(1–20, default 6); the solver returns the lowest N natural frequencies (Hz) + their mode shapes,
computed by **LOBPCG preconditioned with the existing geometric-multigrid V-cycle**, against a
**lumped, density-scaled mass matrix**. Each mode is surfaced as an **output result-case** under a
new `kind: "modal"`, riding the existing `ResultEntry` / result-stash / viewer-switcher infra — so
mode switching, the deformed view, and animation come for free. Looking at a mode **auto-starts the
animation** (fixed visual rate, symmetric ± swing). The work reuses the static-FEA pipeline
end-to-end; the only genuinely new code is the eigensolver + lumped mass in `sig-core`.

## 1. Scope

| In | Out (deferred / rejected) |
|----|----|
| Constrained modal (uses Loads-step supports) | Free-free / inertia-relief modal (future; plumbing exists from build-sim §3a) |
| Undamped natural frequencies (Hz) + mode shapes | Damping (Rayleigh/ζ), forced/frequency response, transient |
| User-selected mode count (1–20, default 6) | Modal stress in calibrated MPa (magnitude is arbitrary) |
| Printed **or** solid, honoring the existing `analyzeMode` toggle | Safety factor on a mode shape |
| Reuse static FEA pipeline + result/viewer infra | — |

## 2. Physics & method (decided)

- **Boundary conditions — constrained (Q1).** Solve `K v = λ M v` with the **existing fixed
  supports** applied (the part as mounted). Modal is force-free, so loads/forces are ignored; only
  supports/displacement constraints matter. Free-free is a clean follow-up (the inertia-relief /
  3-2-1 path from build-sim State-2 already exists) but is **out of scope**.
- **Undamped (Q2).** No damping model, no damping input. Light polymer damping (ζ≈1–5 %) barely
  moves the natural frequencies and changes no mode shape. Damped frequency-response is a separate
  feature if ever wanted.
- **Eigensolver — LOBPCG + multigrid preconditioner (Q3).** The solver is matrix-free
  geometric-multigrid CG (`mg.rs`) — no assembled `K`, so a sparse eigensolver (ARPACK/LAPACK) is
  out. LOBPCG needs only `K·v` (have it), `M·v`, and a preconditioner — and the **existing multigrid
  V-cycle is exactly that preconditioner**. It targets "the lowest N eigenpairs," which is what the
  user picks. (Shift-invert Lanczos considered, rejected: more plumbing, a full MGCG solve per
  Lanczos step, only worth it for clustered modes.)
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

---

*Build the constrained undamped LOBPCG solver first and gate it on the cantilever analytic + the
cross-resolution test before trusting a number. Reuse the `ResultEntry`/viewer-switcher spine — do
not invent a parallel modal-results UI, and do not write modes into the `LoadStep` input matrix.*
