# NestJS setup

```bash
pnpm add @eddyq/queue @eddyq/nestjs
```

Eddyq's NestJS integration has two pieces:

- `EddyqModule.forRoot(...)` — called **once** at the app root. Owns the connection, the worker runtime, and global tuning. There is no per-queue config here.
- `EddyqModule.registerQueue(...)` — called **per queue**, in the feature module that owns it. Declares the queue's defaults, named groups, and schedules. The handle is injected with `@InjectQueue(name)`.

## Root module

```ts
import { Module } from '@nestjs/common'
import { EddyqModule } from '@eddyq/nestjs'

@Module({
  imports: [
    EddyqModule.forRoot({
      databaseUrl: process.env.DATABASE_URL!,
    }),
  ],
})
export class AppModule {}
```

That's the entire root config for the common case. `forRoot` does **not** take a `queues:` array — queues are declared in their feature modules.

## Registering a queue

Each feature module registers the queue(s) it owns:

```ts
import { Module } from '@nestjs/common'
import { EddyqModule } from '@eddyq/nestjs'

import { EmailController } from './email.controller'
import { EmailProcessor } from './email.processor'

@Module({
  imports: [
    EddyqModule.registerQueue({
      name: 'email',
      defaults: { maxAttempts: 5 },
    }),
  ],
  controllers: [EmailController],
  providers: [EmailProcessor],
})
export class EmailModule {}
```

`defaults` is merged into every `enqueue` from this queue's handle — caller options still win on conflict.

`registerQueue` also accepts:

- `groups` — named (concurrency, rate) profiles configured idempotently at bootstrap and addressable by name via `queue.group(key, 'profileName')`.
- `schedules` — cron schedules scoped to this queue. Unioned with `forRoot.schedules` and reconciled together.
- `subscribe: false` — opt this queue out of being auto-subscribed by workers in this process (rare; for enqueue-only queues).

`registerQueue` validates names eagerly — empty strings, whitespace, special characters, or names over 64 chars throw at module import.

## Worker subscriptions are auto-derived

By default, the worker pulls from every queue declared via `registerQueue` in any module imported by the composition root. Splitting API and worker entry points becomes a matter of which feature modules each imports:

```ts
// app.module.ts — API pods enqueue and serve admin reads
@Module({
  imports: [
    EddyqModule.forRoot({ databaseUrl, autoStart: false }),
    EmailModule,
    ReportsModule,
  ],
})
export class AppModule {}

// workers.module.ts — worker pods process jobs
@Module({
  imports: [
    EddyqModule.forRoot({ databaseUrl }),
    EmailModule,        // worker subscribes to "email"
    ReportsModule,      // worker subscribes to "reports"
  ],
})
export class WorkersModule {}
```

To deploy differently-shaped fleets from the same image, pass `subscribeTo` to `forRoot` — that overrides the auto-derived list:

```ts
EddyqModule.forRoot({
  databaseUrl,
  subscribeTo: ['email'],   // this pod only handles email
})
```

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
      connectOptions: { maxConnections: 10 },
    }
  },
})
```

Pool tuning lives under `connectOptions` — see [`ConnectOptions`](/api/queue/interfaces/ConnectOptions). Worker-runtime tuning (sweep intervals, retention, lease durations) lives under `tuning`.

## Migrations

The module **does not** apply migrations on boot — same policy as `@eddyq/queue`. Run `npx eddyq migrate run` as a deploy step before booting your Nest app. See the main [Migrations](/guide/migrations) page.

## Next steps

- [Injecting the queue](./injecting) — `@InjectQueue(name)` and `QueueHandle`
- [Defining workers](./workers) — `@Processor()` and `@JobHandler('kind')`
- [Schedules](./schedules) — declare cron jobs per queue or at the root
