import type {
  BatchEnqueueOutcome,
  BulkEnqueueOutcome,
  ConnectOptions,
  Eddyq,
  EddyqApp,
  EddyqRedis,
  EnqueueBatchInput,
  EnqueueManyItem,
  EnqueueOptions,
  EnqueueOutcome,
  JobCall,
  RedisConnectOptions,
  ScheduleDeclaration,
  StartOptions,
} from "@eddyq/queue";
import type { ModuleMetadata, Type } from "@nestjs/common";

export type {
  ConnectOptions,
  Eddyq,
  EddyqApp,
  EddyqRedis,
  JobCall,
  RedisConnectOptions,
  ScheduleDeclaration,
  StartOptions,
};

/**
 * The runtime client the module manages. Three shapes:
 *   - `Eddyq`        — Postgres only (`databaseUrl` set on forRoot)
 *   - `EddyqRedis`   — Redis only (`redis` set on forRoot)
 *   - `EddyqApp`     — both backends + per-queue routing (`databaseUrl`,
 *                      `redis`, and `queues` all set)
 *
 * The module guards PG-only call sites at runtime so all three shapes
 * coexist in one module definition.
 */
export type EddyqInstance = Eddyq | EddyqRedis | EddyqApp;

/** Redis backend config block for `EddyqModule.forRoot`. */
export interface EddyqRedisOptions {
  /** `redis://...` URL. */
  url: string;
  /** Hash-tag namespace ("line"). Default `"main"`. */
  line?: string;
}

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

/** Per-queue → provider binding for multi-backend setups. */
export interface EddyqQueueRoute {
  /** Queue name (must match what `@InjectQueue` / `enqueue({ queue })` uses). */
  name: string;
  /** Which backend this queue lives on. */
  provider: "postgres" | "redis";
}

/** Options accepted by `EddyqModule.forRoot`. */
export interface EddyqModuleOptions {
  /**
   * Postgres URL. Set this for a Postgres-only app, or alongside `redis`
   * for a multi-backend app (route per-queue via `queues`).
   */
  databaseUrl?: string;

  /**
   * Redis backend config. Set this for a Redis-only app, or alongside
   * `databaseUrl` for a multi-backend app.
   */
  redis?: EddyqRedisOptions;

  /**
   * Per-queue → provider routing. Required when both `databaseUrl` and
   * `redis` are set; otherwise ignored. Queues not listed here fall back
   * to `defaultProvider`.
   *
   * Example: `queues: [{ name: "webhooks", provider: "redis" }, { name: "payments", provider: "postgres" }]`
   */
  queues?: EddyqQueueRoute[];

  /**
   * Default provider for queues not listed in `queues`. Required when both
   * backends are configured (or there's no way to pick on enqueue).
   * Ignored in single-backend setups.
   */
  defaultProvider?: "postgres" | "redis";

  /** Pool / migration-line options forwarded to `Eddyq.connect`. (Postgres only.) */
  connectOptions?: ConnectOptions;

  /** Max in-flight jobs per Node process. Default from core: 10. */
  workerConcurrency?: number;

  /** Named queues to subscribe this worker to. Default `["default"]`. */
  subscribeTo?: string[];

  /** Millisecond budget for graceful shutdown before force-cancelling. Default 30_000. */
  gracefulShutdownMs?: number;

  /**
   * How `onApplicationShutdown` releases the worker pool.
   *   - `"drain"` (default) — wait up to `gracefulShutdownMs` for in-flight
   *     handlers to finish. Best for routine deploys.
   *   - `"force"` — abort the runtime immediately and proactively reclaim
   *     any in-flight DB rows (set `running` → `pending`) so other pods
   *     pick them up without waiting for heartbeat sweep. Use when SIGKILL
   *     is imminent.
   *   - `"abandon"` — drop the runtime without touching DB rows. The next
   *     pod's heartbeat sweep recovers them after `staleAfter`. Use only
   *     on panic exits.
   */
  shutdownMode?: "drain" | "force" | "abandon";

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

/** Token-bucket rate limit applied to a group. Same shape as `setGroupRate`. */
export interface GroupRate {
  count: number;
  periodMs: number;
}

/**
 * Group profile — a (concurrency, rate) pair that can be applied to one or
 * more group keys. Profiles passed under `groups` on `registerQueue` are
 * configured idempotently at bootstrap. Profiles passed to `queue.group(key, profile)`
 * are configured lazily on first use and memoized per process.
 */
export interface GroupProfile {
  concurrency?: number;
  rate?: GroupRate;
}

/**
 * Per-enqueue defaults applied by a {@link QueueHandle} unless overridden by
 * the caller. Every field is optional — the queue handle merges these on top
 * of the eddyq defaults, then the caller's `EnqueueOptions` wins.
 */
export type QueueDefaults = Pick<
  EnqueueOptions,
  | "maxAttempts"
  | "maxStalledCount"
  | "priority"
  | "tags"
  | "removeOnComplete"
  | "removeOnFail"
>;

/** Per-enqueue overrides accepted by `QueueHandle.enqueue`. The `queue` field is bound by the handle and not accepted. */
export type QueueEnqueueOptions = Omit<EnqueueOptions, "queue">;

/** Per-item input for `QueueHandle.enqueueMany`. The `queue` field is bound by the handle and not accepted. */
export type QueueEnqueueManyItem = Omit<EnqueueManyItem, "queue">;

/** Input for `QueueHandle.enqueueBatch`. `queue` on items/onComplete is bound by the handle. */
export interface QueueEnqueueBatchInput {
  items: QueueEnqueueManyItem[];
  onComplete?: QueueEnqueueManyItem;
  metadata?: EnqueueBatchInput["metadata"];
}

/**
 * Per-queue handle returned by `@InjectQueue(name)`. Wraps the global Eddyq
 * client and pre-binds the queue name + per-queue defaults so call sites
 * don't need to repeat them.
 */
export interface QueueHandle {
  /** The queue name this handle is bound to. */
  readonly name: string;

