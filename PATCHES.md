# Hotfix Patches for bundle-dsh.ps1

This file tracks all downstream patches applied during the Tier 2 sidecar build.
Each patch compensates for a gap or bug in the upstream harness that would otherwise
cause the desktop app to crash or misbehave.

**Rule**: When bumping the upstream harness (`UPSTREAM.lock.json`), review each patch
below against the new version. Remove patches that are now upstream-fixed; add new ones
for any regressions found.

---

## PATCH-01: Promote @deepseek-ai/* workspace packages to apps/cli dependencies

- **Why**: `pnpm deploy --prod` only bundles the CLI's own dependency closure. The harness
  `web` profile dynamically imports many `@deepseek-ai/*` workspace packages at runtime
  (e.g. `dsh-client-ui-goal`, `dsh-typert-loader`, `dsh-client-ui-plan`). These are NOT
  in the CLI's transitive deps — they resolve via hoisting in a full workspace install,
  but disappear in deploy, causing `ERR_MODULE_NOT_FOUND`.
- **Fix**: Scan all `apps/`, `packages/`, `vendor/` directories for `@deepseek-ai/*` packages,
  promote them as `workspace:*` dependencies of `apps/cli/package.json`, then re-run `pnpm install`.
- **Upstream issue**: None filed — this is a fundamental mismatch between pnpm deploy's
  single-package scope and the harness's cross-package runtime imports.
- **Verify**: After deploy, every `@deepseek-ai/*` package should exist under
  `dsh-dist/node_modules/`. A missing one indicates a new workspace package was added
  without updating this patch.

## PATCH-02: Force native modules (node-pty, koffi) into apps/cli direct dependencies

- **Why**: `pnpm deploy --legacy` only materializes DIRECT deps of the filtered package.
  Third-party transitive natives get silently dropped:
  - `node-pty`: Used by terminal/bash tools (`@deepseek-ai/dsh-subprocess-local` → `node-pty`)
  - `koffi`: Used by FFI features (`@deepseek-ai/dsh-fs-local` → `koffi`)
- **Fix**: Promote both to DIRECT deps of `apps/cli/package.json`, reading versions from
  the declaring packages so pnpm dedupes to the same lockfile instance.
- **Upstream issue**: None filed — pnpm deploy's single-package scope is by design.
- **Verify**: `smoke.ps1` gate `(b2)` runs a node-pty spawn + koffi load probe. If either
  is missing, the gate fails with a clear error.

## PATCH-03: Materialize dropped @deepseek-ai/* packages from harness checkout

- **Why**: Vendor packages consumed via `file:` specs (e.g. `@deepseek-ai/cordis -> file:vendor/cordis`)
  declare `workspace:^` sub-dependencies (`cosmokit`, `schemastery`, `cordis-plugin-*`) that
  the deploy cannot resolve. The entire closure silently disappears from `dsh-dist`.
  At runtime the harness imports them by package name via hoisted resolution.
- **Fix**: After deploy, scan the harness checkout for all `@deepseek-ai/*` packages.
  For any missing from `dsh-dist/node_modules/`, copy the built package dir (with `lib/`)
  as a real directory. Empty-shell dirs (from NSIS junction dereference) are detected
  by checking for `package.json` presence.
- **Upstream issue**: None filed — `pnpm deploy` limitation with `file:` workspace refs.
- **Verify**: Runtime should not hit `ERR_MODULE_NOT_FOUND` for any `@deepseek-ai/*` package.

## PATCH-04: dsh-settings section() array-tolerance

- **Why**: An upstream migration writes the `llm-pi-ai` settings namespace back to
  `settings.yaml` as a bare ARRAY (e.g. `- name: openai ...`). `dsh-settings`' `section()`
  calls `isPlainObject()` on it, fails, and throws:
  ```
  settings section "llm-pi-ai" must be an object of keys
  ```
  This aborts namespace registration → frontend `protocolChoices()` returns empty list →
  "添加提供方 / 添加自定义提供方" buttons stay greyed out.
- **Fix**: In the deployed `dsh-settings/lib/index.js`, replace the throw with
  `return {}` (tolerance: non-object section → empty object). This keeps the buttons
  usable after every upgrade.
- **Upstream issue**: Should be filed against `deepseek-ai/deepseek-harness` — the migration
  should write objects, not bare arrays. Until fixed, this patch is required.
- **Verify**: After first launch, the "添加提供方" button should be clickable.
  **Upgrade note**: If upstream fixes the migration, the regex match in this patch will
  fail and `bundle-dsh.ps1` will error out — remove the patch and re-release.

## PATCH-05: Resolve workspace packages from the full source tree (incl. `native/`)

- **Why**: `pnpm deploy --prod` materializes only the deploy entry's closure, and workspace
  deps are *symlinked*, never stored in `.pnpm`. The bundler's promotion scan covered only
  `apps/`, `packages/`, `vendor/`, and `Resolve-RealPackage` looked only at top-level
  `node_modules` + `.pnpm` — so a workspace dep living under `native/` was invisible.
  Concrete failure (smoke gate (b), run 34792641045): `@deepseek-ai/dsh-sandbox-local`
  statically imports `@deepseek-ai/node-addon-landlock-run`, which lives at
  `native/landlock-run/packages/entry` (a pure-JS seam; only its runtime is Linux-only).
  It was skipped as "not resolvable in source store (platform-specific optional?)" and the
  harness aborted at startup with `ERR_MODULE_NOT_FOUND`.
- **Fix**: `Build-PackageIndex` (formerly `Build-VirtualStoreIndex`) now indexes BOTH the
  `.pnpm` virtual store AND the workspace source tree (`apps/`, `packages/`, `vendor/`,
  `native/`), keyed by each `package.json`'s real `name`. `Test-PlatformCompatible` honours
  npm `os`/`cpu` (allow- and deny-lists) so Linux-only siblings
  (`node-addon-landlock-run-linux-x64` / `-arm64`) never enter a Windows bundle. A hard gate
  fails the bundle early if `@deepseek-ai/node-addon-landlock-run` is absent or has no built
  `lib/index.js`, instead of aborting later at smoke.
- **Upstream issue**: None filed — `pnpm deploy` + symlinked workspace packages is by design.
- **Verify**: `dsh-dist/node_modules/@deepseek-ai/node-addon-landlock-run/lib/index.js`
  exists, and smoke gate (b) starts the harness without `ERR_MODULE_NOT_FOUND`.

---

## Upgrade Checklist

When bumping `UPSTREAM.lock.json`, run through these steps:

1. **Read each patch above** — is it still needed for the new version?
2. **Check upstream changelog/commits** — any patch mentioned issue now merged?
3. **Update patch comments** — if a patch is no longer needed, comment it out and add
   a "REMOVED: <date>" entry explaining why.
4. **Run smoke test** — `pwsh desktop/scripts/smoke.ps1 -EntryPath ./dsh-dist/lib/bin.js`
5. **Manual smoke test** — launch the app, verify theme persistence, provider selection,
   and terminal/FFI features work.
6. **Update this file** — add a row documenting the bump.
