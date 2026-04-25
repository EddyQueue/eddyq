# Wakeboard

**Wakeboard** is the admin UI for eddyq — a single-page Svelte app that gives you live visibility into your queue: jobs in flight, queue depths, group concurrency state, schedule status, and per-job drill-downs with payload, attempts, and last error.

It ships as a NestJS module (`@eddyq/wakeboard`) that you mount inside your existing Nest app. No separate process, no extra infrastructure — it reads from the same Postgres your workers use.

::: tip Why a NestJS module?
Wakeboard piggy-backs on your existing API process. You already have an HTTP server, an auth story, and a deploy pipeline — Wakeboard plugs into all of them instead of asking you to run a separate dashboard service.
:::

## What you get

- **Job list** with filters (kind, queue, state, group, tag) and full-text search
- **Per-job detail** — payload, attempts, errors, lease info, retry history
- **Queues panel** — depth, in-flight, paused state; pause/resume controls
- **Groups panel** — concurrency caps, rate limits, current usage; live edits
- **Schedules panel** — next fire time per cron, manual fire button
- **Stats** — throughput, latency percentiles, failure rate

## Installation

```bash
pnpm add @eddyq/wakeboard
```

Then [mount it in your NestJS app](./nestjs).

## Auth

Wakeboard ships with HTTP Basic auth as a sensible default. Set `auth.password` in `forRoot()` and the module enforces it on every request to the mount path. Without `auth.password` set, the dashboard returns `503` — there is no "open by default" mode.

For production, you can:

- Use the built-in Basic auth (one shared password)
- Wire your existing Nest auth guards in front of the mount path (omit `auth` from `forRoot`)

See the [NestJS setup page](./nestjs) for both patterns.

## Self-hosted only

Wakeboard runs in your infrastructure, against your database. No external services, no telemetry phoning home, no SaaS vendor in the loop. The dashboard you serve is the dashboard your team uses.
