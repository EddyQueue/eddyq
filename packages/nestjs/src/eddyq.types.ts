import type { ConnectOptions, Eddyq, JobCall } from "@eddyq/queue";
import type { ModuleMetadata, Type } from "@nestjs/common";

export type { ConnectOptions, Eddyq, JobCall };

/**
 * A JS worker handler — async function invoked with the decoded JobCall. Throw
 * to fail / retry; throw a `CancelError` / `RetryError` (from `@eddyq/queue`)
 * for directives.
 */
export type JobHandlerFn = (
  call: JobCall & { signal: AbortSignal },
) => Promise<unknown> | unknown;

/** Options accepted by `EddyqModule.forRoot`. */
export interface EddyqModuleOptions {
  /** Postgres URL. Required. */
  databaseUrl: string;

  /** Pool / migration-line options forwarded to `Eddyq.connect`. */
  connectOptions?: ConnectOptions;

  /** Max in-flight jobs per Node process. Default from core: 10. */
  workerConcurrency?: number;

  /** Named queues to subscribe this worker to. Default `["default"]`. */
  subscribeTo?: string[];

  /** Millisecond budget for graceful shutdown before force-cancelling. Default 30_000. */
  gracefulShutdownMs?: number;

  /**
   * Call `eddyq.start()` automatically during `onApplicationBootstrap`. Default `true`.
   * Set `false` if you want to register handlers dynamically before starting.
   */
  autoStart?: boolean;

  /**
   * Skip the pending-migration guard `start()` normally enforces. Default `false`.
   * See `StartOptions.skipMigrationCheck` in @eddyq/queue.
   */
  skipMigrationCheck?: boolean;

  /**
   * Run pending migrations before `start()`. Default `false` — migrations are
   * a deploy-step concern, not a runtime one. Flip on only for toy apps or tests.
   */
  runMigrations?: boolean;
}

/** Async-config shape for `EddyqModule.forRootAsync`. */
export interface EddyqModuleAsyncOptions
  extends Pick<ModuleMetadata, "imports"> {
  useFactory: (
    ...args: unknown[]
  ) => Promise<EddyqModuleOptions> | EddyqModuleOptions;
  inject?: Array<string | symbol | Type<unknown>>;
}
