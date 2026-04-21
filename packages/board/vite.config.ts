import { svelte } from '@sveltejs/vite-plugin-svelte';
import { resolve } from 'path';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [svelte()],
  root: 'src/frontend',
  base: './',
  build: {
    outDir: resolve(import.meta.dirname, 'dist/public'),
    emptyOutDir: true,
  },
});
