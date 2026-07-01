// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Support popup — the same "thanks for using this free tool" call-to-action
// bumpmesh.com shows on export. Here it pops up whenever a run is started
// (analysis, modal, or optimization). "Don't show this again" suppresses it
// for 7 days (see the store's SUPPORT_SUPPRESS_KEY), then it returns.

import { useState } from "react";
import { useStore } from "../store";

export function SupportModal() {
  const open = useStore((s) => s.supportOpen);
  const closeSupport = useStore((s) => s.closeSupport);
  const [dontShow, setDontShow] = useState(false);
  if (!open) return null;
  return (
    <div className="modalback" onClick={() => closeSupport(dontShow)}>
      <div className="modal support" onClick={(e) => e.stopPropagation()}>
        <h2>Thanks for using filaSim by CNC Kitchen!</h2>
        <p className="sm-lead">
          This tool is provided <b>completely free</b> by CNC Kitchen. While your part is being
          processed, why not check out the store that helps us keep making cool stuff for you?
        </p>

        <div className="sm-actions">
          <a
            className="sm-btn store"
            href="https://geni.us/CNCStoreSim"
            target="_blank"
            rel="noopener noreferrer"
          >
            🛒 Visit CNCKitchen.STORE
          </a>
          <a
            className="sm-btn paypal"
            href="https://www.paypal.me/CNCKitchen"
            target="_blank"
            rel="noopener noreferrer"
          >
            💙 Send a tip on PayPal
          </a>
          <a
            className="sm-btn kofi"
            href="https://ko-fi.com/cnckitchen"
            target="_blank"
            rel="noopener noreferrer"
          >
            ☕ Send a tip on Ko-fi
          </a>
        </div>

        <div className="sm-foot">
          <label className="sm-dontshow">
            <input
              type="checkbox"
              checked={dontShow}
              onChange={(e) => setDontShow(e.target.checked)}
            />
            Don’t show this again
          </label>
          <button className="primary" onClick={() => closeSupport(dontShow)}>
            Close &amp; Continue
          </button>
        </div>
      </div>
    </div>
  );
}
