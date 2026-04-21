# 011. Migrations are a deploy step, not an app-boot side-effect

**Status:** Accepted
**Date:** 2026-04-21

## Context

eddyq owns its own schema and ships migrations. We need a policy for *when*
they run. The two ends of the spectrum:

1. **Auto-migrate on boot.** `queue.start()` notices pending migrations and
   applies them before accepting work. Best DX for solo / side-project users —
   "deploy the new version, it just works."
2. **Explicit deploy step.** Operators run a CLI or one-shot script to apply
   migrations, then start workers. Matches Rails, River, Oban, Prisma.

## Decision

**We follow pattern (2) exactly.** No auto-migrate on boot. App processes
that detect pending migrations refuse to start and tell the operator how to
fix it.

Supported flows:

- **CLI (recommended for prod deploys):**
  ```bash
  eddyq migrate run --database-url "$DATABASE_URL"
  ```
- **Node script (fine for Node-only deploys):**
  ```ts
  import pkg from '@eddyq/queue';
  const q = await pkg.Eddyq.connect(process.env.DATABASE_URL);
  await q.migrate();
  process.exit(0);
  ```
- **Bypass (if schema is managed out-of-band):**
  ```ts
  await eddyq.start({ skipMigrationCheck: true });
  ```

## Why

1. **Migrations can be slow.** Index builds on large tables take minutes.
   Coupling that to app boot means every replica spends minutes starting up.
2. **Concurrent races.** N replicas booting together would all try to apply
   DDL simultaneously. We'd have to solve concurrent-DDL safety anyway
   (advisory locks, which we do have — see ADR 003 follow-up) — but the
   operational model should never *require* that safety net to work right.
3. **DBA / change-review friendly.** Enterprises want DDL reviewed before it
   hits prod. An explicit step makes that natural.
4. **Rollback is harder with coupling.** If migration and app-code deploy
   atomically, you can't roll back just one side when one fails.
5. **Matches the industry.** Rails, River, Oban, Prisma, Ecto — every
   mature DB-backed tool picks the explicit step. Being a newer entrant is
   not a license to re-learn the lesson.

## Consequences

- **Added operator burden:** one extra deploy-pipeline step. Unavoidable.
- **Solo devs need more than "npm install + run".** For local dev, calling
  `await q.migrate()` at the top of their app *in dev mode only* is fine.
  In prod they should factor it into a deploy script.
- **We still need concurrent-DDL safety.** Even with an explicit deploy step,
  someone might run it twice or from two boxes. `migrate()` internally holds
  a `pg_advisory_lock` keyed per-line, so concurrent callers serialize.

## Alternatives considered

- **Auto-migrate + advisory lock:** technically safe, but encourages the
  "migration = app boot" anti-pattern. Rejected; see reason (1).
- **Auto-migrate with lock timeout:** `start()` bails if the migration lock
  is held. Mitigates the boot-blocking problem somewhat, but adds a confusing
  "sometimes start() works, sometimes it doesn't" mode. Rejected.
- **Two bindings, one with auto-migrate, one without:** splits the API for
  no good reason.

## References

- River's migration guide: <https://riverqueue.com/docs/migrations>
- Oban's migration guide: <https://hexdocs.pm/oban/installation.html>
- Prisma: `prisma migrate deploy` is explicit and separate from app start.
