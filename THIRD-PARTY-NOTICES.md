# Third-party notices

`omniroute-rust` is licensed under the MIT License (see [LICENSE](LICENSE)).
It contains code and assets derived from, or bundled with, the third-party
works listed below. Each remains under its own license and copyright.

## 1. OmniRoute (upstream reference implementation)

- Project: **OmniRoute** — <https://github.com/diegosouzapw/OmniRoute>
- Version referenced: v3.8.x (TypeScript / Next.js)
- License: **MIT**
- Copyright (c) 2026 diegosouzapw

This repository is an independent Rust rewrite of the upstream gateway. The
following material is derived from the upstream project and is redistributed
here under the terms of its MIT license:

- `src/server/dashboard_assets/locales/*.json` — locale message packs (66 packs).
- `src/server/dashboard_assets/languages.json` — language index.
- `src/server/dashboard_assets/providers.json` — provider catalog metadata.
- Sidebar structure, page inventory, colour tokens and layout rules in
  `src/server/dashboard_assets/app.css`, `app.js` and `index.html`, which
  mirror upstream `src/shared/constants/sidebarVisibility/sections.ts` and
  `src/app/globals.css`.
- The provider registry data bulk-extracted into `src/registry.rs`.

Upstream MIT license text is reproduced in full:

```
MIT License

Copyright (c) 2026 diegosouzapw

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## 2. Material Symbols Outlined (icon font)

- Project: **Material Symbols** — <https://github.com/google/material-design-icons>
- File: `src/server/dashboard_assets/fonts/material-symbols-outlined.woff2`
- License: **Apache License 2.0**
- Copyright: Google LLC

The icon font is bundled to allow the dashboard to run fully offline. A copy
of the Apache License 2.0 is available at
<https://www.apache.org/licenses/LICENSE-2.0>.

## 3. Rust crate dependencies

All Rust crates resolved in `Cargo.lock` are distributed under
permissive licenses (MIT / Apache-2.0 / BSD-family). Run
`cargo install cargo-license && cargo license` for a per-crate report.

## 4. Trademarks

Provider and product names referenced in the registry and dashboard (for
example OpenAI, Anthropic, Google Gemini, Groq) are trademarks of their
respective owners. They are used here only to describe interoperability and
do not imply any affiliation or endorsement.