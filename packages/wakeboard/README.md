# @eddyq/wakeboard

Web dashboard for [eddyq](https://github.com/eddyqueue/eddyq). Drop-in NestJS
module + Svelte SPA — mount it under your app and stop writing one-off admin
endpoints.

```
pnpm add @eddyq/wakeboard
```

## What you get

- **Live stats** — counts by (queue, state).
- **Job inspection** — paginated listing with filters: state, queue, kind, group, tag.
- **Queue admin** — pause / resume / set concurrency / set timeout.
- **Group admin** — pause / resume / set concurrency cap / set token-bucket rate limit.
- **Schedule management** — enable / disable / remove cron + interval schedules.
- **Cancel pending jobs** — one click.
- **Backend selector** — when both Postgres and Redis are wired into one app, every tab gets a `?provider=` selector to scope the view.

## Quick start

```ts
import { Module } from "@nestjs/common";
import { EddyqModule } from "@eddyq/nestjs";
import { EddyqWakeboardModule } from "@eddyq/wakeboard";

@Module({
  imports: [
    EddyqModule.forRoot({
      databaseUrl: process.env.DATABASE_URL!,
      // …or `redis: { url, line }`, or both with `queues` + `defaultProvider`.
    }),
    EddyqWakeboardModule.forRoot({
      mountPath: "/wakeboard",
      auth: { password: process.env.WAKEBOARD_PASSWORD! },
    }),
  ],
})
export class AppModule {}
```

Hit `http://localhost:3000/wakeboard`. Done.

## Multi-backend support

Wakeboard auto-detects the backend shape from the `EddyqModule.forRoot`
config:

- **Postgres only** → one column of stats, one set of queues/groups/schedules.
- **Redis only** → same shape, just sourced from Redis.
- **Both** (`EddyqApp`) → a backend selector on every tab + a `GET /wakeboard/api/providers` endpoint listing the configured backends. The UI renders each backend's data scoped to its provider.

All admin actions accept `?provider=postgres|redis` so the right backend
handles each call. Queue / group keys don't bleed across backends — a
`tenant-acme` group on Redis is a different bucket than the same name on
Postgres, and the dashboard shows them as such.

## REST API

Everything the UI does is also a clean REST surface — wire it into your
own monitoring or CLI. All endpoints accept `?provider=postgres|redis` in
multi-backend setups.

| Endpoint | Purpose |
|---|---|
| `GET /api/providers` | Backends this app is wired to |
| `GET /api/stats` | Counts by (queue, state) |
| `GET /api/jobs?queue=&state=&kind=&groupKey=&tag=&page=` | Paginated listing (50/page) |
| `GET /api/queues` | Named-queue admin meta |
| `GET /api/groups` | Group meta (concurrency, paused, rate, tokens) |
| `GET /api/schedules` | Cron + interval schedules |
| `POST /api/jobs/:id/cancel` | Cancel pending |
| `POST /api/queues/:name/pause` / `:name/resume` | |
| `POST /api/groups/:key/pause` / `:key/resume` | |
| `POST /api/schedules/:name/enable` / `:name/disable` / `:name/remove` | |

## Security

The default password auth is intentionally simple — it's a
session-cookie-gated middleware. **Front it with your existing auth** for
production (a Nest middleware that checks your JWT, an ingress-level OIDC
proxy, an IP allowlist, etc.) and the `auth.password` option becomes a
fallback you can leave undefined.

## Requirements

- Node ≥ 20
- `@eddyq/nestjs` same minor version (peer dep)
- `@eddyq/queue` same minor version (peer dep)

## License

MIT
