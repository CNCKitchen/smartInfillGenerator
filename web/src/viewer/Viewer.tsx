import { useEffect, useRef } from "react";
import { SceneManager } from "./SceneManager";
import { sceneEvents, useStore } from "../store";

function union(a: Uint32Array, b: Uint32Array): Uint32Array {
  const s = new Set<number>(a as unknown as number[]);
  for (const t of b) s.add(t);
  return Uint32Array.from(s);
}

function subtract(a: Uint32Array, b: Uint32Array): Uint32Array {
  const s = new Set<number>(a as unknown as number[]);
  for (const t of b) s.delete(t);
  return Uint32Array.from(s);
}

function containsAll(a: Uint32Array, b: Uint32Array): boolean {
  const s = new Set<number>(a as unknown as number[]);
  for (const t of b) if (!s.has(t)) return false;
  return true;
}

export function Viewer() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const sceneRef = useRef<SceneManager | null>(null);

  const tool = useStore((s) => s.tool);
  const brushRadius = useStore((s) => s.brushRadius);
  const brushErase = useStore((s) => s.brushErase);

  useEffect(() => {
    const scene = new SceneManager();
    sceneRef.current = scene;
    scene.init(canvasRef.current!, {
      onPickPatch: (tris, additive) => {
        const st = useStore.getState();
        const bc = st.bcs.find((b) => b.id === st.activeBcId);
        if (!bc) return;
        const next =
          !additive || containsAll(bc.tris, tris) ? subtract(bc.tris, tris) : union(bc.tris, tris);
        st.updateBcTris(bc.id, next);
      },
      onBrush: (tris, erase) => {
        const st = useStore.getState();
        const bc = st.bcs.find((b) => b.id === st.activeBcId);
        if (!bc) return;
        st.updateBcTris(bc.id, erase ? subtract(bc.tris, tris) : union(bc.tris, tris));
      },
    });

    sceneEvents.onModelLoaded = (m) => scene.setModel(m);
    sceneEvents.onPatchIdsChanged = (ids) => scene.setPatchIds(ids);
    sceneEvents.onBcsChanged = (bcs, active) => scene.setBcs(bcs, active);
    sceneEvents.onAnimateMode = (mode) => scene.setRbmMode(mode);
    sceneEvents.onDisplacements = (d, stats) => scene.setDisplacements(d, stats);
    sceneEvents.onVertexDensity = (d) => scene.setVertexDensity(d);
    sceneEvents.onRegions = (r) => scene.setRegions(r);
    sceneEvents.onViewState = (mode, scale) => scene.setViewState(mode, scale);

    const obs = new ResizeObserver(() => {
      const el = wrapRef.current;
      if (el) scene.resize(el.clientWidth, el.clientHeight);
    });
    obs.observe(wrapRef.current!);

    return () => {
      obs.disconnect();
      scene.dispose();
    };
  }, []);

  useEffect(() => {
    sceneRef.current?.setTool(tool, brushRadius, brushErase);
  }, [tool, brushRadius, brushErase]);

  // Drag & drop.
  useEffect(() => {
    const el = wrapRef.current!;
    const onDrop = async (ev: DragEvent) => {
      ev.preventDefault();
      const file = ev.dataTransfer?.files?.[0];
      if (!file) return;
      const bytes = await file.arrayBuffer();
      void useStore.getState().loadFile(file.name, bytes);
    };
    const onDrag = (ev: DragEvent) => ev.preventDefault();
    el.addEventListener("drop", onDrop);
    el.addEventListener("dragover", onDrag);
    return () => {
      el.removeEventListener("drop", onDrop);
      el.removeEventListener("dragover", onDrag);
    };
  }, []);

  return (
    <div className="viewer" ref={wrapRef}>
      <canvas ref={canvasRef} />
    </div>
  );
}
