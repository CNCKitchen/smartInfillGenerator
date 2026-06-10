import { Sidebar } from "./ui/Sidebar";
import { Viewer } from "./viewer/Viewer";
import { useStore } from "./store";

export function App() {
  const busy = useStore((s) => s.busy);
  const error = useStore((s) => s.error);
  const clearError = useStore((s) => s.clearError);

  return (
    <div className="app">
      <Sidebar />
      <div className="main">
        <Viewer />
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
      </div>
    </div>
  );
}
