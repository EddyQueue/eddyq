<script lang="ts">
  import { onMount } from 'svelte';
  import Nav from './lib/components/Nav.svelte';
  import Groups from './pages/Groups.svelte';
  import Jobs from './pages/Jobs.svelte';
  import Overview from './pages/Overview.svelte';
  import Queues from './pages/Queues.svelte';
  import Schedules from './pages/Schedules.svelte';

  let hash = window.location.hash || '#/';

  onMount(() => {
    const sync = () => { hash = window.location.hash || '#/'; };
    window.addEventListener('hashchange', sync);
    return () => window.removeEventListener('hashchange', sync);
  });

  $: route = hash.replace(/^#/, '') || '/';
  $: navCurrent = '#' + (route.split('?')[0] ?? '/');
</script>

<div class="min-h-screen bg-gray-950 text-gray-100 flex flex-col">
  <header class="border-b border-gray-800 px-6 py-3 flex items-center gap-6">
    <span class="font-bold text-white text-lg tracking-tight select-none">
      <span class="text-indigo-400">eddy</span>q
    </span>
    <Nav current={navCurrent} />
  </header>

  <main class="flex-1 px-6 py-6 max-w-screen-2xl w-full mx-auto">
    {#if route === '/' || route === ''}
      <Overview />
    {:else if route.startsWith('/queues')}
      <Queues />
    {:else if route.startsWith('/jobs')}
      <Jobs />
    {:else if route.startsWith('/schedules')}
      <Schedules />
    {:else if route.startsWith('/groups')}
      <Groups />
    {:else}
      <Overview />
    {/if}
  </main>
</div>
