# Lifecycle & shutdown

`EddyqModule` integrates with Nest's lifecycle hooks. You don't need to call `start()` or `shutdown()` manually.

## Boot

When Nest boots:

1. The DI container resolves the eddyq client and every `@InjectQueue` handle.
2. `forRoot` config is validated.
3. `eddyq_jobs` schema is checked — if migrations are pending, **boot fails** with a clear message. Run `npx eddyq migrate run` first.
4. Processors register with the engine and begin polling the queues derived from `registerQueue`.
5. Schedules (from `forRoot.schedules` and any `registerQueue({ schedules })`) are reconciled.

Set `autoStart: false` on `forRoot` to skip step 4 — useful for API pods that only enqueue.

## Shutdown

[Enable shutdown hooks](https://docs.nestjs.com/fundamentals/lifecycle-events#application-shutdown) in your `main.ts`:

```ts
async function bootstrap() {
  const app = await NestFactory.create(AppModule)
  app.enableShutdownHooks()
  await app.listen(3000)
}
bootstrap()
```

On `SIGINT` / `SIGTERM`:

1. Nest's shutdown sequence fires.
2. `EddyqModule` calls `eddyq.shutdown()` with `gracefulShutdownMs` (default 30_000).
3. In-flight handlers receive `signal.abort()` and have the grace window to unwind.
4. The DB pool closes.

## Configuring grace

```ts
EddyqModule.forRoot({
  databaseUrl: process.env.DATABASE_URL!,
  gracefulShutdownMs: 60_000, // give long-running jobs more time
})
```

## Shutdown mode

`shutdownMode` controls what happens to in-flight rows:

- `"drain"` (default) — wait up to `gracefulShutdownMs` for handlers to finish. Best for routine deploys.
- `"force"` — abort immediately and proactively reset in-flight rows (`running` → `pending`) so other pods pick them up without waiting for the heartbeat sweep. Use when SIGKILL is imminent.
- `"abandon"` — drop the runtime without touching DB rows. The next pod's heartbeat sweep recovers them after `staleAfter`. Panic-exits only.

## Health checks

Inject the raw client into a `HealthIndicator`:

```ts
import { InjectEddyq, type Eddyq } from '@eddyq/nestjs'

@Injectable()
export class EddyqHealth extends HealthIndicator {
  constructor(@InjectEddyq() private readonly eddyq: Eddyq) {
    super()
  }

  async check(key: string): Promise<HealthIndicatorResult> {
    const stats = await this.eddyq.stats()
    return this.getStatus(key, stats.healthy)
  }
}
```
