import { Eddyq, EddyqApp, EddyqRedis } from "@eddyq/queue";
import {
  type BeforeApplicationShutdown,
  type DynamicModule,
  Global,
  Inject,
  Logger,
  Module,
  type OnApplicationBootstrap,
  type OnApplicationShutdown,
  type Provider,
} from "@nestjs/common";
import { DiscoveryModule } from "@nestjs/core";

import {
  EDDYQ_INSTANCE,
  EDDYQ_OPTIONS,
  getQueueRegistrationToken,
  getQueueToken,
} from "./eddyq.constants.js";
import { EddyqExplorer } from "./eddyq.explorer.js";
import { QueueHandleImpl } from "./eddyq-queue.handle.js";
import { EddyqQueueAggregator } from "./eddyq-queue.aggregator.js";
import type {
  EddyqInstance,
  EddyqModuleAsyncOptions,
  EddyqModuleOptions,
  QueueRegistration,
} from "./eddyq.types.js";

/**
 * The three runtime shapes share a `migrate()` method only on Postgres-only
 * (`Eddyq`) and multi-backend (`EddyqApp`) — `EddyqRedis` does not have
 * migrations. Use this guard before calling `migrate` / `close` etc.
 */
function hasPgPath(queue: EddyqInstance): queue is Eddyq | EddyqApp {
  return typeof (queue as { migrate?: unknown }).migrate === "function";
}

/** True when the runtime is the multi-backend container. */
function isApp(queue: EddyqInstance): queue is EddyqApp {
  return typeof (queue as { hasPostgres?: unknown }).hasPostgres !== "undefined";
}

/**
 * NestJS module for eddyq.
 *
 * Provides an `Eddyq` client as a global DI token, scans providers for
 * `@Processor()` + `@JobHandler(kind)` annotations at bootstrap, registers
 * each handler with `queue.work()`, and starts the worker runtime.
 *
 * Shutdown is split across two hooks so in-flight handlers can finish
 * cleanly even when they depend on resources owned by other modules
 * (Drizzle, ioredis, etc.):
 *
 *   1. `beforeApplicationShutdown` — drain the worker runtime. User
 *      modules have not yet torn down their pools, so handler code can
 *      still complete DB writes, cache reads, etc.
 *   2. `onModuleDestroy` (per user module) — user-owned pools close here.
 *   3. `onApplicationShutdown` — release the eddyq Postgres pool. Runs
 *      last; nothing else in the app needs it by this point.
 */
