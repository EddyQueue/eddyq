<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { cancelJob, listJobs } from '../lib/api.js';
  import Badge from '../lib/components/Badge.svelte';
  import JobDetail from '../lib/components/JobDetail.svelte';
  import type { JobRow } from '../lib/types.js';

  const STATES = ['', 'pending', 'running', 'completed', 'failed', 'scheduled', 'cancelled'];
  const PAGE_SIZE = 50;

  let jobs: JobRow[] = [];
  let total = 0;
  let error = '';
  let cancelling = 0;
  let expanded: number | null = null;
  // Auto-expand detail rows when viewing a single state that has errors (e.g. failed).
  $: autoExpand = filters.state === 'failed';

  // Keyset cursor stack. Top of stack is the current page's `beforeCreatedAt`
  // (null = first page, newest jobs). "Older" pushes the oldest createdAt of
  // the current page; "Newer" pops back. Auto-refresh keeps re-fetching the
  // page at the current cursor so the top page stays live.
  let cursors: (string | null)[] = [null];
  $: cursor = cursors[cursors.length - 1];
  $: atTop = cursors.length === 1;
  $: hasMore = jobs.length === PAGE_SIZE;

  function parseHash() {
    const qs = window.location.hash.split('?')[1] ?? '';
    const p = new URLSearchParams(qs);
    return {
      queue:    p.get('queue')    ?? '',
      state:    p.get('state')    ?? '',
      kind:     p.get('kind')     ?? '',
      groupKey: p.get('groupKey') ?? '',
      tag:      p.get('tag')      ?? '',
    };
  }

  let filters = parseHash();

  function updateHash() {
    const p = new URLSearchParams();
    if (filters.queue)    p.set('queue',    filters.queue);
    if (filters.state)    p.set('state',    filters.state);
    if (filters.kind)     p.set('kind',     filters.kind);
    if (filters.groupKey) p.set('groupKey', filters.groupKey);
    if (filters.tag)      p.set('tag',      filters.tag);
    const qs = p.toString();
    window.location.hash = '#/jobs' + (qs ? '?' + qs : '');
    cursors = [null];
    load();
  }

  async function load() {
    const params: Record<string, string> = {};
    if (filters.queue)    params.queue    = filters.queue;
    if (filters.state)    params.state    = filters.state;
    if (filters.kind)     params.kind     = filters.kind;
    if (filters.groupKey) params.groupKey = filters.groupKey;
    if (filters.tag)      params.tag      = filters.tag;
    if (cursor)           params.before   = cursor;
    try {
      const result = await listJobs(params);
      jobs = result.rows;
      total = result.total;
      error = '';
    } catch (e) {
      error = (e as Error).message;
    }
  }

  function older() {
    if (jobs.length === 0) return;
    const oldest = jobs[jobs.length - 1].createdAt;
    cursors = [...cursors, oldest];
    load();
  }

  function newer() {
    if (cursors.length <= 1) return;
    cursors = cursors.slice(0, -1);
    load();
  }

  function newest() {
    cursors = [null];
    load();
  }

  async function doCancel(id: number) {
    cancelling = id;
    try { await cancelJob(id); await load(); }
    finally { cancelling = 0; }
  }

  function fmtDate(iso?: string) {
    if (!iso) return '—';
    return new Date(iso).toLocaleString('en-US', { dateStyle: 'short', timeStyle: 'medium', hour12: false });
  }

  let timer: ReturnType<typeof setInterval>;
  onMount(() => { load(); timer = setInterval(load, 5000); });
  onDestroy(() => clearInterval(timer));
</script>

