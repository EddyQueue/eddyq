# Releasing eddyq

This repo publishes two npm packages as a single unit:

- `@eddyq/queue` — ships a native binary per platform via NAPI-RS, so 7
  companion packages (`@eddyq/queue-darwin-arm64`, `@eddyq/queue-linux-x64-gnu`,
  …) are published alongside and wired up as `optionalDependencies`.
- `@eddyq/nestjs` — pure TypeScript, peer-depends on `@eddyq/queue`.

Both go out atomically from a single tag-triggered GitHub Actions run.

**Versions are driven by the tag** (`v0.1.0` → `0.1.0` on npm). You never
bump `package.json` — the workflow rewrites it during the publish step. The
value in `package.json` on main is a placeholder.

## One-time setup

1. **Claim the npm scope.** Either an org (`npm org create eddyq`) or publish
   under your personal scope.
2. **Generate a publish token** on npmjs.com with read+write access to the
   `@eddyq` scope. Prefer a "Granular Access Token" scoped to `@eddyq/*`.
3. **Add `NPM_TOKEN` to GitHub repo secrets.**
   `Settings → Secrets and variables → Actions → New repository secret`.

## Cutting a release

Everything happens in the GitHub UI.

1. **Releases tab → Draft a new release.**
2. Click **"Choose a tag"**, type `v0.1.0`, pick **"Create new tag: v0.1.0 on publish"**.
3. Target: `main` (or whatever branch the release reflects).
4. **Write the release notes** — this is the body you want displayed on the
   Releases page and GitHub will surface it in the repo sidebar. The workflow
   does not touch it.
5. Hit **Publish release**.

That's it. Publishing the release pushes the tag, which triggers the
workflow. From there:

- `build` matrix runs across 7 targets (~15 min total)
- `publish` derives the version from the tag, rewrites both `package.json`
  versions, downloads every `.node` artifact into per-target `npm/` subdirs,
  and publishes `@eddyq/queue` (+ 7 per-platform packages) and `@eddyq/nestjs`
  to npm

## Testing a release without publishing

Actions tab → Release → "Run workflow" → leave `dry-run` as `true`.
Runs the full 7-target build and uploads artifacts but skips the publish
job. Useful after changing dependencies or adding a target.

Setting `dry-run: false` on a `workflow_dispatch` runs the full publish, but
only against whatever tag is on the chosen branch's tip — if no tag is
there, it fails fast.

## Verifying after publish

```bash
# in a fresh dir
pnpm init -y
pnpm add @eddyq/queue @eddyq/nestjs
node -e "const { version } = require('@eddyq/queue'); console.log(version())"
```

If the install fails with "no prebuilt binary for this platform", the
per-platform package for your OS/arch didn't publish — check the
`publish` job logs for which target failed.

## Rolling back

npm unpublish is allowed **only within 72 hours** of publishing. Preferred:
publish a patch bump (`0.1.1`) that reverts the problem. If you must unpublish
within the window:

```bash
npm unpublish @eddyq/queue@0.1.0
npm unpublish @eddyq/queue-darwin-arm64@0.1.0   # and the other 6
npm unpublish @eddyq/nestjs@0.1.0
```

## Rust crates (separate pipeline)

The Rust side (`eddyq-core`, `eddyq-client`, `eddyq-cli`) publishes to crates.io
via `cargo release` — configured in `release.toml` with `publish = false` as a
safety until the crates are ready for public release. Those go out separately
from the npm packages.
