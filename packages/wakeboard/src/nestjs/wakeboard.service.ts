import { Injectable } from '@nestjs/common';
import { InjectEddyq, type EddyqInstance } from '@eddyq/nestjs';
import type {
  Eddyq,
  EddyqApp,
  EddyqRedis,
  Group,
  JobList,
  JobStats,
  ListJobsFilter,
  NamedQueue,
  Pagination,
  Schedule,
} from '@eddyq/queue';

type Provider = 'postgres' | 'redis';

// Module-level type guards. They narrow the instance directly — no `as`
// casts at the call sites, and they survive private-field intersection
// quirks that bite `this is …` predicates.
function isApp(q: EddyqInstance): q is EddyqApp {
  return typeof (q as { providerFor?: unknown }).providerFor === 'function';
}
function isPgOnly(q: EddyqInstance): q is Eddyq {
  return !isApp(q) && typeof (q as { migrate?: unknown }).migrate === 'function';
}
function isRedisOnly(q: EddyqInstance): q is EddyqRedis {
  return !isApp(q) && !isPgOnly(q);
}

/**
 * `EddyqApp.providerFor` returns a `string` over NAPI (no Rust enum
 * round-trip). This helper validates + narrows it once so call sites
 * don't sprinkle `as Provider` everywhere.
 */
function providerForQueue(app: EddyqApp, name: string): Provider {
  const p = app.providerFor(name);
  if (p !== 'postgres' && p !== 'redis') {
    throw new Error(`wakeboard: unexpected provider ${JSON.stringify(p)} for queue ${name}`);
  }
  return p;
}

/**
 * Dashboard-facing service. Works against any backend shape — single-PG
 * (`Eddyq`), single-Redis (`EddyqRedis`), or multi-backend (`EddyqApp`).
 *
 * Routing strategy:
 *   - listings (queues / groups / schedules / stats): when both backends
 *     are configured, the dashboard scopes each request with `?provider=`
 *     so each tab on the frontend asks for one backend's slice cleanly.
 *     Without a `provider`, the no-arg listings fall back to the
 *     `defaultProvider`.
 *   - admin actions: the caller passes `provider`; on single-backend
 *     instances it's ignored.
 *
 * Schedule names are expected to be namespaced per-provider when both
 * backends are in play (cron names live in their own keyspace each).
 */
@Injectable()
export class WakeboardService {
  constructor(@InjectEddyq() private readonly q: EddyqInstance) {}

  // --- providers / defaults ------------------------------------------------

  providers(): Provider[] {
    const q = this.q;
    if (isApp(q)) {
      const out: Provider[] = [];
      if (q.hasPostgres) out.push('postgres');
      if (q.hasRedis) out.push('redis');
      return out;
    }
    return isPgOnly(q) ? ['postgres'] : ['redis'];
  }

  private defaultProvider(): Provider {
    return this.providers()[0]!;
  }

  // --- stats ---------------------------------------------------------------

  async getStats(): Promise<JobStats> {
    const q = this.q;
    if (!isApp(q)) return q.getStats();
    // Multi-backend: union the per-provider snapshots. `byQueueState` is a
    // flat (queue, state, count) histogram and queue names aren't globally
    // unique across backends, so we just concat — each row is already
    // self-describing.
    const parts = await Promise.all(this.providers().map((p) => q.getStatsFor(p)));
    return { byQueueState: parts.flatMap((s) => s.byQueueState) };
  }

  getStatsFor(provider: Provider): Promise<JobStats> {
    const q = this.q;
    if (isApp(q)) return q.getStatsFor(provider);
    // Single-backend instance: the requested provider must match what's
    // configured, otherwise return an empty snapshot.
    if (!this.providers().includes(provider)) {
      return Promise.resolve({ byQueueState: [] });
    }
    return q.getStats();
  }

  // --- listings ------------------------------------------------------------

  async listJobs(
    filter: ListJobsFilter = {},
    pagination: Pagination = { limit: 50, offset: 0 },
    provider?: Provider,
  ): Promise<JobList> {
    const q = this.q;
    if (!isApp(q)) return q.listJobs(filter, pagination);
    if (provider !== undefined) return q.listJobsFor(provider, filter, pagination);
    // With a queue filter, the app's routed `listJobs` already picks the
    // right backend — no merge needed.
    if (filter.queue) return q.listJobs(filter, pagination);

    // Multi-backend union by keyset cursor. Each backend is asked for the
    // top `limit` rows older than `filter.beforeCreatedAt` (the caller's
    // cursor); we merge by `createdAt` desc and take the top `limit`. The
    // next-page cursor is the smallest `createdAt` we return — the
    // frontend just passes it back as `beforeCreatedAt`.
    //
    // Why this is exact: every union row older than the cursor must appear
    // in at least one backend's top-K-before-cursor result, because each
    // backend returns its own K newest-before-cursor rows. So the top K of
    // the union ⊆ ⋃ (top K of each). O(K) work per page, regardless of
    // depth — no offset drift, no over-fetch.
    const limit = pagination.limit ?? 50;
    const targets = this.providers();
    const parts = await Promise.all(
      targets.map((p) => q.listJobsFor(p, filter, { limit, offset: 0 })),
    );
    const merged = parts
      .flatMap((p) => p.rows)
      .sort((a, b) => b.createdAt.localeCompare(a.createdAt))
      .slice(0, limit);
    const total = parts.reduce((sum, p) => sum + p.total, 0);
    return { total, rows: merged };
  }

