# Schedules

Cron-style recurring jobs, declared up-front and reconciled on `start()`.

```ts
const q = await Eddyq.connect(process.env.DATABASE_URL!, {
  schedules: [
    {
      name: 'nightly.cleanup',
      cron: '0 3 * * *',
      timezone: 'America/Los_Angeles',
      payload: { keepDays: 30 },
    },
  ],
})

q.work('nightly.cleanup', async ({ payload }) => {
  await deleteOldRows(payload.keepDays)
})

await q.start()
```

## How it works

Schedules are durable — they live in `eddyq_schedules`, not in your worker process. On `start()`, eddyq reconciles your declared schedules against the table:

- New schedules are inserted
- Removed schedules are tombstoned (no new runs, but in-flight runs finish)
- Cron expression or payload changes update the row in place

A single elected leader (the `MAINTENANCE_ROLE`) is responsible for inserting due jobs into the queue. After a deploy or leader handoff, missed runs are caught up automatically.

## Skipping missed runs

By default, missed runs (e.g. during a long deploy) are caught up. To skip them:

```ts
{
  name: 'send.daily.digest',
  cron: '0 9 * * *',
  catchup: false, // missed runs are dropped
}
```

## One-off scheduled jobs

For a single delayed job, prefer `enqueue({ delayMs })` over a schedule.
