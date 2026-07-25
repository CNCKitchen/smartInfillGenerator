// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Foldable sub-section inside a step panel. Station 5 stacks three optimizers
// (infill, print settings, orientation); expanded all at once they buried the
// one the user actually came for. Each is now a header + a body that folds.
// The header keeps the .sec-head look of the flat sections, carries a live
// badge so a FOLDED section still reports its state, and hangs the section's
// explanation on an ⓘ card instead of a paragraph in the panel.

import { useState, type ReactNode } from "react";
import { InfoTip, type HelpContent } from "./HelpTip";

/** Fold state per section title, for the lifetime of the tab. The panel is
 *  unmounted on every step change, and re-folding the section you were working
 *  in each time you come back from step 4 is exactly the kind of small
 *  annoyance a step-by-step UI cannot afford. Not persisted: a reload starts
 *  from the defaults again. */
const foldState = new Map<string, boolean>();

export function Section({
  title,
  badge,
  help,
  defaultOpen = false,
  children,
}: {
  title: string;
  /** Current state in a word or two — shown open OR folded. */
  badge?: ReactNode;
  help?: HelpContent;
  defaultOpen?: boolean;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(() => foldState.get(title) ?? defaultOpen);
  const toggle = () => {
    foldState.set(title, !open);
    setOpen(!open);
  };
  return (
    <div className={`subsec${open ? " open" : ""}`}>
      <div className="subsec-head">
        <button className="sec-head" aria-expanded={open} onClick={toggle}>
          <span className="caret" aria-hidden="true" />
          <span className="t">{title}</span>
          {badge != null && badge !== false && <b>{badge}</b>}
        </button>
        {help && <InfoTip help={help} />}
      </div>
      {open && <div className="subsec-body">{children}</div>}
    </div>
  );
}
