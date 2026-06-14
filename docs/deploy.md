<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com> -->

# Deployment

The app is a fully static SPA (all compute runs client-side in WASM), so hosting
is just serving `web/dist`. Two targets deploy from `master` on every push:

| Target            | Workflow                                   | URL                                            | Base path             |
| ----------------- | ------------------------------------------ | ---------------------------------------------- | --------------------- |
| Cloudflare Worker | `.github/workflows/deploy-cloudflare.yml`  | https://infeall.com                            | `/`                   |
| GitHub Pages      | `.github/workflows/deploy.yml`             | https://cnckitchen.github.io/smartInfillGenerator | `/smartInfillGenerator/` |

The two builds differ only by Vite `base` (root domain vs. project sub-path), so
they cannot share one artifact. Drop `deploy.yml` if GitHub Pages is no longer
needed (halves CI time).

## Cloudflare (infeall.com)

The build runs in GitHub Actions (the Rust nightly + `build-std` MT-WASM build is
too heavy/slow for Cloudflare's build container). The workflow then ships
`web/dist` to the `smartinfillgenerator` Worker via `wrangler deploy`, configured
by `wrangler.jsonc` at the repo root. `web/public/_headers` sets the COOP/COEP
cross-origin-isolation headers natively (the bundled `coi-serviceworker.js` is the
fallback for hosts that can't set headers).

### One-time setup

1. **API token** — Cloudflare dashboard → My Profile → API Tokens → Create Token →
   use the **"Edit Cloudflare Workers"** template (Account → Workers Scripts: Edit).
2. **Account ID** — Workers & Pages dashboard, right sidebar.
3. **GitHub secrets** — repo → Settings → Secrets and variables → Actions → add
   `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`.
4. **Disconnect the Cloudflare ↔ Git build integration** (Worker → Settings →
   Build → Disconnect). Deploys come from GitHub Actions now, not Cloudflare's
   own build, so leaving it connected just produces failing builds.
5. **Custom domain** — attach `infeall.com` to the `smartinfillgenerator` Worker
   (Worker → Settings → Domains & Routes), and remove it from whatever project
   currently serves the design-draft placeholder.
6. Push to `master` (or run the workflow manually) to deploy.