  async listQueues(provider?: Provider): Promise<NamedQueue[]> {
    const q = this.q;
    if (!isApp(q)) return q.listNamedQueues();
    const targets = provider !== undefined ? [provider] : this.providers();
    const out: NamedQueue[] = [];
    for (const p of targets) out.push(...(await q.listNamedQueues(p)));
    return out;
  }

  async listGroups(provider?: Provider): Promise<Group[]> {
    const q = this.q;
    if (!isApp(q)) return q.listGroups();
    const targets = provider !== undefined ? [provider] : this.providers();
    const out: Group[] = [];
    for (const p of targets) out.push(...(await q.listGroups(p)));
    return out;
  }

  async listSchedules(provider?: Provider): Promise<Schedule[]> {
    const q = this.q;
    if (!isApp(q)) return q.listSchedules();
    const targets = provider !== undefined ? [provider] : this.providers();
    const out: Schedule[] = [];
    for (const p of targets) out.push(...(await q.listSchedules(p)));
    return out;
  }

  // --- admin actions -------------------------------------------------------

  cancelJob(id: number, provider?: Provider): Promise<boolean> {
    const q = this.q;
    if (isApp(q)) return q.cancel(id, provider ?? this.defaultProvider());
    return q.cancel(id);
  }

  pauseQueue(name: string, provider?: Provider): Promise<void> {
    const q = this.q;
    if (isApp(q)) return q.pauseQueue(provider ?? providerForQueue(q, name), name);
    return q.pauseQueue(name);
  }

  resumeQueue(name: string, provider?: Provider): Promise<void> {
    const q = this.q;
    if (isApp(q)) return q.resumeQueue(provider ?? providerForQueue(q, name), name);
    return q.resumeQueue(name);
  }

  setQueueConcurrency(name: string, max: number, provider?: Provider): Promise<void> {
    const q = this.q;
    if (isApp(q)) return q.setQueueConcurrency(provider ?? providerForQueue(q, name), name, max);
    return q.setQueueConcurrency(name, max);
  }

  pauseGroup(key: string, provider?: Provider): Promise<void> {
    const q = this.q;
    if (isApp(q)) return q.pauseGroup(provider ?? this.defaultProvider(), key);
    return q.pauseGroup(key);
  }

  resumeGroup(key: string, provider?: Provider): Promise<void> {
    const q = this.q;
    if (isApp(q)) return q.resumeGroup(provider ?? this.defaultProvider(), key);
    return q.resumeGroup(key);
  }

  setGroupConcurrency(key: string, max: number, provider?: Provider): Promise<void> {
    const q = this.q;
    if (isApp(q)) return q.setGroupConcurrency(provider ?? this.defaultProvider(), key, max);
    return q.setGroupConcurrency(key, max);
  }

  setGroupRate(key: string, n: number, ms: number, provider?: Provider): Promise<void> {
    const q = this.q;
    if (isApp(q)) return q.setGroupRate(provider ?? this.defaultProvider(), key, n, ms);
    return q.setGroupRate(key, n, ms);
  }

  clearGroupRate(key: string, provider?: Provider): Promise<void> {
    const q = this.q;
    if (isApp(q)) return q.clearGroupRate(provider ?? this.defaultProvider(), key);
    return q.clearGroupRate(key);
  }

  setScheduleEnabled(name: string, enabled: boolean, _provider?: Provider): Promise<boolean> {
    const q = this.q;
    if (isApp(q)) {
      // No setScheduleEnabled on EddyqApp — schedules live per-backend and
      // names aren't globally unique. Document and surface a clear error
      // rather than guess the wrong backend.
      return Promise.reject(
        new Error(
          'setScheduleEnabled on EddyqApp is not supported — manage schedules per-provider',
        ),
      );
    }
    return q.setScheduleEnabled(name, enabled);
  }

  removeSchedule(name: string, provider?: Provider): Promise<boolean> {
    const q = this.q;
    if (isApp(q)) return q.removeSchedule(provider ?? this.defaultProvider(), name);
    return q.removeSchedule(name);
  }
}