@Global()
@Module({})
export class EddyqModule
  implements
    OnApplicationBootstrap,
    BeforeApplicationShutdown,
    OnApplicationShutdown
{
  private static readonly logger = new Logger(EddyqModule.name);
  private started = false;

  constructor(
    @Inject(EDDYQ_OPTIONS) private readonly options: EddyqModuleOptions,
    @Inject(EDDYQ_INSTANCE) private readonly queue: EddyqInstance,
    private readonly explorer: EddyqExplorer,
    private readonly aggregator: EddyqQueueAggregator,
  ) {}

  static forRoot(options: EddyqModuleOptions): DynamicModule {
    return {
      module: EddyqModule,
      imports: [DiscoveryModule],
      providers: [
        { provide: EDDYQ_OPTIONS, useValue: options },
        eddyqInstanceProvider(),
        EddyqExplorer,
        EddyqQueueAggregator,
      ],
      exports: [EDDYQ_INSTANCE, EDDYQ_OPTIONS],
    };
  }

  static forRootAsync(options: EddyqModuleAsyncOptions): DynamicModule {
    return {
      module: EddyqModule,
      imports: [DiscoveryModule, ...(options.imports ?? [])],
      providers: [
        {
          provide: EDDYQ_OPTIONS,
          useFactory: options.useFactory,
          inject: options.inject ?? [],
        },
        eddyqInstanceProvider(),
        EddyqExplorer,
        EddyqQueueAggregator,
      ],
      exports: [EDDYQ_INSTANCE, EDDYQ_OPTIONS],
    };
  }

  /**
   * Register a queue's per-queue config + producer handle. Call from any
   * feature module — `EddyqModule.forRoot` (or `forRootAsync`) must be
   * imported once at the app root for the connection + worker runtime.
   *
   * Each call adds two providers to DI:
   *   - the {@link QueueRegistration} value (under a namespaced token), so
   *     the aggregator can collect it at bootstrap.
   *   - a {@link QueueHandle} (under `getQueueToken(name)`), injectable via
   *     `@InjectQueue(name)`.
   *
   * The module exports the queue-handle token so consumers in this feature
   * module can inject it without re-importing.
   */
  static registerQueue(registration: QueueRegistration): DynamicModule {
    validateQueueName(registration.name, "registerQueue.name");
    const regToken = getQueueRegistrationToken(registration.name);
    const handleToken = getQueueToken(registration.name);
    // Fresh anonymous class per call. Nest cannot reuse the same module class
    // across multiple dynamic-module factories when those factories don't all
    // satisfy that class's constructor deps — `EddyqModule` itself requires
    // `EddyqExplorer` / `EddyqQueueAggregator`, which only `forRoot` provides.
    // Same trick `BullModule.forFeature` uses.
    class EddyqRegisteredQueueModule {}
    Object.defineProperty(EddyqRegisteredQueueModule, "name", {
      value: `EddyqQueueModule:${registration.name}`,
    });
    return {
      module: EddyqRegisteredQueueModule,
      providers: [
        { provide: regToken, useValue: registration },
        {
          provide: handleToken,
          useFactory: (eddyq: EddyqInstance, reg: QueueRegistration) =>
            new QueueHandleImpl(eddyq, reg),
          inject: [EDDYQ_INSTANCE, regToken],
        },
      ],
      exports: [handleToken],
    };
  }

  async onApplicationBootstrap(): Promise<void> {
    if (this.options.runMigrations) {
      if (hasPgPath(this.queue)) {
        EddyqModule.logger.log("applying migrations…");
        // `EddyqApp.migrate()` returns `null` when no PG backend is wired.
        const report = await this.queue.migrate();
        if (report && report.applied.length > 0) {
          EddyqModule.logger.log(
            `applied ${report.applied.length} migration(s): ${report.applied
              .map((r) => `${r.version}:${r.name}`)
              .join(", ")}`,
          );
        }
      } else {
        EddyqModule.logger.log(
          "runMigrations=true ignored on Redis backend (no schema migrations)",
        );
      }
    }

    const aggregated = this.aggregator.collect();

    // Static groups declared on registerQueue — idempotent setGroup* calls.
    for (const { handle } of aggregated) {
      await handle.configureStaticGroups();
    }

    // Schedule reconciliation: union of forRoot.schedules + every
    // registerQueue's schedules. A per-queue schedule defaults its `queue`
    // to the enclosing queue's name — callers can override per entry.
    const perQueueSchedules = aggregated.flatMap(({ registration }) =>
      (registration.schedules ?? []).map((s) => ({
        ...s,
        queue: s.queue ?? registration.name,
      })),
    );
    const rootSchedules = this.options.schedules;
    if (rootSchedules !== undefined || perQueueSchedules.length > 0) {
      const combined = [...(rootSchedules ?? []), ...perQueueSchedules];
      if (isApp(this.queue)) {
        // Multi-backend: group schedules by which backend their `queue`
        // routes to, then sync each backend independently. A queue without
        // a route lands on the default provider.
        const routes = new Map<string, "postgres" | "redis">();
        for (const r of this.options.queues ?? []) routes.set(r.name, r.provider);
        const defaultProvider = this.options.defaultProvider!;
        const groups: Record<"postgres" | "redis", typeof combined> = {
          postgres: [],
          redis: [],
        };
        for (const s of combined) {
          const provider = routes.get(s.queue ?? "default") ?? defaultProvider;
          groups[provider].push(s);
        }
        for (const provider of ["postgres", "redis"] as const) {
          if (groups[provider].length === 0) continue;
          const report = await this.queue.syncSchedules(provider, groups[provider]);
          EddyqModule.logger.log(
            `synced ${provider} schedules: upserted ${report.upserted}` +
              (report.deleted.length > 0
                ? `, deleted ${report.deleted.length} (${report.deleted.join(", ")})`
                : ""),
          );
        }
      } else {
        const report = await this.queue.syncSchedules(combined);
        EddyqModule.logger.log(
          `synced schedules: upserted ${report.upserted}` +
            (report.deleted.length > 0
              ? `, deleted ${report.deleted.length} (${report.deleted.join(", ")})`
              : ""),
        );
      }
    }

    const handlers = this.explorer.discover();
    for (const { kind, handler } of handlers) {
      this.queue.work(kind, handler as Parameters<Eddyq["work"]>[1]);
    }

    if (this.options.workerConcurrency !== undefined) {
      this.queue.setWorkerConcurrency(this.options.workerConcurrency);
    }

    // Subscription set: explicit `forRoot.subscribeTo` always wins. Otherwise
    // derive from registered queues (those without `subscribe: false`). If
    // nothing is registered, fall back to the core's "default" queue.
    const subscribeTo =
      this.options.subscribeTo ??
      (aggregated.length > 0
        ? aggregated
            .filter(({ registration }) => registration.subscribe !== false)
            .map(({ registration }) => registration.name)
        : undefined);
    if (subscribeTo !== undefined) {
      this.queue.subscribeTo(subscribeTo);
      EddyqModule.logger.log(
        `subscribeTo: [${subscribeTo.map((q) => `"${q}"`).join(", ")}]`,
      );
    }

    const autoStart = this.options.autoStart ?? true;
    if (!autoStart) {
      EddyqModule.logger.log(
        "autoStart=false — registered handlers but not starting. Call queue.start() manually.",
      );
      return;
    }

    if (handlers.length === 0) {
      // Nothing to run. Leave the client connected for enqueue-only use.
      return;
    }

    // Connection-budget warnings only make sense for the Postgres backend.
    // The Redis client is a managed connection multiplexer — no per-pod
    // pool sizing trade-off, no LISTEN socket. Skip the warning when this
    // is an `EddyqApp` since the warning would apply to only one half.
    if (hasPgPath(this.queue) && !isApp(this.queue)) {
      const pool = this.options.connectOptions?.maxConnections ?? 5;
      const concurrency = this.options.workerConcurrency ?? 10;
      const listenSocket = this.options.connectOptions?.pollOnly ? 0 : 1;
      const totalPerPod = pool + listenSocket;
      EddyqModule.logger.log(
        `connection budget: pool=${pool} concurrency=${concurrency} listen=${listenSocket} → ${totalPerPod}/pod` +
        ` — at N pods: N×${totalPerPod} connections to Postgres`,
      );
      if (concurrency > pool * 5) {
        EddyqModule.logger.warn(
          `workerConcurrency (${concurrency}) is high relative to maxConnections (${pool}). ` +
          `Jobs will queue waiting for pool slots under sustained load. ` +
          `Consider raising connectOptions.maxConnections or lowering workerConcurrency.`,
        );
      }
    }

    if (hasPgPath(this.queue)) {
      // `Eddyq.start` and `EddyqApp.start` both accept `StartOptions`;
      // `skipMigrationCheck` is harmless on the multi-backend path
      // (forwarded to the PG side, ignored by Redis).
      await this.queue.start({
        ...(this.options.tuning ?? {}),
        skipMigrationCheck: this.options.skipMigrationCheck,
      });
    } else {
      // EddyqRedis.start does not accept `skipMigrationCheck` (Redis has no
      // migrations). Pass only the shared tuning knobs.
      await this.queue.start(this.options.tuning ?? undefined);
    }
    this.started = true;
    EddyqModule.logger.log("worker runtime started");
  }

  // Drain runs in the *before* phase so user-owned resources (Drizzle,
  // ioredis, etc.) are still open while in-flight handlers finish. Nest
  // fires `onModuleDestroy` next, which is where those pools tear down.
  async beforeApplicationShutdown(signal?: string): Promise<void> {
    if (!this.started) return;
    const reason = signal ? `signal ${signal}` : "shutdown";
    EddyqModule.logger.log(`stopping worker runtime (${reason})`);
    try {
      await this.queue.shutdown({
        mode: this.options.shutdownMode ?? "drain",
        gracefulTimeoutMs: this.options.gracefulShutdownMs ?? 30_000,
      });
    } catch (e) {
      EddyqModule.logger.error(
        `worker shutdown failed: ${(e as Error).message}`,
      );
    }
    this.started = false;
  }

  async onApplicationShutdown(): Promise<void> {
    // Fallback: if Nest skipped `beforeApplicationShutdown` (e.g. user
    // never called `app.enableShutdownHooks()` before forcing close via
    // `app.close()`), drain here so we still cleanly release work.
    if (this.started) {
      try {
        await this.queue.shutdown({
          mode: this.options.shutdownMode ?? "drain",
          gracefulTimeoutMs: this.options.gracefulShutdownMs ?? 30_000,
        });
      } catch (e) {
        EddyqModule.logger.error(
          `worker shutdown failed: ${(e as Error).message}`,
        );
      }
      this.started = false;
    }
    if (hasPgPath(this.queue)) {
      try {
        // `Eddyq.close()` closes the PG pool; `EddyqApp.close()` does the
        // same when its PG backend is configured, no-ops otherwise.
        await this.queue.close();
      } catch (e) {
        EddyqModule.logger.error(
          `pool close failed: ${(e as Error).message}`,
        );
      }
    }
    // Redis backend has no explicit close — ConnectionManager is dropped
    // when this instance is GC'd, which Nest does after the shutdown hook.
  }
}

