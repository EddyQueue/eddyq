# Changesets

This directory holds pending changeset files for the `@eddyq/*` npm packages.

## Adding a changeset

When you make a user-visible change to a package, run:

```bash
pnpm changeset
```

This creates a markdown file in this directory describing the change and the semver bump. CI enforces that PRs touching `packages/*` also include a changeset (or an explicit `--empty` marker).

## Releasing

On merge to `main`, a maintainer runs:

```bash
pnpm changeset version   # apply pending bumps + write CHANGELOGs
pnpm changeset publish   # publish to npm
```

The Rust crates are released separately via `cargo release`.
