// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { useEffect, useRef, useState } from "react";

/**
 * Controlled number input that tolerates in-progress text. The naive
 * `onChange={e => set(Number(e.target.value))}` pattern destroys typing:
 * a leading "-" (or a cleared field) parses to 0/NaN, the re-render writes
 * "0" back into the box, and the minus sign is gone.
 *
 * This keeps a local string while the field is focused, commits every
 * complete parse immediately (so sliders/solves stay live), and snaps the
 * display back to the canonical (possibly clamped) value on blur.
 */
export function NumInput({
  value,
  onCommit,
  ...rest
}: {
  value: number;
  onCommit: (v: number) => void;
} & Omit<React.InputHTMLAttributes<HTMLInputElement>, "value" | "onChange" | "type">) {
  const [text, setText] = useState<string>(String(value));
  const focused = useRef(false);
  useEffect(() => {
    if (!focused.current) setText(String(value));
  }, [value]);
  const commit = (raw: string) => {
    const n = Number(raw);
    if (raw !== "" && Number.isFinite(n)) onCommit(n);
  };
  return (
    <input
      type="number"
      value={text}
      onFocus={() => {
        focused.current = true;
      }}
      onChange={(e) => {
        // Invalid partials ("-", "1e-") read as "" from a number input;
        // writing "" back is a DOM no-op, so the typed text survives.
        setText(e.target.value);
        commit(e.target.value);
      }}
      onBlur={(e) => {
        focused.current = false;
        commit(e.target.value);
        setText(String(value));
      }}
      {...rest}
    />
  );
}
