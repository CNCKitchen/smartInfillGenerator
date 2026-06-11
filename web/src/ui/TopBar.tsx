// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { useStore } from "../store";
import { fmtLen } from "./fmt";

export function TopBar() {
  const s = useStore();
  const m = s.model;
  return (
    <header className="top">
      <div className="brandmark">SI</div>
      <div className="brand">
        <b>Smart Infill Generator</b>
        <span>CNC Kitchen · browser FEA</span>
      </div>
      <div className="partchip">
        {m ? (
          <>
            <b>{s.fileName}</b>
            <span>
              {fmtLen(m.bbox[3] - m.bbox[0])} × {fmtLen(m.bbox[4] - m.bbox[1])} ×{" "}
              {fmtLen(m.bbox[5] - m.bbox[2])} mm
            </span>
            <span>{m.triCount.toLocaleString()} tris</span>
          </>
        ) : (
          <span>no model — open or drop an STL / 3MF</span>
        )}
      </div>
      <div className="grow" />
      <button className="ghost" onClick={() => s.openSettings(true)} title="Materials, infill stiffness curves, density levels">
        ⚙ Settings
      </button>
      <button
        className="primary"
        disabled={!s.optSummary}
        onClick={() => void s.downloadThreeMf()}
        title={
          s.optSummary
            ? "Download the OrcaSlicer / Bambu Studio project"
            : "Run Optimize infill first — the 3MF carries the graded regions"
        }
      >
        Export 3MF
      </button>
    </header>
  );
}
