// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import React from "react";
import ReactDOM from "react-dom/client";
import { App } from "./App";
import { useStore } from "./store";
import { engine } from "./engine/EngineClient";
import "./styles.css";

// Dev-only automation hooks: let e2e scripts drive the app through the store
// (import a model, add BCs, solve, toggle views) without scripted UI clicks.
if (import.meta.env.DEV) {
  (window as unknown as { __store: typeof useStore }).__store = useStore;
  (window as unknown as { __engine: typeof engine }).__engine = engine;
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);
