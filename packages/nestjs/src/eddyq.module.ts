import { Eddyq } from "@eddyq/queue";
import {
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
  EddyqModuleAsyncOptions,
  EddyqModuleOptions,
  QueueRegistration,
} from "./eddyq.types.js";

/**
 * NestJS module for eddyq.
 *
 * Provides an `Eddyq` client as a global DI token, scans providers for
 * `@Processor()` + `@JobHandler(kind)` annotations at bootstrap, registers
 * each handler with `queue.work()`, and starts the worker runtime.
 *
 * On `onApplicationShutdown`, gracefully stops the worker runtime and closes
 * the Postgres pool.
 */
@Global()
@Module({})
export class EddyqModule implements OnApplicationBootstrap, OnApplicationShutdown {
  private static readonly logger = new Logger(EddyqModule.name);
  private started = false;

  constructor(
    @Inject(EDDYQ_OPTIONS) private readonly options: EddyqModuleOptions,
    @Inject(EDDYQ_INSTANCE) private readonly queue: Eddyq,
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
          useFactory: (eddyq: Eddyq, reg: QueueRegistration) =>
            new QueueHandleImpl(eddyq, reg),
          inject: [EDDYQ_INSTANCE, regToken],
        },
      ],
      exports: [handleToken],
    };
  }

  async onApplicationBootstrap(): Promise<void> {
    if (this.options.runMigrations) {
      EddyqModule.logger.log("applying migrations…");
      const report = await this.queue.migrate();
      if (report.applied.length > 0) {
        EddyqModule.logger.log(
          `applied ${report.applied.length} migration(s): ${report.applied
            .map((r) => `${r.version}:${r.name}`)
            .join(", ")}`,
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
      const report = await this.queue.syncSchedules(combined);
      EddyqModule.logger.log(
        `synced schedules: upserted ${report.upserted}` +
          (report.deleted.length > 0
            ? `, deleted ${report.deleted.length} (${report.deleted.join(", ")})`
            : ""),
      );
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

    await this.queue.start({
      ...(this.options.tuning ?? {}),
      skipMigrationCheck: this.options.skipMigrationCheck,
    });
    this.started = true;
    EddyqModule.logger.log("worker runtime started");
  }

  async onApplicationShutdown(signal?: string): Promise<void> {
    const reason = signal ? `signal ${signal}` : "shutdown";
    if (this.started) {
      EddyqModule.logger.log(`stopping worker runtime (${reason})`);
      try {
        await this.queue.shutdown(this.options.gracefulShutdownMs ?? 30_000);
      } catch (e) {
        EddyqModule.logger.error(
          `worker shutdown failed: ${(e as Error).message}`,
        );
      }
      this.started = false;
    }
    try {
      await this.queue.close();
    } catch (e) {
      EddyqModule.logger.error(
        `pool close failed: ${(e as Error).message}`,
      );
    }
  }
}

function eddyqInstanceProvider(): Provider {
  return {
    provide: EDDYQ_INSTANCE,
    useFactory: async (options: EddyqModuleOptions): Promise<Eddyq> =>
      Eddyq.connect(options.databaseUrl, options.connectOptions ?? undefined),
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
