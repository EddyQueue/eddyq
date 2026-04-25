import type {
  ConnectOptions,
  Eddyq,
  JobCall,
  ScheduleDeclaration,
  StartOptions,
} from "@eddyq/queue";
import type { ModuleMetadata, Type } from "@nestjs/common";

export type { ConnectOptions, Eddyq, JobCall, ScheduleDeclaration, StartOptions };

/** Tuning knobs for the worker runtime — `StartOptions` minus the fields the module manages itself. */
export type EddyqTuningOptions = Omit<StartOptions, "skipMigrationCheck">;

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

  /**
   * Worker-runtime tuning forwarded to `eddyq.start()` — sweep/cleanup
   * intervals, retention windows, lease durations, etc. Omit to use defaults.
   * `skipMigrationCheck` is not included here; set it at the top level.
   */
  tuning?: EddyqTuningOptions;

  /**
   * Cron schedules declared in code. When provided, the module reconciles the
   * DB against this list at boot — entries are upserted, and any DB schedule
   * not in the list is **deleted**. Pass `[]` to delete all declared
   * schedules; omit to leave schedules untouched (useful when managing them
   * imperatively via `queue.addSchedule`).
   */
  schedules?: ScheduleDeclaration[];
}

/** Async-config shape for `EddyqModule.forRootAsync`. */
export interface EddyqModuleAsyncOptions
  extends Pick<ModuleMetadata, "imports"> {
  useFactory: (
    ...args: unknown[]
  ) => Promise<EddyqModuleOptions> | EddyqModuleOptions;
  inject?: Array<string | symbol | Type<unknown>>;
}