  /** Enqueue one job onto this queue. */
  enqueue(
    kind: string,
    payload: unknown,
    options?: QueueEnqueueOptions,
  ): Promise<EnqueueOutcome>;

  /** Bulk-enqueue onto this queue. One Postgres round-trip for the batch. */
  enqueueMany(items: QueueEnqueueManyItem[]): Promise<BulkEnqueueOutcome>;

  /** Fan-in batch: items + optional onComplete callback. See `Eddyq.enqueueBatch`. */
  enqueueBatch(input: QueueEnqueueBatchInput): Promise<BatchEnqueueOutcome>;

  /**
   * Ad-hoc retention sweep. Deletes up to `limit`
   * finalized jobs in `state` older than `graceMs` milliseconds. Useful for
   * one-shot pruning from admin endpoints or maintenance scripts; routine
   * retention should go through the configured cleanup tick instead.
   *
   * Note: this is a *global* sweep, not scoped to this handle's queue name —
   * the underlying delete operates on `state + finalized_at`, not on queue.
   * Run it from one designated handle if you need a single entry point.
   */
  clean(
    graceMs: number,
    limit: number,
    state: "completed" | "failed" | "cancelled",
  ): Promise<number>;

  /**
   * Return a sub-handle pre-bound to a group key. The first call for a
   * given (groupKey, profile) configures the group via `setGroupConcurrency`
   * / `setGroupRate` and memoizes the result for the life of the process.
   *
   * `profile` may be a profile name registered on this queue (under
   * `registerQueue({ groups })`) or an inline {@link GroupProfile}.
   */
  group(
    groupKey: string,
    profile: string | GroupProfile,
  ): GroupedQueueHandle;
}

/** Sub-handle returned by `QueueHandle.group` — same enqueue surface, group key pre-bound. */
export interface GroupedQueueHandle {
  readonly name: string;
  readonly groupKey: string;
  enqueue(
    kind: string,
    payload: unknown,
    options?: Omit<QueueEnqueueOptions, "groupKey">,
  ): Promise<EnqueueOutcome>;
  enqueueMany(
    items: Array<Omit<QueueEnqueueManyItem, "groupKey">>,
  ): Promise<BulkEnqueueOutcome>;
}

/**
 * Argument to `EddyqModule.registerQueue` — declarative per-queue config that
 * the module aggregates at bootstrap.
 */
export interface QueueRegistration {
  /** Queue name. Must be unique across `registerQueue` calls in a single app. */
  name: string;

  /**
   * Defaults merged into every enqueue from this queue's handle. Caller's
   * options win on conflict.
   */
  defaults?: QueueDefaults;

  /**
   * Named group profiles. Configured idempotently at bootstrap (each profile
   * is treated as a static group whose key equals the profile name) AND
   * available by name via `queue.group(key, 'profile-name')`.
   */
  groups?: Record<string, GroupProfile>;

  /**
   * Cron schedules owned by this queue. Unioned with `forRoot.schedules` and
   * reconciled together — entries not present in the union are deleted.
   */
  schedules?: ScheduleDeclaration[];

  /**
   * Whether worker pods should subscribe to this queue when `subscribeTo` is
   * not explicitly set on `forRoot`. Default `true`. Set `false` for queues
   * that exist only for enqueueing in this process (rare).
   */
  subscribe?: boolean;
}
