// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { useEffect, useRef, useState } from "react";
import { useShallow } from "zustand/shallow";
import { useStore } from "../store";

const GITHUB_URL = "https://github.com/CNCKitchen/smartInfillGenerator";

export function TopBar() {
  const s = useStore(
    useShallow((s) => ({
      model: s.model,
      fileName: s.fileName,
      busy: s.busy,
      appMode: s.appMode,
      setAppMode: s.setAppMode,
      openSettings: s.openSettings,
      saveProject: s.saveProject,
      openProject: s.openProject,
      loadFile: s.loadFile,
    }))
  );
  const openRef = useRef<HTMLInputElement>(null);
  const saveRef = useRef<HTMLDivElement>(null);
  const [saveOpen, setSaveOpen] = useState(false);
  // Results default to embedded (instant reopen); model + settings are always in.
  const [withResults, setWithResults] = useState(true);
  const m = s.model;

  // Close the save menu on an outside click.
  useEffect(() => {
    if (!saveOpen) return;
    const onDown = (e: MouseEvent) => {
      if (saveRef.current && !saveRef.current.contains(e.target as Node)) setSaveOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [saveOpen]);

  // Load Project also accepts a plain STL/3MF and falls back to a normal import.
  const onLoad = async (f: File | undefined) => {
    if (f) {
      if (/\.filasim$/i.test(f.name)) await s.openProject(f);
      else await s.loadFile(f.name, await f.arrayBuffer());
    }
    if (openRef.current) openRef.current.value = ""; // allow re-picking the same file
  };

  return (
    <header className="top">
      <div className="brandmark">FS</div>
      <div className="brand">
        <b>filaSim</b>
        <span>CNC Kitchen · browser FEA</span>
      </div>
      <label className="workspace" title="Switch workspace">
        <select
          value={s.appMode}
          onChange={(e) => s.setAppMode(e.target.value as "optimize" | "buildsim")}
          disabled={!!s.busy}
        >
          <option value="optimize">Simulate &amp; Optimize</option>
          <option value="buildsim">Build Simulation</option>
        </select>
      </label>
      <div className="grow" />
      <input
        ref={openRef}
        type="file"
        accept=".filasim,.stl,.3mf"
        hidden
        onChange={(e) => void onLoad(e.target.files?.[0] ?? undefined)}
      />
      <div className="saveproj" ref={saveRef}>
        <button
          className="ghost"
          onClick={() => setSaveOpen((o) => !o)}
          disabled={!!s.busy || !m}
          title="Save the project as a .filasim file"
        >
          Save Project ▾
        </button>
        {saveOpen && (
          <div className="savemenu">
            <label className="locked" title="Always saved — the project can't reopen without them">
              <input type="checkbox" checked readOnly disabled />
              <span>Settings</span>
            </label>
            <label className="locked" title="The original imported file, embedded so it opens anywhere">
              <input type="checkbox" checked readOnly disabled />
              <span>Model ({/\.3mf$/i.test(s.fileName ?? "") ? "3MF" : "STL"})</span>
            </label>
            <label title="Embed the FEA results for instant reopen — larger file. Off keeps the optimized design only.">
              <input
                type="checkbox"
                checked={withResults}
                onChange={(e) => setWithResults(e.target.checked)}
              />
              <span>Results (FEA)</span>
            </label>
            <button
              className="primary savebtn"
              onClick={() => {
                setSaveOpen(false);
                void s.saveProject(withResults);
              }}
            >
              Save
            </button>
          </div>
        )}
      </div>
      <button
        className="ghost"
        onClick={() => openRef.current?.click()}
        disabled={!!s.busy}
        title="Open a .filasim project — or a plain STL / 3MF to start fresh"
      >
        Load Project
      </button>
      <button
        className="ghost"
        onClick={() => s.openSettings(true)}
        title="Materials, infill stiffness curves, density levels"
      >
        ⚙ Settings
      </button>
      <a
        className="ghost iconbtn"
        href={GITHUB_URL}
        target="_blank"
        rel="noopener noreferrer"
        title="View source on GitHub"
        aria-label="View source on GitHub"
      >
        <svg width="18" height="18" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true">
          <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z" />
        </svg>
      </a>
    </header>
  );
}
