<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { disableSchedule, enableSchedule, listSchedules, removeSchedule } from '../lib/api.js';
  import type { Schedule } from '../lib/types.js';

  let schedules: Schedule[] = [];
  let error = '';
  let acting = '';

  async function load() {
    try {
      schedules = await listSchedules();
      error = '';
    } catch (e) {
      error = (e as Error).message;
    }
  }

  async function toggle(s: Schedule) {
    acting = s.name;
    try {
      if (s.enabled) await disableSchedule(s.name);
      else await enableSchedule(s.name);
      await load();
    } finally { acting = ''; }
  }

  async function doRemove(name: string) {
    if (!confirm(`Remove schedule "${name}"?`)) return;
    acting = name;
    try { await removeSchedule(name); await load(); }
    finally { acting = ''; }
  }

  function fmtDate(iso?: string) {
    if (!iso) return '—';
    return new Date(iso).toLocaleString('en-US', { dateStyle: 'short', timeStyle: 'medium', hour12: false });
  }

  let timer: ReturnType<typeof setInterval>;
  onMount(() => { load(); timer = setInterval(load, 10_000); });
  onDestroy(() => clearInterval(timer));
</script>

<div class="space-y-4">
  <h1 class="text-xl font-semibold text-gray-200">Schedules</h1>
  {#if error}<div class="text-red-400 text-sm">{error}</div>{/if}

  <div class="overflow-x-auto rounded-lg border border-gray-800">
    <table class="w-full text-sm">
      <thead class="bg-gray-900 text-gray-500 text-xs uppercase tracking-wide">
        <tr>
          <th class="text-left px-4 py-2">Name</th>
          <th class="text-left px-3 py-2">Kind</th>
          <th class="text-left px-3 py-2">Cron</th>
          <th class="text-left px-3 py-2">Next Run</th>
          <th class="text-left px-3 py-2">Last Run</th>
          <th class="text-right px-4 py-2">Actions</th>
        </tr>
      </thead>
      <tbody class="divide-y divide-gray-800">
        {#each schedules as s}
          <tr class="hover:bg-gray-900/50 transition-colors {s.enabled ? '' : 'opacity-50'}">
            <td class="px-4 py-2 font-mono text-gray-200">{s.name}</td>
            <td class="px-3 py-2 font-mono text-gray-400">{s.kind}</td>
            <td class="px-3 py-2 font-mono text-purple-300 text-xs">{s.cronExpr}</td>
            <td class="px-3 py-2 text-gray-400 text-xs tabular-nums">{fmtDate(s.nextRunAt)}</td>
            <td class="px-3 py-2 text-gray-400 text-xs tabular-nums">{fmtDate(s.lastRunAt)}</td>
            <td class="text-right px-4 py-2 flex gap-2 justify-end">
              <button
                class="px-3 py-1 text-xs rounded border transition-colors
                  {s.enabled
                    ? 'border-yellow-700 text-yellow-400 hover:bg-yellow-900/30'
                    : 'border-green-700 text-green-400 hover:bg-green-900/30'}
                  disabled:opacity-50"
                disabled={acting === s.name}
                on:click={() => toggle(s)}
              >
                {acting === s.name ? '…' : s.enabled ? 'Disable' : 'Enable'}
              </button>
              <button
                class="px-3 py-1 text-xs rounded border border-red-800 text-red-400 hover:bg-red-900/30 disabled:opacity-50"
                disabled={acting === s.name}
                on:click={() => doRemove(s.name)}
              >
                Remove
              </button>
            </td>
          </tr>
        {/each}
        {#if schedules.length === 0}
          <tr><td colspan="6" class="px-4 py-6 text-center text-gray-600">No schedules configured.</td></tr>
        {/if}
      </tbody>
    </table>
  </div>
</div>
