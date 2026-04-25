# Lifecycle & shutdown

`EddyqModule` integrates with Nest's lifecycle hooks. You don't need to call `start()` or `shutdown()` manually.

## Boot

When Nest boots:

1. The DI container resolves the `EddyqQueue` provider.
2. `forRoot` config is validated.
3. `eddyq_jobs` schema is checked — if migrations are pending, **boot fails** with a clear message. Run `npx eddyq migrate run` first.
4. Workers register with the engine and begin polling.
5. Schedules are reconciled.

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
2. `EddyqModule` calls `q.shutdown(graceMs)` — defaults to 10 seconds, configurable in `forRoot`.
3. In-flight jobs receive `signal.abort()` and have `graceMs` to unwind.
4. The DB pool closes.

## Configuring grace

```ts
EddyqModule.forRoot({
  databaseUrl: process.env.DATABASE_URL!,
  shutdownGraceMs: 30_000, // give long-running jobs more time
})
```

## Health checks

Inject `EddyqQueue` into a `HealthIndicator`:

```ts
@Injectable()
export class EddyqHealth extends HealthIndicator {
  constructor(@InjectEddyq() private readonly queue: EddyqQueue) {
    super()
  }

  async check(key: string): Promise<HealthIndicatorResult> {
    const stats = await this.queue.stats()
    return this.getStatus(key, stats.healthy)
  }
}
```