function eddyqInstanceProvider(): Provider {
  return {
    provide: EDDYQ_INSTANCE,
    useFactory: async (
      options: EddyqModuleOptions,
    ): Promise<EddyqInstance> => {
      const hasPg = typeof options.databaseUrl === "string" && options.databaseUrl.length > 0;
      const hasRedis = options.redis !== undefined;
      if (!hasPg && !hasRedis) {
        throw new Error(
          "@eddyq/nestjs: forRoot requires `databaseUrl`, `redis`, or both",
        );
      }
      // Multi-backend → construct `EddyqApp`. The `queues` routing table is
      // optional but typically required for non-default queues; without it
      // every queue lands on `defaultProvider`.
      if (hasPg && hasRedis) {
        if (!options.defaultProvider) {
          throw new Error(
            "@eddyq/nestjs: forRoot with both backends requires `defaultProvider` " +
              "('postgres' | 'redis')",
          );
        }
        return EddyqApp.connect({
          postgres: {
            databaseUrl: options.databaseUrl!,
            options: options.connectOptions ?? undefined,
          },
          redis: {
            url: options.redis!.url,
            line: options.redis!.line,
          },
          queues: (options.queues ?? []).map((q) => ({
            name: q.name,
            provider: q.provider,
          })),
          defaultProvider: options.defaultProvider,
        });
      }
      if (hasRedis && options.redis) {
        return EddyqRedis.connect(options.redis.url, {
          line: options.redis.line,
        });
      }
      return Eddyq.connect(options.databaseUrl!, options.connectOptions ?? undefined);
    },
    inject: [EDDYQ_OPTIONS],
  };
}

const MAX_QUEUE_NAME_LEN = 64;
const QUEUE_NAME_RE = /^[a-zA-Z0-9._-]+$/;

/**
 * Mirror of the Rust `validate_queue_name` rule. Kept in sync by hand —
 * the core's check still runs server-side, this fail-fast catches bad
 * inputs at module-construction time so the error surfaces during boot.
 */
function validateQueueName(name: unknown, label: string): asserts name is string {
  if (typeof name !== "string" || name.length === 0) {
    throw new Error(`@eddyq/nestjs: ${label} must be a non-empty string`);
  }
  if (name.length > MAX_QUEUE_NAME_LEN) {
    throw new Error(
      `@eddyq/nestjs: ${label} ${JSON.stringify(name)} exceeds ${MAX_QUEUE_NAME_LEN} chars`,
    );
  }
  if (!QUEUE_NAME_RE.test(name)) {
    throw new Error(
      `@eddyq/nestjs: ${label} ${JSON.stringify(name)} contains invalid characters ` +
        `(allowed: a-z A-Z 0-9 . _ -)`,
    );
  }
}