<div class="space-y-4">
  <h1 class="text-xl font-semibold text-gray-200">Jobs</h1>

  <!-- Filters -->
  <div class="flex flex-wrap gap-3 items-end">
    <div class="flex flex-col gap-1">
      <label for="filter-queue" class="text-xs text-gray-500 uppercase tracking-wide">Queue</label>
      <input
        id="filter-queue"
        class="bg-gray-900 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 w-40 focus:outline-none focus:border-indigo-500"
        placeholder="all"
        bind:value={filters.queue}
        on:change={updateHash}
      />
    </div>
    <div class="flex flex-col gap-1">
      <label for="filter-state" class="text-xs text-gray-500 uppercase tracking-wide">State</label>
      <select
        id="filter-state"
        class="bg-gray-900 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 focus:outline-none focus:border-indigo-500"
        bind:value={filters.state}
        on:change={updateHash}
      >
        {#each STATES as s}
          <option value={s}>{s || 'all states'}</option>
        {/each}
      </select>
    </div>
    <div class="flex flex-col gap-1">
      <label for="filter-kind" class="text-xs text-gray-500 uppercase tracking-wide">Kind</label>
      <input
        id="filter-kind"
        class="bg-gray-900 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 w-40 focus:outline-none focus:border-indigo-500"
        placeholder="all"
        bind:value={filters.kind}
        on:change={updateHash}
      />
    </div>
    <div class="flex flex-col gap-1">
      <label for="filter-group" class="text-xs text-gray-500 uppercase tracking-wide">Group</label>
      <input
        id="filter-group"
        class="bg-gray-900 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 w-40 focus:outline-none focus:border-indigo-500"
        placeholder="all"
        bind:value={filters.groupKey}
        on:change={updateHash}
      />
    </div>
    <div class="flex flex-col gap-1">
      <label for="filter-tag" class="text-xs text-gray-500 uppercase tracking-wide">Tag</label>
      <input
        id="filter-tag"
        class="bg-gray-900 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 w-32 focus:outline-none focus:border-indigo-500"
        placeholder="all"
        bind:value={filters.tag}
        on:change={updateHash}
      />
    </div>
  </div>

  {#if error}<div class="text-red-400 text-sm">{error}</div>{/if}

  <div class="overflow-x-auto rounded-lg border border-gray-800">
    <table class="w-full text-sm">
      <thead class="bg-gray-900 text-gray-500 text-xs uppercase tracking-wide">
        <tr>
          <th class="text-left px-4 py-2 w-20">ID</th>
          <th class="text-left px-3 py-2">Queue</th>
          <th class="text-left px-3 py-2">Kind</th>
          <th class="text-left px-3 py-2">State</th>
          <th class="text-right px-3 py-2">Attempt</th>
          <th class="text-left px-3 py-2">Created</th>
          <th class="text-right px-4 py-2"></th>
        </tr>
      </thead>
      <tbody class="divide-y divide-gray-800">
        {#each jobs as job}
          <tr
            class="hover:bg-gray-900/50 transition-colors cursor-pointer"
            on:click={() => { expanded = expanded === job.id ? null : job.id; }}
          >
            <td class="px-4 py-2 tabular-nums text-gray-500 text-xs">{job.id}</td>
            <td class="px-3 py-2 font-mono text-gray-300">{job.queue}</td>
            <td class="px-3 py-2 font-mono text-gray-200">{job.kind}</td>
            <td class="px-3 py-2"><Badge state={job.state} /></td>
            <td class="text-right px-3 py-2 tabular-nums text-gray-400">{job.attempt}/{job.maxAttempts}</td>
            <td class="px-3 py-2 text-gray-400 text-xs tabular-nums">{fmtDate(job.createdAt)}</td>
            <td class="text-right px-4 py-2">
              {#if job.state === 'pending'}
                <button
                  class="px-2 py-0.5 text-xs rounded border border-red-800 text-red-400 hover:bg-red-900/30 disabled:opacity-50"
                  disabled={cancelling === job.id}
                  on:click|stopPropagation={() => doCancel(job.id)}
                >
                  {cancelling === job.id ? '…' : 'Cancel'}
                </button>
              {/if}
            </td>
          </tr>
          {#if expanded === job.id || autoExpand}
            <tr class="bg-gray-950">
              <td colspan="7" class="px-4 py-3">
                <JobDetail {job} />
              </td>
            </tr>
          {/if}
        {/each}
        {#if jobs.length === 0}
          <tr><td colspan="7" class="px-4 py-6 text-center text-gray-600">No jobs found.</td></tr>
        {/if}
      </tbody>
    </table>
  </div>

  <div class="flex items-center justify-between text-xs text-gray-500">
    <span class="tabular-nums">
      {total.toLocaleString()} matching{atTop ? '' : ` · page ${cursors.length}`}
    </span>
    <div class="flex items-center gap-2">
      <button
        class="px-3 py-1 rounded border border-gray-700 text-gray-300 hover:bg-gray-800 disabled:opacity-30 disabled:hover:bg-transparent"
        disabled={atTop}
        on:click={newest}
      >Newest</button>
      <button
        class="px-3 py-1 rounded border border-gray-700 text-gray-300 hover:bg-gray-800 disabled:opacity-30 disabled:hover:bg-transparent"
        disabled={atTop}
        on:click={newer}
      >← Newer</button>
      <button
        class="px-3 py-1 rounded border border-gray-700 text-gray-300 hover:bg-gray-800 disabled:opacity-30 disabled:hover:bg-transparent"
        disabled={!hasMore}
        on:click={older}
      >Older →</button>
    </div>
  </div>
</div>
