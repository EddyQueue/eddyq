import type {
  BatchEnqueueOutcome,
  BulkEnqueueOutcome,
  Eddyq,
  EnqueueOutcome,
} from "@eddyq/queue";
import { Logger } from "@nestjs/common";

import type {
  GroupProfile,
  GroupedQueueHandle,
  QueueDefaults,
  QueueEnqueueBatchInput,
  QueueEnqueueManyItem,
  QueueEnqueueOptions,
  QueueHandle,
  QueueRegistration,
} from "./eddyq.types.js";

const logger = new Logger("EddyqQueueHandle");

/**
 * Concrete queue handle backing `@InjectQueue(name)`. Pre-binds the queue
 * name + per-queue defaults so call sites don't restate them.
 *
 * Group memoization is per-handle (i.e. per-process): the first
 * `queue.group(key, profile)` call hits Postgres to call
 * `setGroupConcurrency` / `setGroupRate`; subsequent calls for the same
 * (groupKey, profile-fingerprint) skip the round-trip. Both ops are
 * idempotent server-side — concurrent boots are safe.
 */
export class QueueHandleImpl implements QueueHandle {
  readonly name: string;
  private readonly defaults: QueueDefaults;
  private readonly profiles: Record<string, GroupProfile>;
  // Keyed by `${groupKey}|${fingerprint}` — re-applies if the profile shape changes.
  private readonly configuredGroups = new Map<string, Promise<void>>();

  constructor(
    private readonly eddyq: Eddyq,
    registration: QueueRegistration,
  ) {
    this.name = registration.name;
    this.defaults = registration.defaults ?? {};
    this.profiles = registration.groups ?? {};
  }

  enqueue(
    kind: string,
    payload: unknown,
    options: QueueEnqueueOptions = {},
  ): Promise<EnqueueOutcome> {
    return this.eddyq.enqueue(kind, payload, {
      ...this.defaults,
      ...options,
      queue: this.name,
    });
  }

  enqueueMany(items: QueueEnqueueManyItem[]): Promise<BulkEnqueueOutcome> {
    return this.eddyq.enqueueMany(
      items.map((item) => ({
        ...this.defaults,
        ...item,
        queue: this.name,
      })),
    );
  }

  enqueueBatch(input: QueueEnqueueBatchInput): Promise<BatchEnqueueOutcome> {
    return this.eddyq.enqueueBatch({
      items: input.items.map((item) => ({
        ...this.defaults,
        ...item,
        queue: this.name,
      })),
      onComplete: input.onComplete
        ? { ...this.defaults, ...input.onComplete, queue: this.name }
        : undefined,
      metadata: input.metadata,
    });
  }

  group(groupKey: string, profile: string | GroupProfile): GroupedQueueHandle {
    const resolved = this.resolveProfile(profile);
    const configure = this.ensureGroupConfigured(groupKey, resolved);
    return new GroupedQueueHandleImpl(this, groupKey, configure);
  }

  /**
   * Configure the static (named) groups declared on `registerQueue`. Called
   * once at bootstrap by the module — each profile is applied with its name
   * as the group key, idempotently.
   */
  async configureStaticGroups(): Promise<void> {
    for (const [profileName, profile] of Object.entries(this.profiles)) {
      const groupKey = `${this.name}:${profileName}`;
      await this.applyProfile(groupKey, profile);
    }
  }

  // ----- Internals -----

  /** Used by GroupedQueueHandleImpl to round-trip the inherited eddyq client. */
  rawEnqueue(
    kind: string,
    payload: unknown,
    options: QueueEnqueueOptions,
  ): Promise<EnqueueOutcome> {
    return this.enqueue(kind, payload, options);
  }

  rawEnqueueMany(items: QueueEnqueueManyItem[]): Promise<BulkEnqueueOutcome> {
    return this.enqueueMany(items);
  }

  private resolveProfile(profile: string | GroupProfile): GroupProfile {
    if (typeof profile !== "string") return profile;
    const named = this.profiles[profile];
    if (!named) {
      throw new Error(
        `@eddyq/nestjs: queue "${this.name}" has no group profile named "${profile}". ` +
          `Declare it under registerQueue({ groups }) or pass an inline profile.`,
      );
    }
    return named;
  }

  private ensureGroupConfigured(
    groupKey: string,
    profile: GroupProfile,
  ): Promise<void> {
    const cacheKey = `${groupKey}|${fingerprint(profile)}`;
    const existing = this.configuredGroups.get(cacheKey);
    if (existing) return existing;
    const pending = this.applyProfile(groupKey, profile).catch((err) => {
      // Drop the failed promise from the cache so the next enqueue retries.
      this.configuredGroups.delete(cacheKey);
      throw err;
    });
    this.configuredGroups.set(cacheKey, pending);
    return pending;
  }

  private async applyProfile(
    groupKey: string,
    profile: GroupProfile,
  ): Promise<void> {
    try {
      if (profile.concurrency !== undefined) {
        await this.eddyq.setGroupConcurrency(groupKey, profile.concurrency);
      }
      if (profile.rate) {
        await this.eddyq.setGroupRate(
          groupKey,
          profile.rate.count,
          profile.rate.periodMs,
        );
      }
    } catch (err) {
      logger.warn(
        `failed to configure group "${groupKey}" on queue "${this.name}": ${
          (err as Error).message
        }`,
      );
      throw err;
    }
  }
}

class GroupedQueueHandleImpl implements GroupedQueueHandle {
  readonly name: string;
  readonly groupKey: string;

  constructor(
    private readonly parent: QueueHandleImpl,
    groupKey: string,
    private readonly configure: Promise<void>,
  ) {
    this.name = parent.name;
    this.groupKey = groupKey;
  }

  async enqueue(
    kind: string,
    payload: unknown,
    options: Omit<QueueEnqueueOptions, "groupKey"> = {},
  ): Promise<EnqueueOutcome> {
    await this.configure;
    return this.parent.rawEnqueue(kind, payload, {
      ...options,
      groupKey: this.groupKey,
    });
  }

  async enqueueMany(
    items: Array<Omit<QueueEnqueueManyItem, "groupKey">>,
  ): Promise<BulkEnqueueOutcome> {
    await this.configure;
    return this.parent.rawEnqueueMany(
      items.map((item) => ({ ...item, groupKey: this.groupKey })),
    );
  }
}

function fingerprint(profile: GroupProfile): string {
  // Stable shape for cache-key purposes — order matters here, so write
  // explicitly rather than JSON.stringify.
  const c = profile.concurrency ?? "_";
  const r = profile.rate ? `${profile.rate.count}/${profile.rate.periodMs}` : "_";
  return `${c}:${r}`;
}
