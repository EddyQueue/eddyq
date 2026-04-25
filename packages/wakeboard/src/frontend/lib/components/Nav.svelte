<script lang="ts">
  export let current: string;

  const links = [
    { href: '#/',          label: 'Overview'  },
    { href: '#/queues',    label: 'Queues'    },
    { href: '#/jobs',      label: 'Jobs'      },
    { href: '#/schedules', label: 'Schedules' },
    { href: '#/groups',    label: 'Groups'    },
  ];
</script>

<nav class="flex gap-1">
  {#each links as { href, label }}
    <a
      {href}
      class="px-3 py-1.5 rounded text-sm transition-colors {current === href
        ? 'bg-indigo-600 text-white'
        : 'text-gray-400 hover:text-gray-100 hover:bg-gray-800'}"
    >
      {label}
    </a>
  {/each}
</nav>
