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
} from "./eddyq.constants.js";
import { EddyqExplorer } from "./eddyq.explorer.js";
import type {
  EddyqModuleAsyncOptions,
  EddyqModuleOptions,
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
  ) {}

  static forRoot(options: EddyqModuleOptions): DynamicModule {
    return {
      module: EddyqModule,
      imports: [DiscoveryModule],
      providers: [
        { provide: EDDYQ_OPTIONS, useValue: options },
        eddyqInstanceProvider(),
        EddyqExplorer,
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
      ],
      exports: [EDDYQ_INSTANCE, EDDYQ_OPTIONS],
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

    if (this.options.schedules !== undefined) {
      const declared = this.options.schedules;
      const report = await this.queue.syncSchedules(declared);
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
    if (this.options.subscribeTo !== undefined) {
      this.queue.subscribeTo(this.options.subscribeTo);
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
