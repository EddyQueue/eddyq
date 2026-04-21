# Releasing eddyq

This repo publishes two npm packages as a single unit:

- `@eddyq/queue` — ships a native binary per platform via NAPI-RS, so 7
  companion packages (`@eddyq/queue-darwin-arm64`, `@eddyq/queue-linux-x64-gnu`,
  …) are published alongside and wired up as `optionalDependencies`.
- `@eddyq/nestjs` — pure TypeScript, peer-depends on `@eddyq/queue`.

Both go out atomically from a single tag-triggered GitHub Actions run.

## One-time setup

1. **Claim the npm scope.** Either an org (`npm org create eddyq`) or publish
   under your personal scope.
2. **Generate a publish token** on npmjs.com with read+write access to the
   `@eddyq` scope. Prefer a "Granular Access Token" scoped to `@eddyq/*`.
3. **Add `NPM_TOKEN` to GitHub repo secrets.**
   `Settings → Secrets and variables → Actions → New repository secret`.
4. **Enable provenance** by making sure the repo is on a `pnpm`/Node version
   new enough to emit it (the workflow uses Node 22 + `--provenance`).
   Provenance also requires the workflow to run with
   `permissions: { id-token: write }` — already set in `release.yml`.

## Cutting a release

1. **Bump versions.** The workspace versions should match across both
   packages (they're published as a pair).
   ```bash
   # from the repo root
   pnpm -C packages/queue  version 0.1.0 --no-git-tag-version
   pnpm -C packages/nestjs version 0.1.0 --no-git-tag-version
   ```
2. **Commit + tag.**
   ```bash
   git add packages/queue/package.json packages/nestjs/package.json
   git commit -m "chore(release): 0.1.0"
   git tag v0.1.0
   git push origin main --tags
   ```
3. **Watch the workflow** at `https://github.com/<you>/eddyq/actions`.
   - `build` matrix runs across 7 targets (~15 min total)
   - `publish` downloads every `.node` artifact, runs
     `napi prepublish -t npm` (publishes the 7 per-platform packages),
     then `npm publish` the main `@eddyq/queue`, then
     `pnpm publish` for `@eddyq/nestjs`
   - A GitHub release is drafted automatically with generated notes

## Testing a release without publishing

`workflow_dispatch` with `dry-run: true` (the default) runs the full build
matrix and uploads artifacts but skips the publish job. Useful for verifying
cross-compile works after changing dependencies or targets.

```
Actions → Release → Run workflow → dry-run: true
```

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
