<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { getStats, listQueues, pauseQueue, resumeQueue } from '../lib/api.js';
  import type { NamedQueue, QueueStateCount } from '../lib/types.js';

  let queues: NamedQueue[] = [];
  let statRows: QueueStateCount[] = [];
  let error = '';
  let acting = '';

  $: byQueue = statRows.reduce<Record<string, Record<string, number>>>((acc, r) => {
    if (!acc[r.queue]) acc[r.queue] = {};
    acc[r.queue][r.state] = r.count;
    return acc;
  }, {});

  async function load() {
    try {
      const [nqs, stats] = await Promise.all([listQueues(), getStats()]);
      queues = nqs;
      statRows = stats.byQueueState;
      error = '';
    } catch (e) {
      error = (e as Error).message;
    }
  }

  async function toggle(q: NamedQueue) {
    acting = q.name;
    try {
      if (q.paused) await resumeQueue(q.name);
      else await pauseQueue(q.name);
      await load();
    } finally {
      acting = '';
    }
  }

  let timer: ReturnType<typeof setInterval>;
  onMount(() => { load(); timer = setInterval(load, 5000); });
  onDestroy(() => clearInterval(timer));
</script>

<div class="space-y-4">
  <h1 class="text-xl font-semibold text-gray-200">Queues</h1>

  {#if error}<div class="text-red-400 text-sm">{error}</div>{/if}

  <div class="overflow-x-auto rounded-lg border border-gray-800">
    <table class="w-full text-sm">
      <thead class="bg-gray-900 text-gray-500 text-xs uppercase tracking-wide">
        <tr>
          <th class="text-left px-4 py-2">Name</th>
          <th class="text-right px-3 py-2">Running</th>
          <th class="text-right px-3 py-2">Concurrency</th>
          <th class="text-right px-3 py-2">Pending</th>
          <th class="text-right px-3 py-2">Failed</th>
          <th class="text-right px-3 py-2">Timeout</th>
          <th class="text-right px-4 py-2">Status</th>
        </tr>
      </thead>
      <tbody class="divide-y divide-gray-800">
        {#each queues as q}
          <tr class="hover:bg-gray-900/50 transition-colors">
            <td class="px-4 py-2 font-mono text-gray-200">
              <a href="#/jobs?queue={encodeURIComponent(q.name)}" class="hover:text-indigo-400">{q.name}</a>
            </td>
            <td class="text-right px-3 py-2 tabular-nums text-blue-300">{q.runningCount}</td>
            <td class="text-right px-3 py-2 tabular-nums text-gray-400">{q.maxConcurrency}</td>
            <td class="text-right px-3 py-2 tabular-nums text-yellow-300">
              {byQueue[q.name]?.pending?.toLocaleString() ?? '—'}
            </td>
            <td class="text-right px-3 py-2 tabular-nums text-red-300">
              {byQueue[q.name]?.failed?.toLocaleString() ?? '—'}
            </td>
            <td class="text-right px-3 py-2 tabular-nums text-gray-400">
              {q.defaultTimeoutMs != null ? `${q.defaultTimeoutMs}ms` : '—'}
            </td>
            <td class="text-right px-4 py-2">
              <button
                class="px-3 py-1 text-xs rounded border transition-colors
                  {q.paused
                    ? 'border-green-700 text-green-400 hover:bg-green-900/30'
                    : 'border-yellow-700 text-yellow-400 hover:bg-yellow-900/30'}
                  disabled:opacity-50"
                disabled={acting === q.name}
                on:click={() => toggle(q)}
              >
                {acting === q.name ? '…' : q.paused ? 'Resume' : 'Pause'}
              </button>
            </td>
          </tr>
        {/each}
        {#if queues.length === 0}
          <tr><td colspan="7" class="px-4 py-6 text-center text-gray-600">No named queues configured.</td></tr>
        {/if}
      </tbody>
    </table>
  </div>
</div>
