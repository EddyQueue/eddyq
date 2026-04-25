import { defineConfig } from 'vitepress'
import llmstxt from 'vitepress-plugin-llms'

const patternsSection = {
  text: 'Patterns',
  items: [
    { text: 'Bulk enqueue across queues', link: '/patterns/bulk-mixed-queue' },
    { text: 'Idempotent jobs', link: '/patterns/idempotency' },
    { text: 'Throttling jobs', link: '/patterns/throttling' },
    { text: 'Stop retrying', link: '/patterns/stop-retrying' },
    { text: 'Job timeouts', link: '/patterns/timeouts' },
    { text: 'Fail fast on Postgres outage', link: '/patterns/fail-fast' },
  ],
}

const nodeSidebar = [
  {
    text: 'Guide',
    items: [
      { text: 'Quick start', link: '/node/quick-start' },
      { text: 'Workers', link: '/node/workers' },
      { text: 'Enqueueing jobs', link: '/node/enqueueing' },
      { text: 'Retries & cancellation', link: '/node/retries' },
      { text: 'Parallelism & concurrency', link: '/node/concurrency' },
      { text: 'Group concurrency', link: '/node/groups' },
      { text: 'Named queues', link: '/node/queues' },
      { text: 'Schedules', link: '/node/schedules' },
      { text: 'Maintenance & cleanup', link: '/node/maintenance' },
      { text: 'Graceful shutdown', link: '/node/shutdown' },
      { text: 'Deployment', link: '/node/deployment' },
    ],
  },
  patternsSection,
  {
    text: 'Reference',
    items: [
      { text: 'API: @eddyq/queue ↗', link: '/api/queue/', target: '_blank' },
    ],
  },
]

const nestjsSidebar = [
  {
    text: 'Guide',
    items: [
      { text: 'Setup', link: '/nestjs/setup' },
      { text: 'Defining workers', link: '/nestjs/workers' },
      { text: 'Injecting the queue', link: '/nestjs/injecting' },
      { text: 'Schedules', link: '/nestjs/schedules' },
      { text: 'Lifecycle & shutdown', link: '/nestjs/lifecycle' },
      { text: 'Example app', link: '/nestjs/example' },
    ],
  },
  patternsSection,
  {
    text: 'Reference',
    items: [
      { text: 'API: @eddyq/nestjs ↗', link: '/api/nestjs/', target: '_blank' },
    ],
  },
]

const wakeboardSidebar = [
  {
    text: 'Wakeboard',
    items: [
      { text: 'Overview', link: '/wakeboard/' },
      { text: 'NestJS setup', link: '/wakeboard/nestjs' },
    ],
  },
]

export default defineConfig({
  title: 'eddyq',
  description: 'Postgres-backed job queue with a Rust core and first-class Node bindings.',
  base: '/eddyq/',
  cleanUrls: true,
  lastUpdated: true,
  head: [
    ['meta', { name: 'theme-color', content: '#3c82f6' }],
    ['meta', { property: 'og:type', content: 'website' }],
    ['meta', { property: 'og:title', content: 'eddyq' }],
    ['meta', { property: 'og:description', content: 'Postgres-backed job queue with a Rust core and first-class Node bindings.' }],
  ],
  themeConfig: {
    nav: [
      { text: 'Guide', link: '/guide/', activeMatch: '/guide/' },
      { text: 'Node.js', link: '/node/quick-start', activeMatch: '/node/' },
      { text: 'NestJS', link: '/nestjs/setup', activeMatch: '/nestjs/' },
      { text: 'Wakeboard', link: '/wakeboard/', activeMatch: '/wakeboard/' },
      {
        text: 'API Reference',
        items: [
          { text: '@eddyq/queue', link: '/api/queue/' },
          { text: '@eddyq/nestjs', link: '/api/nestjs/' },
        ],
      },
    ],
    sidebar: {
      '/guide/': [
        {
          text: 'Introduction',
          items: [
            { text: 'What is eddyq', link: '/guide/' },
            { text: 'Installation', link: '/guide/installation' },
            { text: 'Migrations', link: '/guide/migrations' },
          ],
        },
        {
          text: 'Pick your stack',
          items: [
            { text: 'Node.js →', link: '/node/quick-start' },
            { text: 'NestJS →', link: '/nestjs/setup' },
            { text: 'Wakeboard admin UI →', link: '/wakeboard/' },
          ],
        },
      ],
      '/node/': nodeSidebar,
      '/nestjs/': nestjsSidebar,
      '/wakeboard/': wakeboardSidebar,
      '/patterns/': nodeSidebar,
    },
    socialLinks: [
      { icon: 'github', link: 'https://github.com/EddyQueue/eddyq' },
    ],
    search: { provider: 'local' },
    editLink: {
      pattern: 'https://github.com/EddyQueue/eddyq/edit/main/docs/:path',
      text: 'Edit this page on GitHub',
    },
    footer: {
      message: 'Released under the MIT or Apache-2.0 License.',
      copyright: 'Copyright © 2026 eddyq contributors',
    },
  },
  vite: {
    plugins: [
      llmstxt({
        description: 'Postgres-backed job queue with a Rust core and first-class Node + NestJS bindings.',
      }) as any,
    ],
  },
})
