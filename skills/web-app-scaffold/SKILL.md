---
name: web-app-scaffold
description: The golden path for starting a web app prototype — scaffold with the baked create-vite/vite/typescript/eslint/prettier toolchain instead of hand-rolling index.html plus script tags, seed configs and the multi-stage verify file in the same step, then get eyes on it. Use whenever a build starts with "make me a web app / site / UI / dashboard".
---

# web-app-scaffold

Post-Q1, the prototyping image bakes `create-vite`, `vite`, `typescript`,
`eslint`, `prettier`, and `tailwindcss` as global `npm` packages — no
registry fetch needed, so scaffolding works with **no network egress**.
Reach for this the moment a build is a browser-facing app: don't hand-write
`index.html` + a `<script>` tag when a real scaffold is one command away.

## 1. Scaffold, offline-safe

```bash
mkdir -p /data/<project> && cd /data/<project>
npm create vite@latest . -- --template vanilla-ts   # default
# npm create vite@latest . -- --template react-ts   # only when the user asked for React
```

Because the packages are global, this never touches the network — confirm
with `command -v vite` first if you want a sanity check before running it.
`vanilla-ts` is the default: reach for `react-ts` (or `vue-ts`) only when the
user names the framework, not by reflex. Commit right after
(`git init && git add -A && git commit -m "init: vite scaffold"`) per
[[coding-task]] before you touch a single file.

## 2. Seed configs in the same step, not later

Do this immediately after scaffolding, before the first feature line:

- **`tsconfig.json`** — the `-ts` templates already ship one; if you scaffold
  a non-TS template anyway, add a minimal one (`target: "ES2020"`,
  `strict: true`, `moduleResolution: "bundler"`).
- **ESLint** — global `eslint` has no project config to run against. Add a
  flat `eslint.config.js` (ESLint 9+ default format) that extends the
  TypeScript recommended set for `src/**/*.ts`. Probe first —
  `command -v eslint` — so a stale image that predates Q1 degrades with a
  clear message instead of a confusing "command not found" mid-build.
- **Prettier** — a two-line `.prettierrc` (`{"semi": true, "singleQuote":
  true}` or whatever the user's style implies) is enough; don't bikeshed it.

## 3. Write the matching multi-stage verify, at scaffold time

Write `.copperclaw/verify` before the first feature commit, not at the end —
one stage per discipline, `name: command`:

```
lint: npx eslint .
typecheck: tsc --noEmit
build: npm run build
```

Swap the third line for `test: npm test` once real tests exist. Probe each
tool before writing its line (`command -v eslint`, `command -v tsc`) — a
missing binary means an older pre-Q1 image, and a stage that can never pass
just burns the fix-cycle budget for nothing; drop that line and note the
degradation instead. See [[coding-task]] for how the gate enforces this file
and [[testing]] for reading a failing stage's tail without truncating past
the useful error.

## 4. Once the dev server is up: look at it

```bash
npm run dev &                      # or vite --port <n>
```

Then run `ui_screenshot` against `http://127.0.0.1:<port>` (loopback only —
that's the point) before calling any visual milestone done. Load
`load_skill("frontend-design")` for the critique checklist to run against
what you see — that skill (landing separately) is the depth reference for
*what* to fix; this skill only teaches *when* to look. Screenshot again after
fixing the worst offenders. Ship the final screenshot alongside the
prototype-ready card with `send_file` per [[coding-task]]'s delivery ritual.

## Related skills

[[coding-task]] (the verify gate + delivery ritual this plugs into),
[[preview]] (serving the dev build to the operator), [[testing]] (reading
verify output), [[frontend-design]] (the critique checklist `ui_screenshot`
feeds).
