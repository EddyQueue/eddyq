# Schedules

Declare cron schedules in `EddyqModule.forRoot` — same shape as the Node API.

```ts
EddyqModule.forRoot({
  databaseUrl: process.env.DATABASE_URL!,
  schedules: [
    {
      name: 'nightly.cleanup',
      cron: '0 3 * * *',
      timezone: 'America/Los_Angeles',
      payload: { keepDays: 30 },
    },
    {
      name: 'send.daily.digest',
      cron: '0 9 * * *',
      catchup: false, // skip missed runs after a deploy
    },
  ],
})
```

The schedules table is reconciled on app boot — adding, removing, or editing a schedule in code propagates to the database. See the [Node schedules page](/node/schedules) for the full mechanics.

## Handling scheduled jobs

A schedule just enqueues a job with the given `name` and `payload`. Define the worker normally:

```ts
@EddyqWorker()
export class CleanupWorker {
  @EddyqProcess('nightly.cleanup')
  async clean(ctx: JobContext<{ keepDays: number }>) {
    await this.repo.deleteOlderThan(ctx.payload.keepDays)
  }
}
```

## Async config

Schedules can come from `useFactory` too:

```ts
EddyqModule.forRootAsync({
  inject: [ConfigService],
  useFactory: (config: ConfigService) => ({
    databaseUrl: config.getOrThrow('DATABASE_URL'),
    schedules: config.get('eddyq.schedules', []),
  }),
})
```
