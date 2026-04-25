<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { getStats } from '../lib/api.js';
  import StatCard from '../lib/components/StatCard.svelte';
  import type { QueueStateCount } from '../lib/types.js';

  const STATE_ORDER = ['pending', 'running', 'completed', 'failed', 'scheduled', 'cancelled'];
  const STATE_COLORS: Record<string, 'yellow' | 'blue' | 'green' | 'red' | 'purple' | 'slate'> = {
    pending:   'yellow',
    running:   'blue',
    completed: 'green',
    failed:    'red',
    scheduled: 'purple',
    cancelled: 'slate',
  };

  let rows: QueueStateCount[] = [];
  let loading = true;
  let error = '';

  // totals[state] = count across all queues
  $: totals = rows.reduce<Record<string, number>>((acc, r) => {
    acc[r.state] = (acc[r.state] ?? 0) + r.count;
    return acc;
  }, {});

  // byQueue[queue][state] = count
  $: byQueue = rows.reduce<Record<string, Record<string, number>>>((acc, r) => {
    if (!acc[r.queue]) acc[r.queue] = {};
    acc[r.queue][r.state] = r.count;
    return acc;
  }, {});
  $: queues = Object.keys(byQueue).sort();

  async function load() {
    try {
      const stats = await getStats();
      rows = stats.byQueueState;
      error = '';
    } catch (e) {
      error = (e as Error).message;
    } finally {
      loading = false;
    }
  }

  let timer: ReturnType<typeof setInterval>;
  onMount(() => { load(); timer = setInterval(load, 5000); });
  onDestroy(() => clearInterval(timer));
</script>

<div class="space-y-6">
  <h1 class="text-xl font-semibold text-gray-200">Overview</h1>

  {#if error}
    <div class="text-red-400 text-sm">{error}</div>
  {/if}

  <div class="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-6 gap-4">
    {#each STATE_ORDER as state}
      <a href="#/jobs?state={state}" class="block hover:opacity-80 transition-opacity">
        <StatCard label={state} value={totals[state] ?? 0} color={STATE_COLORS[state] ?? 'slate'} />
      </a>
    {/each}
  </div>

  {#if queues.length > 0}
    <div>
      <h2 class="text-sm font-semibold text-gray-400 uppercase tracking-wide mb-3">Queues</h2>
      <div class="overflow-x-auto rounded-lg border border-gray-800">
        <table class="w-full text-sm">
          <thead class="bg-gray-900 text-gray-500 text-xs uppercase tracking-wide">
            <tr>
              <th class="text-left px-4 py-2">Queue</th>
              {#each STATE_ORDER as s}
                <th class="text-right px-3 py-2">{s}</th>
              {/each}
            </tr>
          </thead>
          <tbody class="divide-y divide-gray-800">
            {#each queues as q}
              <tr class="hover:bg-gray-900/50 transition-colors">
                <td class="px-4 py-2 font-mono text-gray-200">
                  <a href="#/jobs?queue={encodeURIComponent(q)}" class="hover:text-indigo-400">{q}</a>
                </td>
                {#each STATE_ORDER as s}
                  <td class="text-right px-3 py-2 tabular-nums text-gray-400">
                    {#if byQueue[q][s]}
                      <a href="#/jobs?queue={encodeURIComponent(q)}&state={s}"
                         class="hover:text-gray-200">
                        {byQueue[q][s].toLocaleString()}
                      </a>
                    {:else}
                      <span class="text-gray-700">—</span>
                    {/if}
                  </td>
                {/each}
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </div>
  {:else if !loading}
    <p class="text-gray-500 text-sm">No jobs yet.</p>
  {/if}
</div>
