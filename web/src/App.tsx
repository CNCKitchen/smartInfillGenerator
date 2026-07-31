// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { useEffect, useRef, useState } from "react";
import { TopBar } from "./ui/TopBar";
import { StepRail } from "./ui/StepRail";
import { StepPanel } from "./ui/StepPanel";
import { Inspector, useInspectorPopulated } from "./ui/Inspector";
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
  // Narrow windows only: below the drawer breakpoint the inspector leaves the
  // column row and overlays the stage's right edge, so it needs a handle. Wide
  // layouts never see this — the tab is display:none above the breakpoint and
  // the collapse class is inert there.
  //
  // It starts CLOSED under the breakpoint (and only there): an open drawer
  // covers the stage's right edge, which is where the color legend sits, and
  // the first thing someone should see on a small window is an unobstructed
  // part. Read once at mount — after that the tab is the user's to drive, and
  // yanking it open or shut mid-resize would be worse than either default.
  const [inspectorOpen, setInspectorOpen] = useState(
    () => !window.matchMedia("(max-width: 1040px)").matches
  );
  const inspectorPopulated = useInspectorPopulated();

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
      <div className={`mid${inspectorOpen ? "" : " inspect-off"}`}>
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
        {inspectorPopulated && (
          <button
            className="inspecttab"
            onClick={() => setInspectorOpen((o) => !o)}
            title={inspectorOpen ? "Hide the results panel" : "Show the results panel"}
            aria-label={inspectorOpen ? "Hide the results panel" : "Show the results panel"}
          >
            {inspectorOpen ? "›" : "‹"}
          </button>
        )}
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
