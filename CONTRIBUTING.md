# Contributing

Thanks for your interest! Two ground rules keep this project sustainable —
please read them before opening a PR.

## 1. Contributor License Agreement (CLA)

This project is dual-licensed: open source under AGPL-3.0-only for everyone,
with commercial exceptions sold to companies that want to embed it in
proprietary software (see [COMMERCIAL.md](COMMERCIAL.md)). That model only
works if the project owner holds the licensing rights to the whole codebase.

Therefore every contribution requires agreeing to the CLA: you keep the
copyright to your contribution, but you grant the project owner a perpetual,
irrevocable, worldwide right to license your contribution under any terms,
including proprietary ones. By submitting a pull request you confirm:

> I have the right to submit this contribution, and I grant Stefan Hermann
> (CNC Kitchen) a perpetual, worldwide, non-exclusive, irrevocable,
> royalty-free license to use, modify, sublicense, and relicense my
> contribution under licenses of his choosing, including the AGPL-3.0-only
> and commercial licenses.

We're transparent about why: without this, a single outside patch would
legally block the commercial-exception model that funds development.

## 2. Dependency license policy (CRITICAL — checked on every PR)

The dual-licensing model collapses if the core ever contains third-party
copyleft code we don't own: we cannot sell a commercial exception covering
someone else's (A)GPL code. **Every new dependency — Rust crate, npm
package, or vendored snippet — must be license-vetted before it lands.**

Allowed in the core (engine crates + web app):

- MIT, Apache-2.0 (incl. LLVM exception), BSD-2/3-Clause, ISC, Zlib,
  CC0-1.0, Unicode-3.0 / Unicode-DFS-2016
- MPL-2.0 (file-level copyleft — acceptable for commercial licensees)

**Never** in the core, regardless of how useful:

- GPL (any version), LGPL (everything here links statically — wasm/Rust),
  AGPL code we don't own, SSPL, BSL/FSL, "non-commercial" (CC-BY-NC etc.),
  JSON license, unlicensed/no-license code, and copy-pasted code of unknown
  origin (Stack Overflow snippets are CC BY-SA — do not paste them).

If a copyleft component is ever genuinely needed, the options are: isolate
it behind a process/network boundary as an optional component, buy a
commercial license for it, or write our own. Ask first.

Checking: `cargo deny check licenses` (allowlist in [deny.toml](deny.toml))
for Rust; `npx license-checker --summary` for npm. CI runs these on PRs.

## Practicalities

- Engine work: `cargo test -p sig-core` must stay green (validation tests
  compare against analytic solutions — treat tolerance changes as red flags).
- Full pipeline: `wasm-pack build crates/sig-wasm --target web --out-dir
  ../../web/src/wasm && node smoke-wasm.mjs`.
- Web: `cd web && npm run build` (tsc strict + vite).
- New source files carry the SPDX header:
  `// SPDX-License-Identifier: AGPL-3.0-only`
