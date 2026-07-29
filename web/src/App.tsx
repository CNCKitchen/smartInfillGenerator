// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { useEffect, useRef } from "react";
import { TopBar } from "./ui/TopBar";
import { StepRail } from "./ui/StepRail";
import { StepPanel } from "./ui/StepPanel";
import { Inspector } from "./ui/Inspector";
import { StatusStrip } from "./ui/StatusStrip";
import { ViewportChips } from "./ui/ViewportChips";
import { SettingsModal } from "./ui/Settings";
import { PropertyManagerModal } from "./ui/PropertyManager";
import { MaterialManagerModal } from "./ui/MaterialManager";
import { UnitsModal } from "./ui/UnitsModal";
import { ImportUnitsModal } from "./ui/ImportUnitsModal";
import { LoadStepsModal } from "./ui/LoadSteps";
import { ImprintModal } from "./ui/Imprint";
import { Disclaimer } from "./ui/Disclaimer";
import { SupportBanner } from "./ui/SupportBanner";
import { SupportModal } from "./ui/SupportModal";
import { NerdLog } from "./ui/NerdLog";
import { TitleTipLayer } from "./ui/HelpTip";
import { Viewer } from "./viewer/Viewer";
import { useStore } from "./store";
import { engine } from "./engine/EngineClient";
import { stepImporter } from "./engine/StepImporter";

export function App() {
  const busy = useStore((s) => s.busy);
  const importingStep = useStore((s) => s.importingStep);
  const error = useStore((s) => s.error);
  const notice = useStore((s) => s.notice);
  const model = useStore((s) => s.model);
  const clearError = useStore((s) => s.clearError);

  // Startup default: the bundled sample model with its loads applied, so the
  // first paint invites poking at a working setup instead of a bare drop
  // zone. The action itself is race-safe (one-shot latch + re-checks), and
  // opting out lives in the nerd log next to the disclaimer skip.
  useEffect(() => {
    const s = useStore.getState();
    if (!s.sampleSkipped) void s.loadSampleModel();
  }, []);

  // Drag & drop anywhere in the window. Overlays (the empty-state drop card,
  // panels, chips) sit above the viewer canvas, so an element-scoped listener
  // misses drops on them and the browser navigates to the file instead.
  useEffect(() => {
    const onDrop = (ev: DragEvent) => {
      ev.preventDefault();
      const f = ev.dataTransfer?.files?.[0];
      if (!f) return;
      void (async () => {
        const s = useStore.getState();
        if (/\.filasim$/i.test(f.name)) await s.openProject(f);
        else await s.loadFile(f.name, await f.arrayBuffer());
      })();
    };
    const onDrag = (ev: DragEvent) => ev.preventDefault();
    window.addEventListener("drop", onDrop);
    window.addEventListener("dragover", onDrag);
    return () => {
      window.removeEventListener("drop", onDrop);
      window.removeEventListener("dragover", onDrag);
    };
  }, []);

  return (
    <div className="app">
      <TopBar />
      <div className="mid">
        <StepRail />
        <StepPanel />
        <div className="stage">
          <Viewer />
          <SupportBanner />
          <ViewportChips />
          {!model && <DropZone />}
          <NerdLog />
          {busy && (
            <div className="busychip">
              <div className="spinner" />
              {busy}
              {importingStep ? (
                // STEP conversion: Stop terminates the meshStep worker (the
                // sync import can't be interrupted any other way, DESIGN §18).
                <button className="stopbtn" onClick={() => stepImporter.cancel()}>
                  ■ Stop
                </button>
              ) : (
                engine.canCancel && (
                  <button className="stopbtn" onClick={() => useStore.getState().cancelRun()}>
                    ■ Stop
                  </button>
                )
              )}
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
        <Inspector />
      </div>
      <StatusStrip />
      <SettingsModal />
      <PropertyManagerModal />
      <MaterialManagerModal />
      <UnitsModal />
      <ImportUnitsModal />
      <LoadStepsModal />
      <ImprintModal />
      <SupportModal />
      <Disclaimer />
      <TitleTipLayer />
    </div>
  );
}

/** Empty viewport = an invitation to act: drop target with an open button. */
function DropZone() {
  const loadFile = useStore((s) => s.loadFile);
  const fileRef = useRef<HTMLInputElement>(null);
  const onFile = async (f: File | undefined) => {
    if (!f) return;
    await loadFile(f.name, await f.arrayBuffer());
  };
  return (
    <div className="dropzone">
      <div className="dz-card">
        <b>Drop an STL, 3MF, or STEP here</b>
        <div className="small">STL units are set on import — the file never leaves your browser.</div>
        <input
          ref={fileRef}
          type="file"
          accept=".stl,.3mf,.step,.stp"
          hidden
          onChange={(e) => void onFile(e.target.files?.[0])}
        />
        <button className="primary" onClick={() => fileRef.current?.click()}>
          Open model…
        </button>
      </div>
    </div>
  );
}
