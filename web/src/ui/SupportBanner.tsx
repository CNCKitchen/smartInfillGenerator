// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Support banner — same call-to-action as bumpmesh.com. A dismissible pill
// floated at the bottom-center of the viewport (clear of the section/wireframe
// chips on the left and the axis gizmo on the right). Dismissal is remembered
// in localStorage so it stays gone across sessions.

import { useState } from "react";

const KEY = "filasim.supportBanner.dismissed";

export function SupportBanner() {
  const [hidden, setHidden] = useState(() => {
    try {
      return localStorage.getItem(KEY) === "1";
    } catch {
      return false;
    }
  });
  if (hidden) return null;
  const dismiss = () => {
    try {
      localStorage.setItem(KEY, "1");
    } catch {
      /* private mode — just hide for this session */
    }
    setHidden(true);
  };
  return (
    <div className="supportbanner">
      <span>
        Support this tool? Shop at{" "}
        <a href="https://geni.us/CNCStoreSim" target="_blank" rel="noopener noreferrer">
          CNCKitchen.STORE
        </a>{" "}
        or send a tip via{" "}
        <a href="https://www.paypal.me/CNCKitchen" target="_blank" rel="noopener noreferrer">
          PayPal
        </a>{" "}
        /{" "}
        <a href="https://ko-fi.com/cnckitchen" target="_blank" rel="noopener noreferrer">
          Ko-fi
        </a>
      </span>
      <button className="sb-x" onClick={dismiss} title="Dismiss" aria-label="Dismiss">
        ×
      </button>
    </div>
  );
}
