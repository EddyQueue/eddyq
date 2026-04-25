# NestJS setup

```bash
pnpm add @eddyq/queue @eddyq/nestjs
```

## Register the module

```ts
import { Module } from '@nestjs/common'
import { EddyqModule } from '@eddyq/nestjs'

@Module({
  imports: [
    EddyqModule.forRoot({
      databaseUrl: process.env.DATABASE_URL!,
      schedules: [
        { name: 'nightly.cleanup', cron: '0 3 * * *' },
      ],
    }),
  ],
})
export class AppModule {}
```

## Subscribing to specific queues

By default, the worker pulls from the `default` queue. Pass `subscribeTo` to restrict (or expand) which named queues this pod processes — the SQL fetcher filters at claim time, so unsubscribed queues are invisible to this pod.

```ts
EddyqModule.forRoot({
  databaseUrl: process.env.DATABASE_URL!,
  subscribeTo: ['emails', 'webhooks'],   // this pod only handles these
})
```

A typical production split:

```ts
// api.module.ts — no autoStart (or no @JobHandler), just enqueues
EddyqModule.forRoot({ databaseUrl, autoStart: false })

// web-worker.module.ts — fast, latency-sensitive
EddyqModule.forRoot({ databaseUrl, subscribeTo: ['default'] })

// batch-worker.module.ts — long-running, large memory
EddyqModule.forRoot({ databaseUrl, subscribeTo: ['batch', 'reports'] })
```

See [Named queues](/node/queues) for the broader queue model.

## Async configuration

When you need values from `ConfigService` or another async source:

```ts
EddyqModule.forRootAsync({
  imports: [ConfigModule],
  inject: [ConfigService],
  useFactory: (config: ConfigService) => ({
    databaseUrl: config.getOrThrow('DATABASE_URL'),
  }),
})
```

## Connection options

`databaseUrl` is a single Postgres connection string — same as `pg`, `psql`, or the `DATABASE_URL` env var. There is no host/port/user/password breakdown. If your config is structured, compose it in the factory:

```ts
EddyqModule.forRootAsync({
  inject: [ConfigService],
  useFactory: (config: ConfigService) => {
    const { user, pass, host, port, db } = config.get('postgres')!
    return {
      databaseUrl: `postgres://${encodeURIComponent(user)}:${encodeURIComponent(pass)}@${host}:${port}/${db}`,
      maxConnections: 10,
    }
  },
})
```

Pool tuning (`maxConnections`, `pollOnly`, `acquireTimeoutMs`, etc.) is passed alongside `databaseUrl` — see [`ConnectOptions`](/api/queue/interfaces/ConnectOptions).

## Migrations

The module **does not** apply migrations on boot — same policy as `@eddyq/queue`. Run `npx eddyq migrate run` as a deploy step before booting your Nest app. See the main [Migrations](/guide/migrations) page.

## Next steps

- [Defining workers](./workers) — `@EddyqWorker` and `@EddyqProcess`
- [Injecting the queue](./injecting) — enqueue from controllers and services
- [Schedules](./schedules) — declare cron jobs in `forRoot`
