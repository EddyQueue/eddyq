<script lang="ts">
  import type { JobRow } from '../types.js';
  export let job: JobRow;

  function fmt(v: unknown): string {
    return JSON.stringify(v, null, 2);
  }
</script>

<div class="bg-gray-900 rounded-lg border border-gray-700 p-4 space-y-3 text-sm">
  <div class="grid grid-cols-2 gap-x-6 gap-y-1 text-gray-400">
    <span>ID</span>        <span class="text-gray-200 tabular-nums">{job.id}</span>
    <span>Kind</span>      <span class="text-gray-200 font-mono">{job.kind}</span>
    <span>Queue</span>     <span class="text-gray-200">{job.queue}</span>
    <span>Attempt</span>   <span class="text-gray-200 tabular-nums">{job.attempt}/{job.maxAttempts}</span>
    <span>Created</span>   <span class="text-gray-200 tabular-nums">{job.createdAt}</span>
    {#if job.finalizedAt}
      <span>Finalized</span> <span class="text-gray-200 tabular-nums">{job.finalizedAt}</span>
    {/if}
    {#if job.groupKey}
      <span>Group</span>   <span class="text-gray-200 font-mono">{job.groupKey}</span>
    {/if}
    {#if job.tags?.length}
      <span>Tags</span>    <span class="text-gray-200 font-mono">{job.tags.join(', ')}</span>
    {/if}
  </div>

  {#if job.payload !== undefined}
    <div>
      <div class="text-xs text-gray-500 mb-1 uppercase tracking-wide">Payload</div>
      <pre class="bg-gray-950 rounded p-3 text-xs text-green-300 overflow-auto max-h-40">{fmt(job.payload)}</pre>
    </div>
  {/if}

  {#if job.result !== undefined}
    <div>
      <div class="text-xs text-gray-500 mb-1 uppercase tracking-wide">Result</div>
      <pre class="bg-gray-950 rounded p-3 text-xs text-blue-300 overflow-auto max-h-40">{fmt(job.result)}</pre>
    </div>
  {/if}

  {#if job.errors !== undefined && job.errors !== null}
    <div>
      <div class="text-xs text-gray-500 mb-1 uppercase tracking-wide">Errors</div>
      <pre class="bg-gray-950 rounded p-3 text-xs text-red-300 overflow-auto max-h-40">{fmt(job.errors)}</pre>
    </div>
  {/if}
</div>
