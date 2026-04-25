<script lang="ts">
  export let label: string;
  export let value: number;
  export let color: 'slate' | 'yellow' | 'blue' | 'green' | 'red' | 'purple' | 'orange' = 'slate';

  const borders: Record<string, string> = {
    slate:  'border-slate-700',
    yellow: 'border-yellow-600',
    blue:   'border-blue-600',
    green:  'border-green-600',
    red:    'border-red-600',
    purple: 'border-purple-600',
    orange: 'border-orange-600',
  };
  const texts: Record<string, string> = {
    slate:  'text-slate-300',
    yellow: 'text-yellow-300',
    blue:   'text-blue-300',
    green:  'text-green-300',
    red:    'text-red-300',
    purple: 'text-purple-300',
    orange: 'text-orange-300',
  };
</script>

<div class="rounded-lg border bg-gray-900 p-4 flex flex-col gap-1 {borders[color]}">
  <span class="text-xs uppercase tracking-wide text-gray-500">{label}</span>
  <span class="text-3xl font-bold tabular-nums {texts[color]}">{value.toLocaleString()}</span>
</div>
