# Schedules

A schedule fires a job on a cron expression. The module reconciles declared schedules against the DB on boot — adding, removing, or editing a schedule in code propagates automatically. See the [Node schedules page](/node/schedules) for the full mechanics.

## Declaring schedules on a queue

The common case: a schedule belongs to a specific queue. Declare it on that queue's `registerQueue`:

```ts
@Module({
  imports: [
    EddyqModule.registerQueue({
      name: 'reports',
      schedules: [
        {
          name: 'daily-report',
          cronExpr: '0 0 8 * * *',  // sec min hour dom month dow — 08:00 UTC
          kind: 'report.generate',
          payload: { scope: 'daily' },
          // queue defaults to the enclosing registerQueue's name ("reports")
        },
      ],
    }),
  ],
  providers: [ReportsProcessor],
})
export class ReportsModule {}
```

The processor handles the fired job like any other:

```ts
@Processor()
export class ReportsProcessor {
  @JobHandler('report.generate')
  async generate({ payload }: JobCall) {
    const { scope } = payload as { scope: string }
    // ...
  }
}
```

## Declaring schedules at the root

Schedules that aren't tied to a single feature module can go on `forRoot`. They're unioned with the per-queue schedules and reconciled together:

```ts
EddyqModule.forRoot({
  databaseUrl: process.env.DATABASE_URL!,
  schedules: [
    {
      name: 'nightly-cleanup',
      cronExpr: '0 0 3 * * *',
      kind: 'cleanup.run',
      payload: { keepDays: 30 },
      queue: 'maintenance',
    },
  ],
})
```

`queue` is required at the root since there's no enclosing `registerQueue` to default to.

## Reconciliation

The DB is reconciled against the union of `forRoot.schedules` and every `registerQueue({ schedules })`. Entries in the DB that aren't in the union are **deleted**. Omit `schedules` everywhere if you're managing them imperatively via `eddyq.addSchedule(...)`.

## Async config

Schedules can come from a factory:

```ts
EddyqModule.forRootAsync({
  inject: [ConfigService],
  useFactory: (config: ConfigService) => ({
    databaseUrl: config.getOrThrow('DATABASE_URL'),
    schedules: config.get('eddyq.schedules', []),
  }),
})
```
