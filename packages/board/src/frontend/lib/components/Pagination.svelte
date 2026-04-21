<script lang="ts">
  export let page: number;
  export let total: number;
  export let pageSize = 50;
  export let onPage: (p: number) => void;

  $: totalPages = Math.max(1, Math.ceil(total / pageSize));
  $: from = (page - 1) * pageSize + 1;
  $: to = Math.min(page * pageSize, total);
</script>

{#if totalPages > 1}
  <div class="flex items-center gap-3 text-sm text-gray-400">
    <span>{from}–{to} of {total}</span>
    <button
      class="px-3 py-1 rounded border border-gray-700 hover:bg-gray-800 disabled:opacity-40 disabled:cursor-not-allowed"
      disabled={page <= 1}
      on:click={() => onPage(page - 1)}
    >
      Prev
    </button>
    <span class="tabular-nums">{page} / {totalPages}</span>
    <button
      class="px-3 py-1 rounded border border-gray-700 hover:bg-gray-800 disabled:opacity-40 disabled:cursor-not-allowed"
      disabled={page >= totalPages}
      on:click={() => onPage(page + 1)}
    >
      Next
    </button>
  </div>
{:else if total > 0}
  <span class="text-sm text-gray-500">{total} job{total !== 1 ? 's' : ''}</span>
{/if}
