// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { Sidebar } from "./ui/Sidebar";
import { SettingsModal } from "./ui/Settings";
import { NerdLog } from "./ui/NerdLog";
import { Viewer } from "./viewer/Viewer";
import { useStore } from "./store";

export function App() {
  const busy = useStore((s) => s.busy);
  const error = useStore((s) => s.error);
  const notice = useStore((s) => s.notice);
  const clearError = useStore((s) => s.clearError);

  return (
    <div className="app">
      <Sidebar />
      <div className="main">
        <Viewer />
        <NerdLog />
        {busy && (
          <div className="busy">
            <div className="spinner" />
            {busy}
          </div>
        )}
        {error && (
          <div className="toast" onClick={clearError}>
            {error}
            <span className="dim"> — click to dismiss</span>
          </div>
        )}
        {!error && notice && (
          <div className="toast notice" onClick={clearError}>
            {notice}
          </div>
        )}
      </div>
      <SettingsModal />
    </div>
  );
}
