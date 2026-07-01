# Display Units — Design Note

*Drafted 2026-06-29 via a "grill-me" design interview. Status: **design resolved, pre-implementation**.
This captures the decisions and — more importantly — the **invariants** that keep multi-unit support
from becoming a source of silent numeric bugs.*

## 0. One-paragraph summary

Add user-selectable display units for length, area, volume, force, moment, pressure, stress, modulus,
mass, density (and the dimensionless/odd ones: strain, angle). The **engine, store, solver, project
files, and every stored number stay canonical forever (mm, N, and the N/mm-derived units)**. Units are
a **pure presentation layer**: a value is converted to the user's unit only when rendered, and converted
straight back to canonical the instant it is committed. Selection is **per-quantity** (a "salad bar," so
moments read N·m and moduli read GPa — units derived units don't compose into anything humans quote),
which means consistency is **no longer guaranteed by construction** and must be re-engineered by hand via
a central quantity registry + compile-time quantity branding. A **presets-first** UI (default Metric;
SI-mm / US-in available) restores most of the lost consistency, and an opt-in **consistent-units mode**
gives pros a self-multiplying readout by force-deriving every unit from the base set and locking the
overrides.

## 1. The one decision everything hangs on (DECIDED — display-only)

> **Canonical is the only source of truth. Units are presentation. Nothing downstream of an input
> widget ever sees a non-canonical number.**

- Engine / solver / store / **project files** are all canonical: **mm**, **N**, and everything derived
  from them (MPa = N/mm², etc.).
- A value is converted to the user's unit **only at render**, and converted back **only at commit**.
- Rejected: "working units" (values stored/serialized/passed-to-engine in the chosen unit). That is how
  every unit-bug horror story starts, and the codebase is already canonical, so display-only is also the
  cheapest path.

## 2. Per-quantity selection (DECIDED — option B), grouped by *concept*, not dimension

Selection is **per physical quantity**, not "metric vs imperial" and not "pick base dimensions, derive the
rest." Reason: derived units don't compose into idiomatic readouts — with length=mm, force=N the
"consistent" moment is **N·mm** (nobody quotes that) and modulus would be **MPa** (everyone wants GPa).

The grouping key is the **conceptual quantity-kind**, NOT the physical dimension. The proof:

| Same dimension, kept **separate** | Why |
|---|---|
| **Moment** vs **Energy** (both force·length) | N·m vs J — identical dimension, never the same unit. *This pair is the entire justification for per-quantity selection.* |
| **Stress** vs **Pressure** vs **Modulus** (all force/area) | MPa (result stress) vs e.g. bar/psi (a pressure BC input) vs GPa (modulus). Three independent pickers. |

| Collapsed to **one** | Why |
|---|---|
| **Length / Displacement / part size / warp / mesh & voxel size / §4.2 blur-X** | All "length," one picker. µm-vs-mm is **magnitude auto-scaling in the formatter**, not a second unit. |

**Area** and **Volume** are **separate** pickers from Length (deliberate: people want cm³/L even with mm
lengths). Standalone pickers: **Force**, **Mass**, **Density** (mass/volume).

Edge kinds handled by the same registry: **Strain** (dimensionless; "unit" = factor 1 / 100 / 1e6 for
raw / % / µε) and **Angle** (deg / rad; print orientation, §8 of the build-sim note).

**Watch-out:** build-sim **bed-peel "traction" is force/area → it is a *Stress*** (MPa), not a Force.
It must land in the stress bucket, or it'll be converted with the force factor.

## 3. The registry + how the wrong-converter bug is made impossible

Per-quantity selection threw away consistency-by-construction, so reliability is engineered back via:

### 3.1 A central quantity registry (DECIDED)
One enumerated catalog — `Length, Displacement(→Length), Area, Volume, Force, Moment, Pressure, Stress,
Modulus, Mass, Density, Strain, Angle, Energy, …` — where each entry owns: dimension, **canonical unit**,
selectable units, **conversion factors**, and **per-(quantity,unit) display precision + magnitude-scaling
thresholds**. **Every** number that hits the screen or an input routes through **one chokepoint** keyed by
this enum. The existing [fmt.ts](../web/src/ui/fmt.ts) helpers (`fmtDisp`, `fmtDispParts`, `fmtLen` — they
hardcode "mm"/"µm") are **deleted/rewritten** to route through it.

### 3.2 Compile-time branding, anchored at the type definitions (DECIDED over runtime-only)
The catastrophic bug is **silent**: a stress run through the *length* converter, plausible-looking garbage
under a "psi" label. Prevent it at compile time, cheaply, by branding where values are *born* — not at
every call site:

- **Branded scalars:** `type Stress = number & { readonly __q: "stress" }`, etc. Zero runtime cost (erased).
- **Annotate at the type definitions** — the engine-result interfaces in [types.ts](../web/src/types.ts),
  store fields, physical constants. A field declared `peelTraction: Stress` propagates the tag through
  every reader for free; `formatStress(aLength)` becomes a compile error at the source.
- **Bulk fields** (`Float32Array` of per-voxel stress) can't brand per-element → wrap as
  `{ kind: QuantityKind; data: Float32Array }`. That struct is the single chokepoint for all
  legend/colorbar/3D-paint code (warp, peel, stress fields).
- **Runtime lint backstop:** ban stray `.toFixed` + literal unit strings (`"mm"`, `"MPa"`, …) to catch the
  dynamic/string-concat cases types can't, and to force the `fmt.ts` rewrite to be total, not partial.
- **Nerd log stays canonical.** The raw engine/debug log ([NerdLog](../web/src/ui/NerdLog.tsx)) mirrors
  internals and is **never** unit-converted — it always reads N/mm regardless of display selection.

## 4. Selection UX (DECIDED — presets-first)

- **Presets are the primary control.** Default **Metric** (see §5). One click sets every quantity to a
  mutually-sensible set; most users never open the per-quantity pickers, so most users never build an
  inconsistent salad.
- **Per-quantity overrides** live behind an "advanced/customize" disclosure.
- **Consistent-units mode** (opt-in, for pros): see §6.

## 5. Defaults & persistence

- **First-run default:** **mm, N**, no geolocation (3D printing is SI). Visible idiomatic defaults:
  **MPa** stress, **GPa** modulus, **N·m** moment, **g/cm³** density, **mm²** area, **mm³** volume,
  **°** angle, **µm/mm** auto-scaled displacement.
- **Persistence (DECIDED — localStorage):** unit selection is a **per-user UI preference in localStorage**
  — survives reload, identical across projects, and **never written into the project file** (per §1).
  **Future:** when accounts land, move the preference to the user account so it follows the user across
  devices; localStorage becomes the signed-out fallback. (Rejected: storing it per-project — the unit
  system is a property of *who is looking*, not of the document.)

## 6. Consistent-units mode (DECIDED — SI-mm and US-in)

For pros who want a self-checking readout. **Definition (precise):** force-derive *every* quantity's unit
from the chosen base set and **grey out / disable the per-quantity overrides** while it's on — because a
"consistent" badge that can be overridden is a lying badge.

- **What "consistent" means *here*:** NOT "the solver has no conversion factors" (the solver never sees
  user units — §1 — so it can't). It means **every number on screen composes**: a user hand-checking
  σ = F/A by reading the Force, Area and Stress readouts gets numbers that multiply to the label.
- **The deliberate trade:** display-consistency conflicts with the idiomatic units that motivated §2.
  Turning it on **gives up GPa modulus → MPa, N·m moment → N·mm, J energy → N·mm**. That's the point, and
  pros expect exactly this (cf. Abaqus's unit-system discipline).
- **Systems offered:** **SI-mm** (mm, N, tonne, MPa) and **US-in** (in, lbf, slinch, psi).
- **Cost:** ~free. It's a **preset generator** (base set → coherent unit for every quantity) plus a
  **lock flag** that disables the overrides. Same registry, no new machinery.

## 7. Editing round-trip — the subtle invariants (DECIDED)

Display-only is only bulletproof until a value is *editable* in a non-canonical unit. Three invariants:

1. **Canonical is the only source of truth; display rounding is strictly one-way and never written back.**
   If canonical = 25.412 mm shows as "1.001 in" and the user doesn't edit, canonical stays 25.412 — the
   rounded display must never round-trip into the store. On commit, convert the **user's typed string**,
   not the rounded display. (Compounds the re-render-writes-degraded-value bug already noted in
   [NumInput.tsx](../web/src/ui/NumInput.tsx).)
2. **Conversion always goes canonical → display, never display → display.** Switching mm→inch converts from
   stored canonical, not from the rounded on-screen value, so flipping units back and forth shows *stable*
   numbers and never accumulates rounding error.
3. **Validation bounds live in canonical, shown converted.** Every min/max/step (voxel-size min, blur-X,
   wall-derived thicknesses) is defined in canonical mm and *displayed* converted, so the constraint is
   unit-invariant. A literal "min 0.5" in a widget means 0.5 mm to one user and 0.5 in to another — the
   same footgun as §8, one level down.

Ownership:
- **Per-(quantity,unit) precision** lives in the registry beside the factor (mm→2dp, inch→3–4dp, psi→0dp,
  MPa→1–2dp).
- **Auto-magnitude** (mm↔µm, inch↔mil) is **formatter behavior on read-only surfaces only**, keyed off
  value magnitude — **never** on an editable field (a field whose unit silently changes under the cursor
  is its own nightmare) and **never** a separate unit setting.

## 8. STL import (DECIDED)

STL is **unitless** — a 1×1×1 file is *either* 1 mm or 1 inch and the file cannot say which (US CAD exports
inch STLs → they arrive 25.4× too small with zero error). The governing distinction:

> **"What unit was the file authored in" (import: one-time, irreversible bake to canonical) and
> "what unit do I want to see" (display: reversible, runtime) are different things. Conflating them —
> e.g. interpreting the STL in the current *display* unit — is the bug** (geometry would then depend on a
> UI toggle).

Decisions:
- **Import-unit picker on every import**, default **mm**, with **"don't ask again" → remember that choice
  and stop prompting.**
- Picker reuses the Length unit list but plays a different role (one-time bake, not reversible display).
- Geometry is **baked to canonical mm at import; the import unit is then immutable** — switching display
  units later must not reinterpret the file.
- **Backstop for silent ("don't ask again") mode:** the existing **bounding-box readout in the bottom bar**
  catches a wrong guess instantly (0.04 mm vs 40 mm) → **sanity-check DONE**. Plus: keep "don't ask again"
  re-enableable in settings, and provide a **rescale ×25.4 / ÷25.4** escape hatch (no re-import needed).
- **No auto-detect** from geometry size — heuristics fail on legitimately tiny/huge parts and are *trusted*
  100% of the time while wrong some of it. A good default + loud readout beats a guess.
- **Future 3MF/STEP:** they carry an embedded unit → **honor it, no prompt.** STL is the only ambiguous
  format because it's the only one that drops the unit.

## 9. Export (DECIDED) — mirror of import, risk runs the other way

§8-of-build-sim export emits predeformed **STL/3MF** and morphed **gcode**. Here *we* can inflict the
25.4× bug on a downstream consumer, so:

- **STL: always mm**, no picker. The receiving slicer assumes mm; honoring that beats honoring a preference.
- **gcode: always mm + emit G21.** No choice.
- **3MF: write canonical mm + an explicit unit attribute** by default — the *only* output format that
  self-describes, so it's the only one where an alternate export unit is safe. *Optional:* allow an inch
  export **for 3MF only** for US round-trips (import inch → give it back inch).
- **Never export "in whatever the viewer shows."** Export unit is a property of the export *action*
  (one-time conversion from canonical), decoupled from the display unit — like import.

## 10a. Where the selection UI lives (DECIDED)

The entry point is the existing **units chip in the bottom-right of the status strip** —
[StatusStrip.tsx:110](../web/src/ui/StatusStrip.tsx#L110), today a static `mm · MPa`. Clicking it opens
a unit-settings **popover/window**. Rationale: low-clutter, discoverable-when-wanted, and it's already the
place units are reported.

- **It is global, not mode-scoped.** The status strip is shared across both app modes (Simulate&Optimize /
  Build Sim, per build-sim §8), so units are an app-wide preference — correct, units are a property of who's
  looking, not of the mode.
- **Chip content (DECIDED):** show the units for **length · stress · mass** — e.g. `mm · MPa · g` — the
  three most-representative quantities. (These are a proxy: other quantities, e.g. moment/force, can still
  diverge under Custom without showing here — the popover is the source of truth for the full set.)
- **Popover is presets-first (RECOMMENDED, holds §4):** it **leads with the preset selector** (Metric /
  SI-mm consistent / US-in consistent / Custom); the per-quantity grid sits **behind an "advanced /
  customize" toggle**, not as the landing view — otherwise the grid becomes the primary UI and salad is the
  default. Selecting a **consistent** preset (SI-mm/US-in) **greys out the grid** (§6 lock).
- The hardcoded `mm` in the strip's part-info and grid lines
  ([StatusStrip.tsx:42](../web/src/ui/StatusStrip.tsx#L42),
  [:51](../web/src/ui/StatusStrip.tsx#L51)) become chokepoint conversion sites.

## 10. Implementation checklist (derived from the above)

Status: **[x] = done, [~] = partial, [ ] = todo** (as of 2026-06-30 — feature complete).

- [x] Quantity registry module ([units.ts](../web/src/units.ts)): enum + per-entry {canonical unit,
      selectable units, factors, per-unit precision, magnitude thresholds}.
- [~] Branded scalar types: scaffold shipped in [units.ts](../web/src/units.ts)
      (`Canonical<K>`, `canonical()`, `raw()`, `Field`). Annotating every engine-result/store field is the
      one **intentionally deferred** additive pass (§3.2) — the runtime chokepoint + the `.tsx` lint are the
      shipped protection.
- [x] Single `format(value, kind)` chokepoint; [fmt.ts](../web/src/ui/fmt.ts) rewritten to route through it.
      Input chokepoint = [UnitInput.tsx](../web/src/ui/UnitInput.tsx).
- [x] Lint backstop: [scripts/check-units.mjs](../web/scripts/check-units.mjs) bans stray
      `.toFixed` + literal unit strings in `.tsx` (`npm run check:units`).
- [x] Preset system (Metric default; Imperial; SI-mm / US-in consistent) + per-quantity override UI behind
      advanced; consistent-mode lock (greys overrides) — [UnitsModal.tsx](../web/src/ui/UnitsModal.tsx).
- [x] Unit preference in localStorage (`sig.units.v1`); module mirror primed before first render.
- [x] Input validation bounds canonical, display converted (UnitInput converts min/max/step); applied across
      all migrated inputs.
- [x] STL import-unit dialog ([ImportUnitsModal.tsx](../web/src/ui/ImportUnitsModal.tsx)) + "don't ask
      again" (`sig.import.v1`), Settings re-enable, ×25.4/÷25.4 rescale escape hatch in the Model step.
- [x] Export: STL/gcode already canonical mm; 3MF already writes `unit="millimeter"` (verified against the
      golden tests). Optional inch-3MF round-trip not implemented (explicitly optional).
- [x] Legend/colorbar/3D-marker routing through the registry, incl. editable-bound display↔canonical
      round-trip; NerdLog stays canonical.
- [x] Units chip in [StatusStrip.tsx](../web/src/ui/StatusStrip.tsx) → clickable, opens presets-first
      popover; chip shows length · stress · mass (e.g. `mm · MPa · g`).
- [x] Inputs migrated to UnitInput: material (E/ρ/σ), loads (force/pressure/moment/disp incl VectorInput),
      print/analysis lengths (line width, layer height, voxel h, min-member), load-steps grid.
- [x] Display readouts: bbox, grid h, legend, hover probe, 3D min/max markers, DRO (mass + deflection),
      provenance (|u|, mass, skin), symmetry label, mesh legend, build-sim peel/strain chips.

---

*Invariant to honor from line one (the expensive-to-retrofit ones): **store canonical, convert only at the
boundary** (§1); **brand quantities so the wrong converter is a compile error** (§3.2); **import unit ≠
display unit** (§8); **canonical-only source of truth, one-way display rounding** (§7).*
