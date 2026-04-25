# Migrations

::: danger Migrations are a deploy step, not auto-applied
eddyq does **not** run migrations at app boot. Slow migrations (index builds, table rewrites) would block every replica's startup. Apply migrations as an explicit deploy step, then boot workers.
:::

`eddyq.start()` refuses to boot if migrations are pending and tells you exactly how to fix it. That's the safety net.

## CLI

`@eddyq/queue` ships an `eddyq` bin. After installing the package:

```bash
npx eddyq migrate run    --database-url "$DATABASE_URL"
npx eddyq migrate list   --database-url "$DATABASE_URL"
npx eddyq migrate down   --database-url "$DATABASE_URL" --max-steps 1 --confirm
```

`--database-url` falls back to `$DATABASE_URL`. `migrate down` without `--confirm` is a dry-run that prints what it *would* roll back.

Wire it into your deploy pipeline:

```yaml
# deploy.yml (sketch)
- run: npx eddyq migrate run
- run: systemctl restart workers.service
```

## From a Node script

If you'd rather migrate inline — e.g. inside a custom deploy script that already has `DATABASE_URL` loaded:

```ts
// scripts/migrate.ts
import { Eddyq } from '@eddyq/queue'

const q = await Eddyq.connect(process.env.DATABASE_URL!)
const report = await q.migrate()
console.log('applied:', report.applied.map(m => `${m.version}:${m.name}`))
await q.close()
```

`migrate()` is idempotent and holds a `pg_advisory_lock` per migration line, so running it from two deploy hosts at once serializes safely.

## Migration lines

A **line** is a named migration track. Default is `"main"` — every connection uses the same schema unless told otherwise.

```ts
const q = await Eddyq.connect(url, { line: 'experimental' })
```

Lines exist for two real use cases:

1. **Schema versioning across major upgrades.** Run `v1` and `v2` workers side-by-side during a rollout, each pinned to its own line. No coordinated cutover.
2. **Multi-tenant isolation by schema.** Different tenants on different lines if you need hard isolation of their job tables. Rare — group concurrency usually solves the same problem better and at lower cost.

Lines are mostly an advanced escape hatch. If you don't have a specific reason to use them, leave the default.

`migrationStatus()` and `migrate()` operate on the line your client was built for — no cross-line bleed.
