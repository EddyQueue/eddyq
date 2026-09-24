import {
  Inject,
  Injectable,
  Logger,
  Module,
  type OnModuleDestroy,
  type Type,
} from "@nestjs/common";
import { ModulesContainer } from "@nestjs/core";

import { EDDYQ_INSTANCE, EDDYQ_OPTIONS } from "./eddyq.constants.js";
import type { EddyqInstance, EddyqModuleOptions } from "./eddyq.types.js";

/**
 * Owns the "is the worker runtime running" flag so the drain can be
 * triggered from whichever hook fires first, exactly once.
 */
@Injectable()
export class EddyqWorkerRuntime {
  // Same context as before the split so log lines read `[EddyqModule] …`.
  private static readonly logger = new Logger("EddyqModule");
  private running = false;
  private stopping: Promise<void> | undefined;

  constructor(
    @Inject(EDDYQ_OPTIONS) private readonly options: EddyqModuleOptions,
    @Inject(EDDYQ_INSTANCE) private readonly queue: EddyqInstance,
  ) {}

  markStarted(): void {
    this.running = true;
  }

  /** Idempotent: concurrent/later callers await the same drain. */
  stop(): Promise<void> {
    if (!this.running) return this.stopping ?? Promise.resolve();
    this.running = false;
    EddyqWorkerRuntime.logger.log("stopping worker runtime");
    this.stopping = this.queue
      .shutdown({
        mode: this.options.shutdownMode ?? "drain",
        gracefulTimeoutMs: this.options.gracefulShutdownMs ?? 30_000,
      })
      .catch((e: unknown) => {
        EddyqWorkerRuntime.logger.error(
          `worker shutdown failed: ${(e as Error).message}`,
        );
      });
    return this.stopping;
  }
}

/**
 * Drains the worker runtime before any other module is torn down.
 *
 * Nest's shutdown runs `onModuleDestroy` → `beforeApplicationShutdown` →
 * `onApplicationShutdown`, and within each phase walks modules by import
 * distance from the root (root is 1, global modules are `MAX_VALUE`). User
 * code commonly closes its pools in `onModuleDestroy`, so no hook on the
 * global `EddyqModule` fires early enough — in-flight handlers would lose
 * their DB or cache mid-job.
 *
 * The walk direction differs by major: Nest 11+ tears down root-first
 * (ascending distance), Nest 10 runs every phase deepest-first
 * (descending). Rather than sniff the version, one drain module sits at
 * each extreme; whichever runs first drains and the other finds nothing
 * left to stop. They have no init hooks, so where they sort on boot is
 * irrelevant. The distance must be set at construction: Nest caches the
 * sorted module list the first time it runs a lifecycle hook.
 */
function drainModuleAt(distance: number, name: string): Type<OnModuleDestroy> {
  @Module({})
  class EddyqDrainModule implements OnModuleDestroy {
    constructor(
      modules: ModulesContainer,
      private readonly runtime: EddyqWorkerRuntime,
    ) {
      for (const moduleRef of modules.values()) {
        if (moduleRef.metatype === EddyqDrainModule) moduleRef.distance = distance;
      }
    }

    onModuleDestroy(): Promise<void> {
      return this.runtime.stop();
    }
  }
  Object.defineProperty(EddyqDrainModule, "name", { value: name });
  return EddyqDrainModule;
}

export const EDDYQ_DRAIN_MODULES = [
  drainModuleAt(-1, "EddyqDrainModule"),
  drainModuleAt(Infinity, "EddyqDrainModuleNest10"),
];
