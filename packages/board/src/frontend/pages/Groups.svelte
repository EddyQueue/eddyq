<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { listGroups, pauseGroup, resumeGroup } from '../lib/api.js';
  import type { Group } from '../lib/types.js';

  let groups: Group[] = [];
  let error = '';
  let acting = '';

  async function load() {
    try {
      groups = await listGroups();
      error = '';
    } catch (e) {
      error = (e as Error).message;
    }
  }

  async function toggle(g: Group) {
    acting = g.key;
    try {
      if (g.paused) await resumeGroup(g.key);
      else await pauseGroup(g.key);
      await load();
    } finally { acting = ''; }
  }

  let timer: ReturnType<typeof setInterval>;
  onMount(() => { load(); timer = setInterval(load, 5000); });
  onDestroy(() => clearInterval(timer));
</script>

<div class="space-y-4">
  <h1 class="text-xl font-semibold text-gray-200">Groups</h1>
  {#if error}<div class="text-red-400 text-sm">{error}</div>{/if}

  <div class="overflow-x-auto rounded-lg border border-gray-800">
    <table class="w-full text-sm">
      <thead class="bg-gray-900 text-gray-500 text-xs uppercase tracking-wide">
        <tr>
          <th class="text-left px-4 py-2">Key</th>
          <th class="text-right px-3 py-2">Running</th>
          <th class="text-right px-3 py-2">Concurrency</th>
          <th class="text-left px-3 py-2">Rate Limit</th>
          <th class="text-right px-3 py-2">Tokens</th>
          <th class="text-right px-4 py-2">Status</th>
        </tr>
      </thead>
      <tbody class="divide-y divide-gray-800">
        {#each groups as g}
          <tr class="hover:bg-gray-900/50 transition-colors {g.paused ? 'opacity-60' : ''}">
            <td class="px-4 py-2 font-mono text-gray-200">
              <a href="#/jobs?groupKey={encodeURIComponent(g.key)}" class="hover:text-indigo-400">{g.key}</a>
            </td>
            <td class="text-right px-3 py-2 tabular-nums text-blue-300">{g.runningCount}</td>
            <td class="text-right px-3 py-2 tabular-nums text-gray-400">{g.maxConcurrency}</td>
            <td class="px-3 py-2 text-gray-400 text-xs">
              {#if g.rateCount && g.ratePeriodMs}
                {g.rateCount} / {g.ratePeriodMs}ms
              {:else}
                <span class="text-gray-700">—</span>
              {/if}
            </td>
            <td class="text-right px-3 py-2 tabular-nums text-gray-400 text-xs">
              {g.rateCount ? g.tokens.toFixed(1) : '—'}
            </td>
            <td class="text-right px-4 py-2">
              <button
                class="px-3 py-1 text-xs rounded border transition-colors
                  {g.paused
                    ? 'border-green-700 text-green-400 hover:bg-green-900/30'
                    : 'border-yellow-700 text-yellow-400 hover:bg-yellow-900/30'}
                  disabled:opacity-50"
                disabled={acting === g.key}
                on:click={() => toggle(g)}
              >
                {acting === g.key ? '…' : g.paused ? 'Resume' : 'Pause'}
              </button>
            </td>
          </tr>
        {/each}
        {#if groups.length === 0}
          <tr><td colspan="6" class="px-4 py-6 text-center text-gray-600">No groups found.</td></tr>
        {/if}
      </tbody>
    </table>
  </div>
</div>
