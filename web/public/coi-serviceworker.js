// SPDX-License-Identifier: MIT
// Based on coi-serviceworker v0.1.7 by Guido Zuidhof
// https://github.com/gzuidhof/coi-serviceworker
// Enables cross-origin isolation on hosts that can't set custom headers
// (e.g. GitHub Pages) so SharedArrayBuffer / threaded WASM works.
(() => {
  if (typeof window === "undefined") {
    // ── Service worker context ───────────────────────────────────────────
    self.addEventListener("install", () => self.skipWaiting());
    self.addEventListener("activate", (ev) => ev.waitUntil(self.clients.claim()));
    self.addEventListener("fetch", (ev) => {
      const req = ev.request;
      // Avoid intercepting opaque requests that would break.
      if (req.cache === "only-if-cached" && req.mode !== "same-origin") return;
      // Navigate requests: re-create to avoid cloning issues in some browsers.
      const fetchReq =
        req.mode === "navigate"
          ? new Request(req.url, { headers: req.headers, method: req.method, mode: "same-origin" })
          : req;
      ev.respondWith(
        fetch(fetchReq).then((res) => {
          if (res.status === 0) return res;
          const headers = new Headers(res.headers);
          headers.set("Cross-Origin-Opener-Policy", "same-origin");
          headers.set("Cross-Origin-Embedder-Policy", "require-corp");
          return new Response(res.body, { status: res.status, statusText: res.statusText, headers });
        })
      );
    });
    return;
  }

  // ── Page context: register the service worker ────────────────────────
  if (self.crossOriginIsolated) return; // already isolated, nothing to do

  if (!navigator.serviceWorker) {
    console.warn("coi-serviceworker: service workers unavailable — threaded WASM will not work");
    return;
  }

  navigator.serviceWorker
    .register(window.document.currentScript.src)
    .then((reg) => {
      reg.addEventListener("updatefound", () => {
        reg.installing.addEventListener("statechange", function () {
          if (this.state === "installed") window.location.reload();
        });
      });
      // SW was already installed but this tab isn't controlled yet → reload once.
      if (reg.active && !navigator.serviceWorker.controller) window.location.reload();
    })
    .catch((err) => console.error("coi-serviceworker: registration failed —", err));
})();
