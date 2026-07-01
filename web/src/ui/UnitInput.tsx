// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { NumInput } from "./NumInput";
import {
  convertFromCanonical,
  convertToCanonical,
  unitDef,
  type QuantityKind,
} from "../units";

/**
 * Unit-aware number input. The store always holds CANONICAL values (mm, N,
 * MPa…); this widget edits in the user's active display unit and converts on the
 * boundary (see docs/units-design.md §7):
 *   - value in  → canonical, shown converted to display (rounded to the unit's
 *     precision so the field isn't a 0.39370078… mess);
 *   - value out → the user's typed display number, converted back to canonical
 *     ONCE at commit. The canonical store value is never overwritten by a
 *     re-rounded display number, and switching units re-derives from canonical.
 *
 * `min`/`max`/`step` are given in CANONICAL units and converted to display, so a
 * bound stays unit-invariant (the §7 "validation bounds live in canonical"
 * rule). The caller is still responsible for clamping the committed canonical
 * value if a hard clamp is required.
 */
export function UnitInput({
  value,
  kind,
  onCommit,
  min,
  max,
  step,
  ...rest
}: {
  value: number;
  kind: QuantityKind;
  onCommit: (canonical: number) => void;
  min?: number;
  max?: number;
  step?: number;
} & Omit<
  React.InputHTMLAttributes<HTMLInputElement>,
  "value" | "onChange" | "type" | "min" | "max" | "step"
>) {
  const u = unitDef(kind);
  const disp = Number(convertFromCanonical(value, kind).toFixed(u.decimals));
  const toDisp = (v: number | undefined) =>
    v == null ? undefined : convertFromCanonical(v, kind);
  return (
    <NumInput
      value={disp}
      onCommit={(v) => onCommit(convertToCanonical(v, kind))}
      min={toDisp(min)}
      max={toDisp(max)}
      // a canonical step doesn't have a clean display analogue; let the unit's
      // own precision drive the spinner unless the caller forces one.
      step={step != null ? convertFromCanonical(step, kind) : undefined}
      {...rest}
    />
  );
}
