// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Startup disclaimer: shown on EVERY page load (deliberately not persisted —
// consent is per session), closeable only through the consent button.
// The dev/testing escape hatch lives in the nerd log.

import { useStore } from "../store";

const GITHUB_URL = "https://github.com/CNCKitchen/smartInfillGenerator";

export function Disclaimer() {
  const open = useStore((s) => s.disclaimerOpen);
  const consent = useStore((s) => s.consentDisclaimer);
  if (!open) return null;
  return (
    <div className="modalback disclaimerback">
      <div className="modal disclaimer" role="alertdialog" aria-modal="true">
        <h2>
          <em>
            “Finite Element Analysis makes a good engineer better
            <br />
            and a bad engineer dangerous”
          </em>
        </h2>
        <ul>
          <li>
            <b>This is a tech demo</b>, provided <b>“as is”</b> and free of charge, with{" "}
            <b>no warranty of any kind</b> — express or implied — including merchantability,
            fitness for a particular purpose, accuracy, or non-infringement. Nothing in this
            tool has been properly tested, verified, or validated.
          </li>
          <li>
            <b>Simulation results can be plain wrong.</b> Voxel resolution, simplified material
            models, calibrated infill curves, boundary conditions, and ordinary software bugs
            all introduce errors — sometimes large ones, without any warning.
          </li>
          <li>
            <b>Never</b> rely on this tool for parts whose failure could cause injury, death, or
            damage to property — no safety-critical, load-bearing, medical, automotive, or
            aerospace use. It is not a substitute for professional engineering judgment.
          </li>
          <li>
            <b>You are solely responsible</b> for anything you build. Verify every design with
            sound engineering practice and real-world testing.{" "}
            <b>Use entirely at your own risk</b> — to the maximum extent permitted by law, the
            authors and contributors accept no liability for any damages arising from the use of
            this software (see AGPL-3.0 §15–16).
          </li>
          <li>
            Everything runs locally in your browser — your models never leave your machine. The
            code is fully open source (AGPL-3.0):{" "}
            <a href={GITHUB_URL} target="_blank" rel="noopener noreferrer">
              github.com/CNCKitchen/smartInfillGenerator
            </a>
          </li>
        </ul>
        <button className="primary consent" onClick={consent}>
          I understand and consent — use at my own risk
        </button>
      </div>
    </div>
  );
}
