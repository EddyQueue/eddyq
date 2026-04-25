<script lang="ts">
  export let state: string;

  const map: Record<string, string> = {
    pending:   'bg-yellow-900 text-yellow-300',
    running:   'bg-blue-900 text-blue-300',
    completed: 'bg-green-900 text-green-300',
    failed:    'bg-red-900 text-red-300',
    scheduled: 'bg-purple-900 text-purple-300',
    cancelled: 'bg-gray-700 text-gray-400',
  };
  $: cls = map[state] ?? 'bg-gray-700 text-gray-400';
</script>

<span class="inline-flex items-center px-2 py-0.5 rounded text-xs font-medium {cls}">
  {state}
</span>
